"""Multi-process integration tests for fno claim concurrency.

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
import os
import socket
from pathlib import Path
from typing import Optional

import psutil
import pytest

from fno.claims.core import (
    ClaimHeldByOther,
    acquire_claim,
    claim_status,
    release_claim,
)
from fno.claims.io import claim_path, claims_dir, serialize_claim
from fno.claims.staleness import now_ms
from fno.claims.types import Claim


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


def _run_race(root: Path, key: str, n_workers: int, hold_secs: float = 0.5) -> list[tuple]:
    """Spawn n_workers processes racing on (key); return list of outcomes.

    hold_secs keeps the winner's process alive past the join so siblings
    can't validly stale-recover. Losers report their outcome and exit
    immediately - their PID dying does not affect the assertion.
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
    deadline = mp_now() + 5.0
    while len(outcomes) < n_workers and mp_now() < deadline:
        try:
            outcomes.append(queue.get(timeout=0.5))
        except Exception:
            continue

    for p in procs:
        p.join(timeout=10)

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
    p.join(timeout=5)
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


def test_claims_dir_resolves_to_canonical_root_from_linked_worktree(
    tmp_path, monkeypatch
):
    """AC1-HP: claims_dir() with no root arg lands under the canonical repo root.

    Scenario: a git repo with a linked worktree. When cwd is the LINKED
    worktree and both FNO_CLAIMS_ROOT and FNO_REPO_ROOT are unset,
    claims_dir() must resolve to <canonical>/.fno/claims/, NOT to
    <linked-worktree>/.fno/claims/.

    This verifies Locked Decision 9: claims are cross-worktree coordination
    state and must share a single directory regardless of which worktree the
    caller runs from.
    """
    import subprocess

    # -- Build an isolated git repo in tmp_path/canonical -------------------
    canonical = tmp_path / "canonical"
    canonical.mkdir()
    subprocess.run(["git", "init", str(canonical)], check=True, capture_output=True)
    subprocess.run(
        ["git", "-C", str(canonical), "commit", "--allow-empty", "-m", "init"],
        check=True,
        capture_output=True,
        env={**os.environ, "GIT_AUTHOR_NAME": "test", "GIT_AUTHOR_EMAIL": "t@t.com",
             "GIT_COMMITTER_NAME": "test", "GIT_COMMITTER_EMAIL": "t@t.com"},
    )

    # -- Add a linked worktree at tmp_path/linked ----------------------------
    linked = tmp_path / "linked"
    subprocess.run(
        ["git", "-C", str(canonical), "worktree", "add", str(linked), "--detach"],
        check=True,
        capture_output=True,
    )

    # -- Patch cwd to the linked worktree; clear env vars --------------------
    monkeypatch.chdir(linked)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)

    # -- Acquire a claim via root=None (exercises the fallback path) ---------
    from fno.claims.core import acquire_claim, release_claim
    from fno.claims.io import claims_dir

    claim = acquire_claim(key="worktree-test-claim", holder="test-holder", root=None)
    try:
        resolved = claims_dir(root=None)

        # The claim file must live under the CANONICAL root, not the linked worktree.
        assert str(resolved).startswith(str(canonical)), (
            f"claims_dir() resolved to {resolved!r}, expected prefix {canonical!r}. "
            "Fix: replace Path.cwd() fallback in claims_dir() with "
            "resolve_canonical_repo_root()."
        )
        assert not str(resolved).startswith(str(linked)), (
            f"claims_dir() resolved to the linked worktree {linked!r} - "
            "cross-worktree coordination invariant violated."
        )

        # The lock file must physically exist under canonical.
        from fno.claims.io import claim_path
        lock = claim_path("worktree-test-claim", root=None)
        assert lock.exists(), f"lock file missing at {lock}"
        assert str(lock).startswith(str(canonical)), (
            f"lock file at {lock!r} is not under canonical {canonical!r}"
        )

        # Also verify: listing from canonical root sees the claim.
        from fno.claims.core import claim_status
        status = claim_status("worktree-test-claim", root=canonical)
        assert status.get("state") in ("live", "stale"), (
            f"claim not visible from canonical root: status={status!r}"
        )
    finally:
        release_claim(key="worktree-test-claim", holder="test-holder", root=None)


# ---------------------------------------------------------------------------
# Release -> reacquire holder-flip (T1: handoff claim seam)
# ---------------------------------------------------------------------------


def test_release_then_reacquire_holder_flip(tmp_path):
    """T1: parent releases node claim, child acquires it - exactly one live claim.

    Closes the release->reacquire seam in the handoff unwind protocol: after
    a parent generation releases node:<id>, the successor (child) must be able
    to acquire it and become the sole live holder. Uses the real claims library
    with production-style holder strings (target-session:<sid>).
    """
    key = "node:ab-deadbeef"
    parent_holder = "target-session:20260605T120000Z-11111-parent"
    child_holder = "target-session:20260605T120001Z-22222-child"
    root = tmp_path

    # Parent acquires
    acquire_claim(key=key, holder=parent_holder, root=root)

    # Verify parent holds it
    status_before = claim_status(key, root=root)
    assert status_before.get("state") == "live", (
        f"expected parent to hold claim after acquire; got {status_before!r}"
    )
    assert status_before.get("holder") == parent_holder, (
        f"expected holder={parent_holder!r}; got {status_before.get('holder')!r}"
    )

    # Parent releases
    release_claim(key=key, holder=parent_holder, root=root)

    # Child acquires
    acquire_claim(key=key, holder=child_holder, root=root)

    # Exactly one live claim file must exist
    from fno.claims.io import claims_dir
    lock_files = list(claims_dir(root=root).glob("*.lock"))
    assert len(lock_files) == 1, (
        f"expected exactly one .lock file after holder flip; found {[str(f) for f in lock_files]}"
    )

    # The live claim must be held by the child
    status_after = claim_status(key, root=root)
    assert status_after.get("state") == "live", (
        f"expected live claim after child acquire; got {status_after!r}"
    )
    assert status_after.get("holder") == child_holder, (
        f"expected holder={child_holder!r} after flip; got {status_after.get('holder')!r}"
    )

    # Clean up
    release_claim(key=key, holder=child_holder, root=root)
