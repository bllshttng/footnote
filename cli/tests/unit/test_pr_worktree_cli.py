import sys
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.pr.cli import pr_app


@pytest.mark.parametrize(
    ("command", "args", "handler"),
    [
        (
            "verify",
            ["--kind", "merged", "--pr-number", "42", "--state-file", "state"],
            "fno.pr._verify.run_verify_merged",
        ),
        ("base-lineage-check", ["42"], "fno.pr._base_lineage.run_base_lineage_check"),
        ("merge-result-check", ["42"], "fno.pr._merge_result.run_merge_result_check"),
        ("coverage-check", ["42"], "fno.pr._coverage_gate.run_coverage_check"),
        ("status", ["42"], "fno.pr._status.main"),
    ],
)
def test_pr_commands_from_canonical_use_the_pr_worktree(
    command, args, handler, monkeypatch, tmp_path
):
    canonical = tmp_path / "canonical"
    feature = tmp_path / "feature-worktree"
    canonical.mkdir()
    feature.mkdir()
    monkeypatch.chdir(canonical)
    resolver_calls = []
    handler_calls = []

    def resolve(pr, repo):
        resolver_calls.append((pr, repo))
        return str(feature)

    monkeypatch.setattr("fno.pr._review_hold.resolve_pr_worktree", resolve)
    monkeypatch.setattr(handler, lambda *a, **k: handler_calls.append(Path.cwd()) or 0)
    monkeypatch.setattr(sys, "argv", ["fno", "do", "pr", command, *args])

    result = CliRunner().invoke(pr_app, [command, *args])

    assert result.exit_code == 0, result.exception
    assert resolver_calls == [(42, str(canonical))]
    assert handler_calls == [feature]
