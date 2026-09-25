"""The queue-return subject: free a wedged node, not just its claim.

``unclaim`` releases a claim; ``requeue`` releases a NODE. Status is derived: ``locked_by`` OR an open ``do`` session row holds ``in_progress`` (graph_store.rs ``recompute_statuses``), so a worker that died mid-do leaves the node invisible to every reader that takes ``ready``.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

import typer

# Allowlist, fail closed: suspect is still owned, corrupted is unreadable, and a state added later must refuse rather than pass a deny-list never updated.
_REQUEUEABLE_CLAIM_STATES = ("free", "stale")

def _graph_path():
    from fno.graph.cli import _graph_path as _cli_graph_path

    return _cli_graph_path()


def _read_node(node_id: str, graph_path) -> Optional[dict]:
    from fno.graph import api as graph_api

    row = graph_api.node(node_id, path=graph_path)
    return row.model_dump(by_alias=True) if row else None


def _invoking_session_id() -> Optional[str]:
    """Best-effort id of the running session, for the unclaim is-this-lockfile-mine check. None => treat any live holder as foreign."""
    try:
        from fno.carveout.core import resolve_session_id
        from fno.graph._intake import repo_root

        # repo_root() returns a str; resolve_session_id() needs a Path or the TypeError is swallowed below and this always returns None.
        return resolve_session_id(Path(repo_root()))
    except Exception:
        return None


def _invoking_claim_holder() -> Optional[str]:
    """Best-effort holder from the active target manifest; falls back to the target session id."""
    try:
        from fno.graph._intake import repo_root
        from fno.paths import target_state_path_or_legacy

        state = target_state_path_or_legacy(Path(repo_root()))
        for line in state.read_text(encoding="utf-8").splitlines():
            if line.lstrip().startswith("target_claim_holder:"):
                value = line.split(":", 1)[1].strip().strip("\"'")
                if value and value != "null":
                    return value
    except Exception:
        pass

    sid = _invoking_session_id()
    return f"target-session:{sid}" if sid else None


def _release_node_lockfile(node_id: str) -> str:
    """Best-effort release of the ``node:<id>`` lockfile; never raises. Releases stale or own holders; keeps a LIVE foreign holder."""
    try:
        from fno.claims.core import claim_status, release_claim
        from fno.claims.io import claims_root_for
    except Exception:
        return "lockfile untouched (claims module unavailable)"

    key = f"node:{node_id}"
    try:
        root = claims_root_for(key)
        status = claim_status(key, root=root)
        state = status.get("state")

        if state == "free":
            return "no lockfile"
        if state == "stale":
            return "released stale lockfile" if release_claim(key, status.get("holder") or "", root=root) else "lockfile changed"
        if state == "corrupted":
            typer.echo(f"warning: lockfile {key} is corrupted; graph claim cleared but lockfile left intact. Use `fno agents claim release {key} --force -R <why>` to repair.", err=True)
            return "lockfile left (corrupted)"

        # live or suspect: only release when it is ours; a suspect claim (TTL-unexpired, dead pid) is still owned.
        holder = status.get("holder") or ""
        if holder == _invoking_claim_holder():
            return "released own lockfile" if release_claim(key, holder, root=root) else "lockfile changed"

        typer.echo(f"warning: lockfile {key} held by LIVE holder {holder!r}; lockfile left intact. Use `fno agents claim release {key} --force -R <why>` to override.", err=True)
        return "lockfile left (live foreign holder)"
    except Exception as exc:
        return f"lockfile untouched ({exc})"


def _wedge_refusal(verb: str, node_id: str, open_do: int) -> None:
    plural = "s" if open_do != 1 else ""
    typer.echo(f"{verb}: {node_id} still reads in_progress after clearing the claim ({open_do} open do row{plural}). The claim was not what held it. Use: fno backlog requeue {node_id}", err=True)
    raise typer.Exit(code=3)


def _settle_status_after_release(node_id: str) -> None:
    from fno.graph.statuses import settle_released_node
    from fno.graph.store import commit_rows_via_store

    commit_rows_via_store(_graph_path(), settle_released_node(node_id))


def _unclaim_node(task_id: str) -> None:
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.statuses import is_open_do_row

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    node_id = task_id
    if _read_node(node_id, _graph_path()) is None:
        raise typer.BadParameter(f"unclaim: graph node {node_id} not found")
    lock_note = _release_node_lockfile(node_id)
    if lock_note.startswith("lockfile"):
        raise typer.BadParameter(f"unclaim refused: {lock_note}")

    _settle_status_after_release(node_id)

    after = _read_node(node_id, _graph_path())
    if (after or {}).get("persisted_status") == "in_progress":
        _wedge_refusal("unclaim", node_id, sum(is_open_do_row(r) for r in ((after or {}).get("sessions") or [])))

    typer.echo(f"Unclaimed {node_id} ({lock_note})")


def cmd_requeue(node: str, *, json_out: bool = False) -> None:
    """Return a node wedged ``in_progress`` by a dead worker to the queue."""
    from fno.agents.reachability import REACHABLE, classify_reachability, inference_samples
    from fno.agents.session_truth import _humanize_age, resolve_session_truth
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for
    from fno.graph import api as graph_api
    from fno.graph.fuzzy import resolve_node
    from fno.graph.statuses import is_open_do_row
    from fno.graph.store import reap_open_session_record

    rows = [
        n.model_dump(by_alias=True)
        for n in graph_api.nodes(include_archived=True, path=_graph_path()).nodes
    ]

    match = resolve_node(node, rows)
    if match.kind != "exact":
        typer.echo(f"requeue: no exact node matches {node!r}.", err=True)
        raise typer.Exit(code=2)
    row = match.candidates[0]
    node_id = row["id"]
    status_before = row.get("persisted_status")

    # has_pr outranks open_do in the derivation, so an in_review node never reaches the settle; refusing here also keeps requeue off any PR.
    if status_before != "in_progress":
        typer.echo(f"requeue: {node_id} reads status {status_before!r}, not in_progress; only an in_progress node can be returned to the queue.", err=True)
        raise typer.Exit(code=2)

    # The lockfile reader, never `fno agents claim status`: requeue wants the claim record, and the composite verdict still reads unknown when an unresolved roster row's worktree names this node.
    key = f"node:{node_id}"
    claim = claim_status(key, root=claims_root_for(key))
    state = claim.get("state")
    if state not in _REQUEUEABLE_CLAIM_STATES:
        from fno.claims.verdict import reclaimable_note

        holder = claim.get("holder")
        holder_note = f", holder {holder!r}" if holder else ""
        basis = claim.get("basis")
        basis_note = f" ({basis})" if basis else ""
        note = reclaimable_note(claim)
        grace_note = f"; {note}: re-run fno backlog requeue {node_id} then" if note else ""
        typer.echo(f"requeue: claim {key} reads {state}{basis_note}{holder_note}; only {' or '.join(_REQUEUEABLE_CLAIM_STATES)} may requeue{grace_note}.", err=True)
        raise typer.Exit(code=3)

    open_rows = [r for r in (row.get("sessions") or []) if is_open_do_row(r)]
    pairs = [(r, resolve_session_truth(r.get("session_id") or "")) for r in open_rows]
    for r, truth in pairs:
        # falsifier=None: requeue holds no registry row, and a falsifier can
        # only LOWER a verdict, so passing none can never invent a refusal.
        reach = classify_reachability(
            truth_state=truth.get("state"),
            age_s=truth.get("last_activity_age_s"),
            falsifier=None,
            # A 429 corpse dies writing its error, so its age is freshest at death.
            observed_model=truth.get("observed_model"),
        )
        if reach.verdict == REACHABLE:
            from datetime import datetime, timezone
            from fno.graph.maintain import abandoned_do_rows, do_row_idle_s

            why = next((a.reason for a in abandoned_do_rows([{**row, "locked_by": None}], set(), strict=False) if a.session_id == r.get("session_id") and a.verdict == "held"), None)
            if why is None:
                continue
            # reap-open is NOT named here: this worker reads reachable, so a
            # death claim would be false. The owner's honest self-close is.
            now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
            typer.echo(
                f"requeue: {r.get('harness')}:{r.get('session_id')} reads {reach.render()}; "
                "a reachable worker still owns the do window. If that session is "
                f"yours and has stopped this node: fno backlog session add {node_id} "
                f"--phase do --ended-at {now}. The do row stays: {why}.",
                err=True,
            )
            raise typer.Exit(code=3)

    for r in open_rows:
        reap_open_session_record(_graph_path(), node_id, phase="do", harness=r.get("harness") or "", session_id=r.get("session_id") or "")

    note = _release_node_lockfile(node_id)
    if note.startswith("lockfile"):
        typer.echo(f"requeue: {node_id} claim was not released ({note}); the node stays claimed.", err=True)
        raise typer.Exit(code=3)
    if note.startswith(("released", "no lockfile")):
        _settle_status_after_release(node_id)

    after = _read_node(node_id, _graph_path())
    status_after = (after or {}).get("persisted_status")
    remaining = sum(is_open_do_row(r) for r in ((after or {}).get("sessions") or []))
    if after is None or status_after == "in_progress":
        typer.echo(f"requeue: {node_id} still reads in_progress after settling ({remaining} open do row(s) remain).", err=True)
        raise typer.Exit(code=1)

    # `working, 0 samples` names a corpse and `working, 31 samples` names a
    # worker. None is a harness that keeps no transcript, never a zero.
    from datetime import datetime, timezone
    from fno.graph.maintain import do_row_idle_s
    settled = [{"harness": r.get("harness"), "session_id": r.get("session_id"), "state": truth.get("state"), "samples": inference_samples(truth.get("observed_model")), "last_event_at": truth.get("last_event_at"), "age": _humanize_age(truth.get("last_activity_age_s")), "row_idle_s": do_row_idle_s(row, r, datetime.now(timezone.utc).timestamp())} for r, truth in pairs]
    receipt = {"node_id": node_id, "status_before": status_before, "status_after": status_after, "settled": settled}
    if json_out:
        typer.echo(json.dumps(receipt, sort_keys=True))
        return
    typer.echo(f"requeued {node_id} ({status_before} -> {status_after})")
    for s in settled:
        samples = "?" if s["samples"] is None else s["samples"]
        typer.echo(f"  {s['harness']}:{s['session_id']} state={s['state']} samples={samples} last_event_at={s['last_event_at']} age={s['age']}")
