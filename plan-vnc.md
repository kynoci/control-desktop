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

### Spike findings (2026-06-23) — ✅ `vnc-rs` confirmed as primary

Spiked against vnc-rs **0.5.3** (crates.io package name **`vnc-rs`**, lib name `vnc`, by HsuJv — *not* the unrelated `vnc` 0.4.0 on crates.io). Built a headless test binary + a minimal mock RFB 3.8 server (no live VNC server was available on this machine). All four make-or-break properties pass — at runtime, not just on paper:

- **(a) No C toolchain ✅** — `cargo build` is clean on Windows (rustc 1.94). The only `-sys` crates in the tree are `windows-sys` (pure FFI bindings, already pulled by tokio) and `miniz_oxide` (pure-Rust; flate2's default backend). **Do not enable flate2's `zlib` feature** or it pulls C. DES for VNC Auth is implemented *in-crate* (`src/client/security/des.rs`) — no `des` dep, and the §8 "DES quirk" worry is moot (the crate handles it).
- **(b) Raw pixel rects ✅** — `poll_event()` yields `VncEvent::RawImage(Rect{x,y,width,height: u16}, Vec<u8>)`. With `set_pixel_format(PixelFormat::rgba())` the bytes arrive as `[r,g,b,a]` in network order (**verified: a red pixel came back `[255,0,0,255]`**). This maps 1:1 to the planned wire tag-3 `[3][x][y][w][h][rgba…]` + `putImageData` — **zero per-pixel swizzling**. (Simplifies §4 "pixel conversion" and §5.)
- **(c) VNC Auth ✅** — `set_auth_method(async move { Ok(password) })`. Supports **None + VncAuth only**. Wrong password → `VncError::WrongPassword`; no callback set → `VncError::NoPassword`. Any other security type (VeNCrypt=19, RA2, Tls, Tight-security, etc.) fails with `VncError::General("Security type apart from Vnc Auth has not been implemented")` — match on that for the actionable §8 message. Note: if a server offers *both* None and VncAuth, the crate picks None.
- **(d) Caller-driven loop ✅** — the client spawns its own internal tokio net + decode tasks over mpsc; `poll_event(&self)` is a **non-blocking `try_recv`** (returns `Ok(None)` when idle) and `input(&self)` also takes `&self`, so a single `tokio::select!` over `{cancel, input_rx.recv(), interval.tick()}` drives both — same shape as `rdp.rs` `run_session`. **Must re-send `X11Event::Refresh` each tick** (incremental update request) to keep frames flowing — confirmed (652 rects in 5 s). `X11Event::FullRefresh` forces a full update.

Other facts that change the plan:

- **Encodings to request (v1): `Zrle + CopyRect + Raw` (+ `DesktopSizePseudo`).** **Avoid `Tight` in v1** — Tight subrects come back as `VncEvent::JpegImage(rect, jpeg_bytes)`, which would need a JPEG decode path (base64 `<img>` on the frontend, or a pure-Rust `jpeg-decoder`). ZRLE gives compression via the already-present pure-Rust flate2/miniz_oxide. Available encodings: Raw, CopyRect, Tight, Trle, Zrle, CursorPseudo, DesktopSizePseudo, LastRectPseudo.
- **CopyRect** arrives as `VncEvent::Copy(dst, src)` — the backend must apply it against the local framebuffer (it's not a single RGBA blit), or just answer with `FullRefresh`. Adjust §4 step 8.
- **Client-initiated resize is NOT supported.** The crate only surfaces *server*-driven resolution changes via `DesktopSizePseudo` → `VncEvent::SetResolution(Screen)`. There is no client-side `SetDesktopSize` API, so `vnc_resize` (§4/§7) is effectively a no-op — rely on server-fixed size + CSS-scaled canvas (the §10.7 baseline). Treat `vnc_resize` as best-effort/stub.
- **Input types match §6 exactly:** `X11Event::PointerEvent(ClientMouseEvent { position_x: u16, position_y: u16, bottons: u8 })` (button bitmask — note the upstream typo `bottons`) and `X11Event::KeyEvent(ClientKeyEvent { keycode: u32, down: bool })` where `keycode` is the X11 keysym. Verified end-to-end (pointer + keysym `0x61` reached the mock server).
- **Builder shape:** `VncConnector::new(tcp).set_auth_method(..).add_encoding(..).allow_shared(true).set_pixel_format(PixelFormat::rgba()).build()?.try_start().await?.finish()?` → `VncClient` (which is `Clone`). `VncEvent` and `X11Event` are `#[non_exhaustive]`, so match arms need a `_ =>`.

**Verdict:** go with `vnc-rs` 0.5.3. No need for the rust-vnc / hand-rolled fallbacks. Spike code lives in the scratchpad (`vnc-spike/`, `src/main.rs` + `src/bin/mock_server.rs`) — not committed.

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

1. ~~**Spike (½ day):** pick the crate, prove headless connect + pixel rects + VNC Auth against a local server. Update §3.~~ ✅ **Done (2026-06-23)** — `vnc-rs` 0.5.3 confirmed; all four properties verified (build + mock RFB server). See §3 findings.
2. ~~**Backend skeleton:** `vnc.rs` with `VncState`, the four commands, registration in `lib.rs`.~~ ✅ **Done (2026-06-23)** — full `vnc.rs` (not just a stub), `mod vnc` + `.manage` + handlers in `lib.rs`, `vnc-rs = "0.5"` in `Cargo.toml`. `cargo check` green. No capability change needed (`core:default` covers it, as §9 predicted).
3. ~~**Framebuffer rendering:** decode updates → local framebuffer → tag-3 rect frames; loop re-requests incremental updates. Frontend tag-3 branch.~~ ✅ **Done** — `handle_event` blits RawImage + applies CopyRect against a local RGBA framebuffer, emits tag-3 rects; 16 ms tick re-arms incremental `Refresh`. `App.vue handleFrame` has the tag-3 branch.
4. ~~**Pointer input:** button bitmask + move + wheel.~~ ✅ **Done** — `Pointer` tracks position + button bitmask; wheel = press/release of wheel buttons.
5. ~~**Keyboard input:** `keysyms.ts` + protocol-aware `onKey` + backend `KeyEvent`.~~ ✅ **Done** — `src/keysyms.ts` (printable via `e.key`, named/modifier/keypad via `e.code`); `onKey` branches on session protocol.
6. ~~**Form/UX:** protocol dropdown, default port 5900, hide the Windows-RDP button for VNC, protocol-aware input/disconnect routing.~~ ✅ **Done** — VNC `<option>`, protocol→port watcher, `🖥` hidden for VNC, `connect`/`input`/`disconnect`/`resize` all routed by `runtime.protocol`. `npm run build` green.
7. **Resize (best-effort):** ⚠️ **Stubbed** — `vnc_resize` is a logged no-op (vnc-rs 0.5.3 has no client-side `SetDesktopSize`); rely on server-fixed size + CSS-scaled canvas. `SetResolution` events from the server *are* handled (re-init + framebuffer realloc).
8. **Hardening:** ✅ unsupported-auth + wrong-password messages done (`map_connect_err`); connection-lost surfaces as a status frame. ⏳ No reconnect loop (VNC has no RDP-style handoff disconnect, so likely unneeded — revisit if servers drop post-auth).
9. ~~**Docs:** update `CLAUDE.md` (two embedded clients) and `README.md` (protocol table, VNC notes).~~ ✅ **Done** — `CLAUDE.md` (architecture, tag-3, vnc commands, keysyms, `vnc.rs` internals) and `README.md` (corrected the stale FreeRDP backend table → embedded pure-Rust clients; added VNC setup/auth notes, project structure, roadmap).

> **⏳ Not yet done: live end-to-end test.** Both build gates pass, but the integrated app has *not* been run against a real VNC server (none available on the dev box). The spike's mock RFB server exercised the crate path; milestone 11's manual matrix (TigerVNC/x11vnc, auth cases, input) still needs a real server via `npm run tauri dev`.

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
