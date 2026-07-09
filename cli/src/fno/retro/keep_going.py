"""Autonomous keep-going engine (x-3360): classify surviving carve-out
follow-ups into think/build/file arms and dispatch under a shared per-day
firehose ceiling.

The load-bearing safety property is the ceiling. An autonomous loop that spawns
its own follow-up work is unbounded without a hard daily cap - each merge
harvesting N carve-outs, each spawning a think/target, each of those merging and
harvesting again. So the ceiling reuses the SAME per-install per-day counter
``fno think dispatch`` bumps (:mod:`fno.provenance.spawn_think`): think + target
dispatches share ONE budget - a single ceiling on total autonomous fan-out per
day, governed by ``config.think_spawn.daily_cap`` (0 = off).

Every follow-up is ALREADY a filed backlog node (``land.py`` filed it before this
runs), so this layer never files or drops anything: a capped or failed dispatch
simply leaves the node in place (the safety net). It only decides whether to
dispatch work ON TOP:

  - deferred carve-out (a deferred DECISION -> design unclear) -> ``/think``
  - oos-bug carve-out (a concrete, scoped fix -> design clear) -> ``/target``
  - anything else                                              -> file-only

Runs in AUTONOMOUS mode only (interactive harvests queue nodes for a human ack;
auto-dispatching would bypass it) and only when ``config.keep_going.enabled``.
Strictly non-fatal: any arm failure leaves the node filed and the ritual
continues, then self-ends.

Interaction with born-with-why: both default OFF. With BOTH armed, a deferred
carve-out node can get a born-with-why ``/think`` (reason=birth) AND this engine's
``/think`` (reason=conversational) - distinct dedup tokens, so worst case is one
extra bounded /think, never a storm. No suppression machinery for an opt-in-both
combo (YAGNI).
"""
from __future__ import annotations

import json
import logging
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional

from fno import _subprocess_util
from fno.provenance.spawn_think import (
    _bump_daily_count,
    _daily_cap,
    _daily_count,
    _name_slug,
    _parse_short_id,
)
from fno.retro.land import LandResult
from fno.retro.types import KIND_CARVEOUT, Candidate

_LOG = logging.getLogger(__name__)

ARM_THINK = "think"
ARM_BUILD = "build"
ARM_FILE = "file"

# Highest-precedence explicit override (tests + force on/off); mirrors
# spawn_think._ENV_OVERRIDE. Otherwise the gate reads config.keep_going.enabled.
_ENV_OVERRIDE = "FNO_KEEP_GOING"
_TRUTHY = frozenset({"1", "true", "yes", "on"})


def keep_going_enabled(
    *, project_root: Optional[Path] = None, env: Optional[dict] = None
) -> bool:
    """Resolve whether the autonomous keep-going engine is armed.

    Precedence (mirrors ``spawn_think.think_spawn_enabled``):
      1. ``FNO_KEEP_GOING`` env override (explicit force on/off).
      2. ``config.keep_going.enabled`` from the repo settings (local > global).
      3. default False.

    Fail-safe: ANY settings-read error degrades to False rather than raising into
    the harvest pipeline that calls it.
    """
    environ = os.environ if env is None else env
    override = environ.get(_ENV_OVERRIDE)
    if override is not None:
        return override.strip().lower() in _TRUTHY
    try:
        from fno.config import load_settings, load_settings_for_repo

        settings = (
            load_settings_for_repo(Path(project_root))
            if project_root is not None
            else load_settings()
        )
        return bool(settings.keep_going.enabled)
    except Exception as exc:  # noqa: BLE001 - fail-safe to disabled
        _LOG.debug("keep_going_enabled: settings read failed, off: %s", exc)
        return False

# dispatch outcomes recorded per follow-up.
OUTCOME_DISPATCHED = "dispatched"
OUTCOME_FILED = "filed"      # file-only arm: node already exists, nothing added
OUTCOME_CAPPED = "capped"    # ceiling reached: node stays filed, no dispatch
OUTCOME_FAILED = "failed"    # dispatch attempted but failed: node stays filed


