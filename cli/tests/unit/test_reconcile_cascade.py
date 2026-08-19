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


# -- a sweep that closes three nodes must not report closing one --


def test_contained_closes_are_reported_in_the_human_summary(world, merged_pr,
                                                            dispatches):
    """Silent extra closes read as "already in sync" to the next operator."""
    result = _reconcile()
    assert result.exit_code == 0
    assert KID_A in result.output
    assert KID_B in result.output
    assert "contained" in result.output.lower()


def test_contained_closes_are_reported_in_the_json_payload(world, merged_pr,
                                                           dispatches):
    """The SessionStart hook runs reconcile with --json and discards stderr.

    Separate from `closed`, whose entries all carry their own pr_number - a
    contained node has none, so folding it in would need a null-PR row.
    """
    result = _reconcile("--json")
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert sorted(payload["contained_closed"]) == sorted([KID_A, KID_B])
    assert [c["node_id"] for c in payload["closed"]] == [UNIT]


def test_dry_run_previews_the_contained_closes_and_mutates_nothing(world, merged_pr,
                                                                   dispatches):
    """A preview that under-reports is worse than no preview."""
    result = _reconcile("--dry-run")
    assert result.exit_code == 0
    assert KID_A in result.output and KID_B in result.output

    nodes = world[1]()
    for nid in (UNIT, KID_A, KID_B):
        assert nodes[nid]["completed_at"] is None, nid


def test_clean_graph_still_reports_in_sync(world, merged_pr, monkeypatch):
    """The new accumulator must not turn a no-op sweep into a noisy one."""
    import fno.graph._reconcile as rec

    monkeypatch.setattr(rec, "scan_merge_drift", lambda entries, node_id=None: [])
    result = _reconcile()
    assert result.exit_code == 0
    assert "in sync" in result.output


# -- codex P1: a unit that shipped BEFORE the containment record existed --


def test_strandable_contained_finds_a_child_of_an_already_done_unit():
    """The state the merge-time cascade structurally cannot reach.

    Re-running an `adopt` spec back-fills `contained_in` onto a node adopted by
    an older fno. If that node's owner already merged, the back-fill removes it
    from selection with nothing left that would ever complete it: visible,
    unbuildable, never done. `scan_merge_drift` skips the closed owner and the
    cascade only fires while closing one.
    """
    from fno.graph.cli import _strandable_contained_ids

    entries = _world(Path("/tmp"))
    for e in entries:
        if e["id"] == UNIT:
            e["completed_at"] = "2026-07-28T00:00:00+00:00"
    assert _strandable_contained_ids(entries) == {KID_A, KID_B}


def test_strandable_contained_ignores_an_open_or_missing_owner():
    """Only an owner that is DONE strands its children.

    An open owner closes them itself on its merge, and a dangling owner id is
    not evidence anything shipped - closing on it would invent a completion.
    """
    from fno.graph.cli import _strandable_contained_ids

    assert _strandable_contained_ids(_world(Path("/tmp"))) == set()

    dangling = [_node(KID_A, contained_in="x-0000")]
    assert _strandable_contained_ids(dangling) == set()


def test_reconcile_heals_a_stranded_contained_node_with_no_drift(world, monkeypatch,
                                                                 dispatches):
    """No merged PR this sweep, and the heal still runs and is reported.

    Gated on `strandable_contained` so the lock is taken for a heal-only run;
    without that the sweep would only fire when some OTHER node happened to
    have drift, which is arbitrary.
    """
    import fno.graph._reconcile as rec

    write, read = world
    entries = _world(Path("/tmp"))
    for e in entries:
        if e["id"] == UNIT:
            e["completed_at"] = "2026-07-28T00:00:00+00:00"
            e["cost_usd"] = 18.22
    write(entries)
    monkeypatch.setattr(rec, "scan_merge_drift", lambda entries, node_id=None: [])

    result = _reconcile()
    assert result.exit_code == 0

    nodes = read()
    for nid in (KID_A, KID_B):
        assert nodes[nid]["completed_at"] is not None, nid
        assert nodes[nid]["cost_usd"] is None, nid
        assert UNIT in (nodes[nid]["completion_note"] or ""), nid
    assert KID_A in result.output


