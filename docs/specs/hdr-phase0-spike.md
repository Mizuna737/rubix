# Spec — HDR Phase 0: half-float offscreen + custom shader feasibility spike

## Goal
Prove, on this machine's actual stack (open NVIDIA module 610.x, GLES via Smithay 0.7
`GlesRenderer`), that we can (a) bind a **half-float** offscreen render target, (b) run a
**custom fragment-shader** pass into/over it, and (c) get **genuinely extended-range float
precision** back out (not a silent 8-bit downgrade). This is the single gate that decides
whether Rubix's HDR color pipeline can live in our own code (offscreen linear compositing)
or whether we are forced to fork Smithay's renderer. Output is a GO/NO-GO verdict plus a
findings doc — **not** production pipeline code.

## Background (read for context, do not modify)
Rubix currently composites **directly to scanout** with stock Smithay GLES shaders and has
**no offscreen stage on the main path** and **no custom shaders**. Relevant existing code
(for reference only — DO NOT wire the spike into these):
- `src/udev.rs:106-107` — `RubixRenderer` = `MultiRenderer<GbmGlesBackend<GlesRenderer,…>>`.
- `src/udev.rs:201` — `GpuManager::new(GbmGlesBackend::with_context_priority(High))`.
- `src/udev.rs:379-386` — how the project obtains an EGL context / `dmabuf_render_formats()`
  from the primary GPU. Use this as a **pattern reference** for building the probe's context.
- `src/udev.rs:863,949` — the live `render_surface()` → `DrmOutput::render_frame()` path.
  **The spike must not touch this.**
- `src/screencopy.rs:324-361` — existing offscreen pattern: `create_buffer(Fourcc::Xrgb8888,…)`
  → `bind()` → `OutputDamageTracker::render_output()` → `copy_framebuffer()`. This is the
  closest existing example of binding an offscreen target; mirror it, but with a **half-float
  format** instead of `Xrgb8888`, and add a read-back precision check.

We are on **stock smithay 0.7.0** (crates.io) — the spike needs **no fork and no Cargo dep
changes** beyond possibly registering a new `[[example]]` target. Confirm the exact Smithay
0.7 API names yourself (signatures vary by version); discovering whether these APIs actually
work is the point of the spike.

## Scope
- **Files to CREATE:**
  - `examples/hdr_offscreen_probe.rs` — standalone probe binary.
  - `docs/hdr-phase0-findings.md` — findings report (also printed to stdout).
- **Cargo.toml:** add an `[[example]]` entry for `hdr_offscreen_probe` **only if** the example
  needs it to build (e.g. to set `required-features`); otherwise leave Cargo.toml untouched.
  Do **not** add, remove, or bump any dependency.
- **Files NOT to edit:** `src/udev.rs`, `src/state.rs`, `src/model/grid.rs`, `src/hdr.rs`,
  `src/screencopy.rs`, `src/portal/*`, any keybind/config code. This is additive only.
- **Out of scope:** the actual linear-compositing pipeline, PQ encode, tone mapping, the
  `dividebysandwich` fork pin, `wp_color_management_v1`, EDID parsing. Those are Phases 1–3.

## Deliverables

### 1. `examples/hdr_offscreen_probe.rs`
A headless probe that builds a minimal EGL/GLES context on the render node
(`/dev/dri/renderD128`, falling back to any available render node) via `gbm` +
`smithay::backend::egl` + `unsafe GlesRenderer::new(egl_context)` — mirroring the context
construction pattern at `udev.rs:379-386`. It must, in order, and log each result clearly:

1. **Context/extension probe.** Log: GLES major/minor version, and presence/absence of each of:
   `GL_EXT_color_buffer_half_float`, `GL_OES_texture_half_float`,
   `GL_OES_texture_half_float_linear`, `GL_EXT_color_buffer_float` (if queryable). These
   determine whether a half-float renderable FBO is even legal on this driver.

2. **Half-float offscreen bind.** Attempt to create + bind an offscreen render target in a
   half-float format — try `Fourcc::Abgr16161616f` first, then `Fourcc::Argb16161616f` as a
   fallback — using Smithay's `Offscreen`/`create_buffer` + `bind` machinery (the
   `screencopy.rs` pattern, half-float format substituted). A modest size is fine (e.g.
   256×256). Log which format bound, or the exact error if none did.

3. **Custom shader pass.** Compile a trivial custom fragment shader via Smithay 0.7's custom
   texture/pixel shader API (e.g. `GlesRenderer::compile_custom_texture_shader` or the
   pixel-shader equivalent — find the correct 0.7 entry point) and execute one pass that
   writes known values into the half-float target. The shader only needs to output a constant
   or a simple function of coordinates — the goal is to prove the **custom-shader hook
   compiles and runs against an offscreen half-float target**, which is exactly what the real
   EOTF/PQ passes will need. Log compile success/failure (with the shader info-log on failure)
   and whether the pass executed.

