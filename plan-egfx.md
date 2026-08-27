# Plan: RDP Graphics Pipeline (EGFX) support — required for GNOME / Ubuntu

Goal: make the embedded IronRDP client able to connect to **GNOME Remote Desktop**
(Ubuntu). Today the connection authenticates successfully and is then closed by the
server before any picture arrives.

---

## 1. Root cause (recorded 2026-08-27)

**`gnome-remote-desktop` only sends its screen over the RDP Graphics Pipeline
(EGFX, `MS-RDPEGFX`). It has no legacy bitmap / surface-command output path.
Our client never advertises EGFX, so the server closes the connection at
Capabilities Exchange.**

This is a **client-side gap, not a server misconfiguration.** No GNOME setting fixes
it, and it is unrelated to Desktop Sharing vs Remote Login — both are the same
FreeRDP-based server with the same requirement.

### Evidence

Server side, `journalctl --user -u gnome-remote-desktop -f` during a connect attempt:

```
gnome-remote-desktop-daemon: [RDP] Client did not advertise support for the
                             Graphics Pipeline, closing connection
[ERROR][com.freerdp.core.peer] - [rdp_peer_handle_state_demand_active]:
   [CONNECTION_STATE_CAPABILITIES_EXCHANGE_DEMAND_ACTIVE]
   freerdp_peer::Capabilities() callback failed
[RDP] Network or intentional disconnect, stopping session
```

Client side, `%LOCALAPPDATA%\com.kynoci.remote\logs\Control Desktop.log`:

```
early_capability_flags: ClientEarlyCapabilityFlags(SUPPORT_ERR_INFO_PDU |
    WANT_32_BPP_SESSION | STRONG_ASYMMETRIC_KEYS | VALID_CONNECTION_TYPE |
    SUPPORT_SKIP_CHANNELJOIN)
...
Received message=ServerDeactivateAll(ServerDeactivateAll)
Skipping Server Deactivate All PDU received during Capabilities Exchange,
    awaiting Server Demand Active
ERROR rdp session ended with error: authentication/finalize failed:
    reason: received disconnect provider ultimatum: UserRequested
```

`SUPPORT_DYN_VC_GFX_PROTOCOL` (`0x0100`) is **absent** from the advertised flags.
Meanwhile the server's negotiation response *did* advertise
`DYNVC_GFX_PROTOCOL_SUPPORTED` — it was waiting for us to ask.

The flag is absent because `ironrdp-connector` **hardcodes** the early capability
flag list with no way to opt in (`vendor/ironrdp-connector/src/connection.rs:702-714`;
still hardcoded upstream in 0.10 at `connection.rs:835-848`).

### What the failure looks like, stage by stage

| Stage | Result |
| --- | --- |
| TCP + X.224 negotiation | OK — server confirmed `HYBRID` |
| TLS upgrade | OK |
| CredSSP / NLA (NTLM) | OK — full round trip, `pub_key_auth` verified |
| MCS connect + channel join | OK — channels 1004/1005, io 1003 |
| `ClientInfoPdu` (autologon) | OK — sent |
| Licensing | OK — server skipped it |
| **Capabilities Exchange** | **server sends `ServerDeactivateAll`, then a Disconnect Provider Ultimatum (`UserRequested`)** |

Credentials, TLS, ports and firewall are all fine. The server logs us in and then
declines to hand over a desktop.

### Dead ends ruled out along the way

- **Not** a port / NAT / firewall problem. An earlier `os error 10061` was a
  *separate*, transient failure caused by `gnome-remote-desktop` restarting while
  its settings were being toggled. The forward `22389 → 3389` is fine.
- **Not** credentials — CredSSP completes every time.
- **Not** a locked screen, not Xorg (`XDG_SESSION_TYPE=wayland`), not PipeWire.
- **Not** a competing VNC client (that was a different VM entirely).
- **Not** the GNOME "session handoff" this codebase already retries for. The
  `is_handoff_disconnect` retry loop in `rdp.rs` matches this error text and burns
  all 6 attempts on a condition that retrying can never fix.

---

## 2. What has to change

### 2.1 Upgrade the IronRDP stack (prerequisite)

