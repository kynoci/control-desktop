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
use ironrdp::pdu::nego::NegoRequestData;
use ironrdp::pdu::Action;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{fast_path, ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_bulk::BulkCompressor;
use ironrdp_egfx::client::GraphicsPipelineClient;
use ironrdp_tokio::{single_sequence_step_read, FramedWrite as _, TokioFramed};

use crate::clipboard::{advertise_text, LocalClipboard, WebClipboardBackend};
use crate::egfx::EgfxHandler;
use crate::redirect::{parse_redirect_frame, rdstls_authenticate, Redirect};
use crate::ConnectRequest;

// --- Channel wire format (raw bytes, little-endian) ----------------------------
// tag 0 = init:  [0][width u16][height u16]      -> frontend sizes the canvas
// tag 1 = strip: [1][y u16][height u16][rgba..]  -> full-width RGBA strip at row y
// tag 2 = status:[2][utf-8 message]              -> human-readable status / error
// tag 3 = rect:  [3][x][y][w][h][rgba..]         -> arbitrary dirty rect
pub(crate) const TAG_INIT: u8 = 0;
// tag 1 = strip: the legacy path's update format. It widens every dirty rectangle
// to the desktop width, which sends far more bytes than the rectangle needs — but
// the pixels are then already contiguous in `DecodedImage`, so it is one memcpy
// into the message and one `putImageData` on the canvas. The per-rectangle tag-3
// encoding that briefly replaced it sent less data but cost two copies (row by row
// into a buffer, then into the message) and measured *worse* on Windows, which
// tends to emit full-width surface bits anyway. EGFX keeps tag 3, where the
// updates are genuinely small tiles.
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
    // GNOME Remote Login sends a Server Redirection PDU twice — system daemon to
    // login screen, login screen to the user's session — and each one means a
    // fresh connection. The bound only stops a server that redirects forever.
    const MAX_REDIRECTS: u32 = 4;
    let mut redirect: Option<Redirect> = None;
    for hop in 0..=MAX_REDIRECTS {
        let Some((framed, result)) = connect_with_retry(
            &req,
            &password,
            redirect.as_ref(),
            width,
            height,
            &on_frame,
            &clip_local,
            &clip_tx,
            &cancel,
        )
        .await?
        else {
            return Ok(());
        };
        match run_active(framed, result, &on_frame, &mut input_rx, &mut resize_rx, &mut clip_rx, &cancel).await? {
            None => return Ok(()),
            Some(next) => {
                log::info!("rdp: server redirection {} received: {next:?}", hop + 1);
                if next.flags & 0x1 != 0 {
                    // A target address is the server's view of the network; behind
                    // NAT (a router port forward) it is unreachable, so reconnect to
                    // the host and port the user entered, as the routing token
                    // already tells the server which session to hand us.
                    log::info!("rdp: ignoring redirect target address; reconnecting to {}:{}", req.host, req.port);
                }
                let _ = on_frame.send(status_msg("Server redirected the session, reconnecting..."));
                redirect = Some(next);
            }
        }
    }
    Err(format!("server redirected the session more than {MAX_REDIRECTS} times"))
}

/// Connect (or reconnect after a redirect), retrying the post-auth "handoff
/// disconnect". `Ok(None)` means the session was cancelled while connecting.
#[allow(clippy::too_many_arguments)]
async fn connect_with_retry(
    req: &ConnectRequest,
    password: &str,
    redirect: Option<&Redirect>,
    width: u16,
    height: u16,
    on_frame: &Channel<InvokeResponseBody>,
    clip_local: &LocalClipboard,
    clip_tx: &mpsc::UnboundedSender<ClipboardMessage>,
    cancel: &CancellationToken,
) -> Result<
    Option<(
        TokioFramed<tokio_rustls::client::TlsStream<TcpStream>>,
        ConnectionResult,
    )>,
    String,
