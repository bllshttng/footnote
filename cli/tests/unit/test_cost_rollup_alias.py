"""A ledger row's `sessions[]` is an alias set for ONE run, not a session count.

`cost/_register.py` deliberately records every identifier form it knows for a
run (transcript UUID + the scalar fno/run id) in `sessions[]`. The rollup used
to divide the row's cost by `len(sessions)`, so a run recorded under two names
read as two sessions each costing half as much. Totals stayed right; the
breakdown was fiction.

These tests pin the rule that replaced the division - one ledger row yields one
cost row carrying its whole cost, keyed by the scalar run id - plus the upsert
that keeps a re-reported session cost a level rather than an increment.

The totals-unchanged test is the reason this ships without a backfill, but note
that it alone would not catch a reintroduced divisor: N rows of cost/N sum back
to cost either way. The row-count assertions are what actually pin the model.
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


def _rollup(plan_path: str = "/p", **node_fields) -> dict:
    from fno.done.cli import _rollup_from_ledger
    return _rollup_from_ledger({"id": "x-unit", "plan_path": plan_path, **node_fields})


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
    assert rollup["cost_sessions"][0]["session_id"] == "R"
    assert rollup["cost_usd"] == 18.22


def test_row_is_keyed_by_session_id_when_fno_id_absent(ledger):
    """Older rows carry only `session_id`."""
    _seed(ledger, [{
        "plan_path": "/p",
        "cost_usd": 4.0,
        "session_id": "R",
        "sessions": ["U", "R"],
    }])
    rollup = _rollup()
    assert len(rollup["cost_sessions"]) == 1
    assert rollup["cost_sessions"][0]["session_id"] == "R"
    assert rollup["cost_sessions"][0]["cost_usd"] == 4.0


# -- aliases that do not match the scalar are still aliases --


def test_aliases_that_do_not_equal_the_scalar_still_do_not_divide(ledger):
    """The codex shape: a rollout-log basename and the bare thread uuid.

    Neither equals the scalar run id, so a rule that only dropped
    scalar-equal members would still have halved this run's cost.
    """
    _seed(ledger, [{
        "plan_path": "/p",
        "cost_usd": 10.0,
        "fno_id": "R",
        "session_id": "R",
        "sessions": ["rollout-2026-07-25T18-45-56-019f9c19", "019f9c19", "R"],
    }])
    rollup = _rollup()
    assert len(rollup["cost_sessions"]) == 1
    assert rollup["cost_sessions"][0]["session_id"] == "R"
    assert rollup["cost_sessions"][0]["cost_usd"] == 10.0


def test_two_sessions_means_two_ledger_rows(ledger):
    """Genuine multi-session work is multiple rows, never one row's aliases."""
    _seed(ledger, [
        {"plan_path": "/p", "cost_usd": 6.0, "fno_id": "R1", "sessions": ["U1", "R1"]},
        {"plan_path": "/p", "cost_usd": 4.0, "fno_id": "R2", "sessions": ["U2", "R2"]},
    ])
    rollup = _rollup()
    assert [r["session_id"] for r in rollup["cost_sessions"]] == ["R1", "R2"]
    assert [r["cost_usd"] for r in rollup["cost_sessions"]] == [6.0, 4.0]
    assert rollup["cost_usd"] == 10.0


def test_row_with_aliases_but_no_scalar_keys_on_the_first_alias(ledger):
    _seed(ledger, [{"plan_path": "/p", "cost_usd": 9.0, "sessions": ["U1", "U2", "U3"]}])
    rollup = _rollup()
    assert len(rollup["cost_sessions"]) == 1
    assert rollup["cost_sessions"][0]["session_id"] == "U1"
    assert rollup["cost_sessions"][0]["cost_usd"] == 9.0


# -- AC3: totals never drift, whatever the row shape --


