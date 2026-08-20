"""TTL renewal + loud-expiry semantics (x-a7ab 1.4).

A ticking worker refreshes its node claim so a long-running loop never silently
expires its TTL and frees the node for a twin. When a worker stops ticking, an
expired claim must still surface its prior holder - a dispatcher reading the
claim never sees a silently-free node.
"""
import os

import psutil

from fno.claims.core import acquire_claim, claim_status, refresh_claim
from fno.claims.io import claim_path, serialize_claim
from fno.claims.staleness import classify, now_ms
from fno.claims.types import Claim, ClaimState


def _acquire_ttl(root, key="node:N", holder="target-session:me", ttl_ms=60_000):
    return acquire_claim(key, holder, ttl_ms=ttl_ms, pid=os.getpid(), root=root)


def test_refresh_extends_ttl_window(tmp_path):
    # AC4-FR: a refreshed TTL claim's window moves forward (stays current).
    claim = _acquire_ttl(tmp_path)
    assert claim.expires_at is not None
    refreshed = refresh_claim("node:N", "target-session:me", ttl_ms=60_000, root=tmp_path)
    assert refreshed is not None
    assert refreshed.expires_at > now_ms()
    # Still classified live (holder pid is this process).
    assert classify(refreshed) in (ClaimState.LIVE, ClaimState.SUSPECT)


def test_refresh_idempotent_under_repeated_ticks(tmp_path):
    # A loop ticks refresh every boundary; repeated refreshes never error and
    # keep the claim current.
    _acquire_ttl(tmp_path)
    for _ in range(5):
        r = refresh_claim("node:N", "target-session:me", ttl_ms=60_000, root=tmp_path)
        assert r is not None and r.expires_at > now_ms()
    assert claim_status("node:N", root=tmp_path)["state"] in ("live", "suspect")


def test_refresh_is_noop_for_pid_liveness_claim(tmp_path):
    # A PID-only claim (no expires_at) refreshes to None - safe to call from a
    # generic timer that does not know the claim's mode.
    acquire_claim("node:P", "target-session:me", pid=os.getpid(), root=tmp_path)
    assert refresh_claim("node:P", "target-session:me", root=tmp_path) is None


def test_refresh_holder_mismatch_is_rejected(tmp_path):
    # A respawned/foreign holder must not extend another session's claim.
    from fno.claims.core import HolderMismatch

    _acquire_ttl(tmp_path, holder="target-session:OWNER")
    import pytest

    with pytest.raises(HolderMismatch):
        refresh_claim("node:N", "target-session:RIVAL", ttl_ms=60_000, root=tmp_path)


