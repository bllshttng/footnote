"""Unit tests for fno.claims.core: the six verbs.

Tests are organized by verb. Each verb exercises the design-doc ACs:
HP (happy path), ERR (error path), EDGE (edge case), FR (functional req).

Filesystem isolation: every test uses a tmp_path root via the ``root``
argument supported by every verb. Events emission goes to .fno/events.jsonl
which the typed-builders write best-effort; tests focus on lock-file state
and exceptions, not on event log content (the event types are covered by
the parity corpus and test_validator_parity.py).
"""
from __future__ import annotations

import os
import socket
from pathlib import Path
from unittest.mock import patch

import psutil
import pytest

from fno.claims.core import (
    ClaimAlreadyHeld,
    ClaimCorrupted,
    ClaimGoneAway,
    ClaimHeldByOther,
    ClaimValidationError,
    HolderMismatch,
    acquire_claim,
    claim_status,
    force_release_claim,
    list_claims,
    refresh_claim,
    release_claim,
)
from fno.claims.io import claim_path, claims_dir, serialize_claim
from fno.claims.types import Claim, ClaimState


HOLDER_A = "target-session:sid-a"
HOLDER_B = "target-session:sid-b"


# ---------------------------------------------------------------------------
# acquire
# ---------------------------------------------------------------------------


class TestAcquire:
    def test_AC1_HP_fresh_key(self, tmp_path):
        claim = acquire_claim("node:ab-1", HOLDER_A, root=tmp_path)
        assert claim.holder == HOLDER_A
        assert claim_path("node:ab-1", root=tmp_path).exists()

    def test_AC1_FR_pid_liveness_omits_expires_at(self, tmp_path):
        claim = acquire_claim("k", HOLDER_A, root=tmp_path)
        assert claim.expires_at is None
        text = claim_path("k", root=tmp_path).read_text()
        assert "expires_at" not in text

    def test_AC1_FR_ttl_sets_expires_at(self, tmp_path):
        claim = acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        assert claim.expires_at is not None
        assert claim.expires_at > claim.acquired_at

    def test_AC1_ERR_key_too_long_rejected(self, tmp_path):
        with pytest.raises(ClaimValidationError):
            acquire_claim("x" * 300, HOLDER_A, root=tmp_path)

    def test_AC1_ERR_ttl_below_min_rejected(self, tmp_path):
        with pytest.raises(ClaimValidationError):
            acquire_claim("k", HOLDER_A, ttl_ms=100, root=tmp_path)

    def test_AC1_ERR_ttl_above_max_rejected(self, tmp_path):
        with pytest.raises(ClaimValidationError):
            acquire_claim("k", HOLDER_A, ttl_ms=86_400_001, root=tmp_path)

    def test_AC1_ERR_empty_holder_rejected(self, tmp_path):
        with pytest.raises(ClaimValidationError):
            acquire_claim("k", "", root=tmp_path)

    def test_AC1_EDGE_live_other_raises(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        with pytest.raises(ClaimHeldByOther) as exc:
            acquire_claim("k", HOLDER_B, root=tmp_path)
        assert exc.value.holder == HOLDER_A
        assert exc.value.key == "k"

    def test_AC1_FR_idempotent_reacquire_same_holder(self, tmp_path):
        first = acquire_claim("k", HOLDER_A, root=tmp_path)
        # Same holder, second call must succeed (not raise).
        second = acquire_claim("k", HOLDER_A, root=tmp_path)
        assert second.holder == HOLDER_A
        # acquired_at is refreshed
        assert second.acquired_at >= first.acquired_at

    def test_AC4_EDGE_stale_pid_recovered(self, tmp_path):
        """A claim whose holder process is dead is reclaimable by another holder."""
        # Pick a definitely-dead PID and hand-write a claim for it.
        dead_pid = 999_999
        while psutil.pid_exists(dead_pid):
            dead_pid += 1
        from fno.claims.staleness import now_ms
        stale = Claim(
            key="k",
            holder=HOLDER_A,
            acquired_at=now_ms() - 100_000,
            expires_at=None,
            pid=dead_pid,
            host=socket.gethostname(),
        )
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialize_claim(stale))

        # New holder can take over.
        new = acquire_claim("k", HOLDER_B, root=tmp_path)
        assert new.holder == HOLDER_B
        # The old claim is archived.
        archive_dir = claims_dir(tmp_path) / ".expired"
        assert archive_dir.exists()
        assert any(archive_dir.iterdir())

    def test_AC1_FR_ttl_expired_recovered(self, tmp_path):
        """A TTL claim past expires_at whose pid is dead is reclaimable.

        The recorded pid must be dead: under the hybrid liveness arm an
        expired TTL claim whose pid is still ALIVE on this host stays LIVE
        and is NOT reclaimable (see test_hybrid_expired_live_pid_not_reclaimable)."""
        from fno.claims.staleness import now_ms
        dead_pid = 999_999
        while psutil.pid_exists(dead_pid):
            dead_pid += 1
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        expired = Claim(
            key="k",
            holder=HOLDER_A,
            acquired_at=now_ms() - 200_000,
            expires_at=now_ms() - 100_000,
            pid=dead_pid,
            host=socket.gethostname(),
        )
        path.write_text(serialize_claim(expired))

        new = acquire_claim("k", HOLDER_B, root=tmp_path)
        assert new.holder == HOLDER_B

    def test_hybrid_expired_live_pid_not_reclaimable(self, tmp_path):
        """HYBRID (codex P1): an expired TTL claim whose recorded pid is a live
        process on this host is NOT reclaimable - acquire must honor the same
        hybrid liveness as classify(), so a peer parks instead of stealing the
        node from a suspended-but-alive session (AC1-ERR)."""
        from fno.claims.staleness import now_ms
        # Anchor acquired_at AFTER this process's create_time so is_live's
        # pid-reuse guard (create_time < acquired_at) passes; both timestamps
        # are in the past so the TTL is expired.
        proc_create_ms = int(psutil.Process(os.getpid()).create_time() * 1000)
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        expired_live = Claim(
            key="k",
            holder=HOLDER_A,
            acquired_at=proc_create_ms + 1000,  # after this proc started -> live
            expires_at=proc_create_ms + 2000,   # in the past -> TTL lapsed
            pid=os.getpid(),                     # alive on this host
            host=socket.gethostname(),
        )
        path.write_text(serialize_claim(expired_live))

        with pytest.raises(ClaimHeldByOther) as exc:
            acquire_claim("k", HOLDER_B, root=tmp_path)
        assert exc.value.holder == HOLDER_A
        # The live claim was NOT archived.
        archive_dir = claims_dir(tmp_path) / ".expired"
        assert not (archive_dir.exists() and any(archive_dir.iterdir()))


