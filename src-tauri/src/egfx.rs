//! Graphics Pipeline (EGFX, MS-RDPEGFX) output for the embedded RDP client.
//!
//! Some servers never use the legacy bitmap/surface-command path at all.
//! `gnome-remote-desktop` is one: without the `SUPPORT_DYN_VC_GFX_PROTOCOL`
//! early-capability bit *and* a live EGFX channel it closes the connection during
//! Capabilities Exchange ("Client did not advertise support for the Graphics
//! Pipeline"). See `plan-egfx.md` for the full diagnosis.
//!
//! This module is **not** specific to one server: once the EGFX channel is up it
//! carries the whole screen for every host, Windows and Linux alike. What differs
//! is the subset of EGFX each server uses, so the handler has to cover the union:
//!
//! - `gnome-remote-desktop` streams RemoteFX **Progressive** (`WireToSurface2`),
//!   which `ironrdp-egfx` only `trace!`s. Decoded here with `ProgressiveDecoder`
//!   from `ironrdp-graphics`, which exists for this purpose but is never wired up
//!   upstream.
//! - **Windows** sends comparable content as Progressive, but puts UI, text and
//!   icons through **ClearCodec** (occasionally **Planar**) and leans on
//!   `SurfaceToCache`/`CacheToSurface` and `SurfaceToSurface` to move pixels it
//!   already sent. Ignoring any of those leaves stale blocks on screen — that was
//!   the white square over the Windows 11 sign-in UI.
//!
//! `ironrdp-egfx` decodes only `Uncompressed` and `Avc420` (and we ship no H.264
//! decoder, so AVC never arrives); its documented contract is that everything else
//! reaches the `GraphicsPipelineHandler` — `on_unhandled_pdu` for other codecs,
//! and a dedicated callback per surface/cache command. Implementing them here is
//! the intended use, not a workaround.
//!
//! Handling the copy commands is why every surface carries a **local RGBA
//! framebuffer**: they reference pixels a draw-and-forget client no longer has.
//! Every producer composites into that framebuffer, and the canvas is then painted
//! from it, so the two never drift.
//!
//! That framebuffer is also what makes the output *smooth*. EGFX addresses the
//! screen in small pieces — a progressive refresh of a 1000x662 desktop is 176
//! 64x64 tiles — and sending each one as its own channel message left the webview
//! doing 176 `putImageData` calls per frame, which stutters and shows half-painted
//! frames. Updates are instead accumulated between `StartFrame` and `EndFrame`,
//! merged back into as few rectangles as they will fold into, and only then sent.
//!
//! Everything ends up as `TAG_RECT` frames — the same wire format `vnc.rs` uses
//! for RFB dirty rectangles, so the canvas code is shared.

use std::collections::{HashMap, HashSet};

use ironrdp::graphics::clearcodec::ClearCodecDecoder;
use ironrdp::graphics::progressive::ProgressiveDecoder;
use ironrdp::graphics::rdp6::BitmapStreamDecoder;
use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineHandler, Surface};
use ironrdp_egfx::pdu::{
    CacheImportReplyPdu, CacheToSurfacePdu, CapabilitySet, Codec1Type, DeleteEncodingContextPdu,
    EvictCacheEntryPdu, GfxPdu, SolidFillPdu, SurfaceToCachePdu, SurfaceToSurfacePdu, WireToSurface1Pdu,
    WireToSurface2Pdu,
};
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::rdp::{merge_rects, send_rect, DirtyRect, TAG_INIT};

/// Progressive tiles are always 64x64 (MS-RDPRFX 2.2.2.3.4).
const TILE: u16 = 64;

/// A server-created surface: geometry, where it sits on the output, and its pixels.
struct SurfaceInfo {
    width: u16,
    height: u16,
    origin_x: u32,
    origin_y: u32,
    /// Mapped to the output. Windows also creates *offscreen* surfaces purely to
    /// stage cache content; drawing those to the canvas would paint garbage over
    /// the desktop.
    mapped: bool,
    /// RGBA, `width * height * 4`, row-major.
    pixels: Vec<u8>,
}

