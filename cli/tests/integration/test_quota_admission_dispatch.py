"""Admission through the dispatch transaction (x-1afa).

Every assertion pins a positive marker: a persisted reservation row, a
receipt naming the pool it charged, or a typed defer reason. An exit code or
an absence is never the proof.
"""
from __future__ import annotations

import json
import os
import time
from pathlib import Path

import pytest

from fno.adapters.providers import admission, runtime_state as rs
from fno.adapters.providers import loader as loader_mod
from fno.adapters.providers.model import ProviderRecord, ProvidersConfig
from fno.adapters.providers.usage import UsageSnapshot, UsageWindow
from fno.config._routing_admission import AdmissionPolicy

# The gate seam and the deferral preview read the real clock, so the seeded
# evidence must be fresh against time.time(), not a synthetic epoch.
NOW = time.time()

_REPO_ROOT = Path(__file__).resolve().parents[3]


@pytest.fixture(autouse=True)
def _pin_rust_owner(monkeypatch):
    """The admission owner is the Rust verb; pin THIS checkout's build so a
    stale installed binary cannot answer the suite with the wrong math."""
    if not os.environ.get("FNO_AGENTS_BIN"):
        candidate = _REPO_ROOT / "crates" / "fno-agents" / "target" / "debug" / "fno-agents"
        if candidate.is_file():
            monkeypatch.setenv("FNO_AGENTS_BIN", str(candidate))


def _policy(**kw) -> AdmissionPolicy:
    base = dict(
        enabled=True,
        max_inflight_per_pool=3,
        reservation_ttl_seconds=900.0,
        demand_pct={"do": {"high": 15}},
        reserve_pct={"do": {"high": 10}},
    )
    base.update(kw)
    return AdmissionPolicy(**base)


def _record(record_id: str, pool: str | None = None) -> ProviderRecord:
    return ProviderRecord(
        id=record_id,
        name=record_id,
        harness="claude",
        auth="api_key",
        env={"ANTHROPIC_API_KEY": "sk-test"},
        quota_pool=pool,
    )


@pytest.fixture()
def armed(tmp_path, monkeypatch):
    """Pin runtime state, the provider table, and an armed policy."""
    state_path = tmp_path / "runtime-state.json"
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(state_path))
    providers = ProvidersConfig(records=[_record("rec-a"), _record("rec-b", pool="family")])
    monkeypatch.setattr(loader_mod, "load_providers", lambda **kw: providers)
    monkeypatch.setattr(
        "fno.config._routing_admission.resolve_admission_policy", lambda: _policy()
    )
    monkeypatch.setattr(
        "fno.adapters.providers.admission.resolve_admission_policy", lambda: _policy()
    )
    snap = UsageSnapshot(
        provider_id="rec-a",
        windows=(UsageWindow(label="5h", used_pct=60.0, resets_at=NOW + 600.0),),
        probed_at=NOW,
        source="test",
    )
    snap_b = UsageSnapshot(
        provider_id="rec-b",
        windows=(UsageWindow(label="5h", used_pct=60.0, resets_at=NOW + 600.0),),
        probed_at=NOW,
        source="test",
    )
    assert rs.write_usage_snapshot(snap, now=NOW)
    assert rs.write_usage_snapshot(snap_b, now=NOW)
    return state_path


def test_ac3_hp_launch_transaction_agrees_on_pool_and_session(armed):
    """Reservation, observed account, and the real worker session agree."""
    record = _record("rec-a")
    receipt = admission.reserve_admission(
        record, dispatch_id="spawn:w1", verb="do", difficulty="high", now=NOW,
    )
    assert receipt.admitted
    assert receipt.pool == "api:claude/rec-a"
    assert admission.commit_reservation(
        receipt.reservation_id, dispatch_id="spawn:w1",
        session_id="sess-abc", now=NOW,
    )
    row = json.loads(armed.read_text())["reservations"][receipt.reservation_id]
    assert row["pool"] == receipt.pool == "api:claude/rec-a"
    assert row["provider_id"] == "rec-a"
    assert row["session_id"] == "sess-abc"
    assert row["state"] == "committed"


