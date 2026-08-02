# Spec — HDR Phase 1a: fork pin + connector metadata commit (DP-3 OSD → HDR)

## Goal
Pin the `dividebysandwich/smithay` HDR fork at an exact rev and make a `hdr = true` output
actually commit `HDR_OUTPUT_METADATA` + `Colorspace` + `max_bpc` connector properties, so the
physical display's on-screen menu reports HDR mode. This is **Milestone 1** — the metadata
signalling path proven end-to-end, BEFORE any linear-compositing/render work (that is Phase 2).
Correct-looking HDR pixels are explicitly NOT a goal here; only that the panel switches into
HDR mode.

## Critical execution constraints (read first)
- This modifies the **live compositor render path** on the user's **daily-driver machine**.
  **DO NOT run, launch, or restart the compositor.** Do not execute the `rubix` binary. It
  would fight for the DRM master and break the running session. Your job ends at a clean
  `cargo build`. The user performs the session-swap test themselves on a TTY.
- Any build command must be wrapped in `timeout` (e.g. `timeout 600 cargo build`).
- **Rust snake_case** for all identifiers — Rubix uses Rust idiom, NOT the global camelCase
  rule. Match surrounding module style.
- **Two-step, with a hard checkpoint.** Do Deliverable 1 (pin + build + adapt the one broken
  call site) and **STOP to report** before starting Deliverable 2+. If pinning the fork breaks
  MORE than the single documented call site, stop and report rather than fixing broadly.

## Reference material (already gathered — read, don't re-derive)
- Fork source, cloned at the exact pinned rev, is at:
  `/tmp/claude-1000/-home-max-dotfiles/9281e39e-3513-4c32-a61b-26dd4d5018e5/scratchpad/smithay-hdr`
  (smithay fork, rev `57c805c8`) and `.../scratchpad/niri-hdr` (niri usage reference).
- A full integration map is at `.../scratchpad/INTEGRATION_MAP.md` — consult it for exact
  file:line refs and signatures. Key facts reproduced inline below.

### Fork color API (in `smithay-hdr/src/backend/drm/color.rs`)
```rust
pub enum Colorspace { Default, Bt2020Rgb, Bt2020Ycc, DciP3RgbD65, Unknown }
pub enum Eotf { TraditionalSdr, TraditionalHdr, SmpteSt2084, Hlg }
pub struct HdrOutputMetadata {
    pub eotf: Eotf,
    pub display_primaries: [CtaCoordinate; 3],
    pub white_point: CtaCoordinate,
    pub max_display_mastering_luminance: u16,  // cd/m²
    pub min_display_mastering_luminance: u16,  // 0.0001 cd/m² units
    pub max_cll: u16,
    pub max_fall: u16,
}
pub struct ConnectorColorState {
    pub colorspace: Colorspace,
    pub hdr_metadata: Option<HdrOutputMetadata>,
    pub max_bpc: Option<u32>,
}
```
Confirm `CtaCoordinate`'s constructor (how niri builds it — see `build_hdr_metadata` in
niri `src/backend/tty.rs` ~3690-3735) and the exact import paths from the fork yourself.

