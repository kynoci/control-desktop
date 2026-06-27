# IronRDP — Features Not Yet Used

A working checklist of IronRDP capabilities this project does **not** use yet, so they
can be added one at a time. Ordered by value-for-effort.

**Currently enabled** (`src-tauri/Cargo.toml`):
`connector, session, graphics, input, pdu, svc, dvc, displaycontrol, core`

**Key wiring points** referenced below:
- `establish()` — `src-tauri/src/rdp.rs:428` — where static/dynamic channels are registered before connect.
- `build_config()` — `src-tauri/src/rdp.rs:518` — the connector `Config` (most off-switches live here).
- session loop — `src-tauri/src/rdp.rs:281` — where `ActiveStageOutput` variants are handled.
- Tauri command registry — `src-tauri/src/lib.rs:152`.
- wire protocol tags — `src-tauri/src/rdp.rs:41` (init / strip / status).

Legend: **Effort** S = small, M = medium, L = large.

---

## Tier 1 — High value, do these first

### [x] 1. Clipboard redirection (`cliprdr`) — text done
Copy/paste **text** between local machine and the remote session, both directions.
File copy (FileGroupDescriptorW) is intentionally deferred — overlaps item 2.

**Shipped:**
- `cliprdr` feature added to the `ironrdp` dep (`Cargo.toml`).
- `src-tauri/src/clipboard.rs` — `WebClipboardBackend` (a `CliprdrBackend`), text-only,
  advertises empty capabilities so the server never negotiates file transfer.
- `rdp.rs`: registers the CLIPRDR static channel in `establish()`; new `TAG_CLIPBOARD = 3`
  wire tag; `dispatch_clipboard()` turns queued `ClipboardMessage`s into PDUs; a `clip_rx`
  arm in the session `select!`; new `rdp_clipboard_offer` command.
- `App.vue`: tag-3 frames → `navigator.clipboard.writeText`; `offerLocalClipboard()` reads
  the local clipboard on canvas focus + window focus and calls `rdp_clipboard_offer`.

**Data flow:** remote copy → auto-paste → pushed to local clipboard; local copy → offered
on focus → answered when the remote requests it.

**Known limitations / follow-ups:**
- Text only (no files/images/HTML).
- `navigator.clipboard.readText()` is best-effort: may be blocked without focus/permission
  in the webview, in which case local→remote silently no-ops.
- Remote→local `writeText` fails silently if the app isn't focused at copy time (no caching
  + replay-on-focus yet).
- Not yet verified against a live RDP host (compiles clean; needs manual test).

### [ ] 2. Device redirection — folders / drives (`rdpdr`)
Share a local folder/drive into the session. Gives you file transfer "for free" and
aligns with the SFTP milestone (M4) on the roadmap.

- **Crate / feature:** add `rdpdr`; pull `ironrdp-rdpdr`.
- **Backend:** register the RDPDR channel in `establish()`. Map a chosen local path to a
  redirected drive.
- **Frontend:** a folder-picker per connection (or a global "shared folder" setting).
- **Effort:** L. RDPDR is the most involved channel (filesystem op emulation).
- **Notes:** can also carry printer / smart-card redirection later. Do folder sharing first.

---

## Tier 2 — Nice to have

### [x] 3. Audio playback (`rdpsnd`) — done
Stream remote sound to the local machine.

**Shipped:**
- `rdpsnd` feature added to the `ironrdp` dep (`Cargo.toml`).
- `src-tauri/src/audio.rs` — `WebAudioBackend` (an `RdpsndClientHandler`). The
  `Rdpsnd` SVC handles all protocol negotiation; we just advertise one format and
  receive PCM in `wave()`, forwarded to the webview as TAG_AUDIO frames.
- `rdp.rs`: `TAG_AUDIO = 4`; registers the RDPSND static channel in `establish()`;
  flipped `enable_audio_playback: true` in `build_config()`.
- `App.vue`: `playAudio()` — a WebAudio player that schedules each PCM chunk on a
  running playhead for gapless output; `AudioContext` per session, closed on tab close.

**Design note:** we advertise a **single** format (PCM 44.1 kHz / 16-bit / stereo).
The crate negotiates formats via an unordered `HashSet` intersection and the wave PDU
references a format by index, so multiple formats couldn't be mapped back to their
parameters. One format ⇒ index is always 0 and the parameters are known. The Rust
constants in `audio.rs` and the JS constants in `App.vue` must stay in sync.

**Known limitations / follow-ups:**
- Output only (no microphone / audio capture).
- If a host doesn't offer PCM 44.1k/16/stereo the intersection is empty and there's no
  audio — drop `SAMPLE_RATE` to 22050 in `audio.rs` + `App.vue` if that happens.
- `AudioContext` may start suspended until a user gesture; we call `resume()` on each
  chunk, and clicking into the session counts as the gesture.
- No volume/pitch handling (server volume PDUs are ignored).

