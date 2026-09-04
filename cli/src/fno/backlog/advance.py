"""fno backlog advance - merge-triggered auto-continue dispatcher.

tracker-owned machinery: every entry path into this module is a tracker-owned
backlog verb behind the shared external-backend refusal, so its direct graph
reads are census-classified as guarded machinery
(scripts/diagnostics/tracker-consumers.py --reads).

Node ab-3cd195b6. When a backlog node's PR merges, a merge-detector
(``fno backlog reconcile`` or the /pr merged skill) calls this verb after the
node-close write commits. If auto-continue is armed for the project and no live
walk owns it, advance dispatches a fresh background ``/target`` worker (with the
merge posture from ``config.auto_merge.grant``, default ``none``) for the
next now-unblocked node, so a merge-gated epic walks itself group-by-
group across merges with no manual re-invocation.

Locked Decisions this module embodies:
  1. Decoupled from the loop driver - driven by the merge event, so megawalk /
     /target / /megatron all inherit auto-continue (no driver-specific code).
  4. Fire-and-forget dispatch: ``fno agents spawn`` -> ``/target [--no-merge] <id>``
     (the ``--no-merge`` flag is gated on ``config.auto_merge.grant``; x-4391/x-4be1).
  5. Concurrency via ``fno agents claim``: honor ``walker:<root>`` (no double-dispatch
     during a live walk); reserve ``dispatch:<id>`` (O_EXCL dedup + bridge token
     that outlives this short-lived process until the worker owns ``node:<id>``,
     LD#11 / AC1-CLAIM - mirrors handoff.sh + dispatch-node.sh).
  6. advance never merges the PR itself - it dispatches a worker whose merge
     posture comes from ``config.auto_merge.grant`` (default ``none``);
     an actual merge, when enabled, is still gated by the worker's own
     ``config.auto_merge.*`` review layer (x-4391, revisits epic LD#4).
  7. Non-fatal: a failed spawn never wedges the host op (reconcile/post-merge).
 12. Every code path emits EXACTLY ONE decision event before returning
     (advance_dispatched | advance_skipped{reason} | advance_failed), so a
     silent stall is impossible.

The ``dispatch:<id>`` reservation uses a TTL claim (not PID-liveness) precisely
so it survives advance's exit (AC1-CLAIM): the just-dispatched node stays
"claimed" for the boot window, so a concurrent reconcile/post-merge sees it as
already-being-worked. The spawned worker acquires ``node:<id>`` cleanly on its
own ``fno do target init`` (free at that point); the reservation then expires by
TTL once the worker owns the node.
"""

from __future__ import annotations

import json
import logging
import os
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Literal, NamedTuple, Optional

from fno import _subprocess_util
from fno import route_resolve as _route_resolve
from fno.agents.naming import agent_name
from fno.provenance import autobrief as _autobrief

_LOG = logging.getLogger(__name__)

# Override knob (Claude's Discretion #4: retained as the highest-precedence
# explicit override for tests + same-process force-enable/disable). The
# campaign-arm marker file rank (`.fno/.auto-continue-armed`) was removed
# 2026-08 (x-aaaf wave 1): its documented writer, "/megawalk auto-continue",
# no longer exists (skills/megawalk is deleted), so the rank had no writer and
# no expiry while silently outranking the live config key.
_ENV_OVERRIDE = "FNO_AUTO_CONTINUE"

# Mirror handoff.sh / dispatch-node.sh: a 3-minute TTL bridge token covers the
# spawn->worker-init boot window. TTL (not PID) liveness is mandatory so the
# reservation outlives this process (LD#11 / AC1-CLAIM).
_DISPATCH_TTL_MS = 180_000  # 3m

_TRUTHY = frozenset({"1", "true", "yes", "on"})


def _auto_continue_resolve(
    project_root: Optional[Path] = None,
) -> tuple[bool, str]:
    """Resolve auto-continue's armed state AND which precedence rank supplied it.

    Precedence (highest first):
      0. ``config.autonomy.enabled`` master switch off -> rank "autonomy".
         Checked BEFORE the env override: a panic switch something else can
         bypass is not a panic switch (x-aaaf wave 3).
      1. ``FNO_AUTO_CONTINUE`` env override (explicit force on/off) -> rank "env".
      2. ``config.auto_continue.enabled`` from settings.yaml (local>global via
         load_settings deep-merge) -> rank "config".
      3. default False -> rank "default".

    The rank is stamped onto every dispatch decision event (x-aaaf wave 1,
    AC3-HP) so "what armed this" is answerable from the log instead of by
    inference - the gap that made the 2026-06-20 to 06-26 event window
    unattributable.

    Fail-safe (AC2-ERR): ANY exception reading settings degrades to
    (False, "default") rather than raising into the merge ritual.
    """
    from fno.config import autonomy_master_enabled

    if not autonomy_master_enabled(project_root):
        return False, "autonomy"

    env = os.environ.get(_ENV_OVERRIDE)
    if env is not None:
        return env.strip().lower() in _TRUTHY, "env"

    try:
        from fno.config import load_settings, load_settings_for_repo

        settings = (
            load_settings_for_repo(Path(project_root)) if project_root else load_settings()
        )
        return bool(settings.auto_continue.enabled), "config"
    except Exception as exc:  # noqa: BLE001 - fail-safe to disabled (AC2-ERR)
        # Diagnosable without changing the safety posture: false-disabled is
        # strictly safer than false-enabled for a background dispatcher, but a
        # silent swallow would hide a genuinely-broken settings load from an
        # operator wondering why the chain never advances.
        _LOG.debug("auto_continue_enabled: settings read failed, defaulting off: %s", exc)
        return False, "default"


def auto_continue_enabled(
    project: Optional[str] = None,
    project_root: Optional[Path] = None,
) -> bool:
    """Resolve whether auto-continue is armed for this project.

    See :func:`_auto_continue_resolve` for the precedence chain. This wrapper
    drops the rank for callers that only need the boolean.
    """
    return _auto_continue_resolve(project_root)[0]


# Decision-event kinds (registered in cli/src/fno/events/schema.yaml).
EVENT_DISPATCHED = "advance_dispatched"
EVENT_SKIPPED = "advance_skipped"
EVENT_FAILED = "advance_failed"
# x-0676: paired receipt (not a decision) emitted just before an advance_dispatched
# when on_exhaustion=failover rotates off an exhausted provider.
EVENT_SPAWNED = "dispatch_spawned"
EVENT_FAILOVER = "dispatch_failover"
EVENT_CLAIM_OBSERVED = "dispatch_claim_observed"
EVENT_DEAD_FAILURE_LIMIT = "dispatch_dead_failure_limit"
EVENT_SELECTION_DIVERGED = "dispatch_selection_diverged"
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


@dataclass(frozen=True)
class DispatchClaimObservation:
    """Structured family-2 decision shared by every node-dispatch caller."""

    verdict: str
    claim_state: Optional[str]
    holder: str
    truth_status: str
    action: str

    @property
    def blocks_dispatch(self) -> bool:
        return self.action in ("blocked", "auto-deferred", "defer-failed")

    @property
    def refusal_reason(self) -> Optional[str]:
        if self.action == "blocked":
            return "already-claimed"
        return self.action if self.blocks_dispatch else None


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


# Bound so a pathological `parent` chain (or an undetected cycle) can never spin
# the dead-ancestor walk. Deeper than any real epic nesting.
_MAX_ANCESTOR_WALK = 64


def selection_guards(
    entry: dict,
    entries_by_id: dict,
    now=None,
    *,
    staleness_days: int = 21,
) -> Optional[str]:
    """Return a skip-reason for a would-be-selected node, or None to select it.

    The single narrowing choke point shared by ``next`` selection (_pick_ready)
    and the converge readiness filter (_direct_dependents), so the two paths
    can never disagree about what is dispatchable. Guards, in order:

      contained: the node carries ``contained_in`` - its work ships inside
        another node's PR, so it is not a delivery unit and dispatching it
        would open a second PR for one plan. Returns ``contained:<owner-id>``,
        which names where the work actually went (``dead-ancestor`` would only
        say the subtree is dead). First, because containment is a fact about
        THIS node while every guard below reads its ancestors or its plan.
        Belt-and-braces: the write-site refusal (x-d9a4) already stops new
        double-bindings, so this is a read of a state that should not exist.
        It is also only HALF the coverage - selection_guards is autonomous-only
        (see the design-stage note below), so `fno do target init` carries the
        named-dispatch half. A guard on one of two reachable paths is
        decorative.

      dead-ancestor: any transitive ``parent`` in {superseded, deferred} - the
        subtree is abandoned, so building a leaf under a killed epic is wasted
        work. Returns ``dead-ancestor:<ancestor-id>``. A missing parent id ends
        the walk with no verdict (select normally). Depth-bounded + cycle-safe.

      stale-quarantine: a ready node with no movement signal older than
        ``staleness_days`` -> ``stale-quarantine``. The guard only EXCLUDES
        here; the reversible defer is owned by ``maintain --apply`` (guards
        never mutate the graph as a selection side effect - epic LD1/LD2).

      design-stage: the linked plan is still a design doc (frontmatter
        ``status: design``), so the node is planned but not blueprinted ->
        ``design-stage``. Only AUTONOMOUS selection routes through this
        function; an explicitly-named node dispatches from any rung, naming
        being the consent (epic LD8). This is what retires the
        keep-plans-unlinked workaround: linking a design doc now lands a
        visible-but-unarmed node instead of arming dispatch.

    Hold reads fail closed before the compatibility guard: an unreadable plan
    cannot prove a hold is absent. Every remaining guard stays fail-open (epic
    Errors): a read failure returns None and emits one loud stderr line.
    """
    from datetime import datetime, timezone

    # The plan hold is the one fail-CLOSED policy in this selector. A present
    # plan whose hold state is malformed or unreadable cannot prove that its
    # hold is absent, so it must never fall through the broad fail-open
    # compatibility guard below. The helper also checks parents and the
    # contained delivery owner.
    from fno.graph.ladder import dispatch_hold_verdict

    hold = dispatch_hold_verdict(entry, entries_by_id)
    if hold is not None:
        return hold.guard_reason

    try:
        owner = entry.get("contained_in")
        if isinstance(owner, str) and owner:
            return f"contained:{owner}"

        seen: set[str] = set()
        cur = entry.get("parent")
        steps = 0
        while cur and steps < _MAX_ANCESTOR_WALK:
            if cur in seen:
                break  # cycle - stop, no verdict
            seen.add(cur)
            anc = entries_by_id.get(cur)
            if anc is None:
                break  # missing parent - no verdict, select normally
            # Field-based, not just derived `status`: read_graph returns the
            # persisted status and does NOT recompute, so a superseded/deferred
            # ancestor whose `status` was not re-persisted still reads its own
            # bucket here via the underlying fields. Checking both is robust to
            # either read path.
            if (
                anc.get("status") in ("superseded", "deferred")
                or anc.get("superseded_by")
                or anc.get("deferred_at")
            ):
                return f"dead-ancestor:{cur}"
            cur = anc.get("parent")
            steps += 1

        from fno.graph import maintain as _maintain
        from fno.graph.ladder import UNSELECTABLE_RUNGS, Rung, plan_rung

        if now is None:
            now = datetime.now(timezone.utc)

        # BEFORE the stale check: an undesigned node is unarmed by design, so it
        # accrues none of the movement signals (sessions, pr_number, claims)
        # that dispatch used to supply, and would age into `stale-quarantine` -
        # reporting the wrong reason and letting `maintain --apply` auto-defer
        # a perfectly healthy design doc off the board.
        #
        # Keys on the RUNG, not on `is_design_stage`, because the persisted
        # `ready` above it can be stale: a plan doc is external mutable state
        # that `/blueprint` (or a hand edit) rewrites without touching the graph,
        # and `read_graph` does not recompute. A doc rewritten down to `idea` -
        # or an old scaffold still spelled `stub` - therefore sits behind a
        # `ready` row, and a DESIGN-only probe waves it straight through to
        # dispatch. Re-probing live is the whole reason this guard exists; it has
        # to ask about every undesigned rung, not just one of them.
        #
        # One `plan_rung` call, shared with the policy set, so the reason stays
        # rung-specific without a second filesystem read per candidate.
        if entry.get("status") == "ready":
            rung = plan_rung(entry)
            if rung in UNSELECTABLE_RUNGS:
                return "design-stage" if rung is Rung.DESIGN else "idea-stage"

        if entry.get("status") == "ready" and _maintain.is_stale_ready(
            entry, now, staleness_days
        ):
            return "stale-quarantine"
        return None
    except Exception as exc:  # noqa: BLE001 - fail OPEN, never starve on a guard bug
        sys.stderr.write(
            f"warning: selection_guards failed for {entry.get('id')!r}, "
            f"selecting anyway: {exc}\n"
        )
        return None


def _guard_staleness_days() -> int:
    """``config.backlog.staleness_days`` (default 21), fail-open to the default.

    A config read error must never wedge selection, so a bad/missing config
    degrades to the schema default rather than raising into the picker.
    """
    try:
        from fno.config import load_settings

        return load_settings().backlog.staleness_days
    except Exception:  # noqa: BLE001 - selection must survive a bad config
        return 21


def _next_node(project: Optional[str]) -> Optional[dict]:
    """Return the next ready node summary (or None), via ``fno backlog next``.

    Project-scoped (Open Question 2 RESOLVED: the same selection bare megawalk
    uses). Raises on a non-zero/garbled response so advance skips rather than
    guessing a node (Failure Modes: Errors).
    """
    cmd = [*_subprocess_util.fno_py_cmd(), "backlog", "next"]
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
                [*_subprocess_util.fno_py_cmd(), "backlog", "get", node["id"]],
                capture_output=True, text=True, timeout=30,
            )
            if gp.returncode == 0 and (gp.stdout or "").strip():
                full = json.loads(gp.stdout)
                if isinstance(full, dict) and full.get("_resolved_cwd"):
                    node["_resolved_cwd"] = full["_resolved_cwd"]
        except Exception:  # noqa: BLE001 - best-effort enrichment
            pass
    return node


# A node with no `domain` set collapses into ONE bucket in `_live_lane_domains`
# seeding. Since domain stopped excluding candidates this affects only
# the `+same-domain` annotation - and the classifier skips the annotation for an
# unset domain, so it never emits a bare `+same-domain:` suffix.
_DOMAIN_UNSET = ""


def _live_lane_domains(*, claims_root: Optional[Path] = None) -> set[str]:
    """Domains currently held by live lane slots, seeding the domain annotation.

    Since the guard reorder, domain no longer excludes a candidate from
    lane fill - the file-collision gate decides - but the ``+same-domain``
    annotation on an unevaluated candidate is only truthful if the seed reads
    the live-claim world, not just this call's own picks. Each lane records its
    ``domain`` in slot metadata at acquire time, so peer-lane domains are
    readable here without a per-node lookup. A slot with no recorded domain
    (e.g. one taken via a bare ``fno agents claim acquire --lane`` CLI) collapses to the
    ``_DOMAIN_UNSET`` bucket.
    """
    from fno.claims.core import list_claims
    from fno.claims.lanes import LANE_SLOT_PREFIX

    domains: set[str] = set()
    for claim in list_claims(prefix=LANE_SLOT_PREFIX, root=claims_root):
        meta = claim.get("metadata") or {}
        domains.add(meta.get("domain") or _DOMAIN_UNSET)
    return domains


_NODE_CLAIM_PREFIX = "node:"


def _live_worked_entries(claims_root: Optional[Path] = None) -> list[dict]:
    """Collision-comparable graph entries for every node a live worker holds.

    Two claim shapes count as in flight, because both mean somebody is editing
    those files: a ``lane-slot:`` holder (a peer lane) and a bare ``node:<id>``
    claim (a manually started or non-lane ``/target``, which holds no slot).
    Reading only lane slots would leave the gate blind to every hand-run worker.

    Entries pass through with their real fields: ``find_collisions`` rejects
    anything done/deferred/superseded itself, and a claim outliving its node
    (a corpse claim) is exactly the case that filter exists for - synthesizing a
    ready status here would resurrect a finished node into the comparison set and
    let it block a dispatchable one.
    """
    from fno.claims.core import list_claims
    from fno.claims.lanes import LANE_HOLDER_PREFIX, LANE_SLOT_PREFIX
    from fno.graph.collision import has_file_surface, resolve_plan_path
    from fno.graph.store import read_graph
    from fno.paths import graph_json

    held: set[str] = set()
    for claim in list_claims(prefix=LANE_SLOT_PREFIX, root=claims_root):
        holder = claim.get("holder") or ""
        if holder.startswith(LANE_HOLDER_PREFIX):
            held.add(holder[len(LANE_HOLDER_PREFIX):])
    for claim in list_claims(prefix=_NODE_CLAIM_PREFIX, root=claims_root):
        key = claim.get("key") or ""
        if key.startswith(_NODE_CLAIM_PREFIX):
            held.add(key[len(_NODE_CLAIM_PREFIX):])
    if not held:
        return []
    entries = [
        e for e in read_graph(graph_json())
        if e.get("id") in held and e.get("plan_path")
    ]
    # A comparator with no readable surface is skipped inside find_collisions,
    # so the gate would read clean without ever having compared against it.
    # Same unevaluated-is-not-clean rule the candidate side follows.
    comparable = [e for e in entries if has_file_surface(resolve_plan_path(e["plan_path"]))]
    if len(comparable) < len(held):
        _LOG.warning(
            "collision gate: %d of %d in-flight nodes have no comparable file "
            "surface; those nodes cannot be collided against",
            len(held) - len(comparable), len(held),
        )
    return comparable


def _high_collision(node: dict, inflight: list[dict]):
    """The first high-severity file overlap between ``node`` and in-flight work.

    Raises on an unreadable plan rather than failing open here. The sole caller,
    :func:`_classify_lane_candidate`, owns that guard, because a swallow at THIS
    frame returns the same ``None`` as a clean comparison and the caller cannot
    tell "compared, no overlap" from "never compared" - which is how a node whose
    collision safety was never evaluated reaches the frontier reported as clean.
    The caller has somewhere to put that distinction; this function does not.

    Assumes the caller has already established a comparable file surface (it
    checks ``has_file_surface`` before calling), so no second surface check here.
    """
    plan = node.get("plan_path")
    if not plan or not inflight:
        return None
    from fno.graph.collision import find_collisions, resolve_plan_path

    for c in find_collisions(resolve_plan_path(plan), inflight, self_id=node.get("id")):
        if c.severity == "high":
            return c
    return None


def _undispatched_nodes(
    project: Optional[str], mission: Optional[str] = None
) -> dict:
    """Read the independent planned-unclaimed observer receipt."""
    cmd = [*_subprocess_util.fno_py_cmd(), "backlog", "undispatched", "--json"]
    if project:
        cmd += ["--project", project]
    if mission:
        cmd += ["--mission", mission]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    if proc.returncode != 0:
        raise RuntimeError(
            f"fno backlog undispatched exited {proc.returncode}: {proc.stderr.strip()[:200]}"
        )
    out = (proc.stdout or "").strip()
    try:
        receipt = json.loads(out)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"fno backlog undispatched returned invalid JSON: {out[:200]}") from exc
    if (
        not isinstance(receipt, dict)
        or receipt.get("status") != "ok"
        or not isinstance(receipt.get("entries_scanned"), int)
        or not isinstance(receipt.get("rows"), list)
    ):
        raise RuntimeError("fno backlog undispatched returned an unreadable receipt")
    return receipt


def _dispatch_safe_observer(receipt: dict) -> dict:
    """Reapply selector-only safety guards before lane-fill recovery."""
    rows = receipt.get("rows", [])
    if not rows:
        return receipt
    from fno import paths
    from fno.graph.store import read_graph_strict

    try:
        entries = read_graph_strict(paths.graph_json())
    except Exception as exc:  # noqa: BLE001 - unknown safety state refuses recovery
        raise RuntimeError(f"observer safety graph unreadable: {exc}") from exc
    by_id = {entry.get("id"): entry for entry in entries}
    now = datetime.now(timezone.utc)
    staleness_days = _guard_staleness_days()
    safe: list[dict] = []
    for row in rows:
        entry = by_id.get(row.get("id"))
        if entry is None:
            continue
        facts = row.get("facts")
        if not isinstance(facts, dict):
            raise RuntimeError(f"observer row {row.get('id')!r} has no predicate facts")
        if facts.get("has_pr") or facts.get("batch_owner") or facts.get("completed"):
            continue
        if selection_guards(entry, by_id, now, staleness_days=staleness_days):
            continue
        safe.append(row)
    return {**receipt, "rows": safe}


