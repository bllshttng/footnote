"""What a spawn records about the node it launched.

``dispatch`` and ``cli`` re-export these names, so every import site and every
test that patches one keeps working. Fields: docs/architecture/node-provenance.md.
"""
from __future__ import annotations

import os
import sys
from typing import Optional


def _capture_parent_edge() -> tuple[Optional[str], Optional[str], Optional[str]]:
    """Capture the spawning session's ambient identity from environment variables.

    Returns ``(session_id, harness, cwd)`` — all three are strings or None.
    Precedence applies within one harness family; markers from two families
    attribute NOTHING (a foreign inherited marker must not be laundered into
    the parent record). Never raises; always returns a triple
    (missing fields degrade to None).

    Harness detection order (Task 2.2):
      CODEX_THREAD_ID        -> harness="codex"
      CLAUDE_CODE_SESSION_ID -> harness="claude"
      CODEX_SESSION_ID       -> harness="codex"
      GEMINI_SESSION_ID      -> harness="gemini"
      OPENCODE_SESSION_ID    -> harness="opencode"
    """
    # OWNED, not precedence: this triple is stamped onto the SPAWNED
    # row as its `spawned_by_*` edge, so an inherited marker records a stranger
    # as the parent for the life of that row. An ambiguous resolve records no
    # lineage rather than a wrong one.
    from fno.claims.self_identity import resolve_self_identity

    identity = resolve_self_identity()

    # $PWD may be unset (non-interactive shells, cron, daemonized procs); fall
    # back to os.getcwd(), which for a `fno agents spawn` subprocess is the
    # spawning session's cwd (inherited), so the parent cwd is always captured.
    parent_cwd: Optional[str] = (os.environ.get("PWD") or os.getcwd()).strip() or None

    # with NO marker the walk still names the family, and an ANCESTOR
    # cannot be the stranger an inherited MARKER can. Gated on an empty marker
    # set, never a missing harness: a present marker that resolved to nothing
    # is a contradiction, and / rule that attributes nothing.
    harness = identity.harness
    if not harness and not identity.markers_present:
        from fno.claims.session_pid import resolve_session_harness

        try:
            harness = resolve_session_harness()
        except Exception:  # noqa: BLE001 - the walk never fails a spawn
            harness = None

    return identity.session_id, harness, parent_cwd


def _lineage_reason(session_id: Optional[str]) -> Optional[str]:
    """Why a mint with no parent session could not name one: the identity
    resolution's disposition and markers, or None when a session was
    captured. A null can be CORRECT (a foreign inherited marker would
    record a stranger as parent); the defect was its silence. Pure: no
    printing, so a register or fallback path can carry a reason without
    emitting the spawn notice.
    """
    if session_id:
        return None
    try:
        from fno.claims.self_identity import resolve_self_identity

        identity = resolve_self_identity()
        markers = ",".join(m for m, _h, _v in identity.markers_present) or "no markers"
        return f"identity disposition={identity.disposition}, markers={markers}"
    except Exception:  # noqa: BLE001 - the notice never breaks the spawn
        return "identity unreadable"


def _report_unlinked_parent(session_id: Optional[str]) -> Optional[str]:
    """Name an unrecorded parent edge in the spawn output, and return the
    reason so the spawn event can carry it : the event holds either
    a session id or this reason, never both empty.
    """
    reason = _lineage_reason(session_id)
    if reason is not None:
        print(f"spawn: parent edge NOT recorded ({reason}); this worker will not "
              f"appear in its spawner's orphan check", file=sys.stderr)
    return reason


