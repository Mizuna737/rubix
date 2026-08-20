#!/usr/bin/env bash
# Sample a process's RSS over time. Used to tell a real leak from a warm-up.
#   tools/rssWatch.sh [processName] [durationSeconds] [intervalSeconds]
set -uo pipefail
name="${1:-rubix}"
duration="${2:-600}"
interval="${3:-30}"
pid=$(pgrep -x "$name" | head -1)
[ -z "$pid" ] && { echo "no process named $name"; exit 1; }
echo "pid $pid ($name), sampling every ${interval}s for ${duration}s"
echo "elapsed_s  rss_mb  vsz_mb  fds  threads"
end=$((SECONDS + duration))
while [ $SECONDS -lt $end ]; do
    [ -d "/proc/$pid" ] || { echo "process gone"; exit 0; }
    read -r rss vsz threads < <(ps -o rss=,vsz=,nlwp= -p "$pid")
    fds=$(ls /proc/$pid/fd 2>/dev/null | wc -l)
    # awk, not bc: bc is not installed on this system and its absence made the
    # first version of this script silently print nothing but errors.
    awk -v s="$SECONDS" -v r="$rss" -v v="$vsz" -v f="$fds" -v t="$threads" \
        'BEGIN { printf "%8d  %6.1f  %6.1f  %3d  %3d\n", s, r/1024, v/1024, f, t }' 
    sleep "$interval"
done