def _ready_nodes(
    project: Optional[str],
    mission: Optional[str] = None,
    *,
    events_path: Optional[Path] = None,
) -> list[dict]:
    """Ordered ready-node summaries with independent omission recovery.

    The normal ranked list remains the selector. The independent observer is
    compared with it so a named omission is recovered before lane-fill applies
    its existing claim, collision, and spawn guards. Raises on a
    garbled response so the caller skips rather than guessing.
    ``mission`` restricts to that mission's nodes, mirroring the sequential
    path's ``MegawalkQueue::with_mission`` (codex P1 on PR #137).
    """
    cmd = [*_subprocess_util.fno_py_cmd(), "backlog", "ready"]
    if project:
        cmd += ["--project", project]
    if mission:
        cmd += ["--mission", mission]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    if proc.returncode != 0:
        raise RuntimeError(
            f"fno backlog ready exited {proc.returncode}: {proc.stderr.strip()[:200]}"
        )
    out = (proc.stdout or "").strip()
    if not out or out == "null":
        normal = []
    else:
        nodes = json.loads(out)
        if not isinstance(nodes, list):
            raise RuntimeError(f"fno backlog ready returned an unexpected shape: {out[:200]}")
        normal = [n for n in nodes if isinstance(n, dict) and n.get("id")]
    observer = _dispatch_safe_observer(_undispatched_nodes(project, mission))
    from fno.backlog.undispatched import prepend_missed_rows

    merged, missed = prepend_missed_rows(normal, observer)
    if missed:
        scope = f"project={project or '*'}"
        if mission:
            scope += f",mission={mission}"
        ev_path = events_path if events_path is not None else _events_path(None)
        for row in missed:
            _emit(
                EVENT_SELECTION_DIVERGED,
                {
                    "node_id": row["id"],
                    "selector_command": "fno backlog ready --json",
                    "observer_command": "fno backlog undispatched --json",
                    "scope": scope,
                    "selector_entries_scanned": len(normal),
                    "observer_entries_scanned": observer["entries_scanned"],
                },
                ev_path,
            )
    return merged


def select_lane_fill(
    max_lanes: int,
    project: Optional[str] = None,
    *,
    mission: Optional[str] = None,
    claim: bool = True,
    claims_root: Optional[Path] = None,
    report: Optional[dict] = None,
) -> list[dict]:
    """Select up to ``max_lanes`` ready nodes, each collision-clean to dispatch.

    The parallel-mode (epic x-42d5, group 2) lane-fill selector. With
    ``claim=True`` each pick atomically acquires a dispatch-time lane slot (the
    group-1 primitive ``acquire_lane_slot``), so the concurrency cap is enforced
    by claim atomicity, never a counted snapshot (Locked Decision #7). Each
    returned node already holds a slot keyed ``parallel-lane:<id>``; the caller
    spawns one worker per node and the worker's ``target init`` reconciles that
    same slot (Locked Decision #8) rather than acquiring a fresh one.

    Collision-cleanliness is recomputed AFTER each claim from a FRESH ready-list,
    never a pre-claim snapshot: between two picks a peer may claim a node or a
    lane may finish, and re-querying reflects that. This is the x-7441 "stops at
    a claimed head" hazard - selection must skip claimed heads across every
    domain. A node a live peer lane already holds is skipped so a
    not-yet-node-claimed lane is never double-dispatched. (Two dispatchers
    racing the SAME node are prevented upstream by the singleton
    ``walker:<root>`` claim, so this stays a single-dispatcher selector, not a
    distributed lock.)

    Domain is NOT a selection rule: the file-collision gate decides,
    so two same-domain nodes with disjoint surfaces co-schedule. What remains
    of domain is the annotation on an unevaluated candidate - see
    :func:`_classify_lane_candidate`.

    ``max_lanes == 1`` selects a single ready node: this is the retargeted
    active_backlog daemon's sequential
    fire-and-forget dispatch (x-0ad6). ``max_lanes < 1`` returns ``[]`` with no
    side effects.

    ``claim=False`` previews the selection (which nodes WOULD dispatch) without
    holding any slot - the read-only mode, mirroring ``fno backlog next`` sans
    ``--claim``.

    ``claim=True`` assumes the caller runs under the singleton ``walker:<root>``
    claim (the dispatch context does): that serialization is what prevents two
    concurrent callers from both selecting the SAME node and each grabbing a
    distinct slot for it (which would inflate the cap - the group-1 primitive is
    idempotent only for a single caller's retries). It is NOT a standalone
    distributed lock; do not run two ``--claim`` selectors concurrently outside
    the walker.
    """
    from fno.claims.lanes import acquire_lane_slot, release_lane_slot

    if report is not None:
        report.clear()
        report.update({
            "requested": max_lanes,
            "filled": 0,
            "stop": "no-candidate",
            "excluded": [],
        })

    if max_lanes < 1:
        return []

    selected: list[dict] = []
    # Seed from domains already held by live lanes (peer lanes from prior ticks).
    # Domain no longer excludes a candidate - it feeds only the
    # `+same-domain` annotation on an unevaluated one - but the seed keeps that
    # annotation truthful across ticks, not just within this call.
    # The peer-lane set is stable within a single-dispatcher call (the singleton
    # walker:<root> claim serializes dispatchers), so it is seeded once here; this
    # call's own picks are added below as they are acquired. Fails open like the
    # in-flight seed below: a read fault must not kill the dispatch round to
    # protect a log suffix.
    try:
        used_domains: set[str] = _live_lane_domains(claims_root=claims_root)
    except Exception as exc:  # noqa: BLE001 - annotation-only seed; fail open
        _LOG.warning(
            "lane-fill: live-lane domain seed unreadable (same-domain "
            "annotations may be missing): %s", exc,
        )
        used_domains = set()
    picked_ids: set[str] = set()
    # Nodes already in flight, for the file-surface collision gate below. Seeded
    # from live workers; this call's own picks are appended as they land. Seeding
    # fails open like the gate itself - a claims or graph read error must not
    # wedge dispatch, it just leaves the gate with nothing to compare against.
    try:
        inflight: list[dict] = _live_worked_entries(claims_root=claims_root)
    except Exception as exc:  # noqa: BLE001 - fail open, never wedge dispatch
        _LOG.warning("collision gate unavailable (in-flight read failed): %s", exc)
        inflight = []

    try:
        while len(selected) < max_lanes:
            # ponytail: fresh ready-list per pick is O(max_lanes * ready_count).
            # max_lanes is small (2-3) and the ready-list is short, so this is
            # cheap; if a huge backlog makes the re-query hurt, cache the list
            # and refresh only the claim-state. The fresh query is what makes
            # distinctness "recomputed after each claim" not snapshot-stale.
            candidate = None
            pick_excluded: list[dict] = []
            for node in _ready_nodes(project, mission):
                nid = node["id"]
                if nid in picked_ids:
                    continue
                reason = _classify_lane_candidate(
                    node, used_domains=used_domains, inflight=inflight,
                    claims_root=claims_root,
                )
                # Live dispatch fails OPEN on an unevaluated node (no comparable
                # file surface): it dispatches anyway, today's behavior. Only a
                # concrete exclusion (peer-lane / high-collision) holds it back.
                # The shadow report is the conservative twin - it serializes the
                # unevaluated node instead (schedule_shadow).
                if reason is not None and not reason.startswith(_UNEVALUATED_PREFIX):
                    if reason.startswith(_HIGH_COLLISION_PREFIX):
                        _LOG.warning("lane-fill: skipping %s - %s", nid, reason)
                    if report is not None and len(pick_excluded) < 5:
                        pick_excluded.append({"id": nid, "reason": reason})
                    continue  # leave it ready; reversible, retried next round
                if reason is not None and (
                    inflight
                    or selected
                    or _SAME_DOMAIN_ANNOTATION in reason
                    or (
                        not (node.get("domain") or _DOMAIN_UNSET)
                        and _DOMAIN_UNSET in used_domains
                    )
                ):
                    # Unevaluated (no comparable file surface): dispatch anyway
                    # (fail-open) but say so LOUDLY - a silent pass would read
                    # as "gate clean" when it never ran.
                    #
                    # Normally only when something is actually in flight: with
                    # nothing to collide against, an unknown surface risks
                    # nothing, and every plan-less node (which is every
                    # `backlog idea` node) would otherwise warn on every
                    # candidate of every tick. Three dispatch shapes the guard
                    # reorder turned from excluded to fail-open stay loud even
                    # with an empty in-flight set: a held domain (the
                    # annotation on the token - a surfaceless PEER drops out
                    # of inflight and would otherwise silence the riskiest
                    # case), a second unevaluated pick in THIS fill
                    # (`selected` - two unknown surfaces now run concurrently,
                    # and a plan-less pick never joins inflight to warn the
                    # next one), and a held UNSET-domain bucket cross-tick (an
                    # unset domain cannot carry the annotation, and the old
                    # empty-bucket exclusion is what used to block this pair).
                    _LOG.warning(
                        "lane-fill: %s file surface UNEVALUATED (%s) - "
                        "dispatching anyway (fail-open)", nid, reason,
                    )
                candidate = (node, node.get("domain") or _DOMAIN_UNSET)
                break
            if candidate is None:
                if report is not None:
                    report["excluded"].extend(pick_excluded)
                break  # no selectable, unclaimed node left

            node, domain = candidate
            if claim:
                slot = acquire_lane_slot(
                    max_lanes,
                    node["id"],
                    extra_metadata={"domain": domain},
                    root=claims_root,
                )
                if slot is None:
                    if report is not None:
                        report["excluded"].extend(pick_excluded)
                        report["stop"] = "cap-full"
                    break  # cap full: every slot held by a live peer lane
            selected.append(node)
            if report is not None:
                report["excluded"].extend(pick_excluded)
                report["filled"] = len(selected)
            used_domains.add(domain)
            picked_ids.add(node["id"])
            if node.get("plan_path"):
                inflight.append({
                    "id": node["id"], "title": node.get("title", ""),
                    "plan_path": node["plan_path"], "created_at": "", "status": "ready",
                })
    except BaseException:
        # A mid-loop raise (a garbled `fno backlog ready` on a LATER pick, or a
        # filesystem error during a claim probe) must not orphan the slots
        # already acquired: the caller never receives `selected`, so it cannot
        # release them, and they would sit held until TTL. Release what we hold,
        # then re-raise unchanged. Preview mode holds no slot, so this is a
        # no-op there. Each release is guarded so a secondary error cannot mask
        # the original exception or strand the remaining slots (gemini medium).
        if claim:
            for held in selected:
                try:
                    release_lane_slot(held["id"], root=claims_root)
                except Exception:  # noqa: BLE001 - best-effort cleanup
                    pass
        raise

    if report is not None and len(selected) >= max_lanes:
        report["stop"] = "filled"
    elif report is not None and report.get("stop") != "cap-full":
        report["stop"] = "no-candidate"

    return selected


# The hard ceiling on live writers per project during the initial bounded
# rollout (plan x-24f7 Change 3). Requested caps clamp up into [1, this]: a
# value below one normalizes to one (never zero writers), and any larger
# request is capped here until measured shadow evidence authorizes lifting it.
# The shadow report applies and reports this bound so an operator sees exactly
# the frontier the live scheduler will honor - it does NOT change live dispatch,
# which still reads the raw configured cap (that gate is a separate change).
_INITIAL_LIVE_CAP = 2

# The reason-token namespace for the "unknown collision safety" class, matched by
# both consumers (select_lane_fill fails open on it, schedule_shadow serializes
# it). Shared so the two prefix checks cannot drift if the token is ever renamed.
_UNEVALUATED_PREFIX = "unevaluated:"

# The domain-tiebreak annotation appended to an unevaluated token. A shared
# constant for the same reason as the two prefixes below: the classifier
# builds it and select_lane_fill's warning arm matches it, so two literals
# could drift apart and silently disarm the loud warning.
_SAME_DOMAIN_ANNOTATION = "+same-domain:"

# Same reasoning for the file-overlap token: the producer builds it and
# select_lane_fill matches it to decide how loudly to log the skip.
_HIGH_COLLISION_PREFIX = "high-collision:"


def lane_fill_filter_name(reason: Optional[str]) -> str:
    """Map one classifier reason token to its canonical lane-fill filter name.

    The canonical names the ``--explain --epic`` SELECTION section reports.
    Living HERE, beside the tokens, keeps the explainer's vocabulary from
    drifting from the selector's: both derive from the classifier's stable
    tokens, never from a second hand-written list. An unmapped token maps to
    itself (its head up to the first colon), so a future token shows up in
    the report under its own name instead of vanishing into a bucket that
    no longer matches.
    """
    if not reason:
        return ""
    if reason == "peer-lane":
        return "live-lane"
    if reason.startswith(_HIGH_COLLISION_PREFIX):
        return "in-flight-collision"
    if _SAME_DOMAIN_ANNOTATION in reason:
        return "live-lane-domain"
    if reason.startswith(_UNEVALUATED_PREFIX):
        return "unevaluated"
    return reason.split(":", 1)[0]


def _classify_lane_candidate(
    node: dict,
    *,
    used_domains: set[str],
    inflight: list[dict],
    claims_root: Optional[Path] = None,
) -> Optional[str]:
    """Classify one ready node for lane-fill. ``None`` = selectable, else a typed
    exclusion reason. Read-only (acquires no slot): the SINGLE per-candidate
    truth shared by :func:`select_lane_fill` (live) and :func:`schedule_shadow`
    (the read-only report), so the two can never disagree about why a node is
    held back. Duplicating this sequence into a second copy is the drift the
    codebase's path-uniqueness rule exists to prevent.

    Guard order: peer-lane, then collision, then domain. Domain was a proxy for
    "these will not collide"; the collision gate is the real measurement, so it
    runs first and domain NEVER excludes an evaluated candidate - two
    same-domain nodes with disjoint file surfaces co-schedule.

    Reason tokens (all stable, machine-readable):

      ``peer-lane``              a live peer lane already holds this exact node.
      ``high-collision:<id>``    a high-severity file overlap with in-flight work.
      ``unevaluated:no-surface`` the plan states no comparable file surface, so
        collision safety is UNKNOWN. This is a distinct class, not an exclusion:
        live dispatch fails open on it (dispatches anyway); the shadow report is
        conservative and serializes it with this diagnostic (plan Change 1).
        When the node's domain is already held (by a live lane or an earlier
        pick) the token carries ``+same-domain:<domain>`` - the domain tiebreak
        survives only as that annotation, so the report can still explain a
        serialized unknown. That subclass is the one behavior change inside the
        class: a held domain excluded such a candidate before the reorder and
        now it dispatches, loudly (select_lane_fill warns on the annotation),
        with the mandatory-surface intake refusal as the standing control.
      ``unevaluated:collision-error`` the collision gate raised, so safety is
        unknown for the same reason and gets the same fail-open treatment. It is
        a stated verdict rather than a swallowed error precisely so it cannot
        reach the frontier looking like a clean comparison. Carries the same
        ``+same-domain:<domain>`` annotation when the domain is held.
    """
    from fno.claims.lanes import find_lane_slot

    if find_lane_slot(node["id"], root=claims_root) is not None:
        return "peer-lane"
    domain = node.get("domain") or _DOMAIN_UNSET
    # The domain tiebreak as ONE suffix, appended to either unevaluated token
    # (an unset domain never annotates, so no bare `+same-domain:` is emitted).
    domain_suffix = (
        f"{_SAME_DOMAIN_ANNOTATION}{domain}"
        if domain and domain in used_domains
        else ""
    )
    # Unknown file state is its own verdict, not a silent pass: a node whose plan
    # states no comparable surface cannot be collision-checked, so its safety is
    # unevaluated rather than clean (plan Change 1: "serialize unknown ... state").
    from fno.graph.collision import has_file_surface, resolve_plan_path

    plan = node.get("plan_path")
    # ONE guard over the whole collision evaluation - the path resolve, the
    # surface probe, and the overlap scan. Fail-open lives here and nowhere
    # below, because this is the only frame that can express "the gate did not
    # run" as a verdict. A handler further down returns the same None a clean
    # comparison does, so the node reports as `selected` with an empty reason and
    # nothing in `degraded`: the frontier then OVERSTATES by co-scheduling nodes
    # whose overlap was never actually compared.
    try:
        if not plan or not has_file_surface(resolve_plan_path(plan)):
            return f"unevaluated:no-surface{domain_suffix}"
        hit = _high_collision(node, inflight)
    except Exception as exc:  # noqa: BLE001 - fail open, but as a stated verdict
        _LOG.warning(
            "collision gate UNEVALUATED for %s: %s", node.get("id"), exc,
        )
        return f"{_UNEVALUATED_PREFIX}collision-error{domain_suffix}"
    if hit is not None:
        return f"{_HIGH_COLLISION_PREFIX}{hit.with_node_id}"
    return None


@dataclass(frozen=True)
class ScheduleDecision:
    """One node's verdict in a shadow schedule (plan x-24f7 Change 1)."""

    id: str
    slug: Optional[str]
    domain: str
    verdict: Literal["selected", "serialized", "unevaluated"]
    reason: str  # "" for selected; a typed token otherwise

    def as_dict(self) -> dict:
        return {
            "id": self.id, "slug": self.slug, "domain": self.domain,
            "verdict": self.verdict, "reason": self.reason,
        }


def schedule_shadow(
    max_lanes: int,
    project: Optional[str] = None,
    *,
    mission: Optional[str] = None,
    claims_root: Optional[Path] = None,
) -> dict:
    """Read-only bounded-frontier decision report - the shadow-first core (x-24f7).

    Runs the SAME per-candidate classification as :func:`select_lane_fill` over
    the guard-eligible ready set (``fno backlog ready`` already applies the
    dependency / design-stage / stale guards, so those exclusions never reach
    here), greedily filling up to the bounded effective cap, and records a typed
    verdict for EVERY ready node. It acquires no
    slot and spawns nothing - purely observational (plan: "perform no dispatch in
    shadow mode").

    ``effective_cap`` is the initial-rollout ceiling ``_INITIAL_LIVE_CAP``:
    a request below one normalizes to one, larger requests clamp down. Reported
    so the operator sees the bound the live scheduler will honor. Empty,
    singleton, and packet-larger ready sets all produce bounded output.

    Fail-safe: an unreadable ready list yields an empty frontier with a
    ``ready-unreadable`` note rather than raising, and an in-flight / live-lane
    read fault degrades the collision + domain seed to empty (fail-open, same as
    live dispatch) rather than wedging the report.
    """
    effective_cap = min(max(max_lanes, 1), _INITIAL_LIVE_CAP)

    try:
        ready = _ready_nodes(project, mission)
    except Exception as exc:  # noqa: BLE001 - a garbled ready list is not a crash
        _LOG.warning(
            "schedule shadow: ready list unreadable, empty frontier UNDERSTATES "
            "dispatch (the safe direction): %s", exc,
        )
        # Same key set as the healthy return, so a scripted consumer reading
        # e.g. report["remaining_capacity"] gets a number on exactly the path
        # where it most needs one instead of a KeyError. Both capacity fields
        # are zero because this short-circuits BEFORE the slot read: nothing was
        # measured, and zero remaining is the fail-closed value (this report
        # authorizes no dispatch). `degraded` is what says not to trust them.
        return {
            "effective_cap": effective_cap, "requested_cap": max_lanes,
            "occupied_slots": 0, "remaining_capacity": 0,
            "note": "ready-unreadable", "degraded": ["ready"],
            "selected": [], "serialized": [], "unevaluated": [], "decisions": [],
        }

    # Seed the domain + in-flight sets from the live-claim world exactly as
    # select_lane_fill does, so the shadow frontier reflects real peer lanes.
    # Each read fails open (an error leaves the seed empty) but is LOUD about it -
    # both logged AND recorded in `degraded`. A silently-collapsed in-flight seed
    # produces a frontier byte-identical to a healthy one, and this report IS the
    # evidence that gates live scheduling: an operator reading the JSON must be
    # able to see that a seed threw, or they gate on an overstated frontier - and
    # over-dispatch is silently reintroducible by any future swallowed read. (A
    # collapsed DOMAIN seed now only degrades the `+same-domain` annotation,
    # since domain no longer excludes; it stays flagged for parity with the live
    # selector's seed.)
    degraded: list[str] = []
    try:
        used_domains: set[str] = _live_lane_domains(claims_root=claims_root)
    except Exception as exc:  # noqa: BLE001 - fail open, but visibly
        _LOG.warning(
            "schedule shadow: live-lane domain seed unreadable, same-domain "
            "annotations may be missing on unevaluated verdicts: %s", exc,
        )
        used_domains = set()
        degraded.append("live-lane-domains")
    try:
        inflight: list[dict] = _live_worked_entries(claims_root=claims_root)
    except Exception as exc:  # noqa: BLE001 - fail open, but visibly (parity with select_lane_fill)
        _LOG.warning(
            "schedule shadow: in-flight seed unreadable, missed file collisions "
            "mean the frontier may OVERSTATE dispatch: %s", exc,
        )
        inflight = []
        degraded.append("inflight")

    # Slots already held by live lanes count AGAINST the cap, so a cap-two report
    # with one lane already live can start only ONE more node. Counting from zero
    # would overstate the frontier during fill-vacant-lanes runs.
    #
    # Count EVERY live lane, not just the ones at an index below the cap. It is
    # tempting to count only what acquire_lane_slot(cap) would contend for, since
    # that predicts the acquire call exactly - but effective_cap is a ceiling on
    # live WRITERS, and the live selector still acquires with the raw configured
    # max_lanes (3 here), so a lane routinely sits at lane-slot:2 while this
    # report bounds itself to 2. Ignoring that lane would let a cap-two report
    # authorize two more starts alongside it: three writers under a ceiling of
    # two. That acquire_lane_slot(2) would in fact grant the third is a shrink
    # bug in the cap primitive, not a truth this report should mirror into the
    # evidence that authorizes live scheduling.
    from fno.claims.lanes import active_lane_count

    try:
        occupied = active_lane_count(root=claims_root)
    except Exception as exc:  # noqa: BLE001 - fail open, but visibly (this is the capacity guard)
        _LOG.warning(
            "schedule shadow: live slot count unreadable, remaining_capacity "
            "may OVERSTATE the frontier: %s", exc,
        )
        occupied = 0
        degraded.append("occupied-slots")
    remaining_capacity = max(0, effective_cap - occupied)

    decisions: list[ScheduleDecision] = []
    picked: set[str] = set()
    selected_count = 0
    # Declared so each branch assignment below is checked against the legal set
    # (the tuple-unpack forms otherwise widen to plain str, which the Literal
    # field on ScheduleDecision then rejects).
    verdict: Literal["selected", "serialized", "unevaluated"]
    for node in ready:
        nid = node["id"]
        if nid in picked:
            continue
        picked.add(nid)
        domain = node.get("domain") or _DOMAIN_UNSET
        reason = _classify_lane_candidate(
            node, used_domains=used_domains, inflight=inflight, claims_root=claims_root,
        )
        if reason is not None:
            verdict = "unevaluated" if reason.startswith(_UNEVALUATED_PREFIX) else "serialized"
            if verdict == "unevaluated":
                # Mirror the live selector's post-pick state: live fail-opens
                # this candidate and its domain joins the seed, so the NEXT
                # same-domain unevaluated candidate carries the annotation
                # there. Without this, the report an operator gates dispatch
                # on understates exactly that arming.
                used_domains.add(domain)
        elif selected_count >= remaining_capacity:
            verdict, reason = "serialized", "cap-full"
        else:
            verdict, reason = "selected", ""
            selected_count += 1
            # Feeds only the +same-domain annotation on a later unevaluated pick
            # no exclusion rides on it.
            used_domains.add(domain)
            if node.get("plan_path"):
                # so later picks collide against this one, like the live selector
                inflight.append({
                    "id": nid, "title": node.get("title", ""),
                    "plan_path": node["plan_path"], "created_at": "", "status": "ready",
                })
        decisions.append(
            ScheduleDecision(
                id=nid, slug=node.get("slug"), domain=domain,
                verdict=verdict, reason=reason,
            )
        )

    # Checked AFTER the loop, so it reflects the resolution the comparisons above
    # actually used rather than a fresh probe. A cwd fallback makes collisions
    # false-negative, which overstates the frontier in the same direction a
    # swallowed seed read would - so it belongs in `degraded`, not only on stderr.
    from fno.graph.collision import repo_root_resolution_degraded

    if repo_root_resolution_degraded():
        degraded.append("plan-path-resolution")

    return {
        "effective_cap": effective_cap,
        "requested_cap": max_lanes,
        "occupied_slots": occupied,
        "remaining_capacity": remaining_capacity,
        # Non-empty => a live-claim seed threw and was failed open; the frontier
        # may be inaccurate. A consumer gating live scheduling should refuse a
        # degraded report rather than trust it.
        "degraded": degraded,
        "selected": [d.as_dict() for d in decisions if d.verdict == "selected"],
        "serialized": [d.as_dict() for d in decisions if d.verdict == "serialized"],
        "unevaluated": [d.as_dict() for d in decisions if d.verdict == "unevaluated"],
        "decisions": [d.as_dict() for d in decisions],
    }


