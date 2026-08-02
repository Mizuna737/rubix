# Spec — HDR Phase 2: linear-light compositing core (16F offscreen, gated per-output)

## Goal
Restructure the udev render path so an `hdr = true` output composites its surfaces into a
**16-bit-float (`Abgr16161616f`) linear-light offscreen** — each surface sRGB→linear decoded
*before* blending, blended in linear, then encoded back out — instead of drawing surfaces
straight to the scanout buffer. **Phase 2's output-encode stage is the identity round-trip
(linear→sRGB)**, so the HDR output must look **byte-identical to today**. That is the whole
acceptance test: correctness of the restructure is proven by *no visible change*. Actual HDR
output (linear→PQ/BT.2020 encode, 10-bit scanout coordination) is **Phase 3** — explicitly NOT
this spec. Non-HDR outputs must keep the current path byte-for-byte.

## Critical execution constraints (read first)
- Modifies the **live compositor render path** on the user's **daily-driver machine**.
  **DO NOT run, launch, or restart the compositor.** No `cargo run`, no launching `rubix`. It
  fights the DRM master and breaks the session. Your job ends at a clean `cargo build` +
  `cargo test`. The user does the hardware/session test themselves on a TTY.
- Wrap every build/test in `timeout` (e.g. `timeout 600 cargo build --release`).
- **Rust snake_case** throughout — Rubix uses Rust idiom, not the global camelCase rule.
- **HARD CHECKPOINT — Deliverable 0 first.** Resolve the `MultiRenderer`→`GlesRenderer` access
  question (below) and STOP to report before writing any pipeline code. If the underlying
  `GlesRenderer` cannot be reached mutably to call `compile_custom_texture_shader` /
  `override_default_tex_program`, the entire approach is invalid — report that rather than
  hacking around it.
- **Anti-loop:** if you produce >10000 tokens between tool calls, stop and write a one-line
  "next step" note, then continue. Do NOT re-read the same file repeatedly — read once, act.

## Background (already established — do not re-derive)
Phase 0 (`docs/hdr-phase0-findings.md`, `examples/hdr_offscreen_probe.rs`) proved on this
NVIDIA 610.43.03 / GLES 3.2 stack: `Abgr16161616f` offscreen binds, custom shaders compile and
run, and extended-range values (4.0) survive read-back without clamping. The linear pipeline
therefore lives entirely in Rubix's own code — **no Smithay renderer fork of the compositing
path is needed** (we already ride the `dividebysandwich/smithay` fork pinned at `57c805c8`, but
only for its DRM color-state commit + the shader-override API below).

### Confirmed fork API (verified in the pinned source — use these; do not guess)
All on the GLES renderer/frame (`src/backend/renderer/gles/mod.rs` in the fork):
- `GlesRenderer::compile_custom_texture_shader(&mut self, shader: impl AsRef<str>,
  additional_uniforms: &[UniformName]) -> Result<GlesTexProgram, GlesError>` — the shader body
  receives `uniform sampler2D tex`, `varying vec2 v_coords`, `uniform float alpha`, plus any
  declared custom uniforms; must contain a `//_DEFINES` line (the renderer substitutes
  `#define`s there). This is the decode/encode shader compiler.
- `GlesFrame::override_default_tex_program(&mut self, program: GlesTexProgram,
  additional_uniforms: Vec<Uniform<'static>>)` — sets the program used for **all texture draws
  in that frame that pass `program = None`**, which **includes `WaylandSurfaceRenderElement`**
  (its `draw()` calls `render_texture_from_to(..)` with `None`). This is how per-surface decode
  is injected with no per-element wrapping.
- `GlesFrame::render_texture_from_to(&mut self, texture, src, dest, damage, opaque_regions,
  transform, alpha, program: Option<&GlesTexProgram>, additional_uniforms: &[Uniform])` — the
  explicit-program form, for the encode pass over the 16F texture.
