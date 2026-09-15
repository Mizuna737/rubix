#!/usr/bin/env python3
"""Waybar module: workAssistant recording status.

Polls the recording PID marker every POLL_INTERVAL, checking process
liveness (not just file existence) so a marker left behind by a crashed
recordToggle.sh run doesn't stick forever. "Transcribing" moved to
vramArbiterBar.py once workAssistant's transcription paths started
registering with vramArbiter. "Notes" was dropped: meetingIngest.py's lock
file is deliberately never deleted after use (avoids an fcntl inode race
for LOCK_EX waiters), so file-existence polling can never reflect its real
state -- it was stuck showing "Notes" permanently as a result."""

import json
import os
import sys
import time
from pathlib import Path

PID_FILE = Path("/tmp/workAssistant-record.pid")
POLL_INTERVAL = 2.0


def isRecording() -> bool:
    if not PID_FILE.exists():
        return False
    try:
        pid = int(PID_FILE.read_text().split()[0])
    except (ValueError, IndexError, OSError):
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True  # process exists, just not signalable by us -- still alive
    return True


def emit(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()


def render(recording: bool) -> dict:
    if recording:
        return {"text": " ● REC ", "class": "recording"}
    return {"text": "", "class": "idle"}


def main():
    sys.stdout.reconfigure(line_buffering=True)
    lastText = None
    while True:
        payload = render(isRecording())
        if payload["text"] != lastText:
            emit(payload)
            lastText = payload["text"]
        time.sleep(POLL_INTERVAL)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(0)