@dataclass
class FollowupResult:
    node_id: str
    arm: str
    outcome: str
    detail: str = ""


def classify_followup(candidate: Candidate) -> str:
    """Rule-first arm for one carve-out candidate; safe default is file-only.

    Only carve-outs are dispatch-eligible (reviews/deferred-findings/postmortems
    have their own flows). The carve-out ``subkind`` drives the arm:
      - ``oos-bug``  -> build  (a concrete out-of-scope bug: design is clear)
      - ``deferred`` -> think  (a deferred decision: design is unclear)
      - otherwise    -> file   (unknown/backfill: never dispatch on a guess)
    """
    extra = candidate.extra  # dataclass field (default_factory=dict), never None
    if extra.get("kind") != KIND_CARVEOUT:
        return ARM_FILE
    subkind = (extra.get("subkind") or "").strip().lower()
    if subkind == "oos-bug":
        return ARM_BUILD
    if subkind == "deferred":
        return ARM_THINK
    return ARM_FILE


# --- dispatch seams (real defaults; injected in tests) ----------------------


def _dispatch_think(node_id: str, cwd: Optional[str]) -> bool:
    """``fno think dispatch <node> --json`` - a bg /think carrying the ritual's
    live context. Self-bumps the shared daily counter ONLY on a real spawn, so
    this arm must NOT bump again.

    Return True only when the decision is ``spawned``. Exit 0 alone is NOT proof:
    the CLI exits 0 for ``spawned`` AND ``offered``/``noop`` too, so a bumped
    counter and a "dispatched" outcome could diverge (a false dispatch that never
    incremented the ceiling). We assert ``decision == "spawned"`` from the JSON so
    the outcome matches the bump exactly. (In practice dispatch_conversational
    forces the gate on + attended=spawn, so offered/noop shouldn't arise via this
    path - but keying off the decision is contract-correct, not exit-code-lucky.)
    """
    cmd = [*_subprocess_util.fno_py_cmd(), "think", "dispatch", node_id, "--json"]
    try:
        proc = subprocess.run(
            cmd, capture_output=True, text=True, timeout=600,
            cwd=cwd or None,
        )
    except Exception as exc:  # noqa: BLE001 - dispatch failure is never fatal
        _LOG.debug("keep_going: think dispatch failed for %s: %s", node_id, exc)
        return False
    if proc.returncode != 0:
        # exit 1 is an expected skip (dedup/cap/no-origin); exit 2 is bad input.
        # Log the reason (else the stderr is discarded) - the node stays filed.
        _LOG.debug("keep_going: think dispatch for %s exited %d: %s",
                   node_id, proc.returncode, (proc.stderr or "").strip()[:200])
        return False
    try:
        decision = (json.loads(proc.stdout or "{}") or {}).get("decision")
    except (ValueError, TypeError):
        decision = None
    if decision != "spawned":
        # exit 0 but not a spawn (offered/noop/malformed) => no counter bump
        # happened, so do NOT report it as a dispatch. Node stays filed.
        _LOG.debug("keep_going: think dispatch for %s decision=%r (not spawned)",
                   node_id, decision)
        return False
    return True


def _spawn_target_worker(node_id: str, cwd: Optional[str]) -> bool:
    """Fire-and-forget bg ``/target <node> no-merge`` worker (the next loop
    iteration). ``no-merge`` because an autonomous worker lands a PR for review,
    never an auto-merge. Mirrors ``spawn_think._spawn_think_worker``: the slash
    command rides as the prompt, ``--substrate bg`` is the detached claude thread
    (never ``-p``). Returns True on a spawn receipt (a short_id), False otherwise.
    """
    name = f"keepgo-{_name_slug(node_id) or node_id}"[:64].rstrip("-")
    cmd = [*_subprocess_util.fno_py_cmd(), "agents", "spawn",
           "--provider", "claude", "--substrate", "bg"]
    if cwd:
        cmd += ["--cwd", cwd]
    else:
        cmd += ["--fresh"]
    cmd += [name, f"/target {node_id} no-merge"]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    except Exception as exc:  # noqa: BLE001 - spawn failure is never fatal
        _LOG.debug("keep_going: target spawn failed for %s: %s", node_id, exc)
        return False
    if proc.returncode != 0:
        _LOG.debug("keep_going: target spawn for %s exited %d: %s",
                   node_id, proc.returncode, (proc.stderr or "").strip()[:200])
        return False
    return bool(_parse_short_id(proc.stdout or ""))


