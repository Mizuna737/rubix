# Spec — HDR output image-description advertisement (browser HDR detection)

## Goal
Advertise a proper HDR image description on `hdr`-enabled outputs via the color-management
protocol, so HDR-aware clients that *query the output* before producing HDR (Chromium/Vivaldi,
etc.) detect HDR headroom and enable HDR rendering. Currently Rubix leaves
`ColorManagementHandler::description_for_output` at its default (returns `ImageDescription::SRGB`),
so browsers conclude the display is SDR and never emit HDR — even though our per-surface decode
(Phase 1b) would handle it. Symptom being fixed: testufo.com/hdr shows the WCG flash (color
management IS active) but not the HDR flash. mpv is unaffected (it force-outputs PQ regardless);
this is specifically the browser-detection path.

## Critical execution constraints (read first)
- Live daily driver. **DO NOT run/launch/restart the compositor.** Stop at clean
  `timeout 600 cargo build --release` + `timeout 600 cargo test`.
- **Rust snake_case.** Do NOT touch `src/model/grid.rs`. Do NOT `git commit`/`git add`.

## Established facts (verified — do not re-investigate)
- Trait (fork, src/wayland/color/management/mod.rs:460-493):
  `fn description_for_output(&mut self, output: &Output) -> ImageDescription` — default returns
  `ImageDescription::SRGB`. Called from the `GetImageDescription` dispatch (mod.rs:876-880) via
  `Output::from_resource`.
- `ImageDescription` fields (mod.rs:232-272): `transfer: TransferFunction`,
  `primaries: PrimariesOption { named: Option<Primaries>, values: Option<Chromaticities> }`,
  `max_cll: Option<u32>`, `max_fall: Option<u32>`, `mastering_luminance: Option<(u32,u32)>`,
  `mastering_primaries: Option<Chromaticities>`, `luminances: Option<(u32,u32,u32)>` (min in
  0.0001 cd/m², max in cd/m², reference white in cd/m²), `windows_scrgb: bool`,
  `windows_bt2100: bool`.
- Enum variants: `TransferFunction::St2084Pq`, `Primaries::Bt2020` (import from the same re-export
  path rubix's color_management.rs already uses for `TransferFunction`/`Primaries`).
- `ImageDescription::SRGB` is a predefined constant (keep using it for the SDR case).
- Refresh mechanism: `ColorManagementState::output_description_changed(&mut self, output: &Output)`
  (mod.rs:680-691) notifies all bound `wp_color_management_output_v1` objects for that output to
  re-fetch. Pull-based otherwise — no pre-binding needed.
- Rubix handler impl is src/color_management.rs:92-114 (currently overrides only
  `color_management_state` + `schedule_image_description_info`; leaves the two description methods
  defaulted). `RubixState.sdr_white_nits: f32` is the live SDR-white value.
- Per-output HDR state: `RubixState.udev_handle: Option<Rc<RefCell<UdevData>>>`;
  `UdevData.backends: HashMap<DrmNode, BackendData>`; `BackendData.surfaces:
  HashMap<crtc::Handle, SurfaceData>`; `SurfaceData { output: Output, hdr: bool, .. }`. Match
  `surface.output == *output` to find the live `hdr` bool.
- The live HDR toggle lives in `udev::toggle_hdr` (udev.rs), invoked via `RubixState::toggle_hdr`
  (state.rs) from the `NavAction::ToggleHdr` arm.

## Deliverable 1 — override `description_for_output` (src/color_management.rs)
Add a helper that builds the HDR description:
```rust
/// PQ/BT.2020 image description advertised on HDR-enabled outputs so HDR-aware
/// clients detect display HDR headroom. `ref_white` ties the advertised
/// reference white to the live SDR-white slider so SDR content sits at the same
/// level clients expect.
fn hdr_output_description(ref_white: u32) -> ImageDescription {
    ImageDescription {
        transfer: TransferFunction::St2084Pq,
        primaries: PrimariesOption { named: Some(Primaries::Bt2020), values: None },
        max_cll: None,
        max_fall: None,
        mastering_luminance: None,
        mastering_primaries: None,
        // (min 0.0001 cd/m², max cd/m², reference white cd/m²). 1000-nit peak
        // matches our Phase 1a mastering metadata; ref white follows the slider.
        luminances: Some((50, 1000, ref_white)),
        windows_scrgb: false,
        windows_bt2100: false,
    }
}
```
Override the trait method:
```rust
fn description_for_output(&mut self, output: &Output) -> ImageDescription {
    let is_hdr = self
        .udev_handle
        .as_ref()
        .and_then(|udev| udev.try_borrow().ok().map(|u| {
            u.backends.values().any(|b| {
                b.surfaces.values().any(|s| &s.output == output && s.hdr)
            })
        }))
        .unwrap_or(false);
    if is_hdr {
        hdr_output_description(self.sdr_white_nits.round().clamp(1.0, 10_000.0) as u32)
    } else {
        ImageDescription::SRGB
    }
}
```
Use `try_borrow` (NOT `borrow`) — this runs during protocol dispatch and must never panic on an
already-borrowed `UdevData`; fall back to SRGB if borrowed. Import `Output`, `ImageDescription`,
`TransferFunction`, `Primaries`, `PrimariesOption` as needed (match rubix's existing import style;
`SurfaceData`/`UdevData` fields must be reachable — if `surfaces`/`output`/`hdr` aren't visible
from color_management.rs's module, add the minimal `pub(crate)` visibility rather than moving
logic). If matching `&s.output == output` needs `Output: PartialEq` (it is), fine; otherwise
compare by `Output::name()`.

## Deliverable 2 — refresh clients on live toggle
When `udev::toggle_hdr` flips an output, bound browser color-management-output objects should be
told to re-query so HDR turns on/off without a page reload. Because `output_description_changed`
lives on `ColorManagementState` (on `RubixState`), do the notification at the `RubixState` layer:
- In `RubixState::toggle_hdr` (state.rs), AFTER calling `crate::udev::toggle_hdr(udev)`, iterate
  the HDR-capable outputs and call
  `self.color_management_state.output_description_changed(&output)` for each. Get the outputs from
  the udev surfaces (borrow `udev`, collect the `Output`s of `hdr_capable` surfaces into a `Vec`,
  drop the borrow, then notify) or from `self.space.outputs()` — whichever avoids a double borrow
  of `udev`/`self`. Keep it panic-free (`try_borrow`).
- If threading the borrow is awkward, an acceptable simpler form: collect the toggled outputs
  inside `udev::toggle_hdr` and return them, and have `RubixState::toggle_hdr` notify from that
  returned list. Choose whichever is cleaner; document the choice in the report.

## Verification
- `timeout 600 cargo build --release` — clean, no new warnings.
- `timeout 600 cargo test` — green (120+; add a unit test for `hdr_output_description` asserting
  transfer==St2084Pq, primaries named==Bt2020, luminances==(50,1000,ref_white) for a sample
  ref_white — pure logic, cheap).
- `git status --short` — only in-scope files, NOT `src/model/grid.rs`. No commit/add.
- snake_case. **Do NOT run the compositor.**

## Tail — report back
STOP after clean build + green tests. Do NOT commit. Report: the final `description_for_output`
override + helper, how Deliverable 2's toggle-refresh was wired (and the borrow approach chosen),
whether any visibility (`pub(crate)`) had to be widened and where, build+test results, and the
user's test steps (rebuild; restart on TTY with DP-3 hdr on; open testufo.com/hdr in Vivaldi —
the HDR flash should now appear alongside the WCG flash; toggling Super+Alt+h should flip HDR
detection live without reloading the page).
