# Spec B — Per-surface dmabuf scanout tranche

## Goal
Tell fullscreen clients *how to allocate a buffer the display controller can actually scan out*.
Rubix currently advertises only **default** dmabuf feedback seeded with the primary GPU's render
formats (udev.rs:471-495). It never sends a per-surface **scanout tranche**, so a client whose window
is the primary-plane candidate keeps allocating render-optimal buffers — on NVIDIA, typically with
modifiers the display engine cannot read. `DrmCompositor` then refuses promotion with
`RenderingReason::FormatUnsupported` and silently composites forever.

This is the missing half of direct scanout: Spec A makes the element list *eligible* for promotion;
this spec makes the client's buffer *capable* of it.

Land this AFTER Spec A (`docs/specs/direct-scanout-fullscreen.md`).

## Critical execution constraints (read first)
- Live daily driver. **DO NOT run/launch/restart the compositor** (no `cargo run`, no launching
  `rubix`). Stop at a clean `timeout 600 cargo build --release` + `timeout 600 cargo test`.
- **Rust snake_case** throughout.
- **Do NOT touch `src/model/grid.rs`** or anything under `src/model/`.
- Do NOT `git commit` / `git add`. Do NOT edit anything outside `~/Projects/rubix`.

## Reference implementation
The pinned fork ships the canonical version of this in **anvil**. Read it before writing anything:
- `/home/max/.cargo/git/checkouts/smithay-56902e19d4822414/57c805c/anvil/src/udev.rs:697-758`
  — `get_surface_dmabuf_feedback`, which builds the two feedback objects.
- Same file, lines 1030-1060 — the bringup call site
  (`drm_output.with_compositor(|compositor| get_surface_dmabuf_feedback(.., compositor.surface()))`).
- `anvil/src/state.rs:906-962` — `post_repaint`, which per-frame calls `window.send_frame(..)` and
  `window.send_dmabuf_feedback(output, surface_primary_scanout_output, |surface, _| select_dmabuf_feedback(surface, render_element_states, &render_feedback, &scanout_feedback))`.
- `anvil/src/state.rs:1079-1110` — `update_primary_scanout_output`, which must run on the frame's
  `RenderElementStates` *before* `post_repaint` so `surface_primary_scanout_output` is populated.

Mirror anvil's structure; do not invent a different one.

## Established facts (verified — do not re-investigate)
- `DmabufFeedbackBuilder::new(main_device, formats)` and
  `.add_preference_tranche(target_device, Option<TrancheFlags>, formats)` (fork
  `src/wayland/dmabuf/mod.rs:313` and `:339`). The scanout tranche uses
  `zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout`.
- The scanout tranche's format set must be the plane formats **intersected with** formats we can also
  render from, so a render fallback always exists if a given buffer turns out not to be scannable
  (anvil comments this explicitly at udev.rs:718-721).
- Helpers live in `smithay::desktop::utils` / are re-exported from `smithay::desktop`:
  `surface_primary_scanout_output`, `update_surface_primary_scanout_output`,
  `default_primary_scanout_output_compare`, `send_dmabuf_feedback_surface_tree`
  (`src/desktop/wayland/utils.rs:169,185,275`). `Window::send_dmabuf_feedback`
  (`src/desktop/wayland/window.rs:346`) and `LayerSurface::send_dmabuf_feedback`
  (`src/desktop/wayland/layer.rs:668`) wrap the surface-tree version.
- `select_dmabuf_feedback` is an anvil-local helper — check whether the fork exports an equivalent
  (`smithay::backend::renderer::element::default_primary_scanout_output_compare` neighbourhood /
  `smithay::wayland::dmabuf`); if it does not, port anvil's small version into rubix rather than
  depending on anvil.
- Rubix side: `SurfaceData` (udev.rs:186+) is where per-output state lives; `render_surface`
  (udev.rs:1078) produces the `RenderFrameResult` whose `.states` field is the
  `RenderElementStates` these helpers need. The existing frame-callback loop is around
  udev.rs:1040-1073 (`layer.send_frame(..)` etc.).