def _worker_agent_name(
    node_id: str, node_slug: Optional[str], prefix: str = "target"
) -> str:
    """Provenance-carrying bg worker name: ``<prefix>-<full-node-id>-<slug>``.

    Thin adapter over the canonical owner (``fno.agents.naming``), which also
    enforces the runtime's 64-char limit this call site used to skip - a long
    configured node id assembled a name ``fno agents spawn`` rejected, losing
    the dispatch with no session and no event (x-3218). ``prefix`` is
    ``reconcile`` for the G4 de-stub pass so its worker name never collides
    with the (ended) first pass's ``target-<id>-<slug>``.

    Raises :class:`~fno.agents.naming.AgentNameError` when the required
    identity cannot be represented; the dispatch path projects that as a
    node-identifying failure event rather than a launched lane.
    """
    return agent_name(prefix, node_id, slug=node_slug)


def _refuse_repeated_dead_dispatch(
    node_id: str,
    node_cwd: Optional[str],
) -> Optional[str]:
    """Auto-defer at the durable failure limit; return the refusal action."""
    from fno.graph import failure

    try:
        from fno.config import load_settings, load_settings_for_repo

        settings_obj = load_settings_for_repo(Path(node_cwd)) if node_cwd else load_settings()
    except Exception:
        settings_obj = None
    try:
        failure_limit = (
            int(settings_obj.active_backlog.failure_limit)
            if settings_obj is not None
            else 3
        )
    except Exception:
        failure_limit = 3
    project_events = Path(node_cwd) / ".fno" / "events.jsonl" if node_cwd else None
    events = failure.read_events()
    if project_events is not None and project_events.exists():
        events = failure.merge_event_histories(
            events,
            failure.read_events(project_events),
        )
    streak = failure.consecutive_failures(node_id, events)
    if streak < failure_limit:
        return None

    reason = (
        f"auto-failure: {streak} consecutive dead dispatches (worker reaped without termination)"
    )
    proc = subprocess.run(
        [*_subprocess_util.fno_py_cmd(), "backlog", "defer", node_id, "--reason", reason],
        cwd=node_cwd or None,
        capture_output=True,
        text=True,
    )
    action = "auto-deferred" if proc.returncode == 0 else "defer-failed"
    from fno.agents import events as agent_events

    agent_events.emit(
        EVENT_DEAD_FAILURE_LIMIT,
        node_id=node_id,
        consecutive_failures=streak,
        failure_limit=failure_limit,
        action=action,
    )
    from fno.notify._impl import send_notification

    send_notification(
        "footnote: dead dispatch limit",
        f"{node_id}: {streak} dead dispatches; {action}. No worker launched.",
    )
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()[:200]
        print(
            f"dead-dispatch limit reached for {node_id}; defer failed "
            f"(exit {proc.returncode}: {detail}); refusing another worker",
            file=sys.stderr,
        )
    return action


def _spawn_receipt_identity(proc_stdout: Optional[str]) -> str:
    """Launch identity from a thread-substrate spawn receipt (shared scan).

    A ``thread``/``bg`` spawn prints a compact JSON receipt whose launch proof
    is ``{"name", "short_id", ...}`` for claude, or for a codex thread the FULL
    ``harness_session_id`` (codex has no short id; a head-8 slice is refused by
    shape elsewhere). Scans past lines that merely MENTION an id field but are
    not the receipt (banner/log noise) - only a line whose id actually parses
    stops the scan. Empty string = no receipt (the caller decides severity).
    """
    short_id = ""
    harness_session_id = ""
    for line in (proc_stdout or "").splitlines():
        if '"short_id"' not in line and '"harness_session_id"' not in line and '"session_id"' not in line:
            continue
        try:
            receipt = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(receipt, dict):
            continue
        short_id = receipt.get("short_id") or ""
        harness_session_id = (
            receipt.get("harness_session_id") or receipt.get("session_id") or ""
        )
        if short_id or harness_session_id:
            break
    return short_id or harness_session_id


def _launch_harness_axis(launch: str, node_cwd: Optional[str] = None) -> Optional[str]:
    """The harness ``launch`` names, or None when nothing can answer.

    ``provider`` on the spawn seam is the harness axis under an older spelling,
    so most values answer for themselves. The exception is an ACCOUNT RECORD
    (ccm, ccr): a real binary on PATH, not a harness, whose registry row names
    the harness it runs. Reading through the row is what lets an account-pinned
    spawn keep its alias on ``--harness`` while the command is spelled for the
    harness that alias actually is.

    None means unverifiable - an undeclared binary with no record - and an
    unverifiable value is left alone rather than guessed at: it pins nothing and
    triggers no disagreement refusal.
    """
    if not launch:
        return None
    from fno.agents import harness_map

    if harness_map.is_declared(launch):
        return launch
    try:
        from fno.adapters.providers.loader import load_providers

        rec = load_providers(
            repo_root=Path(node_cwd) if node_cwd else None
        ).by_id.get(launch)
        return (getattr(rec, "harness", "") or "").strip() or None
    except Exception:  # noqa: BLE001 - an unreadable registry verifies nothing
        return None


def _spawn_worker(
    node_id: str,
    node_cwd: Optional[str],
    node_slug: Optional[str] = None,
    *,
    reconcile_manifest: Optional[str] = None,
    model: Optional[str] = None,
    provider: Optional[str] = None,
    vendor: Optional[str] = None,
    harness: Optional[str] = None,
    verb: Optional[str] = None,
    brief: Optional[str] = None,
    extra_env: Optional[dict] = None,
    dispatch_account: Optional[str] = None,
    permission_mode: Optional[str] = None,
    node: Optional[dict] = None,
    caller: str = "unknown",
    events_path: Optional[Path] = None,
) -> str:
    """Dispatch a fire-and-forget autonomous ``/target`` (or ``dispatch_verb``) worker.

    Routes the substrate + the per-harness-normalized command through the shared
    resolver (``fno.agents.harness_map.resolve_dispatch``) instead of hardcoding
    ``--substrate bg`` + a ``/target`` f-string (x-0676). ``harness`` (the selected
    provider record's ``cli``; ``None`` = config/``claude``) picks the substrate:
    ``bg`` for claude (the detached ``claude --bg`` thread that self-isolates into a
    worktree, never the pane default that would STALL a fire-and-forget dispatch,
    x-2c27), ``headless`` for codex/others. A node's ``dispatch_verb``/
    ``dispatch_brief`` (``verb``/``brief``) route the verb path (``/think {id}``,
    brief on ``TARGET_BRIEF`` env); with no verb the builtin ``/target`` is used.

    Merge posture (x-4391) stays a launcher decision, never baked into a node verb:
    the default builtin bakes ``no-merge``; ``config.auto_merge.grant`` routes the
    ``/target`` verb path (which omits ``no-merge``); reconcile stays an explicit
    ``/target [--no-merge] --reconcile <manifest> {id}`` template. The agent is named
    ``target-<full-node-id>-<slug>`` (``reconcile`` prefix when G4), and the cwd
    resolves to the node's recorded root (``--cwd``) or canonical main (``--fresh``).

    ``dispatch_account`` is a quota cutover's destination provider RECORD id, and
    it rides argv. The credentials never do: the spawn front door resolves the
    record and applies its overlay where the harness is exec'd. That matters
    because a non-claude record's overlay is a HOME override and footnote reads
    HOME to find its own state root, so an overlay merged into THIS wrapper's env
    would file the worker's registry row and claim under the account's home
    (x-c33e). ``extra_env`` is refused outright for any such key.

    Returns the spawn receipt's LAUNCH IDENTITY: the claude short_id, or for a
    codex thread the FULL harness_session_id (codex has no short id; a head-8
    slice is refused by shape - ruling d-513d9d22). Raises SpawnAlreadyRunning
    on a name-collision (a peer beat us in the boot window),
    DispatchResolveError on an unresolvable harness/substrate/verb (caught
    non-fatally by the caller), and SpawnError otherwise.
    """
    is_reconcile = bool(reconcile_manifest)
    agent_name = _worker_agent_name(
        node_id, node_slug, prefix="reconcile" if is_reconcile else "target"
    )
    # --provider selects the account/record (or a bare kind like "claude"); a
    # per-node or dispatch-time pin overrides the claude default. Layer-separate
    # from `harness` (the record's cli, which drives the resolver's substrate).
    #
    # x-374b: `prov` is no longer computed here. It used to be
    # `(provider or "").strip() or "claude"`, decided BEFORE the resolver ran,
    # so the launch binary and the command surface came from two different
    # sources: an unpinned dispatch launched claude while `resolve_dispatch`
    # read the stage table and spelled the command for codex. The specimen was a
    # claude worker whose first user line was `$fno:target x-30c2`. Both now come
    # off one `resolve_dispatch` below.
    launch = (provider or "").strip()

    # Capacity-grid deferral receiving end: the automatic dispatch callers pass
    # resolve_difficulty=False to node_model precisely so difficulty picks the
    # lane HERE, at the seam that can read live capacity - the spawned argv
    # always carries an explicit --harness, so the spawn-CLI grid can never fire
    # on this path. See _grid_lane_for; harness-keyed placement sites resolve
    # there instead and arrive already pinned - on a grid decline the pin is the
    # placement harness. An explicit harness therefore skips this consult: under
    # it the grid could pick a harness the caller's placement did not key for.
    grid_why: Optional[str] = None
    if harness is None:
        grid_harness, grid_model, grid_why = _grid_lane_for(node, model=model, provider=provider)
        if grid_harness is not None:
            model = grid_model
            # The resolver must see the grid's harness or it resolves a
            # claude substrate/command for a codex spawn (bg is claude-only).
            harness = grid_harness

    # x-4391/x-4be1: merge posture from config.auto_merge.grant, read with the
    # node_cwd precedence so a cross-project dispatch reads the DEPENDENT node's
    # config (AC2-EDGE), never the merged repo's. advance takes no per-run flag,
    # so config is the sole non-builtin rung; any read failure -> no-merge
    # (Locked Decision 6). The same settings object feeds the resolver
    # (config.dispatch.*) and the permission-mode read below, so all three
    # config reads are node-consistent.
    allow_merge = False
    settings_obj = None
    try:
        from fno.config import load_settings, load_settings_for_repo

        settings_obj = (
            load_settings_for_repo(Path(node_cwd)) if node_cwd else load_settings()
        )
    except Exception:  # noqa: BLE001 - unreadable config -> defaults below
        settings_obj = None
    # Read the grant in its OWN guard so a missing/odd block never disables the
    # independent permission-mode read that also consumes settings_obj. Only the
    # literal "dispatch" grants (a typo or a stub settings object never does).
    if settings_obj is not None:
        try:
            allow_merge = (
                getattr(getattr(settings_obj, "auto_merge", None), "grant", None)
                == "dispatch"
            )
        except Exception:  # noqa: BLE001 - fail-safe to no-merge (never grant on error)
            allow_merge = False

    # x-0676: resolve substrate + normalized command. A node dispatch_verb takes the
    # verb path (never a merge); reconcile stays explicit and spells its own posture.
    # With neither, the builtin rung reads config.auto_merge.grant itself (x-8e59),
    # so this caller no longer routes a merge grant through the verb path to work
    # around a builtin that ignored the key. A DispatchResolveError propagates to the
    # caller's non-fatal spawn-failure path.
    from fno.agents import harness_map
    from fno.harness_identity import (
        CODEX_SHORT_ADDRESS_RULE,
        is_unsafe_short_address,
    )

    # One harness axis. `harness` is the explicit pin; `provider` is the same
    # axis arriving under the older spelling, so it must reach the resolver too
    # or the command is spelled for whatever the stage table says while the
    # worker launches on `provider`. An account record (ccm/ccr) is not itself a
    # harness, so it answers through its registry row.
    launch_axis = _launch_harness_axis(launch, node_cwd)
    node_verb = (verb or "").strip() or None
    resolve_kwargs: dict = {
        "harness": ((harness or "").strip() or launch_axis or None),
        "node_id": node_id,
        "brief": (brief or None),
        "trigger": "autonomous",
        "settings": settings_obj,
    }
    if is_reconcile:
        # x-8151: the refusal spelling is inserted by the shared vocabulary
        # helper, never a second hardcoded "--no-merge " string.
        resolve_kwargs["command"] = f"/target --reconcile {reconcile_manifest} {{id}}"
        if not allow_merge:
            resolve_kwargs["command"] = harness_map.inject_no_merge_into_command(
                resolve_kwargs["command"]
            )
    elif node_verb:
        resolve_kwargs["verb"] = node_verb
    resolved = harness_map.resolve_dispatch(**resolve_kwargs)
    substrate = resolved["substrate"]
    target_cmd = resolved["command"]
    spawn_env = resolved.get("env") or {}

    # The launch binary. An account record keeps its own alias (that IS the
    # point of the record); everything else takes the resolver's answer, which
    # already owns the builtin claude fallback the deleted `or "claude"` used to
    # duplicate one rung too early.
    prov = launch or resolved["harness"]
    if launch_axis and launch_axis != resolved["harness"]:
        raise SpawnError(
            f"refusing to spawn {node_id}: the launch harness and the command "
            f"surface disagree. --harness {prov!r} runs {launch_axis!r} while "
            f"the resolved command is spelled for {resolved['harness']!r} "
            f"({target_cmd!r}). Pass one axis, not two."
        )

    cmd = [
        *_subprocess_util.fno_py_cmd(),
        "agents", "spawn", "--harness", prov, "--substrate", substrate,
    ]
    if vendor:
        cmd += ["--provider", vendor]
    if node_cwd:
        cmd += ["--cwd", node_cwd]
    else:
        cmd += ["--fresh"]
    # x-571f: a per-node model pin rides as a spawn flag. Empty/None = provider
    # default, byte-identical to today.
    if model:
        cmd += ["--model", model]
    # x-dfa4: an explicit permission_mode wins; else the autonomous-dispatcher
    # config default (config.agents.spawn_permission_mode). Both empty = unchanged.
    mode = (permission_mode or "").strip()
    if not mode and settings_obj is not None:
        try:
            mode = (settings_obj.agents.spawn_permission_mode or "").strip()
        except Exception:  # noqa: BLE001 - fail-safe to unset (unchanged)
            mode = ""
    # CLAUDE-ONLY, mirroring dispatch-node.sh: the spawn seam exit-2 rejects a
    # mapped --permission-mode for a non-claude harness on a non-pane substrate.
    # Gate on the RESOLVED harness, not the raw `prov` string: `provider` may carry
    # a claude ACCOUNT record (e.g. ccm/ccr) that resolves to harness=claude and
    # MUST still get the flag, else the account-pinned worker keeps hanging - the
    # exact bug this change fixes. A failover leg landing on codex/gemini gets its
    # bypass from its own resolved caps, not this claude-native value. Silent skip
    # (parity); the receipt omits permission_mode, so the posture stays inspectable.
    if mode and resolved.get("harness") == "claude":
        cmd += ["--permission-mode", mode]
    # A quota cutover's destination account rides argv as a RECORD ID, never as
    # env: the spawn front door resolves it inside cmd_spawn and applies the
    # overlay where the harness is exec'd. A codex record's overlay is
    # {HOME: <account_dir>/home}, and footnote resolves its own state root off
    # HOME too, so putting it on this wrapper would move the registry, the claim
    # and the events into the account's home where nothing looks (x-c33e).
    if dispatch_account:
        cmd += ["--dispatch-account", dispatch_account]
    cmd += ["--name", agent_name, target_cmd]

    # The brief (US3) rides the spawn subprocess env as TARGET_BRIEF (never the
    # command line), mirroring dispatch-node.sh's `export TARGET_BRIEF`. A failover
    # account does NOT ride here: `--provider <harness>` selects the CLI and
    # `--dispatch-account <record>` selects the account (x-0676, x-c33e;
    # --provider never carries a record id).
    from fno.agents.account_env import STATE_ROOT_ENV_KEYS

    for key in sorted(STATE_ROOT_ENV_KEYS & set(extra_env or {})):
        raise SpawnError(
            f"refusing to put {key} on the `fno agents spawn` wrapper: footnote "
            f"resolves its own state root off {key}, so the worker's registry "
            "row, claim and events would land in the account's home where "
            "nothing looks. Pass the destination account as "
            "--dispatch-account <record> instead; the spawn front door applies "
            "the overlay where the harness is exec'd."
        )
    merged_env = {**spawn_env, **(extra_env or {})}
    # x-9d11: the resolver's env is AUTHORITATIVE for the merge posture, so the
    # inherited TARGET_NO_MERGE never survives into a successor the resolver just
    # granted allow-merge (this verb runs as a subprocess of the prior no-merge
    # worker, whose exported carrier would otherwise silently kill the config's
    # auto-merge posture - review round 5). Dropped from the base BEFORE the
    # merge so the resolver's own value (either way) is the only one that lands.
    base_env = {k: v for k, v in os.environ.items() if k != "TARGET_NO_MERGE"}
    run_env = {**base_env, **merged_env} if merged_env else (base_env or None)
    proc = subprocess.run(
        cmd, capture_output=True, text=True, timeout=600, env=run_env
    )
    if proc.returncode != 0:
        stderr = (proc.stderr or "").strip()
        if proc.returncode == 2 and _SPAWN_ALREADY_EXISTS in stderr:
            raise SpawnAlreadyRunning(f"agent {agent_name} already exists")
        raise SpawnError(
            f"fno agents spawn exited {proc.returncode}: "
            f"{(stderr or proc.stdout or '').strip()[:200]}"
        )

    def _receipt(short_id: str) -> str:
        """One ``dispatch_spawned`` row per launch, then hand back the identity.

        The spawner is the only place that knows the resolved argv, so the
        receipt is emitted here rather than folded into the callers'
        ``advance_dispatched`` rows: those name a node and an agent, and three
        different dispatch doors (lane fill, auto-continue, active backlog) were
        indistinguishable in the file. Best-effort via ``_emit``, so a journal
        failure never wedges a launch that already happened.
        """
        _emit(
            EVENT_SPAWNED,
            {
                "node_id": node_id,
                "short_id": short_id,
                "agent_name": agent_name,
                "harness": prov,
                "vendor": vendor or "",
                "model": model or "",
                "substrate": substrate,
                "command": target_cmd,
                "cwd": node_cwd or "",
                "caller": caller,
                "grid": grid_why or "",
                "decision": "; ".join(resolved.get("decision") or []),
            },
            events_path if events_path is not None else _events_path(None),
        )
        return short_id

    # Receipt shape is substrate-dependent (mirrors dispatch-node.sh). A `bg`
    # spawn lands a DETACHED thread and returns a compact JSON receipt whose
    # launch identity we require as launch proof: {"name", "short_id", ...} for
    # claude, and for a codex thread {"short_id": "", "harness_session_id"/
    # "session_id": <full id>} - codex has no short id, so the FULL session id
    # is the launch proof (x-de10; the old short_id-only parse raised
    # SpawnError for every codex thread dispatch). A `headless` one-shot (a
    # codex/others failover) already ran to completion on exit 0 - no detached
    # thread, no id - so the clean exit IS the proof and we skip the
    # requirement (else the parse below would raise SpawnError, release the
    # reservation, and redispatch a node whose headless worker already ran).
    if substrate != "thread":
        return _receipt("headless")
    # Keep scanning past a line that merely MENTIONS an id field but is not the
    # JSON receipt (banner/log noise) - only stop once an id actually parses.
    launch_identity = _spawn_receipt_identity(proc.stdout)
    if not launch_identity:
        raise SpawnError(
            f"fno agents spawn exit 0 but no launch-identity receipt: "
            f"{(proc.stdout or proc.stderr or '').strip()[:200]}"
        )
    # A bare 8-hex id aimed at codex is a 65.5-second timestamp bucket, not an
    # address: refuse by shape instead of binding a duplicate worker to the
    # wrong session (ruling d-513d9d22, harness_identity.is_unsafe_short_address).
    resolved_harness = (resolved.get("harness") or "").strip()
    if is_unsafe_short_address(launch_identity, resolved_harness or None):
        raise SpawnError(
            f"fno agents spawn receipt carries a codex head-8 launch "
            f"identity ({launch_identity}): {CODEX_SHORT_ADDRESS_RULE}"
        )
    return _receipt(launch_identity)


