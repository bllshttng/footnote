"""Unit tests for the shadow-first bounded scheduler (x-24f7, Change 1).

Covers `advance.schedule_shadow` and the shared `_classify_lane_candidate`: the
read-only frontier decision report that emits selected / serialized / unevaluated
verdicts with stable typed reasons and performs NO dispatch. The plan's ready-set
shapes are each a test: empty, singleton, independent, file-colliding,
same-domain (disjoint surfaces co-select; overlap serializes on
collision), unknown-liveness (peer-lane), unevaluated (no file surface), and
oversized (cap-bounded). `_ready_nodes` and the collision/in-flight seams are
monkeypatched so the logic is tested without shelling `fno backlog ready`; the
claims root is isolated to `tmp_path`.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.backlog import advance
from fno.claims.lanes import acquire_lane_slot, find_lane_slot, release_lane_slot


def _nodes(*specs):
    """Build ready-node summaries from (id, domain) pairs, each with a surface."""
    return [
        {"id": i, "domain": d, "title": i, "slug": i, "plan_path": f"/plans/{i}.md"}
        for i, d in specs
    ]


class _Hit:
    """Minimal stand-in for a collision hit (only with_node_id is read)."""

    def __init__(self, with_node_id):
        self.with_node_id = with_node_id


@pytest.fixture(autouse=True)
def _hermetic(monkeypatch):
    """Default seams: every node has a real file surface, nothing collides, and
    no live peer lanes or in-flight work. Each test overrides what it exercises.
    """
    import fno.graph.collision as collision

    monkeypatch.setattr(collision, "has_file_surface", lambda p: True)
    monkeypatch.setattr(collision, "resolve_plan_path", lambda p: p)
    # Process-global memo: another test in the same session can leave it in the
    # degraded (cwd-fallback) state, which would then show up in every report
    # here as a `plan-path-resolution` degrade. Pin it healthy; the two tests
    # that care override it.
    monkeypatch.setattr(collision, "_repo_root_cache", (True, Path("/repo")))
    monkeypatch.setattr(advance, "_high_collision", lambda node, inflight: None)
    monkeypatch.setattr(advance, "_live_lane_domains", lambda claims_root=None: set())
    monkeypatch.setattr(advance, "_live_worked_entries", lambda claims_root=None: [])


def _ready(monkeypatch, ready):
    monkeypatch.setattr(
        advance, "_ready_nodes", lambda project=None, mission=None: list(ready)
    )


def test_empty_ready_set(monkeypatch, tmp_path):
    _ready(monkeypatch, [])
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert r["selected"] == [] and r["serialized"] == [] and r["unevaluated"] == []
    assert r["decisions"] == []
    assert r["effective_cap"] == 2


def test_singleton_selected(monkeypatch, tmp_path):
    _ready(monkeypatch, _nodes(("n-a", "code")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert [d["id"] for d in r["selected"]] == ["n-a"]
    assert r["selected"][0]["verdict"] == "selected"
    assert r["selected"][0]["reason"] == ""


def test_independent_distinct_domains_up_to_cap(monkeypatch, tmp_path):
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "docs"), ("n-c", "infra")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert [d["id"] for d in r["selected"]] == ["n-a", "n-b"]
    # third distinct-domain node is otherwise-selectable but past the cap
    assert [(d["id"], d["reason"]) for d in r["serialized"]] == [("n-c", "cap-full")]


def test_same_domain_disjoint_surfaces_both_selected(monkeypatch, tmp_path):
    """Domain no longer serializes - two `code` nodes with (hermetic)
    clean surfaces fill the frontier together."""
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "code")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert [d["id"] for d in r["selected"]] == ["n-a", "n-b"]
    assert r["serialized"] == []


def test_same_domain_overlap_serializes_on_collision(monkeypatch, tmp_path):
    """The shadow twin of the live AC2-ERR - same-domain nodes sharing
    a high-severity overlap serialize with `high-collision:<id>`, a collision
    verdict, never a domain one."""
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "code")))
    monkeypatch.setattr(
        advance, "_high_collision",
        lambda node, inflight: _Hit("n-a") if node["id"] == "n-b" else None,
    )
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert [d["id"] for d in r["selected"]] == ["n-a"]
    assert [(d["id"], d["reason"]) for d in r["serialized"]] == [
        ("n-b", "high-collision:n-a")
    ]


def test_no_surface_in_a_held_domain_carries_the_same_domain_annotation(
    monkeypatch, tmp_path
):
    """The domain tiebreak survives only as an annotation on an
    unevaluated candidate, so the report can still explain the serialized
    unknown: `unevaluated:no-surface+same-domain:<domain>`."""
    import fno.graph.collision as collision

    monkeypatch.setattr(
        collision, "has_file_surface", lambda p: p != "/plans/n-b.md"
    )
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "code")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert [d["id"] for d in r["selected"]] == ["n-a"]
    assert [(d["id"], d["reason"]) for d in r["unevaluated"]] == [
        ("n-b", "unevaluated:no-surface+same-domain:code")
    ]


def test_file_collision_is_serialized(monkeypatch, tmp_path):
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "docs")))
    # n-b collides with in-flight n-a once n-a is selected.
    monkeypatch.setattr(
        advance, "_high_collision",
        lambda node, inflight: _Hit("n-a") if node["id"] == "n-b" else None,
    )
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert [d["id"] for d in r["selected"]] == ["n-a"]
    assert [(d["id"], d["reason"]) for d in r["serialized"]] == [
        ("n-b", "high-collision:n-a")
    ]


def test_raising_collision_gate_is_unevaluated_not_selected(monkeypatch, tmp_path):
    """A collision gate that THREW must not report as a clean comparison.

    This is the frontier's most dangerous silent degrade: a swallowed error one
    frame down returns the same None a genuine non-overlap does, so the node
    lands `selected` with an empty reason and nothing marks the report degraded.
    An operator gating live scheduling then co-schedules two nodes whose file
    overlap was never actually compared.
    """
    def boom(node, inflight):
        raise RuntimeError("collision index unreadable")

    monkeypatch.setattr(advance, "_high_collision", boom)
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "docs")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert r["selected"] == [], "a node whose gate threw must not read as clean"
    assert [(d["id"], d["reason"]) for d in r["unevaluated"]] == [
        ("n-a", "unevaluated:collision-error"),
        ("n-b", "unevaluated:collision-error"),
    ]


def test_unreadable_plan_path_does_not_escape_the_gate(monkeypatch, tmp_path):
    """The surface probe is inside the guard, not hoisted above it.

    `resolve_plan_path` reaches `Path.cwd()`, which raises when the cwd has been
    deleted - a live scenario in a worktree-archiving system. Left outside the
    fail-open guard it propagates through `select_lane_fill`, which re-raises
    after releasing slots, wedging the live dispatcher the guard exists to protect.
    """
    import fno.graph.collision as collision

    def boom(p):
        raise FileNotFoundError("cwd has been archived")

    monkeypatch.setattr(collision, "resolve_plan_path", boom)
    _ready(monkeypatch, _nodes(("n-a", "code")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert [(d["id"], d["reason"]) for d in r["unevaluated"]] == [
        ("n-a", "unevaluated:collision-error")
    ]


def test_no_file_surface_is_unevaluated(monkeypatch, tmp_path):
    import fno.graph.collision as collision

    # n-b states no comparable file surface -> its safety is unknown, so the
    # conservative shadow report serializes it as unevaluated (not selected).
    monkeypatch.setattr(
        collision, "has_file_surface", lambda p: p != "/plans/n-b.md"
    )
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "docs")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert [d["id"] for d in r["selected"]] == ["n-a"]
    assert [(d["id"], d["reason"]) for d in r["unevaluated"]] == [
        ("n-b", "unevaluated:no-surface")
    ]
    assert r["serialized"] == []


def test_missing_plan_path_is_unevaluated(monkeypatch, tmp_path):
    ready = [{"id": "n-a", "domain": "code", "title": "n-a", "slug": "n-a"}]
    _ready(monkeypatch, ready)
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert [(d["id"], d["reason"]) for d in r["unevaluated"]] == [
        ("n-a", "unevaluated:no-surface")
    ]


def test_live_peer_lane_is_serialized(monkeypatch, tmp_path):
    # A live peer lane already holds n-a -> it must not be re-selected.
    acquire_lane_slot(max_lanes=3, lane_id="n-a", root=tmp_path)
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "docs")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert [d["id"] for d in r["selected"]] == ["n-b"]
    assert [(d["id"], d["reason"]) for d in r["serialized"]] == [("n-a", "peer-lane")]


def test_live_slot_reduces_remaining_capacity(monkeypatch, tmp_path):
    """codex P1 (#631): a live lane occupies a slot even on a DISTINCT domain, so
    a cap-two report with one lane live can start only ONE more node. The frontier
    must not overstate what select_lane_fill(2) could actually acquire.
    """
    # A live lane holds a slot on a domain NOT present in the ready set, so the
    # constraint is the SLOT count, not distinct-domain (which _hermetic zeroes).
    acquire_lane_slot(max_lanes=3, lane_id="live-x", root=tmp_path)
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "docs")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert r["occupied_slots"] == 1
    assert r["remaining_capacity"] == 1
    assert [d["id"] for d in r["selected"]] == ["n-a"]
    assert [(d["id"], d["reason"]) for d in r["serialized"]] == [("n-b", "cap-full")]


def test_all_slots_occupied_selects_nothing(monkeypatch, tmp_path):
    """Two live lanes fill the cap-two frontier: nothing more can start."""
    acquire_lane_slot(max_lanes=2, lane_id="live-x", root=tmp_path)
    acquire_lane_slot(max_lanes=2, lane_id="live-y", root=tmp_path)
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "docs")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert r["remaining_capacity"] == 0
    assert r["selected"] == []
    assert [d["reason"] for d in r["serialized"]] == ["cap-full", "cap-full"]


def test_lane_above_the_cap_still_counts_against_the_writers_ceiling(monkeypatch, tmp_path):
    """effective_cap bounds live WRITERS, not acquire's scan window.

    This is the normal steady state, not a cap-shrink edge case: live dispatch
    acquires with the raw configured max_lanes (3 in this repo) while the report
    bounds itself to _INITIAL_LIVE_CAP (2), so a live lane routinely sits at
    lane-slot:2. Counting only the slots acquire_lane_slot(2) would contend for
    scores that lane as zero and lets a cap-two report authorize two more starts
    beside it - three writers under a ceiling of two. That acquire_lane_slot(2)
    would in fact grant the third is a shrink bug in the cap primitive; the
    report must not launder it into the evidence that gates live scheduling.
    """
    for lane in ("live-a", "live-b", "live-c"):
        acquire_lane_slot(max_lanes=3, lane_id=lane, root=tmp_path)
    release_lane_slot("live-a", root=tmp_path)
    release_lane_slot("live-b", root=tmp_path)
    assert find_lane_slot("live-c", root=tmp_path) == "lane-slot:2"

    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "docs"), ("n-c", "infra")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert r["occupied_slots"] == 1
    assert r["remaining_capacity"] == 1
    assert [d["id"] for d in r["selected"]] == ["n-a"]
    # The invariant that matters: acting on this report never exceeds the cap.
    assert r["occupied_slots"] + len(r["selected"]) <= r["effective_cap"]
    assert [(d["id"], d["reason"]) for d in r["serialized"]] == [
        ("n-b", "cap-full"), ("n-c", "cap-full"),
    ]


def test_oversized_ready_set_bounded_by_effective_cap(monkeypatch, tmp_path):
    _ready(monkeypatch, _nodes(
        ("n-a", "code"), ("n-b", "docs"), ("n-c", "infra"),
        ("n-d", "ml"), ("n-e", "ops"),
    ))
    r = advance.schedule_shadow(99, claims_root=tmp_path)
    assert r["effective_cap"] == 2  # hard-limited to the initial-rollout ceiling
    assert r["requested_cap"] == 99
    assert len(r["selected"]) == 2
    assert all(d["reason"] == "cap-full" for d in r["serialized"])


def test_cap_below_one_normalizes_to_one(monkeypatch, tmp_path):
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "docs")))
    r = advance.schedule_shadow(0, claims_root=tmp_path)
    assert r["effective_cap"] == 1
    assert [d["id"] for d in r["selected"]] == ["n-a"]
    assert [(d["id"], d["reason"]) for d in r["serialized"]] == [("n-b", "cap-full")]


def test_ready_list_unreadable_fails_safe(monkeypatch, tmp_path):
    _ready(monkeypatch, _nodes(("n-a", "code")))
    healthy = advance.schedule_shadow(2, claims_root=tmp_path)

    def boom(project=None, mission=None):
        raise RuntimeError("garbled backlog ready")

    monkeypatch.setattr(advance, "_ready_nodes", boom)
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert r["note"] == "ready-unreadable"
    assert r["degraded"] == ["ready"]
    assert r["selected"] == [] and r["decisions"] == []
    # Fail-closed capacity: this report authorizes no dispatch.
    assert r["occupied_slots"] == 0 and r["remaining_capacity"] == 0
    # Key-set parity with the healthy return, `note` aside (present only here).
    # Asserted structurally so a field added to one path and forgotten on the
    # other fails HERE, rather than as a KeyError in a consumer that reached the
    # degraded path - the one path where it most needs a number, not a crash.
    assert set(r) - {"note"} == set(healthy)


def test_healthy_run_reports_no_degradation(monkeypatch, tmp_path):
    _ready(monkeypatch, _nodes(("n-a", "code")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert r["degraded"] == []


def test_occupied_slot_read_failure_is_loud_not_silent(monkeypatch, tmp_path, caplog):
    """The capacity guard must not silently collapse: if the slot count raises,
    occupied fails open to 0 (frontier may overstate), but the degrade is logged
    AND surfaced in `degraded` so an operator gating on the JSON sees it.
    """
    import fno.claims.lanes as lanes

    def boom(*a, **k):
        raise RuntimeError("claims dir locked")

    monkeypatch.setattr(lanes, "active_lane_count", boom)
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "docs")))
    with caplog.at_level("WARNING"):
        r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert "occupied-slots" in r["degraded"]
    assert r["occupied_slots"] == 0
    assert "remaining_capacity" in caplog.text.lower() or "slot count" in caplog.text.lower()


def test_live_seed_read_failures_are_recorded(monkeypatch, tmp_path):
    """Both live-claim seed reads (domains + in-flight) fail open visibly."""
    def boom(*a, **k):
        raise RuntimeError("read fault")

    monkeypatch.setattr(advance, "_live_lane_domains", boom)
    monkeypatch.setattr(advance, "_live_worked_entries", boom)
    _ready(monkeypatch, _nodes(("n-a", "code")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert "live-lane-domains" in r["degraded"]
    assert "inflight" in r["degraded"]


def test_decisions_cover_every_ready_node(monkeypatch, tmp_path):
    ready = _nodes(("n-a", "code"), ("n-b", "code"), ("n-c", "docs"))
    _ready(monkeypatch, ready)
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    # every ready node appears exactly once across the three buckets
    assert len(r["decisions"]) == 3
    ids = {d["id"] for d in r["decisions"]}
    assert ids == {"n-a", "n-b", "n-c"}


# -- CLI seam: `fno backlog schedule` --

def test_duplicate_ready_ids_are_decided_once(monkeypatch, tmp_path):
    """A ready list repeating an id must not yield two decisions for it.

    `fno backlog ready` is an external command; a duplicate entry is a garbled
    input this report should absorb, not a reason to double-count a node against
    the cap or emit two verdicts for one id.
    """
    dup = _nodes(("n-a", "code"))[0]
    _ready(monkeypatch, [dup, dict(dup), *_nodes(("n-b", "docs"))])
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert [d["id"] for d in r["decisions"]] == ["n-a", "n-b"]
    assert [d["id"] for d in r["selected"]] == ["n-a", "n-b"]


def test_a_pick_becomes_a_collision_comparator_for_later_picks(monkeypatch, tmp_path):
    """Each selection joins the in-flight set the NEXT candidate is compared to.

    Without that accumulation the report happily co-schedules two nodes that
    overlap each other, since neither was live when the frontier started. The
    live selector does the same thing in its own loop; this is the shadow copy
    of that accumulation, so it needs its own test.
    """
    seen_comparators = []

    def collide_against_inflight(node, inflight):
        seen_comparators.append((node["id"], [e["id"] for e in inflight]))
        return _Hit(inflight[0]["id"]) if inflight else None

    monkeypatch.setattr(advance, "_high_collision", collide_against_inflight)
    _ready(monkeypatch, _nodes(("n-a", "code"), ("n-b", "docs")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)

    # n-a is compared against an empty set; n-b is compared against n-a.
    assert seen_comparators == [("n-a", []), ("n-b", ["n-a"])]
    assert [d["id"] for d in r["selected"]] == ["n-a"]
    assert [(d["id"], d["reason"]) for d in r["serialized"]] == [
        ("n-b", "high-collision:n-a")
    ]


def test_degraded_plan_path_resolution_reaches_the_report(monkeypatch, tmp_path):
    """A cwd fallback in repo-root resolution makes collisions false-negative.

    That overstates the frontier in the same direction a swallowed seed read
    would, so it has to land in `degraded` rather than dying on stderr.
    """
    import fno.graph.collision as collision

    monkeypatch.setattr(collision, "_repo_root_cache", (False, tmp_path))
    _ready(monkeypatch, _nodes(("n-a", "code")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert "plan-path-resolution" in r["degraded"]


def test_healthy_plan_path_resolution_is_not_flagged(monkeypatch, tmp_path):
    import fno.graph.collision as collision

    monkeypatch.setattr(collision, "_repo_root_cache", (True, tmp_path))
    _ready(monkeypatch, _nodes(("n-a", "code")))
    r = advance.schedule_shadow(2, claims_root=tmp_path)
    assert r["degraded"] == []