> {
    // Some servers drop the connection right after authentication
    // (ServerDeactivateAll + an MCS Disconnect Ultimatum) as part of a session
    // handover, expecting the client to reconnect. mstsc does this transparently;
    // mirror it with a bounded retry.
    const MAX_ATTEMPTS: u32 = 6;
    let pair = {
        let mut established = None;
        let mut last_err = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            if cancel.is_cancelled() {
                return Ok(None);
            }
            let clip_backend = WebClipboardBackend::new(clip_local.clone(), clip_tx.clone(), on_frame.clone());
            match establish(req, password, redirect, width, height, clip_backend, on_frame).await {
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
    Ok(Some(pair))
}

/// Drive one connected session until it ends. `Ok(Some(redirect))` means the
/// server asked the client to reconnect elsewhere (Server Redirection).
async fn run_active(
    upgraded_framed: TokioFramed<tokio_rustls::client::TlsStream<TcpStream>>,
    connection_result: ConnectionResult,
    on_frame: &Channel<InvokeResponseBody>,
    input_rx: &mut mpsc::UnboundedReceiver<Vec<Operation>>,
    resize_rx: &mut mpsc::UnboundedReceiver<(u16, u16)>,
    clip_rx: &mut mpsc::UnboundedReceiver<ClipboardMessage>,
    cancel: &CancellationToken,
) -> Result<Option<Redirect>, String> {
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
    // Throughput of the legacy path, summarised every few seconds. Strips are
    // full-width, so a server dirtying a caret and one repainting the desktop look
    // identical from outside — these numbers separate "few large updates" (fine)
    // from "full-screen repaint every frame" (why it would feel heavy).
    let mut stat = LegacyStats::new();
    // Paints waiting to be presented as one frame. The server splits a single
    // screen update across several fast-path PDUs, and `ironrdp-session` gives us
    // one `GraphicsUpdate` per PDU — painting each as it lands is what makes a
    // window open as a top-to-bottom wipe instead of appearing at once, the way
    // mstsc shows it. Adjacent bands are merged and presented together.
    let mut pending_paint: Vec<DirtyRect> = Vec::new();
    let mut paint_deadline: Option<tokio::time::Instant> = None;

    'session: loop {
        let outputs: Vec<ActiveStageOutput> = tokio::select! {
            biased;
            _ = cancel.cancelled() => break 'session,
            // A frame is due. Polled before the reader so a burst of PDUs cannot
            // starve the present and stretch one frame into a visible wipe.
            () = async {
                match paint_deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                flush_paint(&on_frame, &image, &mut pending_paint)?;
                paint_deadline = None;
                Vec::new()
            }
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
                // `ironrdp-session` cannot decode a Server Redirection PDU (it
                // rejects the share control type), so catch it first.
                if action == Action::X224 {
                    match parse_redirect_frame(&payload) {
                        Some(Ok(redirect)) => return Ok(Some(redirect)),
                        Some(Err(e)) => return Err(format!("server redirection failed: {e}")),
                        None => {}
                    }
                }
                active_stage.process(&mut image, action, &payload).map_err(|e| {
                    // The error alone ("decode error") says nothing about what the
                    // server sent; the head of the PDU does.
                    let head = &payload[..payload.len().min(48)];
                    log::warn!("rdp: undecodable {action:?} PDU ({} bytes): {head:02x?}", payload.len());
                    format!("frame decode failed: {e}")
                })?
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
                    stat.record(&rect, image.width());
                    // Widened to the full desktop here rather than at send time, so
                    // that vertically adjacent bands merge into one strip — which is
                    // exactly the shape a fragmented screen update arrives in.
                    pending_paint.push(DirtyRect {
                        x: 0,
                        y: rect.top,
                        w: image.width(),
                        h: rect.bottom.saturating_sub(rect.top).saturating_add(1),
                    });
                    if paint_deadline.is_none() {
                        paint_deadline = Some(tokio::time::Instant::now() + COALESCE_WINDOW);
                    }
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

        stat.report();

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

    Ok(None)
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
///
/// After a Server Redirection, `redirect` carries what the server said to come
/// back with: the routing token always, and either replacement credentials for
/// CredSSP or — when the password is encrypted to the target — an RDSTLS
/// exchange in place of CredSSP.
async fn establish(
    req: &ConnectRequest,
    password: &str,
    redirect: Option<&Redirect>,
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
    // The Graphics Pipeline is mandatory for servers with no legacy bitmap path
    // (gnome-remote-desktop), and optional for Windows, which can stream legacy
    // surface commands that ironrdp-session decodes itself. See `legacy_graphics`.
    let enable_egfx = !req.legacy_graphics;
    let use_rdstls = redirect.is_some_and(Redirect::needs_rdstls);
    let username = redirect
        .and_then(|r| r.username.clone())
        .unwrap_or_else(|| req.username.clone());
    let password = redirect
        .and_then(Redirect::plain_password)
        .unwrap_or_else(|| password.to_owned());
    let mut config = build_config(&username, password, width, height, enable_egfx);
    if let Some(r) = redirect {
        config.request_data = r.routing_token().map(NegoRequestData::routing_token);
        config.redirected_session_id = Some(r.session_id);
        config.domain = r.domain.clone().filter(|d| !d.is_empty());
        if use_rdstls {
            config.enable_credssp = false;
            config.enable_rdstls = true;
        }
    }

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
    let mut drdynvc = DrdynvcClient::new().with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())));
    if enable_egfx {
        drdynvc = drdynvc.with_dynamic_channel(GraphicsPipelineClient::new(
            Box::new(EgfxHandler::new(on_frame.clone())),
            None,
        ));
    } else {
        // Opening the channel while the capability bit is unset would be a protocol
        // lie, and the server would never use it anyway.
        log::info!("rdp: legacy graphics requested — EGFX not advertised for this session");
    }
    let cliprdr: CliprdrClient = Cliprdr::new(clipboard_backend);
    let mut connector = ClientConnector::new(config, client_addr)
        .with_static_channel(drdynvc)
        .with_static_channel(cliprdr);

    let _ = on_frame.send(status_msg("Negotiating RDP protocol..."));
    // A healthy server answers the Connection Request immediately. A wedged
    // gnome-remote-desktop daemon can leave it unanswered forever (the kernel
    // still completes the TCP handshake), which otherwise hangs the tab on this
    // status with no error.
    const NEGOTIATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
    let should_upgrade = tokio::time::timeout(NEGOTIATION_TIMEOUT, ironrdp_tokio::connect_begin(&mut framed, &mut connector))
        .await
        .map_err(|_| {
            format!(
                "the server accepted the connection but did not answer the RDP request within {}s. \
                 Its RDP service is likely stuck — on Ubuntu run `sudo systemctl restart gnome-remote-desktop`; \
                 on Windows restart the Remote Desktop Services service or reboot.",
                NEGOTIATION_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| {
            let e = e.to_string();
            // IronRDP reports a server that hangs up without answering as
            // "read frame by hint … custom error". Seen when gnome-remote-desktop's
            // daemon wedged after an internal assertion: it still accepted TCP but
            // closed every connection unlogged until restarted.
            if e.contains("read frame by hint") {
                format!(
                    "the server accepted the connection but closed it without answering the RDP request. \
                     Its RDP service is likely stuck — on Ubuntu run `sudo systemctl restart gnome-remote-desktop`; \
                     on Windows restart the Remote Desktop Services service or reboot. ({e})"
                )
            } else {
                format!("RDP negotiation failed: {e}")
            }
        })?;

    // The connection must now be wrapped in TLS before CredSSP/NLA continues.
    let _ = on_frame.send(status_msg("Securing connection (TLS)..."));
    let initial_stream = framed.into_inner_no_leftover();
    let (mut tls_stream, server_public_key) = tls_upgrade(initial_stream, &req.host)
        .await
        .map_err(|e| format!("TLS upgrade failed: {e}"))?;

    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    if let Some(r) = redirect.filter(|_| use_rdstls) {
        // RDSTLS replaces CredSSP: the connector skips NLA for this protocol, and
        // the exchange runs on the raw TLS stream before any framed RDP traffic.
        let _ = on_frame.send(status_msg("Handing over to the redirected session (RDSTLS)..."));
        rdstls_authenticate(&mut tls_stream, r).await?;
    }
    let mut upgraded_framed = TokioFramed::new(tls_stream);

    if !use_rdstls {
        let _ = on_frame.send(status_msg(&format!("Signing in as {username} (NLA/CredSSP)...")));
    }
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

/// How long a paint waits for the rest of its frame before being presented.
///
/// The server fragments one screen update across several fast-path PDUs; this is
/// the window in which those pieces are collected so they appear together. Half a
/// 60Hz frame: long enough to catch the fragments of one update, short enough that
/// it is not felt as input lag. Nothing is ever held longer than this.
const COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(8);

/// Present everything collected since the last frame: merge the bands (a run of
/// adjacent full-width bands becomes one strip) and send them.
fn flush_paint(
    on_frame: &Channel<InvokeResponseBody>,
    image: &DecodedImage,
    pending: &mut Vec<DirtyRect>,
) -> Result<(), String> {
    if pending.is_empty() {
        return Ok(());
    }
    for rect in merge_rects(std::mem::take(pending)) {
        send_strip(on_frame, image, rect.y, rect.h)?;
    }
    Ok(())
}

/// Throughput counters for the legacy path, logged at most every `REPORT_EVERY`
/// and only while updates are actually flowing (an idle desktop stays silent).
struct LegacyStats {
    updates: u64,
    /// Rows actually dirtied, before `send_strip` widens them.
    rows: u64,
    /// Bytes put on the channel, i.e. rows widened to the full desktop.
    bytes: u64,
    /// Updates already spanning the full desktop width — nothing to widen, so the
    /// strip encoding costs them nothing.
    full_width: u64,
    since: std::time::Instant,
}

impl LegacyStats {
    const REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

    fn new() -> Self {
        Self {
            updates: 0,
            rows: 0,
            bytes: 0,
            full_width: 0,
            since: std::time::Instant::now(),
        }
    }

    fn record(&mut self, rect: &InclusiveRectangle, image_width: u16) {
        let rows = u64::from(rect.bottom.saturating_sub(rect.top)) + 1;
        let width = u64::from(rect.right.saturating_sub(rect.left)) + 1;
        self.updates += 1;
        self.rows += rows;
        self.bytes += rows * u64::from(image_width) * 4;
        if width >= u64::from(image_width) {
            self.full_width += 1;
        }
    }

    fn report(&mut self) {
        let elapsed = self.since.elapsed();
        if self.updates == 0 || elapsed < Self::REPORT_EVERY {
            return;
        }
        let secs = elapsed.as_secs_f64();
        log::info!(
            "rdp legacy: {:.1} updates/s, {:.1} rows/update, {:.1} MB/s on the wire, {}% already full-width",
            self.updates as f64 / secs,
            self.rows as f64 / self.updates as f64,
            self.bytes as f64 / secs / (1024.0 * 1024.0),
            self.full_width * 100 / self.updates,
        );
        *self = Self::new();
    }
}

/// Send the changed rows as a full-width RGBA strip (tag 1) — the legacy path's
/// update format, used only when EGFX is off.
///
/// Every dirty rectangle is widened to the full image width so the pixel data is
/// already contiguous in `DecodedImage`: one slice, one memcpy into the message,
/// one `putImageData` on the canvas. That costs bandwidth on a narrow update and
/// saves CPU on a wide one, and Windows' legacy surface bits are usually wide.
fn send_strip(
    on_frame: &Channel<InvokeResponseBody>,
    image: &DecodedImage,
    top: u16,
    height: u16,
) -> Result<(), String> {
    if height == 0 || top >= image.height() {
        return Ok(());
    }
    let bottom = top.saturating_add(height - 1).min(image.height().saturating_sub(1));
    let strip_height = bottom - top + 1;
    let full = InclusiveRectangle {
        left: 0,
        top,
        right: image.width().saturating_sub(1),
        bottom,
    };
    let data = image.data_for_rect(&full);

    let mut msg = Vec::with_capacity(5 + data.len());
    msg.push(TAG_STRIP);
    msg.extend_from_slice(&top.to_le_bytes());
    msg.extend_from_slice(&strip_height.to_le_bytes());
    msg.extend_from_slice(data);
    on_frame.send(InvokeResponseBody::Raw(msg)).map_err(|e| e.to_string())
}

/// A dirty rectangle waiting to be sent.
#[derive(Clone, Copy)]
pub(crate) struct DirtyRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// Above this many rectangles in one batch, stop merging pairwise and just send
/// the bounding box — the merge is O(n^2) and a batch this fragmented is usually a
/// full-screen redraw anyway.
const MAX_MERGED_RECTS: usize = 48;

/// Fold dirty rectangles into as few as possible: EGFX paints a 64x64 tile grid, so
/// a row of tiles collapses into one rectangle and a full redraw into a handful.
/// Merges runs that share an edge exactly (same height side by side, or same width
/// stacked), which is the shape a tile grid produces; anything left over is sent as
/// it is rather than over-merged into a bounding box that would repaint clean
/// pixels.
pub(crate) fn merge_rects(mut rects: Vec<DirtyRect>) -> Vec<DirtyRect> {
    if rects.len() <= 1 {
        return rects;
    }

    // A very fragmented frame is a full redraw in disguise; one bounding box beats
    // a quadratic merge over hundreds of rectangles.
    if rects.len() > MAX_MERGED_RECTS {
        return vec![bounding_box(&rects)];
    }

    rects.sort_by_key(|r| (r.y, r.x));

    // Horizontal first (tiles arrive in rows), then vertical over the merged rows.
    let mut merged = merge_pass(rects, true);
    merged = merge_pass(merged, false);
    merged
}

/// One merge pass: `horizontal` joins rectangles that sit side by side with the
/// same top and height, otherwise ones stacked with the same left and width.
fn merge_pass(rects: Vec<DirtyRect>, horizontal: bool) -> Vec<DirtyRect> {
    let mut out: Vec<DirtyRect> = Vec::with_capacity(rects.len());

    for rect in rects {
        let mut merged = false;
        for existing in &mut out {
            let joins = if horizontal {
                existing.y == rect.y && existing.h == rect.h && existing.x + existing.w == rect.x
            } else {
                existing.x == rect.x && existing.w == rect.w && existing.y + existing.h == rect.y
            };
            if joins {
                if horizontal {
                    existing.w += rect.w;
                } else {
                    existing.h += rect.h;
                }
                merged = true;
                break;
            }
        }
        if !merged {
            out.push(rect);
        }
    }

    out
}

/// Smallest rectangle covering all of `rects`.
fn bounding_box(rects: &[DirtyRect]) -> DirtyRect {
    let left = rects.iter().map(|r| r.x).min().unwrap_or(0);
    let top = rects.iter().map(|r| r.y).min().unwrap_or(0);
    let right = rects.iter().map(|r| r.x.saturating_add(r.w)).max().unwrap_or(0);
    let bottom = rects.iter().map(|r| r.y.saturating_add(r.h)).max().unwrap_or(0);
    DirtyRect {
        x: left,
        y: top,
        w: right.saturating_sub(left),
        h: bottom.saturating_sub(top),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Progressive tiles are 64x64; that grid is what the merge exists for.
    const TILE_PX: u16 = 64;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> DirtyRect {
        DirtyRect { x, y, w, h }
    }

    #[test]
    fn a_row_of_tiles_becomes_one_rect() {
        // What a progressive refresh actually produces: 64x64 tiles left to right.
        let tiles = (0..5).map(|i| rect(i * TILE_PX, 0, TILE_PX, TILE_PX)).collect();
        let merged = merge_rects(tiles);
        assert_eq!(merged.len(), 1);
        assert_eq!((merged[0].x, merged[0].y, merged[0].w, merged[0].h), (0, 0, 5 * TILE_PX, TILE_PX));
    }

    #[test]
    fn a_block_of_tiles_becomes_one_rect() {
        let mut tiles = Vec::new();
        for row in 0..3u16 {
            for col in 0..4u16 {
                tiles.push(rect(col * TILE_PX, row * TILE_PX, TILE_PX, TILE_PX));
            }
        }
        let merged = merge_rects(tiles);
        assert_eq!(merged.len(), 1);
        assert_eq!((merged[0].w, merged[0].h), (4 * TILE_PX, 3 * TILE_PX));
    }

    #[test]
    fn stacked_full_width_bands_become_one_strip() {
        // The legacy case: one screen update arrives split across several fast-path
        // PDUs as consecutive full-width bands. Presenting them separately is the
        // top-to-bottom wipe; they have to come out as a single strip.
        const W: u16 = 1000;
        let bands = (0..6).map(|i| rect(0, i * 40, W, 40)).collect();
        let merged = merge_rects(bands);
        assert_eq!(merged.len(), 1);
        assert_eq!((merged[0].y, merged[0].h), (0, 240));
    }

    #[test]
    fn a_gap_between_bands_is_not_bridged() {
        // Two updates far apart vertically must stay apart — merging them would
        // repaint everything between, which is the cost the strip encoding already
        // pays once per band.
        let merged = merge_rects(vec![rect(0, 0, 1000, 40), rect(0, 600, 1000, 40)]);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn far_apart_rects_are_left_alone() {
        // Merging these would repaint the whole screen to update a clock and a
        // cursor, which is exactly what batching is meant to avoid.
        let merged = merge_rects(vec![rect(0, 0, 16, 16), rect(900, 600, 16, 16)]);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn a_very_fragmented_frame_collapses_to_its_bounds() {
        let many = (0..=MAX_MERGED_RECTS as u16)
            .map(|i| rect(i * 3, i * 5, 2, 2))
            .collect::<Vec<_>>();
        let last = *many.last().expect("non-empty");
        let merged = merge_rects(many);
        assert_eq!(merged.len(), 1);
        assert_eq!((merged[0].x, merged[0].y), (0, 0));
        assert_eq!(merged[0].w, last.x + last.w);
        assert_eq!(merged[0].h, last.y + last.h);
    }
}

fn build_config(username: &str, password: String, width: u16, height: u16, enable_egfx: bool) -> connector::Config {
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
        // Advertising EGFX makes a Windows server abandon the legacy path entirely,
        // so a connection that asked for legacy graphics must not claim it.
        enable_egfx,
        // Both set by `establish` only when reconnecting after a Server Redirection.
        enable_rdstls: false,
        redirected_session_id: None,
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
