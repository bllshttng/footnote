"""Unit tests for fno.graph.render - kanban rendering."""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.graph.render import (
    UNSCOPED_LABEL,
    _kanban_column,
    _kanban_card,
    _lane_sort_key,
    _project_key,
    _rank_band,
    render_graph_md,
    _graph_sort_key,
    in_progress_epic_ids,
)


def _entry(eid: str, **kwargs) -> dict:
    base = {
        "id": eid,
        "title": eid,
        "type": "feature",
        "priority": "p2",
        "completed_at": None,
        "session_id": None,
        "status": "ready",
        "blocked_by": [],
        "plan_path": None,
        "pr_url": None,
        "created_at": "2026-01-01T00:00:00Z",
    }
    base.update(kwargs)
    return base


# -- _kanban_column --

def test_column_ready_p2_goes_next():
    """Intent mapping: ready+p2 (this-week-ish) lands in Next."""
    e = _entry("ab-11111111", status="ready", priority="p2")
    assert _kanban_column(e) == "Next"


def test_column_ready_p0_goes_now():
    """p0 (drop-everything) is today-ish - Now column regardless of session."""
    e = _entry("ab-11111112", status="ready", priority="p0")
    assert _kanban_column(e) == "Now"


def test_column_ready_p1_goes_now():
    """p1 (next-up) is today-or-tomorrow-ish - Now column."""
    e = _entry("ab-11111113", status="ready", priority="p1")
    assert _kanban_column(e) == "Now"


def test_column_ready_p3_goes_later():
    """p3 (long-tail) lands in Later regardless of session state."""
    e = _entry("ab-11111114", status="ready", priority="p3")
    assert _kanban_column(e) == "Later"


def test_column_claimed_overrides_priority():
    """A claimed (in-session) node lands in Now even if it'd otherwise be Later."""
    e = _entry("ab-22222222", status="in_progress", priority="p3")
    assert _kanban_column(e) == "Now"


def test_column_queued_goes_triage():
    """A queued node (awaiting human ack) lands in Triage, not Now, regardless
    of priority - it must not inflate the Now lane (ab-95a4a479)."""
    e = _entry("ab-22222223", status="ready", priority="p3", queued_at="2026-05-12T12:00:00Z")
    assert _kanban_column(e) == "Triage"
    # priority is irrelevant once queued: a queued p1 still goes to Triage.
    e_p1 = _entry("ab-2222222a", status="ready", priority="p1", queued_at="2026-05-12T12:00:00Z")
    assert _kanban_column(e_p1) == "Triage"


def test_column_claimed_beats_queued():
    """claimed wins over queued: an actively-worked node stays in Now even if
    it also carries queued_at."""
    e = _entry(
        "ab-2222222b",
        status="in_progress",
        priority="p3",
        queued_at="2026-05-12T12:00:00Z",
    )
    assert _kanban_column(e) == "Now"


def test_column_queued_does_not_override_deferred():
    """A queued+deferred node is excluded - the explicit pause wins over queue intent."""
    e = _entry(
        "ab-22222224",
        status="deferred",
        deferred_at="2026-05-12T12:00:00Z",
        queued_at="2026-05-11T12:00:00Z",
    )
    assert _kanban_column(e) is None


def test_column_blocked_rides_priority():
    """Blocked is no longer a Later override - it rides its priority. Surface as visual flag."""
    e = _entry("ab-33333331", status="blocked", priority="p1")
    assert _kanban_column(e) == "Now"
    e2 = _entry("ab-33333332", status="blocked", priority="p2")
    assert _kanban_column(e2) == "Next"
    e3 = _entry("ab-33333333", status="blocked", priority="p3")
    assert _kanban_column(e3) == "Later"


def test_column_done_goes_done():
    e = _entry("ab-44444444", status="done", completed_at="2026-01-01T00:00:00Z")
    assert _kanban_column(e) == "Done"


def test_column_deferred_is_excluded():
    """Deferred rows drop off the board entirely; reactivate to bring them back."""
    e = _entry(
        "ab-55555555",
        deferred_at="2026-01-01T00:00:00Z",
        deferred_reason="stale",
        status="deferred",
    )
    assert _kanban_column(e) is None


