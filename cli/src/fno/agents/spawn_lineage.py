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
    the parent record, x-b57a). Never raises; always returns a triple
    (missing fields degrade to None).

    Harness detection order (Task 2.2, x-30f6):
      CODEX_THREAD_ID        -> harness="codex"
      CLAUDE_CODE_SESSION_ID -> harness="claude"
      CODEX_SESSION_ID       -> harness="codex"
      GEMINI_SESSION_ID      -> harness="gemini"
      OPENCODE_SESSION_ID    -> harness="opencode"
    """
    # OWNED, not precedence (x-20f1): this triple is stamped onto the SPAWNED
    # row as its `spawned_by_*` edge, so an inherited marker records a stranger
    # as the parent for the life of that row. An ambiguous resolve records no
    # lineage rather than a wrong one.
    from fno.claims.self_identity import resolve_self_identity

    identity = resolve_self_identity()

    # $PWD may be unset (non-interactive shells, cron, daemonized procs); fall
    # back to os.getcwd(), which for a `fno agents spawn` subprocess is the
    # spawning session's cwd (inherited), so the parent cwd is always captured.
    parent_cwd: Optional[str] = (os.environ.get("PWD") or os.getcwd()).strip() or None

    # x-5c25: with NO marker the walk still names the family, and an ANCESTOR
    # cannot be the stranger an inherited MARKER can. Gated on an empty marker
    # set, never a missing harness: a present marker that resolved to nothing
    # is a contradiction, and x-b57a / x-0992 rule that attributes nothing.
    harness = identity.harness
    if not harness and not identity.markers_present:
        from fno.claims.session_pid import resolve_session_harness

        try:
            harness = resolve_session_harness()
        except Exception:  # noqa: BLE001 - the walk never fails a spawn
            harness = None

    return identity.session_id, harness, parent_cwd


def _report_unlinked_parent(session_id: Optional[str]) -> Optional[str]:
    """Name an unrecorded parent edge in the spawn output, and return the
    reason so the spawn event can carry it (x-5283): the event holds either
    a session id or this reason, never both empty. A null can be CORRECT
    (a foreign inherited marker would record a stranger as parent); the
    defect was its silence, so say it with the identity resolution's reason.
    """
    if session_id:
        return None
    try:
        from fno.claims.self_identity import resolve_self_identity

        identity = resolve_self_identity()
        markers = ",".join(m for m, _h, _v in identity.markers_present) or "no markers"
        reason = f"identity disposition={identity.disposition}, markers={markers}"
    except Exception:  # noqa: BLE001 - the notice never breaks the spawn
        reason = "identity unreadable"
    print(f"spawn: parent edge NOT recorded ({reason}); this worker will not "
          f"appear in its spawner's orphan check", file=sys.stderr)
    return reason


# The prompt lane opens a row only for a message that leads with a review
# verb: the x-4342 complaint shape is a review worker spawned with the node id
# in its prompt. A do worker whose prompt mentions a SIBLING id must not get a
# reviewer row stamped on that sibling, so prose and other verbs arm nothing.
from fno.agents.spawn_phase import REVIEW_VERB_PREFIXES as _REVIEW_VERB_PREFIXES  # noqa: E402


def _resolve_spawn_merge_grant(message: str) -> dict:
    """The spawner's OWN merge-grant verdict for a do-phase worker.

    Explicit refusal first (the message carries --no-merge), then the standing
    config: auto_merge enabled with grant=dispatch grants, and every other
    shape records an explicit false anyway. Recording the false - rather than
    omitting the grant - is what makes AC9-EDGE work (a newer refusal outranks
    an older grant at resolve time, because the newest RECEIPT wins) and keeps
    the verdict the spawner's own observation: a worker can never mint or
    rewrite it, and absence on a row this old predating the field reads as
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
    """Open the node's sessions row for a spawned contributor (x-4342).

    A spawned reviewer never holds the claim, so it crosses none of the
    mechanical stamping chokepoints (claim acquire/release, plan-bind, PR-link)
    and its work lands in no sessions array. This stamp runs spawn-side, where
    the receipt already knows the node and the worker's identity, and is
    best-effort with a named stderr skip - matching the claim-path stamps, a
    provenance miss must never fail the spawn.

    ``node`` is the ALREADY-RESOLVED node id from the ``--node`` lane
    (cmd_spawn's own ``resolve_provenance`` pass, reused rather than repeated).
    Without it, the prompt lane fires only for a message leading with a review
    verb that names exactly ONE node id - conservative by design, because a
    bare id in prose names a sibling more often than a target.

    The row's identity is the WORKER's harness-native session id (the registry
    row's harness_session_id; the receipt's session uuid as fallback), never
    the spawning session's id and never a prefix-shaped short id: the
    observed_model reader refuses those, so the row would be born unreadable.
    The row closes via `fno backlog session reap-open --phase all` when the
    daemon observer proves the worker dead (fill, not remove - the provenance
    stands).
    """
    from datetime import datetime, timezone

    from fno.agents.mux_spawn import resolve_provenance
    from fno.graph._constants import extract_node_ids
    from fno.graph.store import append_session_record
    from fno.paths import graph_json

    node_id = node
    who = node
    if node_id is None:
        msg = (message or "").lstrip()
        ids = extract_node_ids(message or "")
        if not msg.startswith(_REVIEW_VERB_PREFIXES) or len(ids) != 1:
            return  # not a single-node review prompt: no row, nothing to say
        who = ids[0]
        try:
            node_id = resolve_provenance(who, None, None).get("FNO_NODE")
        except Exception as exc:  # noqa: BLE001 - provenance never fails the spawn
            print(f"spawn: session row open skipped for {who}: {exc}", file=sys.stderr)
            return
    if not node_id:
        print(
            f"spawn: session row open skipped for {who} "
            f"(node not in graph); the row was not written. Skipped.",
            file=sys.stderr,
        )
        return
    if not phase:
        # A verb cmd_spawn could not label (a /think worker is neither do nor
        # review). A guessed label would lie on an append-only record.
        print(
            f"spawn: session row open skipped for {node_id} "
            f"(phase unknown for the message verb; pass --session-phase); "
            f"the row was not written. Skipped.",
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
    """Record WHO LAUNCHED this node's worker, on the node itself (x-5c25).

    The sibling of :func:`_stamp_spawned_session_row`, which records who WORKED
    it. Refuses rather than half-writes: no node, no write; no proven parent
    session, no write; never overwrites an existing edge, because launch is the
    FIRST launch. Never raises. Why: docs/architecture/node-provenance.md.
    """
    if not node:
        return
    session_id, harness, parent_cwd = _capture_parent_edge()
    if not session_id:
        return

    # Read before paying the locked write; the sibling stamp already spends a
    # keeper cycle here. Say when nothing was written, or a graph missing the
    # node commits an unchanged snapshot and exits 0.
    try:
        from fno.graph.store import locked_mutate_graph, read_graph
        from fno.paths import graph_json
        from fno.tracker import active_backend_name

        if active_backend_name() != "graph":
            # Under an external tracker this graph.json is not the record.
            return

        existing = next((r for r in read_graph() if r.get("id") == node), None)
        if existing is None:
            print(
                f"spawn: launch edge not recorded on {node} (node not in graph); "
                f"the edge was not written. Skipped.",
                file=sys.stderr,
            )
            return
        if existing.get("spawned_by_session"):
            print(
                f"spawn: launch edge on {node} already names "
                f"{existing['spawned_by_session']}; kept.",
                file=sys.stderr,
            )
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

        locked_mutate_graph(graph_json(), mutator)
    except (Exception, SystemExit) as exc:  # noqa: BLE001 - never fail the spawn
        print(f"spawn: launch edge not recorded on {node}: {exc}", file=sys.stderr)