## Deliverable 1 — build the two feedback objects at output bringup
Add a `SurfaceDmabufFeedback { render_feedback: DmabufFeedback, scanout_feedback: DmabufFeedback }`
struct and a `get_surface_dmabuf_feedback(..)` function mirroring anvil's, adapted to rubix's types
(rubix has a single primary GPU; `render_node == primary_gpu` in practice, so the multi-GPU
`render_node: Option<DrmNode>` branching can be simplified — but keep the *format* logic identical,
including the `intersection(&all_render_formats)` step).

Store the result as `dmabuf_feedback: Option<SurfaceDmabufFeedback>` on `SurfaceData`, built at
bringup inside the existing `drm_output.with_compositor(|c| ..)` region so `c.surface()` is reachable
(that is the same accessor anvil uses).

Leave the existing default-feedback global (udev.rs:475-495) exactly as it is — XWayland/DRI3
discovery depends on it.

## Deliverable 2 — update primary scanout output + send feedback per frame
In the render path, after `render_frame` returns and **using that frame's**
`RenderElementStates`:
1. Call the rubix equivalent of anvil's `update_primary_scanout_output` — iterate
   `state.space.elements()` and the output's `layer_map_for_output(..).layers()`, calling
   `update_surface_primary_scanout_output(surface, output, states, None, render_element_states,
   default_primary_scanout_output_compare)` via each element's `with_surfaces(..)`.
2. Then, for each window/layer surface on this output, call `send_dmabuf_feedback(output,
   surface_primary_scanout_output, |surface, _| select_dmabuf_feedback(surface,
   render_element_states, &fb.render_feedback, &fb.scanout_feedback))`, guarded on
   `surface.dmabuf_feedback.is_some()`.

Ordering matters: step 1 must precede step 2, and both must see the *current* frame's states.

Placement: this needs `&RenderElementStates` from `render_frame` and `&mut RubixState`. Fit it into
the existing structure — the natural home is alongside the existing per-frame `send_frame` loop
(udev.rs:1040-1073) if the states can be threaded there, otherwise at the end of `render_surface`
before returning. **Do not** restructure the render path or the existing borrow discipline to make
this fit; if the borrows genuinely conflict, return the needed data out of `render_surface` and do
the work in the caller, and explain the choice in your report.

Note that Spec A's HDR-composite path (`render_surface_hdr` / `render_surface_hdr_zrun`) also
produces a frame result. Feedback only matters for the plain path (the HDR composite path can never
scan out), so it is acceptable — and simpler — to send feedback only from the plain `render_frame`
path. Say which you did.

## Deliverable 3 — diagnostic
Extend the Spec A scanout diagnostic: when logging `promoted = false` for a fullscreen output, also
log once whether `surface.dmabuf_feedback.is_some()` and the number of formats in the scanout
tranche, so a missing/empty tranche is distinguishable from a client that ignored it.

## Verification
- `timeout 600 cargo build --release` — clean, no new warnings from these files.
- `timeout 600 cargo test` — green.
- `git status --short` — only `src/udev.rs` (and possibly `src/state.rs`) plus spec files.
  **`src/model/grid.rs` must still show as the user's own untouched modification.** No `git add`, no
  `git commit`.
- **Do NOT run the compositor.**

## Tail — report back
STOP after a clean build + green tests. Report:
1. `get_surface_dmabuf_feedback` as written, and how rubix's single-GPU case simplified anvil's.
2. Where the per-frame update + send landed, and the borrow approach chosen.
3. Whether `select_dmabuf_feedback` was found in the fork or ported from anvil (and its body if
   ported).
4. Whether feedback is sent from the HDR composite path too, or only the plain path.
5. Build + test output, and `git status --short`.
6. Anything contradicting the "Established facts" section — say so explicitly.
