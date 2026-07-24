"""Age-based rename-steal of stale mkdir mutexes.

Covers the design-doc ACs for the two mutexes that share the disease:
``events.jsonl.lock.d`` (AC1/AC2/AC3) and ``<claim>.lock.recovery.d`` (AC4).

The live incident these guard against: a process died holding both, and every
waiter spun against the corpse for eight days.
"""
from __future__ import annotations

import json
import os
import re
import socket
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import psutil
import pytest

from fno import mutex
from fno.claims.core import acquire_claim
from fno.claims.io import claim_path, claims_dir, serialize_claim
from fno.claims.staleness import now_ms
from fno.claims.types import Claim
from fno.events import append_event, mission_started
from fno.mutex import STALE_MUTEX_STEAL_S, acquire_dir_mutex, release_dir_mutex, steal_if_stale

HOLDER_A = "target-session:sid-a"
HOLDER_B = "target-session:sid-b"


def _age(path: Path, seconds: float) -> None:
    """Backdate a lock dir's mtime, which is what the steal predicate reads."""
    old = time.time() - seconds
    os.utime(path, (old, old))


def _age_nofollow(path: Path, seconds: float) -> None:
    """Backdate a symlink itself rather than the target it points at."""
    old = time.time() - seconds
    os.utime(path, (old, old), follow_symlinks=False)


class _FrozenClock:
    """Stands in for the `time` module so an exact age is decidable."""

    def __init__(self, lock_dir: Path, age_s: float) -> None:
        self._now = lock_dir.lstat().st_mtime + age_s

    def time(self) -> float:
        return self._now

    def monotonic_ns(self) -> int:
        return 0


def _event() -> dict:
    return mission_started(mission_id="m-steal")


def test_threshold_matches_the_rust_constant():
    """The `.recovery.d` mutex is wire protocol, so the thresholds must agree.

    Both sides plant corpses using the Python constant, so a drifted Rust
    constant would leave every other test green while the two implementations
    disagreed about which locks are corpses.
    """
    root = next(
        (p for p in Path(__file__).resolve().parents if (p / "crates").is_dir()), None
    )
    if root is None:
        pytest.skip("crates/ not present (installed-wheel test run)")

    src = (root / "crates/fno-agents/src/claims.rs").read_text(encoding="utf-8")
    m = re.search(
        r"const STALE_MUTEX_STEAL: Duration = Duration::from_secs\((\d+)\)", src
    )
    assert m, "STALE_MUTEX_STEAL not found in claims.rs (renamed or reshaped?)"
    assert int(m.group(1)) == STALE_MUTEX_STEAL_S


