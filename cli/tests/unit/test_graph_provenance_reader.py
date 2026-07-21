"""The origin/related reader on ``fno backlog provenance`` (x-d157, Part C).

source_node_id shipped a month before anything read it back, which is why the
field looked broken while working correctly: a write-only field is
indistinguishable from one that is not being written. These tests pin the read
side so that cannot recur silently.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


def _node(node_id: str, **over) -> dict:
    base = {
        "id": node_id,
        "title": f"Node {node_id}",
        "_status": "ready",
        "domain": "code",
        "project": "fno",
        "slug": f"node-{node_id}",
    }
    base.update(over)
    return base


@pytest.fixture
def graph(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"

    def seed(entries: list[dict]) -> Path:
        g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")
        return g

    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    for var in ("FNO_NODE", "CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    seed([])
    return seed


# ---------------------------------------------------------------------------
# AC1-UI: the origin renders, including when it is null
# ---------------------------------------------------------------------------


def test_ac1_ui_origin_renders_with_its_title(graph):
    """The line reads on its own; no second lookup to learn what the origin was."""
    graph([
        _node("x-aaaa", title="The origin"),
        _node("x-bbbb", source_node_id="x-aaaa"),
    ])
    result = runner.invoke(app, ["backlog", "provenance", "x-bbbb"])
    assert result.exit_code == 0, result.output
    assert "origin: x-aaaa (The origin)" in result.output


def test_ac1_ui_null_origin_renders_explicitly(graph):
    """A missing origin says so rather than dropping the line.

    An omitted line reads as "this verb does not report origins", which is
    precisely how the field stayed invisible for a month.
    """
    graph([_node("x-aaaa")])
    result = runner.invoke(app, ["backlog", "provenance", "x-aaaa"])
    assert result.exit_code == 0, result.output
    assert "origin: (none)" in result.output


def test_ac1_ui_filing_receipt_names_the_origin_on_stderr(graph, monkeypatch):
    """A filing that resolved an origin says which one; stdout stays JSON."""
    graph([_node("x-aaaa")])
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-1")
    monkeypatch.setenv("FNO_NODE", "x-aaaa")

    result = runner.invoke(app, ["backlog", "idea", "follow-up"])
    assert result.exit_code == 0, result.output
    assert "origin: x-aaaa" in result.output
    # stdout remains a clean JSON payload for `| jq` consumers.
    assert json.loads(result.stdout)["title"] == "follow-up"


def test_related_renders_both_populated_and_empty(graph):
    graph([
        _node("x-aaaa", related=["x-bbbb"]),
        _node("x-bbbb", title="The peer", related=["x-aaaa"]),
        _node("x-cccc"),
    ])
    out = runner.invoke(app, ["backlog", "provenance", "x-aaaa"]).output
    assert "x-bbbb (The peer)" in out
    assert "related: (none)" in runner.invoke(
        app, ["backlog", "provenance", "x-cccc"]
    ).output


def test_json_output_carries_origin_and_related(graph):
    graph([
        _node("x-aaaa", title="The origin"),
        _node("x-bbbb", source_node_id="x-aaaa", related=["x-aaaa"]),
    ])
    result = runner.invoke(app, ["backlog", "provenance", "x-bbbb", "--json"])
    payload = json.loads(result.stdout)
    assert payload["source_node_id"] == "x-aaaa"
    assert payload["source_node_title"] == "The origin"
    assert payload["related"] == ["x-aaaa"]


# ---------------------------------------------------------------------------
# AC5-HP / AC1-EDGE / AC2-EDGE: the reverse walk
# ---------------------------------------------------------------------------


def test_ac5_hp_reverse_walk_depth_is_traversal_derived(graph):
    """AC5-HP: depth comes from the traversal, so a fourth node reports depth 3."""
    graph([
        _node("x-aaaa"),
        _node("x-bbbb", source_node_id="x-aaaa"),
        _node("x-cccc", source_node_id="x-bbbb"),
        _node("x-dddd", source_node_id="x-cccc"),
    ])
    payload = json.loads(
        runner.invoke(
            app, ["backlog", "provenance", "x-aaaa", "--spawned", "--json"]
        ).stdout
    )
    assert {n["id"]: n["depth"] for n in payload["spawned"]["nodes"]} == {
        "x-bbbb": 1,
        "x-cccc": 2,
        "x-dddd": 3,
    }


def test_ac1_edge_cycle_terminates_keeps_results_and_is_flagged(graph):
    """AC1-EDGE: a mutually-attributing pair truncates rather than recursing.

    The walk must still return the descendant it found. Returning an empty set
    with a cycle flag would satisfy "terminates" while discarding the answer -
    the silent no-op this AC exists to catch.
    """
    graph([
        _node("x-aaaa", source_node_id="x-bbbb"),
        _node("x-bbbb", source_node_id="x-aaaa"),
    ])
    result = runner.invoke(
        app, ["backlog", "provenance", "x-aaaa", "--spawned", "--json"]
    )
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    ids = [n["id"] for n in payload["spawned"]["nodes"]]
    assert ids == ["x-bbbb"], "the real descendant must survive the cycle guard"
    assert payload["spawned"]["cycle_detected"] is True


def test_ac2_edge_no_descendants_states_so_and_exits_zero(graph):
    """AC2-EDGE: an empty walk is an empty result, not an error."""
    graph([_node("x-aaaa")])
    result = runner.invoke(app, ["backlog", "provenance", "x-aaaa", "--spawned"])
    assert result.exit_code == 0, result.output
    assert "spawned: (none)" in result.output


def test_walk_is_bounded_by_the_depth_cap(graph):
    """A legitimately deep chain truncates and says so, rather than dumping it all."""
    from fno.graph.cli import _SPAWNED_MAX_DEPTH

    chain = [_node("x-0000")]
    for i in range(1, _SPAWNED_MAX_DEPTH + 3):
        chain.append(_node(f"x-{i:04d}", source_node_id=f"x-{i - 1:04d}"))
    graph(chain)

    payload = json.loads(
        runner.invoke(
            app, ["backlog", "provenance", "x-0000", "--spawned", "--json"]
        ).stdout
    )
    assert len(payload["spawned"]["nodes"]) == _SPAWNED_MAX_DEPTH
    assert payload["spawned"]["truncated_at_depth"] == _SPAWNED_MAX_DEPTH


def test_spawned_is_opt_in(graph):
    """Without --spawned the verb output is unchanged for existing consumers."""
    graph([_node("x-aaaa"), _node("x-bbbb", source_node_id="x-aaaa")])
    payload = json.loads(
        runner.invoke(app, ["backlog", "provenance", "x-aaaa", "--json"]).stdout
    )
    assert "spawned" not in payload


def test_a_chain_ending_exactly_at_the_cap_is_not_reported_truncated(graph):
    """Truncation means something was cut, not that the walk used its whole budget."""
    from fno.graph.cli import _SPAWNED_MAX_DEPTH

    chain = [_node("x-0000")]
    for i in range(1, _SPAWNED_MAX_DEPTH + 1):
        chain.append(_node(f"x-{i:04d}", source_node_id=f"x-{i - 1:04d}"))
    graph(chain)

    payload = json.loads(
        runner.invoke(
            app, ["backlog", "provenance", "x-0000", "--spawned", "--json"]
        ).stdout
    )
    assert len(payload["spawned"]["nodes"]) == _SPAWNED_MAX_DEPTH
    assert payload["spawned"]["truncated_at_depth"] is None
