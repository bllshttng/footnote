"""Node-lifecycle reversal verbs: undefer and unsupersede.

Extracted from graph/cli.py under the file-budget ratchet: the verbs a
session calls to reverse a park or a supersession live here, registered into
the backlog app by :func:`register_lifecycle_commands`. The helpers they
share with the rest of the backlog surface are injected at registration, so
this module never imports graph.cli (the cycle would be unimportable).
"""

from __future__ import annotations

from pathlib import Path
from typing import Callable, List

import typer


def register_lifecycle_commands(
    cli: typer.Typer,
    expand_valid_ids: Callable[[List[str]], List[str]],
    require_nodes: Callable[[list, List[str]], None],
    graph_path: Callable[[], Path],
    project_plans_from_graph: Callable[[List[str]], None],
) -> None:
    from fno.graph._intake import _find_node
    from fno.graph.store import locked_mutate_graph

    @cli.command("undefer", hidden=True)
    def cmd_undefer(
        task_ids: List[str] = typer.Argument(
            ...,
            help="Feature IDs (ab-XXXXXXXX). Multiple via space and/or comma: 'ab-X,ab-Y ab-Z'.",
        ),
    ) -> None:
        """Clear deferred state on one or more backlog nodes. Idempotent.

        Atomic across the batch: if any ID is unknown, none are cleared.
        Reports each ID's prior state; warns (non-fatally) for IDs that were
        not actually deferred. Each node that WAS deferred gets its own
        streak-reset event. The verb prints the reason it clears and any plan
        ruling against the node.
        """
        ids = expand_valid_ids(task_ids)

        was_deferred: list[tuple[str, bool, str | None, str | None, str | None]] = []

        def mutator(entries):
            require_nodes(entries, ids)
            for tid in ids:
                node = _find_node(entries, tid)
                was_deferred.append(
                    (
                        tid,
                        bool(node.get("deferred_at")),
                        node.get("deferred_kind"),
                        node.get("deferred_reason"),
                        node.get("cwd"),
                    )
                )
                node["deferred_at"] = None
                node["deferred_reason"] = None
                node.pop("deferred_kind", None)
            return entries

        locked_mutate_graph(graph_path(), mutator)

        from fno.paths import plans_content_dir
        from fno.plan.rulings import plan_rulings, ruling_lines

        for tid, did, kind, reason, cwd in was_deferred:
            if did:
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
                rulings = plan_rulings(
                    tid,
                    plans_content_dir(Path(cwd)) if cwd else plans_content_dir(),
                )
                for line in ruling_lines(rulings, "undefer", tid):
                    typer.echo(line, err=True)
            else:
                typer.echo(f"warning: {tid} was not deferred", err=True)
            typer.echo(f"Undeferred {tid}")
        project_plans_from_graph(ids)

    @cli.command("unsupersede", hidden=True)
    def cmd_unsupersede(
        node_id: str = typer.Argument(..., help="The superseded node ID to revive"),
    ) -> None:
        """Reverse a supersede on ``node_id``. Idempotent in the safe direction.
        Full contract: docs/architecture/backlog-graph-verb-contracts.md
        """
        from fno.graph._constants import has_node_id_prefix

        if not has_node_id_prefix(node_id):
            typer.echo(
                f"Error: node_id must be a <prefix>-<4..8 hex> node id, got '{node_id}'",
                err=True,
            )
            raise typer.Exit(code=1)

        was_superseded_holder: list[bool] = [False]
        canonical_id_box: list[str] = [node_id]
        supersession_box: list[dict | None] = [None]
        replacer_box: list[str | None] = [None]

        def mutator(entries):
            node = _find_node(entries, node_id)
            if node is None:
                typer.echo(f"Error: node {node_id} not found", err=True)
                raise typer.Exit(code=1)
            replacer = node.get("superseded_by")
            was_superseded_holder[0] = bool(replacer)
            canonical_id_box[0] = node.get("id", node_id)
            # Captured before the clear: the cause and replacer are exactly
            # what this verb erases, so they are what it prints.
            raw = node.get("supersession")
            supersession_box[0] = dict(raw) if isinstance(raw, dict) else None
            replacer_box[0] = str(replacer) if replacer else None
            if not replacer:
                # Not superseded: nothing to reverse. Return WITHOUT touching
                # deferred_at, so a node that is merely deferred (not
                # superseded) keeps its park. Clearing it here would
                # reactivate parked work.
                return entries
            # Drop the backref so the replacer's `supersedes` list no longer
            # claims a node it no longer supersedes - a stale claim would make
            # the chain read as live after we just broke it. Compare against
            # the canonical id, not the (possibly abbreviated) argument: the
            # backref stores the full id.
            new_node = _find_node(entries, replacer)
            if new_node is not None:
                new_node["supersedes"] = [
                    s for s in (new_node.get("supersedes") or []) if s != node["id"]
                ]
            node["superseded_by"] = None
            node["supersession"] = None
            # Any pre-supersession deferral survives the reversal on purpose.
            return entries

        locked_mutate_graph(graph_path(), mutator)

        if not was_superseded_holder[0]:
            typer.echo(f"warning: {node_id} was not superseded", err=True)
        else:
            # Give a revived node the same streak-reset boundary cmd_undefer
            # gives a human-recovered one: a superseded node is often
            # auto-deferred first, so without this the next maintain pass
            # could re-defer it from stale failure history. Best-effort, like
            # undefer.
            from fno.graph.failure import emit_undefer_boundary

            emit_undefer_boundary(canonical_id_box[0])
            session = supersession_box[0] or {}
            typer.echo(
                f"unsupersede: cleared the supersession of {canonical_id_box[0]} "
                f"by {replacer_box[0]}: {session.get('cause') or ''}",
                err=True,
            )
            from fno.paths import plans_content_dir
            from fno.plan.rulings import plan_rulings, ruling_lines

            rulings = plan_rulings(canonical_id_box[0], plans_content_dir())
            for line in ruling_lines(rulings, "unsupersede", canonical_id_box[0]):
                typer.echo(line, err=True)
        typer.echo(f"Unsuperseded {node_id}")
        # Force the revived node's plan status off terminal `superseded` (the
        # forward-only projector refuses to leave a terminal): without this
        # the graph is active while the plan doc stays superseded. Scoped to
        # the revived node only.
        cid = canonical_id_box[0]
        project_plans_from_graph([cid], force_status_off_terminal_for=cid)
        # Recompute+persist after projection. The graph status was derived
        # during the mutation while the plan still read `superseded`, so it
        # must follow the now-corrected plan or `backlog get`/the board keep
        # reporting `ready` and a fail-closed `design` never makes the node
        # non-dispatchable. Always, not only on a fresh reverse: an
        # interrupted earlier unsupersede can leave the plan reading
        # `superseded` after superseded_by is already clear, and this rerun
        # heals that plan through the no-replacer branch too. A no-op mutator
        # still triggers the recompute+write; idempotent on a clean call.
        locked_mutate_graph(graph_path(), lambda entries: entries)
