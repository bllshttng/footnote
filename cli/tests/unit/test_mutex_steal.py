"""Age-based rename-steal of stale mkdir mutexes.

Covers the design-doc ACs for the two mutexes that share the disease:
``events.jsonl.lock.d`` (AC1/AC2/AC3) and ``<claim>.lock.recovery.d`` (AC4).

The live incident these guard against: a process died holding both, and every
waiter spun against the corpse for eight days.
"""
from __future__ import annotations

import os
import socket
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import psutil
import pytest

from fno.claims.core import acquire_claim
from fno.claims.io import claim_path, claims_dir, serialize_claim
from fno.claims.staleness import now_ms
from fno.claims.types import Claim
from fno.events import append_event, mission_started
from fno.mutex import STALE_MUTEX_STEAL_S, steal_if_stale

HOLDER_A = "target-session:sid-a"
HOLDER_B = "target-session:sid-b"


def _age(path: Path, seconds: float) -> None:
    """Backdate a lock dir's mtime, which is what the steal predicate reads."""
    old = time.time() - seconds
    os.utime(path, (old, old))


def _event() -> dict:
    return mission_started(mission_id="m-steal")


class TestStealHelper:
    def test_AC2_EDGE_fresh_lock_never_stolen(self, tmp_path):
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        assert steal_if_stale(lock) is False
        assert lock.exists()

    def test_AC2_EDGE_at_threshold_not_stolen(self, tmp_path):
        """Strictly older than the threshold, not at it."""
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        _age(lock, STALE_MUTEX_STEAL_S - 1)
        assert steal_if_stale(lock) is False
        assert lock.exists()

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
        """A leftover reap dir from our own earlier steal is inert, not fatal."""
        lock = tmp_path / "a.lock.d"
        lock.mkdir()
        leftover = tmp_path / f"a.lock.d.reap.{os.getpid()}"
        leftover.mkdir()
        (leftover / "junk").write_text("x")
        _age(lock, STALE_MUTEX_STEAL_S + 60)
        assert steal_if_stale(lock) is True
        assert not lock.exists()


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
        """Exactly one rename wins; both events land as whole lines."""
        events = tmp_path / "events.jsonl"
        lock = tmp_path / "events.jsonl.lock.d"
        lock.mkdir()
        _age(lock, STALE_MUTEX_STEAL_S + 60)

        def emit() -> None:
            append_event(_event(), events_path=events, lock_timeout_seconds=10)

        with ThreadPoolExecutor(max_workers=2) as pool:
            for fut in [pool.submit(emit), pool.submit(emit)]:
                fut.result()

        lines = [ln for ln in events.read_text().splitlines() if ln.strip()]
        assert len(lines) == 2
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
