"""`fno do pr attestation retract` - revocation that addresses the event, not
the identity.

Before the attester binding, the only revocation path was emitting a `fail`
under the SAME env markers as the pass, so undoing an impersonation required
performing it a second time. Once the binding refused that, a forged pass
became permanently irretractable. The verb names the revoked pair explicitly
and records the RETRACTING session's own identity.
"""
from __future__ import annotations

import json
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
