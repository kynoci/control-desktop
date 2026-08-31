//! ClearCodec bitmap decoder and encoder (MS-RDPEGFX 2.2.4.1).
//!
//! ClearCodec is a mandatory lossless codec for EGFX that uses three-layer
//! compositing (residual BGR RLE, bands with V-bar caching, subcodecs) to
//! efficiently encode text, UI elements, and icons.

mod glyph_cache;
mod vbar_cache;

pub use self::glyph_cache::{GLYPH_CACHE_SIZE, GlyphCache, GlyphEntry};
pub use self::vbar_cache::{FullVBar, ShortVBar, VBarCache};

/// Glyph cache size as u16 for index arithmetic. GLYPH_CACHE_SIZE=4000 fits in u16.
const GLYPH_CACHE_WRAP: u16 = 4_000;

use ironrdp_core::{DecodeResult, ReadCursor, ensure_size, invalid_field_err};
use ironrdp_pdu::codecs::clearcodec::{
    Band, ClearCodecBitmapStream, CompositePayload, FLAG_GLYPH_INDEX, MAX_BAND_HEIGHT, RgbRunSegment,
    ShortVBarCacheMiss, SubcodecId, VBar, decode_residual_layer, decode_subcodec_layer, encode_residual_layer,
};

/// ClearCodec decoder maintaining persistent cache state across frames.
pub struct ClearCodecDecoder {
    vbar_cache: VBarCache,
    glyph_cache: GlyphCache,
    /// PATCH (Control Desktop): one-line notes about regions that did not decode
    /// exactly, drained by the caller for its log. The crate has no logging facade,
    /// and silently painting a partial region would hide the reason.
    diagnostics: Vec<String>,
}

impl ClearCodecDecoder {
    pub fn new() -> Self {
        Self {
            vbar_cache: VBarCache::new(),
            glyph_cache: GlyphCache::new(),
            diagnostics: Vec::new(),
        }
    }

    /// PATCH (Control Desktop): take and clear pending decode notes.
    pub fn take_diagnostics(&mut self) -> Vec<String> {
        core::mem::take(&mut self.diagnostics)
    }

    /// Record a note, bounded — these repeat every frame.
    fn note(&mut self, msg: String) {
        if self.diagnostics.len() < 8 {
            self.diagnostics.push(msg);
        }
    }

    /// Decode a ClearCodec bitmap stream into BGRA pixel data.
    ///
    /// The output buffer is `width * height * 4` bytes in BGRA format.
    /// The caller is responsible for compositing the result onto the target
    /// surface at the destination rectangle.
    ///
    /// **Alpha contract:** ClearCodec is lossless on the three color channels
    /// (B, G, R) per MS-RDPEGFX 2.2.4.1. The wire format does not transmit
    /// alpha; this decoder fills the alpha byte of every output pixel with
    /// `0xFF` unconditionally. Callers that need to preserve alpha across the
    /// network must transport it separately.
    pub fn decode(&mut self, data: &[u8], width: u16, height: u16) -> DecodeResult<Vec<u8>> {
        let mut src = ReadCursor::new(data);
        let stream = ClearCodecBitmapStream::decode(&mut src)?;

        // Handle cache reset
        if stream.is_cache_reset() {
            self.vbar_cache.reset();
        }

        // Validate glyph index range per spec: 0..3999 inclusive
        if let Some(idx) = stream.glyph_index {
            if idx >= GLYPH_CACHE_WRAP {
                return Err(invalid_field_err!("glyphIndex", "glyph index out of range 0-3999"));
            }
        }

        let w = usize::from(width);
        let h = usize::from(height);
        let pixel_count = w
            .checked_mul(h)
            .ok_or_else(|| invalid_field_err!("dimensions", "width * height overflow"))?;

        // Handle glyph hit: return cached pixel data
        if stream.is_glyph_hit() {
            let glyph_index = stream
                .glyph_index
                .ok_or_else(|| invalid_field_err!("flags", "GLYPH_HIT without GLYPH_INDEX"))?;
            let entry = self
                .glyph_cache
                .get(glyph_index)
                .ok_or_else(|| invalid_field_err!("glyphIndex", "glyph cache miss on hit"))?;
            if entry.width != width || entry.height != height {
                return Err(invalid_field_err!("glyphIndex", "cached glyph dimensions mismatch"));
            }
            return Ok(entry.pixels.clone());
        }

        // Cap allocation to prevent OOM from adversarial dimensions.
        // MS-RDPEGFX caps surfaces at 32767x32767; the spec does not
        // mandate a separate tile cap. We cap each tile dimension at
        // 8192 (supports 8K displays at 7680x4320 plus headroom). The
        // per-dimension form rather than a per-pixel-count form is
        // important because the original pixel-count cap (8192*8192
        // = 67M) accepted degenerate aspect ratios like 63961x771
        // (49M pixels, under cap) that allocate ~197MB from a few
        // attacker-controlled bytes. Capping each axis directly
        // rejects implausible tile shapes regardless of total area.
        const MAX_DECODE_DIM: u16 = 8192;
        if width > MAX_DECODE_DIM || height > MAX_DECODE_DIM {
            return Err(invalid_field_err!(
                "dimensions",
                "width or height exceeds 8192-pixel decoder limit"
            ));
        }

        // Decode composite payload
        let mut output = vec![0u8; pixel_count * 4];

        if let Some(ref composite) = stream.composite {
            self.decode_composite(composite, &mut output, width, height)?;
        }

        // Store in glyph cache if applicable (area <= 1024 pixels)
        if stream.flags & FLAG_GLYPH_INDEX != 0 {
            if let Some(glyph_index) = stream.glyph_index {
                if pixel_count <= 1024 {
                    self.glyph_cache.store(
                        glyph_index,
                        GlyphEntry {
                            width,
                            height,
                            pixels: output.clone(),
                        },
                    );
                }
            }
        }

        Ok(output)
    }