# ---------------------------------------------------------------------------
# Lane dispatch (parallel mode, epic x-42d5 group 3): spawn + per-lane isolation
# ---------------------------------------------------------------------------
#
# G1 shipped the atomic lane-slot cap (claims/lanes.py); G2 the lane-fill
# selector (select_lane_fill above) + the `fno backlog lane-fill` preview CLI.
# G3 is the SPAWN layer: it takes G2's selection (which already holds a
# dispatch-time lane slot per node, LD#8) and launches each pick as an ISOLATED
# background lane - one worktree off origin/main, one branch, one PR stream.
#
# The isolation is the whole point (why x-cbce is a hard dep). Every worktree
# shares the canonical config.toml (symlinked by setup-worktree.sh). G3 seeds
# each lane a `.fno/config.local.toml` (x-cbce's per-worktree override, allowlist
# {project.id}) giving project.id a per-lane value. The per-lane project.id
# neuters the lane's own nested auto-continue: its post-merge
# `advance(project=<lane-id>)` finds no same-project `next`, so the top-level
# parallel dispatcher stays the single lane authority instead of each lane
# fanning out past `max_lanes`.
#
# The parking lot is NOT lane-isolated (x-071c): the post-merge ritual resolves
# `parking_lot_path` against the canonical root unconditionally and writes there.
# It is a serial one-shot durable step whose write vehicles are already safe on
# the shared canonical file (capture add file-locks; the narrative append is
# per-PR single-flight under the reconcile mutex with O_APPEND), so a per-lane
# redirect bought nothing and orphaned the prose into an untracked file that
# archive-worktree.sh deletes.
#
# NOT here (deferred to G4): merge serialization (LD#9 - lanes must rebase +
# merge one at a time), full failure isolation via _redispatch (x-370f), and the
# grid status rollup. G3 releases a lane slot on spawn failure so the node stays
# re-dispatchable, but the richer dead-lane recovery is G4's. Live wiring into
# the auto-continue drain is likewise deferred until merge-serialization lands,
# so this stays a callable, independently-tested primitive (`fno backlog
# dispatch-lanes`), mirroring how G1/G2 shipped runnable layers without flipping
# the global live switch.


class WorktreeEnsureError(RuntimeError):
    """`fno agents workspace worktree ensure` failed; the lane cannot be isolated, so it is skipped."""


class LaneRootError(RuntimeError):
    """A selected node has no repository where its lane can be isolated."""


def _canonical_root() -> Path:
    """The canonical (main-checkout) repo root a lane worktree spawns from."""
    from fno.paths import resolve_canonical_repo_root

    return resolve_canonical_repo_root()


def _node_repo_root(node: dict) -> Path:
    """Resolve a selected node's own canonical repository root."""
    raw = node.get("_resolved_cwd") or node.get("cwd")
    node_id = node.get("id") or "unknown"
    if not raw or not str(raw).strip():
        raise LaneRootError(f"node {node_id} has empty cwd")
    path = Path(str(raw)).expanduser()
    from fno.paths import resolve_canonical_worktree

    root = resolve_canonical_worktree(path) or path
    if not (root / ".git").exists():
        raise LaneRootError(f"node {node_id} cwd {path} has no .git")
    return root.resolve()


def _base_project_id(canonical_root: Path) -> str:
    """The shared project.id lane ids are derived from (fallback: repo basename)."""
    try:
        from fno.config import load_settings_for_repo

        pid = load_settings_for_repo(canonical_root).project.id
        if pid:
            return pid
    except Exception:  # noqa: BLE001 - a settings read error must not crash dispatch
        pass
    return canonical_root.name


# The non-claude harness kinds (KNOWN_PROVIDERS minus claude). Any other provider
# value - a claude ACCOUNT record (ccm/ccr), z.ai/glm, an empty/None - runs under
# the claude harness, so it maps to claude for the worktree-native decision.
_NON_CLAUDE_HARNESSES = frozenset({"codex", "gemini", "agy", "opencode"})


def _grid_lane_for(
    node: Optional[dict], *, model: Optional[str], provider: Optional[str]
) -> tuple[Optional[str], Optional[str], Optional[str]]:
    """``(harness, model, decline_reason)`` the capacity grid picks for an UNPINNED spawn.

    On a pick the reason is ``None``; on a decline the harness and model are
    ``None`` and the reason names WHY.

    ``resolve_grid`` already returns ``(candidate, chain)`` whose last element
    is its terminal reason, and this function used to throw that away as
    ``_chain``. A bare ``(None, None)`` collapsed three different outcomes into
    one value - the caller pinned a model, capacity is unknown, or the
    inventory is empty - so a spawn site could not tell a deliberate pin from a
    config gap and fell through to the ambient default in silence.

    That is not hypothetical. With ``config.routing.models`` empty no band can
    ever match: ``resolve_grid`` appends ``grid=no-inventory-declared`` and
    returns no candidate, so EVERY banded plan buys the ambient fleet forever
    rather than momentarily. Observed on this node's own joiners, which both
    spawned on the most expensive lane while the receipt said only "grid
    declined, harness null, model null".

    The reason is for RECEIPTS, never for refusing. Routing degrades and never
    blocks a spawn (Locked 10), and an empty inventory is a config gap, not a
    capacity failure. Naming it is what makes it fixable.

    Deliberately ONE function rather than a two-tuple wrapper around a
    three-tuple worker. Tests monkeypatch this name, and an internal caller
    that reached past the wrapper would silently bypass every such patch - the
    first cut of this change did exactly that and two tests caught it.

    The receiving end of the difficulty deferral: dispatch callers resolve
    difficulty to nothing precisely so it picks the lane HERE, where live
    capacity is readable - the spawned argv's explicit --harness can never
    trigger the spawn-CLI grid. Only a fully unpinned spawn defers (an explicit
    model or provider stays operator authority); unknown capacity falls back to
    the caller's defaults (Locked 10: routing degrades, never blocks a spawn).
    Dispatch sites that make HARNESS-KEYED decisions before spawning (lane
    worktree placement) must call this first and thread the result through
    both decisions, so placement and spawn always agree.
    """
    if model is not None or (provider or "").strip() or node is None:
        return None, None, None
    try:
        from fno import route_resolve

        inventory = route_resolve.resolve_inventory()
        capacity: dict[str, object] = dict(
            route_resolve.runtime_capacity(inventory=inventory)
        )
        # The same planning/execution role floor the spawn seam applies: an
        # unplanned node auto-dispatched here bills planning too, or the two
        # dispatch doors would price one node differently.
        role: Optional[str] = None
        if not (node.get("plan_path") or "").strip():
            role = "planning"
        candidate, chain = route_resolve.resolve_grid(
            node.get("difficulty"),
            node.get("priority"),
            capacity,
            role=role,
            inventory=inventory,
        )
    except Exception as exc:  # noqa: BLE001 - unknown capacity spawns on defaults
        return None, None, f"grid=unreadable ({str(exc)[:80]})"
    # The chain's last element is the terminal reason on every path, so it is
    # surfaced verbatim rather than reformatted - the strings are the existing
    # receipt vocabulary and rewording them here would fork it.
    terminal = str(chain[-1]) if chain else "grid=no-reason-recorded"
    if candidate is None:
        return None, None, terminal
    # Placement commits a harness-keyed worktree, which unknown capacity must
    # not buy: the grid's unknown-permitted posture is right for injection
    # (defaults still compose the argv), wrong for a lane decision with no
    # data at all. Fall back to the caller's defaults there.
    state = capacity.get(candidate["harness"])
    verdict = state.get("state", "unknown") if isinstance(state, dict) else state
    if str(verdict).lower() not in ("ok", "low", "available"):
        return None, None, f"grid=capacity-{str(verdict).lower()}"
    return candidate["harness"], candidate["model"], None


def _lane_harness(eff_provider: Optional[str]) -> str:
    """Resolve a lane's dispatch provider to its worktree harness.

    The worktree-native decision only cares whether the worker runs under claude:
    an explicit non-claude harness (codex/gemini/agy/opencode) keeps its own
    harness (-> external base); everything else - claude, a claude account record
    (ccm/ccr), an empty/None provider - resolves to claude (-> harness-native),
    so the worktree lands where the worker actually runs. `_spawn_worker` reads
    the same axis through `_launch_harness_axis` plus the resolver's own claude
    fallback, and refuses a spawn where the two answers disagree.
    """
    return eff_provider if (eff_provider and eff_provider in _NON_CLAUDE_HARNESSES) else "claude"


def _run_setup_worktree(worktree: Path, canonical_root: Path) -> None:
    """Link shared `.fno`/`internal`/`.claude` state into a fresh lane worktree.

    `fno agents workspace worktree ensure` is git-mechanism-only (x-73ca) and deliberately leaves
    this to the caller; without it the lane has no symlinked settings.yaml and
    falls through to global config. Best-effort: a bare `pip install fno` ships
    no repo scripts, and a link failure must not abort an otherwise-launchable
    lane, so any non-zero / missing-script outcome is swallowed (the worker's
    own `fno do target start` re-heals what it can).
    """
    script = canonical_root / "scripts" / "setup" / "setup-worktree.sh"
    if not script.exists():
        return
    try:
        subprocess.run(
            ["bash", str(script)],
            cwd=str(worktree),
            capture_output=True,
            text=True,
            timeout=300,
        )
    except Exception as exc:  # noqa: BLE001 - non-fatal state linking
        _LOG.debug("dispatch_lanes: setup-worktree.sh failed for %s: %s", worktree, exc)


def _ensure_lane_worktree(
    node_id: str, *, canonical_root: Path, harness: str = "claude"
) -> Path:
    """Idempotently isolate a lane worktree off origin/main; return its path.

    Delegates to `fno agents workspace worktree ensure` (x-73ca): a git-only, idempotent verb
    that creates `<worktrees_base>/<repo>/<node_id>` on branch `feature/<node_id>`
    (base origin/main), or reuses it. Raises WorktreeEnsureError on failure (empty
    stdout / non-zero) so the caller releases the lane slot and skips this lane
    without touching the others (Failure Modes: Errors).

    ``harness`` is the lane worker's dispatch harness (the node's provider,
    defaulting to claude the way `_spawn_worker` resolves the same axis):
    claude lands the lane harness-native at `<repo>/.claude/worktrees/`,
    a non-claude harness at the external base. A `never` project returns the repo
    root (guarded below) regardless of the harness.
    """
    # Deliberately the PRE-FOLD spelling. `fno_py_cmd()` resolves the INSTALLED
    # fno-py off PATH, not this source, and the shim only forwards old to new.
    # `worktree ensure` therefore runs on an install from either side of the
    # fold; `workspace worktree ensure` hard-fails an install that predates it.
    # A cross-process caller migrates LAST, once the window closes.
    proc = subprocess.run(
        [*_subprocess_util.fno_py_cmd(), "worktree", "ensure",
         "--repo", str(canonical_root), "--name", node_id, "--harness", harness],
        capture_output=True,
        text=True,
        timeout=300,
    )
    path = (proc.stdout or "").strip()
    if proc.returncode != 0 or not path:
        raise WorktreeEnsureError(
            f"fno agents workspace worktree ensure failed for {node_id}: "
            f"{(proc.stderr or proc.stdout or '').strip()[:200]}"
        )
    worktree = Path(path)
    # policy=never (x-168b): ensure printed the repo main-checkout path itself
    # (launch in place, no worktree). Skip every worktree-only side effect - the
    # `.fno` heal + setup-worktree.sh would corrupt the canonical checkout
    # (Locked Decision 4: callers guard worktree-only work on path == repo root).
    if worktree.resolve() == canonical_root.resolve():
        return worktree
    # Heal a whole-dir `.fno` symlink (a REUSED worktree can carry one) BEFORE
    # setup-worktree.sh runs: setup links shared state into `.fno/*`, and through
    # the symlink those links would land in the CANONICAL checkout; the later
    # seed would then replace `.fno` with a bare real dir, stranding the lane
    # without its settings.yaml/state links. Heal first so setup populates the
    # REAL per-worktree dir (mirrors the heal `fno do target start` does before its
    # setup hook). A fresh worktree has no `.fno` yet, so this is a no-op there.
    fno_dir = worktree / ".fno"
    if fno_dir.is_symlink():
        fno_dir.unlink()
        fno_dir.mkdir()
    _run_setup_worktree(worktree, canonical_root)
    return worktree


def _seed_lane_local_settings(
    worktree: Path, node_id: str, base_project_id: str
) -> None:
    """Write the lane's `.fno/config.local.toml` per-worktree isolation seed.

    Overrides ONLY x-cbce's sole allowlisted key on top of the shared (symlinked)
    config.toml: `project.id` -> a per-lane value so the lane's post-merge
    auto-continue is scoped to itself. Written unconditionally: a lane worktree
    is machine-owned and the content is deterministic, so a re-dispatch re-seeds
    identically (idempotent).

    Note: `post_merge.parking_lot_path` is NOT seeded (x-071c). The post-merge
    ritual is a serial one-shot durable write whose vehicles are already safe on
    the shared canonical file (capture add under a file lock; narrative append
    under the per-PR reconcile mutex with O_APPEND), so a per-lane redirect only
    orphaned the prose into an untracked `.fno/parking-lot.md` that
    archive-worktree.sh deletes. The ritual now resolves the parking lot against
    the canonical root unconditionally.
    """
    import tomli_w

    fno_dir = worktree / ".fno"
    # A reused worktree may carry `.fno` as a WHOLE-DIR symlink to canonical (the
    # bg-worktree footgun `fno do target start` already heals). Writing through it
    # would create/overwrite the CANONICAL config.local.toml, so every lane
    # would then share one project.id - the exact collision this seed prevents.
    # Unlink and recreate a real per-worktree dir first.
    if fno_dir.is_symlink():
        fno_dir.unlink()
    fno_dir.mkdir(parents=True, exist_ok=True)
    # Flat config.local.toml (no `config:` wrapper); tomli_w escapes the id value.
    body = tomli_w.dumps(
        {
            "project": {"id": f"{base_project_id}-{node_id}"},
        }
    )
    (fno_dir / "config.local.toml").write_text(
        "# Auto-seeded per-lane isolation (parallel mode, epic x-42d5 G3).\n"
        "# Only x-cbce's per-worktree override allowlist {project.id}; overrides\n"
        "# the shared config.toml so concurrent lanes never collide on node\n"
        "# attribution / nested auto-continue.\n"
        + body
    )


def dispatch_lanes(
    max_lanes: int,
    project: Optional[str] = None,
    *,
    mission: Optional[str] = None,
    project_root: Optional[Path] = None,
    events_path: Optional[Path] = None,
    claims_root: Optional[Path] = None,
    model: Optional[str] = None,
    harness: Optional[str] = None,
    vendor: Optional[str] = None,
    report: Optional[dict] = None,
) -> list[dict]:
    """Select and spawn up to ``max_lanes`` isolated background lanes.

    Dispatch-time ``model``/``harness``/``vendor`` values apply to every lane
    spawned this run and outrank each node's own annotation (Locked Decision 1).

    The parallel-mode dispatcher (epic x-42d5, group 3). Selects collision-clean
    ready nodes via :func:`select_lane_fill` (which atomically holds a lane slot
    per pick, LD#8), then for each pick: isolates a worktree off origin/main,
    seeds its per-lane `.fno/settings.local.yaml` (x-cbce), and spawns a detached
    `claude --bg` `/target --no-merge` worker rooted in that worktree. The worker's
    `fno do target init` reconciles the already-held slot rather than acquiring a
    fresh one.

    ``max_lanes == 1`` dispatches a single node (the retargeted active_backlog
    daemon's sequential fire-and-forget path, x-0ad6); ``max_lanes < 1`` selects
    nothing and returns ``[]``.

    Per-lane spawn/isolation failure is contained: the lane's slot is released so
    the node stays re-dispatchable and its receipt records ``skipped``; peer
    lanes are unaffected (Failure Modes: Errors). Returns one receipt dict per
    selected lane (``status`` ``dispatched`` | ``skipped``).
    """
    from fno.claims.core import CLAIM_UNAVAILABLE, acquire_claim
    from fno.claims.lanes import release_lane_slot

    selected = select_lane_fill(
        max_lanes,
        project,
        mission=mission,
        claim=True,
        claims_root=claims_root,
        report=report,
    )
    if not selected:
        if report is not None:
            report["dispatched"] = 0
            report["skipped"] = 0
        return []

    canonical = _canonical_root()
    ev_path = events_path or _events_path(project_root or canonical)

    receipts: list[dict] = []
    for node in selected:
        node_id = node["id"]
        slug = node.get("slug") or node.get("title")

        def _skip(reason: str, _nid: str = node_id) -> None:
            # A pick we will not spawn must return its dispatch-time lane slot,
            # or the cap stays wrong until TTL. Non-raising cleanup. _nid is bound
            # per-iteration (default arg) so the closure never captures a later
            # loop value.
            try:
                release_lane_slot(_nid, root=claims_root)
            except Exception as exc:  # noqa: BLE001
                _LOG.warning(
                    "dispatch_lanes: slot release failed for %s (%s); slot lingers to TTL",
                    _nid,
                    exc,
                )
            receipts.append({"node_id": _nid, "status": "skipped", "error": reason})

        try:
            root = _node_repo_root(node)
            # The lane slot (parallel-lane:<id>) is invisible to the sequential
            # advance()/dispatch-node.sh path, which dedups on node:<id> +
            # dispatch:<id>. Guard with the same dispatch:<id> reservation.
            block_reason = _node_dispatch_block_reason(node_id, str(root))
            dispatch_key = f"dispatch:{node_id}"
            dispatch_holder = f"advance:{os.getpid()}"
            dispatch_root = _claims_root_for(dispatch_key)
        except LaneRootError as exc:
            _skip(f"lane-root: {exc}")
            continue
        except Exception as exc:  # noqa: BLE001 - one lane cannot abort its peers
            _LOG.warning("dispatch_lanes: lane %s preflight failed: %s", node_id, exc)
            _skip(f"preflight-error: {str(exc)[:180]}")
            continue

        if block_reason:
            _skip(block_reason)
            continue
        try:
            acquire_claim(
                dispatch_key,
                dispatch_holder,
                ttl_ms=_DISPATCH_TTL_MS,
                reason=f"parallel lane dispatch for {node_id}",
                root=dispatch_root,
            )
        except CLAIM_UNAVAILABLE:
            # Two advance() passes racing this key is ordinary contention,
            # not a fault.
            _skip("already-claimed")
            continue
        except Exception as exc:  # noqa: BLE001
            _skip(f"claim-error: {str(exc)[:120]}")
            continue

        # dispatch:<id> is reserved just above (bridges the boot window until the
        # worker owns node:<id>); the lane slot select_lane_fill acquired is
        # re-anchored to the worker's lifecycle in target_cli._maybe_reconcile_lane_slot
        # (LD#8) once its target-init claims the node. Both are released on the
        # failure path below.
        try:
            eff_harness = harness if harness is not None else node.get("provider")
            resolved_model = _route_resolve.node_model(
                node, explicit=model, provider=eff_harness, resolve_difficulty=False
            )
            # The grid must decide BEFORE the worktree is placed: placement is
            # harness-keyed (claude-native vs external base), so it has to agree
            # with the harness the spawn will actually use. Threading the result
            # into both decisions keeps placement and spawn one decision. A
            # DECLINE pins too: an unpinned spawn re-consults the grid at the
            # spawn seam, and a capacity change in between could land the worker
            # on a harness the worktree was not keyed for.
            lane_grid_harness, lane_grid_model, _lane_grid_why = _grid_lane_for(
                node, model=resolved_model, provider=eff_harness
            )
            lane_placement_harness = _lane_harness(lane_grid_harness or eff_harness)
            worktree = _ensure_lane_worktree(
                node_id,
                canonical_root=root,
                harness=lane_placement_harness,
            )
            # A never-policy lane runs in the canonical checkout in place; seeding
            # a per-lane config.local.toml there would write into canonical .fno.
            if worktree.resolve() != root.resolve():
                _seed_lane_local_settings(
                    worktree, node_id, _base_project_id(root)
                )
            _brief, _brief_tag = _autobrief.resolve_dispatch_brief(node)
            short_id = _spawn_worker(
                node_id,
                str(worktree),
                slug,
                model=lane_grid_model or resolved_model,
                provider=lane_grid_harness or eff_harness,
                vendor=vendor,
                # The placement value unconditionally: a grid pick is always a
                # fixed point of _lane_harness today, and if that ever stops
                # holding, the raw pick would reopen the split this pins shut.
                harness=lane_placement_harness,
                verb=node.get("dispatch_verb"),
                brief=_brief,
                node=node,
                caller="dispatch_lanes",
                events_path=ev_path,
            )
        except Exception as exc:  # noqa: BLE001 - one lane's failure never aborts the fleet
            # Release BOTH the boot-window reservation and the dispatch-time lane
            # slot so the node returns to the pool (a later tick re-dispatches it).
            _safe_release(dispatch_key, dispatch_holder, dispatch_root)
            _LOG.warning("dispatch_lanes: lane %s skipped: %s", node_id, exc)
            _skip(str(exc)[:200])
            continue

        # Dispatched. Leave dispatch:<id> to expire by TTL: the worker now owns
        # (or is acquiring) node:<id> and reconciles its lane slot at target init.
        _emit(
            EVENT_DISPATCHED,
            {
                "node_id": node_id,
                "short_id": short_id,
                "agent_name": _worker_agent_name(node_id, slug),
                "lane": True,
                "worktree": str(worktree),
                "brief": _brief_tag,
            },
            ev_path,
        )
        receipts.append(
            {
                "node_id": node_id,
                "status": "dispatched",
                "short_id": short_id,
                "worktree": str(worktree),
            }
        )
    if report is not None:
        report["dispatched"] = sum(
            receipt.get("status") == "dispatched" for receipt in receipts
        )
        report["skipped"] = sum(
            receipt.get("status") == "skipped" for receipt in receipts
        )
    return receipts


