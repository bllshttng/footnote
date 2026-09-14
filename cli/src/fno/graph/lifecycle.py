"""Node-lifecycle verbs: defer, retract, undefer, and unsupersede.

tracker-owned machinery: every entry path is a tracker-owned registered
verb, so this module's graph reads are those verbs' own orchestration.

Extracted from graph/cli.py under the file-budget ratchet: the verbs a
session calls to make or reverse a park or a supersession live here,
registered into the backlog app by :func:`register_lifecycle_commands`. The
helpers they share with the rest of the backlog surface are injected at
registration, so this module never imports graph.cli (the cycle would be
unimportable).

x-665f: these verbs are TRANSPORTS over the native patch door
(`fno-agents backlog-update`). They keep only what is not store logic: the
batch atomicity pre-check, the dependents WARN, the boundary events, the
plan-ruling lines, and the plan projection. Every field write rides the
door, so a receipt can only describe what the store actually committed.
"""

from __future__ import annotations

from pathlib import Path
from typing import Callable, List, Optional

import typer


def _door(task_id: str, args: List[str], graph_path: Callable[[], Path]):
    """One round through the patch door; `(exit, receipt)` back."""
    from fno.graph.note_cli import native_update

    return native_update(task_id, args, graph_path=graph_path())


#: The legacy cmd_update field flags that cannot ride one call with the door
#: flags: the two write paths validate differently, and a mixed call could not
#: say which rules it asked for. Any param whose parsed value differs from its
#: unset sentinel (None, or False for the two pure switches) counts as passed.
_LEGACY_UPDATE_PARAMS = (
    "locked_by", "locked_by_harness", "locked_by_harness_session", "has_brief",
    "plan_path", "pr_number", "pr_url", "repo", "priority", "blocks_everything",
    "title", "details", "details_file", "domain", "size", "difficulty", "model",
    "_model_tier_tombstone", "batch", "orphan_ok", "dispatch_verb",
    "dispatch_brief", "type_", "public", "project", "cwd", "source_node",
    "related", "blocked_by", "add_blocker", "remove_blocker",
    "acknowledge_collisions", "parent", "completion_note", "add_pr",
    "add_pr_url", "add_pr_note", "remove_pr", "caused_by", "fixes_pr",
    "reverted", "tag", "untag", "force",
)

#: The spellings the mechanical `--kebab-of-the-param` rule cannot produce.
_FLAG_SPELLING_EXCEPTIONS = {"_model_tier_tombstone": "--model-tier", "type_": "--type"}


def _flag_spelling(param: str) -> str:
    return _FLAG_SPELLING_EXCEPTIONS.get(param, "--" + param.replace("_", "-"))


#: The door flags `fno backlog update` carries as extra args. ONE copy: the
#: stray refusal and cmd_update's forward condition both read this.
DOOR_FLAGS = ("--status", "--leave", "--set")


def refuse_stray_update_flags(door_args: List[str]) -> None:
    """Extra args may only be door flags: ``ignore_unknown_options`` would
    revive a retired spelling (``--completed``) as a silent no-op."""
    strays = [a for a in door_args if a.startswith("-") and a.split("=", 1)[0] not in DOOR_FLAGS]
    if strays:
        typer.echo(
            f"Error: no such option: {strays[0]}. `fno backlog update` carries "
            "one door per call: --status, --leave, and repeatable --set field=value "
            "(legacy one-flag-per-field spellings are retired).",
            err=True,
        )
        raise typer.Exit(code=2)


def forward_update_door(
    task_id: str, door_args: List[str], graph_path: Path, values: dict
) -> None:
    """`cmd_update`'s forwarding half (x-665f): relay the door flags to the
    native backlog-update action, refusing a mixed call. `values` is the
    caller's `locals()` - the legacy flags' parsed values, screened here
    against :data:`_LEGACY_UPDATE_PARAMS` so the over-budget cli.py only
    carries the four-line handoff."""
    legacy_flags = [
        _flag_spelling(param)
        for param in _LEGACY_UPDATE_PARAMS
        # Identity, not `in (None, False)`: `--fixes-pr 0` means clear, and
        # `0 == False` would silently drop that flag from the mix refusal.
        if values.get(param) is not None and values.get(param) is not False
    ]
    if legacy_flags:
        typer.echo(
            "Error: --status/--leave/--set cannot be mixed with the legacy field flags "
            f"({', '.join(sorted(legacy_flags))}). Run two calls: one through the door, "
            "one with the legacy flags.",
            err=True,
        )
        raise typer.Exit(code=2)
    exit_code, _ = _door_text(task_id, door_args, graph_path)
    raise typer.Exit(code=exit_code)