    fn decode_composite(
        &mut self,
        composite: &CompositePayload<'_>,
        output: &mut [u8],
        width: u16,
        _height: u16,
    ) -> DecodeResult<()> {
        let w = usize::from(width);

        // Layer 1: Residual (BGR RLE) - fills the entire output.
        // Cap pixel writes to the output buffer size to prevent CPU-spin DoS
        // from adversarial run_length values (FreeRDP CVE GHSA-32q9-m5qr-9j2v).
        if !composite.residual_data.is_empty() {
            let segments = decode_residual_layer(composite.residual_data)?;
            let max_offset = output.len();
            let mut offset = 0;
            for seg in &segments {
                let pixels_remaining = (max_offset.saturating_sub(offset)) / 4;
                let effective_run = u32::try_from(pixels_remaining).unwrap_or(u32::MAX).min(seg.run_length);
                for _ in 0..effective_run {
                    output[offset] = seg.blue;
                    output[offset + 1] = seg.green;
                    output[offset + 2] = seg.red;
                    output[offset + 3] = 0xFF; // Alpha
                    offset += 4;
                }
                if offset >= max_offset {
                    break;
                }
            }
        }

        // Layer 2: Bands (V-bar cached columns) - composite on top
        if !composite.bands_data.is_empty() {
            let bands = decode_bands_layer(composite.bands_data)?; // local, see below
            for band in &bands {
                let band_height = band.y_end - band.y_start + 1;
                for (col_offset, vbar) in band.vbars.iter().enumerate() {
                    let x = usize::from(band.x_start) + col_offset;
                    if x >= w {
                        continue;
                    }

                    let full_vbar =
                        self.resolve_vbar(vbar, band_height, band.blue_bkg, band.green_bkg, band.red_bkg)?;

                    // Blit the full V-bar column into the output
                    let pixel_rows = full_vbar.pixels.len() / 3;
                    for row in 0..pixel_rows {
                        let y = usize::from(band.y_start) + row;
                        let dst_offset = (y * w + x) * 4;
                        let src_offset = row * 3;
                        if dst_offset + 3 < output.len() && src_offset + 2 < full_vbar.pixels.len() {
                            output[dst_offset] = full_vbar.pixels[src_offset];
                            output[dst_offset + 1] = full_vbar.pixels[src_offset + 1];
                            output[dst_offset + 2] = full_vbar.pixels[src_offset + 2];
                            output[dst_offset + 3] = 0xFF;
                        }
                    }
                }
            }
        }

        // Layer 3: Subcodecs - composite on top
        if !composite.subcodec_data.is_empty() {
            let subcodecs = decode_subcodec_layer(composite.subcodec_data)?;
            for sub in &subcodecs {
                // PATCH (Control Desktop): a region the decoder rejects used to fail
                // the entire ClearCodec rect, leaving that whole area stale. Skip it
                // and composite the rest.
                if let Err(e) = self.decode_subcodec_region(sub, output, width) {
                    self.note(format!(
                        "subcodec {:?} region {}x{} at ({},{}) failed: {e}",
                        sub.codec_id, sub.width, sub.height, sub.x_start, sub.y_start
                    ));
                }
            }
        }

        Ok(())
    }

    fn resolve_vbar(
        &mut self,
        vbar: &VBar<'_>,
        band_height: u16,
        bg_blue: u8,
        bg_green: u8,
        bg_red: u8,
    ) -> DecodeResult<FullVBar> {
        match vbar {
            VBar::CacheHit { index } => {
                let cached = self
                    .vbar_cache
                    .get_vbar(*index)
                    .ok_or_else(|| invalid_field_err!("vbarIndex", "V-bar cache miss on hit"))?;
                Ok(cached.clone())
            }
            VBar::ShortCacheHit { index, y_on } => {
                let cached_short = self
                    .vbar_cache
                    .get_short_vbar(*index)
                    .ok_or_else(|| invalid_field_err!("shortVbarIndex", "short V-bar cache miss on hit"))?;
                // Create a modified short vbar with the y_on from this reference
                let modified = ShortVBar {
                    y_on: *y_on,
                    pixel_count: cached_short.pixel_count,
                    pixels: cached_short.pixels.clone(),
                };
                let full = VBarCache::reconstruct_full_vbar(&modified, band_height, bg_blue, bg_green, bg_red);
                // Store reconstructed full V-bar in cache
                self.vbar_cache.store_vbar(full.clone());
                Ok(full)
            }
            VBar::ShortCacheMiss(miss) => {
                let short = ShortVBar {
                    y_on: miss.y_on,
                    pixel_count: miss.y_off_delta,
                    pixels: miss.pixel_data.to_vec(),
                };
                // Store in short V-bar cache
                self.vbar_cache.store_short_vbar(short.clone());
                // Reconstruct and store full V-bar
                let full = VBarCache::reconstruct_full_vbar(&short, band_height, bg_blue, bg_green, bg_red);
                self.vbar_cache.store_vbar(full.clone());
                Ok(full)
            }
        }
    }

