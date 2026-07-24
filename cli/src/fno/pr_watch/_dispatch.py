"""PR-state watcher: headless dispatch + impure tick orchestrator.

``fire_skill`` fires a headless ``claude --print`` for one PR; ``tick``
is the impure orchestrator that ties together discovery, state, decisions,
and dispatch for one poll interval.

All I/O dependencies are injectable (runner, emit, store, claim,
reviewers_for, post_merge_readiness_fn) so the entire tick is unit-testable
without a live claude / gh / launchd / filesystem.

See the task 1.2 spec for the full contract.
"""
from __future__ import annotations

import json
import logging
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Literal, Optional

log = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# DispatchResult
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class DispatchResult:
    """Outcome of a single ``fire_skill`` call.

    ``ok`` is the load-bearing field: it is False when:
    - rc != 0
    - rc == 0 but ``is_error`` is True in the JSON envelope
    - stdout is not valid JSON
    - a subprocess timeout or OSError occurred
    """

    ok: bool
    rc: int
    is_error: bool
    raw: str  # raw stdout for forensic logging


# ---------------------------------------------------------------------------
# TickResult
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class TickResult:
    """Summary of one tick() invocation."""

    open_prs: int
    acted: int
    skipped: int = 0
    # A tick that could not take its lock did no work at all. Reporting that as
    # open_prs=0 made 172 consecutive wedged ticks look like empty sweeps for
    # five days, so it gets its own state instead of a zero count.
    lock_held: bool = False
    lock_holder: str = ""


# ---------------------------------------------------------------------------
# fire_skill
# ---------------------------------------------------------------------------

_ENV_SEAM = "PR_WATCH_FIRE_CMD"
_DEFAULT_MODEL = "claude-haiku-4-5"

# Per-verb wall-clock ceiling for the headless review fire. A launchd tick never
# overlaps, so an unbounded child call wedges every future tick forever (x-97d8);
# this bound turns a hung fire into a normal failed dispatch the retry/park
# machinery already handles. The post-merge ritual no longer fires here: pr-watch
# runs ``fno pr ritual`` directly, and that verb owns its own judgment leg.
_TIMEOUT_FOR_VERB: dict[str, float] = {"check": 180.0}
_DEFAULT_FIRE_TIMEOUT = 300.0


