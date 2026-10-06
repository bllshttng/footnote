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
MIN_ROWS, MAX_ROWS = 3, 6
# Under this many columns the original buddy showed a one-line face.
NARROW = 60
# The original speech bubble held about 30 columns of text.
BUBBLE_W = 30
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


def bubble(text, room, widest=BUBBLE_W):
    """A thought bubble over `room` rows, bottom-aligned, its trail of dots pointing at the buddy.

    Three rows or more get the outline; fewer get the bare words.
    """
    # The outline needs its full width; a terminal too narrow for it gets the bare words.
    framed = room >= 3 and widest >= BUBBLE_W + 4
    rows = room - 2 if framed else room
    if rows < 1 or not text.strip():
        return [""] * room
    # Start at the original's 30 columns and widen until the words fit the rows beside the buddy.
    w = min(BUBBLE_W, max(widest, 8))
    lines = wrap(text, w)
    while len(lines) > rows and w < widest:
        w = min(widest, w + 4)
        lines = wrap(text, w)
    if len(lines) > rows:
        lines = lines[: rows - 1] + [cut(" ".join(lines[rows - 1 :]), w)]
    bw = max(width(line) for line in lines)
    body = [line + " " * (bw - width(line)) for line in lines]
    if framed:
        body = ["╭" + "─" * (bw + 2) + "╮"] + [f"│ {b} │" for b in body] + ["╰" + "─" * (bw + 2) + "╯ ◦ ·"]
        body = [b if i == len(body) - 1 else b + "    " for i, b in enumerate(body)]
    return [""] * (room - len(body)) + body


def wrap(text, n):
    """Greedy word wrap by terminal cells, so full-width text stays inside the bubble."""
    lines, line = [], ""
    for word in text.split():
        while width(word) > n:
            if line:
                lines.append(line)
                line = ""
            head = cut(word, n + 1)[:-1]  # the longest prefix that fits n cells
            lines.append(head)
            word = word[len(head):]
        if line and width(line) + 1 + width(word) > n:
            lines.append(line)
            line = word
        else:
            line = f"{line} {word}" if line else word
    if line:
        lines.append(line)
    return lines


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
    if cols + 4 < NARROW:
        return face_row(left, frame, cols)
    color = f"\x1b[{COLORS.get(frame.get('color'), 90)}m"
    art = [r.rstrip() for r in frame.get("sprite", [])]
    while art and not art[0].strip():
        art.pop(0)
    name = frame.get("name", "")
    fleet = frame.get("fleet") or ""
    # Like the original: the full sprite with its name row below, at 60 columns or more.
    aw = max([width(a) for a in art] + [len(name)])
    art.append(name.center(aw).rstrip())
    # Speech wraps beside the body like the original bubble, bottom-aligned; the fleet line sits beside the name row.
    labels = [""] * len(art)
    labels[-1] = fleet
    # The bubble may widen into the room the user's rows leave free beside the buddy.
    # The bubble may take the whole width beside the buddy, over the user's rows, while it shows.
    widest = min(BUBBLE_W * 2, cols - aw - 12)
    for i, line in enumerate(bubble(frame.get("speech") or "", len(art) - 1, widest)):
        labels[i] = line

    least = max(len(left), len(art), MIN_ROWS)
    if least > MAX_ROWS:
        return face_row(left, frame, cols)
    # Prefer a row count where the user's rows fit whole; else keep the full buddy and cut the rows beside it.
    tries = [*range(least, MAX_ROWS + 1), least]
    for n, rows in enumerate(tries):
        lefts = left + [""] * (rows - len(left))
        top = rows - len(art)
        if n == len(tries) - 1 or all(width(lefts[top + i]) + aw + 2 <= cols for i in range(len(art))):
            out = lefts[:top]
            for i, a in enumerate(art):
                l = lefts[top + i] or LEAD
                label = cut(labels[i], cols - aw - 4)
                # Only the buddy wears its rarity color; its words use the terminal's own text color.
                words = label + " " if label else ""
                # While the buddy talks, its bubble covers the user's row; the row comes back when it fades.
                room = cols - width(words) - aw - 1
                if width(l) > room:
                    l = cut(ANSI.sub("", l), room) if room > 1 else LEAD
                pad = cols - width(l) - width(words) - aw
                out.append(l + " " * pad + words + color + a.ljust(aw) + "\x1b[0m")
            return out

    return face_row(left, frame, cols)


def face_row(left, frame, cols):
    """The one-line face on the lowest row that has room."""
    color = f"\x1b[{COLORS.get(frame.get('color'), 90)}m"
    face = f"{frame.get('face', '')} {frame.get('name', '')}"
    if frame.get("speech"):
        face += f": {frame['speech']}"
    lefts = left + ([""] if len(left) < MAX_ROWS else [])
    for i in range(len(lefts) - 1, -1, -1):
        l = lefts[i] or LEAD
        free = cols - width(l) - 2
        if free >= width(frame.get("face", "")) + 1:
            right = cut(face, free)
            head = cut(f"{frame.get('face', '')} {frame.get('name', '')}", free)
            lefts[i] = l + " " * (cols - width(l) - width(right)) + color + head + "\x1b[0m" + right[len(head):]
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
