# Remote PC Manager

A cross-platform desktop app for managing remote PC connections over **RDP** and **VNC** (SSH/SFTP planned). It ships **embedded, pure-Rust clients** for both protocols — clicking a connection opens a live remote desktop in a tab, no external program required. Passwords are stored in the OS keychain (Windows Credential Manager / macOS Keychain / Linux Secret Service), never in plain text.

Built with **Tauri 2** (Rust backend) + **Vue 3 + TypeScript** (UI).

---

## Quick start

Open terminal in folder: `C:\Users\engineer\Projects\remote2`

1. First time only: `npm install`
2. Test / run app: `npm run tauri dev`
3. Build installer: `npm run tauri build`

---

## Prerequisites

You need these installed once on your machine:

- **Rust** — install from https://rustup.rs (gives you `rustc` and `cargo`)
- **Node.js** LTS — https://nodejs.org
- **WebView2 Runtime** — already on Windows 11 by default
- Platform build tools:
  - **Windows:** Microsoft C++ Build Tools (comes with Visual Studio Build Tools)
  - **macOS:** Xcode Command Line Tools (`xcode-select --install`)
  - **Linux:** `webkit2gtk-4.1`, `libssl`, `build-essential` (see Tauri prerequisites for your distro)

Verify they are installed:

```powershell
rustc --version
node --version
npm --version
```

---

## Run the app (no build needed)

```powershell
npm install        # only the first time
npm run tauri dev
```

- **First run:** Rust compiles ~300 dependencies — takes 5–10 minutes. Just wait.
- **After that:** subsequent runs start in seconds. Code changes hot-reload automatically.
- A desktop window titled **Remote PC Manager** will open.

`tauri dev` is the right command for development and testing. You do **not** need to run `tauri build` unless you want a final installer.

---

## How to test the app

1. Click **+ New** in the sidebar.
2. Fill in a connection:
   - **Name:** any label, e.g. `My work PC`
   - **Host / IP:** the target machine's IP or hostname
   - **Protocol:** **RDP** or **VNC**
   - **Port:** auto-fills the protocol default (`3389` for RDP, `5900` for VNC); change it if needed
   - **Username:** the remote user (RDP needs this; classic VNC ignores it)
   - **Password:** saved to the **OS keychain**, not the database. For RDP and VNC Auth this is required; a VNC server set to *no authentication* can use a blank password.
3. Click **Save**.
4. Click the connection card (or its sidebar entry) to connect — the remote desktop opens in a **new tab**, driven by the embedded client. Mouse and keyboard work in the tab; close the tab to disconnect. You can open several sessions at once.
5. **Windows RDP fallback (optional):** for an RDP connection, the 🖥 button on a card hands off to Windows' built-in **mstsc.exe** in its own window instead of the embedded client.

VNC notes:
- Supported auth is **None** and **VNC Authentication**. Servers requiring **VeNCrypt/TLS** or **RealVNC** encryption aren't supported yet — you'll get a clear error suggesting you enable standard VNC Auth or use RDP.
- The server's resolution is fixed; the canvas scales to fit the window.

To verify the password really is in the keychain (and not in the DB):
- Open Windows: **Control Panel → User Accounts → Credential Manager → Windows Credentials**
- For the Windows RDP fallback you'll see a `TERMSRV/your-host` entry while the app holds it; embedded sessions read the stored password directly from the keychain.

The SQLite database is stored at:

```
%APPDATA%\com.kynoci.remote\remote.db
```

It contains the connection metadata (name, host, port, username) but **never the password**.

---

## Build a release installer

When you want to ship a real `.msi` / `.dmg` / `.deb` installer:

```powershell
npm run tauri build
```

Output goes to `src-tauri/target/release/bundle/`.

---

## Debugging the built app

The built `.exe` has no terminal, so `println!` / `console.log` are not visible by default. Two ways to debug:

**1. Open dev tools (F12)**
Right-click anywhere in the app window → **Inspect Element**, or press **F12 / Ctrl+Shift+I**. Shows the Console tab with all JS logs + errors.

**2. Read the log file**
All Rust and JS logs are written to:

```
%LOCALAPPDATA%\com.kynoci.remote\logs\Remote PC Manager.log
```

(macOS: `~/Library/Logs/com.kynoci.remote/`, Linux: `~/.local/share/com.kynoci.remote/logs/`)

Open the file in any editor — it appends in real time, so you can watch it while using the app (e.g. `Get-Content -Wait` in PowerShell).

---

## Cross-platform notes

Both remote-desktop clients are **embedded and pure-Rust** (RDP via the `ironrdp`
crates, VNC via `vnc-rs`), so they run on every platform with **no external client
to install** — no FreeRDP, no separate VNC viewer. This is the primary path on all
platforms.

| Client | Backend | Platforms | Status |
|---|---|---|---|
| **RDP (embedded)** | `ironrdp` (pure Rust) | Windows / macOS / Linux | Primary path |
| **VNC (embedded)** | `vnc-rs` (pure Rust) | Windows / macOS / Linux | Implemented (None + VNC Auth) |
| **RDP via mstsc** | `cmdkey` + `mstsc.exe` | Windows only | Optional fallback (🖥 button) |

The build stays free of any C/NASM toolchain requirement: `rustls` uses the `ring`
backend and `vnc-rs` decompresses ZRLE via the pure-Rust `flate2`/`miniz_oxide`.
TLS certificate verification is intentionally relaxed for embedded RDP (RDP hosts
usually present self-signed certs on a trusted LAN); classic VNC is plain TCP.

The keychain code uses target-specific Cargo features, so each platform automatically uses its native credential store.

---

## Project structure

```
src/
  App.vue            # Vue UI: connection manager + tabbed sessions + canvas rendering
  scancodes.ts       # KeyboardEvent.code -> RDP set-1 scancode map
  keysyms.ts         # KeyboardEvent -> VNC X11 keysym map
src-tauri/
  src/lib.rs         # Rust backend: keychain commands, mstsc fallback, command registration
  src/rdp.rs         # Embedded IronRDP client (background tokio task + frame streaming)
  src/vnc.rs         # Embedded VNC/RFB client (vnc-rs), sibling of rdp.rs
  vendor/            # Patched local copy of ironrdp-connector (see CLAUDE.md)
  Cargo.toml         # Rust dependencies
  tauri.conf.json    # App config (window size, title, identifier)
  capabilities/      # Tauri permission grants for the webview
```

---

## Roadmap (next features)

- ~~VNC support~~ ✅ done (embedded `vnc-rs` client; None + VNC Auth)
- VNC follow-ups: VeNCrypt/TLS auth, clipboard sync, client-driven resize
- SSH terminal (xterm.js + `russh`)
- SFTP file browser
- Connection grouping, search, import from existing `.rdp` / PuTTY profiles
- Optional master password to lock the app