class TestStealHelper:
    def test_AC2_EDGE_fresh_lock_never_stolen(self, tmp_path):
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        assert steal_if_stale(lock) is False
        assert lock.exists()

    def test_AC2_EDGE_exactly_at_threshold_is_held(self, tmp_path, monkeypatch):
        """The predicate is `<=`, so equal-to-threshold is held, not stolen.

        Backdating with utime cannot pin this: microseconds elapse before the
        predicate reads the clock, pushing the age just past the threshold. A
        frozen clock is what makes `<=` vs `<` actually decidable.
        """
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        monkeypatch.setattr(mutex, "time", _FrozenClock(lock, STALE_MUTEX_STEAL_S))

        assert steal_if_stale(lock) is False
        assert lock.exists()

    def test_AC2_EDGE_one_second_past_threshold_is_stolen(self, tmp_path, monkeypatch):
        """The other side of the same boundary, on the same frozen clock."""
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        monkeypatch.setattr(mutex, "time", _FrozenClock(lock, STALE_MUTEX_STEAL_S + 1))

        assert steal_if_stale(lock) is True
        assert not lock.exists()

    def test_AC1_HP_corpse_stolen(self, tmp_path):
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        _age(lock, STALE_MUTEX_STEAL_S + 60)
        assert steal_if_stale(lock) is True
        assert not lock.exists()

    def test_AC1_EDGE_non_empty_corpse_stolen(self, tmp_path):
        """Emptiness is not assumed: a corpse with contents still goes."""
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        (lock / "holder").write_text("junk")
        _age(lock, STALE_MUTEX_STEAL_S + 60)
        assert steal_if_stale(lock) is True
        assert not lock.exists()

    def test_AC2_ERR_vanished_lock_reports_retry(self, tmp_path):
        """Released between contention and stat: retry the mkdir, do not wait."""
        assert steal_if_stale(tmp_path / "gone.lock.d") is True

    def test_AC1_HP_steal_logs_one_line(self, tmp_path, caplog):
        """A silent steal would hide a crasher that keeps minting corpses."""
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        _age(lock, STALE_MUTEX_STEAL_S + 60)
        with caplog.at_level("WARNING", logger="fno.mutex"):
            steal_if_stale(lock)
        assert len([r for r in caplog.records if "stole stale mutex" in r.message]) == 1

    def test_AC1_EDGE_prior_reap_leftover_does_not_block(self, tmp_path):
        """A leftover reap dir from an earlier steal is inert, not fatal.

        Reap names are unique per attempt, so an undeletable leftover cannot
        collide and permanently disable stealing for this process.
        """
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        leftover = tmp_path / f"a.lock.d.reap.{os.getpid()}"
        leftover.mkdir()
        (leftover / "junk").write_text("x")
        _age(lock, STALE_MUTEX_STEAL_S + 60)
        assert steal_if_stale(lock) is True
        assert not lock.exists()

    def test_AC1_EDGE_repeated_steals_never_collide(self, tmp_path):
        """Two steals of the same path in one process must both succeed."""
        for _ in range(3):
            lock = tmp_path / "a.lock.d"
            lock.mkdir()
            _age(lock, STALE_MUTEX_STEAL_S + 60)
            assert steal_if_stale(lock) is True
            assert not lock.exists()

    def test_AC2_ERR_dangling_symlink_never_spins(self, tmp_path):
        """A dangling symlink is EEXIST to mkdir but ENOENT to a following stat.

        Returning "retry now" there would spin the caller's loop forever at
        100% CPU with its timeout unreachable, so the age must be read with
        lstat and an old dangling link stolen like any other corpse.
        """
        lock = tmp_path / "a.lock.d"
        lock.symlink_to(tmp_path / "nonexistent-target")
        _age_nofollow(lock, STALE_MUTEX_STEAL_S + 60)

        assert steal_if_stale(lock) is True
        assert not lock.is_symlink()

    def test_AC1_EDGE_stolen_symlink_leaves_no_garbage(self, tmp_path):
        """rmtree raises NotADirectoryError on a symlink, so the reaped link
        would survive as litter under ignore_errors."""
        lock = tmp_path / "a.lock.d"
        lock.symlink_to(tmp_path / "nonexistent-target")
        _age_nofollow(lock, STALE_MUTEX_STEAL_S + 60)

        assert steal_if_stale(lock) is True

        leftovers = [p for p in tmp_path.iterdir() if ".reap." in p.name]
        assert leftovers == [], f"reaped symlink left behind: {leftovers}"

    def test_AC3_EDGE_a_live_lock_swapped_in_is_not_stolen(self, tmp_path, monkeypatch):
        """The rename is atomic per path, not per token.

        A waiter can be descheduled after reading the age and owner token,
        another stealer win, and a fresh holder acquire at the same path
        (stamping its own owner token). Renaming then moves a LIVE lock using
        the corpse's age; the owner-token mismatch is what detects it and puts
        the live lock back.
        """
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        (lock / "owner").write_text("corpse-token")
        _age(lock, STALE_MUTEX_STEAL_S + 60)

        real_rename = os.rename

        def swap_then_rename(src, dst):
            # Stand in for the race: the corpse is replaced by a fresh holder
            # (with its own owner token) between our age read and our rename.
            os.rmdir(src)
            os.mkdir(src)
            (src / "owner").write_text("fresh-token")
            return real_rename(src, dst)

        monkeypatch.setattr(mutex.os, "rename", swap_then_rename)

        assert steal_if_stale(lock) is False, "stole a freshly acquired lock"
        assert lock.is_dir(), "the live lock was not restored"

    def test_AC3_EDGE_identity_survives_inode_reuse(self, tmp_path):
        """Identity is by owner token, independent of inode.

        Linux hands a new directory the inode just freed by the old one, so an
        inode compare could not tell a corpse from a fresh live lock that reused
        it. The owner token carries identity without caring about the inode: a
        reaped dir matches on token, and a swapped-in fresh lock (different
        token) does not.
        """
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        (lock / "owner").write_text("corpse-token")

        assert mutex._same_owner(lock, "corpse-token") is True

        # A fresh holder swapped in stamps its own token: not the same lock.
        (lock / "owner").write_text("fresh-token")
        assert mutex._same_owner(lock, "corpse-token") is False

    def test_AC2_ERR_fresh_dangling_symlink_is_waited_on(self, tmp_path):
        """The same path, still fresh, must fall through to the wait loop."""
        lock = tmp_path / "a.lock.d"
        lock.symlink_to(tmp_path / "nonexistent-target")

        assert steal_if_stale(lock) is False
        assert lock.is_symlink()


