# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A cross-platform desktop **Remote PC Manager** (RDP) built with **Tauri 2** (Rust backend) + **Vue 3 / TypeScript** (UI). Connection metadata lives in SQLite; passwords live only in the OS keychain, never in the DB.

## Commands

```bash
npm install            # first time only (also compiles ~300 Rust crates on first dev run, 5–10 min)
npm run tauri dev      # run the full desktop app with hot-reload (use this for development)
npm run tauri build    # build a release installer -> src-tauri/target/release/bundle/
npm run build          # type-check (vue-tsc --noEmit) + vite build of the frontend only
```

There is no test suite and no linter configured. `npm run build` is the only type-check gate — run it after frontend changes. For Rust-only changes, `cargo check` / `cargo build` inside `src-tauri/` is faster than a full `tauri dev` cycle. `npm run dev` (bare Vite, port 1420) serves the UI in a browser but Tauri `invoke` calls fail there — only useful for pure styling work.

## Architecture

Two RDP connection paths share one credential/DB layer:

1. **Embedded IronRDP client (primary)** — `src-tauri/src/rdp.rs`. A pure-Rust RDP client (the `ironrdp` crates) that speaks RDP in a background tokio task, decodes the framebuffer, and streams it to a webview `<canvas>`. No external program required; works on all platforms. This is what clicking a connection card uses.
2. **`mstsc.exe` fallback (Windows only)** — `connect_rdp` / `launch_mstsc` in `src-tauri/src/lib.rs`. Injects the password via `cmdkey` then launches Windows' native client in its own window. Triggered by the 🖥 button on a card. Returns an error on non-Windows.

> Note: `README.md` is partly stale — it describes FreeRDP (Linux/macOS) and mstsc as *the* launch backends. The code has since moved to embedded IronRDP as the primary path; there is **no FreeRDP code** in the repo. Trust the source over the README here.

### Frontend ↔ backend contract

- **Tauri commands** (registered in `lib.rs` `run()` via `generate_handler!`): `save_password`, `get_password`, `delete_password` (keychain); `connect_rdp` (mstsc); and `rdp_connect`, `rdp_input`, `rdp_resize`, `rdp_disconnect` (embedded client).
- **Frame streaming uses a raw binary protocol over a Tauri `Channel`**, not JSON. Little-endian, first byte is a tag (see top of `rdp.rs` and `handleFrame` in `App.vue` — they must stay in sync):
  - `0` init: `[0][width u16][height u16]` → frontend sizes the canvas.
  - `1` strip: `[1][y u16][height u16][rgba...]` → a full-width RGBA row strip drawn via `putImageData`. Dirty rects are widened to full width so pixel data is contiguous.
  - `2` status: `[2][utf-8]` → human-readable status/error shown on the tab.
- **Input** flows back through `rdp_input` as JSON tagged events (`move`/`button`/`wheel`/`key`). Keyboard uses **set-1 scancodes**; `src/scancodes.ts` maps DOM `KeyboardEvent.code` → scancode and must mirror the values `rdp.rs` expects.

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

## Important constraints & gotchas

- **`src-tauri/vendor/ironrdp-connector/` is a patched local copy** of ironrdp-connector 0.9.0 (one-line fix to tolerate a Server Deactivate All PDU during capabilities exchange), wired in via `[patch.crates-io]` in `Cargo.toml`. Don't delete it; remove only once the upstream fix lands.
- Crypto uses the **`ring`** rustls backend (not aws-lc) specifically to avoid needing a C/NASM toolchain at build time. Keep it that way unless you're prepared to add that toolchain.
- The OS window frame is disabled (`decorations: false`); the title bar, tabs, and min/maximize/close buttons are custom HTML in `App.vue` using `data-tauri-drag-region` and `core:window:*` permissions.
- Any new Tauri command must be added to **both** `generate_handler!` in `lib.rs` **and** the relevant capability in `src-tauri/capabilities/default.json`, or the webview call is rejected.
- SQLite schema changes go through `tauri_plugin_sql` **migrations** in `lib.rs` `run()` (append a new `Migration` with the next version — never edit an existing one). DB file: `%APPDATA%\com.kynoci.remote\remote.db`.

## Debugging

The built `.exe` has no console. Use **F12 / Inspect Element** for the JS console, or read the log file at `%LOCALAPPDATA%\com.kynoci.remote\logs\Remote PC Manager.log` (Rust + JS logs append in real time). The `ironrdp_connector` target is pinned to `Debug` level so server disconnects/redirections are diagnosable.
