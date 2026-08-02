# Spec — Live HDR on/off toggle (keybind)

## Goal
Add a keybind that toggles HDR **live** on HDR-capable outputs, with no restart. Toggling ON
stages the PQ/BT.2020 connector color state + routes the output through the HDR render pipeline;
toggling OFF reverts the connector to the SDR default color state + routes through the plain SDR
path. Purpose: instant A/B comparison of the same content with/without HDR (currently `surface.hdr`
is set once at bringup and "never changes live", forcing a config-edit + TTY restart to compare).

## Critical execution constraints (read first)
- Live render path on the user's **daily-driver**. **DO NOT run/launch/restart the compositor**
  (no `cargo run`). Stop at a clean `timeout 600 cargo build --release` + `timeout 600 cargo test`.
- **Rust snake_case** throughout.
- Do NOT touch `src/model/grid.rs` (unrelated dirty user work). Do NOT `git commit` / `git add`.
- A brief panel flash on toggle (the connector entering/leaving HDR mode) is expected and
  acceptable — do not try to suppress it.

## Established facts (verified — do not re-investigate)
- `SurfaceData.hdr: bool` (src/udev.rs:186-190) gates the HDR render branch; set at bringup from
  `output_hdr` (udev.rs:748, from `OutputConfig::hdr` at udev.rs:615).
- `set_hdr_output_properties(drm_output: &RubixDrmOutput)` (udev.rs:799-870) probes
  `supported_colorspaces` / `hdr_metadata_supported` / `max_bpc_range` via
  `drm_output.with_compositor(|comp| ...)`, then `comp.use_color_state(desired)` where `desired =
  crate::hdr::default_hdr_color_state()`. Graceful-degrades (warns, leaves SDR) if unsupported.
- Reverting to SDR: `comp.use_color_state(ConnectorColorState::default())` — `Colorspace::Default`
  (sRGB/BT.709) + `hdr_metadata: None` + `max_bpc: None`. `use_color_state` stages for the next
  atomic commit; safe to call repeatedly at runtime, no re-init. (`ConnectorColorState` is in
  `smithay::backend::drm` — same import path as `Colorspace` already used in udev.rs.)
- Runtime surface iteration: `UdevData.backends: HashMap<DrmNode, BackendData>`, each with
  `surfaces: HashMap<crtc::Handle, SurfaceData>`. `nudge_all_renders(udev: &Rc<RefCell<UdevData>>)`
  (udev.rs:1609-1621) shows the iterate-then-`schedule_render(udev, node, crtc, Duration::ZERO)`
  pattern. `schedule_render` takes `&Rc<RefCell<UdevData>>`, so it must be called AFTER any
  `udev.borrow_mut()` is dropped.
- Keybind path: `NavAction` enum (input.rs:27-47); the `IncreaseSdrWhite`/`DecreaseSdrWhite`
  dispatch arms (input.rs:362-373) call `self.nudge_render()`. `RubixState::nudge_render`
  (state.rs:323-327) → `crate::udev::nudge_all_renders(udev)`. Config deserializes the chord-string
  value straight into the `NavAction` variant name via serde — no mapping table. Default bind-count
  assertion is `24` in config_tests.rs:14. default.toml `[keybinds]` at line 58+, sdr_white binds
  at 99-103.

## Deliverable 1 — per-output capability flag (src/udev.rs)
Add `hdr_capable: bool` to `SurfaceData` (next to `hdr`). Set it = `output_hdr` at bringup
(udev.rs:748 area: `hdr: output_hdr, hdr_capable: output_hdr,`). `hdr` now means the **live**
state; `hdr_capable` is the fixed "this output may use HDR" gate (so the toggle never tries to
enable HDR on a non-HDR output like the HDMI strip). Update the `hdr` doc comment (it currently
says "never changes live" — it does now; note it's gated by `hdr_capable`).

## Deliverable 2 — SDR revert + toggle fn (src/udev.rs)
- Add `fn set_sdr_output_properties(drm_output: &RubixDrmOutput)` mirroring
  `set_hdr_output_properties`'s `with_compositor` shape but staging the SDR default:
  ```rust
  drm_output.with_compositor(|comp| {
      let desired = ConnectorColorState::default();
      if comp.pending_color_state() != desired {
          match comp.use_color_state(desired) {
              Ok(()) => tracing::info!("HDR: reverted connector to SDR default color state"),
              Err(e) => tracing::warn!("HDR: failed to revert to SDR color state: {e}"),
          }
      }
  });
  ```
  (Import `ConnectorColorState` alongside the existing `Colorspace` import.)
- Add `pub(crate) fn toggle_hdr(udev: &Rc<RefCell<UdevData>>)`:
  1. In a `udev.borrow_mut()` scope, iterate `backends.values_mut()` → `surfaces` as
     `(crtc, surface)` and, for each surface where `surface.hdr_capable`, flip
     `surface.hdr = !surface.hdr`; then call `set_hdr_output_properties(&surface.drm_output)` if
     now on, else `set_sdr_output_properties(&surface.drm_output)`. Collect the `(node, crtc)` of
     each toggled surface into a `Vec` (need the `DrmNode` key too — iterate
     `backends.iter_mut()` for `(node, backend)`). Log one `info!` line per toggle with the new
     state and output name.
  2. Drop the borrow, then `schedule_render(udev, node, crtc, Duration::ZERO)` for each collected
     target (same as `nudge_all_renders`). This forces the repaint AND flushes the staged color
     state on the next atomic commit.
  - If no surface is `hdr_capable`, log a single `info!("HDR toggle: no HDR-capable output")` and
    do nothing.

## Deliverable 3 — state + action wiring
- `src/state.rs`: add `pub(crate) fn toggle_hdr(&self)` mirroring `nudge_render` — guards on
  `self.udev_handle` and calls `crate::udev::toggle_hdr(udev)`.
- `src/input.rs`: add `ToggleHdr` to `NavAction`; add the dispatch arm:
  `NavAction::ToggleHdr => { self.toggle_hdr(); }` (no layout/geometry touched; `toggle_hdr` does
  its own render scheduling, so do NOT also call apply_layout).

## Deliverable 4 — config binds + test
- `config/default.toml`: under `[keybinds]`, add (with a short comment) a NON-colliding default
  chord — use `"Alt+Shift+h" = "ToggleHdr"` (verify `Alt+Shift+h` isn't already bound in the file;
  if it is, pick another free Alt+Shift+<letter>). Matches the file's Alt-for-nested-testing note.
- `src/config_tests.rs`: bump the default bind-count assertion `24` → `25`.
- (The user's real-session bind in `~/.config/rubix/config.toml` is handled separately by Opus —
  do NOT edit anything outside the rubix repo.)

## Verification
- `timeout 600 cargo build --release` — clean, no new warnings.
- `timeout 600 cargo test` — green (config_tests bind-count now 25).
- `git status --short` shows only in-scope rubix files, NOT `src/model/grid.rs`. Do NOT commit/add.
- snake_case; no camelCase.
- **Do NOT run the compositor.**

## Tail — report back
STOP after clean build + green tests. Do NOT commit. Report: the `toggle_hdr` fn body + the
`set_sdr_output_properties` fn, the NavAction arm, the exact default chord chosen (and confirmation
it doesn't collide), build + test results, and the user's test steps (rebuild; restart on TTY;
press the toggle chord over a paused HDR clip; expect the HDR half's highlights to drop to SDR
white and back on each press, with a brief panel flash).
