"""TTL renewal + loud-expiry semantics (x-a7ab 1.4).

A ticking worker refreshes its node claim so a long-running loop never silently
expires its TTL and frees the node for a twin. When a worker stops ticking, an
expired claim must still surface its prior holder - a dispatcher reading the
claim never sees a silently-free node.
"""
import os

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
