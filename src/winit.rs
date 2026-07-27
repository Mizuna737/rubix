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
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Physical, Point, Rectangle, Scale, Transform},
};

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

                    // Ghost elements for any in-flight rotation wrap, built from
                    // `active_ghosts` (populated by `step_animations` above, same
                    // frame). Output scale is 1.0 here, so a logical Pos maps
                    // numerically to physical directly -- if that ever changes,
                    // this needs `.to_physical_precise_round(scale)` from a
                    // logical point instead.
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

                    smithay::desktop::space::render_output::<
                        _,
                        WaylandSurfaceRenderElement<GlesRenderer>,
                        _,
                        _,
                    >(
                        &output,
                        renderer,
                        &mut framebuffer,
                        1.0,
                        0,
                        [&state.space],
                        &ghost_elements,
                        &mut damage_tracker,
                        [0.1, 0.1, 0.1, 1.0],
                    )
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