impl SurfaceInfo {
    fn new(width: u16, height: u16, mapped: bool) -> Self {
        Self {
            width,
            height,
            origin_x: 0,
            origin_y: 0,
            mapped,
            pixels: vec![0; usize::from(width) * usize::from(height) * 4],
        }
    }
}

/// One bitmap-cache slot. Its dimensions come from the `SurfaceToCache` that filled
/// it — `CacheToSurface` carries only destination points.
struct CacheEntry {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
}

/// Bridges `ironrdp-egfx` callbacks onto the frontend frame channel.
pub struct EgfxHandler {
    on_frame: Channel<InvokeResponseBody>,
    surfaces: HashMap<u16, SurfaceInfo>,
    /// Bitmap cache slots, filled by `SurfaceToCache`. The server owns eviction
    /// (`EvictCacheEntry`) and sizes the cache from the capability set it picked.
    cache: HashMap<u16, CacheEntry>,
    /// Tile state for RemoteFX Progressive, keyed internally by codec context id.
    /// Progressive frames are differential — later frames refine earlier ones — so
    /// this must live for the whole session, not per PDU.
    progressive: ProgressiveDecoder,
    /// ClearCodec glyph + V-bar caches. Also session-lived and stateful: a later
    /// rect may be nothing but a reference into these caches.
    clear: ClearCodecDecoder,
    /// RDP6 planar decoder (just a reusable scratch buffer, but session-lived to
    /// avoid reallocating per rect).
    planar: BitmapStreamDecoder,
    /// Last size announced to the frontend, so a repeated `ResetGraphics` at the
    /// same size doesn't clear the canvas for nothing.
    output_size: Option<(u16, u16)>,
    /// Whether the server has mapped any surface to the output. Until it does,
    /// every surface counts as visible: a server that draws without ever sending
    /// `MapSurfaceToOutput` would otherwise render nothing at all.
    any_mapped: bool,
    /// Event kinds already reported. EGFX events repeat every frame, so an
    /// un-deduplicated log would bury everything else.
    seen: HashSet<String>,
    /// Rects actually drawn, so "connected but black" is distinguishable from
    /// "drawing, but wrong".
    drawn: u64,
    /// Dirty rectangles for the frame in flight, per surface.
    pending: HashMap<u16, Vec<DirtyRect>>,
    /// Whether the server frames its updates. Until the first `EndFrame` arrives
    /// we paint immediately: a server that never framed anything would otherwise
    /// accumulate dirty rects that are never flushed, and show nothing at all.
    frames_updates: bool,
    /// Channel messages saved by merging, logged once so the batching is visible.
    coalesced: u64,
}

impl EgfxHandler {
    pub fn new(on_frame: Channel<InvokeResponseBody>) -> Self {
        Self {
            on_frame,
            surfaces: HashMap::new(),
            cache: HashMap::new(),
            progressive: ProgressiveDecoder::new(),
            clear: ClearCodecDecoder::new(),
            planar: BitmapStreamDecoder::default(),
            output_size: None,
            any_mapped: false,
            seen: HashSet::new(),
            drawn: 0,
            pending: HashMap::new(),
            frames_updates: false,
            coalesced: 0,
        }
    }

    /// Log `msg` the first time this kind of event is seen.
    fn note_once(&mut self, key: &str, msg: &str) {
        if self.seen.insert(key.to_owned()) {
            log::warn!("egfx: {msg}");
        }
    }

