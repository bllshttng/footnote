"""fno backlog advance - merge-triggered auto-continue dispatcher.

Node ab-3cd195b6. When a backlog node's PR merges, a merge-detector
(``fno backlog reconcile`` or the /pr merged skill) calls this verb after the
node-close write commits. If auto-continue is armed for the project and no live
walk owns it, advance dispatches a fresh background ``/target no-merge`` worker
for the next now-unblocked node, so a merge-gated epic walks itself group-by-
group across merges with no manual re-invocation.

Locked Decisions this module embodies:
  1. Decoupled from the loop driver - driven by the merge event, so megawalk /
     /target / /megatron all inherit auto-continue (no driver-specific code).
  4. Fire-and-forget dispatch: ``fno agents spawn`` -> ``/target no-merge <id>``.
  5. Concurrency via ``fno claim``: honor ``walker:<root>`` (no double-dispatch
     during a live walk); reserve ``dispatch:<id>`` (O_EXCL dedup + bridge token
     that outlives this short-lived process until the worker owns ``node:<id>``,
     LD#11 / AC1-CLAIM - mirrors handoff.sh + dispatch-node.sh).
  6. advance never merges - only dispatches no-merge workers.
  7. Non-fatal: a failed spawn never wedges the host op (reconcile/post-merge).
 12. Every code path emits EXACTLY ONE decision event before returning
     (advance_dispatched | advance_skipped{reason} | advance_failed), so a
     silent stall is impossible.

The ``dispatch:<id>`` reservation uses a TTL claim (not PID-liveness) precisely
so it survives advance's exit (AC1-CLAIM): the just-dispatched node stays
"claimed" for the boot window, so a concurrent reconcile/post-merge sees it as
already-being-worked. The spawned worker acquires ``node:<id>`` cleanly on its
own ``fno target init`` (free at that point); the reservation then expires by
TTL once the worker owns the node.
"""
from __future__ import annotations

import json
import logging
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

_LOG = logging.getLogger(__name__)

# Campaign-arm + override knobs (Claude's Discretion #4: the campaign arm is a
# per-project STATE FILE, not an env var, because an env var set by a live
# /megawalk does NOT survive to the later, detached SessionStart reconcile that
# observes the web merge. The env var is retained only as a highest-precedence
# explicit override (tests + same-process force-enable/disable).
_ENV_OVERRIDE = "FNO_AUTO_CONTINUE"
_ARM_MARKER_REL = Path(".fno") / ".auto-continue-armed"

# Mirror handoff.sh / dispatch-node.sh: a 3-minute TTL bridge token covers the
# spawn->worker-init boot window. TTL (not PID) liveness is mandatory so the
# reservation outlives this process (LD#11 / AC1-CLAIM).
_DISPATCH_TTL_MS = 180_000  # 3m

_TRUTHY = frozenset({"1", "true", "yes", "on"})


def auto_continue_enabled(
    project: Optional[str] = None,
    project_root: Optional[Path] = None,
) -> bool:
    """Resolve whether auto-continue is armed for this project.

    Precedence (highest first), mirroring config.auto_merge's local>global idea:
      1. ``FNO_AUTO_CONTINUE`` env override (explicit force on/off).
      2. campaign-arm marker file ``.fno/.auto-continue-armed`` (written
         by ``/megawalk auto-continue``; survives the merge->reconcile boundary).
      3. ``config.auto_continue.enabled`` from settings.yaml (local>global via
         load_settings deep-merge).
      4. default False.

    Fail-safe (AC2-ERR): ANY exception reading settings degrades to False rather
    than raising into the merge ritual.
    """
    env = os.environ.get(_ENV_OVERRIDE)
    if env is not None:
        return env.strip().lower() in _TRUTHY

    root = Path(project_root) if project_root is not None else Path.cwd()
    try:
        if (root / _ARM_MARKER_REL).exists():
            return True
    except OSError:
        pass

    try:
        from fno.config import load_settings

        return bool(load_settings().config.auto_continue.enabled)
    except Exception as exc:  # noqa: BLE001 - fail-safe to disabled (AC2-ERR)
        # Diagnosable without changing the safety posture: false-disabled is
        # strictly safer than false-enabled for a background dispatcher, but a
        # silent swallow would hide a genuinely-broken settings load from an
        # operator wondering why the chain never advances.
        _LOG.debug("auto_continue_enabled: settings read failed, defaulting off: %s", exc)
        return False


