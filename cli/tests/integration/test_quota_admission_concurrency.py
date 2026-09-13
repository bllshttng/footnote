"""Real-process contention, canonical-account revalidation, preview purity
for shared-account admission (x-1afa).

The central claim: the reserve and the check are ONE decision under the
runtime-state file lock, so N concurrent dispatchers can never reserve more
than the windows cover - a racy count-then-reserve would let every caller
spend the same remaining allowance. The race below drives real processes at
one filesystem path, the pattern test_lane_slots_concurrency.py established.

No production quota mutation and no real login is a precondition: runtime
state is pinned by env, the provider table and the binding proof are
monkeypatched, and the usage snapshots are written fixtures.
"""
from __future__ import annotations

import json
import multiprocessing as mp
import os
import time
from pathlib import Path

import pytest

from fno.adapters.providers import runtime_state as rs
from fno.adapters.providers.model import ProviderRecord
from fno.adapters.providers.usage import UsageSnapshot, UsageWindow
from fno.config._routing_admission import AdmissionPolicy

NOW = time.time()


def _record(record_id: str = "rec-a") -> ProviderRecord:
    return ProviderRecord(
        id=record_id,
        name=record_id,
        harness="claude",
        auth="api_key",
        env={"ANTHROPIC_API_KEY": "sk-test"},
    )


def _seed(state_path: Path, used_pct: float = 60.0, record_id: str = "rec-a") -> None:
    snap = UsageSnapshot(
        provider_id=record_id,
        windows=(UsageWindow(label="5h", used_pct=used_pct, resets_at=NOW + 600.0),),
        probed_at=NOW,
        source="test",
    )
    assert rs.write_usage_snapshot(snap, now=NOW)


def _child_reserve(state_path_str: str, dispatch_id: str, result_queue) -> None:
    """Child worker: reserve with an armed policy against the pinned file.

    The policy patch is the child-side arm: config discovery in a spawned
    child would read the operator's real (disabled) settings, and the race
    must exercise the same armed path every dispatch would run.
    """
    try:
        os.environ["FNO_RUNTIME_STATE_PATH"] = state_path_str
        sys_path = str(Path(__file__).resolve().parents[2] / "src")
        import sys

        if sys_path not in sys.path:
            sys.path.insert(0, sys_path)
        from fno.adapters.providers import admission
        from fno.adapters.providers.model import ProviderRecord
        from fno.config._routing_admission import AdmissionPolicy

        policy = AdmissionPolicy(
            enabled=True,
            max_inflight_per_pool=8,
            reservation_ttl_seconds=900.0,
            demand_pct={"do": {"high": 10}},
            reserve_pct={"do": {"high": 10}},
        )
        admission.resolve_admission_policy = lambda: policy
        record = ProviderRecord(
            id="rec-a", name="rec-a", harness="claude", auth="api_key",
            env={"ANTHROPIC_API_KEY": "sk-test"},
        )
        receipt = admission.reserve_admission(
            record, dispatch_id=dispatch_id, verb="do", difficulty="high",
            policy=policy,
        )
        result_queue.put((receipt.status, receipt.reservation_id))
    except Exception as exc:  # pragma: no cover - surfaced as a failure
        result_queue.put((f"error: {exc!r}", None))


def test_concurrent_reserves_admit_only_what_the_window_covers(tmp_path, monkeypatch):
    """40% remains, 10% reserve, 10% demand each: exactly 3 of 6 may win.

    Winners are mutually exclusive by the file lock: a fourth admit would
    spend demand that was already promised to a live worker.
    """
    state_path = tmp_path / "runtime-state.json"
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(state_path))
    _seed(state_path, used_pct=60.0)

    ctx = mp.get_context("spawn")
    queue = ctx.Queue()
    procs = [
        ctx.Process(
            target=_child_reserve,
            args=(str(state_path), f"race-{i}", queue),
        )
        for i in range(6)
    ]
    for p in procs:
        p.start()
    outcomes: list[tuple] = []
    deadline = time.monotonic() + 30.0
    while len(outcomes) < 6 and time.monotonic() < deadline:
        try:
            outcomes.append(queue.get(timeout=0.5))
        except Exception:
            continue
    for p in procs:
        p.join(timeout=15)

    admitted = [rid for status, rid in outcomes if status == "admitted"]
    refused = [status for status, rid in outcomes if status == "reserved_capacity"]
    assert len(admitted) == 3, outcomes
    assert len(refused) == 3, outcomes
    # The positive marker: the file itself holds exactly the winners' rows,
    # with distinct ids (two dispatches never share a token).
    on_disk = json.loads(state_path.read_text())["reservations"]
    assert len(on_disk) == 3
    assert set(on_disk) == set(admitted)


