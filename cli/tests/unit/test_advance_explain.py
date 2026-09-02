"""The selection cascade, and the explanation that must not drift from it.

`fno backlog next` narrows the open graph through a fixed sequence of filters
and every drop was silent. The 2026-09-01 orchestration audit had to
reconstruct "why did this node not launch" by reading source.

The load-bearing property under test is not that the explanation is pretty. It
is that `cmd_next._pick_ready` and `advance --explain` run THE SAME cascade
object, so an explanation cannot describe a selection that did not happen.

Journal isolation: the dry run writes no event; the conftest per-module pin
sets FNO_EVENTS_PATH to a per-test tmp journal anyway, so a future emitter
cannot reach the live file from here.
"""
from __future__ import annotations

from datetime import datetime, timedelta, timezone

from fno.backlog.explain import (
    SelectionFilter,
    build_selection_filters,
    run_cascade,
)

# Fresh, deliberately: the stale-ready guard quarantines a `ready` node
# untouched past the staleness window, so a fixture with a fixed old date is
# dropped by that guard and never reaches the filter under test.
_FRESH = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat()


def _node(node_id: str, **over) -> dict:
    base = {
        "id": node_id,
        "status": "ready",
        "project": "fno",
        "parent": None,
        "pr_number": None,
        "created_at": _FRESH,
        "touched_at": _FRESH,
        "priority": "p2",
    }
    base.update(over)
    return base


# -- the cascade primitive --


def test_a_drop_is_attributed_to_the_first_filter_that_took_it():
    """A node dropped by three filters is reported by the one to act on."""
    nodes = [_node("a"), _node("b")]
    filters = [
        SelectionFilter("first", "gone here", lambda c: [e for e in c if e["id"] != "b"]),
        SelectionFilter("second", "also would", lambda c: [e for e in c if e["id"] != "b"]),
    ]
    r = run_cascade(nodes, filters)
    assert [e["id"] for e in r.survivors] == ["a"]
    assert r.reason_for("b") == "first"
    assert r.reason_for("a") is None


def test_every_filter_reports_its_drop_count_in_cascade_order():
    """AC3-HP. The 856 nodes that did not win are the point of the report."""
    nodes = [_node(x) for x in ("a", "b", "c", "d")]
    filters = [
        SelectionFilter("f1", "", lambda c: [e for e in c if e["id"] not in {"a", "b"}]),
        SelectionFilter("f2", "", lambda c: [e for e in c if e["id"] != "c"]),
        SelectionFilter("f3", "", lambda c: c),
    ]
    r = run_cascade(nodes, filters)
    assert r.drops == [("f1", 2), ("f2", 1), ("f3", 0)]
    assert [e["id"] for e in r.survivors] == ["d"]


def test_a_filter_that_takes_nothing_still_appears():
    """A zero is a fact about that filter, not a reason to omit the row.

    An omitted filter reads as "not evaluated", which is a different claim.
    """
    r = run_cascade([_node("a")], [SelectionFilter("quiet", "", lambda c: c)])
    assert r.drops == [("quiet", 0)]


# -- the real filters --


def test_the_open_pr_filter_names_the_node_it_dropped():
    """AC4-HP. The commonest silent drop: a ready node already in review."""
    nodes = [_node("a"), _node("b", pr_number=1178)]
    filters = build_selection_filters(
        nodes,
        roadmap_id=None,
        mission=None,
        parent_target_id=None,
        project_filter="fno",
        all_=False,
        claimed=frozenset(),
        container_ids=frozenset(),
    )
    r = run_cascade(nodes, filters)
    assert r.reason_for("b") == "unmerged-open-pr"
    assert [e["id"] for e in r.survivors] == ["a"]


def test_a_container_is_dropped_and_says_so():
    nodes = [_node("epic"), _node("leaf")]
    filters = build_selection_filters(
        nodes,
        roadmap_id=None,
        mission=None,
        parent_target_id=None,
        project_filter="fno",
        all_=False,
        claimed=frozenset(),
        container_ids=frozenset({"epic"}),
    )
    r = run_cascade(nodes, filters)
    assert r.reason_for("epic") == "container"


