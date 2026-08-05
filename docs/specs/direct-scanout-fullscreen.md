# Spec A — Direct scanout for exclusive fullscreen windows, HDR-aware

## Goal
Make a fullscreen window (games, native Wayland or Xwayland) eligible for DRM **primary-plane direct
scanout** so its client buffer is presented zero-copy with no composite pass — **including HDR
games**, which are the best case: a PQ/BT.2020 client buffer scanned out to a PQ/BT.2020 connector is
an identity pass, strictly better than today's PQ -> linear -> 16F -> PQ round trip.

The governing rule introduced here: **while an exclusive fullscreen window covers an output, the
connector's color state follows that window's own declared transfer function.** HDR is not a mode
that blocks scanout; it is a property the scanned-out content either has or doesn't.

Today promotion never happens. Four independent blockers each individually prevent it; all four are
fixed here. (A fifth — per-surface dmabuf scanout tranches — is Spec B, landing separately.)

## Critical execution constraints (read first)
- Live daily driver. **DO NOT run/launch/restart the compositor** (no `cargo run`, no launching
  `rubix`). Stop at a clean `timeout 600 cargo build --release` + `timeout 600 cargo test`.
- **Rust snake_case** throughout (this repo is the exception to the global camelCase rule).
- **Do NOT touch `src/model/grid.rs`** — unrelated dirty user work. Do NOT touch anything under
  `src/model/`. All changes belong in `src/state.rs` and `src/udev.rs`.
- Do NOT `git commit` / `git add`. Do NOT edit anything outside `~/Projects/rubix`.
- Where this spec quotes line numbers, treat them as starting points — verify against the file.

## Established facts (verified in the pinned fork — do not re-investigate)
Fork checkout: `/home/max/.cargo/git/checkouts/smithay-56902e19d4822414/57c805c`.

- `DrmCompositor::render_frame` (compositor/mod.rs:1997) only attempts primary-plane scanout when
  `remaining_elements == 1 && primary_plane_elements.is_empty()` — the candidate must be the **last**
  element in the list AND **nothing above it** may have been assigned to the primary plane for
  rendering. It additionally requires
  `crtc_background_matches_clear_color || (element_spans_complete_output && element_is_opaque)`.
  Rubix's clear color is `[0.1, 0.1, 0.1, 1.0]` (not black, alpha 1), so that first branch is always
  false — promotion depends entirely on the element spanning the whole output and being opaque.
- Elements *below* a fully-opaque output-spanning element are auto-culled (compositor/mod.rs:1918
  short-circuit), so background/bottom layer surfaces and other tiled windows below the game are
  harmless. Elements *above* it are not culled and are fatal.
- `render_frame` returns `RenderFrameResult` (compositor/frame_result.rs:40) with
  `pub is_empty: bool`, `pub states: RenderElementStates`, and
  `pub primary_element: PrimaryPlaneElement<'a, B, F, E>` whose variants are `Swapchain(..)`
  (composited) and `Element(&E)` (direct scanout).
  `RenderElementPresentationState` is `Rendering { reason: Option<RenderingReason> } | ZeroCopy |
  Skipped`; `RenderingReason` is `FormatUnsupported | ScanoutFailed` (both in
  `smithay::backend::renderer::element`).
- `FrameFlags::DEFAULT` already includes `ALLOW_PRIMARY_PLANE_SCANOUT | ALLOW_OVERLAY_PLANE_SCANOUT |
  ALLOW_CURSOR_PLANE_SCANOUT`. **Keep `FrameFlags::DEFAULT`** — do NOT add
  `ALLOW_PRIMARY_PLANE_SCANOUT_ANY`; it permits a primary-plane format change and risks swapchain
  churn/flicker. We diagnose first.

Rubix-side:
- `RubixState.fullscreen_windows: HashSet<u32>` (state.rs:152); populated from
  `handlers/xdg_shell.rs:194` and `handlers/xwayland.rs:228`.