def test_column_superseded_is_excluded():
    """Superseded rows drop off the board (the successor carries the work)."""
    e = _entry("ab-55555556", status="superseded", superseded_by="ab-aaaaaaaa")
    assert _kanban_column(e) is None


def test_column_idea_rides_priority():
    """Idea status (no plan yet) still rides priority. Surface 'needs plan' as visual flag."""
    e = _entry("ab-55555557", status="idea", priority="p1")
    assert _kanban_column(e) == "Now"


def test_ac1_hp_column_roadmap_excluded():
    e = _entry("ab-66666666", type="roadmap")
    assert _kanban_column(e) is None


# -- in-progress epic -> Now (x-33b2) --


def test_in_progress_epic_ids_detects_done_or_claimed_child():
    entries = [
        _entry("ab-epic0001"),                                   # in-progress (claimed child)
        _entry("ab-kid00001", status="in_progress", parent="ab-epic0001"),
        _entry("ab-epic0002"),                                   # in-progress (done child)
        _entry("ab-kid00002", completed_at="2026-01-01T00:00:00Z", parent="ab-epic0002"),
        _entry("ab-epic0003"),                                   # NOT in progress (ready child)
        _entry("ab-kid00003", parent="ab-epic0003"),
        _entry("ab-loose001"),                                   # not a parent at all
    ]
    ids = in_progress_epic_ids(entries)
    assert ids == frozenset({"ab-epic0001", "ab-epic0002"})


def test_column_in_progress_epic_goes_now():
    """An in-progress epic (passed in the set) lands in Now even at p3, derived
    from its children - its own status is left untouched (still `ready`)."""
    epic = _entry("ab-epic0001", priority="p3")  # would be Later by priority
    assert _kanban_column(epic, frozenset({"ab-epic0001"})) == "Now"
    # status was never mutated to a session-less "in_progress".
    assert epic["status"] == "ready"


def test_column_epic_not_in_progress_rides_priority():
    """A parent with no started children is NOT forced to Now - it rides its
    priority column like any other node (the promotion is in-progress only)."""
    epic = _entry("ab-epic0003", priority="p2")
    assert _kanban_column(epic, frozenset()) == "Next"


def test_column_done_epic_stays_done_even_if_in_progress_set():
    """A completed epic is Done; the in-progress-epic override never resurrects a
    done container into Now (done precedence wins)."""
    epic = _entry("ab-epic0004", completed_at="2026-01-01T00:00:00Z")
    assert _kanban_column(epic, frozenset({"ab-epic0004"})) == "Done"


# -- _kanban_card --

def test_ac1_hp_card_basic_format():
    e = _entry("ab-77777777", title="My Feature")
    card = _kanban_card(e, {})
    assert "My Feature" in card
    assert "ab-77777777" in card
    assert "[ ]" in card


def test_ac1_hp_card_done_shows_x():
    e = _entry("ab-88888888", title="Done Feature", completed_at="2026-01-01T00:00:00Z")
    card = _kanban_card(e, {})
    assert "[x]" in card


def test_ac1_hp_card_shows_plan_path():
    e = _entry("ab-99999999", title="With Plan", plan_path="plans/feature.md")
    card = _kanban_card(e, {})
    assert "plans/feature.md" in card


# -- _graph_sort_key --

def test_ac1_hp_sort_key_priority_order():
    p0 = _entry("ab-aaaaaaa0", priority="p0")
    p1 = _entry("ab-aaaaaaaa", priority="p1")
    p2 = _entry("ab-bbbbbbbb", priority="p2")
    p3 = _entry("ab-cccccccc", priority="p3")
    assert _graph_sort_key(p0) < _graph_sort_key(p1) < _graph_sort_key(p2) < _graph_sort_key(p3)