def fire_skill(
    verb: Literal["check"],
    pr_number: int,
    repo_dir: Path,
    *,
    model: str = _DEFAULT_MODEL,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    env_seam: str = _ENV_SEAM,
    timeout_s: Optional[float] = None,
) -> DispatchResult:
    """Fire a headless ``claude --print`` for one PR's review poll and return it.

    The command runs in the repository with ``--output-format json`` so we can
    detect ``is_error:true`` even when the exit code is 0. A test seam
    (``PR_WATCH_FIRE_CMD`` env, or *env_seam*) replaces the ``claude`` binary with
    an arbitrary command string for unit tests; when set, the command is built as
    ``["<seam>"]`` and the runner receives it like any other call.

    This is the review (``check``) fire only. The post-merge ritual no longer
    fires here: pr-watch runs ``fno pr ritual <n> --autonomous`` directly, and
    that verb owns its own conditional headless judgment leg - so fire_skill
    carries no merged branch, no post-merge model role, and no whole-ritual
    prompt (AC1-HP, AC10-EDGE).

    ``timeout_s`` bounds the fire so a hung claude cannot wedge the launchd tick
    forever (x-97d8); when None it defaults via ``_TIMEOUT_FOR_VERB``. A timeout
    surfaces as a normal failed dispatch.

    SUCCESS = rc == 0 AND parsed ``is_error`` is ``False``. Every other outcome
    is a failure.
    """
    seam_cmd = os.environ.get(env_seam)

    if seam_cmd:
        cmd = [seam_cmd]
    else:
        # --dangerously-skip-permissions is required for headless unattended use.
        cmd = [
            "claude",
            "--print",
            "--output-format",
            "json",
            "--dangerously-skip-permissions",
        ]
        if model:
            cmd += ["--model", model]
        cmd.append(f"/fno:pr {verb} {pr_number}")

    fire_timeout = (
        timeout_s
        if timeout_s is not None
        else _TIMEOUT_FOR_VERB.get(verb, _DEFAULT_FIRE_TIMEOUT)
    )

    try:
        result = runner(
            cmd,
            capture_output=True,
            text=True,
            check=False,
            cwd=str(repo_dir),
            timeout=fire_timeout,
        )
    except subprocess.TimeoutExpired as exc:
        log.warning("fire_skill %s #%d timed out: %s", verb, pr_number, exc)
        return DispatchResult(ok=False, rc=-1, is_error=True, raw="")
    except OSError as exc:
        log.warning("fire_skill %s #%d OSError: %s", verb, pr_number, exc)
        return DispatchResult(ok=False, rc=-1, is_error=True, raw="")

    raw = result.stdout or ""

    if result.returncode != 0:
        log.warning(
            "fire_skill %s #%d rc=%d stderr=%s",
            verb,
            pr_number,
            result.returncode,
            (result.stderr or "").strip()[:200],
        )
        return DispatchResult(ok=False, rc=result.returncode, is_error=True, raw=raw)

    # rc == 0: parse JSON envelope and check is_error.
    try:
        envelope = json.loads(raw)
        if not isinstance(envelope, dict):
            envelope = {"is_error": True}
    except json.JSONDecodeError as exc:
        log.warning("fire_skill %s #%d stdout not JSON: %s", verb, pr_number, exc)
        return DispatchResult(ok=False, rc=result.returncode, is_error=True, raw=raw)

    is_error = bool(envelope.get("is_error", False))
    if is_error:
        log.warning(
            "fire_skill %s #%d rc=0 but is_error=true (skill-level failure)", verb, pr_number
        )
    return DispatchResult(
        ok=not is_error,
        rc=result.returncode,
        is_error=is_error,
        raw=raw,
    )


# ---------------------------------------------------------------------------
# Claim helper protocol (duck-typed; tests inject stubs)
# ---------------------------------------------------------------------------


class _NullClaim:
    """Default no-op claim helper used when no claim system is injected.

    Tests always inject their own stub; this prevents tick() from requiring
    a live claim system in environments where it is not wired yet.
    """

    def acquire_tick_lock(self, key: str, holder: str) -> None:  # pragma: no cover
        pass

    def release_tick_lock(self, key: str, holder: str) -> None:  # pragma: no cover
        pass

    def acquire_pr_lock(self, key: str, holder: str) -> None:  # pragma: no cover
        pass

    def release_pr_lock(self, key: str, holder: str) -> None:  # pragma: no cover
        pass

    def is_node_live(self, node_id: str) -> bool:  # pragma: no cover
        return False


# ---------------------------------------------------------------------------
# tick()
# ---------------------------------------------------------------------------

_MAX_RETRIES = 3
_TICK_CLAIM_KEY = "pr-watch:tick"


