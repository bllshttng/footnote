"""`fno done --pr` never writes a pr_number without a pr_url.

A url-less pr_number names no repo, and PR numbers collide across repos, so any
consumer matching on the bare number can attribute a same-numbered foreign PR.
The writer resolves the url with the reader's own origin-then-gh chain and
refuses the stamp when neither leg resolves.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    for mod, attr, val in (
        (gc, "GRAPH_JSON", g),
        (gc, "GRAPH_MD", tmp_path / "graph.md"),
        (gc, "GRAPH_HTML", tmp_path / "graph.html"),
        (gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock"),
        (gs, "GRAPH_JSON", g),
        (gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock"),
    ):
        monkeypatch.setattr(mod, attr, val)
    g.write_text(json.dumps({"entries": [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ]}, indent=2) + "\n")
    return g


def _first(g: Path) -> dict:
    return json.loads(g.read_text())["entries"][0]


def test_writer_degrades_a_stale_node_cwd_to_the_invocation_cwd():
    """A writer stands in the repo it is stamping, so a recorded cwd that no
    longer exists must not cost it the url. The bulk backfill deliberately does
    NOT share this fallback - see test_maintain."""
    from fno.graph._reconcile import pr_url_for_repo

    seen: list = []

    def runner(argv, cwd):
        seen.append(cwd)
        return (0, "git@github.com:o/r.git\n") if argv[0] == "git" else (1, "")

    url = pr_url_for_repo(5, "/definitely/not/a/dir", runner=runner)

    assert url == "https://github.com/o/r/pull/5"
    assert seen == [None]  # the dead path was dropped, not passed to git


def test_resolved_url_is_written_to_the_node(tmp_graph, monkeypatch):
    import fno.done.cli as done_cli

    monkeypatch.setattr(done_cli, "pr_url_for_repo", lambda pr, cwd=None: f"https://github.com/o/r/pull/{pr}")

    result = runner.invoke(app, ["done", "ab-00000001", "--pr", "123"])

    assert result.exit_code == 0, result.output
    node = _first(tmp_graph)
    assert node["pr_number"] == 123
    assert node["pr_url"] == "https://github.com/o/r/pull/123"


def test_unresolvable_repo_refuses_the_stamp(tmp_graph, monkeypatch):
    import fno.done.cli as done_cli

    monkeypatch.setattr(done_cli, "pr_url_for_repo", lambda pr, cwd=None: None)

    result = runner.invoke(app, ["done", "ab-00000001", "--pr", "123"])

    assert result.exit_code != 0
    # Naming only one remedy leaves the caller stuck.
    assert "gh auth login" in result.output and "--pr-url" in result.output
    assert _first(tmp_graph).get("pr_number") is None


def test_explicit_pr_url_wins_without_derivation(tmp_graph, monkeypatch):
    import fno.done.cli as done_cli

    def _boom(pr, cwd=None):
        raise AssertionError("derivation must not run when --pr-url is supplied")

    monkeypatch.setattr(done_cli, "pr_url_for_repo", _boom)

    result = runner.invoke(app, [
        "done", "ab-00000001", "--pr", "123", "--pr-url", "https://github.com/o/r/pull/123",
    ])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["pr_url"] == "https://github.com/o/r/pull/123"


def test_unparseable_pr_url_is_rejected(tmp_graph):
    result = runner.invoke(app, [
        "done", "ab-00000001", "--pr", "123", "--pr-url", "not-a-url",
    ])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("pr_number") is None


def test_pr_url_without_pr_is_refused(tmp_graph):
    """--note suppresses branch auto-detect, so the url would be dropped silently."""
    result = runner.invoke(app, [
        "done", "ab-00000001", "--pr-url", "https://github.com/o/r/pull/9",
        "--note", "shipped",
    ])

    assert result.exit_code != 0
    node = _first(tmp_graph)
    assert node.get("pr_url") is None
    assert node.get("completion_note") is None
