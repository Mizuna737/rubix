//! Text rasterization.
//!
//! Turns a string into a premultiplied `Argb8888` [`MemoryRenderBuffer`] --
//! the same buffer kind [`crate::wallpaper`] already uploads through
//! [`MemoryRenderBufferRenderElement`](smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement),
//! so a text run composites through the exact machinery a wallpaper quad
//! already does.
//!
//! This is Phase 1: the capability, with no caller. Nothing here is wired
//! into any render path yet -- no `[bar]`/`[text]` config, no `FontSystem` on
//! `RubixState`, no glyph atlas. That is Phase 2, once a bar element exists
//! to splice into `compose_output_elements`.

use std::collections::HashMap;

use cosmic_text::{
    Attrs, Buffer as CosmicBuffer, Color as CosmicColor, Family, FontSystem, Metrics, Shaping,
    SwashCache,
};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{MemoryBuffer, MemoryRenderBuffer};
use smithay::utils::Transform;

/// Cache key: the full identity of a render request. `f32` is not `Hash`, so
/// `size_px` is quantised to thousandths of a pixel rather than reaching for
/// a float-hashing crate.
type CacheKey = (String, i32, [u8; 4]);

/// A rasterized run plus everything a caller needs to place it.
///
/// `buffer` is cropped to ink extents, so its size is **not** the run's
/// layout size. `ink_offset` is where that crop sits relative to the
/// layout origin: drawing `buffer` at `origin + ink_offset` reproduces the
/// position cosmic-text laid the glyphs out at. Placing `buffer` at
/// `origin` directly is the jitter bug -- two strings of the same length
/// have different ink extents ("11:59" vs "12:00"), so a bar clock would
/// bounce every minute.
#[derive(Clone, Debug)]
pub(crate) struct TextRun {
    pub buffer: MemoryRenderBuffer,
    /// Ink-box origin relative to the layout origin, in pixels.
    pub ink_offset: (i32, i32),
    /// Ink-box size in pixels. Redundant with the buffer's own size, kept
    /// so a caller can lay out without querying the buffer.
    pub ink_size: (u32, u32),
    /// Horizontal advance of the whole run: the width a container should
    /// budget, which is not the ink width (it includes side bearings and
    /// any trailing space).
    pub advance_width: f32,
    /// Baseline-to-baseline distance, i.e. the row height to reserve.
    pub line_height: f32,
    /// Distance from the layout origin down to the baseline. Vertically
    /// centring a run means centring `line_height`, then placing at the
    /// resulting top edge -- never centring `ink_size`.
    pub ascent: f32,
}

fn quantize_size(size_px: f32) -> i32 {
    (size_px * 1000.0).round() as i32
}

/// A caching text rasterizer. One per application, mirroring cosmic-text's
/// own guidance for `FontSystem`/`SwashCache` lifetimes.
pub(crate) struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    cache: HashMap<CacheKey, TextRun>,
}

impl TextRenderer {
    pub(crate) fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            cache: HashMap::new(),
        }
    }

    /// Rasterizes `text` at `size_px` in `color` (non-premultiplied sRGB +
    /// alpha) into a premultiplied Argb8888 buffer sized to the run's ink
    /// extents. Returns `None` for an empty string or a run that rasterizes
    /// to nothing (e.g. all-whitespace).
    pub(crate) fn render(
        &mut self,
        text: &str,
        size_px: f32,
        color: [u8; 4],
    ) -> Option<TextRun> {
        let key: CacheKey = (text.to_owned(), quantize_size(size_px), color);
        if let Some(run) = self.cache.get(&key) {
            return Some(run.clone());
        }

        let rasterized =
            rasterize(&mut self.font_system, &mut self.swash_cache, text, size_px, color)?;

        let buffer = MemoryRenderBuffer::from_memory(
            MemoryBuffer::from_slice(
                &rasterized.pixels,
                Fourcc::Argb8888,
                (rasterized.width as i32, rasterized.height as i32),
            ),
            1,
            Transform::Normal,
            // No opaque-region hint: unlike a wallpaper, a text run's alpha
            // genuinely varies texel to texel.
            None,
        );
        let run = TextRun {
            buffer,
            ink_offset: (rasterized.min_x, rasterized.min_y),
            ink_size: (rasterized.width, rasterized.height),
            advance_width: rasterized.advance_width,
            line_height: rasterized.line_height,
            ascent: rasterized.ascent,
        };
        self.cache.insert(key, run.clone());
        Some(run)
    }
}

/// One coverage-shaded texel, in buffer-local (not glyph-local) coordinates.
struct InkTexel {
    x: i32,
    y: i32,
    /// Non-premultiplied colour with the requested alpha already folded in
    /// (see the comment in `rasterize` on why that folding happens here).
    color: [u8; 4],
}

/// Everything `rasterize` produces: the cropped pixels plus the layout
/// metrics a caller needs to place them. See [`TextRun`] for what each field
/// means -- this is its pre-`MemoryRenderBuffer` twin.
struct Rasterized {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    min_x: i32,
    min_y: i32,
    advance_width: f32,
    line_height: f32,
    ascent: f32,
}

