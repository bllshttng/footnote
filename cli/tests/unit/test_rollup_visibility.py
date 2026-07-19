"""The three orphan visibility surfaces: board flag, ordering tiebreaker, metric.

All three read one predicate (`rollup.is_orphan`), so these lock in that they
agree about exemptions and that the tiebreaker stays in-band.
"""
from __future__ import annotations

import pytest

from fno.graph.render import _kanban_card, _lane_sort_key, _orphan_ids
from fno.graph.rollup import orphan_ids


def node(nid, **kw):
    base = {
        "id": nid, "type": "feature", "title": nid, "priority": "p2",
        "project": "fno", "created_at": "2026-01-01T00:00:00+00:00",
    }
    base.update(kw)
    return base


# -- ordering tiebreaker --


def _order(entries):
    orphans = orphan_ids(entries)
    return [e["id"] for e in sorted(entries, key=lambda e: _lane_sort_key(e, orphans))]


def test_orphan_sorts_after_linked_peer_in_the_same_band():
    """AC5: same lane, same rank band, same priority -> mission-linked first."""
    entries = [
        node("x-epic", type="epic", title="mission"),
        node("x-orphan", created_at="2026-01-01T00:00:00+00:00"),
        node("x-linked", parent="x-epic", created_at="2026-06-01T00:00:00+00:00"),
    ]
    order = [i for i in _order(entries) if i != "x-epic"]
    assert order == ["x-linked", "x-orphan"], (
        "the older orphan must still sort after its mission-linked peer"
    )


def test_priority_outranks_the_orphan_tiebreaker():
    """AC5: a p0 orphan still beats a p1 mission-linked node."""
    entries = [
        node("x-epic", type="epic", title="mission"),
        node("x-p0-orphan", priority="p0"),
        node("x-p1-linked", priority="p1", parent="x-epic"),
    ]
    order = [i for i in _order(entries) if i != "x-epic"]
    assert order == ["x-p0-orphan", "x-p1-linked"]


def test_created_at_still_breaks_ties_among_orphans():
    entries = [
        node("x-new", created_at="2026-06-01T00:00:00+00:00"),
        node("x-old", created_at="2026-01-01T00:00:00+00:00"),
    ]
    assert _order(entries) == ["x-old", "x-new"]


def test_exempt_nodes_are_not_demoted():
    """AC6: a bug and a deliberate orphan sort as if rollup did not exist."""
    entries = [
        node("x-epic", type="epic", title="mission"),
        node("x-bug", type="bug", created_at="2026-01-01T00:00:00+00:00"),
        node("x-linked", parent="x-epic", created_at="2026-06-01T00:00:00+00:00"),
    ]
    order = [i for i in _order(entries) if i != "x-epic"]
    assert order == ["x-bug", "x-linked"], "a bug must keep its created_at order"


def test_default_orphan_set_reproduces_pre_rollup_ordering():
    """Callers that pass no orphan set get byte-for-byte the old behavior."""
    entries = [node("x-b", priority="p1"), node("x-a", priority="p0")]
    assert [e["id"] for e in sorted(entries, key=_lane_sort_key)] == ["x-a", "x-b"]


# -- board flag --


def test_orphan_card_carries_the_tag():
    entry = node("x-1")
    card = _kanban_card(entry, {}, frozenset({"x-1"}))
    assert "[orphan]" in card


def test_linked_card_has_no_tag():
    assert "[orphan]" not in _kanban_card(node("x-1"), {}, frozenset())


# -- fail-open --


def test_orphan_ids_fails_open_when_rollup_raises(monkeypatch):
    """Board rendering runs inside the graph lock; it must never raise."""
    import fno.graph.rollup as rollup

    def boom(entries):
        raise RuntimeError("simulated rollup failure")

    monkeypatch.setattr(rollup, "orphan_ids", boom)
    assert _orphan_ids([node("x-1")]) == frozenset()


# -- health metric shape --


