//! Embedded RDP client built on the pure-Rust IronRDP crates.
//!
//! Unlike the `mstsc.exe` fallback (see `connect_rdp` in `lib.rs`), this needs no
//! external program: it speaks RDP itself in a background tokio task, decodes the
//! remote framebuffer, and streams it to the webview `<canvas>` over a Tauri
//! `Channel`. Keyboard/mouse events flow back the other way through `rdp_input`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use serde::Deserialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager as _, State};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use ironrdp::connector::connection_activation::ConnectionActivationState;
use ironrdp::connector::{
    self, ClientConnector, ConnectionResult, ConnectorError, ConnectorErrorExt as _, ConnectorResult, Credentials,
    ServerName,
};
use ironrdp::connector::sspi::generator::NetworkRequest;
use ironrdp::core::WriteBuf;
use ironrdp::cliprdr::backend::ClipboardMessage;
use ironrdp::cliprdr::{Cliprdr, CliprdrClient};
use ironrdp::displaycontrol::client::DisplayControlClient;
use ironrdp::dvc::DrdynvcClient;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{fast_path, ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_bulk::BulkCompressor;
use ironrdp_egfx::client::GraphicsPipelineClient;
use ironrdp_tokio::{single_sequence_step_read, FramedWrite as _, TokioFramed};

use crate::clipboard::{advertise_text, LocalClipboard, WebClipboardBackend};
use crate::egfx::EgfxHandler;
use crate::ConnectRequest;

// --- Channel wire format (raw bytes, little-endian) ----------------------------
// tag 0 = init:  [0][width u16][height u16]      -> frontend sizes the canvas
// tag 1 = strip: [1][y u16][height u16][rgba..]  -> full-width RGBA strip at row y
// tag 2 = status:[2][utf-8 message]              -> human-readable status / error
// tag 3 = rect:  [3][x][y][w][h][rgba..]         -> arbitrary dirty rect (EGFX)
pub(crate) const TAG_INIT: u8 = 0;
const TAG_STRIP: u8 = 1;
const TAG_STATUS: u8 = 2;
// tag 3 = rect: [3][x u16][y u16][w u16][h u16][rgba..] -> arbitrary dirty rect.
// Used by the Graphics Pipeline (EGFX) path, whose updates aren't full-width
// strips. Same format `vnc.rs` sends for RFB rectangles.
const TAG_RECT: u8 = 3;
// tag 7 = clipboard: [7][utf-8 text] -> remote clipboard text for the webview.
// NOTE: this was tag 3 until EGFX landed, which collided with the rect tag above
// and meant `handleFrame` in App.vue never reached the clipboard branch.
pub(crate) const TAG_CLIPBOARD: u8 = 7;
// tag 6 = ended: [6][utf-8 message] -> the session is over (error or clean close).
// Kept distinct from tag 2 so the UI can tell "still working on it" apart from
// "it stopped", instead of guessing from the wording.
const TAG_ENDED: u8 = 6;

fn status_msg(text: &str) -> InvokeResponseBody {
    tagged(TAG_STATUS, text)
}

fn ended_msg(text: &str) -> InvokeResponseBody {
    tagged(TAG_ENDED, text)
}

fn tagged(tag: u8, text: &str) -> InvokeResponseBody {
    let mut v = Vec::with_capacity(1 + text.len());
    v.push(tag);
    v.extend_from_slice(text.as_bytes());
    InvokeResponseBody::Raw(v)
}

/// Per-app state holding every active session, keyed by a frontend-assigned id
/// (one per tab). Supports multiple concurrent remote desktops.
#[derive(Default)]
pub struct RdpState {
    sessions: Mutex<HashMap<u32, SessionHandle>>,
}

struct SessionHandle {
    input_tx: mpsc::UnboundedSender<Vec<Operation>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    /// Text the webview offers for the remote to paste; read by the clipboard backend.
    clip_local: LocalClipboard,
    /// Outbound clipboard messages drained by the session loop.
    clip_tx: mpsc::UnboundedSender<ClipboardMessage>,
    cancel: CancellationToken,
}

/// Input event coming from the webview canvas.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RdpInputEvent {
    /// Absolute pointer position in remote-desktop pixels.
    Move { x: u16, y: u16 },
    /// Mouse button using the web `MouseEvent.button` numbering (0=left,1=mid,2=right).
    Button { button: u8, pressed: bool },
    /// Mouse wheel; `delta` in wheel units (positive = up/left).
    Wheel { vertical: bool, delta: i16 },
    /// Keyboard key as a set-1 scancode (extended keys carry the 0xE000 prefix).
    Key { scancode: u16, pressed: bool },
}