- `GlesRenderer::set_default_tex_program_override(Some((GlesTexProgram, Vec<Uniform>)))` — a
  renderer-global default that applies to frames created *after* the call (incl. frames
  `DrmCompositor` creates internally). Available if per-frame override can't be reached, but
  prefer the per-frame form.
- `GlesRenderer::set_solid_color_transform(Option<Box<dyn Fn(Color32F)->Color32F>>)` — apply
  the same linearization to solid-color elements / clear color so they blend consistently.

## Current render path (mapped — file:line anchors)
- `src/udev.rs:809` `render()` — acquires the renderer:
  ```rust
  let mut renderer = if primary == render_node {
      udev_data.gpus.single_renderer(&render_node)
  } else {
      udev_data.gpus.renderer(&primary, &render_node, format)
  }.expect("failed to acquire renderer");   // type: RubixRenderer<'_> = MultiRenderer<..>
  ```
- `src/udev.rs:901-1019` `render_surface()` — builds layer/space/ghost/cursor elements into
  `Vec<RubixRenderElement<RubixRenderer<'_>>>` (`:986-993`) and hands them to scanout:
  ```rust
  let frame = surface.drm_output
      .render_frame(renderer, &elements, [0.1,0.1,0.1,1.0], FrameFlags::DEFAULT)   // :995
  ```
- `src/udev.rs:106-107` `RubixRenderer<'_>` alias = `MultiRenderer<'_,'_, GbmGlesBackend<..>,
  GbmGlesBackend<..>>`.  **← the crux: shader APIs are on `GlesRenderer`, not `MultiRenderer`.**
- `src/cursor.rs:56-60` `RubixRenderElement` = `render_elements!{ Surface =
  WaylandSurfaceRenderElement<R>, Memory = MemoryRenderBufferRenderElement<R> }`.
- `src/udev.rs:112-117` `SUPPORTED_FORMATS` (scanout formats: `Abgr2101010, Argb2101010,
  Abgr8888, Argb8888`). **Do NOT add 16F here** — 16F is the intermediate, never scanned out.
- Prior art for offscreen render into a bound target: `src/screencopy.rs:238-261, 345-362`
  (`Offscreen::<GlesTexture>::create_buffer(&mut renderer, fourcc, size)` then
  `renderer.bind(&mut texture)` then `renderer.render(&mut target, size, transform)` →
  `GlesFrame`), and `examples/hdr_offscreen_probe.rs:231-306`. The winit path
  (`src/winit.rs:184`) uses `OutputDamageTracker::render_output` into a bound framebuffer.

