"""Admission owner: pure verdicts, atomic reservations, pool grouping (x-1afa).

The receipt is the positive marker every test asserts on: an admitted or
refused STATUS pinned to a persisted reservation row, never an exit code.
The Python side is a thin call into the Rust owner, so these tests run the
real binary (pinned by ``_pin_rust_owner``); mutate modes drive the verb
directly, exactly as a caller would.
"""

from __future__ import annotations

import concurrent.futures
import json
import os
from pathlib import Path

import pytest

from fno.adapters.providers import runtime_state as rs
from fno.adapters.providers.model import ProviderRecord
from fno.adapters.providers.usage import UsageSnapshot, UsageWindow
from fno.config.routing_blocks import RoutingAdmissionBlock

TEN_MIN = 600.0

_REPO_ROOT = Path(__file__).resolve().parents[3]


@pytest.fixture(autouse=True)
def _pin_rust_owner(monkeypatch):
    """The admission owner is the Rust verb; pin THIS checkout's build so a
    stale installed binary cannot answer the suite with the wrong math."""
    if not os.environ.get("FNO_AGENTS_BIN"):
        candidate = _REPO_ROOT / "crates" / "fno-agents" / "target" / "debug" / "fno-agents"
        if candidate.is_file():
            monkeypatch.setenv("FNO_AGENTS_BIN", str(candidate))


def _policy(**kw) -> RoutingAdmissionBlock:
    base = dict(
        enabled=True,
        max_inflight_per_pool=3,
        reservation_ttl_seconds=TEN_MIN,
        demand_pct={"do": {"high": 15}},
        reserve_pct={"do": {"high": 10}},
    )
    base.update(kw)
    return RoutingAdmissionBlock(**base)


def _mutate(mode, rid, dispatch_id, state_path, *, session_id=None, now=None, ttl=None):
    """Drive a mutate mode through the verb, the way a caller would."""
    from fno.rust_binary import verb_call

    payload = {
        "mode": mode,
        "reservation_id": rid,
        "dispatch_id": dispatch_id,
        "session_id": session_id,
        "ttl_seconds": ttl,
        "now": now,
        "state_path": str(state_path),
        "policy": {"enabled": False, "max_inflight_per_pool": 1, "reservation_ttl_seconds": 900},
    }
    return bool(verb_call("admission", payload).get("ok"))


def _record(record_id: str, pool: str | None = None) -> ProviderRecord:
    return ProviderRecord(
        id=record_id,
        name=record_id,
        harness="claude",
        auth="api_key",
        env={"ANTHROPIC_API_KEY": "sk-test"},
        quota_pool=pool,
    )


def _snap(
    record_id: str,
    used_pct: float,
    *,
    resets_in: float | None = TEN_MIN,
    probed_ago: float = 0.0,
    extra_windows: tuple[UsageWindow, ...] = (),
    partial: bool = False,
) -> UsageSnapshot:
    # ``resets_in`` is relative to NOW (epoch 1000.0): a window resets in the
    # future unless the test explicitly passes None (the reset-less x-763a
    # case) or a negative (already reset).
    resets_at = None if resets_in is None else 1000.0 + resets_in
    windows = (UsageWindow(label="5h", used_pct=used_pct, resets_at=resets_at),) + extra_windows
    return UsageSnapshot(
        provider_id=record_id,
        windows=windows,
        probed_at=1000.0 - probed_ago,
        source="test",
        partial=partial,
    )


@pytest.fixture()
def state_file(tmp_path, monkeypatch):
    path = tmp_path / "runtime-state.json"
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(path))
    return path


def _seed(state_file: Path, snap: UsageSnapshot) -> None:
    assert rs.write_usage_snapshot(snap, now=1000.0)


NOW = 1000.0


def test_ac2_hp_reserve_persists_and_names_window_and_headroom(state_file):
    _seed(state_file, _snap("rec-a", 60.0))  # 40 remaining
    rec = _record("rec-a")
    first = rs.reserve_admission(
        rec,
        dispatch_id="d1",
        demand_pct=10.0,
        policy=_policy(),
        now=NOW,
    )
    assert first["status"] == "admitted"
    second = rs.reserve_admission(
        rec,
        dispatch_id="d2",
        demand_pct=15.0,
        policy=_policy(),
        now=NOW,
    )
    assert second["status"] == "admitted"
    assert second["binding_window"] == "rec-a/5h"
    assert second["remaining_admission_pct"] == 5.0
    on_disk = json.loads(state_file.read_text())["reservations"]
    assert len(on_disk) == 2
    assert on_disk[second["reservation_id"]]["demand_pct"] == 15.0
    assert on_disk[second["reservation_id"]]["pool"] == "api:claude/rec-a"


