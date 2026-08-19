use std::time::Duration;

use smithay::{
    backend::{
        renderer::{
            damage::OutputDamageTracker,
            element::{surface::WaylandSurfaceRenderElement, AsRenderElements},
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    desktop::layer_map_for_output,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Physical, Point, Rectangle, Scale, Transform},
    wayland::shell::wlr_layer::Layer,
};

use crate::cursor::{pointer_render_elements, RubixRenderElement};
use crate::RubixState;

pub fn init_winit(
    event_loop: &mut EventLoop<RubixState>,
    data: &mut RubixState,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit) = winit::init()?;

    // TODO(winit dmabuf): winit never creates the zwp_linux_dmabuf_v1 global
    // (unlike udev's device_added). Nested-dev clients fall back to SHM,
    // which is fine for the winit dev backend; the udev backend is the
    // daily-driver path and does advertise the dmabuf global.

    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };

    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<RubixState>(&data.display_handle);
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);

    data.space.map_output(&output, (0, 0));
    data.bind_output_monitor(&output);

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    // XDG_SESSION_TYPE=wayland steers Chromium/Electron `auto` backend detection
    // onto Wayland -- it keys off the session type, not WAYLAND_DISPLAY.
    // SAFETY: edition 2024 marks set_var unsafe because concurrent env reads
    // from other threads would be UB. We set this once at startup, before any
    // client threads exist, so there is no concurrent access.
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &data.socket_name);
        std::env::set_var("XDG_SESSION_TYPE", "wayland");
    }

    event_loop.handle().insert_source(winit, move |event, _, data| {
        match event {
            WinitEvent::Resized { size, .. } => {
                output.change_current_state(
                    Some(Mode {
                        size,
                        refresh: 60_000,
                    }),
                    None,
                    None,
                    None,
                );
            }
            WinitEvent::Input(event) => data.process_input_event(event),
            WinitEvent::Redraw => {
                let size = backend.window_size();
                let damage = Rectangle::from_size(size);

                data.step_animations();

                {
                    let (renderer, mut framebuffer) = backend.bind().unwrap();

                    // Z-order, top-to-bottom: overlay -> top -> ghosts -> tiled
                    // windows (space) -> bottom -> background. `render_output`'s
                    // helper (smithay::desktop::space::render_output) only has a
                    // slot for `custom_elements` rendered ABOVE the space -- no
                    // slot below it -- so a background layer passed that way
                    // would draw on top of every window (the classic inversion).
                    // Building one combined element list ourselves and driving
                    // `OutputDamageTracker` directly is the only way to get the
                    // wallpaper beneath the tiled windows.
                    let scale = 1.0_f64;
                    let mut background: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
                    let mut bottom: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
                    let mut top: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
                    let mut overlay: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
                    {
                        let map = layer_map_for_output(&output);
                        for layer in map.layers() {
                            let Some(geo) = map.layer_geometry(layer) else { continue };
                            let loc = geo.loc.to_physical_precise_round(scale);
                            let elems = layer
                                .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                                    renderer,
                                    loc,
                                    Scale::from(scale),
                                    1.0,
                                );
                            match layer.layer() {
                                Layer::Background => background.extend(elems),
                                Layer::Bottom => bottom.extend(elems),
                                Layer::Top => top.extend(elems),
                                Layer::Overlay => overlay.extend(elems),
                            }
                        }
                    }

                    // `space_render_elements` is intentionally not used here: as
                    // of smithay 0.7 (wayland_frontend feature) it already folds
                    // the output's LayerMap into its result, which would
                    // double-render every layer surface if combined with the
                    // pass above. `render_elements_for_region` gives the space's
                    // own contribution alone.
                    let space_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = data
                        .space
                        .output_geometry(&output)
                        .map(|geo| data.space.render_elements_for_region(renderer, &geo, scale, 1.0))
                        .unwrap_or_default();

                    // Ghost elements for any in-flight rotation wrap, built from
                    // `active_ghosts` (populated by `step_animations` above, same
                    // frame). Output scale is 1.0 here, so a logical Pos maps
                    // numerically to physical directly -- if that ever changes,
                    // this needs `.to_physical_precise_round(scale)` from a
                    // logical point instead. Rendered between top/overlay and
                    // the space so ghosts stay above tiled windows but below
                    // chrome-style layer surfaces.
                    let ghost_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = data
                        .active_ghosts
                        .iter()
                        .filter_map(|(id, pos)| data.windows.get(id).map(|w| (w.clone(), *pos)))
                        .flat_map(|(w, pos)| {
                            w.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                                renderer,
                                Point::<i32, Physical>::from((pos.x, pos.y)),
                                Scale::from(1.0),
                                1.0,
                            )
                        })
                        .collect();

                    // Windows mid-Reveal, drawn scaled about their own centre. They are
                    // unmapped from the Space for the tween's duration, so this list is their
                    // only draw -- dropping it makes them vanish for the animation rather than
                    // merely render unscaled. Same z-slot as the ghosts, for the same reason.
                    let scaled_elements = crate::state::reveal_scale_elements(data, renderer);

                    // Cursor built last (it needs `renderer` too) so it stays
                    // in the same "collect before the combined render call"
                    // discipline as the ghost/layer lists above -- the
                    // borrow is released before `damage_tracker.render_output`.
                    let cursor_elements = pointer_render_elements(
                        renderer,
                        &data.cursor_status,
                        data.pointer_location,
                        scale,
                    );

                    // Collected before this point so the mutable `renderer`
                    // borrow used to build each element list is released in
                    // time for `damage_tracker.render_output` below. Cursor is
                    // prepended -- front of the Vec is topmost, and it must
                    // draw above everything else, including overlay layers.
                    let mut elements: Vec<RubixRenderElement<GlesRenderer>> = Vec::new();
                    elements.extend(cursor_elements);
                    elements.extend(overlay.into_iter().map(RubixRenderElement::Surface));
                    elements.extend(top.into_iter().map(RubixRenderElement::Surface));
                    elements.extend(ghost_elements.into_iter().map(RubixRenderElement::Surface));
                    elements.extend(scaled_elements.into_iter().map(RubixRenderElement::Rescaled));
                    // The winit backend has no HDR output, so borders always take their plain
                    // configured color -- `hdr = false` makes the whole luminance mechanism inert.
                    elements.extend(
                        crate::decoration::border_elements(data, &output, scale, false)
                            .into_iter()
                            .map(RubixRenderElement::Solid),
                    );
                    elements.extend(space_elements.into_iter().map(RubixRenderElement::Surface));
                    elements.extend(bottom.into_iter().map(RubixRenderElement::Surface));
                    elements.extend(background.into_iter().map(RubixRenderElement::Surface));

                    damage_tracker
                        .render_output(renderer, &mut framebuffer, 0, &elements, [0.1, 0.1, 0.1, 1.0])
                        .unwrap();
                }
                backend.submit(Some(&[damage])).unwrap();

                // Service any screencopy captures now that this frame is
                // presented. Re-bind to reach the renderer (the earlier bind's
                // borrow ended with the render block); fulfill re-renders the
                // output into its own offscreen buffer, so the winit surface we
                // just submitted is untouched.
                if !data.pending_screencopy.is_empty() {
                    let (renderer, _fb) = backend.bind().unwrap();
                    crate::screencopy::fulfill_pending(data, renderer, &output);
                }

                data.space.elements().for_each(|window| {
                    window.send_frame(
                        &output,
                        data.start_time.elapsed(),
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    )
                });
                // Layer-shell surfaces (waybar, etc.) need frame callbacks too,
                // or they paint their first buffer and freeze.
                {
                    let map = layer_map_for_output(&output);
                    for layer in map.layers() {
                        layer.send_frame(
                            &output,
                            data.start_time.elapsed(),
                            Some(Duration::ZERO),
                            |_, _| Some(output.clone()),
                        );
                    }
                }

                data.space.refresh();
                data.popups.cleanup();
                let _ = data.display_handle.flush_clients();

                // Ask for redraw to schedule new frame.
                backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => {
                data.loop_signal.stop();
            }
            _ => (),
        };
    })?;

    Ok(())
}
