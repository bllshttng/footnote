"""Moved verb spellings: the old name works and points at the new one.

A move is not a break. The old spelling stays registered for one release,
forwards in-process to the new path, and prints one line to stderr naming
the new spelling - stderr, because callers parse stdout and a shim that
edits stdout breaks the parser it is trying to spare.

The forwarding in ``LazyTypeGroup.resolve_command`` is gated on the
destination root being registered, so a wave can seed this table before it
mints the destination; until then the old registration serves the call,
announced rather than silent.
"""

from __future__ import annotations

from collections.abc import Collection
from typing import NamedTuple


class Move(NamedTuple):
    """One moved top-level spelling.

    ``kind="deprecated"`` forwards, prints the new spelling to stderr, and
    is removed one release out. ``kind="alias"`` forwards silently and is
    permanent. ``silent_leaves`` carves hot leaves out of the deprecation
    print, so a busy spelling stays quiet while its siblings announce.

    NamedTuple rather than dataclass: this module imports on every
    invocation with arguments, ``typing`` is already warm there, and
    ``dataclasses`` is not (a cold ~0.5ms module load to consult a
    two-entry table). Frozen semantics and keyword construction are the
    same.
    """

    kind: str
    to: str
    silent_leaves: frozenset[str] = frozenset()


VERB_MOVES: dict[str, Move] = {
    "approvals": Move(kind="deprecated", to="inbox approvals"),
    "notify": Move(kind="deprecated", to="inbox notify"),
    "outstanding": Move(kind="deprecated", to="inbox outstanding"),
    "pr": Move(
        kind="deprecated",
        to="do pr",
        silent_leaves=frozenset({"status", "merge", "rebase"}),
    ),
}


def move_for(verb: str) -> Move | None:
    """The move registered for a top-level spelling, if any."""
    return VERB_MOVES.get(verb)


def destination_is_registered(move: Move, roots: Collection[str]) -> bool:
    """Whether a move's top-level destination exists in this registry."""
    return move.to.split(maxsplit=1)[0] in roots


def deprecation_line(verb: str, rest: list[str], move: Move) -> str | None:
    """The one stderr line a moved spelling prints, or None to stay silent.

    ``kind="alias"`` never prints. A ``deprecated`` entry whose first
    argument sits in ``silent_leaves`` never prints - that is how a hot
    leaf keeps its two-level spelling permanently and quietly. A
    deprecated entry that carries ``silent_leaves`` announces the
    leaf-qualified destination for every other first argument
    (``fno pr create`` -> ``fno do pr create``); an entry without them
    announces the bare destination, because its arguments are values
    (``fno inbox notify TITLE BODY``), not subcommands to teach.
    """
    if move.kind == "alias":
        return None
    if rest and rest[0] in move.silent_leaves:
        return None
    if move.silent_leaves and rest and not rest[0].startswith("-"):
        return f"fno {verb} {rest[0]} is now fno {move.to} {rest[0]}"
    return f"fno {verb} is now fno {move.to}"