`ironrdp-egfx 0.3.0` requires **`ironrdp-pdu 0.9`, `ironrdp-dvc 0.8`,
`ironrdp-graphics 0.9`**. We are on **pdu 0.8 / dvc 0.6 / graphics 0.8** via
`ironrdp 0.15`. Those are semver-incompatible: Cargo would link two copies of
`ironrdp-pdu` and the `DvcProcessor` types would not unify.

So: `ironrdp 0.15 → 0.17`, `ironrdp-tokio 0.9 → 0.10`. Riskiest step; land it
alone with `cargo check` green before anything else.

### 2.2 Re-vendor `ironrdp-connector` — the patch shrinks

The existing local patch (tolerate `ServerDeactivateAll` during Capabilities
Exchange) **has landed upstream** in connector 0.10, with the same reasoning:

```
connection_activation.rs:143  // Some servers (e.g. GNOME Remote Desktop) send a
                              // ServerDeactivateAll PDU ...
```

But 0.10 still hardcodes the early capability flags. So the vendored crate is
re-based on 0.10 and its patch becomes a single line:

```rust
| ClientEarlyCapabilityFlags::SUPPORT_DYN_VC_GFX_PROTOCOL   // 0x0100
```

Net: vendor goes from a behaviour patch to a one-line capability patch. Worth
upstreaming as a `connector::Config` knob eventually.

### 2.3 Register the EGFX dynamic channel

Add `ironrdp-egfx = "0.3"` with **default features only** — its H.264 support sits
behind the optional `openh264*` features, and leaving them off preserves the
no-C-toolchain build rule (same reason we use `ring` over aws-lc).

`GraphicsPipelineClient` implements `DvcProcessor` + `DvcClientProcessor`, so it
plugs in exactly like Display Control already does in `establish()`:

```rust
let drdynvc = DrdynvcClient::new()
    .with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())))
    .with_dynamic_channel(GraphicsPipelineClient::new(handler, None)); // None = no H.264
```

### 2.4 Implement `GraphicsPipelineHandler` — the only real new code

The crate does all the decoding and hands us:

```rust
BitmapUpdate { destination_rectangle, data: Vec<u8> /* RGBA */, width, height, .. }
```

**That is already our tag-3 wire format.** `[3][x u16][y u16][w u16][h u16][rgba…]`
exists for VNC and `handleFrame` in `App.vue` already draws it. So:

- `on_bitmap_update` → emit **tag 3**. No frontend change, no new protocol.
- `on_reset_graphics(w, h)` / `on_surface_mapped` → emit **tag 0** to size the canvas.
- The handler runs inside the DVC processor (called from `active_stage.process`), so
  it needs a cloned `Channel` — the same pattern `WebClipboardBackend` already uses.

The legacy `GraphicsUpdate` / `send_strip` path stays for Windows hosts. EGFX is
added *alongside* it, not instead of it.

### 2.5 Capability sets and frame acknowledgement

The default `capabilities()` advertises V10.7 / V8.1 with AVC, but the crate filters
AVC out automatically when no H.264 decoder is configured and falls back to V8-only.
Correct for GNOME (it will use RemoteFX progressive / uncompressed). Override
explicitly so negotiation is predictable, and verify `FrameAcknowledge` is emitted —
GNOME throttles if frames go unacknowledged.

### 2.6 Cleanups this investigation exposed

- `is_handoff_disconnect` (`rdp.rs:586`) matches this failure and wastes 6 connect
  attempts on something retrying cannot fix.
- The friendly "server kept dropping the session…" message (`rdp.rs:293`) is
  **unreachable dead code**: on the final attempt the `attempt < MAX_ATTEMPTS` guard
  is false, so the match arm below returns the raw error first. That is why the UI
  shows raw IronRDP text instead of guidance.

---

## 3. Sequencing

1. Upgrade to `ironrdp 0.17` + `ironrdp-tokio 0.10`; `cargo check` green.
2. Re-vendor connector 0.10 with the one-line EGFX capability flag.
3. Add `ironrdp-egfx`, register the channel, implement the handler.
4. Fix the retry-loop / dead-code issues.
5. Test against Ubuntu (GNOME) **and** a Windows host, to confirm the legacy path
   did not regress.