def test_ac3_err_failed_launch_releases_only_itself(armed):
    """One launch fails while another is live: only the failed one refunds."""
    live = admission.reserve_admission(
        _record("rec-a"), dispatch_id="spawn:live", demand_pct=10.0, now=NOW,
    )
    failed = admission.reserve_admission(
        _record("rec-a"), dispatch_id="spawn:failed", demand_pct=10.0, now=NOW,
    )
    assert live.admitted and failed.admitted
    assert admission.release_reservation(
        failed.reservation_id, dispatch_id="spawn:failed", now=NOW,
    )
    rows = json.loads(armed.read_text())["reservations"]
    assert set(rows) == {live.reservation_id}
    # The surviving dispatch cannot be released by someone else's identity,
    # and an ambiguous launch (never committed) never manufactured a refund.
    assert admission.release_reservation(
        live.reservation_id, dispatch_id="spawn:failed", now=NOW,
    ) is False
    assert live.reservation_id in json.loads(armed.read_text())["reservations"]


def test_ac3_queue_typed_wait_then_launch_with_matching_receipt(armed, monkeypatch):
    """All capacity refused -> typed defer; fresh evidence -> launch receipt."""
    from fno.agents.autonomous_route import _admission_deferral

    # 20% remaining, 10% reserve, 15% demanded: 20 - 0 - 15 = 5, which is
    # below the reserve for a normal dispatch.
    tight = UsageSnapshot(
        provider_id="rec-a",
        windows=(UsageWindow(label="5h", used_pct=80.0, resets_at=NOW + 600.0),),
        probed_at=NOW,
        source="test",
    )
    assert rs.write_usage_snapshot(tight, now=NOW)
    held = _admission_deferral(
        "rec-a", priority=None, node_cwd=None, verb="do", difficulty="high",
    )
    assert held is not None
    assert held.action == "defer"
    assert held.reason.startswith("admission:")
    assert held.retry_at is None or held.retry_at > NOW - 1

    # p0 is the priority exception: it may consume the reserve (5 >= 0).
    priority = _admission_deferral(
        "rec-a", priority="p0", node_cwd=None, verb="do", difficulty="high",
    )
    assert priority is None

    # The wait clears (operator tops up the window), the retry launches, and
    # the gate's reservation is the matching receipt for the queued node.
    fresh = UsageSnapshot(
        provider_id="rec-a",
        windows=(UsageWindow(label="5h", used_pct=20.0, resets_at=NOW + 600.0),),
        probed_at=NOW,
        source="test",
    )
    assert rs.write_usage_snapshot(fresh, now=NOW)
    assert _admission_deferral(
        "rec-a", priority=None, node_cwd=None, verb="do", difficulty="high",
    ) is None
    receipt = admission.reserve_admission(
        _record("rec-a"), dispatch_id="spawn:retry", verb="do", difficulty="high",
        now=NOW,
    )
    assert receipt.admitted
    assert receipt.reservation_id in json.loads(armed.read_text())["reservations"]


def test_gate_seam_refusal_carries_the_typed_receipt(armed, monkeypatch):
    """The launch seam refuses with reason account_admission_refused."""
    from fno.agents.spawn_gate import GateRefused, _reserve_account_budget

    # The pool already holds 25%: a second 15% demand lands at 40-25-15 = 0,
    # below the 10% floor, while the first reserve alone admitted at 15.
    first = admission.reserve_admission(
        _record("rec-a"), dispatch_id="spawn:w0", demand_pct=25.0, now=NOW,
    )
    assert first.admitted
    with pytest.raises(GateRefused) as excinfo:
        _reserve_account_budget(
            "rec-a", "w1", verb="do", difficulty="high", consume_reserve=False,
        )
    receipt = excinfo.value.receipt
    assert receipt["reason"] == "account_admission_refused"
    assert receipt["admission_status"] == "reserved_capacity"
    assert receipt["units"] == "subscription-percent"


def test_gate_seam_admits_and_returns_the_budget_receipt(armed):
    from fno.agents.spawn_gate import _reserve_account_budget

    receipt = _reserve_account_budget(
        "rec-a", "w1", verb="do", difficulty="high", consume_reserve=False,
    )
    assert receipt is not None
    assert receipt["status"] == "admitted"
    assert receipt["pool"] == "api:claude/rec-a"
    assert receipt["units"] == "subscription-percent"
    rows = json.loads(armed.read_text())["reservations"]
    assert receipt["reservation_id"] in rows


def test_preview_rows_never_consume(armed, capsys):
    """The route admission preview is pure: byte-identical state, rows marked."""
    from fno.route_cli import admission_cmd

    before = armed.read_text()
    admission_cmd(verb="do", difficulty="high")
    assert armed.read_text() == before
    out = capsys.readouterr().out
    # The positive preview marker and the units ride the header line; the
    # row names the record it previews.
    assert "preview (no reservation consumed)" in out
    assert "units=subscription-percent" in out
    assert "rec-a" in out
    assert "pool=" in out