def test_heal_is_full_sweep_only(world, monkeypatch, dispatches):
    """A node-scoped run must not close nodes it was not pointed at.

    Same scoping as the stranded-epic sweep: `reconcile --node <id>` is a
    targeted operation, and healing unrelated subtrees from it is a surprise.
    """
    import fno.graph._reconcile as rec

    write, read = world
    entries = _world(Path("/tmp"))
    for e in entries:
        if e["id"] == UNIT:
            e["completed_at"] = "2026-07-28T00:00:00+00:00"
    write(entries)
    monkeypatch.setattr(rec, "scan_merge_drift", lambda entries, node_id=None: [])

    assert _reconcile("--node", DEP).exit_code == 0
    assert read()[KID_A]["completed_at"] is None


def test_heal_is_previewed_by_dry_run_and_mutates_nothing(world, monkeypatch,
                                                          dispatches):
    import fno.graph._reconcile as rec

    write, read = world
    entries = _world(Path("/tmp"))
    for e in entries:
        if e["id"] == UNIT:
            e["completed_at"] = "2026-07-28T00:00:00+00:00"
    write(entries)
    monkeypatch.setattr(rec, "scan_merge_drift", lambda entries, node_id=None: [])

    result = _reconcile("--dry-run")
    assert result.exit_code == 0
    assert KID_A in result.output
    assert read()[KID_A]["completed_at"] is None


def test_heal_is_idempotent_across_repeated_sweeps(world, monkeypatch, dispatches):
    """reconcile auto-fires on SessionStart; the second run must be a no-op."""
    import fno.graph._reconcile as rec

    write, read = world
    entries = _world(Path("/tmp"))
    for e in entries:
        if e["id"] == UNIT:
            e["completed_at"] = "2026-07-28T00:00:00+00:00"
    write(entries)
    monkeypatch.setattr(rec, "scan_merge_drift", lambda entries, node_id=None: [])

    assert _reconcile().exit_code == 0
    first = read()[KID_A]["completed_at"]
    result = _reconcile()
    assert result.exit_code == 0
    assert read()[KID_A]["completed_at"] == first
    assert "in sync" in result.output


# -- sigma round: defects the panel found in the first cut --


def test_contained_node_gets_its_own_starvation_reason_not_quarantined(tmp_path):
    """`quarantined` reads as stale work needing attention; this is neither.

    A decomposed epic printed one bogus line per adopted node on every
    `fno backlog next`, and it could not clear until the unit merged - permanent
    noise the operator cannot act on. The other named guards each get their own
    reason for exactly this reason.
    """
    from datetime import datetime, timezone

    from fno.graph.cli import _starvation_receipts

    now = datetime(2026, 7, 30, tzinfo=timezone.utc)
    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: ready\n---\n")
    entries = [
        _node(KID_A, contained_in=UNIT, plan_path=str(plan),
              created_at=now.isoformat()),
        _node(UNIT, plan_path=str(plan), created_at=now.isoformat()),
    ]
    receipts = _starvation_receipts(
        entries, None, True, None, set(), now, 21
    )
    assert (KID_A, "contained") in receipts
    assert not any(r == (KID_A, "quarantined") for r in receipts)


def test_sweep_calls_the_strandable_scan_once_not_once_per_entry():
    """It ran inside a set comprehension: O(N^2) on a SessionStart-hot path.

    Counting calls rather than timing: a timing assertion would be flaky, and
    the defect is the call count, not the wall clock.
    """
    import fno.graph.cli as gcli

    entries = _world(Path("/tmp"))
    for e in entries:
        if e["id"] == UNIT:
            e["completed_at"] = "2026-07-28T00:00:00+00:00"

    real = gcli._strandable_contained_ids
    calls = []

    def counted(es):
        calls.append(1)
        return real(es)

    try:
        gcli._strandable_contained_ids = counted
        gcli._sweep_close_stranded_contained(entries)
    finally:
        gcli._strandable_contained_ids = real
    assert len(calls) == 1, f"scanned {len(calls)}x for {len(entries)} entries"