# ---------------------------------------------------------------------------
# Join (epic x-956c, x-8d1d): spawn execute-waves joiners into a HELD worktree
# ---------------------------------------------------------------------------
#
# dispatch_lanes is one worker per node and a second `/target <id>` refuses
# (target init takes the node claim). Join is the complement: N
# `/fno:execute waves <plan>` workers run INSIDE the holder's worktree as
# visitors - they take task claims under their FNO_WORKER_NAME and never the
# node claim (Locked Decision 1). Worker count is bounded by the plan's
# ready-graph width, never by N alone (Locked Decision 2); joiner 1 of the
# epic made each worker's holder provable via FNO_WORKER_NAME.


class JoinRefuse(Exception):
    """A join precondition failed; ``code`` is the CLI exit code.

    2 = no live node claim (nothing to join), 3 = width 1 (a second worker
    has nothing to pull), 4 = no usable bound plan, 5 = already joined
    (live ``j-<node>-*`` workers exist).
    """

    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


def _emit_join_event(kind: str, **data: Any) -> None:
    """Best-effort join telemetry through the agents journal. Never raises.

    Same posture as the spawn gate's ``_emit_gate_event``: telemetry can
    never change a join outcome.
    """
    try:
        from fno.agents import events

        events.emit(kind, **data)
    except Exception:  # noqa: BLE001 - telemetry never changes a join outcome
        pass


_BAND_RANK = {"low": 0, "medium": 1, "high": 2}


class _PlanTaskGraph(NamedTuple):
    """One parse of a plan's Execution Strategy, shared by every join reader.

    ``_plan_parallel_width``, ``_plan_wave_bands``, and the write-policy
    renderer each used to load + parse the same file; this is the one load.
    ``wave_bands`` is parallel to ``waves`` (``""`` when a wave resolves to no
    legal band, the same fallback rule ``_plan_wave_bands`` always applied).
    """

    waves: list[tuple[str, list[str]]]
    surfaces: dict[str, list[str]]
    wave_bands: list[str]
    declared_blockers: dict[str, set[str]]


def _plan_task_graph(plan_path: Path) -> _PlanTaskGraph:
    """Load and parse a plan's task graph exactly once for all join readers."""
    from fno.plan._doc import load_plan
    from fno.plan.brief import parse_execution_strategy

    doc = load_plan(plan_path)
    fallback = str(doc.frontmatter.get("difficulty") or "").strip().lower()
    fallback = fallback if fallback in _BAND_RANK else ""
    body = doc.get_section("Execution Strategy")
    if body is None:
        return _PlanTaskGraph([], {}, [], {})
    raw = parse_execution_strategy(body)

    waves: list[tuple[str, list[str]]] = []
    wave_bands: list[str] = []
    for wave_data in raw.get("waves", []):
        if not isinstance(wave_data, dict):
            continue
        tasks_raw = wave_data.get("tasks", [])
        tasks = [str(t) for t in tasks_raw] if isinstance(tasks_raw, list) else [str(tasks_raw)]
        waves.append((str(wave_data.get("mode", "sequential")), tasks))
        band = str(wave_data.get("difficulty") or "").strip().lower()
        wave_bands.append(band if band in _BAND_RANK else fallback)
    surfaces = {
        str(t["id"]): [str(s) for s in t.get("surface", [])]
        for t in raw.get("tasks", [])
        if isinstance(t, dict) and t.get("id")
    }
    declared_blockers = {
        str(t["id"]): {str(d) for d in t.get("blocked_by", [])}
        for t in raw.get("tasks", [])
        if isinstance(t, dict) and t.get("id") and t.get("blocked_by_declared", False)
    }
    return _PlanTaskGraph(waves, surfaces, wave_bands, declared_blockers)


def _width_from_graph(graph: _PlanTaskGraph) -> int:
    """Largest simultaneously-ready task set across a full topological walk.

    Mirrors the orchestrator's scheduling rule (``effective_blockers`` +
    ``apply_partition_edges`` + ``ready_tasks``) so join's width can never
    disagree with what a joined worker's ``--ready`` query actually returns:
    a declared ``blocked_by`` wins outright, an undeclared task inherits the
    previous wave's whole task list, and every wave serializes collision
    groups (the derived partition edges). A plan with no Execution Strategy
    yields 0; the caller refuses anything below 2.
    """
    from fno.graph.collision import HIDDEN_SHARED_OUTPUT_ROOTS, partition

    waves = graph.waves
    surfaces = graph.surfaces
    if not waves:
        return 0

    blockers: dict[str, set[str]] = {
        tid: set(deps) for tid, deps in graph.declared_blockers.items()
    }
    all_ids: list[str] = []
    for pos, (_mode, tasks) in enumerate(waves):
        for tid in tasks:
            all_ids.append(tid)
            if tid not in blockers:
                blockers[tid] = set(waves[pos - 1][1]) if pos else set()

    # Derived within-wave edges (the orchestrator's partition_edges): group
    # order serializes, unevaluated tasks wait out the evaluated ones.
    #
    # Runs for EVERY wave, mirroring `apply_partition_edges`. Both used to skip
    # non-parallel waves, so two tasks in one `sequential` wave editing the
    # same file read as simultaneously ready and the label named `sequential`
    # scheduled MORE in parallel than the one named `parallel`. These two
    # implementations of one scheduling rule must change together or join's
    # width stops matching what a joined worker's --ready query returns, which
    # is the invariant this function's docstring promises.
    for pos, (mode, tasks) in enumerate(waves):
        items = [(tid, set(surfaces.get(tid, []))) for tid in tasks]
        groups, unevaluated = partition(items, shared_roots=HIDDEN_SHARED_OUTPUT_ROOTS)
        edges: dict[str, list[str]] = {}
        for group in groups:
            ordered = [tid for tid in tasks if tid in group]
            for prev, cur in zip(ordered, ordered[1:]):
                edges.setdefault(cur, []).append(prev)
        evaluated = [tid for tid in tasks if tid not in unevaluated]
        for tid in tasks:
            if tid in unevaluated and evaluated:
                edges[tid] = list(evaluated)
        for tid, extra in edges.items():
            base = blockers.get(tid)
            if base is None:
                base = set(waves[pos - 1][1]) if pos else set()
            blockers[tid] = base | set(extra)

    done: set[str] = set()
    width = 0
    while True:
        batch = [
            tid for tid in all_ids
            if tid not in done and blockers.get(tid, set()) <= done
        ]
        if not batch:
            break
        width = max(width, len(batch))
        done.update(batch)
    stuck = [tid for tid in all_ids if tid not in done]
    if stuck:
        # A cycle or a blocker naming an unknown id: no schedule exists, and
        # reporting this as "width 1" would send the operator hunting for a
        # narrow plan instead of the malformed edges.
        raise ValueError(
            f"task graph is unsolvable (cycle or unknown blocker): "
            f"{', '.join(sorted(set(stuck))[:5])}"
        )
    return width


def _plan_parallel_width(plan_path: Path) -> int:
    """Width of the plan at ``plan_path`` (see :func:`_width_from_graph`)."""
    return _width_from_graph(_plan_task_graph(plan_path))


# The operator's sizing for a bare join, as one table: workers asked for,
# indexed by the node's priority and the plan's highest wave band. Code in
# one function's table, never config - the audit measured config files
# already contradicting each other on three keys, and a second decider is
# how that happens. The width rule caps whatever this asks for.
_JOIN_WORKER_TABLE = {
    "p0": {"low": 3, "medium": 4, "high": 4},
    "p1": {"low": 2, "medium": 3, "high": 4},
    "p2": {"low": 1, "medium": 2, "high": 3},
    "p3": {"low": 1, "medium": 1, "high": 2},
}


def _highest_wave_band(graph: _PlanTaskGraph) -> str:
    """The strongest band the plan carries, or the medium default when it
    carries none (an unbanded plan sizes like the default band everywhere
    else in fno). The first row of the distinct-bands ranking is that
    highest band; no second implementation of the ranking."""
    bands = _bands_from_graph(graph)
    return bands[0] if bands else "medium"


def _derive_join_workers(graph: _PlanTaskGraph, priority: str) -> tuple[int, str]:
    """The worker count a bare join asks for, plus the band it read so the
    receipt can name its inputs. An unknown priority sizes as p2, the
    backlog default."""
    band = _highest_wave_band(graph)
    row = _JOIN_WORKER_TABLE.get(str(priority or "").strip().lower())
    if row is None:
        row = _JOIN_WORKER_TABLE["p2"]
    return row[band], band


# Write roots every joiner legitimately needs regardless of band: version
# control, fno's own per-worktree state (claims, briefs, join state), and the
# build/cache directories a test run touches. ``.git/hooks`` and ``.git/config``
# stay denied by the sandbox's own ug5 list.
INFRA_WRITE_ROOTS = (
    ".git/",
    ".fno/",
    "target/",
    "node_modules/",
    ".venv/",
    "__pycache__/",
)


class JoinWritePolicy(NamedTuple):
    """One band's sandbox verdict: ``enforced`` carries the two lists; the
    refusals to narrow (``overlapping``, ``unevaluated``) carry none (LD2)."""

    band: str
    verdict: str  # enforced | overlapping | unevaluated
    allow_write: Optional[tuple[str, ...]]
    deny_edit: Optional[tuple[str, ...]]


def render_join_write_policy(
    graph: _PlanTaskGraph, bands: list[str]
) -> dict[str, JoinWritePolicy]:
    """Per-band write policy rendered from the plan's own partition (LD1/LD2).

    Items are ``(band, union of that band's task surfaces)`` run through
    ``collision.partition`` at BAND grain with no shared roots: the infra
    roots are granted to every band by design, so they must not merge bands
    the way the task-grain hidden roots do. A singleton group with at least
    one usable path renders ``allow_write`` (own surfaces + INFRA_WRITE_ROOTS)
    and ``deny_edit`` (the peer bands' surfaces minus its own - the peer set,
    never the complement, so the deny list cannot name a path the worker
    legitimately needs). Anything else reads ``overlapping`` or
    ``unevaluated`` and carries no policy.
    """
    from fno.graph.collision import partition

    band_surfaces: dict[str, set[str]] = {}
    for pos, (_mode, tasks) in enumerate(graph.waves):
        band = graph.wave_bands[pos] if pos < len(graph.wave_bands) else ""
        if not band:
            continue
        acc = band_surfaces.setdefault(band, set())
        for tid in tasks:
            acc.update(graph.surfaces.get(tid, []))
    items = [(band, set(paths)) for band, paths in band_surfaces.items()]
    groups, unevaluated = partition(items)
    group_of = {band: i for i, group in enumerate(groups) for band in group}
    policies: dict[str, JoinWritePolicy] = {}
    for band in bands:
        own = band_surfaces.get(band)
        if not own or band in unevaluated:
            policies[band] = JoinWritePolicy(band, "unevaluated", None, None)
            continue
        if len(groups[group_of[band]]) > 1:
            policies[band] = JoinWritePolicy(band, "overlapping", None, None)
            continue
        peers: set[str] = set()
        for other, paths in band_surfaces.items():
            if other != band:
                peers |= paths
        policies[band] = JoinWritePolicy(
            band,
            "enforced",
            tuple(sorted(own)) + INFRA_WRITE_ROOTS,
            tuple(sorted(peers - own)),
        )
    return policies


def _bands_from_graph(graph: _PlanTaskGraph) -> list[str]:
    """The plan's distinct wave bands, highest first (x-dadc).

    Only legal bands survive; an illegal wave spelling reads as unbanded and
    takes the frontmatter fallback, the same rule the orchestrator's
    ``_wave_band`` applies when it reports ``bands``. A plan with no band
    anywhere returns ``[]`` and the join degrades to joiner 2's shapeless
    spawn.
    """
    bands: set[str] = {b for b in graph.wave_bands if b}
    return sorted(bands, key=lambda b: -_BAND_RANK[b])

# Terminal states of the claude harness store (`claude agents --json --all`);
# anything else (working, blocked, a future spelling) reads as alive.
_HARNESS_TERMINAL_STATES = {"done", "stopped", "failed"}

# A joiner transcript untouched this long reads idle-dead even when the
# harness store has no row for it yet (registration can lag the spawn).
_JOINER_IDLE_WINDOW = 30 * 60


