# Plan: Add VNC (RFB) protocol support

Goal: let users connect to a host over **VNC** the same way they already do over RDP — click a saved connection, get a live remote desktop in a tab, with mouse + keyboard, password stored in the OS keychain. The embedded RDP client (`src-tauri/src/rdp.rs`) is the template; VNC should be a parallel module that reuses as much of the existing plumbing as possible.

---

## 1. How the existing RDP path works (anchor)

Understanding what to mirror:

- **`src-tauri/src/rdp.rs`** — pure-Rust client running in a background tokio task. Per-session state lives in `RdpState` (`Mutex<HashMap<u32, SessionHandle>>`, managed by Tauri). Each `SessionHandle` holds input/resize mpsc senders + a `CancellationToken`. Commands: `rdp_connect`, `rdp_input`, `rdp_resize`, `rdp_disconnect`.
- **Frame streaming** is a raw little-endian binary protocol over a Tauri `Channel<InvokeResponseBody>` (see top of `rdp.rs` and `handleFrame` in `App.vue`):
  - `0` init `[0][w u16][h u16]` → size the canvas
  - `1` strip `[1][y u16][h u16][rgba…]` → full-width RGBA row strip via `putImageData`
  - `2` status `[2][utf-8]` → human-readable status/error on the tab
- **Input** flows back through `rdp_input` as JSON-tagged events (`move`/`button`/`wheel`/`key`). Keyboard uses **set-1 scancodes** mapped from `KeyboardEvent.code` in `src/scancodes.ts`.
- **Frontend** (`App.vue`): tab-per-session model, `runtimes` `Map` kept out of Vue reactivity, `connectBuiltin()` always calls `rdp_connect`. The connection form has a protocol `<select>` currently offering only RDP.
- **Credentials**: `save_password`/`get_password`/`delete_password` (in `lib.rs`) store the password in the OS keychain keyed by `conn_<id>`. The DB (`connections` table) already has a `protocol` column defaulting to `'rdp'`.

The frame wire format and the keychain/DB layer are **protocol-agnostic** — VNC can reuse them directly. The main new work is the RFB client, a keysym keyboard map, and protocol dispatch.

---

## 2. Design overview

Add a sibling module **`src-tauri/src/vnc.rs`** mirroring `rdp.rs`:

- Its own `VncState` (or generalize to a shared `SessionState`) and commands `vnc_connect`, `vnc_input`, `vnc_disconnect` (+ optional `vnc_resize`).
- Reuse the **same `Channel` binary frame protocol** (tags 0/1/2) so the frontend canvas/rendering code is shared. Add one new tag (`3` rect) for VNC's arbitrary dirty rectangles — see §5.
- Reuse `crate::get_password` for the stored VNC password.
- Frontend dispatches on `connection.protocol` in `connectBuiltin()`: `rdp` → `rdp_connect`, `vnc` → `vnc_connect`.

Key protocol differences from RDP that drive the work:

| Concern | RDP (today) | VNC (new) |
|---|---|---|
| Default port | 3389 | **5900** (display N = 5900 + N) |
| Auth | NLA/CredSSP, NTLM, username+password | **VNC Auth** (DES challenge, password only) or None; username usually ignored |
| Keyboard | set-1 scancodes | **X11 keysyms** |
| Pointer | absolute move + button ops | absolute x/y + **button bitmask** |
| Framebuffer | full-width strips | **arbitrary rectangles**, multiple encodings |
| Resize | Display Control channel | server-fixed; client resize only via optional SetDesktopSize ext |
| Transport | TLS + CredSSP always | plain TCP (classic); TLS only via VeNCrypt (defer) |

---

## 3. Rust VNC library selection

We want a **pure-Rust** RFB client to keep the "no C/NASM toolchain" property (the reason `rustls` uses the `ring` backend). Candidates to evaluate, in order of preference:

