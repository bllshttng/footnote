"""Commit-bound verification receipt validation and reduction."""
from __future__ import annotations

import json
import datetime as dt
from pathlib import Path

import pytest

from fno.events import ValidationError, validate
from fno.pr._preflight import (
    _preflight_revocation,
    check_verification_evidence,
    hosted_ci_decision,
    hosted_workflow_state,
    local_verification_required,
    next_verification_generation,
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
    generation: int = 1,
    scope: list[str] | None = None,
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
            "scope": PREFLIGHT_SCOPE if scope is None else scope,
            "started_at": "2026-07-26T01:00:00Z",
            "finished_at": "2026-07-26T01:02:03Z",
            "mode": mode,
            "result": result,
            "producer": {"kind": "preflight", "id": "builder-1:42"},
            "generation": generation,
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
        ("generation", 0),
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
    older = receipt(ts="2026-07-26T01:00:00Z", result="failed", generation=1)
    latest = receipt(ts="2026-07-26T03:00:00+00:00", result="passed", generation=2)
    global_log = tmp_path / "global.jsonl"
    delivery_log = tmp_path / "delivery.jsonl"
    write(global_log, latest, older)
    write(delivery_log, older, latest)

    decision = verification_decision(SHA, [global_log, delivery_log])

    assert decision["satisfied"] is True
    assert decision["receipt"]["ts"] == "2026-07-26T03:00:00+00:00"
    assert decision["coverage"]["deduped_events"] == 2


def test_latest_exact_receipt_wins_even_when_file_order_disagrees(tmp_path: Path) -> None:
    passed = receipt(ts="2026-07-26T01:00:00Z", generation=1)
    pending = receipt(ts="2026-07-26T02:00:00Z", result="pending", generation=2)
    first = tmp_path / "first.jsonl"
    second = tmp_path / "second.jsonl"
    write(first, pending)
    write(second, passed)

    decision = verification_decision(SHA, [second, first])

    assert decision["satisfied"] is False
    assert decision["result"] == "pending"


def test_equal_timestamp_conflict_is_unavailable_not_lexical_green(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    write(journal, receipt(result="failed"), receipt(result="passed"))

    decision = verification_decision(SHA, [journal])

    assert decision["satisfied"] is False
    assert decision["result"] == "unavailable"
    assert decision["coverage"]["conflicting_latest"] == 2


def test_generation_supersedes_timestamp_after_clock_rollback(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    write(
        journal,
        receipt(ts="2026-07-26T03:00:00Z", result="passed", generation=1),
        receipt(ts="2026-07-26T02:00:00Z", result="failed", generation=2),
    )

    decision = verification_decision(SHA, [journal])

    assert decision["satisfied"] is False
    assert decision["result"] == "failed"
    assert decision["receipt"]["data"]["generation"] == 2


def test_next_generation_uses_every_discovered_exact_sha_journal(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    project = tmp_path / "project.jsonl"
    delivery = tmp_path / "delivery.jsonl"
    write(project, receipt(generation=2))
    write(delivery, receipt(generation=5), receipt(candidate_sha=OTHER_SHA, generation=9))
    monkeypatch.setattr(
        "fno.pr._preflight.verification_event_paths",
        lambda **_kwargs: ([project, delivery], []),
    )

    assert next_verification_generation(cwd=str(tmp_path), candidate_sha=SHA) == 6


def test_next_generation_fails_closed_on_uncertain_journal(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    corrupt = tmp_path / "events.jsonl"
    corrupt.write_text("not json\n")
    monkeypatch.setattr(
        "fno.pr._preflight.verification_event_paths",
        lambda **_kwargs: ([corrupt], []),
    )

    with pytest.raises(ValueError, match="malformed"):
        next_verification_generation(cwd=str(tmp_path), candidate_sha=SHA)


def test_unregistered_checkouts_share_the_global_generation_floor(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    first = tmp_path / "clone-a"
    second = tmp_path / "clone-b"
    first.mkdir()
    second.mkdir()
    global_dir = tmp_path / "global"
    global_dir.mkdir()
    write(global_dir / "events.jsonl", receipt(generation=4))
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries":[]}\n')
    monkeypatch.setattr("fno.paths.state_dir", lambda: global_dir)
    monkeypatch.setattr("fno.paths.ledger_json", lambda: ledger)

    assert next_verification_generation(cwd=str(second), candidate_sha=SHA) == 5


def test_independent_checkout_accepts_canonical_global_receipt_without_local_mirror(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    clone = tmp_path / "clone-b"
    common = clone / ".git"
    common.mkdir(parents=True)
    global_events = tmp_path / "global-events.jsonl"
    write(global_events, receipt(generation=4))
    monkeypatch.setattr("fno.pr._preflight._git_common_dir", lambda _repo: common)

    decision = check_verification_evidence(
        cwd=str(clone), candidate_sha=SHA, event_paths=[global_events]
    )

    assert decision["satisfied"] is True
    assert decision["receipt"]["data"]["generation"] == 4


def test_next_generation_reports_non_object_receipt_data(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    malformed = tmp_path / "events.jsonl"
    malformed.write_text(
        json.dumps(
            {
                "ts": "2026-07-26T01:00:00Z",
                "type": "verification_receipt",
                "source": "target",
                "data": [],
            }
        )
        + "\n"
    )
    monkeypatch.setattr(
        "fno.pr._preflight.verification_event_paths",
        lambda **_kwargs: ([malformed], []),
    )

    with pytest.raises(ValueError, match="data must be an object"):
        next_verification_generation(cwd=str(tmp_path), candidate_sha=SHA)


def test_future_dated_receipt_fails_closed(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    write(journal, receipt(ts="2099-01-01T00:00:00Z"))

    decision = verification_decision(SHA, [journal])

    assert decision["satisfied"] is False
    assert decision["coverage"]["malformed_lines"] == 1


def test_future_pass_cannot_shadow_later_appended_failure(tmp_path: Path) -> None:
    now = dt.datetime.now(dt.timezone.utc)
    future = (now + dt.timedelta(minutes=4)).isoformat().replace("+00:00", "Z")
    current = now.isoformat().replace("+00:00", "Z")
    journal = tmp_path / "events.jsonl"
    write(journal, receipt(ts=future), receipt(ts=current, result="failed"))

    decision = verification_decision(SHA, [journal])

    assert decision["satisfied"] is False
    assert decision["result"] == "failed"
    assert decision["coverage"]["malformed_lines"] == 1


def test_optional_squads_guard_absence_keeps_actual_five_step_scope_eligible(
    tmp_path: Path,
) -> None:
    scope = PREFLIGHT_SCOPE[:-1]
    event = receipt(scope=scope, steps_expected=5, steps_executed=5)
    journal = tmp_path / "events.jsonl"
    write(journal, event)

    assert verification_decision(SHA, [journal])["satisfied"] is True


def test_delivery_journal_discovery_failure_fails_closed(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    common = tmp_path / ".git"
    common.mkdir()
    monkeypatch.setattr("fno.pr._preflight._git_common_dir", lambda _repo: common)
    monkeypatch.setattr(
        "fno.pr._preflight._preflight_revocation",
        lambda _repo, _sha: ("absent", None),
    )
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


def test_live_revocation_blocks_older_pass(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    common = tmp_path / ".git"
    common.mkdir()
    monkeypatch.setattr("fno.pr._preflight._git_common_dir", lambda _repo: common)
    journal = tmp_path / "events.jsonl"
    write(journal, receipt())
    monkeypatch.setattr(
        "fno.pr._preflight._preflight_revocation",
        lambda _repo, _sha: ("revoked", None),
    )

    decision = check_verification_evidence(
        cwd=str(tmp_path), candidate_sha=SHA, event_paths=[journal]
    )

    assert decision["result"] == "passed"
    assert decision["coverage"]["revoked"] is True
    assert decision["satisfied"] is False


def test_unavailable_revocation_state_fails_closed(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    common = tmp_path / ".git"
    common.mkdir()
    monkeypatch.setattr("fno.pr._preflight._git_common_dir", lambda _repo: common)
    journal = tmp_path / "events.jsonl"
    write(journal, receipt())
    monkeypatch.setattr(
        "fno.pr._preflight._preflight_revocation",
        lambda _repo, _sha: ("unavailable", "marker unreadable"),
    )

    decision = check_verification_evidence(
        cwd=str(tmp_path), candidate_sha=SHA, event_paths=[journal]
    )

    assert decision["coverage"]["complete"] is False
    assert decision["coverage"]["revocation_error"] == "marker unreadable"
    assert decision["satisfied"] is False


def test_reader_refuses_during_preflight_transition(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    common = tmp_path / ".git"
    lock = common / ".preflight.lock.d"
    lock.mkdir(parents=True)
    (lock / "holder").write_text("pid=42 sha=" + SHA + "\n")
    monkeypatch.setattr("fno.pr._preflight._git_common_dir", lambda _repo: common)
    journal = tmp_path / "events.jsonl"
    write(journal, receipt())

    decision = check_verification_evidence(
        cwd=str(tmp_path), candidate_sha=SHA, event_paths=[journal]
    )

    assert decision["satisfied"] is False
    assert decision["result"] == "unavailable"
    assert decision["coverage"]["lock_error"] == "preflight transition in progress"


def test_reader_release_failure_invalidates_verdict(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    common = tmp_path / ".git"
    common.mkdir()
    monkeypatch.setattr("fno.pr._preflight._git_common_dir", lambda _repo: common)
    monkeypatch.setattr(
        "fno.pr._preflight._preflight_revocation",
        lambda _repo, _sha: ("absent", None),
    )
    monkeypatch.setattr(
        Path,
        "rmdir",
        lambda _path: (_ for _ in ()).throw(OSError("simulated release failure")),
    )
    journal = tmp_path / "events.jsonl"
    write(journal, receipt())

    decision = check_verification_evidence(
        cwd=str(tmp_path), candidate_sha=SHA, event_paths=[journal]
    )

    assert decision["satisfied"] is False
    assert decision["result"] == "unavailable"
    assert "release failed" in decision["coverage"]["lock_error"]


def test_reader_partial_acquisition_cleanup_failure_is_reported(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    common = tmp_path / ".git"
    common.mkdir()
    monkeypatch.setattr("fno.pr._preflight._git_common_dir", lambda _repo: common)
    original_write = Path.write_text

    def fail_holder_write(path: Path, *args, **kwargs):
        if path.name == "holder":
            raise OSError("simulated holder write failure")
        return original_write(path, *args, **kwargs)

    monkeypatch.setattr(Path, "write_text", fail_holder_write)
    monkeypatch.setattr(
        Path,
        "rmdir",
        lambda _path: (_ for _ in ()).throw(OSError("simulated cleanup failure")),
    )

    decision = check_verification_evidence(cwd=str(tmp_path), candidate_sha=SHA)

    assert decision["result"] == "unavailable"
    assert "partial acquisition cleanup failed" in decision["coverage"]["lock_error"]


def test_revocation_markers_are_candidate_scoped(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    common = tmp_path / ".git"
    directory = common / ".preflight-revoked"
    directory.mkdir(parents=True)
    (directory / SHA).write_text(SHA + "\n")
    (directory / OTHER_SHA).write_text(OTHER_SHA + "\n")
    monkeypatch.setattr("fno.pr._preflight._git_common_dir", lambda _repo: common)

    assert _preflight_revocation(str(tmp_path), SHA) == ("revoked", None)
    assert _preflight_revocation(str(tmp_path), OTHER_SHA) == ("revoked", None)


def test_malformed_revocation_marker_is_unavailable(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    common = tmp_path / ".git"
    marker = common / ".preflight-revoked" / SHA
    marker.parent.mkdir(parents=True)
    marker.write_text("not-a-sha\n")
    monkeypatch.setattr("fno.pr._preflight._git_common_dir", lambda _repo: common)

    state, error = _preflight_revocation(str(tmp_path), SHA)

    assert state == "unavailable"
    assert error == "revocation marker is malformed"


def test_broken_revocation_symlink_is_unavailable(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    common = tmp_path / ".git"
    common.mkdir()
    (common / ".preflight-revoked").symlink_to(tmp_path / "missing-target")
    monkeypatch.setattr("fno.pr._preflight._git_common_dir", lambda _repo: common)

    state, error = _preflight_revocation(str(tmp_path), SHA)

    assert state == "unavailable"
    assert error is not None


def test_local_verification_policy_preserves_explicit_exemptions(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    runner = tmp_path / "scripts" / "ci" / "preflight.sh"
    runner.parent.mkdir(parents=True)
    runner.write_text("#!/bin/sh\n")
    runner.chmod(0o755)

    def docs_git(args, *_args, **_kwargs):
        output = "100755 blob abc\tscripts/ci/preflight.sh\n" if args[0] == "ls-tree" else "docs/readme.md\n"
        return type("Result", (), {"returncode": 0, "stdout": output})()

    monkeypatch.setattr("fno.pr._preflight._git", docs_git)
    assert local_verification_required(cwd=str(tmp_path)) == (False, "docs-only")
    assert local_verification_required(
        cwd=str(tmp_path), env={"FNO_SKIP_PREFLIGHT": "1"}
    ) == (False, "explicit-skip")

    def code_git(args, *_args, **_kwargs):
        output = "100755 blob abc\tscripts/ci/preflight.sh\n" if args[0] == "ls-tree" else "cli/code.py\n"
        return type("Result", (), {"returncode": 0, "stdout": output})()

    monkeypatch.setattr("fno.pr._preflight._git", code_git)
    assert local_verification_required(cwd=str(tmp_path)) == (True, "required")


def test_base_configured_runner_removal_requires_verification(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setattr(
        "fno.pr._preflight._git",
        lambda *_args, **_kwargs: type(
            "Result",
            (),
            {
                "returncode": 0,
                "stdout": "100755 blob abc\tscripts/ci/preflight.sh\n",
            },
        )(),
    )

    assert local_verification_required(cwd=str(tmp_path)) == (True, "runner-removed")


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