def _claude_harness_session_states() -> dict[str, str]:
    """``{sessionId: state}`` from the claude harness store, else ``{}``.

    Read-only; a missing CLI or an unreadable answer means "unknown" and the
    caller falls through to the transcript probe.
    """
    try:
        proc = subprocess.run(
            ["claude", "agents", "--json", "--all"],
            capture_output=True, text=True, timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return {}
    try:
        data = json.loads(proc.stdout)
        rows = data if isinstance(data, list) else data.get("agents", [])
        return {
            str(r["sessionId"]): str(r.get("state", ""))
            for r in rows
            if isinstance(r, dict) and r.get("sessionId")
        }
    except (TypeError, ValueError, KeyError):
        return {}


def _transcript_recently_active(session_id: str) -> bool:
    """Whether this claude session's transcript moved inside the idle window.

    The transcript is the last truth that outlives a dead daemon (liveness
    probes and stored status fields have both lied). No transcript at all is
    activity-nothing; an unreadable glob is activity-UNKNOWN and reads False
    here, so the caller treats it as dead only when the harness store also
    went quiet - the transcript is the second probe, never the only one.
    """
    if not session_id:
        return False
    projects = Path.home() / ".claude" / "projects"
    try:
        for transcript in projects.glob(f"*/{session_id}.jsonl"):
            age = time.time() - transcript.stat().st_mtime
            if age <= _JOINER_IDLE_WINDOW:
                return True
    except OSError:
        return False
    return False


def _live_joiner_names(node_id: str) -> list[str]:
    """Live roster names ``j-<node>-*``, probed - not the stored field.

    A second join into the same node rewrites the join brief (dropping the
    first join's band table) and then dies on the already-taken lead name -
    after nearly spawning duplicate workers. The live proof hit this when a
    JOINER itself re-ran join on its own node. But the registry's
    ``status: live`` is a stored field: a crashed daemon leaves it behind and
    a bare read would lock every later join out of the node. So a row counts
    only when something still answers: the claude harness store lists its
    session non-terminally, or its transcript moved inside the idle window.
    Join spawns are claude-only (the thread substrate), so the claude probes
    cover every row this guard can collide with.
    """
    from fno.paths import agents_registry_path

    try:
        reg = json.loads(Path(agents_registry_path()).read_text())
    except Exception:  # noqa: BLE001 - an unreadable registry must not block a join
        return []
    prefix = f"j-{node_id}-"
    candidates = [
        (str(row.get("name")), str(row.get("harness_session_id") or ""))
        for row in reg.get("agents", [])
        if str(row.get("name", "")).startswith(prefix) and row.get("status") == "live"
    ]
    if not candidates:
        return []
    harness_states = _claude_harness_session_states()
    live = []
    for name, session_id in candidates:
        state = harness_states.get(session_id)
        if state is not None and state not in _HARNESS_TERMINAL_STATES:
            live.append(name)
        elif state is None and _transcript_recently_active(session_id):
            live.append(name)
    return sorted(live)


def _plan_wave_bands(plan_path: Path) -> list[str]:
    """The plan's distinct wave bands, highest first (see :func:`_bands_from_graph`)."""
    return _bands_from_graph(_plan_task_graph(plan_path))


def _sandbox_brief_section(
    node_id: str,
    worker_bands: list[str],
    policies: dict,
    sandbox_on: bool,
) -> str:
    """The brief paragraph naming each worker's file set, so a refused write
    reads as a reason and not as a broken machine."""
    if not sandbox_on:
        return ""
    lines: list[str] = ["write partitions (join.sandbox is ON):"]
    any_policy = False
    for k, band in enumerate(worker_bands, start=1):
        pol = policies.get(band)
        verdict = pol.verdict if pol else "unevaluated"
        if pol is not None and pol.verdict == "enforced":
            any_policy = True
            lines.append(
                f"- j-{node_id}-{k} (band {band}) may write: "
                f"{', '.join(pol.allow_write or ())}. A write outside it is "
                f"refused at the Edit/Write layer and by the OS sandbox."
            )
        else:
            lines.append(
                f"- j-{node_id}-{k} (band {band or 'unbanded'}) is NOT "
                f"narrowed ({verdict}); the sandbox layer is off for it."
            )
    return "\n".join(lines) if any_policy else ""


def _sandbox_block(worktree: Path, policy: JoinWritePolicy) -> dict:
    """The claude sandbox settings block for one band's rendered policy.

    The workspace itself is writable under the sandbox by default (verified
    live 2026-08-28 with one headless sandboxed session), so the per-band
    separation rides ``denyWrite`` of the peer bands' surfaces - denyWrite
    wins over both allowWrite and the workspace default. allowWrite only
    opens what a joiner legitimately needs OUTSIDE the worktree: the global
    fno state dir (graph, claims ledger, mail), plus the worktree infra
    roots as a belt for setups where the workspace default does not apply.
    """
    from fno import paths

    root = Path(worktree).resolve()
    return {
        "enabled": True,
        "filesystem": {
            "allowWrite": [
                str(paths.state_dir()),
                *(str(root / r) for r in INFRA_WRITE_ROOTS),
            ],
            "denyWrite": [str(root / d) for d in (policy.deny_edit or ())],
        },
    }


def join_node(
    node_id: str, workers: Optional[int] = None, *, model: Optional[str] = None
) -> dict:
    """Join with a trace: emit ``join_dispatched`` / ``join_refused`` around it.

    The event carries the brief inputs already in hand (node, width,
    requested, spawned names, band map, lead) so an orchestration pass can
    read what join did without archaeology; the refusal carries node, exit
    code and reason. Emission is best-effort. See :func:`_join_node` for the
    join contract itself. ``requested`` records the resolved ask: the
    operator's ``--workers``, or the derived count when it was omitted.
    """
    try:
        receipt = _join_node(node_id, workers, model=model)
    except JoinRefuse as exc:
        _emit_join_event(
            "join_refused", node=node_id, code=exc.code, reason=exc.message
        )
        raise
    _emit_join_event(
        "join_dispatched",
        node=node_id,
        width=receipt["width"],
        requested=workers if workers is not None else len(receipt["spawned"]),
        spawned=receipt["spawned"],
        bands={name: lane.get("band", "") for name, lane in receipt["lanes"].items()},
        lead=receipt["lead"],
    )
    return receipt


def _join_node(
    node_id: str, workers: Optional[int] = None, *, model: Optional[str] = None
) -> dict:
    """Spawn width-bounded joiners into a held node's worktree (x-8d1d).

    Resolves the holder's worktree from the LIVE ``node:<id>`` claim (never a
    manifest snapshot), computes the bound plan's ready-graph width, and
    spawns ``/fno:execute waves <plan>`` workers there via ``fno agents spawn
    --substrate thread`` - one per distinct wave band when the plan carries
    bands (highest band first, the lead; each lane resolved per band by
    ``_grid_lane_for``), else ``min(workers, width - 1)`` shapeless workers
    (joiner 2). Either way the width rule caps the count: the node holder is
    one of the width workers. ``workers`` is the requested joiner count;
    ``None`` (the CLI default) derives the ask from the sizing table - the
    node's priority against the plan's highest wave band - instead of
    defaulting to one joiner, which is the default that kept bare joins
    from ever being worth running. The brief rides TWO channels: the file
    ``<worktree>/.fno/join-briefs/<node>.md`` (reaches daemon-forked workers,
    which the waves.md joiner posture reads) and TARGET_BRIEF (reaches lanes
    that inherit the spawner's env, e.g. panes); a banded brief also carries
    the per-worker band table, the band's durable channel beside the
    best-effort ``FNO_WORKER_BAND`` env export. The spawned process exports
    FNO_WORKER_NAME from ``--name``, so each joiner's task-claim holder is
    its own roster name (the joiner 1 contract; where the env export cannot
    reach, resolve_task_holder reads the roster binding). ``model`` rides as
    an explicit ``--model``: a typed model with no vendor implication
    overrides a config-injected default whose lane would refuse.

    Returns the receipt ``{"node", "worktree", "width", "priority", "band",
    "workers", "workers_source", "spawned", "lead", "lanes"}`` - the three
    sizing inputs ride beside the requested count and where it came from
    (``derived`` | ``explicit``), so the receipt answers "why this many".
    ``lanes`` maps each spawned name to its ``band``/``harness``/
    ``model``/``sandbox`` (``enforced`` | ``overlapping`` | ``unevaluated`` |
    ``off``, from ``config.join.sandbox`` and the plan's band partition; plus
    ``"grid": "declined"`` when the grid declined that band
    and the joiner rides the caller's default lane). Raises JoinRefuse (exit
    2/3/4/5) on a precondition failure and SpawnError when the lead spawn
    itself fails; a non-lead spawn failure warns to stderr and shrinks
    ``spawned`` instead of aborting the join.
    """
    from fno.claims.core import claim_status
    from fno.graph.collision import resolve_plan_path
    from fno.graph.store import read_graph
    from fno.paths import graph_json

    entry = next((e for e in read_graph(graph_json()) if e.get("id") == node_id), None)
    if entry is None:
        raise JoinRefuse(2, f"no graph node {node_id}")
    plan_raw = entry.get("plan_path")
    if not plan_raw:
        raise JoinRefuse(4, f"{node_id} has no bound plan")
    claim_key = f"node:{node_id}"
    status = claim_status(claim_key, root=_claims_root_for(claim_key))
    if status.get("state") != "live":
        raise JoinRefuse(
            2,
            f"{node_id} has no live node claim (state: {status.get('state')}); "
            "nothing to join",
        )
    worktree = str((status.get("metadata") or {}).get("worktree") or "").strip()
    if not worktree:
        raise JoinRefuse(
            2, f"live claim for {node_id} names no holder worktree; nothing to join"
        )
    if not Path(worktree).is_dir():
        raise JoinRefuse(
            2,
            f"holder worktree for {node_id} is gone ({worktree}); nothing to join",
        )
    live = _live_joiner_names(node_id)
    if live:
        raise JoinRefuse(
            5,
            f"{node_id} is already joined by {', '.join(live)}; join is not "
            "idempotent - a second run rewrites the brief and races the "
            "first join's spawns",
        )
    try:
        plan_path = resolve_plan_path(str(plan_raw))
        graph = _plan_task_graph(plan_path)
        width = _width_from_graph(graph)
        bands = _bands_from_graph(graph)
    except ValueError as exc:
        raise JoinRefuse(4, f"{node_id} plan task graph unsolvable: {str(exc)[:140]}") from exc
    except Exception as exc:  # noqa: BLE001 - an unreadable plan is no plan to join
        raise JoinRefuse(4, f"{node_id} bound plan unreadable: {str(exc)[:160]}") from exc
    if width < 2:
        raise JoinRefuse(
            3, f"width {width}: a second worker has nothing to pull"
        )
    # A bare join derives its ask from the sizing table; an explicit count
    # passes through untouched. Both paths print the inputs beside the
    # count, so the receipt answers "why this many" without re-deriving.
    priority = str(entry.get("priority") or "").strip().lower()
    if workers is None:
        workers, sizing_band = _derive_join_workers(graph, priority)
        workers_source = "derived"
    else:
        sizing_band = _highest_wave_band(graph)
        workers_source = "explicit"
    if priority not in _JOIN_WORKER_TABLE:
        priority = "p2"
    # Lane count and band assignment are two questions, and one number used to
    # answer both. `len(bands)` sat inside this min, so the number of DISTINCT
    # difficulty bands capped the joiner count and `--workers` could not
    # override it: a single-band plan of width 6 got one joiner. Bands ROUTE
    # difficulty (a pulling worker takes only tasks at or below its own band);
    # their cardinality was never a capacity. Measured over the 45 joinable
    # banded plans since bands existed, 14 were capped below width - 1, losing
    # 34 of 197 joiner slots in a fortnight.
    #
    # The width rule still caps, and it is the real one: the node holder is one
    # of the width workers, so joiners stay under it.
    # One switch covers BOTH enforcement layers (the OS allowlist and the
    # Edit/Write guard): partial enforcement that reads as enforcement is the
    # failure mode this feature exists to prevent, so they are not separately
    # toggleable. An overlapping or unevaluated band is never narrowed (LD2).
    from fno.config import load_settings

    sandbox_on = bool(load_settings().join.sandbox)

    count = min(max(1, workers), width - 1)
    if bands:
        # THE BAND IS THE ISOLATION UNIT WHEN ENFORCEMENT IS ON.
        # `render_join_write_policy` is keyed by BAND, so two lanes sharing a
        # band share one `allow_write` set and get an EMPTY `deny_edit` (the
        # deny list is the peer bands' surfaces, and a reused band has no
        # peer). Cycling under enforcement would hand several sessions
        # byte-identical, mutually unrestricted policies - the isolation the
        # partition exists to provide, silently gone. So the band cardinality
        # legitimately caps the lane count HERE, and only here.
        #
        # With enforcement off (the default) it does not, and that is the
        # whole point of the fix: `len(bands)` used to sit in this min
        # unconditionally, so a single-band plan of width 6 got one joiner and
        # `--workers` could not override it. Bands ROUTE difficulty - a pulling
        # worker takes only tasks at or below its own band - and their
        # cardinality was never a capacity. Measured over the 45 joinable
        # banded plans since bands existed, 14 were capped below width - 1,
        # losing 34 of 197 joiner slots in a fortnight.
        if sandbox_on:
            count = min(count, len(bands))
        # Cycle rather than truncate, so lane count and band assignment stop
        # being the same number. `bands` is highest-first, so the lead keeps
        # the highest band and extra lanes reuse the available ones in order.
        worker_bands = [bands[k % len(bands)] for k in range(count)]
    else:
        worker_bands = [""] * count
    policies = render_join_write_policy(graph, worker_bands) if sandbox_on else {}

    lead = f"j-{node_id}-1"
    # The joiner brief rides a FILE, not only TARGET_BRIEF: a daemon-forked
    # worker inherits the claude daemon's env (x-6de8), so the env export in
    # the spawn below reaches panes but not this lane's serving sessions.
    # waves.md's joiner posture reads this file before dispatching - the band
    # table is the band's durable channel for the same reason.
    brief_dir = Path(worktree) / ".fno" / "join-briefs"
    try:
        brief_dir.mkdir(parents=True, exist_ok=True)
        band_table = ""
        if bands:
            rows = "\n".join(
                f"| j-{node_id}-{k} | {band} |"
                for k, band in enumerate(worker_bands, start=1)
            )
            band_table = (
                "\nband table (your band is your row's; the lead is the "
                "highest band):\n\n"
                "| worker | band |\n"
                "|--------|------|\n"
                f"{rows}\n\n"
                "For every dispatch round: run the --ready query with "
                "`--band <your row's band>` and take only the FIRST entry of "
                "`ready` - claim it, work it to done, then re-query. Entries "
                "under `above_band` are not yours. Never dispatch the whole "
                "set; a joined worker that races the whole set undoes the "
                "partition this table encodes.\n\n"
                "A band can REPEAT in the table above: lane count follows the "
                "plan's width, not the number of distinct bands. If a peer "
                "shares your band you will sometimes both claim the same first "
                "entry and one of you will lose the claim. That is the claim "
                "doing its job, not an error - re-query and take the next "
                "entry. Never retry a lost claim.\n"
            )
        # The brief NAMES the hub and used to leave the rule implicit. The
        # spawn brief carries "you are the mail hub" / "mail hub is <lead>"
        # per worker, but the FILE every joiner reads carried neither line, so
        # a joiner learned who the lead was and not what that meant. Observed
        # cost on this node's own run: two joiners independently mailed the
        # node holder the same finding, one of them duplicating the other.
        brief_text = (
            f"# Joiner brief: {node_id}\n\n"
            f"lead and mail hub: {lead}\n\n"
            f"Route findings and questions through {lead}, not to the node "
            f"holder or a king. The lead talks up; everyone else talks to the "
            f"lead. That is what keeps one session from being addressed by "
            f"every joiner at once, and it is why two joiners relaying the "
            f"same finding separately is a defect rather than diligence. If "
            f"you ARE {lead}, you are the hub: collect, dedupe, and send one "
            f"message up.\n"
            f"{band_table}\n"
        )
        sandbox_section = _sandbox_brief_section(
            node_id, worker_bands, policies, sandbox_on
        )
        if sandbox_section:
            # Appended only when present: with the flag off (or no enforced
            # band) the brief stays byte-identical to the historical shape.
            brief_text += f"{sandbox_section}\n"
        brief_text += (
            f"Before dispatching any worker, claim ONE ready task via "
            f"`fno backlog task update {node_id} <task> --status in_progress` "
            f"(the waves.md 3e step; your roster name binds the holder).\n"
        )
        (brief_dir / f"{node_id}.md").write_text(brief_text)
        # The per-worker policy file is written BEFORE any spawn: both
        # enforcement layers (the Edit/Write guard and the sandbox block the
        # session reads at start) must see it from the worker's first tool
        # call, and a Bash write refused without the file would read as a
        # broken machine rather than a partition.
        policy_dir = Path(worktree) / ".fno" / "join-partition"
        for k, band in enumerate(worker_bands, start=1):
            pol = policies.get(band)
            if pol is None or pol.verdict != "enforced":
                continue
            name = f"j-{node_id}-{k}"
            policy_dir.mkdir(parents=True, exist_ok=True)
            (policy_dir / f"{name}.json").write_text(
                json.dumps(
                    {
                        "worker": name,
                        "band": band,
                        "verdict": "enforced",
                        "allow_write": list(pol.allow_write or ()),
                        "deny_edit": list(pol.deny_edit or ()),
                        "sandbox": _sandbox_block(Path(worktree), pol),
                    }
                )
            )
    except OSError as exc:
        raise SpawnError(f"join cannot write the joiner brief: {str(exc)[:160]}") from exc
    spawned: list[str] = []
    lanes: dict[str, dict] = {}
    for k, band in enumerate(worker_bands, start=1):
        name = f"j-{node_id}-{k}"
        brief = (
            f"lead joiner of {node_id}: you are the mail hub for j-{node_id}-*"
            if k == 1
            else f"joiner of {node_id}: mail hub is {lead}"
        ) + (
            f"; your band is {band}"
            if band
            else ""
        ) + (
            f". Before dispatching any worker, claim ONE ready task via "
            f"fno backlog task update {node_id} <task> --status in_progress "
            f"(the waves.md 3e step; your roster name binds the holder)"
        )
        # The grid picks this band's lane, not the node's: the joiner pulls
        # the waves its band can carry, so its lane must match the band.
        # An unbanded joiner (or an explicit --model) skips the grid and
        # rides the caller's default lane. The thread substrate is
        # claude-only (hard error on any other harness), so a grid lane
        # naming another harness reads as declined - taking only its model
        # would put a foreign-vendor model on the claude lane, the exact
        # mismatch the spawn refuses.
        lane_h: Optional[str] = None
        lane_m: Optional[str] = None
        grid_why: Optional[str] = None
        pol = policies.get(band) if sandbox_on else None
        enforced = pol is not None and pol.verdict == "enforced"
        if band:
            grid_h, grid_m, grid_why = _grid_lane_for(
                {
                    "difficulty": band,
                    "plan_path": str(plan_path),
                    "priority": entry.get("priority"),
                },
                model=model,
                provider=None,
            )
            if grid_h in (None, "claude"):
                lane_h, lane_m = grid_h, grid_m
            else:
                # The grid PICKED, and the pick is unusable here: the thread
                # substrate is claude-only, so a foreign harness reads as
                # declined. That path left `grid_why` None (the grid had no
                # complaint), so the receipt said `declined` with no reason -
                # the exact ambiguity this field exists to remove, on the
                # decline most likely to puzzle a reader.
                grid_why = f"grid=harness-not-claude ({grid_h})"
        cmd = [
            *_subprocess_util.fno_py_cmd(),
            # `thread`, deliberately, overriding spawn's own `pane` default.
            # A thread hosts no pane, so the team runs invisibly and the mail
            # hub has no watchable surface - a real cost, and flipping this one
            # word plus the shipped `--split`/`--tab` flags would buy a visible
            # squad. It stays a thread because the flip REVERSES the 2026-08-24
            # operator ruling that sent spawns to threads after a closable pane
            # was ended mid-flight, and that reversal is gated on pane-keeper
            # durability being confirmed end to end: a keeper must survive
            # `fno mux kill-server` and re-adopt the SAME pid.
            #
            # Not confirmed as of 2026-09-02. `fno mux pane keeper list` shows
            # only the operator's own main pane, no joiner survivor to read
            # durability off, and the one machine that could run the kill-server
            # test was hosting live joiners at the time. Untested, not false.
            # Do not flip this word on the strength of pane-keeper EXISTING;
            # run the restart test first. An invisible team that survives beats
            # a visible one that dies.
            "agents", "spawn", "--substrate", "thread",
            # Explicit harness, like _spawn_worker's resolved axis: an
            # untagged spawn leaves the lane unresolved, and this machine's
            # codex-scoped agents.defaults.model then injects onto it and
            # trips the vendor-mismatch refusal (live proof, 2026-08-27).
            "--harness", lane_h or "claude",
            # x-571f shape: an explicit model rides as a spawn flag.
            *(("--model", lane_m or model) if (lane_m or model) else ()),
            "--cwd", worktree, "--name", name,
            *(
                (
                    "--sandbox-write-policy",
                    str(Path(worktree) / ".fno" / "join-partition" / f"{name}.json"),
                )
                if enforced
                else ()
            ),
            f"/fno:execute waves {plan_path}",
        ]
        spawn_env = {**os.environ, "TARGET_BRIEF": brief}
        if band:
            # Best-effort channel: reaches panes, not daemon-forked threads;
            # the brief's band table is the durable one (x-6de8).
            spawn_env["FNO_WORKER_BAND"] = band
        proc = subprocess.run(
            cmd, capture_output=True, text=True, timeout=600,
            env=spawn_env,
        )
        identity = ""
        if proc.returncode == 0:
            identity = _spawn_receipt_identity(proc.stdout)
        if not identity:
            detail = (proc.stderr or proc.stdout or "").strip()[:200]
            if not spawned:
                raise SpawnError(f"join spawn {name} failed: {detail}")
            print(f"join: spawn {name} failed: {detail}", file=sys.stderr)
            continue
        spawned.append(name)
        lanes[name] = {
            "band": band,
            "harness": lane_h,
            "model": lane_m,
            # enforced | overlapping | unevaluated | off: a run that could
            # not narrow says so rather than looking like a run that did.
            "sandbox": (
                pol.verdict if pol is not None else "unevaluated"
            )
            if sandbox_on
            else "off",
        }
        if band and lane_h is None:
            lanes[name]["grid"] = "declined"
            # WHY it declined, not just that it did. A declined lane spawns on
            # the ambient default, which is the most expensive thing the fleet
            # can buy, and `grid=no-inventory-declared` (an empty
            # `config.routing.models`) reads identically to a momentarily busy
            # one without this. Verbatim from resolve_grid's chain, so the
            # receipt vocabulary stays single-sourced.
            if grid_why:
                lanes[name]["grid_reason"] = grid_why
    return {
        "node": node_id,
        "worktree": worktree,
        "width": width,
        "priority": priority,
        "band": sizing_band,
        "workers": workers,
        "workers_source": workers_source,
        "spawned": spawned,
        "lead": lead,
        "lanes": lanes,
    }


# ---------------------------------------------------------------------------
# Claim helpers (route each key like the `fno agents claim` CLI's _node_aware_root)
# ---------------------------------------------------------------------------


def _claims_root_for(key: str):
    """Resolve the claims root for a key (delegates to the shared helper).

    Global-id kinds (``node:``/``dispatch:``/``reconcile:``) live in the global
    ($HOME) root; repo-local keys use the cwd/env default (canonical repo root,
    honoring FNO_CLAIMS_ROOT). Delegating to fno.claims.io.claims_root_for keeps
    advance, reconcile_dispatch, spawn-guard, and the `fno agents claim` CLI on ONE
    routing rule so they cannot drift -- and roots the boot-window dispatch:<id>
    token globally so cross-repo dispatchers dedup against each other."""
    from fno.claims.io import claims_root_for

    return claims_root_for(key)


def _walker_key() -> str:
    """``walker:<canonical_repo_root>`` - byte-identical to the key the Rust
    loop runtime writes for walker-scoped claims."""
    from fno.paths import resolve_canonical_repo_root

    return f"walker:{resolve_canonical_repo_root()}"


def _observe_node_claim(
    node_id: str,
    node_cwd: Optional[str] = None,
    *,
    enforce_failure_limit: bool = True,
    emit: bool = True,
) -> DispatchClaimObservation:
    """Family-2 pre-dispatch verdict shared by Python and shell routes."""
    try:
        from fno.target_cli import _classify_node_claim

        verdict, info = _classify_node_claim(node_id)
    except Exception:  # noqa: BLE001 - an unreadable claim must not crash advance
        verdict, info = "free", None
    info = info or {}
    try:
        from fno.agents.truth_status import resolve_truth_status

        truth = resolve_truth_status(node_id).get("state") or "unknown"
    except Exception:  # noqa: BLE001 - truth is diagnostic, claim verdict is authority
        truth = "unknown"
    claim_state = info.get("state")
    holder = info.get("holder") or "unknown"
    occupied = verdict in ("ours", "foreign_live")
    dead_action = (
        None
        if occupied or not enforce_failure_limit
        else _refuse_repeated_dead_dispatch(node_id, node_cwd)
    )
    action = (
        "blocked"
        if occupied
        else dead_action
        if dead_action is not None
        else "redispatch"
        if verdict == "dead_predecessor"
        else "dispatch"
    )

    if emit:
        from fno.agents import events as agent_events

        agent_events.emit(
            EVENT_CLAIM_OBSERVED,
            node_id=node_id,
            claim_verdict=verdict,
            claim_state=claim_state,
            holder=holder,
            truth_status=truth,
            action=action,
        )
    if emit and claim_state in ("stale", "suspect"):
        message = (
            f"dispatch {action} for {node_id}: node claim is {claim_state}, "
            f"prior holder={holder}, truth_status={truth}"
        )
        print(f"advance: WARNING: {message}", file=sys.stderr)
        from fno.notify._impl import send_notification

        send_notification("footnote: contested node dispatch", message)
    return DispatchClaimObservation(
        verdict=verdict,
        claim_state=claim_state,
        holder=holder,
        truth_status=truth,
        action=action,
    )


def _node_dispatch_block_reason(node_id: str, node_cwd: Optional[str] = None) -> Optional[str]:
    """One pre-birth decision for node ownership plus boot reservation."""
    observation = _observe_node_claim(node_id, node_cwd)
    if observation.blocks_dispatch:
        return observation.refusal_reason
    if _claim_is_live(f"dispatch:{node_id}"):
        return "already-claimed"
    return None


def _claim_is_live(key: str, node_cwd: Optional[str] = None) -> bool:
    # "occupied" for dispatch: a live OR a suspect claim (x-ba4b) blocks
    # selection. suspect = TTL-unexpired, dead pid (respawned worker); the TTL
    # still protects the slot, so selection must skip it, never steal.
    if key.startswith("node:"):
        return _observe_node_claim(key.removeprefix("node:"), node_cwd).blocks_dispatch

    from fno.claims.core import claim_status

    try:
        return claim_status(key, root=_claims_root_for(key)).get("state") in (
            "live",
            "suspect",
        )
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
    """The journal to emit into: the caller's root, else the resolved one.

    The ``project_root is None`` leg MUST go through
    :func:`fno.paths.project_events_json`, never ``Path.cwd()``. A hand-built
    cwd path consults neither ``FNO_EVENTS_PATH`` (the hermetic journal pin) nor
    ``FNO_REPO_ROOT``, so an unpathed emit under test wrote into the developer's
    live journal. Measured on 2026-08-27: eight ``advance_skipped`` rows naming
    ``ab-hp`` and ``ab-950001``, node ids that exist only in
    ``cli/tests/integration/test_backlog_reconcile.py``.

    A caller that HOLDS a root still wins, which is the contract
    ``project_events_json`` documents.
    """
    if project_root is not None:
        return Path(project_root) / ".fno" / "events.jsonl"
    from fno.paths import project_events_json

    return project_events_json()


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
    model: Optional[str] = None,
    provider: Optional[str] = None,
) -> AdvanceResult:
    """Dispatch the next now-unblocked node, if armed and unclaimed.

    A dispatch-time ``model``/``provider`` (from ``fno backlog advance -m/-p``)
    is the operator's in-the-moment word and outranks the node's own annotation
    (Locked Decision 1); absent, the node's ``model``/``provider`` keys are used.

    Invoked ONLY after the node-close write commits (keyed by ``closed_node_id``,
    AC1-RACE), so within one reconcile/post-merge run the closed node is already
    reflected before ``next`` is read. Emits exactly one decision event and is
    strictly non-fatal: any failure resolves to advance_failed/advance_skipped
    and the host op continues.
    """
    ev_path = events_path if events_path is not None else _events_path(project_root)
    # AC3-HP: resolved ONCE (armed AND rank together, like advance_dependents
    # and advance_epic) and reused below - two independent calls could
    # observe different env/config state between them (FNO_AUTO_CONTINUE is
    # deliberately same-process settable, for tests), stamping a rank that
    # does not describe the armed value it is attached to.
    armed, rank = _auto_continue_resolve(project_root)

    def skip(
        reason: str,
        *,
        node_id: Optional[str] = None,
        detail: Optional[str] = None,
        provider: Optional[str] = None,
        retry_at: Optional[float] = None,
    ) -> AdvanceResult:
        data: dict = {"reason": reason, "rank": rank}
        if closed_node_id:
            data["closed_node_id"] = closed_node_id
        if node_id:
            data["node_id"] = node_id
        if provider:
            data["provider"] = provider
        if retry_at is not None:
            data["retry_at"] = retry_at
        if detail:
            data["detail"] = detail[:200]
        _emit(EVENT_SKIPPED, data, ev_path)
        return AdvanceResult(
            "skipped", EVENT_SKIPPED, reason=reason, node_id=node_id, detail=detail
        )

    def failed(node_id: str, error: str) -> AdvanceResult:
        data = {"node_id": node_id, "error": error[:200], "rank": rank}
        if closed_node_id:
            data["closed_node_id"] = closed_node_id
        _emit(EVENT_FAILED, data, ev_path)
        return AdvanceResult(
            "failed", EVENT_FAILED, reason="spawn-failed", node_id=node_id, detail=error
        )

    # 1. Armed?
    if not armed:
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
    block_reason = _node_dispatch_block_reason(node_id, node_cwd)
    if block_reason:
        return skip(block_reason, node_id=node_id)

    # 4b. Quota-aware defer (x-5d3e). advance IS an autonomous path, so it may
    #     defer when the resolved provider has no headroom and defer_dispatch is
    #     on. Fail-open + opt-in: off by default, p0 never defers, UNKNOWN never
    #     defers. The node stays in ready (skip mutates nothing); the next tick
    #     after the reset dispatches it. Never fatal - a defer read failure just
    #     proceeds to dispatch.
    #     The route decision itself is shared with `fno agents dispatch` so
    #     both autonomous launchers stay / defer / cut over identically; the
    #     tuple it returns is pinned for this attempt and the worker never
    #     re-switches. The dispatch_failover receipt is emitted after the spawn
    #     below so it only lands when a worker actually launched.
    failover_record: Optional[str] = None
    failover_harness: Optional[str] = None
    failover_from: Optional[str] = None
    failover_window: Optional[str] = None
    failover_reason: Optional[str] = None
    try:
        from fno.adapters.providers.loader import effective_active
        from fno.agents.autonomous_route import (
            launch_is_pinned,
            select_autonomous_route,
        )

        # Match the SAME provider precedence the spawn below uses
        # (eff_provider = provider arg -> node pin -> active default), so the
        # quota decision evaluates the provider the worker will actually run on,
        # not a mismatched active record (x-5d3e review). `effective_active` (not
        # the raw `.active` pointer) is what `fno agents dispatch` probes: with managed
        # rotation past the pointer the two launchers would otherwise probe
        # different records and disagree about the route.
        provider_id = (
            provider
            or node.get("provider")
            # Scoped to the NODE's repository, like every other read in the
            # route decision: resolving the active record from the dispatcher's
            # own checkout would evaluate one project's quota for another
            # project's launch.
            or effective_active(repo_root=Path(node_cwd) if node_cwd else None)
            or ""
        )
        route = select_autonomous_route(
            provider_id=provider_id,
            priority=node.get("priority"),
            # Every explicit launch intent pins: a provider or model named on the
            # invocation or the node, and a configured dispatch harness (which
            # outranks quota policy by precedence). A pinned launch may still
            # defer; it must never be rerouted, or a claude-only model would ride
            # a cutover onto codex.
            pinned=launch_is_pinned(
                node, provider=provider, model=model, node_cwd=node_cwd
            ),
            node_cwd=node_cwd,
            node_id=node_id,
        )
    except Exception:  # noqa: BLE001 - a quota read must never wedge advance
        route = None
    if route is not None and route.action == "defer":
        return skip(
            "quota-deferred",
            node_id=node_id,
            provider=route.source_record,
            retry_at=route.retry_at,
        )
    if route is not None and route.action == "cutover":
        failover_record = route.record_id
        failover_harness = route.harness
        failover_from = route.source_record
        failover_window = route.window
        failover_reason = route.reason

    # 5. Reserve dispatch:<id> (O_EXCL dedup + boot-window bridge token).
    from fno.claims.core import CLAIM_UNAVAILABLE, acquire_claim

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
    except CLAIM_UNAVAILABLE:
        return skip("already-claimed", node_id=node_id)
    except Exception as exc:  # noqa: BLE001
        return skip("claim-error", node_id=node_id, detail=str(exc))

    # 6. Spawn the worker. On any failure, release the reservation so the node
    #    stays re-dispatchable (a later reconcile retries - AC2-FR). The release
    #    is non-raising (_safe_release) so the decision event below always lands.
    try:
        if failover_record is not None:
            # --provider is the HARNESS (a record id would be rejected by the spawn
            # front door's known-provider gate); the account rides
            # --dispatch-account, which the front door resolves and applies where
            # the harness is exec'd (x-c33e).
            eff_provider = failover_harness
        else:
            eff_provider = provider if provider is not None else node.get("provider")
        _brief, _brief_tag = _autobrief.resolve_dispatch_brief(node)
        short_id = _spawn_worker(
            node_id,
            node_cwd,
            node.get("slug") or node.get("title"),
            model=_route_resolve.node_model(
                node, explicit=model, provider=eff_provider, resolve_difficulty=False
            ),
            provider=eff_provider,
            harness=failover_harness,
            dispatch_account=failover_record,
            node=node,
            verb=node.get("dispatch_verb"),
            brief=_brief,
            caller="advance",
            events_path=ev_path,
        )
    except SpawnAlreadyRunning:
        _safe_release(dispatch_key, holder, dispatch_root)
        return skip("already-claimed", node_id=node_id)
    except Exception as exc:  # noqa: BLE001
        _safe_release(dispatch_key, holder, dispatch_root)
        return failed(node_id, str(exc))

    # 7. Dispatched. Leave dispatch:<id> to expire by TTL: the worker now owns
    #    (or is acquiring) node:<id>, which guards later dispatches.
    if failover_record is not None:
        # The cutover receipt (from -> to), emitted only now that a worker
        # actually launched: a routing decision is not a completed cutover, so a
        # failed spawn must not leave a receipt claiming one. Paired with the
        # advance_dispatched below, not a competing decision. `to` is the record
        # id; --provider got the harness, the account env selected it.
        _emit(
            EVENT_FAILOVER,
            {
                "node_id": node_id,
                "from": failover_from or "",
                "to": failover_record,
                "harness_to": failover_harness or "",
                "window": failover_window or "",
                "reason": failover_reason or "",
            },
            ev_path,
        )
    _emit(
        EVENT_DISPATCHED,
        {
            "node_id": node_id,
            "short_id": short_id,
            "agent_name": _worker_agent_name(node_id, node.get("slug") or node.get("title")),
            "brief": _brief_tag,
            "rank": rank,
            **({"closed_node_id": closed_node_id} if closed_node_id else {}),
        },
        ev_path,
    )
    if verbose:
        print(
            f"advance: dispatched {node_id} -> target worker {short_id} (brief={_brief_tag})",
            file=sys.stderr,
        )
    # Wake the active-backlog drain daemon (node x-c070): a successor may now be
    # unblocked. Best-effort; the poll floor is the guarantee.
    try:
        from fno.active_backlog import touch_nudge

        touch_nudge()
    except Exception:
        pass
    return AdvanceResult("dispatched", EVENT_DISPATCHED, node_id=node_id, short_id=short_id)


