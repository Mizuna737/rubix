# Rubix

A from-scratch Wayland compositor in Rust, built on [Smithay](https://github.com/Smithay/smithay),
with a **Rubik's-cube spatial model**: each monitor is a 2D grid of window groups. Columns are
positionally pinned and never move; what moves is content flowing through them. A horizontal move
rotates the active group across every column, off-screen ones included, while a vertical move
scrolls one column's pointer through its own group stack.

Status: **daily driver.** Rubix has been the author's primary compositor since 2026-07-29. It runs
standalone on the TTY (udev/DRM/libinput), and the session layer — bars, notifications, clipboard,
keyring, XWayland, native-Wayland Electron — is wired and verified. It is not yet packaged or
recommended for anyone else's daily use.

See [`docs/design.md`](docs/design.md) for the full design schema,
[`docs/hdr-status.md`](docs/hdr-status.md) for the colour pipeline, and
[`docs/theming.md`](docs/theming.md) for wallpaper-derived theming.

## What works

- **Cube model** — column/group/tiling-tree layout, binary splits, rotation on both axes.
- **Bare-metal session** — udev/DRM/libinput, multi-monitor with output rotation, XWayland,
  layer-shell (bars, notifications, launchers), screen power management.
- **HDR, composited** — not fullscreen-only. HDR and SDR windows coexist tiled: the frame is
  composited in linear light and encoded to PQ, with per-window decode for PQ and Windows-scRGB
  content. HDR content is tone-mapped down on SDR outputs, so a mixed monitor set is correct on
  both heads.
- **Gaming** — fullscreen sits outside the tiling grid; direct scanout for native Wayland clients;
  HDR games via GE-Proton + gamescope.
- **Screencast portal** — in-process, per-window and per-monitor, with capture tone-mapping.
- **Wallpaper** — drawn by the compositor itself, including HDR AVIF, with slideshow support.
  External wallpaper tools cannot tag an image through `wp_color_management_v1`.
- **Decoration** — server-side borders, rounded corners, per-window opacity and fade, driven by
  `app_id`/`title` rules.
- **IPC** — a Unix-socket command/event surface for external tooling.

## Build

```sh
cargo build --release
```

Requires a system Rust toolchain (edition 2024) with `rustfmt` and `clippy`, plus development
headers for libinput, libdrm, libseat, libxkbcommon, and PipeWire. `cargo test` runs the model,
config, HDR, and wallpaper suites.

## Project layout

```
src/
├── main.rs                entry: CLI, tracing init, event-loop bootstrap
├── state.rs               top-level compositor state
├── model/                 spatial model — grid, tiling tree, pure geometry
├── handlers/              Smithay protocol handlers (xdg, layer-shell, dmabuf, xwayland, …)
├── grabs/                 interactive move/resize
├── portal/                xdg-desktop-portal ScreenCast, in-process
├── udev.rs / winit.rs     bare-metal and nested backends
├── hdr*.rs                linear working space, PQ/scRGB decode, tone-map shaders
├── color_management.rs    wp_color_management_v1
├── wallpaper.rs           image decode, placement, slideshow
├── decoration.rs          borders, corners, opacity rules
├── config.rs              TOML load / validate / hot-reload
└── ipc.rs                 Unix-socket control
config/default/           annotated reference config, split by area
docs/                      design schema, HDR findings, theming, portal notes
```

## Conventions

Idiomatic Rust naming — `config.toml` keys mirror the Rust identifiers exactly (`snake_case`
fields, `PascalCase` enum variants, no serde renames) so config and code cannot drift.

## License

MIT © 2026 Max Hefley
