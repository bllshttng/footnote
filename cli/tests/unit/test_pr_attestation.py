"""`fno do pr attestation retract` - revocation that addresses the event, not
the identity.

Before the attester binding, the only revocation path was emitting a `fail`
under the SAME env markers as the pass, so undoing an impersonation required
performing it a second time. Once the binding refused that, a forged pass
became permanently irretractable. The verb names the revoked pair explicitly
and records the RETRACTING session's own identity.

Also home to the `classify --attest` writer: the review verb records its own
round, verdict measured from the classified findings, never typed.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.pr.cli import pr_app

HEAD = "ab" * 20


def _pass_line(attester: str = "sess-A", reviewer: str = "code-review") -> str:
    return json.dumps(
        {
            "ts": "2026-08-25T00:00:00Z",
            "type": "review_attestation",
            "source": "target",
            "data": {
                "reviewer": reviewer,
                "head_sha": HEAD,
                "verdict": "pass",
                "session_id": "run-1",
                "attester_session_id": attester,
                "branch": "feature/x",
            },
        }
    )


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture
def journal(tmp_path: Path) -> Path:
    events = tmp_path / ".fno" / "events.jsonl"
    events.parent.mkdir(parents=True)
    events.write_text(_pass_line() + "\n")
    return events


def _invoke(runner: CliRunner, journal: Path, *extra: str):
    return runner.invoke(
        pr_app,
        [
            "attestation",
            "retract",
            "--reviewer", "code-review",
            "--attester", "sess-A",
            "--head", HEAD,
            "--reason", "forged pass",
            "--events", str(journal),
            *extra,
        ],
    )


def test_retract_revokes_the_named_pair_and_records_the_retractor(
    runner: CliRunner, journal: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """An operator session that is NOT sess-A revokes sess-A's pass: coverage's
    key is the NAMED pair, and the event records the operator's own identity."""
    import fno.harness_identity as hi

    monkeypatch.setattr(hi, "resolve_attester_identity", lambda: ("sess-operator", "process"))
    monkeypatch.chdir(tmp_path)

    result = _invoke(runner, journal)
    assert result.exit_code == 0, result.stderr

    lines = journal.read_text().splitlines()
    assert len(lines) == 2
    data = json.loads(lines[1])["data"]
    assert data["verdict"] == "fail"
    assert data["retracts_attester"] == "sess-A"
    assert data["attester_session_id"] == "sess-operator"
    assert data["attester_witness"] == "process"
    assert data["retraction_reason"] == "forged pass"
    assert data["head_sha"] == HEAD


