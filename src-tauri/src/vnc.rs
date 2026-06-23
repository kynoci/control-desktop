//! Embedded VNC (RFB) client built on the pure-Rust `vnc-rs` crate.
//!
//! Sibling to `rdp.rs`: it speaks RFB itself in a background tokio task, receives
//! the remote framebuffer as pixel rectangles, and streams them to the webview
//! `<canvas>` over the same Tauri `Channel` wire protocol. Mouse/keyboard events
//! flow back through `vnc_input`.
//!
//! See `plan-vnc.md` §3 for the spike that validated `vnc-rs` 0.5.3:
//! - We request `PixelFormat::rgba()`, so rect bytes arrive as `[r,g,b,a]` in
//!   network order — directly usable by `putImageData`, no swizzling.
//! - We request `Zrle + CopyRect + Raw` (no `Tight`, which would yield JPEG
//!   sub-rects needing a separate decode path). `DesktopSizePseudo` lets the
//!   server tell us about resolution changes.
//! - The client runs its own internal net/decode tasks; `poll_event()` is a
//!   non-blocking drain, so a single `tokio::select!` drives input + frames,
//!   mirroring `rdp.rs`'s `run_session`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager as _, State};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use vnc::{
    ClientKeyEvent, ClientMouseEvent, PixelFormat, VncConnector, VncEncoding, VncError, VncEvent,
    X11Event,
};

use crate::ConnectRequest;

// --- Channel wire format (raw bytes, little-endian) ----------------------------
// Shared with rdp.rs (tags 0/2). VNC adds tag 3 for arbitrary dirty rectangles,
// since RFB updates are not full-width strips like RDP. Keep in sync with
// `handleFrame` in App.vue.
// tag 0 = init:  [0][width u16][height u16]               -> frontend sizes the canvas
// tag 2 = status:[2][utf-8 message]                       -> human-readable status / error
// tag 3 = rect:  [3][x u16][y u16][w u16][h u16][rgba..]  -> ctx.putImageData at (x, y)
const TAG_INIT: u8 = 0;
const TAG_STATUS: u8 = 2;
const TAG_RECT: u8 = 3;

fn status_msg(text: &str) -> InvokeResponseBody {
    let mut v = Vec::with_capacity(1 + text.len());
    v.push(TAG_STATUS);
    v.extend_from_slice(text.as_bytes());
    InvokeResponseBody::Raw(v)
}

/// Per-app state holding every active VNC session, keyed by a frontend-assigned id
/// (one per tab). Mirrors `RdpState`; both can be active at once.
#[derive(Default)]
pub struct VncState {
    sessions: Mutex<HashMap<u32, SessionHandle>>,
}

struct SessionHandle {
    input_tx: mpsc::UnboundedSender<VncInputEvent>,
    cancel: CancellationToken,
}

/// Input event coming from the webview canvas. Same shape as `RdpInputEvent`
/// except keyboard carries an X11 keysym instead of a set-1 scancode.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum VncInputEvent {
    /// Absolute pointer position in remote-desktop pixels.
    Move { x: u16, y: u16 },
    /// Mouse button using the web `MouseEvent.button` numbering (0=left,1=mid,2=right).
    Button { button: u8, pressed: bool },
    /// Mouse wheel; `delta` in wheel units (positive = up/left).
    Wheel { vertical: bool, delta: i16 },
    /// Keyboard key as an X11 keysym (see `src/keysyms.ts`).
    Key { keysym: u32, pressed: bool },
}

// RFB pointer button bitmask bits (RFC 6143 §7.5.5).
const BTN_LEFT: u8 = 1 << 0;
const BTN_MIDDLE: u8 = 1 << 1;
const BTN_RIGHT: u8 = 1 << 2;
const BTN_WHEEL_UP: u8 = 1 << 3;
const BTN_WHEEL_DOWN: u8 = 1 << 4;
const BTN_WHEEL_LEFT: u8 = 1 << 5;
const BTN_WHEEL_RIGHT: u8 = 1 << 6;

/// Tracks pointer position + held-button bitmask so each RFB `PointerEvent`
/// carries the full current state (RFB has no relative/button-delta events).
#[derive(Default)]
struct Pointer {
    x: u16,
    y: u16,
    buttons: u8,
}

impl Pointer {
    fn pointer_event(&self) -> X11Event {
        X11Event::PointerEvent(ClientMouseEvent {
            position_x: self.x,
            position_y: self.y,
            bottons: self.buttons,
        })
    }
}

