# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

## What this is

A cross-platform desktop **Control Desktop** remote-access app (RDP + VNC) built with **Tauri 2** (Rust backend) + **Vue 3 / TypeScript** (UI). Connection metadata lives in SQLite; passwords live only in the OS keychain, never in the DB.

## Commands

```bash
npm install            # first time only (also compiles ~300 Rust crates on first dev run, 5–10 min)
npm run tauri dev      # run the full desktop app with hot-reload (use this for development)
npm run tauri build    # build a release installer -> src-tauri/target/release/bundle/
npm run build          # type-check (vue-tsc --noEmit) + vite build of the frontend only
```

There is no test suite and no linter configured. `npm run build` is the only type-check gate — run it after frontend changes. For Rust-only changes, `cargo check` / `cargo build` inside `src-tauri/` is faster than a full `tauri dev` cycle. `npm run dev` (bare Vite, port 1420) serves the UI in a browser but Tauri `invoke` calls fail there — only useful for pure styling work.

## Architecture

Connection paths share one credential/DB layer. Clicking a connection card uses an embedded pure-Rust client chosen by the connection's `protocol`:

1. **Embedded IronRDP client (RDP, primary)** — `src-tauri/src/rdp.rs`. A pure-Rust RDP client (the `ironrdp` crates) that speaks RDP in a background tokio task, decodes the framebuffer, and streams it to a webview `<canvas>`. No external program required; works on all platforms.
2. **Embedded VNC/RFB client** — `src-tauri/src/vnc.rs`. A pure-Rust RFB client (the `vnc-rs` crate) built as a sibling of `rdp.rs`: same background-task / `Channel` / `CancellationToken` shape, same credential + DB layer. Used when `protocol === 'vnc'`. Supports None + VNC Auth only; requests RGBA pixels + ZRLE/CopyRect/Raw encodings (no Tight). See `plan-vnc.md` for the design and the spike that picked the crate.
3. **`mstsc.exe` fallback (Windows only)** — `connect_rdp` / `launch_mstsc` in `src-tauri/src/lib.rs`. Injects the password via `cmdkey` then launches Windows' native client in its own window. Triggered by the 🖥 button on a card (RDP connections only). Returns an error on non-Windows.

> Note: `README.md` is partly stale — it describes FreeRDP (Linux/macOS) and mstsc as _the_ launch backends. The code has since moved to embedded IronRDP as the primary path; there is **no FreeRDP code** in the repo. Trust the source over the README here.

### Frontend ↔ backend contract