def test_neither_id_read_raises_on_a_row_carrying_contained_in_without_an_id():
    """A KeyError here aborts the WHOLE reconcile - it runs outside any guard.

    Same failure class as the SessionStart jq bug in this PR: a read of
    untrusted graph rows must never be the thing that takes the sweep down.
    """
    from fno.graph.cli import _cascade_close_contained, _strandable_contained_ids

    entries = [
        {"contained_in": UNIT},                      # no id at all
        {"id": None, "contained_in": UNIT},          # null id
        _node(UNIT, completed_at="2026-07-28T00:00:00+00:00"),
        _node(KID_A, contained_in=UNIT),
    ]
    assert _strandable_contained_ids(entries) == {KID_A}
    assert _cascade_close_contained(entries, UNIT) == [KID_A]


def test_heal_only_sweep_does_not_claim_there_were_PRs(world, monkeypatch,
                                                        dispatches):
    """With no drift there are no "those PRs" to point at, and no "Also"."""
    import fno.graph._reconcile as rec

    write, _read = world
    entries = _world(Path("/tmp"))
    for e in entries:
        if e["id"] == UNIT:
            e["completed_at"] = "2026-07-28T00:00:00+00:00"
    write(entries)
    monkeypatch.setattr(rec, "scan_merge_drift", lambda entries, node_id=None: [])

    out = _reconcile().output
    assert "Closed 0 node(s)" not in out
    assert "those PRs" not in out
    assert "already-merged delivery units" in out


def test_reparenting_away_from_the_unit_un_adopts(world, dispatches):
    """The only escape from a mistyped `adopt` id, and it must exist.

    Nothing else clears `contained_in`: decompose is the sole writer, re-running
    the spec without the entry leaves the stale value, and graph.json is a
    hook-blocked forbidden surface. One typo therefore unarmed a real delivery
    unit permanently AND had the cascade later stamp it "shipped inside
    <owner>" - a false completion note on work that never shipped.
    """
    from typer.testing import CliRunner

    from fno.graph.cli import cli

    write, read = world
    write(_world(Path("/tmp")))

    assert CliRunner().invoke(
        cli, ["update", KID_A, "--parent", "null"]
    ).exit_code == 0
    assert read()[KID_A].get("contained_in") is None


def test_reparenting_within_the_unit_keeps_containment(world, dispatches):
    """Keyed on moving away from THE OWNER, not on any re-parent at all.

    A contained node re-parented to its own delivery unit is still contained;
    clearing on every `--parent` would silently re-arm it.
    """
    from typer.testing import CliRunner

    from fno.graph.cli import cli

    write, read = world
    write(_world(Path("/tmp")))

    assert CliRunner().invoke(
        cli, ["update", KID_A, "--parent", UNIT]
    ).exit_code == 0
    assert read()[KID_A]["contained_in"] == UNIT


def test_reparenting_onto_a_descendant_of_the_unit_keeps_containment(world,
                                                                     dispatches):
    """codex P2: an identity test contradicted its own comment.

    A node moved onto a DESCENDANT of its delivery unit is still inside that
    unit, but `== owner` un-contained it - making it independently dispatchable
    and costed again, and dropping it from the owner's merge cascade.
    """
    from typer.testing import CliRunner

    from fno.graph.cli import cli

    write, read = world
    entries = _world(Path("/tmp"))
    entries.append(_node("x-7c2a", parent=UNIT))   # a node under the unit
    write(entries)

    assert CliRunner().invoke(
        cli, ["update", KID_A, "--parent", "x-7c2a"]
    ).exit_code == 0
    assert read()[KID_A]["contained_in"] == UNIT


def test_reparenting_outside_the_unit_subtree_still_un_contains(world, dispatches):
    """The escape hatch must survive the subtree widening."""
    from typer.testing import CliRunner

    from fno.graph.cli import cli

    write, read = world
    entries = _world(Path("/tmp"))
    entries.append(_node("x-5e11"))   # unrelated, not under the unit
    write(entries)

    assert CliRunner().invoke(
        cli, ["update", KID_A, "--parent", "x-5e11"]
    ).exit_code == 0
    assert read()[KID_A].get("contained_in") is None


