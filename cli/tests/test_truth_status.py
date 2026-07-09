"""Tests for fno.agents.truth_status - the fno-truth worker status resolver.

Covers the state table (working/waiting/suspect/stalled/unknown), the node-id
parse, the loop_check tail scan, render strings, missing-file degradation, and
one real-claim-file integration path.
"""

from __future__ import annotations

import json

import pytest

from fno.agents import truth_status as ts

SID = "20260709T001358Z-cl21834-267287"
HOLDER = f"target-session:{SID}"


# --------------------------------------------------------------------------
# node-id parse
# --------------------------------------------------------------------------
@pytest.mark.parametrize(
    "name,expected",
    [
        ("target-x-4a48-fleet-status-derive-working-wh", "x-4a48"),
        ("target-ab-1a2b3c4d-dashless-spawn", "ab-1a2b3c4d"),
        ("target-x-4a48", "x-4a48"),  # no slug tail
        ("phasestall", None),
        ("", None),
        (None, None),
        ("worker-x-4a48-foo", None),  # not a target- name
    ],
)
def test_parse_node_id(name, expected):
    assert ts.parse_node_id(name) == expected


# --------------------------------------------------------------------------
# state table (claim_status monkeypatched -> tests the combine logic only)
# --------------------------------------------------------------------------
def _patch_claim(monkeypatch, status):
    monkeypatch.setattr(ts, "claim_status", lambda key, root=None: status)


def test_live_recent_fire_is_working(monkeypatch):  # AC1-HP
    _patch_claim(monkeypatch, {"state": "live", "holder": HOLDER})
    r = ts.resolve_truth_status("x-4a48", loop_check_ages={SID: 120})
    assert r["state"] == "working"
    assert r["last_loop_check_age_s"] == 120
    assert r["session_id"] == SID


def test_live_stale_fire_is_waiting(monkeypatch):  # AC3-EDGE
    _patch_claim(monkeypatch, {"state": "live", "holder": HOLDER})
    r = ts.resolve_truth_status(
        "x-4a48", loop_check_ages={SID: 9999}, recency_window_s=1800
    )
    assert r["state"] == "waiting"


def test_live_no_fire_is_waiting(monkeypatch):  # AC3-EDGE (Locked Decision 2)
    _patch_claim(monkeypatch, {"state": "live", "holder": HOLDER})
    r = ts.resolve_truth_status("x-4a48", loop_check_ages={})
    assert r["state"] == "waiting"
    assert r["last_loop_check_age_s"] is None


def test_suspect_claim(monkeypatch):  # AC3b-EDGE
    _patch_claim(monkeypatch, {"state": "suspect", "holder": HOLDER})
    r = ts.resolve_truth_status("x-4a48", loop_check_ages={SID: 10})
    assert r["state"] == "suspect"  # never working/waiting even with a fire


def test_stale_claim(monkeypatch):  # AC4-HP
    _patch_claim(monkeypatch, {"state": "stale", "holder": HOLDER})
    r = ts.resolve_truth_status("x-4a48", loop_check_ages={SID: 10})
    assert r["state"] == "stalled"


def test_free_claim_is_unknown(monkeypatch):  # AC7-FR (released / never claimed)
    _patch_claim(monkeypatch, {"state": "free"})
    r = ts.resolve_truth_status("x-4a48", loop_check_ages={SID: 10})
    assert r["state"] == "unknown"


def test_corrupted_claim_is_unknown(monkeypatch):
    _patch_claim(monkeypatch, {"state": "corrupted", "error": "bad"})
    r = ts.resolve_truth_status("x-4a48", loop_check_ages={})
    assert r["state"] == "unknown"


def test_no_node_id_is_unknown():  # AC7-FR (unresolvable join)
    r = ts.resolve_truth_status(None)
    assert r["state"] == "unknown"


