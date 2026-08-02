# Spec — HDR Phase 3: PQ/BT.2020 output encode + 10-bit scanout

## Goal
Make an `hdr = true` output display **correct HDR** by replacing Phase 2's identity output
encode (`ENCODE_LINEAR_TO_SRGB`) with a real **BT.709→BT.2020 + ST 2084 (PQ)** encode at an
SDR-white luminance, written to a **10-bit** scanout the HDR-signaled panel (Phase 1a set
ST2084 EOTF + BT.2020 colorspace) decodes correctly. Milestone: DP-3 shows a correct-looking
desktop in HDR mode — no longer dark/oversaturated. All content is still SDR (no HDR clients
until Phase 1b), so there is nothing above panel peak: **no tone-mapping is required** this
phase — a straight SDR-white-scaled PQ encode is the whole job.

## Critical execution constraints (read first)
- Live render path on the user's **daily-driver**. **DO NOT run/launch/restart the compositor**
  (no `cargo run`, no launching `rubix`) — it fights the DRM master. Job ends at a clean
  `cargo build` + `cargo test`; the user session-tests on a TTY.
- Wrap builds/tests in `timeout` (`timeout 600 cargo build --release`).
- **Rust snake_case** throughout.
- Do NOT touch `src/model/grid.rs` (unrelated pending user work). Do NOT commit.
- Preserve everything Phase 2 built: the hdr gating, decode pass, per-frame fresh-`Id` encode
  element, `FrameFlags::empty()` composition, graceful SDR fallback on error. Only the **encode
  transfer function** and the **nits uniform + scanout-format verification** change.

## Current state (built in Phase 2, commit 7512ae2)
- `src/hdr_shaders.rs`: `DECODE_SRGB_TO_LINEAR` (keep as-is), `ENCODE_LINEAR_TO_SRGB` (the
  identity round-trip — **superseded by this phase**), `HdrShaders { decode, encode }`,
  `compile_hdr_shaders`, `srgb_to_linear_solid`.
- `src/udev.rs` `render_surface_hdr`: decode pass renders `elements` into an `Abgr16161616f`
  offscreen (linear, BT.709 primaries, values ~[0,1] scene-referred relative); encode pass draws
  that offscreen through `shaders.encode` via `render_frame(.., FrameFlags::empty())`. The
  encode override is set with `Vec::new()` additional uniforms today.
- `src/udev.rs:112-117` `SUPPORTED_FORMATS` = `[Abgr2101010, Argb2101010, Abgr8888, Argb8888]`
  (10-bit first). `initialize_output` walks these picking the first the connector accepts.

## Deliverable 1 — the PQ encode shader
Add `ENCODE_LINEAR_TO_PQ` to `src/hdr_shaders.rs`, same GLSL ES 100 / `//_DEFINES_` /
`sampler2D tex` / `varying vec2 v_coords` / `uniform float alpha` skeleton as the existing
shaders, PLUS a `uniform float sdr_white_nits;` (declared via `UniformName::new("sdr_white_nits",
UniformType::_1f)` when compiling — see Deliverable 2). The offscreen holds **linear BT.709,
scene-referred** values. Encode does, per fragment:
1. `vec3 lin709 = texture2D(tex, v_coords).rgb;`
2. **BT.709→BT.2020** (linear-light 3×3, row-major, verified constants):
   ```glsl
   const mat3 BT709_TO_BT2020 = mat3(
       0.627403896, 0.069097289, 0.016391439,   // column 0 (GLSL mat3 is column-major!)
       0.329283038, 0.919540395, 0.088013308,   // column 1
       0.043313066, 0.011362316, 0.895595253);  // column 2
   vec3 lin2020 = BT709_TO_BT2020 * lin709;
   ```
   NOTE: GLSL `mat3(...)` is **column-major**. The matrix whose ROWS are
   `[0.6274 0.3293 0.0433] / [0.0691 0.9195 0.0114] / [0.0164 0.0880 0.8956]` must be entered
   column-major as above. Double-check by confirming `BT709_TO_BT2020 * vec3(1,1,1) ≈
   vec3(1,1,1)` (white maps to white). If unsure, build the mat3 from three explicit column
   vectors and verify the white-point identity in a comment.
3. **Absolute nits, normalized to the PQ 10000-nit range:**
   `vec3 y = clamp(lin2020 * sdr_white_nits / 10000.0, 0.0, 1.0);`
