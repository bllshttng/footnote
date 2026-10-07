"""Integration: the rollup ladder fires on the `idea`/`add` intake path.

Covers AC1 (auto-link + receipt), AC2 (suggest below the bar), the orphan line,
and AC4 (a rollup failure never breaks intake).
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

import fno.graph._constants as gc
import fno.graph.store as gs

runner = CliRunner()



def _route_graph(g: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # Seam readers resolve fno.paths.graph_json at call time; pin the
    # resolver to the same hermetic file (module-attr pins do not reach it).
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)


def _epic(nid: str, title: str) -> dict:
    return {
        "id": nid, "parent": None, "title": title, "type": "epic",
        "project": "fno", "cwd": "/tmp/proj", "priority": "p1", "domain": "code",
        "blocked_by": [], "created_at": "2026-01-01T00:00:00+00:00",
    }


@pytest.fixture
def graph(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    g = tmp_path / "graph.json"
    # The idea door is native: the binary resolves the store through
    # FNO_CONFIG's state_dir, so pin it to this fixture's tmp dir.
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "config.toml"))
    (tmp_path / "config.toml").write_text('state_dir = "%s"\n' % tmp_path)

    def _write(entries: list[dict]) -> Path:
        seed_graph(g, json.dumps({"entries": entries}))
        _route_graph(g, tmp_path, monkeypatch)
        return g

    return _write


class _Result:
    """The CliRunner-shaped face of a native-door idea run."""

    def __init__(self, code: int, out: str, err: str):
        self.exit_code = code
        self.output = out
        self.stdout = out
        self.stderr = err


def _invoke_idea(*args: str) -> _Result:
    """The idea door: the create verb is native; this execs the binary
    against the fixture store the graph fixture wired."""
    from tests._native_door import run_native

    code, out, err = run_native("backlog", "idea", *args)
    return _Result(code, out, err)


def _nodes(g: Path) -> list[dict]:
    # The store owns state; graph.json is a frozen export, so read-backs
    # come from store rows.
    from fno.graph.store import read_graph_strict

    return read_graph_strict(g)


def _created(g: Path, title: str) -> dict:
    return next(e for e in _nodes(g) if e.get("title") == title)


def test_auto_link_sets_parent_and_prints_receipt(graph):
    """AC1: a clear match is linked in the same write, with an undo command."""
    g = graph([_epic("x-mux0001", "mux pane layout polish")])
    title = "mux pane layout polish resize"

    res = _invoke_idea(title, "--cwd", "/tmp/proj", "--difficulty", "low", "--separate")

    assert res.exit_code == 0
    assert _created(g, title)["parent"] == "x-mux0001"
    assert "rollup: auto-linked" in res.stderr
    assert "x-mux0001" in res.stderr
    assert "--parent null" in res.stderr


def test_auto_link_survives_a_related_edge_on_the_same_create(graph):
    """x-129e (second site): set_related() rebinds `entries` under the create
    mutator too, so the rollup block below it must keep writing `parent` onto
    the LIVE node, not a copy orphaned by that rebind.
    """
    g = graph([
        _epic("x-mux0001", "mux pane layout polish"),
        {
            "id": "x-peer0001", "parent": None, "title": "unrelated peer",
            "type": "feature", "project": "fno", "cwd": "/tmp/proj",
            "priority": "p2", "domain": "code", "blocked_by": [],
            "created_at": "2026-01-01T00:00:00+00:00",
        },
    ])
    title = "mux pane layout polish resize"

    res = _invoke_idea( title, "--cwd", "/tmp/proj", "--difficulty", "low",
        "--separate", "--related", "x-peer0001",
    )

    assert res.exit_code == 0, res.stderr
    created = _created(g, title)
    assert created["parent"] == "x-mux0001"
    assert created["related"] == ["x-peer0001"]


def test_suggest_below_the_bar_writes_no_parent(graph):
    """AC2: near-tied epics produce suggestions and no mutation."""
    g = graph([
        _epic("x-aaa00001", "billing invoice export pipeline"),
        _epic("x-bbb00002", "billing invoice export workflow"),
    ])
    title = "billing invoice export"

    res = _invoke_idea(title, "--cwd", "/tmp/proj", "--difficulty", "low", "--separate")

    assert res.exit_code == 0
    assert _created(g, title).get("parent") is None
    assert "--parent x-aaa00001" in res.stderr
    assert "--parent x-bbb00002" in res.stderr
    assert "auto-linked" not in res.stderr


def test_no_candidates_prints_the_orphan_hint(graph):
    """AC3: with a live epic present, an unmatched feature is told it is one."""
    g = graph([_epic("x-mux0001", "mux pane layout polish")])
    title = "quantum teapot calibration"

    res = _invoke_idea(title, "--cwd", "/tmp/proj", "--difficulty", "low", "--separate")

    assert res.exit_code == 0
    assert _created(g, title).get("parent") is None
    assert "--orphan-ok" in res.stderr


def test_greenfield_graph_is_quiet_and_does_not_crash(graph):
    """No epics means no mission to resolve; intake must not narrate that."""
    g = graph([])
    res = _invoke_idea("first ever node", "--cwd", "/tmp/proj", "--difficulty", "low", "--separate")
    assert res.exit_code == 0
    assert len(_nodes(g)) == 1
    assert "rollup" not in res.stderr


def test_explicit_parent_is_never_second_guessed(graph):
    """A hand-set parent already resolves, so the ladder stays silent."""
    g = graph([
        _epic("x-mux0001", "mux pane layout polish"),
        _epic("x-oth00002", "other mission"),
    ])
    title = "mux pane layout polish resize"

    res = _invoke_idea( title, "--cwd", "/tmp/proj", "--parent", "x-oth00002", "--difficulty", "low", "--separate"
    )

    assert _created(g, title)["parent"] == "x-oth00002"
    assert "rollup:" not in res.stderr


def test_filing_under_a_closed_parent_refuses_instead_of_dropping(graph):
    """x-1c7f: a closed parent cannot hold a live child. The strand healers
    (the reconcile re-parent sweep, the close-guard release) clear that edge
    after birth, so exiting 0 with the flag would drop it on the floor. The
    birth path refuses naming why instead."""
    done = _epic("x-done0001", "shipped mux epic")
    done["status"] = "done"
    g = graph([done, _epic("x-mux0001", "mux pane layout polish")])
    title = "mux pane layout polish resize"

    res = _invoke_idea( title, "--cwd", "/tmp/proj", "--parent", "x-done0001", "--difficulty", "low", "--separate"
    )

    assert res.exit_code == 1
    assert "x-done0001" in res.stderr
    assert "done" in res.stderr
    assert "reconcile" in res.stderr
    assert all(e.get("title") != title for e in _nodes(g))


def test_bug_type_is_exempt_from_the_ladder(graph):
    """AC6: a bug never gets a rollup line, however well it scores."""
    graph([_epic("x-mux0001", "mux pane layout polish")])
    res = _invoke_idea( "mux pane layout polish", "--cwd", "/tmp/proj", "--difficulty", "low", "--separate",
        "--type", "bug",
    )
    assert res.exit_code == 0
    assert "rollup:" not in res.stderr











def test_stdout_stays_pure_json_for_machine_callers(graph):
    """Regression: callers do `json.loads(result.output)["id"]`.

    The receipt is advisory human output; putting it on stdout corrupted the
    intake verb's machine-readable payload for every scripted consumer.
    """
    graph([_epic("x-mux0001", "mux pane layout polish")])

    res = _invoke_idea("mux pane layout polish resize", "--cwd", "/tmp/proj", "--difficulty", "low", "--separate")

    payload = json.loads(res.stdout)
    assert payload["title"] == "mux pane layout polish resize"
    assert payload["id"]
    assert "rollup" in res.stderr, "the receipt must still be surfaced, on stderr"


# -- promoted filer: the parent comes from the role scope, not the scorer --


def _role(scope):
    return {"level": 1, "scope": scope, "grantor": "human",
            "label": f"L1 {scope}", "text": f"L1 {scope} (by human)"}
