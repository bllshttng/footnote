"""Carveout severity, stamped by provenance and routed to priority (x-cbab AC6).

A carveout filed to satisfy the fidelity gate is, by construction, "a planned
deliverable is unbuilt" and lands high/p1 - not p3. The routing already existed
(``retro.classify.severity_to_priority`` + ``_resolve_priority`` reads
``item.severity``); only the field was missing. This test pins the plumbing end
to end: the record carries severity, harvest reads it, classify routes it.
"""
from __future__ import annotations

import json

import pytest

from fno.carveout.core import (
    CarveoutError,
    VALID_SEVERITIES,
    add_carveout,
    read_carveouts,
)
from fno.retro.classify import _resolve_priority, severity_to_priority
from fno.retro.types import KIND_CARVEOUT, RawItem


def test_add_carveout_stamps_severity_onto_the_record(tmp_path):
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="unbuilt deliverable",
        severity="high", storage_root=tmp_path,
    )
    assert cv.severity == "high"
    rec = read_carveouts(tmp_path)[0]
    assert rec["severity"] == "high"


def test_invalid_severity_is_refused(tmp_path):
    with pytest.raises(CarveoutError):
        add_carveout(
            tmp_path, kind="deferred", description="x",
            severity="bogus", storage_root=tmp_path,
        )


def test_severity_routes_a_gate_carveout_to_p1_not_p3():
    """AC6-SEV: a gate-stamped carveout (severity=high) lands p1."""
    assert severity_to_priority("high") == "p1"
    item = RawItem(kind=KIND_CARVEOUT, text="unbuilt deliverable", severity="high")
    assert _resolve_priority(item) == "p1"


def test_no_severity_keeps_todays_p3_default():
    item = RawItem(kind=KIND_CARVEOUT, text="minor item")  # severity None
    assert _resolve_priority(item) == "p3"


def test_explicit_priority_still_wins_over_severity():
    """A hand-filed --priority overrides severity (precision beats provenance
    when the filer was more specific)."""
    item = RawItem(
        kind=KIND_CARVEOUT, text="x", severity="high", priority="p2"
    )
    assert _resolve_priority(item) == "p2"


def test_all_severities_route():
    assert severity_to_priority("critical") == "p0"
    assert severity_to_priority("high") == "p1"
    assert severity_to_priority("medium") == "p2"
    assert severity_to_priority("low") == "p3"
    assert severity_to_priority(None) == "p3"
    assert VALID_SEVERITIES == ("critical", "high", "medium", "low")


def test_a_legacy_record_without_severity_parses_as_none(tmp_path):
    """Existing JSONL records predate the field; a missing key reads None, so the
    retro harvest never crashes and they keep today's p3."""
    from fno.paths import project_log

    ledger = project_log("carveouts.jsonl", project_root=tmp_path)
    ledger.parent.mkdir(parents=True, exist_ok=True)
    legacy = {
        "id": "cv-old", "ts": "t", "session_id": None, "kind": "deferred",
        "priority": None, "need": None, "description": "old", "truncated": False,
        "scope": None,
    }
    ledger.write_text(json.dumps(legacy) + "\n", encoding="utf-8")
    rec = read_carveouts(tmp_path)[0]
    assert rec.get("severity") is None
