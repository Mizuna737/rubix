#!/usr/bin/env python3
"""Waybar module: focused window class (centered).

Port of the AwesomeWM wibar `focused_window_class`. Second subscriber to the
Rubix IPC socket (the socket supports many); emits just the focused window's
short app_id for the bar's center slot. Shares the connect/coalesce discipline
of rubixBar.py."""

import glob
import json
import os
import select
import socket
import sys
import time


def socketPath():
    runtimeDir = os.environ.get("XDG_RUNTIME_DIR")
    if not runtimeDir:
        return None
    plain = os.path.join(runtimeDir, "rubix.sock")
    if os.path.exists(plain):
        return plain
    candidates = glob.glob(os.path.join(runtimeDir, "rubix-*.sock"))
    return max(candidates, key=os.path.getmtime) if candidates else plain


def shortAppId(window):
    appId = window.get("app_id")
    if appId:
        leaf = appId.rsplit(".", 1)[-1]
        return leaf or appId
    title = window.get("title")
    if title:
        parts = title.split()
        return parts[0] if parts else title
    return "—"


def focusedWindow(snapshot):
    for column in snapshot.get("columns", []):
        for group in column.get("groups", []):
            for window in group.get("windows", []):
                if window.get("focused"):
                    return window
    return None


def emit(text):
    sys.stdout.write(json.dumps({"text": text, "class": "focused" if text else "empty"}) + "\n")
    sys.stdout.flush()


def streamSnapshots(sock, lastText):
    sock.setblocking(False)
    buffer = b""
    while True:
        select.select([sock], [], [], None)
        try:
            chunk = sock.recv(65536)
        except BlockingIOError:
            continue
        if not chunk:
            return lastText
        buffer += chunk
        while select.select([sock], [], [], 0.03)[0]:
            try:
                extra = sock.recv(65536)
            except BlockingIOError:
                break
            if not extra:
                return lastText
            buffer += extra
        latest = None
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
                latest = message
        if latest is not None:
            window = focusedWindow(latest)
            text = shortAppId(window) if window else ""
            if text != lastText:
                emit(text)
                lastText = text


def main():
    sys.stdout.reconfigure(line_buffering=True)
    lastText = None
    while True:
        sock = None
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.connect(socketPath())
            sock.sendall(b'{"type":"subscribe"}\n')
            lastText = streamSnapshots(sock, lastText)
        except (FileNotFoundError, ConnectionRefusedError, OSError):
            pass
        finally:
            if sock is not None:
                sock.close()
        if lastText != "":
            emit("")
            lastText = ""
        time.sleep(2)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(0)