def test_a_live_claim_is_reported_as_a_claim_not_as_absence():
    nodes = [_node("held"), _node("free")]
    filters = build_selection_filters(
        nodes,
        roadmap_id=None,
        mission=None,
        parent_target_id=None,
        project_filter="fno",
        all_=False,
        claimed=frozenset({"held"}),
        container_ids=frozenset(),
    )
    r = run_cascade(nodes, filters)
    assert r.reason_for("held") == "live-claim"


def test_the_project_filter_narrows_a_list_not_a_row():
    """`filter_by_project` DETECTS the project from the candidates it is given.

    Expressing it as a per-row predicate would silently change what it does,
    which is why a filter narrows a list.
    """
    nodes = [_node("a", project="fno"), _node("b", project="c3po")]
    filters = build_selection_filters(
        nodes,
        roadmap_id=None,
        mission=None,
        parent_target_id=None,
        project_filter="fno",
        all_=False,
        claimed=frozenset(),
        container_ids=frozenset(),
    )
    r = run_cascade(nodes, filters)
    assert r.reason_for("b") == "project"


def test_every_filter_carries_an_actionable_why():
    """The report's value is the sentence, not the slug."""
    filters = build_selection_filters(
        [],
        roadmap_id=None,
        mission=None,
        parent_target_id=None,
        project_filter="fno",
        all_=False,
        claimed=frozenset({"x"}),
        container_ids=frozenset(),
    )
    assert filters, "the cascade is never empty"
    for f in filters:
        assert f.why.strip(), f"{f.name} has no explanation"


# ---------------------------------------------------------------------------
# --explain --epic: the daemon's lane-fill cascade (task 5.1, LD5)
#
# The daemon's only walk is active_backlog shelling `advance --epic`, which
# reaches select_lane_fill - a second selector beside next. An epic question
# answered with the next cascade is the second-selector lie, so the epic
# explain runs the fill itself and names ITS drops.
# ---------------------------------------------------------------------------

def _lane_fill_world(monkeypatch, ready, *, high_collision_for=None, max_lanes=2,
                     live_slots=0):
    """Hermetic seams for build_lane_fill_report; returns the advance module."""
    from fno.backlog import advance as adv
    from types import SimpleNamespace

    monkeypatch.setattr(adv, "_spawn_headroom", lambda *a, **k: max_lanes)
    monkeypatch.setattr(adv, "_ready_nodes", lambda project, mission: ready)
    monkeypatch.setattr(adv, "_live_lane_domains", lambda *a, **k: set())
    monkeypatch.setattr(adv, "_live_worked_entries", lambda *a, **k: [])
    monkeypatch.setattr(adv, "_auto_continue_resolve", lambda: (False, "config"))
    monkeypatch.setattr("fno.claims.lanes.find_lane_slot", lambda nid, root=None: None)
    monkeypatch.setattr("fno.claims.lanes.active_lane_count", lambda *a, **k: live_slots)
    # A stated file surface, so the collision gate actually runs for every node.
    monkeypatch.setattr(
        "fno.graph.collision.resolve_plan_path", lambda p: p
    )
    monkeypatch.setattr("fno.graph.collision.has_file_surface", lambda p: True)
    hits = high_collision_for or {}

    def _fake_high_collision(node, inflight):
        target = hits.get(node["id"])
        return SimpleNamespace(with_node_id=target) if target else None

    monkeypatch.setattr(adv, "_high_collision", _fake_high_collision)
    return adv


def _ready_node(nid, **kw):
    base = {"id": nid, "title": f"t-{nid}", "priority": "p1", "difficulty": "m",
            "project": "fno", "domain": "code", "plan_path": "plan.md"}
    base.update(kw)
    return base