# --------------------------------------------------------------------------
# loop_check tail scan
# --------------------------------------------------------------------------
def _write_events(path, records):
    path.write_text("".join(json.dumps(r) + "\n" for r in records))


def test_build_index_newest_fire_wins(tmp_path):
    ev = tmp_path / "events.jsonl"
    _write_events(
        ev,
        [
            {"ts": "2026-07-09T01:00:00Z", "type": "loop_check", "data": {"session_id": SID}},
            {"ts": "2026-07-09T01:05:00Z", "type": "loop_check", "data": {"session_id": SID}},
            {"ts": "2026-07-09T01:02:00Z", "type": "other", "data": {"session_id": SID}},
        ],
    )
    now = _epoch("2026-07-09T01:07:00Z")
    idx = ts.build_loop_check_index(events_path=ev, now_s=now)
    assert idx[SID] == pytest.approx(120.0)  # newest fire, 01:05 -> 2m ago


def test_build_index_missing_file_is_empty(tmp_path):  # AC5-ERR
    idx = ts.build_loop_check_index(events_path=tmp_path / "nope.jsonl")
    assert idx == {}


def test_build_index_skips_malformed_line(tmp_path):  # Concurrency: partial append
    ev = tmp_path / "events.jsonl"
    ev.write_text(
        json.dumps({"ts": "2026-07-09T01:00:00Z", "type": "loop_check", "data": {"session_id": SID}})
        + "\n"
        + '{"ts":"2026-07-09T01:05:00Z","type":"loop_check","data":{"sess'  # truncated
    )
    now = _epoch("2026-07-09T01:01:00Z")
    idx = ts.build_loop_check_index(events_path=ev, now_s=now)
    assert idx[SID] == pytest.approx(60.0)  # only the whole line counted


def _epoch(iso):
    from datetime import datetime

    return datetime.fromisoformat(iso.replace("Z", "+00:00")).timestamp()


# --------------------------------------------------------------------------
# render strings
# --------------------------------------------------------------------------
@pytest.mark.parametrize(
    "state,age,expected",
    [
        ("working", 120, "Working (loop 2m ago)"),
        ("working", 45, "Working (loop 45s ago)"),
        ("working", 7200, "Working (loop 2h ago)"),
        ("waiting", None, "Waiting (claim live)"),
        ("suspect", None, "Suspect (claim suspect)"),
        ("stalled", None, "Stalled (claim stale)"),
        ("unknown", None, None),
    ],
)
def test_render_truth_status(state, age, expected):
    r = {"state": state, "last_loop_check_age_s": age}
    assert ts.render_truth_status(r) == expected


# --------------------------------------------------------------------------
# integration: a real claim file + real events tail -> working (AC1-HP e2e)
# --------------------------------------------------------------------------
def test_real_live_claim_end_to_end(tmp_path):
    from fno.claims.core import acquire_claim

    # Live claim: default pid = this test process (alive), future TTL.
    acquire_claim(
        "node:x-4a48",
        HOLDER,
        root=tmp_path,
        ttl_ms=600_000,
        reason="test",
    )
    ev = tmp_path / "events.jsonl"
    _write_events(
        ev, [{"ts": "2026-07-09T01:05:00Z", "type": "loop_check", "data": {"session_id": SID}}]
    )
    now = _epoch("2026-07-09T01:06:00Z")
    r = ts.resolve_truth_status(
        "x-4a48", claims_root=tmp_path, events_path=ev, now_s=now
    )
    assert r["state"] == "working"
    assert r["claim_state"] == "live"
    assert ts.render_truth_status(r) == "Working (loop 1m ago)"


def test_missing_signals_degrade_to_unknown(tmp_path):  # AC5-ERR
    # Empty claims root (no claim) + no events file -> unknown, no raise.
    r = ts.resolve_truth_status(
        "x-4a48", claims_root=tmp_path, events_path=tmp_path / "nope.jsonl"
    )
    assert r["state"] == "unknown"
    assert ts.render_truth_status(r) is None
