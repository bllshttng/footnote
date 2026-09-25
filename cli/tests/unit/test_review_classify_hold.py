"""A no-PR review holds its findings on branch and HEAD (the Fixes-line PR reads them).

AC10: `/fno:review <level> --comment` with no PR yet writes the same
`review_attestation` row the Rust publish-review leg reads back when the PR
opens. This pins the no-PR path the Rust leg reads: the journal's newest row
carries the branch, that HEAD, and every finding_key.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner


def _temp_repo(tmp_path: Path) -> Path:
    """A throwaway git repo with a real one-file diff and no network remote."""
    sub = tmp_path / "repo"
    sub.mkdir()
    for args in (
        ["git", "init", "-q", "-b", "feature/held-review"],
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


def _two_findings() -> str:
    return json.dumps(
        [
            {
                "category": "correctness",
                "file": "b0.py",
                "line": 1,
                "summary": "first held bug",
                "failure_scenario": "wrong result",
            },
            {
                "category": "correctness",
                "file": "b1.py",
                "line": 2,
                "summary": "second held bug",
                "failure_scenario": "crash",
            },
        ]
    )


@pytest.fixture
def hold_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """The same hermetic attest environment test_pr_attestation builds."""
    import fno.harness_identity as hi

    repo = _temp_repo(tmp_path)
    monkeypatch.setattr(hi, "resolve_attester_identity", lambda: ("sess-hold", "process"))
    monkeypatch.setattr(
        "fno.paths.global_events_json", lambda: tmp_path / "global-events.jsonl"
    )
    monkeypatch.setattr(
        "fno.claims.core.claim_status", lambda key, root=None: {}
    )
    monkeypatch.setenv("FNO_HOME", str(tmp_path / "fno-home"))
    monkeypatch.chdir(repo)
    return repo


def test_no_pr_review_holds_findings_on_branch_and_head(
    hold_env: Path, tmp_path: Path
) -> None:
    findings = tmp_path / "findings.json"
    findings.write_text(_two_findings(), encoding="utf-8")

    from fno.paths import project_log

    from fno.review.cli import review_app
    from tests._event_rows import event_rows

    result = CliRunner().invoke(
        review_app,
        [
            "classify",
            "--findings-file", str(findings),
            "--emit-record",
            "--attest", "code-review",
        ],
    )
    assert result.exit_code == 0, result.stderr

    journal = project_log("events.jsonl", project_root=hold_env)
    rows = [
        row
        for row in event_rows(journal)
        if row.get("type") == "review_attestation"
    ]
    assert rows, "the classify call must hold a review_attestation row"
    data = rows[-1]["data"]
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=hold_env, check=True, capture_output=True, text=True,
    ).stdout.strip()
    assert data["branch"] == "feature/held-review"
    assert data["head_sha"] == head
    keys = {f.get("finding_key") for f in data.get("findings") or []}
    assert {"b0.py:1:correctness", "b1.py:2:correctness"} <= keys
