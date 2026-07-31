//! Software cursor: xcursor theme loading + render-element construction.
//!
//! Known limitations, recorded here rather than solved this pass:
//! - Animated cursors render frame 0 only -- no animation timer yet.
//! - Output scale is assumed 1.0, same caveat as the ghost/layer element
//!   lists already built by winit.rs/udev.rs.
//! - Single-monitor: `pointer_location` (state.rs) is clamped to the first
//!   output only (input.rs); multi-monitor pointer traversal is later work.
//! - Under winit the HOST compositor also draws its own cursor, so you will
//!   see two cursors nested during development. Expected -- the real target
//!   is the udev/TTY backend. No host-cursor-hiding is added here.

use std::cell::RefCell;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                texture::TextureRenderElement,
                Kind,
            },
            ImportAll, ImportMem, Renderer, RendererSuper, Texture,
        },
    },
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    render_elements,
    utils::{Logical, Point, Transform},
    wayland::compositor,
};

// Unifies the surface-backed cursor path (a client-set `wl_surface`, e.g. an
// I-beam a text field requests) with the memory-backed path (our loaded
// xcursor theme image) behind one render-element type, so both backends can
// collect a single `Vec<RubixRenderElement<R>>` for the whole frame exactly
// like they already do for `WaylandSurfaceRenderElement<R>` alone (see
// winit.rs `elements` / udev.rs `render_surface`'s `elements`).
//
// Patterned on smithay's own `SpaceRenderElements<R, E>`
// (desktop/space/mod.rs:520-526), which unifies the same way for the space's
// internal element list -- same macro, same one-generic-parameter shape.
//
// `R` needs `ImportAll` because `WaylandSurfaceRenderElement`'s `RenderElement`
// impl requires it (backend/renderer/element/surface.rs:371-374) and
// `ImportMem` because `MemoryRenderBufferRenderElement`'s does
// (backend/renderer/element/memory.rs:631-635). Both bounds are already
// satisfied by `GlesRenderer` (winit) and by udev's `RubixRenderer<'_>` alias
// for `MultiRenderer<'_, '_, GbmGlesBackend<..>, GbmGlesBackend<..>>`, which
// implements `ImportMem` for any `R, T: GraphicsApi`
// (backend/renderer/multigpu/mod.rs:1956) with no lifetime-specific bound
// needed here -- the enum is generic over `R` alone, so udev's per-frame
// `RubixRenderer<'_>` and winit's `GlesRenderer` both just substitute for `R`
// at their respective call sites without the enum itself needing to know
// about the lifetime.
//
// `Texture = TextureRenderElement<R::TextureId>` (HDR Phase 2): the encode
// pass in `udev::render_surface`'s HDR branch wraps the linear 16F offscreen
// as a single fullscreen element. `TextureRenderElement<T>: RenderElement<R>`
// requires `R: Renderer<TextureId = T>` (backend/renderer/element/texture.rs
// ~685), which `T = R::TextureId` satisfies trivially -- no extra bound
// needed beyond what `render_elements!` already assumes for `R`. Used at
// `R = GlesRenderer` specifically (the encode pass calls `drm_output
// .render_frame` with `renderer.as_mut(): &mut GlesRenderer`, not the outer
// `MultiRenderer` -- see udev.rs's HDR branch comment for why), so in
// practice this variant only ever holds `TextureRenderElement<GlesTexture>`.
render_elements! {
    pub RubixRenderElement<R> where R: ImportAll + ImportMem;
    Surface = WaylandSurfaceRenderElement<R>,
    Memory = MemoryRenderBufferRenderElement<R>,
    Texture = TextureRenderElement<<R as RendererSuper>::TextureId>,
}

struct LoadedCursor {
    theme: String,
    size: u32,
    buffer: MemoryRenderBuffer,
    hotspot: Point<i32, Logical>,
}

thread_local! {
    // The compositor's calloop event loop runs entirely on the main thread,
    // so a thread-local is enough here -- no cross-thread access, no need for
    // a Mutex. Rebuilt only when XCURSOR_THEME/XCURSOR_SIZE change.
    static CURSOR_CACHE: RefCell<Option<LoadedCursor>> = const { RefCell::new(None) };
}

fn xcursor_theme() -> String {
    std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_string())
}

fn xcursor_size() -> u32 {
    std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24)
}

