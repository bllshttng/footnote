"""Scope growth per epic, and its refusal to report past its evidence (x-d157).

The metric answers "how much did this epic grow after decomposition" from the
source_node_id edge. At the capture rate it shipped against, a low growth figure
is indistinguishable from poor capture -- and errs low, which flatters the
process. So coverage travels with the number and the number is withheld below a
floor.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.graph.rollup import SCOPE_GROWTH_COVERAGE_FLOOR, scope_growth

runner = CliRunner()

EPIC_BORN = "2026-01-01T00:00:00+00:00"
LATER = "2026-02-01T00:00:00+00:00"


def _node(node_id: str, **over) -> dict:
    base = {
        "id": node_id,
        "title": f"Node {node_id}",
        "_status": "ready",
        "domain": "code",
        "project": "fno",
        "slug": f"node-{node_id}",
        "created_at": LATER,
    }
    base.update(over)
    return base


def _epic(node_id: str = "x-epic", **over) -> dict:
    return _node(node_id, type="epic", created_at=EPIC_BORN, **over)


# ---------------------------------------------------------------------------
# The follow-up set
# ---------------------------------------------------------------------------


def test_follow_up_is_reachable_by_origin_but_not_a_planned_child():
    """A node produced by a child counts as growth; a decomposed sibling does not."""
    entries = [
        _epic(),
        _node("x-c1", parent="x-epic"),
        _node("x-c2", parent="x-epic"),           # decomposed in, not grown
        _node("x-f1", source_node_id="x-c1"),     # grown out of a child
    ]
    growth = scope_growth(entries, "x-epic", floor=0.0)
    assert growth.follow_up_ids == ("x-f1",)


def test_a_child_that_also_carries_an_origin_is_still_not_growth():
    """parent wins: work planned into the epic was never grown, however it was filed."""
    entries = [
        _epic(),
        _node("x-c1", parent="x-epic"),
        _node("x-c2", parent="x-epic", source_node_id="x-c1"),
    ]
    assert scope_growth(entries, "x-epic", floor=0.0).follow_up_ids == ()


def test_follow_ups_are_transitive():
    """A follow-up's own follow-up is still work the epic grew."""
    entries = [
        _epic(),
        _node("x-c1", parent="x-epic"),
        _node("x-f1", source_node_id="x-c1"),
        _node("x-f2", source_node_id="x-f1"),
    ]
    assert scope_growth(entries, "x-epic", floor=0.0).follow_up_ids == ("x-f1", "x-f2")


def test_origin_cycle_terminates():
    """A mutually-attributing pair must not hang the walk."""
    entries = [
        _epic(),
        _node("x-c1", parent="x-epic"),
        _node("x-f1", source_node_id="x-c1"),
        _node("x-f2", source_node_id="x-f1"),
    ]
    entries[2]["source_node_id"] = "x-f2"  # f1 <- f2 <- f1, reachable from no child
    entries.append(_node("x-f3", source_node_id="x-c1"))
    growth = scope_growth(entries, "x-epic", floor=0.0)
    assert "x-f3" in growth.follow_up_ids


def test_empty_epic_reports_zero_growth_not_an_error():
    """An epic with no children and no follow-ups is a clean zero."""
    growth = scope_growth([_epic()], "x-epic", floor=0.0)
    assert growth.follow_up_ids == ()
    assert growth.realized_nodes == 0


# ---------------------------------------------------------------------------
# AC3-FR: the metric refuses to report past its evidence
# ---------------------------------------------------------------------------


def _window(with_origin: int, without_origin: int) -> list[dict]:
    """An epic with one child and one real follow-up, plus a window of the
    given capture ratio.

    The follow-up (x-grew, produced by the child) is what makes the reportable
    case have something to report; the x-w/x-n filler only moves coverage.
    """
    entries = [
        _epic(),
        _node("x-c1", parent="x-epic"),
        _node("x-seed", created_at=EPIC_BORN),
        _node("x-grew", source_node_id="x-c1"),
    ]
    for i in range(with_origin):
        entries.append(_node(f"x-w{i:03d}", source_node_id="x-seed"))
    for i in range(without_origin):
        entries.append(_node(f"x-n{i:03d}"))
    return entries