# Decision-event kinds (registered in docs/architecture/events-schema.yaml).
EVENT_DISPATCHED = "advance_dispatched"
EVENT_SKIPPED = "advance_skipped"
EVENT_FAILED = "advance_failed"
_EVENT_SOURCE = "backlog"


# (decision, event) pairs that are legal to construct. Guards against a refactor
# minting a mismatched result (e.g. decision="dispatched" with EVENT_SKIPPED)
# that would then emit the wrong event kind.
_VALID_DECISION_EVENTS = {
    ("dispatched", EVENT_DISPATCHED),
    ("skipped", EVENT_SKIPPED),
    ("failed", EVENT_FAILED),
}


@dataclass(frozen=True)
class AdvanceResult:
    """Outcome of one advance() run. ``event`` is the single kind emitted."""

    decision: str  # "dispatched" | "skipped" | "failed"
    event: str
    reason: Optional[str] = None  # skip reason / failure category
    node_id: Optional[str] = None
    short_id: Optional[str] = None
    detail: Optional[str] = None

    def __post_init__(self) -> None:
        # Make an invalid (decision, event) combination a loud construction
        # failure rather than a silently-wrong emitted event kind.
        if (self.decision, self.event) not in _VALID_DECISION_EVENTS:
            raise ValueError(
                f"invalid AdvanceResult (decision, event): "
                f"({self.decision!r}, {self.event!r})"
            )


# The discriminator `fno agents spawn` prints on a name collision (exit 2). Kept
# as a named constant so a future spawn-verb message change has one grep hit.
_SPAWN_ALREADY_EXISTS = "already exists"


class SpawnAlreadyRunning(RuntimeError):
    """A peer dispatcher / live worker already owns this node's launch."""


class SpawnError(RuntimeError):
    """``fno agents spawn`` failed for a reason that leaves the node re-dispatchable."""


# ---------------------------------------------------------------------------
# Seams (subprocess to the public CLI; patched in unit tests)
# ---------------------------------------------------------------------------


def _next_node(project: Optional[str]) -> Optional[dict]:
    """Return the next ready node summary (or None), via ``fno backlog next``.

    Project-scoped (Open Question 2 RESOLVED: the same selection bare megawalk
    uses). Raises on a non-zero/garbled response so advance skips rather than
    guessing a node (Failure Modes: Errors).
    """
    cmd = ["fno", "backlog", "next"]
    if project:
        cmd += ["--project", project]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    if proc.returncode != 0:
        raise RuntimeError(
            f"fno backlog next exited {proc.returncode}: {proc.stderr.strip()[:200]}"
        )
    out = (proc.stdout or "").strip()
    if not out or out == "null":
        return None
    node = json.loads(out)
    if not isinstance(node, dict) or not node.get("id"):
        raise RuntimeError(f"fno backlog next returned an unexpected shape: {out[:200]}")
    # `fno backlog next` omits `_resolved_cwd` (the work-map-resolved project
    # root); only `fno backlog get` derives it. Enrich best-effort so the worker
    # launches from the mapped root rather than a raw/misscoped recorded cwd
    # (codex P2). A get failure is non-fatal - _spawn_worker falls back to .cwd.
    if not node.get("_resolved_cwd"):
        try:
            gp = subprocess.run(
                ["fno", "backlog", "get", node["id"]],
                capture_output=True, text=True, timeout=30,
            )
            if gp.returncode == 0 and (gp.stdout or "").strip():
                full = json.loads(gp.stdout)
                if isinstance(full, dict) and full.get("_resolved_cwd"):
                    node["_resolved_cwd"] = full["_resolved_cwd"]
        except Exception:  # noqa: BLE001 - best-effort enrichment
            pass
    return node


