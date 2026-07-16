"""fno backlog advance - merge-triggered auto-continue dispatcher.

Node ab-3cd195b6. When a backlog node's PR merges, a merge-detector
(``fno backlog reconcile`` or the /pr merged skill) calls this verb after the
node-close write commits. If auto-continue is armed for the project and no live
walk owns it, advance dispatches a fresh background ``/target`` worker (with the
merge posture from ``config.dispatch.auto_merge``, default ``no-merge``) for the
next now-unblocked node, so a merge-gated epic walks itself group-by-
group across merges with no manual re-invocation.

Locked Decisions this module embodies:
  1. Decoupled from the loop driver - driven by the merge event, so megawalk /
     /target / /megatron all inherit auto-continue (no driver-specific code).
  4. Fire-and-forget dispatch: ``fno agents spawn`` -> ``/target [no-merge] <id>``
     (the ``no-merge`` token is gated on ``config.dispatch.auto_merge``; x-4391).
  5. Concurrency via ``fno claim``: honor ``walker:<root>`` (no double-dispatch
     during a live walk); reserve ``dispatch:<id>`` (O_EXCL dedup + bridge token
     that outlives this short-lived process until the worker owns ``node:<id>``,
     LD#11 / AC1-CLAIM - mirrors handoff.sh + dispatch-node.sh).
  6. advance never merges the PR itself - it dispatches a worker whose merge
     posture comes from ``config.dispatch.auto_merge`` (default ``no-merge``);
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

from fno import _subprocess_util
from fno import route_resolve as _route_resolve

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

        return bool(load_settings().auto_continue.enabled)
    except Exception as exc:  # noqa: BLE001 - fail-safe to disabled (AC2-ERR)
        # Diagnosable without changing the safety posture: false-disabled is
        # strictly safer than false-enabled for a background dispatcher, but a
        # silent swallow would hide a genuinely-broken settings load from an
        # operator wondering why the chain never advances.
        _LOG.debug("auto_continue_enabled: settings read failed, defaulting off: %s", exc)
        return False


# Decision-event kinds (registered in cli/src/fno/events/schema.yaml).
EVENT_DISPATCHED = "advance_dispatched"
EVENT_SKIPPED = "advance_skipped"
EVENT_FAILED = "advance_failed"
# x-0676: paired receipt (not a decision) emitted just before an advance_dispatched
# when on_exhaustion=failover rotates off an exhausted provider.
EVENT_FAILOVER = "dispatch_failover"
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


# A node with no `domain` set collapses into ONE lane bucket (not one lane
# each), so a domain-less backlog never fans out into undifferentiated lanes.
_DOMAIN_UNSET = ""


def _live_lane_domains(*, claims_root: Optional[Path] = None) -> set[str]:
    """Domains currently held by live lane slots, for distinct-domain seeding.

    LD#8 recomputes distinctness against the live-claim world, not just this
    call's picks: a lane already working a ``code`` node must stop a fill from
    selecting another ``code`` node (else two same-domain lanes run concurrently,
    the exact collision domain-lane parallelism exists to prevent). Each lane
    records its ``domain`` in slot metadata at acquire time, so peer-lane domains
    are readable here without a per-node lookup. A slot with no recorded domain
    (e.g. one taken via the bare ``fno claim lane-acquire`` CLI) collapses to the
    ``_DOMAIN_UNSET`` bucket - conservatively blocking co-schedule with an
    unknown-domain lane rather than guessing it is safe.
    """
    from fno.claims.core import list_claims
    from fno.claims.lanes import LANE_SLOT_PREFIX

    domains: set[str] = set()
    for claim in list_claims(prefix=LANE_SLOT_PREFIX, root=claims_root):
        meta = claim.get("metadata") or {}
        domains.add(meta.get("domain") or _DOMAIN_UNSET)
    return domains


def _ready_nodes(project: Optional[str], mission: Optional[str] = None) -> list[dict]:
    """Ordered ready-node summaries via ``fno backlog ready`` (JSON list).

    Reuses the SAME selection surface as ``fno backlog next``: claim-filtered,
    open-PR-filtered, container-filtered, and rank-sorted. Lane-fill therefore
    never diverges from the single-node dispatch path. Raises on a garbled
    response so the caller skips rather than guessing (Failure Modes: Errors).
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
        return []
    nodes = json.loads(out)
    if not isinstance(nodes, list):
        raise RuntimeError(f"fno backlog ready returned an unexpected shape: {out[:200]}")
    return [n for n in nodes if isinstance(n, dict) and n.get("id")]


