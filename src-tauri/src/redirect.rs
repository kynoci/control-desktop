//! Server Redirection ([MS-RDPBCGR] 2.2.13) and the RDSTLS exchange that follows it.
//!
//! `gnome-remote-desktop` in **Remote Login** mode never serves a desktop on the
//! first connection. The system daemon authenticates the client (CredSSP, with the
//! Remote Login credentials), starts a login-screen session, and then sends a
//! Server Redirection PDU telling the client to reconnect carrying a routing token
//! and a one-time username/password. It does it again once the user signs in at
//! the login screen, to move the client into their own session. mstsc and FreeRDP
//! follow these; `ironrdp-session` has no support at all — it rejects share control
//! type `0xA` as "unexpected share control PDU type", which surfaced as a bare
//! "frame decode failed: decode error" about four seconds after connecting.
//!
//! The password in a GNOME redirect is encrypted to the target server's
//! certificate (`LB_PASSWORD_IS_PK_ENCRYPTED`). The client cannot read it and does
//! not need to: it reconnects requesting PROTOCOL_RDSTLS and hands the blob back
//! verbatim in an RDSTLS Authentication Request ([MS-RDPBCGR] 2.2.17), in place of
//! CredSSP.

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Share control PDU type of the Enhanced Security Server Redirection PDU.
const PDUTYPE_SERVER_REDIR_PKT: u16 = 0x000A;
/// `Flags` value that opens every RDP_SERVER_REDIRECTION_PACKET.
const SEC_REDIRECTION_PKT: u16 = 0x0400;

// RedirFlags ([MS-RDPBCGR] 2.2.13.1). The optional fields follow in this order.
const LB_TARGET_NET_ADDRESS: u32 = 0x0000_0001;
const LB_LOAD_BALANCE_INFO: u32 = 0x0000_0002;
const LB_USERNAME: u32 = 0x0000_0004;
const LB_DOMAIN: u32 = 0x0000_0008;
const LB_PASSWORD: u32 = 0x0000_0010;
const LB_TARGET_FQDN: u32 = 0x0000_0100;
const LB_TARGET_NETBIOS_NAME: u32 = 0x0000_0200;
const LB_CLIENT_TSV_URL: u32 = 0x0000_1000;
const LB_PASSWORD_IS_PK_ENCRYPTED: u32 = 0x0000_4000;
const LB_REDIRECTION_GUID: u32 = 0x0000_8000;

const ROUTING_TOKEN_PREFIX: &str = "Cookie: msts=";

/// What a Server Redirection PDU tells the client to reconnect with. Only the
/// fields a reconnect needs are kept; target addresses are deliberately ignored
/// (see `run_session`).
#[derive(Clone)]
pub(crate) struct Redirect {
    pub session_id: u32,
    pub flags: u32,
    /// LoadBalanceInfo, sent back as the X.224 routing token.
    pub load_balance_info: Option<Vec<u8>>,
    pub username: Option<String>,
    pub domain: Option<String>,
    /// Opaque when `LB_PASSWORD_IS_PK_ENCRYPTED` is set; UTF-16LE otherwise.
    pub password: Option<Vec<u8>>,
    pub redirection_guid: Option<Vec<u8>>,
}

// Hand-written so a redirect can be logged without its credentials.
impl std::fmt::Debug for Redirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redirect")
            .field("session_id", &self.session_id)
            .field("flags", &format_args!("{:#x}", self.flags))
            .field("routing_token", &self.routing_token())
            .field("username", &self.username)
            .field("domain", &self.domain)
            .field("password_len", &self.password.as_ref().map(Vec::len))
            .field("guid_len", &self.redirection_guid.as_ref().map(Vec::len))
            .finish()
    }
}

impl Redirect {
    /// The routing token to put in the reconnect's X.224 Connection Request, without
    /// the `Cookie: msts=` prefix and CRLF that `ironrdp-pdu`'s `RoutingToken` adds.
    pub fn routing_token(&self) -> Option<String> {
        let raw = self.load_balance_info.as_ref()?;
        let text = String::from_utf8_lossy(raw);
        let text = text.trim_end_matches(['\0', '\r', '\n']);
        Some(text.strip_prefix(ROUTING_TOKEN_PREFIX).unwrap_or(text).to_owned())
    }

    /// The password can't be read by the client, so the reconnect must use RDSTLS
    /// rather than CredSSP.
    pub fn needs_rdstls(&self) -> bool {
        self.password.is_some() && self.flags & LB_PASSWORD_IS_PK_ENCRYPTED != 0
    }

    /// A cleartext redirected password, if the server sent one.
    pub fn plain_password(&self) -> Option<String> {
        if self.flags & LB_PASSWORD_IS_PK_ENCRYPTED != 0 {
            return None;
        }
        self.password.as_deref().map(utf16_to_string)
    }
}

