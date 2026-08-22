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

fn quantize_size(size_px: f32) -> i32 {
    (size_px * 1000.0).round() as i32
}

/// A caching text rasterizer. One per application, mirroring cosmic-text's
/// own guidance for `FontSystem`/`SwashCache` lifetimes.
pub(crate) struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    cache: HashMap<CacheKey, MemoryRenderBuffer>,
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
    ) -> Option<MemoryRenderBuffer> {
        let key: CacheKey = (text.to_owned(), quantize_size(size_px), color);
        if let Some(buffer) = self.cache.get(&key) {
            return Some(buffer.clone());
        }

        let (pixels, width, height) =
            rasterize(&mut self.font_system, &mut self.swash_cache, text, size_px, color)?;

        let buffer = MemoryRenderBuffer::from_memory(
            MemoryBuffer::from_slice(&pixels, Fourcc::Argb8888, (width as i32, height as i32)),
            1,
            Transform::Normal,
            // No opaque-region hint: unlike a wallpaper, a text run's alpha
            // genuinely varies texel to texel.
            None,
        );
        self.cache.insert(key, buffer.clone());
        Some(buffer)
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

/// Shapes and rasterizes `text`, returning premultiplied Argb8888 pixels
/// tightly cropped to the run's ink extents, plus that crop's width and
/// height. Split out from `render` so tests can assert on raw pixels without
/// going through `MemoryRenderBuffer`.
fn rasterize(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    text: &str,
    size_px: f32,
    color: [u8; 4],
) -> Option<(Vec<u8>, u32, u32)> {
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

    Some((out, width, height))
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
        let (_, width, height) =
            rasterize(&mut font_system, &mut swash_cache, "Hi", 24.0, [255, 255, 255, 255])
                .expect("plain ASCII should rasterize to something");
        assert!(width > 0);
        assert!(height > 0);
    }

    #[test]
    fn transparent_texels_are_exactly_zero() {
        let mut font_system = FontSystem::new();
        let mut swash_cache = SwashCache::new();
        let (pixels, width, height) =
            rasterize(&mut font_system, &mut swash_cache, "Hi", 24.0, [200, 100, 50, 255])
                .expect("plain ASCII should rasterize to something");

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
        let (pixels, width, height) =
            rasterize(&mut font_system, &mut swash_cache, "M", 32.0, color)
                .expect("plain ASCII should rasterize to something");

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
        let (_, w1, h1) =
            rasterize(&mut font_system, &mut swash_cache, "clock", 18.0, [255, 255, 255, 255])
                .unwrap();
        let (_, w2, h2) =
            rasterize(&mut font_system, &mut swash_cache, "clock", 18.0, [255, 255, 255, 255])
                .unwrap();
        assert_eq!((w1, h1), (w2, h2));
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
}
