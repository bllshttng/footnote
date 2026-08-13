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
    # --- Leaves with zero callers anywhere a contributor reads, confirmed by a
    # second whole-repo walk sharing no code with `verb-callers.py --dead`. A
    # zero is a candidate, not a ruling: `loops resume-all` also scored zero and
    # STAYS, because it is the only release for the `pause-all` kill switch and
    # deleting it would strand a paused fleet.
    "agents mcp": (
        "`fno mcp` - the same Typer app. `agents mcp` was a second registration "
        "of it, and the Rust daemon's deliver_envelope shells the top-level path"
    ),
    # Removed from BOTH fronts (Python routing table and client.rs dispatch).
    # Half-removing would have left `fno agents push-channel` auto-routing to
    # the binary while leaving the enumerated surface, which is the invisible
    # live verb this whole node exists to make impossible.
    "agents register-channel": (
        "nothing - the channel CLI wrappers had no caller. The `channel.*` "
        "daemon RPCs they fronted are live and untouched; the MCP sidecar "
        "(`fno.mcp.sidecar`) speaks them directly, which is the real path"
    ),
    "agents unregister-channel": (
        "nothing - see `fno agents register-channel`; the `channel.*` RPC is live"
    ),
    "agents push-channel": (
        "nothing - see `fno agents register-channel`; the `channel.*` RPC is live"
    ),
    "backlog backfill-slugs": (
        "nothing - the one-shot migration that gave every node a slug has run. "
        "`fno backlog idea` assigns a slug at creation"
    ),
    "backlog briefs": (
        "nothing - the last-N listing had no reader. The briefs themselves are "
        "files under `paths.briefs_dir()` (`~/.fno/briefs/<id>.md`)"
    ),
    "backlog exec-graph": (
        "nothing - the derived execution graph had no reader outside its own "
        "compile step, which still runs"
    ),
    "backlog release": "`fno backlog unclaim` - `release` was an alias for it",
    "backlog schedule": (
        "nothing - the shadow-first frontier report never left shadow mode and "
        "no gate ever read its exit code. `fno backlog next` picks work"
    ),
    "backlog tree": "`fno backlog view` - the board view nothing had switched off",
    "backlog validate": (
        "nothing - the unknown-blocker and cycle checks had no caller, in CI or "
        "out. `fno backlog maintain` is the live hygiene pass but does NOT "
        "check blocker integrity, and `backlog triage validate` validates a "
        "proposal file rather than the graph"
    ),
    "backlog batch abandon": (
        "`fno backlog batch ship` - it is the documented failure path and "
        "already requeues members as individual PRs; `abandon` was wired to "
        "nothing despite its docstring naming the daemon"
    ),
    "backlog batch close": (
        "`fno backlog batch ship` - the only close path the daemon takes"
    ),
    "backlog batch policy": (
        "nothing - the selection wiring that was to shell this verb never "
        "landed. `fno backlog next` decides what runs"
    ),
    "backlog triage pile": (
        "`fno backlog view` - the rendered board carries the deferred nodes. "
        "`triage health` does NOT: it reports collisions, stale, "
        "failure-prone, and the idea pile, not the deferred pile"
    ),
    "claim incarnation-fence": (
        "`fno claim status <key>` - it reports the live holder, which is the "
        "only ownership truth the fence was checking"
    ),
    "claim lane-count": (
        "`fno claim list` - it shows the live claims this counted"
    ),
    "resume receipt version": (
        "nothing - the schema version had no reader; "
        "`fno resume receipt validate` fails on a version it cannot read"
    ),
    "runtime probe": "`fno doctor` - the live environment-health check",
    "runtime reap-dead-workers": (
        "`fno agents reap` - the live dead-row GC, and the same sweep the "
        "daemon runs on its idle tick"
    ),
    "state list-fields": (
        "nothing - the field list had no reader; the schema is "
        "`cli/src/fno/schemas`"
    ),
    "state path": (
        "nothing - it resolved only `ledger`, and nothing called it. "
        "`fno.paths.ledger_json()` is the in-process resolver"
    ),
    "state validate": (
        "nothing - `fno state set` validates on write, which is where an "
        "invalid field is actually stopped"
    ),
    "stub-manifest check-pr": (
        "`fno stub-manifest reconcile-validate` - the drift gate with callers"
    ),
    "stub-manifest validate": (
        "`fno stub-manifest reconcile-validate` - it validates the manifest as "
        "part of the check that runs"
    ),
    "target resume-bind": (
        "`fno target init` - it acquires the node claim against the live "
        "session pid, which is the rebinding this verb did separately"
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