def test_expired_claim_status_names_prior_holder(tmp_path):
    # AC4-FR loud expiry: an expired claim whose pid is dead reads STALE, and its
    # status still names the prior holder - never a silently-free node.
    expired = Claim(
        schema_version=1,
        key="node:GHOST",
        holder="target-session:gone",
        acquired_at=now_ms() - 200_000,
        expires_at=now_ms() - 100_000,
        pid=999_999,  # definitely dead
        host=os.uname().nodename,
    )
    path = claim_path("node:GHOST", root=tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(serialize_claim(expired))
    status = claim_status("node:GHOST", root=tmp_path)
    assert status["state"] == "stale"
    assert status["holder"] == "target-session:gone"


def test_suspect_claim_status_names_holder(tmp_path):
    # A TTL claim inside its window whose pid is dead reads SUSPECT (TTL-
    # protected, not stealable) and still names its holder.
    suspect = Claim(
        schema_version=1,
        key="node:SUS",
        holder="target-session:maybe",
        acquired_at=now_ms() - 1_000,
        expires_at=now_ms() + 60_000,  # not yet expired
        pid=999_999,  # dead
        host=os.uname().nodename,
    )
    path = claim_path("node:SUS", root=tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(serialize_claim(suspect))
    status = claim_status("node:SUS", root=tmp_path)
    assert status["state"] == "suspect"
    assert status["holder"] == "target-session:maybe"


def test_free_claim_carries_no_holder(tmp_path):
    # A node with no claim file reads free with no holder - the contrast that
    # makes a stale/suspect holder "loud".
    status = claim_status("node:FREE", root=tmp_path)
    assert status["state"] == "free"
    assert "holder" not in status


# ---------------------------------------------------------------------------
# renewal re-anchors a corpse instead of preserving it (x-05be)
# ---------------------------------------------------------------------------


def _dead_pid():
    dead = 999_999
    while psutil.pid_exists(dead):
        dead += 1
    return dead


def _anchor(monkeypatch, pid):
    monkeypatch.setattr("fno.claims.session_pid.resolve_session_pid", lambda **_kw: pid)


class TestRefreshReanchorsADeadPid:
    """The root cause of SUSPECT meaning two things.

    A respawned worker renewing under a new pid used to leave a claim
    byte-identical to a dead worker's: dead pid, unexpired TTL. Nothing on disk
    separated them, so every reader that must not steal from the live one was
    forced to protect the dead one.
    """

    def test_a_dead_anchor_is_replaced_by_the_durable_session_pid(
        self, tmp_path, monkeypatch
    ):
        acquire_claim(
            key="node:x-resp", holder="target-session:s", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )
        assert claim_status("node:x-resp", root=tmp_path)["state"] == "suspect"

        _anchor(monkeypatch, os.getpid())
        refreshed = refresh_claim(
            key="node:x-resp", holder="target-session:s", ttl_ms=3_600_000,
            root=tmp_path,
        )
        assert refreshed.pid == os.getpid()
        assert claim_status("node:x-resp", root=tmp_path)["state"] == "live"

    def test_the_anchor_is_held_while_the_pid_moves(self, tmp_path, monkeypatch):
        """acquired_at STAYS. The do provenance row keys started_at on it, so
        moving it makes the release stamp open a second row instead of closing
        the one this claim opened. Reuse detection still passes because the
        anchor process started BEFORE the claim, asserted here by classifying
        LIVE rather than by reading the field alone."""
        from fno.claims.core import claim_status

        original = acquire_claim(
            key="node:x-anchor", holder="target-session:s", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )
        assert claim_status("node:x-anchor", root=tmp_path)["state"] == "suspect"
        _anchor(monkeypatch, os.getpid())
        refreshed = refresh_claim(
            key="node:x-anchor", holder="target-session:s", ttl_ms=3_600_000,
            root=tmp_path,
        )
        assert refreshed.acquired_at == original.acquired_at
        assert refreshed.pid == os.getpid()
        assert claim_status("node:x-anchor", root=tmp_path)["state"] == "live"

    def test_a_re_anchoring_refresh_still_extends_the_deadline(
        self, tmp_path, monkeypatch
    ):
        """The deadline runs from NOW, never from the held acquire time. Tying
        it to `acquired_at` made this refresh extend the lease by zero, and on a
        short window it wrote a deadline in the PAST - the heartbeat driving its
        own claim from suspect to stale."""
        original = acquire_claim(
            key="node:x-deadline", holder="target-session:s", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )
        _anchor(monkeypatch, os.getpid())
        refreshed = refresh_claim(
            key="node:x-deadline", holder="target-session:s", ttl_ms=3_600_000,
            root=tmp_path,
        )
        assert refreshed.acquired_at == original.acquired_at
        assert refreshed.expires_at > original.expires_at, (
            "a re-anchoring refresh did not extend the lease"
        )
        assert refreshed.expires_at > now_ms()

    def test_a_live_anchor_is_never_rewritten(self, tmp_path, monkeypatch):
        """A healthy claim keeps the anchor it was acquired with, so a peer
        knowing the holder string cannot take over a running session."""
        original = acquire_claim(
            key="node:x-healthy", holder="target-session:s", ttl_ms=3_600_000,
            pid=os.getpid(), root=tmp_path,
        )
        _anchor(monkeypatch, 424242)
        refreshed = refresh_claim(
            key="node:x-healthy", holder="target-session:s", ttl_ms=3_600_000,
            root=tmp_path,
        )
        assert refreshed.pid == os.getpid()
        assert refreshed.acquired_at == original.acquired_at

    def test_no_harness_ancestor_leaves_the_anchor_alone(self, tmp_path, monkeypatch):
        """Plain-shell ancestry has no better anchor to write, and a transient
        renewer pid is a worse one. The deadline moves alone, as before."""
        dead = _dead_pid()
        original = acquire_claim(
            key="node:x-noharness", holder="target-session:s", ttl_ms=3_600_000,
            pid=dead, root=tmp_path,
        )
        _anchor(monkeypatch, None)
        refreshed = refresh_claim(
            key="node:x-noharness", holder="target-session:s", ttl_ms=3_600_000,
            root=tmp_path,
        )
        assert refreshed.pid == dead
        assert refreshed.acquired_at == original.acquired_at
        assert refreshed.expires_at > original.expires_at

    def test_an_off_machine_corpse_is_never_rewritten(self, tmp_path, monkeypatch):
        """We cannot read another box's pid table, so a dead-looking pid there
        is unverified and only the deadline may move."""
        original = acquire_claim(
            key="node:x-foreign", holder="target-session:s", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )
        monkeypatch.setattr("fno.claims.core.is_same_machine", lambda *_a: False)
        _anchor(monkeypatch, os.getpid())
        refreshed = refresh_claim(
            key="node:x-foreign", holder="target-session:s", ttl_ms=3_600_000,
            root=tmp_path,
        )
        assert refreshed.pid == original.pid
