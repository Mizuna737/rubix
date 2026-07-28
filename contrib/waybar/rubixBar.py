#!/usr/bin/env python3
"""Waybar bridge for the Rubix compositor IPC socket.

Runs as a waybar `custom` module in continuous mode (`return-type: "json"`):
stays alive, subscribes to Rubix's IPC socket, and prints one waybar JSON line
per cube-state change. Waybar re-renders on each line.

Rendering: "column pips + focus title".
  - One pip per column, 1-based. Empty columns show just the number; occupied
    non-active columns show `<n>:<count>`; the active column shows
    `<n>:<app>` with the focused window's short app_id, wrapped in pango markup
    so it stands out (waybar gives a custom module only one CSS class for the
    whole strip, so per-pip emphasis has to live in the text as markup).
  - The focused window's title trails the pips.

Resilience: if the socket is missing (rubix not up yet) or the connection drops
(rubix restarted), emit a muted placeholder and keep retrying -- a waybar module
that exits would leave a dead slot in the bar.
"""

import glob
import json
import os
import select
import socket
import sys
import time

# Pango colors for the active pip / focused title. Tweak in one place; the CSS
# handles the rest of the module chrome.
ACTIVE_COLOR = "#8ec07c"
TITLE_COLOR = "#928374"


def socketPath():
    """Locate the Rubix socket. Prefer the display-agnostic `rubix.sock`, else
    the most recently modified `rubix-<n>.sock` (future-proofs the naming)."""
    runtimeDir = os.environ.get("XDG_RUNTIME_DIR")
    if not runtimeDir:
        return None
    plain = os.path.join(runtimeDir, "rubix.sock")
    if os.path.exists(plain):
        return plain
    candidates = glob.glob(os.path.join(runtimeDir, "rubix-*.sock"))
    if not candidates:
        # Return the plain path anyway so connect() fails cleanly and we retry.
        return plain
    return max(candidates, key=os.path.getmtime)


def shortAppId(appId, title, windowId):
    """Collapse a reverse-DNS app_id to its leaf (`org.mozilla.zen` -> `zen`).
    Fall back to the first word of the title, then the raw window id."""
    if appId:
        leaf = appId.rsplit(".", 1)[-1]
        return leaf or appId
    if title:
        return title.split()[0] if title.split() else title
    return f"#{windowId}"


def escapePango(text):
    """Minimal pango/markup escaping for text embedded in waybar spans."""
    return (
        text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
    )


def focusedWindow(snapshot):
    """Return the focused WindowView dict, or None."""
    for column in snapshot.get("columns", []):
        for group in column.get("groups", []):
            for window in group.get("windows", []):
                if window.get("focused"):
                    return window
    return None


def columnWindows(column):
    """Flatten a column's groups into a single window list."""
    windows = []
    for group in column.get("groups", []):
        windows.extend(group.get("windows", []))
    return windows


def renderPips(snapshot):
    """Build the pango-markup pip string for `text`."""
    active = snapshot.get("active_column", 0)
    pips = []
    for index, column in enumerate(snapshot.get("columns", [])):
        windows = columnWindows(column)
        label = str(index + 1)
        if index == active:
            focused = next((w for w in windows if w.get("focused")), None)
            source = focused or (windows[0] if windows else None)
            if source is not None:
                app = shortAppId(
                    source.get("app_id"), source.get("title"), source.get("id")
                )
                label = f"{index + 1}:{app}"
            pip = f"<span foreground='{ACTIVE_COLOR}' weight='bold'>▶{escapePango(label)}</span>"
        else:
            if windows:
                label = f"{index + 1}:{len(windows)}"
            pip = escapePango(label)
        pips.append(f"[ {pip} ]")
    return "".join(pips) if pips else "[ – ]"


def renderTooltip(snapshot):
    """Per-column breakdown with window app_ids/titles."""
    active = snapshot.get("active_column", 0)
    total = len(snapshot.get("columns", []))
    lines = [
        f"columns: {total}  on-face: {snapshot.get('visible_columns', 0)}  active: {active + 1}",
    ]
    for index, column in enumerate(snapshot.get("columns", [])):
        marker = "▶" if index == active else " "
        windows = columnWindows(column)
        if not windows:
            lines.append(f"{marker} col {index + 1}: (empty)")
            continue
        parts = []
        for window in windows:
            app = shortAppId(
                window.get("app_id"), window.get("title"), window.get("id")
            )
            parts.append(f"*{app}" if window.get("focused") else app)
        lines.append(f"{marker} col {index + 1}: {' | '.join(parts)}")
    return escapePango("\n".join(lines))


