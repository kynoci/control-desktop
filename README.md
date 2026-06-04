# Remote PC Manager

A cross-platform desktop app for managing remote PC connections (RDP for now; SSH/SFTP/VNC planned). Passwords are stored in the OS keychain (Windows Credential Manager / macOS Keychain / Linux Secret Service), never in plain text.

Built with **Tauri 2** (Rust backend) + **Vue 3 + TypeScript** (UI).

---

## Quick start

Open terminal in folder: `C:\Users\engineer\Desktop\work\remote`

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
   - **Port:** `3389` (default RDP port)
   - **Username:** the Windows user on the remote machine
   - **Password:** the user's password — saved to **Windows Credential Manager**, not the database
   - **Protocol:** RDP (only option for now)
3. Click **Save**.
4. Click the connection in the list, then **Connect**.
5. Windows' built-in **mstsc.exe** (Remote Desktop) opens and logs in automatically — no password prompt.

To verify the password really is in the keychain (and not in the DB):
- Open Windows: **Control Panel → User Accounts → Credential Manager → Windows Credentials**
- You'll see an entry like `TERMSRV/your-host` while the app holds it.

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

| Platform | RDP launch backend | Status |
|---|---|---|
| **Windows** | `cmdkey` + `mstsc.exe` (built into Windows) | Fully implemented |
| **Linux** | FreeRDP — `xfreerdp3` / `xfreerdp` (install: `sudo apt install freerdp3-x11`, or `freerdp2-x11` on older Ubuntu) | Implemented |
| **macOS** | FreeRDP — `sdl-freerdp` (install: `brew install freerdp`) | Implemented |

On Linux and macOS the app auto-detects the installed FreeRDP client (it probes
`sdl-freerdp`/`xfreerdp3`/`xfreerdp`, including Homebrew paths on macOS) and
adjusts its certificate flag for FreeRDP 2 vs 3. The password is passed to
FreeRDP so login is automatic — no prompt. If FreeRDP isn't installed, **Connect**
returns an error telling you the exact install command.

> **Why FreeRDP on macOS instead of Microsoft Remote Desktop?** The MS app can't
> accept a password from the command line, so it would always prompt. FreeRDP
> supports password injection, keeping the "no password prompt" behavior
> consistent with Windows and Linux.

The keychain code uses target-specific Cargo features, so each platform automatically uses its native credential store.

---

## Project structure

```
src/                 # Vue UI (App.vue is the whole app for now)
src-tauri/
  src/lib.rs         # Rust backend: keychain commands + RDP launcher
  Cargo.toml         # Rust dependencies
  tauri.conf.json    # App config (window size, title, identifier)
  capabilities/      # Tauri permission grants for the webview
```

---

## Roadmap (next features)

- M3: SSH terminal (xterm.js + `russh`)
- M4: SFTP file browser
- M5: VNC support
- M6: Connection grouping, search, import from existing `.rdp` / PuTTY profiles
- M6: Optional master password to lock the app
