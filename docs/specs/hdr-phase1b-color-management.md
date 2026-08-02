# Spec — HDR Phase 1b: wp_color_management_v1 + per-surface HDR decode

## Goal
Make HDR-capable Wayland clients (mpv `--vo=gpu-next`, Firefox) able to submit **real
HDR content** (PQ / ST 2084, BT.2020) and have it displayed as HDR. Today every surface is
decoded as SDR sRGB in the decode pass (Phase 2/3's single renderer-global override), so a
PQ-encoded video is misread as sRGB and looks wrong. This phase:

1. Wires up the fork's server-side `wp_color_management_v1` so clients can bind the global and
   attach an image description (transfer function + primaries) to their surfaces.
2. Reworks the decode pass so **each surface is decoded by its own transfer function** — SDR
   surfaces via sRGB→linear, HDR surfaces via PQ→linear — compositing into one linear working
   space, then re-encoded to the panel's PQ output.

Milestone: an mpv HDR10 clip on an `hdr = true` output shows correct HDR (bright specular
highlights, saturated wide-gamut color) instead of the washed-out/flat SDR misread.

## Deferred (explicitly OUT of scope this phase — do not build)
- **Compositor-side tone-mapping.** Content is clamped to the PQ 10000-nit range and the panel
  tone-maps down to its own peak. No mapping to an EDID/target peak this phase.
- **EDID peak-nits parse.** `smithay-drm-extras`' `display-info` feature stays disabled (the
  `libdisplay-info-sys` version-pin conflict is a separate follow-up). Keep the static 1000-nit
  mastering metadata from Phase 1a (`src/hdr.rs`) as-is.
- **HLG / scRGB / windows_scrgb transfer functions.** Support **sRGB and ST2084 PQ only** this
  phase. Any other declared transfer function falls back to the SDR decode with a one-time
  `warn!` (safe: worst case it looks like today).
- **Per-subsurface color.** A window's decode kind is taken from its **root/toplevel** surface's
  image description and applied to all its render elements. (Effectively no client sets
  per-subsurface color descriptions.)

## Critical execution constraints (read first)
- Live render path on the user's **daily-driver**. **DO NOT run/launch/restart the compositor**
  (no `cargo run`, no launching `rubix`). Job ends at a clean `cargo build --release` +
  `cargo test`; the user session-tests on a TTY himself.
- Wrap all builds/tests in `timeout` (`timeout 600 cargo build --release`,
  `timeout 600 cargo test`).