**⚠️ Does NOT work against modern Windows 11 hosts (investigated 2026-06-20).**
Traced the full channel negotiation against a Win11 host: the `rdpsnd` static channel
joins correctly (id 1005) and `enable_audio_playback` requests sound, but the server
**never sends any rdpsnd PDU** — `get_formats()` is never called, zero waves. The host
opens only dynamic (DVC) channels (Video/Geometry/Input/Camera/DisplayControl/…) and
opens **no** audio channel at all; it never falls back to static rdpsnd. mstsc gets sound
on the same host because it implements Microsoft's dynamic audio path
(`AUDIO_PLAYBACK_DVC`), which IronRDP 0.x's `rdpsnd` (static-channel only) does not.
Conclusion: this code is correct and works against hosts that use the static rdpsnd
channel (older Windows, some Linux RDP servers), but **not modern Windows over DVC**.
**UPDATE (2026-06-20): `AUDIO_PLAYBACK_DVC` implemented and tested — host still silent.**
Implemented a DVC audio processor (`src-tauri/src/audio_dvc.rs`) for the dynamic
`AUDIO_PLAYBACK_DVC` channel, registered on `drdynvc` alongside DisplayControl, with the
frontend player sizing itself from a negotiated-format frame (`TAG_AUDIO_FORMAT`). Both
the static rdpsnd path and the DVC path are now wired. Result against the Win11 host:
- Static rdpsnd: server sends nothing (get_formats never called).
- DVC: server never opens `AUDIO_PLAYBACK_DVC` (verified via `ironrdp_dvc` debug — the
  host opens Video/Geometry/Input/Camera/MouseCursor/ECHO/etc. but no audio channel),
  even after dropping the static rdpsnd advertisement to mimic mstsc.
- `ClientInfo` correctly requests audio (`NO_AUDIO_PLAYBACK` absent on the wire).
- Clipboard (static channel) works on the same host, proving channel handling is fine.
- Both MMR (YouTube/Edge) and non-MMR (system sounds) are silent.
Conclusion: this host won't redirect audio to an IronRDP client over any channel it
supports, while mstsc gets sound — a server-side negotiation difference we can't bridge
without Microsoft's full multimedia stack. **Audio is PARKED.** Both code paths are kept
(they're correct and cost nothing; they'll work against hosts that actually drive
rdpsnd/AUDIO_PLAYBACK_DVC). Diagnostic logging has been reverted.

**FINAL (2026-06-22): switched to native cpal backend; host still won't transmit.**
Now uses IronRDP's official native backend `ironrdp-rdpsnd-native = "0.6"` (matches our
`rdpsnd 0.8`; `default-features = false` drops the Opus C-dependency). `rdp.rs` builds the
audio backend as `Box::new(ironrdp_rdpsnd_native::cpal::RdpsndBackend::new())`, which plays
remote audio straight to local speakers via cpal/WASAPI (bypasses the webview). The browser
paths in `audio.rs`/`audio_dvc.rs` are now unused (`#[allow(dead_code)]`). A local WebAudio
beep AND cpal output both confirmed working. Definitive test: a plain **system sound**
(Asterisk) with Output device = "Remote Audio", volume 100, audio services running — server
captured it to the Remote Audio endpoint — yet our client got **0 rdpsnd packets**. mstsc
gets sound on the same host. So the server refuses to transmit audio to a non-Microsoft
client; nothing on our side can change that. Use the mstsc (🖥) button for audio on this VM.
ChatGPT note for the record: it suggested `rdpsnd-native = "0.5"` + bump ironrdp to 0.16 —
wrong; 0.5 needs `rdpsnd 0.7` (we have 0.8) and bumping ironrdp breaks the vendored connector
patch. The correct pin is `0.6`.

### [ ] 4. Bulk compression
Compress the framebuffer / bulk data to cut bandwidth on slow links.

- **Where:** `compression_type: None` in `build_config()` (`rdp.rs:561`) — set an MPPC/RDP6 type.
- **Effort:** S (config) + verify decode path handles compressed updates.
- **Notes:** pairs well with item 6.

### [ ] 5. Graphics codecs — `qoi` / `qoiz`
QOI (and zstd-compressed QOI) codecs instead of raw RGBA strips — less data over the
Tauri channel and the wire.

- **Crate / feature:** add `qoi` and/or `qoiz` to the `ironrdp` features.
- **Where:** affects the graphics pipeline feeding `send_strip()` (`rdp.rs:492`).
- **Effort:** M.

### [ ] 6. Performance flags
Tell the server to disable wallpaper, themes, font smoothing, menu animations, etc. for
faster redraw on slow connections.

- **Where:** `performance_flags: PerformanceFlags::default()` in `build_config()` (`rdp.rs:564`).
- **Effort:** S. Possibly expose as a per-connection "optimize for slow network" toggle.

### [ ] 7. Remote pointer rendering
Render the actual remote cursor sprite instead of relying on the local OS cursor.

- **Where:** `enable_server_pointer: false` (`rdp.rs:557`); pointer updates are currently
  dropped at the `_ => {}` arm in the session loop (`rdp.rs:335`).
- **Frontend:** draw the cursor shape on/over the canvas.
- **Effort:** M.
- **Notes:** mainly matters if the remote app hides/changes the cursor (games, design tools).

---

## Tier 3 — Specialized / situational

### [ ] 8. Unicode keyboard input
Send characters directly as Unicode, covering keys with no set-1 scancode mapping.

- **Where:** the `input` crate already supports it; today only scancodes are sent
  (`SCANCODES` in `src/scancodes.ts`, encoded in `to_operations()` at `rdp.rs:79`).
- **Effort:** M.
- **Notes:** helps with international layouts / dead keys / IME.

### [ ] 9. Kerberos / domain authentication
Currently NTLM-only — `NoKdcNetworkClient` (`rdp.rs:105`) deliberately refuses KDC
round-trips. Real AD/domain logins need a working `sspi` network client.

- **Where:** replace `NoKdcNetworkClient` with an `sspi` network client.
- **Effort:** L. Only worth it if connecting to domain-joined hosts.

### [ ] 10. Multitransport (UDP)
RDP's UDP transport for lower latency / better performance.

- **Where:** `multitransport_flags: None` in `build_config()` (`rdp.rs:563`).
- **Effort:** L.

---

## Out of scope (don't bother)

These exist in IronRDP but don't fit a client app:

- **`server` / `acceptor`** — for *being* an RDP server, not connecting to one.
- **`echo`** — a test/diagnostic dynamic channel only.
</content>
</invoke>
