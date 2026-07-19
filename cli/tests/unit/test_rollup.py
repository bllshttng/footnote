"""Rollup resolution: orphan predicate, epic matching, and the intake ladder."""
from __future__ import annotations

import pytest

from fno.graph.relatedness import epic_candidates
from fno.graph.rollup import (
    AUTO_LINK_MARGIN,
    AUTO_LINK_MIN,
    has_epic_ancestor,
    is_orphan,
    orphan_ids,
    receipt_lines,
    resolve,
)


def node(nid, **kw):
    base = {"id": nid, "type": "feature", "title": nid, "details": ""}
    base.update(kw)
    return base


def epic(nid, title, **kw):
    return node(nid, type="epic", title=title, **kw)


# -- orphan predicate --


def test_feature_with_no_parent_is_orphan():
    entries = [node("x-1")]
    assert orphan_ids(entries) == {"x-1"}


def test_feature_parented_to_epic_is_not_orphan():
    entries = [epic("x-e", "mux polish"), node("x-1", parent="x-e")]
    assert orphan_ids(entries) == frozenset()


def test_epic_ancestor_found_through_intermediate_node():
    """A leaf under a group child under an epic still has a mission edge."""
    entries = [
        epic("x-e", "mux polish"),
        node("x-mid", parent="x-e"),
        node("x-leaf", parent="x-mid"),
    ]
    assert orphan_ids(entries) == frozenset()


def test_parent_pointing_at_missing_node_is_orphan():
    entries = [node("x-1", parent="x-gone")]
    assert orphan_ids(entries) == {"x-1"}


def test_parent_cycle_terminates_and_reports_orphan():
    entries = [node("x-a", parent="x-b"), node("x-b", parent="x-a")]
    assert orphan_ids(entries) == {"x-a", "x-b"}


@pytest.mark.parametrize("type_", ["bug", "epic", "roadmap"])
def test_non_rollup_types_are_never_orphans(type_):
    """AC6: bugs are exempt by type; so are containers."""
    entries = [node("x-1", type=type_)]
    assert orphan_ids(entries) == frozenset()


def test_orphan_ok_exempts_the_node():
    """AC6: a deliberate orphan is invisible to the predicate."""
    entries = [node("x-1", orphan_ok="infra")]
    assert orphan_ids(entries) == frozenset()


def test_empty_orphan_ok_does_not_exempt():
    """An empty reason is not an opt-out - it is an unset field."""
    entries = [node("x-1", orphan_ok="")]
    assert orphan_ids(entries) == {"x-1"}


def test_has_epic_ancestor_on_malformed_entries():
    assert has_epic_ancestor({"parent": None}, {}) is False
    assert has_epic_ancestor({}, {}) is False


def test_is_orphan_tolerates_non_dict():
    assert is_orphan("not-a-node", {}) is False  # type: ignore[arg-type]


# -- epic candidate scoring --


def test_epic_candidates_ranks_the_overlapping_epic_first():
    entries = [
        epic("x-mux", "mux pane layout polish"),
        epic("x-mail", "mail relay delivery"),
    ]
    subject = node("x-1", title="mux pane layout resize polish")
    ranked = epic_candidates(subject, entries)
    assert ranked[0][0] == "x-mux"


def test_epic_candidates_ignores_non_epics_and_self():
    entries = [
        node("x-1", title="mux pane layout"),
        node("x-2", type="feature", title="mux pane layout"),
    ]
    assert epic_candidates(entries[0], entries) == []


@pytest.mark.parametrize("status", ["done", "superseded", "deferred"])
def test_retired_epics_are_not_candidates(status):
    entries = [epic("x-e", "mux pane layout polish", _status=status)]
    subject = node("x-1", title="mux pane layout polish")
    assert epic_candidates(subject, entries) == []


def test_epic_candidates_on_empty_graph():
    assert epic_candidates(node("x-1"), []) == []


def test_epic_candidates_handles_empty_details():
    """Boundary: title/slug tokens alone still score."""
    entries = [epic("x-e", "billing invoice export")]
    subject = node("x-1", title="billing invoice export", details=None)
    assert epic_candidates(subject, entries)[0][0] == "x-e"


def test_epic_candidates_is_deterministic_on_ties():
    entries = [epic("x-b", "shared words here"), epic("x-a", "shared words here")]
    subject = node("x-1", title="shared words here")
    ids = [c[0] for c in epic_candidates(subject, entries)]
    assert ids == sorted(ids)


# -- the ladder --


def test_ladder_auto_links_a_clear_match():
    """AC1: far above the bar with a clear margin -> linked."""
    entries = [epic("x-mux", "mux pane layout polish"), epic("x-mail", "mail relay")]
    subject = node("x-1", title="mux pane layout polish", parent=None)
    entries.append(subject)
    res = resolve(subject, entries)
    assert res.kind == "linked"
    assert res.epic_id == "x-mux"
    assert res.score >= AUTO_LINK_MIN


