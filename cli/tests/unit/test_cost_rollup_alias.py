"""A ledger row's `sessions[]` is an alias set for ONE run, not a session count.

`cost/_register.py` deliberately records every identifier form it knows for a
run (transcript UUID + the scalar fno/run id) in `sessions[]`. The rollup used
to divide the row's cost by `len(sessions)`, so a run recorded under two names
read as two sessions each costing half as much. Totals stayed right; the
breakdown was fiction.

These tests pin the collapse rule (drop a member equal to the row's own scalar
id, fall back to the scalar when that empties the set) plus the upsert that
keeps a re-reported session cost a level rather than an increment.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest


# -- fixtures --


@pytest.fixture
def ledger(tmp_path, monkeypatch) -> Path:
    """A ledger.json routed to `_rollup_from_ledger`'s lookup path."""
    path = tmp_path / "ledger.json"
    path.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    monkeypatch.setattr(gc, "LEDGER_JSON", path)
    return path


def _seed(ledger_path: Path, rows: list[dict]) -> None:
    ledger_path.write_text(json.dumps({"entries": rows}, indent=2) + "\n")


def _rollup(plan_path: str = "/p") -> dict:
    from fno.done.cli import _rollup_from_ledger
    return _rollup_from_ledger(plan_path)


# -- AC1: a row recorded under two aliases of one run yields one cost row --


def test_alias_of_scalar_id_is_not_a_second_session(ledger):
    _seed(ledger, [{
        "plan_path": "/p",
        "cost_usd": 18.22,
        "fno_id": "R",
        "session_id": "R",
        "sessions": ["U", "R"],
        "completed": "2026-07-27T09:07:20.801626",
    }])
    rollup = _rollup()
    assert len(rollup["cost_sessions"]) == 1
    assert rollup["cost_sessions"][0]["cost_usd"] == 18.22
    assert rollup["cost_sessions"][0]["session_id"] == "U"
    assert rollup["cost_usd"] == 18.22


def test_scalar_falls_back_to_session_id_when_fno_id_absent(ledger):
    """Older rows carry only `session_id`; the alias must still collapse."""
    _seed(ledger, [{
        "plan_path": "/p",
        "cost_usd": 4.0,
        "session_id": "R",
        "sessions": ["U", "R"],
    }])
    rollup = _rollup()
    assert len(rollup["cost_sessions"]) == 1
    assert rollup["cost_sessions"][0]["cost_usd"] == 4.0


# -- AC2: two genuinely distinct sessions still split --


def test_two_real_sessions_still_split(ledger):
    _seed(ledger, [{
        "plan_path": "/p",
        "cost_usd": 10.0,
        "fno_id": "R",
        "session_id": "R",
        "sessions": ["U1", "U2", "R"],
    }])
    rollup = _rollup()
    assert [r["session_id"] for r in rollup["cost_sessions"]] == ["U1", "U2"]
    assert all(r["cost_usd"] == 5.0 for r in rollup["cost_sessions"])
    assert rollup["cost_usd"] == 10.0


def test_sessions_without_the_scalar_are_untouched(ledger):
    """A row whose aliases were never polluted keeps today's behavior."""
    _seed(ledger, [{
        "plan_path": "/p",
        "cost_usd": 9.0,
        "fno_id": "R",
        "sessions": ["U1", "U2", "U3"],
    }])
    rollup = _rollup()
    assert len(rollup["cost_sessions"]) == 3
    assert all(r["cost_usd"] == 3.0 for r in rollup["cost_sessions"])


# -- AC3: totals never drift, whatever the row shape --


def test_totals_unchanged_across_every_row_shape(ledger):
    """The load-bearing invariant: the rollup total equals the ledger sum."""
    rows = [
        # alias pair (the x-24f7 shape)
        {"plan_path": "/p", "cost_usd": 18.22, "fno_id": "R1", "sessions": ["U", "R1"]},
        # two real sessions plus an alias
        {"plan_path": "/p", "cost_usd": 10.0, "fno_id": "R2", "sessions": ["U1", "U2", "R2"]},
        # scalar id alone
        {"plan_path": "/p", "cost_usd": 7.5, "fno_id": "R3", "sessions": ["R3"]},
        # no sessions at all
        {"plan_path": "/p", "cost_usd": 3.0, "sessions": []},
        # malformed sessions
        {"plan_path": "/p", "cost_usd": 1.25, "fno_id": "R4", "sessions": None},
        # non-numeric cost coerces to 0.0
        {"plan_path": "/p", "cost_usd": "nope", "fno_id": "R5", "sessions": ["U5", "R5"]},
    ]
    _seed(ledger, rows)
    expected = 18.22 + 10.0 + 7.5 + 3.0 + 1.25
    assert _rollup()["cost_usd"] == pytest.approx(expected)