def test_cascade_failure_reaches_the_json_payload_not_only_stderr(world, merged_pr,
                                                                  dispatches,
                                                                  monkeypatch):
    """The SessionStart hook runs `reconcile --json` and discards stderr.

    A warning only on stderr meant a repeatedly-failing cascade left contained
    nodes open forever with no signal reaching any automated reader - a leg
    whose failure is unobservable is indistinguishable from one that never ran.
    """
    import fno.graph.cli as gcli

    def boom(entries, node_id):
        raise RuntimeError("cascade exploded")

    monkeypatch.setattr(gcli, "_cascade_close_contained", boom)
    result = _reconcile("--json")
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload["contained_errors"], payload
    err = payload["contained_errors"][0]
    assert err["owner"] == UNIT
    assert "cascade exploded" in err["error"]
    # The delivery unit still closed - the cascade is a warning, never an abort.
    assert [c["node_id"] for c in payload["closed"]] == [UNIT]


def test_clean_sweep_reports_no_contained_errors(world, merged_pr, dispatches):
    """The key is always present so a reader need not distinguish absent from empty."""
    payload = json.loads(_reconcile("--json").stdout)
    assert payload["contained_errors"] == []


def test_dry_run_never_crashes_on_a_fallible_cascade(world, merged_pr, dispatches,
                                                     monkeypatch):
    """The preview must not fail harder than the run it previews.

    The real mutator wraps both legs precisely because they are fallible; the
    simulation called them bare, so a raise crashed --dry-run with a traceback
    while `contained_errors` stayed [] - asserting no errors for a leg that
    never completed.
    """
    import fno.graph.cli as gcli

    def boom(entries, node_id):
        raise RuntimeError("cascade exploded")

    monkeypatch.setattr(gcli, "_cascade_close_contained", boom)
    result = _reconcile("--dry-run", "--json")
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["contained_errors"], payload
    assert "cascade exploded" in payload["contained_errors"][0]["error"]


def test_deferring_a_unit_preserves_its_contained_children(world, dispatches):
    """A reversible pause keeps the delivery unit folded as one unit."""
    from typer.testing import CliRunner

    from fno.graph.cli import cli

    write, read = world
    write(_world(Path("/tmp")))
    assert read()[KID_A]["contained_in"] == UNIT

    assert CliRunner().invoke(
        cli, ["defer", UNIT, "--reason", "parked"]
    ).exit_code == 0

    kid = read()[KID_A]
    assert kid.get("contained_in") == UNIT
    assert kid.get("completed_at") is None


def test_release_helper_covers_every_way_a_unit_dies(world, dispatches):
    """One helper, because there are SIX writers of this transition.

    Wiring three of them was the decorative-guard shape this whole change is
    about: the maintain auto-defer legs reach the same state as `cmd_defer` and
    must free the same children.
    """
    from fno.graph.cli import _release_contained_children

    entries = _world(Path("/tmp"))
    freed = _release_contained_children(entries, UNIT)
    assert sorted(freed) == sorted([KID_A, KID_B])
    assert all(e.get("contained_in") is None for e in entries)
    # Released, never closed - a unit dying is not a claim its children shipped.
    assert all(e.get("completed_at") is None for e in entries)
    # Idempotent, and a falsy owner frees nothing rather than everything.
    assert _release_contained_children(entries, UNIT) == []
    assert _release_contained_children(_world(Path("/tmp")), None) == []
    assert _release_contained_children(_world(Path("/tmp")), "") == []