def _name_slug(raw: Optional[str]) -> str:
    """Normalize a slug/title tail to match the shell dispatchers byte-for-byte.

    Mirrors the pipeline in skills/target/scripts/dispatch-node.sh and
    skills/agent/scripts/normalize.sh: lowercase, replace any non-[a-z0-9-] run
    with a hyphen, collapse repeats, strip, then ``cut -c1-30`` and trim a
    trailing hyphen. The 30-char cut matters: graph slugs are capped at 48
    (``slug._LEN_CAP``), so an un-truncated tail here would diverge from the
    shell name and defeat the same-name spawn-collision dedup (codex P2 /
    gemini HIGH on PR #525).
    """
    if not raw:
        return ""
    s = re.sub(r"-+", "-", re.sub(r"[^a-z0-9-]", "-", raw.lower())).strip("-")
    return s[:30].rstrip("-")


def _worker_agent_name(node_id: str, node_slug: Optional[str]) -> str:
    """Provenance-carrying bg worker name: ``target-<full-node-id>-<slug>``.

    Mirrors skills/target/scripts/dispatch-node.sh: the ``target`` verb prefix
    plus the full node id and the node's title-derived slug (sanitized via
    ``_name_slug``), so the thread title reads at a glance (e.g.
    ``target-ab-4040eee8-cargo-bootstrapper``). A node with no usable slug
    degrades to ``target-<full-node-id>``.
    """
    base = f"target-{node_id}"
    slug = _name_slug(node_slug)
    return f"{base}-{slug}" if slug else base


def _spawn_worker(
    node_id: str, node_cwd: Optional[str], node_slug: Optional[str] = None
) -> str:
    """Dispatch a fire-and-forget ``/target no-merge <id>`` claude bg worker.

    Mirrors skills/target/scripts/dispatch-node.sh exactly: ``no-merge`` rides
    as a command token (NOT an env var; the shipped sibling proves this is the
    reliable channel), the agent is named ``target-<full-node-id>-<slug>`` (see
    ``_worker_agent_name``), and the cwd resolves to the node's recorded root
    (``--cwd``) or canonical main (``--fresh``).

    Returns the spawn receipt's short_id. Raises SpawnAlreadyRunning on a
    name-collision (a peer beat us in the boot window) and SpawnError otherwise.
    """
    agent_name = _worker_agent_name(node_id, node_slug)
    cmd = ["fno", "agents", "spawn", "--provider", "claude"]
    if node_cwd:
        cmd += ["--cwd", node_cwd]
    else:
        cmd += ["--fresh"]
    cmd += [agent_name, f"/target no-merge {node_id}"]

    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    if proc.returncode != 0:
        stderr = (proc.stderr or "").strip()
        if proc.returncode == 2 and _SPAWN_ALREADY_EXISTS in stderr:
            raise SpawnAlreadyRunning(f"agent {agent_name} already exists")
        raise SpawnError(
            f"fno agents spawn exited {proc.returncode}: "
            f"{(stderr or proc.stdout or '').strip()[:200]}"
        )
    # Receipt is one compact JSON line on clean stdout: {"name", "short_id", ...}.
    # Keep scanning past a line that merely MENTIONS short_id but is not the JSON
    # receipt (banner/log noise) - only stop once a short_id is actually parsed.
    short_id = ""
    for line in (proc.stdout or "").splitlines():
        if '"short_id"' in line:
            try:
                short_id = json.loads(line).get("short_id", "")
            except json.JSONDecodeError:
                continue
            if short_id:
                break
    if not short_id:
        raise SpawnError(
            f"fno agents spawn exit 0 but no short_id receipt: "
            f"{(proc.stdout or proc.stderr or '').strip()[:200]}"
        )
    return short_id