4. **Extended-range precision read-back (the decisive test).** Write the value **4.0** (well
   above the 1.0 UNORM ceiling) into the target — via the shader pass or a clear — then read
   the pixels back (raw `glReadPixels` with `GL_FLOAT`/`GL_HALF_FLOAT` via the context's gl
   bindings, or whatever read-back the renderer exposes). 
   - If read-back ≈ **4.0** → target is genuinely extended-range float. **PASS.**
   - If read-back clamps to **1.0** → it silently downgraded to 8-bit UNORM. **FAIL.**
   As a secondary check, also write two values differing by less than `1/255` (e.g. `0.5000`
   and `0.5020`) into two regions and confirm they read back **distinct** (sub-8-bit-LSB
   precision survives). Log both raw read-back numbers.

5. **Verdict.** Print a final line exactly of the form `HDR_PHASE0_VERDICT: GO` or
   `HDR_PHASE0_VERDICT: NO-GO`, where **GO** requires: a half-float format bound (step 2) AND
   the custom shader compiled+ran (step 3) AND the 4.0 read-back was preserved, not clamped
   (step 4). Anything else is NO-GO, with a one-line reason.

The probe must be **self-contained and non-interactive**, run to completion, and **exit on
its own** (no mainloop, no lingering GPU process). Wrap any GPU/context teardown so the
process always exits cleanly.

### 2. `docs/hdr-phase0-findings.md`
Written by the probe (or by you after running it) capturing: the extension table from step 1,
which half-float format bound, the shader compile result, both precision read-back numbers,
and the final verdict — plus, **if NO-GO**, a short note on *why* (missing extension? clamp?
API gap?) so we know whether the renderer-fork path is required.

## Constraints
- **Conventions:** Rust **snake_case** for all identifiers (this is Rubix — the global
  camelCase rule does NOT apply; Rubix mirrors Rust idiom). Match surrounding module style.
- **Dependencies:** stock `smithay 0.7.0` and crates already in `Cargo.toml` only (`gbm`,
  `smithay`, etc. are already present). Add **no** new dependencies. No fork, no
  `[patch.crates-io]`.
- **Behavior to preserve:** the live compositor is untouched — this is a new example target
  only. `cargo build` of the main binary must be unaffected.
- **Safety:** any process that could hang a GPU/context must exit on its own; this is a
  headless one-shot, no mainloop.

## Risk Assessment
- **API discovery risk:** exact Smithay 0.7 names for the custom-shader entry point and
  offscreen half-float creation may differ from the guesses above. If an API genuinely does
  not exist in 0.7, **that itself is a valid finding** — record it in the findings doc as a
  NO-GO reason rather than forcing a workaround. Do not pull in new crates to route around it.
- **Read-back mechanism:** Smithay may not expose `glReadPixels` directly; you may need
  `renderer`'s raw gl/ffi access under `unsafe`, or an EGL `make_current` + raw `gl` call. Use
  whatever the context exposes; the read-back is essential to the verdict.
- **No-render-node / permissions:** if `/dev/dri/renderD128` is unavailable, try other render
  nodes and log clearly; do not fall back to anything that touches the live DRM master.
- **Edge case:** a driver may *advertise* a half-float extension but still clamp on read-back —
  that is exactly why step 4 (the 4.0 test) is the real gate, not the extension list.

## Anti-loop guard
> If you produce more than ~10000 tokens between tool calls, stop and write a one-line "next
> step" note before continuing. This is a tripwire, not a hard limit — reasoning is welcome,
> but checkpoint when stuck.

## Verification
- `cargo build --example hdr_offscreen_probe` compiles clean.
- `cargo build` (main binary) still compiles clean — the example did not perturb it.
- Run the probe (headless, one-shot): `cargo run --example hdr_offscreen_probe` — it must exit
  on its own and print exactly one `HDR_PHASE0_VERDICT:` line.
- `docs/hdr-phase0-findings.md` exists and contains the extension table, both read-back
  numbers, and the verdict.
- snake_case check on the new file:
  `grep -nE '\b[a-z]+[A-Z]' examples/hdr_offscreen_probe.rs` should return nothing
  (no camelCase identifiers).

## Tail
> Stop after verification. Do not commit. Report what you changed, the probe's full stdout
> (especially the extension table, the two read-back numbers, and the `HDR_PHASE0_VERDICT:`
> line), and — if NO-GO — your read on whether a Smithay renderer fork is the required path.