- `RubixState::apply_layout` builds `targets: Vec<(u32, Rect)>` from `monitor.compute_layout(..)`
  (state.rs:820-825), then at state.rs:827-853 tries to add fullscreen windows at full output bounds
  but guards with `if !targets.iter().any(|(tid, _)| *tid == id)`. Because a fullscreen window stays
  in the grid model, `compute_layout` already returned it with a *tiled* rect, so the full-output
  rect is always skipped.
- All `space.map_element(..)` calls in state.rs pass `activate = false`, so nothing is ever raised.
- `render_surface` (udev.rs:1078) builds `background`/`bottom`/`top`/`overlay` from the output layer
  map (udev.rs:1096-1112), then `space_elements`, then `ghosts`, then `cursor_elements`, and
  assembles `elements` top-to-bottom at udev.rs:1186-1193.
- `has_fullscreen_on_output` (udev.rs:1159-1169) gates the existing cursor suppression. It is weak —
  it only tests whether the window's *origin point* falls inside the output geometry.
- `surface.hdr` gates the HDR composite path at udev.rs:1213. `SurfaceData.hdr_capable: bool` marks
  outputs configured `hdr = true`.
- `set_hdr_output_properties(&RubixDrmOutput)` (udev.rs:815) and
  `set_sdr_output_properties(&RubixDrmOutput)` (udev.rs:893) stage connector color state for the next
  atomic commit. Both are cheap and idempotent (`set_sdr_output_properties` already no-ops when
  `pending_color_state()` matches; give `set_hdr_output_properties` the same guard if it lacks one).
- **Already confirmed for you — do not spend tool calls re-checking these:**
  - `Space::raise_element(&mut self, element: &E, activate: bool)` exists with exactly that
    signature (fork `src/desktop/space/mod.rs:173`); it removes the element and re-inserts it at
    `self.elements.len()`, i.e. topmost.
  - `PrimaryPlaneElement` is defined in `compositor/frame_result.rs` and re-exported by
    `pub use frame_result::*;` (compositor/mod.rs:190), so the import path is
    `smithay::backend::drm::compositor::PrimaryPlaneElement`. Variants: `Swapchain(..)` and
    `Element(..)` (constructed at compositor/mod.rs:2370 and :2377).
- `crate::color_management::surface_decode_kind(&WlSurface) -> DecodeKind` (color_management.rs:52),
  `DecodeKind::{Sdr, HdrPq}`. `output_has_hdr_window` (udev.rs:1266) shows the
  space-iteration + bbox-overlap idiom to copy.
- `udev::toggle_hdr` currently flips `surface.hdr` **and** calls `set_hdr_output_properties` /
  `set_sdr_output_properties` directly.

## Deliverable 1 — actually apply the fullscreen rect (src/state.rs)
In `apply_layout`, the fullscreen block (state.rs:827-853) must **override** the tiled target rather
than skip it. Replace the `if !targets.iter().any(..)` guard: if an entry for `id` already exists in
`targets`, overwrite its `Rect` with the full output bounds; otherwise push a new entry. Leave the
output lookup and `Rect` construction from `space.output_geometry` as they are.

Keep the existing `as u32` casts on `bounds.loc` — that is pre-existing behaviour and outputs are
laid out at non-negative coordinates in this config. Do not restructure `Rect`.

## Deliverable 2 — raise the fullscreen window (src/state.rs)
A fullscreen window must be topmost in the `Space` stack or a tiled window can render above it and
kill promotion. **After** all `map_element` calls in `apply_layout` have completed (both the SNAP
path around state.rs:874-926 and the animated path), raise each currently-mapped id in
`self.fullscreen_windows`:

```rust
for id in self.fullscreen_windows.iter().copied().collect::<Vec<_>>() {
    if let Some(window) = self.windows.get(&id).cloned() {
        self.space.raise_element(&window, false);
    }
}
```
`activate = false` — do not change keyboard focus here. Verify `Space::raise_element`'s signature in
the pinned fork (`smithay::desktop::space`) and adapt if it differs; the requirement is simply that
the fullscreen window ends up topmost in `space.elements()`.

