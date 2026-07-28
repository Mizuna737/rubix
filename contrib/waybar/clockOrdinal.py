#!/usr/bin/env python3
"""Waybar module: date + time with an ordinal day suffix.

Port of the AwesomeWM wibar date/time widget ("Monday, July 28th" + "01:23
PM"). strftime can't produce st/nd/rd/th, so this small persistent module
formats it and re-emits every second (cheap; the label only changes each
minute but a 1s tick keeps it crisp)."""

import json
import sys
import time
from datetime import datetime


def ordinal(day):
    if 11 <= day % 100 <= 13:
        return "th"
    return {1: "st", 2: "nd", 3: "rd"}.get(day % 10, "th")


def render(now):
    date = now.strftime("%A, %B ") + str(now.day) + ordinal(now.day)
    clock = now.strftime("%I:%M %p").lstrip("0")
    return {"text": f"{date}   {clock}", "tooltip": now.strftime("%Y-%m-%d %H:%M:%S")}


def emit(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()


def main():
    sys.stdout.reconfigure(line_buffering=True)
    lastText = None
    while True:
        payload = render(datetime.now())
        if payload["text"] != lastText:
            emit(payload)
            lastText = payload["text"]
        time.sleep(1)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(0)