- **Rust snake_case** throughout (this repo's exception to the global camelCase rule).
- Do NOT touch `src/model/grid.rs` (unrelated pending user work — it is dirty in the tree; leave
  it). Do NOT `git commit` and do NOT `git add` anything — stop at green build + tests and report.
- Preserve everything Phases 1a/2/3/4 built: the `surface.hdr` gating, the 16F offscreen, the
  graceful SDR fallback on error, the live `sdr_white_nits` slider, the Phase-1a metadata commit.

## Key facts already established (do not re-investigate)
- **Protocol EXISTS in the fork** at `smithay::wayland::color::management` — exports
  `ColorManagementHandler`, `ColorManagementState`, `ImageDescription`, `TransferFunction`
  (variants incl. `Srgb`, `St2084Pq`, `Hlg`, `ExtLinear`, ...), `Primaries`, `RenderIntent`, and
  `get_surface_description(&WlSurface) -> (Option<ImageDescription>, RenderIntent)`. It is
  **metadata-only** — no automatic decode; we write the shaders.
- Dispatch is auto: the fork uses a blanket `delegate_dispatch2!(RubixState)` at
  `src/handlers/mod.rs:80`, so no per-protocol `delegate_*` macro is needed — just impl the
  handler trait, create the state, and `create_global`.
- **`MultiFrame` does NOT expose the underlying `GlesFrame`.** The per-element
  `GlesFrame::override_default_tex_program` is unreachable through rubix's `MultiRenderer`. The
  shader override can only be set **renderer-globally** via `renderer.as_mut()`
  (`&mut GlesRenderer`) BEFORE a render pass. This is why the render design below uses multiple
  passes rather than per-element overrides.
- `MultiRenderer::bind(&mut self, &'a mut Target) -> Framebuffer<'a>` — the framebuffer borrows
  the **texture** (`'a`), not the renderer, so you can bind the offscreen once and then borrow
  the renderer again for multiple `render()` passes into it.
- `MultiRenderer::render(&'frame mut self, framebuffer: &'frame mut Framebuffer, size, transform)
  -> MultiFrame` returns an owned frame guard (not closure-based). Drop it to release the renderer
  borrow before setting the next global override.
- `RenderElement::draw(&self, frame, src: Rect<f64,BufferCoords>, dst: Rect<i32,Physical>,
  damage: &[Rect<i32,Physical>], opaque_regions: &[Rect<i32,Physical>], cache: Option<&UserDataMap>)`.
- Current render path (src/udev.rs): `render_surface` builds `elements:
  Vec<RubixRenderElement<RubixRenderer>>` (front-to-back: cursor, overlay, top, ghosts, space,
  bottom, background) then, if `surface.hdr`, calls `render_surface_hdr(surface, renderer,
  &elements, state.sdr_white_nits)`. `render_surface_hdr` = decode pass (bind offscreen +
  `damage_tracker.render_output` with the global sRGB→linear override + `srgb_to_linear_solid`
  solid transform) then encode pass (offscreen wrapped as a fresh-`Id` `TextureRenderElement`,
  `drm_output.render_frame(.., FrameFlags::empty())` with the global encode override carrying the
  `sdr_white_nits` uniform).

---

## Color science — the new working space
Working space of the 16F offscreen changes from Phase 3's *linear BT.709, SDR-white-relative* to
**linear BT.2020, absolute luminance normalized to 10000 nits** (i.e. `1.0` == 10000 cd/m²,
PQ-linear). This unifies SDR and HDR into one space and simplifies the encode. Consequences:
- The BT.709→BT.2020 matrix and the `sdr_white_nits` scaling **move OUT of the encode shader and
  INTO the SDR decode shader**.
- The encode shader collapses to a bare PQ OETF.
- HDR-PQ content decodes straight to this space (PQ EOTF already yields absolute/10000, BT.2020
  already correct → passthrough).

## Deliverable 1 — shaders (`src/hdr_shaders.rs`)
Keep the GLSL ES 100 / `//_DEFINES_` / `sampler2D tex` / `varying vec2 v_coords` /
`uniform float alpha` skeleton for all shaders. Reuse the existing verified `pq_oetf` constants
and `BT709_TO_BT2020` matrix (currently in the encode shader).

**(a) Rework the decode shader → `DECODE_SDR`** (rename `DECODE_SRGB_TO_LINEAR`). After the
existing piecewise sRGB EOTF, add BT.709→BT.2020 and the nits scaling; declare the uniform:
```glsl
uniform float sdr_white_nits;
// ... existing srgb_to_linear() ...
void main() {
    vec4 c = texture2D(tex, v_coords);
    vec3 lin709 = srgb_to_linear(c.rgb);
    vec3 lin2020 = BT709_TO_BT2020 * lin709;      // reuse the existing verified column-major mat3
    vec3 abs10k  = lin2020 * (sdr_white_nits / 10000.0);
    // preserve existing NO_ALPHA handling for the .a channel, then * alpha as today
    gl_FragColor = vec4(abs10k, /*a per NO_ALPHA*/) * alpha;
}
```
Compile it with the `sdr_white_nits` uniform declared (`UniformName::new("sdr_white_nits",
UniformType::_1f)`).

**(b) New `DECODE_HDR_PQ`** — ST 2084 **inverse** EOTF (PQ decode), BT.2020 passthrough. No
custom uniform (alpha only). Input is PQ-encoded BT.2020; output linear BT.2020 absolute/10000:
```glsl
vec3 pq_eotf(vec3 e) {                 // inverse of pq_oetf; same constants
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    vec3 ep = pow(e, vec3(1.0 / m2));
    vec3 num = max(ep - c1, 0.0);
    vec3 den = c2 - c3 * ep;
    return pow(num / den, vec3(1.0 / m1));   // 0..1 == 0..10000 nits
}
void main() {
    vec4 c = texture2D(tex, v_coords);
    gl_FragColor = vec4(pq_eotf(c.rgb), 1.0) * alpha;   // opaque video; keep * alpha for parity
}
```
(PQ content is BT.2020 in practice — assume BT.2020 primaries; no matrix.)

**(c) Simplify the encode shader → bare PQ.** Remove `BT709_TO_BT2020` usage, the
`sdr_white_nits` uniform, and the scaling from the encode. Keep the `pq_oetf` helper:
```glsl
void main() {
    vec3 lin = clamp(texture2D(tex, v_coords).rgb, 0.0, 1.0);  // already BT.2020 absolute/10000
    gl_FragColor = vec4(pq_oetf(lin), 1.0) * alpha;
}
```
Compile the encode with **no** custom uniforms now.

**(d) `HdrShaders` gains a field:** `{ decode_sdr: GlesTexProgram, decode_hdr_pq: GlesTexProgram,
encode: GlesTexProgram }`. `compile_hdr_shaders` compiles all three.

**(e) Solid-color transform.** The clear color / `SolidColorRenderElement`s are always SDR, but
their transform must now match `DECODE_SDR` (full sRGB→linear→BT.2020→×nits), and it needs the
live nits value. Replace the bare `srgb_to_linear_solid` fn with a **builder that captures nits**:
`pub fn sdr_solid_transform(sdr_white_nits: f32) -> impl Fn(Color32F) -> Color32F + 'static` that
does per-channel sRGB EOTF, the BT.709→BT.2020 matrix (CPU, same constants), then
`* (sdr_white_nits / 10000.0)`, preserving `.a`. Box it into `set_solid_color_transform`. Set it
once for the whole decode (all runs) — solids are never HDR.

**(f)** Keep `SDR_WHITE_NITS = 203.0` and its doc role (serde default / clamp center).

## Deliverable 2 — color-management protocol (`src/color_management.rs`, new)
- Read the fork's `ColorManagementHandler` trait in
  `~/.cargo/git/checkouts/smithay-56902e19d4822414/57c805c/src/wayland/color/management/mod.rs`
  and impl it for `RubixState` with the **minimal** required methods. Advertise support for:
  transfer functions `Srgb` and `St2084Pq`; primaries `Srgb` (BT.709) and `Bt2020`; render intent
  `Perceptual` (plus whatever the trait requires as mandatory). Whatever `Feature`/capability set
  the constructor takes, enable only what's needed to accept parametric image descriptions with
  those TFs + primaries.
- Add `color_management_state: ColorManagementState` to `RubixState` (src/state.rs); construct it
  in `RubixState::new` and `create_global` the manager (mirror how `screencopy::init` /
  `src/main.rs:217` create globals — pick the version arg the fork's `ColorManagementState::new`
  or `::global` expects).
- Provide a small helper in this module:
  `pub fn surface_decode_kind(surface: &WlSurface) -> DecodeKind` where
  `pub enum DecodeKind { Sdr, HdrPq }`. It calls `get_surface_description(surface)`; if the
  description's transfer function is `St2084Pq` → `HdrPq`; else (`Srgb`, none, or any unsupported
  TF) → `Sdr`. Log a one-time `warn!` (use a `std::sync::Once` or a static flag) the first time an
  unsupported-but-non-sRGB TF is seen, so the fallback is visible in the journal.

