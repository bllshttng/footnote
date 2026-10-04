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
        (
            "verify",
            ["--kind", "merged", "--pr-number=42", "--state-file", "state"],
            "fno.pr._verify.run_verify_merged",
        ),
        ("base-lineage-check", ["42"], "fno.pr._base_lineage.run_base_lineage_check"),
        ("merge-result-check", ["42"], "fno.pr._merge_result.run_merge_result_check"),
        ("coverage-check", ["42"], "fno.pr._coverage_gate.run_coverage_check"),
        ("status", ["42"], "fno.pr.cli._forward_to_binary"),
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

    # No local worktree on the PR branch: the command runs from the caller's
    # checkout instead of dying in a chdir traceback. The first invoke left
    # the process inside the feature worktree, so go back first.
    monkeypatch.setattr("fno.pr._review_hold.resolve_pr_worktree", lambda pr, repo: "")
    monkeypatch.chdir(canonical)
    handler_calls.clear()
    monkeypatch.setattr(sys, "argv", ["fno", "do", "pr", command, *args])

    result = CliRunner().invoke(pr_app, [command, *args])

    assert result.exit_code == 0, result.exception
    assert handler_calls == [canonical]


def test_hold_check_repo_option_from_canonical_uses_pr_worktree(
    monkeypatch, tmp_path
):
    canonical = tmp_path / "canonical"
    feature = tmp_path / "feature-worktree"
    canonical.mkdir()
    feature.mkdir()
    monkeypatch.chdir(canonical)
    resolver_calls = []
    hold_calls = []
    monkeypatch.setattr(
        "fno.pr._review_hold.resolve_pr_worktree",
        lambda pr, repo: resolver_calls.append((pr, repo)) or str(feature),
    )
    monkeypatch.setattr(
        "fno.pr._hold.merge_hold_reason",
        lambda pr, repo: hold_calls.append((pr, Path(repo))) or None,
    )
    monkeypatch.setattr(
        sys, "argv", ["fno", "do", "pr", "hold-check", "42", "--repo", str(canonical)]
    )

    result = CliRunner().invoke(
        pr_app, ["hold-check", "42", "--repo", str(canonical)]
    )

    assert result.exit_code == 0, result.exception
    assert resolver_calls == [(42, str(canonical))]
    assert hold_calls == [(42, feature)]

    # No local worktree on the PR branch: the hold reader is graph-resident,
    # so it is still asked - from the caller's checkout, never skipped.
    monkeypatch.setattr("fno.pr._review_hold.resolve_pr_worktree", lambda pr, repo: "")
    hold_calls.clear()
    monkeypatch.setattr(
        sys, "argv", ["fno", "do", "pr", "hold-check", "42", "--repo", str(canonical)]
    )

    result = CliRunner().invoke(
        pr_app, ["hold-check", "42", "--repo", str(canonical)]
    )

    assert result.exit_code == 0, result.exception
    assert hold_calls == [(42, canonical)]
    assert "no plan dispatch hold" in result.output