- **Tauri commands** (registered in `lib.rs` `run()` via `generate_handler!`): `save_password`, `get_password`, `delete_password` (keychain); `connect_rdp` (mstsc); `rdp_connect`, `rdp_input`, `rdp_resize`, `rdp_disconnect` (embedded RDP); and `vnc_connect`, `vnc_input`, `vnc_resize`, `vnc_disconnect` (embedded VNC). The frontend picks `rdp_*` vs `vnc_*` per the session's `protocol`.
- **Frame streaming uses a raw binary protocol over a Tauri `Channel`**, not JSON. Little-endian, first byte is a tag (see top of `rdp.rs` / `vnc.rs` and `handleFrame` in `App.vue` — they must stay in sync):
  - `0` init: `[0][width u16][height u16]` → frontend sizes the canvas. (both)
  - `1` strip: `[1][y u16][height u16][rgba...]` → a full-width RGBA row strip drawn via `putImageData`. Dirty rects are widened to full width so pixel data is contiguous. (RDP)
  - `2` status: `[2][utf-8]` → human-readable status/error shown on the tab. (both)
  - `3` rect: `[3][x u16][y u16][w u16][h u16][rgba...]` → an arbitrary dirty rectangle drawn via `putImageData(…, x, y)`. (VNC — RFB updates aren't full-width strips)
  - `6` ended: `[6][utf-8]` → the session stopped (error or clean close). Deliberately distinct from `2` so the UI can tell "still connecting" from "it stopped" without parsing the wording. (both)
- **Input** flows back through `rdp_input` / `vnc_input` as JSON tagged events (`move`/`button`/`wheel`/`key`). The mouse events are protocol-neutral; only `key` differs: RDP uses **set-1 scancodes** (`src/scancodes.ts`), VNC uses **X11 keysyms** (`src/keysyms.ts`). Each map must mirror what its backend expects. `onKey` in `App.vue` branches on the active session's protocol.

### Connection organisation (folders & ordering)

- Connections live in a **folder tree** (`folders` table, `parent_id` self-reference; `connections.folder_id`). `NULL` = the root, shown as "Home".
- Folders and connections inside one parent share **a single `sort_order` sequence**, so drag-sorting can interleave them. `itemsIn(folderId)` in `App.vue` merges both tables into one ordered `Item[]`; `persistOrder()` rewrites `sort_order` as `0..n-1` after a drop.
- Drag & drop (HTML5 DnD, no library): a card's outer 25% edges mean *insert before/after*, the middle of a **folder** card means *move inside*. Breadcrumb crumbs, sidebar folder rows, and the grid background are also drop targets (all route through `moveInto`). `isDescendantOf` blocks dropping a folder into its own subtree.
- Deleting a folder **never deletes connections** — its contents are re-parented to the deleted folder's parent.
- The sidebar tree is rendered from `treeRows`, a *pre-flattened* list of `{item, depth, expanded}` — flattening avoids needing a recursive component inside the single-file `App.vue`.

### Session model

- The embedded client supports **multiple concurrent sessions**, one per UI tab. Each is keyed by a frontend-assigned `session_id` (incrementing integer, distinct from the DB connection `id`).
- Backend session state lives in `RdpState` (a `Mutex<HashMap<u32, SessionHandle>>` managed by Tauri). Each `SessionHandle` holds input/resize mpsc senders + a `CancellationToken`. Reconnecting on the same tab cancels and replaces the old session.
- In `App.vue`, **canvas/ctx and per-session runtime are deliberately kept out of Vue reactivity** (plain `Map<number, Runtime>`), because proxying a canvas breaks rendering. Only `sessions` (tab metadata) and `connections` are reactive refs.
- Thumbnails: the canvas is periodically snapshotted to a JPEG data URL and cached in `localStorage` under `thumb:<connId>`, shown on the connection cards.

### Connection internals (`rdp.rs`)

- `establish()` does one full attempt: TCP → RDP negotiation → TLS upgrade → CredSSP/NLA. `run_session()` wraps it in a **bounded reconnect loop** (`MAX_ATTEMPTS = 6`) to tolerate the post-auth "handoff disconnect" some servers (e.g. GNOME Remote Login) issue — see `is_handoff_disconnect`.
- **NLA/CredSSP is enforced** (`enable_credssp: true`, `autologon: true`) so stored credentials log straight in. Only **NTLM** is supported — Kerberos/KDC round-trips fail fast (`NoKdcNetworkClient`).
- **TLS certificate verification is intentionally disabled** (`danger::NoCertificateVerification`) — RDP hosts present self-signed certs; this mirrors clicking through the mstsc/FreeRDP warning on a trusted LAN.
- Server-initiated resize (`DeactivateAll`) is handled by `reactivate()`, which reruns the activation sequence and rebuilds the framebuffer at the new size. Client-initiated resize goes through the Display Control dynamic channel (`encode_resize`).

### Connection internals (`vnc.rs`)

- `run_session()`: TCP → `VncConnector` (RFB handshake + auth) → a `tokio::select!` over `{cancel, input_rx, 16ms tick}`. **RFB is request-driven**: the tick drains `poll_event()` (non-blocking) and re-sends `X11Event::Refresh` so the screen keeps updating.
- **Auth: None + VNC Auth only** (the `vnc-rs` crate does the DES internally). Any other security type (VeNCrypt/TLS, RealVNC RA2, …) is detected via the crate's error string and surfaced as an actionable message (`map_connect_err`) — mirrors `rdp.rs`'s clear-error approach. Plain TCP, no TLS.
- **Pixels: request `PixelFormat::rgba()`** so rects arrive as `[r,g,b,a]` — forwarded straight to the canvas, no swizzling. A local RGBA framebuffer is kept only to service **CopyRect** (`VncEvent::Copy`), which references previously-drawn pixels.
- **Client-initiated resize works via a vendored `vnc-rs` patch.** Upstream 0.5.3 has no resize at all, so `src-tauri/vendor/vnc-rs/` adds the `ExtendedDesktopSize` pseudo-encoding (-308), the `SetDesktopSize` client message (251), `X11Event::SetDesktopSize`, and `VncEvent::DesktopResizeSupported`; every change is marked `PATCH (Control Desktop)`. **`SetDesktopSize` must never be sent until the server has sent an `ExtendedDesktopSize` rect** — RFB gives a server no way to skip an unknown client message, so one lacking the extension *drops the connection*. `run_session` therefore gates on `resize_supported` (set by `DesktopResizeSupported`) and, when unsupported, warns the user once instead of sending. Encodings requested: ZRLE/CopyRect/Raw (no Tight, which would yield JPEG sub-rects).
- See `plan-vnc.md` §3 for the spike findings behind these choices.

## Important constraints & gotchas

- **`src-tauri/vendor/ironrdp-connector/` is a patched local copy** of ironrdp-connector 0.9.0 (one-line fix to tolerate a Server Deactivate All PDU during capabilities exchange), wired in via `[patch.crates-io]` in `Cargo.toml`. Don't delete it; remove only once the upstream fix lands.
- **`src-tauri/vendor/vnc-rs/` is likewise a patched local copy** of vnc-rs 0.5.3, adding client-initiated resize (see the VNC section above). Both vendored crates are wired in through the same `[patch.crates-io]` block. Grep `PATCH (Control Desktop)` to see every local change.
- Crypto uses the **`ring`** rustls backend (not aws-lc) specifically to avoid needing a C/NASM toolchain at build time. Keep it that way unless you're prepared to add that toolchain.
- The OS window frame is disabled (`decorations: false`); the title bar, tabs, and min/maximize/close buttons are custom HTML in `App.vue` using `data-tauri-drag-region` and `core:window:*` permissions.
- Any new Tauri command must be added to **both** `generate_handler!` in `lib.rs` **and** the relevant capability in `src-tauri/capabilities/default.json`, or the webview call is rejected.
- SQLite schema changes go through `tauri_plugin_sql` **migrations** in `lib.rs` `run()` (append a new `Migration` with the next version — never edit an existing one). DB file: `%APPDATA%\com.kynoci.remote\remote.db`.

## Debugging

The built `.exe` has no console. Use **F12 / Inspect Element** for the JS console, or read the log file at `%LOCALAPPDATA%\com.kynoci.remote\logs\Control Desktop.log` (Rust + JS logs append in real time). The `ironrdp_connector` target is pinned to `Debug` level so server disconnects/redirections are diagnosable.