# ---------------------------------------------------------------------------
# advance_dependents() - cross-project successor dispatch (G1 / AC5-FR)
# ---------------------------------------------------------------------------
#
# advance() above dispatches the project-scoped `next` ready node (same-project
# auto-continue). It deliberately CANNOT reach a dependent in another project:
# `fno backlog next --project <closed.project>` filters foreign nodes out. So a
# merge of A (project etl) never dispatches B (project web, blocked_by A).
#
# advance_dependents() closes that gap by following `blocked_by` EDGES instead of
# a project-scoped selection: for each now-unblocked DIRECT dependent in a
# DIFFERENT project, it spawns `/target --no-merge <dep> --cwd <dep project root>`.
# The two paths are intentionally distinct (Domain Pitfall: dispatch-by-edge vs
# select-next must not be conflated) and share the same dispatch:<id> dedup +
# node:<id> liveness + spawn machinery, so the same successor observed by both
# advance() and advance_dependents() (or by two triggers) dispatches at most once.


def _direct_dependents(closed_node_id: str, closed_project: Optional[str]) -> list[dict]:
    """Ready, direct ``blocked_by`` dependents of the closed node.

    Reads the graph (``read_graph`` recomputes ``status`` at read), so a
    dependent whose only open blocker was the just-closed node already reads
    ``ready`` here. Returns minimal dicts
    ``{id, project, slug, cwd, model, difficulty, cross_project}``.

    RC1 (x-33b2): returns BOTH same-project and cross-project dependents, each
    tagged with ``cross_project = (project != closed_project)``. The caller routes
    a same-project dependent through the node's OWN recorded ``cwd`` (advance()'s
    same-project spawn) and a cross-project one through its work-map root. The two
    routes share the same ``dispatch:<id>`` + ``node:<id>`` dedup so a successor
    seen by both this path and advance()'s ``next`` selection dispatches at most
    once. advance_dependents fails closed when ``closed_project`` is None (it
    cannot classify, so prefers dispatching nothing over a misroute). Raises on a
    graph read error so advance_dependents skips rather than guessing (Failure
    Modes: Errors).
    """
    from fno.graph.store import read_graph
    from fno.paths import graph_json
    from fno.graph.ladder import is_cold_dispatchable

    entries = read_graph(graph_json())
    # Containers are never dispatched as workers (x-33b2): a dependent that is
    # itself some other node's `parent` is an epic, and `/target` builds its
    # leaves, not the box. Mirror cmd_next's `_pick_ready` exclusion on this
    # edge-following path so a now-unblocked epic dependent is skipped here too.
    parent_ids = {
        e.get("parent") for e in entries
        if isinstance(e, dict) and isinstance(e.get("parent"), str)
    }
    # Shared guard inputs (dead-ancestor + stale-ready quarantine): the same
    # selection_guards() the `next` picker applies, so a converge dispatch never
    # revives a leaf under a killed epic or a long-abandoned ready node.
    by_id = {e.get("id"): e for e in entries if isinstance(e, dict) and e.get("id")}
    staleness_days = _guard_staleness_days()
    out: list[dict] = []
    for e in entries:
        if not isinstance(e, dict):
            continue
        if closed_node_id not in (e.get("blocked_by") or []):
            continue
        # "now-unblocked" == ready OR a plan-less idea (x-e24a): blocker done + no
        # other open blocker. A still-blocked dependent reads `blocked` (status
        # derivation resolves an unresolved blocker to blocked, never idea), so an
        # idea-status dependent is genuinely unblocked and cold-dispatchable; a
        # claimed/done/deferred one reads its own bucket. A linked-but-undesigned
        # stub (Rung.IDEA) is filtered out by is_cold_dispatchable's rung check.
        if e.get("status") != "ready" and not is_cold_dispatchable(e):
            continue
        if selection_guards(e, by_id, staleness_days=staleness_days):
            continue  # dead-ancestor or stale-quarantine - do not revive
        # An in-flight PR (pr_number set, not yet merged-and-closed) still reads
        # `ready` because completed_at is only set at close. The project-scoped
        # `next` path excludes these via _has_unmerged_open_pr; mirror it here so
        # a dependent already in review is not re-dispatched once the dispatch TTL
        # expires and a later reconcile/advance fires for the same blocker (codex
        # P2). The PID-based node:<id> claim dies with the builder, leaving no
        # in-flight signal behind, so this field guard is the durable one.
        if e.get("pr_number") and not e.get("completed_at"):
            continue
        node_id = e.get("id")
        if not node_id:
            continue
        if node_id in parent_ids:
            continue  # epic/container dependent - build its leaves, not the box
        # RC1: no longer exclude same-project successors here. advance()'s `next`
        # selection can skip past an already-claimed/epic head and never reach a
        # genuinely-unblocked same-project dependent (the reported starvation);
        # tag the dependent so the caller spawns it via the same-project route
        # (cwd = its own root), deduped against `next` by dispatch:<id>+node:<id>.
        out.append({
            "id": node_id,
            "project": e.get("project"),
            "slug": e.get("slug") or e.get("title"),
            "cwd": e.get("cwd"),
            # x-571f: carry the model pin so _dispatch_one_dependent threads it.
            # difficulty rides alongside so the grid resolver sees the work axis.
            "model": e.get("model"),
            "difficulty": e.get("difficulty"),
            "cross_project": (e.get("project") or None) != (closed_project or None),
        })
    return out


def _project_unblocked(node_ids: list[str]) -> None:
    """Best-effort graph->doc projection for the given ids (advance's unblocked
    dependents). Reads the graph fresh and calls the shared converger directly
    (never imports graph.cli). Never raises: a projection failure must not block
    the merge-triggered dispatch that follows."""
    if not node_ids:
        return
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json
        from fno.plan._project import project_graph_nodes

        project_graph_nodes(read_graph(graph_json()), node_ids)
    except Exception as exc:  # noqa: BLE001 - convergence, never fatal
        sys.stderr.write(f"warning: unblocked-dependent projection failed: {exc}\n")


def _walker_live_at(project_root: str) -> bool:
    """True when the DEPENDENT project's own megawalk/active-backlog walker is
    live. Its ``walker:<root>`` claim lives under ``<root>/.fno/claims`` (the
    megawalk loop writes it from that project's checkout), which is a different
    claims root from this process's, so check it there explicitly. A live walker
    there will pick the node up itself; spawning would double-launch into that
    repo (codex P2). Best-effort: a probe error never blocks dispatch."""
    from fno.claims.core import claim_status

    try:
        # live OR suspect (x-ba4b): a suspect walker claim is still an occupied
        # lane; treat it as live so we never double-launch into that repo.
        return claim_status(
            f"walker:{project_root}", root=Path(project_root)
        ).get("state") in ("live", "suspect")
    except Exception:  # noqa: BLE001 - a probe error must not block dispatch
        return False


def _converge_one(
    node_meta: dict,
    root: str,
    ev_path: Path,
    verbose: bool,
    *,
    cross_project: bool = False,
    mission: Optional[str] = None,
    closed_node_id: Optional[str] = None,
    model: Optional[str] = None,
    provider: Optional[str] = None,
    rank: Optional[str] = None,
) -> AdvanceResult:
    """The one shared converge-dispatch core: dedup, reserve, spawn, one receipt.

    Extracted from ``_dispatch_one_dependent`` (x-9608 K1) so the two triggers -
    merge-advance's per-dependent dispatch and the epic advance / mission-drain
    fan-out - run the IDENTICAL claim choreography + spawn + single decision event
    and can never fork. The caller owns root resolution (same-project cwd vs
    cross-project work-map vs epic-advance work-map) and passes the resolved ``root``;
    everything downstream is common. ``mission`` (the epic id, epic-advance/drain only)
    tags each receipt so a mission dispatch is attributable in the event stream;
    ``closed_node_id`` (merge-advance only) keys the AC1-RACE trigger. The two are
    mutually exclusive in practice but both are optional here.

    Emits exactly one of advance_dispatched / advance_skipped / advance_failed
    (LD#12) and returns the matching AdvanceResult. Never raises: a spawn failure
    releases the reservation (node stays re-dispatchable) and resolves to failed.
    """
    node_id = node_meta["id"]
    slug = node_meta.get("slug") or node_meta.get("title")

    def _tag(data: dict) -> dict:
        # Common receipt fields: the trigger key (closed_node_id) and/or the
        # mission tag ride every event this converge emits. Omitted when unset so
        # a merge-advance receipt stays byte-identical to pre-K1.
        if closed_node_id:
            data["closed_node_id"] = closed_node_id
        if mission:
            data["mission"] = mission
        if rank:
            data["rank"] = rank
        return data

    def skip(reason: str, detail: Optional[str] = None) -> AdvanceResult:
        data = _tag({"reason": reason, "node_id": node_id})
        if detail:
            data["detail"] = detail[:200]
        _emit(EVENT_SKIPPED, data, ev_path)
        return AdvanceResult(
            "skipped", EVENT_SKIPPED, reason=reason, node_id=node_id, detail=detail
        )

    def failed(error: str) -> AdvanceResult:
        _emit(EVENT_FAILED, _tag({"node_id": node_id, "error": error[:200]}), ev_path)
        return AdvanceResult(
            "failed", EVENT_FAILED, reason="spawn-failed", node_id=node_id, detail=error
        )

    # The spawned worker runs in the target repo, not this one. If that project
    # already has a live walker, let it claim the node - spawning here would launch
    # a second target into that repo (codex P2). Checked at the target root because
    # its walker claim lives under that root's .fno/claims.
    if _walker_live_at(root):
        return skip("walker-live")

    # Already being worked? Same liveness gate as advance() step 4. This is what
    # makes epic-advance idempotent (AC1-EDGE): a re-run finds the first pass's workers
    # holding node:<id> and dispatches nothing, WITHOUT depending on the 3-min
    # dispatch TTL still being live.
    block_reason = _node_dispatch_block_reason(node_id, root)
    if block_reason:
        return skip(block_reason)

    from fno.claims.core import CLAIM_UNAVAILABLE, acquire_claim

    dispatch_key = f"dispatch:{node_id}"
    holder = f"advance:{os.getpid()}"
    dispatch_root = _claims_root_for(dispatch_key)
    try:
        acquire_claim(
            dispatch_key,
            holder,
            ttl_ms=_DISPATCH_TTL_MS,
            reason=f"converge dispatch for {node_id}"
            + (f" (mission {mission})" if mission else "")
            + (f" (dep of {closed_node_id})" if closed_node_id else ""),
            root=dispatch_root,
        )
    except CLAIM_UNAVAILABLE:
        return skip("already-claimed")
    except Exception as exc:  # noqa: BLE001
        return skip("claim-error", detail=str(exc))

    try:
        eff_provider = provider if provider is not None else node_meta.get("provider")
        _brief, _brief_tag = _autobrief.resolve_dispatch_brief(node_meta)
        short_id = _spawn_worker(
            node_id,
            root,
            slug,
            model=_route_resolve.node_model(
                node_meta, explicit=model, provider=eff_provider, resolve_difficulty=False
            ),
            provider=eff_provider,
            verb=node_meta.get("dispatch_verb"),
            brief=_brief,
            node=node_meta,
            caller="_converge_one",
            events_path=ev_path,
        )
    except SpawnAlreadyRunning:
        _safe_release(dispatch_key, holder, dispatch_root)
        return skip("already-claimed")
    except Exception as exc:  # noqa: BLE001
        _safe_release(dispatch_key, holder, dispatch_root)
        return failed(str(exc))

    _emit(
        EVENT_DISPATCHED,
        _tag(
            {
                "node_id": node_id,
                "short_id": short_id,
                "agent_name": _worker_agent_name(node_id, slug),
                "cross_project": cross_project,
                "brief": _brief_tag,
            }
        ),
        ev_path,
    )
    if verbose:
        _scope = f"mission {mission} " if mission else ""
        _kind = "cross-project" if cross_project else "same-project"
        print(
            f"advance: dispatched {_scope}{_kind} {node_id} -> "
            f"target worker {short_id} (--cwd {root}) (brief={_brief_tag})",
            file=sys.stderr,
        )
    return AdvanceResult("dispatched", EVENT_DISPATCHED, node_id=node_id, short_id=short_id)


def _dispatch_one_dependent(
    dep: dict, closed_node_id: str, ev_path: Path, verbose: bool,
    *, model: Optional[str] = None, provider: Optional[str] = None,
    rank: Optional[str] = None,
) -> AdvanceResult:
    """Resolve one dependent's own project root, then converge-dispatch it.

    Reuses advance()'s claim + spawn + event machinery via :func:`_converge_one`.
    The ``--cwd`` root differs by route (RC1 / LD#2): a CROSS-project dependent
    launches in its work-map root; a SAME-project dependent launches in the node's
    OWN recorded ``cwd`` (NEVER the work-map root, which for a foreign-shaped record
    could land it on a protected branch where the bg worker dies). Everything
    downstream of root resolution - dedup, spawn, single decision event - is
    ``_converge_one`` and is identical to the epic-advance path.
    """
    node_id = dep["id"]
    cross_project = bool(dep.get("cross_project"))

    def skip(reason: str, detail: Optional[str] = None) -> AdvanceResult:
        data: dict = {"reason": reason, "node_id": node_id, "closed_node_id": closed_node_id}
        if rank:
            data["rank"] = rank
        if detail:
            data["detail"] = detail[:200]
        _emit(EVENT_SKIPPED, data, ev_path)
        return AdvanceResult("skipped", EVENT_SKIPPED, reason=reason, node_id=node_id, detail=detail)

    project = dep.get("project")
    if not project:
        return skip("no-project")
    from fno.graph._intake import project_root_from_settings

    if cross_project:
        # Cross-project: resolve the dependent's OWN project root from the work
        # map. Reject (never guess a cwd for) an unmapped project, surfacing it by
        # name so the operator sees which project is missing from
        # config.work.workspaces (Boundaries).
        root = project_root_from_settings(project)
        if not root:
            return skip("unmapped-project", detail=project)
    else:
        # Same-project (RC1 / LD#2): launch in the node's OWN project root. Resolve
        # it the way advance()'s `next` path does - the work-map root is the cwd
        # authority and recorded `cwd` is fallback data (codex P2: a stale/absent
        # recorded cwd would otherwise start the worker in the wrong checkout). For
        # a same-project node this resolves to its OWN project root, never a
        # foreign/cross-project root, so LD#2's anti-misroute intent holds. Fail
        # closed if neither resolves (rather than guess canonical main).
        root = project_root_from_settings(project) or dep.get("cwd")
        if not root:
            return skip("no-cwd")

    return _converge_one(
        dep, root, ev_path, verbose,
        cross_project=cross_project, closed_node_id=closed_node_id,
        model=model, provider=provider, rank=rank,
    )


