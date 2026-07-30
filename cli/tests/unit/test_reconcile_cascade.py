"""A merged PR closes every node it delivered (x-e957 tasks 1.5a + 1.5).

`plan == PR == node` holds for the DELIVERY UNIT. A node folded into a group
child by `decompose ... adopt:` carries `contained_in` and ships inside that
unit's PR - it has no PR of its own, so nothing ever closes it. Dispatch and
cost each learned to read the containment fact; completion had no inference at
all, and a contained node stayed open forever with a null cost that read as
"never measured" rather than "measured on the unit".

The truth arrives from GitHub, so the cascade lives in reconcile and not in
`fno backlog done`: an operator typing a close is not evidence a PR merged.

Lives beside test_reconcile_dispatch.py rather than in a general
test_reconcile.py, matching how reconcile tests are already split by concern.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner


UNIT = "x-6320"
KID_A = "x-261c"
KID_B = "x-3f8d"
DEP = "x-9b7e"
PLAN = "/p/containment.md"


# -- fixtures --


def _node(node_id: str, **overrides) -> dict:
    base = {
        "id": node_id,
        "title": f"node {node_id}",
        "type": "feature",
        "project": "fno",
        "cwd": None,
        "parent": None,
        "blocked_by": [],
        "status": "ready",
        "completed_at": None,
        "plan_path": None,
        "cost_usd": None,
        "cost_sessions": [],
        "created_at": "2026-07-01T00:00:00+00:00",
    }
    base.update(overrides)
    return base


def _world(tmp_path: Path, *, dependent_blocked_on=KID_A) -> list[dict]:
    """A delivery unit with two contained children, plus a dependent.

    The dependent blocks on a CONTAINED node - the mis-modeled edge the epic
    says should not exist. Whether it dispatches is the one thing nobody had
    measured, so the fixture makes it observable rather than assumed.
    """
    return [
        _node(UNIT, plan_path=PLAN, pr_number=700,
              pr_url="https://github.com/o/r/pull/700"),
        _node(KID_A, plan_path=PLAN, parent=UNIT, contained_in=UNIT),
        _node(KID_B, plan_path=PLAN, parent=UNIT, contained_in=UNIT),
        _node(DEP, blocked_by=[dependent_blocked_on]),
    ]


@pytest.fixture
def world(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Graph + ledger wired into the CLI; returns (write_entries, read_entries)."""
    import fno.graph._constants as gc
    import fno.graph.store as gs

    graph_path = tmp_path / "graph.json"
    ledger_path = tmp_path / "ledger.json"
    ledger_path.write_text(json.dumps({"entries": [{
        "plan_path": PLAN,
        "cost_usd": 18.22,
        "points": 5,
        "fno_id": "R1",
        "sessions": ["R1"],
        "completed": "2026-07-28T09:00:00",
    }]}) + "\n")

    monkeypatch.setattr(gc, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger_path)
    monkeypatch.setattr(gs, "GRAPH_JSON", graph_path)
    monkeypatch.setattr("fno.paths.retro_pending_dir", lambda: tmp_path / "retro")

    def write(entries: list[dict]) -> None:
        graph_path.write_text(json.dumps({"entries": entries}) + "\n")

    def read() -> dict:
        raw = json.loads(graph_path.read_text())["entries"]
        return {e["id"]: e for e in raw}

    write(_world(tmp_path))
    return write, read


@pytest.fixture
def merged_pr(monkeypatch: pytest.MonkeyPatch):
    """`gh` says the unit's PR #700 merged; nothing else has a PR at all.

    Stubs the scan rather than gh itself: what merged is the INPUT to this
    behavior, and resolving it is already covered by the drift-scan tests.
    """
    import fno.graph._reconcile as rec

    def _scan(entries, node_id=None):
        return [rec.MergeDriftRecord(
            node_id=UNIT,
            plan_path=PLAN,
            pr_number=700,
            pr_url="https://github.com/o/r/pull/700",
            pr_state="MERGED",
            merged_at="2026-07-29T00:00:00Z",
        )]

    monkeypatch.setattr(rec, "scan_merge_drift", _scan)


@pytest.fixture
def dispatches(monkeypatch: pytest.MonkeyPatch) -> list[tuple[str, str]]:
    """Capture every auto-continue call the close triggers, per closed node."""
    import fno.backlog.advance as adv
    import fno.backlog.reconcile_dispatch as rd

    calls: list[tuple[str, str]] = []
    monkeypatch.setattr(
        adv, "advance",
        lambda closed_node_id=None, **k: calls.append(("advance", closed_node_id)))
    monkeypatch.setattr(
        adv, "advance_dependents",
        lambda closed_node_id=None, **k: calls.append(("dependents", closed_node_id)))
    monkeypatch.setattr(
        rd, "dispatch_reconcile_for_blocker",
        lambda closed_node_id=None, **k: calls.append(("destub", closed_node_id)))
    return calls


def _reconcile(*args):
    from fno.graph.cli import cli

    return CliRunner().invoke(cli, ["reconcile", *args])


# -- 1.5a: what a dependent of a contained node does, measured not assumed --


