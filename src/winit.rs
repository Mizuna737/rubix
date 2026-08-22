use std::time::Duration;

use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent},
    },
    desktop::layer_map_for_output,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Rectangle, Transform},
};

use crate::cursor::RubixRenderElement;
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

                    // Decorated ghost + reveal-tween elements, built from
                    // `active_ghosts`/`active_scales` (populated by
                    // `step_animations` above, same frame). Rendered between
                    // top/overlay and the space so tween windows stay above
                    // tiled windows but below chrome-style layer surfaces. See
                    // `tween_elements` for the coordinate-space caveat (`pos`
                    // is not region-local, same as this replaced).
                    let (tween_elements, tween_backdrops) = crate::rounding::tween_elements(
                        data,
                        renderer,
                        &output,
                        scale,
                        crate::rounding::SpaceMode::Fixed(crate::rounding::RoundMode::Plain),
                        false,
                    );

                    // winit is the dev backend: no HDR output, no capture, so
                    // the wallpaper is drawn plainly. An HDR image here is not
                    // tone-mapped (`tonemap: false`) -- the nested window has
                    // no colour pipeline to tone-map into.
                    let elements: Vec<RubixRenderElement<GlesRenderer>> =
                        crate::compose::compose_output_elements(
                            data,
                            renderer,
                            &output,
                            crate::compose::ComposeOptions {
                                scale,
                                space_mode: crate::rounding::SpaceMode::Fixed(
                                    crate::rounding::RoundMode::Plain,
                                ),
                                wrap: crate::compose::WrapMode::Sdr { tonemap: false },
                                include_wallpaper: true,
                                cursor: crate::compose::CursorMode::Global,
                                suppress_chrome: false,
                            },
                            tween_elements,
                            tween_backdrops,
                        );

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