def test_totals_unchanged_across_every_row_shape(ledger):
    """The load-bearing invariant: the rollup total equals the ledger sum."""
    rows = [
        # alias pair: transcript uuid + scalar run id
        {"plan_path": "/p", "cost_usd": 18.22, "fno_id": "R1", "sessions": ["U", "R1"]},
        # three aliases of one run
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


def test_scalar_id_as_the_only_alias_keeps_its_full_cost(ledger):
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


def test_cost_update_leaves_the_sidecar_valid(tmp_path):
    """A stale sidecar makes the next load_graph raise GraphCorruptionError,
    after which cost attribution is silently skipped from then on."""
    import hashlib
    from fno.cost import _update_graph_node
    from fno.graph.load import load_graph

    graph_path = _graph(tmp_path)
    load_graph(graph_path)  # first contact writes the sidecar
    _update_graph_node(graph_path, "ab-12345678", "S1", 4.0)

    sidecar = Path(str(graph_path) + ".sha256")
    assert sidecar.read_text().strip() == hashlib.sha256(graph_path.read_bytes()).hexdigest()
    assert load_graph(graph_path)[0]["cost_usd"] == 4.0


def test_cost_update_never_empties_a_legacy_root_list_graph(tmp_path):
    """The whole backlog, not just this node's cost, rides on this.

    A root-list graph.json read as `{"entries": []}` and written back is a
    total loss of the graph. Refuse the write instead.
    """
    from fno.cost import _update_graph_node

    graph_path = tmp_path / "graph.json"
    original = json.dumps([{"id": "ab-12345678", "title": "T", "cost_sessions": []}])
    graph_path.write_text(original)

    assert _update_graph_node(graph_path, "ab-12345678", "S1", 4.0) is False
    assert graph_path.read_text() == original


def test_cost_update_reports_a_node_it_could_not_find(tmp_path):
    from fno.cost import _update_graph_node

    graph_path = _graph(tmp_path)
    assert _update_graph_node(graph_path, "ab-99999999", "S1", 4.0) is False
    assert _update_graph_node(graph_path, "ab-12345678", "S1", 4.0) is True


def test_update_surfaces_whether_the_node_got_the_cost(tmp_path, monkeypatch):
    """`ok: True` used to mean "the ledger row landed", never the attribution."""
    from fno import cost as cost_mod

    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}')
    graph_path = _graph(tmp_path)
    monkeypatch.setattr(cost_mod, "_run_session_cost", lambda *a, **k: None, raising=False)

    result = cost_mod.update(
        "S1", 100, 4.0, ledger_path=ledger, graph_path=graph_path, node_id="ab-99999999"
    )
    assert result["ok"] is True
    assert result["graph_updated"] is False


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


# -- the node's session_id must name a row that exists --


def test_node_session_id_names_a_cost_row_it_actually_owns(ledger):
    """`sessions[]` order need not end on the scalar.

    `_register.py` appends (caller id, run id, harness id, ...), so the scalar
    sits mid-list whenever the harness reports an id of its own. Resolving the
    node's session_id as "the last alias" then named a session owning no cost
    row, which is exactly what the metrics backfills cross-reference on.
    """
    _seed(ledger, [{
        "plan_path": "/p",
        "cost_usd": 12.00,
        "fno_id": "R",
        "sessions": ["U", "R", "HARNESS"],
        "completed": "2026-07-27T09:07:20.801626",
    }])
    rollup = _rollup()
    keys = [r["session_id"] for r in rollup["cost_sessions"]]
    assert keys == ["R"]
    assert rollup["session_id"] in keys


# -- upsert survives the graph shapes the read side already defends against --


def test_upsert_drops_rows_that_cannot_be_cost_rows():
    from fno.cost import upsert_cost_session

    node = {"id": "ab-1", "cost_sessions": [{"session_id": "S1", "cost_usd": 2.0}, "junk", None]}
    upsert_cost_session(node, "S2", 3.0)
    assert [r["session_id"] for r in node["cost_sessions"]] == ["S1", "S2"]
    assert node["cost_usd"] == 5.0


def test_upsert_treats_a_non_list_cost_sessions_as_empty():
    from fno.cost import upsert_cost_session

    node = {"id": "ab-1", "cost_sessions": {"S1": 2.0}}
    upsert_cost_session(node, "S2", 3.0)
    assert node["cost_sessions"] == [
        {"session_id": "S2", "cost_usd": 3.0, "timestamp": node["cost_sessions"][0]["timestamp"]}
    ]
    assert node["cost_usd"] == 3.0


def test_upsert_scores_a_junk_cost_as_zero_rather_than_raising():
    from fno.cost import upsert_cost_session

    node = {"id": "ab-1", "cost_sessions": [
        {"session_id": "S1", "cost_usd": "not-a-number"},
        {"session_id": "S2", "cost_usd": True},
    ]}
    upsert_cost_session(node, "S3", 4.0)
    assert node["cost_usd"] == 4.0