def advance_dependents(
    *,
    closed_node_id: str,
    closed_project: Optional[str] = None,
    project_root: Optional[Path] = None,
    events_path: Optional[Path] = None,
    verbose: bool = False,
    model: Optional[str] = None,
    provider: Optional[str] = None,
) -> list[AdvanceResult]:
    """Dispatch the closed node's now-unblocked direct dependents (G1 + RC1).

    Called alongside advance() on the merge event (reconcile + ``backlog
    advance --closed``). Gated on the same opt-in as advance() and strictly
    non-fatal. Covers BOTH same-project dependents (RC1, x-33b2: advance()'s
    `next` can skip past an unbuildable head and starve them) and cross-project
    dependents (G1). Emits exactly one decision event per dependent (dispatched /
    skipped / failed); a clean run with no dependents emits nothing and returns
    ``[]`` (Boundaries: a zero-dependent close is a no-op).
    """
    ev_path = events_path if events_path is not None else _events_path(project_root)

    # Same opt-in gate as advance(); resolved against the closed node's project
    # context. advance() already recorded the disabled/walker-live decision for
    # this merge event, so we add no duplicate event here - just no-op.
    armed, rank = _auto_continue_resolve(project_root)
    if not armed:
        return []
    if _claim_is_live(_walker_key()):
        return []

    # Fail closed (RC1 Errors / LD#2): without the closed node's project we cannot
    # tell a same-project dependent (spawn --cwd its own root) from a cross-project
    # one (spawn --cwd its work-map root). Misrouting a same-project node through
    # the cross-project path lands it on a protected branch where the bg worker
    # dies, so prefer dispatching nothing. RC2 ensures both callers now resolve
    # closed_project from the graph, so a falsy value here is the genuine last
    # resort. `not closed_project` (vs `is None`) also catches an empty-string
    # project, which would otherwise misclassify every dependent as cross-project.
    if not closed_project:
        _emit(
            EVENT_SKIPPED,
            {"reason": "closed-project-unknown", "closed_node_id": closed_node_id, "rank": rank},
            ev_path,
        )
        return [AdvanceResult("skipped", EVENT_SKIPPED, reason="closed-project-unknown")]

    try:
        deps = _direct_dependents(closed_node_id, closed_project)
    except Exception as exc:  # noqa: BLE001 - never guess on a read error
        _emit(
            EVENT_SKIPPED,
            {
                "reason": "dependents-error",
                "closed_node_id": closed_node_id,
                "detail": str(exc)[:200],
                "rank": rank,
            },
            ev_path,
        )
        return [AdvanceResult("skipped", EVENT_SKIPPED, reason="dependents-error", detail=str(exc))]

    # Repaint each now-unblocked dependent's doc so a merge-gated dependent
    # carries current mirror fields the moment its blocker closes (US5).
    # Best-effort: convergence, never a dispatch blocker.
    _project_unblocked([d["id"] for d in deps])

    return [
        _dispatch_one_dependent(
            dep, closed_node_id, ev_path, verbose, model=model, provider=provider, rank=rank
        )
        for dep in deps
    ]


# ---------------------------------------------------------------------------
# Epic advance / converge (x-9608 K1): fan out an epic's ready leaf children
# ---------------------------------------------------------------------------
#
# The mission's manual entry point (and, later, K2's per-tick drain reuse the
# same _converge_one core). A "mission" is an epic node plus its transitive
# children (the parent EDGE is the mission key; mission_id is untouched -
# Locked Decision 2). Epic advance marks the mission active (a durable graph field on
# the epic, readable from Python AND Rust, cleared on cascade-close), then runs
# converge pass 1: for every currently-ready LEAF child across all projects, it
# resolves the child's work-map root and converge-dispatches it, one receipt per
# child, respecting per-project max_lanes + an overall --max. It is idempotent
# (a re-run dispatches nothing already node:<id>-claimed) and per-child isolated
# (one child's failure never aborts the pass).

# Mission-activation events (registered in cli/src/fno/events/schema.yaml).
EVENT_MISSION_ACTIVATED = "mission_activated"
EVENT_MISSION_DEACTIVATED = "mission_deactivated"

# The graph field epic advance sets on the epic node to mark the mission active.
# Durable (graph.json), crash-safe, and read by both Python (read_graph) and
# Rust (K2's drain loop reads graph.json directly). Cleared on cascade-close
# (_cascade_close_parents) or an explicit `--epic <id> --stop`.
MISSION_ACTIVE_FIELD = "mission_active"


@dataclass(frozen=True)
class AdvanceEpicResult:
    """Outcome of one advance_epic() run."""

    epic_id: str
    error: Optional[str] = None  # no-such-node | not-a-container | disabled | walker-live
    activated: bool = False
    deactivated: bool = False
    all_done: bool = False
    dispatched: tuple = ()  # node ids successfully dispatched this pass
    child_results: tuple = ()  # AdvanceResult per attempted child


def _ready_leaf_children(epic_id: str) -> list[dict]:
    """Ready LEAF children of an epic, across ALL projects.

    Shells the shipped ``fno backlog ready --parent <epic> --all`` surface (the
    SAME selection `next`/lane-fill use) so the epic advance never diverges from it: the
    result is already container-filtered, claim-filtered, open-PR-filtered,
    batch-filtered, and rank-sorted. Transitive children via ``--parent``
    semantics (descendants_of). Raises on a garbled response so the caller skips
    rather than guessing (Failure Modes: Errors).
    """
    cmd = [
        *_subprocess_util.fno_py_cmd(),
        "backlog", "ready", "--parent", epic_id, "--all",
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    if proc.returncode != 0:
        raise RuntimeError(
            f"fno backlog ready --parent {epic_id} exited {proc.returncode}: "
            f"{proc.stderr.strip()[:200]}"
        )
    out = (proc.stdout or "").strip()
    if not out or out == "null":
        return []
    nodes = json.loads(out)
    if not isinstance(nodes, list):
        raise RuntimeError(
            f"fno backlog ready --parent returned an unexpected shape: {out[:200]}"
        )
    return [n for n in nodes if isinstance(n, dict) and n.get("id")]


def _live_workers_by_project() -> dict[str, int]:
    """Count occupied per-project lanes to seed max_lanes.

    max_lanes is a per-project concurrency cap, so a project that already has a
    worker occupies a lane and the epic advance must count it before deciding how many
    MORE to dispatch. Counts BOTH live/suspect ``node:<id>`` claims (a running
    worker) AND live/suspect ``dispatch:<id>`` reservations (the boot-window
    bridge a just-dispatched worker holds before it owns node:<id>) - else an
    immediate rerun during that boot window would under-count the lane and
    over-dispatch a second same-project child past the cap (codex P2). Deduped by
    node id so a child holding both claims counts once. Best-effort: any read
    fault degrades to an empty map (no seed), never blocks the pass.
    """
    counts: dict[str, int] = {}
    try:
        from fno.claims.core import list_claims
        from fno.claims.io import global_claims_root
        from fno.graph.store import read_graph
        from fno.paths import graph_json

        root = global_claims_root()
        occupied: set[str] = set()
        for prefix in ("node:", "dispatch:"):
            for claim in list_claims(prefix=prefix, include_stale=False, root=root):
                key = claim.get("key")
                if isinstance(key, str):
                    occupied.add(key.removeprefix(prefix))
        if not occupied:
            return counts
        by_id = {
            e["id"]: e for e in read_graph(graph_json())
            if isinstance(e, dict) and isinstance(e.get("id"), str)
        }
        for nid in occupied:
            proj = (by_id.get(nid) or {}).get("project")
            if proj:
                counts[proj] = counts.get(proj, 0) + 1
    except Exception:  # noqa: BLE001 - a live-count read must never block the epic advance
        return counts
    return counts


def _binding_provider() -> Optional[str]:
    """The configured provider with the least lane headroom, or None.

    The unpinned epic advance could route anywhere, so the most constrained
    CONFIGURED provider is the one whose cap binds the next spawn. One walk,
    shared by :func:`_spawn_headroom` (the width) and the explain surfaces
    (the row that explains the width).
    """
    from fno.agents import spawn_gate
    from fno.config import load_settings

    binding: Optional[str] = None
    binding_remaining: Optional[int] = None
    for name, budget in dict(load_settings().agents.provider_limits).items():
        cap = spawn_gate.provider_lanes_cap(budget)
        if cap is None:
            continue  # an uncapped provider cannot bind anything
        remaining = cap - spawn_gate.provider_live_count(name)
        if binding_remaining is None or remaining < binding_remaining:
            binding, binding_remaining = name, remaining
    return binding


def _spawn_headroom(provider: Optional[str] = None) -> int:
    """Dispatch width from the spawn gate's own counters.

    ``config.parallel.max_lanes`` once gated the epic advance here, but it was
    a second concurrency authority beside the real one: a spawn is refused by
    the spawn gate's ``max_live`` and per-provider ``lanes``, and those are the
    caps that actually bind. The knob is retired (the deletion ruling stands;
    the key stays parseable for one release with a deprecation line), and the
    width now derives from the gate's own counters through the SAME functions
    ``fno agents top`` and ``advance --explain`` read, so no surface can
    disagree with the refusal that follows it:

    - fleet: ``agents.max_live`` minus the live census slot count
    - provider: ``lanes`` minus the live count for ``provider``; with no pin,
      the most-constrained CONFIGURED provider bounds the next spawn, because
      the grid may route it anywhere

    The number is advisory width, not the refusal - the gate still refuses at
    spawn time. Fleet or provider headroom at or below zero returns 0: the
    fleet is full, and dispatching would only manufacture refusals. A failed
    reading degrades to 1 (the conservative single lane the retired config
    default carried) with a warning naming what could not be read.
    """
    try:
        from fno.agents import spawn_gate
        from fno.config import load_settings

        agents_cfg = load_settings().agents
        fleet_remaining = int(agents_cfg.max_live) - spawn_gate.census().slot_count
        limits = dict(agents_cfg.provider_limits)
        if provider is not None:
            from fno.agents.spawn_defaults import resolve_lane_vendor

            # The pin is a HARNESS (`--provider` resolves on the harness axis);
            # provider_limits is keyed by VENDOR. Map through the shipped
            # resolver and fall back to the raw pin, which may already be a
            # vendor. A harness pin read against this vendor-keyed table
            # directly would miss (`codex` is not a key) and silently drop the
            # one cap that binds.
            pin_vendor = resolve_lane_vendor([], harness=provider) or provider
            budgets = {pin_vendor: limits.get(pin_vendor)}
        else:
            binding = _binding_provider()
            budgets = {} if binding is None else {binding: limits.get(binding)}
        provider_remaining: Optional[int] = None
        for name, budget in budgets.items():
            cap = spawn_gate.provider_lanes_cap(budget)
            if cap is None:
                continue  # an uncapped provider cannot bound the width
            remaining = cap - spawn_gate.provider_live_count(name)
            if provider_remaining is None or remaining < provider_remaining:
                provider_remaining = remaining
        bound = [fleet_remaining]
        if provider_remaining is not None:
            bound.append(provider_remaining)
        return max(0, min(bound))
    except Exception as exc:  # noqa: BLE001 - degrade to the conservative lane, loudly
        _LOG.warning("spawn headroom unreadable, degrading to 1 lane: %s", exc)
        return 1


def _set_mission_active(epic_id: str, active: bool) -> bool:
    """Set/clear the epic's ``mission_active`` graph field. Returns whether it changed.

    One locked graph mutation via locked_mutate_graph (never a direct Edit/Write -
    the HARD-GATE). Idempotent: setting an already-active mission (or clearing an
    inactive one) is a no-op that returns False.
    """
    from fno.graph._intake import _find_node
    from fno.graph.store import locked_mutate_graph
    from fno.paths import graph_json

    changed = [False]

    def mutator(entries):
        node = _find_node(entries, epic_id)
        if node is None:
            return entries
        if active:
            if node.get(MISSION_ACTIVE_FIELD) is not True:
                node[MISSION_ACTIVE_FIELD] = True
                changed[0] = True
        elif MISSION_ACTIVE_FIELD in node:
            node.pop(MISSION_ACTIVE_FIELD, None)
            changed[0] = True
        return entries

    locked_mutate_graph(graph_json(), mutator)
    return changed[0]


def advance_epic(
    epic_id: str,
    *,
    max_dispatch: Optional[int] = None,
    project_root: Optional[Path] = None,
    events_path: Optional[Path] = None,
    verbose: bool = False,
    model: Optional[str] = None,
    provider: Optional[str] = None,
    stop: bool = False,
    continuation: bool = False,
) -> AdvanceEpicResult:
    """Advance (or stop) an epic mission: mark active + converge pass 1.

    Refuses a non-container node by name (an epic's work is its children, never
    the box). ``stop`` deactivates the mission and dispatches nothing. Otherwise
    marks the mission active and fans out every currently-ready LEAF child across
    all projects via the shared _converge_one core, respecting per-project
    ``config.parallel.max_lanes`` and an overall ``max_dispatch`` (--max). All
    children already done -> a no-op receipt + deactivate (verify the cascade
    closed the epic). Gated on the same auto-continue opt-in and strictly
    non-fatal; each child's failure is isolated (its own receipt, never aborts
    the pass). Idempotent: a re-run dispatches nothing already node:<id>-claimed.

    ``continuation`` is the K2 daemon-drain mode: NEVER (re)activate the mission,
    and refuse (retire) an already-inactive one. Activation is the operator's
    kickoff act; a daemon tick that raced an operator ``--stop`` must not undo it,
    so a continuation pass on an inactive epic dispatches nothing and returns
    ``deactivated`` (the drain loop then retires).
    """
    ev_path = events_path if events_path is not None else _events_path(project_root)

    from fno.graph._intake import _find_node, descendants_of
    from fno.graph.store import read_graph
    from fno.paths import graph_json

    try:
        entries = read_graph(graph_json())
    except Exception as exc:  # noqa: BLE001 - a graph read fault skips cleanly
        return AdvanceEpicResult(epic_id, error=f"graph-error: {str(exc)[:120]}")

    epic = _find_node(entries, epic_id)
    if epic is None:
        return AdvanceEpicResult(epic_id, error="no-such-node")
    canon = epic["id"]

    # Refuse a non-container by name: only an epic (some node's parent) is a
    # mission. A leaf has no children to fan out; an epic advance on it is an operator
    # error, not a silent no-op.
    from fno.graph.cli import _container_ids

    if canon not in _container_ids(entries):
        return AdvanceEpicResult(canon, error="not-a-container")

    # Explicit stop: deactivate the mission, dispatch nothing.
    if stop:
        _set_mission_active(canon, False)
        _emit(EVENT_MISSION_DEACTIVATED, {"epic_id": canon, "reason": "stop"}, ev_path)
        return AdvanceEpicResult(canon, deactivated=True)

    # Same opt-in gate as advance()/advance_dependents. A live walker owning THIS
    # repo would pick nodes up itself; the epic-advance verb is the explicit converge tool, so a
    # global walker is a skip. (Per-child, a foreign-repo walker is handled in
    # _converge_one's own _walker_live_at.) Unlike the merge-advance path, this
    # standalone epic verb has no paired advance() call to record the decision, so
    # emit the skip receipt here or a gated epic advance is silent in the event stream
    # (codex P2 - LD#12 parity).
    armed, rank = _auto_continue_resolve(project_root)
    if not armed:
        _emit(EVENT_SKIPPED, {"reason": "disabled", "mission": canon, "rank": rank}, ev_path)
        return AdvanceEpicResult(canon, error="disabled")
    if _claim_is_live(_walker_key()):
        _emit(EVENT_SKIPPED, {"reason": "walker-live", "mission": canon, "rank": rank}, ev_path)
        return AdvanceEpicResult(canon, error="walker-live")

    # All descendants already done -> mission complete: verify the cascade closed
    # the epic, deactivate, emit a no-op receipt. (_container_ids guaranteed at
    # least one child above.)
    descendants = descendants_of(entries, canon)
    by_id = {e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)}
    all_done = bool(descendants) and all(
        (by_id.get(d) or {}).get("completed_at") for d in descendants
    )
    if all_done:
        _set_mission_active(canon, False)
        epic_done = bool((by_id.get(canon) or {}).get("completed_at"))
        _emit(
            EVENT_MISSION_DEACTIVATED,
            {"epic_id": canon, "reason": "complete", "epic_closed": epic_done},
            ev_path,
        )
        return AdvanceEpicResult(canon, deactivated=True, all_done=True)

    # Mark the mission active (durable graph field) before dispatching, so a crash
    # mid-fanout still leaves the mission drainable by K2. Emit the activation
    # receipt once, on the first epic advance (a re-run of an already-active mission
    # skips the event but still runs the converge pass - idempotent recovery).
    # CONTINUATION (K2 daemon drain): never reactivate; refuse an inactive epic so
    # an operator --stop between drain ticks sticks (the loop retires next tick).
    if continuation:
        if not (by_id.get(canon) or {}).get("mission_active"):
            return AdvanceEpicResult(canon, deactivated=True)
    elif _set_mission_active(canon, True):
        _emit(EVENT_MISSION_ACTIVATED, {"epic_id": canon}, ev_path)

    # Ready leaf children across all projects, via the shipped selection surface.
    try:
        children = _ready_leaf_children(canon)
    except Exception as exc:  # noqa: BLE001 - never guess on a read error
        _emit(
            EVENT_SKIPPED,
            {"reason": "children-error", "mission": canon, "detail": str(exc)[:200], "rank": rank},
            ev_path,
        )
        return AdvanceEpicResult(
            canon, activated=True,
            child_results=(AdvanceResult("skipped", EVENT_SKIPPED, reason="children-error"),),
        )

    # Width: spawn-gate headroom (fleet + provider). Live workers already
    # consumed their capacity inside the read (the census and provider counts
    # subtract them), so the bound here is how many MORE spawns this pass may
    # make - not a per-project threshold. A --provider pin reads that
    # provider's lanes; unpinned, the most constrained configured provider
    # bounds it. An overall --max caps total dispatches this run.
    max_lanes = _spawn_headroom(provider)

    results: list[AdvanceResult] = []
    dispatched: list[str] = []
    total = 0
    for child in children:
        if max_dispatch is not None and total >= max_dispatch:
            break  # overall cap reached; remaining ready children wait for a drain/re-run
        # A project-less child cannot be capped, mapped, or launched - skip it with
        # the accurate `no-project` reason (matching _dispatch_one_dependent), not a
        # misleading `unmapped-project` with an empty detail. Falsy check so an
        # empty-string project is treated as missing too (gemini).
        proj = child.get("project")
        if not proj:
            _emit(
                EVENT_SKIPPED,
                {"reason": "no-project", "node_id": child["id"], "mission": canon, "rank": rank},
                ev_path,
            )
            results.append(
                AdvanceResult("skipped", EVENT_SKIPPED, reason="no-project", node_id=child["id"])
            )
            continue
        # A mapped-but-absent project cannot be launched; surface it by name AND the
        # exact config key (Boundaries), then continue - one unmapped project never
        # blocks the others.
        from fno.graph._intake import project_root_from_settings

        root = project_root_from_settings(proj)
        if not root:
            results.append(_converge_skip_unmapped(child, proj, canon, ev_path, rank=rank))
            continue
        # Spawn-gate headroom exhausted this pass (0 = the fleet or the
        # binding provider is already full). The remaining ready children wait
        # for a drain / re-run; the gate itself still refuses at spawn time if
        # the world changed since the read.
        if total >= max_lanes:
            _emit(
                EVENT_SKIPPED,
                {"reason": "lane-cap", "node_id": child["id"], "mission": canon,
                 "detail": f"{proj}: headroom={max_lanes} (spawn gate)", "rank": rank},
                ev_path,
            )
            results.append(
                AdvanceResult("skipped", EVENT_SKIPPED, reason="lane-cap", node_id=child["id"])
            )
            continue
        res = _converge_one(
            child, root, ev_path, verbose,
            cross_project=True, mission=canon, model=model, provider=provider, rank=rank,
        )
        results.append(res)
        if res.decision == "dispatched":
            dispatched.append(res.node_id or child["id"])
            total += 1

    return AdvanceEpicResult(
        canon, activated=True,
        dispatched=tuple(dispatched), child_results=tuple(results),
    )


def _converge_skip_unmapped(
    child: dict, project: str, mission: str, ev_path: Path, *, rank: Optional[str] = None
) -> AdvanceResult:
    """Emit the loud unmapped-project skip for one epic-advance child (names the key)."""
    detail = f"{project} (add config.work.workspaces.<ws>.projects[].path)"
    data = {"reason": "unmapped-project", "node_id": child["id"], "mission": mission,
            "detail": detail}
    if rank:
        data["rank"] = rank
    _emit(EVENT_SKIPPED, data, ev_path)
    return AdvanceResult(
        "skipped", EVENT_SKIPPED, reason="unmapped-project",
        node_id=child["id"], detail=detail,
    )