def test_ac2_race_capacity_for_one_yields_one_admit_and_one_refusal(state_file):
    _seed(state_file, _snap("rec-a", 60.0))  # 40 remaining, demand 25 each
    rec = _record("rec-a")
    policy = _policy()

    def _enter(i: int) -> dict:
        return rs.reserve_admission(
            rec,
            dispatch_id=f"race-{i}",
            demand_pct=25.0,
            policy=policy,
            now=NOW,
        )

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(_enter, range(2)))
    admitted = [r for r in results if r["status"] == "admitted"]
    refused = [r for r in results if r["status"] == rs.RESERVED_CAPACITY]
    assert len(admitted) == 1
    assert len(refused) == 1
    on_disk = json.loads(state_file.read_text())["reservations"]
    assert len(on_disk) == 1


def test_ac2_edge_exhausted_window_wins_and_reserve_consumption_never_bypasses(state_file):
    _seed(
        state_file,
        _snap(
            "rec-a",
            100.0,
            extra_windows=(UsageWindow(label="weekly", used_pct=0.0, resets_at=NOW + 1),),
        ),
    )
    rec = _record("rec-a")
    verdict = rs.reserve_admission(
        rec,
        dispatch_id="d1",
        demand_pct=1.0,
        consume_reserve=True,
        policy=_policy(),
        now=NOW,
    )
    assert verdict["status"] == rs.EXHAUSTED
    assert verdict["binding_window"] == "rec-a/5h"
    assert json.loads(state_file.read_text()).get("reservations", {}) == {}


def test_ac2_edge_stale_evidence_refuses_and_never_persists(state_file):
    _seed(state_file, _snap("rec-a", 0.0, probed_ago=99999.0))
    rec = _record("rec-a")
    verdict = rs.reserve_admission(
        rec,
        dispatch_id="d1",
        demand_pct=5.0,
        policy=_policy(),
        now=NOW,
    )
    assert verdict["status"] == rs.STALE_OBSERVATION
    assert json.loads(state_file.read_text()).get("reservations", {}) == {}


def test_stale_reads_through_the_quota_ttl_not_the_policy(state_file, monkeypatch):
    """The freshness word comes from the probe TTL the caller pins."""
    _seed(state_file, _snap("rec-a", 0.0, probed_ago=200.0))
    rec = _record("rec-a")
    fresh = rs.preview_admission(
        rec,
        demand_pct=5.0,
        policy=_policy(),
        ttl_seconds=300.0,
        now=NOW,
    )
    stale = rs.preview_admission(
        rec,
        demand_pct=5.0,
        policy=_policy(),
        ttl_seconds=100.0,
        now=NOW,
    )
    assert fresh["status"] == "admitted"
    assert stale["status"] == rs.STALE_OBSERVATION


def test_partial_observation_is_stale_never_headroom(state_file):
    _seed(state_file, _snap("rec-a", 0.0, partial=True))
    verdict = rs.preview_admission(
        _record("rec-a"),
        demand_pct=5.0,
        policy=_policy(),
        now=NOW,
    )
    assert verdict["status"] == rs.STALE_OBSERVATION


def test_same_dispatch_is_idempotent_but_two_dispatches_never_share(state_file):
    _seed(state_file, _snap("rec-a", 60.0))
    rec = _record("rec-a")
    policy = _policy()
    a = rs.reserve_admission(rec, dispatch_id="d1", demand_pct=10.0, policy=policy, now=NOW)
    b = rs.reserve_admission(rec, dispatch_id="d1", demand_pct=10.0, policy=policy, now=NOW)
    assert a["status"] == b["status"] == "admitted" and a["reservation_id"] == b["reservation_id"]
    on_disk = json.loads(state_file.read_text())["reservations"]
    assert len(on_disk) == 1
    with pytest.raises(ValueError):
        rs.reserve_admission(rec, dispatch_id="  ", demand_pct=1.0, policy=policy, now=NOW)


