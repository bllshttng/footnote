"""Unit tests for lane-slot claims (parallel-mode cap primitive, x-d050).

Covers the single-process behavior of the atomic concurrency cap:
acquisition/release, the derived count, lane-level idempotency, and the
degenerate max_lanes cases. The real cross-process cap race lives in
tests/integration/test_lane_slots_concurrency.py.
"""
from __future__ import annotations

import pytest

from fno.claims import ClaimValidationError
from fno.claims.lanes import (
    LANE_SLOT_PREFIX,
    acquire_lane_slot,
    active_lane_count,
    find_lane_slot,
    release_lane_slot,
)


def test_acquire_returns_slot_and_count_reflects_it(tmp_path):
    claim = acquire_lane_slot(max_lanes=3, lane_id="node-a", root=tmp_path)
    assert claim is not None
    assert claim.key.startswith(LANE_SLOT_PREFIX)
    assert active_lane_count(root=tmp_path) == 1


def test_cap_enforced_distinct_lanes(tmp_path):
    """max_lanes=2: the third distinct lane cannot acquire; count stays 2."""
    a = acquire_lane_slot(max_lanes=2, lane_id="node-a", root=tmp_path)
    b = acquire_lane_slot(max_lanes=2, lane_id="node-b", root=tmp_path)
    c = acquire_lane_slot(max_lanes=2, lane_id="node-c", root=tmp_path)
    assert a is not None and b is not None
    assert a.key != b.key, "distinct lanes must occupy distinct slots"
    assert c is None, "cap full: third lane must be refused"
    assert active_lane_count(root=tmp_path) == 2


def test_max_lanes_one_is_sequential(tmp_path):
    """max_lanes=1 degrades to a single-slot lock (today's sequential path)."""
    a = acquire_lane_slot(max_lanes=1, lane_id="node-a", root=tmp_path)
    b = acquire_lane_slot(max_lanes=1, lane_id="node-b", root=tmp_path)
    assert a is not None
    assert b is None
    assert active_lane_count(root=tmp_path) == 1


def test_release_frees_a_slot(tmp_path):
    acquire_lane_slot(max_lanes=1, lane_id="node-a", root=tmp_path)
    assert acquire_lane_slot(max_lanes=1, lane_id="node-b", root=tmp_path) is None
    release_lane_slot(lane_id="node-a", root=tmp_path)
    assert active_lane_count(root=tmp_path) == 0
    # Slot is now free for the next lane.
    b = acquire_lane_slot(max_lanes=1, lane_id="node-b", root=tmp_path)
    assert b is not None
    assert active_lane_count(root=tmp_path) == 1


def test_same_lane_reacquire_is_idempotent(tmp_path):
    """Re-acquiring the same lane returns its own slot; count does not grow."""
    first = acquire_lane_slot(max_lanes=3, lane_id="node-a", root=tmp_path)
    second = acquire_lane_slot(max_lanes=3, lane_id="node-a", root=tmp_path)
    assert first is not None and second is not None
    assert first.key == second.key
    assert active_lane_count(root=tmp_path) == 1


def test_reacquire_reuses_own_slot_after_earlier_slot_freed(tmp_path):
    """A lane holding a higher slot must not grab a freed lower slot (no cap inflation).

    node-a -> slot0, node-b -> slot1. Release node-a (slot0 frees). node-b
    re-dispatches: it must reuse slot1, NOT grab the now-free slot0, or it would
    hold two slots and inflate the cap.
    """
    a = acquire_lane_slot(max_lanes=2, lane_id="node-a", root=tmp_path)
    b = acquire_lane_slot(max_lanes=2, lane_id="node-b", root=tmp_path)
    assert a is not None and b is not None
    b_slot = b.key

    release_lane_slot(lane_id="node-a", root=tmp_path)
    assert active_lane_count(root=tmp_path) == 1

    b_again = acquire_lane_slot(max_lanes=2, lane_id="node-b", root=tmp_path)
    assert b_again is not None
    assert b_again.key == b_slot, "lane must reuse its own slot, not a freed one"
    assert active_lane_count(root=tmp_path) == 1