/// Load the named cursor from the theme. Tries "default" first, then
/// "left_ptr" -- some themes only ship one of the two names for the arrow.
///
/// xcursor files pack every nominal size AND every animation frame as
/// separate `Image`s in one blob; we pick the image whose `size` is closest
/// to the requested size and take it as-is. For an animated cursor that means
/// frame 0 of whichever size matched -- an accepted first cut per the spec;
/// building the animation timer is not done here (see module doc).
fn load_cursor(theme_name: &str, size: u32) -> Option<(MemoryRenderBuffer, Point<i32, Logical>)> {
    let theme = xcursor::CursorTheme::load(theme_name);
    let path = theme
        .load_icon("default")
        .or_else(|| theme.load_icon("left_ptr"))?;
    let content = std::fs::read(path).ok()?;
    let images = xcursor::parser::parse_xcursor(&content)?;
    let image = images
        .into_iter()
        .min_by_key(|img| (img.size as i64 - size as i64).abs())?;

    // xcursor's `pixels_rgba` is byte order R,G,B,A in memory, which is the
    // DRM/smithay `Abgr8888` layout (fourcc names list bytes MSB-first for a
    // little-endian dword, so "ABGR" reads back to front in memory as R,G,B,A).
    let buffer = MemoryRenderBuffer::from_slice(
        &image.pixels_rgba,
        Fourcc::Abgr8888,
        (image.width as i32, image.height as i32),
        1,
        Transform::Normal,
        None,
    );
    let hotspot = Point::from((image.xhot as i32, image.yhot as i32));
    Some((buffer, hotspot))
}

/// Render elements for the cursor this frame, front-to-back (a single element
/// today; a Vec keeps call sites uniform with the other element lists the
/// backends already build). Empty for `CursorImageStatus::Hidden`.
///
/// `scale` is the output scale -- 1.0 today, same single-scale assumption the
/// ghost/layer element code already makes (see winit.rs/udev.rs comments).
pub fn pointer_render_elements<R>(
    renderer: &mut R,
    cursor_status: &CursorImageStatus,
    pointer_location: Point<f64, Logical>,
    scale: f64,
) -> Vec<RubixRenderElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Texture + Clone + Send + 'static,
{
    match cursor_status {
        CursorImageStatus::Hidden => Vec::new(),

        // Patterned on anvil's client-surface-cursor path (not in the crates
        // cache; the shape here follows `render_elements_from_surface_tree`'s
        // own doc example at backend/renderer/element/surface.rs:90-101 plus
        // the hotspot lookup documented on `CursorImageSurfaceData`,
        // input/pointer/cursor_image.rs:14-28).
        CursorImageStatus::Surface(surface) => {
            let hotspot = compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map(|data| data.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            let loc = (pointer_location - hotspot.to_f64()).to_physical_precise_round::<f64, i32>(scale);
            render_elements_from_surface_tree(renderer, surface, loc, scale, 1.0, Kind::Cursor)
                .into_iter()
                .map(RubixRenderElement::Surface)
                .collect()
        }

        // Named (including the default arrow): draw the cached xcursor theme
        // image via a `MemoryRenderBufferRenderElement`, patterned directly on
        // the module's own worked example at
        // backend/renderer/element/memory.rs:94-97
        // (`MemoryRenderBufferRenderElement::from_buffer(&mut renderer, location, &buffer, None, None, None, Kind::Unspecified)`),
        // using `Kind::Cursor` here since this element IS the cursor.
        CursorImageStatus::Named(_) => {
            let theme = xcursor_theme();
            let size = xcursor_size();
            let stale = CURSOR_CACHE.with(|c| {
                c.borrow()
                    .as_ref()
                    .map(|cached| cached.theme != theme || cached.size != size)
                    .unwrap_or(true)
            });
            if stale {
                match load_cursor(&theme, size) {
                    Some((buffer, hotspot)) => CURSOR_CACHE.with(|c| {
                        *c.borrow_mut() = Some(LoadedCursor { theme, size, buffer, hotspot });
                    }),
                    None => tracing::warn!("failed to load xcursor theme {theme:?} size {size}"),
                }
            }

            CURSOR_CACHE.with(|c| {
                let cache = c.borrow();
                let Some(cached) = cache.as_ref() else {
                    return Vec::new();
                };
                let loc = (pointer_location - cached.hotspot.to_f64()).to_physical(scale);
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    loc,
                    &cached.buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    Ok(elem) => vec![RubixRenderElement::Memory(elem)],
                    Err(_) => {
                        tracing::warn!("failed to upload cursor texture");
                        Vec::new()
                    }
                }
            })
        }
    }
}
