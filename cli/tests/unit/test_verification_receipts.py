"""Commit-bound verification receipt validation and reduction."""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.events import ValidationError, validate
from fno.pr._preflight import (
    check_verification_evidence,
    hosted_ci_decision,
    hosted_workflow_state,
    verification_decision,
)


SHA = "a" * 40
OTHER_SHA = "b" * 40
PREFLIGHT_SCOPE = [
    "smoke",
    "rustfmt:fno-agents",
    "rustfmt:fno",
    "cargo-test:fno-agents",
    "cargo-test:fno",
    "squads-leak-guard:fno",
]


def receipt(
    *,
    ts: str = "2026-07-26T01:02:04Z",
    candidate_sha: str = SHA,
    mode: str = "full",
    result: str = "passed",
    steps_expected: int = 6,
    steps_executed: int = 6,
) -> dict:
    return {
        "ts": ts,
        "type": "verification_receipt",
        "source": "target",
        "data": {
            "candidate_sha": candidate_sha,
            "command": ["scripts/ci/preflight.sh", "--force"],
            "environment": {
                "host": "builder-1",
                "platform": "Darwin-arm64",
                "runner": "scripts/ci/preflight.sh",
            },
            "scope": PREFLIGHT_SCOPE,
            "started_at": "2026-07-26T01:00:00Z",
            "finished_at": "2026-07-26T01:02:03Z",
            "mode": mode,
            "result": result,
            "producer": {"kind": "preflight", "id": "builder-1:42"},
            "steps_expected": steps_expected,
            "steps_executed": steps_executed,
        },
    }


def write(path: Path, *events: dict) -> None:
    path.write_text("".join(json.dumps(e) + "\n" for e in events))


def test_full_receipt_validates_and_satisfies_exact_candidate(tmp_path: Path) -> None:
    event = receipt()
    validate(event)
    journal = tmp_path / "events.jsonl"
    write(journal, event)

    decision = verification_decision(SHA, [journal])

    assert decision["satisfied"] is True
    assert decision["mode"] == "full"
    assert decision["result"] == "passed"
    assert decision["receipt"]["data"]["candidate_sha"] == SHA


@pytest.mark.parametrize("mode", ["subset", "void", "advisory"])
def test_non_full_modes_never_satisfy(mode: str, tmp_path: Path) -> None:
    event = receipt(mode=mode, result="failed" if mode == "void" else "passed")
    validate(event)
    journal = tmp_path / "events.jsonl"
    write(journal, event)

    decision = verification_decision(SHA, [journal])

    assert decision["satisfied"] is False
    assert decision["mode"] == mode
    assert decision["result"] == event["data"]["result"]


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("candidate_sha", "short"),
        ("command", []),
        ("environment", {}),
        ("scope", []),
        ("started_at", "not-a-time"),
        ("finished_at", "2026-07-25T00:00:00Z"),
        ("mode", "complete"),
        ("result", "green"),
        ("producer", {}),
        ("steps_expected", -1),
        ("steps_executed", 3),
    ],
)
def test_malformed_receipt_rejected(field: str, value: object) -> None:
    event = receipt()
    event["data"][field] = value
    with pytest.raises(ValidationError):
        validate(event)


def test_full_pass_requires_real_steps() -> None:
    for expected, executed in ((0, 0), (2, 0), (2, 1)):
        with pytest.raises(ValidationError):
            validate(receipt(steps_expected=expected, steps_executed=executed))


def test_command_argument_count_is_bounded_across_receipt_contract() -> None:
    event = receipt()
    event["data"]["command"] = ["x"] * 4097
    with pytest.raises(ValidationError):
        validate(event)


def test_type_specific_source_is_enforced() -> None:
    event = receipt()
    event["source"] = "subagent"
    with pytest.raises(ValidationError):
        validate(event)


@pytest.mark.parametrize(
    "mutate",
    [
        lambda event: event.update(source="test"),
        lambda event: event["data"].update(producer={"kind": "untrusted", "id": "x"}),
        lambda event: event["data"].update(command=["true"]),
        lambda event: event["data"].update(scope=[f"fake-{i}" for i in range(6)]),
    ],
)
def test_shape_valid_but_untrusted_receipt_cannot_satisfy_ship_gate(
    mutate, tmp_path: Path
) -> None:
    event = receipt()
    mutate(event)
    validate(event)
    journal = tmp_path / "events.jsonl"
    write(journal, event)

    decision = verification_decision(SHA, [journal])

    assert decision["result"] == "passed"
    assert decision["satisfied"] is False