1. **`vnc` (vnc-rs, HsuJv/vnc-rs)** — async, tokio-based, RFB client with built-in decoders (Raw, CopyRect, Tight, Zrle) and VncAuth. Closest fit to the tokio session model already used. **Recommended primary**, pending an API spike.
2. **`rust-vnc` (whitequark/rust-vnc)** — sync, lower-level RFB protocol types. Solid for handshake/messages but we'd implement the update loop and some decoders ourselves; would need a `spawn_blocking` bridge or manual async.
3. **Hand-rolled RFB** on top of the existing `tokio` stack — most control, most work. Fallback only if no crate fits.

**Action:** before committing, do a ~half-day spike — add the crate, connect to a local TigerVNC server, confirm it: (a) builds with no C deps, (b) exposes framebuffer rects as raw pixels we can convert to RGBA, (c) supports VNC Authentication, (d) lets us drive the update/event loop from our own tokio task. Record findings here. If decoders are missing (e.g. Tight needs zlib — `flate2` is already a transitive dep), note which encodings we'll request from the server.

Risk: some crates assume they own the event loop / render to their own window. We need a *headless* client that hands us pixel rects. Verify this in the spike — it's the make-or-break property.

---

## 4. Backend: `vnc.rs` structure

Mirror `rdp.rs`:

```
pub struct VncState { sessions: Mutex<HashMap<u32, SessionHandle>> }   // or share with RDP
struct SessionHandle { input_tx, resize_tx, cancel }                    // same shape as RDP

#[tauri::command] pub async fn vnc_connect(session_id, req: ConnectRequest, width, height, on_frame, state, app)
#[tauri::command] pub fn vnc_input(session_id, event: VncInputEvent, state)
#[tauri::command] pub fn vnc_disconnect(session_id, state)
#[tauri::command] pub fn vnc_resize(session_id, w, h, state)            // optional / best-effort
```

