# Rubix

A from-scratch Wayland compositor in Rust, built on [Smithay](https://github.com/Smithay/smithay),
with a **Rubik's-cube spatial model**: each monitor is a 2D grid of window groups. The viewport
slides across columns on the X axis (shared), while each column scrolls independently through its
group stack on the Y axis — plus three zoom levels (Grid → Square → Window).

Status: **early development** — bootstrapping. See [`docs/design.md`](docs/design.md) for the full design schema.

## Build

```sh
cargo run
```

Requires a system Rust toolchain (rustc/cargo 1.97+, edition 2024) with `rustfmt` and `clippy`.

## Project layout (target)

Modules are added when a phase needs them, not stubbed ahead of time — so the tree below is the
map, and the repo grows into it.

```
src/
├── main.rs          entry: CLI, tracing init, event-loop bootstrap
├── compositor.rs    top-level state + Smithay handler impls
├── model/           spatial data model — Monitor, Column, Group, TilingNode
├── input/           seat, keybindings, actions
├── render/          rendering + animation
├── config.rs        TOML load / validate
└── ipc.rs           Unix-socket control (later)
config/              default rubix.toml
docs/design.md       design schema (source of truth)
```

## Conventions

Idiomatic Rust naming: `snake_case` items, `CamelCase` types, `SCREAMING_SNAKE_CASE` consts —
a deliberate departure from the camelCase-everywhere used elsewhere in the author's projects,
adopted here to build fluency in idiomatic Rust.

## License

MIT © 2026 Max Hefley