def test_1_5a_observed_a_contained_node_has_no_pr_to_close_it(world, merged_pr,
                                                              dispatches):
    """1.5a OBSERVATION, recorded before 1.5 changed anything.

    Driving reconcile's merged-PR path against today's `_reconcile.py` and
    `_direct_dependents`, with the unit's PR #700 merged:

      - the delivery unit closes;
      - BOTH contained children stay open, because `scan_merge_drift` only ever
        returns nodes carrying a PR and a contained node carries none. There is
        no cascade for them to miss - completion simply has no inference;
      - so the dependent's blocker never completes and it is not dispatched.
        It is not stranded by a dispatch bug, it is stranded by the child never
        closing at all.

    The epic's position - that a contained node should not be a legal
    `blocked_by` target - was inference. What is measured here is narrower and
    enough to decide 1.5: the dependent is stranded TODAY, so any dispatch
    choice 1.5 makes is an improvement over the status quo rather than a
    regression from it.

    This test now pins the SHAPE of the pre-change world with the cascade
    disabled, so it keeps describing what 1.5 fixed instead of rotting.
    """
    import fno.graph.cli as gcli

    # Disable only the new cascade; everything else is the pre-change path.
    with pytest.MonkeyPatch.context() as mp:
        mp.setattr(gcli, "_cascade_close_contained", lambda entries, node_id: [])
        assert _reconcile().exit_code == 0

    _write, read = world
    nodes = read()
    assert nodes[UNIT]["completed_at"] is not None
    assert nodes[KID_A]["completed_at"] is None
    assert nodes[KID_B]["completed_at"] is None
    # The dependent's only blocker is still open, so it never became eligible.
    assert nodes[DEP]["completed_at"] is None
    assert [nid for _kind, nid in dispatches] == [UNIT, UNIT, UNIT]


def test_1_5_pins_the_dependent_dispatch_choice_explicitly(world, merged_pr,
                                                           dispatches):
    """AC6: the choice, asserted - a cascade-closed child dispatches NOTHING.

    A contained node is not a delivery unit, so it is not a legal `blocked_by`
    target either; a dependent pointing at one is a mis-modeled edge that should
    point at the unit. Running auto-continue per contained node would also fan
    one merge out into N dispatches with no bound - a storm keyed on how finely
    an epic happened to be decomposed.

    The dependent is not stranded by this. Its blocker now carries
    `completed_at`, so status recompute derives it eligible and the next
    autonomous selection picks it up. What it loses is the INSTANT dispatch,
    which is the bounded, reversible half of the choice.

    If this ever needs to flip, the cascade has to call advance per contained
    node AND the storm needs its own bound - a finding to surface, not a thing
    to grow silently out of a test edit.
    """
    assert _reconcile().exit_code == 0

    _write, read = world
    assert read()[KID_A]["completed_at"] is not None
    # Every dispatch this sweep is attributed to the delivery unit, not a child.
    assert {nid for _kind, nid in dispatches} == {UNIT}


def test_dependent_of_a_cascade_closed_child_becomes_eligible(world, merged_pr,
                                                              dispatches):
    """The other half of AC6: not-dispatched must not mean not-unblocked.

    `blocked` is derived from whether the blockers carry `completed_at`, so the
    cascade close is what re-arms the dependent for the next selection pass.
    Without this the choice above would be quietly indistinguishable from
    stranding it.
    """
    assert _reconcile().exit_code == 0

    _write, read = world
    nodes = read()
    assert nodes[KID_A]["completed_at"] is not None
    assert nodes[DEP]["status"] != "blocked"


# -- AC5: all three close, one carries the cost, the others say where it went --


def test_ac5_merged_pr_closes_the_unit_and_both_contained_nodes(world, merged_pr,
                                                                dispatches):
    assert _reconcile().exit_code == 0

    _write, read = world
    nodes = read()
    for nid in (UNIT, KID_A, KID_B):
        assert nodes[nid]["completed_at"] is not None, nid
        assert nodes[nid]["status"] == "done", nid


def test_ac5_the_delivery_unit_carries_the_whole_cost(world, merged_pr, dispatches):
    """One plan, one ledger row, one node that owns the figure."""
    assert _reconcile().exit_code == 0

    nodes = world[1]()
    assert nodes[UNIT]["cost_usd"] == pytest.approx(18.22)
    assert nodes[UNIT]["points"] == 5


def test_ac5_contained_nodes_carry_null_cost_and_say_where_it_went(world, merged_pr,
                                                                   dispatches):
    """The note is what makes the null readable as located rather than missing.

    All three nodes share one plan_path, so a rollup keyed on the plan would
    hand each of them the same $18.22 and the flat project sum would report
    $54.66 for one run - the 2026-07-28 failure, exactly.
    """
    assert _reconcile().exit_code == 0

    nodes = world[1]()
    for nid in (KID_A, KID_B):
        assert nodes[nid]["cost_usd"] is None, nid
        assert nodes[nid]["cost_sessions"] == [], nid
        assert nodes[nid]["session_id"] is None, nid
        note = nodes[nid]["completion_note"] or ""
        assert UNIT in note, note
        assert "700" in note, note


