"""Tests for the a2a status-breakpoint protocol family (x-dbaf).

Covers the extended envelope validation for task_started / task_done / blocked /
run_summary and the self-contained public schema subset:
  - envelope-level routable fields (v, run, node, task, from, ...) validate
  - additionalProperties:false for the new family only (AC2-ERR)
  - outcome enum + placement rules (AC1-ERR)
  - non-session producers omit from/model entirely (AC1-EDGE)
  - public schema subset stays in sync with the validator (no drift)
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.events import (
    PROTOCOL_ENVELOPE_ALLOWED,
    PROTOCOL_FAMILY_TYPES,
    ValidationError,
    _build,
    validate,
)

REPO_ROOT = Path(__file__).resolve().parents[3]
PUBLIC_SCHEMA = REPO_ROOT / "schemas" / "events-protocol-v1.json"


def _task_started(**over):
    ev = {
        "ts": "2026-07-10T22:00:00.000Z",
        "v": 1,
        "type": "task_started",
        "source": "target",
        "run": "tgt-run-1",
        "data": {"title": "do the thing", "executor": "do"},
    }
    ev.update(over)
    return ev


def _task_done(**over):
    ev = {
        "ts": "2026-07-10T22:00:00.000Z",
        "v": 1,
        "type": "task_done",
        "source": "target",
        "run": "tgt-run-1",
        "outcome": "SUCCESS",
        "data": {"commit": "abc123", "concerns": 0},
    }
    ev.update(over)
    return ev


def _run_summary(**over):
    ev = {
        "ts": "2026-07-10T22:00:00.000Z",
        "v": 1,
        "type": "run_summary",
        "source": "target",
        "run": "tgt-run-1",
        "outcome": "SUCCESS",
        "data": {"tasks_started": 2, "tasks_done": 2, "tasks_failed": 0},
    }
    ev.update(over)
    return ev


# -- happy path: each kind validates with the full envelope --

def test_task_started_valid() -> None:
    validate(_task_started(node="prj-0001", task="2.1"))
    # explicit envelope fields at top level ('from' is a keyword, set via dict)
    ev = _task_started()
    ev["node"] = "prj-0001"
    ev["task"] = "2.1"
    ev["from"] = "claude:sess-abc"
    ev["model"] = "claude-sonnet-5"
    ev["host"] = "mbp16"
    ev["parent"] = "claude:sess-parent"
    validate(ev)


def test_task_done_valid() -> None:
    validate(_task_done())


def test_run_summary_valid() -> None:
    validate(_run_summary())


# -- AC1-ERR: outcome enum + placement --

def test_out_of_enum_outcome_rejected() -> None:
    with pytest.raises(ValidationError) as exc:
        validate(_task_done(outcome="PARTIAL"))
    assert "outcome" in str(exc.value)


def test_task_done_requires_outcome() -> None:
    ev = _task_done()
    del ev["outcome"]
    with pytest.raises(ValidationError):
        validate(ev)


def test_outcome_forbidden_on_task_started() -> None:
    with pytest.raises(ValidationError) as exc:
        validate(_task_started(outcome="SUCCESS"))
    assert "outcome" in str(exc.value)


# -- AC2-ERR: additionalProperties:false for the new family only --

def test_unknown_envelope_field_rejected() -> None:
    with pytest.raises(ValidationError) as exc:
        validate(_task_started(bogus="x"))
    assert "bogus" in str(exc.value)


def test_legacy_type_keeps_extra_field_tolerance() -> None:
    # A legacy type carrying a stray top-level field is NOT rejected (the family
    # additionalProperties:false rule is scoped to the new types only).
    legacy = {
        "ts": "2026-07-10T22:00:00.000Z",
        "type": "mission_started",
        "source": "megatron",
        "data": {"mission_id": "m1"},
        "stray": "tolerated",
    }
    validate(legacy)  # must not raise


# -- envelope required fields --

def test_missing_v_rejected() -> None:
    ev = _task_started()
    del ev["v"]
    with pytest.raises(ValidationError):
        validate(ev)


def test_wrong_v_rejected() -> None:
    with pytest.raises(ValidationError):
        validate(_task_started(v=2))


def test_missing_run_rejected() -> None:
    ev = _task_started()
    del ev["run"]
    with pytest.raises(ValidationError):
        validate(ev)


# -- AC1-EDGE: non-session producer omits from/model entirely --

def test_build_omits_none_envelope_fields() -> None:
    event = _build(
        "task_started",
        "test",
        {"title": "t"},
        envelope={"v": 1, "run": "r1", "from": None, "model": None, "node": "prj-0001"},
    )
    assert "from" not in event  # omitted, not empty string
    assert "model" not in event
    assert event["node"] == "prj-0001"
    assert event["run"] == "r1"


# -- public schema subset: exists, self-contained, in sync with the validator --

def test_public_schema_exists_and_self_contained() -> None:
    assert PUBLIC_SCHEMA.is_file(), f"missing: {PUBLIC_SCHEMA}"
    text = PUBLIC_SCHEMA.read_text(encoding="utf-8")
    assert '"$ref"' not in text, "public schema must be self-contained (no $ref)"
    schema = json.loads(text)
    assert schema["additionalProperties"] is False
    assert set(schema["required"]) == {"ts", "v", "type", "source", "run", "data"}


def test_public_schema_matches_validator_allowlist() -> None:
    schema = json.loads(PUBLIC_SCHEMA.read_text(encoding="utf-8"))
    # every declared property is an allowed envelope field, and vice versa
    assert set(schema["properties"]) == PROTOCOL_ENVELOPE_ALLOWED
    assert set(schema["properties"]["type"]["enum"]) == PROTOCOL_FAMILY_TYPES