4. **ST 2084 PQ OETF** (per channel), exact constants:
   ```glsl
   // SMPTE ST 2084 / Rec. 2100 PQ, m1 m2 c1 c2 c3
   vec3 pq_oetf(vec3 y) {
       const float m1 = 0.1593017578125;   // 2610/16384
       const float m2 = 78.84375;          // 2523/4096 * 128
       const float c1 = 0.8359375;         // 3424/4096
       const float c2 = 18.8515625;        // 2413/4096 * 32
       const float c3 = 18.6875;           // 2392/4096 * 32
       vec3 ym = pow(y, vec3(m1));
       return pow((c1 + c2 * ym) / (1.0 + c3 * ym), vec3(m2));
   }
   ```
5. `gl_FragColor = vec4(pq_oetf(y), 1.0) * alpha;` (the offscreen is opaque fullscreen; `alpha`
   is 1.0 in practice — keep the `* alpha` for shape parity with the other shaders. Do NOT
   apply alpha before the PQ curve.)

Update `HdrShaders`/`compile_hdr_shaders`: `encode` must now compile `ENCODE_LINEAR_TO_PQ` with
the `sdr_white_nits` uniform declared. **Remove `ENCODE_LINEAR_TO_SRGB`** (superseded — no dead
code; git history preserves it). Keep `DECODE_SRGB_TO_LINEAR` and `srgb_to_linear_solid`
unchanged.

## Deliverable 2 — feed the nits uniform + swap in `render_surface_hdr`
- Add a `const SDR_WHITE_NITS: f32 = 203.0;` (BT.2408 reference white) near the shader consts,
  with a comment that Phase 4 replaces this constant with a live per-output value from the SDR
  brightness slider (80–300). Do NOT build the slider now.
- In the encode pass, set the override with the uniform:
  `gles.set_default_tex_program_override(Some((shaders.encode.clone(),
  vec![Uniform::new("sdr_white_nits", SDR_WHITE_NITS)])));` (import `Uniform` — same type the
  Phase 0 probe used). Everything else in the encode pass stays byte-identical (fresh `Id`,
  `FrameFlags::empty()`, clear override after, `queue_frame`).
- The decode pass, gating, fallback, and offscreen management are UNCHANGED.

## Deliverable 3 — verify 10-bit scanout (diagnostic, not a behavior change)
PQ over 8-bit bands severely. Confirm the connector actually negotiated a 10-bit format:
- Find where `initialize_output` / the `DrmOutput` reports its chosen framebuffer format (the
  `DrmOutput`/compositor exposes a `format()` — it's already used at udev.rs ~830 in the
  multi-GPU renderer branch). At output bring-up for an `hdr` output, log once at info:
  the negotiated `Fourcc` (e.g. `HDR output DP-3 scanout format = Abgr2101010`).
- If the negotiated format is 8-bit (`Abgr8888`/`Argb8888`) despite `Abgr2101010` being offered,
  do NOT hack the format list — just log a clear `warn!` that HDR will band on 8-bit and note it
  in your report. (Forcing 10-bit / reordering negotiation is a follow-up if it happens.) This
  deliverable is purely a logged confirmation so the user can check the journal.

## Out of scope (later phases)
- Tone-mapping / HDR-content handling (no content exceeds SDR white yet) — Phase 6/1b.
- The live SDR-brightness slider + IPC — Phase 4 (this phase only wires the static uniform).
- `wp_color_management_v1`, EDID peak-nits — Phase 1b (static 203-nit white + the Phase-1a
  static 1000-nit mastering metadata are fine for now).
- Per-window HDR gating, screencopy HDR→SDR — Phase 4.

## Verification
- `timeout 600 cargo build --release` — clean (no new warnings; `ENCODE_LINEAR_TO_SRGB` fully
  removed, no dead-code warning).
- `timeout 600 cargo test` — green (118+; add a unit test only for a pure-logic helper if you
  introduce one — GLSL isn't unit-testable in-agent).
- `git status --short` shows only in-scope files (NOT `src/model/grid.rs`).
- snake_case; no new camelCase.
- **Do NOT run the compositor.**

## Tail
> STOP after a clean build + green tests. Do NOT commit. Report: the final
> `ENCODE_LINEAR_TO_PQ` GLSL (so I can eyeball the matrix + PQ math), the exact encode-pass
> override call with the uniform, what the scanout-format log will print / how you obtained the
> format, build + test results, and the user's hardware-test steps (DP-3 already `hdr = true`;
> rebuild; restart Rubix on a TTY; the desktop should now look correct — normal brightness, not
> oversaturated — in HDR mode; check the journal line for the negotiated scanout format).