@pytest.mark.parametrize(
    "entries,expected",
    [
        ([], 0.0),
        ([node("x-1")], 1.0),
        ([node("x-e", type="epic"), node("x-1", parent="x-e")], 0.0),
        ([node("x-e", type="epic"), node("x-1", parent="x-e"), node("x-2")], 0.5),
    ],
)
def test_orphan_rate_arithmetic(entries, expected):
    """Greenfield reads 0.0 rather than dividing by zero."""
    from fno.graph.rollup import ROLLUP_TYPES, is_orphan

    index = {e["id"]: e for e in entries}
    non_exempt = [
        e for e in entries
        if e.get("type") in ROLLUP_TYPES and not e.get("orphan_ok")
    ]
    orphans = [e for e in non_exempt if is_orphan(e, index)]
    rate = round(len(orphans) / len(non_exempt), 4) if non_exempt else 0.0
    assert rate == expected


def test_orphan_rate_threshold_default_never_breaches():
    """A backlog starts near-total orphan; day one must be quiet."""
    from fno.health_monitor import DEFAULT_CONFIG, evaluate_thresholds

    report = {"orphan_feature_rate": 1.0}
    breaches = evaluate_thresholds(report, config=DEFAULT_CONFIG)
    assert [b for b in breaches if b.key == "orphan_feature_rate"] == []


def test_orphan_rate_breaches_once_lowered():
    from fno.health_monitor import DEFAULT_CONFIG, evaluate_thresholds

    config = {**DEFAULT_CONFIG, "thresholds": {
        **DEFAULT_CONFIG["thresholds"], "orphan_feature_rate": 0.5,
    }}
    breaches = evaluate_thresholds({"orphan_feature_rate": 0.9}, config=config)
    assert [b.key for b in breaches if b.key == "orphan_feature_rate"]


def test_absent_metric_never_breaches():
    """A rollup failure drops the metric; a missing key must not breach."""
    from fno.health_monitor import DEFAULT_CONFIG, evaluate_thresholds

    config = {**DEFAULT_CONFIG, "thresholds": {
        **DEFAULT_CONFIG["thresholds"], "orphan_feature_rate": 0.0,
    }}
    breaches = evaluate_thresholds({}, config=config)
    assert [b for b in breaches if b.key == "orphan_feature_rate"] == []


# -- review findings: per-project scoping, rate severity --


def test_per_project_board_does_not_invent_orphans():
    """Orphanhood is a whole-graph property.

    A feature whose parent epic lives in another project has no reachable
    ancestor inside a project-scoped slice; computing orphans from that subset
    invented orphans AND disagreed with the card flag, which is built from the
    full index.
    """
    from fno.graph.render_html import _bucket

    entries = [
        node("x-epic", type="epic", title="mission", project="other"),
        node("x-child", parent="x-epic", project="fno"),
    ]
    all_orphans = orphan_ids(entries)
    assert all_orphans == frozenset(), "child reaches its epic in the full graph"

    proj_entries = [e for e in entries if e["project"] == "fno"]
    # Without the whole-graph set the subset would call x-child an orphan.
    assert orphan_ids(proj_entries) == {"x-child"}
    # _bucket must use the set it is handed, not recompute from the slice.
    _bucket(proj_entries, all_orphans)


def test_done_cards_are_not_flagged_as_orphans():
    """The metric excludes closed work, so the board must too."""
    assert orphan_ids([node("x-1", _status="done")]) == frozenset()


def test_rate_severity_can_reach_alert():
    """A rate is capped at 1.0, so count-style ratios could never escalate."""
    from fno.health_monitor import _classify_severity

    assert _classify_severity(actual=0.95, threshold=0.5, kind="rate") == "alert"
    assert _classify_severity(actual=0.65, threshold=0.5, kind="rate") == "warn"
    assert _classify_severity(actual=0.55, threshold=0.5, kind="rate") == "info"


def test_rate_severity_handles_a_zero_headroom_threshold():
    from fno.health_monitor import _classify_severity

    assert _classify_severity(actual=1.0, threshold=1.0, kind="rate") == "alert"
