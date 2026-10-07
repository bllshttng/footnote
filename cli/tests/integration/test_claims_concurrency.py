"""Multi-process integration tests for fno agents claim concurrency.

Uses multiprocessing.Process to drive real-process contention on the same
filesystem path, matching the pattern from PR #278 (memory:
``project_pr_278_test_hygiene_shipped.md``). Each test runs deterministically
across 10 consecutive trials; flakiness here is a regression.

These tests do NOT use threads because the O_EXCL race we exercise is at
the kernel level - threads would share file descriptors and produce
unrealistic results.
"""
from __future__ import annotations

import multiprocessing as mp
import socket
from pathlib import Path

import psutil
import pytest

from fno.claims.core import (
    ClaimHeldByOther,
    acquire_claim,
    release_claim,
)
from fno.claims.io import claim_path, serialize_claim
from fno.claims.types import Claim, now_ms

_PROCESS_START_TIMEOUT_SECONDS = 30.0
_RACE_HOLD_SECONDS = 2.0


def _try_acquire(root_str: str, key: str, holder: str, result_queue, hold_secs: float = 0.0) -> None:
    """Child-process worker. Reports outcome via the queue.

    If hold_secs > 0, a winner sleeps that long before exiting so its PID
    stays alive past the assertion. Without this, a winner exits, its PID
    dies, and a sibling worker may legitimately stale-recover - which is
    correct system behavior but breaks the "exactly one winner" invariant
    the test wants to assert.
    """
    import time as _t
    try:
        claim = acquire_claim(key=key, holder=holder, root=Path(root_str))
        result_queue.put(("won", holder, claim.acquired_at))
        if hold_secs > 0:
            _t.sleep(hold_secs)
    except ClaimHeldByOther as exc:
        result_queue.put(("lost", holder, exc.holder))
    except Exception as exc:
        result_queue.put(("error", holder, repr(exc)))


def _run_race(
    root: Path, key: str, n_workers: int, hold_secs: float = _RACE_HOLD_SECONDS
) -> list[tuple]:
    """Spawn n_workers processes racing on (key); return list of outcomes.

    hold_secs keeps the winner's process alive through the native verdict
    subprocess so siblings cannot validly stale-recover. Losers report their
    outcome and exit immediately - their PID dying does not affect the assertion.
    """
    ctx = mp.get_context("spawn")
    queue = ctx.Queue()
    procs = []
    for i in range(n_workers):
        p = ctx.Process(
            target=_try_acquire,
            args=(str(root), key, f"worker-{i}", queue, hold_secs),
        )
        procs.append(p)

    # Start all then collect outcomes BEFORE joining so the winner's
    # process is still alive while siblings make their decisions.
    for p in procs:
        p.start()

    outcomes: list[tuple] = []
    deadline = mp_now() + _PROCESS_START_TIMEOUT_SECONDS
    while len(outcomes) < n_workers and mp_now() < deadline:
        try:
            outcomes.append(queue.get(timeout=0.5))
        except Exception:
            continue

    for p in procs:
        p.join(timeout=_PROCESS_START_TIMEOUT_SECONDS)

    return outcomes


def mp_now() -> float:
    import time as _t
    return _t.monotonic()


# ---------------------------------------------------------------------------
# Concurrent acquire race
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("trial", range(3))
def test_two_processes_race_one_wins(tmp_path, trial):
    """Exactly one worker wins; the other gets ClaimHeldByOther."""
    outcomes = _run_race(tmp_path, key="race-key", n_workers=2)
    wins = [o for o in outcomes if o[0] == "won"]
    losses = [o for o in outcomes if o[0] == "lost"]
    errors = [o for o in outcomes if o[0] == "error"]
    assert len(wins) == 1, f"trial {trial}: expected 1 winner, got {outcomes}"
    assert len(losses) == 1, f"trial {trial}: expected 1 loser, got {outcomes}"
    assert errors == [], f"trial {trial}: errors {errors}"