def tick(
    *,
    # Graph / discovery
    graph_path: Optional[Path] = None,
    discover_fn: Optional[Callable] = None,
    read_pr_state_fn: Optional[Callable] = None,
    # Watermark store
    store_path: Optional[Path] = None,
    # Dispatch
    fire_skill_fn: Optional[Callable] = None,
    dispatch_ritual_fn: Optional[Callable] = None,
    # I/O seams
    emit: Optional[Callable[[str, dict], None]] = None,
    reviewers_for: Optional[Callable[[Path], list]] = None,
    claim: Optional[Any] = None,
    notify: Optional[Callable] = None,
    post_merge_readiness_fn: Optional[Callable] = None,
    # Clock
    now_iso: Optional[str] = None,
    max_age_days: int = 14,
    # Retry cap (default matches _MAX_RETRIES; override with config.pr_watch.retries)
    max_retries: Optional[int] = None,
) -> TickResult:
    """Impure tick orchestrator: discover, decide, dispatch, persist.

    Every I/O dependency is injectable so unit tests can run without a live
    claude, gh, launchd, or ~/.fno filesystem.

    Step overview (see spec for full contract):
        1.  Acquire tick-level lock.  If held -> return immediately (no events).
        2.  Discover open PR candidates from the graph.
        3.  For each candidate: skip (no-checkout / live-claimed) OR decide /
            dispatch / persist.
        4.  Emit ``pr_watch_tick`` heartbeat with aggregate counts.
        5.  Release tick lock.
    """
    import datetime


    if now_iso is None:
        now_iso = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    _emit = emit if emit is not None else _noop_emit
    _claim = claim if claim is not None else _NullClaim()
    _notify = notify if notify is not None else (lambda *a, **kw: None)
    _fire = fire_skill_fn if fire_skill_fn is not None else fire_skill
    _dispatch_ritual = (
        dispatch_ritual_fn if dispatch_ritual_fn is not None else _default_dispatch_ritual
    )
    _reviewers_for = reviewers_for if reviewers_for is not None else (lambda _: [])
    _post_merge = post_merge_readiness_fn if post_merge_readiness_fn is not None else _noop_readiness
    # Bind the grace window into the default discover so a PR stays watchable
    # across the PR-green -> merge window (test seams inject their own
    # single-arg discover_fn and control candidates directly).
    _discover = (
        discover_fn
        if discover_fn is not None
        else (lambda entries: _default_discover(entries, now_iso=now_iso, max_age_days=max_age_days))
    )
    _read_state = read_pr_state_fn if read_pr_state_fn is not None else _default_read_pr_state
    _max_retries = max_retries if max_retries is not None else _MAX_RETRIES

    holder = f"pr-watch:{os.getpid()}"

    # Step 1: tick-level mutex
    try:
        _claim.acquire_tick_lock(_TICK_CLAIM_KEY, holder)
    except Exception as exc:  # noqa: BLE001 - any acquire failure means no work ran
        # No work ran either way, so never report counts. Still NO events (a
        # wedged predecessor must not emit one every 600s); the caller prints
        # this reason so the wedge shows up in launchd's out.log within a tick.
        held_by = getattr(exc, "holder", "")
        if held_by:
            pid = getattr(exc, "pid", None)
            reason = f"lock held by {held_by}" + (f" (pid {pid})" if pid else "")
        else:
            # Not contention: the claim subsystem itself failed. Calling that a
            # held lock would disguise a hard failure as routine skipping, which
            # is the exact confusion this result state exists to remove.
            log.warning("pr-watch: tick lock unavailable: %s", exc)
            reason = f"tick lock unavailable: {exc}"
        return TickResult(open_prs=0, acted=0, lock_held=True, lock_holder=reason)

    try:
        return _run_tick(
            graph_path=graph_path,
            store_path=store_path,
            discover_fn=_discover,
            read_pr_state_fn=_read_state,
            fire_skill_fn=_fire,
            dispatch_ritual_fn=_dispatch_ritual,
            emit=_emit,
            reviewers_for=_reviewers_for,
            claim=_claim,
            notify=_notify,
            post_merge_readiness_fn=_post_merge,
            now_iso=now_iso,
            max_age_days=max_age_days,
            max_retries=_max_retries,
            holder=holder,
        )
    finally:
        try:
            _claim.release_tick_lock(_TICK_CLAIM_KEY, holder)
        except Exception as exc:
            log.warning("pr-watch: failed to release tick lock: %s", exc)


