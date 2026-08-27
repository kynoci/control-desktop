//! Graphics Pipeline (EGFX, MS-RDPEGFX) output for the embedded RDP client.
//!
//! Some servers never use the legacy bitmap/surface-command path at all.
//! `gnome-remote-desktop` is one: without the `SUPPORT_DYN_VC_GFX_PROTOCOL`
//! early-capability bit *and* a live EGFX channel it closes the connection during
//! Capabilities Exchange ("Client did not advertise support for the Graphics
//! Pipeline"). See `plan-egfx.md` for the full diagnosis.
//!
//! `ironrdp-egfx` owns the channel state machine and `FrameAcknowledge`, and
//! decodes two codecs: uncompressed and AVC420. It does **not** decode RemoteFX
//! Progressive, which arrives as `WireToSurface2` and is what GNOME streams once
//! AVC is off the table (we ship no H.264 decoder). That path is decoded here with
//! `ironrdp-graphics`' `ProgressiveDecoder`, which exists for exactly this purpose
//! but is never wired up by `ironrdp-egfx`.
//!
//! Everything ends up as `TAG_RECT` frames — the same wire format `vnc.rs` uses
//! for RFB dirty rectangles, so the canvas code is shared.

use std::collections::{HashMap, HashSet};

use ironrdp::graphics::progressive::ProgressiveDecoder;
use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineHandler, Surface};
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, CapabilitySet, GfxPdu, SolidFillPdu, SurfaceToSurfacePdu, WireToSurface2Pdu,
};
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::rdp::{send_rect, TAG_INIT};

/// Progressive tiles are always 64x64 (MS-RDPRFX 2.2.2.3.4).
const TILE: u16 = 64;

/// What we need to remember about a server-created surface: its size, to size the
/// progressive tile grid, and where it sits on the output, because both bitmap
/// updates and tiles are addressed relative to the surface.
#[derive(Clone, Copy)]
struct SurfaceInfo {
    width: u16,
    height: u16,
    origin_x: u32,
    origin_y: u32,
}

/// Bridges `ironrdp-egfx` callbacks onto the frontend frame channel.
pub struct EgfxHandler {
    on_frame: Channel<InvokeResponseBody>,
    surfaces: HashMap<u16, SurfaceInfo>,
    /// Tile state for RemoteFX Progressive, keyed internally by codec context id.
    /// Progressive frames are differential — later frames refine earlier ones — so
    /// this must live for the whole session, not per PDU.
    progressive: ProgressiveDecoder,
    /// Last size announced to the frontend, so a repeated `ResetGraphics` at the
    /// same size doesn't clear the canvas for nothing.
    output_size: Option<(u16, u16)>,
    /// Latched so an unsupported operation is reported once, not per frame.
    warned_unsupported: bool,
    /// Event kinds already reported. EGFX events repeat every frame, so an
    /// un-deduplicated log would bury everything else.
    seen: HashSet<String>,
    /// Rects actually drawn, so "connected but black" is distinguishable from
    /// "drawing, but wrong".
    drawn: u64,
}

impl EgfxHandler {
    pub fn new(on_frame: Channel<InvokeResponseBody>) -> Self {
        Self {
            on_frame,
            surfaces: HashMap::new(),
            progressive: ProgressiveDecoder::new(),
            output_size: None,
            warned_unsupported: false,
            seen: HashSet::new(),
            drawn: 0,
        }
    }

    /// Log `msg` the first time this kind of event is seen.
    fn note_once(&mut self, key: &str, msg: &str) {
        if self.seen.insert(key.to_owned()) {
            log::warn!("egfx: {msg}");
        }
    }

    /// Surface geometry, falling back to the output size for a surface the server
    /// never announced (it can still map one implicitly).
    fn surface_info(&self, surface_id: u16) -> SurfaceInfo {
        self.surfaces.get(&surface_id).copied().unwrap_or_else(|| {
            let (width, height) = self.output_size.unwrap_or((0, 0));
            SurfaceInfo {
                width,
                height,
                origin_x: 0,
                origin_y: 0,
            }
        })
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
        self.surfaces.insert(
            surface.id,
            SurfaceInfo {
                width: surface.width,
                height: surface.height,
                origin_x: surface.output_origin_x,
                origin_y: surface.output_origin_y,
            },
        );
    }

