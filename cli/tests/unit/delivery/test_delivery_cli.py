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
        ["do", "delivery", "evaluate", "--json", "--plan-path", str(plan), "--events", str(events)],
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
    assert payload["evidence_revision"].startswith("sha256:")
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
        "evidence_revision": None,
        "verdict": None,
        "diagnostics": ["generic delivery completion is not activated"],
    }


def test_delivery_evidence_revision_ignores_loop_audit_events(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    _, before = _invoke(plan, events)
    with events.open("a") as stream:
        stream.write(
            json.dumps(
                {
                    "ts": "2026-08-02T12:01:00Z",
                    "type": "loop_check",
                    "source": "hook",
                    "data": {"session_id": "s1"},
                }
            )
            + "\n"
        )

    _, after = _invoke(plan, events)

    assert before["fact_revision"] != after["fact_revision"]
    assert before["evidence_revision"] == after["evidence_revision"]

    current = json.loads(events.read_text().splitlines()[0])
    unrelated = json.loads(json.dumps(current))
    unrelated["data"]["evidence"]["id"] = "other-evidence"
    unrelated["data"]["evidence"]["work_order_id"] = "x-other"
    unrelated["data"]["evidence"]["attempt_id"] = "other-attempt"
    unrelated["data"]["evidence"]["subject_id"] = "other-artifact"
    historical = json.loads(json.dumps(current))
    historical["ts"] = "2026-08-01T12:00:00Z"
    historical["data"]["observed_at"] = "2026-08-01T12:00:00Z"
    historical["data"]["source_revision"] = "old-source"
    historical["data"]["evidence"]["result"] = "failed"
    with events.open("a") as stream:
        stream.write(json.dumps(unrelated) + "\n")
        stream.write(json.dumps(historical) + "\n")

    _, noisy = _invoke(plan, events)

    assert after["evidence_revision"] == noisy["evidence_revision"]


def test_unrelated_work_cannot_replace_same_named_evidence(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    current = json.loads(events.read_text())
    unrelated = json.loads(json.dumps(current))
    unrelated["ts"] = "2026-08-02T12:01:00Z"
    unrelated["data"]["observed_at"] = "2026-08-02T12:01:00Z"
    unrelated["data"]["evidence"]["work_order_id"] = "x-other"
    unrelated["data"]["evidence"]["attempt_id"] = "other-attempt"
    unrelated["data"]["evidence"]["subject_id"] = "other-artifact"
    unrelated["data"]["evidence"]["result"] = "failed"
    events.write_text("")
    _, missing = _invoke(plan, events)
    events.write_text(json.dumps(unrelated) + "\n")
    _, foreign_only = _invoke(plan, events)

    assert foreign_only["evidence_revision"] == missing["evidence_revision"]

    events.write_text(json.dumps(current) + "\n" + json.dumps(unrelated) + "\n")

    _, payload = _invoke(plan, events)

    assert payload["verdict"]["aggregate"] == "passed"


def test_ac_d5_inv_activation_without_required_slot_is_inactive(tmp_path: Path) -> None:
    plan = _plan(tmp_path, required=False)
    events, _ = _events(tmp_path)

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    assert payload["status"] == "inactive"
    assert payload["verdict"] is None
    assert "valid company work" in " ".join(payload["diagnostics"])


def test_ac_d2_err_malformed_observed_event_retains_requirement_diagnostic(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    event = json.loads(events.read_text())
    event["data"].pop("adapter_version")
    events.write_text(json.dumps(event) + "\n")

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    assert payload["status"] == "evaluated"
    assert payload["verdict"]["aggregate"] == "unknown"
    row = payload["verdict"]["requirements"][0]
    assert row["evidence_id"] == "artifact-ready"
    assert "adapter:test" in " ".join(row["diagnostics"])
    assert "malformed" in " ".join(row["diagnostics"])


def test_ac_d2_hp_newer_valid_observation_replaces_malformed_history(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    valid = json.loads(events.read_text())
    malformed = json.loads(json.dumps(valid))
    malformed["ts"] = "2026-08-02T11:59:00Z"
    malformed["data"]["observed_at"] = "2026-08-02T11:59:00Z"
    malformed["data"].pop("adapter_version")
    events.write_text(json.dumps(malformed) + "\n" + json.dumps(valid) + "\n")

    _, payload = _invoke(plan, events)

    assert payload["verdict"]["aggregate"] == "passed"
    assert "malformed" not in json.dumps(payload["verdict"])


def test_ac_d2_err_newer_malformed_observation_replaces_valid_history(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    valid = json.loads(events.read_text())
    malformed = json.loads(json.dumps(valid))
    malformed["ts"] = "2026-08-02T12:01:00Z"
    malformed["data"]["observed_at"] = "2026-08-02T12:01:00Z"
    malformed["data"].pop("adapter_version")
    events.write_text(json.dumps(valid) + "\n" + json.dumps(malformed) + "\n")

    _, payload = _invoke(plan, events)

    assert payload["verdict"]["aggregate"] == "unknown"
    assert "malformed" in json.dumps(payload["verdict"])


def test_ac_d2_err_unordered_observation_never_revives_older_pass(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    for timestamp in (None, "not-a-timestamp"):
        events, _ = _events(tmp_path)
        valid = json.loads(events.read_text())
        malformed = json.loads(json.dumps(valid))
        if timestamp is None:
            malformed.pop("ts")
        else:
            malformed["ts"] = timestamp
        malformed["data"].pop("adapter_version")
        events.write_text(json.dumps(valid) + "\n" + json.dumps(malformed) + "\n")

        _, payload = _invoke(plan, events)

        assert payload["verdict"]["aggregate"] == "unknown"
        assert "malformed" in json.dumps(payload["verdict"])

        corrected = json.loads(json.dumps(valid))
        corrected["ts"] = "2026-08-02T12:01:00Z"
        corrected["data"]["observed_at"] = "2026-08-02T12:01:00Z"
        events.write_text(
            json.dumps(valid)
            + "\n"
            + json.dumps(malformed)
            + "\n"
            + json.dumps(corrected)
            + "\n"
        )

        _, corrected_payload = _invoke(plan, events)

        assert corrected_payload["verdict"]["aggregate"] == "passed"


def test_ac_d2_err_attempt_mismatch_retains_producer_and_rejected_binding(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    event = json.loads(events.read_text())
    event["data"]["evidence"]["attempt_id"] = "attempt-old"
    events.write_text(json.dumps(event) + "\n")

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    assert payload["status"] == "evaluated"
    row = payload["verdict"]["requirements"][0]
    assert row["result"] == "unknown"
    assert "adapter:test" in " ".join(row["diagnostics"])
    assert "attempt-old" in " ".join(row["diagnostics"])
    assert "rejected binding" in " ".join(row["diagnostics"])


def test_ac_d2_err_observed_fact_from_non_target_source_is_rejected(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    event = json.loads(events.read_text())
    event["source"] = "approvals"
    events.write_text(json.dumps(event) + "\n")

    _, payload = _invoke(plan, events)

    row = payload["verdict"]["requirements"][0]
    assert row["result"] == "unknown"
    assert "source approvals" in " ".join(row["diagnostics"])


def test_ac_d2_err_stale_fact_retains_producer_and_freshness_boundary(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    event = json.loads(events.read_text())
    event["data"]["fresh_until"] = "2026-08-02T12:00:01Z"
    events.write_text(json.dumps(event) + "\n")

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    row = payload["verdict"]["requirements"][0]
    assert row["result"] == "unknown"
    assert "adapter:test" in " ".join(row["diagnostics"])
    assert "stale after 2026-08-02T12:00:01+00:00" in " ".join(row["diagnostics"])


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


def test_ac_d10_con_journal_mutation_returns_versioned_unknown_verdict(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.delivery import reader

    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    original = reader._read_coherent_bytes

    def concurrent_read(path: Path) -> bytes:
        return original(
            path,
            after_read=lambda: path.write_bytes(path.read_bytes() + b"\n"),
        )

    monkeypatch.setattr(reader, "_read_coherent_bytes", concurrent_read)

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    assert payload["version"] == "delivery-evaluate-response.v1"
    assert payload["status"] == "evaluated"
    assert payload["verdict"]["aggregate"] == "unknown"
    assert payload["verdict"]["fact_revision"].startswith("conflict:")
    diagnostics = " ".join(payload["verdict"]["diagnostics"])
    assert "event journal changed during read" in diagnostics
    assert diagnostics.count("stat:") >= 2


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
        "event_id": "event-request-1",
    }
    rows = [
        {"ts": "2026-08-02T12:00:00Z", "type": "approval_requested", "source": "approvals", "data": request},
        {"ts": "2026-08-02T12:01:00Z", "type": "approval_decided", "source": "approvals", "data": {**base, "decision": "approved", "deciding_principal_id": "principal-1", "event_id": "event-decision-1"}},
        {"ts": "2026-08-02T12:02:00Z", "type": "effect_state_changed", "source": "approvals", "data": {**base, "idempotency_key": "effect-key-1", "state": "executing", "previous_state": "prepared", "event_id": "event-effect-1"}},
        {"ts": "2026-08-02T12:03:00Z", "type": "effect_state_changed", "source": "approvals", "data": {**base, "idempotency_key": "effect-key-1", "state": "acknowledged", "previous_state": "executing", "external_ref": "message-42", "event_id": "event-effect-2"}},
    ]
    events = tmp_path / "effect-events.jsonl"
    events.write_text("".join(json.dumps(row) + "\n" for row in rows))

    result, payload = _invoke(plan, events)

    assert result.exit_code == 0
    assert payload["status"] == "evaluated"
    assert payload["verdict"]["aggregate"] == "passed"
    assert {row["result"] for row in payload["verdict"]["requirements"]} == {"passed"}

    foreign_effect = json.loads(json.dumps(rows[-1]))
    foreign_effect["ts"] = "2026-08-02T12:04:00Z"
    foreign_effect["data"].update(
        {
            "work_order_id": "x-other",
            "state": "failed",
            "previous_state": "acknowledged",
            "event_id": "event-effect-foreign",
        }
    )
    events.write_text(
        "".join(json.dumps(row) + "\n" for row in rows + [foreign_effect])
    )
    _, foreign_payload = _invoke(plan, events)
    assert foreign_payload["verdict"]["aggregate"] == "passed"
    assert foreign_payload["evidence_revision"] == payload["evidence_revision"]

    wrong_attempt_effect = json.loads(json.dumps(rows[-1]))
    wrong_attempt_effect["ts"] = "2026-08-02T12:05:00Z"
    wrong_attempt_effect["data"].update(
        {
            "attempt_id": "attempt-old",
            "state": "failed",
            "previous_state": "acknowledged",
            "event_id": "event-effect-wrong-attempt",
        }
    )
    events.write_text(
        "".join(json.dumps(row) + "\n" for row in rows + [wrong_attempt_effect])
    )
    _, wrong_attempt_payload = _invoke(plan, events)
    assert wrong_attempt_payload["verdict"]["aggregate"] == "unknown"
    wrong_attempt_row = next(
        row
        for row in wrong_attempt_payload["verdict"]["requirements"]
        if row["evidence_id"] == "effect-ready"
    )
    diagnostics = " ".join(wrong_attempt_row["diagnostics"])
    assert "event:approvals:effect_state_changed" in diagnostics
    assert "attempt-old" in diagnostics
    assert "rejected binding" in diagnostics

    conflicting_decision = json.loads(json.dumps(rows[1]))
    conflicting_decision["ts"] = "2026-08-02T12:06:00Z"
    conflicting_decision["data"].update(
        {
            "decision": "declined",
            "event_id": "event-decision-conflicting",
        }
    )
    events.write_text(
        "".join(json.dumps(row) + "\n" for row in rows + [conflicting_decision])
    )
    _, conflicting_decision_payload = _invoke(plan, events)
    assert conflicting_decision_payload["verdict"]["aggregate"] == "unknown"
    approval_row = next(
        row
        for row in conflicting_decision_payload["verdict"]["requirements"]
        if row["evidence_id"] == "approval-ready"
    )
    assert approval_row["result"] == "unknown"
    assert "conflicting immutable approval decisions" in " ".join(
        approval_row["diagnostics"]
    )

    duplicate_approval = json.loads(json.dumps(rows[1]))
    duplicate_approval["ts"] = "2026-08-02T12:07:00Z"
    duplicate_approval["data"].update(
        {
            "deciding_principal_id": "principal-2",
            "event_id": "event-decision-other-principal",
        }
    )
    events.write_text(
        "".join(json.dumps(row) + "\n" for row in rows + [duplicate_approval])
    )
    _, duplicate_approval_payload = _invoke(plan, events)
    assert duplicate_approval_payload["verdict"]["aggregate"] == "unknown"
    approval_row = next(
        row
        for row in duplicate_approval_payload["verdict"]["requirements"]
        if row["evidence_id"] == "approval-ready"
    )
    assert approval_row["result"] == "unknown"
    assert "conflicting immutable approval decisions" in " ".join(
        approval_row["diagnostics"]
    )

    conflicting_request = json.loads(json.dumps(rows[0]))
    conflicting_request["ts"] = "2026-08-02T12:08:00Z"
    conflicting_request["data"].update(
        {
            "destination": "email:other@example.com",
            "event_id": "event-request-conflicting",
        }
    )
    events.write_text(
        "".join(json.dumps(row) + "\n" for row in rows + [conflicting_request])
    )
    _, conflicting_request_payload = _invoke(plan, events)
    assert conflicting_request_payload["verdict"]["aggregate"] == "unknown"
    approval_row = next(
        row
        for row in conflicting_request_payload["verdict"]["requirements"]
        if row["evidence_id"] == "approval-ready"
    )
    request_diagnostics = " ".join(approval_row["diagnostics"])
    assert "conflicting immutable approval requests" in request_diagnostics
    assert "event:approvals:approval_requested" in request_diagnostics
    effect_row = next(
        row
        for row in conflicting_request_payload["verdict"]["requirements"]
        if row["evidence_id"] == "effect-ready"
    )
    assert effect_row["result"] == "unknown"
    assert "rejected binding" in " ".join(effect_row["diagnostics"])

    events.write_text(
        "".join(json.dumps(row) + "\n" for row in rows + [rows[-1]])
    )
    _, duplicate_payload = _invoke(plan, events)
    assert duplicate_payload["evidence_revision"] == payload["evidence_revision"]

    conflicting_duplicate = json.loads(json.dumps(rows[1]))
    conflicting_duplicate["data"]["decision"] = "declined"
    events.write_text(
        "".join(
            json.dumps(row) + "\n"
            for row in rows + [conflicting_duplicate]
        )
    )
    _, conflicting_payload = _invoke(plan, events)
    assert conflicting_payload["status"] == "undeterminable"
    assert "event_id" in " ".join(conflicting_payload["diagnostics"])

    malformed_decision = {
        "ts": "2026-08-02T12:04:00Z",
        "type": "approval_decided",
        "source": "approvals",
        "data": {
            "work_order_id": "x-delivery",
            "attempt_id": "attempt-1",
            "effect_id": "effect-1",
            "decision": "declined",
            "deciding_principal_id": "principal-1",
        },
    }
    events.write_text(
        "".join(json.dumps(row) + "\n" for row in rows + [malformed_decision])
    )

    _, malformed_payload = _invoke(plan, events)

    approval_row = next(
        row
        for row in malformed_payload["verdict"]["requirements"]
        if row["evidence_id"] == "approval-ready"
    )
    assert approval_row["result"] == "unknown"
    assert "missing request_digest" in " ".join(approval_row["diagnostics"])

    for event_type in (
        "approval_requested",
        "approval_decided",
        "effect_state_changed",
    ):
        events.write_text(
            "".join(
                json.dumps(row) + "\n"
                for row in rows
                + [
                    {
                        "ts": "2026-08-02T12:05:00Z",
                        "type": event_type,
                        "source": "approvals",
                        "data": None,
                    }
                ]
            )
        )

        _, null_payload = _invoke(plan, events)

        assert null_payload["status"] == "undeterminable"
        assert event_type in " ".join(null_payload["diagnostics"])

    for source_event, evidence_id in (
        (rows[1], "approval-ready"),
        (rows[3], "effect-ready"),
    ):
        for timestamp in (None, "not-a-timestamp"):
            malformed_timestamp = json.loads(json.dumps(source_event))
            malformed_timestamp["data"]["event_id"] = (
                f'{source_event["data"]["event_id"]}-malformed-{timestamp}'
            )
            if timestamp is None:
                malformed_timestamp.pop("ts")
            else:
                malformed_timestamp["ts"] = timestamp
            events.write_text(
                "".join(
                    json.dumps(row) + "\n"
                    for row in rows + [malformed_timestamp]
                )
            )

            _, malformed_timestamp_payload = _invoke(plan, events)

            evidence_row = next(
                row
                for row in malformed_timestamp_payload["verdict"]["requirements"]
                if row["evidence_id"] == evidence_id
            )
            assert evidence_row["result"] == "unknown"
            assert "rejected" in " ".join(evidence_row["diagnostics"])

    malformed_request = {
        "ts": "2026-08-02T12:04:00Z",
        "type": "approval_requested",
        "source": "approvals",
        "data": {
            "work_order_id": "x-delivery",
            "attempt_id": "attempt-1",
            "effect_id": "effect-1",
            "request_id": "request-2",
            "principal_id": "principal-1",
            "effect_class": "communication.external",
            "destination": "email:customer@example.com",
            "action_digest": "action-digest-2",
            "expires_at": "2026-08-03T12:00:00Z",
        },
    }
    events.write_text(
        "".join(json.dumps(row) + "\n" for row in rows + [malformed_request])
    )

    _, malformed_payload = _invoke(plan, events)

    approval_row = next(
        row
        for row in malformed_payload["verdict"]["requirements"]
        if row["evidence_id"] == "approval-ready"
    )
    assert approval_row["result"] == "unknown"
    assert "missing request_digest" in " ".join(approval_row["diagnostics"])


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


def test_ac_d10_err_cross_producer_revisions_are_retained_as_conflict(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    first = json.loads(events.read_text())
    first["data"]["fact_revision"] = "producer-a-revision"
    second = json.loads(json.dumps(first))
    second["data"]["producer"] = "adapter:producer-b"
    second["data"]["fact_revision"] = "producer-b-revision"
    second["data"]["evidence"]["result"] = "failed"
    events.write_text(json.dumps(first) + "\n" + json.dumps(second) + "\n")

    _, payload = _invoke(plan, events)

    assert payload["verdict"]["aggregate"] == "unknown"
    row = payload["verdict"]["requirements"][0]
    assert set(row["producers"]) == {"adapter:test", "adapter:producer-b"}
    assert "conflicting duplicate facts" in json.dumps(payload["verdict"])


def test_ac_d2_err_newer_malformed_effect_state_poisons_old_acknowledgment(
    tmp_path: Path,
) -> None:
    frontmatter = {
        "node": "x-delivery",
        "status": "ready",
        "created": "2026-08-02",
        "completion": "delivery",
        "company_work": {
            "work_order": {"node_id": "x-delivery", "attempt_id": "attempt-1"},
            "deliverables": [{
                "id": "external-send",
                "kind": "arbitrary-output",
                "work_order_id": "x-delivery",
                "attempt_id": "attempt-1",
                "effect_id": "effect-1",
                "required_evidence_ids": ["effect-ready"],
            }],
            "effects": [{
                "id": "effect-1",
                "work_order_id": "x-delivery",
                "attempt_id": "attempt-1",
                "deliverable_id": "external-send",
                "effect_class": "communication.external",
                "destination": "email:customer@example.com",
                "idempotency_key": "effect-key-1",
                "approval_id": "request-digest-1",
            }],
            "evidence": [{
                "id": "effect-ready",
                "work_order_id": "x-delivery",
                "attempt_id": "attempt-1",
                "subject_kind": "effect",
                "subject_id": "effect-1",
                "result": "unknown",
            }],
        },
    }
    plan = tmp_path / "effect-plan.md"
    plan.write_text(f"---\n{yaml.safe_dump(frontmatter, sort_keys=False)}---\n# Plan\n")
    binding = {
        "request_digest": "request-digest-1",
        "work_order_id": "x-delivery",
        "attempt_id": "attempt-1",
        "effect_id": "effect-1",
    }
    request = {
        **binding,
        "request_id": "request-1",
        "principal_id": "principal-1",
        "effect_class": "communication.external",
        "destination": "email:customer@example.com",
        "action_digest": "action-digest-1",
        "expires_at": "2026-08-03T12:00:00Z",
    }
    acknowledged = {
        **binding,
        "idempotency_key": "effect-key-1",
        "state": "acknowledged",
        "previous_state": "executing",
        "external_ref": "message-42",
    }
    malformed_failed = {
        **binding,
        "state": "failed",
        "previous_state": "acknowledged",
    }
    rows = [
        {"ts": "2026-08-02T12:00:00Z", "type": "approval_requested", "source": "approvals", "data": request},
        {"ts": "2026-08-02T12:01:00Z", "type": "effect_state_changed", "source": "approvals", "data": acknowledged},
        {"ts": "2026-08-02T12:02:00Z", "type": "effect_state_changed", "source": "approvals", "data": malformed_failed},
    ]
    events = tmp_path / "effect-events.jsonl"
    events.write_text("".join(json.dumps(row) + "\n" for row in rows))

    _, payload = _invoke(plan, events)

    assert payload["verdict"]["aggregate"] == "unknown"
    row = payload["verdict"]["requirements"][0]
    assert row["result"] == "unknown"
    assert "event:approvals:effect_state_changed" in " ".join(row["diagnostics"])
    assert "missing idempotency_key" in " ".join(row["diagnostics"])


def test_cli_human_output_leads_with_aggregate_and_names_nonpassing_rows(tmp_path: Path) -> None:
    plan = _plan(tmp_path)
    events, _ = _events(tmp_path)
    event = json.loads(events.read_text())
    event["data"]["fresh_until"] = "2026-08-02T12:00:01Z"
    events.write_text(json.dumps(event) + "\n")

    result = runner.invoke(
        app,
        ["do", "delivery", "evaluate", "--plan-path", str(plan), "--events", str(events)],
    )

    assert result.exit_code == 0
    assert result.stdout.splitlines()[0] == "delivery: unknown"
    assert "artifact-ready: unknown" in result.stdout
    assert "adapter:test" in result.stdout