def _run_tick(
    *,
    graph_path,
    store_path,
    discover_fn,
    read_pr_state_fn,
    fire_skill_fn,
    dispatch_ritual_fn,
    emit,
    reviewers_for,
    claim,
    notify,
    post_merge_readiness_fn,
    now_iso,
    max_age_days,
    max_retries,
    holder,
) -> TickResult:
    """Inner tick body (called once tick lock is held)."""
    from fno.graph._reconcile import ReconcileError
    from fno.graph.store import read_graph
    from fno.paths import graph_json as default_graph_json
    from fno.pr_watch import decide
    from fno.pr_watch._state import WatermarkStore, make_watermark_key

    gpath = graph_path or default_graph_json()
    entries = read_graph(gpath) if gpath.exists() else []
    candidates = discover_fn(entries)

    store = WatermarkStore(path=store_path)
    # Load once up-front; resets to {} on corruption (baseline discipline)
    store.load()

    acted = 0
    skipped = 0

    for cand in candidates:
        pr = cand.pr_number
        slug = cand.repo_slug
        key = make_watermark_key(repo_slug=slug, pr_number=pr)

        # Skip: no local checkout
        if cand.repo_dir is None:
            emit("pr_watch_skipped", {"pr": pr, "reason": "no-checkout"})
            skipped += 1
            continue

        # Skip: node has a live session claim
        if claim.is_node_live(cand.node_id):
            emit("pr_watch_skipped", {"pr": pr, "reason": "claimed"})
            skipped += 1
            continue

        # Per-PR concurrency guard
        pr_lock_key = f"pr-watch:{slug or 'unknown'}:{pr}"
        try:
            claim.acquire_pr_lock(pr_lock_key, holder)
        except Exception:
            log.debug("pr-watch: PR #%d already being processed, skipping", pr)
            skipped += 1
            continue

        try:
            # Fetch current state
            try:
                reviewers = reviewers_for(cand.repo_dir)
                obs = read_pr_state_fn(cand, reviewers=reviewers)
            except ReconcileError as exc:
                log.warning("pr-watch: gh query failed for PR #%d: %s", pr, exc)
                # Transient: leave watermark unchanged, continue to next PR
                continue

            entry = store.get(key)

            # Guard: a corrupt non-dict entry is treated as absent (re-baseline).
            if entry is not None and not isinstance(entry, dict):
                log.warning("pr-watch: corrupt watermark entry for %s (not a dict); re-baselining", key)
                entry = None

            # First-seen baseline: record state without firing
            if entry is None:
                baseline = {
                    "last_review_ts": obs.latest_review_ts,
                    "last_seen_state": obs.state,
                    "merge_dispatched": obs.state == "MERGED",
                    "retries": 0,
                    "parked": None,
                }
                store.set(key, baseline)
                log.debug("pr-watch: first-seen PR #%d baselined as %s", pr, obs.state)
                continue

            # Skip parked PRs entirely
            if entry.get("parked"):
                continue

            # Compute merge-readiness only when needed
            merge_ready = False
            if obs.state == "MERGED":
                try:
                    merge_ready = post_merge_readiness_fn(cand.repo_dir).is_ready
                except Exception as exc:
                    log.warning("pr-watch: post_merge_readiness failed for PR #%d: %s", pr, exc)

            decision = decide(
                obs,
                watermark=entry,
                reviewers=reviewers_for(cand.repo_dir),
                merge_ready=merge_ready,
                now_iso=now_iso,
                max_age_days=max_age_days,
            )

            if decision.kind == "noop":
                pass  # nothing to do; no event

            elif decision.kind == "park":
                entry["parked"] = decision.reason
                store.set(key, entry)
                emit("pr_watch_parked", {"pr": pr, "reason": decision.reason})

            elif decision.kind in ("merge", "review"):
                dispatch_ok = False
                dispatch_extra: dict[str, Any] = {}
                if decision.kind == "merge":
                    # Route through the shared post-merge dispatcher: warm
                    # inject first, this daemon's headless fire as the cold
                    # fallback, and the SAME per-merge-SHA marker reconcile
                    # consults -- so one merge is handed off exactly once no
                    # matter which detector sees it first.
                    try:
                        pm = dispatch_ritual_fn(cand, obs, fire_skill_fn)
                    except Exception as exc:  # noqa: BLE001 - degrade to retry path
                        log.warning("pr-watch: ritual dispatch for PR #%d failed: %s", pr, exc)
                        pm = None
                    if pm is not None and pm.outcome == "already-dispatched":
                        # marker-exists = a completed hand-off: advance the
                        # watermark so this PR stops re-deciding. lock-contention
                        # = another detector holds the lock RIGHT NOW but may
                        # still fail before writing the marker; do NOT advance,
                        # so the next tick retries (by then either the marker
                        # exists -> genuine skip, or the holder released a failed
                        # claim -> this tick dispatches). Advancing on contention
                        # would silently drop the ritual if that holder crashed.
                        if getattr(pm, "detail", None) == "lock-contention":
                            emit("pr_watch_skipped", {"pr": pr, "reason": "dispatch-in-flight"})
                        else:
                            entry["merge_dispatched"] = True
                            store.set(key, entry)
                            emit("pr_watch_skipped", {"pr": pr, "reason": "already-dispatched"})
                        skipped += 1
                        continue
                    if pm is not None and pm.outcome == "disabled":
                        # auto_run opt-in is off: a deliberate no-op, NOT a
                        # failure. Park so the tick stops re-deciding it (a done
                        # node stays in the discovery window for max_age_days);
                        # no retry, no failure notify. If the operator later
                        # arms auto_run, the past merge is handled manually.
                        entry["parked"] = "auto-run-disabled"
                        store.set(key, entry)
                        emit("pr_watch_skipped", {"pr": pr, "reason": "auto-run-disabled"})
                        skipped += 1
                        continue
                    dispatch_ok = pm is not None and pm.outcome in (
                        "dispatched", "routed-warm",
                    )
                    if dispatch_ok:
                        dispatch_extra = {
                            "route": "warm" if pm.outcome == "routed-warm" else "cold",
                        }
                        log.info(
                            "pr-watch: PR #%d post-merge ritual %s (%s)",
                            pr, pm.outcome, pm.detail or pm.short_id or "",
                        )
                else:
                    result = fire_skill_fn("check", pr, cand.repo_dir)
                    dispatch_ok = result.ok

                if dispatch_ok:
                    acted += 1
                    if decision.kind == "merge":
                        entry["merge_dispatched"] = True
                    else:
                        entry["last_review_ts"] = obs.latest_review_ts
                    entry["retries"] = 0
                    store.set(key, entry)
                    emit("pr_watch_dispatched", {"kind": decision.kind, "pr": pr, **dispatch_extra})
                else:
                    # Dispatch failed: bump retry counter (safe with None/non-int stored value)
                    try:
                        retries = int(entry.get("retries") or 0) + 1
                    except (TypeError, ValueError):
                        retries = 1
                    entry["retries"] = retries
                    store.set(key, entry)
                    emit("pr_watch_dispatch_failed", {"pr": pr, "retries": retries})
                    if retries >= max_retries:
                        entry["parked"] = "retries-exhausted"
                        store.set(key, entry)
                        emit("pr_watch_parked", {"pr": pr, "reason": "retries-exhausted"})
                        try:
                            notify(
                                f"PR #{pr} ({slug}) parked after {retries} failed dispatch attempts",
                                pr=pr,
                                repo_slug=slug,
                            )
                        except Exception as exc:
                            log.warning("pr-watch: notify failed: %s", exc)

        finally:
            try:
                claim.release_pr_lock(pr_lock_key, holder)
            except Exception as exc:
                log.warning("pr-watch: failed to release PR lock for #%d: %s", pr, exc)

    # Heartbeat: always emitted (even on empty/quiet tick)
    emit("pr_watch_tick", {"open_prs": len(candidates), "acted": acted})
    return TickResult(open_prs=len(candidates), acted=acted, skipped=skipped)


