//! The compositor-drawn status bar: a hardcoded background rect and a
//! hardcoded text label, colored from the solved theme. Deliberately dumb --
//! no widget trait, no modules, no clock. See `[bar]` in `src/config.rs` and
//! `config/default/bar.toml`.

use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::{Id, Kind, RenderElement};
use smithay::backend::renderer::{Color32F, ImportAll, ImportMem, Renderer, Texture};
use smithay::output::Output;
use smithay::utils::{Physical, Point, Rectangle};

use crate::config::BarPosition;
use crate::cursor::RubixRenderElement;
use crate::rounding::GlesAccess;
use crate::state::RubixState;
use crate::theme::abs10k_to_display_srgb;

/// Fallback bar colours (background, foreground) used until a theme has been
/// solved, or when theming is disabled outright. Plain display sRGB, same
/// space `abs10k_to_display_srgb` produces.
const FALLBACK_COLORS: ([f32; 3], [f32; 3]) = ([0.12, 0.12, 0.14], [0.9, 0.9, 0.9]);

/// Build the bar's render elements for one output: the background rect and,
/// if the label rasterizes to anything, the text run on top of it. Returns an
/// empty `Vec` when the bar is disabled, the output has no usable geometry, or
/// the output is too small to hold the bar.
pub(crate) fn bar_elements<R>(
    state: &RubixState,
    renderer: &mut R,
    output: &Output,
    scale: f64,
) -> Vec<RubixRenderElement<R>>
where
    R: Renderer + ImportAll + ImportMem + GlesAccess,
    R::TextureId: Texture + Clone + Send + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    if !state.config.bar.enabled {
        return Vec::new();
    }

    let Some(output_geo) = state.space.output_geometry(output) else {
        return Vec::new();
    };
    let output_size = output_geo.size;
    if output_size.w <= 0 || output_size.h <= 0 {
        return Vec::new();
    }

    let bar = &state.config.bar;
    let bar_height = bar.height as i32;
    if bar_height <= 0 || bar_height > output_size.h {
        return Vec::new();
    }

    let strip_y = match bar.position {
        BarPosition::Top => 0,
        BarPosition::Bottom => output_size.h - bar_height,
    };

    let (background_abs10k, foreground_abs10k) = match state.theme() {
        Some(theme) => (theme.surface.abs10k, theme.foreground.abs10k),
        None => FALLBACK_COLORS,
    };
    let background_srgb = abs10k_to_display_srgb(background_abs10k, state.sdr_white_nits);
    let foreground_srgb = abs10k_to_display_srgb(foreground_abs10k, state.sdr_white_nits);

    let strip_loc_physical: Point<i32, Physical> =
        Point::from((0, strip_y)).to_f64().to_physical_precise_round(scale);
    let strip_size_physical =
        smithay::utils::Size::<i32, smithay::utils::Logical>::from((output_size.w, bar_height))
            .to_physical_precise_round(scale);
    let strip_geo = Rectangle::<i32, Physical>::new(strip_loc_physical, strip_size_physical);

    let background = SolidColorRenderElement::new(
        Id::new(),
        strip_geo,
        smithay::backend::renderer::utils::CommitCounter::default(),
        Color32F::new(background_srgb[0], background_srgb[1], background_srgb[2], 1.0),
        Kind::Unspecified,
    );

    // Front of the returned Vec is topmost within the bar's own slice, same
    // convention `compose_output_elements` uses for the whole frame -- so the
    // text goes in before the background.
    let mut elements: Vec<RubixRenderElement<R>> = Vec::new();

    // Padding of strip_height / 4 rather than a bare literal: it scales with
    // the bar instead of looking cramped or oversized at other heights.
    let padding = bar_height / 4;
    let color_u8 = |c: [f32; 3]| -> [u8; 4] {
        [
            (c[0].clamp(0.0, 1.0) * 255.0).round() as u8,
            (c[1].clamp(0.0, 1.0) * 255.0).round() as u8,
            (c[2].clamp(0.0, 1.0) * 255.0).round() as u8,
            255,
        ]
    };
    if let Some(run) = state.text.borrow_mut().render(&bar.label, bar.font_size, color_u8(foreground_srgb))
    {
        // Vertically centre using `line_height`, never `ink_size` -- centring
        // the ink box is the jitter bug `TextRun` exists to prevent.
        let layout_top = strip_y as f32 + (bar_height as f32 - run.line_height) / 2.0;
        let layout_origin_logical = (padding as f32, layout_top);
        let draw_origin_logical = (
            layout_origin_logical.0 + run.ink_offset.0 as f32,
            layout_origin_logical.1 + run.ink_offset.1 as f32,
        );
        let draw_loc_physical: Point<i32, Physical> = Point::<f64, smithay::utils::Logical>::from((
            draw_origin_logical.0 as f64,
            draw_origin_logical.1 as f64,
        ))
        .to_physical_precise_round(scale);

        if let Ok(text_elem) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            draw_loc_physical.to_f64(),
            &run.buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        ) {
            elements.push(RubixRenderElement::Memory(text_elem));
        }
    }

    elements.push(RubixRenderElement::Solid(background));
    elements
}
