"""CLI-layer tests for `fno pr merge` after the in-package port (ab-d4c98550).

The merge logic itself is characterized in test_pr_merge.py; here we only
assert the Typer verb dispatches into the in-package _merge module and
propagates its exit code (the old "forwards to pr-merge.sh" assertions are
retired - the bash is gone).
"""
from __future__ import annotations

import json

from typer.testing import CliRunner

from fno.cli import app
from fno.pr import _merge

runner = CliRunner()


def test_pr_info_prints_rest_metadata(monkeypatch):
    from fno.pr import _rest

    monkeypatch.setattr(
        _rest,
        "fetch_pr_info_rest",
        lambda pr, cwd=None, repo=None: (
            {
                "pr": 930,
                "state": "OPEN",
                "head_sha": "abc123",
                "head_ref": "feature/x",
                "base_ref": "main",
                "mergeable": "MERGEABLE",
                "merged_at": None,
            },
            "",
        ),
    )
    result = runner.invoke(app, ["pr", "info", "930", "--repo", "Owner/Repo"])
    assert result.exit_code == 0
    assert json.loads(result.stdout) == {
        "pr": 930,
        "state": "OPEN",
        "head_sha": "abc123",
        "head_ref": "feature/x",
        "base_ref": "main",
        "mergeable": "MERGEABLE",
        "merged_at": None,
    }


def test_pr_list_prints_rest_summaries(monkeypatch):
    from fno.pr import _rest

    monkeypatch.setattr(
        _rest,
        "list_prs_rest",
        lambda slug, **kwargs: (
            [
                {
                    "number": 930,
                    "state": "OPEN",
                    "title": "Quota reserve",
                    "headRefName": "feature/x",
                    "url": "https://github.com/o/r/pull/930",
                }
            ],
            "",
        ),
    )
    result = runner.invoke(app, ["pr", "list", "--repo", "o/r"])
    assert result.exit_code == 0
    assert json.loads(result.stdout)[0]["headRefName"] == "feature/x"


def test_graphql_exec_rejects_public_coverage_purpose():
    result = runner.invoke(
        app,
        ["pr", "graphql-exec", "--purpose", "coverage", "--", "pr", "view", "930"],
    )
    assert result.exit_code == 2


def test_fno_process_routes_subprocesses_through_proxy(monkeypatch):
    monkeypatch.setenv("PATH", "/real/bin")
    monkeypatch.setattr(
        "fno.setup.github_cli.worker_environment",
        lambda base: {**base, "PATH": "/quota-proxy:/real/bin"},
    )

    def _fake(argv, cwd=None):
        assert __import__("os").environ["PATH"] == "/quota-proxy:/real/bin"
        return 2

    monkeypatch.setattr(_merge, "run_merge", _fake)
    result = runner.invoke(app, ["pr", "merge", "930"])
    assert result.exit_code == 2
    assert __import__("os").environ["PATH"] == "/real/bin"


def test_pr_help_renders():
    result = runner.invoke(app, ["pr", "--help"])
    assert result.exit_code == 0
    assert "merge" in result.stdout


def test_pr_merge_help_renders():
    result = runner.invoke(app, ["pr", "merge", "--help"])
    assert result.exit_code == 0


def test_pr_merge_dispatches_in_package(monkeypatch):
    """The verb calls _merge.run_merge with the forwarded args and exits its rc."""
    captured = {}

    def _fake(argv, cwd=None):
        captured["argv"] = list(argv)
        return 2

    monkeypatch.setattr(_merge, "run_merge", _fake)
    result = runner.invoke(app, ["pr", "merge", "999999"])
    assert result.exit_code == 2
    assert captured["argv"] == ["999999"]


def test_pr_merge_forwards_legacy_invoker_flag(monkeypatch):
    """x-04ab: a legacy --invoker=... is forwarded verbatim (run_merge ignores it)."""
    captured = {}

    def _fake(argv, cwd=None):
        captured["argv"] = list(argv)
        return 2

    monkeypatch.setattr(_merge, "run_merge", _fake)
    result = runner.invoke(app, ["pr", "merge", "--invoker=target", "42"])
    assert result.exit_code == 2
    assert captured["argv"] == ["--invoker=target", "42"]


def test_global_receipt_path_uses_pinned_accessor(monkeypatch, tmp_path):
    from fno import paths

    expected = tmp_path / "events.jsonl"
    monkeypatch.setattr(paths, "global_events_json", lambda: expected)

    result = runner.invoke(app, ["pr", "global-receipt-events-path"])

    assert result.exit_code == 0
    assert result.stdout.strip() == str(expected)