    /// Composite `src` (RGBA, `w` x `h`) into surface `surface_id` at (`x`, `y`)
    /// *within that surface*, then push the same pixels to the canvas if the
    /// surface is on screen. Anything hanging off the surface edge is cropped —
    /// progressive edge tiles rely on that.
    fn paint(&mut self, surface_id: u16, x: u16, y: u16, w: u16, h: u16, src: &[u8], what: &str) {
        if !self.surfaces.contains_key(&surface_id) {
            // A server may draw to a surface it never announced; fall back to a
            // full-output surface rather than dropping the frame.
            let Some((ow, oh)) = self.output_size else {
                self.note_once(
                    "no-surface",
                    &format!("draw for unknown surface {surface_id} before any output size, dropped"),
                );
                return;
            };
            self.note_once(
                "implicit-surface",
                &format!("drawing to surface {surface_id}, which was never created; assuming full output"),
            );
            self.surfaces.insert(surface_id, SurfaceInfo::new(ow, oh, true));
        }

        let any_mapped = self.any_mapped;
        let visible;
        let (origin_x, origin_y);
        let (cw, ch);
        let out;
        {
            let Some(surface) = self.surfaces.get_mut(&surface_id) else {
                return;
            };
            if x >= surface.width || y >= surface.height {
                return;
            }
            cw = w.min(surface.width - x);
            ch = h.min(surface.height - y);
            if cw == 0 || ch == 0 {
                return;
            }

            let src_stride = usize::from(w) * 4;
            if src.len() < src_stride * usize::from(h) {
                log::warn!(
                    "egfx: {what} gave {} bytes for {w}x{h}, expected {}",
                    src.len(),
                    src_stride * usize::from(h)
                );
                return;
            }

            let dst_stride = usize::from(surface.width) * 4;
            let row_bytes = usize::from(cw) * 4;
            let mut buf = Vec::with_capacity(row_bytes * usize::from(ch));
            for row in 0..usize::from(ch) {
                let s = row * src_stride;
                let row_px = &src[s..s + row_bytes];
                let d = (usize::from(y) + row) * dst_stride + usize::from(x) * 4;
                surface.pixels[d..d + row_bytes].copy_from_slice(row_px);
                buf.extend_from_slice(row_px);
            }

            out = buf;
            visible = surface.mapped || !any_mapped;
            origin_x = surface.origin_x;
            origin_y = surface.origin_y;
        }

        if !visible {
            return;
        }

        // Inside a frame, remember the rectangle and let `on_frame_complete` send
        // it along with the rest; the pixels are already in the framebuffer.
        if self.frames_updates {
            self.pending.entry(surface_id).or_default().push(DirtyRect {
                x,
                y,
                w: cw,
                h: ch,
            });
            if self.drawn == 0 {
                log::info!("egfx: first rect queued — {cw}x{ch} at ({x},{y}) via {what}");
            }
            return;
        }

        let dx = clamp_u16(u32::from(x) + origin_x);
        let dy = clamp_u16(u32::from(y) + origin_y);
        self.emit(dx, dy, cw, ch, &out, what);
    }

    /// Send everything painted during the frame that just ended, as few messages as
    /// the rectangles will merge into.
    fn flush_frame(&mut self) {
        let surfaces: Vec<u16> = self.pending.keys().copied().collect();
        for surface_id in surfaces {
            let Some(rects) = self.pending.remove(&surface_id) else {
                continue;
            };
            if rects.is_empty() {
                continue;
            }

            let before = rects.len();
            let merged = merge_rects(rects);
            self.coalesced += (before - merged.len()) as u64;
            let Some(&(origin_x, origin_y)) = self
                .surfaces
                .get(&surface_id)
                .map(|s| (s.origin_x, s.origin_y))
                .as_ref()
            else {
                continue;
            };

            for rect in merged {
                // Read back from the framebuffer: a merged rectangle spans pixels
                // from several updates, and may cover some that never changed.
                let Some((w, h, pixels)) = self.read_rect(surface_id, rect.x, rect.y, rect.w, rect.h) else {
                    continue;
                };
                let dx = clamp_u16(u32::from(rect.x) + origin_x);
                let dy = clamp_u16(u32::from(rect.y) + origin_y);
                self.emit(dx, dy, w, h, &pixels, "frame");
            }
        }
    }