def renderLine(snapshot):
    """Transform one Rubix snapshot into a waybar JSON payload."""
    focused = focusedWindow(snapshot)
    text = renderPips(snapshot)
    if focused and focused.get("title"):
        title = escapePango(focused["title"])
        text = f"{text}  <span foreground='{TITLE_COLOR}'>{title}</span>"
    return {
        "text": text,
        "tooltip": renderTooltip(snapshot),
        "class": "focused" if focused else "empty",
    }


def debugLog(message):
    """Append a diagnostic line when RUBIX_BAR_DEBUG points at a file. No-op
    otherwise. Lets us confirm waybar actually spawns and reads the bridge."""
    path = os.environ.get("RUBIX_BAR_DEBUG")
    if not path:
        return
    try:
        with open(path, "a") as handle:
            handle.write(f"{time.time():.3f} {message}\n")
    except OSError:
        pass


def emit(payload):
    debugLog(f"emit {json.dumps(payload)}")
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()


def emitPlaceholder():
    emit(
        {
            "text": "<span foreground='#928374'>rubix –</span>",
            "tooltip": "rubix IPC socket unavailable",
            "class": "disconnected",
        }
    )


def streamSnapshots(sock):
    """Read newline-delimited replies and emit ONE coalesced waybar line per
    readable batch. Returns (normally) when the connection closes.

    Waybar's continuous reader processes one line per buffer read, so if two
    JSON objects land in a single read it drops one and can stall the module.
    We defend against that: on each wakeup we drain everything currently
    available (plus a brief settle window to absorb a rapid burst), then emit
    only the LAST `state` snapshot. Server-side coalescing (ipc_dirty) already
    collapses most bursts; this makes it airtight from the client side too."""
    sock.setblocking(False)
    buffer = b""
    while True:
        # Block until the socket is readable (no busy-wait, no periodic churn).
        select.select([sock], [], [], None)
        try:
            chunk = sock.recv(65536)
        except BlockingIOError:
            continue
        if not chunk:
            return  # server closed
        buffer += chunk
        # Settle: soak up any near-simultaneous follow-up snapshots so a burst
        # collapses into a single emit rather than several back-to-back lines.
        while select.select([sock], [], [], 0.03)[0]:
            try:
                extra = sock.recv(65536)
            except BlockingIOError:
                break
            if not extra:
                return
            buffer += extra
        # Parse all complete lines; keep only the last state snapshot.
        lastSnapshot = None
        while b"\n" in buffer:
            rawLine, buffer = buffer.split(b"\n", 1)
            rawLine = rawLine.strip()
            if not rawLine:
                continue
            try:
                message = json.loads(rawLine)
            except json.JSONDecodeError:
                continue
            if message.get("type") == "state":
                lastSnapshot = message
        if lastSnapshot is not None:
            emit(renderLine(lastSnapshot))


def main():
    # Line-buffer stdout: waybar reads line-by-line, and an explicit flush isn't
    # always enough to push a line out of Python's text-layer buffer into the
    # pipe. Line buffering guarantees every "\n" reaches waybar immediately.
    sys.stdout.reconfigure(line_buffering=True)

    # Connect first; do NOT emit a placeholder up front. On the happy path
    # (rubix already running) waybar's very first read is a single snapshot
    # line -- never a placeholder+snapshot burst that waybar would mis-handle.
    while True:
        path = socketPath()
        sock = None
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.connect(path)
            sock.sendall(b'{"type":"subscribe"}\n')
            streamSnapshots(sock)
        except (FileNotFoundError, ConnectionRefusedError, OSError):
            pass
        finally:
            if sock is not None:
                sock.close()
        # Only when disconnected / never connected: one placeholder line, then
        # back off. The next real snapshot is >=2s away (reconnect), so it never
        # coalesces with this line in a single waybar read.
        emitPlaceholder()
        time.sleep(2)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(0)