def test_ladder_suggests_when_margin_is_too_thin():
    """AC2: two near-tied epics never coin-flip a parent edge."""
    entries = [
        epic("x-a", "billing invoice export pipeline"),
        epic("x-b", "billing invoice export workflow"),
    ]
    subject = node("x-1", title="billing invoice export")
    entries.append(subject)
    res = resolve(subject, entries)
    assert res.kind == "suggest"
    top, second = res.candidates[0][1], res.candidates[1][1]
    assert (top - second) < AUTO_LINK_MARGIN


def test_ladder_reports_orphan_with_no_candidates():
    entries = [epic("x-e", "totally unrelated domain")]
    subject = node("x-1", title="quantum teapot calibration")
    entries.append(subject)
    assert resolve(subject, entries).kind == "orphan"


def test_ladder_is_silent_on_a_graph_with_no_epics():
    """Greenfield: with no mission to serve, the orphan line is unactionable.

    The node is still an orphan to the metric and the board flag - only the
    intake line is suppressed, so intake does not narrate the obvious on every
    single add.
    """
    subject = node("x-1")
    assert resolve(subject, [subject]).kind == "exempt"
    assert orphan_ids([subject]) == {"x-1"}, "still an orphan to the metric"


def test_ladder_reports_orphan_once_a_live_epic_exists():
    entries = [epic("x-e", "totally unrelated domain"), node("x-1", title="quantum teapot")]
    assert resolve(entries[1], entries).kind == "orphan"


def test_retired_epics_do_not_re_enable_the_orphan_line():
    entries = [epic("x-e", "unrelated", _status="done"), node("x-1", title="quantum teapot")]
    assert resolve(entries[1], entries).kind == "exempt"


def test_ladder_exempts_bugs_and_deliberate_orphans():
    entries = [epic("x-e", "mux pane layout polish")]
    bug = node("x-b", type="bug", title="mux pane layout polish")
    deliberate = node("x-d", title="mux pane layout polish", orphan_ok="spike")
    assert resolve(bug, entries + [bug]).kind == "exempt"
    assert resolve(deliberate, entries + [deliberate]).kind == "exempt"


def test_ladder_exempts_an_already_linked_node():
    entries = [epic("x-e", "mux pane layout polish")]
    linked = node("x-1", title="mux pane layout polish", parent="x-e")
    assert resolve(linked, entries + [linked]).kind == "exempt"


def test_resolve_never_mutates_the_node():
    """Invariant: rollup resolution is pure; the caller applies it."""
    entries = [epic("x-mux", "mux pane layout polish")]
    subject = node("x-1", title="mux pane layout polish")
    before = dict(subject)
    resolve(subject, entries + [subject])
    assert subject == before


# -- receipts --


def test_linked_receipt_names_epic_score_and_undo():
    entries = [epic("x-mux", "mux pane layout polish")]
    index = {e["id"]: e for e in entries}
    res = resolve(node("x-1", title="mux pane layout polish"), entries)
    (line,) = receipt_lines(res, "x-1", index)
    assert "auto-linked x-1 -> x-mux" in line
    assert "mux pane layout polish" in line
    assert "fno backlog update x-1 --parent null" in line


def test_orphan_receipt_offers_the_orphan_ok_hint():
    entries = [epic("x-e", "unrelated mission"), node("x-1", title="quantum teapot")]
    (line,) = receipt_lines(resolve(entries[1], entries), "x-1", {})
    assert "--orphan-ok" in line


def test_suggest_receipt_lists_copy_paste_commands():
    entries = [
        epic("x-a", "billing invoice export pipeline"),
        epic("x-b", "billing invoice export workflow"),
    ]
    index = {e["id"]: e for e in entries}
    subject = node("x-1", title="billing invoice export")
    lines = receipt_lines(resolve(subject, entries + [subject]), "x-1", index)
    assert any("--parent x-a" in ln for ln in lines)
    assert any("--parent x-b" in ln for ln in lines)


def test_exempt_resolution_prints_nothing():
    assert receipt_lines(resolve(node("x-1", type="bug"), []), "x-1", {}) == []


# -- review findings: explicit parent, closed work --


def test_an_explicit_non_epic_parent_is_never_overwritten():
    """Rollup proposes an edge where none exists; it never overrules a human.

    `--parent <feature>` is accepted by add/idea by design. Auto-linking over it
    would destroy operator intent, and the printed undo (`--parent null`) could
    not restore it.
    """
    entries = [
        epic("x-epic", "billing invoice export"),
        node("x-host", title="host feature"),
    ]
    child = node("x-1", title="billing invoice export", parent="x-host")
    res = resolve(child, entries + [child])
    assert res.kind == "exempt"
    assert res.epic_id is None


@pytest.mark.parametrize("status", ["done", "superseded", "deferred"])
def test_closed_work_is_never_an_orphan(status):
    """A shipped feature is history, not a rollup anyone can still make."""
    entries = [node("x-1", _status=status)]
    assert orphan_ids(entries) == frozenset()


def test_open_statuses_still_count():
    for status in ("ready", "idea", "blocked", "claimed", None):
        assert orphan_ids([node("x-1", _status=status)]) == {"x-1"}, status
