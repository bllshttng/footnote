"""Integration tests for the --parent epic-scope filter (C2, ab-facfaade).

`fno backlog next --parent <epic>` / `ready --parent <epic>` restrict
candidates to the transitive children of an epic so a walk can drain one
epic's subtree. Mirrors the existing --roadmap-id filter.
"""
from tests.fixtures.graph_seed import seed_graph
from tests.goldens._door import door_graph
import json

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    seed_graph(g, '{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # Seam readers resolve fno.paths.graph_json at call time; pin the
    # resolver to the same hermetic file (module-attr pins do not reach it).
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _add(tmp_graph, title, **opts) -> str:
    # The create verb is native; drive the binary over the same store the
    # fixture seeded, the way _set_parent below does.
    import os as _os
    import subprocess as _sp

    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    args = ["backlog", "add", title, "--difficulty", "medium"]
    for k, v in opts.items():
        args += [f"--{k}", str(v)]
    proc = _sp.run(
        [str(binary), *args],
        capture_output=True,
        text=True,
        timeout=60,
        env={
            "PATH": _os.environ["PATH"],
            "HOME": str(tmp_graph.parent),
            "FNO_STATE_DIR": str(tmp_graph.parent),
            "FNO_TRACKER_BACKEND": "graph",
        },
        cwd=str(tmp_graph.parent),
    )
    assert proc.returncode == 0, proc.stderr
    return json.JSONDecoder().raw_decode(proc.stdout)[0]["id"]


def _set_parent(tmp_graph, child_id, parent_id):
    # The update leaf answers natively; the --parent mutation drives the dev
    # binary over the same store the fixture seeded.
    import os as _os
    import subprocess as _sp

    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    proc = _sp.run(
        [str(binary), "backlog", "update", child_id, "--parent", parent_id],
        capture_output=True,
        text=True,
        timeout=60,
        env={
            "PATH": _os.environ["PATH"],
            "HOME": str(tmp_graph.parent),
            "FNO_STATE_DIR": str(tmp_graph.parent),
            "FNO_TRACKER_BACKEND": "graph",
        },
        cwd=str(tmp_graph.parent),
    )
    assert proc.returncode == 0, proc.stderr


def _epic_with_children(tmp_graph):
    epic = _add(tmp_graph, "Epic")
    c1 = _add(tmp_graph, "Child one")
    c2 = _add(tmp_graph, "Child two")
    loose = _add(tmp_graph, "Loose node")
    _set_parent(tmp_graph, c1, epic)
    _set_parent(tmp_graph, c2, epic)
    return epic, c1, c2, loose


def test_ac2_hp_next_parent_scopes_to_children(tmp_graph):
    """`next --parent <epic>` only ever returns a child of the epic."""
    epic, c1, c2, loose = _epic_with_children(tmp_graph)
    code, out, err = door_graph(
        tmp_graph, "next", "--parent", epic, "--include-ideas", "--all",
    )
    assert code == 0, err
    picked = json.loads(out)
    assert picked is not None
    assert picked["id"] in {c1, c2}
    assert picked["id"] != loose


def test_ac2_hp_ready_parent_scopes_to_children(tmp_graph):
    """`ready --parent <epic>` lists only the epic's children, not loose nodes."""
    epic, c1, c2, loose = _epic_with_children(tmp_graph)
    r = runner.invoke(
        app, ["backlog", "ready", "--parent", epic, "--include-ideas", "--all"],
        catch_exceptions=False,
    )
    assert r.exit_code == 0, r.output
    ids = {e["id"] for e in json.loads(r.output)}
    assert ids == {c1, c2}
    assert loose not in ids
    assert epic not in ids


def test_ac2_err_next_missing_parent_exits_nonzero(tmp_graph):
    """`--parent ab-doesnotexist` is a hard error, not silent nothing."""
    _epic_with_children(tmp_graph)
    code, out, err = door_graph(tmp_graph, "next", "--parent", "ab-doesnotexist", "--all")
    assert code != 0
    assert "no such node" in err.lower() or "not found" in err.lower()


def test_ac2_edge_parent_with_no_children_emits_message(tmp_graph):
    """A valid node with no children returns null + a 'no children' note,
    so the walker can fall back rather than treating it as an error."""
    epic, c1, c2, loose = _epic_with_children(tmp_graph)
    code, out, err = door_graph(
        tmp_graph, "next", "--parent", loose, "--include-ideas", "--all",
    )
    assert code == 0, err
    # null payload on stdout, advisory message on stderr.
    assert "null" in out
    assert "no children under" in err.lower()


def test_parent_combines_with_priority_order(tmp_graph):
    """Within an epic, higher-priority children come first."""
    epic = _add(tmp_graph, "Epic")
    lo = _add(tmp_graph, "low child", priority="p3")
    hi = _add(tmp_graph, "high child", priority="p1")
    _set_parent(tmp_graph, lo, epic)
    _set_parent(tmp_graph, hi, epic)
    code, out, err = door_graph(
        tmp_graph, "next", "--parent", epic, "--include-ideas", "--all",
    )
    assert code == 0, err
    assert json.loads(out)["id"] == hi
