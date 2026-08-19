#!/usr/bin/env bash
# Ground truth for "is this connector actually in HDR right now?", read from the
# kernel via DRM connector properties.
#
# Independent of both Rubix's own logging and of what the picture looks like --
# which matters, because HDR is genuinely hard to judge by eye, and a game
# rendering with HDR tone curves into an SDR-mode panel looks "different"
# without the panel ever having left SDR.
#
# Usage:
#   tools/hdrState.sh            sample once
#   tools/hdrState.sh -w [secs]  sample every 2s (default 120), print only changes
#
# Prefer the watch form: start it, then launch the game. The transition is the
# evidence; a single sample at the wrong moment proves nothing.
conn="${CONN:-DP-3}"

read_colorspace() {
  timeout 20 modetest -c 2>/dev/null | awk -v conn="$conn" '
    $0 ~ ("[[:space:]]" conn "[[:space:]]") { inconn = ($3 ~ /^connected/) ? 1 : 0; next }
    inconn && /^[0-9]+[[:space:]]/ { inconn = 0 }
    inconn && /[[:space:]]Colorspace:/ { grab = 1; next }
    inconn && grab && /value:/ { print $2; exit }
  '
}

verdict() {
  local v; v="$(read_colorspace)"
  case "$v" in
    9)  printf 'colorspace=BT2020_RGB (9)   => connector is in HDR mode\n' ;;
    10) printf 'colorspace=BT2020_YCC (10)  => connector is in HDR mode (YCC)\n' ;;
    0)  printf 'colorspace=Default (0)      => connector is in SDR mode\n' ;;
    "") printf 'colorspace=<unreadable>     => %s not connected, or modetest blocked\n' "$conn" ;;
    *)  printf 'colorspace=%s (unrecognised)\n' "$v" ;;
  esac
}

if [[ "$1" != "-w" ]]; then
  printf '=== %s @ %s ===\n' "$conn" "$(date +%T)"
  verdict
  exit 0
fi

secs="${2:-120}"
printf 'watching %s for %ss, printing changes only. Launch the game now.\n\n' "$conn" "$secs"
last=""; end=$(( SECONDS + secs ))
while (( SECONDS < end )); do
  now="$(verdict)"
  [[ "$now" != "$last" ]] && { printf '%s  %s\n' "$(date +%T)" "$now"; last="$now"; }
  sleep 2
done
printf '\ndone.\n'