fn to_operations(ev: RdpInputEvent) -> Vec<Operation> {
    match ev {
        RdpInputEvent::Move { x, y } => vec![Operation::MouseMove(MousePosition { x, y })],
        RdpInputEvent::Button { button, pressed } => match MouseButton::from_web_button(button) {
            Some(b) if pressed => vec![Operation::MouseButtonPressed(b)],
            Some(b) => vec![Operation::MouseButtonReleased(b)],
            None => vec![],
        },
        RdpInputEvent::Wheel { vertical, delta } => vec![Operation::WheelRotations(WheelRotations {
            is_vertical: vertical,
            rotation_units: delta,
        })],
        RdpInputEvent::Key { scancode, pressed } => {
            let sc = Scancode::from_u16(scancode);
            vec![if pressed {
                Operation::KeyPressed(sc)
            } else {
                Operation::KeyReleased(sc)
            }]
        }
    }
}

/// CredSSP network client. We only support NTLM (no Kerberos/KDC), so this is
/// never actually called during NTLM authentication; if a server insists on a
/// KDC round-trip we fail with a clear message instead of hanging.
struct NoKdcNetworkClient;

impl ironrdp_tokio::NetworkClient for NoKdcNetworkClient {
    async fn send(&mut self, _request: &NetworkRequest) -> ConnectorResult<Vec<u8>> {
        Err(ConnectorError::general(
            "Kerberos/KDC authentication is not supported (use a local NTLM account)",
        ))
    }
}

// --- Tauri commands ------------------------------------------------------------

