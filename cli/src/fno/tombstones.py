"""Removed verbs, and what replaced each one.

A verb that is deleted outright fails its callers with ``No such command
'inbox'.`` - true, useless, and identical to the error for a typo. The caller
learns that the verb is gone and nothing about where the capability went, so
every removal costs somebody a bisect or a docs crawl.

A tombstone is cheaper than a broken caller. Removing a verb means moving its
name into :data:`TOMBSTONES` with the replacement, not deleting the name
outright, and the refusal then teaches:

    $ fno backlog inbox list
    Error: `fno backlog inbox` was removed. Use `fno backlog capture` - it is
    the same command; `inbox` was a duplicate registration of the same app.

The table is keyed by the FULL verb path so ``backlog inbox`` and a
hypothetical top-level ``inbox`` never collide, and it is matched on the
longest prefix so one entry covers a whole removed subtree rather than one line
per leaf.

Tombstones are not baselined verbs. They resolve to a refusal, never to a
command, so they add nothing to the surface the ratchet counts - which is the
point: the name stays reachable as a signpost while the capability is gone.
"""

from __future__ import annotations

import click
import typer.core

TOMBSTONES: dict[str, str] = {
    "backlog inbox": (
        "`fno backlog capture` - the same command. `inbox` was a second "
        "registration of the identical Typer app, so every one of its nine "
        "subcommands was a duplicate of a `capture` subcommand"
    ),
}


def tombstone_for(path: str) -> tuple[str, str] | None:
    """``(removed path, replacement)`` for the verb path ``path``.

    Matched on longest prefix first, so a removed GROUP needs one entry rather
    than one per leaf, and a deeper tombstone inside a removed subtree wins over
    the shallower one.

    Then matched on SUFFIX, which is the part that is not obvious. A group is
    reachable two ways: through the root binary (``fno backlog inbox``, where
    the context knows the full path) and directly (``runner.invoke(graph_cli,
    ["inbox"])``, where it knows only ``inbox``, and under the group's internal
    name ``graph`` rather than its user-facing ``backlog``). Keying strictly on
    the absolute path guards the first path and silently misses the second -
    the guard-on-one-of-N-paths shape - which is how the first version of this
    passed a live CLI check and failed its own test.

    An ambiguous suffix (two removed verbs sharing a leaf name) resolves to
    None, so the caller falls back to the generic unknown-command error. A
    tombstone that names the wrong replacement is worse than none.
    """
    tokens = path.split()
    if not tokens:
        return None
    for n in range(len(tokens), 0, -1):
        key = " ".join(tokens[:n])
        if key in TOMBSTONES:
            return key, TOMBSTONES[key]
    suffix = tuple(tokens)
    hits = [k for k in TOMBSTONES if tuple(k.split())[-len(suffix):] == suffix]
    if len(hits) == 1:
        return hits[0], TOMBSTONES[hits[0]]
    return None


def refuse(path: str) -> click.UsageError:
    """The refusal a removed verb raises, naming its replacement."""
    hit = tombstone_for(path)
    if hit is None:  # pragma: no cover - callers check first
        return click.UsageError(f"No such command {path.split()[-1]!r}.")
    removed, replacement = hit
    return click.UsageError(f"`fno {removed}` was removed. Use {replacement}.")


class TombstoneGroup(typer.core.TyperGroup):
    """A group whose unknown-command error consults :data:`TOMBSTONES`.

    Subclasses ``TyperGroup``, not ``click.Group``: Typer asserts the ``cls=``
    it is handed is one of its own, and a plain Click group fails at import
    time rather than at the first unknown verb.

    Hooked at ``get_command`` rather than only at ``resolve_command``: both are
    reachable (``resolve_command`` on a real invocation, ``get_command`` from
    help rendering and from any tool that walks the tree), and a guard on one of
    two paths is the shape that lets a removal ship looking guarded. Returning
    ``None`` from ``get_command`` is what makes Click raise, so raising here
    turns the anonymous error into the named one on every path that resolves a
    child.
    """

    def get_command(self, ctx: click.Context, cmd_name: str) -> click.Command | None:
        cmd = super().get_command(ctx, cmd_name)
        if cmd is not None:
            return cmd
        full = f"{self._verb_path(ctx)} {cmd_name}".strip()
        if tombstone_for(full) is not None:
            raise refuse(full)
        return None

    @staticmethod
    def _verb_path(ctx: click.Context) -> str:
        """The verb path of ``ctx``, without the ``fno`` binary name.

        ``ctx.command_path`` is the invoked spelling, so it reads ``fno-py
        backlog`` under the Python entry point and ``fno backlog`` under the
        Rust front. Dropping the first token normalises both to the path the
        tombstone table is keyed by.
        """
        parts = (ctx.command_path or "").split()
        return " ".join(parts[1:])