def test_declared_pool_groups_aliases_undeclared_stays_separate(state_file):
    _seed(state_file, _snap("rec-a", 60.0))
    _seed(state_file, _snap("rec-b", 60.0))
    family_a = _record("rec-a", pool="family")
    family_b = _record("rec-b", pool="family")
    solo = _record("rec-c")
    _seed(state_file, _snap("rec-c", 60.0))
    first = rs.reserve_admission(
        family_a,
        dispatch_id="d1",
        demand_pct=10.0,
        policy=_policy(),
        now=NOW,
    )
    assert first["status"] == "admitted" and first["pool"] == "declared:family"
    # The alias shares the pot: 40 remaining - 10 outstanding - 25 demanded < 10 reserve.
    alias = rs.reserve_admission(
        family_b,
        dispatch_id="d2",
        demand_pct=25.0,
        policy=_policy(),
        now=NOW,
    )
    assert alias["status"] == rs.RESERVED_CAPACITY
    # An undeclared record is its own pool and is untouched by the alias's hold.
    independent = rs.reserve_admission(
        solo,
        dispatch_id="d3",
        demand_pct=25.0,
        policy=_policy(),
        now=NOW,
    )
    assert independent["status"] == "admitted" and independent["pool"] == "api:claude/rec-c"


def test_inflight_cap_refuses_when_pool_is_full(state_file):
    _seed(state_file, _snap("rec-a", 0.0))
    rec = _record("rec-a")
    policy = _policy(max_inflight_per_pool=2, demand_pct={}, reserve_pct={})
    assert (
        rs.reserve_admission(rec, dispatch_id="d1", demand_pct=1.0, policy=policy, now=NOW)[
            "status"
        ]
        == "admitted"
    )
    assert (
        rs.reserve_admission(rec, dispatch_id="d2", demand_pct=1.0, policy=policy, now=NOW)[
            "status"
        ]
        == "admitted"
    )
    third = rs.reserve_admission(rec, dispatch_id="d3", demand_pct=1.0, policy=policy, now=NOW)
    assert third["status"] == rs.INFLIGHT_CAP


def test_release_only_its_own_and_commit_stamps_the_session(state_file):
    _seed(state_file, _snap("rec-a", 0.0))
    rec = _record("rec-a")
    policy = _policy(max_inflight_per_pool=1, demand_pct={}, reserve_pct={})
    kept = rs.reserve_admission(rec, dispatch_id="keep", demand_pct=1.0, policy=policy, now=NOW)
    gone = rs.reserve_admission(rec, dispatch_id="gone", demand_pct=1.0, policy=policy, now=NOW)
    assert gone["status"] == rs.INFLIGHT_CAP
    # Another dispatch cannot release a token it does not hold.
    assert _mutate("release", kept["reservation_id"], "gone", state_file, now=NOW) is False
    assert json.loads(state_file.read_text())["reservations"]
    assert _mutate("release", kept["reservation_id"], "keep", state_file, now=NOW) is True
    assert json.loads(state_file.read_text())["reservations"] == {}
    # A committed reservation carries the real session id.
    held = rs.reserve_admission(rec, dispatch_id="live", demand_pct=1.0, policy=policy, now=NOW)
    assert (
        _mutate(
            "commit",
            held["reservation_id"],
            "live",
            state_file,
            session_id="sess-1",
            now=NOW,
        )
        is True
    )
    row = json.loads(state_file.read_text())["reservations"][held["reservation_id"]]
    assert row["state"] == "committed"
    assert row["session_id"] == "sess-1"


def test_expired_reservation_stops_counting_without_a_write(state_file):
    _seed(state_file, _snap("rec-a", 0.0))
    rec = _record("rec-a")
    policy = _policy(
        max_inflight_per_pool=1, demand_pct={}, reserve_pct={}, reservation_ttl_seconds=50
    )
    first = rs.reserve_admission(rec, dispatch_id="d1", demand_pct=1.0, policy=policy, now=NOW)
    assert first["status"] == "admitted"
    later = NOW + 100.0
    second = rs.reserve_admission(rec, dispatch_id="d2", demand_pct=1.0, policy=policy, now=later)
    assert second["status"] == "admitted"
    on_disk = json.loads(state_file.read_text())["reservations"]
    assert set(on_disk) == {second["reservation_id"]}


def test_reservation_survives_an_unrelated_write(state_file):
    _seed(state_file, _snap("rec-a", 0.0))
    rec = _record("rec-a")
    held = rs.reserve_admission(
        rec,
        dispatch_id="d1",
        demand_pct=5.0,
        policy=_policy(),
        now=NOW,
    )
    assert held["status"] == "admitted"
    assert rs.write_usage_snapshot(_snap("other", 10.0), now=NOW)
    on_disk = json.loads(state_file.read_text())["reservations"]
    assert held["reservation_id"] in on_disk
    # The usage write must not have eaten the rec-a snapshot either.
    assert "rec-a" in json.loads(state_file.read_text())["usage"]