def test_sort_key_tolerates_null_created_at(tmp_path):
    """ab-6be35f53: sorting same-priority nodes where created_at is None (an
    explicit null in graph.json, which _apply_graph_defaults never backfills)
    must not raise 'NoneType < str'. Covers both _graph_sort_key (Next/etc.)
    and the Done column's completed_at sort."""
    # Mix of null and real timestamps at the same priority - the exact crash shape.
    nodes = [
        _entry("ab-null0001", priority="p2", created_at=None),
        _entry("ab-real0001", priority="p2", created_at="2026-02-02T00:00:00Z"),
        _entry("ab-null0002", priority="p2", created_at=None),
    ]
    # Direct: sorting by the key must not raise and null sorts before real.
    ordered = sorted(nodes, key=_graph_sort_key)
    assert _graph_sort_key(nodes[0]) == (2, "")
    assert ordered[-1]["id"] == "ab-real0001"

    # Done column path: two completed nodes with null completed_at must not crash.
    done = [
        _entry("ab-done0001", status="done", completed_at=None),
        _entry("ab-done0002", status="done", completed_at=None),
    ]
    output = tmp_path / "graph.md"
    render_graph_md(done + nodes, output)  # must not raise TypeError
    assert output.exists()


# -- _project_key / _lane_sort_key (ab-95a4a479: swimlanes + ranking) --

def test_project_key_unscoped_for_null_or_blank():
    assert _project_key(_entry("ab-pk000001", project=None)) == UNSCOPED_LABEL
    assert _project_key(_entry("ab-pk000002", project="")) == UNSCOPED_LABEL
    assert _project_key(_entry("ab-pk000003", project="  ")) == UNSCOPED_LABEL
    assert _project_key(_entry("ab-pk000004", project="web")) == "web"


def test_lane_sort_key_clusters_by_project():
    """AC2-HP: the lane key groups cards by project (contiguous runs)."""
    web1 = _entry("ab-la000001", project="web")
    web2 = _entry("ab-la000002", project="web")
    etl1 = _entry("ab-la000003", project="etl")
    ordered = sorted([web1, etl1, web2], key=_lane_sort_key)
    projs = [_project_key(e) for e in ordered]
    # each project's cards are contiguous (no interleaving)
    assert projs == ["etl", "web", "web"]


def test_lane_sort_key_unscoped_lane_sorts_last():
    """AC2-UI: the (unscoped) lane orders after every named project lane."""
    named = _entry("ab-la000010", project="zeta")
    unscoped = _entry("ab-la000011", project=None)
    ordered = sorted([unscoped, named], key=_lane_sort_key)
    assert [_project_key(e) for e in ordered] == ["zeta", UNSCOPED_LABEL]


def test_lane_sort_key_ranked_precedes_unranked_within_lane():
    """Invariant: a ranked card leads unranked cards in the same lane,
    even when the unranked card has higher priority."""
    ranked = _entry("ab-la000020", project="web", priority="p3", rank=0.0)
    unranked_hi = _entry("ab-la000021", project="web", priority="p0")
    ordered = sorted([unranked_hi, ranked], key=_lane_sort_key)
    assert [e["id"] for e in ordered] == ["ab-la000020", "ab-la000021"]


def test_rank_band_excludes_bool_and_nonfinite():
    """A bool, NaN, inf, or huge-int rank degrades to the unranked band (1, 0.0)
    so the sort key stays a total order (NaN compares False both ways) and the
    render never raises (a giant int would raise OverflowError on float())."""
    assert _rank_band(_entry("ab-rb000001", rank=None)) == (1, 0.0)
    assert _rank_band(_entry("ab-rb000002", rank=True)) == (1, 0.0)
    assert _rank_band(_entry("ab-rb000003", rank=float("nan"))) == (1, 0.0)
    assert _rank_band(_entry("ab-rb000004", rank=float("inf"))) == (1, 0.0)
    assert _rank_band(_entry("ab-rb000006", rank=10**400)) == (1, 0.0)  # no OverflowError
    assert _rank_band(_entry("ab-rb000005", rank=2.5)) == (0, 2.5)


def test_render_md_tolerates_huge_int_rank(tmp_path):
    """A huge-int rank (hand-edited graph.json) must not raise out of the render
    path: render fires inside locked_mutate_graph and only OSError is swallowed."""
    entries = [_entry("ab-rb000020", project="web", priority="p1", rank=10**400)]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)  # must not raise OverflowError
    assert output.exists()
    assert "ab-rb000020" in output.read_text()