def test_find_lane_slot(tmp_path):
    assert find_lane_slot("node-a", root=tmp_path) is None
    claim = acquire_lane_slot(max_lanes=2, lane_id="node-a", root=tmp_path)
    assert find_lane_slot("node-a", root=tmp_path) == claim.key
    assert find_lane_slot("node-b", root=tmp_path) is None


def test_release_unheld_lane_is_noop(tmp_path):
    release_lane_slot(lane_id="never-acquired", root=tmp_path)  # must not raise
    assert active_lane_count(root=tmp_path) == 0


@pytest.mark.parametrize("bad", [0, -1, -5])
def test_max_lanes_below_one_rejected(tmp_path, bad):
    with pytest.raises(ClaimValidationError):
        acquire_lane_slot(max_lanes=bad, lane_id="node-a", root=tmp_path)


def test_empty_lane_id_rejected(tmp_path):
    with pytest.raises(ClaimValidationError):
        acquire_lane_slot(max_lanes=2, lane_id="", root=tmp_path)


def test_ttl_none_coerces_to_default_not_pid_liveness(tmp_path):
    """A lane slot is always TTL-anchored: ttl_ms=None must NOT create a
    PID-liveness claim (which would die with the transient acquirer and defeat
    the cap). expires_at must be set to the lane default window."""
    from fno.claims.lanes import DEFAULT_LANE_TTL_MS

    claim = acquire_lane_slot(max_lanes=2, lane_id="node-a", ttl_ms=None, root=tmp_path)
    assert claim is not None
    assert claim.expires_at is not None, "ttl_ms=None must not yield a PID-liveness slot"
    # Window is roughly the lane default (acquired_at + DEFAULT_LANE_TTL_MS).
    assert claim.expires_at - claim.acquired_at == DEFAULT_LANE_TTL_MS


def test_count_ignores_other_claim_kinds(tmp_path):
    """active_lane_count filters by the lane-slot prefix, not all claims."""
    from fno.claims import acquire_claim

    acquire_claim(key="node:ab-1234", holder="target-session:x", root=tmp_path)
    acquire_claim(key="walker:/some/root", holder="walker:x", root=tmp_path)
    acquire_lane_slot(max_lanes=2, lane_id="node-a", root=tmp_path)
    assert active_lane_count(root=tmp_path) == 1


# ---------------------------------------------------------------------------
# CLI surface (fno claim lane-acquire / lane-release / lane-count)
# ---------------------------------------------------------------------------

import json  # noqa: E402

from typer.testing import CliRunner  # noqa: E402

from fno.claims.cli import cli  # noqa: E402

_runner = CliRunner()


@pytest.fixture
def cli_claims_root(tmp_path, monkeypatch):
    """Route root=None lane keys to a tmp claims dir (FNO_CLAIMS_ROOT wins)."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    return tmp_path


def test_cli_acquire_cap_release_flow(cli_claims_root):
    # Two acquisitions fit under cap 2.
    r = _runner.invoke(cli, ["lane-acquire", "--lane-id", "node-a", "--max-lanes", "2"])
    assert r.exit_code == 0, r.output
    r = _runner.invoke(cli, ["lane-acquire", "--lane-id", "node-b", "--max-lanes", "2"])
    assert r.exit_code == 0, r.output

    # Count reflects two live lanes.
    r = _runner.invoke(cli, ["lane-count", "--json"])
    assert r.exit_code == 0
    assert json.loads(r.output)["active_lanes"] == 2

    # Third acquisition is capped -> exit 1.
    r = _runner.invoke(cli, ["lane-acquire", "--lane-id", "node-c", "--max-lanes", "2"])
    assert r.exit_code == 1, r.output

    # Release frees a slot; the third now fits.
    r = _runner.invoke(cli, ["lane-release", "--lane-id", "node-a"])
    assert r.exit_code == 0
    r = _runner.invoke(cli, ["lane-acquire", "--lane-id", "node-c", "--max-lanes", "2", "--json"])
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["lane_id"] == "node-c"


def test_cli_acquire_rejects_bad_max_lanes(cli_claims_root):
    r = _runner.invoke(cli, ["lane-acquire", "--lane-id", "node-a", "--max-lanes", "0"])
    assert r.exit_code == 2, r.output
