//! Clipboard redirection (CLIPRDR) — text only, both directions.
//!
//! IronRDP drives clipboard through a [`CliprdrBackend`] whose callbacks fire
//! synchronously inside the session loop while it processes incoming PDUs. Those
//! callbacks can't reach back into the (borrowed) `Cliprdr` channel to send PDUs,
//! so the backend instead pushes [`ClipboardMessage`]s onto an mpsc queue; the
//! session loop drains that queue and turns each message into wire PDUs (see
//! `dispatch_clipboard` in `rdp.rs`).
//!
//! Data flows:
//! - Remote copy  → `on_remote_copy` advertises → we auto-paste (`SendInitiatePaste`)
//!   → `on_format_data_response` pushes the text to the webview (TAG_CLIPBOARD).
//! - Local copy   → the webview calls `rdp_clipboard_offer`, which stores the text
//!   and enqueues `SendInitiateCopy`; when the remote asks for it,
//!   `on_format_data_request` answers from the stored text.
//!
//! Files (FileGroupDescriptorW) are intentionally not supported yet: we advertise
//! empty capabilities so the server never negotiates file transfer.

use std::sync::{Arc, Mutex};

use tauri::ipc::{Channel, InvokeResponseBody};
use tokio::sync::mpsc;

use ironrdp::cliprdr::backend::{CliprdrBackend, ClipboardMessage};
use ironrdp::cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp::core::IntoOwned as _;

use crate::rdp::TAG_CLIPBOARD;

/// The single clipboard format we exchange: Windows CF_UNICODETEXT (UTF-16LE).
fn unicode_text() -> ClipboardFormat {
    ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)
}

/// Text the local (webview) side has offered for the remote to paste. Shared
/// between the Tauri command that sets it and the backend that reads it.
pub type LocalClipboard = Arc<Mutex<Option<String>>>;

/// Build the `SendInitiateCopy` message advertising local text to the remote.
/// Used by the `rdp_clipboard_offer` command after it stores the text.
pub fn advertise_text() -> ClipboardMessage {
    ClipboardMessage::SendInitiateCopy(vec![unicode_text()])
}

/// CLIPRDR backend bridging the RDP clipboard channel to the webview clipboard.
pub struct WebClipboardBackend {
    tmp: String,
    local: LocalClipboard,
    /// Outbound queue drained by the session loop (see `dispatch_clipboard`).
    evt: mpsc::UnboundedSender<ClipboardMessage>,
    /// Same channel used for framebuffer strips; carries TAG_CLIPBOARD frames.
    frontend: Channel<InvokeResponseBody>,
}

impl WebClipboardBackend {
    pub fn new(
        local: LocalClipboard,
        evt: mpsc::UnboundedSender<ClipboardMessage>,
        frontend: Channel<InvokeResponseBody>,
    ) -> Box<dyn CliprdrBackend> {
        Box::new(Self {
            tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            local,
            evt,
            frontend,
        })
    }

    fn enqueue(&self, msg: ClipboardMessage) {
        // If the receiver is gone the session has ended; ignore.
        let _ = self.evt.send(msg);
    }
}

// Manual Debug (Channel has no meaningful Debug we want to print).
impl std::fmt::Debug for WebClipboardBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebClipboardBackend").finish_non_exhaustive()
    }
}

impl ironrdp::core::AsAny for WebClipboardBackend {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl CliprdrBackend for WebClipboardBackend {
    fn temporary_directory(&self) -> &str {
        &self.tmp
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // Text only: no file-transfer / lock capabilities advertised.
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        log::info!("cliprdr: channel ready");
    }

    fn on_request_format_list(&mut self) {
        // Advertise local text if the webview has already offered some.
        let has_text = self.local.lock().unwrap().is_some();
        if has_text {
            self.enqueue(advertise_text());
        }
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // The remote copied something; if it has Unicode text, pull it eagerly so
        // it's ready in the local clipboard for a paste.
        let has_text = available_formats
            .iter()
            .any(|f| f.id == ClipboardFormatId::CF_UNICODETEXT);
        if has_text {
            self.enqueue(ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        // The remote wants our clipboard data. Answer with the stored text.
        let response = if request.format == ClipboardFormatId::CF_UNICODETEXT {
            match self.local.lock().unwrap().as_deref() {
                Some(text) => FormatDataResponse::new_unicode_string(text).into_owned(),
                None => FormatDataResponse::new_error().into_owned(),
            }
        } else {
            FormatDataResponse::new_error().into_owned()
        };
        self.enqueue(ClipboardMessage::SendFormatData(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        // Remote sent the text we asked for; push it to the webview clipboard.
        if response.is_error() {
            return;
        }
        match response.to_unicode_string() {
            Ok(text) => {
                let mut frame = Vec::with_capacity(1 + text.len());
                frame.push(TAG_CLIPBOARD);
                frame.extend_from_slice(text.as_bytes());
                let _ = self.frontend.send(InvokeResponseBody::Raw(frame));
            }
            Err(e) => log::warn!("cliprdr: decode remote text failed: {e}"),
        }
    }

    // --- File transfer: unsupported (empty capabilities advertised) ---
    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}
}