    fn on_surface_mapped(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        if let Some(info) = self.surfaces.get_mut(&surface_id) {
            info.origin_x = origin_x;
            info.origin_y = origin_y;
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

        let info = self.surface_info(update.surface_id);
        let x = clamp_u16(u32::from(update.destination_rectangle.left) + info.origin_x);
        let y = clamp_u16(u32::from(update.destination_rectangle.top) + info.origin_y);
        let (w, h) = (update.width, update.height);
        let data = update.data.clone();
        self.emit(x, y, w, h, &data, "bitmap");
    }

    /// RemoteFX Progressive. `ironrdp-egfx` only traces this PDU, so decoding and
    /// compositing happen here.
    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        let info = self.surface_info(pdu.surface_id);
        if info.width == 0 || info.height == 0 {
            self.note_once(
                "wire2-nosurface",
                &format!("progressive data for unknown surface {}, dropped", pdu.surface_id),
            );
            return;
        }

        let tiles = match self
            .progressive
            .decode_bitmap(pdu.codec_context_id, info.width, info.height, &pdu.bitmap_data)
        {
            Ok(tiles) => tiles,
            Err(e) => {
                // Errors here tend to repeat every frame, so don't spam the log.
                self.note_once("wire2-err", &format!("progressive decode failed: {e}"));
                return;
            }
        };

        for tile in tiles {
            let tile_x = tile.x_idx.saturating_mul(TILE);
            let tile_y = tile.y_idx.saturating_mul(TILE);
            // Edge tiles hang off the surface; the decoder still returns a full
            // 64x64 tile, so crop before drawing or the rows would smear.
            let w = TILE.min(info.width.saturating_sub(tile_x));
            let h = TILE.min(info.height.saturating_sub(tile_y));
            if w == 0 || h == 0 {
                continue;
            }

            let cropped = crop_tile(&tile.pixels, w, h);
            if cropped.is_empty() {
                continue;
            }
            let x = clamp_u16(u32::from(tile_x) + info.origin_x);
            let y = clamp_u16(u32::from(tile_y) + info.origin_y);
            self.emit(x, y, w, h, &cropped, "progressive");
        }
    }

    fn on_solid_fill(&mut self, pdu: &SolidFillPdu) {
        let info = self.surface_info(pdu.surface_id);
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
            let x = clamp_u16(u32::from(rect.left) + info.origin_x);
            let y = clamp_u16(u32::from(rect.top) + info.origin_y);
            self.emit(x, y, w, h, &rgba, "solid fill");
        }
    }

    fn on_surface_to_surface(&mut self, _pdu: &SurfaceToSurfacePdu) {
        self.warn_unsupported_op("SurfaceToSurface");
    }

    fn on_cache_to_surface(&mut self, _pdu: &CacheToSurfacePdu) {
        self.warn_unsupported_op("CacheToSurface");
    }

    fn on_unhandled_pdu(&mut self, pdu: &GfxPdu) {
        match pdu {
            // The server is dropping progressive tile state for this context; ours
            // must go too, or the next frame decodes against stale tiles.
            GfxPdu::DeleteEncodingContext(p) => {
                self.progressive.delete_context(p.codec_context_id);
            }
            GfxPdu::WireToSurface1(p) => {
                let kind = format!("WireToSurface1 codec={:?}", p.codec_id);
                self.note_once(&format!("unhandled:{kind}"), &format!("unhandled PDU {kind}"));
            }
            _ => self.note_once("unhandled:other", "unhandled EGFX PDU"),
        }
    }

    fn on_close(&mut self) {
        log::info!("egfx: channel closed after {} rects", self.drawn);
    }
}

impl EgfxHandler {
    /// Report an EGFX operation we don't render, once per session. These need a
    /// local per-surface framebuffer to implement; silently ignoring them would
    /// show a subtly wrong screen with no clue why.
    fn warn_unsupported_op(&mut self, what: &str) {
        if !self.warned_unsupported {
            self.warned_unsupported = true;
            log::warn!("egfx: {what} is not rendered; parts of the screen may be stale");
        }
    }
}

/// Take the top-left `w`x`h` corner out of a 64x64 RGBA tile. Returns empty if the
/// tile isn't the expected size.
fn crop_tile(pixels: &[u8], w: u16, h: u16) -> Vec<u8> {
    let stride = usize::from(TILE) * 4;
    if pixels.len() < stride * usize::from(TILE) {
        return Vec::new();
    }
    if w == TILE && h == TILE {
        return pixels.to_vec();
    }

    let row_bytes = usize::from(w) * 4;
    let mut out = Vec::with_capacity(row_bytes * usize::from(h));
    for row in 0..usize::from(h) {
        let start = row * stride;
        out.extend_from_slice(&pixels[start..start + row_bytes]);
    }
    out
}

/// Clamp an EGFX u32 coordinate onto the u16 the frame protocol carries.
fn clamp_u16(v: u32) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}