    // NsCodec variant will use decoder state in Phase A7
    fn decode_subcodec_region(
        &mut self,
        sub: &ironrdp_pdu::codecs::clearcodec::Subcodec<'_>,
        output: &mut [u8],
        surface_width: u16,
    ) -> DecodeResult<()> {
        let sw = usize::from(surface_width);
        let sh = output.len() / (sw * 4).max(1);

        let x_end = usize::from(sub.x_start) + usize::from(sub.width);
        let y_end = usize::from(sub.y_start) + usize::from(sub.height);
        if x_end > sw || y_end > sh {
            return Err(invalid_field_err!("subcodec", "region exceeds surface bounds"));
        }

        match sub.codec_id {
            SubcodecId::Raw => {
                let w = usize::from(sub.width);
                let h = usize::from(sub.height);
                let expected = w
                    .checked_mul(h)
                    .and_then(|v| v.checked_mul(3))
                    .ok_or_else(|| invalid_field_err!("bitmapData", "raw subcodec dimensions overflow"))?;
                if sub.bitmap_data.len() < expected {
                    return Err(invalid_field_err!("bitmapData", "raw subcodec data too short"));
                }
                for row in 0..h {
                    for col in 0..w {
                        let x = usize::from(sub.x_start) + col;
                        let y = usize::from(sub.y_start) + row;
                        let src_idx = (row * w + col) * 3;
                        let dst_idx = (y * sw + x) * 4;
                        output[dst_idx] = sub.bitmap_data[src_idx];
                        output[dst_idx + 1] = sub.bitmap_data[src_idx + 1];
                        output[dst_idx + 2] = sub.bitmap_data[src_idx + 2];
                        output[dst_idx + 3] = 0xFF;
                    }
                }
            }
            SubcodecId::Rlex => {
                self.decode_rlex_region(sub, output, sw)?;
            }
            SubcodecId::NsCodec => {
                // Not yet implemented; encoder avoids generating NSCodec tiles.
            }
        }

        Ok(())
    }
}

impl ClearCodecDecoder {
    /// RLEX subcodec (MS-RDPEGFX 2.2.4.1.1.3.1.3).
    ///
    /// PATCH (Control Desktop): decoded here instead of through
    /// `ironrdp_pdu::codecs::clearcodec::decode_rlex`, which special-cases a
    /// single-entry palette by reading every byte as a run length. The segment header
    /// byte is always on the wire — with one palette entry the stopIndex field is
    /// simply zero bits wide and suiteDepth takes all eight — so dropping it
    /// desynchronised the segment stream and the region ran past its own pixel count
    /// ("invalid `rlex`: suite exceeds region pixel count" against Windows 11, which
    /// left taskbar and icon rects stale). Follows FreeRDP
    /// `clear_decompress_subcode_rlex`, except that running out of room stops the
    /// region rather than rejecting it: partially decoded pixels beat a dropped rect.
    fn decode_rlex_region(
        &mut self,
        sub: &ironrdp_pdu::codecs::clearcodec::Subcodec<'_>,
        output: &mut [u8],
        surface_width: usize,
    ) -> DecodeResult<()> {
        /// Spec cap on RLEX palette entries.
        const MAX_PALETTE_COUNT: u8 = 127;

        let mut src = ReadCursor::new(sub.bitmap_data);
        ensure_size!(ctx: "RlexPalette", in: src, size: 1);
        let palette_count = src.read_u8();
        if palette_count == 0 || palette_count > MAX_PALETTE_COUNT {
            return Err(invalid_field_err!("paletteCount", "palette count out of range 1..=127"));
        }

        ensure_size!(ctx: "RlexPalette", in: src, size: usize::from(palette_count) * 3);
        let mut palette = Vec::with_capacity(usize::from(palette_count));
        for _ in 0..palette_count {
            let b = src.read_u8();
            let g = src.read_u8();
            let r = src.read_u8();
            palette.push([b, g, r]);
        }

        // stopIndex takes ceil(log2(paletteCount)) bits of the header byte and
        // suiteDepth the rest. u16 masks because a one-entry palette gives suiteDepth
        // all 8 bits, and `1u8 << 8` would overflow.
        let stop_bits = if palette_count <= 1 {
            0
        } else {
            32 - u32::from(palette_count - 1).leading_zeros()
        };
        let stop_mask = (1u16 << stop_bits) - 1;
        let depth_mask = (1u16 << (8 - stop_bits)) - 1;

        let region_w = usize::from(sub.width);
        let region_pixels = region_w * usize::from(sub.height);
        let x_start = usize::from(sub.x_start);
        let y_start = usize::from(sub.y_start);
        let mut px = 0usize;
        let mut truncated = false;

        // A segment is the header byte plus at least one run-length byte.
        while src.len() >= 2 && px < region_pixels {
            let packed = u16::from(src.read_u8());
            let stop_index = usize::from(packed & stop_mask);
            let suite_depth = usize::from((packed >> stop_bits) & depth_mask);
            let start_index = stop_index.saturating_sub(suite_depth);
            let run_length = read_rlex_run_length(&mut src)?;

            if stop_index >= palette.len() {
                return Err(invalid_field_err!("rlex", "stop index exceeds palette size"));
            }

            let run_color = palette[start_index];
            for _ in 0..run_length {
                if px >= region_pixels {
                    truncated = true;
                    break;
                }
                write_rlex_pixel(output, surface_width, x_start, y_start, region_w, px, &run_color);
                px += 1;
            }

            for color in &palette[start_index..=stop_index] {
                if px >= region_pixels {
                    truncated = true;
                    break;
                }
                write_rlex_pixel(output, surface_width, x_start, y_start, region_w, px, color);
                px += 1;
            }
        }

        if truncated || px != region_pixels {
            self.note(format!(
                "rlex region {}x{} palette={palette_count} filled {px}/{region_pixels} px, {} bytes unread",
                sub.width,
                sub.height,
                src.len()
            ));
        }

        Ok(())
    }
}