def test_canonical_account_change_recomputes_pool_at_launch(tmp_path, monkeypatch):
    """AC4-HP: the principal proven at LAUNCH owns the reservation.

    Selection previewed under one canonical account; the operator switches
    the canonical slot; the launch re-derives the pool from the newly proven
    principal and attributes the reservation there. The earlier reservation
    stays attributed to the pool it actually used. Admission never switches
    accounts itself: it only reads the proof.
    """
    import json

    import fno.adapters.providers.binding as binding_mod
    from fno.adapters.providers import admission

    state_path = tmp_path / "runtime-state.json"
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(state_path))
    _seed(state_path, record_id="managed")

    def _binding_for(principal: str):
        def _resolve(record, **kw):
            return binding_mod.AccountBinding(
                "claude",
                binding_mod.MATCHED,
                requested_record=record.id,
                observed_principal=principal,
                observed_label=f"{principal}@example.test",
                matched_record=record.id,
            )

        return _resolve

    monkeypatch.setattr(
        binding_mod, "resolve_account_binding", _binding_for("makers")
    )
    managed = ProviderRecord(id="managed", name="managed", harness="claude", auth="managed")
    policy = AdmissionPolicy(enabled=True, demand_pct={}, reserve_pct={})
    first = admission.reserve_admission(
        managed, dispatch_id="spawn:sel", demand_pct=10.0, policy=policy, now=NOW,
    )
    assert first.admitted
    assert first.pool == "principal:makers"

    # The operator manually signs the canonical slot into another account.
    monkeypatch.setattr(
        binding_mod, "resolve_account_binding", _binding_for("readyrule")
    )
    second = admission.reserve_admission(
        managed, dispatch_id="spawn:launch", demand_pct=10.0, policy=policy, now=NOW,
    )
    assert second.admitted
    assert second.pool == "principal:readyrule"
    on_disk = json.loads(state_path.read_text())["reservations"]
    assert on_disk[first.reservation_id]["pool"] == "principal:makers"
    assert on_disk[second.reservation_id]["pool"] == "principal:readyrule"

    # An unprovable principal refuses rather than guessing a pool.
    def _unknown(record, **kw):
        return binding_mod.AccountBinding(
            "claude", binding_mod.UNKNOWN, reason="credential-unreadable"
        )

    monkeypatch.setattr(binding_mod, "resolve_account_binding", _unknown)
    from fno.adapters.providers.admission import UNKNOWN_IDENTITY

    refused = admission.reserve_admission(
        managed, dispatch_id="spawn:third", demand_pct=1.0, policy=policy, now=NOW,
    )
    assert refused.status == UNKNOWN_IDENTITY
    assert "credential-unreadable" in (refused.reason or "")


def test_repeated_previews_leave_the_state_byte_identical(tmp_path, monkeypatch):
    """AC4-EDGE: previews render reservations without consuming or touching
    them, and every receipt carries its units and evidence age."""
    from fno.adapters.providers import admission

    state_path = tmp_path / "runtime-state.json"
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(state_path))
    _seed(state_path)
    policy = AdmissionPolicy(enabled=True, demand_pct={"do": {"high": 15}}, reserve_pct={"do": {"high": 10}})
    record = _record()
    held = admission.reserve_admission(
        record, dispatch_id="spawn:live", demand_pct=10.0, policy=policy, now=NOW,
    )
    assert held.admitted
    before = state_path.read_bytes()

    for _ in range(5):
        verdict = admission.preview_admission(
            record, verb="do", difficulty="high", policy=policy,
        )
        assert verdict.admitted
        assert verdict.units == "subscription-percent"
        assert verdict.evidence_age_s is not None
        snapshot = admission.reservations_snapshot()
        assert held.reservation_id in snapshot
    assert state_path.read_bytes() == before
