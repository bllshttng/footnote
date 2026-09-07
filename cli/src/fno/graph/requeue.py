"""The queue-return subject: free a wedged node, not just its claim.

``unclaim`` releases a claim; ``requeue`` releases a NODE. Status is derived,
never stored: ``locked_by`` OR an open ``do`` session row holds
``in_progress`` (graph_store.rs ``recompute_statuses``). A worker that died
mid-do leaves the row open, so the node stays invisible to every reader that
takes ``ready``. ``requeue`` proves the worker dead, settles the row, and
reports where the derivation landed.
"""
from __future__ import annotations

import json
from typing import Optional

import typer

# A claim may hand a node back only when no live owner can still be writing
# it. ``free`` and ``stale`` are that proof; ``suspect`` is still owned
# (TTL-unexpired), ``corrupted`` is unreadable rather than unowned, and a
# state added later must fail closed instead of passing a deny-list it was
# never added to.
_REQUEUEABLE_CLAIM_STATES = ("free", "stale")

# Transcript-truth states that mean a live worker still owns the do window.
# ``stalled``, ``done`` and ``unknown`` proceed: an unread transcript on a
# node wedged for hours is the ordinary reaped-session case, and the claim
# gate above is what fails closed.
_WARM_TRUTH_STATES = ("working", "your-move", "watching")


def _graph_path():
    from fno.graph.cli import _graph_path as _cli_graph_path

    return _cli_graph_path()


def _read_all() -> list[dict]:
    from fno.graph.store import read_graph

    return read_graph(_graph_path())


def _read_node(node_id: str) -> Optional[dict]:
    return next((e for e in _read_all() if e.get("id") == node_id), None)


def _release_node_lockfile(node_id: str) -> str:
    """Best-effort release of the ``node:<id>`` fno-claim lockfile.

    Releases when the holder is stale (PID dead / TTL expired) or matches the
    invoking session; refuses a LIVE foreign holder (warn + point at
    ``force-release``) so we never silently yank a live peer's claim. Returns a
    short human note for the command summary. Never raises - the graph clear is
    the load-bearing part and must not be undone by a lockfile hiccup.
    """
    try:
        from fno.claims.core import (
            claim_status,
            release_claim,
        )
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
            # Holder-verified release, NOT unconditional force-release (codex P1):
            # between this stale snapshot and the unlink, another dispatcher can
            # reclaim the dead lock with a NEW holder. release_claim() only
            # removes the file if its holder still matches the stale holder we
            # saw, so a fresh live holder is left intact rather than yanked.
            release_claim(key, holder=status.get("holder") or "", root=root)
            return "released stale lockfile"
        if state == "corrupted":
            typer.echo(
                f"warning: lockfile {key} is corrupted; graph claim cleared but "
                f"lockfile left intact. Use `fno agents claim release {key} --force -R <why>` "
                f"to repair.",
                err=True,
            )
            return "lockfile left (corrupted)"

        # state == "live" or "suspect" (x-ba4b): only release if it is ours -
        # a suspect claim (TTL-unexpired, dead pid) is still owned, so a peer's
        # is left intact and only our own is cleared.
        from fno.graph.cli import _invoking_claim_holder

        holder = status.get("holder") or ""
        mine = holder == _invoking_claim_holder()
        if mine:
            release_claim(key, holder=holder, root=root)
            return "released own lockfile"

        typer.echo(
            f"warning: lockfile {key} held by LIVE holder {holder!r}; graph claim "
            f"cleared but lockfile left intact. Use "
            f"`fno agents claim release {key} --force -R <why>` to override.",
            err=True,
        )
        return "lockfile left (live foreign holder)"
    except Exception as exc:  # never let a lockfile error mask the graph clear
        return f"lockfile untouched ({exc})"


def _clear_locked_by(task_id: str) -> Optional[str]:
    """The shared graph clear: ``locked_by``/``locked_at`` -> None.

    Same field clear as ``update --locked-by null``; the keeper's recompute
    derives status back from the now-empty lock on the same write. Returns
    the resolved node id, or None when the node vanished mid-mutation.
    """
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
        node["locked_by"] = None
        node["locked_at"] = None
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    return resolved_id