/// Bands layer (MS-RDPEGFX 2.2.4.1.1.2).
///
/// PATCH (Control Desktop): decoded here rather than through
/// `ironrdp_pdu::codecs::clearcodec::decode_bands_layer`, whose SHORT_VBAR_CACHE_MISS
/// parse has the two Y fields swapped: it reads shortVBarYOn from bits 6..13 and
/// shortVBarYOff from bits 0..5, where the wire puts **shortVBarYOn in the low 8
/// bits and shortVBarYOff in bits 8..13** — as FreeRDP
/// `clear_decompress_bands_data` reads them. Real Windows bands therefore failed
/// the yOff >= yOn sanity check ("invalid `shortVBarCacheMiss`") and the whole
/// ClearCodec rect was dropped, leaving text and icons stale.
///
/// Everything else matches upstream, and it returns upstream types so the
/// compositing path is unchanged.
fn decode_bands_layer(data: &[u8]) -> DecodeResult<Vec<Band<'_>>> {
    /// Band header: 4 x u16 + 3 x u8.
    const BAND_HEADER_SIZE: usize = 11;

    let mut bands = Vec::new();
    let mut src = ReadCursor::new(data);

    while src.len() >= BAND_HEADER_SIZE {
        let x_start = src.read_u16();
        let x_end = src.read_u16();
        let y_start = src.read_u16();
        let y_end = src.read_u16();
        let blue_bkg = src.read_u8();
        let green_bkg = src.read_u8();
        let red_bkg = src.read_u8();

        if x_end < x_start {
            return Err(invalid_field_err!("xEnd", "xEnd < xStart"));
        }
        let height = y_end
            .checked_sub(y_start)
            .and_then(|h| h.checked_add(1))
            .ok_or_else(|| invalid_field_err!("yEnd", "yEnd < yStart"))?;
        if height > MAX_BAND_HEIGHT {
            return Err(invalid_field_err!("bandHeight", "band height exceeds 52"));
        }

        // x_end - x_start can be u16::MAX, so widen before the inclusive +1.
        let column_count = usize::from(x_end - x_start) + 1;
        let mut vbars = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            vbars.push(decode_vbar(&mut src, height)?);
        }

        bands.push(Band {
            x_start,
            x_end,
            y_start,
            y_end,
            blue_bkg,
            green_bkg,
            red_bkg,
            vbars,
        });
    }

    Ok(bands)
}

/// One V-bar, discriminated by the top two bits of its first word.
fn decode_vbar<'a>(src: &mut ReadCursor<'a>, band_height: u16) -> DecodeResult<VBar<'a>> {
    ensure_size!(ctx: "VBar", in: src, size: 2);
    let first_word = src.read_u16();

    // 1x: full V-bar cache hit, 15-bit index.
    if first_word & 0x8000 != 0 {
        return Ok(VBar::CacheHit {
            index: first_word & 0x7FFF,
        });
    }

    // 01: short V-bar cache hit, 14-bit index plus a yOn byte.
    if first_word & 0x4000 != 0 {
        let index = first_word & 0x3FFF;
        ensure_size!(ctx: "ShortVBarCacheHit", in: src, size: 1);
        let y_on = src.read_u8();
        return Ok(VBar::ShortCacheHit { index, y_on });
    }

    // 00: short V-bar cache miss, with inline pixels.
    //   bits 0..7  = shortVBarYOn  (8 bits)
    //   bits 8..13 = shortVBarYOff (6 bits)
    // Pixel count is shortVBarYOff - shortVBarYOn.
    let y_on = u8::try_from(first_word & 0xFF).expect("masked to 8 bits");
    let y_off = u8::try_from((first_word >> 8) & 0x3F).expect("masked to 6 bits");

    if y_off < y_on {
        return Err(invalid_field_err!("shortVBarCacheMiss", "shortVBarYOff < shortVBarYOn"));
    }
    if u16::from(y_off) > band_height {
        return Err(invalid_field_err!(
            "shortVBarCacheMiss",
            "shortVBarYOff exceeds band height"
        ));
    }

    let pixel_count = y_off - y_on;
    let pixel_byte_count = usize::from(pixel_count) * 3;
    ensure_size!(ctx: "ShortVBarCacheMiss", in: src, size: pixel_byte_count);
    let pixel_data = src.read_slice(pixel_byte_count);

    Ok(VBar::ShortCacheMiss(ShortVBarCacheMiss {
        y_on,
        y_off_delta: pixel_count,
        pixel_data,
    }))
}

/// One RLEX pixel, addressed inside its region and written into the whole-bitmap
/// RGBA buffer.
#[expect(clippy::too_many_arguments)]
fn write_rlex_pixel(
    output: &mut [u8],
    surface_width: usize,
    x_start: usize,
    y_start: usize,
    region_w: usize,
    px: usize,
    color: &[u8; 3],
) {
    let x = x_start + px % region_w;
    let y = y_start + px / region_w;
    let idx = (y * surface_width + x) * 4;
    if idx + 3 < output.len() {
        output[idx] = color[0];
        output[idx + 1] = color[1];
        output[idx + 2] = color[2];
        output[idx + 3] = 0xFF;
    }
}

/// RLEX run length: one byte, widening to two then four on an all-ones escape.
fn read_rlex_run_length(src: &mut ReadCursor<'_>) -> DecodeResult<u32> {
    ensure_size!(ctx: "RlexRunLength", in: src, size: 1);
    let factor1 = src.read_u8();
    if factor1 < 0xFF {
        return Ok(u32::from(factor1));
    }

    ensure_size!(ctx: "RlexRunLength", in: src, size: 2);
    let factor2 = src.read_u16();
    if factor2 < 0xFFFF {
        return Ok(u32::from(factor2));
    }

    ensure_size!(ctx: "RlexRunLength", in: src, size: 4);
    Ok(src.read_u32())
}