def test_ac9_project_total_counts_the_run_once(world, merged_pr, dispatches):
    """AC9 through the real close path, not just the rollup helper.

    The project total is a flat `sum(cost_usd)` and stays one: contained nodes
    contributing nothing is what keeps it correct without teaching it to dedup.
    """
    assert _reconcile().exit_code == 0

    nodes = world[1]()
    total = sum(e.get("cost_usd", 0) or 0 for e in nodes.values())
    assert total == pytest.approx(18.22)


# -- the cascade is a warning, never an abort --


def test_cascade_never_aborts_the_close_it_rides_on(world, merged_pr, dispatches,
                                                    monkeypatch, capsys):
    """The merge already happened; the unit's own close is load-bearing.

    A cascade that raised would leave the delivery unit open against a merged
    PR - strictly worse than the bug it fixes.
    """
    import fno.graph.cli as gcli

    def boom(entries, node_id):
        raise RuntimeError("cascade exploded")

    monkeypatch.setattr(gcli, "_cascade_close_contained", boom)
    assert _reconcile().exit_code == 0

    nodes = world[1]()
    assert nodes[UNIT]["completed_at"] is not None
    assert nodes[KID_A]["completed_at"] is None


def test_cascade_is_idempotent_over_an_already_closed_child(world, merged_pr,
                                                            dispatches):
    """A child closed out of band keeps its own completion, note and all.

    Overwriting it would relabel a node that shipped its own PR as contained
    cargo, and reconcile runs on every SessionStart.
    """
    write, read = world
    entries = _world(Path("/tmp"))
    for e in entries:
        if e["id"] == KID_A:
            e["completed_at"] = "2026-07-01T00:00:00+00:00"
            e["completion_note"] = "closed by hand"
            e["cost_usd"] = 2.0
    write(entries)

    assert _reconcile().exit_code == 0
    nodes = read()
    assert nodes[KID_A]["completed_at"] == "2026-07-01T00:00:00+00:00"
    assert nodes[KID_A]["completion_note"] == "closed by hand"
    assert nodes[KID_A]["cost_usd"] == 2.0
    # The sibling that was genuinely open still closed.
    assert nodes[KID_B]["completed_at"] is not None


def test_no_contained_nodes_leaves_the_close_path_unchanged(world, merged_pr,
                                                             dispatches, tmp_path):
    """The overwhelmingly common shape: nothing is contained in anything."""
    write, read = world
    write([e for e in _world(tmp_path) if e["id"] not in (KID_A, KID_B)])

    assert _reconcile().exit_code == 0
    nodes = read()
    assert nodes[UNIT]["completed_at"] is not None
    assert nodes[UNIT]["cost_usd"] == pytest.approx(18.22)
    assert [nid for _kind, nid in dispatches] == [UNIT, UNIT, UNIT]


# -- the helper's own invariants --


def _cascade(entries, node_id):
    from fno.graph.cli import _cascade_close_contained

    return _cascade_close_contained(entries, node_id)


def test_every_closed_node_carries_the_note_that_explains_it():
    """Regression: a NameError mid-loop left a node done with no note.

    The caller downgrades a raised cascade to a warning and keeps the delivery
    unit's close, so anything fallible between `_apply_completion_fields` and
    the note assignment ships nodes that are done for no stated reason - and
    their null cost then reads as missing again, which is the whole failure this
    field was added to end. Pinned as a property over the returned ids rather
    than as one example.
    """
    entries = _world(Path("/tmp"))
    closed = _cascade(entries, UNIT)
    assert set(closed) == {KID_A, KID_B}
    by_id = {e["id"]: e for e in entries}
    for nid in closed:
        assert by_id[nid]["completed_at"] is not None, nid
        assert UNIT in (by_id[nid]["completion_note"] or ""), nid


def test_contained_close_leaves_merge_status_unset():
    """`merge_status` means "GitHub confirmed THIS node's PR merged".

    A contained node has no PR, so asserting a merge on it would be a plain
    lie - the same reason the PR-less epic cascade leaves the field alone.
    """
    entries = _world(Path("/tmp"))
    _cascade(entries, UNIT)
    by_id = {e["id"]: e for e in entries}
    assert by_id[KID_A].get("merge_status") is None


def test_containment_is_one_level_not_a_chain():
    """Closing the unit does not reach a node contained in a contained node.

    decompose cannot produce that shape - `contained_in` is a direct relation to
    the node that owns the PR. Walking it like the parent chain would invent a
    transitive close nothing writes.
    """
    entries = _world(Path("/tmp"))
    entries.append(_node("x-4d21", contained_in=KID_A))
    closed = _cascade(entries, UNIT)
    assert "x-4d21" not in closed


def test_cascade_tolerates_malformed_rows_and_a_missing_unit():
    """read_graph can hand back rows this never validated; never raise on them."""
    entries = [None, "junk", {"no_id": True}, _node(KID_A, contained_in=UNIT)]
    assert _cascade(entries, UNIT) == [KID_A]
    # A node_id matching nothing closes nothing and still returns cleanly.
    assert _cascade(_world(Path("/tmp")), "x-0000") == []