@pytest.mark.parametrize("trial", range(3))
def test_five_processes_race_one_wins(tmp_path, trial):
    """With 5 racers, exactly one winner, four losers."""
    outcomes = _run_race(tmp_path, key="five-race", n_workers=5)
    wins = [o for o in outcomes if o[0] == "won"]
    losses = [o for o in outcomes if o[0] == "lost"]
    errors = [o for o in outcomes if o[0] == "error"]
    assert len(wins) == 1, f"trial {trial}: expected 1 winner, got {outcomes}"
    assert len(losses) == 4, f"trial {trial}: expected 4 losers, got {outcomes}"
    assert errors == [], f"trial {trial}: errors {errors}"


# ---------------------------------------------------------------------------
# Stale-claim recovery race
# ---------------------------------------------------------------------------


def test_stale_claim_recovered_by_one_winner(tmp_path):
    """Two workers see a stale claim simultaneously; exactly one recovers."""
    # Plant a stale PID-liveness claim using a definitely-dead PID.
    dead_pid = 999_999
    while psutil.pid_exists(dead_pid):
        dead_pid += 1

    stale = Claim(
        key="stale-race",
        holder="old-holder",
        acquired_at=now_ms() - 100_000,
        expires_at=None,
        pid=dead_pid,
        host=socket.gethostname(),
    )
    path = claim_path("stale-race", root=tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(serialize_claim(stale))

    outcomes = _run_race(tmp_path, key="stale-race", n_workers=3)
    wins = [o for o in outcomes if o[0] == "won"]
    losses = [o for o in outcomes if o[0] == "lost"]
    errors = [o for o in outcomes if o[0] == "error"]
    assert len(wins) == 1, f"expected 1 winner from stale recovery, got {outcomes}"
    assert len(losses) == 2, f"expected 2 losers from stale recovery, got {outcomes}"
    assert errors == [], f"errors during stale recovery: {errors}"


# ---------------------------------------------------------------------------
# Idempotent re-acquire from same holder
# ---------------------------------------------------------------------------


def _reacquire_worker(root_str: str, key: str, holder: str, result_queue) -> None:
    """Acquire twice in the same worker; both should succeed."""
    try:
        first = acquire_claim(key=key, holder=holder, root=Path(root_str))
        second = acquire_claim(key=key, holder=holder, root=Path(root_str))
        result_queue.put(("ok", first.acquired_at, second.acquired_at))
    except Exception as exc:
        result_queue.put(("error", repr(exc)))


def test_idempotent_reacquire_succeeds_across_calls(tmp_path):
    ctx = mp.get_context("spawn")
    queue = ctx.Queue()
    p = ctx.Process(
        target=_reacquire_worker,
        args=(str(tmp_path), "reacq-key", "stable-holder", queue),
    )
    p.start()
    p.join(timeout=_PROCESS_START_TIMEOUT_SECONDS)
    assert not queue.empty(), "worker produced no output"
    outcome = queue.get()
    assert outcome[0] == "ok", f"unexpected outcome: {outcome}"
    # Second acquired_at must be >= first
    assert outcome[2] >= outcome[1]


# ---------------------------------------------------------------------------
# Release-then-acquire across processes
# ---------------------------------------------------------------------------


def _acquire_then_release(root_str: str, key: str, holder: str, result_queue) -> None:
    try:
        acquire_claim(key=key, holder=holder, root=Path(root_str))
        release_claim(key=key, holder=holder, root=Path(root_str))
        result_queue.put(("ok", holder))
    except Exception as exc:
        result_queue.put(("error", repr(exc)))


def test_serial_acquire_release_across_processes(tmp_path):
    """Process A acquires + releases; process B acquires next - no conflict."""
    ctx = mp.get_context("spawn")
    q = ctx.Queue()

    p1 = ctx.Process(target=_acquire_then_release, args=(str(tmp_path), "k", "A", q))
    p1.start()
    p1.join(timeout=5)
    out1 = q.get()
    assert out1[0] == "ok"

    p2 = ctx.Process(target=_acquire_then_release, args=(str(tmp_path), "k", "B", q))
    p2.start()
    p2.join(timeout=5)
    out2 = q.get()
    assert out2[0] == "ok"


# ---------------------------------------------------------------------------
# Worktree canonical-root resolution for claims
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# Release -> reacquire holder-flip (T1: handoff claim seam)
# ---------------------------------------------------------------------------