# ---------------------------------------------------------------------------
# Claim helpers (route each key like the `fno claim` CLI's _node_aware_root)
# ---------------------------------------------------------------------------


def _claims_root_for(key: str):
    """node:* claims live in the global ($HOME) root; others use the cwd/env
    default (canonical repo root, honoring FNO_CLAIMS_ROOT). Mirrors
    fno.claims.cli._node_aware_root so advance reads/writes the SAME
    location the init shell + the Rust walker use."""
    if key.startswith("node:"):
        from fno.claims.io import global_claims_root

        return global_claims_root()
    return None


def _walker_key() -> str:
    """``walker:<canonical_repo_root>`` - byte-identical to the key the Rust
    megawalk loop writes (loop_megawalk.rs)."""
    from fno.paths import resolve_canonical_repo_root

    return f"walker:{resolve_canonical_repo_root()}"


def _claim_is_live(key: str) -> bool:
    from fno.claims.core import claim_status

    try:
        return claim_status(key, root=_claims_root_for(key)).get("state") == "live"
    except Exception:  # noqa: BLE001 - a probe error must not crash advance
        return False


def _safe_release(key: str, holder: str, root) -> None:
    """Release a claim, swallowing any error.

    ``release_claim`` is best-effort by intent but NOT contractually no-raise
    (an OSError on unlink, say, can still escape). It is called on the
    spawn-failure path BEFORE the decision event is emitted, so a raising
    release would both lose the decision event (LD#12 / AC1-UI) and leak the
    reservation. Making the release truly non-raising keeps "exactly one
    decision event, always" an invariant rather than a happy-path hope.
    """
    from fno.claims.core import release_claim

    try:
        release_claim(key, holder, root=root)
    except Exception as exc:  # noqa: BLE001
        _LOG.debug("advance: dispatch-reservation release failed for %s: %s", key, exc)


# ---------------------------------------------------------------------------
# Event emission (non-fatal; exactly one per run - LD#7 / LD#12 / AC1-UI)
# ---------------------------------------------------------------------------


def _events_path(project_root: Optional[Path]) -> Path:
    root = Path(project_root) if project_root is not None else Path.cwd()
    return root / ".fno" / "events.jsonl"


def _emit(kind: str, data: dict, events_path: Path) -> None:
    """Best-effort event emit. Never raises (LD#7: never wedge the host op)."""
    try:
        from fno.events import _build, append_event

        append_event(_build(kind, _EVENT_SOURCE, data), events_path)
    except Exception as exc:  # noqa: BLE001
        print(f"advance: WARNING: event emit failed ({kind}): {exc}", file=sys.stderr)


# ---------------------------------------------------------------------------
# advance() - the decision matrix
# ---------------------------------------------------------------------------


