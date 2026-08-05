# Spec C — Detect fullscreen requested *before* a window maps

## Goal
Rubix only ever learns about fullscreen that is toggled **after** a window is mapped. A client that
comes up fullscreen — the normal case for a game — is never entered into `state.fullscreen_windows`,
so every piece of the direct-scanout work (full-output rect override, raise-to-top, layer clearing,
HDR connector follow) is dead code for exactly the workload it was written for.

Observed: Outward (Unity → SDL2 → Proton → Xwayland) launched under Rubix, was tiled into its grid
slot, and its content overflowed the slot bounds.

Land this AFTER Spec A (`direct-scanout-fullscreen.md`) and Spec B
(`direct-scanout-dmabuf-tranche.md`), whose changes are already in the working tree, uncommitted.

## Critical execution constraints (read first)
- Live daily driver. **DO NOT run/launch/restart the compositor** — no `cargo run`, no launching
  `rubix`, no executing the built binary. Stop at a clean
  `timeout 600 cargo build --release` + `timeout 600 cargo test`.
- **Rust snake_case** throughout (this repo is the exception to the camelCase house rule).
- **Do NOT touch `src/model/grid.rs`** or anything else under `src/model/`. It carries the user's
  own uncommitted work and must stay exactly as-is.
- Do NOT `git commit` / `git add` / `git stash` / `git checkout` / `git restore`. Do NOT edit
  anything outside `~/Projects/rubix`.
- `src/state.rs` and `src/udev.rs` already carry uncommitted Spec A/B changes. Add to them; do not
  revert or restructure what is there.

## Established facts (verified — do not re-investigate, do not spend tool calls re-checking)
- EWMH: a client that wants to start fullscreen sets the `_NET_WM_STATE` **property** before
  `XMapWindow`. The `ClientMessage` form is only for changing state on an already-mapped window.
  SDL2 follows this exactly.
- The pinned smithay fork **already reads that pre-map property** into the surface's `net_state`,
  in the `MapRequest` arm at
  `/home/max/.cargo/git/checkouts/smithay-56902e19d4822414/57c805c/src/xwayland/xwm/mod.rs:1665-1674`,
  immediately before it dispatches `state.map_window_request(..)`.
  Therefore **`X11Surface::is_fullscreen()` is already accurate inside `map_window_request`.**
  No fork patch is needed. Do not modify the fork.
- `XwmHandler::fullscreen_request` (fork `xwm/mod.rs:2522-2531`) only fires from the `_NET_WM_STATE`
  ClientMessage path — i.e. post-map only. That is why the X11 side misses it today.

## Deliverable 1 — X11: honor initial fullscreen at map time
`src/handlers/xwayland.rs:69` `map_window_request`.

The `window: X11Surface` argument is moved into `Window::new_x11_window(window)` on the line after
`set_mapped`. **Read `window.is_fullscreen()` into a local before that move.**

After the id is allocated and the window inserted into `self.windows`, and **before** the
`self.apply_layout()` call, insert the id into `self.fullscreen_windows` when that local is true.
Log it at `info!` so the new session log shows the decision.

The rest of `map_window_request` is unchanged — the window still enters the grid model via
`monitor.add_window(..)`. That is deliberate and matches how Spec A treats fullscreen: the window
keeps its slot in the model and is merely drawn at full output size on top of it.

Do not add a `set_fullscreen(true)` call back to the client; the client already believes it is
fullscreen and the fork's `net_state` already carries the atom.

## Deliverable 2 — Wayland: honor fullscreen requested before the first commit
`src/handlers/xdg_shell.rs`.

`new_toplevel` stages the window in `self.unmapped` keyed by id; it is promoted into `self.windows`
on its first buffer commit in `src/handlers/compositor.rs:59-85`. The **id is stable across that
promotion**. But `fullscreen_request` (`xdg_shell.rs:194`) searches only `self.windows`, so a client
that calls `set_fullscreen` before its first commit — the normal way to start fullscreen — is
silently dropped.

Change `fullscreen_request` so that when the surface is not found in `self.windows`, it also looks in
`self.unmapped`. When found there:
- insert the id into `self.fullscreen_windows` (it will be live by the time the window is promoted);
- set `xdg_toplevel::State::Fullscreen` in the pending state **and** set `state.size` to the active
  monitor's output bounds, so the client's *initial* configure already carries the fullscreen size
  and its first buffer is output-sized rather than arriving small and resizing;
- `send_pending_configure()`;
- do **not** call `self.apply_layout()` — the window is not in the model yet, and the promotion path
  in `compositor.rs` calls it.

Use the same output-bounds lookup Spec A used for the fullscreen rect override in `state.rs`
(`workspace.active_monitor()` → `output_bounds_for(..)`); if it yields `None`, set the Fullscreen
state anyway and skip the size.

Apply the mirror-image change to `unfullscreen_request` (remove from `fullscreen_windows`, unset the
state) so a client that sets then clears fullscreen before mapping does not leave a stale id behind.

## Deliverable 3 — do not leak ids for windows that never map
`src/handlers/xdg_shell.rs:67-76` (`toplevel_destroyed`, the path that drops an id out of
`self.unmapped`) must now also `self.fullscreen_windows.remove(&id)`, since Deliverable 2 can insert
an id while the window is still unmapped. Without this, a toplevel that requests fullscreen and then
dies before committing leaves a permanently-set id in `fullscreen_windows`, which Spec A's
`fullscreen_scanout_target` and the `apply_layout` raise loop both iterate.

Check the X11 side too: `remove_x11_window` (`xwayland.rs:31-50`) already removes from
`fullscreen_windows` — confirm it and say so, do not duplicate it.

## Deliverable 4 — diagnostic
One `info!` per decision point, phrased so they are greppable in `~/.cache/rubix/session.log`:
- X11 map-time detection fired (include the window id and the class/title if cheaply available).
- Wayland pre-map fullscreen request captured (include the window id).
Keep these to one line each. No DEBUG flags, no per-frame logging.

## Explicitly out of scope
- `CropRenderElement` containment of over-sized client buffers — deliberately held until the session
  log tells us what actually overflowed. Do not implement it.
- `configure_request`'s fullscreen branch (`xwayland.rs:154-165`) uses `active_monitor()` rather than
  the monitor that actually owns the window. Known, deliberately unfixed here. Leave it.
- Any change to the pinned smithay fork.
- Any change to `src/model/`.

## Verification
- `timeout 600 cargo build --release` — clean, no new warnings from the touched files. (Five
  pre-existing warnings from `grid.rs` / `config.rs` / `traits.rs` are expected; ignore them.)
- `timeout 600 cargo test` — green. Baseline is 121 passed / 0 failed / 1 ignored.
- `git status --short` — expect ` M src/model/grid.rs` (the user's, untouched),
  ` M src/state.rs`, ` M src/udev.rs`, ` M src/handlers/xwayland.rs`,
  ` M src/handlers/xdg_shell.rs`, ` M src/handlers/compositor.rs` (only if you needed it), plus the
  untracked spec files. **No `git add`, no `git commit`.**
- **Do NOT run the compositor.**

## Tail — report back
STOP after a clean build + green tests. Report:
1. The exact edit made in `map_window_request`, and confirmation that `is_fullscreen()` was read
   before the `X11Surface` was moved.
2. How `fullscreen_request` distinguishes the mapped vs unmapped case, and whether the initial
   configure carries the output size.
3. Whether `unfullscreen_request` and the destroy paths were covered, and confirmation of what
   `remove_x11_window` already does.
4. Build + test output, and `git status --short`.
5. Anything that contradicts the "Established facts" section — say so explicitly rather than working
   around it silently.