#[tauri::command]
pub async fn rdp_connect(
    session_id: u32,
    req: ConnectRequest,
    width: u16,
    height: u16,
    on_frame: Channel<InvokeResponseBody>,
    state: State<'_, RdpState>,
    app: AppHandle,
) -> Result<(), String> {
    let password = crate::get_password(req.id)?
        .ok_or_else(|| "No saved password for this connection".to_string())?;

    log::info!(
        "rdp_connect: session={} conn_id={} host={} port={} user={} size={}x{}",
        session_id, req.id, req.host, req.port, req.username, width, height
    );

    // Replace any existing session under this id (e.g. a reconnect on the same tab).
    let cancel = CancellationToken::new();
    let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<Operation>>();
    let (resize_tx, resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    let (clip_tx, clip_rx) = mpsc::unbounded_channel::<ClipboardMessage>();
    let clip_local: LocalClipboard = Arc::new(Mutex::new(None));
    {
        let mut guard = state.sessions.lock().unwrap();
        if let Some(old) = guard.insert(
            session_id,
            SessionHandle {
                input_tx,
                resize_tx,
                clip_local: clip_local.clone(),
                clip_tx: clip_tx.clone(),
                cancel: cancel.clone(),
            },
        ) {
            old.cancel.cancel();
        }
    }

    tauri::async_runtime::spawn(async move {
        let frame = on_frame.clone();
        let outcome = run_session(
            req, password, width, height, on_frame, input_rx, resize_rx, clip_local, clip_tx, clip_rx, cancel,
        )
        .await;
        // Remove ourselves from the registry once the task is over.
        if let Some(state) = app.try_state::<RdpState>() {
            state.sessions.lock().unwrap().remove(&session_id);
        }
        match outcome {
            Err(e) => {
                log::error!("rdp session {session_id} ended with error: {e}");
                let _ = frame.send(ended_msg(&format!("Disconnected: {e}")));
            }
            Ok(()) => {
                let _ = frame.send(ended_msg("Session closed"));
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub fn rdp_input(session_id: u32, event: RdpInputEvent, state: State<'_, RdpState>) -> Result<(), String> {
    let guard = state.sessions.lock().unwrap();
    if let Some(session) = guard.get(&session_id) {
        // If the receiver is gone the session already ended; ignore the error.
        let _ = session.input_tx.send(to_operations(event));
    }
    Ok(())
}

/// The webview's local clipboard text changed; store it and advertise it to the
/// remote so it can be pasted there.
#[tauri::command]
pub fn rdp_clipboard_offer(session_id: u32, text: String, state: State<'_, RdpState>) -> Result<(), String> {
    let guard = state.sessions.lock().unwrap();
    if let Some(session) = guard.get(&session_id) {
        *session.clip_local.lock().unwrap() = Some(text);
        let _ = session.clip_tx.send(advertise_text());
    }
    Ok(())
}

#[tauri::command]
pub fn rdp_resize(
    session_id: u32,
    width: u16,
    height: u16,
    state: State<'_, RdpState>,
) -> Result<(), String> {
    let guard = state.sessions.lock().unwrap();
    if let Some(session) = guard.get(&session_id) {
        let _ = session.resize_tx.send((width, height));
    }
    Ok(())
}

#[tauri::command]
pub fn rdp_disconnect(session_id: u32, state: State<'_, RdpState>) -> Result<(), String> {
    if let Some(session) = state.sessions.lock().unwrap().remove(&session_id) {
        session.cancel.cancel();
    }
    Ok(())
}

// --- Session task --------------------------------------------------------------

async fn run_session(
    req: ConnectRequest,
    password: String,
    width: u16,
    height: u16,
    on_frame: Channel<InvokeResponseBody>,
    mut input_rx: mpsc::UnboundedReceiver<Vec<Operation>>,
    mut resize_rx: mpsc::UnboundedReceiver<(u16, u16)>,
    clip_local: LocalClipboard,
    clip_tx: mpsc::UnboundedSender<ClipboardMessage>,
    mut clip_rx: mpsc::UnboundedReceiver<ClipboardMessage>,
    cancel: CancellationToken,
) -> Result<(), String> {
    // Some servers — notably GNOME Remote Desktop in "Remote Login" mode — drop
    // the connection right after authentication (ServerDeactivateAll + an MCS
    // Disconnect Ultimatum) as part of a session handover, expecting the client to
    // reconnect. mstsc does this transparently; mirror it with a bounded retry.
    const MAX_ATTEMPTS: u32 = 6;
    let (upgraded_framed, connection_result) = {
        let mut established = None;
        let mut last_err = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let clip_backend = WebClipboardBackend::new(clip_local.clone(), clip_tx.clone(), on_frame.clone());
            match establish(&req, &password, width, height, clip_backend, &on_frame).await {
                Ok(pair) => {
                    established = Some(pair);
                    break;
                }
                // Retry only while attempts remain. The final attempt falls through
                // to the arm below so `established` stays `None` and the caller gets
                // the explanatory message rather than the raw IronRDP error.
                Err(e) if is_handoff_disconnect(&e) && attempt < MAX_ATTEMPTS => {
                    log::warn!("connect attempt {attempt}/{MAX_ATTEMPTS} hit handoff disconnect; reconnecting: {e}");
                    let _ = on_frame.send(status_msg(&format!(
                        "Server handed off the session, reconnecting (attempt {attempt})..."
                    )));
                    last_err = e;
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                }
                Err(e) if is_handoff_disconnect(&e) => {
                    last_err = e;
                }
                Err(e) => return Err(e),
            }
        }
        match established {
            Some(pair) => pair,
            None => {
                return Err(format!(
                    "server kept dropping the session after {MAX_ATTEMPTS} reconnect attempts (last: {last_err}). \
                     If the host is GNOME/Ubuntu, check `journalctl --user -u gnome-remote-desktop -f` on it while \
                     connecting — it names the reason. Otherwise use the Windows RDP button for this host."
                ));
            }
        }
    };

    let ConnectionResult {
        io_channel_id,
        user_channel_id,
        message_channel_id,
        share_id,
        static_channels,
        desktop_size: desktop,
        enable_server_pointer,
        pointer_software_rendering,
        activation_factory,
        compression_type,
    } = connection_result;
    log::info!("rdp_connect: negotiated desktop {}x{}", desktop.width, desktop.height);

    let mut image = DecodedImage::new(PixelFormat::RgbA32, desktop.width, desktop.height);
    let mut active_stage = ActiveStageBuilder {
        static_channels,
        user_channel_id,
        io_channel_id,
        message_channel_id,
        share_id,
        compression_type,
        enable_server_pointer,
        pointer_software_rendering,
    }
    .build();
    let mut db = Database::new();

    // Split so we can read PDUs and write input/response frames concurrently.
    let (mut reader, mut writer) = ironrdp_tokio::split_tokio_framed(upgraded_framed);

    // Tell the frontend the real (negotiated) size so it can size the canvas.
    let mut init = Vec::with_capacity(5);
    init.push(TAG_INIT);
    init.extend_from_slice(&desktop.width.to_le_bytes());
    init.extend_from_slice(&desktop.height.to_le_bytes());
    on_frame
        .send(InvokeResponseBody::Raw(init))
        .map_err(|e| e.to_string())?;

    // Latest requested desktop size that we couldn't apply yet (e.g. the Display
    // Control channel wasn't ready). Retried after each loop iteration.
    let mut pending_resize: Option<(u16, u16)> = None;

    'session: loop {
        let outputs: Vec<ActiveStageOutput> = tokio::select! {
            biased;
            _ = cancel.cancelled() => break 'session,
            maybe = resize_rx.recv() => {
                match maybe {
                    // Coalesce to the most recent size; applied below once per loop.
                    Some(size) => { pending_resize = Some(size); }
                    None => break 'session,
                }
                Vec::new()
            }
            maybe = input_rx.recv() => {
                let Some(ops) = maybe else { break 'session };
                let events = db.apply(ops);
                active_stage
                    .process_fastpath_input(&mut image, &events)
                    .map_err(|e| format!("input encode failed: {e}"))?
            }
            maybe = clip_rx.recv() => {
                // Clipboard PDUs to send (advertise/paste/data). The receiver only
                // closes when the session is torn down, so `None` ends the loop.
                let Some(msg) = maybe else { break 'session };
                match dispatch_clipboard(&mut active_stage, msg) {
                    Ok(bytes) if !bytes.is_empty() => {
                        writer
                            .write_all(&bytes)
                            .await
                            .map_err(|e| format!("clipboard write failed: {e}"))?;
                    }
                    Ok(_) => {}
                    Err(e) => log::warn!("clipboard dispatch failed: {e}"),
                }
                Vec::new()
            }
            res = reader.read_pdu() => {
                let (action, payload) = match res {
                    Ok(v) => v,
                    Err(e) => return Err(format!("connection lost: {e}")),
                };
                active_stage
                    .process(&mut image, action, &payload)
                    .map_err(|e| format!("frame decode failed: {e}"))?
            }
        };

        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => {
                    writer
                        .write_all(&frame)
                        .await
                        .map_err(|e| format!("write failed: {e}"))?;
                }
                ActiveStageOutput::GraphicsUpdate(rect) => {
                    send_strip(&on_frame, &image, &rect)?;
                }
                ActiveStageOutput::Terminate(reason) => {
                    log::info!("rdp session terminated: {reason}");
                    break 'session;
                }
                ActiveStageOutput::DeactivateAll => {
                    // Server-initiated Deactivation-Reactivation (e.g. after a resize).
                    // Drive a fresh activation sequence to completion, then rebuild the
                    // framebuffer and processors at the new desktop size.
                    let mut cas = activation_factory.create();
                    reactivate(
                        &mut reader,
                        &mut writer,
                        &mut cas,
                        io_channel_id,
                        user_channel_id,
                        compression_type,
                        &mut image,
                        &mut active_stage,
                        &on_frame,
                    )
                    .await?;
                }
                // Pointer updates: we rely on the local OS cursor over the canvas,
                // so the remote pointer sprite is intentionally ignored for now.
                _ => {}
            }
        }

        // Apply a pending resize once the Display Control channel is ready.
        if let Some((w, h)) = pending_resize {
            match active_stage.encode_resize(u32::from(w), u32::from(h), None, None) {
                Some(Ok(frame)) => {
                    writer
                        .write_all(&frame)
                        .await
                        .map_err(|e| format!("resize write failed: {e}"))?;
                    log::info!("rdp: requested resize to {w}x{h}");
                    pending_resize = None;
                }
                Some(Err(e)) => {
                    log::warn!("rdp: resize encode failed: {e}");
                    pending_resize = None;
                }
                // Channel not ready yet — keep the request and retry next iteration.
                None => {}
            }
        }
    }

    Ok(())
}

/// Run the Deactivation-Reactivation sequence after a `ServerDeactivateAll`, then
/// rebuild the framebuffer and active-stage processors for the new desktop size
/// and tell the frontend to resize its canvas.
async fn reactivate<R, W>(
    reader: &mut TokioFramed<R>,
    writer: &mut TokioFramed<W>,
    cas: &mut ironrdp::connector::connection_activation::ConnectionActivationSequence,
    io_channel_id: u16,
    user_channel_id: u16,
    compression_type: Option<ironrdp::pdu::rdp::client_info::CompressionType>,
    image: &mut DecodedImage,
    active_stage: &mut ActiveStage,
    on_frame: &Channel<InvokeResponseBody>,
) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Send + Sync + Unpin,
    W: tokio::io::AsyncWrite + Send + Sync + Unpin,
{
    log::info!("rdp: server reactivation sequence");
    let mut buf = WriteBuf::new();
    loop {
        let written = single_sequence_step_read(reader, cas, &mut buf)
            .await
            .map_err(|e| format!("reactivation step failed: {e}"))?;
        if written.size().is_some() {
            writer
                .write_all(buf.filled())
                .await
                .map_err(|e| format!("reactivation write failed: {e}"))?;
        }
        if let ConnectionActivationState::Finalized {
            desktop_size,
            share_id,
            enable_server_pointer,
            pointer_software_rendering,
        } = cas.connection_activation_state()
        {
            *image = DecodedImage::new(PixelFormat::RgbA32, desktop_size.width, desktop_size.height);
            // Rebuilding the fast-path processor drops its bulk decompressor, so
            // recreate one when compression was negotiated — otherwise every
            // compressed server PDU after a reactivation would fail to decode.
            let bulk_decompressor = compression_type.and_then(|ct| BulkCompressor::new(to_bulk_compression_type(ct)).ok());
            active_stage.set_fastpath_processor(
                fast_path::ProcessorBuilder {
                    io_channel_id,
                    user_channel_id,
                    share_id,
                    enable_server_pointer,
                    pointer_software_rendering,
                    bulk_decompressor,
                }
                .build(),
            );
            active_stage.set_share_id(share_id);
            active_stage.set_enable_server_pointer(enable_server_pointer);

            let mut init = Vec::with_capacity(5);
            init.push(TAG_INIT);
            init.extend_from_slice(&desktop_size.width.to_le_bytes());
            init.extend_from_slice(&desktop_size.height.to_le_bytes());
            let _ = on_frame.send(InvokeResponseBody::Raw(init));
            log::info!("rdp: reactivated at {}x{}", desktop_size.width, desktop_size.height);
            return Ok(());
        }
    }
}

/// Turn a queued [`ClipboardMessage`] into the wire bytes to send on the CLIPRDR
/// channel. Looks up the live `Cliprdr` processor in the active stage, invokes the
/// matching method, and encodes the resulting SVC messages.
fn dispatch_clipboard(stage: &mut ActiveStage, msg: ClipboardMessage) -> Result<Vec<u8>, String> {
    let cliprdr = stage
        .get_svc_processor_mut::<CliprdrClient>()
        .ok_or_else(|| "cliprdr channel not available".to_string())?;
    let svc_messages = match msg {
        ClipboardMessage::SendInitiateCopy(formats) => cliprdr.initiate_copy(&formats),
        ClipboardMessage::SendInitiatePaste(format_id) => cliprdr.initiate_paste(format_id),
        ClipboardMessage::SendFormatData(response) => cliprdr.submit_format_data(response),
        // File transfer is unsupported (we advertise empty capabilities).
        ClipboardMessage::SendFileContentsRequest(_)
        | ClipboardMessage::SendFileContentsResponse(_)
        | ClipboardMessage::SendInitiateFileCopy(_) => {
            return Ok(Vec::new());
        }
        ClipboardMessage::Error(e) => {
            log::warn!("clipboard backend error: {e}");
            return Ok(Vec::new());
        }
    }
    .map_err(|e| format!("clipboard encode failed: {e}"))?;
    stage
        .process_svc_processor_messages(svc_messages)
        .map_err(|e| format!("clipboard svc encode failed: {e}"))
}

/// One full connect attempt: TCP → RDP negotiation → TLS → CredSSP/NLA. Returns
/// the upgraded framed stream plus the negotiated connection result, ready for
/// the active session. Kept separate from `run_session` so it can be retried.
async fn establish(
    req: &ConnectRequest,
    password: &str,
    width: u16,
    height: u16,
    clipboard_backend: Box<dyn ironrdp::cliprdr::backend::CliprdrBackend>,
    on_frame: &Channel<InvokeResponseBody>,
) -> Result<
    (
        TokioFramed<tokio_rustls::client::TlsStream<TcpStream>>,
        ConnectionResult,
    ),
    String,
> {
    let config = build_config(&req.username, password.to_owned(), width, height);

    // Each step below can stall for seconds on a slow or unreachable host, so
    // report them as they start — otherwise the UI just says "connecting".
    let server_addr = format!("{}:{}", req.host, req.port);
    let _ = on_frame.send(status_msg(&format!("Connecting to {server_addr}...")));
    let tcp = TcpStream::connect(&server_addr)
        .await
        .map_err(|e| format!("TCP connect to {server_addr} failed: {e}"))?;
    let client_addr = tcp.local_addr().map_err(|e| e.to_string())?;

    let mut framed: TokioFramed<TcpStream> = TokioFramed::new(tcp);
    // Dynamic channels: Display Control (resize) and the Graphics Pipeline (EGFX).
    // Static channel: clipboard. EGFX is mandatory for servers with no legacy
    // bitmap path — notably gnome-remote-desktop; see plan-egfx.md. `None` here is
    // the H.264 decoder: we don't ship one, so the crate negotiates a non-AVC
    // capability set and the server falls back to RemoteFX/uncompressed.
    let drdynvc = DrdynvcClient::new()
        .with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())))
        .with_dynamic_channel(GraphicsPipelineClient::new(
            Box::new(EgfxHandler::new(on_frame.clone())),
            None,
        ));
    let cliprdr: CliprdrClient = Cliprdr::new(clipboard_backend);
    let mut connector = ClientConnector::new(config, client_addr)
        .with_static_channel(drdynvc)
        .with_static_channel(cliprdr);

    let _ = on_frame.send(status_msg("Negotiating RDP protocol..."));
    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
        .await
        .map_err(|e| format!("RDP negotiation failed: {e}"))?;

    // The connection must now be wrapped in TLS before CredSSP/NLA continues.
    let _ = on_frame.send(status_msg("Securing connection (TLS)..."));
    let initial_stream = framed.into_inner_no_leftover();
    let (tls_stream, server_public_key) = tls_upgrade(initial_stream, &req.host)
        .await
        .map_err(|e| format!("TLS upgrade failed: {e}"))?;

    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let mut upgraded_framed = TokioFramed::new(tls_stream);

    let _ = on_frame.send(status_msg(&format!(
        "Signing in as {} (NLA/CredSSP)...",
        req.username
    )));
    let mut network_client = NoKdcNetworkClient;
    let connection_result = ironrdp_tokio::connect_finalize(
        upgraded,
        connector,
        &mut upgraded_framed,
        &mut network_client,
        ServerName::new(&req.host),
        server_public_key,
        None,
    )
    .await
    .map_err(|e| format!("authentication/finalize failed: {e}"))?;

    let _ = on_frame.send(status_msg("Starting desktop session..."));
    Ok((upgraded_framed, connection_result))
}

/// Map the PDU-layer compression type onto the bulk crate's. Mirrors the private
/// helper of the same name in `ironrdp-session`, needed because `reactivate()`
/// rebuilds the fast-path processor by hand.
fn to_bulk_compression_type(ct: ironrdp::pdu::rdp::client_info::CompressionType) -> ironrdp_bulk::CompressionType {
    use ironrdp::pdu::rdp::client_info::CompressionType as Pdu;
    match ct {
        Pdu::K8 => ironrdp_bulk::CompressionType::Rdp4,
        Pdu::K64 => ironrdp_bulk::CompressionType::Rdp5,
        Pdu::Rdp6 => ironrdp_bulk::CompressionType::Rdp6,
        Pdu::Rdp61 => ironrdp_bulk::CompressionType::Rdp61,
    }
}

/// True when an error is the post-auth "session handover" disconnect some servers
/// (e.g. GNOME Remote Login) issue, expecting the client to reconnect.
fn is_handoff_disconnect(err: &str) -> bool {
    err.contains("disconnect provider ultimatum") || err.contains("UserRequested")
}

/// Send an arbitrary RGBA dirty rectangle (tag 3). Used by the EGFX path, whose
/// updates are surface rectangles rather than full-width strips.
pub(crate) fn send_rect(
    on_frame: &Channel<InvokeResponseBody>,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    rgba: &[u8],
) -> Result<(), String> {
    let mut msg = Vec::with_capacity(9 + rgba.len());
    msg.push(TAG_RECT);
    msg.extend_from_slice(&x.to_le_bytes());
    msg.extend_from_slice(&y.to_le_bytes());
    msg.extend_from_slice(&w.to_le_bytes());
    msg.extend_from_slice(&h.to_le_bytes());
    msg.extend_from_slice(rgba);
    on_frame.send(InvokeResponseBody::Raw(msg)).map_err(|e| e.to_string())
}

/// Send the changed rows as a full-width RGBA strip. We widen every dirty
/// rectangle to the full image width so the pixel data is contiguous in
/// `DecodedImage` and maps directly onto a canvas `putImageData`.
fn send_strip(
    on_frame: &Channel<InvokeResponseBody>,
    image: &DecodedImage,
    rect: &InclusiveRectangle,
) -> Result<(), String> {
    let top = rect.top;
    let bottom = rect.bottom;
    let strip_height = bottom - top + 1;
    let full = InclusiveRectangle {
        left: 0,
        top,
        right: image.width() - 1,
        bottom,
    };
    let data = image.data_for_rect(&full);

    let mut msg = Vec::with_capacity(5 + data.len());
    msg.push(TAG_STRIP);
    msg.extend_from_slice(&top.to_le_bytes());
    msg.extend_from_slice(&strip_height.to_le_bytes());
    msg.extend_from_slice(data);
    on_frame
        .send(InvokeResponseBody::Raw(msg))
        .map_err(|e| e.to_string())
}

fn build_config(username: &str, password: String, width: u16, height: u16) -> connector::Config {
    use ironrdp::pdu::gcc::KeyboardType;
    use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
    use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};

    connector::Config {
        credentials: Credentials::UsernamePassword {
            username: username.to_owned(),
            password,
        },
        domain: None,
        // Enforce NLA (CredSSP) so the stored credentials log us straight in
        // without a graphical login screen.
        enable_tls: false,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: connector::DesktopSize {
            width: width.max(200),
            height: height.max(200),
        },
        bitmap: None,
        client_build: 0,
        client_name: "remote-pc-manager".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),

        #[cfg(windows)]
        platform: MajorPlatformType::WINDOWS,
        #[cfg(target_os = "macos")]
        platform: MajorPlatformType::MACINTOSH,
        #[cfg(target_os = "linux")]
        platform: MajorPlatformType::UNIX,
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        platform: MajorPlatformType::UNIX,

        enable_server_pointer: false,
        request_data: None,
        autologon: true,
        enable_audio_playback: false,
        compression_type: None,
        pointer_software_rendering: false,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

async fn tls_upgrade(
    stream: TcpStream,
    server_name: &str,
) -> Result<(tokio_rustls::client::TlsStream<TcpStream>, Vec<u8>), String> {
    use tokio_rustls::rustls;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(danger::NoCertificateVerification))
        .with_no_client_auth();
    // CredSSP does not support TLS session resumption.
    config.resumption = rustls::client::Resumption::disabled();

    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let dns = rustls::pki_types::ServerName::try_from(server_name.to_owned())
        .map_err(|e| format!("invalid server name: {e}"))?;
    let tls = connector
        .connect(dns, stream)
        .await
        .map_err(|e| format!("TLS handshake: {e}"))?;

    let server_public_key = {
        let (_, conn) = tls.get_ref();
        let cert = conn
            .peer_certificates()
            .and_then(|certs| certs.first())
            .ok_or_else(|| "server presented no certificate".to_string())?;
        extract_tls_server_public_key(cert).map_err(|e| format!("read server public key: {e}"))?
    };

    Ok((tls, server_public_key))
}

fn extract_tls_server_public_key(cert: &[u8]) -> Result<Vec<u8>, String> {
    use x509_cert::der::Decode as _;

    let cert = x509_cert::Certificate::from_der(cert).map_err(|e| e.to_string())?;
    let key = cert
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .ok_or_else(|| "subject public key BIT STRING is not byte-aligned".to_string())?
        .to_owned();
    Ok(key)
}

/// Accept any server certificate. RDP hosts typically present a self-signed
/// certificate; on a trusted LAN this matches the behaviour of mstsc/FreeRDP
/// after the user clicks through the certificate warning.
mod danger {
    use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use tokio_rustls::rustls::{pki_types, DigitallySignedStruct, Error, SignatureScheme};

    #[derive(Debug)]
    pub(super) struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _: &pki_types::CertificateDer<'_>,
            _: &[pki_types::CertificateDer<'_>],
            _: &pki_types::ServerName<'_>,
            _: &[u8],
            _: pki_types::UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PKCS1_SHA1,
                SignatureScheme::ECDSA_SHA1_Legacy,
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::ECDSA_NISTP521_SHA512,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ED25519,
                SignatureScheme::ED448,
            ]
        }
    }
}