def test_lane_sort_key_total_order_with_nan_rank():
    """A NaN-ranked card does not break sorting (degrades to unranked band)."""
    nan_card = _entry("ab-rb000010", project="web", rank=float("nan"))
    ranked = _entry("ab-rb000011", project="web", rank=1.0)
    ordered = sorted([nan_card, ranked], key=_lane_sort_key)  # must not raise
    # the genuinely-ranked card leads; the NaN card falls to the unranked flow
    assert ordered[0]["id"] == "ab-rb000011"


def test_lane_sort_key_ranked_orders_by_rank_ascending():
    a = _entry("ab-la000030", project="web", rank=2.0)
    b = _entry("ab-la000031", project="web", rank=1.0)
    ordered = sorted([a, b], key=_lane_sort_key)
    assert [e["id"] for e in ordered] == ["ab-la000031", "ab-la000030"]


# -- render_graph_md --

def test_ac1_hp_render_creates_file(tmp_path):
    """AC1-HP: render_graph_md creates a kanban markdown file."""
    entries = [_entry("ab-dddddddd", title="Render Test")]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    assert output.exists()
    content = output.read_text()
    assert "kanban-plugin: board" in content
    assert "Render Test" in content


def test_ac1_hp_render_columns_present(tmp_path):
    """AC1-HP: render_graph_md includes all kanban columns, incl. Triage."""
    entries = [_entry("ab-eeeeeeee")]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    content = output.read_text()
    for col in ("Now", "Next", "Later", "Triage", "Done"):
        assert f"## {col}" in content


def test_render_obsidian_false_omits_kanban_scaffolding(tmp_path):
    """obsidian=False drops the Kanban-plugin frontmatter and settings block
    (ab-917f813e) while keeping a usable plain-markdown column board."""
    entries = [_entry("ab-ff000001", title="Plain Card")]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output, obsidian=False)
    content = output.read_text()
    assert "kanban-plugin: board" not in content
    assert "%% kanban:settings" not in content
    assert not content.startswith("---")
    # Columns + card content still render.
    assert "## Now" in content
    assert "Plain Card" in content


def test_render_obsidian_true_keeps_kanban_scaffolding(tmp_path):
    """obsidian=True (the default) keeps the Obsidian Kanban scaffolding."""
    entries = [_entry("ab-ff000002", title="Obsidian Card")]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output, obsidian=True)
    content = output.read_text()
    assert content.startswith("---\nkanban-plugin: board\n---")
    assert "%% kanban:settings" in content


def test_render_queued_card_lands_under_triage(tmp_path):
    """A queued node renders under the Triage column, not Now (ab-95a4a479)."""
    entries = [
        _entry("ab-eeeeeee1", title="QueuedCard", priority="p1",
               queued_at="2026-05-12T12:00:00Z"),
        _entry("ab-eeeeeee2", title="NowCard", priority="p1"),
    ]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    content = output.read_text()
    # Slice out the Triage section (up to the next "## " header) and assert the
    # queued card lives there while the genuine p1 stays in Now.
    triage_body = content.split("## Triage", 1)[1].split("\n## ", 1)[0]
    now_body = content.split("## Now", 1)[1].split("\n## ", 1)[0]
    assert "QueuedCard" in triage_body and "QueuedCard" not in now_body
    assert "NowCard" in now_body and "NowCard" not in triage_body


def test_ac2_ui_md_card_shows_project_label(tmp_path):
    """AC2-UI: each md card carries a `· <project>` label; unscoped is labeled."""
    entries = [
        _entry("ab-md000001", title="Scoped", project="web", priority="p1"),
        _entry("ab-md000002", title="Loose", project=None, priority="p1"),
    ]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    content = output.read_text()
    assert "· web" in content
    assert f"· {UNSCOPED_LABEL}" in content