**Connect flow (`run_session` equivalent):**
1. `get_password(req.id)` for the VNC password (may be empty if server uses None auth).
2. TCP connect to `host:port`.
3. RFB handshake: negotiate ProtocolVersion → security type. Support **VNC Authentication** (DES-encrypt the 16-byte challenge with the password) and **None**. If the server only offers VeNCrypt/other, fail with a clear message (see §8).
4. `ClientInit` (shared-desktop = true so we don't kick existing viewers).
5. Read `ServerInit` → framebuffer width/height, pixel format, desktop name. Send `init` frame (tag 0) to the frontend.
6. Set our preferred **pixel format** to 32bpp true-color matching the canvas (RGBA/BGRA — pick to minimize per-pixel swizzling) and an encodings list (CopyRect, Tight or ZRLE, Raw as fallback; plus DesktopSize pseudo-encoding if we attempt resize).
7. Maintain a local framebuffer `Vec<u8>` (w*h*4 RGBA).
8. Loop with `tokio::select!` like RDP:
   - `cancel.cancelled()` → break
   - `input_rx.recv()` → translate to RFB `PointerEvent`/`KeyEvent`, write to socket
   - `resize_rx.recv()` → best-effort SetDesktopSize (or ignore)
   - drive the VNC client: on `FramebufferUpdate`, decode each rect into the local framebuffer, then emit a **rect frame** (tag 3) per dirty rect; request the next incremental update (`FramebufferUpdateRequest incremental=1`).
9. On exit, remove from `sessions`, send a status frame (tag 2), same as RDP.

**Pixel conversion:** convert the server's pixel format to RGBA once per rect (or rely on the crate if it can deliver RGBA directly). Keep it simple first (correctness over speed); optimize later.

**Continuous updates:** unlike RDP's server-push fast-path, VNC is request-driven — after each `FramebufferUpdate` we must send another `FramebufferUpdateRequest`. Make sure the loop always re-arms a pending incremental request so the screen keeps refreshing.

---

## 5. Wire protocol change (frontend ↔ backend)

VNC dirty regions are arbitrary rectangles, not full-width strips. Add one tag rather than widening every rect to full width (which wastes bandwidth on large desktops):

- **`3` rect** `[3][x u16][y u16][w u16][h u16][rgba…]` → `ctx.putImageData(new ImageData(rgba, w, h), x, y)`.

Frontend `handleFrame` gains a `tag === 3` branch. RDP keeps using tag 1 unchanged. (Alternative: have VNC reuse tag 1 by blitting into a full-width strip — zero frontend change but more data per update. Recommend tag 3.)

Keep tags 0 (init) and 2 (status) shared as-is.

---

## 6. Backend: input mapping

New `VncInputEvent` enum (JSON-tagged, like `RdpInputEvent`):
- `Move { x, y }`, `Button { button, pressed }`, `Wheel { vertical, delta }` — same shape as RDP; backend maintains a **button bitmask** (RFB `PointerEvent` sends current mask + position; wheel = button 4/5/6/7 press+release).
- `Key { keysym: u32, pressed: bool }` — RFB `KeyEvent` takes an X11 keysym, **not** a scancode.

So the frontend must send keysyms for VNC. Add **`src/keysyms.ts`** mapping `KeyboardEvent.code`/`.key` → X11 keysym (mirrors `scancodes.ts`):
- Printable ASCII: keysym == char code for most Latin-1.
- Special keys: `Enter`→0xFF0D, `Backspace`→0xFF08, `Tab`→0xFF09, `Escape`→0xFF1B, arrows 0xFF51–54, F-keys 0xFFBE+, modifiers (Shift_L 0xFFE1, Control_L 0xFFE3, Alt_L 0xFFE9, Super_L 0xFFEB), etc.
- Handle shifted symbols using `e.key` when it's a single printable char (so Shift+2 sends `@`), falling back to the `e.code` table otherwise.

`onKey` in `App.vue` branches by the active session's protocol: RDP path sends `{kind:'key', scancode}` from `SCANCODES`; VNC path sends `{kind:'key', keysym}` from `KEYSYMS`.

---

## 7. Frontend changes (`App.vue`)

1. **Protocol dropdown**: add `<option value="vnc">VNC</option>` to the form `<select>`.
2. **Default port**: when protocol switches to `vnc`, default `draft.port` to 5900 (only if the user hasn't changed it / it's still the RDP default). Small watcher or on-change handler.
3. **Dispatch in `connectBuiltin(c)`**: `if (c.protocol === 'vnc') invoke('vnc_connect', …) else invoke('rdp_connect', …)`. Same args shape (`sessionId`, `req`, `width`, `height`, `onFrame` channel).
4. **`handleFrame`**: add the tag-3 rect branch (§5).
5. **`onKey`**: protocol-aware keysym vs scancode (§6).
6. **Mouse**: `onMouseMove`/`onMouseButton`/`onWheel` payloads are already protocol-neutral (`move`/`button`/`wheel`); they can be routed to `vnc_input` unchanged. Add a tiny `sendInput` that targets the right command based on the session's protocol — track `protocol` in the `SessionMeta`/`Runtime` so input/disconnect calls hit `vnc_*` vs `rdp_*`.
7. **`closeTab`/disconnect**: call `vnc_disconnect` or `rdp_disconnect` per protocol.
8. The **Windows-RDP fallback button (🖥)** stays RDP-only — hide it for VNC connections.
9. Thumbnails, tabs, resize debounce: unchanged (resize just calls `vnc_resize`, which is best-effort).

---

## 8. Authentication & security

- **v1 supported:** VNC Authentication (DES challenge-response) and None. Username is collected but unused for classic VNC (keep the field; some servers ignore it).
- **Password storage:** unchanged — reuse the keychain. Empty password allowed for None-auth servers.
- **VeNCrypt / TLS / RA2 (RealVNC):** **out of scope for v1.** Detect and fail with an actionable message ("This server requires VeNCrypt/TLS, not yet supported — use an RDP connection or enable VNC Auth on the server"). Mirrors the clear-error approach `rdp.rs` already uses (e.g. the Kerberos and GNOME-handoff messages).
- **DES quirk:** VNC Auth uses DES with **bit-reversed key bytes** — easy to get wrong. If the chosen crate handles auth internally, great; if we implement it, unit-test against a known challenge/response vector. (`des` crate is already in the dependency tree transitively.)
- No TLS cert handling needed for classic VNC (plain TCP). If/when VeNCrypt lands, reuse the `rustls` + `NoCertificateVerification` pattern from `rdp.rs`.

---

## 9. Registration & capabilities

- In `lib.rs` `run()`: `.manage(vnc::VncState::default())` and add `vnc::vnc_connect, vnc::vnc_input, vnc::vnc_disconnect, vnc::vnc_resize` to `tauri::generate_handler![…]`. Add `mod vnc;`.
- `src-tauri/capabilities/default.json`: the existing `core:default` + the `core:event`/IPC permissions cover Channel-based commands the same way RDP works — **verify** no extra permission is needed (RDP commands aren't individually listed there, so likely nothing to add). Confirm during implementation.
- DB: no migration needed — `protocol` column already exists. New rows just store `'vnc'`.
- `Cargo.toml`: add the chosen VNC crate (and `des`/`flate2` only if hand-rolling). Keep the `[patch.crates-io]` ironrdp note untouched.

---

## 10. Implementation milestones

1. **Spike (½ day):** pick the crate, prove headless connect + pixel rects + VNC Auth against a local server. Update §3.
2. **Backend skeleton:** `vnc.rs` with `VncState`, the four commands, registration in `lib.rs`. Stub `vnc_connect` that connects, reads `ServerInit`, sends an `init` frame + a status message. Verify a tab opens and shows the negotiated size.
3. **Framebuffer rendering:** decode updates → local framebuffer → tag-3 rect frames; loop re-requests incremental updates. Frontend tag-3 branch. Goal: see the live remote screen (read-only).
4. **Pointer input:** button bitmask + move + wheel. Goal: mouse works.
5. **Keyboard input:** `keysyms.ts` + protocol-aware `onKey` + backend `KeyEvent`. Goal: typing works, including modifiers and shifted symbols.
6. **Form/UX:** protocol dropdown, default port 5900, hide the Windows-RDP button for VNC, protocol-aware input/disconnect routing.
7. **Resize (best-effort):** SetDesktopSize if the server advertises it; otherwise letterbox the fixed framebuffer (canvas CSS already scales).
8. **Hardening:** error messages for unsupported auth, connection-lost handling, cancel/reconnect parity with RDP.
9. **Docs:** update `CLAUDE.md` (two embedded clients now) and `README.md` (protocol table, VNC notes).

---

## 11. Testing

- **Servers to test against:** TigerVNC (`vncserver`), TightVNC, `x11vnc` (attach to a real X session), and macOS Screen Sharing (ARD) if available. Use a couple of encodings (force Raw, then Tight/ZRLE) to exercise decoders.
- **Auth matrix:** None, VNC Auth (correct + wrong password → clear error), and a VeNCrypt-only server (expect the actionable "not supported" message).
- **Manual checks:** live screen updates under motion (drag a window), mouse drag/right-click/scroll, keyboard incl. Shift/Ctrl/Alt and arrows, tab open/close, reconnect on same tab, multiple simultaneous VNC + RDP tabs.
- **Unit test:** DES challenge-response vector if we implement VNC Auth ourselves.
- No automated UI tests exist in the repo; gate on `npm run build` (type-check) + `cargo build`, then manual `npm run tauri dev`.

---

## 12. Risks & open questions

- **Crate fit** is the biggest unknown — must be headless and expose raw pixels. Mitigated by the spike (§3). Fallback: hand-roll RFB on the existing tokio stack (handshake + Raw/CopyRect/ZRLE is tractable; Tight is the hard one).
- **Performance:** Raw encoding on a large desktop is bandwidth-heavy; per-rect RGBA conversion + Channel send must stay off the UI thread (it already is — background tokio task). Optimize pixel conversion only if needed.
- **Keysym coverage:** non-US layouts and dead keys are fiddly; v1 targets US-QWERTY parity with the RDP scancode table, expand later.
- **Resize:** many servers don't support SetDesktopSize; accept fixed-size + scaled canvas as the baseline.
- **VeNCrypt/TLS demand:** common on hardened servers; if users hit the "not supported" path often, prioritize it as the first follow-up.

---

## 13. Out of scope (v1)

- VeNCrypt / TLS / RealVNC RA2 encryption.
- Clipboard sync, file transfer, audio.
- Client-driven resolution changes beyond best-effort SetDesktopSize.
- Non-US keyboard layouts / IME.

These become the VNC follow-up backlog once the core read/write session works.