def test_release_parented_children_clears_non_done_revivable():
    """The membership-axis sibling of _release_contained_children.

    _release_parented_children clears `parent` on a dying unit's NON-DONE
    children (live, deferred, superseded - all can revive and strand); only a
    done child keeps it as history. _live_child_ids - the guard's read side -
    returns only parent-ONLY LIVE children: a child also marked contained_in is
    folded delivery work the contained release handles routinely, so the guard
    must not refuse the supersede over it, and a deferred/superseded child is
    not currently dispatchable so it must not block the supersede either.
    """
    from fno.graph.cli import _live_child_ids, _release_parented_children

    def _kid(
        id_,
        *,
        parent=None,
        contained_in=None,
        completed_at=None,
        deferred_at=None,
        superseded_by=None,
    ):
        return {
            "id": id_,
            "parent": parent,
            "contained_in": contained_in,
            "completed_at": completed_at,
            "deferred_at": deferred_at,
            "superseded_by": superseded_by,
        }

    entries = [
        _kid("live-a", parent="x-epic"),
        _kid("live-b", parent="x-epic"),
        _kid("cont-f", parent="x-epic", contained_in="x-epic"),  # folded work
        _kid("done-c", parent="x-epic", completed_at="2026-07-01T00:00:00+00:00"),
        _kid("def-d", parent="x-epic", deferred_at="2026-07-01T00:00:00+00:00"),
        _kid("sup-e", parent="x-epic", superseded_by="x-other"),
        _kid("unrelated", parent="x-someone-else"),
    ]
    # Guard sees parent-ONLY LIVE children: cont-f (contained), def-d (deferred),
    # sup-e (superseded) are all excluded - they do not block the supersede.
    assert sorted(_live_child_ids(entries, "x-epic")) == ["live-a", "live-b"]
    # Release clears parent on every NON-DONE child of the unit (cont-f, def-d,
    # sup-e too: each can revive later and would strand under a dead parent).
    assert sorted(_release_parented_children(entries, "x-epic")) == [
        "cont-f", "def-d", "live-a", "live-b", "sup-e",
    ]

    by_id = {e["id"]: e for e in entries}
    assert by_id["live-a"]["parent"] is None
    assert by_id["cont-f"]["parent"] is None
    assert by_id["def-d"]["parent"] is None
    assert by_id["sup-e"]["parent"] is None
    # Done keeps parent as history; unrelated nodes untouched.
    assert by_id["done-c"]["parent"] == "x-epic"
    assert by_id["unrelated"]["parent"] == "x-someone-else"
    # Idempotent + falsy owner frees nothing.
    assert _release_parented_children(entries, "x-epic") == []
    assert _release_parented_children(entries, None) == []


def test_reversible_defer_writers_preserve_contained_children():
    """Direct, maintenance, and triage defers must not unbundle the unit."""
    import inspect

    from fno.graph import cli as gcli
    from fno.graph import triage

    for writer in (gcli.cmd_defer, gcli.cmd_maintain, triage.cmd_apply):
        assert "_release_contained_children" not in inspect.getsource(writer)

    # Permanent death still releases children so they do not strand forever.
    assert "_release_contained_children" in inspect.getsource(gcli.cmd_remove)
    assert "_release_contained_children" in inspect.getsource(gcli.cmd_supersede)


def test_release_is_reported_not_silent(world, dispatches):
    """A silent release turns N invisible nodes into buildable ones unannounced.

    The bare "Deferred <id>" receipt gave the operator no way to know what the
    next selection pass would pick up - and the helper already returned the ids,
    which every call site discarded.
    """
    from typer.testing import CliRunner

    from fno.graph.cli import cli

    write, _read = world
    write(_world(Path("/tmp")))
    result = CliRunner().invoke(cli, ["defer", UNIT, "--reason", "parked"])
    assert result.exit_code == 0, result.output
    assert "Released 2 contained node(s)" in result.output
    assert KID_A in result.output and KID_B in result.output


def test_release_receipt_is_silent_when_nothing_was_contained(world, dispatches):
    """No noise on the overwhelmingly common shape."""
    from typer.testing import CliRunner

    from fno.graph.cli import cli

    write, _read = world
    write([e for e in _world(Path("/tmp")) if e["id"] not in (KID_A, KID_B)])
    result = CliRunner().invoke(cli, ["defer", UNIT, "--reason", "parked"])
    assert result.exit_code == 0, result.output
    assert "Released" not in result.output