impl Default for ClearCodecDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// ClearCodec encoder for server-side bitmap compression.
///
/// Encodes BGRA pixel data into ClearCodec bitmap streams using the residual
/// (BGR RLE) layer. The residual-only strategy gives good compression for
/// solid regions and text without requiring V-bar cache synchronization.
pub struct ClearCodecEncoder {
    seq_number: u8,
    glyph_cache: GlyphCache,
    next_glyph_index: u16,
}

impl ClearCodecEncoder {
    pub fn new() -> Self {
        Self {
            seq_number: 0,
            glyph_cache: GlyphCache::new(),
            next_glyph_index: 0,
        }
    }

    /// Encode BGRA pixel data into a ClearCodec bitmap stream.
    ///
    /// Input: BGRA pixels in row-major order, `width * height * 4` bytes.
    /// Returns the wire-format ClearCodec bitmap stream ready for
    /// `WireToSurface1Pdu.bitmap_data`.
    ///
    /// **Alpha contract:** ClearCodec is lossless on the three color channels
    /// (B, G, R) per MS-RDPEGFX 2.2.4.1. The wire format does not transmit
    /// alpha; this encoder reads only B, G, R from each input pixel and
    /// discards the alpha byte. Callers that need to preserve alpha across
    /// the network must transport it separately.
    pub fn encode(&mut self, bgra: &[u8], width: u16, height: u16) -> Vec<u8> {
        let w = usize::from(width);
        let h = usize::from(height);
        let pixel_count = w.saturating_mul(h);
        let use_glyph = pixel_count <= 1024;

        // Check glyph cache for exact match
        if use_glyph {
            if let Some((hit_index, _)) = self.find_glyph_match(bgra, width, height) {
                return self.encode_glyph_hit(hit_index);
            }
        }

        // Convert BGRA to BGR run segments
        let segments = bgra_to_run_segments(bgra, pixel_count);
        let residual_data = encode_residual_layer(&segments);

        let mut flags = 0u8;
        let glyph_index = if use_glyph {
            flags |= FLAG_GLYPH_INDEX;
            let idx = self.next_glyph_index;
            self.glyph_cache.store(
                idx,
                GlyphEntry {
                    width,
                    height,
                    pixels: bgra.to_vec(),
                },
            );
            self.next_glyph_index = (idx + 1) % GLYPH_CACHE_WRAP;
            Some(idx)
        } else {
            None
        };

        let seq = self.seq_number;
        self.seq_number = seq.wrapping_add(1);

        // Build the wire-format bitmap stream
        let mut out = Vec::with_capacity(2 + 2 + 12 + residual_data.len());
        out.push(flags);
        out.push(seq);

        if let Some(idx) = glyph_index {
            out.extend_from_slice(&idx.to_le_bytes());
        }

        // Composite payload: residual only (bands=0, subcodec=0)
        let residual_len = u32::try_from(residual_data.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&residual_len.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // bandsByteCount
        out.extend_from_slice(&0u32.to_le_bytes()); // subcodecByteCount
        out.extend_from_slice(&residual_data);

        out
    }

    /// Encode a cache reset message (FLAG_CACHE_RESET).
    pub fn encode_cache_reset(&mut self) -> Vec<u8> {
        let seq = self.seq_number;
        self.seq_number = seq.wrapping_add(1);
        vec![ironrdp_pdu::codecs::clearcodec::FLAG_CACHE_RESET, seq]
    }

    fn find_glyph_match(&self, bgra: &[u8], width: u16, height: u16) -> Option<(u16, &GlyphEntry)> {
        // Linear scan of recently used glyph indices.
        // For small cache usage this is fine; a hash index could be added later.
        let search_range = GLYPH_CACHE_WRAP;
        for idx in 0..search_range {
            if let Some(entry) = self.glyph_cache.get(idx) {
                if entry.width == width && entry.height == height && entry.pixels == bgra {
                    return Some((idx, entry));
                }
            }
        }
        None
    }

    fn encode_glyph_hit(&mut self, index: u16) -> Vec<u8> {
        let seq = self.seq_number;
        self.seq_number = seq.wrapping_add(1);

        let flags = FLAG_GLYPH_INDEX | ironrdp_pdu::codecs::clearcodec::FLAG_GLYPH_HIT;
        let mut out = Vec::with_capacity(4);
        out.push(flags);
        out.push(seq);
        out.extend_from_slice(&index.to_le_bytes());
        out
    }
}

impl Default for ClearCodecEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert BGRA pixels to BGR run-length segments.
fn bgra_to_run_segments(bgra: &[u8], pixel_count: usize) -> Vec<RgbRunSegment> {
    if pixel_count == 0 {
        return Vec::new();
    }

    // Cap to the number of complete pixels actually present in the input
    let available_pixels = bgra.len() / 4;
    let pixel_count = pixel_count.min(available_pixels);

    let mut segments = Vec::new();
    let mut i = 0;

    while i < pixel_count {
        let offset = i * 4;
        if offset + 2 >= bgra.len() {
            break;
        }

        let blue = bgra[offset];
        let green = bgra[offset + 1];
        let red = bgra[offset + 2];
        // Alpha channel is discarded (ClearCodec is always opaque BGR)

        let mut run_length = 1u32;
        let mut j = i + 1;
        while j < pixel_count {
            let jo = j * 4;
            if jo + 2 >= bgra.len() {
                break;
            }
            if bgra[jo] == blue && bgra[jo + 1] == green && bgra[jo + 2] == red {
                run_length += 1;
                j += 1;
            } else {
                break;
            }
        }

        segments.push(RgbRunSegment {
            blue,
            green,
            red,
            run_length,
        });
        i = j;
    }

    segments
}

#[cfg(test)]
mod tests {
    use ironrdp_pdu::codecs::clearcodec::{FLAG_CACHE_RESET, FLAG_GLYPH_HIT};

