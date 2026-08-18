"""Commit-bound verification receipt validation and reduction."""
from __future__ import annotations

import json
import datetime as dt
from pathlib import Path

import pytest

from fno.events import ValidationError, validate
from fno.pr._preflight import (
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
    generation: int | float = 1,
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


def test_canonical_exact_receipt_ignores_unreadable_optional_mirror(
    tmp_path: Path,
) -> None:
    canonical = tmp_path / "global.jsonl"
    mirror = tmp_path / "delivery.jsonl"
    write(canonical, receipt(generation=4))
    mirror.mkdir()

    decision = verification_decision(SHA, [canonical, mirror])

    assert decision["satisfied"] is True
    assert decision["receipt"]["data"]["generation"] == 4
    assert decision["coverage"]["unreadable_paths"] == 0
    assert decision["coverage"]["unavailable_mirrors"] == 1


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

    decision = verification_decision(SHA, [first, second])

    assert decision["satisfied"] is False
    assert decision["result"] == "pending"


@pytest.mark.parametrize("canonical_state", ["missing", "empty", "wrong-sha"])
def test_mirror_cannot_originate_satisfaction(
    canonical_state: str, tmp_path: Path
) -> None:
    canonical = tmp_path / "global.jsonl"
    mirror = tmp_path / "delivery.jsonl"
    if canonical_state == "empty":
        canonical.write_text("")
    elif canonical_state == "wrong-sha":
        write(canonical, receipt(candidate_sha=OTHER_SHA))
    write(mirror, receipt())

    decision = verification_decision(SHA, [canonical, mirror])

    assert decision["satisfied"] is False
    assert decision["coverage"]["canonical_required"] is True
    assert decision["result"] in {"unavailable", "stale"}


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


def test_void_pending_generation_supersedes_older_pass(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    write(
        journal,
        receipt(generation=1),
        receipt(
            mode="void",
            result="pending",
            generation=2,
            scope=["preflight-execution"],
            steps_expected=1,
            steps_executed=0,
        ),
    )

    decision = verification_decision(SHA, [journal])

    assert decision["satisfied"] is False
    assert decision["mode"] == "void"
    assert decision["result"] == "pending"


@pytest.mark.parametrize("mirror_generation", [100, 100.0])
def test_mirror_ahead_cannot_supersede_canonical_pending(
    mirror_generation: int | float, tmp_path: Path
) -> None:
    canonical = tmp_path / "global.jsonl"
    mirror = tmp_path / "delivery.jsonl"
    write(
        canonical,
        receipt(generation=4),
        receipt(
            mode="void",
            result="pending",
            generation=5,
            scope=["preflight-execution"],
            steps_expected=1,
            steps_executed=0,
        ),
    )
    write(mirror, receipt(generation=mirror_generation))

    decision = verification_decision(SHA, [canonical, mirror])

    assert decision["satisfied"] is False
    assert decision["result"] == "unavailable"
    assert decision["coverage"]["mirror_ahead"] is True


def test_next_generation_uses_every_discovered_exact_sha_journal(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    project = tmp_path / "project.jsonl"
    delivery = tmp_path / "delivery.jsonl"
    project.write_text("")
    write(delivery, receipt(generation=5), receipt(candidate_sha=OTHER_SHA, generation=9))
    monkeypatch.setattr(
        "fno.pr._preflight.verification_event_paths",
        lambda **_kwargs: ([project, delivery], []),
    )

    assert next_verification_generation(cwd=str(tmp_path), candidate_sha=SHA) == 6


def test_next_generation_refuses_mirror_ahead_of_canonical(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    canonical = tmp_path / "global.jsonl"
    mirror = tmp_path / "delivery.jsonl"
    write(canonical, receipt(generation=4))
    write(mirror, receipt(generation=100))
    monkeypatch.setattr(
        "fno.pr._preflight.verification_event_paths",
        lambda **_kwargs: ([canonical, mirror], []),
    )

    with pytest.raises(ValueError, match="mirror generation exceeds"):
        next_verification_generation(cwd=str(tmp_path), candidate_sha=SHA)


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
    monkeypatch.setattr(
        "fno.paths.global_events_json", lambda: global_dir / "events.jsonl"
    )
    monkeypatch.setattr("fno.paths.ledger_json", lambda: ledger)

    assert next_verification_generation(cwd=str(second), candidate_sha=SHA) == 5


def test_fresh_install_without_ledger_starts_generation_one(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setattr(
        "fno.paths.global_events_json", lambda: tmp_path / "global-events.jsonl"
    )
    monkeypatch.setattr("fno.paths.ledger_json", lambda: tmp_path / "ledger.json")

    assert next_verification_generation(cwd=str(tmp_path), candidate_sha=SHA) == 1


@pytest.mark.parametrize(
    "body",
    [
        '{"entries": null}\n',
        '{"wrong": []}\n',
        '"not-a-ledger"\n',
        '[null]\n',
        '{"entries": [{"root_path": false}]}\n',
        '{"entries": [{"root_path": 1}]}\n',
        '{"entries": [{"root_path": "relative-root"}]}\n',
        '{"entries": [{"root_path": "~fno-user-that-does-not-exist-1932"}]}\n',
        '{"entries": [{"canonical_root_path": []}]}\n',
        '{"entries": [{"canonical_root_path": ""}]}\n',
        '{"entries": [{"canonical_root_path": "relative-root"}]}\n',
        '{"entries": [{"canonical_root_path": "~fno-user-that-does-not-exist-1932"}]}\n',
    ],
)
def test_present_structurally_invalid_ledger_fails_closed(
    body: str, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    ledger = tmp_path / "ledger.json"
    ledger.write_text(body)
    monkeypatch.setattr("fno.paths.ledger_json", lambda: ledger)

    with pytest.raises(ValueError, match="ledger"):
        next_verification_generation(cwd=str(tmp_path), candidate_sha=SHA)


def test_missing_and_null_ledger_discovery_fields_are_absent(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": [{}, {"root_path": null, "canonical_root_path": null}]}\n')
    monkeypatch.setattr("fno.paths.ledger_json", lambda: ledger)

    assert next_verification_generation(cwd=str(tmp_path), candidate_sha=SHA) == 1


def test_unavailable_salvage_scan_fails_closed(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    ledger = tmp_path / "ledger.json"
    ledger.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "canonical_root_path": str(tmp_path / "canonical"),
                    }
                ]
            }
        )
    )
    monkeypatch.setattr("fno.paths.ledger_json", lambda: ledger)
    monkeypatch.setattr(
        "fno.pr._preflight.os.scandir",
        lambda _path: (_ for _ in ()).throw(PermissionError("denied")),
    )

    with pytest.raises(ValueError, match="salvage journal discovery failed"):
        next_verification_generation(cwd=str(tmp_path), candidate_sha=SHA)


def test_canonical_generation_ignores_unreadable_optional_mirror(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    canonical = tmp_path / "global.jsonl"
    mirror = tmp_path / "delivery.jsonl"
    write(canonical, receipt(generation=4))
    mirror.mkdir()
    monkeypatch.setattr(
        "fno.pr._preflight.verification_event_paths",
        lambda **_kwargs: ([canonical, mirror], []),
    )

    assert next_verification_generation(cwd=str(tmp_path), candidate_sha=SHA) == 5


def test_independent_checkout_accepts_canonical_global_receipt_without_local_mirror(
    tmp_path: Path,
) -> None:
    clone = tmp_path / "clone-b"
    global_events = tmp_path / "global-events.jsonl"
    write(global_events, receipt(generation=4))

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


def test_reader_succeeds_while_preflight_write_lock_is_held(tmp_path: Path) -> None:
    common = tmp_path / ".git"
    lock = common / ".preflight.lock.d"
    lock.mkdir(parents=True)
    (lock / "holder").write_text("pid=42 sha=" + SHA + "\n")
    journal = tmp_path / "events.jsonl"
    write(journal, receipt())

    decision = check_verification_evidence(
        cwd=str(tmp_path), candidate_sha=SHA, event_paths=[journal]
    )

    assert decision["satisfied"] is True
    assert decision["result"] == "passed"
    assert "lock_error" not in decision["coverage"]


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


def test_root_readme_is_docs_only(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    runner = tmp_path / "scripts" / "ci" / "preflight.sh"
    runner.parent.mkdir(parents=True)
    runner.write_text("#!/bin/sh\n")
    runner.chmod(0o755)

    def readme_git(args, *_args, **_kwargs):
        output = "100755 blob abc\tscripts/ci/preflight.sh\n" if args[0] == "ls-tree" else "README.md\n"
        return type("Result", (), {"returncode": 0, "stdout": output})()

    monkeypatch.setattr("fno.pr._preflight._git", readme_git)
    assert local_verification_required(cwd=str(tmp_path)) == (False, "docs-only")


@pytest.mark.parametrize(
    "runtime_markdown",
    [
        "skills/target/SKILL.md",
        "agents/code-reviewer.md",
        "commands/fno-target.md",
        "AGENTS.md",
    ],
)
def test_runtime_markdown_requires_local_verification(
    runtime_markdown: str, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    runner = tmp_path / "scripts" / "ci" / "preflight.sh"
    runner.parent.mkdir(parents=True)
    runner.write_text("#!/bin/sh\n")
    runner.chmod(0o755)

    def runtime_markdown_git(args, *_args, **_kwargs):
        output = (
            "100755 blob abc\tscripts/ci/preflight.sh\n"
            if args[0] == "ls-tree"
            else f"{runtime_markdown}\n"
        )
        return type("Result", (), {"returncode": 0, "stdout": output})()

    monkeypatch.setattr("fno.pr._preflight._git", runtime_markdown_git)

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


# ---- rebase-equivalent evidence: a receipt survives a rebase ------------------


def _git_ok(repo: Path, *args: str) -> str:
    import subprocess

    proc = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True
    )
    assert proc.returncode == 0, proc.stderr
    return proc.stdout.strip()


def _commit(repo: Path, message: str, fname: str, content: str) -> str:
    (repo / fname).write_text(content)
    _git_ok(repo, "add", fname)
    _git_ok(
        repo,
        "-c",
        "user.email=t@example.com",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "-m",
        message,
    )
    return _git_ok(repo, "rev-parse", "HEAD")


def _rebase_fixture(tmp_path: Path) -> tuple[Path, Path, str]:
    """A feature commit rebased onto a newer main, plus its old-SHA receipt."""
    repo = tmp_path / "repo"
    repo.mkdir()
    _git_ok(repo, "init", "-q")
    _git_ok(repo, "symbolic-ref", "HEAD", "refs/heads/main")
    _git_ok(repo, "config", "user.email", "t@example.com")
    _git_ok(repo, "config", "user.name", "t")
    _commit(repo, "base", "base.txt", "base\n")
    _git_ok(repo, "checkout", "-q", "-b", "feature")
    sha_a = _commit(repo, "a", "a.txt", "a\n")
    journal = tmp_path / "events.jsonl"
    write(journal, receipt(candidate_sha=sha_a))
    _git_ok(repo, "checkout", "-q", "main")
    _commit(repo, "sibling", "sibling.txt", "sibling\n")
    # patch identity is measured against origin/main, like the real check
    _git_ok(
        repo,
        "update-ref",
        "refs/remotes/origin/main",
        _git_ok(repo, "rev-parse", "HEAD"),
    )
    _git_ok(repo, "checkout", "-q", "feature")
    _git_ok(repo, "rebase", "main")
    return repo, journal, sha_a


def test_rebase_equivalent_receipt_satisfies_only_with_the_flag(
    tmp_path: Path,
) -> None:
    repo, journal, sha_a = _rebase_fixture(tmp_path)

    strict = check_verification_evidence(cwd=str(repo), event_paths=[journal])

    assert strict["satisfied"] is False
    assert strict["result"] == "stale"

    equivalent = check_verification_evidence(
        cwd=str(repo), event_paths=[journal], allow_equivalent=True
    )

    assert equivalent["satisfied"] is True
    assert equivalent["result"] == "equivalent"
    assert equivalent["coverage"]["matched_sha"] == sha_a
    assert equivalent["coverage"]["equivalence"] == "patch-id-verbatim"


def test_equivalence_refused_after_a_code_change(tmp_path: Path) -> None:
    repo, journal, _sha_a = _rebase_fixture(tmp_path)
    _commit(repo, "extra", "extra.txt", "extra\n")

    decision = check_verification_evidence(
        cwd=str(repo), event_paths=[journal], allow_equivalent=True
    )

    assert decision["satisfied"] is False
    assert decision["result"] == "stale"


def test_equivalence_refused_after_a_whitespace_only_edit(tmp_path: Path) -> None:
    """The default ``patch-id --stable`` strips whitespace, so the identity
    keys on ``--verbatim`` ids: a re-indent that changes Python semantics
    must not borrow the old receipt. The edit amends the rebased commit so
    HEAD stays a single patch-equal commit; adding a second commit would
    refuse on commit count alone and stay green under ``--stable``."""
    repo, journal, _sha_a = _rebase_fixture(tmp_path)
    (repo / "a.txt").write_text("   a\n")
    _git_ok(repo, "add", "a.txt")
    _git_ok(repo, "commit", "-q", "--amend", "-m", "re-indent")

    decision = check_verification_evidence(
        cwd=str(repo), event_paths=[journal], allow_equivalent=True
    )

    assert decision["satisfied"] is False
    assert decision["result"] == "stale"


def test_equivalence_survives_a_rebase_where_main_touched_the_same_file(
    tmp_path: Path,
) -> None:
    """The identity must key on per-commit patch ids, not net-diff blobs: a
    clean rebase over a main that edited the same file outside the hunk
    context still borrows the pre-rebase receipt."""
    repo = tmp_path / "repo"
    repo.mkdir()
    _git_ok(repo, "init", "-q")
    _git_ok(repo, "symbolic-ref", "HEAD", "refs/heads/main")
    _git_ok(repo, "config", "user.email", "t@example.com")
    _git_ok(repo, "config", "user.name", "t")
    base_lines = [f"l{i}\n" for i in range(1, 21)]
    _commit(repo, "base", "f.txt", "".join(base_lines))
    _git_ok(repo, "checkout", "-q", "-b", "feature")
    feat_lines = list(base_lines)
    feat_lines[0] = "CHANGED\n"
    sha_a = _commit(repo, "feature", "f.txt", "".join(feat_lines))
    journal = tmp_path / "events.jsonl"
    write(journal, receipt(candidate_sha=sha_a))
    _git_ok(repo, "checkout", "-q", "main")
    main_lines = list(base_lines)
    main_lines[19] = "BY-MAIN\n"
    _commit(repo, "main-side edit far from the hunk", "f.txt", "".join(main_lines))
    _git_ok(
        repo,
        "update-ref",
        "refs/remotes/origin/main",
        _git_ok(repo, "rev-parse", "HEAD"),
    )
    _git_ok(repo, "checkout", "-q", "feature")
    _git_ok(repo, "rebase", "main")

    decision = check_verification_evidence(
        cwd=str(repo), event_paths=[journal], allow_equivalent=True
    )

    assert decision["satisfied"] is True
    assert decision["coverage"]["matched_sha"] == sha_a


def test_equivalence_never_rescues_a_pending_receipt_for_head(
    tmp_path: Path,
) -> None:
    """An unfinished verification attempt for HEAD deserves the same
    protection as a fresh red: an interrupted run leaves result pending."""
    repo, journal, sha_a = _rebase_fixture(tmp_path)
    head = _git_ok(repo, "rev-parse", "HEAD")
    write(
        journal,
        receipt(candidate_sha=sha_a, ts="2026-07-26T01:02:04Z"),
        receipt(candidate_sha=head, ts="2026-07-26T02:02:04Z", result="pending"),
    )

    decision = check_verification_evidence(
        cwd=str(repo), event_paths=[journal], allow_equivalent=True
    )

    assert decision["satisfied"] is False
    assert decision["result"] == "pending"


def test_equivalence_never_rescues_a_failed_receipt_for_head(
    tmp_path: Path,
) -> None:
    """A fresh red for HEAD outranks an older green for a patch-equal
    ancestor: the same patches can fail against a newer main."""
    repo, journal, sha_a = _rebase_fixture(tmp_path)
    head = _git_ok(repo, "rev-parse", "HEAD")
    write(
        journal,
        receipt(candidate_sha=sha_a, ts="2026-07-26T01:02:04Z"),
        receipt(candidate_sha=head, ts="2026-07-26T02:02:04Z", result="failed"),
    )

    decision = check_verification_evidence(
        cwd=str(repo), event_paths=[journal], allow_equivalent=True
    )

    assert decision["satisfied"] is False
    assert decision["result"] == "failed"


def test_equivalence_never_rescues_a_failed_receipt_masked_by_canonical_required(
    tmp_path: Path,
) -> None:
    """The aggregate decision can read stale while a failed exact-HEAD
    receipt lives only in a non-canonical journal. The no-rescue rule must
    hold inside the walk, wherever the failed receipt lives."""
    repo, journal, sha_a = _rebase_fixture(tmp_path)
    head = _git_ok(repo, "rev-parse", "HEAD")
    mirror = tmp_path / "mirror.jsonl"
    write(
        mirror,
        receipt(candidate_sha=head, ts="2026-07-26T03:02:04Z", result="failed"),
    )

    decision = check_verification_evidence(
        cwd=str(repo), event_paths=[journal, mirror], allow_equivalent=True
    )

    assert decision["satisfied"] is False


def test_equivalence_blocked_by_incomplete_journal_coverage(
    tmp_path: Path,
) -> None:
    """The strict path refuses on malformed journal lines; the equivalence
    path must not relax that fail-closed rule."""
    repo, journal, sha_a = _rebase_fixture(tmp_path)
    journal.write_text(
        json.dumps(receipt(candidate_sha=sha_a)) + "\nnot-json\n",
        encoding="utf-8",
    )

    decision = check_verification_evidence(
        cwd=str(repo), event_paths=[journal], allow_equivalent=True
    )

    assert decision["satisfied"] is False
    assert decision["coverage"]["complete"] is False


def test_equivalence_refused_when_borrowed_sha_does_not_resolve(
    tmp_path: Path,
) -> None:
    repo, journal, _sha_a = _rebase_fixture(tmp_path)
    write(journal, receipt(candidate_sha="c" * 40))

    decision = check_verification_evidence(
        cwd=str(repo), event_paths=[journal], allow_equivalent=True
    )

    assert decision["satisfied"] is False
    assert decision["result"] == "stale"


def test_equivalence_never_rescues_incomplete_discovery(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    repo, journal, _sha_a = _rebase_fixture(tmp_path)
    monkeypatch.setattr(
        "fno.pr._preflight.verification_event_paths",
        lambda **_kwargs: ([journal], ["delivery journal discovery failed"]),
    )

    decision = check_verification_evidence(
        cwd=str(repo), allow_equivalent=True
    )

    assert decision["satisfied"] is False