def _door_text(task_id: str, args: List[str], graph_path: Path):
    from fno.graph.note_cli import native_update

    return native_update(task_id, args, graph_path=graph_path, json_out=False)


def register_lifecycle_commands(
    cli: typer.Typer,
    expand_valid_ids: Callable[..., List[str]],
    require_nodes: Callable[..., None],
    graph_path: Callable[[], Path],
    project_plans_from_graph: Callable[..., None],
) -> None:
    @cli.command(
        "defer",
        epilog="Paired verb: `fno backlog undefer <id>...` reverses this (hidden; run its own --help).",
    )
    def cmd_defer(
        task_ids: List[str] = typer.Argument(
            ...,
            help="Feature IDs (ab-XXXXXXXX). Multiple via space and/or comma: 'ab-X,ab-Y ab-Z'.",
        ),
        reason: str = typer.Option(
            ...,
            "--reason",
            "-R",
            help="Why these nodes are being deferred (applies to all). Free text, surfaced in triage.",
        ),
        kind: Optional[str] = typer.Option(
            None,
            "--kind",
            "-K",
            help=(
                "Classify the deferral (expired|blocked|wont_do|retracted|superseded|later|"
                "contingent|carveout|internal_only|junk). Omitted: stamped only when "
                "the reason exactly matches a known machine-stamped string."
            ),
        ),
    ) -> None:
        """Mark one or more backlog nodes as deferred. Sets ``deferred_at`` + ``deferred_reason``.

        Atomic across the batch: if any ID is unknown, none are deferred.
        Same reason applies to every ID in the batch.
        """
        from fno.graph._constants import (
            DEFERRED_KINDS,
            classify_deferred_reason,
        )
        from fno.graph._intake import _find_dependents
        from fno.graph.store import read_graph

        if kind is not None and kind not in DEFERRED_KINDS:
            typer.echo(
                f"Error: --kind must be one of {', '.join(DEFERRED_KINDS)}, got '{kind}'",
                err=True,
            )
            raise typer.Exit(code=1)

        ids = expand_valid_ids(task_ids)

        # Strip and validate the reason at the CLI boundary so direct invocation
        # cannot land an empty-reason deferral. The triage validator already
        # rejects blank reasons; matching that contract here keeps both write
        # paths producing identically-shaped graph state.
        cleaned_reason = reason.strip()
        if not cleaned_reason:
            typer.echo("Error: --reason cannot be blank", err=True)
            raise typer.Exit(code=1)

        # Resolve every id and abort naming ALL missing ones before any write,
        # mirroring cmd_queue's all-or-nothing batch atomicity. The door write
        # is per-node, so this pre-check is what keeps the batch atomic.
        entries = read_graph(graph_path())
        require_nodes(entries, ids)
        for tid in ids:
            dependents = _find_dependents(entries, tid)
            if dependents:
                typer.echo(
                    f"WARN: Deferring {tid} blocks: {', '.join(dependents)}",
                    err=True,
                )

        # Explicit --kind wins; else classify ONLY by exact match against the
        # machine-stamped table (the maintain drain self-classifies with no
        # flag). No match leaves the kind unset - an honest unknown, never a
        # guess from prose; the door clears any stale kind on the write.
        resolved_kind = kind or classify_deferred_reason(cleaned_reason)
        set_args = ["--status", "deferred", "--set", f"deferred_reason={cleaned_reason}"]
        if resolved_kind:
            set_args.extend(["--set", f"deferred_kind={resolved_kind}"])

        for tid in ids:
            exit_code, _ = _door(tid, set_args, graph_path)
            if exit_code:
                raise typer.Exit(code=exit_code)
            typer.echo(f'Deferred {tid}: "{cleaned_reason}"')
        project_plans_from_graph(ids)

    @cli.command("undefer", hidden=True)
    def cmd_undefer(
        task_ids: List[str] = typer.Argument(
            ...,
            help="Feature IDs (ab-XXXXXXXX). Multiple via space and/or comma: 'ab-X,ab-Y ab-Z'.",
        ),
    ) -> None:
        """Clear deferred state on one or more backlog nodes. Idempotent.

        Atomic across the batch: if any ID is unknown, none are cleared.
        A node still superseded refuses and names the door route - clearing
        its park would print Undeferred while the node stays superseded
        (x-e3c4). Each node that WAS deferred gets its own streak-reset
        event. The verb prints the reason it clears and any plan ruling
        against the node.
        """
        # Call-time imports: the verbs must read whatever the running test or
        # caller patched onto the source modules, never a register-time copy.
        from fno.graph._intake import _find_node
        from fno.graph.store import read_graph

        ids = expand_valid_ids(task_ids)

        entries = read_graph(graph_path())
        require_nodes(entries, ids)

        # AC5-HP: on a node carrying BOTH facts the park rides on the
        # supersession, so leaving deferred alone would no-op; refuse and
        # name the route that actually revives the node.
        def _node_of(tid: str) -> dict:
            node = _find_node(entries, tid)
            assert node is not None  # require_nodes already guaranteed it
            return node

        still_superseded = [
            (tid, _node_of(tid).get("superseded_by"))
            for tid in ids
            if _node_of(tid).get("superseded_by")
        ]
        if still_superseded:
            for tid, replacer in still_superseded:
                typer.echo(
                    f"refused: {tid} is superseded by {replacer}; the park rides on the "
                    f"supersession. Supply it with: fno backlog update {tid} --status idea "
                    f"(or fno backlog unsupersede {tid})",
                    err=True,
                )
            raise typer.Exit(code=2)

        from fno.paths import plans_content_dir
        from fno.plan.rulings import plan_rulings, ruling_lines

        for tid in ids:
            node = _node_of(tid)
            was_deferred = bool(node.get("deferred_at"))
            kind = node.get("deferred_kind")
            reason = node.get("deferred_reason")
            exit_code, receipt = _door(
                tid, ["--leave", "deferred"], graph_path
            )
            if exit_code:
                raise typer.Exit(code=exit_code)
            if not was_deferred or (receipt or {}).get("unchanged"):
                # The door's unchanged receipt, relayed: nothing was written
                # and the caller sees the no-op as a fact, not a silence.
                typer.echo(f"{tid} unchanged")
                typer.echo(f"warning: {tid} was not deferred", err=True)
                continue
            # Mark a streak-reset boundary so the failed-node cascade (#34)
            # gives a human-recovered node a clean slate: it needs N FRESH
            # consecutive failures before auto-defer re-triggers (AC5-FR).
            # Best-effort - a failed emit only means the node keeps its
            # pre-undefer streak, never a crash in undefer.
            from fno.graph.failure import emit_undefer_boundary

            emit_undefer_boundary(tid)
            if reason:
                typer.echo(
                    f"undefer: cleared the deferral of {tid} ({kind or 'no kind'}): {reason}",
                    err=True,
                )
            # A plan ruling against this node is the judgment the park
            # enforced; whoever lifts the park reads it before the node
            # dispatches. Per-node cwd keeps a foreign plansDirectory
            # override holding.
            cwd = node.get("cwd")
            rulings = plan_rulings(
                tid,
                plans_content_dir(Path(cwd)) if cwd else plans_content_dir(),
            )
            for line in ruling_lines(rulings, "undefer", tid):
                typer.echo(line, err=True)
            typer.echo(f"Undeferred {tid}")
        project_plans_from_graph(ids)

    @cli.command(
        "retract",
        hidden=True,  # the advertised surface caps at 12; the lifecycle table in docs/backlog-usage.md is its discovery surface
        epilog="Reversal: `fno backlog undefer <id>...` (hidden; run its own --help).",
    )
    def cmd_retract(
        task_ids: List[str] = typer.Argument(
            ...,
            help="Feature IDs (ab-XXXXXXXX). Multiple via comma: 'ab-X,ab-Y'.",
        ),
        reason: str = typer.Argument(
            ...,
            help="The false premise this row was filed on (applies to all). Surfaced by `fno backlog undefer` and the think-inspect receipt.",
        ),
    ) -> None:
        """Retract one or more backlog nodes: defer + stamp ``deferred_kind: retracted``.

        Usage: ``fno backlog retract <ids> "<the false premise>"``. One act
        for a row filed on a false premise. The deferral removes it from every
        dispatch reader, and the retracted kind is the halt signal the
        blueprint consolidation gate reads, so planning against it stops too.
        Forwards to ``cmd_defer`` with the kind forced; batch atomicity and
        the blank-reason refusal are inherited. The reason rides a positional,
        not a flag: the Python flag surface is shrink-only (x-72fc).
        """
        cmd_defer(task_ids=task_ids, reason=reason, kind="retracted")

    @cli.command("unsupersede", hidden=True)
    def cmd_unsupersede(
        node_id: str = typer.Argument(..., help="The superseded node ID (or slug) to revive"),
    ) -> None:
        """Reverse a supersede on ``node_id``. Idempotent in the safe direction.
        Full contract: docs/architecture/backlog-graph-verb-contracts.md
        """
        from fno.graph._intake import _find_node
        from fno.graph.store import read_graph

        # The door resolves id or slug (the read resolver's contract); the
        # receipt names the canonical id either way.
        entries = read_graph(graph_path())
        node = _find_node(entries, node_id)
        if node is None:
            typer.echo(f"Error: node {node_id} not found", err=True)
            raise typer.Exit(code=1)
        canonical_id = node.get("id", node_id)
        replacer = node.get("superseded_by")
        raw = node.get("supersession")
        session = dict(raw) if isinstance(raw, dict) else {}

        exit_code, receipt = _door(node_id, ["--leave", "superseded"], graph_path)
        if exit_code:
            raise typer.Exit(code=exit_code)
        if (receipt or {}).get("unchanged"):
            typer.echo(f"warning: {node_id} was not superseded", err=True)
        else:
            # Give a revived node the same streak-reset boundary cmd_undefer
            # gives a human-recovered one: a superseded node is often
            # auto-deferred first, so without this the next maintain pass
            # could re-defer it from stale failure history. Best-effort, like
            # undefer.
            from fno.graph.failure import emit_undefer_boundary

            emit_undefer_boundary(canonical_id)
            typer.echo(
                f"unsupersede: cleared the supersession of {canonical_id} "
                f"by {replacer}: {session.get('cause') or ''}",
                err=True,
            )
            from fno.paths import plans_content_dir
            from fno.plan.rulings import plan_rulings, ruling_lines

            rulings = plan_rulings(canonical_id, plans_content_dir())
            for line in ruling_lines(rulings, "unsupersede", canonical_id):
                typer.echo(line, err=True)
            typer.echo(f"Unsuperseded {node_id}")
        # Force the revived node's plan status off terminal `superseded` (the
        # forward-only projector refuses to leave a terminal): without this
        # the graph is active while the plan doc stays superseded. Scoped to
        # the revived node only. Always, not only on a fresh reverse: an
        # interrupted earlier unsupersede can leave the plan reading
        # `superseded` after superseded_by is already clear, and this rerun
        # heals that plan through the no-replacer branch too.
        project_plans_from_graph([canonical_id], force_status_off_terminal_for=canonical_id)
        # Recompute+persist after projection. The graph status was derived
        # during the mutation while the plan still read `superseded`, so it
        # must follow the now-corrected plan or `backlog get`/the board keep
        # reporting `ready` and a fail-closed `design` never makes the node
        # non-dispatchable. A no-op mutator still triggers the
        # recompute+write; idempotent on a clean call.
        from fno.graph.store import locked_mutate_graph

        locked_mutate_graph(graph_path(), lambda e: e)