def select_lane_fill(
    max_lanes: int,
    project: Optional[str] = None,
    *,
    mission: Optional[str] = None,
    claim: bool = True,
    claims_root: Optional[Path] = None,
) -> list[dict]:
    """Select up to ``max_lanes`` ready nodes from DISTINCT domains, one lane each.

    The parallel-mode (epic x-42d5, group 2) lane-fill selector. With
    ``claim=True`` each pick atomically acquires a dispatch-time lane slot (the
    group-1 primitive ``acquire_lane_slot``), so the concurrency cap is enforced
    by claim atomicity, never a counted snapshot (Locked Decision #7). Each
    returned node already holds a slot keyed ``parallel-lane:<id>``; the caller
    spawns one worker per node and the worker's ``target init`` reconciles that
    same slot (Locked Decision #8) rather than acquiring a fresh one.

    Distinctness is recomputed AFTER each claim from a FRESH ready-list, never a
    pre-claim snapshot: between two picks a peer may claim a node or a lane may
    finish, and re-querying reflects that. This is the x-7441 "stops at a
    claimed head" hazard - selection must skip claimed heads across every domain.
    A node a live peer lane already holds is skipped so a not-yet-node-claimed
    lane is never double-dispatched. (Two dispatchers racing the SAME node are
    prevented upstream by the singleton ``walker:<root>`` claim, so this stays a
    single-dispatcher selector, not a distributed lock.)

    ``max_lanes == 1`` selects a single ready node (distinct-domain is a no-op
    for one pick): this is the retargeted active_backlog daemon's sequential
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
    from fno.claims.lanes import acquire_lane_slot, find_lane_slot, release_lane_slot

    if max_lanes < 1:
        return []

    selected: list[dict] = []
    # Seed from domains already held by live lanes (peer lanes from prior ticks):
    # LD#8 recomputes distinctness against the live-claim world, so a live `code`
    # lane blocks this fill from selecting another `code` node (codex P2 on #130).
    # The peer-lane set is stable within a single-dispatcher call (the singleton
    # walker:<root> claim serializes dispatchers), so it is seeded once here; this
    # call's own picks are added below as they are acquired.
    used_domains: set[str] = _live_lane_domains(claims_root=claims_root)
    picked_ids: set[str] = set()

    try:
        while len(selected) < max_lanes:
            # ponytail: fresh ready-list per pick is O(max_lanes * ready_count).
            # max_lanes is small (2-3) and the ready-list is short, so this is
            # cheap; if a huge backlog makes the re-query hurt, cache the list
            # and refresh only the claim-state. The fresh query is what makes
            # distinctness "recomputed after each claim" not snapshot-stale.
            candidate = None
            for node in _ready_nodes(project, mission):
                nid = node["id"]
                if nid in picked_ids:
                    continue
                domain = node.get("domain") or _DOMAIN_UNSET
                if domain in used_domains:
                    continue
                if find_lane_slot(nid, root=claims_root) is not None:
                    continue  # a live peer lane already owns this node
                candidate = (node, domain)
                break
            if candidate is None:
                break  # no distinct-domain, unclaimed node left

            node, domain = candidate
            if claim:
                slot = acquire_lane_slot(
                    max_lanes,
                    node["id"],
                    extra_metadata={"domain": domain},
                    root=claims_root,
                )
                if slot is None:
                    break  # cap full: every slot held by a live peer lane
            selected.append(node)
            used_domains.add(domain)
            picked_ids.add(node["id"])
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

    return selected


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


def _worker_agent_name(
    node_id: str, node_slug: Optional[str], prefix: str = "target"
) -> str:
    """Provenance-carrying bg worker name: ``<prefix>-<full-node-id>-<slug>``.

    Mirrors skills/target/scripts/dispatch-node.sh: the verb prefix plus the
    full node id and the node's title-derived slug (sanitized via
    ``_name_slug``), so the thread title reads at a glance (e.g.
    ``target-ab-4040eee8-cargo-bootstrapper``). A node with no usable slug
    degrades to ``<prefix>-<full-node-id>``. ``prefix`` is ``reconcile`` for the
    G4 de-stub pass so its worker name never collides with the (ended) first
    pass's ``target-<id>-<slug>``.
    """
    base = f"{prefix}-{node_id}"
    slug = _name_slug(node_slug)
    return f"{base}-{slug}" if slug else base


def _spawn_worker(
    node_id: str,
    node_cwd: Optional[str],
    node_slug: Optional[str] = None,
    *,
    reconcile_manifest: Optional[str] = None,
    model: Optional[str] = None,
    provider: Optional[str] = None,
    harness: Optional[str] = None,
    verb: Optional[str] = None,
    brief: Optional[str] = None,
    extra_env: Optional[dict] = None,
    permission_mode: Optional[str] = None,
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
    the default builtin bakes ``no-merge``; ``config.dispatch.auto_merge`` routes the
    ``/target`` verb path (which omits ``no-merge``); reconcile stays an explicit
    ``/target [no-merge] --reconcile <manifest> {id}`` template. The agent is named
    ``target-<full-node-id>-<slug>`` (``reconcile`` prefix when G4), and the cwd
    resolves to the node's recorded root (``--cwd``) or canonical main (``--fresh``).

    Returns the spawn receipt's short_id. Raises SpawnAlreadyRunning on a
    name-collision (a peer beat us in the boot window), DispatchResolveError on an
    unresolvable harness/substrate/verb (caught non-fatally by the caller), and
    SpawnError otherwise.
    """
    is_reconcile = bool(reconcile_manifest)
    agent_name = _worker_agent_name(
        node_id, node_slug, prefix="reconcile" if is_reconcile else "target"
    )
    # --provider selects the account/record (or a bare kind like "claude"); a
    # per-node or dispatch-time pin overrides the claude default. Layer-separate
    # from `harness` (the record's cli, which drives the resolver's substrate).
    prov = (provider or "").strip() or "claude"

    # x-4391: merge posture from config.dispatch.auto_merge, read with the node_cwd
    # precedence so a cross-project dispatch reads the DEPENDENT node's config
    # (AC2-EDGE), never the merged repo's. advance takes no per-run flag, so config
    # is the sole non-builtin rung; any read failure -> no-merge (Locked Decision
    # 6). The same settings object feeds the resolver (config.dispatch.*) and the
    # permission-mode read below, so all three config reads are node-consistent.
    allow_merge = False
    settings_obj = None
    try:
        from fno.config import load_settings, load_settings_for_repo

        settings_obj = (
            load_settings_for_repo(Path(node_cwd)) if node_cwd else load_settings()
        )
    except Exception:  # noqa: BLE001 - unreadable config -> defaults below
        settings_obj = None
    # Read auto_merge in its OWN guard so a missing/odd .dispatch never disables the
    # independent permission-mode read that also consumes settings_obj.
    if settings_obj is not None:
        try:
            allow_merge = bool(settings_obj.dispatch.auto_merge)
        except Exception:  # noqa: BLE001 - fail-safe to no-merge (never grant on error)
            allow_merge = False

    # x-0676: resolve substrate + normalized command. A node dispatch_verb takes the
    # verb path (never a merge); with no verb, auto_merge routes the /target verb
    # path (omits no-merge, byte-identical to x-4391's token drop on claude) while
    # the default bakes no-merge via the builtin; reconcile stays explicit. A
    # DispatchResolveError propagates to the caller's non-fatal spawn-failure path.
    from fno.agents import harness_map

    node_verb = (verb or "").strip() or None
    resolve_kwargs: dict = {
        "harness": (harness or None),
        "node_id": node_id,
        "brief": (brief or None),
        "trigger": "autonomous",
        "settings": settings_obj,
    }
    if is_reconcile:
        _nm = "" if allow_merge else "no-merge "
        resolve_kwargs["command"] = (
            f"/target {_nm}--reconcile {reconcile_manifest} {{id}}"
        )
    elif node_verb:
        resolve_kwargs["verb"] = node_verb
    elif allow_merge:
        resolve_kwargs["verb"] = "/target"
    resolved = harness_map.resolve_dispatch(**resolve_kwargs)
    substrate = resolved["substrate"]
    target_cmd = resolved["command"]
    spawn_env = resolved.get("env") or {}

    cmd = [
        *_subprocess_util.fno_py_cmd(),
        "agents", "spawn", "--provider", prov, "--substrate", substrate,
    ]
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
    cmd += [agent_name, target_cmd]

    # The brief (US3) rides the spawn subprocess env as TARGET_BRIEF (never the
    # command line), mirroring dispatch-node.sh's `export TARGET_BRIEF`. A failover
    # account's env (dispatch_env: CLAUDE_CONFIG_DIR / base_url / api key) rides
    # here too, so `--provider <harness>` selects the CLI while extra_env selects
    # the account (x-0676; --provider never carries a record id).
    merged_env = {**spawn_env, **(extra_env or {})}
    run_env = {**os.environ, **merged_env} if merged_env else None
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
    # Receipt shape is substrate-dependent (mirrors dispatch-node.sh). A `bg`
    # spawn lands a DETACHED thread and returns a compact JSON receipt with a
    # short_id we require as launch proof: {"name", "short_id", ...}. A `headless`
    # one-shot (a codex/others failover) already ran to completion on exit 0 - no
    # detached thread, no short_id - so the clean exit IS the proof and we skip
    # the requirement (else the parse below would raise SpawnError, release the
    # reservation, and redispatch a node whose headless worker already ran).
    if substrate != "bg":
        return "headless"
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
# Lane dispatch (parallel mode, epic x-42d5 group 3): spawn + per-lane isolation
# ---------------------------------------------------------------------------
#
# G1 shipped the atomic lane-slot cap (claims/lanes.py); G2 the distinct-domain
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
    """`fno worktree ensure` failed; the lane cannot be isolated, so it is skipped."""