def build_spawn_provenance(
    *,
    explicit_origin: Optional[dict] = None,
    explicit_owner: Optional[dict] = None,
    cause: Optional[str] = None,
) -> Optional[dict]:
    """The Python half of the one spawn door (v33): the validated origin+owner
    record. ``None`` when a session caller cannot be proven. Explicit producers
    MUST name a mission/crown owner; ``cause`` speaks the vocabulary minus ``sob``."""
    if explicit_origin is not None and explicit_owner is not None:
        _validate_explicit_provenance(explicit_origin, explicit_owner, cause)
        return {"origin": explicit_origin, "owner": explicit_owner}

    # Explicit dispatch context outranks ambient capture.
    carried_origin = os.environ.get("FNO_SPAWN_ORIGIN")
    carried_owner = os.environ.get("FNO_SPAWN_OWNER")
    if carried_origin or carried_owner:
        if not (carried_origin and carried_owner):
            raise ValueError("FNO_SPAWN_ORIGIN and FNO_SPAWN_OWNER must be exported together")
        import json as _json

        origin = _json.loads(carried_origin)
        owner = _json.loads(carried_owner)
        return build_spawn_provenance(explicit_origin=origin, explicit_owner=owner, cause=cause)

    from fno.claims.self_identity import resolve_self_identity

    identity = resolve_self_identity()
    session_id = identity.session_id
    harness = identity.harness
    cwd = (os.environ.get("PWD") or os.getcwd()).strip()
    if not session_id or not harness:
        return None
    parent = {"harness": harness, "session_id": session_id, "cwd": cwd}
    return {
        "origin": {"kind": "session", "parent": parent, "invocation": None},
        "owner": {"kind": "session", **parent},
    }


def _validate_explicit_provenance(origin: dict, owner: dict, cause: Optional[str]) -> None:
    """Mirror the door's rules for explicit non-session provenance."""
    source = origin.get("source") if origin.get("kind") == "non_session" else None
    if source is None:
        return
    if source.get("kind") not in ("daemon", "launch_agent"):
        return
    if owner.get("kind") not in ("mission", "crown"):
        raise ValueError(
            "daemon/launch-agent origin requires a mission or crown owner; "
            "the daemon starter is never substituted"
        )
    declared = source.get("cause")
    if cause is not None and declared is None:
        source["cause"] = cause
        declared = cause
    if declared == "sob":
        raise ValueError("cause 'sob' is retired; it cannot ride a spawn")
    if declared is not None:
        from fno.agents.naming import dispatch_sources

        if declared not in dispatch_sources():
            raise ValueError(f"cause {declared!r} is not in the naming-codes vocabulary")


def _resolve_spawn_merge_grant(message: str) -> dict:
    """The spawner's OWN merge-grant verdict for a do-phase worker.

    Explicit refusal first (--no-merge), then the standing config. Recording
    the false - rather than omitting the grant - is what makes AC9-EDGE work
    (the newest RECEIPT wins at resolve time), keeps the verdict the
    spawner's own observation, and makes a predating absence read as
    "nobody resolved", never as a grant.
    """
    from datetime import datetime, timezone

    from fno.agents.harness_map import message_carries_no_merge
    from fno.config import load_settings

    recorded_by = (os.environ.get("FNO_AGENT_SELF") or "").strip() or "spawn"
    recorded_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    if message_carries_no_merge(message):
        return {
            "approved": False,
            "source": "no-merge-flag",
            "recorded_by": recorded_by,
            "recorded_at": recorded_at,
        }
    try:
        auto_merge = load_settings().auto_merge
        granted = bool(auto_merge.enabled) and str(auto_merge.grant or "none") == "dispatch"
    except Exception:  # noqa: BLE001 - an unreadable config never grants
        granted = False
    return {
        "approved": granted,
        "source": "config" if granted else "none",
        "recorded_by": recorded_by,
        "recorded_at": recorded_at,
    }