# ---------------------------------------------------------------------------
# release
# ---------------------------------------------------------------------------


class TestRelease:
    def test_AC2_HP_release_removes_lock_file(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        release_claim("k", HOLDER_A, root=tmp_path)
        assert not claim_path("k", root=tmp_path).exists()

    def test_AC2_HP_release_missing_is_idempotent(self, tmp_path):
        # No claim filed; release must succeed.
        release_claim("k", HOLDER_A, root=tmp_path)

    def test_AC2_FR_release_silently_skips_other_holder(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        # Wrong holder: silent success (no exception).
        release_claim("k", HOLDER_B, root=tmp_path)
        assert claim_path("k", root=tmp_path).exists()

    def test_AC2_ERR_release_strict_raises_on_mismatch(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        with pytest.raises(HolderMismatch):
            release_claim("k", HOLDER_B, strict=True, root=tmp_path)

    def test_AC2_ERR_empty_key_rejected(self, tmp_path):
        with pytest.raises(ClaimValidationError):
            release_claim("", HOLDER_A, root=tmp_path)

    def test_AC2_FR_release_emits_duration(self, tmp_path):
        # Just verify the call doesn't raise; duration is best-effort visible in events.jsonl
        acquire_claim("k", HOLDER_A, root=tmp_path)
        release_claim("k", HOLDER_A, root=tmp_path)


# ---------------------------------------------------------------------------
# refresh
# ---------------------------------------------------------------------------


class TestRefresh:
    def test_AC3_HP_refresh_extends_expires_at(self, tmp_path):
        first = acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        # Sleep to ensure now_ms() advances.
        import time
        time.sleep(0.01)
        refreshed = refresh_claim("k", HOLDER_A, ttl_ms=120_000, root=tmp_path)
        assert refreshed is not None
        assert refreshed.expires_at > first.expires_at

    def test_AC3_FR_refresh_pid_liveness_returns_none(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)  # no TTL
        result = refresh_claim("k", HOLDER_A, root=tmp_path)
        assert result is None

    def test_AC3_ERR_refresh_missing_raises_gone_away(self, tmp_path):
        with pytest.raises(ClaimGoneAway):
            refresh_claim("k", HOLDER_A, root=tmp_path)

    def test_AC3_ERR_refresh_wrong_holder_raises(self, tmp_path):
        acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        with pytest.raises(HolderMismatch):
            refresh_claim("k", HOLDER_B, root=tmp_path)

    def test_AC3_ERR_refresh_ttl_out_of_range(self, tmp_path):
        acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        with pytest.raises(ClaimValidationError):
            refresh_claim("k", HOLDER_A, ttl_ms=10, root=tmp_path)


# ---------------------------------------------------------------------------
# status
# ---------------------------------------------------------------------------


class TestStatus:
    def test_AC4_HP_status_free(self, tmp_path):
        result = claim_status("k", root=tmp_path)
        assert result["state"] == ClaimState.FREE.value
        assert result["key"] == "k"

    def test_AC4_HP_status_live(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        result = claim_status("k", root=tmp_path)
        assert result["state"] == ClaimState.LIVE.value
        assert result["holder"] == HOLDER_A
        assert result["pid"] == os.getpid()

    def test_AC4_HP_status_corrupted(self, tmp_path):
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("not yaml at all: ::::")
        result = claim_status("k", root=tmp_path)
        assert result["state"] == ClaimState.CORRUPTED.value
        assert "error" in result


# ---------------------------------------------------------------------------
# list
# ---------------------------------------------------------------------------


class TestList:
    def test_AC5_HP_list_empty_when_no_claims(self, tmp_path):
        assert list_claims(root=tmp_path) == []

    def test_AC5_HP_list_returns_live_claims(self, tmp_path):
        acquire_claim("node:ab-1", HOLDER_A, root=tmp_path)
        acquire_claim("node:ab-2", HOLDER_A, root=tmp_path)
        results = list_claims(root=tmp_path)
        keys = sorted(r["key"] for r in results)
        assert keys == ["node:ab-1", "node:ab-2"]

    def test_AC5_FR_list_filters_by_prefix(self, tmp_path):
        acquire_claim("node:ab-1", HOLDER_A, root=tmp_path)
        acquire_claim("fleet:m1", HOLDER_A, root=tmp_path)
        results = list_claims(prefix="node:", root=tmp_path)
        assert [r["key"] for r in results] == ["node:ab-1"]

    def test_AC5_FR_list_excludes_stale_by_default(self, tmp_path):
        from fno.claims.staleness import now_ms
        # Write an expired TTL claim whose holder is DEAD. Under hybrid liveness
        # (ab-cc5553f2) an expired TTL claim with a still-LIVE on-host pid stays
        # LIVE, so pinning pid=os.getpid() here made the claim flip to LIVE once
        # this pytest process had run longer than the TTL window (create_time
        # then precedes acquired_at and is_live's pid-reuse guard passes) - a
        # latent flake that only fired late in a full-suite run. A definitely-
        # dead pid keeps the claim unambiguously STALE (mirrors the dead-pid
        # pattern the sibling stale-claim tests already use).
        dead_pid = 999_999
        while psutil.pid_exists(dead_pid):
            dead_pid += 1
        path = claim_path("expired", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialize_claim(Claim(
            key="expired",
            holder=HOLDER_A,
            acquired_at=now_ms() - 200_000,
            expires_at=now_ms() - 100_000,
            pid=dead_pid,
            host=socket.gethostname(),
        )))
        # Default: stale excluded
        assert list_claims(root=tmp_path) == []
        # include_stale=True surfaces it
        all_results = list_claims(include_stale=True, root=tmp_path)
        assert any(r["key"] == "expired" for r in all_results)


# ---------------------------------------------------------------------------
# force-release
# ---------------------------------------------------------------------------


class TestForceRelease:
    def test_AC6_HP_force_release_removes_live_claim(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        force_release_claim("k", reason="operator override", root=tmp_path)
        assert not claim_path("k", root=tmp_path).exists()

    def test_AC6_HP_force_release_missing_succeeds(self, tmp_path):
        force_release_claim("k", reason="cleanup", root=tmp_path)

    def test_AC6_ERR_empty_reason_rejected(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        with pytest.raises(ClaimValidationError):
            force_release_claim("k", reason="", root=tmp_path)

    def test_AC6_FR_archives_to_expired_dir(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        force_release_claim("k", reason="cleanup", root=tmp_path)
        archive = claims_dir(tmp_path) / ".expired"
        assert archive.exists()
        assert any(archive.iterdir())
