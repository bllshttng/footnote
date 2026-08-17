"""Tests for the work-item tracker seam (bring-your-own-id foundation).

Covers the five-field read projection, the single close write, the footnote-
owned sidecar roundtrip, the backend factory, and the backend-selected sidecar
store (graph projection vs external per-id file). The partition invariant
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
    TrackerCandidate,
    TrackerError,
    TrackerNode,
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
    assert set(TrackerNode.model_fields) == {"id", "title", "state", "parent", "blocked_by"}


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


def test_sidecar_roundtrip(tmp_path, monkeypatch, external_mode):
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


def test_sidecar_rejects_tracker_owned_field():
    # The static partition gate checks declared field names. extra="forbid" is
    # the runtime backstop: a tracker-owned field (title, state, priority) must
    # fail at construction, not persist a forbidden second copy. A positive
    # assertion that the rejection fires, not an absence.
    from pydantic import ValidationError

    with pytest.raises(ValidationError):
        Sidecar(id="ab-deadbeef", title="leaked tracker field")
    with pytest.raises(ValidationError):
        Sidecar(id="ab-deadbeef", priority="p1")


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


def test_list_open_returns_candidates_with_ordering_inputs(tmp_path):
    # The selection projection: every open item carries priority / rank /
    # created_at (the inputs make_selection_sort_key reads off the candidate)
    # while read() stays five-field. Positive assertions on each widened field.
    g = _write_graph(
        tmp_path / "graph.json",
        [
            {
                "id": "ab-1", "plan_path": "/p.md", "priority": "p0",
                "rank": 2.0, "created_at": "2026-01-02T00:00:00Z",
                "cwd": "/repo", "pr_number": 9,
            },
            {"id": "ab-2", "plan_path": "/q.md"},
        ],
    )
    by_id = {c.id: c for c in GraphTracker(path=g).list_open()}
    assert set(by_id) == {"ab-1", "ab-2"}
    assert isinstance(by_id["ab-1"], TrackerCandidate)
    assert by_id["ab-1"].priority == "p0"
    assert by_id["ab-1"].rank == 2.0
    assert by_id["ab-1"].created_at == "2026-01-02T00:00:00Z"
    # Absent entry values fall back to the projection defaults, and the
    # sidecar-only fields (cwd, pr_number) stay OFF the candidate.
    assert by_id["ab-2"].priority == "p2"
    assert by_id["ab-2"].rank is None
    assert "cwd" not in TrackerCandidate.model_fields
    assert "pr_number" not in TrackerCandidate.model_fields


def test_candidate_ordering_reproduces_priority_rank_recency(tmp_path):
    # Distinct ordering inputs produce a distinct, reproducible order once
    # footnote's sort key runs over candidates - the parity property AC5-EDGE
    # leans on. Rank beats priority; priority beats created_at.
    from fno.graph._intake import make_selection_sort_key

    g = _write_graph(
        tmp_path / "graph.json",
        [
            {"id": "ab-lo", "plan_path": "/a.md", "priority": "p3",
             "created_at": "2026-01-01T00:00:00Z"},
            {"id": "ab-hi", "plan_path": "/b.md", "priority": "p1",
             "created_at": "2026-03-01T00:00:00Z"},
            {"id": "ab-ranked", "plan_path": "/c.md", "priority": "p3",
             "rank": 1.0, "created_at": "2026-02-01T00:00:00Z"},
        ],
    )
    cands = GraphTracker(path=g).list_open()
    as_entries = [c.model_dump() for c in cands]
    ordered = sorted(as_entries, key=make_selection_sort_key(as_entries))
    assert [e["id"] for e in ordered] == ["ab-ranked", "ab-hi", "ab-lo"]


# -- sidecar store selection (graph projection vs external per-id file) --


@pytest.fixture
def graph_mode(monkeypatch):
    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)


@pytest.fixture
def external_mode(monkeypatch):
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")


def test_sidecar_graph_mode_projects_from_entry(tmp_path, monkeypatch, graph_mode):
    g = _write_graph(
        tmp_path / "graph.json",
        [{
            "id": "ab-1", "cwd": "/repo", "plan_path": "/p.md",
            "pr_number": 7, "cost_usd": 1.5, "claimed_at": "2026-01-01T00:00:00Z",
            "batch": "batch-1", "contained_in": "ab-0",
            "title": "tracker-owned, must not cross",
        }],
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    sc = load("ab-1")
    assert sc.cwd == "/repo"
    assert sc.plan_path == "/p.md"
    assert sc.pr_number == 7
    assert sc.cost_usd == 1.5
    assert sc.claimed_at == "2026-01-01T00:00:00Z"
    assert sc.batch == "batch-1"
    assert sc.contained_in == "ab-0"
    # Tracker-owned fields never ride the sidecar projection.
    assert not hasattr(sc, "title")


def test_sidecar_graph_mode_roundtrips_through_entry(tmp_path, monkeypatch, graph_mode):
    g = _write_graph(
        tmp_path / "graph.json",
        [{"id": "ab-1", "cwd": "/old", "plan_path": "/p.md"}],
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setattr(sidecar_mod, "sidecar_path", lambda i: tmp_path / "sidecars" / f"{i}.json")
    sc = load("ab-1")
    sc.cwd = "/new"
    sc.pr_number = 42
    returned = save(sc)
    # Graph mode returns the graph path and updates the entry in place...
    assert returned == g
    entries = json.loads(g.read_text())["entries"]
    entry = next(e for e in entries if e["id"] == "ab-1")
    assert entry["cwd"] == "/new"
    assert entry["pr_number"] == 42
    assert entry["plan_path"] == "/p.md"
    # ...and never creates a per-id sidecar file (one physical owner; plan Risk 1).
    assert not (tmp_path / "sidecars" / "ab-1.json").exists()


def test_sidecar_graph_mode_missing_id_is_empty(tmp_path, monkeypatch, graph_mode):
    g = _write_graph(tmp_path / "graph.json", [{"id": "ab-1"}])
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    sc = load("ab-missing")
    assert sc == Sidecar(id="ab-missing")


def test_sidecar_graph_mode_save_missing_id_raises(tmp_path, monkeypatch, graph_mode):
    g = _write_graph(tmp_path / "graph.json", [{"id": "ab-1"}])
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    with pytest.raises(NodeNotFound):
        save(Sidecar(id="ab-ghost", cwd="/nowhere"))


def test_sidecar_external_mode_never_reads_the_graph(
    tmp_path, monkeypatch, external_mode
):
    # Contradictory sentinel (plan Verification step 7): the graph file carries
    # one cwd, the per-id sidecar file another. External mode must return the
    # sidecar sentinel - positive evidence it never fell back to the graph.
    g = _write_graph(
        tmp_path / "graph.json",
        [{"id": "EXT-1", "cwd": "/graph-sentinel", "plan_path": "/graph.md"}],
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    sidecars = tmp_path / "sidecars"
    sidecars.mkdir()
    (sidecars / "EXT-1.json").write_text(
        json.dumps({"id": "EXT-1", "cwd": "/external-sentinel",
                    "plan_path": "/external.md"}),
        encoding="utf-8",
    )
    monkeypatch.setattr(sidecar_mod, "sidecar_path", lambda i: sidecars / f"{i}.json")
    sc = load("EXT-1")
    assert sc.cwd == "/external-sentinel"
    assert sc.plan_path == "/external.md"
    sc.pr_number = 5
    path = save(sc)
    assert path == sidecars / "EXT-1.json"
    # The graph file is byte-identical: external mode never wrote through it.
    assert json.loads(g.read_text())["entries"][0]["cwd"] == "/graph-sentinel"


def test_sidecar_external_mode_missing_file_is_empty(tmp_path, monkeypatch, external_mode):
    monkeypatch.setattr(
        sidecar_mod, "sidecar_path", lambda i: tmp_path / "sidecars" / f"{i}.json"
    )
    assert load("EXT-new") == Sidecar(id="EXT-new")


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


# -- github backend contract hardening (exceptions stay in the contract) --


def test_github_gh_missing_binary_raises_tracker_error(monkeypatch):
    # FileNotFoundError (no gh on PATH) must surface as TrackerError, not escape
    # past the NodeTracker contract a caller degrades on.
    import fno.tracker.github_backend as ghmod

    def _boom(*a, **k):
        raise FileNotFoundError("gh")

    monkeypatch.setattr(ghmod.subprocess, "run", _boom)
    with pytest.raises(TrackerError):
        GitHubIssuesTracker(default_repo="o/r").read("o/r#1")


def test_github_read_non_json_raises_tracker_error():
    # rc=0 with non-JSON stdout (a gh regression or truncated pipe) is a backend
    # fault, not a JSONDecodeError escaping the contract.
    t = _FakeGH({("issue", "view"): (0, "not json at all", "")})
    with pytest.raises(TrackerError):
        t.read("owner/repo#5")


def test_github_bad_id_raises_tracker_error():
    # parse_github_id raises ValueError; the read/close boundary must convert it
    # to TrackerError so callers keyed on the contract degrade, not crash.
    with pytest.raises(TrackerError):
        GitHubIssuesTracker().read("ENG-441")
    with pytest.raises(TrackerError):
        GitHubIssuesTracker().close("not-a-github-id")


def test_github_list_open_warns_when_no_repo(capsys):
    # No repo scope is a misconfiguration: warn (a signal), not a silent empty
    # list that lets dispatch stall with no clue.
    items = GitHubIssuesTracker(default_repo=None).list_open()
    assert items == []
    err = capsys.readouterr().err
    assert "FNO_TRACKER_GITHUB_REPO" in err


# -- verb refusal on an external backend --


@pytest.mark.parametrize(
    ("verb", "args"),
    [
        ("add", ["add", "t"]),
        ("idea", ["idea", "t"]),
        ("new", ["new", "t"]),
        ("decompose", ["decompose", "ab-deadbeef", "--groups", "x"]),
        ("intake", ["intake", "someplan.md"]),
    ],
)
def test_create_verbs_refuse_on_external_backend(verb, args, monkeypatch):
    # Every creation entry point must refuse on an external backend. The guard
    # lives in _create_node_impl (add/idea) AND at the top of cmd_new,
    # cmd_decompose, cmd_intake, which write through their own mutators. A guard
    # on only some reachable paths is decorative, so this exercises each path:
    # if a future creation verb bypasses the helper, this fails loudly.
    from typer.testing import CliRunner

    from fno.cli import app

    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    result = CliRunner().invoke(app, ["backlog", *args])
    assert result.exit_code == 1, f"{verb} did not refuse: {result.output}"
    assert "github" in result.output
    assert "tracker" in result.output.lower()


def test_active_backend_name_default_and_override(monkeypatch):
    from fno.tracker import active_backend_name

    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    assert active_backend_name() == "graph"
    assert active_backend_name("github") == "github"
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    assert active_backend_name() == "github"