def test_wrong_sha_is_stale_not_absent(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    write(journal, receipt(candidate_sha=OTHER_SHA))

    decision = verification_decision(SHA, [journal])

    assert decision["satisfied"] is False
    assert decision["result"] == "stale"
    assert decision["receipt"]["data"]["candidate_sha"] == OTHER_SHA


def test_missing_or_corrupt_journal_is_unavailable(tmp_path: Path) -> None:
    missing = verification_decision(SHA, [tmp_path / "missing.jsonl"])
    assert missing["result"] == "unavailable"
    assert missing["satisfied"] is False

    corrupt = tmp_path / "corrupt.jsonl"
    corrupt.write_text("not json\n")
    broken = verification_decision(SHA, [corrupt])
    assert broken["result"] == "unavailable"
    assert broken["coverage"]["malformed_lines"] == 1


def test_corrupt_coverage_fails_closed_even_with_valid_full_pass(tmp_path: Path) -> None:
    journal = tmp_path / "mixed.jsonl"
    journal.write_text("not json\n" + json.dumps(receipt()) + "\n")

    decision = verification_decision(SHA, [journal])

    assert decision["mode"] == "full"
    assert decision["result"] == "passed"
    assert decision["coverage"]["complete"] is False
    assert decision["satisfied"] is False


def test_journals_dedupe_and_select_latest_parsed_timestamp(tmp_path: Path) -> None:
    older = receipt(ts="2026-07-26T01:00:00Z", result="failed")
    latest = receipt(ts="2026-07-26T03:00:00+00:00", result="passed")
    global_log = tmp_path / "global.jsonl"
    delivery_log = tmp_path / "delivery.jsonl"
    write(global_log, latest, older)
    write(delivery_log, older, latest)

    decision = verification_decision(SHA, [global_log, delivery_log])

    assert decision["satisfied"] is True
    assert decision["receipt"]["ts"] == "2026-07-26T03:00:00+00:00"
    assert decision["coverage"]["deduped_events"] == 2


def test_latest_exact_receipt_wins_even_when_file_order_disagrees(tmp_path: Path) -> None:
    passed = receipt(ts="2026-07-26T01:00:00Z")
    pending = receipt(ts="2026-07-26T02:00:00Z", result="pending")
    first = tmp_path / "first.jsonl"
    second = tmp_path / "second.jsonl"
    write(first, pending)
    write(second, passed)

    decision = verification_decision(SHA, [second, first])

    assert decision["satisfied"] is False
    assert decision["result"] == "pending"


def test_delivery_journal_discovery_failure_fails_closed(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    journal = tmp_path / "events.jsonl"
    write(journal, receipt())
    monkeypatch.setattr(
        "fno.pr._preflight.verification_event_paths",
        lambda **_kwargs: ([journal], ["delivery journal discovery failed"]),
    )

    decision = check_verification_evidence(cwd=str(tmp_path), candidate_sha=SHA)

    assert decision["result"] == "passed"
    assert decision["coverage"]["complete"] is False
    assert decision["coverage"]["discovery_errors"] == [
        "delivery journal discovery failed"
    ]
    assert decision["satisfied"] is False


@pytest.mark.parametrize(
    ("declared_none", "workflow_state", "observed_sha", "checks", "result", "satisfied"),
    [
        (True, "absent", None, [], "not_configured", True),
        (False, "absent", None, [], "pending", False),
        (True, "present", None, [], "pending", False),
        (False, "unavailable", None, [], "unavailable", False),
        (False, "present", OTHER_SHA, [], "stale", False),
        (
            False,
            "present",
            None,
            [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
            "unavailable",
            False,
        ),
        (
            False,
            "present",
            SHA,
            [{"name": "ci", "status": "IN_PROGRESS", "conclusion": ""}],
            "pending",
            False,
        ),
        (
            False,
            "present",
            SHA,
            [{"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"}],
            "failed",
            False,
        ),
        (
            False,
            "present",
            SHA,
            [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
            "passed",
            True,
        ),
    ],
)
def test_hosted_ci_states_remain_distinct(
    declared_none: bool,
    workflow_state: str,
    observed_sha: str | None,
    checks: list[dict],
    result: str,
    satisfied: bool,
) -> None:
    decision = hosted_ci_decision(
        declared_none=declared_none,
        workflow_state=workflow_state,
        candidate_sha=SHA,
        observed_sha=observed_sha,
        checks=checks,
    )

    assert decision["result"] == result
    assert decision["satisfied"] is satisfied


def test_hosted_workflow_discovery_distinguishes_absent_present_and_unavailable(
    tmp_path: Path,
) -> None:
    assert hosted_workflow_state(tmp_path) == "absent"
    workflows = tmp_path / ".github" / "workflows"
    workflows.mkdir(parents=True)
    (workflows / "ci.yml").write_text("name: ci\n")
    assert hosted_workflow_state(tmp_path) == "present"
    (workflows / "ci.yml").unlink()
    workflows.rmdir()
    workflows.write_text("not a directory\n")
    assert hosted_workflow_state(tmp_path) == "unavailable"