class TestEventsMutex:
    def test_AC1_HP_corpse_stolen_and_event_lands(self, tmp_path):
        events = tmp_path / "events.jsonl"
        lock = tmp_path / "events.jsonl.lock.d"
        lock.mkdir()
        _age(lock, STALE_MUTEX_STEAL_S + 60)

        append_event(_event(), events_path=events, lock_timeout_seconds=1)

        assert events.read_text().count("\n") == 1
        assert not lock.exists()

    def test_AC2_EDGE_fresh_lock_still_times_out(self, tmp_path):
        """Honest contention keeps today's caller-facing behavior exactly."""
        events = tmp_path / "events.jsonl"
        (tmp_path / "events.jsonl.lock.d").mkdir()

        with pytest.raises(TimeoutError, match="events.jsonl lock timeout"):
            append_event(_event(), events_path=events, lock_timeout_seconds=1)

    def test_AC3_FR_concurrent_stealers_both_land(self, tmp_path):
        """Exactly one rename wins; both events land as whole lines.

        The barrier is load-bearing. Without it the first thread steals,
        appends and releases before the second even contends, so the test
        passes under full serialization and never exercises the race.
        """
        events = tmp_path / "events.jsonl"
        lock = tmp_path / "events.jsonl.lock.d"
        lock.mkdir()
        _age(lock, STALE_MUTEX_STEAL_S + 60)

        workers = 4
        gate = threading.Barrier(workers)

        def emit() -> None:
            gate.wait()
            append_event(_event(), events_path=events, lock_timeout_seconds=10)

        with ThreadPoolExecutor(max_workers=workers) as pool:
            for fut in [pool.submit(emit) for _ in range(workers)]:
                fut.result()

        lines = [ln for ln in events.read_text().splitlines() if ln.strip()]
        assert len(lines) == workers
        assert all(json.loads(ln)["type"] == "mission_started" for ln in lines)
        assert not lock.exists()

    def test_AC3_FR_only_one_stealer_wins_the_rename(self, tmp_path, caplog):
        """Contend directly on the predicate so the race is unavoidable."""
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        _age(lock, STALE_MUTEX_STEAL_S + 60)

        workers = 8
        gate = threading.Barrier(workers)
        results: list[bool] = []
        lk = threading.Lock()

        def race() -> None:
            gate.wait()
            got = steal_if_stale(lock)
            with lk:
                results.append(got)

        with caplog.at_level("WARNING", logger="fno.mutex"):
            threads = [threading.Thread(target=race) for _ in range(workers)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()

        steals = [r for r in caplog.records if "stole stale mutex" in r.message]
        assert len(steals) == 1, f"expected exactly one winner, got {len(steals)}"
        assert not lock.exists()

    def test_AC2_EDGE_release_after_steal_leaves_new_holder_intact(self, tmp_path):
        """AC2: a holder whose lock was stolen mid-write must not delete the new
        holder's lock on release.

        This is the wrongful-delete vector the owner token exists to close.
        Before the token, release rmdir'd by path and could remove a different
        holder's live lock, cascading into lock timeouts under load. Release now
        verifies ownership and leaves a mismatched dir intact; the new holder's
        subsequent release still succeeds.
        """
        lock = tmp_path / "events.jsonl.lock.d"

        # Victim acquires, then is suspended past the steal threshold.
        victim_token = acquire_dir_mutex(lock, 5)
        assert victim_token is not None
        _age(lock, STALE_MUTEX_STEAL_S + 60)

        # A stealer reaps the corpse, then a new holder acquires at the path.
        assert steal_if_stale(lock) is True
        assert not lock.exists()
        new_token = acquire_dir_mutex(lock, 5)
        assert new_token is not None
        assert new_token != victim_token

        # Victim resumes and releases: the new holder's lock must survive.
        release_dir_mutex(lock, victim_token)
        assert lock.exists(), "victim's release deleted the new holder's lock"

        # The new holder releases cleanly (token matches -> rmtree).
        release_dir_mutex(lock, new_token)
        assert not lock.exists()


class TestRecoveryMutex:
    def _write_stale_claim(self, tmp_path: Path, key: str) -> Path:
        dead_pid = 999_999
        while psutil.pid_exists(dead_pid):
            dead_pid += 1
        stale = Claim(
            key=key,
            holder=HOLDER_A,
            acquired_at=now_ms() - 100_000,
            expires_at=None,
            pid=dead_pid,
            host=socket.gethostname(),
        )
        path = claim_path(key, root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialize_claim(stale))
        return path

    def test_AC4_HP_recovery_corpse_no_longer_bricks_a_claim(self, tmp_path):
        """The permanence mechanism of the 8-day outage: a dead recoverer."""
        path = self._write_stale_claim(tmp_path, "k")
        recovery_lock = path.with_name(path.name + ".recovery.d")
        recovery_lock.mkdir()
        _age(recovery_lock, STALE_MUTEX_STEAL_S + 60)

        new = acquire_claim("k", HOLDER_B, root=tmp_path)

        assert new.holder == HOLDER_B
        assert not recovery_lock.exists()
        assert any((claims_dir(tmp_path) / ".expired").iterdir())

    def test_AC4_ERR_dangling_recovery_lock_does_not_recurse(self, tmp_path):
        """The waiter must not follow symlinks either.

        `exists()` reports a dangling `.recovery.d` absent while `mkdir` still
        raises EEXIST, so the waiter returns instantly and acquire_claim
        recurses with no pause until RecursionError.
        """
        from fno.claims.core import _wait_for_recovery_release

        recovery_lock = tmp_path / "k.lock.recovery.d"
        recovery_lock.symlink_to(tmp_path / "nonexistent-target")

        started = time.monotonic()
        _wait_for_recovery_release(recovery_lock)

        assert time.monotonic() - started >= 1.0, (
            "waiter returned instantly on a dangling mutex; the caller would "
            "recurse without pause"
        )

    def test_AC4_EDGE_fresh_recovery_lock_is_respected(self, tmp_path):
        """A live recoverer's mutex is never stolen; the waiter backs off.

        Asserted on the predicate rather than by driving acquire_claim: a
        contended fresh recovery mutex makes acquire wait-and-recurse, which
        only terminates once the mutex clears or ages past the threshold.
        """
        path = self._write_stale_claim(tmp_path, "k")
        recovery_lock = path.with_name(path.name + ".recovery.d")
        recovery_lock.mkdir()

        assert steal_if_stale(recovery_lock) is False
        assert recovery_lock.exists()