## Deliverable 3 — per-surface decode in the render path (`src/udev.rs`)
The decode pass must decode each surface with its own transfer function. Because the shader
override is renderer-global-only (see Key facts), this is done with **multiple render passes
grouped into contiguous z-runs**, accumulating into the same offscreen.

**Fast path (no HDR surface present) — unchanged.** Before doing any of the below, check whether
any surface contributing to this output declares `St2084Pq`. If none, run the decode pass
**exactly as today** (single `render_output` with the `DECODE_SDR` override + the SDR solid
transform). This keeps normal desktop rendering byte-for-byte on the existing, proven path — no
regression, no per-element overhead. (Detect via the window/layer surfaces on the output; a cheap
`any(...)` over their root surfaces' `surface_decode_kind`.)

**Slow path (≥1 HDR surface):**
1. Build the element list **paired with a decode kind per element**:
   `Vec<(DecodeKind, RubixRenderElement<RubixRenderer>)>`, in the existing front-to-back order.
   - Cursor, background, and all layer-shell (overlay/top/bottom) elements → `Sdr`.
   - **Window elements** need per-window attribution. First check whether
     `WaylandSurfaceRenderElement` exposes its `WlSurface` (or a stable id you can map back to a
     surface); if it does, tag each element directly. If it does NOT, gather window elements
     **per window** instead of via the single `space.render_elements_for_region` batch: iterate
     the space's windows in the same z-order, and for each window call
     `window.render_elements::<WaylandSurfaceRenderElement<_>>(renderer, loc, scale, alpha)` (the
     same call the **ghost** path already uses at ~udev.rs:1044), tagging that window's batch with
     `surface_decode_kind(window.toplevel_root_surface)`. **You must reproduce the exact geometry,
     scale, alpha, and output-region culling that `render_elements_for_region` performs today** —
     read its implementation first and match it, so tiled window placement is unchanged. (Layout
     geometry is the user's domain; do not alter it — only change how elements are *collected*, and
     only on the slow path.)
2. **Partition into maximal contiguous runs of equal `DecodeKind`**, preserving order. (Reverse
   to back-to-front for painter's-algorithm drawing.)
3. Bind the offscreen once: `let mut fb = renderer.bind(&mut offscreen.texture)?;`. Set the SDR
   solid transform (with live nits) once for the whole pass.
4. For each run, in back-to-front order:
   - Set the renderer-global tex override for the run's kind:
     `renderer.as_mut().set_default_tex_program_override(Some((prog, uniforms)))` where `prog` is
     `shaders.decode_sdr` with `vec![Uniform::new("sdr_white_nits", sdr_white_nits)]` for `Sdr`,
     or `shaders.decode_hdr_pq` with `Vec::new()` for `HdrPq`.
   - `let mut frame = renderer.render(&mut fb, size, Transform::Normal)?;`
   - **First run only:** `frame.clear([0.0,0.0,0.0,1.0], &[full_rect])?;` (black == 0 in every
     space; subsequent runs must NOT clear, so contents accumulate).
   - For each element in the run (back-to-front), compute draw args from the element's own
     geometry and call `element.draw(&mut frame, src, dst, &damage, &opaque_regions, None)?`:
     - `dst = element.geometry(scale)` (Physical),
     - `src = element.src()` (BufferCoords f64),
     - `damage = &[Rectangle::from_loc_and_size((0,0), dst.size)]` (full — the offscreen is redrawn
       fully every frame; matches the fresh-`Id` full-damage behavior Phase 2 relies on),
     - `opaque_regions = element.opaque_regions(scale)`.
   - `drop(frame);` to release the renderer borrow before the next run's override.
   - Clear the tex override after the loop; clear the solid transform after the pass (as today).
5. **Encode pass — unchanged in structure**, except the encode override now carries **no
   uniforms** (`vec![]`) since the encode shader dropped `sdr_white_nits`. Everything else stays:
   offscreen wrapped as a fresh `Id::new()` `TextureRenderElement`, `drm_output.render_frame(..,
   FrameFlags::empty())`, clear override, `queue_frame`.

`render_surface_hdr` keeps its signature (`sdr_white_nits: f32` is still used — now by the SDR
decode override + solid transform instead of the encode). The graceful SDR fallback on any `Err`
is unchanged.

## Deliverable 4 — wiring
- `src/main.rs`: create the color-management global at startup (next to the other `create_global`
  calls).
- `src/state.rs`: field + init (Deliverable 2). If `reload_config` touches nits, no change needed
  here for color management.
- Register the module: `mod color_management;` where the other `mod`s live.

## Tests
- `timeout 600 cargo build --release` — clean, no new warnings (no dead `ENCODE`/`DECODE`
  leftovers).
- `timeout 600 cargo test` — green. Add pure-logic unit tests only where cheap:
  - `surface_decode_kind` mapping is hard to unit-test without a surface — skip unless trivial.
  - If you factor the CPU BT.709→BT.2020 + sRGB EOTF math into a testable fn for the solid
    transform, add a unit test asserting white (1,1,1) sRGB → `sdr_white_nits/10000` in each
    channel (within epsilon) and black → 0.
  - Keep the existing config/bind-count tests passing; the slider binds and count are unchanged
    this phase.
- `git status --short` shows only in-scope files, NOT `src/model/grid.rs`. Do NOT commit/add.
- snake_case; no camelCase.
- **Do NOT run the compositor.**

## Tail — report back
STOP after a clean build + green tests. Do NOT commit. Report:
1. The final GLSL for all three shaders (so the color math can be eyeballed).
2. The `ColorManagementHandler` impl (which methods the trait required, what you advertised).
3. How window-element → decode-kind attribution was done (direct surface access vs. per-window
   gather) and, if per-window, exactly how you matched `render_elements_for_region`'s geometry.
4. The z-run render loop (the accumulate-into-offscreen, clear-first-only sequence).
5. Build + test results.
6. The user's hardware-test steps: DP-3 is already `hdr = true`; rebuild; restart Rubix on a TTY;
   play an HDR10 clip with `mpv --vo=gpu-next <clip>`; expect correct HDR (bright highlights,
   saturated color) instead of washed-out; SDR windows and the brightness knob still behave; check
   the journal for the color-management global bind and any unsupported-TF warn.