def test_scalar_id_alone_collapses_to_one_full_cost_row(ledger):
    """Collapsing must never empty the set and divide by zero."""
    _seed(ledger, [{
        "plan_path": "/p",
        "cost_usd": 7.5,
        "fno_id": "R",
        "sessions": ["R"],
    }])
    rollup = _rollup()
    assert len(rollup["cost_sessions"]) == 1
    assert rollup["cost_sessions"][0]["session_id"] == "R"
    assert rollup["cost_sessions"][0]["cost_usd"] == 7.5


# -- AC4: a row with no sessions still attributes its cost --


def test_empty_sessions_still_emits_one_row(ledger):
    _seed(ledger, [{"plan_path": "/p", "cost_usd": 3.0, "sessions": []}])
    rollup = _rollup()
    assert len(rollup["cost_sessions"]) == 1
    assert rollup["cost_sessions"][0]["session_id"] is None
    assert rollup["cost_sessions"][0]["cost_usd"] == 3.0


def test_malformed_sessions_coerce_rather_than_raise(ledger):
    _seed(ledger, [{"plan_path": "/p", "cost_usd": 2.0, "sessions": "not-a-list"}])
    rollup = _rollup()
    assert len(rollup["cost_sessions"]) == 1
    assert rollup["cost_sessions"][0]["session_id"] is None
    assert rollup["cost_sessions"][0]["cost_usd"] == 2.0


# -- AC5: a re-reported session cost is a level, not an increment --


def _graph(tmp_path: Path, rows: list[dict] | None = None) -> Path:
    path = tmp_path / "graph.json"
    node = {"id": "ab-12345678", "title": "T", "cost_usd": None,
            "cost_sessions": rows if rows is not None else []}
    path.write_text(json.dumps({"entries": [node]}))
    return path


def _node(graph_path: Path) -> dict:
    return json.loads(graph_path.read_text())["entries"][0]


def test_cost_update_replaces_an_existing_session_row(tmp_path):
    from fno.cost import _update_graph_node

    graph_path = _graph(tmp_path)
    _update_graph_node(graph_path, "ab-12345678", "S", 4.0)
    _update_graph_node(graph_path, "ab-12345678", "S", 9.0)

    node = _node(graph_path)
    assert len(node["cost_sessions"]) == 1
    assert node["cost_sessions"][0]["cost_usd"] == 9.0
    assert node["cost_usd"] == 9.0


def test_cost_update_rehashes_the_sidecar_it_invalidates(tmp_path):
    """This writer bypasses locked_mutate_graph, so it owns the sidecar.

    A stale sidecar makes the next load_graph raise GraphCorruptionError, and
    cost attribution is silently skipped from then on.
    """
    import hashlib
    from fno.cost import _update_graph_node
    from fno.graph.load import load_graph

    graph_path = _graph(tmp_path)
    load_graph(graph_path)  # first contact writes the sidecar
    _update_graph_node(graph_path, "ab-12345678", "S1", 4.0)

    sidecar = Path(str(graph_path) + ".sha256")
    assert sidecar.read_text().strip() == hashlib.sha256(graph_path.read_bytes()).hexdigest()
    assert load_graph(graph_path)[0]["cost_usd"] == 4.0


def test_cost_update_still_appends_a_distinct_session(tmp_path):
    from fno.cost import _update_graph_node

    graph_path = _graph(tmp_path)
    _update_graph_node(graph_path, "ab-12345678", "S1", 4.0)
    _update_graph_node(graph_path, "ab-12345678", "S2", 6.0)

    node = _node(graph_path)
    assert [r["session_id"] for r in node["cost_sessions"]] == ["S1", "S2"]
    assert node["cost_usd"] == 10.0


def test_graph_cost_command_replaces_an_existing_session_row(tmp_path, monkeypatch):
    from typer.testing import CliRunner
    import fno.graph._constants as gc
    import fno.graph.store as gs
    from fno.graph.cli import cli

    graph_path = _graph(tmp_path)
    monkeypatch.setattr(gc, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", graph_path)

    runner = CliRunner()
    for amount in ("4.00", "9.00"):
        result = runner.invoke(
            cli, ["cost", "ab-12345678", "--amount", amount, "--session-id", "S"]
        )
        assert result.exit_code == 0, result.stdout

    node = _node(graph_path)
    assert len(node["cost_sessions"]) == 1
    assert node["cost_sessions"][0]["cost_usd"] == 9.0
    assert node["cost_usd"] == 9.0