fn web_button_bit(button: u8) -> Option<u8> {
    match button {
        0 => Some(BTN_LEFT),
        1 => Some(BTN_MIDDLE),
        2 => Some(BTN_RIGHT),
        _ => None,
    }
}

/// Translate a webview input event into RFB `X11Event`s, updating pointer state.
/// A wheel tick becomes a press+release of the corresponding wheel "button".
fn to_x11(ev: VncInputEvent, ptr: &mut Pointer) -> Vec<X11Event> {
    match ev {
        VncInputEvent::Move { x, y } => {
            ptr.x = x;
            ptr.y = y;
            vec![ptr.pointer_event()]
        }
        VncInputEvent::Button { button, pressed } => match web_button_bit(button) {
            Some(bit) => {
                if pressed {
                    ptr.buttons |= bit;
                } else {
                    ptr.buttons &= !bit;
                }
                vec![ptr.pointer_event()]
            }
            None => vec![],
        },
        VncInputEvent::Wheel { vertical, delta } => {
            let bit = match (vertical, delta >= 0) {
                (true, true) => BTN_WHEEL_UP,
                (true, false) => BTN_WHEEL_DOWN,
                (false, true) => BTN_WHEEL_RIGHT,
                (false, false) => BTN_WHEEL_LEFT,
            };
            // Press then release at the current position.
            let down = X11Event::PointerEvent(ClientMouseEvent {
                position_x: ptr.x,
                position_y: ptr.y,
                bottons: ptr.buttons | bit,
            });
            vec![down, ptr.pointer_event()]
        }
        VncInputEvent::Key { keysym, pressed } => {
            vec![X11Event::KeyEvent(ClientKeyEvent {
                keycode: keysym,
                down: pressed,
            })]
        }
    }
}

// --- Tauri commands ------------------------------------------------------------