### How to reach + apply it from our managed `DrmOutput`
`DrmOutput::with_compositor(|comp| { ... })` gives `&mut DrmCompositor`, which exposes:
```rust
comp.supported_colorspaces(connector) -> FrameResult<Vec<Colorspace>>
comp.hdr_metadata_supported(connector) -> FrameResult<bool>
comp.max_bpc_range(connector) -> FrameResult<Option<RangeInclusive<u32>>>
comp.pending_color_state() -> ConnectorColorState
comp.use_color_state(state: ConnectorColorState) -> FrameResult<()>   // auto-folds into next commit
```
niri's apply pattern (reference): build `desired: ConnectorColorState`; `if
comp.pending_color_state() != desired { comp.use_color_state(desired) }`. No explicit flush —
it rides the next `render_frame`.

## Relevant Rubix code
- `src/udev.rs:112-117` — `SUPPORTED_FORMATS` (10-bit first).
- `src/udev.rs:379-386` — renderer/dmabuf formats obtained from the primary GPU's EGL context.
- `src/udev.rs:421-428` — `DrmOutputManager::new(...)` call site. **This is the one call the
  fork changes** (adds color/renderer format params — see Deliverable 1).
- `src/udev.rs:671-744` — `set_hdr_output_properties()`, currently a documented no-op stub;
  this is what Deliverable 3 replaces with the real commit.
- `src/udev.rs` `connector_connected` — the caller that invokes `set_hdr_output_properties`
  when an output is `hdr = true` (locate it; keep the call gated on the per-output hdr flag).
- `src/hdr.rs` — our hand-rolled 32-byte metadata blob + offset tests (`src/hdr_tests.rs`),
  now SUPERSEDED by the fork's typed `HdrOutputMetadata`. Deliverable 2 repurposes it.
- `src/config.rs` — `OutputConfig` already has a `hdr: bool` field (default false).

## Scope
- **Files to edit:** `Cargo.toml`, `src/udev.rs`, `src/hdr.rs`, `src/hdr_tests.rs`.
- **Files NOT to edit:** `src/model/grid.rs`, `src/state.rs`, `src/input.rs`, IPC/handlers,
  keybind code, the compositor main loop beyond the specific functions named above.
- **Out of scope:** `wp_color_management_v1` client protocol, per-surface ContentColor, full
  EDID parsing, the linear-compositing pipeline, PQ encode, tone mapping, 10-bit scanout
  format changes. All later phases. For 1a, HDR metadata values may use sensible **static
  defaults** (see Deliverable 3) — EDID-derived values are Phase 1b.

## Deliverables

### Deliverable 1 — Pin the fork and get a clean build (CHECKPOINT — stop & report after this)
1. `Cargo.toml`: add a patch section pinning BOTH smithay and smithay-drm-extras (they are one
   workspace; they must move together or versions will clash):
   ```toml
   [patch.crates-io]
   smithay = { git = "https://github.com/dividebysandwich/smithay", rev = "57c805c8e6d0b34601b07d89053b376905008d8a" }
   smithay-drm-extras = { git = "https://github.com/dividebysandwich/smithay", rev = "57c805c8e6d0b34601b07d89053b376905008d8a" }
   ```
   If `smithay-drm-extras` is not a member of the fork repo (verify), drop that line and note
   it. Keep the existing `smithay = "0.7"` / `smithay-drm-extras = "0.1"` dependency lines as-is
   — the patch overrides them; the fork reports version 0.7.0 so the override is accepted.
2. `timeout 600 cargo build`. The expected break is the `DrmOutputManager::new()` call site at
   `src/udev.rs:421-428`: the fork adds color-format + renderer-format parameters. Adapt the
   call to the fork's new signature (read it in
   `scratchpad/smithay-hdr/src/backend/drm/output.rs` ~line 242). Rubix already computes a
   `SUPPORTED_FORMATS` list and dmabuf render formats nearby (udev.rs:112-117, 379-386) — pass
   the appropriate lists per the new signature. Do the minimal change to satisfy the compiler.
3. **STOP and report:** whether the build is clean, the exact adapted `DrmOutputManager::new`
   call, and whether anything beyond that single call site broke. Do not proceed to Deliverable
   2 until this builds clean. If more than the one documented call site breaks, report the full
   list and your read before continuing.

### Deliverable 2 — Repurpose `src/hdr.rs` to the fork's typed metadata
- Replace the hand-rolled `build_hdr_metadata_blob() -> Vec<u8>` byte-packing with a function
  that returns the fork's types from our chosen default parameters, e.g.
  `pub fn default_hdr_color_state() -> ConnectorColorState` (or a `HdrOutputMetadata` builder) —
  PQ EOTF (`Eotf::SmpteSt2084`), BT.2020 primaries, D65 white point, using our existing
  constant values as the seed. Keep our primaries/white-point/luminance constants; feed them
  into `HdrOutputMetadata`/`CtaCoordinate` instead of packing bytes.
- `src/hdr_tests.rs`: the 12 byte-offset tests are now obsolete (the fork owns packing).
  Replace them with a small test that our builder produces a `ConnectorColorState` with the
  expected `Colorspace::Bt2020Rgb`, `Eotf::SmpteSt2084`, `max_bpc: Some(10)`, and non-None
  `hdr_metadata`. Remove the obsolete offset assertions — do not leave dead test code.

### Deliverable 3 — Replace the no-op with the real commit in `src/udev.rs`
Rewrite `set_hdr_output_properties()` (udev.rs:671-744) so that, when invoked for an output
with `hdr = true`, it:
1. Uses `drm_output.with_compositor(|comp| { ... })` to **probe**: `supported_colorspaces`
   contains `Colorspace::Bt2020Rgb`, `hdr_metadata_supported` is true, and `max_bpc_range`
   allows 10. If the connector lacks support, log a clear warning and return without error
   (graceful no-op — do not panic).
2. Builds `desired: ConnectorColorState` from Deliverable 2's helper: `colorspace:
   Bt2020Rgb`, `hdr_metadata: Some(<PQ metadata with static defaults>)`, `max_bpc: Some(10)`
   (clamp to the probed `max_bpc_range` if narrower). For Phase 1a, static default mastering
   luminance is fine (e.g. `max_display_mastering_luminance: 1000`, `min: 1` in 0.0001-cd/m²
   units, `max_cll`/`max_fall` reasonable defaults — mirror niri's fallbacks); EDID-derived
   values come in Phase 1b.
3. Applies it: `if comp.pending_color_state() != desired { comp.use_color_state(desired) }`,
   logging success/failure. On `Err`, log and return gracefully (no panic; the session must
   survive a failed color commit).
- Keep the call site in `connector_connected` gated on the per-output `hdr` flag (only
  `hdr = true` outputs get here). Preserve all non-HDR output behaviour byte-for-byte.

## Constraints
- **Conventions:** Rust snake_case throughout.
- **Dependencies:** only the fork pin above — add no other crates. No new features needed
  (color support is baked into the fork's core types; confirm via its `[features]`).
- **Behavior to preserve:** the entire non-HDR path (outputs without `hdr = true`) must be
  unchanged; a failed/unsupported HDR commit must degrade gracefully, never crash the session.

## Risk Assessment
- **Fork build breakage beyond the known call site:** possible but the map says only
  `DrmOutputManager::new` changed among APIs we use — the Deliverable-1 checkpoint exists to
  catch anything more before we invest further.
- **`CtaCoordinate` construction:** its exact constructor/units (0.00002 CTA steps) must match
  the fork — copy niri's `coord(...)` helper approach rather than guessing.
- **max_bpc unsupported:** some links won't allow 10 bpc; clamp to the probed range and still
  set colorspace+metadata (8-bit HDR signalling is valid for Milestone 1).
- **No behavioural test possible in-agent:** you cannot run the compositor, so correctness of
  the actual commit is verified by the user on hardware. Your bar is: clean build + the logic
  matches niri's proven sequence + graceful failure paths.

## Anti-loop guard
> If you produce more than ~10000 tokens between tool calls, stop and write a one-line "next
> step" note before continuing. This is a tripwire, not a hard limit — reasoning is welcome,
> but checkpoint when stuck.

## Verification
- After Deliverable 1: `timeout 600 cargo build` clean; report the adapted call.
- After Deliverables 2-3: `timeout 600 cargo build` clean; `timeout 600 cargo test` green
  (the new hdr builder test passes; no obsolete offset tests remain).
- snake_case check: `grep -nE '\b[a-z]+[A-Z]' src/hdr.rs src/udev.rs` shows no new camelCase
  identifiers introduced by your edits (pre-existing matches in unedited lines are fine).
- Confirm `git status` shows only the four in-scope files changed (plus `Cargo.lock`).
- **Do NOT run the compositor.** No `cargo run`, no launching `rubix`.

## Tail
> Stop after verification. Do not commit. Report: the Cargo patch added, the adapted
> `DrmOutputManager::new` call, the new `set_hdr_output_properties` logic, build + test
> results, and a one-paragraph note of the exact steps the USER must do to test on hardware
> (add `hdr = true` to the DP-3 `[[output]]` block, rebuild, restart the Rubix session on a
> TTY, check the DP-3 OSD for HDR mode).
