"""Tests for the work-item tracker seam (bring-your-own-id foundation).

Covers the five-field read projection, the single close write, the footnote-
owned sidecar roundtrip, and the backend factory. The partition invariant
itself (zero overlap between sidecar and read interface) has its own CI gate
in scripts/ci/check-tracker-partition.sh, exercised in test_partition_gate.py.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.paths import sidecar_path
from fno.tracker import (
    GitHubIssuesTracker,
    GraphTracker,
    NodeNotFound,
    TrackerError,
    TrackerState,
    get_tracker,
)
from fno.tracker import sidecar as sidecar_mod
from fno.tracker.github_backend import parse_github_id
from fno.tracker.sidecar import Sidecar, load, save


def _write_graph(path: Path, entries: list[dict]) -> Path:
    path.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    return path


def test_read_projects_five_fields(tmp_path):
    g = _write_graph(
        tmp_path / "graph.json",
        [{"id": "ab-deadbeef", "title": "Fix login", "plan_path": "/p.md"}],
    )
    node = GraphTracker(path=g).read("ab-deadbeef")
    # Exactly the five-field interface, nothing more.
    assert node.id == "ab-deadbeef"
    assert node.title == "Fix login"
    assert node.state is TrackerState.open
    assert node.parent is None
    assert node.blocked_by == []
    assert set(node.model_fields) == {"id", "title", "state", "parent", "blocked_by"}


def test_read_missing_raises(tmp_path):
    g = _write_graph(tmp_path / "graph.json", [{"id": "ab-deadbeef"}])
    with pytest.raises(NodeNotFound):
        GraphTracker(path=g).read("ab-missing")


def test_state_closed_only_for_terminal_rungs(tmp_path):
    g = _write_graph(
        tmp_path / "graph.json",
        [
            {"id": "ab-done", "completed_at": "2026-01-01T00:00:00Z"},
            {"id": "ab-sup", "superseded_by": "ab-other"},
            {"id": "ab-open", "plan_path": "/p.md"},
        ],
    )
    t = GraphTracker(path=g)
    assert t.read("ab-done").state is TrackerState.closed
    assert t.read("ab-sup").state is TrackerState.closed
    assert t.read("ab-open").state is TrackerState.open


def test_close_sets_completed_and_flips_state(tmp_path):
    g = _write_graph(
        tmp_path / "graph.json",
        [{"id": "ab-deadbeef", "plan_path": "/p.md"}],
    )
    t = GraphTracker(path=g)
    assert t.read("ab-deadbeef").state is TrackerState.open
    t.close("ab-deadbeef")
    # close sets completed_at; the store derives status=done, which projects to
    # closed on the next read. The point of the test: close is observable via
    # the read interface, not by poking at stored fields.
    assert t.read("ab-deadbeef").state is TrackerState.closed


def test_close_missing_raises(tmp_path):
    g = _write_graph(tmp_path / "graph.json", [{"id": "ab-deadbeef"}])
    with pytest.raises(NodeNotFound):
        GraphTracker(path=g).close("ab-missing")


def test_graph_tracker_satisfies_protocol():
    # runtime_checkable: GraphTracker is structurally a NodeTracker.
    from fno.tracker.types import NodeTracker

    assert isinstance(GraphTracker(path=Path("/nonexistent")), NodeTracker)


def test_get_tracker_default_is_graph():
    t = get_tracker()
    assert t.name == "graph"
    assert isinstance(t, GraphTracker)


def test_get_tracker_unknown_backend():
    with pytest.raises(ValueError):
        get_tracker("linear")  # not shipped in the foundation


def test_sidecar_roundtrip(tmp_path, monkeypatch):
    monkeypatch.setattr(sidecar_mod, "sidecar_path", lambda i: tmp_path / f"{i}.json")
    sc = Sidecar(id="ENG-441", cwd="/repo", plan_path="/plan.md", pr_number=7)
    save_path = save(sc)
    loaded = load("ENG-441")
    assert loaded.cwd == "/repo"
    assert loaded.plan_path == "/plan.md"
    assert loaded.pr_number == 7
    assert save_path.exists()


def test_sidecar_path_encodes_separators(monkeypatch, tmp_path):
    # owner/repo#123 contains a path separator and must land as one filename,
    # reusing the claims key encoder. A positive assertion on the encoded name,
    # not an absence: the encoded name contains no raw '/' or '#'.
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    name = sidecar_path("owner/repo#123").name
    assert "/" not in name
    assert "#" not in name
    assert name.endswith(".json")
    assert sidecar_path("owner/repo#123").parent == tmp_path / "sidecar"


# -- list_open (the enumeration `backlog next` ranks) --


def test_list_open_excludes_terminal(tmp_path):
    g = _write_graph(
        tmp_path / "graph.json",
        [
            {"id": "ab-done", "completed_at": "2026-01-01T00:00:00Z"},
            {"id": "ab-open1", "plan_path": "/p.md"},
            {"id": "ab-sup", "superseded_by": "ab-other"},
            {"id": "ab-open2", "plan_path": "/q.md"},
        ],
    )
    open_ids = [n.id for n in GraphTracker(path=g).list_open()]
    assert open_ids == ["ab-open1", "ab-open2"]


# -- GitHub Issues backend --


class _FakeGH(GitHubIssuesTracker):
    """Overrides the one gh I/O method with a scripted response table."""

    def __init__(self, responses):
        super().__init__(default_repo="owner/repo")
        self._responses = responses  # args-tuple -> (rc, out, err)
        self.calls = []

    def _gh(self, args):
        self.calls.append(args)
        # Match on the first two tokens (e.g. ("issue", "view")) + number.
        key = tuple(args[:2])
        return self._responses.get(key, (1, "", "could not resolve to an issue"))


def test_parse_github_id_good_and_bad():
    assert parse_github_id("owner/repo#123") == ("owner", "repo", 123)
    with pytest.raises(ValueError):
        parse_github_id("ENG-441")
    with pytest.raises(ValueError):
        parse_github_id("owner/repo")


def test_github_read_projects():
    t = _FakeGH({("issue", "view"): (0, '{"title":"Fix","state":"OPEN"}', "")})
    node = t.read("owner/repo#123")
    assert node.title == "Fix"
    assert node.state is TrackerState.open
    assert node.parent is None
    assert node.blocked_by == []
    # The view call targeted the right repo/number.
    assert t.calls[0][:3] == ["issue", "view", "123"]
    assert "-R" in t.calls[0] and "owner/repo" in t.calls[0]


def test_github_read_closed_state():
    t = _FakeGH({("issue", "view"): (0, '{"title":"Done","state":"CLOSED"}', "")})
    assert t.read("owner/repo#7").state is TrackerState.closed


def test_github_read_not_found_raises_node_not_found():
    t = _FakeGH({("issue", "view"): (1, "", "could not resolve to an issue")})
    with pytest.raises(NodeNotFound):
        t.read("owner/repo#999")


def test_github_read_infra_failure_raises_tracker_error():
    # A network failure is NOT a not-found; it must surface as TrackerError so
    # callers can degrade rather than treat the item as absent.
    t = _FakeGH({("issue", "view"): (1, "", "connection timed out")})
    with pytest.raises(TrackerError):
        t.read("owner/repo#1")


def test_github_close_invokes_gh_close():
    t = _FakeGH({("issue", "close"): (0, "", "")})
    t.close("owner/repo#42")
    assert t.calls[0][:3] == ["issue", "close", "42"]


def test_github_list_open_needs_repo_scope():
    # Without a default_repo, list_open returns [] (callers fall back) rather
    # than guessing a scope.
    t = GitHubIssuesTracker(default_repo=None)
    assert t.list_open() == []


def test_github_list_open_projects():
    payload = '[{"number":1,"title":"A","state":"OPEN"},{"number":2,"title":"B","state":"OPEN"}]'
    t = _FakeGH({("issue", "list"): (0, payload, "")})
    items = t.list_open()
    assert [n.id for n in items] == ["owner/repo#1", "owner/repo#2"]
    assert items[0].title == "A" and items[0].state is TrackerState.open


def test_get_tracker_github_via_env(monkeypatch):
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    monkeypatch.setenv("FNO_TRACKER_GITHUB_REPO", "owner/repo")
    t = get_tracker()
    assert t.name == "github"
    assert isinstance(t, GitHubIssuesTracker)


def test_github_tracker_satisfies_protocol():
    from fno.tracker.types import NodeTracker

    assert isinstance(GitHubIssuesTracker(), NodeTracker)


# -- verb refusal on an external backend --


def test_add_refuses_on_external_backend(monkeypatch):
    # The guard fires before any graph read: creating work on an external
    # backend belongs to the tracker, not graph.json. add/idea/new all route
    # through _create_node_impl, so this covers the creation class.
    from typer.testing import CliRunner

    from fno.cli import app

    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    result = CliRunner().invoke(app, ["backlog", "add", "phantom work"])
    assert result.exit_code == 1
    assert "github" in result.output
    # The message must point the user at the tracker, not fail opaquely.
    assert "tracker" in result.output.lower()


def test_active_backend_name_default_and_override(monkeypatch):
    from fno.tracker import active_backend_name

    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    assert active_backend_name() == "graph"
    assert active_backend_name("github") == "github"
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    assert active_backend_name() == "github"


