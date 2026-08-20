"""Read the operator's own ranked lane (``my-priorities.md``).

The single implementation. `fno king board` and `fno outstanding` both read
through this module, so a second parser anywhere in the tree is the drift
this file exists to prevent.

A MISSING file is an empty lane, not an error: most installs keep no lane,
and the correct steady state there is silence. A file that EXISTS and cannot
be read (``OSError``, ``UnicodeDecodeError``) sets ``error``. An unreadable
queue is not an empty one, and a reader that could not tell them apart would
call a broken file a clean lane.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from pathlib import Path

from fno.graph._constants import NODE_ID_BODY

_ITEM_RE = re.compile(r"^- \[( |x|X)\] (.*)$")

# One `->` suffix carries both resolutions, so there is one anchor to learn
# and one regex to read. `-> <node-id>` means filed, the same convention the
# capture inbox uses for a promoted row. `-> parked: <reason>` means parked.
# `\S` on the reason is load-bearing: a bare `-> parked:` would let a king
# silence any item by typing one word, turning the escape into a mute button.
_SUFFIX_RE = re.compile(
    r"->\s*(?:(?P<node>" + NODE_ID_BODY + r")|parked:\s*(?P<reason>\S.*?))\s*$"
)


@dataclass(frozen=True)
class LaneItem:
    text: str
    node: "str | None"
    parked: "str | None"
    done: bool
    line: int


@dataclass(frozen=True)
class LaneRead:
    items: "list[LaneItem]" = field(default_factory=list)
    error: str = ""
    path: "Path | None" = None


def _parse_line(raw: str, line_no: int) -> "LaneItem | None":
    m = _ITEM_RE.match(raw)
    if not m:
        return None
    done = m.group(1) != " "
    rest = m.group(2)
    node = None
    parked = None
    suffix_match = _SUFFIX_RE.search(rest)
    if suffix_match:
        node = suffix_match.group("node")
        parked = suffix_match.group("reason")
        rest = rest[: suffix_match.start()].rstrip()
    return LaneItem(text=rest.strip(), node=node, parked=parked, done=done, line=line_no)


def read_lane(path: "Path | None" = None) -> LaneRead:
    """Parse the lane file. Missing is empty; unreadable sets ``error``."""
    from fno import paths

    resolved = path if path is not None else paths.operator_lane()
    try:
        text = resolved.read_text(encoding="utf-8")
    except FileNotFoundError:
        return LaneRead(items=[], path=resolved)
    except (OSError, UnicodeDecodeError) as exc:
        return LaneRead(error=f"cannot read operator lane {resolved}: {exc}", path=resolved)

    items: "list[LaneItem]" = []
    for i, raw in enumerate(text.splitlines(), start=1):
        item = _parse_line(raw, i)
        if item is not None:
            items.append(item)
    return LaneRead(items=items, path=resolved)


def open_items(read: LaneRead) -> "list[LaneItem]":
    """Items with no linked node, no park reason, and not ticked."""
    return [i for i in read.items if not i.done and i.node is None and i.parked is None]


def parked_items(read: LaneRead) -> "list[LaneItem]":
    return [i for i in read.items if i.parked is not None]
