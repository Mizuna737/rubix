//! Single shared per-output render-element assembly, replacing five duplicated
//! copies (`src/udev.rs` x2, `src/winit.rs`, `src/screencopy.rs`,
//! `src/portal/capture.rs`) of the same layer-bucketing + space + cursor +
//! wallpaper ordering. All five are now converted. `WrapMode::HdrComposite`
//! is the abs10k composite pass: it installs no pass-wide decode program, so
//! every element must be wrapped with its own.

use smithay::backend::renderer::element::{
    surface::WaylandSurfaceRenderElement, AsRenderElements, RenderElement,
};
use smithay::backend::renderer::{ImportAll, ImportMem, Renderer, Texture};
use smithay::desktop::layer_map_for_output;
use smithay::utils::Scale;

use crate::color_management::DecodeKind;
use crate::cursor::{dnd_icon_render_elements, pointer_render_elements, RubixRenderElement};
use crate::rounding::{GlesAccess, SpaceMode};
use crate::state::RubixState;

pub(crate) enum CursorMode {
    /// No cursor element (portal capture).
    Hidden,
    /// Drawn at the raw global pointer location. Correct only where the output
    /// is assumed to be a single head at the origin (winit, screencopy).
    Global,
    /// Drawn only when the pointer is over this output, translated into the
    /// output's local space. Without the translation every extra monitor
    /// redraws a phantom cursor at the global coordinate.
    OutputLocal,
}

/// How each layer-shell and wallpaper element is wrapped before it enters the
/// list. The HDR composite pass installs no pass-wide program, so an element
/// without its own draws through whatever the previous one left in the slot --
/// which is why this is a mode rather than a bool.
pub(crate) enum WrapMode<'a> {
    /// Ordinary SDR output and capture destinations. `tonemap` tone-maps HDR
    /// surfaces and the wallpaper down; false passes them through unwrapped.
    Sdr { tonemap: bool },
    /// The abs10k composite pass: every element carries its own decode program.
    HdrComposite {
        round: &'a Option<crate::rounding::RoundShaders>,
        /// Forwarded to `space_elements`. The wallpaper element itself is always
        /// requested untone-mapped on this pass and wrapped with its own decode below.
        wallpaper_sdr_tonemap: bool,
    },
}

/// Wraps a bare element with its own decode program when rounding shaders are
/// available, matching by variant since there is no enum slot for "a
/// `RoundedElement` around the whole enum" (and adding one would be
/// self-referential). Used by the HDR composite pass for the cursor and the
/// wallpaper, both of which are already a `RubixRenderElement` rather than a
/// bare surface element by the time they need wrapping.
fn wrap_decode<R>(
    elem: RubixRenderElement<R>,
    round: &Option<crate::rounding::RoundShaders>,
    kind: DecodeKind,
    scale: f64,
    sdr_white_nits: f32,
) -> RubixRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem + GlesAccess,
    R::TextureId: Texture + Clone + Send + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    match (round, elem) {
        (Some(shaders), RubixRenderElement::Surface(inner)) => {
            RubixRenderElement::Rounded(crate::rounding::RoundedElement::with_program(
                inner,
                shaders,
                crate::rounding::RoundMode::Decode(kind),
                Scale::from(scale),
                sdr_white_nits,
            ))
        }
        (Some(shaders), RubixRenderElement::Memory(inner)) => {
            RubixRenderElement::RoundedMemory(crate::rounding::RoundedElement::with_program(
                inner,
                shaders,
                crate::rounding::RoundMode::Decode(kind),
                Scale::from(scale),
                sdr_white_nits,
            ))
        }
        // Compositor-drawn solid chrome (the bar background). Without this arm
        // a solid rect reaches the HDR composite pass carrying no program and
        // draws through whatever the previous element left in the slot --
        // correct on SDR, wrong on the HDR output, and silent on both.
        (Some(shaders), RubixRenderElement::Solid(inner)) => {
            RubixRenderElement::RoundedSolid(crate::rounding::RoundedElement::with_program(
                inner,
                shaders,
                crate::rounding::RoundMode::Decode(kind),
                Scale::from(scale),
                sdr_white_nits,
            ))
        }
        (_, other) => other,
    }
}

