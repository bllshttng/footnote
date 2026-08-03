from __future__ import annotations

import hashlib
import json
from pathlib import Path

import yaml
from typer.testing import CliRunner

from fno.cli import app


runner = CliRunner()


def _plan(tmp_path: Path, *, active: bool = True, required: bool = True) -> Path:
    evidence = {
        "id": "artifact-ready",
        "work_order_id": "x-delivery",
        "attempt_id": "attempt-1",
        "subject_kind": "artifact",
        "subject_id": "artifact-1",
        "result": "passed",
    }
    frontmatter = {
        "node": "x-delivery",
        "status": "ready",
        "created": "2026-08-02",
        "company_work": {
            "work_order": {"node_id": "x-delivery", "attempt_id": "attempt-1"},
            "deliverables": [
                {
                    "id": "release",
                    "kind": "unknown-function-output",
                    "work_order_id": "x-delivery",
                    "attempt_id": "attempt-1",
                    "required_evidence_ids": ["artifact-ready"] if required else [],
                }
            ],
            "evidence": [evidence] if required else [],
        },
    }
    if active:
        frontmatter["completion"] = "delivery"
    path = tmp_path / "plan.md"
    path.write_text(f"---\n{yaml.safe_dump(frontmatter, sort_keys=False)}---\n# Plan\n")
    return path


def _events(tmp_path: Path) -> tuple[Path, str]:
    fact = {
        "version": "delivery-evidence-fact.v1",
        "evidence": {
            "id": "artifact-ready",
            "work_order_id": "x-delivery",
            "attempt_id": "attempt-1",
            "subject_kind": "artifact",
            "subject_id": "artifact-1",
            "result": "passed",
        },
        "producer": "adapter:test",
        "observed_at": "2026-08-02T12:00:00Z",
        "source_revision": "artifact-sha",
        "fresh_until": "2099-08-02T12:05:00Z",
        "adapter_version": "test.v1",
        "fact_revision": "ignored-input-revision",
    }
    raw = (
        json.dumps(
            {
                "ts": "2026-08-02T12:00:00Z",
                "type": "delivery_evidence_observed",
                "source": "target",
                "data": fact,
            },
            separators=(",", ":"),
        )
        + "\n"
    ).encode()
    path = tmp_path / "events.jsonl"
    path.write_bytes(raw)
    return path, hashlib.sha256(raw).hexdigest()


def _invoke(plan: Path, events: Path) -> tuple[object, dict]:
    result = runner.invoke(
        app,
        ["delivery", "evaluate", "--json", "--plan-path", str(plan), "--events", str(events)],
    )
    return result, json.loads(result.stdout)