def test_both_production_writers_agree_on_one_nodes_precision(tmp_path, monkeypatch):
    """`fno graph cost` and the internal cost writer share one node.

    They used to round it to different places (2 and 4), so the stored total's
    last digits depended on which wrote last. Drives both real call sites
    rather than the helper, because the helper alone cannot disagree with
    itself - the defect lived in what the callers asked for.
    """
    from typer.testing import CliRunner
    import fno.graph._constants as gc
    import fno.graph.store as gs
    from fno.graph.cli import cli
    from fno.cost import _update_graph_node

    graph_path = _graph(tmp_path)
    monkeypatch.setattr(gc, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", graph_path)

    _update_graph_node(graph_path, "ab-12345678", "S1", 1.005)
    result = CliRunner().invoke(
        cli, ["cost", "ab-12345678", "--amount", "2.004", "--session-id", "S2"]
    )
    assert result.exit_code == 0, result.stdout

    node = _node(graph_path)
    assert node["cost_usd"] == pytest.approx(1.005 + 2.004)


def test_cost_update_degrades_when_the_graph_write_raises(tmp_path, monkeypatch, capsys):
    """"Best-effort" has to cover every failure, not just the SystemExit one."""
    import fno.graph.store as gs
    from fno.cost import _update_graph_node

    graph_path = _graph(tmp_path)

    def boom(*a, **k):
        raise OSError("read-only .fno")

    monkeypatch.setattr(gs, "locked_mutate_graph", boom)
    assert _update_graph_node(graph_path, "ab-12345678", "S1", 4.0) is False
    assert "read-only .fno" in capsys.readouterr().err


def test_cost_update_says_so_when_there_is_no_graph(tmp_path, capsys):
    from fno.cost import _update_graph_node

    assert _update_graph_node(tmp_path / "absent.json", "ab-12345678", "S1", 4.0) is False
    assert "no graph at" in capsys.readouterr().err


# -- AC9: a contained node projects no rollup at all (x-e957 task 1.4) --


def test_contained_node_takes_no_cost_from_the_shared_plan(ledger):
    """N nodes on one plan each matched the same rows and each claimed the lot.

    The plan is the join key, so containment is invisible from plan_path alone -
    which is why the rollup reads the node. An unlinked node already returned
    empty, but by accident of the ledger's grain: it stops being safe the moment
    a child is linked again, which is exactly what happened on 2026-07-28.
    """
    _seed(ledger, [{
        "plan_path": "/p",
        "cost_usd": 18.22,
        "points": 5,
        "fno_id": "R",
        "sessions": ["R"],
        "completed": "2026-07-27T09:07:20.801626",
    }])
    # The delivery unit takes the whole figure.
    assert _rollup()["cost_usd"] == 18.22

    # A node contained in it, on the SAME plan, takes none of it.
    contained = _rollup(contained_in="x-unit0001")
    assert contained == {
        "session_id": None, "cost_usd": None, "cost_sessions": [], "points": None,
    }


def test_contained_suppression_covers_points_and_session_not_just_cost(ledger):
    """They ride one return, so a partial suppression claims a run never made.

    session_id is what the metrics backfills cross-reference; a contained node
    carrying it attributes a real session to a node that opened no PR.
    """
    _seed(ledger, [{
        "plan_path": "/p", "cost_usd": 3.0, "points": 8,
        "fno_id": "R", "sessions": ["R"], "completed": "2026-07-27T09:07:20",
    }])
    assert _rollup()["points"] == 8 and _rollup()["session_id"] == "R"
    contained = _rollup(contained_in="x-unit0001")
    assert contained["points"] is None
    assert contained["session_id"] is None


def test_empty_contained_in_is_not_containment(ledger):
    """Only a non-empty owner id suppresses; "" and None are ordinary nodes.

    A falsy value here would silently zero a delivery unit's cost, and the
    symptom (money quietly missing from the project total) is one nobody reads
    as a bug in a containment guard.
    """
    _seed(ledger, [{
        "plan_path": "/p", "cost_usd": 7.5,
        "fno_id": "R", "sessions": ["R"], "completed": "2026-07-27T09:07:20",
    }])
    assert _rollup(contained_in=None)["cost_usd"] == 7.5
    assert _rollup(contained_in="")["cost_usd"] == 7.5


def test_ac9_flat_project_total_sums_delivery_units_only(ledger):
    """The project sum stays a flat sum with no dedup logic of its own.

    Teaching it to dedup was the alternative and is explicitly not the design:
    contained nodes contributing zero is what keeps the naive
    `sum(cost_usd)` correct. This pins the invariant at the summation itself,
    not just at the rollup that feeds it.
    """
    unit = {"id": "x-unit0001", "type": "feature", "plan_path": "/p", "cost_usd": 18.22}
    kids = [
        {"id": f"x-kid0000{i}", "type": "feature", "plan_path": "/p",
         "contained_in": "x-unit0001",
         "cost_usd": _rollup(contained_in="x-unit0001")["cost_usd"]}
        for i in (1, 2)
    ]
    features = [unit, *kids]
    assert sum(e.get("cost_usd", 0) or 0 for e in features) == pytest.approx(18.22)


def test_rollup_tolerates_a_non_dict_node(ledger):
    """Every caller resolves the node from the graph and can hand back None."""
    from fno.done.cli import _rollup_from_ledger

    assert _rollup_from_ledger(None)["cost_usd"] is None
