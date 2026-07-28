#!/usr/bin/env python3
"""Waybar module: volume with grow/shrink animation.

Port of the AwesomeWM wibar volume widget. Same event source (`pactl
subscribe`) plus a periodic drift-correction query. Emits a block gauge + %,
tags the module `.active` on a change and reverts to `.idle` ~1s later; the CSS
animates the grow/shrink + accent color on those class changes.

Single-threaded: we `select` on the `pactl subscribe` pipe with a timeout, so
the same loop handles live events, the settle-to-idle timer, and periodic
drift correction without threads."""

import json
import select
import subprocess
import sys
import time

GAUGE_CELLS = 10
FILLED = "█"
EMPTY = "░"

DRIFT_INTERVAL = 5.0      # periodic re-query to correct missed events
ACTIVE_HOLD = 1.0         # how long the module stays ".active" after a change


def queryVolume():
    """Return integer volume percent of the default sink, or None."""
    try:
        out = subprocess.run(
            ["pactl", "get-sink-volume", "@DEFAULT_SINK@"],
            capture_output=True, text=True, timeout=2,
        ).stdout
    except (subprocess.SubprocessError, OSError):
        return None
    for token in out.split():
        if token.endswith("%"):
            try:
                return int(token[:-1])
            except ValueError:
                continue
    return None


def queryMuted():
    try:
        out = subprocess.run(
            ["pactl", "get-sink-mute", "@DEFAULT_SINK@"],
            capture_output=True, text=True, timeout=2,
        ).stdout
    except (subprocess.SubprocessError, OSError):
        return False
    return "yes" in out.lower()


def gauge(percent):
    filled = round(min(percent, 100) / 100 * GAUGE_CELLS)
    return FILLED * filled + EMPTY * (GAUGE_CELLS - filled)


def emit(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()


def render(percent, muted, active):
    if muted:
        text = f" 󰝟 {gauge(0)} muted "
        cssClass = "muted"
    else:
        text = f" 󰕾 {gauge(percent)} {percent}% "
        cssClass = "active" if active else "idle"
    return {"text": text, "class": cssClass, "percentage": min(percent, 100)}


def main():
    sys.stdout.reconfigure(line_buffering=True)

    proc = subprocess.Popen(
        ["pactl", "subscribe"], stdout=subprocess.PIPE, text=True,
    )

    percent = queryVolume() or 0
    muted = queryMuted()
    emit(render(percent, muted, active=False))

    lastDrift = time.monotonic()
    activeUntil = 0.0
    wasActive = False

    while True:
        now = time.monotonic()
        # Wake for: a subscribe event, the active-hold expiry, or drift re-query.
        timeout = DRIFT_INTERVAL
        if activeUntil > now:
            timeout = min(timeout, activeUntil - now)

        ready, _, _ = select.select([proc.stdout], [], [], timeout)
        changed = False

        if ready:
            line = proc.stdout.readline()
            if line == "":
                break  # pactl died
            # Sink change (volume or mute) -- re-query the authoritative value.
            if "on sink" in line or "server" in line:
                newPercent = queryVolume()
                newMuted = queryMuted()
                if newPercent is not None and (newPercent != percent or newMuted != muted):
                    percent, muted = newPercent, newMuted
                    activeUntil = time.monotonic() + ACTIVE_HOLD
                    changed = True

        now = time.monotonic()
        if now - lastDrift >= DRIFT_INTERVAL:
            lastDrift = now
            newPercent = queryVolume()
            newMuted = queryMuted()
            if newPercent is not None and (newPercent != percent or newMuted != muted):
                percent, muted = newPercent, newMuted
                # Drift correction shouldn't trigger the grow animation.
                changed = True

        active = activeUntil > now
        # Emit on a value change OR when the active->idle edge is crossed, so the
        # shrink-back animation fires exactly once.
        if changed or (wasActive and not active):
            emit(render(percent, muted, active))
            wasActive = active


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(0)