def _stamp_spawned_session_row(
    *,
    node: "str | None",
    message: str,
    phase: str,
    worker_name: "str | None",
    worker_harness: "str | None",
    worker_session_uuid: "str | None",
    worker_effort: "str | None" = None,
) -> None:
    """Open the node's sessions row for a spawned contributor.

    A spawned worker never holds the claim, so it crosses none of the
    mechanical stamping chokepoints (claim acquire/release, plan-bind, PR-link)
    and its work lands in no sessions array. This stamp runs spawn-side, where
    the receipt already knows the node and the worker's identity, and is
    best-effort with a named stderr skip - matching the claim-path stamps, a
    provenance miss must never fail the spawn.

    ``node`` is the ALREADY-RESOLVED node id from the ``--node`` lane
    (cmd_spawn's own ``resolve_provenance`` pass, reused rather than repeated).
    The row's identity is the WORKER's harness-native session id (the registry
    row's harness_session_id; the receipt's session uuid as fallback), never
    the spawning session's id and never a prefix-shaped short id: the
    observed_model reader refuses those, so the row would be born unreadable.
    The row closes via `fno backlog session reap-open --phase all` when the
    daemon observer proves the worker dead (fill, not remove - the provenance
    stands).
    """
    from datetime import datetime, timezone

    from fno.graph.store import append_session_record
    from fno.paths import graph_json

    node_id = node
    who = node
    if node_id is None:
        return
    if not node_id:
        print(
            f"spawn: session row open skipped for {who} "
            f"(node not in graph); the row was not written. Skipped.",
            file=sys.stderr,
        )
        return

    harness = None
    session_id = None
    if worker_name:
        try:
            from fno.agents.registry import load_registry

            row = next((r for r in load_registry() if r.name == worker_name), None)
        except Exception:  # noqa: BLE001 - fall through to the receipt fallback
            row = None
        if row is not None:
            harness = row.harness
            session_id = row.harness_session_id
            worker_effort = row.effort or worker_effort
    if not session_id:
        # A pane binds its session uuid after the registry row exists, and a
        # one-shot tears its row down before returning; the receipt's own uuid
        # is the remaining honest source.
        harness = worker_harness
        session_id = worker_session_uuid
    if not harness or not session_id:
        if worker_name:
            from fno.paths import agents_registry_path
            from fno.rust_binary import verb_call
            try:
                grant = _resolve_spawn_merge_grant(message) if phase == "do" else None
                verb_call("pending-session-row", {"action": "park", "name": worker_name,
                         "phase": phase, "merge_grant": grant,
                         "registry": str(agents_registry_path())})
            except (Exception, SystemExit) as exc:  # noqa: BLE001 - never fail the spawn
                print(f"spawn: park skipped for {node_id}: {exc}", file=sys.stderr)
            return
        print(
            f"spawn: session row open skipped for {node_id} "
            f"(no harness session id at spawn); the row was not written. Skipped.",
            file=sys.stderr,
        )
        return

    started = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    # The durable grant rides only a do-phase row: review/think workers never
    # merge, and a grant on their rows would be a receipt nobody should read.
    merge_grant = _resolve_spawn_merge_grant(message) if phase == "do" else None
    try:
        found, _added = append_session_record(
            graph_json(), node_id, phase=phase,
            harness=harness, session_id=session_id, started_at=started,
            effort=worker_effort,
            merge_grant=merge_grant,
        )
    except (Exception, SystemExit) as exc:  # noqa: BLE001 - never fail the spawn
        print(f"spawn: session row open skipped for {node_id}: {exc}", file=sys.stderr)
        return
    if not found:
        print(
            f"spawn: session row open skipped for {node_id} "
            f"(node not in graph); the row was not written. Skipped.",
            file=sys.stderr,
        )


def _stamp_launch_edge(node: "str | None") -> None:
    """Record WHO LAUNCHED this node's worker, on the node itself.

    The sibling of :func:`_stamp_spawned_session_row`, which records who WORKED
    it. Refuses rather than half-writes: no node, no write; no proven parent
    session, no write; launch is the FIRST launch, never overwritten.
    """
    if not node:
        return
    session_id, harness, parent_cwd = _capture_parent_edge()
    if not session_id:
        return

    # Read before paying the locked write; say when nothing was written, or
    # a graph missing the node commits an unchanged snapshot and exits 0.
    try:
        from fno.graph.api import wire_rows
        from fno.graph.store import commit_rows_via_store
        from fno.paths import graph_json
        from fno.tracker import active_backend_name

        if active_backend_name() != "graph":
            # Under an external tracker this graph.json is not the record.
            return

        existing = next((r for r in wire_rows(path=graph_json()) if r.get("id") == node), None)
        if existing is None:
            print(f"spawn: launch edge not recorded on {node} (node not in graph); "
                  f"the edge was not written. Skipped.", file=sys.stderr)
            return
        if existing.get("spawned_by_session"):
            who = existing["spawned_by_session"]
            print(f"spawn: launch edge on {node} already names {who}; kept.",
                  file=sys.stderr)
            return

        def mutator(entries: "list[dict]") -> "list[dict]":
            for row in entries:
                # Re-check under the lock: the read above is a snapshot, and a
                # racing spawn may have landed the first launch since.
                if row.get("id") != node or row.get("spawned_by_session"):
                    continue
                row["spawned_by_session"] = session_id
                row["spawned_by_harness"] = harness
                row["spawned_by_cwd"] = parent_cwd
            return entries

        commit_rows_via_store(graph_json(), mutator)
    except (Exception, SystemExit) as exc:  # noqa: BLE001 - never fail the spawn
        print(f"spawn: launch edge not recorded on {node}: {exc}", file=sys.stderr)

