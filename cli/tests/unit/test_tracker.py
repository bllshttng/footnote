"""Tests for the work-item tracker seam (bring-your-your-own-id foundation).

Every backend answers in Rust (``crates/fno-agents/src/tracker/``); the
backends' behavior is tested there (AC1-AC5 of the seam plan). This module
covers the exec client, the footnote-owned sidecar store, and the verb
refusals. The partition invariant itself (zero overlap between sidecar and
read interface) has its own CI gate in scripts/ci/check-tracker-partition.sh,
exercised in test_partition_gate.py.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.paths import sidecar_path
from fno.tracker import (
    NodeNotFound,
    TrackerError,
    TrackerNode,
    TrackerState,
    get_tracker,
)
from fno.tracker import sidecar as sidecar_mod
from fno.tracker.sidecar import Sidecar, load, save
from fno.graph.store import read_graph_strict


def _write_graph(path: Path, entries: list[dict]) -> Path:
    """Rows the way the store writes them: the typed api drops a row the
    model cannot parse, so seeds carry the stamped fields."""
    complete = []
    for e in entries:
        row = {"type": "feature", "priority": "p2", "status": "idea", **e}
        row.setdefault("title", e.get("id", "node"))
        row.setdefault("slug", e.get("id", "node"))
        if row["status"] == "done" and not row.get("completed_at"):
            row["completed_at"] = "2026-09-01T00:00:00Z"
        complete.append(row)
    path.write_text(json.dumps({"entries": complete}), encoding="utf-8")
    return path


def _stub_verb_call(answer_by_op: dict):
    """A verb_call stand-in keyed by the door's ``tracker`` op."""

    def _call(verb, payload, unavailable, *, timeout=30, passthrough_stderr=False):
        assert verb == "graph-get"
        op = payload["tracker"]
        answer = answer_by_op[op]
        if isinstance(answer, Exception):
            raise answer
        return answer

    return _call


# -- the Rust exec client --


def test_client_read_maps_node_payload(monkeypatch):
    monkeypatch.setattr(
        "fno.rust_binary.verb_call",
        _stub_verb_call({"read": {"node": {
            "id": "E-1", "title": "T", "state": "open", "parent": None,
            "blocked_by": [], "details": "d", "url": "u", "size": None,
        }}}),
    )
    node = get_tracker("graph").read("E-1")
    assert node.id == "E-1"
    assert node.title == "T"
    assert node.state is TrackerState.open
    # The extra Rust fields (details, url, size) are ignored by the parse
    # target: only Sidecar sets extra="forbid".
    assert set(TrackerNode.model_fields) == {"id", "title", "state", "parent", "blocked_by"}


def test_client_read_maps_not_found(monkeypatch):
    monkeypatch.setattr(
        "fno.rust_binary.verb_call",
        _stub_verb_call({"read": {"not_found": True}}),
    )
    with pytest.raises(NodeNotFound):
        get_tracker("graph").read("E-gone")


def test_client_read_maps_error(monkeypatch):
    monkeypatch.setattr(
        "fno.rust_binary.verb_call",
        _stub_verb_call({"read": {"error": "gh issue view failed for E-9: x"}}),
    )
    with pytest.raises(TrackerError):
        get_tracker("graph").read("E-9")


def test_client_list_open_and_snapshot_and_close(monkeypatch):
    monkeypatch.setattr(
        "fno.rust_binary.verb_call",
        _stub_verb_call({
            "list-open": {"candidates": [{
                "id": "E-1", "title": "T", "state": "open", "parent": None,
                "blocked_by": [], "priority": "p1", "rank": None,
                "created_at": "2026-01-01T00:00:00Z", "closed_at": None,
            }]},
            "snapshot": {"backend": "graph", "entries": [], "errors": []},
            "close": {"closed": "E-1"},
        }),
    )
    t = get_tracker("graph")
    cands = t.list_open()
    assert cands[0].id == "E-1"
    assert cands[0].priority == "p1"
    assert t._call("snapshot")["backend"] == "graph"
    t.close("E-1")


def test_get_tracker_unknown_backend_fails_at_first_call(monkeypatch):
    # No ValueError at construction: the backend is resolved in Rust at the
    # first call, with the Rust refusal text.
    monkeypatch.setattr(
        "fno.rust_binary.verb_call",
        _stub_verb_call({"read": {"error": "unknown tracker backend: linear. Available: graph, github"}}),
    )
    with pytest.raises(TrackerError, match="linear"):
        get_tracker("linear").read("X-1")


def test_get_tracker_name_is_the_selected_backend(monkeypatch):
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    assert get_tracker().name == "github"
    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    assert get_tracker().name == "graph"


# -- sidecar roundtrip --


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
    entries = read_graph_strict(g)
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


def test_sidecar_null_list_fields_degrade_to_empty(tmp_path, monkeypatch, graph_mode):
    """A graph entry with an explicit null (not absent) list field must not
    drop the whole node - every other reader already tolerates this shape
    via `node.get(...) or []`."""
    g = _write_graph(
        tmp_path / "graph.json",
        [{"id": "ab-1", "additional_prs": None, "cost_sessions": None, "sessions": None}],
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    sc = load("ab-1")
    assert sc.additional_prs == []
    assert sc.cost_sessions == []
    assert sc.sessions == []


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
    assert read_graph_strict(g)[0]["cwd"] == "/graph-sentinel"


def test_sidecar_external_mode_missing_file_is_empty(tmp_path, monkeypatch, external_mode):
    monkeypatch.setattr(
        sidecar_mod, "sidecar_path", lambda i: tmp_path / "sidecars" / f"{i}.json"
    )
    assert load("EXT-new") == Sidecar(id="EXT-new")


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
