"""Evidence-backed pre-design closure classification."""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest

from fno.graph.relatedness import _resolve_main_sha, _run_main_probe, classify_closure


SHA = "a" * 40
BEHAVIOR = "login accepts a valid session"
COMMAND = ["fno", "test", "cli/tests/unit/test_login.py"]


@pytest.fixture(autouse=True)
def closure_evidence(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("fno.graph.relatedness._resolve_main_sha", lambda _repo: SHA)
    monkeypatch.setattr(
        "fno.graph.relatedness._commit_on_main",
        lambda _repo, commit, main: commit == SHA and main == SHA,
    )
    monkeypatch.setattr(
        "fno.graph.relatedness._commit_names_behavior",
        lambda _repo, commit, behavior: commit == SHA and behavior == BEHAVIOR,
    )
    monkeypatch.setattr(
        "fno.graph.relatedness._run_main_probe",
        lambda _repo, _main, command: {
            "status": "failed" if command == ["false"] else "passed",
            "exit_code": 1 if command == ["false"] else 0,
        }
        if command and all(command)
        else None,
    )


def classify(**kwargs: object) -> dict[str, object]:
    return classify_closure(repo=Path("/repo"), **kwargs)


def test_passing_current_main_probe_proves_already_shipped() -> None:
    result = classify(behavior=BEHAVIOR, probe_command=COMMAND)

    assert result["state"] == "already_shipped"
    assert result["proof"]["kind"] == "current_main_probe"
    assert result["proof"]["head"] == SHA
    assert result["proof"]["command"] == COMMAND
    assert result["proof"]["exit_code"] == 0


def test_classifier_executes_command_instead_of_trusting_claimed_status() -> None:
    result = classify(behavior=BEHAVIOR, probe_command=["false"])

    assert result["state"] == "live"
    assert result["proof"]["result"] == "failed"
    assert result["proof"]["exit_code"] == 1


def test_reachable_named_merged_commit_proves_already_shipped() -> None:
    result = classify(behavior=BEHAVIOR, merged_commit=SHA)

    assert result["state"] == "already_shipped"
    assert result["proof"]["kind"] == "merged_commit"
    assert result["proof"]["commit"] == SHA


def test_unreachable_or_unnamed_merged_commit_is_unknown(monkeypatch: pytest.MonkeyPatch) -> None:
    assert classify(behavior=BEHAVIOR, merged_commit="b" * 40)["state"] == "unknown"
    monkeypatch.setattr("fno.graph.relatedness._commit_names_behavior", lambda *_args: False)
    assert classify(behavior=BEHAVIOR, merged_commit=SHA)["state"] == "unknown"


def test_malformed_or_unavailable_probe_is_unknown(monkeypatch: pytest.MonkeyPatch) -> None:
    assert classify(behavior=BEHAVIOR, probe_command=[])["state"] == "unknown"
    monkeypatch.setattr("fno.graph.relatedness._run_main_probe", lambda *_args: None)
    assert classify(behavior=BEHAVIOR, probe_command=COMMAND)["state"] == "unknown"


def test_absent_behavior_is_unknown() -> None:
    assert classify(behavior="", probe_command=COMMAND, merged_commit=SHA)["state"] == "unknown"


def test_unresolved_main_fails_closed(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("fno.graph.relatedness._resolve_main_sha", lambda _repo: None)
    assert classify(behavior=BEHAVIOR, probe_command=COMMAND)["state"] == "unknown"


def test_live_remote_main_resolution_does_not_trust_local_tracking_ref(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[list[str]] = []

    def fake_run(command, **_kwargs):
        calls.append(command)
        return subprocess.CompletedProcess(command, 0, f"{SHA}\trefs/heads/main\n", "")

    monkeypatch.setattr(subprocess, "run", fake_run)

    assert _resolve_main_sha(Path("/repo")) == SHA
    assert calls == [["git", "ls-remote", "--exit-code", "origin", "refs/heads/main"]]


def test_main_probe_executes_archived_candidate_source(tmp_path: Path) -> None:
    subprocess.run(["git", "init", "-q"], cwd=tmp_path, check=True)
    subprocess.run(["git", "config", "user.email", "test@example.com"], cwd=tmp_path, check=True)
    subprocess.run(["git", "config", "user.name", "Test"], cwd=tmp_path, check=True)
    (tmp_path / "marker.txt").write_text("current-main\n")
    subprocess.run(["git", "add", "marker.txt"], cwd=tmp_path, check=True)
    subprocess.run(["git", "commit", "-qm", BEHAVIOR], cwd=tmp_path, check=True)
    sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=tmp_path, check=True, capture_output=True, text=True
    ).stdout.strip()

    observed = _run_main_probe(
        tmp_path,
        sha,
        [sys.executable, "-c", "from pathlib import Path; assert Path('marker.txt').read_text() == 'current-main\\n'"],
    )

    assert observed == {"status": "passed", "exit_code": 0}
