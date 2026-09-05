"""Tests for `fno backlog contain` and the containment contract it stamps.

Covers:
- the contain verb: happy path, owner refusals (done / deferred / missing),
  target refusals (children / live claim / open PR), batch atomicity
- ``plan_base``'s refusal teaching the verb for a plan-less epic
- ``_container_ids`` treating an all-contained parent as a delivery unit
- the board line: a contained row ships inside its owner, a deferred row hides
- the named-dispatch redirect (``_redirect_if_contained``)
- the merge cascade (``_cascade_close_contained``) and ``_strandable_contained_ids``
- undefer after contain keeps containment
"""
from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path

import pytest
import typer
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """Fresh empty graph.json routed to tmp_path (the test_defer.py pattern)."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _invoke(*args):
    return runner.invoke(app, list(args), catch_exceptions=False)


def _read_entries(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def _by_id(g: Path) -> dict:
    return {e["id"]: e for e in _read_entries(g)}


def _seed_idea(g: Path, title: str, *extra: str) -> str:
    """Mint a node and return its id from the GRAPH (stdout wrapping varies).

    --separate skips the non-interactive fold offer: without it a title
    resembling an existing node prints "fold offered" and mints nothing.
    """
    before = {e.get("id") for e in _read_entries(g)}
    r = _invoke(
        "backlog", "idea", title, "--difficulty", "medium", "--separate", *extra
    )
    assert r.exit_code == 0, r.output
    new = [e["id"] for e in _read_entries(g) if e.get("id") not in before]
    assert len(new) == 1, f"expected one new node, saw {new}"
    return new[0]


def _seed_owner_with_children(g: Path, n: int = 3) -> tuple[str, list[str]]:
    owner = _seed_idea(g, "owner epic")
    kids = [_seed_idea(g, f"child {i}", "--parent", owner) for i in range(n)]
    return owner, kids


# ---------------------------------------------------------------------------
# The verb: happy path
# ---------------------------------------------------------------------------


def test_contain_stamps_containment_and_parent_on_every_child(tmp_graph):
    owner, kids = _seed_owner_with_children(tmp_graph, 3)
    r = _invoke("backlog", "contain", owner, *kids)
    assert r.exit_code == 0, r.output
    rows = _by_id(tmp_graph)
    for kid in kids:
        assert rows[kid]["contained_in"] == owner
        assert rows[kid]["parent"] == owner
        assert f"contained {kid} into {owner}" in r.output
        assert "it ships inside" in r.output


def test_contain_json_receipt(tmp_graph):
    owner, kids = _seed_owner_with_children(tmp_graph, 2)
    r = _invoke("--json", "backlog", "contain", owner, *kids)
    assert r.exit_code == 0, r.output
    payload = json.loads(r.output.strip().splitlines()[-1])
    assert payload["owner"] == owner
    assert sorted(payload["contained"]) == sorted(kids)
    assert payload["warnings"] == []


def test_contain_is_idempotent_on_rerun(tmp_graph):
    owner, kids = _seed_owner_with_children(tmp_graph, 1)
    assert _invoke("backlog", "contain", owner, *kids).exit_code == 0
    r = _invoke("backlog", "contain", owner, *kids)
    assert r.exit_code == 0, r.output
    row = _by_id(tmp_graph)[kids[0]]
    assert row["contained_in"] == owner


# ---------------------------------------------------------------------------
# The verb: owner refusals
# ---------------------------------------------------------------------------


def test_contain_refuses_a_done_owner_and_stamps_nothing(tmp_graph):
    owner, kids = _seed_owner_with_children(tmp_graph, 2)
    _invoke("backlog", "done", owner)
    r = _invoke("backlog", "contain", owner, *kids)
    assert r.exit_code == 2, r.output
    assert "is done" in r.output
    rows = _by_id(tmp_graph)
    assert all("contained_in" not in rows[k] for k in kids)


def test_contain_refuses_a_deferred_owner_and_stamps_nothing(tmp_graph):
    owner, kids = _seed_owner_with_children(tmp_graph, 1)
    _invoke("backlog", "defer", owner, "--reason", "parked")
    r = _invoke("backlog", "contain", owner, *kids)
    assert r.exit_code == 2, r.output
    assert "is deferred" in r.output
    assert "contained_in" not in _by_id(tmp_graph)[kids[0]]


def test_contain_refuses_a_missing_owner(tmp_graph):
    kid = _seed_idea(tmp_graph, "lone child")
    r = _invoke("backlog", "contain", "x-dead0001", kid)
    assert r.exit_code == 3, r.output
    assert "owner not found" in r.output
    assert "contained_in" not in _by_id(tmp_graph)[kid]


def test_contain_refuses_the_owner_naming_itself(tmp_graph):
    owner, _kids = _seed_owner_with_children(tmp_graph, 1)
    r = _invoke("backlog", "contain", owner, owner)
    assert r.exit_code == 1, r.output


def test_contain_refuses_one_id_spelled_two_ways(tmp_graph):
    owner, kids = _seed_owner_with_children(tmp_graph, 1)
    prefix, hex_part = kids[0].split("-")
    short = f"{prefix}-{hex_part[:4]}"  # fuzzy prefix of the same node
    r = _invoke("backlog", "contain", owner, kids[0], short)
    assert r.exit_code == 1, r.output
    assert "resolve to the same node" in r.output


# ---------------------------------------------------------------------------
# The verb: target refusals, batch atomicity
# ---------------------------------------------------------------------------


def test_contain_refuses_a_target_with_children(tmp_graph):
    owner, kids = _seed_owner_with_children(tmp_graph, 2)
    grandchild = _seed_idea(tmp_graph, "grandchild", "--parent", kids[0])
    r = _invoke("backlog", "contain", owner, kids[0])
    assert r.exit_code == 2, r.output
    assert "containment is one level" in r.output
    assert grandchild


def test_contain_refuses_a_target_with_an_open_pr_and_stamps_nothing_in_the_batch(
    tmp_graph,
):
    owner, kids = _seed_owner_with_children(tmp_graph, 2)
    rows = _by_id(tmp_graph)
    rows[kids[0]]["pr_number"] = 4242
    tmp_graph.write_text(json.dumps({"entries": list(rows.values())}))
    r = _invoke("backlog", "contain", owner, *kids)
    assert r.exit_code == 2, r.output
    assert "own delivery unit mid-flight" in r.output
    fresh = _by_id(tmp_graph)
    assert all("contained_in" not in fresh[k] for k in kids)


def test_contain_refuses_a_target_with_a_live_claim(tmp_graph, monkeypatch):
    owner, kids = _seed_owner_with_children(tmp_graph, 1)
    import fno.graph.cli as graph_cli

    monkeypatch.setattr(graph_cli, "_live_worker", lambda node_id: "worker-7")
    r = _invoke("backlog", "contain", owner, *kids)
    assert r.exit_code == 2, r.output
    assert "being built right now by worker-7" in r.output
    assert "contained_in" not in _by_id(tmp_graph)[kids[0]]


def test_contain_live_worker_positive_control_on_an_unclaimed_node(tmp_graph):
    from fno.graph.cli import _live_worker

    owner, kids = _seed_owner_with_children(tmp_graph, 1)
    assert _live_worker(kids[0]) is None
    assert _invoke("backlog", "contain", owner, *kids).exit_code == 0


def test_contain_withholds_containment_for_a_done_target_with_a_pr(tmp_graph):
    # Two kids so `backlog done` on one cannot cascade the owner closed via
    # _cascade_close_parents (an all-children-done epic closes automatically).
    owner, kids = _seed_owner_with_children(tmp_graph, 2)
    kid = kids[0]
    _invoke("backlog", "done", kid)
    rows = _by_id(tmp_graph)
    assert not rows[owner].get("completed_at"), "owner must stay open"
    rows[kid]["pr_number"] = 4243
    tmp_graph.write_text(json.dumps({"entries": list(rows.values())}))
    r = _invoke("backlog", "contain", owner, kid)
    assert r.exit_code == 0, r.output
    row = _by_id(tmp_graph)[kid]
    assert row["parent"] == owner
    assert "contained_in" not in row
    assert "did NOT mark it contained" in r.output


# ---------------------------------------------------------------------------
# plan_base teaches the verb
# ---------------------------------------------------------------------------


def test_plan_base_refusal_names_the_contain_verb():
    from fno.graph._decompose import DecomposeError, plan_base

    with pytest.raises(DecomposeError) as exc:
        plan_base(None)
    assert "fno backlog contain <epic> <id>..." in str(exc.value)


# ---------------------------------------------------------------------------
# A box is a node with a child that is not contained in it
# ---------------------------------------------------------------------------


def test_container_ids_ignores_an_all_contained_brood():
    from fno.graph.cli import _container_ids

    entries = [
        {"id": "o"},
        {"id": "c1", "parent": "o", "contained_in": "o"},
        {"id": "c2", "parent": "o", "contained_in": "o"},
    ]
    assert _container_ids(entries) == set()


def test_container_ids_counts_a_free_child():
    from fno.graph.cli import _container_ids

    entries = [
        {"id": "o"},
        {"id": "c1", "parent": "o", "contained_in": "o"},
        {"id": "c2", "parent": "o"},
    ]
    assert _container_ids(entries) == {"o"}


def test_next_returns_the_owner_when_its_only_children_are_contained(tmp_graph):
    """The delivery-unit rule: an all-contained brood makes the owner drain.

    A top-level scan selects the owner itself (before the container-rule fix
    it was an undispatchable box forever), and selection_guards holds each
    contained child out of autonomous dispatch.
    """
    from fno.backlog.advance import selection_guards

    owner, kids = _seed_owner_with_children(tmp_graph, 2)
    assert _invoke("backlog", "contain", owner, *kids).exit_code == 0
    r = _invoke("backlog", "next", "--ideas")
    assert r.exit_code == 0, r.output
    assert owner in r.output
    for kid in kids:
        assert kid not in r.output
        now = datetime.now(timezone.utc)
        assert selection_guards(
            _by_id(tmp_graph)[kid], _by_id(tmp_graph), now
        ) == f"contained:{owner}"


# ---------------------------------------------------------------------------
# The board says where a contained row ships
# ---------------------------------------------------------------------------


def test_kanban_column_and_card_for_a_contained_row(tmp_graph):
    from fno.graph.render import _kanban_column, render_graph_md

    owner, kids = _seed_owner_with_children(tmp_graph, 1)
    assert _invoke("backlog", "contain", owner, *kids).exit_code == 0
    kid_row = _by_id(tmp_graph)[kids[0]]
    assert _kanban_column(kid_row) is not None
    card = tmp_graph.with_name("card.md")
    render_graph_md(_read_entries(tmp_graph), card, obsidian=False)
    assert f"ships inside: {owner}" in card.read_text()


def test_kanban_column_hides_a_deferred_row(tmp_graph):
    from fno.graph.render import _kanban_column

    owner, kids = _seed_owner_with_children(tmp_graph, 1)
    assert _invoke("backlog", "contain", owner, *kids).exit_code == 0
    _invoke("backlog", "defer", kids[0], "--reason", "parked")
    assert _kanban_column(_by_id(tmp_graph)[kids[0]]) is None


# ---------------------------------------------------------------------------
# The named-dispatch redirect
# ---------------------------------------------------------------------------


def test_redirect_if_contained_exits_2_naming_the_owner(tmp_graph, capsys):
    from fno.target_cli import _redirect_if_contained

    owner, kids = _seed_owner_with_children(tmp_graph, 1)
    assert _invoke("backlog", "contain", owner, *kids).exit_code == 0
    with pytest.raises(typer.Exit) as exc:
        _redirect_if_contained(_by_id(tmp_graph)[kids[0]])
    assert exc.value.exit_code == 2
    assert f"ships inside {owner}'s PR" in capsys.readouterr().err


# ---------------------------------------------------------------------------
# The merge cascade
# ---------------------------------------------------------------------------


def test_cascade_close_contained_sets_completion_with_the_shipped_inside_note():
    from fno.graph.cli import _cascade_close_contained

    entries = [
        {"id": "o", "pr_number": 77},
        {"id": "c1", "parent": "o", "contained_in": "o"},
        {"id": "c2", "parent": "o", "contained_in": "o", "completed_at": "2026-01-01"},
    ]
    closed = _cascade_close_contained(entries, "o")
    assert closed == ["c1"]
    c1 = next(e for e in entries if e["id"] == "c1")
    assert c1["completed_at"]
    assert "shipped inside o (PR #77)" in c1["completion_note"]


def test_strandable_contained_ids_names_open_nodes_of_a_done_owner():
    from fno.graph.cli import _strandable_contained_ids

    entries = [
        {"id": "o", "completed_at": "2026-01-01"},
        {"id": "c1", "contained_in": "o"},
        {"id": "c2", "contained_in": "o", "completed_at": "2026-01-02"},
    ]
    assert _strandable_contained_ids(entries) == {"c1"}


# ---------------------------------------------------------------------------
# Undefer after contain keeps containment
# ---------------------------------------------------------------------------


def test_undefer_after_contain_keeps_containment(tmp_graph):
    from fno.backlog.advance import selection_guards

    owner, kids = _seed_owner_with_children(tmp_graph, 1)
    kid = kids[0]
    _invoke("backlog", "defer", kid, "--reason", "waiting on the owner")
    assert _invoke("backlog", "contain", owner, kid).exit_code == 0
    r = _invoke("backlog", "undefer", kid)
    assert r.exit_code == 0, r.output
    row = _by_id(tmp_graph)[kid]
    assert not row.get("deferred_at")
    assert row["contained_in"] == owner
    now = datetime.now(timezone.utc)
    assert selection_guards(row, _by_id(tmp_graph), now) == f"contained:{owner}"