#[tauri::command]
pub async fn vnc_connect(
    session_id: u32,
    req: ConnectRequest,
    _width: u16,
    _height: u16,
    on_frame: Channel<InvokeResponseBody>,
    state: State<'_, VncState>,
    app: AppHandle,
) -> Result<(), String> {
    // Empty password is valid for None-auth servers; VNC Auth ignores the username.
    let password = crate::get_password(req.id)?.unwrap_or_default();

    log::info!(
        "vnc_connect: session={} conn_id={} host={} port={}",
        session_id, req.id, req.host, req.port
    );

    let cancel = CancellationToken::new();
    let (input_tx, input_rx) = mpsc::unbounded_channel::<VncInputEvent>();
    {
        let mut guard = state.sessions.lock().unwrap();
        if let Some(old) = guard.insert(
            session_id,
            SessionHandle {
                input_tx,
                cancel: cancel.clone(),
            },
        ) {
            old.cancel.cancel();
        }
    }

    tauri::async_runtime::spawn(async move {
        let frame = on_frame.clone();
        let outcome = run_session(req, password, on_frame, input_rx, cancel).await;
        if let Some(state) = app.try_state::<VncState>() {
            state.sessions.lock().unwrap().remove(&session_id);
        }
        match outcome {
            Err(e) => {
                log::error!("vnc session {session_id} ended with error: {e}");
                let _ = frame.send(status_msg(&format!("Disconnected: {e}")));
            }
            Ok(()) => {
                let _ = frame.send(status_msg("Session closed"));
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub fn vnc_input(session_id: u32, event: VncInputEvent, state: State<'_, VncState>) -> Result<(), String> {
    let guard = state.sessions.lock().unwrap();
    if let Some(session) = guard.get(&session_id) {
        let _ = session.input_tx.send(event);
    }
    Ok(())
}

/// Best-effort only: `vnc-rs` 0.5.3 exposes no client-initiated resize
/// (`SetDesktopSize`). The server's framebuffer size is fixed; the frontend
/// scales the canvas via CSS. Kept so the frontend can call it uniformly.
#[tauri::command]
pub fn vnc_resize(session_id: u32, width: u16, height: u16, _state: State<'_, VncState>) -> Result<(), String> {
    log::debug!("vnc_resize: session={session_id} {width}x{height} (no-op; server-fixed size)");
    Ok(())
}

#[tauri::command]
pub fn vnc_disconnect(session_id: u32, state: State<'_, VncState>) -> Result<(), String> {
    if let Some(session) = state.sessions.lock().unwrap().remove(&session_id) {
        session.cancel.cancel();
    }
    Ok(())
}

// --- Session task --------------------------------------------------------------

/// Frame interval for polling decoded events and re-arming the incremental
/// framebuffer-update request. RFB is request-driven (the server only sends an
/// update after a `FramebufferUpdateRequest`), so we re-request on a timer.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

async fn run_session(
    req: ConnectRequest,
    password: String,
    on_frame: Channel<InvokeResponseBody>,
    mut input_rx: mpsc::UnboundedReceiver<VncInputEvent>,
    cancel: CancellationToken,
) -> Result<(), String> {
    let server_addr = format!("{}:{}", req.host, req.port);
    let tcp = TcpStream::connect(&server_addr)
        .await
        .map_err(|e| format!("TCP connect to {server_addr} failed: {e}"))?;

    if cancel.is_cancelled() {
        return Ok(());
    }

    let _ = on_frame.send(status_msg("Authenticating..."));

    let client = VncConnector::new(tcp)
        .set_auth_method(async move { Ok(password) })
        // Request order = preference. ZRLE for compression (pure-Rust flate2),
        // CopyRect for scroll/move, Raw as the universal fallback. No Tight (JPEG).
        .add_encoding(VncEncoding::Zrle)
        .add_encoding(VncEncoding::CopyRect)
        .add_encoding(VncEncoding::Raw)
        .add_encoding(VncEncoding::DesktopSizePseudo)
        .allow_shared(true)
        .set_pixel_format(PixelFormat::rgba())
        .build()
        .map_err(map_connect_err)?
        .try_start()
        .await
        .map_err(map_connect_err)?
        .finish()
        .map_err(map_connect_err)?;

    log::info!("vnc_connect: connected + authenticated to {server_addr}");

    // Local RGBA framebuffer, needed to service CopyRect (which references
    // previously-drawn pixels). Sized on the first SetResolution event.
    let mut fb: Vec<u8> = Vec::new();
    let mut fb_w: u16 = 0;
    let mut fb_h: u16 = 0;
    let mut ptr = Pointer::default();

    let mut tick = tokio::time::interval(FRAME_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Kick off the first (full) framebuffer update.
    client
        .input(X11Event::FullRefresh)
        .await
        .map_err(|e| format!("initial refresh failed: {e}"))?;

    'session: loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break 'session,
            maybe = input_rx.recv() => {
                let Some(ev) = maybe else { break 'session };
                for x11 in to_x11(ev, &mut ptr) {
                    if let Err(e) = client.input(x11).await {
                        return Err(format!("input send failed: {e}"));
                    }
                }
            }
            _ = tick.tick() => {
                // Drain everything the decode task has produced so far.
                loop {
                    match client.poll_event().await {
                        Ok(Some(event)) => {
                            handle_event(event, &on_frame, &mut fb, &mut fb_w, &mut fb_h)?;
                        }
                        Ok(None) => break,
                        Err(e) => return Err(format!("connection lost: {e}")),
                    }
                }
                // Re-arm an incremental update so the screen keeps refreshing.
                if let Err(e) = client.input(X11Event::Refresh).await {
                    return Err(format!("refresh request failed: {e}"));
                }
            }
        }
    }

    let _ = client.close().await;
    Ok(())
}

/// Apply one decoded VNC event: update the local framebuffer and forward the
/// affected pixels to the frontend.
fn handle_event(
    event: VncEvent,
    on_frame: &Channel<InvokeResponseBody>,
    fb: &mut Vec<u8>,
    fb_w: &mut u16,
    fb_h: &mut u16,
) -> Result<(), String> {
    match event {
        VncEvent::SetResolution(screen) => {
            *fb_w = screen.width;
            *fb_h = screen.height;
            fb.clear();
            fb.resize(screen.width as usize * screen.height as usize * 4, 0);
            send_init(on_frame, screen.width, screen.height)?;
            log::info!("vnc: framebuffer {}x{}", screen.width, screen.height);
        }
        VncEvent::RawImage(rect, data) => {
            blit(fb, *fb_w, *fb_h, rect.x, rect.y, rect.width, rect.height, &data);
            send_rect(on_frame, rect.x, rect.y, rect.width, rect.height, &data)?;
        }
        VncEvent::Copy(dst, src) => {
            // Copy within our framebuffer, then push the destination region.
            if let Some(region) = copy_within_fb(fb, *fb_w, *fb_h, dst, src) {
                send_rect(on_frame, dst.x, dst.y, dst.width, dst.height, &region)?;
            }
        }
        VncEvent::SetCursor(_rect, _data) => {
            // Local OS cursor is shown over the canvas; ignore the remote sprite.
        }
        VncEvent::JpegImage(..) => {
            // Only possible with Tight, which we don't request. Ignore defensively.
            log::warn!("vnc: unexpected JpegImage event (Tight not requested)");
        }
        VncEvent::SetPixelFormat(_) => {}
        VncEvent::Bell => {}
        VncEvent::Text(_) => {} // clipboard sync is out of scope for v1
        VncEvent::Error(e) => return Err(format!("server error: {e}")),
        _ => {}
    }
    Ok(())
}

fn send_init(on_frame: &Channel<InvokeResponseBody>, width: u16, height: u16) -> Result<(), String> {
    let mut msg = Vec::with_capacity(5);
    msg.push(TAG_INIT);
    msg.extend_from_slice(&width.to_le_bytes());
    msg.extend_from_slice(&height.to_le_bytes());
    on_frame.send(InvokeResponseBody::Raw(msg)).map_err(|e| e.to_string())
}

fn send_rect(
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

/// Write a tightly-packed RGBA rect into the local framebuffer at (x, y).
/// Out-of-bounds rows/pixels are clipped defensively.
fn blit(fb: &mut [u8], fb_w: u16, fb_h: u16, x: u16, y: u16, w: u16, h: u16, data: &[u8]) {
    if fb_w == 0 || fb.is_empty() {
        return;
    }
    let fb_w = fb_w as usize;
    let fb_h = fb_h as usize;
    let (x, y, w, h) = (x as usize, y as usize, w as usize, h as usize);
    for row in 0..h {
        let dy = y + row;
        if dy >= fb_h {
            break;
        }
        let cols = w.min(fb_w.saturating_sub(x));
        let src = &data[row * w * 4..row * w * 4 + cols * 4];
        let dst_start = (dy * fb_w + x) * 4;
        fb[dst_start..dst_start + cols * 4].copy_from_slice(src);
    }
}

/// Apply a CopyRect inside the framebuffer and return the destination region's
/// pixels (tightly packed RGBA) for forwarding. Rows are copied top-down or
/// bottom-up depending on overlap direction to avoid clobbering source pixels.
fn copy_within_fb(
    fb: &mut [u8],
    fb_w: u16,
    fb_h: u16,
    dst: vnc::Rect,
    src: vnc::Rect,
) -> Option<Vec<u8>> {
    if fb_w == 0 || fb.is_empty() {
        return None;
    }
    let fb_w = fb_w as usize;
    let fb_h = fb_h as usize;
    let w = dst.width as usize;
    let h = dst.height as usize;
    let (sx, sy) = (src.x as usize, src.y as usize);
    let (dx, dy) = (dst.x as usize, dst.y as usize);

    // Clip so we never index past the framebuffer.
    if sx + w > fb_w || dx + w > fb_w || sy + h > fb_h || dy + h > fb_h {
        return None;
    }

    let mut out = vec![0u8; w * h * 4];
    // Stage the source region first (handles overlapping src/dst correctly).
    for row in 0..h {
        let s = ((sy + row) * fb_w + sx) * 4;
        out[row * w * 4..(row + 1) * w * 4].copy_from_slice(&fb[s..s + w * 4]);
    }
    for row in 0..h {
        let d = ((dy + row) * fb_w + dx) * 4;
        fb[d..d + w * 4].copy_from_slice(&out[row * w * 4..(row + 1) * w * 4]);
    }
    Some(out)
}

/// Map a `vnc-rs` connect/auth error to an actionable user-facing message.
fn map_connect_err(e: VncError) -> String {
    match e {
        VncError::WrongPassword => "Authentication failed: wrong VNC password.".to_string(),
        VncError::NoPassword => {
            "This server requires a password. Save one for this connection and retry.".to_string()
        }
        // The crate only implements None + VNC Auth. Anything else (VeNCrypt/TLS,
        // RealVNC RA2, Tight security, …) surfaces as this General message.
        VncError::General(msg) if msg.contains("Security type apart from Vnc Auth") => {
            "This VNC server requires an unsupported security type (e.g. VeNCrypt/TLS or RealVNC \
             encryption), which is not supported yet. Enable standard VNC Authentication on the \
             server, or use an RDP connection."
                .to_string()
        }
        other => format!("VNC connection failed: {other}"),
    }
}
