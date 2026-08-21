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
    permanent. ``kind="leaf-alias"`` is the post-expiry state: only entries
    in ``permanent_leaves`` forward, while retired siblings fail loud.

    NamedTuple rather than dataclass: this module imports on every
    invocation with arguments, ``typing`` is already warm there, and
    ``dataclasses`` is not (a cold ~0.5ms module load to consult a
    two-entry table). Frozen semantics and keyword construction are the
    same.
    """

    kind: str
    to: str
    permanent_leaves: frozenset[str] = frozenset()
    leaf_destinations: tuple[tuple[str, str], ...] = ()


VERB_MOVES: dict[str, Move] = {
    "approvals": Move(kind="deprecated", to="inbox approvals"),
    "autonomy": Move(kind="deprecated", to="agents autonomy"),
    "bundle": Move(kind="deprecated", to="doctor bundle"),
    "codemap": Move(kind="deprecated", to="doctor codemap"),
    "claim": Move(kind="deprecated", to="agents claim"),
    "delivery": Move(kind="deprecated", to="do delivery"),
    "dispatch": Move(kind="deprecated", to="agents dispatch"),
    "evals": Move(kind="deprecated", to="doctor evals"),
    "event": Move(kind="deprecated", to="doctor event"),
    "king": Move(
        kind="deprecated",
        to="agents king",
        leaf_destinations=(("board", "inbox board"),),
    ),
    "lint": Move(kind="deprecated", to="doctor lint"),
    "loops": Move(kind="deprecated", to="do loops"),
    "mail": Move(kind="deprecated", to="agents mail"),
    "mcp": Move(kind="deprecated", to="agents mcp"),
    "notify": Move(kind="deprecated", to="inbox notify"),
    "observer": Move(kind="deprecated", to="doctor observer"),
    "outstanding": Move(kind="deprecated", to="inbox outstanding"),
    "phase": Move(kind="deprecated", to="do phase"),
    "plan": Move(kind="deprecated", to="do plan"),
    "pr": Move(
        kind="deprecated",
        to="do pr",
        permanent_leaves=frozenset({"status", "merge", "rebase"}),
    ),
    "pr-watch": Move(kind="deprecated", to="do pr watch"),
    "research": Move(kind="deprecated", to="do research"),
    "restart": Move(kind="deprecated", to="agents restart"),
    "resume": Move(kind="deprecated", to="do resume"),
    "review": Move(kind="deprecated", to="do review"),
    "roles": Move(kind="deprecated", to="agents roles"),
    "state": Move(kind="deprecated", to="do state"),
    "status-fanout": Move(kind="deprecated", to="doctor event fanout"),
    "stub-manifest": Move(kind="deprecated", to="do pr stub-manifest"),
    "skill-diff": Move(kind="deprecated", to="doctor skill-diff"),
    "target": Move(kind="deprecated", to="do target"),
    "test": Move(kind="deprecated", to="doctor test"),
    "think": Move(kind="deprecated", to="do think"),
    "update": Move(kind="deprecated", to="doctor update"),
    "worker": Move(kind="deprecated", to="agents worker"),
}


def move_for(verb: str) -> Move | None:
    """The move registered for a top-level spelling, if any."""
    return VERB_MOVES.get(verb)


def _destination(rest: list[str], move: Move) -> str:
    if rest:
        for leaf, destination in move.leaf_destinations:
            if rest[0] == leaf:
                return destination
    return move.to


def destination_is_registered(
    move: Move, roots: Collection[str], rest: list[str] | None = None
) -> bool:
    """Whether a move's top-level destination exists in this registry."""
    return _destination(rest or [], move).split(maxsplit=1)[0] in roots


def forwarding_args(rest: list[str], move: Move) -> list[str] | None:
    """Destination argv, or None when a post-expiry sibling is retired."""
    if move.kind == "leaf-alias" and (not rest or rest[0] not in move.permanent_leaves):
        return None
    destination = _destination(rest, move)
    residual = rest[1:] if rest and destination != move.to else rest
    return [*destination.split(), *residual]


def deprecation_line(verb: str, rest: list[str], move: Move) -> str | None:
    """The one stderr line a moved spelling prints, or None to stay silent.

    ``kind="alias"`` and ``kind="leaf-alias"`` never print. A ``deprecated`` entry whose first
    argument sits in ``permanent_leaves`` never prints - that is how a hot
    leaf keeps its two-level spelling permanently and quietly. A
    deprecated entry that carries ``permanent_leaves`` announces the
    leaf-qualified destination for every other first argument
    (``fno pr create`` -> ``fno do pr create``); an entry without them
    announces the bare destination, because its arguments are values
    (``fno inbox notify TITLE BODY``), not subcommands to teach.
    """
    if move.kind in {"alias", "leaf-alias"}:
        return None
    if rest and rest[0] in move.permanent_leaves:
        return None
    destination = _destination(rest, move)
    if destination != move.to:
        return f"fno {verb} {rest[0]} is now fno {destination}"
    if move.permanent_leaves and rest and not rest[0].startswith("-"):
        return f"fno {verb} {rest[0]} is now fno {move.to} {rest[0]}"
    return f"fno {verb} is now fno {move.to}"