    /// Read a rectangle out of a surface as a tightly packed RGBA buffer, cropped
    /// to the surface. Returns the cropped size alongside the pixels.
    fn read_rect(&self, surface_id: u16, left: u16, top: u16, w: u16, h: u16) -> Option<(u16, u16, Vec<u8>)> {
        let surface = self.surfaces.get(&surface_id)?;
        if left >= surface.width || top >= surface.height {
            return None;
        }
        let cw = w.min(surface.width - left);
        let ch = h.min(surface.height - top);
        if cw == 0 || ch == 0 {
            return None;
        }

        let stride = usize::from(surface.width) * 4;
        let row_bytes = usize::from(cw) * 4;
        let mut out = Vec::with_capacity(row_bytes * usize::from(ch));
        for row in 0..usize::from(ch) {
            let s = (usize::from(top) + row) * stride + usize::from(left) * 4;
            out.extend_from_slice(&surface.pixels[s..s + row_bytes]);
        }
        Some((cw, ch, out))
    }

    /// Size of a surface, copied out so a decoder can run without holding a borrow.
    /// Falls back to the output size for a surface the server never announced.
    fn geometry(&self, surface_id: u16) -> Option<(u16, u16)> {
        self.surfaces
            .get(&surface_id)
            .map(|s| (s.width, s.height))
            .or(self.output_size)
            .filter(|&(w, h)| w > 0 && h > 0)
    }

    /// Send one RGBA rect and count it.
    fn emit(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: &[u8], what: &str) {
        if let Err(e) = send_rect(&self.on_frame, x, y, w, h, rgba) {
            log::warn!("egfx: {what} send failed: {e}");
            return;
        }
        self.drawn += 1;
        if self.drawn == 1 {
            log::info!("egfx: first rect drawn — {w}x{h} at ({x},{y}) via {what}");
        }
    }

    /// Codecs `ironrdp-egfx` forwards to us rather than decoding: ClearCodec (all
    /// of Windows' text, icons and UI chrome) and RDP6 Planar.
    fn decode_wire_to_surface1(&mut self, pdu: &WireToSurface1Pdu) {
        let rect = &pdu.destination_rectangle;
        let w = rect.right.saturating_sub(rect.left);
        let h = rect.bottom.saturating_sub(rect.top);
        if w == 0 || h == 0 {
            return;
        }

        match pdu.codec_id {
            // ClearCodec decodes to BGRA; the canvas wants RGBA.
            Codec1Type::ClearCodec => {
                let decoded = self.clear.decode(&pdu.bitmap_data, w, h);
                // The decoder paints what it can and reports the rest here rather
                // than failing the rect; those notes repeat per frame, so dedupe.
                for note in self.clear.take_diagnostics() {
                    self.note_once(&format!("clear-note:{note}"), &format!("ClearCodec: {note}"));
                }
                match decoded {
                    Ok(mut px) => {
                        bgra_to_rgba_in_place(&mut px);
                        self.paint(pdu.surface_id, rect.left, rect.top, w, h, &px, "clear");
                    }
                    Err(e) => self.note_once("clear-err", &format!("ClearCodec decode failed: {e}")),
                }
            }
            Codec1Type::Planar => {
                let mut rgb = Vec::new();
                match self
                    .planar
                    .decode_bitmap_stream_to_rgb24(&pdu.bitmap_data, &mut rgb, usize::from(w), usize::from(h))
                {
                    Ok(()) => {
                        let rgba = rgb24_to_rgba(&rgb);
                        self.paint(pdu.surface_id, rect.left, rect.top, w, h, &rgba, "planar");
                    }
                    Err(e) => self.note_once("planar-err", &format!("Planar decode failed: {e}")),
                }
            }
            other => {
                let kind = format!("{other:?}");
                self.note_once(
                    &format!("codec:{kind}"),
                    &format!("no decoder for EGFX codec {kind}; those areas stay stale"),
                );
            }
        }
    }
}

impl GraphicsPipelineHandler for EgfxHandler {
    fn on_capabilities_confirmed(&mut self, caps: &CapabilitySet) {
        log::info!("egfx: server confirmed capability set {caps:?}");
    }

    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        // The tile grid is sized per surface; a graphics reset invalidates all of
        // it, and stale tile state would decode later frames into the wrong place.
        self.progressive.reset();

        let (w, h) = (clamp_u16(width), clamp_u16(height));
        if self.output_size == Some((w, h)) {
            return;
        }
        self.output_size = Some((w, h));