def advance(
    *,
    closed_node_id: Optional[str] = None,
    project: Optional[str] = None,
    project_root: Optional[Path] = None,
    events_path: Optional[Path] = None,
    verbose: bool = False,
) -> AdvanceResult:
    """Dispatch the next now-unblocked node, if armed and unclaimed.

    Invoked ONLY after the node-close write commits (keyed by ``closed_node_id``,
    AC1-RACE), so within one reconcile/post-merge run the closed node is already
    reflected before ``next`` is read. Emits exactly one decision event and is
    strictly non-fatal: any failure resolves to advance_failed/advance_skipped
    and the host op continues.
    """
    ev_path = events_path if events_path is not None else _events_path(project_root)

    def skip(reason: str, *, node_id: Optional[str] = None, detail: Optional[str] = None) -> AdvanceResult:
        data: dict = {"reason": reason}
        if closed_node_id:
            data["closed_node_id"] = closed_node_id
        if node_id:
            data["node_id"] = node_id
        if detail:
            data["detail"] = detail[:200]
        _emit(EVENT_SKIPPED, data, ev_path)
        return AdvanceResult("skipped", EVENT_SKIPPED, reason=reason, node_id=node_id, detail=detail)

    def failed(node_id: str, error: str) -> AdvanceResult:
        data = {"node_id": node_id, "error": error[:200]}
        if closed_node_id:
            data["closed_node_id"] = closed_node_id
        _emit(EVENT_FAILED, data, ev_path)
        return AdvanceResult("failed", EVENT_FAILED, reason="spawn-failed", node_id=node_id, detail=error)

    # 1. Armed?
    if not auto_continue_enabled(project=project, project_root=project_root):
        return skip("disabled")

    # 2. A live walk already owns this project -> let it pick the node up.
    if _claim_is_live(_walker_key()):
        return skip("walker-live")

    # 3. Next ready node (project-scoped). Never guess on error.
    try:
        node = _next_node(project)
    except Exception as exc:  # noqa: BLE001
        return skip("next-error", detail=str(exc))
    if node is None:
        return skip("no-work")
    node_id = node["id"]
    node_cwd = node.get("_resolved_cwd") or node.get("cwd") or None

    # 4. Already being worked? A live node:<id> claim means a worker is running;
    #    a live dispatch:<id> reservation means a peer advance is mid-flight (its
    #    bridge token still covers the boot window). Either way, skip - this
    #    liveness check (not just the O_EXCL acquire below) is what dedups a
    #    same-process re-run AND a peer whose reservation already exists.
    if _claim_is_live(f"node:{node_id}") or _claim_is_live(f"dispatch:{node_id}"):
        return skip("already-claimed", node_id=node_id)

    # 5. Reserve dispatch:<id> (O_EXCL dedup + boot-window bridge token).
    from fno.claims.core import ClaimHeldByOther, acquire_claim

    dispatch_key = f"dispatch:{node_id}"
    holder = f"advance:{os.getpid()}"
    dispatch_root = _claims_root_for(dispatch_key)
    try:
        acquire_claim(
            dispatch_key,
            holder,
            ttl_ms=_DISPATCH_TTL_MS,
            reason=f"auto-continue dispatch for {node_id}",
            root=dispatch_root,
        )
    except ClaimHeldByOther:
        return skip("already-claimed", node_id=node_id)
    except Exception as exc:  # noqa: BLE001
        return skip("claim-error", node_id=node_id, detail=str(exc))

    # 6. Spawn the worker. On any failure, release the reservation so the node
    #    stays re-dispatchable (a later reconcile retries - AC2-FR). The release
    #    is non-raising (_safe_release) so the decision event below always lands.
    try:
        short_id = _spawn_worker(node_id, node_cwd, node.get("slug") or node.get("title"))
    except SpawnAlreadyRunning:
        _safe_release(dispatch_key, holder, dispatch_root)
        return skip("already-claimed", node_id=node_id)
    except Exception as exc:  # noqa: BLE001
        _safe_release(dispatch_key, holder, dispatch_root)
        return failed(node_id, str(exc))

    # 7. Dispatched. Leave dispatch:<id> to expire by TTL: the worker now owns
    #    (or is acquiring) node:<id>, which guards later dispatches.
    _emit(
        EVENT_DISPATCHED,
        {
            "node_id": node_id,
            "short_id": short_id,
            "agent_name": _worker_agent_name(node_id, node.get("slug") or node.get("title")),
            **({"closed_node_id": closed_node_id} if closed_node_id else {}),
        },
        ev_path,
    )
    if verbose:
        print(f"advance: dispatched {node_id} -> target worker {short_id}", file=sys.stderr)
    return AdvanceResult("dispatched", EVENT_DISPATCHED, node_id=node_id, short_id=short_id)
