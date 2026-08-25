"""Unit tests for fno.graph.render - kanban rendering."""
from __future__ import annotations

from fno.graph.render import (
    UNSCOPED_LABEL,
    _kanban_column,
    _kanban_card,
    _project_key,
    _rank_band,
    render_graph_md,
    _graph_sort_key,
    in_progress_epic_ids,
    make_kanban_column,
)
from fno.graph._intake import make_selection_sort_key


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


def _lane_key(entries, orphans=frozenset()):
    return make_selection_sort_key(entries, orphans, swimlane=True)


# -- _kanban_column --

def test_column_ready_p2_goes_next():
    """Intent mapping: ready+p2 (this-week-ish) lands in Next."""
    e = _entry("ab-11111111", status="ready", priority="p2")
    assert _kanban_column(e) == "Next"


def test_column_ready_p0_goes_now():
    """p0 (drop-everything) is today-ish - Now column regardless of session."""
    e = _entry("ab-11111112", status="ready", priority="p0")
    assert _kanban_column(e) == "Now"


def test_kanban_card_renders_artifact_url():
    """artifact_url has a purpose-built reader on the board card.

    The field had a writer (fno done --link) and no rendered reader. This pins
    the URL is consumed by the card render (a positive marker at a named
    consumer), not merely that the write landed.
    """
    e = _entry(
        "ab-art",
        status="done",
        completed_at="2026-01-01T00:00:00Z",
        artifact_url="https://figma.com/file/x",
    )
    card = _kanban_card(e, {e["id"]: e})
    assert "https://figma.com/file/x" in card
    assert "artifact:" in card


def test_column_ready_p1_goes_now():
    """p1 (next-up) is today-or-tomorrow-ish - Now column."""
    e = _entry("ab-11111113", status="ready", priority="p1")
    assert _kanban_column(e) == "Now"


def test_column_ready_p3_goes_later():
    """p3 (long-tail) lands in Later regardless of session state."""
    e = _entry("ab-11111114", status="ready", priority="p3")
    assert _kanban_column(e) == "Later"


def test_column_in_progress_has_distinct_lane():
    """Active work lands in In Progress even if priority would put it in Later."""
    e = _entry("ab-22222222", status="in_progress", priority="p3")
    assert _kanban_column(e) == "In Progress"


def test_column_queued_goes_triage():
    """A queued node (awaiting human ack) lands in Triage, not Now, regardless
    of priority - it must not inflate the Now lane (ab-95a4a479)."""
    e = _entry("ab-22222223", status="ready", priority="p3", queued_at="2026-05-12T12:00:00Z")
    assert _kanban_column(e) == "Triage"
    # priority is irrelevant once queued: a queued p1 still goes to Triage.
    e_p1 = _entry("ab-2222222a", status="ready", priority="p1", queued_at="2026-05-12T12:00:00Z")
    assert _kanban_column(e_p1) == "Triage"


def test_column_in_progress_beats_queued():
    """Active status wins over queued intent and keeps its distinct lane."""
    e = _entry(
        "ab-2222222b",
        status="in_progress",
        priority="p3",
        queued_at="2026-05-12T12:00:00Z",
    )
    assert _kanban_column(e) == "In Progress"


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


# -- in-progress epic -> In Progress --


def test_in_progress_epic_ids_detects_done_or_claimed_child():
    entries = [
        _entry("ab-epic0001", type="epic"),                      # in-progress (claimed child)
        _entry("ab-kid00001", status="in_progress", parent="ab-epic0001"),
        _entry("ab-epic0002", type="epic"),                      # in-progress (done child)
        _entry("ab-kid00002", completed_at="2026-01-01T00:00:00Z", parent="ab-epic0002"),
        _entry("ab-epic0003", type="epic"),                      # NOT in progress (ready child)
        _entry("ab-kid00003", parent="ab-epic0003"),
        _entry("ab-loose001"),                                   # not a parent at all
    ]
    ids = in_progress_epic_ids(entries)
    assert ids == frozenset({"ab-epic0001", "ab-epic0002"})


def test_live_claimed_child_does_not_promote_parent_epic_to_now():
    epic = _entry("ab-epic0004", type="epic", priority="p3")
    child = _entry("ab-kid00004", parent=epic["id"], priority="p3")

    column_for = make_kanban_column([epic, child], {child["id"]})

    assert column_for(epic) == "Later"
    assert column_for(child) == "Later"
    assert epic["status"] == "ready"