/// Check whether an X.224 frame is a Server Redirection PDU. `None` means it is
/// something else and should go to the active stage as usual; `Some(Err)` means it
/// is a redirect that could not be parsed.
pub(crate) fn parse_redirect_frame(frame: &[u8]) -> Option<Result<Redirect, String>> {
    let ctx = ironrdp::pdu::mcs::decode_send_data_indication(frame).ok()?;
    let data = ctx.user_data;
    // ShareControlHeader: totalLength u16, pduType u16, pduSource u16.
    if data.len() < 6 || read_u16(data, 2)? & 0x000F != PDUTYPE_SERVER_REDIR_PKT {
        return None;
    }
    let body = &data[6..];
    // The Enhanced Security variant puts pad2Octets before the packet; tolerate a
    // server that leaves it out.
    let packet = if read_u16(body, 2) == Some(SEC_REDIRECTION_PKT) {
        &body[2..]
    } else if read_u16(body, 0) == Some(SEC_REDIRECTION_PKT) {
        body
    } else {
        return Some(Err("server redirection PDU without a redirection packet".into()));
    };
    Some(parse_packet(packet))
}

/// Parse an RDP_SERVER_REDIRECTION_PACKET ([MS-RDPBCGR] 2.2.13.1).
fn parse_packet(packet: &[u8]) -> Result<Redirect, String> {
    let mut r = Reader { buf: packet, pos: 0 };
    let _flags = r.u16()?;
    let length = usize::from(r.u16()?);
    if length >= 12 && length < packet.len() {
        r.buf = &packet[..length];
    }
    let session_id = r.u32()?;
    let flags = r.u32()?;

    let field = |bit: u32, r: &mut Reader<'_>| -> Result<Option<Vec<u8>>, String> {
        if flags & bit == 0 {
            return Ok(None);
        }
        let len = r.u32()? as usize;
        r.bytes(len).map(|b| Some(b.to_vec()))
    };

    // Field order is fixed by the spec. Everything after the GUID (the target
    // certificate and address list) is not needed for a reconnect.
    let _target_net_address = field(LB_TARGET_NET_ADDRESS, &mut r)?;
    let load_balance_info = field(LB_LOAD_BALANCE_INFO, &mut r)?;
    let username = field(LB_USERNAME, &mut r)?;
    let domain = field(LB_DOMAIN, &mut r)?;
    let password = field(LB_PASSWORD, &mut r)?;
    let _fqdn = field(LB_TARGET_FQDN, &mut r)?;
    let _netbios = field(LB_TARGET_NETBIOS_NAME, &mut r)?;
    let _tsv_url = field(LB_CLIENT_TSV_URL, &mut r)?;
    let redirection_guid = field(LB_REDIRECTION_GUID, &mut r)?;

    Ok(Redirect {
        session_id,
        flags,
        load_balance_info,
        username: username.as_deref().map(utf16_to_string),
        domain: domain.as_deref().map(utf16_to_string),
        password,
        redirection_guid,
    })
}

// --- RDSTLS ([MS-RDPBCGR] 2.2.17) ----------------------------------------------

const RDSTLS_VERSION_1: u16 = 0x0001;
const RDSTLS_TYPE_CAPABILITIES: u16 = 0x0001;
const RDSTLS_TYPE_AUTHREQ: u16 = 0x0002;
const RDSTLS_TYPE_AUTHRSP: u16 = 0x0004;
const RDSTLS_DATA_PASSWORD_CREDS: u16 = 0x0001;
const RDSTLS_DATA_RESULT_CODE: u16 = 0x0001;

/// Run the client side of RDSTLS on a freshly upgraded TLS stream: read the
/// server's capabilities, send the redirected credentials, check the result.
pub(crate) async fn rdstls_authenticate<S>(stream: &mut S, redirect: &Redirect) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut caps = [0u8; 8];
    stream
        .read_exact(&mut caps)
        .await
        .map_err(|e| format!("RDSTLS: reading server capabilities: {e}"))?;
    if read_u16(&caps, 2) != Some(RDSTLS_TYPE_CAPABILITIES) {
        return Err(format!("RDSTLS: expected capabilities, got {caps:02x?}"));
    }

    stream
        .write_all(&auth_request(redirect))
        .await
        .map_err(|e| format!("RDSTLS: sending credentials: {e}"))?;
    stream.flush().await.map_err(|e| format!("RDSTLS: flush: {e}"))?;

    let mut rsp = [0u8; 10];
    stream
        .read_exact(&mut rsp)
        .await
        .map_err(|e| format!("RDSTLS: reading authentication response: {e}"))?;
    if read_u16(&rsp, 2) != Some(RDSTLS_TYPE_AUTHRSP) || read_u16(&rsp, 4) != Some(RDSTLS_DATA_RESULT_CODE) {
        return Err(format!("RDSTLS: unexpected authentication response {rsp:02x?}"));
    }
    match u32::from_le_bytes([rsp[6], rsp[7], rsp[8], rsp[9]]) {
        0 => Ok(()),
        code => Err(format!(
            "RDSTLS: the redirected session rejected the handover credentials (status {code:#010x})"
        )),
    }
}

