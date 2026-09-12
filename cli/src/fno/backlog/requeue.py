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

_UNSET = object()


def _graph_path():
    from fno.graph.cli import _graph_path as _cli_graph_path

    return _cli_graph_path()


def _read_node(node_id: str, graph_path) -> Optional[dict]:
    from fno.graph.store import read_graph

    return next((e for e in read_graph(graph_path) if e.get("id") == node_id), None)


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
            # Holder-verified, never a blind force-release: a new dispatcher may hold the lock between this stale snapshot and the unlink.
            release_claim(key, holder=status.get("holder") or "", root=root)
            return "released stale lockfile"
        if state == "corrupted":
            typer.echo(f"warning: lockfile {key} is corrupted; graph claim cleared but lockfile left intact. Use `fno agents claim release {key} --force -R <why>` to repair.", err=True)
            return "lockfile left (corrupted)"

        # live or suspect (x-ba4b): only release when it is ours; a suspect claim (TTL-unexpired, dead pid) is still owned.
        holder = status.get("holder") or ""
        if holder == _invoking_claim_holder():
            release_claim(key, holder=holder, root=root)
            return "released own lockfile"

        typer.echo(f"warning: lockfile {key} held by LIVE holder {holder!r}; graph claim cleared but lockfile left intact. Use `fno agents claim release {key} --force -R <why>` to override.", err=True)
        return "lockfile left (live foreign holder)"
    except Exception as exc:  # never let a lockfile error mask the graph clear
        return f"lockfile untouched ({exc})"


def _clear_locked_by(task_id: str, *, expect_locked_by: object = _UNSET) -> Optional[str]:
    """The shared graph clear: ``locked_by``/``locked_at`` -> None. Returns the resolved node id. ``expect_locked_by`` (requeue passes the value its first read saw) aborts when a claim landed between that read and this commit; the sentinel keeps unclaim's operator override unconditional."""
    from fno.graph._intake import _find_node
    from fno.graph.store import locked_mutate_graph

    resolved_id: Optional[str] = None

    def mutator(entries):
        nonlocal resolved_id
        node = _find_node(entries, task_id)
        if node is None:
            typer.echo(f"Error: graph node {task_id} not found", err=True)
            raise typer.Exit(code=1)
        resolved_id = node["id"]
        if expect_locked_by is not _UNSET and node.get("locked_by") != expect_locked_by:
            typer.echo(f"requeue: a claim landed on {resolved_id} between the read and this write (locked_by {node.get('locked_by')!r}); clear skipped, the claim is left intact.", err=True)
            raise typer.Exit(code=3)
        node["locked_by"] = None
        node["locked_at"] = None
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    return resolved_id


def _wedge_refusal(verb: str, node_id: str, open_do: int) -> None:
    """The earned-success rule shared by unclaim and update: a lock clear that leaves the node in_progress did not return it to the queue."""
    plural = "s" if open_do != 1 else ""
    typer.echo(f"{verb}: {node_id} still reads in_progress after clearing the claim ({open_do} open do row{plural}). The claim was not what held it. Use: fno backlog requeue {node_id}", err=True)
    raise typer.Exit(code=3)


def verify_lock_stamp_receipt(stored_node: dict, locked_by: str, fallback_id: str = "") -> None:
    """The post-commit read-back for ``update --locked-by``: the Updated
    receipt answers "was the command accepted", never "is the value there",
    and only the committed row can answer the second. Refuses the receipt
    when the stored owner differs; a non-null stamp with no backing claim
    lockfile warns (mirror-only state claim hygiene clears); a null release
    that leaves an open do row wedged refuses, naming the settling verb.
    """
    from fno.claims.io import node_has_live_claim

    node_id = stored_node.get("id") or fallback_id
    expected_owner = None if locked_by == "null" else locked_by
    stored_owner = stored_node.get("locked_by")
    if stored_owner != expected_owner:
        typer.echo(
            f"error: {node_id} read back locked_by={stored_owner!r}, not "
            f"{expected_owner!r}: the write did not persist. A concurrent "
            "claim transition may have cleared it; re-check before trusting.",
            err=True,
        )
        raise typer.Exit(code=1)
    if expected_owner is None:
        # Earned-success rule, same as unclaim: a lock clear that left the
        # node in_progress on its own open do rows did not return it to the
        # queue, so the receipt refuses and names the verb that settles it.
        if stored_node.get("status") == "in_progress":
            from fno.graph.statuses import is_open_do_row

            _wedge_refusal(
                "update",
                node_id,
                sum(is_open_do_row(r) for r in (stored_node.get("sessions") or [])),
            )
        return
    try:
        has_claim = node_has_live_claim(f"node:{node_id}")
    except Exception:  # noqa: BLE001 - the probe must not fail a write that landed
        return
    if not has_claim:
        typer.echo(
            f"warning: no live claim lockfile backs node:{node_id}; claim "
            "hygiene (fno agents claim reap) clears locked_by without one. "
            f"To hold the node: fno agents claim acquire node:{node_id}",
            err=True,
        )