def test_in_progress_epic_signal_requires_a_live_epic_parent():
    feature_parent = _entry("feature-parent", type="feature", priority="p2")
    feature_child = _entry(
        "feature-child",
        parent="feature-parent",
        status="done",
        completed_at="2026-01-01T00:00:00Z",
    )
    dead_epic = _entry(
        "dead-epic", type="epic", status="superseded", priority="p2"
    )
    dead_child = _entry(
        "dead-child",
        parent="dead-epic",
        status="done",
        completed_at="2026-01-01T00:00:00Z",
    )
    entries = [feature_parent, feature_child, dead_epic, dead_child]

    assert in_progress_epic_ids(entries) == frozenset()
    column_for = make_kanban_column(entries)
    assert column_for(feature_parent) == "Next"
    assert column_for(dead_epic) is None


def test_column_in_progress_epic_has_distinct_lane():
    """A derived active epic has a distinct lane without mutating its status."""
    epic = _entry("ab-epic0001", priority="p3")  # would be Later by priority
    assert _kanban_column(epic, frozenset({"ab-epic0001"})) == "In Progress"
    # status was never mutated to a session-less "in_progress".
    assert epic["status"] == "ready"


def test_column_epic_not_in_progress_rides_priority():
    """A parent with no started children is NOT forced to Now - it rides its
    priority column like any other node (the promotion is in-progress only)."""
    epic = _entry("ab-epic0003", priority="p2")
    assert _kanban_column(epic, frozenset()) == "Next"


def test_column_overlays_ignore_unhashable_malformed_id():
    malformed = _entry("placeholder", priority="p2")
    malformed["id"] = []

    assert _kanban_column(
        malformed,
        frozenset({"active-epic"}),
        frozenset({"claimed-node"}),
    ) == "Next"


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
    entries = [web1, etl1, web2]
    ordered = sorted(entries, key=_lane_key(entries))
    projs = [_project_key(e) for e in ordered]
    # each project's cards are contiguous (no interleaving)
    assert projs == ["etl", "web", "web"]


def test_lane_sort_key_unscoped_lane_sorts_last():
    """AC2-UI: the (unscoped) lane orders after every named project lane."""
    named = _entry("ab-la000010", project="zeta")
    unscoped = _entry("ab-la000011", project=None)
    entries = [unscoped, named]
    ordered = sorted(entries, key=_lane_key(entries))
    assert [_project_key(e) for e in ordered] == ["zeta", UNSCOPED_LABEL]


def test_lane_sort_key_ranked_precedes_unranked_within_lane():
    """Invariant: a ranked card leads unranked cards in the same lane,
    even when the unranked card has higher priority."""
    ranked = _entry("ab-la000020", project="web", priority="p3", rank=0.0)
    unranked_hi = _entry("ab-la000021", project="web", priority="p0")
    entries = [unranked_hi, ranked]
    ordered = sorted(entries, key=_lane_key(entries))
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
    entries = [nan_card, ranked]
    ordered = sorted(entries, key=_lane_key(entries))  # must not raise
    # the genuinely-ranked card leads; the NaN card falls to the unranked flow
    assert ordered[0]["id"] == "ab-rb000011"


def test_lane_sort_key_ranked_orders_by_rank_ascending():
    a = _entry("ab-la000030", project="web", rank=2.0)
    b = _entry("ab-la000031", project="web", rank=1.0)
    entries = [a, b]
    ordered = sorted(entries, key=_lane_key(entries))
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
    for col in ("In Progress", "Now", "Next", "Later", "Triage", "Done"):
        assert f"## {col}" in content


def test_render_separates_active_ready_and_status_only_done(tmp_path):
    entries = [
        _entry("active", title="ActiveCard", status="in_progress", priority="p3"),
        _entry("ready", title="ReadyCard", status="ready", priority="p1"),
        _entry("done", title="DoneCard", status="done", priority="p1"),
    ]
    output = tmp_path / "graph.md"

    render_graph_md(entries, output)

    content = output.read_text()
    active_body = content.split("## In Progress", 1)[1].split("\n## ", 1)[0]
    now_body = content.split("## Now", 1)[1].split("\n## ", 1)[0]
    done_body = content.split("## Done", 1)[1].split("\n***", 1)[0]
    assert "ActiveCard" in active_body
    assert "ReadyCard" in now_body
    assert "- [x] **DoneCard**" in done_body
    assert "DoneCard" not in active_body
    assert "DoneCard" not in now_body


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


