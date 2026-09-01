"""The selection cascade, and the explanation that must not drift from it.

`fno backlog next` narrows the open graph through a fixed sequence of filters
and every drop was silent. The 2026-09-01 orchestration audit had to
reconstruct "why did this node not launch" by reading source.

The load-bearing property under test is not that the explanation is pretty. It
is that `cmd_next._pick_ready` and `advance --explain` run THE SAME cascade
object, so an explanation cannot describe a selection that did not happen.
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