def test_ac3_fr_low_coverage_suppresses_the_headline():
    """AC3-FR: below the floor, the figure is withheld and the coverage is stated."""
    growth = scope_growth(_window(with_origin=2, without_origin=8), "x-epic", floor=0.5)
    assert growth.reportable is False
    # Fixture-derived, not recomputed from the result's own fields: the window
    # is x-c1 + x-seed + x-grew + 2 with-origin + 8 without = 13, of which
    # x-grew and the 2 fillers carry a joinable origin.
    assert (growth.window_total, growth.window_with_origin) == (13, 3)
    # Suppression is not silence: the count is withheld, the evidence is not.
    assert growth.follow_up_ids is None


def test_ac3_fr_the_same_epic_reports_once_coverage_clears_the_floor():
    """AC3-FR: above the floor the figure prints, with its coverage alongside."""
    growth = scope_growth(_window(with_origin=8, without_origin=2), "x-epic", floor=0.5)
    assert growth.reportable is True
    assert (growth.window_total, growth.window_with_origin) == (13, 9)
    assert growth.follow_up_ids == ("x-grew",)


def test_coverage_window_excludes_nodes_older_than_the_epic():
    """A node that predates the epic could not have named it, so it is not evidence."""
    entries = [
        _epic(),
        _node("x-old", created_at="2025-01-01T00:00:00+00:00"),
        _node("x-new", source_node_id="x-old"),
    ]
    growth = scope_growth(entries, "x-epic", floor=0.0)
    assert growth.window_total == 1  # x-new only


def test_ground_truth_join_reports_realized_cost():
    """Realized nodes and PRs travel with the figure so it can be falsified.

    An epic reporting near-zero growth that nonetheless shipped far past its
    declared size is evidence against the capture, not about the epic.
    """
    entries = [
        _epic(size="M"),
        _node("x-c1", parent="x-epic", pr_number=11),
        _node("x-c2", parent="x-epic", pr_number=12),
        _node("x-c3", parent="x-epic", pr_number=12),  # same PR, counted once
    ]
    growth = scope_growth(entries, "x-epic", floor=0.0)
    assert (growth.realized_nodes, growth.realized_prs, growth.declared_size) == (3, 2, "M")


def test_the_floor_is_a_real_constant_not_zero():
    """A floor of 0 would make the suppression branch dead code."""
    assert 0.0 < SCOPE_GROWTH_COVERAGE_FLOOR <= 1.0


# ---------------------------------------------------------------------------
# The epic verb surfaces it
# ---------------------------------------------------------------------------


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
    return seed


def test_epic_status_states_why_a_figure_is_withheld(graph):
    graph(_window(with_origin=1, without_origin=9))
    out = runner.invoke(app, ["backlog", "epic", "status", "x-epic"]).output
    assert "scope growth: withheld" in out
    assert "below the" in out and "floor" in out


def test_epic_status_json_never_emits_a_count_it_cannot_support(graph):
    graph(_window(with_origin=1, without_origin=9))
    payload = json.loads(
        runner.invoke(app, ["backlog", "epic", "status", "x-epic", "--json"]).stdout
    )
    sg = payload["scope_growth"]
    assert sg["reportable"] is False
    assert sg["follow_ups"] is None, "a suppressed figure must be null, not 0"
    # Coverage evidence ships regardless, so the null explains itself.
    assert sg["window_total"] > 0


def test_a_dangling_origin_is_not_counted_as_capture():
    """An origin naming a node the graph no longer has joins nothing.

    Counting it would inflate coverage past the floor while the walk finds
    zero follow-ups, licensing a confidently-measured zero - the same
    flattering error the floor exists to prevent, arriving from the other side.
    """
    entries = [_epic(), _node("x-c1", parent="x-epic")]
    for i in range(9):
        entries.append(_node(f"x-d{i:03d}", source_node_id="x-vanished"))

    growth = scope_growth(entries, "x-epic", floor=0.5)
    assert growth.window_with_origin == 0
    assert growth.window_dangling == 9
    assert growth.reportable is False


def test_an_undated_epic_has_no_window_rather_than_the_whole_graph():
    """`created_at >= ""` would admit every node ever filed."""
    entries = [_node("x-epic", type="epic", created_at=None),
               _node("x-other", source_node_id="x-other")]
    growth = scope_growth(entries, "x-epic", floor=0.5)
    assert growth.window_total == 0
    assert growth.reportable is False


def test_epic_status_renders_the_reportable_line(graph):
    """The happy-path line is user-facing surface and needs rendering at least once."""
    graph(_window(with_origin=9, without_origin=1))
    out = runner.invoke(app, ["backlog", "epic", "status", "x-epic"]).output
    assert "scope growth: 1 follow-ups" in out
    assert "origin capture" in out
    assert "realized" in out