/// Shapes and rasterizes `text`, returning premultiplied Argb8888 pixels
/// tightly cropped to the run's ink extents, plus the layout metrics needed
/// to place that crop. Split out from `render` so tests can assert on raw
/// pixels without going through `MemoryRenderBuffer`.
fn rasterize(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    text: &str,
    size_px: f32,
    color: [u8; 4],
) -> Option<Rasterized> {
    if text.is_empty() || size_px <= 0.0 {
        return None;
    }

    let metrics = Metrics::new(size_px, size_px * 1.2);
    let mut buffer = CosmicBuffer::new(font_system, metrics);
    let mut buffer = buffer.borrow_with(font_system);
    // Unbounded: a bar label is one line, not a paragraph to wrap.
    buffer.set_size(None, None);

    let [r, g, b, a] = color;
    let attrs = Attrs::new().family(Family::Monospace);
    buffer.set_text(text, &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(true);

    let base_color = CosmicColor::rgba(r, g, b, a);

    let mut texels: Vec<InkTexel> = Vec::new();
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;

    buffer.draw(swash_cache, base_color, |x, y, w, h, pixel_color| {
        // cosmic-text's mask rasterization path (`SwashCache::with_pixels`)
        // replaces the alpha channel with the glyph's coverage byte but does
        // NOT multiply in the base colour's own alpha -- so a caller-requested
        // alpha below 255 has to be folded in by hand here, or partially
        // transparent text would render fully opaque wherever the glyph has
        // any coverage at all.
        let coverage = pixel_color.a();
        if coverage == 0 {
            return;
        }
        let alpha = ((coverage as u32) * (a as u32) / 255) as u8;
        if alpha == 0 {
            return;
        }
        for off_y in 0..h as i32 {
            for off_x in 0..w as i32 {
                let px = x + off_x;
                let py = y + off_y;
                min_x = min_x.min(px);
                min_y = min_y.min(py);
                max_x = max_x.max(px);
                max_y = max_y.max(py);
                texels.push(InkTexel { x: px, y: py, color: [r, g, b, alpha] });
            }
        }
    });

    if texels.is_empty() {
        // Nothing rasterized -- e.g. an all-whitespace run.
        return None;
    }

    let width = (max_x - min_x + 1) as u32;
    let height = (max_y - min_y + 1) as u32;
    if width == 0 || height == 0 {
        return None;
    }

    // Take the max run width rather than assuming exactly one layout run, so
    // an unexpected wrap cannot silently truncate the advance budget. The
    // baseline (`ascent`) comes from the first run: a bar label is one line,
    // and there is no meaningful single baseline for a wrapped multi-line
    // run anyway.
    let mut layout_runs = buffer.layout_runs();
    let first_run = layout_runs.next()?;
    let ascent = first_run.line_y;
    let advance_width = std::iter::once(first_run.line_w)
        .chain(layout_runs.map(|r| r.line_w))
        .fold(f32::MIN, f32::max);

    // Zero-initialised: every texel starts as exactly [0, 0, 0, 0] and stays
    // that way unless ink actually lands on it. This is the transparency
    // invariant the tests check -- see the module doc on why a stray nonzero
    // value in an empty region matters on this compositor's HDR path.
    let mut out = vec![0u8; (width as usize) * (height as usize) * 4];
    for texel in texels {
        let lx = (texel.x - min_x) as u32;
        let ly = (texel.y - min_y) as u32;
        let idx = ((ly * width + lx) * 4) as usize;
        let [tr, tg, tb, ta] = texel.color;
        // Argb8888: fourcc names list bytes MSB-first for a little-endian
        // dword, so "ARGB" reads back to front in memory as B, G, R, A (see
        // the matching comment in src/cursor.rs on Abgr8888).
        out[idx] = premultiply(tb, ta);
        out[idx + 1] = premultiply(tg, ta);
        out[idx + 2] = premultiply(tr, ta);
        out[idx + 3] = ta;
    }

    Some(Rasterized {
        pixels: out,
        width,
        height,
        min_x,
        min_y,
        advance_width,
        line_height: metrics.line_height,
        ascent,
    })
}

/// Multiplies a non-premultiplied channel by alpha, rounding to nearest.
fn premultiply(channel: u8, alpha: u8) -> u8 {
    ((channel as u32 * alpha as u32 + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let idx = ((y * width + x) * 4) as usize;
        [pixels[idx], pixels[idx + 1], pixels[idx + 2], pixels[idx + 3]]
    }

    #[test]
    fn empty_string_returns_none() {
        let mut renderer = TextRenderer::new();
        assert!(renderer.render("", 16.0, [255, 255, 255, 255]).is_none());
    }

    #[test]
    fn plain_ascii_produces_nonempty_dimensions() {
        let mut font_system = FontSystem::new();
        let mut swash_cache = SwashCache::new();
        let rasterized =
            rasterize(&mut font_system, &mut swash_cache, "Hi", 24.0, [255, 255, 255, 255])
                .expect("plain ASCII should rasterize to something");
        assert!(rasterized.width > 0);
        assert!(rasterized.height > 0);
    }

    #[test]
    fn transparent_texels_are_exactly_zero() {
        let mut font_system = FontSystem::new();
        let mut swash_cache = SwashCache::new();
        let rasterized =
            rasterize(&mut font_system, &mut swash_cache, "Hi", 24.0, [200, 100, 50, 255])
                .expect("plain ASCII should rasterize to something");
        let (pixels, width, height) = (rasterized.pixels, rasterized.width, rasterized.height);

        let mut saw_transparent = false;
        for y in 0..height {
            for x in 0..width {
                let px = pixel(&pixels, width, x, y);
                if px[3] == 0 {
                    saw_transparent = true;
                    assert_eq!(px, [0, 0, 0, 0], "transparent texel at ({x}, {y}) is not exactly zero");
                }
            }
        }
        // A tightly cropped ink run with any anti-aliasing at all should
        // still have some fully-transparent corner texels; if this trips,
        // the crop or the coverage threshold needs revisiting, not the test.
        assert!(saw_transparent, "expected at least one fully-transparent texel in the crop");
    }

    #[test]
    fn opaque_texel_matches_requested_color() {
        let mut font_system = FontSystem::new();
        let mut swash_cache = SwashCache::new();
        let color = [10, 200, 30, 255];
        let rasterized = rasterize(&mut font_system, &mut swash_cache, "M", 32.0, color)
            .expect("plain ASCII should rasterize to something");
        let (pixels, width, height) = (rasterized.pixels, rasterized.width, rasterized.height);

        // At alpha 255, premultiplication is a no-op, so a fully opaque
        // texel must equal the requested colour exactly, not the colour
        // scaled twice.
        let mut found_opaque = false;
        for y in 0..height {
            for x in 0..width {
                let px = pixel(&pixels, width, x, y);
                if px[3] == 255 {
                    found_opaque = true;
                    assert_eq!(px, [color[2], color[1], color[0], color[3]]);
                }
            }
        }
        assert!(found_opaque, "expected at least one fully-opaque texel for 'M' at size 32");
    }

    #[test]
    fn identical_requests_share_one_cache_entry() {
        let mut renderer = TextRenderer::new();
        assert!(renderer.render("clock", 18.0, [255, 255, 255, 255]).is_some());
        assert!(renderer.render("clock", 18.0, [255, 255, 255, 255]).is_some());
        assert_eq!(renderer.cache.len(), 1);
    }

    #[test]
    fn identical_requests_produce_identical_dimensions() {
        let mut font_system = FontSystem::new();
        let mut swash_cache = SwashCache::new();
        let r1 =
            rasterize(&mut font_system, &mut swash_cache, "clock", 18.0, [255, 255, 255, 255])
                .unwrap();
        let r2 =
            rasterize(&mut font_system, &mut swash_cache, "clock", 18.0, [255, 255, 255, 255])
                .unwrap();
        assert_eq!((r1.width, r1.height), (r2.width, r2.height));
    }

    #[test]
    fn different_color_makes_a_distinct_entry() {
        let mut renderer = TextRenderer::new();
        renderer.render("clock", 18.0, [255, 255, 255, 255]).unwrap();
        renderer.render("clock", 18.0, [0, 0, 0, 255]).unwrap();
        assert_eq!(renderer.cache.len(), 2);
    }

    #[test]
    fn different_size_makes_a_distinct_entry() {
        let mut renderer = TextRenderer::new();
        renderer.render("clock", 18.0, [255, 255, 255, 255]).unwrap();
        renderer.render("clock", 20.0, [255, 255, 255, 255]).unwrap();
        assert_eq!(renderer.cache.len(), 2);
    }

    #[test]
    fn ink_offset_is_returned_not_discarded() {
        let mut font_system = FontSystem::new();
        let mut swash_cache = SwashCache::new();
        // A descender-heavy glyph sits below the layout origin, so its ink
        // box starts partway down the line -- exactly the offset that gets
        // discarded if a caller only keeps the crop.
        let rasterized = rasterize(&mut font_system, &mut swash_cache, "g", 32.0, [255, 255, 255, 255])
            .expect("'g' should rasterize to something");
        assert!(rasterized.min_y > 0, "expected a positive ink offset for a descender glyph");
    }

    #[test]
    fn runs_of_equal_length_share_advance_width() {
        let mut font_system = FontSystem::new();
        let mut swash_cache = SwashCache::new();
        let a = rasterize(&mut font_system, &mut swash_cache, "11:59", 18.0, [255, 255, 255, 255])
            .expect("'11:59' should rasterize to something");
        let b = rasterize(&mut font_system, &mut swash_cache, "12:00", 18.0, [255, 255, 255, 255])
            .expect("'12:00' should rasterize to something");
        assert_eq!(
            a.advance_width, b.advance_width,
            "monospace runs of equal length must share advance_width regardless of ink extents"
        );
    }
}