## Deliverable 3 — exclusive-fullscreen detection + suppress chrome above it (src/udev.rs)
Replace `has_fullscreen_on_output` with:

```rust
/// The fullscreen window that exclusively covers `output`, plus the decode kind
/// it declares, if any. Stricter than the old origin-point test: the window's
/// bbox must actually contain the whole output geometry, which is the same
/// condition `DrmCompositor` requires for primary-plane promotion.
fn fullscreen_scanout_target(
    state: &RubixState,
    output: &Output,
) -> Option<DecodeKind>
```
Implementation: get `state.space.output_geometry(output)`; for each id in `state.fullscreen_windows`,
look up the window in `state.windows`, get `state.space.element_bbox(&window)`, and require
`bbox.contains_rect(output_geo)`. Return `Some(surface_decode_kind(&window.wl_surface()?))` for the
first match (defaulting to `DecodeKind::Sdr` if the window has no `wl_surface`). Cheap — no renderer
work. Call it **once** near the top of `render_surface` and reuse the result.

When it is `Some(..)` for this output:
- Clear the `top` and `overlay` layer vectors before element assembly (waybar and any layer-shell
  overlay). This is both required for promotion and correct behaviour for exclusive fullscreen.
  Leave `bottom`/`background` built — they are culled by the opaque short-circuit and are the
  fallback if promotion fails. Build the lists as today and clear afterwards so the existing borrow
  discipline is untouched.
- Clear the `ghosts` vector (animation ghosts sit above space elements).
- Keep the existing cursor suppression, now keyed off this helper instead of the old one.

When it is `None`, every frame is byte-for-byte unchanged from today.

## Deliverable 4 — connector color state follows the scanned-out content (src/udev.rs)
This is the core of the HDR story. Two coupled changes.

### 4a. Make `render_surface` the single owner of connector color state
Add to `SurfaceData` a field recording the last *applied* connector mode, e.g.:
```rust
/// Connector color state currently staged on this output. `render_surface`
/// is the sole owner: it computes the desired mode each frame and only
/// touches DRM on a transition, so nothing else may call
/// `set_hdr_output_properties` / `set_sdr_output_properties` directly.
applied_connector_hdr: Option<bool>,
```
(initialised `None` at bringup so the first frame always applies).

In `render_surface`, compute:
```rust
let desired_connector_hdr = match fullscreen_kind {
    // Exclusive fullscreen: the connector follows the content.
    Some(DecodeKind::HdrPq) => surface.hdr_capable && surface.hdr,
    Some(DecodeKind::Sdr) => false,
    // Desktop: today's behaviour.
    None => surface.hdr,
};
```
Then, only when `surface.applied_connector_hdr != Some(desired_connector_hdr)`, call
`set_hdr_output_properties(&surface.drm_output)` or `set_sdr_output_properties(&surface.drm_output)`
accordingly, update the field, and emit one `tracing::info!` line naming the output, the new mode,
and the reason (`fullscreen-hdr` / `fullscreen-sdr` / `desktop`).

Then **remove the direct `set_hdr_output_properties` / `set_sdr_output_properties` calls from
`udev::toggle_hdr`** — it now only flips `surface.hdr` (on `hdr_capable` surfaces) and schedules the
render, and `render_surface` picks the change up on the next frame. This removes a latent conflict
between the toggle and the fullscreen logic. Keep everything else about `toggle_hdr` (including the
`Vec<Output>` return used for `output_description_changed`) intact.

### 4b. Bypass the HDR composite path under exclusive fullscreen
The HDR path (udev.rs:1213) renders into a 16F offscreen and hands `render_frame` a **single texture
element**, which makes direct scanout structurally impossible. So:

```rust
if surface.hdr && fullscreen_kind.is_none() {
    // ... existing HDR composite branch, unchanged ...
}
```
i.e. when an exclusive fullscreen window covers the output, fall through to the plain
`render_frame(renderer, &elements, [0.1, 0.1, 0.1, 1.0], FrameFlags::DEFAULT)` call.