def test_ac2_hp_md_clusters_cards_by_project(tmp_path):
    """AC2-HP: within a column, cards are grouped by project (contiguous)."""
    entries = [
        _entry("ab-md000010", title="W1", project="web", priority="p1"),
        _entry("ab-md000011", title="E1", project="etl", priority="p1"),
        _entry("ab-md000012", title="W2", project="web", priority="p1"),
    ]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    now_body = output.read_text().split("## Now", 1)[1].split("\n## ", 1)[0]
    # Both web cards appear on the same side of the etl card (contiguous run).
    iw1, iw2, ie1 = now_body.index("W1"), now_body.index("W2"), now_body.index("E1")
    assert (iw1 < iw2 < ie1) or (ie1 < iw1 < iw2)


def test_ac3_fr_md_headings_stay_clean(tmp_path):
    """AC3-FR: column headings stay exactly `## Now` (no count), so the
    Obsidian Kanban plugin keeps per-column state across re-renders."""
    entries = [
        _entry("ab-md000020", project="web", priority="p1"),
        _entry("ab-md000021", project="etl", priority="p1"),
    ]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    content = output.read_text()
    for col in ("Now", "Next", "Later", "Triage", "Done"):
        assert f"## {col}\n" in content        # heading line is bare
        assert f"## {col} " not in content      # no count/space-suffix on heading


def test_ac1_hp_render_done_cap_at_10(tmp_path):
    """AC1-HP: Done column shows at most 10 entries."""
    entries = [
        _entry(f"ab-{i:08x}", title=f"DoneEntry{i:02d}",
               status="done", completed_at=f"2026-01-{i:02d}T00:00:00Z")
        for i in range(1, 16)
    ]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    content = output.read_text()
    # Count unique "DoneEntryNN" occurrences -- should be 10, not 15
    done_count = sum(1 for i in range(1, 16) if f"DoneEntry{i:02d}" in content)
    assert done_count <= 10


# -- x-4845: live node-claim overlay --

def test_overlay_live_claim_routes_to_now():
    """AC: a node with a LIVE node:<id> claim and no session_id (so status is
    not 'claimed') routes to Now off the lockfile, not by bare priority."""
    e = _entry("x-aaaa", priority="p3")  # p3 would be Later without the overlay
    assert _kanban_column(e) == "Later"
    assert _kanban_column(e, frozenset(), frozenset({"x-aaaa"})) == "Now"


def test_overlay_absent_for_unclaimed_node():
    """AC: a node NOT in the live set rides its normal priority (overlay is
    additive; a STALE/absent claim contributes nothing since the set is built
    with include_stale=False upstream)."""
    e = _entry("x-bbbb", priority="p2")
    assert _kanban_column(e, frozenset(), frozenset({"x-other"})) == "Next"


def test_overlay_never_demotes_claimed():
    """AC: a node whose status is already 'claimed' stays in Now regardless of
    the overlay (additive, never demotes)."""
    e = _entry("x-cccc", session_id="s1", status="in_progress")
    assert _kanban_column(e, frozenset(), frozenset()) == "Now"
    assert _kanban_column(e, frozenset(), frozenset({"x-cccc"})) == "Now"


def test_overlay_does_not_resurrect_offboard():
    """Invariant: the overlay is additive to on-board lanes and never resurrects
    a deferred/superseded (off-board) node even if a claim leaks onto it."""
    for st in ("deferred", "superseded"):
        e = _entry("x-dddd", status=st, deferred_at="2026-01-01T00:00:00Z")
        assert _kanban_column(e, frozenset(), frozenset({"x-dddd"})) is None


def test_overlay_degrades_when_claims_unreadable(tmp_path, monkeypatch):
    """AC: claims subsystem unreadable -> the helper returns an empty overlay
    (it swallows faults), so render still succeeds with status-only placement."""
    monkeypatch.setattr("fno.graph.render.live_claimed_node_ids", lambda: set())
    entries = [_entry("x-eeee", priority="p2")]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    assert output.exists()


def test_render_md_places_live_claimed_in_now(tmp_path, monkeypatch):
    """End-to-end: render_graph_md consults live_claimed_node_ids and places the
    claimed node under the Now column even though its priority is p3."""
    monkeypatch.setattr("fno.graph.render.live_claimed_node_ids", lambda: {"x-ffff"})
    entries = [_entry("x-ffff", title="LiveNode", priority="p3")]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    content = output.read_text()
    now_section = content.split("## Next")[0]  # everything before Next col
    assert "LiveNode" in now_section