def test_ac_d7_hp_hidden_cli_evaluates_one_coherent_snapshot(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, digest = _events(tmp_path)

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    assert payload["version"] == "delivery-evaluate-response.v1"
    assert payload["status"] == "evaluated"
    assert payload["fact_revision"] == f"sha256:{digest}"
    assert payload["verdict"]["aggregate"] == "passed"
    assert [row["evidence_id"] for row in payload["verdict"]["requirements"]] == [
        "artifact-ready"
    ]


def test_ac_d5_inv_company_work_without_activation_is_inactive(tmp_path: Path) -> None:
    plan = _plan(tmp_path, active=False)
    events, _ = _events(tmp_path)

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    assert payload == {
        "version": "delivery-evaluate-response.v1",
        "status": "inactive",
        "fact_revision": None,
        "verdict": None,
        "diagnostics": ["generic delivery completion is not activated"],
    }


def test_ac_d5_inv_activation_without_required_slot_is_undeterminable(tmp_path: Path) -> None:
    plan = _plan(tmp_path, required=False)
    events, _ = _events(tmp_path)

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    assert payload["status"] == "undeterminable"
    assert payload["verdict"] is None
    assert "required evidence slot" in " ".join(payload["diagnostics"])


def test_ac_d10_err_malformed_journal_is_undeterminable(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events = tmp_path / "events.jsonl"
    events.write_text('{"type":"delivery_evidence_observed"\n')

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    assert payload["status"] == "undeterminable"
    assert payload["verdict"] is None
    assert "malformed" in " ".join(payload["diagnostics"])


def test_ac_d10_err_partial_delivery_event_is_undeterminable(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events = tmp_path / "events.jsonl"
    events.write_text(
        json.dumps(
            {
                "ts": "2026-08-02T12:00:00Z",
                "type": "delivery_evidence_observed",
                "source": "target",
                "data": {"version": "delivery-evidence-fact.v1"},
            }
        )
        + "\n"
    )

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    assert payload["status"] == "undeterminable"
    assert payload["verdict"] is None


def test_ac_d7_hp_latest_approval_and_effect_state_are_projected(tmp_path: Path) -> None:
    frontmatter = {
        "node": "x-delivery",
        "status": "ready",
        "created": "2026-08-02",
        "completion": "delivery",
        "company_work": {
            "work_order": {"node_id": "x-delivery", "attempt_id": "attempt-1"},
            "deliverables": [
                {
                    "id": "external-send",
                    "kind": "arbitrary-output",
                    "work_order_id": "x-delivery",
                    "attempt_id": "attempt-1",
                    "effect_id": "effect-1",
                    "required_evidence_ids": ["approval-ready", "effect-ready", "ack-ready"],
                }
            ],
            "effects": [
                {
                    "id": "effect-1",
                    "work_order_id": "x-delivery",
                    "attempt_id": "attempt-1",
                    "deliverable_id": "external-send",
                    "effect_class": "communication.external",
                    "destination": "email:customer@example.com",
                    "idempotency_key": "effect-key-1",
                    "approval_id": "request-digest-1",
                }
            ],
            "evidence": [
                {
                    "id": "approval-ready",
                    "work_order_id": "x-delivery",
                    "attempt_id": "attempt-1",
                    "subject_kind": "approval",
                    "subject_id": "request-digest-1",
                    "result": "unknown",
                },
                {
                    "id": "effect-ready",
                    "work_order_id": "x-delivery",
                    "attempt_id": "attempt-1",
                    "subject_kind": "effect",
                    "subject_id": "effect-1",
                    "result": "unknown",
                },
                {
                    "id": "ack-ready",
                    "work_order_id": "x-delivery",
                    "attempt_id": "attempt-1",
                    "subject_kind": "acknowledgment",
                    "subject_id": "effect-1",
                    "result": "unknown",
                },
            ],
        },
    }
    plan = tmp_path / "effect-plan.md"
    plan.write_text(f"---\n{yaml.safe_dump(frontmatter, sort_keys=False)}---\n# Plan\n")
    base = {
        "request_digest": "request-digest-1",
        "work_order_id": "x-delivery",
        "attempt_id": "attempt-1",
        "effect_id": "effect-1",
    }
    request = {
        **base,
        "request_id": "request-1",
        "principal_id": "principal-1",
        "effect_class": "communication.external",
        "destination": "email:customer@example.com",
        "action_digest": "action-digest-1",
        "expires_at": "2026-08-03T12:00:00Z",
    }
    rows = [
        {"ts": "2026-08-02T12:00:00Z", "type": "approval_requested", "source": "approvals", "data": request},
        {"ts": "2026-08-02T12:01:00Z", "type": "approval_decided", "source": "approvals", "data": {**base, "decision": "approved", "deciding_principal_id": "principal-1"}},
        {"ts": "2026-08-02T12:02:00Z", "type": "effect_state_changed", "source": "approvals", "data": {**base, "idempotency_key": "effect-key-1", "state": "executing", "previous_state": "prepared"}},
        {"ts": "2026-08-02T12:03:00Z", "type": "effect_state_changed", "source": "approvals", "data": {**base, "idempotency_key": "effect-key-1", "state": "acknowledged", "previous_state": "executing", "external_ref": "message-42"}},
    ]
    events = tmp_path / "effect-events.jsonl"
    events.write_text("".join(json.dumps(row) + "\n" for row in rows))

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    assert payload["status"] == "evaluated"
    assert payload["verdict"]["aggregate"] == "passed"
    assert {row["result"] for row in payload["verdict"]["requirements"]} == {"passed"}


def test_ac_d10_hp_refreshed_observed_snapshot_replaces_stale_history(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    current = json.loads(events.read_text())
    stale = json.loads(json.dumps(current))
    stale["ts"] = "2026-08-01T12:00:00Z"
    stale["data"]["observed_at"] = "2026-08-01T12:00:00Z"
    stale["data"]["fresh_until"] = "2026-08-01T12:05:00Z"
    stale["data"]["fact_revision"] = "producer-snapshot-old"
    stale["data"]["evidence"]["result"] = "failed"
    current["data"]["fact_revision"] = "producer-snapshot-current"
    events.write_text(json.dumps(stale) + "\n" + json.dumps(current) + "\n")

    _, payload = _invoke(plan, events)

    assert payload["verdict"]["aggregate"] == "passed"


def test_ac_d10_err_same_snapshot_conflict_remains_unknown(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    passed = json.loads(events.read_text())
    failed = json.loads(json.dumps(passed))
    failed["data"]["producer"] = "adapter:conflict"
    failed["data"]["evidence"]["result"] = "failed"
    events.write_text(json.dumps(passed) + "\n" + json.dumps(failed) + "\n")

    _, payload = _invoke(plan, events)

    assert payload["verdict"]["aggregate"] == "unknown"
    assert "conflicting duplicate facts" in json.dumps(payload["verdict"])