    // PATCH (Control Desktop): coverage for the local RLEX decoder. Upstream has
    // none, and its single-palette path silently desynchronised the segment stream.
    use ironrdp_pdu::codecs::clearcodec::{Subcodec, SubcodecId};

    /// Decode one RLEX region into a `region_w` x `region_h` RGBA buffer.
    fn decode_rlex(bitmap_data: &[u8], region_w: u16, region_h: u16) -> Vec<u8> {
        let sub = Subcodec {
            x_start: 0,
            y_start: 0,
            width: region_w,
            height: region_h,
            codec_id: SubcodecId::Rlex,
            bitmap_data,
        };
        let mut output = vec![0u8; usize::from(region_w) * usize::from(region_h) * 4];
        let mut decoder = ClearCodecDecoder::new();
        decoder
            .decode_rlex_region(&sub, &mut output, usize::from(region_w))
            .expect("rlex region decodes");
        output
    }

    #[test]
    fn bands_short_vbar_cache_miss_field_order() {
        // One 1-column band, 4 rows tall. The V-bar word carries shortVBarYOn in
        // the low 8 bits and shortVBarYOff in bits 8..13: yOn=1, yOff=3 -> 0x0301,
        // which is 2 rows of inline pixels. Read with the fields swapped (as
        // upstream does) this decodes as yOn=12, yOff=1 and is rejected outright.
        let mut data = Vec::new();
        data.extend_from_slice(&0u16.to_le_bytes()); // xStart
        data.extend_from_slice(&0u16.to_le_bytes()); // xEnd
        data.extend_from_slice(&0u16.to_le_bytes()); // yStart
        data.extend_from_slice(&3u16.to_le_bytes()); // yEnd -> height 4
        data.extend_from_slice(&[0x11, 0x22, 0x33]); // background BGR
        data.extend_from_slice(&0x0301u16.to_le_bytes()); // yOn=1, yOff=3
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6]); // 2 pixels BGR

