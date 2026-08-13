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

The table is keyed by the FULL verb path, matched on longest prefix, so one
entry covers a whole removed subtree rather than one line per leaf. Every
lookup is exact: a group is handed its own user-facing path by
:func:`tombstone_group_cls` rather than inferring one, because two rounds of
inference each produced a tombstone that answered confidently for a verb
nobody typed.

Tombstones are not baselined verbs. They resolve to a refusal, never to a
command, so they add nothing to the surface the ratchet counts - which is the
point: the name stays reachable as a signpost while the capability is gone.
"""

from __future__ import annotations

import click
import typer.core

TOMBSTONES: dict[str, str] = {
    # Four top-level groups whose every leaf scored zero in `verb-callers.py
    # --dead`: no skill, doc, script, hook, or cli/src argv named any of them.
    # Confirmed by a second whole-repo walk sharing no code with that tool.
    # The implementation packages stay where live code imports them
    # (`fno.company.contracts` and `fno.wake.signal` have many importers); what
    # is gone is the invocable surface.
    "company": (
        "nothing - the company campaign verbs had no caller anywhere. "
        "`fno.company.contracts` is still imported by the graph, plan, and "
        "delivery code; only the CLI surface was removed"
    ),
    "log": (
        "`fno event emit` - the per-worktree progress log had no reader and no "
        "caller outside its own test"
    ),
    "reality-check": (
        "nothing - the external-reality probes had no caller. The `gh` probe's "
        "job is done by `fno pr status <n>`"
    ),
    "wake": (
        "nothing - the wake-signal ADMIN verbs had no caller. Wake signals "
        "themselves are still written and read by `fno.wake.signal`, which the "
        "inbox drain and mail paths use"
    ),
    "backlog inbox": (
        "`fno backlog capture` - the same command. `inbox` was a second "
        "registration of the identical Typer app, so every one of its nine "
        "subcommands was a duplicate of a `capture` subcommand"
    ),
}


def tombstone_for(path: str) -> tuple[str, str] | None:
    """``(removed path, replacement)`` for the verb path ``path``.

    Matched on longest PREFIX only, so a removed GROUP needs one entry rather
    than one per leaf, and a deeper tombstone inside a removed subtree wins over
    the shallower one. There is no suffix matching, and it took three tries to
    land on that.

    A group is reachable two ways: through the root binary, where the context
    knows the full path, and directly (``runner.invoke(graph_cli, ["inbox"])``),
    where it knows only the leaf, under the group's INTERNAL name ``graph``
    rather than its user-facing ``backlog``. Keying on ``ctx.command_path``
    guarded the first and silently missed the second. Falling back to a suffix
    match fixed that and broke something worse: a bare ``fno inbox`` matched the
    deeper ``backlog inbox`` key and answered confidently about a removal that
    had nothing to do with the name typed, and one level down ``fno backlog
    log`` answered for the top-level ``log`` removal.

    Both hijacks are one defect - guessing a path instead of knowing it - so the
    fix is to know it. :func:`tombstone_group_cls` bakes the group's real
    user-facing prefix into the class, and every lookup here is exact.
    """
    tokens = path.split()
    if not tokens:
        return None
    for n in range(len(tokens), 0, -1):
        key = " ".join(tokens[:n])
        if key in TOMBSTONES:
            return key, TOMBSTONES[key]
    return None


def refuse(path: str) -> click.UsageError:
    """The refusal a removed verb raises, naming its replacement."""
    hit = tombstone_for(path)
    if hit is None:  # pragma: no cover - callers check first
        return click.UsageError(f"No such command {path.split()[-1]!r}.")
    removed, replacement = hit
    # A verb with a successor says "Use X"; one whose capability simply went
    # away says so directly. "Use nothing - ..." is the kind of sentence that
    # makes a reader think the tool is broken rather than the verb gone.
    lead = "" if replacement.startswith("nothing") else "Use "
    return click.UsageError(f"`fno {removed}` was removed. {lead}{replacement}.")


class TombstoneGroup(typer.core.TyperGroup):
    """A group whose unknown-command error consults :data:`TOMBSTONES`.

    Subclasses ``TyperGroup``, not ``click.Group``: Typer asserts the ``cls=``
    it is handed is one of its own, and a plain Click group fails at import time
    rather than at the first unknown verb.

    ``verb_prefix`` is the group's USER-FACING path, baked in by
    :func:`tombstone_group_cls`. It is not read from the context, because the
    context does not reliably know it: a direct ``runner.invoke`` sees only the
    leaf, and ``fno backlog``'s own Typer name is ``graph``. Every attempt to
    infer it produced a tombstone that answered for a verb nobody typed.

    Hooked at ``get_command`` rather than only at ``resolve_command``: both are
    reachable (``resolve_command`` on a real invocation, ``get_command`` from
    help rendering and from any tool that walks the tree), and a guard on one of
    two paths is the shape that lets a removal ship looking guarded.
    """

    verb_prefix: str = ""

    def get_command(self, ctx: click.Context, cmd_name: str) -> click.Command | None:
        cmd = super().get_command(ctx, cmd_name)
        if cmd is not None:
            return cmd
        full = f"{self.verb_prefix} {cmd_name}".strip()
        if tombstone_for(full) is not None:
            raise refuse(full)
        return None


def tombstone_group_cls(verb_prefix: str) -> type[TombstoneGroup]:
    """A :class:`TombstoneGroup` that knows its own user-facing verb path.

    Typer instantiates ``cls=`` with its own kwargs only, so the prefix cannot
    be passed at construction. Baking it into a subclass is the same trick
    ``make_lazy_group_cls`` uses for the root's lazy map.
    """

    class _Cls(TombstoneGroup):
        pass

    _Cls.verb_prefix = verb_prefix
    _Cls.__name__ = f"TombstoneGroup_{verb_prefix or 'root'}"
    return _Cls