def test_unknown_identity_refuses_without_touching_state(state_file, monkeypatch):
    from fno.adapters.providers import binding

    monkeypatch.setattr(
        binding,
        "resolve_account_binding",
        lambda *a, **kw: binding.AccountBinding(
            "claude", binding.UNKNOWN, reason="credential-unreadable"
        ),
    )
    rec = ProviderRecord(id="managed", name="managed", harness="claude", auth="managed")
    verdict = rs.preview_admission(rec, demand_pct=5.0, policy=_policy(), now=NOW)
    assert verdict["status"] == rs.UNKNOWN_IDENTITY
    assert verdict["reason"] == "credential-unreadable" or "identity" in (verdict["reason"] or "")


def test_disabled_policy_never_reserves(state_file):
    _seed(state_file, _snap("rec-a", 0.0))
    verdict = rs.reserve_admission(
        _record("rec-a"),
        dispatch_id="d1",
        demand_pct=5.0,
        policy=RoutingAdmissionBlock(enabled=False),
        now=NOW,
    )
    assert verdict["status"] == rs.STALE_OBSERVATION
    assert "not enabled" in (verdict["reason"] or "")


def test_armed_but_tainted_policy_refuses_with_the_exact_config_error(state_file):
    _seed(state_file, _snap("rec-a", 60.0))
    policy = _policy(reserve_pct={"review": {"default": 150.0}})
    verdict = rs.preview_admission(
        _record("rec-a"),
        demand_pct=5.0,
        policy=policy,
        now=NOW,
    )
    assert verdict["status"] == rs.INVALID_POLICY
    assert "150.0" in (verdict["reason"] or "")
    assert "routing.admission.reserve_pct.review.default" in (verdict["reason"] or "")


def test_armed_currency_amount_refuses_with_the_unit_error(state_file):
    _seed(state_file, _snap("rec-a", 60.0))
    policy = _policy(demand_pct={"do": {"high": "$5"}})
    verdict = rs.preview_admission(
        _record("rec-a"),
        demand_pct=5.0,
        policy=policy,
        now=NOW,
    )
    assert verdict["status"] == rs.INVALID_POLICY
    reason = verdict["reason"] or ""
    assert "currency amount" in reason
    assert "routing.admission.demand_pct.do.high" in reason
    assert "unsupported" in reason


def test_armed_unknown_field_refuses_with_known_vocabulary(state_file):
    _seed(state_file, _snap("rec-a", 60.0))
    policy = RoutingAdmissionBlock.model_validate(
        {"enabled": True, "demand_pct": {}, "reserve_pct": {}, "no_such_knob": True}
    )
    verdict = rs.preview_admission(
        _record("rec-a"),
        demand_pct=5.0,
        policy=policy,
        now=NOW,
    )
    assert verdict["status"] == rs.INVALID_POLICY
    reason = verdict["reason"] or ""
    assert "unknown field(s) no_such_knob" in reason
    assert "demand_pct" in reason


def test_armed_unknown_difficulty_refuses(state_file):
    _seed(state_file, _snap("rec-a", 60.0))
    policy = _policy(demand_pct={"do": {"impossible": 10}})
    verdict = rs.preview_admission(
        _record("rec-a"),
        demand_pct=5.0,
        policy=policy,
        now=NOW,
    )
    assert verdict["status"] == rs.INVALID_POLICY
    assert "unknown difficulty 'impossible'" in (verdict["reason"] or "")


def test_armed_bad_bounds_refuse(state_file):
    _seed(state_file, _snap("rec-a", 60.0))
    policy = _policy(max_inflight_per_pool=0, reservation_ttl_seconds=-5)
    verdict = rs.preview_admission(
        _record("rec-a"),
        demand_pct=5.0,
        policy=policy,
        now=NOW,
    )
    assert verdict["status"] == rs.INVALID_POLICY
    reason = verdict["reason"] or ""
    assert "max_inflight_per_pool: 0 is not a positive concurrency bound" in reason
    assert "reservation_ttl_seconds: -5 is not a positive TTL" in reason


def test_preview_receipt_carries_the_pool_load(state_file):
    """The owner reports the pool's outstanding percent and inflight count,
    so no display re-reads the state document to render a row."""
    _seed(state_file, _snap("rec-a", 60.0))
    rec = _record("rec-a")
    policy = _policy()
    held = rs.reserve_admission(rec, dispatch_id="d1", demand_pct=10.0, policy=policy, now=NOW)
    assert held["status"] == "admitted"
    verdict = rs.preview_admission(rec, demand_pct=5.0, policy=policy, now=NOW)
    assert verdict["outstanding_pct"] == 10.0
    assert verdict["inflight"] == 1