def test_epic_explain_counts_drops_under_the_fill_filter_names(monkeypatch):
    """AC14: the collision drop is counted under in-flight-collision, by name."""
    adv = _lane_fill_world(
        monkeypatch,
        [_ready_node("x-win"), _ready_node("x-coll")],
        high_collision_for={"x-coll": "some-plan"},
    )
    from fno.backlog.explain import build_lane_fill_report

    report = build_lane_fill_report(epic="x-epic")
    assert report["mode"] == "lane-fill"
    drops = {d["filter"]: d["dropped"] for d in report["selection"]["drops"]}
    assert drops["in-flight-collision"] == 1
    assert drops["unevaluated"] == 0
    assert [e["id"] for e in report["selection"]["would_fill"]] == ["x-win"]
    assert report["selection"]["stop"] in ("no-candidate", "filled")


def test_epic_explain_names_the_filter_that_dropped_the_asked_node(monkeypatch):
    adv = _lane_fill_world(
        monkeypatch,
        [_ready_node("x-win"), _ready_node("x-coll")],
        high_collision_for={"x-coll": "some-plan"},
    )
    from fno.backlog.explain import build_lane_fill_report

    asked = build_lane_fill_report(epic="x-epic", node_id="x-coll")["asked"]
    assert asked["dropped_by"] == "high-collision:some-plan"

    winner = build_lane_fill_report(epic="x-epic", node_id="x-win")["asked"]
    assert winner["rank"] == 0

    stranger = build_lane_fill_report(epic="x-epic", node_id="x-elsewhere")["asked"]
    assert stranger["never_a_candidate"] is True


def test_epic_explain_renders_no_next_cascade(monkeypatch):
    """The rendered text carries the fill's drops and none of the next cascade."""
    adv = _lane_fill_world(
        monkeypatch,
        [_ready_node("x-win"), _ready_node("x-coll")],
        high_collision_for={"x-coll": "some-plan"},
    )
    from fno.backlog.explain import build_lane_fill_report, render_lane_fill_report

    text = render_lane_fill_report(build_lane_fill_report(epic="x-epic"))
    assert "lane fill" in text
    assert "in-flight-collision" in text
    assert "unmerged-open-pr" not in text
    assert "selection-guard" not in text


def test_cap_full_counts_the_denied_lanes_under_lane_slot(monkeypatch):
    """Preview holds no slot, so the cap is measured live: width 1, 1 held slot,
    three selectable nodes - the two picks beyond headroom are lane-slot drops."""
    _lane_fill_world(
        monkeypatch,
        [_ready_node("x-a"), _ready_node("x-b"), _ready_node("x-c")],
        max_lanes=1,
        live_slots=1,
    )
    from fno.backlog.explain import build_lane_fill_report

    report = build_lane_fill_report(epic="x-epic")
    drops = {d["filter"]: d["dropped"] for d in report["selection"]["drops"]}
    assert report["selection"]["stop"] == "cap-full"
    assert drops.get("lane-slot") == 1
    assert report["selection"]["would_fill"] == []


def test_advance_explain_epic_routes_to_the_fill_not_the_next_cascade(monkeypatch):
    """The CLI wiring: --explain --epic never builds the next cascade."""
    from types import SimpleNamespace

    from fno.backlog import explain

    calls = {"lane_fill": 0, "next": 0}

    def _fake_fill(**kw):
        calls["lane_fill"] += 1
        return {"mode": "lane-fill", "epic": kw.get("epic"), "selection": {},
               "asked": {}, "gates": [], "routing": {}, "decision": {}}

    def _fail_next(**kw):
        calls["next"] += 1
        raise AssertionError("next cascade built for an epic question")

    monkeypatch.setattr(explain, "build_lane_fill_report", _fake_fill)
    monkeypatch.setattr(explain, "render_lane_fill_report", lambda r: "LANE FILL")
    monkeypatch.setattr(explain, "build_report", _fail_next)

    import fno.graph.cli as gcli

    monkeypatch.setattr(
        "fno.backlog.explain.build_lane_fill_report", _fake_fill
    )
    monkeypatch.setattr(gcli, "_display_entries", lambda *a, **k: [])
    from fno.cli import app
    from typer.testing import CliRunner

    result = CliRunner().invoke(
        app, ["backlog", "advance", "--explain", "--epic", "x-epic"]
    )
    assert result.exit_code == 0, result.output
    assert "LANE FILL" in result.output
    assert calls["lane_fill"] == 1 and calls["next"] == 0
