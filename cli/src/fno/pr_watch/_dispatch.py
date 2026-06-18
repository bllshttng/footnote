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

Verb = Literal["check", "merged"]


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


# ---------------------------------------------------------------------------
# fire_skill
# ---------------------------------------------------------------------------

_ENV_SEAM = "PR_WATCH_FIRE_CMD"
_DEFAULT_MODEL = "claude-haiku-4-5"


def fire_skill(
    verb: Verb,
    pr_number: int,
    repo_dir: Path,
    *,
    model: str = _DEFAULT_MODEL,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    env_seam: str = _ENV_SEAM,
) -> DispatchResult:
    """Fire a headless ``claude --print`` for one PR and return the result.

    The command shape mirrors ``watch.sh``'s ``cd repo && claude --print ...``
    with ``--output-format json`` so we can detect ``is_error:true`` even
    when the exit code is 0.

    A test seam (``PR_WATCH_FIRE_CMD`` env var, or the *env_seam* parameter)
    replaces the ``claude`` binary with an arbitrary command string for unit
    tests that do not want a live claude process.  When the seam is set, the
    command is built as ``["<seam>"]`` (a single-token override); the runner
    receives it like any other call.

    SUCCESS = rc == 0 AND parsed ``is_error`` is ``False``.
    Every other outcome is a failure.
    """
    seam_cmd = os.environ.get(env_seam)

    if seam_cmd:
        cmd = [seam_cmd]
    else:
        # Build the real claude --print invocation.
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

    try:
        result = runner(
            cmd,
            capture_output=True,
            text=True,
            check=False,
            cwd=str(repo_dir),
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
    _reviewers_for = reviewers_for if reviewers_for is not None else (lambda _: [])
    _post_merge = post_merge_readiness_fn if post_merge_readiness_fn is not None else _noop_readiness
    _discover = discover_fn if discover_fn is not None else _default_discover
    _read_state = read_pr_state_fn if read_pr_state_fn is not None else _default_read_pr_state
    _max_retries = max_retries if max_retries is not None else _MAX_RETRIES

    holder = f"pr-watch:{os.getpid()}"

    # Step 1: tick-level mutex
    try:
        _claim.acquire_tick_lock(_TICK_CLAIM_KEY, holder)
    except Exception:
        # Lock held by another tick: return silently with NO events.
        log.debug("pr-watch: tick lock held, skipping this interval")
        return TickResult(open_prs=0, acted=0)

    try:
        return _run_tick(
            graph_path=graph_path,
            store_path=store_path,
            discover_fn=_discover,
            read_pr_state_fn=_read_state,
            fire_skill_fn=_fire,
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
                verb: Verb = "merged" if decision.kind == "merge" else "check"
                result = fire_skill_fn(verb, pr, cand.repo_dir)

                if result.ok:
                    acted += 1
                    if decision.kind == "merge":
                        entry["merge_dispatched"] = True
                    else:
                        entry["last_review_ts"] = obs.latest_review_ts
                    entry["retries"] = 0
                    store.set(key, entry)
                    emit("pr_watch_dispatched", {"kind": decision.kind, "pr": pr})
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


def _default_discover(entries: list[dict[str, Any]]) -> list[Any]:  # pragma: no cover
    from fno.pr_watch._discover import discover_open_prs

    return discover_open_prs(entries)


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
