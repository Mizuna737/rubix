# Rubix — Wayland Compositor Design Schema

## Overview

Rubix is a from-scratch Wayland compositor written in Rust, built on the Smithay library. Its core metaphor is a **Rubik's cube**: each monitor exposes a 2D grid of window groups, navigated by sliding columns independently on the Y axis and the viewport across columns on the X axis. The name "Rubix" is unoccupied in the compositor/WM space.

---

## Core Mental Model

A monitor's surface is a **viewport** into a 2D grid. The grid has an arbitrary number of columns, N of which are visible at once. Each column has an arbitrary number of **groups** stacked vertically, one of which is active per column at any time. Each group is an independent dynamic tiling container.

```
          col 0       col 1       col 2       col 3  ...
row 0  [ group A ] [ group B ] [ group C ] [ group D ]
row 1  [ group E ] [ group F ] [ group G ] [ group H ]
row 2  [ group I ] [ group J ] [ group K ] [ group L ]
         ^                       ^
         col 0 active: row 0     col 2 active: row 2
         (each column scrolls independently on Y)

viewport (N=2 visible): shows col 0 + col 1 at current X offset
```

Navigation:
- **X axis (viewport-level):** slide viewport left/right to reveal different columns
- **Y axis (per-column):** each column independently scrolls through its group stack
- Columns scroll independently — col 0 can be on row 2 while col 1 is on row 0

---

## Data Model

### Hierarchy

```
Compositor
└── Monitor (one per physical display)
    ├── viewport_offset: usize       -- which column is leftmost visible
    ├── visible_columns: usize       -- N columns shown simultaneously
    └── columns: Vec<Column>
        └── Column
            ├── width: u32           -- configurable, in pixels or percentage
            ├── active_row: usize    -- which group is currently visible
            └── groups: Vec<Group>
                └── Group
                    └── windows: DynamicTilingTree
                        └── Window (Wayland surface)
```

### Key Types (conceptual, pre-implementation)

> Note: field/function names use idiomatic Rust `snake_case` (the original schema draft used
> camelCase; converted here to match the project's naming decision).

```rust
struct Compositor {
    monitors: Vec<Monitor>,
}

struct Monitor {
    id: u32,
    visible_columns: usize,
    viewport_offset: usize,
    columns: Vec<Column>,
}

struct Column {
    width: u32,
    active_row: usize,
    groups: Vec<Group>,
}

struct Group {
    layout: TilingNode, // root of dynamic tiling tree
}

// Dynamic tiling tree node — either a split container or a leaf window
enum TilingNode {
    Split {
        direction: SplitDirection,
        ratio: f32,
        left: Box<TilingNode>,
        right: Box<TilingNode>,
    },
    Leaf {
        window_id: u32,
    },
}

enum SplitDirection {
    Horizontal,
    Vertical,
}
```

---

## Zoom Levels

Three zoom levels per monitor, toggled via keybinding:

| Level | Name | Description |
|---|---|---|
| 0 | **Grid** | Default. N columns visible, each showing its active group. Standard tiling within each group. |
| 1 | **Square** | One group fills the entire monitor. Windows within it expand to fill available space. |
| 2 | **Window** | One window fills the entire monitor. |

Zoom transitions should animate (slide/scale) to preserve spatial context.

---

## Navigation Model

### Viewport (X axis)
- `slide_viewport_left` / `slide_viewport_right` — shift `viewport_offset` by 1
- Viewport clamps at edges (no wraparound by default; configurable)

### Column scroll (Y axis)
- `scroll_column_up(col_index)` / `scroll_column_down(col_index)` — change `active_row` for a specific column
- Focus follows the active column implicitly, or user can specify target column

### Window switcher
- Global fuzzy finder overlay listing all windows across all groups/columns
- Selecting a window navigates the grid to its location and animates accordingly (slide X then Y or shortest path)

---

## Dynamic Tiling

Within each group, windows are arranged in a **binary split tree** (similar to i3/bspwm):

- Any window can be split horizontally or vertically
- Removing a window collapses its sibling to fill the space
- No predefined layouts — fully user-driven splits
- Split ratios are adjustable

---

## Rendering & Animation

- **Renderer:** Smithay's built-in GLES2/Vulkan renderer
- **Animations:** slide transitions on viewport/column scroll; scale transitions on zoom level change
- **Animation direction** encodes spatial meaning — sliding left means "moving right in the grid"
- Frame pacing tied to monitor refresh rate via Smithay's `DrmBackend`

---

## Input Handling

Handled via Smithay's `SeatHandler`. All primary actions are keybinding-driven.

Example default bindings (configurable):

| Action | Binding |
|---|---|
| Slide viewport left/right | `Super + H/L` |
| Scroll focused column up/down | `Super + J/K` |
| Scroll specific column | `Super + [1-9] + J/K` |
| Split window horizontal | `Super + S` |
| Split window vertical | `Super + V` |
| Close window | `Super + Q` |
| Zoom in (Grid → Square) | `Super + F` |
| Zoom out (Square → Grid) | `Super + Shift + F` |
| Window switcher overlay | `Super + Space` |
| Move window to group | `Super + Shift + H/J/K/L` |

---

## Configuration

Flat config file (TOML likely). Per-monitor settings:

```toml
[monitor."DP-1"]
visible_columns = 2

[[monitor."DP-1".columns]]
width = 960  # pixels, or eventually percentage

[[monitor."DP-1".columns]]
width = 960

[compositor]
animation_duration_ms = 200
wrap_viewport = false
wrap_columns = false
```

---

## Technology Stack

| Layer | Choice | Rationale |
|---|---|---|
| Language | Rust | Memory safety, modern WM ecosystem |
| Compositor foundation | [Smithay](https://github.com/Smithay/smithay) | Primary Rust Wayland compositor library; used by niri, river |
| Display backend | `DrmBackend` (Smithay) | Direct hardware access, no X dependency |
| Input | `libinput` via Smithay | Standard Wayland input handling |
| IPC | Unix socket + custom protocol (later) | Config reload, external tooling |
| Config format | TOML via `toml` crate | Simple, readable |

---

## Prior Art & Differentiation

| Project | Model | Key difference from Rubix |
|---|---|---|
| PaperWM | Continuous horizontal scroll, GNOME extension | No discrete groups, continuous not grid-based, X11/GNOME-dependent |
| Niri | Infinite horizontal scroll, standalone Wayland | Single axis, no independent column Y scroll, no group concept |
| i3/Sway | Manual binary tree tiling | No spatial grid navigation, no column/group model |
| AwesomeWM | Tag-based, X11 | No spatial model, X11 only |

Rubix is novel in: **independently-scrolling column Y axes + shared viewport X axis + discrete group containers + three zoom levels**.

---

## Implementation Phases (suggested)

### Phase 1 — Rust Fundamentals
- Complete ownership, borrowing, lifetimes, enums, traits, async modules
- Synthesis example program applying all concepts

### Phase 2 — Smithay Basics
- Scaffold a minimal Wayland compositor that opens a window
- Understand surface lifecycle, seat, output

### Phase 3 — Core Data Model
- Implement `Monitor`, `Column`, `Group`, `TilingNode` types
- Basic window placement, no animation

### Phase 4 — Navigation
- Viewport X sliding
- Per-column Y scrolling
- Keybinding infrastructure

### Phase 5 — Dynamic Tiling
- Binary split tree within groups
- Split, remove, resize

### Phase 6 — Zoom Levels
- Grid → Square → Window transitions
- Animations

### Phase 7 — Polish
- Window switcher overlay
- TOML config
- IPC socket
- Animation refinement
