#!/usr/bin/env python3
"""Status line wrapper: the user's own status line on the left, the buddy flush right.

The buddy mod copies this file into its state folder and points statusLine at
the copy. It reads only files the mod wrote beside it, so it stays fast.
"""
import json
import os
import re
import subprocess
import sys
import time
import unicodedata

HOME = os.path.dirname(os.path.abspath(__file__))
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\]8;[^\x07\x1b]*(?:\x07|\x1b\\)")
MIN_ROWS, MAX_ROWS = 4, 6
# A frame older than this belongs to a session that stopped drawing.
STALE_S = 30
# Claude Code trims a row's leading spaces; a braille blank holds the column.
LEAD = "⠀"
COLORS = {"gray": 90, "green": 32, "cyan": 36, "magenta": 35, "yellow": 33}


def width(s):
    return sum(2 if unicodedata.east_asian_width(c) in "WF" else 1 for c in ANSI.sub("", s))


def cut(s, n):
    out, w = "", 0
    for c in s:
        cw = 2 if unicodedata.east_asian_width(c) in "WF" else 1
        if w + cw > n:
            return out[:-1] + "…" if out else ""
        out, w = out + c, w + cw
    return out


def inner_rows(stdin, data):
    try:
        with open(os.path.join(HOME, "inner.json"), encoding="utf-8") as f:
            inner = json.load(f).get("statusLine") or {}
    except (OSError, ValueError):
        inner = {}
    command = inner.get("command") if inner.get("type") == "command" else None
    if command:
        try:
            out = subprocess.run(command, shell=True, input=stdin, capture_output=True, text=True, timeout=5).stdout
        except Exception:
            out = ""
        return out.rstrip("\n").split("\n") if out.strip() else []
    return [default_left(data)]


def default_left(data):
    """A plain left side for someone who had no status line."""
    parts = [(data.get("model") or {}).get("display_name") or ""]
    cwd = (data.get("workspace") or {}).get("current_dir") or data.get("cwd") or ""
    if cwd:
        parts.append(os.path.basename(cwd))
    used = (data.get("context_window") or {}).get("used_percentage")
    if used is not None:
        parts.append(f"ctx {round(used)}%")
    cost = (data.get("cost") or {}).get("total_cost_usd")
    if cost:
        parts.append(f"${cost:.2f}")
    return " · ".join(p for p in parts if p)


def read_frame(session):
    try:
        with open(os.path.join(HOME, "frames", f"{session}.json"), encoding="utf-8") as f:
            frame = json.load(f)
    except (OSError, ValueError):
        return None
    if time.time() - frame.get("at", 0) / 1000 > STALE_S:
        return None
    return frame


def layout(left, frame, cols):
    """Returns the rows to print. Inner text stays on top; the buddy stands on the bottom row."""
    if not frame:
        return left
    color = f"\x1b[{COLORS.get(frame.get('color'), 90)}m"
    art = [r.rstrip() for r in frame.get("sprite", [])]
    while art and not art[0].strip():
        art.pop(0)
    art = art[-(MAX_ROWS - 1):]
    aw = max([width(a) for a in art] + [len(frame.get("name", ""))])
    art.append(frame.get("name", "").center(aw).rstrip())
    # Speech sits beside the body, the fleet line beside the name.
    labels = [""] * len(art)
    labels[-1] = frame.get("fleet") or ""
    if len(art) > 1:
        labels[-2] = frame.get("speech") or ""

    for rows in range(max(len(left), len(art), MIN_ROWS), MAX_ROWS + 1):
        lefts = left + [""] * (rows - len(left))
        top = rows - len(art)
        if all(width(lefts[top + i]) + aw + 2 <= cols for i in range(len(art))):
            out = lefts[:top]
            for i, a in enumerate(art):
                l = lefts[top + i] or LEAD
                free = cols - width(l) - aw - 3
                label = cut(labels[i], free) if free > 3 else ""
                right = (label + " " if label else "") + a.ljust(aw)
                out.append(l + " " * (cols - width(l) - width(right)) + color + right + "\x1b[0m")
            return out

    # Too wide to share any 6-row stack: the one-line face on the first row that fits.
    face = f"{frame.get('face', '')} {frame.get('name', '')}"
    if frame.get("speech"):
        face += f": {frame['speech']}"
    lefts = left + ([""] if len(left) < MAX_ROWS else [])
    for i in range(len(lefts) - 1, -1, -1):
        l = lefts[i] or LEAD
        free = cols - width(l) - 2
        if free >= width(frame.get("face", "")) + 1:
            right = cut(face, free)
            lefts[i] = l + " " * (cols - width(l) - width(right)) + color + right + "\x1b[0m"
            return lefts if lefts[-1] else lefts[:-1]
    return left


def main():
    stdin = sys.stdin.read()
    try:
        data = json.loads(stdin)
    except ValueError:
        data = {}
    session = data.get("session_id", "")
    # The heartbeat tells the mod this session's status line runs the wrapper, whichever settings file set it.
    if session:
        frames = os.path.join(HOME, "frames")
        seen = os.path.join(frames, f"{session}.seen")
        try:
            os.makedirs(frames, exist_ok=True)
            # A session's first run sweeps files that no session has touched for a day.
            if not os.path.exists(seen):
                cutoff = time.time() - 86400
                for entry in os.scandir(frames):
                    if entry.stat().st_mtime < cutoff:
                        os.unlink(entry.path)
            with open(seen, "w") as f:
                f.write(str(int(time.time() * 1000)))
        except OSError:
            pass
    left = inner_rows(stdin, data)
    # Claude Code's usable status width runs a few columns under COLUMNS.
    cols = int(os.environ.get("COLUMNS") or 120) - 4
    print("\n".join(layout(left, read_frame(session), cols)))


if __name__ == "__main__":
    main()