def _unclaim_node(task_id: str) -> None:
    """Free a claimed node in one call: clear the graph claim (always) and best-effort-release the lockfile (stale or owned)."""
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.statuses import is_open_do_row

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    resolved_id = _clear_locked_by(task_id)
    node_id = resolved_id or task_id
    lock_note = _release_node_lockfile(node_id)

    after = _read_node(node_id, _graph_path())
    if (after or {}).get("status") == "in_progress":
        _wedge_refusal("unclaim", node_id, sum(is_open_do_row(r) for r in ((after or {}).get("sessions") or [])))

    typer.echo(f"Unclaimed {node_id} ({lock_note})")


def cmd_requeue(node: str, *, json_out: bool = False) -> None:
    """Return a node wedged ``in_progress`` by a dead worker to the queue."""
    from fno.agents.reachability import REACHABLE, classify_reachability, inference_samples
    from fno.agents.session_truth import _humanize_age, resolve_session_truth
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for
    from fno.graph.fuzzy import resolve_node
    from fno.graph.statuses import is_open_do_row
    from fno.graph.store import read_graph, reap_open_session_record

    match = resolve_node(node, read_graph(_graph_path()))
    if match.kind != "exact":
        typer.echo(f"requeue: no exact node matches {node!r}.", err=True)
        raise typer.Exit(code=2)
    row = match.candidates[0]
    node_id = row["id"]
    status_before = row.get("status")

    # has_pr outranks open_do in the derivation, so an in_review node never reaches the settle; refusing here also keeps requeue off any PR.
    if status_before != "in_progress":
        typer.echo(f"requeue: {node_id} reads status {status_before!r}, not in_progress; only an in_progress node can be returned to the queue.", err=True)
        raise typer.Exit(code=2)

    # The lockfile reader, never `fno agents claim status`: requeue wants the claim record, and the composite verdict still reads unknown when an unresolved roster row's worktree names this node (x-36c3).
    key = f"node:{node_id}"
    claim = claim_status(key, root=claims_root_for(key))
    state = claim.get("state")
    if state not in _REQUEUEABLE_CLAIM_STATES:
        holder = claim.get("holder")
        holder_note = f", holder {holder!r}" if holder else ""
        typer.echo(f"requeue: claim {key} reads {state}{holder_note}; only {' or '.join(_REQUEUEABLE_CLAIM_STATES)} may requeue.", err=True)
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
            typer.echo(
                f"requeue: {r.get('harness')}:{r.get('session_id')} reads {reach.render()}; "
                "a reachable worker still owns the do window.",
                err=True,
            )
            raise typer.Exit(code=3)

    for r in open_rows:
        reap_open_session_record(_graph_path(), node_id, phase="do", harness=r.get("harness") or "", session_id=r.get("session_id") or "")

    _clear_locked_by(node_id, expect_locked_by=row.get("locked_by"))
    _release_node_lockfile(node_id)

    after = _read_node(node_id, _graph_path())
    status_after = (after or {}).get("status")
    remaining = sum(is_open_do_row(r) for r in ((after or {}).get("sessions") or []))
    if after is None or status_after == "in_progress":
        typer.echo(f"requeue: {node_id} still reads in_progress after settling ({remaining} open do row(s) remain).", err=True)
        raise typer.Exit(code=1)

    # `working, 0 samples` names a corpse and `working, 31 samples` names a
    # worker. None is a harness that keeps no transcript, never a zero.
    settled = [{"harness": r.get("harness"), "session_id": r.get("session_id"), "state": truth.get("state"), "samples": inference_samples(truth.get("observed_model")), "last_event_at": truth.get("last_event_at"), "age": _humanize_age(truth.get("last_activity_age_s"))} for r, truth in pairs]
    receipt = {"node_id": node_id, "status_before": status_before, "status_after": status_after, "settled": settled}
    if json_out:
        typer.echo(json.dumps(receipt, sort_keys=True))
        return
    typer.echo(f"requeued {node_id} ({status_before} -> {status_after})")
    for s in settled:
        samples = "?" if s["samples"] is None else s["samples"]
        typer.echo(f"  {s['harness']}:{s['session_id']} state={s['state']} samples={samples} last_event_at={s['last_event_at']} age={s['age']}")