def test_md_board_uses_work_order_inside_project_lane(tmp_path):
    """An epic child leads a loose peer exactly as selection orders them."""
    entries = [
        _entry(
            "epic", title="Epic", type="epic", project="fno", priority="p1",
            created_at="2026-01-01T00:00:00Z",
        ),
        _entry(
            "child", title="Child", parent="epic", project="fno", priority="p1",
            created_at="2026-03-01T00:00:00Z",
        ),
        _entry(
            "loose", title="Loose", project="fno", priority="p1", orphan_ok="infra",
            created_at="2026-01-01T00:00:00Z",
        ),
    ]
    output = tmp_path / "graph.md"

    render_graph_md(entries, output)

    now_body = output.read_text().split("## Now", 1)[1].split("\n## ", 1)[0]
    assert now_body.index("Child") < now_body.index("Loose")


def test_live_epic_priority_promotes_child_column_without_mutation(tmp_path):
    epic = _entry("epic", type="epic", priority="p1", status="ready")
    child = _entry("child", parent="epic", priority="p2", status="ready")
    output = tmp_path / "graph.md"

    render_graph_md([epic, child], output)

    content = output.read_text()
    now_body = content.split("## Now", 1)[1].split("\n## ", 1)[0]
    assert "`child`" in now_body
    assert child["priority"] == "p2"


def test_terminal_epic_priority_does_not_promote_child_column(tmp_path):
    epic = _entry("epic", type="epic", priority="p1", status="superseded")
    child = _entry("child", parent="epic", priority="p2", status="ready")
    output = tmp_path / "graph.md"

    render_graph_md([epic, child], output)

    content = output.read_text()
    now_body = content.split("## Now", 1)[1].split("\n## ", 1)[0]
    next_body = content.split("## Next", 1)[1].split("\n## ", 1)[0]
    assert "`child`" not in now_body
    assert "`child`" in next_body


def test_render_skips_non_dict_rows_without_aborting_write(tmp_path):
    output = tmp_path / "graph.md"
    render_graph_md([None, "poison", _entry("healthy", priority="p1")], output)
    assert "`healthy`" in output.read_text()


def test_render_degrades_malformed_epic_enrichment_without_aborting(tmp_path):
    epic = _entry("epic", type="epic", status=[], priority=[], created_at=[])
    child = _entry("child", parent="epic", priority=[], created_at=[])
    output = tmp_path / "graph.md"

    render_graph_md([epic, child], output)

    assert "child" in output.read_text()


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
    for col in ("In Progress", "Now", "Next", "Later", "Triage", "Done"):
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

def test_overlay_live_claim_does_not_route_to_now():
    """AC6: a live claim without an open do row does not invent display state."""
    e = _entry("x-aaaa", priority="p3")  # p3 would be Later without the overlay
    assert _kanban_column(e) == "Later"
    assert _kanban_column(e, frozenset(), frozenset({"x-aaaa"})) == "Later"


def test_overlay_absent_for_unclaimed_node():
    """AC: a node NOT in the live set rides its normal priority (overlay is
    additive; a STALE/absent claim contributes nothing since the set is built
    with include_stale=False upstream)."""
    e = _entry("x-bbbb", priority="p2")
    assert _kanban_column(e, frozenset(), frozenset({"x-other"})) == "Next"


def test_overlay_never_demotes_in_progress():
    """An active node keeps its distinct lane regardless of claim overlay."""
    e = _entry("x-cccc", session_id="s1", status="in_progress")
    assert _kanban_column(e, frozenset(), frozenset()) == "In Progress"
    assert _kanban_column(e, frozenset(), frozenset({"x-cccc"})) == "In Progress"


def test_overlay_does_not_resurrect_offboard():
    """Invariant: the overlay is additive to on-board lanes and never resurrects
    a deferred/superseded (off-board) node even if a claim leaks onto it."""
    for st in ("deferred", "superseded"):
        e = _entry("x-dddd", status=st, deferred_at="2026-01-01T00:00:00Z")
        assert _kanban_column(e, frozenset(), frozenset({"x-dddd"})) is None


def test_render_does_not_need_claims_for_status_placement(tmp_path):
    """Markdown rendering succeeds from stored status without claim reads."""
    entries = [_entry("x-eeee", priority="p2")]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    assert output.exists()


def test_render_md_does_not_place_live_claimed_in_now(tmp_path):
    """AC6: markdown rendering consumes stored status, not claim liveness."""
    entries = [_entry("x-ffff", title="LiveNode", priority="p3")]
    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    content = output.read_text()
    later_section = content.split("## Later")[1].split("## Triage")[0]
    assert "LiveNode" in later_section
