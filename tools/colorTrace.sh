#!/usr/bin/env bash
# Pull the color-management conversation out of a WAYLAND_DEBUG capture.
#
# WAYLAND_DEBUG=1 logs EVERY request and event -- a fullscreen game at high
# refresh produces hundreds of MB in minutes, almost all of it wl_surface commit
# / frame callback noise. This keeps only the color-management negotiation.
#
# Usage: tools/colorTrace.sh /tmp/kcd2-wayland.log
log="${1:?usage: colorTrace.sh <wayland-debug-log>}"

printf '=== %s (%s) ===\n\n' "$log" "$(du -h "$log" | cut -f1)"

printf -- '--- 1. did the client bind the manager, and what did we advertise? ---\n'
grep -aE "wp_color_manager_v1" "$log" | grep -aE "bind|supported_(feature|tf_named|primaries_named|intent)|done" | head -40

printf -- '\n--- 2. what did it try to CREATE? (the decisive question) ---\n'
grep -aE "create_(parametric_creator|windows_scrgb|windows_bt2100|icc_creator)|get_output|get_surface" "$log" | head -20

printf -- '\n--- 3. what did it set on a parametric description? ---\n'
grep -aE "set_(tf_named|tf_power|primaries_named|primaries|luminances|mastering_display_primaries|max_cll|max_fall)" "$log" | head -30

printf -- '\n--- 4. did the description succeed or fail? ---\n'
printf '    (wp_image_description_v1.ready = success; .failed = rejected, with a cause)\n'
grep -aE "wp_image_description_v1" "$log" | grep -aE "ready|failed" | head -20

printf -- '\n--- 5. was it actually attached to a surface? ---\n'
grep -aE "set_image_description|unset_image_description" "$log" | head -20

printf -- '\n--- 6. protocol errors of any kind ---\n'
grep -aiE "error|unsupported_feature|invalid" "$log" | grep -avE "wl_pointer|wl_keyboard|frame" | head -25

printf -- '\n--- summary ---\n'
for pat in create_windows_scrgb create_windows_bt2100 create_parametric_creator set_image_description "image_description_v1.*failed" "image_description_v1.*ready"; do
  printf '  %-40s %s\n' "$pat" "$(grep -acE "$pat" "$log")"
done
