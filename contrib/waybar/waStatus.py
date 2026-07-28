#!/usr/bin/env python3
"""Waybar module: workAssistant status (REC / Transcribing / Notes).

Port of the AwesomeWM wibar `waWidget`. Polls the same /tmp marker files every
2s and animates a braille spinner at ~0.15s while active. Persistent process:
one JSON line per visual change; emits nothing-but-empty when idle so the slot
collapses. Requires a compositor that delivers frame callbacks to layer
surfaces (Rubix does, as of the layer send_frame fix)."""

import json
import os
import sys
import time

SPINNER_FRAMES = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"]

# Marker file -> (state name, waybar class, label). Order = priority.
MARKERS = [
    ("/tmp/workAssistant-record.pid", "recording", "recording", "● REC"),
    ("/tmp/workAssistant-transcribe.lock", "transcribing", "transcribing", "Transcribing"),
    ("/tmp/workAssistant-notes.lock", "notes", "notes", "Notes"),
]

POLL_INTERVAL = 2.0
SPINNER_INTERVAL = 0.15


def currentState():
    """Return (state, cssClass, label) for the highest-priority active marker,
    or the idle tuple."""
    for path, state, cssClass, label in MARKERS:
        if os.path.exists(path):
            return state, cssClass, label
    return "idle", "idle", ""


def emit(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()


def render(state, cssClass, label, spinnerIdx):
    if state == "idle":
        return {"text": "", "class": "idle"}
    if state == "recording":
        text = f" {label} "
    else:
        text = f" {SPINNER_FRAMES[spinnerIdx]} {label} "
    return {"text": text, "class": cssClass}


def main():
    sys.stdout.reconfigure(line_buffering=True)
    state, cssClass, label = ("", "", "")
    spinnerIdx = 0
    lastPoll = 0.0
    lastText = None

    while True:
        now = time.monotonic()
        if now - lastPoll >= POLL_INTERVAL:
            state, cssClass, label = currentState()
            lastPoll = now

        payload = render(state, cssClass, label, spinnerIdx)
        # Only emit when the rendered text actually changes -- keeps waybar reads
        # to one object each and avoids needless repaints.
        if payload["text"] != lastText:
            emit(payload)
            lastText = payload["text"]

        if state == "idle":
            # Nothing animating: sleep until the next poll, no spinner churn.
            time.sleep(POLL_INTERVAL)
            lastPoll = 0.0
        else:
            spinnerIdx = (spinnerIdx + 1) % len(SPINNER_FRAMES)
            time.sleep(SPINNER_INTERVAL)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(0)
