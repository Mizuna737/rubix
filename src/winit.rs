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
use crate::{CalloopData, RubixState};

pub fn init_winit(
    event_loop: &mut EventLoop<CalloopData>,
    data: &mut CalloopData,
) -> Result<(), Box<dyn std::error::Error>> {
    let display_handle = &mut data.display_handle;
    let state = &mut data.state;

    let (mut backend, winit) = winit::init()?;

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
        },
    );
    let _global = output.create_global::<RubixState>(display_handle);
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);

    state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    // SAFETY: edition 2024 marks set_var unsafe because concurrent env reads
    // from other threads would be UB. We set this once at startup, before any
    // client threads exist, so there is no concurrent access.
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
    }

    event_loop.handle().insert_source(winit, move |event, _, data| {
        let display = &mut data.display_handle;
        let state = &mut data.state;

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
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => {
                let size = backend.window_size();
                let damage = Rectangle::from_size(size);

                state.step_animations();

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
                    let space_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = state
                        .space
                        .output_geometry(&output)
                        .map(|geo| state.space.render_elements_for_region(renderer, &geo, scale, 1.0))
                        .unwrap_or_default();

                    // Ghost elements for any in-flight rotation wrap, built from
                    // `active_ghosts` (populated by `step_animations` above, same
                    // frame). Output scale is 1.0 here, so a logical Pos maps
                    // numerically to physical directly -- if that ever changes,
                    // this needs `.to_physical_precise_round(scale)` from a
                    // logical point instead. Rendered between top/overlay and
                    // the space so ghosts stay above tiled windows but below
                    // chrome-style layer surfaces.
                    let ghost_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = state
                        .active_ghosts
                        .iter()
                        .filter_map(|(id, pos)| state.windows.get(id).map(|w| (w.clone(), *pos)))
                        .flat_map(|(w, pos)| {
                            w.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                                renderer,
                                Point::<i32, Physical>::from((pos.x, pos.y)),
                                Scale::from(1.0),
                                1.0,
                            )
                        })
                        .collect();

                    // Cursor built last (it needs `renderer` too) so it stays
                    // in the same "collect before the combined render call"
                    // discipline as the ghost/layer lists above -- the
                    // borrow is released before `damage_tracker.render_output`.
                    let cursor_elements = pointer_render_elements(
                        renderer,
                        &state.cursor_status,
                        state.pointer_location,
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
                    elements.extend(space_elements.into_iter().map(RubixRenderElement::Surface));
                    elements.extend(bottom.into_iter().map(RubixRenderElement::Surface));
                    elements.extend(background.into_iter().map(RubixRenderElement::Surface));

                    damage_tracker
                        .render_output(renderer, &mut framebuffer, 0, &elements, [0.1, 0.1, 0.1, 1.0])
                        .unwrap();
                }
                backend.submit(Some(&[damage])).unwrap();

                state.space.elements().for_each(|window| {
                    window.send_frame(
                        &output,
                        state.start_time.elapsed(),
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    )
                });

                state.space.refresh();
                state.popups.cleanup();
                let _ = display.flush_clients();

                // Ask for redraw to schedule new frame.
                backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => {
                state.loop_signal.stop();
            }
            _ => (),
        };
    })?;

    Ok(())
}