## Deliverable 0 — CHECKPOINT: resolve MultiRenderer→GlesRenderer access (STOP after)
The shader compile/override calls are `GlesRenderer` methods. udev holds `MultiRenderer`.
Determine the exact, supported way to obtain `&mut GlesRenderer` (or otherwise call
`compile_custom_texture_shader` + `override_default_tex_program`) from within
`render_surface`, given the renderer is a `MultiRenderer<GbmGlesBackend<GlesRenderer,..>, ..>`.
Investigate (read the fork's `src/backend/renderer/multigpu/mod.rs` and `gles.rs` backend):
- Does `MultiRenderer` expose `.as_gles()` / `.as_mut()` / `Deref` to the underlying
  `GlesRenderer`? Does `GbmGlesBackend` provide an accessor?
- Do `compile_custom_texture_shader` / `render()` need the *render*-node renderer specifically
  (the one that will bind the 16F offscreen and draw)? Confirm the texture (client buffers) and
  the offscreen live on the same GL context so the shader can sample them.
- Where does `GlesFrame` come from in a `render_output` flow, and can we set the per-frame
  override on it, or must we use the renderer-global `set_default_tex_program_override`?
**Report:** the exact accessor/call path, whether per-frame or renderer-global override is the
viable one, and any lifetime/borrow constraints. Do NOT proceed to Deliverable 1 until this is
confirmed. If no supported access exists, STOP and report — do not patch the fork.

## Deliverable 1 — shaders + format constant
- New module `src/hdr_shaders.rs` (or extend `src/hdr.rs`) holding two GLSL texture-shader
  source strings + a compile helper:
  - `DECODE_SRGB_TO_LINEAR` — samples `tex`, converts sRGB→linear (proper piecewise sRGB, not
    a bare `pow(x,2.2)` — use the standard 0.04045 threshold / 12.92 / (x+0.055)/1.055 ^2.4),
    preserves alpha, multiplies by `alpha`. Must include the `//_DEFINES` line.
  - `ENCODE_LINEAR_TO_SRGB` — inverse (linear→sRGB, piecewise), the **Phase 2 identity output
    stage**. (Phase 3 will add a `ENCODE_LINEAR_TO_PQ` sibling and switch to it; leave a clear
    seam / comment for that, but do NOT implement PQ here.)
  - A helper that compiles both against the GlesRenderer and returns the `GlesTexProgram`s,
    cached so we compile once (not per frame). Store the compiled programs on `UdevData` (or the
    per-surface/backend struct) — compiling every frame is unacceptable. Decide the cache home
    based on Deliverable 0's renderer-access finding; document it.
- Add a `const HDR_OFFSCREEN_FORMAT: Fourcc = Fourcc::Abgr16161616f;` near `SUPPORTED_FORMATS`
  with a comment that this is the linear intermediate, not a scanout format. (Fall back to
  `Argb16161616f` if bind fails, mirroring the probe's format loop.)

## Deliverable 2 — the gated linear pipeline in `render_surface`
Split `render_surface` so the element-collection stays shared but the *presentation* branches on
the output's hdr flag (read it the same way `set_hdr_output_properties` gating does — via the
`OutputConfig::hdr` for `surface.output`; thread a `bool` in or look it up).

**Non-HDR branch (default):** unchanged — the existing `drm_output.render_frame(renderer,
&elements, clear, flags)` at `:995`. Preserve byte-for-byte. This is the safety net; most
outputs take it.

**HDR branch (`hdr == true`):** — mechanism CONFIRMED by Deliverable 0. The per-frame
`GlesFrame::override_default_tex_program` is **not reachable** (both `render_output` and
`DrmOutput::render_frame` own their `GlesFrame` internally). Use the **renderer-global**
`renderer.as_mut().set_default_tex_program_override(Some((program, uniforms)))` — its doc comment
explicitly names `DrmCompositor`. `renderer.as_mut()` yields `&mut GlesRenderer` via
`MultiRenderer`'s `AsMut` impl. **Always pair every `set_...(Some(..))` with a `set_...(None)`
immediately after the render call** so the override never leaks into the next frame or the
non-HDR outputs (they share the renderer).
1. Get/create an output-sized `Abgr16161616f` offscreen `GlesTexture` (cache per-surface,
   re-create on output resize — never per-frame). Compile the two shaders once and cache them
   (see Deliverable 1); do NOT compile per frame.
2. **Decode pass** into the 16F offscreen:
   `renderer.as_mut().set_default_tex_program_override(Some((decode, vec![])))` and
   `renderer.as_mut().set_solid_color_transform(Some(srgb_to_linear))`; then render the existing
   `elements` into the bound 16F offscreen via `OutputDamageTracker::render_output` (persist a
   tracker per output, as winit does) — its internal `GlesFrame` inherits the global override, so
   surfaces decode + blend in linear. Then immediately clear BOTH:
   `set_default_tex_program_override(None)`, `set_solid_color_transform(None)`.
3. **Encode pass** to scanout: wrap the 16F offscreen as a single `TextureRenderElement`
   (add a `Texture = TextureRenderElement<R>` variant to `RubixRenderElement` in `cursor.rs`).
   Set `set_default_tex_program_override(Some((encode_linear_to_srgb, vec![])))`, call
   `surface.drm_output.render_frame(renderer, &[texture_element], clear, flags)` (the internal
   frame inherits the encode override), then clear the override. `queue_frame` exactly as today.
   - **GOTCHA to verify (flag if it bites):** `DrmCompositor` may promote a single fullscreen
     opaque element to a **direct scanout plane, bypassing GL composition — which would skip the
     encode shader entirely.** Confirm the encode actually runs: check `FrameFlags` for a bit
     that disables direct/overlay-plane promotion for this call (so the element is forced through
     the GL composition path), or make the element ineligible for promotion. If the encode is
     bypassed, the screen will look wrong (double-transfer or raw linear) — that's the signal.
     Prefer the least-invasive `FrameFlags` route; document what you used.

Keep the VBlank/`queue_frame`/reschedule logic (`:1010-1017`) unchanged for both branches.

## Deliverable 3 — winit parity (optional, low priority)
If time permits, mirror the HDR branch in `src/winit.rs` for nested-dev testing gated behind the
same flag. If it complicates the udev work, SKIP it and note so — udev is the only path that
runs on real hardware, and nested winit can't drive real HDR output anyway.

## Scope
- **Edit:** `src/udev.rs`, `src/hdr.rs` (or new `src/hdr_shaders.rs`), possibly `src/cursor.rs`
  (only to add a `RubixRenderElement` variant if the encode step needs it). Optionally
  `src/winit.rs` (Deliverable 3).
- **Do NOT edit:** `src/model/grid.rs` (unrelated pending user work — leave it alone),
  `src/state.rs`, `src/input.rs`, IPC/handlers, keybinds, the model. `SUPPORTED_FORMATS` stays
  as-is (do not add 16F to scanout).
- **Out of scope (Phase 3+):** linear→PQ encode, BT.709→BT.2020 primaries, tone-mapping, 10-bit
  scanout format changes, HDR metadata luminance coordination, per-surface HDR (PQ) *decode*
  (all surfaces are SDR sRGB today), screencopy HDR handling, the SDR brightness slider. Do not
  implement absolute-nits scaling yet — Phase 2 is a pure sRGB↔linear round-trip; the nits-scale
  knob arrives with Phase 3/4 when there's a real output transfer to scale into.

## Behavior to preserve / verify
- **Non-HDR outputs: zero change.** The default branch is the untouched `render_frame(elements)`.
- **HDR output looks identical to today** (the round-trip is visually a no-op) — this is the
  acceptance test the user runs on DP-3 (`hdr = true` already set from Phase 1a testing).
- Shaders compiled **once** and cached, never per-frame.
- Offscreen + damage tracker allocated once per output, re-created only on resize — never
  per-frame.
- No panics on the render path: any shader-compile/bind/offscreen failure must log a
  `tracing::warn!` and **fall back to the non-HDR direct path** for that frame (degrade to SDR
  rather than crash the session). Never `unwrap`/`expect` on the new calls.

## Verification
- `timeout 600 cargo build --release` — clean (0 errors; no NEW warnings beyond the pre-existing
  `CountWindows`/`sum_windows` dead-code ones).
- `timeout 600 cargo test` — green (existing tests unaffected; add a small unit test only if a
  pure-logic helper is introduced — shader GLSL isn't unit-testable in-agent).
- `git status --short` shows only the in-scope files changed (NOT `src/model/grid.rs`).
- snake_case: no new camelCase identifiers.
- **Do NOT run the compositor.**

## Tail
> STOP after Deliverable 0 and report (the checkpoint). Then, after Deliverables 1-2 build clean,
> STOP and report — do not commit. Report: the MultiRenderer→GlesRenderer access path you
> confirmed, the final shape of the HDR branch in `render_surface`, where shaders + offscreen are
> cached, build + test results, and the exact steps the USER runs to test on hardware (DP-3
> already has `hdr = true`; rebuild; restart Rubix on a TTY; confirm the desktop looks normal —
> unchanged — proving the linear round-trip is correct, with the panel still in HDR mode).