        let mut init = Vec::with_capacity(5);
        init.push(TAG_INIT);
        init.extend_from_slice(&w.to_le_bytes());
        init.extend_from_slice(&h.to_le_bytes());
        let _ = self.on_frame.send(InvokeResponseBody::Raw(init));
        log::info!("egfx: output reset to {w}x{h}");
    }

    fn on_surface_created(&mut self, surface: &Surface) {
        let mut info = SurfaceInfo::new(surface.width, surface.height, surface.is_mapped);
        info.origin_x = surface.output_origin_x;
        info.origin_y = surface.output_origin_y;
        self.any_mapped |= surface.is_mapped;
        self.surfaces.insert(surface.id, info);
    }

    fn on_surface_mapped(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        self.any_mapped = true;
        if let Some(info) = self.surfaces.get_mut(&surface_id) {
            info.origin_x = origin_x;
            info.origin_y = origin_y;
            info.mapped = true;
        }
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
    }

    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        // Empty when ironrdp-egfx had no decoder for the codec.
        if update.data.is_empty() {
            let codec = format!("{:?}", update.codec_id);
            self.note_once(
                &format!("empty:{codec}"),
                &format!("server sent {codec} but it decoded to no pixels — that area stays blank"),
            );
            return;
        }
        let expected = usize::from(update.width) * usize::from(update.height) * 4;
        if update.data.len() != expected {
            log::warn!(
                "egfx: dropping {}x{} update with {} bytes (expected {expected})",
                update.width,
                update.height,
                update.data.len()
            );
            return;
        }

        let (x, y) = (update.destination_rectangle.left, update.destination_rectangle.top);
        let (w, h) = (update.width, update.height);
        let data = update.data.clone();
        self.paint(update.surface_id, x, y, w, h, &data, "bitmap");
    }

    /// RemoteFX Progressive. `ironrdp-egfx` only traces this PDU, so decoding and
    /// compositing happen here.
    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        let Some((width, height)) = self.geometry(pdu.surface_id) else {
            self.note_once(
                "wire2-nosurface",
                &format!("progressive data for unknown surface {}, dropped", pdu.surface_id),
            );
            return;
        };

        let tiles = match self
            .progressive
            .decode_bitmap(pdu.codec_context_id, width, height, &pdu.bitmap_data)
        {
            Ok(tiles) => tiles,
            Err(e) => {
                // Errors here tend to repeat every frame, so don't spam the log.
                self.note_once("wire2-err", &format!("progressive decode failed: {e}"));
                return;
            }
        };

        for tile in tiles {
            // Edge tiles come back full-size; `paint` crops them to the surface.
            let x = tile.x_idx.saturating_mul(TILE);
            let y = tile.y_idx.saturating_mul(TILE);
            self.paint(pdu.surface_id, x, y, TILE, TILE, &tile.pixels, "progressive");
        }
    }

    fn on_solid_fill(&mut self, pdu: &SolidFillPdu) {
        // `fill_pixel` is BGRA on the wire; the canvas wants RGBA.
        let px = [pdu.fill_pixel.r, pdu.fill_pixel.g, pdu.fill_pixel.b, 0xff];

        for rect in pdu.rectangles.clone() {
            let (w, h) = (
                rect.right.saturating_sub(rect.left),
                rect.bottom.saturating_sub(rect.top),
            );
            if w == 0 || h == 0 {
                continue;
            }
            let rgba = px.repeat(usize::from(w) * usize::from(h));
            self.paint(pdu.surface_id, rect.left, rect.top, w, h, &rgba, "solid fill");
        }
    }

    /// Blit within or between surfaces — how a server moves pixels it already sent
    /// (scrolling, window drags). The source is copied out first so an overlapping
    /// same-surface copy still reads the original pixels.
    fn on_surface_to_surface(&mut self, pdu: &SurfaceToSurfacePdu) {
        let rect = &pdu.source_rectangle;
        let w = rect.right.saturating_sub(rect.left);
        let h = rect.bottom.saturating_sub(rect.top);
        let Some((w, h, pixels)) = self.read_rect(pdu.source_surface_id, rect.left, rect.top, w, h) else {
            self.note_once(
                "s2s-nosource",
                &format!("SurfaceToSurface from unknown surface {}, dropped", pdu.source_surface_id),
            );
            return;
        };

        for point in pdu.destination_points.clone() {
            self.paint(pdu.destination_surface_id, point.x, point.y, w, h, &pixels, "surface copy");
        }
    }

    fn on_surface_to_cache(&mut self, pdu: &SurfaceToCachePdu) {
        let rect = &pdu.source_rectangle;
        let w = rect.right.saturating_sub(rect.left);
        let h = rect.bottom.saturating_sub(rect.top);
        let Some((width, height, pixels)) = self.read_rect(pdu.surface_id, rect.left, rect.top, w, h) else {
            self.note_once(
                "s2c-nosource",
                &format!("SurfaceToCache from unknown surface {}, dropped", pdu.surface_id),
            );
            return;
        };
        self.cache.insert(pdu.cache_slot, CacheEntry { width, height, pixels });
    }

    fn on_cache_to_surface(&mut self, pdu: &CacheToSurfacePdu) {
        let Some(entry) = self.cache.get(&pdu.cache_slot) else {
            self.note_once(
                "cache-miss",
                &format!("CacheToSurface for empty slot {}, that area stays stale", pdu.cache_slot),
            );
            return;
        };
        let (w, h) = (entry.width, entry.height);
        let pixels = entry.pixels.clone();

        for point in pdu.destination_points.clone() {
            self.paint(pdu.surface_id, point.x, point.y, w, h, &pixels, "cache");
        }
    }

    fn on_evict_cache_entry(&mut self, pdu: &EvictCacheEntryPdu) {
        self.cache.remove(&pdu.cache_slot);
    }

    /// The server believes we restored slots from a persistent cache. We never send
    /// a cache-import offer, so a non-empty reply means it expects content we don't
    /// have.
    fn on_cache_import_reply(&mut self, pdu: &CacheImportReplyPdu) {
        if !pdu.cache_slots.is_empty() {
            self.note_once(
                "cache-import",
                &format!(
                    "server imported {} persistent cache slots we don't have",
                    pdu.cache_slots.len()
                ),
            );
        }
    }

    /// The server is dropping progressive tile state for this context; ours must go
    /// too, or the next frame decodes against stale tiles. Note the crate dispatches
    /// this to its own callback, *not* to `on_unhandled_pdu`.
    fn on_delete_encoding_context(&mut self, pdu: &DeleteEncodingContextPdu) {
        self.progressive.delete_context(pdu.codec_context_id);
    }

    fn on_unhandled_pdu(&mut self, pdu: &GfxPdu) {
        match pdu {
            GfxPdu::WireToSurface1(p) => self.decode_wire_to_surface1(p),
            _ => self.note_once("unhandled:other", "unhandled EGFX PDU"),
        }
    }

    /// End of a logical frame: everything painted since the last one goes out now.
    fn on_frame_complete(&mut self, _frame_id: u32) {
        if !self.frames_updates {
            // First framed update: from here on, batch.
            self.frames_updates = true;
            return;
        }
        self.flush_frame();
    }

    fn on_close(&mut self) {
        // Whatever the last frame painted still deserves to reach the canvas.
        self.flush_frame();
        log::info!(
            "egfx: channel closed after {} rects ({} merged away)",
            self.drawn,
            self.coalesced
        );
    }
}

/// Swap B and R in place, turning a BGRA buffer into RGBA.
fn bgra_to_rgba_in_place(px: &mut [u8]) {
    for chunk in px.chunks_exact_mut(4) {
        chunk.swap(0, 2);
    }
}

/// Widen packed RGB24 to RGBA with an opaque alpha.
fn rgb24_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
    for px in rgb.chunks_exact(3) {
        out.extend_from_slice(&[px[0], px[1], px[2], 0xff]);
    }
    out
}

/// Clamp an EGFX u32 coordinate onto the u16 the frame protocol carries.
fn clamp_u16(v: u32) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}
