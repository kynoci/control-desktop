use serde::{Deserialize, Serialize};
use tauri_plugin_sql::{Migration, MigrationKind};

mod clipboard;
mod rdp;
mod vnc;

const KEYRING_SERVICE: &str = "com.kynoci.remote";

#[derive(Debug, Serialize, Deserialize)]
pub struct ConnectRequest {
    pub id: i64,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub protocol: String,
    /// Selects the connection path. "ironrdp" (default) uses the built-in,
    /// embedded client; "mstsc" launches the native Windows client as a fallback.
    #[serde(default)]
    pub client: String,
}

fn keyring_account(connection_id: i64) -> String {
    format!("conn_{}", connection_id)
}

#[tauri::command]
fn save_password(connection_id: i64, password: String) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_account(connection_id))
        .map_err(|e| e.to_string())?;
    entry.set_password(&password).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_password(connection_id: i64) -> Result<Option<String>, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_account(connection_id))
        .map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(pw) => Ok(Some(pw)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
fn delete_password(connection_id: i64) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_account(connection_id))
        .map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Native Windows fallback: launch the built-in `mstsc.exe` client. The primary
/// path is the embedded IronRDP client (see `rdp::rdp_connect`); this stays as a
/// safety net on Windows and is unavailable on other platforms.
#[tauri::command]
fn connect_rdp(req: ConnectRequest) -> Result<(), String> {
    log::info!(
        "connect_rdp (mstsc): id={} host={} port={} user={}",
        req.id, req.host, req.port, req.username
    );
    let password = get_password(req.id)?
        .ok_or_else(|| "No saved password for this connection".to_string())?;
    let result = launch_mstsc(&req.host, req.port, &req.username, &password);
    if let Err(e) = &result {
        log::error!("launch_mstsc failed: {}", e);
    } else {
        log::info!("launch_mstsc: spawned successfully");
    }
    result
}

// Windows built-in RDP client (Microsoft Terminal Services Client).
#[cfg(target_os = "windows")]
fn launch_mstsc(host: &str, port: u16, username: &str, password: &str) -> Result<(), String> {
    use std::process::Command;

    let target = format!("TERMSRV/{}", host);
    let cmdkey_status = Command::new("cmdkey")
        .args([
            &format!("/generic:{}", target),
            &format!("/user:{}", username),
            &format!("/pass:{}", password),
        ])
        .status()
        .map_err(|e| format!("failed to run cmdkey: {}", e))?;
    if !cmdkey_status.success() {
        return Err("cmdkey failed to inject credentials".into());
    }

    let address = if port == 3389 {
        host.to_string()
    } else {
        format!("{}:{}", host, port)
    };
    Command::new("mstsc.exe")
        .args([format!("/v:{}", address)])
        .spawn()
        .map_err(|e| format!("failed to launch mstsc.exe: {}", e))?;

    Ok(())
}

// The native fallback only exists on Windows. Elsewhere there is no built-in CLI
// RDP client, so callers should use the embedded IronRDP path instead.
#[cfg(not(target_os = "windows"))]
fn launch_mstsc(_host: &str, _port: u16, _username: &str, _password: &str) -> Result<(), String> {
    Err("The native Windows client (mstsc.exe) is only available on Windows. Use the built-in client instead.".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let migrations = vec![Migration {
        version: 1,
        description: "create_connections_table",
        sql: "CREATE TABLE IF NOT EXISTS connections (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            host TEXT NOT NULL,
            port INTEGER NOT NULL DEFAULT 3389,
            username TEXT NOT NULL,
            protocol TEXT NOT NULL DEFAULT 'rdp',
            created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        );",
        kind: MigrationKind::Up,
    }];

    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                // Keep connector at debug so server-side disconnects/redirections are
                // diagnosable (logs each received Share Control PDU by type).
                .level_for("ironrdp_connector", log::LevelFilter::Debug)
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: None,
                    }),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Webview),
                ])
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_sql::Builder::default()
                .add_migrations("sqlite:remote.db", migrations)
                .build(),
        )
        .manage(rdp::RdpState::default())
        .manage(vnc::VncState::default())
        .invoke_handler(tauri::generate_handler![
            save_password,
            get_password,
            delete_password,
            connect_rdp,
            rdp::rdp_connect,
            rdp::rdp_input,
            rdp::rdp_clipboard_offer,
            rdp::rdp_resize,
            rdp::rdp_disconnect,
            vnc::vnc_connect,
            vnc::vnc_input,
            vnc::vnc_resize,
            vnc::vnc_disconnect,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