def _canonical_root() -> Path:
    """The canonical (main-checkout) repo root a lane worktree spawns from."""
    from fno.paths import resolve_canonical_repo_root

    return resolve_canonical_repo_root()


def _base_project_id(canonical_root: Path) -> str:
    """The shared project.id lane ids are derived from (fallback: repo basename)."""
    try:
        from fno.config import load_settings

        pid = load_settings().project.id
        if pid:
            return pid
    except Exception:  # noqa: BLE001 - a settings read error must not crash dispatch
        pass
    return canonical_root.name


def _run_setup_worktree(worktree: Path, canonical_root: Path) -> None:
    """Link shared `.fno`/`internal`/`.claude` state into a fresh lane worktree.

    `fno worktree ensure` is git-mechanism-only (x-73ca) and deliberately leaves
    this to the caller; without it the lane has no symlinked settings.yaml and
    falls through to global config. Best-effort: a bare `pip install fno` ships
    no repo scripts, and a link failure must not abort an otherwise-launchable
    lane, so any non-zero / missing-script outcome is swallowed (the worker's
    own `fno target start` re-heals what it can).
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


def _ensure_lane_worktree(node_id: str, *, canonical_root: Path) -> Path:
    """Idempotently isolate a lane worktree off origin/main; return its path.

    Delegates to `fno worktree ensure` (x-73ca): a git-only, idempotent verb
    that creates `<worktrees_base>/<repo>/<node_id>` on branch `feature/<node_id>`
    (base origin/main), or reuses it. Raises WorktreeEnsureError on failure (empty
    stdout / non-zero) so the caller releases the lane slot and skips this lane
    without touching the others (Failure Modes: Errors).
    """
    proc = subprocess.run(
        [*_subprocess_util.fno_py_cmd(), "worktree", "ensure", "--repo", str(canonical_root), "--name", node_id],
        capture_output=True,
        text=True,
        timeout=300,
    )
    path = (proc.stdout or "").strip()
    if proc.returncode != 0 or not path:
        raise WorktreeEnsureError(
            f"fno worktree ensure failed for {node_id}: "
            f"{(proc.stderr or proc.stdout or '').strip()[:200]}"
        )
    worktree = Path(path)
    # Heal a whole-dir `.fno` symlink (a REUSED worktree can carry one) BEFORE
    # setup-worktree.sh runs: setup links shared state into `.fno/*`, and through
    # the symlink those links would land in the CANONICAL checkout; the later
    # seed would then replace `.fno` with a bare real dir, stranding the lane
    # without its settings.yaml/state links. Heal first so setup populates the
    # REAL per-worktree dir (mirrors the heal `fno target start` does before its
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
    # bg-worktree footgun `fno target start` already heals). Writing through it
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
    provider: Optional[str] = None,
) -> list[dict]:
    """Select and spawn up to ``max_lanes`` isolated background lanes.

    A dispatch-time ``model``/``provider`` applies to every lane spawned this run
    and outranks each node's own annotation (Locked Decision 1).

    The parallel-mode dispatcher (epic x-42d5, group 3). Selects distinct-domain
    ready nodes via :func:`select_lane_fill` (which atomically holds a lane slot
    per pick, LD#8), then for each pick: isolates a worktree off origin/main,
    seeds its per-lane `.fno/settings.local.yaml` (x-cbce), and spawns a detached
    `claude --bg` `/target no-merge` worker rooted in that worktree. The worker's
    `fno target init` reconciles the already-held slot rather than acquiring a
    fresh one.

    ``max_lanes == 1`` dispatches a single node (the retargeted active_backlog
    daemon's sequential fire-and-forget path, x-0ad6); ``max_lanes < 1`` selects
    nothing and returns ``[]``.

    Per-lane spawn/isolation failure is contained: the lane's slot is released so
    the node stays re-dispatchable and its receipt records ``skipped``; peer
    lanes are unaffected (Failure Modes: Errors). Returns one receipt dict per
    selected lane (``status`` ``dispatched`` | ``skipped``).
    """
    from fno.claims.core import ClaimHeldByOther, acquire_claim
    from fno.claims.lanes import release_lane_slot

    selected = select_lane_fill(
        max_lanes, project, mission=mission, claim=True, claims_root=claims_root
    )
    if not selected:
        return []

    canonical = _canonical_root()
    base_pid = _base_project_id(canonical)
    ev_path = events_path or _events_path(project_root)

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
                    "dispatch_lanes: slot release failed for %s (%s); "
                    "slot lingers to TTL", _nid, exc,
                )
            receipts.append({"node_id": _nid, "status": "skipped", "error": reason})

        # The lane slot (parallel-lane:<id>) is invisible to the sequential
        # advance()/dispatch-node.sh path, which dedups on node:<id> + dispatch:<id>.
        # During the boot window before this lane's worker owns node:<id>, that
        # path would see the node as ready+unclaimed and double-launch it. Guard
        # with the SAME dispatch:<id> reservation advance() uses (global-rooted,
        # TTL bridge) so the two dispatchers dedup against each other.
        if _claim_is_live(f"node:{node_id}") or _claim_is_live(f"dispatch:{node_id}"):
            _skip("already-claimed")
            continue
        dispatch_key = f"dispatch:{node_id}"
        dispatch_holder = f"advance:{os.getpid()}"
        dispatch_root = _claims_root_for(dispatch_key)
        try:
            acquire_claim(
                dispatch_key,
                dispatch_holder,
                ttl_ms=_DISPATCH_TTL_MS,
                reason=f"parallel lane dispatch for {node_id}",
                root=dispatch_root,
            )
        except ClaimHeldByOther:
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
            worktree = _ensure_lane_worktree(node_id, canonical_root=canonical)
            _seed_lane_local_settings(worktree, node_id, base_pid)
            eff_provider = provider if provider is not None else node.get("provider")
            short_id = _spawn_worker(
                node_id, str(worktree), slug,
                model=_route_resolve.node_model(node, explicit=model, provider=eff_provider),
                provider=eff_provider,
                verb=node.get("dispatch_verb"),
                brief=node.get("dispatch_brief"),
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
    return receipts


# ---------------------------------------------------------------------------
# Claim helpers (route each key like the `fno claim` CLI's _node_aware_root)
# ---------------------------------------------------------------------------


def _claims_root_for(key: str):
    """Resolve the claims root for a key (delegates to the shared helper).

    Global-id kinds (``node:``/``dispatch:``/``reconcile:``) live in the global
    ($HOME) root; repo-local keys use the cwd/env default (canonical repo root,
    honoring FNO_CLAIMS_ROOT). Delegating to fno.claims.io.claims_root_for keeps
    advance, reconcile_dispatch, spawn-guard, and the `fno claim` CLI on ONE
    routing rule so they cannot drift -- and roots the boot-window dispatch:<id>
    token globally so cross-repo dispatchers dedup against each other."""
    from fno.claims.io import claims_root_for

    return claims_root_for(key)


def _walker_key() -> str:
    """``walker:<canonical_repo_root>`` - byte-identical to the key the Rust
    megawalk loop writes (loop_megawalk.rs)."""
    from fno.paths import resolve_canonical_repo_root

    return f"walker:{resolve_canonical_repo_root()}"


def _claim_is_live(key: str) -> bool:
    # "occupied" for dispatch: a live OR a suspect claim (x-ba4b) blocks
    # selection. suspect = TTL-unexpired, dead pid (respawned worker); the TTL
    # still protects the slot, so selection must skip it, never steal.
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
    root = Path(project_root) if project_root is not None else Path.cwd()
    return root / ".fno" / "events.jsonl"


def _emit(kind: str, data: dict, events_path: Path) -> None:
    """Best-effort event emit. Never raises (LD#7: never wedge the host op)."""
    try:
        from fno.events import _build, append_event

        append_event(_build(kind, _EVENT_SOURCE, data), events_path)
    except Exception as exc:  # noqa: BLE001
        print(f"advance: WARNING: event emit failed ({kind}): {exc}", file=sys.stderr)


def _select_exhaustion_failover(
    node_cwd: Optional[str], exhausted_provider: str
) -> Optional[tuple[str, str, dict]]:
    """x-0676: pick the next non-exhausted provider from the active combo when
    ``config.dispatch.on_exhaustion == "failover"``.

    Returns ``(record_id, harness, account_env)`` for the selected provider, or
    ``None`` for the defer floor: not configured for failover, no active COMBO to
    walk (a bare active-provider is not a combo), the whole combo exhausted, an
    unresolvable harness, an unstageable account, or ANY read error. Failover never
    guesses - every non-clean path degrades to defer.

    The three parts stage a spawn correctly (a combo member is a provider RECORD
    id, NOT a harness): ``harness`` (the record's ``cli``) becomes ``--provider``
    (validated against the harness set, so a record id must never go there) and
    drives the resolver's substrate; ``account_env`` (``dispatch_env`` of the
    record: ``CLAUDE_CONFIG_DIR`` / ``base_url`` / api key) selects the ACCOUNT via
    the spawn subprocess env. A cross-harness failover (claude->codex) rides the
    ``harness`` change.
    """
    try:
        from fno.config import load_settings, load_settings_for_repo

        s = load_settings_for_repo(Path(node_cwd)) if node_cwd else load_settings()
        if (s.dispatch.on_exhaustion or "defer") != "failover":
            return None
    except Exception:  # noqa: BLE001 - unreadable config -> defer floor
        return None
    try:
        from fno.adapters.providers.dispatch import dispatch_env
        from fno.adapters.providers.loader import load_combos, load_providers
        from fno.adapters.providers.rotation import next_healthy_provider
        from fno.sigma_dispatch import resolve_dispatch_target

        # The active combo via the CG8 settings_combo rung (env={} so no TARGET_COMBO
        # pin leaks in; a synthetic agent name has no per-agent pin). A bare active
        # PROVIDER (no combo) resolves to combo_name=None -> defer (nothing to walk).
        target = resolve_dispatch_target(
            "advance-failover",
            repo_root=Path(node_cwd) if node_cwd else None,
            env={},
        )
        if not target.combo_name:
            return None
        combo = load_combos().get(target.combo_name)
        if combo is None:
            return None
        pid = next_healthy_provider(combo, exclude={exhausted_provider})
        if pid is None:
            return None
        rec = load_providers().by_id.get(pid)
        harness = (getattr(rec, "cli", "") or "").strip()
        if not harness:
            return None  # a record with no cli cannot pick a --provider
        # Stage the account env; an unstaged/unresolvable account -> defer (never
        # spawn onto a broken account).
        account_env = dispatch_env(
            pid, repo_root=Path(node_cwd) if node_cwd else None
        )
        return (pid, harness, account_env)
    except Exception:  # noqa: BLE001 - never dispatch onto an unresolved provider
        return None


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

    def skip(
        reason: str,
        *,
        node_id: Optional[str] = None,
        detail: Optional[str] = None,
        provider: Optional[str] = None,
        retry_at: Optional[float] = None,
    ) -> AdvanceResult:
        data: dict = {"reason": reason}
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

    # 4b. Quota-aware defer (x-5d3e). advance IS an autonomous path, so it may
    #     defer when the resolved provider has no headroom and defer_dispatch is
    #     on. Fail-open + opt-in: off by default, p0 never defers, UNKNOWN never
    #     defers. The node stays in ready (skip mutates nothing); the next tick
    #     after the reset dispatches it. Never fatal - a defer read failure just
    #     proceeds to dispatch.
    failover_record: Optional[str] = None
    failover_harness: Optional[str] = None
    failover_env: Optional[dict] = None
    failover_from: Optional[str] = None
    try:
        from fno.adapters.providers.loader import load_providers
        from fno.adapters.providers.runtime_state import evaluate_quota_defer

        # Match the SAME provider precedence the spawn below uses
        # (eff_provider = provider arg -> node pin -> active default), so the
        # quota decision evaluates the provider the worker will actually run on,
        # not a mismatched active record (x-5d3e review).
        provider_id = (
            provider or node.get("provider") or load_providers().active or ""
        )
        decision = evaluate_quota_defer(provider_id, priority=node.get("priority"))
    except Exception:  # noqa: BLE001 - a quota read must never wedge advance
        decision = None
    if decision is not None:
        # x-0676: the resolved provider is exhausted. Default = defer (the floor).
        # config.dispatch.on_exhaustion == "failover" + NO explicit/node provider
        # pin -> rotate to the next healthy provider in the active combo (precedence
        # explicit > node pin > combo, so a pin never fails over). Any miss (not
        # configured, no active combo, whole-combo exhausted, read error) falls back
        # to today's quota-deferred skip. Selection is pinned here; the worker never
        # re-switches. The dispatch_failover receipt is emitted at the spawn below so
        # it only lands when a worker is actually launched.
        has_pin = bool(
            (provider or "").strip() or str(node.get("provider") or "").strip()
        )
        selected = (
            None if has_pin
            else _select_exhaustion_failover(node_cwd, decision.provider_id)
        )
        if selected is None:
            return skip(
                "quota-deferred",
                node_id=node_id,
                provider=decision.provider_id,
                retry_at=decision.retry_at,
            )
        failover_record, failover_harness, failover_env = selected
        failover_from = decision.provider_id

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
        if failover_record is not None:
            # x-0676: the pre-spawn failover receipt (from -> to), paired with the
            # advance_dispatched below - not a competing decision. `to` is the
            # record id; --provider gets the harness, the account env selects it.
            _emit(
                EVENT_FAILOVER,
                {
                    "node_id": node_id,
                    "from": failover_from or "",
                    "to": failover_record,
                    "harness_to": failover_harness or "",
                },
                ev_path,
            )
            # --provider is the HARNESS (a record id would be rejected by the spawn
            # front door's known-provider gate); the account rides extra_env.
            eff_provider = failover_harness
        else:
            eff_provider = provider if provider is not None else node.get("provider")
        short_id = _spawn_worker(
            node_id, node_cwd, node.get("slug") or node.get("title"),
            model=_route_resolve.node_model(node, explicit=model, provider=eff_provider),
            provider=eff_provider,
            harness=failover_harness,
            extra_env=failover_env,
            verb=node.get("dispatch_verb"),
            brief=node.get("dispatch_brief"),
        )
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
# DIFFERENT project, it spawns `/target no-merge <dep> --cwd <dep project root>`.
# The two paths are intentionally distinct (Domain Pitfall: dispatch-by-edge vs
# select-next must not be conflated) and share the same dispatch:<id> dedup +
# node:<id> liveness + spawn machinery, so the same successor observed by both
# advance() and advance_dependents() (or by two triggers) dispatches at most once.


def _direct_dependents(closed_node_id: str, closed_project: Optional[str]) -> list[dict]:
    """Ready, direct ``blocked_by`` dependents of the closed node.

    Reads the graph (``read_graph`` recomputes ``_status`` at read), so a
    dependent whose only open blocker was the just-closed node already reads
    ``ready`` here. Returns minimal dicts
    ``{id, project, slug, cwd, model, model_tier, cross_project}``.

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

    entries = read_graph(graph_json())
    # Containers are never dispatched as workers (x-33b2): a dependent that is
    # itself some other node's `parent` is an epic, and `/target` builds its
    # leaves, not the box. Mirror cmd_next's `_pick_ready` exclusion on this
    # edge-following path so a now-unblocked epic dependent is skipped here too.
    parent_ids = {
        e.get("parent") for e in entries
        if isinstance(e, dict) and isinstance(e.get("parent"), str)
    }
    out: list[dict] = []
    for e in entries:
        if not isinstance(e, dict):
            continue
        if closed_node_id not in (e.get("blocked_by") or []):
            continue
        # "now-unblocked" == ready: blocker done + no other open blocker + has a
        # plan. A still-blocked dependent reads `blocked`; a plan-less one reads
        # `idea`; a claimed/done/deferred one reads its own bucket - all excluded.
        if e.get("_status") != "ready":
            continue
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
            # model_tier rides alongside so the tier resolver sees the annotation.
            "model": e.get("model"),
            "model_tier": e.get("model_tier"),
            "cross_project": (e.get("project") or None) != (closed_project or None),
        })
    return out


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


def _dispatch_one_dependent(
    dep: dict, closed_node_id: str, ev_path: Path, verbose: bool,
    *, model: Optional[str] = None, provider: Optional[str] = None,
) -> AdvanceResult:
    """Resolve one dependent's own project root, dedup, and spawn its worker.

    Reuses advance()'s claim + spawn + event machinery. The ``--cwd`` root
    differs by route (RC1 / LD#2): a CROSS-project dependent launches in its
    work-map root; a SAME-project dependent launches in the node's OWN recorded
    ``cwd`` (NEVER the work-map root, which for a foreign-shaped record could land
    it on a protected branch where the bg worker dies). Everything downstream of
    root resolution - dedup, spawn, single decision event - is identical.
    """
    node_id = dep["id"]
    cross_project = bool(dep.get("cross_project"))

    def skip(reason: str, detail: Optional[str] = None) -> AdvanceResult:
        data: dict = {"reason": reason, "node_id": node_id, "closed_node_id": closed_node_id}
        if detail:
            data["detail"] = detail[:200]
        _emit(EVENT_SKIPPED, data, ev_path)
        return AdvanceResult("skipped", EVENT_SKIPPED, reason=reason, node_id=node_id, detail=detail)

    def failed(error: str) -> AdvanceResult:
        _emit(
            EVENT_FAILED,
            {"node_id": node_id, "closed_node_id": closed_node_id, "error": error[:200]},
            ev_path,
        )
        return AdvanceResult("failed", EVENT_FAILED, reason="spawn-failed", node_id=node_id, detail=error)

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

    # The spawned worker runs in the DEPENDENT's repo, not this one. If that
    # project already has a live walker, let it claim the node - spawning here
    # would launch a second target into that repo (codex P2). Checked at the
    # dependent root because its walker claim lives under that root's .fno/claims.
    if _walker_live_at(root):
        return skip("walker-live")

    # Already being worked? Same liveness gate as advance() step 4.
    if _claim_is_live(f"node:{node_id}") or _claim_is_live(f"dispatch:{node_id}"):
        return skip("already-claimed")

    from fno.claims.core import ClaimHeldByOther, acquire_claim

    dispatch_key = f"dispatch:{node_id}"
    holder = f"advance:{os.getpid()}"
    dispatch_root = _claims_root_for(dispatch_key)
    try:
        acquire_claim(
            dispatch_key,
            holder,
            ttl_ms=_DISPATCH_TTL_MS,
            reason=f"dependent dispatch for {node_id} (dep of {closed_node_id})",
            root=dispatch_root,
        )
    except ClaimHeldByOther:
        return skip("already-claimed")
    except Exception as exc:  # noqa: BLE001
        return skip("claim-error", detail=str(exc))

    try:
        eff_provider = provider if provider is not None else dep.get("provider")
        short_id = _spawn_worker(
            node_id, root, dep.get("slug"),
            model=_route_resolve.node_model(dep, explicit=model, provider=eff_provider),
            provider=eff_provider,
            verb=dep.get("dispatch_verb"),
            brief=dep.get("dispatch_brief"),
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
            "closed_node_id": closed_node_id,
            "agent_name": _worker_agent_name(node_id, dep.get("slug")),
            "cross_project": cross_project,
        },
        ev_path,
    )
    if verbose:
        _kind = "cross-project" if cross_project else "same-project"
        print(
            f"advance: dispatched {_kind} dependent {node_id} -> "
            f"target worker {short_id} (--cwd {root})",
            file=sys.stderr,
        )
    return AdvanceResult("dispatched", EVENT_DISPATCHED, node_id=node_id, short_id=short_id)


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
    if not auto_continue_enabled(project_root=project_root):
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
            {"reason": "closed-project-unknown", "closed_node_id": closed_node_id},
            ev_path,
        )
        return [AdvanceResult("skipped", EVENT_SKIPPED, reason="closed-project-unknown")]

    try:
        deps = _direct_dependents(closed_node_id, closed_project)
    except Exception as exc:  # noqa: BLE001 - never guess on a read error
        _emit(
            EVENT_SKIPPED,
            {"reason": "dependents-error", "closed_node_id": closed_node_id, "detail": str(exc)[:200]},
            ev_path,
        )
        return [AdvanceResult("skipped", EVENT_SKIPPED, reason="dependents-error", detail=str(exc))]

    return [
        _dispatch_one_dependent(
            dep, closed_node_id, ev_path, verbose, model=model, provider=provider
        )
        for dep in deps
    ]