---

## 4. What was actually implemented (steps 1-4 done)

- **`ironrdp 0.15 → 0.17`**, `ironrdp-tokio 0.9 → 0.10`. API churn handled in
  `rdp.rs`: `ActiveStage::new` → `ActiveStageBuilder`; `ActiveStageOutput::DeactivateAll`
  is now a unit variant, so `reactivate()` builds a fresh sequence from
  `ConnectionResult::activation_factory`; `ConnectionActivationState::Finalized`
  no longer carries the channel ids, so they are threaded in from the destructured
  `ConnectionResult`; new `ClipboardMessage::SendInitiateFileCopy` variant ignored
  alongside the other file-transfer messages.
- **`ironrdp-bulk 0.1`** added as a direct dependency. 0.17 gained bulk
  decompression, and rebuilding the fast-path processor in `reactivate()` would
  otherwise silently drop the decompressor — every compressed server PDU after a
  reactivation would fail to decode. `to_bulk_compression_type` mirrors the
  crate-private helper of the same name in `ironrdp-session`.
- **Connector re-vendored at 0.10** carrying only the one-line EGFX capability
  flag (`PATCH (Control Desktop)` marks it).
- **`ironrdp-egfx 0.3`, default features only** (no `openh264`, so the
  no-C-toolchain rule holds), registered as a second dynamic channel beside
  Display Control in `establish()`.
- **New `src-tauri/src/egfx.rs`** implementing `GraphicsPipelineHandler`:
  `on_reset_graphics` → tag 0, `on_bitmap_updated` / `on_solid_fill` → tag 3 rects,
  surface origins tracked from `on_surface_created` / `on_surface_mapped` because
  bitmap rectangles are surface-relative. `SurfaceToSurface` and `CacheToSurface`
  are **not rendered** — they need a per-surface local framebuffer (the way
  `vnc.rs` keeps one for CopyRect); a single warning is logged if a server uses
  them. No server we target does.

### Wire-protocol change: RDP clipboard moved from tag 3 to tag 7

Reusing tag 3 for EGFX rects surfaced a **pre-existing collision**: `rdp.rs` used
tag 3 for clipboard while `vnc.rs` used it for rects, and `handleFrame` in
`App.vue` had two `tag === 3` branches. The rect branch matched first, so the
clipboard branch was unreachable and remote-copied text never reached the webview.

Fixed by moving RDP clipboard to **tag 7**, leaving tag 3 as "arbitrary dirty
rectangle" for both protocols. `TAG_CLIPBOARD` in `rdp.rs` and the `handleFrame`
branch in `App.vue` must stay in sync.

### Second vendor patch: decline the MCS message channel

First test after the EGFX work got **past** Capabilities Exchange — the server no
longer hangs up — but then stalled with the log ending at the Client Info PDU and
the UI stuck on "Signing in as engineer (NLA/CredSSP)…".

Cause: connector 0.10 added a `ConnectTimeAutoDetection` state and started
requesting the MCS **message channel** unconditionally. Its own comment asserts

> "With a message channel the server always sends a PDU next in this phase … so
> waiting here cannot stall."

`gnome-remote-desktop` disproves that. It allocates the channel
(`mcs_message_channel_id: 1006` in the Connect Response) and then sends nothing,
so the connector blocks in `read_pdu()` forever. Connector 0.9 never requested the
channel (`message_channel: None` in the old logs) and went straight to licensing,
which is why this path used to work.

Patched in the vendored copy: `message_channel: None`, and
`SUPPORT_NET_CHAR_AUTODETECT` dropped from the early capability flags since
auto-detect rides that channel. We use neither auto-detect, multitransport nor
heartbeat, so nothing is lost. Remove once upstream stops waiting unconditionally.

The vendored connector now carries **three** `PATCH (Control Desktop)` markers:
add EGFX capability flag, decline message channel, drop auto-detect flag.

### Third gap: ironrdp-egfx decodes almost nothing

With the message channel declined, the connection completed —
`Connected with success`, `negotiated desktop 1000x662`,
`egfx: output reset to 1280x800` — but the canvas stayed black.