# --- the pass ---------------------------------------------------------------


def dispatch_followups(
    landed: list[LandResult],
    *,
    project_root: Optional[Path] = None,
    cwd: Optional[str] = None,
    echo: Callable[[str], None] = print,
    count_fn: Optional[Callable[[], int]] = None,
    bump_fn: Optional[Callable[[], None]] = None,
    cap_fn: Optional[Callable[[Optional[Path]], int]] = None,
    think_fn: Optional[Callable[[str, Optional[str]], bool]] = None,
    build_fn: Optional[Callable[[str, Optional[str]], bool]] = None,
) -> list[FollowupResult]:
    """Classify each landed carve-out node and dispatch its arm under the ceiling.

    ``landed`` are the LandResults from the harvest (already filed nodes). Only
    carve-out nodes with a real ``node_id`` are considered. One shared daily
    counter is read before each dispatch and bumped after: the think arm's
    ``fno think dispatch`` self-bumps, so it is NOT double-bumped here; the build
    arm bumps once itself. When the ceiling is reached, remaining think/build
    items are left filed (never dropped) and ONE cap line is printed.

    The counter/dispatch seams default to the real implementations
    (spawn_think's shared counter + the subprocess dispatchers); resolved at call
    time (``or`` fallback) so a caller/test can override any subset.
    """
    count_fn = count_fn or _daily_count
    bump_fn = bump_fn or _bump_daily_count
    cap_fn = cap_fn or _daily_cap
    think_fn = think_fn or _dispatch_think
    build_fn = build_fn or _spawn_target_worker
    cap = cap_fn(project_root)
    capped = False
    results: list[FollowupResult] = []

    for r in landed:
        node_id = r.node_id
        if not node_id:
            continue
        arm = classify_followup(r.candidate)
        if arm == ARM_FILE:
            results.append(FollowupResult(node_id, arm, OUTCOME_FILED))
            continue

        # Ceiling check (0 disables). The node is already filed, so a cap hit is
        # "parked as a backlog node", never a dropped follow-up.
        # ponytail: SOFT ceiling. This check-then-bump is not atomic, and the
        # think arm's bump lives inside spawn_think's counter (also soft, by its
        # own documented last-writer-wins design), so concurrent rituals can
        # overshoot `daily_cap` by at most the concurrency count. That bounded
        # overshoot is acceptable for a firehose guard whose job is preventing
        # RUNAWAY fan-out, not exact rationing. Exact atomic reservation would
        # mean making the SHARED spawn_think counter lock-guarded (its stated
        # upgrade path) - deferred, see cv on this PR.
        if cap > 0 and count_fn() >= cap:
            capped = True
            results.append(FollowupResult(node_id, arm, OUTCOME_CAPPED))
            continue

        if arm == ARM_THINK:
            ok = think_fn(node_id, cwd)  # self-bumps the shared counter on spawn
        else:  # ARM_BUILD
            ok = build_fn(node_id, cwd)
            if ok:
                bump_fn()
        results.append(
            FollowupResult(node_id, arm, OUTCOME_DISPATCHED if ok else OUTCOME_FAILED)
        )

    if capped:
        echo(
            f"keep-going: daily dispatch cap reached ({cap}); "
            f"parking follow-ups as backlog nodes"
        )
    return results