        let bands = decode_bands_layer(&data).expect("band decodes");
        assert_eq!(bands.len(), 1);
        match &bands[0].vbars[..] {
            [VBar::ShortCacheMiss(miss)] => {
                assert_eq!(miss.y_on, 1);
                assert_eq!(miss.y_off_delta, 2);
                assert_eq!(miss.pixel_data, &[1, 2, 3, 4, 5, 6]);
            }
            other => panic!("expected one ShortCacheMiss, got {other:?}"),
        }
    }

    #[test]
    fn bands_vbar_cache_hits_keep_their_indices() {
        let mut data = Vec::new();
        data.extend_from_slice(&0u16.to_le_bytes()); // xStart
        data.extend_from_slice(&1u16.to_le_bytes()); // xEnd -> 2 columns
        data.extend_from_slice(&0u16.to_le_bytes()); // yStart
        data.extend_from_slice(&1u16.to_le_bytes()); // yEnd
        data.extend_from_slice(&[0, 0, 0]); // background
        data.extend_from_slice(&(0x8000u16 | 42).to_le_bytes()); // full cache hit, index 42
        data.extend_from_slice(&(0x4000u16 | 7).to_le_bytes()); // short cache hit, index 7
        data.push(5); // its yOn byte

        let bands = decode_bands_layer(&data).expect("band decodes");
        match &bands[0].vbars[..] {
            [VBar::CacheHit { index: 42 }, VBar::ShortCacheHit { index: 7, y_on: 5 }] => {}
            other => panic!("unexpected vbars: {other:?}"),
        }
    }

    #[test]
    fn rlex_two_palette_run_then_suite() {
        // paletteCount=2 -> stopIndex is 1 bit, suiteDepth the other 7.
        let data = [
            2, // paletteCount
            0, 0, 0, // palette[0] = black (BGR)
            255, 255, 255, // palette[1] = white
            0x00, 2, // stopIndex=0, suiteDepth=0, run=2 -> 2 black + 1 suite black
            0x01, 0, // stopIndex=1, suiteDepth=0, run=0 -> 1 suite white
        ];

        let out = decode_rlex(&data, 4, 1);
        assert_eq!(&out[0..4], &[0, 0, 0, 0xFF]);
        assert_eq!(&out[4..8], &[0, 0, 0, 0xFF]);
        assert_eq!(&out[8..12], &[0, 0, 0, 0xFF]);
        assert_eq!(&out[12..16], &[255, 255, 255, 0xFF]);
    }

    #[test]
    fn rlex_suite_walks_the_palette() {
        // paletteCount=4 -> stopIndex is 2 bits, suiteDepth the other 6.
        // stopIndex=2, suiteDepth=2 -> startIndex=0, suite walks palette[0..=2].
        let data = [
            4, // paletteCount
            1, 1, 1, // palette[0]
            2, 2, 2, // palette[1]
            3, 3, 3, // palette[2]
            4, 4, 4, // palette[3]
            0b0000_10_10, 0, // stopIndex=2, suiteDepth=2, run=0
        ];

        let out = decode_rlex(&data, 3, 1);
        assert_eq!(&out[0..4], &[1, 1, 1, 0xFF]);
        assert_eq!(&out[4..8], &[2, 2, 2, 0xFF]);
        assert_eq!(&out[8..12], &[3, 3, 3, 0xFF]);
    }

    #[test]
    fn rlex_single_palette_still_reads_the_header_byte() {
        // The regression this decoder exists for: with one palette entry the
        // stopIndex field is zero bits wide, but the header byte is still on the
        // wire. Reading it as a run length desynchronised every later segment.
        let data = [
            1, // paletteCount
            10, 20, 30, // palette[0] (BGR)
            0x00, 1, // suiteDepth=0, run=1 -> 1 run pixel + 1 suite pixel
        ];

        let out = decode_rlex(&data, 2, 1);
        assert_eq!(&out[0..4], &[10, 20, 30, 0xFF]);
        assert_eq!(&out[4..8], &[10, 20, 30, 0xFF]);
    }

    #[test]
    fn rlex_stops_at_the_region_edge_instead_of_failing() {
        // A run longer than the region fills it and stops, rather than rejecting
        // the rect and leaving the area stale.
        let data = [
            1, // paletteCount
            7, 8, 9, // palette[0]
            0x00, 200, // run=200 into a 2-pixel region
        ];

        let out = decode_rlex(&data, 2, 1);
        assert_eq!(&out[0..4], &[7, 8, 9, 0xFF]);
        assert_eq!(&out[4..8], &[7, 8, 9, 0xFF]);
    }

    #[test]
    fn rlex_run_length_escapes_widen() {
        let mut src = ReadCursor::new(&[0x10]);
        assert_eq!(read_rlex_run_length(&mut src).unwrap(), 0x10);

        // 0xFF escapes to a u16.
        let data = [0xFF, 0x34, 0x12];
        let mut src = ReadCursor::new(&data);
        assert_eq!(read_rlex_run_length(&mut src).unwrap(), 0x1234);

        // 0xFFFF escapes again to a u32.
        let data = [0xFF, 0xFF, 0xFF, 0x78, 0x56, 0x34, 0x12];
        let mut src = ReadCursor::new(&data);
        assert_eq!(read_rlex_run_length(&mut src).unwrap(), 0x1234_5678);
    }

    use super::*;

    fn make_residual_only_stream(width: u16, height: u16, blue: u8, green: u8, red: u8) -> Vec<u8> {
        let pixel_count = u32::from(width) * u32::from(height);
        let mut data = Vec::new();

        // Flags=0x00 (no glyph, no cache reset), seq=0x00
        data.push(0x00);
        data.push(0x00);

        // Composite payload header
        // Residual: 4 bytes (1 run segment: BGR + short run)
        let run_length = pixel_count;
        let residual = if run_length < 0xFF {
            vec![blue, green, red, u8::try_from(run_length).unwrap()]
        } else if run_length < 0xFFFF {
            let mut v = vec![blue, green, red, 0xFF];
            v.extend_from_slice(&u16::try_from(run_length).unwrap().to_le_bytes());
            v
        } else {
            let mut v = vec![blue, green, red, 0xFF, 0xFF, 0xFF];
            v.extend_from_slice(&run_length.to_le_bytes());
            v
        };
        let residual_len = u32::try_from(residual.len()).unwrap();

        data.extend_from_slice(&residual_len.to_le_bytes()); // residualByteCount
        data.extend_from_slice(&0u32.to_le_bytes()); // bandsByteCount
        data.extend_from_slice(&0u32.to_le_bytes()); // subcodecByteCount
        data.extend_from_slice(&residual);

        data
    }

    #[test]
    fn decode_solid_red_4x4() {
        let mut decoder = ClearCodecDecoder::new();
        let stream = make_residual_only_stream(4, 4, 0x00, 0x00, 0xFF); // red in BGR
        let pixels = decoder.decode(&stream, 4, 4).unwrap();
        assert_eq!(pixels.len(), 4 * 4 * 4);
        // Check first pixel: BGRA
        assert_eq!(pixels[0], 0x00); // B
        assert_eq!(pixels[1], 0x00); // G
        assert_eq!(pixels[2], 0xFF); // R
        assert_eq!(pixels[3], 0xFF); // A
    }

    #[test]
    fn glyph_cache_round_trip() {
        let mut decoder = ClearCodecDecoder::new();

        // First decode: GLYPH_INDEX set, stores in glyph cache
        let mut stream = Vec::new();
        stream.push(FLAG_GLYPH_INDEX); // flags
        stream.push(0x00); // seq
        stream.extend_from_slice(&42u16.to_le_bytes()); // glyph_index = 42
        // Composite with 1-pixel residual (white)
        let residual = [0xFF, 0xFF, 0xFF, 0x01]; // BGR white, run=1
        stream.extend_from_slice(&4u32.to_le_bytes()); // residual bytes
        stream.extend_from_slice(&0u32.to_le_bytes()); // bands bytes
        stream.extend_from_slice(&0u32.to_le_bytes()); // subcodec bytes
        stream.extend_from_slice(&residual);

        let pixels1 = decoder.decode(&stream, 1, 1).unwrap();
        assert_eq!(pixels1.len(), 4);

        // Second decode: GLYPH_HIT - should return cached data
        let mut hit_stream = Vec::new();
        hit_stream.push(FLAG_GLYPH_INDEX | FLAG_GLYPH_HIT); // flags
        hit_stream.push(0x01); // seq = 1
        hit_stream.extend_from_slice(&42u16.to_le_bytes()); // glyph_index = 42

        let pixels2 = decoder.decode(&hit_stream, 1, 1).unwrap();
        assert_eq!(pixels1, pixels2);
    }

    #[test]
    fn raw_subcodec_decode() {
        let mut decoder = ClearCodecDecoder::new();
        let mut stream = Vec::new();
        stream.push(0x00); // flags
        stream.push(0x00); // seq

        // Composite: no residual, no bands, 1 raw subcodec region
        let mut subcodec_data = Vec::new();
        subcodec_data.extend_from_slice(&0u16.to_le_bytes()); // x_start
        subcodec_data.extend_from_slice(&0u16.to_le_bytes()); // y_start
        subcodec_data.extend_from_slice(&2u16.to_le_bytes()); // width
        subcodec_data.extend_from_slice(&1u16.to_le_bytes()); // height
        subcodec_data.extend_from_slice(&6u32.to_le_bytes()); // 2 pixels * 3 bytes
        subcodec_data.push(0x00); // SubcodecId::Raw
        subcodec_data.extend_from_slice(&[0x00, 0x00, 0xFF]); // pixel 0: red
        subcodec_data.extend_from_slice(&[0xFF, 0x00, 0x00]); // pixel 1: blue

        let subcodec_len = u32::try_from(subcodec_data.len()).unwrap();
        stream.extend_from_slice(&0u32.to_le_bytes()); // residual
        stream.extend_from_slice(&0u32.to_le_bytes()); // bands
        stream.extend_from_slice(&subcodec_len.to_le_bytes()); // subcodec
        stream.extend_from_slice(&subcodec_data);

        let pixels = decoder.decode(&stream, 2, 1).unwrap();
        assert_eq!(pixels.len(), 2 * 4); // 2 pixels * BGRA
        // Pixel 0: red (BGR: 0x00, 0x00, 0xFF)
        assert_eq!(&pixels[0..4], &[0x00, 0x00, 0xFF, 0xFF]);
        // Pixel 1: blue (BGR: 0xFF, 0x00, 0x00)
        assert_eq!(&pixels[4..8], &[0xFF, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn cache_reset_clears_vbar_cursors() {
        let mut decoder = ClearCodecDecoder::new();
        // Decode something to advance cursors, then reset
        let stream = make_residual_only_stream(1, 1, 0, 0, 0);
        decoder.decode(&stream, 1, 1).unwrap();

        // Cache reset message
        let reset_data = [FLAG_CACHE_RESET, 0x01]; // flags=CACHE_RESET, seq=1
        let _ = decoder.decode(&reset_data, 0, 0); // zero dimensions, but cache reset still processed
    }

    // --- Encoder tests ---

    #[test]
    fn encode_solid_color_round_trip() {
        let mut enc = ClearCodecEncoder::new();
        let mut dec = ClearCodecDecoder::new();

        // 4x4 solid red (BGRA: 0,0,255,255)
        let bgra: Vec<u8> = (0..16).flat_map(|_| [0x00, 0x00, 0xFF, 0xFF]).collect();

        let wire = enc.encode(&bgra, 4, 4);
        let result = dec.decode(&wire, 4, 4).unwrap();

        assert_eq!(result, bgra);
    }

    #[test]
    fn encode_two_color_stripe_round_trip() {
        let mut enc = ClearCodecEncoder::new();
        let mut dec = ClearCodecDecoder::new();

        // 4x1: 2 red + 2 blue pixels
        let mut bgra = Vec::new();
        bgra.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]); // red
        bgra.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]); // red
        bgra.extend_from_slice(&[0xFF, 0x00, 0x00, 0xFF]); // blue
        bgra.extend_from_slice(&[0xFF, 0x00, 0x00, 0xFF]); // blue

        let wire = enc.encode(&bgra, 4, 1);
        let result = dec.decode(&wire, 4, 1).unwrap();

        assert_eq!(result, bgra);
    }

    #[test]
    fn encode_glyph_cache_hit() {
        let mut encoder = ClearCodecEncoder::new();

        // Small 1x1 pixel (fits glyph cache: area=1 <= 1024)
        let bgra = vec![0xFF, 0x00, 0x00, 0xFF]; // blue

        let first = encoder.encode(&bgra, 1, 1);
        let second = encoder.encode(&bgra, 1, 1);

        // Second encode should be a glyph hit (shorter)
        assert!(
            second.len() < first.len(),
            "glyph hit should be shorter than full encode"
        );

        // Both should decode to the same pixels
        let mut decoder = ClearCodecDecoder::new();
        let p1 = decoder.decode(&first, 1, 1).unwrap();
        let p2 = decoder.decode(&second, 1, 1).unwrap();
        assert_eq!(p1, p2);
        assert_eq!(p1, bgra);
    }

    #[test]
    fn encode_sequence_numbers_increment() {
        let mut encoder = ClearCodecEncoder::new();
        let bgra = vec![0x00, 0x00, 0x00, 0xFF]; // 1x1 black

        let e1 = encoder.encode(&bgra, 1, 1);
        let e2 = encoder.encode(&bgra, 1, 1);

        // Seq numbers are at byte offset 1
        // First frame starts with glyph_index flag + seq=0
        assert_eq!(e1[1], 0x00);
        // Second is glyph hit: seq=1
        assert_eq!(e2[1], 0x01);
    }

    #[test]
    fn encode_cache_reset() {
        let mut encoder = ClearCodecEncoder::new();
        let reset = encoder.encode_cache_reset();

        let mut decoder = ClearCodecDecoder::new();
        let _ = decoder.decode(&reset, 0, 0);
        // Just verifies it doesn't error
    }

    #[test]
    fn bgra_to_run_segments_compresses_runs() {
        // 8 identical pixels should produce 1 segment with run_length=8
        let bgra: Vec<u8> = (0..8).flat_map(|_| [0xAA, 0xBB, 0xCC, 0xFF]).collect();
        let segments = bgra_to_run_segments(&bgra, 8);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].run_length, 8);
        assert_eq!(segments[0].blue, 0xAA);
        assert_eq!(segments[0].green, 0xBB);
        assert_eq!(segments[0].red, 0xCC);
    }

    #[test]
    fn bgra_to_run_segments_unique_pixels() {
        // 3 different pixels produce 3 segments
        let bgra = vec![
            0x01, 0x02, 0x03, 0xFF, // pixel 1
            0x04, 0x05, 0x06, 0xFF, // pixel 2
            0x07, 0x08, 0x09, 0xFF, // pixel 3
        ];
        let segments = bgra_to_run_segments(&bgra, 3);
        assert_eq!(segments.len(), 3);
        for seg in &segments {
            assert_eq!(seg.run_length, 1);
        }
    }
}
