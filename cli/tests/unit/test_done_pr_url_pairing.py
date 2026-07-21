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


def _runner(remote: str):
    def runner(argv, cwd):
        return (0, remote + "\n") if argv[0] == "git" else (1, "")

    return runner


def test_absent_cwd_resolves_against_the_invocation_checkout():
    """No recorded cwd means no evidence the node lives elsewhere, and the
    caller is standing in the repo it is stamping."""
    from fno.graph._reconcile import pr_url_for_repo

    assert pr_url_for_repo(5, None, runner=_runner("git@github.com:o/r.git")) == (
        "https://github.com/o/r/pull/5"
    )


def test_a_recorded_but_missing_cwd_refuses_rather_than_guessing():
    """A recorded cwd IS evidence the node belongs to another repo, and these
    verbs can name any node in the cross-project graph."""
    from fno.graph._reconcile import pr_url_for_repo

    assert pr_url_for_repo(
        5, "/definitely/not/a/dir", runner=_runner("git@github.com:o/r.git")
    ) is None


@pytest.mark.parametrize("remote,expected", [
    ("git@github.com:o/r.git", "o/r"),
    ("https://github.com/o/r", "o/r"),
    ("https://github.com/o/r.git/", "o/r"),
    ("ssh://git@github.com/o/r.git", "o/r"),
    ("ssh://git@github.com:22/o/r.git", "o/r"),          # port is not the owner
    ("https://user:tok@github.com/o/r.git", "o/r"),
    ("https://gitlab.com/mirrors/github.com/o/r.git", None),  # foreign host
    ("https://notgithub.com/o/r.git", None),
    ("git@github.com:o/r/extra.git", None),              # not owner/repo depth
])
def test_remote_slug_parsing_anchors_the_host(remote, expected):
    """An unanchored match mints a confident slug for a repo the remote does
    not name - and the writer persists it."""
    from fno.graph._reconcile import resolve_current_repo_slug

    assert resolve_current_repo_slug(None, runner=_runner(remote)) == expected


@pytest.mark.parametrize("url,slug,number", [
    ("https://github.com/o/r/pull/12", "o/r", 12),
    ("https://github.com/o/r/pull/12?x=1", "o/r", 12),
    ("https://notgithub.com/o/r/pull/12", None, None),
    ("https://gitlab.com/mirrors/github.com/o/r/pull/12", None, None),
    ("https://github.com/o/r/pull/12suffix", None, None),
])
def test_pr_url_parsing_anchors_the_host_and_the_number(url, slug, number):
    from fno.graph._reconcile import pr_number_from_url, repo_slug_from_url

    assert repo_slug_from_url(url) == slug
    assert pr_number_from_url(url) == number


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


def test_pr_url_naming_a_different_pr_is_refused(tmp_graph):
    """A row pointing at two different PRs matches neither: reconciliation
    queries one number while the link renders another."""
    result = runner.invoke(app, [
        "done", "ab-00000001", "--pr", "123",
        "--pr-url", "https://github.com/o/r/pull/999",
    ])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("pr_number") is None