def test_retract_refuses_when_no_matching_pass_exists(
    runner: CliRunner, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No pass for the named triple -> non-zero, says so, writes nothing: a
    revocation must never land for a pair that never passed."""
    import fno.harness_identity as hi

    monkeypatch.setattr(hi, "resolve_attester_identity", lambda: ("sess-operator", "process"))
    # find_pass also scans the machine-global journal; point it at an empty
    # file so the refusal is this project log's, not whatever runs on the host.
    monkeypatch.setattr(
        "fno.paths.global_events_json", lambda: tmp_path / "global-events.jsonl"
    )
    events = tmp_path / ".fno" / "events.jsonl"
    events.parent.mkdir(parents=True)
    events.write_text(_pass_line(attester="sess-SOMEONE-ELSE") + "\n")

    result = _invoke(runner, events)
    assert result.exit_code == 1
    assert "no passing attestation" in result.stderr
    assert len(events.read_text().splitlines()) == 1


def test_retract_refuses_the_identity_override_shape(
    runner: CliRunner, journal: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A retracting process whose env contradicts its ancestry is refused, same
    as a pass: revocation under a forged identity is the same forgery."""
    import fno.harness_identity as hi

    def boom():
        raise hi.AttesterIdentityConflict("CODEX_THREAD_ID", "sess-true", "sess-forged")

    monkeypatch.setattr(hi, "resolve_attester_identity", boom)
    monkeypatch.chdir(tmp_path)

    result = _invoke(runner, journal)
    assert result.exit_code == 1
    assert "sess-true" in result.stderr and "sess-forged" in result.stderr
    assert len(journal.read_text().splitlines()) == 1


def test_retract_mirrors_the_revocation_to_the_global_log(
    tmp_path, monkeypatch, capsys
):
    """The pass being revoked reached the machine-global log on emit
    (review_attestation rides GLOBAL_MIRROR_TYPES); a retraction appended to
    the project log alone leaves the revoked pass live for every global-log
    reader, which is the forgery the verb exists to undo."""
    import json as _json

    from fno.pr import _attestation

    project = tmp_path / ".fno" / "events.jsonl"
    project.parent.mkdir(parents=True)
    head = "aaaa1111bbbb2222"
    project.write_text(
        _json.dumps(
            {
                "ts": "2026-08-25T00:00:00Z",
                "type": "review_attestation",
                "data": {
                    "reviewer": "code-review",
                    "head_sha": head,
                    "verdict": "pass",
                    "attester_session_id": "sess-author",
                    "branch": "feature/x",
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )
    mirrored_rows: list = []

    def fake_mirror(event, resolved_events, repo_root):
        mirrored_rows.append(event)

    import fno.harness_identity

    monkeypatch.setattr(fno.harness_identity, "resolve_attester_identity", lambda: ("s", "process"))
    monkeypatch.setattr("fno.events.cli.mirror_to_global_log", fake_mirror)
    rc = _attestation.retract(
        "code-review", "sess-author", head, "forged", events=project
    )
    capsys.readouterr()
    assert rc == 0
    assert mirrored_rows, "the retraction must be mirrored like the pass it revokes"
    assert mirrored_rows[0]["data"]["retracts_attester"] == "sess-author"


def test_retract_reaches_a_pass_that_lives_only_in_the_global_log(
    tmp_path, monkeypatch, capsys
):
    """A pass can reach the machine-global journal with no project row behind
    it (mirrored from a worktree that was never this checkout's, or appended
    by a forger straight to the one file every reader stands in). find_pass
    must scan BOTH logs or the verb refuses to retract exactly the pass it
    exists to revoke."""
    import json as _json

    from fno.pr import _attestation

    project = tmp_path / ".fno" / "events.jsonl"
    project.parent.mkdir(parents=True)
    project.write_text("", encoding="utf-8")
    head = "cccc3333dddd4444"
    global_log = tmp_path / "global-events.jsonl"
    global_log.write_text(
        _json.dumps(
            {
                "ts": "2026-08-25T00:00:00Z",
                "type": "review_attestation",
                "data": {
                    "reviewer": "code-review",
                    "head_sha": head,
                    "verdict": "pass",
                    "attester_session_id": "sess-mirrored-only",
                    "branch": "feature/x",
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )
    monkeypatch.setattr("fno.paths.global_events_json", lambda: global_log)
    import fno.harness_identity

    monkeypatch.setattr(fno.harness_identity, "resolve_attester_identity", lambda: ("s", "process"))
    monkeypatch.setattr("fno.events.cli.mirror_to_global_log", lambda *a, **k: None)
    rc = _attestation.retract(
        "code-review", "sess-mirrored-only", head, "mirrored-only pass", events=project
    )
    capsys.readouterr()
    assert rc == 0, "a mirrored-only pass must be retractable, not refused"
    assert "retracts_attester" in project.read_text(encoding="utf-8")


# --- classify --attest: the review verb records its own round ----------------


def _temp_repo(tmp_path: Path) -> Path:
    """A throwaway git repo with a real one-file diff over origin/main."""
    sub = tmp_path / "repo"
    sub.mkdir()
    for args in (
        ["git", "init", "-q", "-b", "feature/attest-lane"],
        ["git", "config", "user.email", "t@t.t"],
        ["git", "config", "user.name", "t"],
        ["git", "commit", "-q", "--allow-empty", "-m", "init"],
    ):
        subprocess.run(args, cwd=sub, check=True)
    base = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=sub, check=True, capture_output=True, text=True
    ).stdout.strip()
    subprocess.run(
        ["git", "update-ref", "refs/remotes/origin/main", base], cwd=sub, check=True
    )
    (sub / "a.py").write_text("x = 1\n")
    subprocess.run(["git", "add", "a.py"], cwd=sub, check=True)
    subprocess.run(["git", "commit", "-qm", "feature"], cwd=sub, check=True)
    return sub


def _findings_payload(blocking: int) -> str:
    entries = [
        {
            "category": "correctness",
            "file": f"b{idx}.py",
            "line": idx + 1,
            "summary": f"bug {idx}",
            "failure_scenario": "wrong result",
        }
        for idx in range(blocking)
    ]
    entries.append(
        {
            "category": "nit",
            "file": "n.py",
            "line": 1,
            "summary": "name",
            "failure_scenario": "none",
        }
    )
    return json.dumps(entries)


def _invoke_classify(runner: CliRunner, repo: Path, findings: Path, *extra: str):
    from fno.review.cli import review_app

    return runner.invoke(
        review_app,
        ["classify", "--findings-file", str(findings), *extra],
    )


@pytest.fixture
def attest_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A hermetic attest environment: identity resolved, global log pointed
    into the fixture, claims read quiet, FNO_HOME inside tmp."""
    import fno.harness_identity as hi

    repo = _temp_repo(tmp_path)
    monkeypatch.setattr(hi, "resolve_attester_identity", lambda: ("sess-lane", "process"))
    monkeypatch.setattr(
        "fno.paths.global_events_json", lambda: tmp_path / "global-events.jsonl"
    )
    monkeypatch.setattr(
        "fno.claims.core.claim_status", lambda key, root=None: {}
    )
    monkeypatch.setenv("FNO_HOME", str(tmp_path / "fno-home"))
    monkeypatch.chdir(repo)
    return repo


def test_attest_measures_fail_on_blocking_findings(
    runner: CliRunner, attest_env: Path, tmp_path: Path
) -> None:
    """Two blocking findings -> ONE row with verdict fail on the PR head, the
    classified record riding on it, and the reviewed ranges measured so the
    row contributes a tile to the coverage chain."""
    findings = tmp_path / "findings.json"
    findings.write_text(_findings_payload(blocking=2), encoding="utf-8")
    result = _invoke_classify(runner, attest_env, findings, "--emit-record", "--attest", "code-review")
    assert result.exit_code == 0, result.stderr

    from fno.paths import project_log

    journal = project_log("events.jsonl", project_root=attest_env)
    lines = [ln for ln in journal.read_text().splitlines() if ln.strip()]
    assert len(lines) == 1, "the classify call is the whole emit; no second command"
    event = json.loads(lines[0])
    assert event["type"] == "review_attestation"
    data = event["data"]
    assert data["verdict"] == "fail"
    assert data["findings_blocking"] == 2
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=attest_env, check=True, capture_output=True, text=True
    ).stdout.strip()
    assert data["head_sha"] == head
    base = subprocess.run(
        ["git", "merge-base", "HEAD", "origin/main"],
        cwd=attest_env, check=True, capture_output=True, text=True,
    ).stdout.strip()
    assert data["reviewed_base_sha"] == base
    assert data["reviewed_head_sha"] == head
    assert data["branch"] == "feature/attest-lane"
    assert data["attester_session_id"] == "sess-lane"
    assert data["reviewer_context"] == "unknown"
    assert event["source"] == "test"  # no manifest bound in the fixture


def test_attest_measures_pass_on_zero_blocking_findings(
    runner: CliRunner, attest_env: Path, tmp_path: Path
) -> None:
    """Zero blocking findings -> verdict pass, and a leading-slash reviewer
    name lands stripped, byte-equal to the shell producer's output."""
    findings = tmp_path / "findings.json"
    findings.write_text(_findings_payload(blocking=0), encoding="utf-8")
    result = _invoke_classify(runner, attest_env, findings, "--attest", "/code-review")
    assert result.exit_code == 0, result.stderr

    from fno.paths import project_log

    journal = project_log("events.jsonl", project_root=attest_env)
    data = json.loads(journal.read_text().splitlines()[-1])["data"]
    assert data["verdict"] == "pass"
    assert data["reviewer"] == "code-review"
    assert data["findings_blocking"] == 0


def test_attest_without_the_flag_emits_nothing(
    runner: CliRunner, attest_env: Path, tmp_path: Path
) -> None:
    """Plain classify stays a classification: no row, no journal."""
    findings = tmp_path / "findings.json"
    findings.write_text(_findings_payload(blocking=0), encoding="utf-8")
    result = _invoke_classify(runner, attest_env, findings, "--emit-record")
    assert result.exit_code == 0, result.stderr

    from fno.paths import project_log

    journal = project_log("events.jsonl", project_root=attest_env)
    assert not journal.exists() or journal.read_text().strip() == ""


def test_attest_refuses_off_a_git_repo(
    runner: CliRunner, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No readable head to pin -> exit 3, nothing emitted: a row that pins no
    commit is the one shape downstream can never scope."""
    import fno.harness_identity as hi

    monkeypatch.setattr(hi, "resolve_attester_identity", lambda: ("sess-lane", "process"))
    monkeypatch.chdir(tmp_path)  # not a git repo
    findings = tmp_path / "findings.json"
    findings.write_text(_findings_payload(blocking=0), encoding="utf-8")
    result = _invoke_classify(runner, tmp_path, findings, "--attest", "code-review")
    assert result.exit_code == 3
    assert "no readable head" in result.stderr