Correctness of this bypass follows from 4a:
- HDR game (`Some(HdrPq)`) on an HDR output: connector stays PQ/BT.2020, the client buffer is already
  PQ/BT.2020, and no shader touches it. Identity — correct, and better than the current round trip.
- SDR game (`Some(Sdr)`) on an HDR output: connector drops to SDR default (sRGB/BT.709), so the
  8-bit client buffer is interpreted correctly. HDR returns automatically on leaving fullscreen.
- Non-HDR output: unchanged.

Note in a comment that HLG content currently maps to `DecodeKind::Sdr` (`surface_decode_kind` only
recognises `St2084Pq`), so an HLG fullscreen client will drive the connector to SDR — acceptable for
now, and it degrades to today's behaviour rather than misrendering.

Do NOT attempt to forward the client's own mastering metadata (max_cll / max_fall /
mastering_luminance from its `ImageDescription`) onto the connector in this spec — that is a
deliberate follow-up. Leave `default_hdr_color_state()` as the HDR blob.

## Deliverable 5 — scanout diagnostic (src/udev.rs)
After the plain `render_frame` call succeeds, log whether promotion actually happened — **only on
change**, so it is silent in steady state and never spams. Add to `SurfaceData`:
`last_scanout_promoted: Option<bool>` (init `None`).

```rust
let promoted = matches!(frame.primary_element, PrimaryPlaneElement::Element(_));
if surface.last_scanout_promoted != Some(promoted) {
    surface.last_scanout_promoted = Some(promoted);
    tracing::info!(
        "direct-scanout on {}: promoted = {promoted} ({} elements, fullscreen = {:?})",
        surface.output.name(),
        elements.len(),
        fullscreen_kind,
    );
    if !promoted && fullscreen_kind.is_some() {
        for (id, st) in frame.states.states.iter().take(10) {
            tracing::info!("  element {id:?}: {:?}", st.presentation_state);
        }
    }
}
```
That per-element dump is what tells us the refusal reason (`FormatUnsupported` / `ScanoutFailed`, or
a stray `Rendering` element proving something is still above the game). Confirm the exact re-export
path for `PrimaryPlaneElement` in the fork before importing it (expected:
`smithay::backend::drm::compositor::PrimaryPlaneElement`).

Borrow discipline: `frame` borrows from the `render_frame` call and `surface` is `&mut` — extract
`promoted` and the log strings into locals first if the borrow checker objects. Do not restructure
the existing error mapping or the `frame.is_empty` early return (note: the diagnostic should run
**before** the `is_empty` early return, or promotion changes on empty frames will be missed).

## Verification
- `timeout 600 cargo build --release` — clean, no new warnings from these files.
- `timeout 600 cargo test` — green (currently 121 passed / 1 ignored). If `apply_layout` is reachable
  from the existing `state_tests.rs` harness, add a test that a fullscreen window's target rect
  equals the full output bounds even when it is also present in the tiled layout (Deliverable 1). If
  it is not cheaply testable, skip it and **say so** rather than inventing scaffolding.
- `git status --short` — only `src/state.rs`, `src/udev.rs`, and the spec files.
  **`src/model/grid.rs` must still show as the user's own untouched modification.** No `git add`, no
  `git commit`.
- **Do NOT run the compositor.**

## Tail — report back
STOP after a clean build + green tests. Report:
1. The final fullscreen-target override, and exactly where the `raise_element` loop landed.
2. The `fullscreen_scanout_target` body, and where `top`/`overlay`/`ghosts` are cleared.
3. The `desired_connector_hdr` computation, the transition-guard code, and confirmation that
   `toggle_hdr` no longer touches connector properties directly.
4. The HDR-bypass condition as written.
5. The diagnostic code and the exact `PrimaryPlaneElement` import path used.
6. Build + test output, and `git status --short`.
7. Anything you found that contradicts the "Established facts" section — say so explicitly rather
   than silently working around it.
