#!/usr/bin/env bash
# Pull the direct-scanout diagnostic out of the current Rubix session log.
# rubix.log is truncated at every start (main.rs open_log_file), so this is
# always exactly the live session -- no marker needed.
log="${1:-$HOME/.cache/rubix/rubix.log}"

printf '=== %s\n=== %s bytes, modified %s\n\n' \
  "$log" "$(stat -c %s "$log")" "$(stat -c %y "$log")"

blocks=$(grep -c 'direct-scanout on' "$log")

if [[ $blocks -eq 0 ]]; then
  cat <<'MSG'
!! NO direct-scanout lines in this session.

On an hdr=true output that is NOT proof scanout is fine -- it means the
diagnostic never ran. udev.rs:1410 gates the HDR composite path on
`surface.hdr && fullscreen_kind.is_none()`, and that path returns before the
diagnostic. So zero lines means fullscreen_kind stayed None, i.e.
fullscreen_scanout_target never matched the game.

That points at Spec C detection (does the window bbox actually contain the
full output geometry?), NOT at buffer formats. Check the fullscreen lines
below to see whether the compositor ever considered the window fullscreen.
MSG
else
  printf -- '--- %s direct-scanout block(s) ---\n' "$blocks"
  # tracing prefixes every line with a timestamp, so detail lines are NOT
  # indented at line start -- their indentation sits after "rubix::udev: ".
  awk '
    /direct-scanout on/            { inblock=1; print; next }
    inblock && /rubix::udev:   +/  { print; next }
    inblock                        { inblock=0 }
  ' "$log"
fi

printf -- '\n--- fullscreen tracking ---\n'
grep -inE 'fullscreen' "$log" | grep -viE 'egl|extensions:' | tail -25

printf -- '\n--- X11 geometry negotiation (asked vs granted) ---\n'
grep -inE 'configure_request|fullscreen_request' "$log" | tail -25

printf -- '\n--- scanout / dmabuf setup ---\n'
grep -inE 'scanout format|dmabuf scanout|advertised zwp_linux_dmabuf|negotiated' "$log" \
  | grep -viE 'egl|extensions:' | tail -15

printf -- '\n--- errors and warnings ---\n'
grep -inE '\b(ERROR|WARN)\b' "$log" | grep -viE 'egl|extensions:' | tail -25
