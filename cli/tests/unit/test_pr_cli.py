"""CLI-layer tests for `fno do pr merge` after the in-package port (ab-d4c98550).

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
                "url": "https://github.com/Owner/Repo/pull/930",
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
    result = runner.invoke(app, ["do", "pr", "info", "930", "--repo", "Owner/Repo"])
    assert result.exit_code == 0
    assert json.loads(result.stdout) == {
        "pr": 930,
        "url": "https://github.com/Owner/Repo/pull/930",
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
    result = runner.invoke(app, ["do", "pr", "list", "--repo", "o/r"])
    assert result.exit_code == 0
    assert json.loads(result.stdout)[0]["headRefName"] == "feature/x"


def test_pr_list_exposes_open_node_binding_verdicts(monkeypatch, tmp_path):
    from fno import paths

    graph_path = tmp_path / "graph.json"
    graph_path.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "x-1111", "status": "ready"},
                    {"id": "x-2222", "status": "ready", "pr_number": 931},
                ]
            }
        )
    )
    monkeypatch.setattr(paths, "graph_json", lambda: graph_path)

    from fno.pr import _rest

    monkeypatch.setattr(
        _rest,
        "list_prs_rest",
        lambda slug, **kwargs: (
            [
                {"number": 930, "state": "OPEN", "title": "missing", "headRefName": "feature/x-1111", "url": "https://github.com/o/r/pull/930"},
                {"number": 931, "state": "OPEN", "title": "bound", "headRefName": "feature/x-2222", "url": "https://github.com/o/r/pull/931"},
                {"number": 932, "state": "OPEN", "title": "untracked", "headRefName": "feature/no-node", "url": "https://github.com/o/r/pull/932"},
                {"number": 933, "state": "OPEN", "title": "ambiguous", "headRefName": "feature/x-1111-x-2222", "url": "https://github.com/o/r/pull/933"},
            ],
            "",
        ),
    )

    result = runner.invoke(app, ["do", "pr", "list", "--repo", "o/r"])

    assert result.exit_code == 0
    rows = {row["number"]: row for row in json.loads(result.stdout)}
    assert rows[930]["node_id"] == "x-1111"
    assert rows[930]["node_binding"] == "missing"
    assert rows[931]["node_id"] == "x-2222"
    assert rows[931]["node_binding"] == "bound"
    assert rows[932]["node_id"] is None
    assert rows[932]["node_binding"] == "untracked"
    assert rows[933]["node_id"] is None
    assert rows[933]["node_binding"] == "ambiguous"


def test_pr_list_preserves_rows_when_binding_graph_is_unreadable(monkeypatch, tmp_path):
    from fno import paths

    graph_path = tmp_path / "graph.json"
    graph_path.write_text("not-json")
    monkeypatch.setattr(paths, "graph_json", lambda: graph_path)

    from fno.pr import _rest

    monkeypatch.setattr(
        _rest,
        "list_prs_rest",
        lambda slug, **kwargs: (
            [
                {
                    "number": 930,
                    "state": "OPEN",
                    "title": "still visible",
                    "headRefName": "feature/x-1111",
                    "url": "https://github.com/o/r/pull/930",
                }
            ],
            "",
        ),
    )

    result = runner.invoke(app, ["do", "pr", "list", "--repo", "o/r"])

    assert result.exit_code == 0
    row = json.loads(result.stdout)[0]
    assert row["title"] == "still visible"
    assert "graph binding read failed" in row["node_binding_error"]


def test_graphql_exec_rejects_public_coverage_purpose():
    result = runner.invoke(
        app,
        ["do", "pr", "graphql-exec", "--purpose", "coverage", "--", "pr", "view", "930"],
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
    result = runner.invoke(app, ["do", "pr", "merge", "930"])
    assert result.exit_code == 2
    assert __import__("os").environ["PATH"] == "/real/bin"


def test_pr_help_renders():
    result = runner.invoke(app, ["do", "pr", "--help"])
    assert result.exit_code == 0
    assert "merge" in result.stdout


def test_pr_merge_help_renders():
    result = runner.invoke(app, ["do", "pr", "merge", "--help"])
    assert result.exit_code == 0


def test_pr_merge_dispatches_in_package(monkeypatch):
    """The verb calls _merge.run_merge with the forwarded args and exits its rc."""
    captured = {}

    def _fake(argv, cwd=None):
        captured["argv"] = list(argv)
        return 2

    monkeypatch.setattr(_merge, "run_merge", _fake)
    result = runner.invoke(app, ["do", "pr", "merge", "999999"])
    assert result.exit_code == 2
    assert captured["argv"] == ["999999"]


def test_pr_merge_forwards_legacy_invoker_flag(monkeypatch):
    """x-04ab: a legacy --invoker=... is forwarded verbatim (run_merge ignores it)."""
    captured = {}

    def _fake(argv, cwd=None):
        captured["argv"] = list(argv)
        return 2

    monkeypatch.setattr(_merge, "run_merge", _fake)
    result = runner.invoke(app, ["do", "pr", "merge", "--invoker=target", "42"])
    assert result.exit_code == 2
    assert captured["argv"] == ["--invoker=target", "42"]


def test_closure_trailer_warns_on_a_dropped_malformed_extra_id(monkeypatch, tmp_path):
    """Round-7 review fix: render_closure_trailer silently drops a malformed
    --extra id with no other signal - the CLI layer must warn rather than
    ship the trailer one node short with no operator visibility."""
    from fno import paths

    graph_path = tmp_path / "graph.json"
    graph_path.write_text(json.dumps({"entries": [{"id": "x-1111", "status": "ready"}]}))
    monkeypatch.setattr(paths, "graph_json", lambda: graph_path)

    result = runner.invoke(
        app, ["do", "pr", "closure-trailer", "x-1111", "--extra", "not-an-id"],
    )
    assert result.exit_code == 0
    assert "warning: dropping malformed --extra id(s)" in result.output
    assert "not-an-id" in result.output
    assert "Backlog-Closure: x-1111" in result.output


def test_global_receipt_path_uses_pinned_accessor(monkeypatch, tmp_path):
    from fno import paths

    expected = tmp_path / "events.jsonl"
    monkeypatch.setattr(paths, "global_events_json", lambda: expected)

    result = runner.invoke(app, ["do", "pr", "global-receipt-events-path"])

    assert result.exit_code == 0
    assert result.stdout.strip() == str(expected)