def _unclaim_node(task_id: str) -> None:
    """Free a claimed node in one call: clear the graph claim (always) and
    best-effort-release the lockfile (stale or owned). Mirrors the graph-side of
    ``update --locked-by null``, then adds the lockfile release the two-step
    dance forced you to do by hand."""
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.statuses import is_open_do_row

    if not has_node_id_prefix(task_id):
        typer.echo(
            f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'",
            err=True,
        )
        raise typer.Exit(code=1)

    resolved_id = _clear_locked_by(task_id)
    node_id = resolved_id or task_id

    lock_note = _release_node_lockfile(node_id)

    # Read-back: the claim clear only transitions the node when the claim was
    # what held it. An open do row holds in_progress on its own, and printing
    # success over that wedge teaches the caller the wrong model of status.
    after = _read_node(node_id)
    open_do = sum(is_open_do_row(r) for r in ((after or {}).get("sessions") or []))
    if (after or {}).get("status") == "in_progress":
        plural = "s" if open_do != 1 else ""
        typer.echo(
            f"unclaim: {node_id} still reads in_progress after clearing the "
            f"claim ({open_do} open do row{plural}).\n"
            f"         The claim was not what held it. "
            f"Use: fno backlog requeue {node_id}",
            err=True,
        )
        raise typer.Exit(code=3)

    typer.echo(f"Unclaimed {node_id} ({lock_note})")


def cmd_unclaim(task_id: str) -> None:
    """Free a claimed node in one call (graph claim + safe lockfile release)."""
    _unclaim_node(task_id)


def cmd_requeue(node: str, *, json_out: bool = False) -> None:
    """Return a node wedged ``in_progress`` by a dead worker to the queue."""
    from fno.agents.session_truth import _humanize_age, resolve_session_truth
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for
    from fno.graph.fuzzy import resolve_node
    from fno.graph.statuses import is_open_do_row
    from fno.graph.store import reap_open_session_record

    match = resolve_node(node, _read_all())
    if match.kind != "exact":
        typer.echo(f"requeue: no exact node matches {node!r}.", err=True)
        raise typer.Exit(code=2)
    row = match.candidates[0]
    node_id = row["id"]
    status_before = row.get("status")

    # has_pr outranks open_do in the derivation, so an in_review node never
    # reaches the settle; refusing here is also what keeps requeue from ever
    # touching a PR it must not clear.
    if status_before != "in_progress":
        typer.echo(
            f"requeue: {node_id} reads status {status_before!r}, not in_progress; "
            f"only an in_progress node can be returned to the queue.",
            err=True,
        )
        raise typer.Exit(code=2)

    # The lockfile reader, never `fno agents claim status`: that CLI consults
    # the worker roster and answers unknown for rows it cannot resolve, so it
    # would refuse on nodes the roster has merely lost sight of (x-7421).
    key = f"node:{node_id}"
    claim = claim_status(key, root=claims_root_for(key))
    state = claim.get("state")
    if state not in _REQUEUEABLE_CLAIM_STATES:
        holder = claim.get("holder")
        holder_note = f", holder {holder!r}" if holder else ""
        typer.echo(
            f"requeue: claim {key} reads {state}{holder_note}; "
            f"only {' or '.join(_REQUEUEABLE_CLAIM_STATES)} may requeue.",
            err=True,
        )
        raise typer.Exit(code=3)

    open_rows = [r for r in (row.get("sessions") or []) if is_open_do_row(r)]
    truths = {}
    for r in open_rows:
        truth = resolve_session_truth(r.get("session_id") or "")
        truths[r.get("session_id")] = truth
        if truth.get("state") in _WARM_TRUTH_STATES:
            typer.echo(
                f"requeue: {r.get('harness')}:{r.get('session_id')} is "
                f"{truth.get('state')} (last activity "
                f"{truth.get('last_activity_age_s')}s ago); a warm worker "
                f"still owns the do window.",
                err=True,
            )
            raise typer.Exit(code=3)

    for r in open_rows:
        reap_open_session_record(
            _graph_path(),
            node_id,
            phase="do",
            harness=r.get("harness") or "",
            session_id=r.get("session_id") or "",
        )

    _clear_locked_by(node_id)
    _release_node_lockfile(node_id)

    after = _read_node(node_id)
    status_after = (after or {}).get("status")
    remaining = sum(is_open_do_row(r) for r in ((after or {}).get("sessions") or []))
    if after is None or status_after == "in_progress":
        typer.echo(
            f"requeue: {node_id} still reads in_progress after settling "
            f"({remaining} open do row(s) remain).",
            err=True,
        )
        raise typer.Exit(code=1)

    settled = []
    for r in open_rows:
        truth = truths.get(r.get("session_id")) or {}
        settled.append(
            {
                "harness": r.get("harness"),
                "session_id": r.get("session_id"),
                "state": truth.get("state"),
                "last_event_at": truth.get("last_event_at"),
                "age": _humanize_age(truth.get("last_activity_age_s")),
            }
        )
    receipt = {
        "node_id": node_id,
        "status_before": status_before,
        "status_after": status_after,
        "settled": settled,
    }
    if json_out:
        typer.echo(json.dumps(receipt, sort_keys=True))
        return
    typer.echo(f"requeued {node_id} ({status_before} -> {status_after})")
    for s in settled:
        typer.echo(
            f"  {s['harness']}:{s['session_id']} state={s['state']} "
            f"last_event_at={s['last_event_at']} age={s['age']}"
        )