`ironrdp-egfx 0.3.0` decodes exactly **two** codecs:

| Codec | Handling in ironrdp-egfx 0.3 |
| --- | --- |
| `Uncompressed` (0x0) | decoded |
| `Avc420` (0xb) | decoded, but only with an H.264 decoder configured |
| `RemoteFx` (0x3), `ClearCodec` (0x8), `Planar` (0xa), `Alpha` (0xc), `Avc444`/`v2` | forwarded to `on_unhandled_pdu`, dropped |
| **`RemoteFxProgressive` (0x9)** — arrives as `WireToSurface2` | `trace!("WireToSurface2 (progressive codec)")` and handed to `on_wire_to_surface2`. **Never decoded.** |

Since we ship no H.264 decoder the crate negotiates a non-AVC capability set, and
gnome-remote-desktop then streams RemoteFX Progressive — straight into the
un-decoded path. Channel healthy, frames acknowledged, zero pixels.

**Fix:** decode progressive ourselves. `ironrdp-graphics 0.9` already ships a
`ProgressiveDecoder` built for precisely this ("High-level progressive bitmap
decoder for EGFX WireToSurface2 processing"; its source even names GNOME Remote
Desktop in a comment about servers that omit the CONTEXT block after the first
frame). `ironrdp-egfx` simply never calls it. `egfx.rs` now owns a
`ProgressiveDecoder`, feeds `WireToSurface2Pdu.bitmap_data` through
`decode_bitmap`, and emits each returned 64x64 tile as a tag-3 rect.

Details that matter:

- Progressive frames are **differential** — later frames refine earlier ones — so
  the decoder is session-lived, not per-PDU.
- Tile state is keyed by `codec_context_id`; `DeleteEncodingContext` must drop ours
  too, and `ResetGraphics` resets all of it, or later frames decode against stale
  tiles.
- The tile grid is sized from the **surface**, so surface dimensions are tracked
  from `CreateSurface` (previously only origins were).
- Edge tiles hang off the surface. The decoder still returns a full 64x64 tile, so
  it is cropped before drawing — otherwise rows smear.

### Fourth gap: wrong YCbCr scale in the progressive reconstruction

With progressive decoding wired up the desktop finally **rendered** — correct
geometry, icons and layout — but the colours were destroyed: the purple wallpaper
came through black, highlights blew out to pure red.

`ironrdp-graphics 0.9.0`'s `TileState::reconstruct_to_rgba` hand-rolls a plain
BT.601 conversion:

```rust
let y = i32::from(y_buf[i]) + 128;
let r = y + ((cr * 91881 + 32768) >> 16);   // >> 16 only
```

That is correct only for coefficients already in a 0..255 domain. **RemoteFX's DWT
output is scaled by 2^5**, so the real conversion offsets by `128 * 32 = 4096` and
shifts right by `16 + 5`. The proof is in-tree: this crate's *legacy* RemoteFX path
delegates to `yuv`'s RDP-specific variant, which does exactly that —

```rust
let yy = ((y + 4096) as i32) * Y_SCALE;   // yuv-0.8.14/src/rdp.rs
let r  = qrshr_n::<21, 8>(yy + B_Y * v);  // shift 21 = 16 + 5
```

— and the crate's own dequantized test vectors run to ±4096, far outside 0..255.
Upstream's encoder (`rgba_to_ycbcr`) is unscaled too, so its round-trip tests pass
while every real server frame saturates: anything below mid-grey clamps to black,
anything above blows out along whichever chroma dominates (hence *red*).

**Fix:** vendored `ironrdp-graphics`, with `reconstruct_to_rgba` delegating to
`color_conversion::ycbcr_to_rgba` — the same conversion the working legacy
RemoteFX path already uses — rather than hand-rolling the math a second time. One
function changed; `clamp_u8` becomes unused and is marked `#[allow(dead_code)]` to
keep the diff to a single function.

This is the **third** vendored crate. Grep `PATCH (Control Desktop)` for every
local change.

### Still to do

Step 5 — test against Ubuntu (GNOME) and a Windows host.
