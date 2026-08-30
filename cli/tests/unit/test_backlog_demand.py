"""`fno backlog demand` - the divergence read over encounters.

Volume is the weak reading of this signal. A p0 with many encounters tells the
operator nothing, because they already ranked it. A p3 or a never-dispatched
node with many encounters is the entire product: it is the shape of what they
are not looking at. So the table sorts by DIVERGENCE, and these tests are
mostly about that ordering rather than about the count.

Rows are built from fixture entries rather than by driving the write verb. The
ordering must not depend on identity resolution, and a test that spawned a
subprocess per row would be measuring the wrong thing slowly.

Two properties carry the ordering contract. Distinct SESSIONS are the numerator,
so a node that somehow accumulated two rows from one session still counts once.
And the read touches nothing: `demand` never writes rank, never consults a
kanban column, and leaves graph.json byte for byte as it found it.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


def _enc(session_id: str, evidence: str = "cost a cycle.") -> dict:
    return {
        "ts": "2026-08-29T05:00:00+00:00",
        "session_id": session_id,
        "harness": "claude",
        "fno_id": session_id[:8],
        "evidence": evidence,
    }


def _node(node_id: str, priority: str = "p2", **over) -> dict:
    entry = {
        "id": node_id,
        "slug": f"slug-{node_id}",
        "title": f"node {node_id}",
        "status": "ready",
        "priority": priority,
        "_kanban_column": "Next",
    }
    entry.update(over)
    return entry


@pytest.fixture
def tmp_graph(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    graph = tmp_path / "graph.json"
    graph.write_text('{"entries": []}\n', encoding="utf-8")
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", graph)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", graph)
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    return graph


def _write(graph: Path, *entries: dict) -> None:
    graph.write_text(json.dumps({"entries": list(entries)}, indent=2) + "\n", encoding="utf-8")


# --- the score ---------------------------------------------------------------


def test_divergence_outranks_volume(tmp_graph):
    """AC6. Three encounters on a p3 beat five on a p0."""
    from fno.graph.demand import demand_rows

    entries = [
        _node("zz-0001", "p3", encounters=[_enc("s1"), _enc("s2"), _enc("s3")], sessions=[{}]),
        _node("zz-0002", "p0", encounters=[_enc(f"s{i}") for i in range(4, 9)], sessions=[{}]),
    ]
    rows = demand_rows(entries)
    assert [row["node"] for row in rows] == ["zz-0001", "zz-0002"]


def test_a_never_dispatched_node_is_the_loudest_row(tmp_graph):
    """Same priority, same count: the one no session was ever sent to wins."""
    from fno.graph.demand import demand_rows

    entries = [
        _node("zz-0001", "p2", encounters=[_enc("s1"), _enc("s2")], sessions=[{"phase": "do"}]),
        _node("zz-0002", "p2", encounters=[_enc("s3"), _enc("s4")]),
    ]
    rows = demand_rows(entries)
    assert rows[0]["node"] == "zz-0002"
    assert rows[0]["score"] == rows[1]["score"] * 2


def test_a_node_with_a_pr_is_not_treated_as_never_dispatched(tmp_graph):
    """A shipped node was worked on, whatever its sessions list says."""
    from fno.graph.demand import demand_rows

    entries = [
        _node("zz-0001", "p2", encounters=[_enc("s1")], pr_number=42),
        _node("zz-0002", "p2", encounters=[_enc("s2")]),
    ]
    rows = {row["node"]: row["score"] for row in demand_rows(entries)}
    assert rows["zz-0002"] == rows["zz-0001"] * 2


def test_distinct_sessions_are_the_numerator(tmp_graph):
    """One session that somehow got two rows past the write verb counts once."""
    from fno.graph.demand import demand_rows

    entries = [
        _node("zz-0001", "p2", encounters=[_enc("s1"), _enc("s1"), _enc("s1")], sessions=[{}]),
        _node("zz-0002", "p2", encounters=[_enc("s2"), _enc("s3")], sessions=[{}]),
    ]
    rows = {row["node"]: row for row in demand_rows(entries)}
    assert rows["zz-0001"]["enc"] == 1
    assert rows["zz-0002"]["enc"] == 2
    assert [row["node"] for row in demand_rows(entries)] == ["zz-0002", "zz-0001"]


def test_a_node_with_no_encounters_is_not_a_row(tmp_graph):
    from fno.graph.demand import demand_rows

    entries = [_node("zz-0001"), _node("zz-0002", encounters=[_enc("s1")], sessions=[{}])]
    assert [row["node"] for row in demand_rows(entries)] == ["zz-0002"]


def test_an_unknown_priority_weighs_as_p2(tmp_graph):
    from fno.graph.demand import PRIORITY_WEIGHT, divergence_score

    entry = {"encounters": [_enc("s1")], "sessions": [{}]}
    assert divergence_score(entry, "nonsense") == PRIORITY_WEIGHT["p2"]


# --- the dispatch context ----------------------------------------------------


def test_dispatch_context_renders_beside_the_number(tmp_graph):
    """AC9. Sybil-by-dispatch is shown, never corrected out of the count.

    A row reading `enc 2, dispatched 2` is a king that fanned out. A row
    reading `enc 3, dispatched 0` is three sessions that hit the node while
    doing something else. Withholding the number would withhold the signal.
    """
    from fno.graph.demand import demand_rows

    entries = [
        _node(
            "zz-0001",
            "p2",
            encounters=[_enc("s1"), _enc("s2")],
            sessions=[{"session_id": "s1"}, {"session_id": "s2"}],
        )
    ]
    row = demand_rows(entries)[0]
    assert row["enc"] == 2
    assert row["dispatched"] == 2


def test_an_incidental_encounter_reports_zero_dispatched(tmp_graph):
    from fno.graph.demand import demand_rows

    entries = [
        _node(
            "zz-0001",
            "p2",
            encounters=[_enc("s1"), _enc("s2"), _enc("s3")],
            sessions=[{"session_id": "s9"}],
        )
    ]
    row = demand_rows(entries)[0]
    assert row["enc"] == 3
    assert row["dispatched"] == 0


# --- the read writes nothing -------------------------------------------------


def test_the_read_leaves_the_graph_untouched(tmp_graph):
    """LD6. demand is a READ. It never writes rank and never moves a column."""
    _write(
        tmp_graph,
        _node("zz-0001", "p3", encounters=[_enc("s1"), _enc("s2")]),
        _node("zz-0002", "p0", encounters=[_enc("s3")]),
    )
    before = tmp_graph.read_bytes()
    result = runner.invoke(app, ["backlog", "demand"])
    assert result.exit_code == 0, result.output
    assert tmp_graph.read_bytes() == before


def test_the_table_names_the_node_and_its_counts(tmp_graph):
    _write(tmp_graph, _node("zz-0001", "p3", encounters=[_enc("s1"), _enc("s2")]))
    result = runner.invoke(app, ["backlog", "demand"])
    assert result.exit_code == 0, result.output
    assert "zz-0001" in result.output
    assert "p3" in result.output


def test_the_row_reports_status_rather_than_a_derived_column(tmp_graph):
    """The kanban column is derived at render time and is absent from a stored
    entry, so reading it rendered blank on every row of the live graph."""
    from fno.graph.demand import demand_rows

    row = demand_rows([_node("zz-0001", "p2", encounters=[_enc("s1")])])[0]
    assert row["status"] == "ready"
    assert "column" not in row


def test_json_output_carries_the_same_rows(tmp_graph):
    _write(
        tmp_graph,
        _node("zz-0001", "p3", encounters=[_enc("s1"), _enc("s2"), _enc("s3")]),
        _node("zz-0002", "p0", encounters=[_enc("s4")], sessions=[{"session_id": "s4"}]),
    )
    result = runner.invoke(app, ["backlog", "demand", "--json"])
    assert result.exit_code == 0, result.output
    rows = json.loads(result.output)
    assert [row["node"] for row in rows] == ["zz-0001", "zz-0002"]
    assert rows[1]["dispatched"] == 1


def test_operator_voter_counts_and_renders_alongside_agent_voters(tmp_graph):
    from fno.graph.demand import demand_rows, format_rows

    entries = [
        _node(
            "zz-operator",
            encounters=[
                _enc("s1"),
                _enc("s2"),
                {
                    "ts": "2026-08-29T06:00:00+00:00",
                    "voter_key": "operator",
                    "voter_kind": "operator",
                    "evidence": "operator hit the same seam.",
                },
            ],
            sessions=[{}],
        )
    ]

    rows = demand_rows(entries)

    assert rows[0]["enc"] == 3
    assert rows[0]["agent"] == 2
    assert rows[0]["operator"] == 1
    assert "(2a/1o)" in format_rows(rows)


def test_legacy_session_ids_remain_the_voter_key(tmp_graph):
    from fno.graph.demand import demand_rows

    row = demand_rows(
        [_node("zz-legacy", encounters=[_enc("s1"), _enc("s2"), _enc("s1")])]
    )[0]

    assert row["enc"] == 2
    assert row["agent"] == 2
    assert row["operator"] == 0


def test_an_empty_signal_says_so_and_exits_clean(tmp_graph):
    """No encounters is not an error; it is the state of a fresh install."""
    _write(tmp_graph, _node("zz-0001"))
    result = runner.invoke(app, ["backlog", "demand"])
    assert result.exit_code == 0, result.output
    assert json.loads(runner.invoke(app, ["backlog", "demand", "--json"]).output) == []


def test_demand_refuses_under_an_external_tracker_backend(tmp_graph, monkeypatch):
    """Encounters are footnote-minted and live only in the graph.

    Reading the graph under another backend would report an unmeasured zero as
    if it were measured, which is the failure this signal exists to replace.
    """
    _write(tmp_graph, _node("zz-0001", "p3", encounters=[_enc("s1")]))
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    result = runner.invoke(app, ["backlog", "demand"])
    assert result.exit_code == 1, result.output
    assert "github" in result.output


def test_demand_reads_normally_on_the_graph_backend(tmp_graph, monkeypatch):
    """Positive control: the refusal above is the BACKEND, not a broken read."""
    _write(tmp_graph, _node("zz-0001", "p3", encounters=[_enc("s1")]))
    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    result = runner.invoke(app, ["backlog", "demand"])
    assert result.exit_code == 0, result.output
    assert "zz-0001" in result.output
