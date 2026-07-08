"""fno.backlog.reconcile_dispatch - G4 merge-triggered reconciliation dispatch.

When a blocker's PR merges, its ``dep=contract`` dependents must NOT get a cold
``/target no-merge`` dispatch: they already built optimistically and carry an
open draft PR (advance_dependents deliberately skips a node with an open PR). G4
routes them here instead:

  - manifest present + unreconciled  -> dispatch ``/target --reconcile <manifest>``
    (AC4-HP): a fresh pass pulls main, runs the drift gate, de-stubs, finalizes.
  - manifest absent (merge-before-manifest) -> write a ``reconcile:<dep>`` pending
    TTL sentinel and dispatch NOTHING (AC8). The dependent's first pass calls
    fire_pending_reconcile() when it writes its manifest, dispatching exactly once.
  - manifest already reconciled -> skip (nothing to do).

Reuses advance's claim + spawn + event machinery verbatim (a reconcile dispatch
is byte-for-byte advance's launch, only with a ``--reconcile`` token and a
``reconcile-`` worker name). The ``dispatch:<id>`` dedup + the global
``node:<id>`` liveness check together guarantee at-most-one reconcile per
dependent across triggers and contexts (Invariant: at-most-one dispatch).

Gated on the same opt-in as advance (auto_continue_enabled) and strictly
non-fatal: every path emits exactly one decision event and the host merge op
never wedges.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Optional

from fno import route_resolve as _route_resolve
from fno import stub_manifest as sm
from fno.backlog.advance import (
    EVENT_DISPATCHED,
    EVENT_FAILED,
    EVENT_SKIPPED,
    AdvanceResult,
    SpawnAlreadyRunning,
    _claim_is_live,
    _emit,
    _events_path,
    _safe_release,
    _spawn_worker,
    _walker_key,
    _worker_agent_name,
    auto_continue_enabled,
)

# The pending sentinel covers the window between the blocker's merge and the
# dependent's manifest-write. Generous TTL: a first pass can run for many
# minutes. If it never writes (the pass died), the sentinel expires and the
# stranded draft PR is surfaced by `triage health` (x-a10e reuse), not by us.
_PENDING_TTL_MS = 6 * 60 * 60 * 1000  # 6h


def _claims_root_for(key: str):
    """Route reconcile claims so writer and reader agree across repos.

    Delegates to fno.claims.io.claims_root_for: ``node:``/``reconcile:``/
    ``dispatch:`` all go to the GLOBAL ($HOME) root. The reconcile sentinel is
    written in the BLOCKER's merge context and read in the DEPENDENT's first-pass
    context (possibly a different repo), so a cwd-relative root would lose it;
    ``dispatch:`` is now global too (it keys on the same global node id), so it
    still shares advance's dedup token -- globally, across repos.
    """
    from fno.claims.io import claims_root_for

    return claims_root_for(key)


def _pending_holder(node_id: str) -> str:
    """Stable holder for the ``reconcile:<node>`` sentinel.

    The sentinel is acquired in the BLOCKER's merge process and released in the
    DEPENDENT's first-pass process - two different PIDs. A pid-based holder would
    never match on release, leaking the sentinel; a node-stable holder lets the
    releaser drop exactly the claim the writer set (and makes re-acquire
    idempotent).
    """
    return f"reconcile-pending:{node_id}"


def _sentinel_is_live(node_id: str) -> bool:
    """Liveness of ``reconcile:<node>`` at the GLOBAL root.

    Routes the probe through ``_claims_root_for`` (the shared claims_root_for
    helper), which roots ``reconcile:`` globally -- the same root the sentinel
    was written at, even when read from a different repo.
    """
    from fno.claims.core import claim_status

    key = f"reconcile:{node_id}"
    try:
        # live OR suspect (x-ba4b) => occupied; a suspect reservation (TTL-
        # unexpired, dead pid) must still dedup so reconcile never double-fires.
        return claim_status(key, root=_claims_root_for(key)).get("state") in (
            "live",
            "suspect",
        )
    except Exception:  # noqa: BLE001 - a probe error must not crash the caller
        return False


def _contract_dependents(closed_node_id: str) -> list[dict]:
    """Direct ``dep=contract`` dependents of the just-merged blocker.

    Both same- and cross-project (unlike advance_dependents, the reconcile path
    does not care about the project boundary: the dependent already exists with
    its own worktree + draft PR). Returns ``{id, project, slug, cwd}`` dicts.
    Raises on a graph read error so the caller skips rather than guessing.
    """
    from fno.graph.store import read_graph
    from fno.paths import graph_json

    out: list[dict] = []
    for e in read_graph(graph_json()):
        if not isinstance(e, dict):
            continue
        if closed_node_id not in (e.get("blocked_by") or []):
            continue
        if e.get("dep") != "contract":
            continue  # hard dependents stay with advance/advance_dependents
        node_id = e.get("id")
        if not node_id:
            continue
        out.append({
            "id": node_id,
            "project": e.get("project"),
            "slug": e.get("slug") or e.get("title"),
            "cwd": e.get("cwd"),
            # x-571f: carry the model pin so the reconcile worker (a /target
            # --reconcile build) honors it, not just advance's dependents.
            # model_tier rides alongside so the tier resolver sees the annotation.
            "model": e.get("model"),
            "model_tier": e.get("model_tier"),
        })
    return out


def _dep_root(dep: dict) -> Optional[str]:
    """The dependent's own project root, for manifest lookup + worker --cwd.

    Prefers the work-map-resolved project root (durable), falls back to the
    node's recorded cwd. None only when neither resolves (an unmapped,
    cwd-less node) -> the caller skips by surfacing it.
    """
    project = dep.get("project")
    if project:
        try:
            from fno.graph._intake import project_root_from_settings

            root = project_root_from_settings(project)
            if root:
                return root
        except Exception:  # noqa: BLE001 - fall back to recorded cwd
            pass
    return dep.get("cwd") or None


def _dispatch_reconcile(
    dep: dict, root: str, manifest_path: Path, ev_path: Path, verbose: bool
) -> AdvanceResult:
    """Dedup + spawn the ``/target --reconcile`` worker for one dependent.

    Shares advance's claim machinery: a live ``node:<id>`` or ``dispatch:<id>``
    short-circuits (a worker/peer already owns it), then an O_EXCL
    ``dispatch:<id>`` reservation guards the boot window.
    """
    node_id = dep["id"]

    def skip(reason: str, detail: Optional[str] = None) -> AdvanceResult:
        data: dict = {"reason": reason, "node_id": node_id, "kind": "reconcile"}
        if detail:
            data["detail"] = detail[:200]
        _emit(EVENT_SKIPPED, data, ev_path)
        return AdvanceResult("skipped", EVENT_SKIPPED, reason=reason, node_id=node_id, detail=detail)

    def failed(error: str) -> AdvanceResult:
        _emit(EVENT_FAILED, {"node_id": node_id, "kind": "reconcile", "error": error[:200]}, ev_path)
        return AdvanceResult("failed", EVENT_FAILED, reason="spawn-failed", node_id=node_id, detail=error)

    # node:<id> (global) is the cross-context dedup backstop: once the reconcile
    # worker inits it owns node:<id>, so a later trigger sees already-claimed.
    if _claim_is_live(f"node:{node_id}") or _claim_is_live(f"dispatch:{node_id}"):
        return skip("already-claimed")

    from fno.claims.core import ClaimHeldByOther, acquire_claim

    dispatch_key = f"dispatch:{node_id}"
    holder = f"reconcile:{os.getpid()}"
    dispatch_root = _claims_root_for(dispatch_key)
    try:
        acquire_claim(
            dispatch_key, holder,
            ttl_ms=180_000,  # 3m boot-window bridge, mirroring advance
            reason=f"reconcile dispatch for {node_id}",
            root=dispatch_root,
        )
    except ClaimHeldByOther:
        return skip("already-claimed")
    except Exception as exc:  # noqa: BLE001
        return skip("claim-error", detail=str(exc))

    try:
        short_id = _spawn_worker(
            node_id, root, dep.get("slug"),
            reconcile_manifest=str(manifest_path),
            model=_route_resolve.node_model(dep, provider=dep.get("provider")),
        )
    except SpawnAlreadyRunning:
        _safe_release(dispatch_key, holder, dispatch_root)
        return skip("already-claimed")
    except Exception as exc:  # noqa: BLE001
        _safe_release(dispatch_key, holder, dispatch_root)
        return failed(str(exc))

    _emit(
        EVENT_DISPATCHED,
        {
            "node_id": node_id,
            "short_id": short_id,
            "kind": "reconcile",
            "agent_name": _worker_agent_name(node_id, dep.get("slug"), prefix="reconcile"),
        },
        ev_path,
    )
    if verbose:
        import sys
        print(f"reconcile: dispatched {node_id} -> worker {short_id}", file=sys.stderr)
    return AdvanceResult("dispatched", EVENT_DISPATCHED, node_id=node_id, short_id=short_id)


def _route_one(dep: dict, ev_path: Path, verbose: bool) -> AdvanceResult:
    """Route one contract dependent: dispatch, pending-sentinel, or skip."""
    node_id = dep["id"]

    def skip(reason: str, detail: Optional[str] = None) -> AdvanceResult:
        data: dict = {"reason": reason, "node_id": node_id, "kind": "reconcile"}
        if detail:
            data["detail"] = detail[:200]
        _emit(EVENT_SKIPPED, data, ev_path)
        return AdvanceResult("skipped", EVENT_SKIPPED, reason=reason, node_id=node_id, detail=detail)

    root = _dep_root(dep)
    if not root:
        return skip("unmapped-project", detail=dep.get("project"))

    manifest_path = sm.manifest_path(node_id, root)
    if not manifest_path.exists():
        # AC8 merge-before-manifest: do NOT dispatch. Reserve a pending sentinel
        # the first pass will fire when it writes the manifest. Best-effort: a
        # sentinel-write failure degrades to a skip (the first pass's own write
        # re-fire is the belt; the stranded-PR surfacing is the suspenders).
        from fno.claims.core import ClaimHeldByOther, acquire_claim

        key = f"reconcile:{node_id}"
        try:
            acquire_claim(
                key, _pending_holder(node_id),
                ttl_ms=_PENDING_TTL_MS,
                reason=f"reconcile pending: {node_id} manifest not yet written",
                root=_claims_root_for(key),
            )
        except ClaimHeldByOther:
            pass  # a sentinel already pending -> idempotent, nothing to add
        except Exception as exc:  # noqa: BLE001
            return skip("sentinel-error", detail=str(exc))
        return skip("reconcile-pending", detail="manifest not yet written")

    # Manifest exists -> reconciled already, or ready to dispatch.
    try:
        manifest = sm.load(manifest_path)
    except sm.StubManifestError:
        # A malformed manifest still HOLDS a dispatch (the reconcile worker's own
        # validate refuses it loudly); routing it lets that pass surface the gap.
        manifest = {}
    if manifest.get("reconciled") is True:
        return skip("already-reconciled")

    return _dispatch_reconcile(dep, root, manifest_path, ev_path, verbose)


def dispatch_reconcile_for_blocker(
    *,
    closed_node_id: str,
    project_root: Optional[Path] = None,
    events_path: Optional[Path] = None,
    verbose: bool = False,
) -> list[AdvanceResult]:
    """Route every ``dep=contract`` dependent of the just-merged blocker (G4).

    Called alongside advance() / advance_dependents() on the merge event. Gated
    on the same opt-in and strictly non-fatal. Emits exactly one decision event
    per dependent; a blocker with no contract dependents emits nothing and
    returns ``[]`` (Boundaries: a pure-hard close is a no-op here).
    """
    ev_path = events_path if events_path is not None else _events_path(project_root)

    if not auto_continue_enabled(project_root=project_root):
        return []
    if _claim_is_live(_walker_key()):
        return []

    try:
        deps = _contract_dependents(closed_node_id)
    except Exception as exc:  # noqa: BLE001 - never guess on a graph read error
        _emit(
            EVENT_SKIPPED,
            {"reason": "dependents-error", "kind": "reconcile",
             "closed_node_id": closed_node_id, "detail": str(exc)[:200]},
            ev_path,
        )
        return [AdvanceResult("skipped", EVENT_SKIPPED, reason="dependents-error", detail=str(exc))]

    return [_route_one(dep, ev_path, verbose) for dep in deps]


def fire_pending_reconcile(node_id: str, root: Path | str) -> Optional[AdvanceResult]:
    """Fire a pending ``reconcile:<node>`` sentinel after the manifest is written.

    Called by ``fno stub-manifest write`` (the dependent's first pass). If a
    sentinel is live (the blocker merged before this manifest existed, AC8), the
    manifest now exists, so dispatch the reconcile pass exactly once and release
    the sentinel. Returns the dispatch result, or None when no sentinel is
    pending (the common case: the blocker had not merged yet). Non-raising: any
    trouble degrades to None so the manifest write never fails on the re-fire.
    """
    key = f"reconcile:{node_id}"
    sentinel_root = _claims_root_for(key)
    if not _sentinel_is_live(node_id):
        return None

    # Look the dependent up so the dispatch carries its slug/project/cwd.
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json

        dep = None
        for e in read_graph(graph_json()):
            if isinstance(e, dict) and e.get("id") == node_id:
                dep = {"id": node_id, "project": e.get("project"),
                       "slug": e.get("slug") or e.get("title"), "cwd": e.get("cwd"),
                       "model": e.get("model"), "model_tier": e.get("model_tier")}
                break
        if dep is None:
            dep = {"id": node_id, "cwd": str(root)}
    except Exception:  # noqa: BLE001
        dep = {"id": node_id, "cwd": str(root)}

    ev_path = _events_path(Path(root) if not isinstance(root, Path) else root)
    manifest_path = sm.manifest_path(node_id, root)
    try:
        result = _dispatch_reconcile(dep, str(root), manifest_path, ev_path, verbose=False)
    except Exception:  # noqa: BLE001 - the manifest write is the contract
        return None
    # Release the sentinel only once the dispatch is owned (dispatched) or proven
    # redundant (already-claimed = a worker exists). On a transient failure, keep
    # the sentinel so a later write/advance retries (Invariant: don't drop it
    # while the dependent is still un-reconciled).
    if result.decision == "dispatched" or result.reason == "already-claimed":
        _safe_release(key, _pending_holder(node_id), sentinel_root)
    return result