/// RDSTLS Authentication Request PDU with Password Credentials. The GUID and the
/// (encrypted) password go back exactly as the redirect carried them; username and
/// domain are null-terminated UTF-16LE, as FreeRDP writes them.
fn auth_request(redirect: &Redirect) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&RDSTLS_VERSION_1.to_le_bytes());
    v.extend_from_slice(&RDSTLS_TYPE_AUTHREQ.to_le_bytes());
    v.extend_from_slice(&RDSTLS_DATA_PASSWORD_CREDS.to_le_bytes());
    push_blob(&mut v, redirect.redirection_guid.as_deref().unwrap_or_default());
    push_blob(&mut v, &utf16z(redirect.username.as_deref().unwrap_or_default()));
    push_blob(&mut v, &utf16z(redirect.domain.as_deref().unwrap_or_default()));
    push_blob(&mut v, redirect.password.as_deref().unwrap_or_default());
    v
}

fn push_blob(v: &mut Vec<u8>, data: &[u8]) {
    let len = u16::try_from(data.len()).unwrap_or(u16::MAX);
    v.extend_from_slice(&len.to_le_bytes());
    v.extend_from_slice(&data[..usize::from(len)]);
}

// --- helpers --------------------------------------------------------------------

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or_else(|| {
            format!("server redirection packet truncated (need {n} bytes at offset {})", self.pos)
        })?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16, String> {
        self.bytes(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        self.bytes(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

fn read_u16(buf: &[u8], at: usize) -> Option<u16> {
    buf.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn utf16_to_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').to_owned()
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(v: &mut Vec<u8>, data: &[u8]) {
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
    }

    /// The shape gnome-remote-desktop sends: routing token, username, encrypted
    /// password, GUID, plus a certificate after it that the parser must not need.
    fn gnome_like_packet() -> Vec<u8> {
        let flags = LB_LOAD_BALANCE_INFO
            | LB_USERNAME
            | LB_PASSWORD
            | LB_PASSWORD_IS_PK_ENCRYPTED
            | LB_REDIRECTION_GUID
            | 0x0001_0000; // LB_TARGET_CERTIFICATE
        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_le_bytes()); // SessionID
        body.extend_from_slice(&flags.to_le_bytes());
        field(&mut body, b"Cookie: msts=3735928559\r\n");
        field(&mut body, &utf16z("gdm-handover"));
        field(&mut body, &[0xAA; 256]);
        field(&mut body, &utf16z("{guid}"));
        field(&mut body, &[0x30; 40]);

        let mut packet = Vec::new();
        packet.extend_from_slice(&SEC_REDIRECTION_PKT.to_le_bytes());
        packet.extend_from_slice(&((4 + body.len()) as u16).to_le_bytes());
        packet.extend_from_slice(&body);
        packet
    }

    #[test]
    fn parses_a_gnome_style_redirect() {
        let r = parse_packet(&gnome_like_packet()).expect("parses");
        assert_eq!(r.session_id, 7);
        assert_eq!(r.routing_token().as_deref(), Some("3735928559"));
        assert_eq!(r.username.as_deref(), Some("gdm-handover"));
        assert_eq!(r.password.as_ref().map(Vec::len), Some(256));
        assert!(r.needs_rdstls());
        assert_eq!(r.plain_password(), None);
        assert_eq!(r.redirection_guid, Some(utf16z("{guid}")));
    }

    #[test]
    fn a_truncated_packet_is_an_error_not_a_panic() {
        let packet = gnome_like_packet();
        assert!(parse_packet(&packet[..40]).is_err());
    }

    #[test]
    fn auth_request_echoes_guid_and_password_verbatim() {
        let r = parse_packet(&gnome_like_packet()).expect("parses");
        let req = auth_request(&r);
        assert_eq!(&req[..6], &[1, 0, 2, 0, 1, 0]);
        let guid = utf16z("{guid}");
        assert_eq!(read_u16(&req, 6), Some(guid.len() as u16));
        assert_eq!(&req[8..8 + guid.len()], guid.as_slice());
        // Password is the last blob.
        assert_eq!(&req[req.len() - 256..], &[0xAA; 256]);
        assert_eq!(read_u16(&req, req.len() - 258), Some(256));
    }

    #[test]
    fn routing_token_without_prefix_is_kept_as_is() {
        let r = Redirect {
            session_id: 0,
            flags: LB_LOAD_BALANCE_INFO,
            load_balance_info: Some(b"abc\r\n".to_vec()),
            username: None,
            domain: None,
            password: None,
            redirection_guid: None,
        };
        assert_eq!(r.routing_token().as_deref(), Some("abc"));
        assert!(!r.needs_rdstls());
    }
}