# ---------------------------------------------------------------------------
# Default no-op helpers (used when callers don't inject)
# ---------------------------------------------------------------------------


def _noop_emit(event_type: str, data: dict[str, Any]) -> None:  # pragma: no cover
    pass


def _noop_readiness(repo_root: Any) -> Any:  # pragma: no cover
    class _V:
        is_ready = False

    return _V()


def _default_discover(
    entries: list[dict[str, Any]],
    *,
    now_iso: Optional[str] = None,
    max_age_days: int = 14,
) -> list[Any]:  # pragma: no cover
    from fno.pr_watch._discover import discover_open_prs

    return discover_open_prs(entries, now_iso=now_iso, max_age_days=max_age_days)


def _default_dispatch_ritual(cand: Any, obs: Any, fire_skill_fn: Callable) -> Any:
    """Hand one merged PR to the shared post-merge dispatcher (post_merge_route).

    The dispatcher decides warm/cold/defer in one place, reserves an attributable
    receipt, then either live-injects the ritual verb or runs
    ``fno pr ritual <n> --autonomous`` directly (no bg thread), deduped on the
    same per-merge-SHA marker reconcile used. ``fire_skill_fn`` is unused here -
    the cold path is the direct verb, and the review check fire is a separate
    tick branch - but it stays on the signature so the tick's
    ``dispatch_ritual_fn`` protocol is uniform.
    """
    from fno.post_merge_route import dispatch_post_merge_ritual

    # Honor the post_merge.auto_run opt-in: a `ready` verdict means "configured +
    # active", NOT "operator armed automatic dispatch". Without this gate, enabling
    # pr-watch would run the ritual on a repo that never opted in. Fail closed on a
    # config-load error.
    auto_run = False
    if cand.repo_dir is not None:
        try:
            from fno.config import load_settings_for_repo

            auto_run = bool(load_settings_for_repo(Path(cand.repo_dir)).post_merge.auto_run)
        except Exception:  # noqa: BLE001 - fail closed
            auto_run = False

    return dispatch_post_merge_ritual(
        cand.pr_number,
        dedup_key=getattr(obs, "merge_sha", None),
        auto_run=auto_run,
        node_cwd=str(cand.repo_dir) if cand.repo_dir else None,
        ship_session_id=getattr(cand, "ship_session_id", None),
        ship_harness=getattr(cand, "ship_harness", None),
        source_session_id=getattr(cand, "source_session_id", None),
        source_harness=getattr(cand, "source_harness", None),
        source_cwd=getattr(cand, "source_cwd", None),
        node_id=getattr(cand, "node_id", None),
        repo_slug=getattr(cand, "repo_slug", None),
    )


def _default_read_pr_state(
    candidate: Any,
    *,
    reviewers: list[str],
    runner: Optional[Callable[..., Any]] = None,
    timeout_s: float = 30.0,
) -> Any:  # pragma: no cover
    """Default read_pr_state adapter: delegates to the real gh-backed implementation.

    This is the production default.  Tests that do NOT want live gh calls must
    inject their own stub via read_pr_state_fn; _noop_read_state is reserved
    for that purpose and must never be the production default.
    """
    from fno.pr_watch._discover import read_pr_state

    return read_pr_state(candidate, reviewers=reviewers, timeout_s=timeout_s)


def _noop_read_state(
    candidate: Any,
    *,
    reviewers: list[str],
    runner: Optional[Callable[..., Any]] = None,
    timeout_s: float = 30.0,
) -> Any:
    """Test-only no-op: always reports OPEN with no review activity.

    Never used as the production default.  Inject explicitly in tests that
    want to verify tick() behaviour without a live gh process.
    """
    from fno.pr_watch._discover import PrObservation

    return PrObservation(
        pr_number=candidate.pr_number,
        state="OPEN",
        latest_review_ts=None,
        opened_at=None,
    )
