"""Unit tests for fno.claims.staleness: PID-liveness + TTL expiry."""
from __future__ import annotations

import os
import socket
import time
from unittest.mock import patch

import psutil
import pytest

from fno.claims.staleness import (
    classify,
    is_expired,
    is_live,
    now_ms,
)
from fno.claims.types import Claim, ClaimState


def _live_claim(**overrides) -> Claim:
    pid = os.getpid()
    proc_create_ms = int(psutil.Process(pid).create_time() * 1000)
    # Mark the claim as created AFTER the process started so it's "ours".
    defaults: dict = {
        "key": "k",
        "holder": "h",
        "acquired_at": proc_create_ms + 1000,
        "expires_at": None,
        "pid": pid,
        "host": socket.gethostname(),
    }
    defaults.update(overrides)
    return Claim(**defaults)


# ---------------------------------------------------------------------------
# is_live
# ---------------------------------------------------------------------------


def test_is_live_returns_true_for_current_process():
    claim = _live_claim()
    assert is_live(claim) is True


def test_is_live_returns_false_for_remote_host():
    claim = _live_claim(host="some-other-host-that-does-not-exist")
    assert is_live(claim) is False


def test_is_live_returns_false_when_pid_does_not_exist():
    # Pick a PID that definitely doesn't exist - very high numbers are safe.
    dead_pid = 999_999
    while psutil.pid_exists(dead_pid):
        dead_pid += 1
    claim = _live_claim(pid=dead_pid)
    assert is_live(claim) is False


def test_is_live_false_when_pid_reused():
    """PID-reuse: process create_time AFTER acquired_at means the slot was recycled."""
    pid = os.getpid()
    proc_create_ms = int(psutil.Process(pid).create_time() * 1000)
    # Claim acquired BEFORE the process started => stale (different process).
    claim = Claim(
        key="k",
        holder="h",
        acquired_at=proc_create_ms - 10_000,
        expires_at=None,
        pid=pid,
        host=socket.gethostname(),
    )
    assert is_live(claim) is False


# ---------------------------------------------------------------------------
# is_expired
# ---------------------------------------------------------------------------


def test_is_expired_true_when_past():
    claim = _live_claim(expires_at=now_ms() - 1000)
    assert is_expired(claim) is True


def test_is_expired_false_when_future():
    claim = _live_claim(expires_at=now_ms() + 60_000)
    assert is_expired(claim) is False


def test_is_expired_false_for_pid_liveness_claim():
    """PID-liveness claims (expires_at=None) are NEVER expired by this check."""
    claim = _live_claim(expires_at=None)
    assert is_expired(claim) is False


def test_is_expired_at_exactly_now_is_expired():
    now = 1747641600000
    claim = _live_claim(expires_at=now)
    assert is_expired(claim, now=now) is True


# ---------------------------------------------------------------------------
# classify
# ---------------------------------------------------------------------------


def test_classify_live_pid_liveness():
    claim = _live_claim()
    assert classify(claim) == ClaimState.LIVE


def test_classify_stale_pid_liveness_dead_process():
    dead_pid = 999_999
    while psutil.pid_exists(dead_pid):
        dead_pid += 1
    claim = _live_claim(pid=dead_pid)
    assert classify(claim) == ClaimState.STALE


def test_classify_stale_ttl_expired_dead_pid():
    """Expired TTL + dead recorded pid -> STALE (byte-for-byte today)."""
    dead_pid = 999_999
    while psutil.pid_exists(dead_pid):
        dead_pid += 1
    claim = _live_claim(pid=dead_pid, expires_at=now_ms() - 1000)
    assert classify(claim) == ClaimState.STALE


def test_classify_live_ttl_expired_with_live_pid():
    """HYBRID ARM (AC1-HP): an expired TTL claim whose recorded pid is a live
    process on this host (create_time < acquired_at) is LIVE, not STALE - the
    session is alive (incl. suspended) even though the clock lapsed.

    This is the one new case the hybrid liveness introduces. Before the change
    this returned STALE; the pid arm only ever extends liveness."""
    claim = _live_claim(expires_at=now_ms() - 1000)  # live pid via _live_claim
    assert classify(claim) == ClaimState.LIVE


def test_classify_stale_ttl_expired_pid_reused():
    """AC4-EDGE: expired TTL + pid whose process started AFTER acquired_at
    (pid reuse) -> STALE. The is_live create_time guard prevents a false-live."""
    pid = os.getpid()
    proc_create_ms = int(psutil.Process(pid).create_time() * 1000)
    claim = Claim(
        key="k",
        holder="h",
        acquired_at=proc_create_ms - 10_000,  # claim filed BEFORE this proc -> reuse
        expires_at=now_ms() - 1000,
        pid=pid,
        host=socket.gethostname(),
    )
    assert classify(claim) == ClaimState.STALE


def test_classify_stale_ttl_expired_remote_host():
    """AC2-FR: an expired TTL claim on another host is STALE - the pid arm is
    same-host-only and does not falsely keep a remote claim alive."""
    claim = _live_claim(
        host="some-other-host-that-does-not-exist", expires_at=now_ms() - 1000
    )
    assert classify(claim) == ClaimState.STALE


def test_classify_live_ttl_unexpired_regardless_of_pid():
    """AC4-ERR: an unexpired TTL claim is LIVE even if the PID is dead -
    the TTL is the contract within the window, not PID-liveness."""
    dead_pid = 999_999
    while psutil.pid_exists(dead_pid):
        dead_pid += 1
    claim = _live_claim(pid=dead_pid, expires_at=now_ms() + 60_000)
    assert classify(claim) == ClaimState.LIVE


# ---------------------------------------------------------------------------
# now_ms is monotonic enough
# ---------------------------------------------------------------------------


def test_now_ms_increases_over_time():
    a = now_ms()
    time.sleep(0.01)
    b = now_ms()
    assert b >= a