pub(crate) struct ComposeOptions<'a> {
    pub scale: f64,
    pub space_mode: SpaceMode,
    /// How layer-shell and wallpaper elements are wrapped before entering the
    /// list.
    pub wrap: WrapMode<'a>,
    /// False only for portal capture, which appends no wallpaper today.
    pub include_wallpaper: bool,
    pub cursor: CursorMode,
    /// Exclusive fullscreen: drop layer-shell top/overlay and animation tweens.
    /// Anything above the candidate element is fatal to primary-plane promotion.
    pub suppress_chrome: bool,
}

pub(crate) fn compose_output_elements<R>(
    state: &RubixState,
    renderer: &mut R,
    output: &smithay::output::Output,
    opts: ComposeOptions<'_>,
    tweens: Vec<RubixRenderElement<R>>,
    tween_backdrops: Vec<RubixRenderElement<R>>,
) -> Vec<RubixRenderElement<R>>
where
    R: Renderer + ImportAll + ImportMem + GlesAccess,
    R::TextureId: Texture + Clone + Send + 'static,
    RubixRenderElement<R>: RenderElement<R>,
{
    // Two separate locals, not one: on `HdrComposite` the pass forwards the
    // caller's flag to `space_elements` but always asks `wallpaper.element`
    // for the untone-mapped image, since this pass installs its own
    // per-element decode instead of a tone-map program. On `Sdr` both are
    // the same `tonemap` bool.
    let (space_tonemap, wallpaper_tonemap) = match opts.wrap {
        WrapMode::Sdr { tonemap } => (tonemap, tonemap),
        WrapMode::HdrComposite { wallpaper_sdr_tonemap, .. } => (wallpaper_sdr_tonemap, false),
    };

    let mut background: Vec<RubixRenderElement<R>> = Vec::new();
    let mut bottom: Vec<RubixRenderElement<R>> = Vec::new();
    let mut top: Vec<RubixRenderElement<R>> = Vec::new();
    let mut overlay: Vec<RubixRenderElement<R>> = Vec::new();
    {
        use smithay::wayland::shell::wlr_layer::Layer;
        let map = layer_map_for_output(output);
        for layer in map.layers() {
            let Some(geo) = map.layer_geometry(layer) else { continue };
            let loc = geo.loc.to_physical_precise_round(opts.scale);
            let elems = layer.render_elements::<WaylandSurfaceRenderElement<R>>(
                renderer,
                loc,
                Scale::from(opts.scale),
                1.0,
            );
            let surface = layer.wl_surface().clone();
            let elems: Vec<RubixRenderElement<R>> = match opts.wrap {
                WrapMode::Sdr { tonemap: true } => elems
                    .into_iter()
                    .map(|e| {
                        crate::rounding::tonemap_sdr_element(
                            renderer,
                            &surface,
                            e,
                            opts.scale,
                            state.sdr_white_nits,
                        )
                    })
                    .collect(),
                WrapMode::Sdr { tonemap: false } => {
                    elems.into_iter().map(RubixRenderElement::Surface).collect()
                }
                // This pass installs no pass-wide program, so every layer
                // surface must carry its own -- and its *own declared* kind
                // via `surface_decode_kind`, not a fixed one, since an HDR
                // wallpaper tool on the background layer has made a
                // statement about its content this pass must not ignore.
                WrapMode::HdrComposite { round, .. } => {
                    let kind = crate::color_management::surface_decode_kind(&surface);
                    match round {
                        Some(shaders) => elems
                            .into_iter()
                            .map(|e| {
                                RubixRenderElement::Rounded(
                                    crate::rounding::RoundedElement::with_program(
                                        e,
                                        shaders,
                                        crate::rounding::RoundMode::Decode(kind),
                                        Scale::from(opts.scale),
                                        state.sdr_white_nits,
                                    ),
                                )
                            })
                            .collect(),
                        None => elems.into_iter().map(RubixRenderElement::Surface).collect(),
                    }
                }
            };
            match layer.layer() {
                Layer::Background => background.extend(elems),
                Layer::Bottom => bottom.extend(elems),
                Layer::Top => top.extend(elems),
                Layer::Overlay => overlay.extend(elems),
            }
        }
    }

    let (space_elements, backdrop_elements) = crate::rounding::space_elements(
        state,
        renderer,
        output,
        opts.scale,
        opts.space_mode,
        space_tonemap,
    );

    let mut tweens = tweens;
    let mut tween_backdrops = tween_backdrops;

    // Compositor-drawn bar: above every window and animation, below
    // layer-shell top/overlay (notifications, launchers still draw over it).
    let mut bar = crate::bar::bar_elements(state, renderer, output, opts.scale);
    // Compositor chrome, never client HDR content, so it is always wrapped
    // `Decode(Sdr)` on the HDR composite pass -- same treatment as the cursor.
    bar = match opts.wrap {
        WrapMode::HdrComposite { round, .. } => bar
            .into_iter()
            .map(|e| wrap_decode(e, round, DecodeKind::Sdr, opts.scale, state.sdr_white_nits))
            .collect(),
        WrapMode::Sdr { .. } => bar,
    };

    // Exclusive fullscreen: chrome above the game (layer-shell top/overlay,
    // animation ghosts/reveal-tweens, the bar) must not render, both because
    // it would be incorrect (a bar shouldn't paint over a fullscreen game)
    // and because anything above the candidate element in the final list is
    // fatal to primary-plane promotion. `bottom`/`background` are left as
    // built -- they're culled by `DrmCompositor`'s opaque short-circuit and
    // are the fallback if promotion fails for some other reason.
    if opts.suppress_chrome {
        top.clear();
        overlay.clear();
        tweens.clear();
        tween_backdrops.clear();
        bar.clear();
    }

    // Cursor built last (it also needs `renderer`), same "collect before the
    // combined render call" discipline as the ghost/layer lists above so the
    // mutable borrow is released before the combined render call.
    // Only the output the pointer is actually over draws it, and the location is
    // translated into that output's local space (subtract its geometry origin) --
    // otherwise every output redraws the cursor at the raw global coordinate,
    // producing a phantom cursor per extra monitor.
    //
    // The cursor is NOT suppressed under exclusive fullscreen. `DrmCompositor`
    // assigns it to the hardware cursor plane from this element list
    // (`FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT`), so a cursor element does not
    // land in `primary_plane_elements` and does not block primary-plane
    // promotion. There is no separate hw-cursor path to fall back on: dropping
    // these elements drops the cursor outright, everywhere, for as long as any
    // fullscreen window covers the output.
    let cursor_elements = match opts.cursor {
        CursorMode::Hidden => Vec::new(),
        CursorMode::Global => pointer_render_elements(
            renderer,
            &state.cursor_status,
            state.pointer_location,
            opts.scale,
        ),
        CursorMode::OutputLocal => {
            let output_geo = state.space.output_geometry(output);
            match output_geo {
                Some(geo) if geo.to_f64().contains(state.pointer_location) => {
                    let local = state.pointer_location - geo.loc.to_f64();
                    pointer_render_elements(renderer, &state.cursor_status, local, opts.scale)
                }
                _ => Vec::new(),
            }
        }
    };
    // The cursor is compositor chrome, never client HDR content, so it is
    // always wrapped `Decode(Sdr)` on the HDR composite pass -- same as
    // every other non-content element here.
    let cursor_elements: Vec<RubixRenderElement<R>> = match opts.wrap {
        WrapMode::HdrComposite { round, .. } => cursor_elements
            .into_iter()
            .map(|e| wrap_decode(e, round, DecodeKind::Sdr, opts.scale, state.sdr_white_nits))
            .collect(),
        WrapMode::Sdr { .. } => cursor_elements,
    };

    // DnD ghost icon, built right after the cursor and with exactly the same
    // per-output/translation rules (see the cursor block above): it must
    // appear only on the one output the pointer is actually over, translated
    // into that output's local space, and not at all under `CursorMode::Hidden`
    // (portal capture) -- getting either wrong paints a phantom icon on every
    // extra monitor, same failure mode the cursor comment warns about.
    // `dnd_icon` is `None` outside a drag (the overwhelming common case), so
    // this is a field check away from a no-op in the hot path -- kept a
    // separate Vec/variable rather than folded into `cursor_elements` because
    // that one can be promoted to the hardware cursor plane
    // (`ALLOW_CURSOR_PLANE_SCANOUT`), and a client surface tree must not land
    // there.
    let dnd_icon_elements = match (&opts.cursor, state.dnd_icon.as_ref()) {
        (CursorMode::Hidden, _) | (_, None) => Vec::new(),
        (CursorMode::Global, Some(icon)) => dnd_icon_render_elements(
            renderer,
            &icon.surface,
            icon.offset,
            state.pointer_location,
            opts.scale,
        ),
        (CursorMode::OutputLocal, Some(icon)) => {
            let output_geo = state.space.output_geometry(output);
            match output_geo {
                Some(geo) if geo.to_f64().contains(state.pointer_location) => {
                    let local = state.pointer_location - geo.loc.to_f64();
                    dnd_icon_render_elements(renderer, &icon.surface, icon.offset, local, opts.scale)
                }
                _ => Vec::new(),
            }
        }
    };
    // The icon is compositor-mediated chrome on this pass (it never reaches
    // the client's own HDR content pipeline), so it gets the same `Decode(Sdr)`
    // wrap as the cursor -- otherwise it comes out colour-mangled on an HDR
    // output.
    let dnd_icon_elements: Vec<RubixRenderElement<R>> = match opts.wrap {
        WrapMode::HdrComposite { round, .. } => dnd_icon_elements
            .into_iter()
            .map(|e| wrap_decode(e, round, DecodeKind::Sdr, opts.scale, state.sdr_white_nits))
            .collect(),
        WrapMode::Sdr { .. } => dnd_icon_elements,
    };

    // Cursor prepended -- front of the Vec is topmost, and it must draw above
    // everything else, including overlay layers. The DnD icon goes directly
    // after it (under the cursor, above everything else) and is never merged
    // into `cursor_elements` -- see the comment on `dnd_icon_elements` above.
    let mut elements: Vec<RubixRenderElement<R>> = Vec::new();
    elements.extend(cursor_elements);
    elements.extend(dnd_icon_elements);
    elements.extend(overlay);
    elements.extend(top);
    elements.extend(bar);
    elements.extend(tweens);
    elements.extend(tween_backdrops);
    elements.extend(space_elements);
    // Per-window backdrop quads (see src/wallpaper.rs). Deliberately below
    // every window rather than interleaved per-window: the quad is opaque
    // within its window's rect, so it occludes not just the wallpaper but
    // anything else physically below that rect at this point in the list --
    // layer-shell `bottom`/`background`, and (accepted simplification) any
    // other window stacked further down that happens to overlap the same
    // screen area. Frost only ever applies over wallpaper, never over another
    // window's own content.
    elements.extend(backdrop_elements);
    elements.extend(bottom);
    elements.extend(background);
    // Last, so it sits under every client. Only wrapped in a tone-map program
    // when this output is SDR and the image is not -- an HDR output installs
    // its decode for the whole pass instead. See src/wallpaper.rs.
    if opts.include_wallpaper
        && let Some(region) = state.space.output_geometry(output)
        && let Some((kind, wallpaper)) = state.wallpaper.element(
            renderer,
            &output.name(),
            region.size,
            opts.scale,
            wallpaper_tonemap,
        )
    {
        let wallpaper = match opts.wrap {
            WrapMode::HdrComposite { round, .. } => {
                wrap_decode(wallpaper, round, kind, opts.scale, state.sdr_white_nits)
            }
            WrapMode::Sdr { .. } => wallpaper,
        };
        elements.push(wallpaper);
    }
    elements
}
