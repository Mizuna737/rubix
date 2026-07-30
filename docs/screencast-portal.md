# ScreenCast portal (in-process xdg-desktop-portal backend)

Rubix serves `org.freedesktop.impl.portal.ScreenCast` (interface version 2)
directly from the compositor process, at bus name
`org.freedesktop.impl.portal.desktop.rubix`, object path
`/org/freedesktop/portal/desktop`. `CreateSession`, `SelectSources`, and
`Start` are implemented; `Start` runs an interactive `rofi`-based source
chooser and streams the chosen monitor or window over PipeWire.

`AvailableSourceTypes` advertises `0b11` (`MONITOR = 1 | WINDOW = 2`) --
both whole-monitor and single-window capture are supported.

## Dependency: rofi

The `Start` chooser shells out to `rofi -dmenu -i -p "Share" -format i`. It
must be on `PATH` for a share request to succeed. `slurp` (pointer-based
region selection) is deliberately NOT used: under Rubix's current
layer-shell handling, pointer clicks into layer-shell surfaces don't
reliably register, while keyboard-driven clients like rofi work. If `rofi`
is missing, `Start` logs an error and returns a failed response rather than
hanging.

## Current status: NOT live

Nothing in `~/.config/xdg-desktop-portal` routes to this backend yet.
`xdg-desktop-portal-wlr` remains the active `ScreenCast` implementation.
This backend can be built and exercised (`RUBIX_PORTAL=0` disables it
entirely if it misbehaves) without affecting real screenshare until the
cutover steps below are performed deliberately.

## Go-live cutover steps

Perform these in order, coordinated with a planned compositor restart --
do not do this mid-session:

1. **Restart the compositor into a build containing this branch**
   (`feat/screencast-portal` or whatever it lands as on `main`). The portal
   registers itself automatically on startup (`init_portal`, gated by
   `RUBIX_PORTAL`, default on).
2. **Install the portal file** so `xdg-desktop-portal` core knows this
   backend exists:
   ```sh
   mkdir -p ~/.local/share/xdg-desktop-portal/portals
   cp dist/rubix.portal ~/.local/share/xdg-desktop-portal/portals/rubix.portal
   ```
3. **Route `ScreenCast` to it** in
   `~/.config/xdg-desktop-portal/portals.conf` by setting (adding if the
   file/section doesn't exist yet):
   ```ini
   [preferred]
   org.freedesktop.impl.portal.ScreenCast=rubix
   ```
   Leave every other interface (`FileChooser`, `Notification`, etc.)
   pointed at whatever backend already serves it -- this line only takes
   over `ScreenCast`.
4. **Restart the portal service** so it re-reads the config and re-resolves
   backends:
   ```sh
   systemctl --user restart xdg-desktop-portal
   ```
5. Test with a real client (e.g. a Teams/browser screenshare prompt): it
   should now spawn Rubix's `rofi` chooser instead of `slurp`/wlr's own UI.

None of steps 2-4 are applied by this change -- they are manual, deliberate
steps for the user to run at their next planned restart.
