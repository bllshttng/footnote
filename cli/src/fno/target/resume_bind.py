"""Native target resume: rebind the node claim to the resumed durable process.

The sole writer for native target resumption (x-2ccd). A native harness can
resume the same durable conversation after the process that began the target run
has exited. The immutable manifest still names the correct harness session and
Footnote run; this primitive proves the ambient process belongs to that attempt
and atomically rebinds the (now dead-pid) claim to the resumed durable pid.

Fail-closed by construction: every path that is not an affirmative same-holder
local rebind returns a structured ``refused`` result naming the failed
precondition, so a resumed agent never silently believes it owns a target it
does not. Policy lives here; every harness adapter is transport-only.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Optional

from fno.claims import RebindRefused, compare_and_rebind
from fno.claims.session_pid import resolve_session_pid
from fno.harness_identity import resolve_harness_identity

from .manifest import manifest_identity, read_target_manifest


def _graph_node_is_terminal(node_id: str) -> Optional[bool]:
    """Is backlog node ``node_id`` in a terminal state? None when unknowable.

    Best-effort: a terminal node (done) must not be rebound, but an unreadable
    graph is not itself a refusal - the manifest + claim are the ownership
    authority, and refusing on every graph read failure would block resume
    during a transient graph corruption. Returns None to let the caller proceed.
    """
    try:
        from fno.graph.load import load_graph
    except Exception:  # noqa: BLE001
        return None
    try:
        nodes = load_graph() or []
    except Exception:  # noqa: BLE001 - corrupt graph is not ownership truth
        return None
    for node in nodes:
        if not isinstance(node, dict):
            continue
        if str(node.get("id")) == node_id:
            status = str(node.get("status") or "").strip().lower()
            return status in {"done", "superseded"}
    return None  # node absent from a readable graph: not provably terminal


def resume_bind(
    project_root: Path,
    *,
    heartbeat: bool = False,
    harness: Optional[str] = None,
    harness_session_id: Optional[str] = None,
    new_pid: Optional[int] = None,
    ttl_ms: Optional[int] = None,
    claims_root: Optional[Path] = None,
) -> dict[str, Any]:
    """Rebind the target node claim to the resumed durable process.

    Returns a structured result:

    - ``{"result": "noop", "reason": "no manifest"}`` - a plain session with no
      target manifest; the adapter stays silent.
    - ``{"result": "refused", "reason": ..., "field": ...}`` - identity not
      proven or ownership unverifiable; nothing changed. Loud and specific.
    - ``{"result": "rebound", "node": ..., "pid": new, "previous_pid": old,
      "previous_state": ...}`` - a dead local owner was rebound.
    - ``{"result": "idempotent", ...}`` - already bound to this pid; lease
      refreshed.

    ``ttl_ms`` None preserves the prior claim's liveness mode and window.
    """
    raw = read_target_manifest(project_root)
    if not raw:
        return {"result": "noop", "reason": "no target manifest in this cwd"}

    ident = manifest_identity(raw)
    if ident is None:
        return {
            "result": "refused",
            "reason": (
                "manifest lacks a complete identity tuple "
                "(harness/harness_session_id/fno_id/graph_node_id/"
                "target_claim_*); a legacy or partial manifest cannot prove "
                "this process owns the target"
            ),
        }

    # Ambient identity (an explicit override wins; else resolved from env).
    hi = resolve_harness_identity()
    ambient_harness = harness or hi.harness
    ambient_sid = harness_session_id or hi.session_id

    if not ambient_harness or ambient_harness != ident["harness"]:
        return {
            "result": "refused",
            "reason": (
                f"ambient harness {ambient_harness!r} does not match manifest "
                f"harness {ident['harness']!r}"
            ),
            "field": "harness",
        }
    if not ambient_sid or ambient_sid != ident["harness_session_id"]:
        return {
            "result": "refused",
            "reason": (
                f"ambient harness_session_id does not match the manifest; "
                f"this is not the same durable session that started the target"
            ),
            "field": "harness_session_id",
        }

    # A terminal node is settled work; never rebind it.
    terminal = _graph_node_is_terminal(ident["graph_node_id"])
    if terminal is True:
        return {
            "result": "refused",
            "reason": (
                f"node {ident['graph_node_id']} is terminal (done/superseded); "
                "a settled target is not rebound"
            ),
            "node": ident["graph_node_id"],
        }

    npid = new_pid if new_pid is not None else (resolve_session_pid() or os.getpid())
    try:
        claim, mode = compare_and_rebind(
            ident["target_claim_key"],
            ident["target_claim_holder"],
            new_pid=npid,
            ttl_ms=ttl_ms,
            root=claims_root,
            fno_id=ident["fno_id"],
            harness_tag=ident["harness"],
            harness_session_id=ident["harness_session_id"],
        )
    except RebindRefused as exc:
        return {
            "result": "refused",
            "reason": exc.reason,
            "claim_state": exc.state,
            "holder": exc.holder,
            "pid": exc.pid,
            "node": ident["graph_node_id"],
            "advice": (
                "inspect `fno backlog provenance "
                f"{ident['graph_node_id']}` or reclaim via "
                f"`fno target start {ident['graph_node_id']}`"
            ),
        }

    return {
        "result": mode,  # "rebind" or "idempotent"
        "node": ident["graph_node_id"],
        "fno_id": ident["fno_id"],
        "harness": ident["harness"],
        "harness_session_id": ident["harness_session_id"],
        "pid": claim.pid,
        "holder": claim.holder,
        "heartbeat": heartbeat,
    }
