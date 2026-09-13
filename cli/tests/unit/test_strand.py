"""Terminal parents must not strand live children (x-a31a).

Three surfaces, one invariant: a live node under a terminal parent is
stranded (visibly owned, never dispatchable), so (1) closing over live
children is refused or re-parents them, (2) the starvation receipt names
terminal-parent stranding on every `next`, and (3) reconcile heals what
predates the guard and reports the count.
"""
from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.backlog.advance import first_dead_ancestor, selection_guards
from fno.cli import app
from fno.graph.strand import (
    _is_live,
    _live_child_ids,
    _reparent_live_children,
    _strandable_orphan_ids,
    _stranded_next_receipts,
    _sweep_reparent_stranded_orphans,
)

runner = CliRunner()

DONE = "2026-09-01T00:00:00+00:00"


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
        "created_at": "2026-07-01T00:00:00+00:00",
    }
    base.update(overrides)
    return base


# ---------------------------------------------------------------------------
# Change 1: the strand family (pure helpers)
# ---------------------------------------------------------------------------


def _strand_world() -> list[dict]:
    """grand(live) -> parent(done) -> {kid, done_kid, contained_kid}; uncle(live) -> cousin."""
    return [
        _node("x-grand"),
        _node("x-parent", parent="x-grand", completed_at=DONE, status="done"),
        _node("x-kid", parent="x-parent"),
        _node("x-donekid", parent="x-parent", completed_at=DONE, status="done"),
        _node("x-contained", parent="x-parent", contained_in="x-parent"),
        _node("x-uncle"),
        _node("x-cousin", parent="x-uncle"),
    ]


def _by_id(entries: list[dict]) -> dict:
    return {e["id"]: e for e in entries}


def test_strandable_orphan_names_live_child_of_done_parent_only():
    out = _strandable_orphan_ids(_strand_world())
    assert out == {"x-kid"}


def test_is_live_reads_the_terminal_fields():
    assert _is_live(_node("x-a"))
    assert not _is_live(_node("x-a", completed_at=DONE))
    assert not _is_live(_node("x-a", deferred_at=DONE))
    assert not _is_live(
        _node("x-a", superseded_by="x-b", supersession={"verified_at": DONE})
    )
    # superseded_by with no verifiable supersession record reads dead.
    assert not _is_live(_node("x-a", superseded_by="x-b", supersession=None))
    # A superseded node whose supersession is recorded but UNVERIFIED is live.
    assert _is_live(_node("x-a", superseded_by="x-b", supersession={"verified_at": None}))


def test_reparent_targets_the_nearest_live_ancestor():
    entries = _strand_world()
    moved = _reparent_live_children(entries, "x-parent")
    assert moved == [("x-kid", "x-grand")]
    assert _by_id(entries)["x-kid"]["parent"] == "x-grand"


def test_reparent_with_terminal_chain_sets_parent_none_key_kept():
    entries = [
        _node("x-top", completed_at=DONE, status="done"),
        _node("x-kid", parent="x-top"),
    ]
    moved = _reparent_live_children(entries, "x-top")
    assert moved == [("x-kid", None)]
    kid = _by_id(entries)["x-kid"]
    assert "parent" in kid and kid["parent"] is None


def test_reparent_leaves_terminal_and_contained_children():
    entries = _strand_world()
    _reparent_live_children(entries, "x-parent")
    rows = _by_id(entries)
    assert rows["x-donekid"]["parent"] == "x-parent"  # history, not stranding
    assert rows["x-contained"]["parent"] == "x-parent"  # the contained axis owns it


def test_reparent_never_returns_the_dead_node_itself():
    """A parent cycle back into the dead node yields None, not the dead id."""
    entries = [
        _node("x-loop", parent="x-kid", completed_at=DONE, status="done"),
        _node("x-kid", parent="x-loop", completed_at=DONE, status="done"),
        _node("x-livekid", parent="x-loop"),
    ]
    moved = _reparent_live_children(entries, "x-loop")
    assert moved == [("x-livekid", None)]
    assert _by_id(entries)["x-livekid"]["parent"] is None


def test_sweep_reparents_every_terminal_parent_in_one_pass():
    entries = _strand_world()
    moved = _sweep_reparent_stranded_orphans(entries)
    rows = _by_id(entries)
    assert rows["x-kid"]["parent"] == "x-grand"
    assert ("x-kid", "x-grand") in moved
    # Idempotent: a second sweep finds nothing.
    assert _sweep_reparent_stranded_orphans(entries) == []


def test_live_child_ids_still_feeds_the_supersede_guard():
    entries = _strand_world()
    assert _live_child_ids(entries, "x-parent") == ["x-kid"]


def test_selection_exclusion_predicate_is_unchanged():
    """x-7190 boundary: done ancestors never gate selection, superseded do."""
    now = datetime.now(timezone.utc)
    sup = _node("x-sup", status="superseded", superseded_by="x-other")
    kid = _node("x-kid", parent="x-sup")
    assert selection_guards(kid, _by_id([sup, kid]), now) == "dead-ancestor:x-sup"

    done = _node("x-donep", completed_at=DONE, status="done")
    kid2 = _node("x-kid2", parent="x-donep")
    result = selection_guards(kid2, _by_id([done, kid2]), now)
    # The done ancestor itself never gates selection (other guards may fire).
    assert not (result or "").startswith("dead-ancestor")


def test_first_dead_ancestor_takes_the_predicate():
    by_id = _by_id(_strand_world())
    kid = by_id["x-kid"]
    assert (
        first_dead_ancestor(kid, by_id, is_dead=lambda a: not _is_live(a))
        == "x-parent"
    )
    assert first_dead_ancestor(kid, by_id, is_dead=lambda a: False) is None


# ---------------------------------------------------------------------------
# Change 3: the receipt classifier
# ---------------------------------------------------------------------------


def test_receipt_names_dead_ancestor_over_plan_less():
    from fno.graph.cli import _starvation_receipts

    now = datetime.now(timezone.utc)
    done = _node("x-donep", completed_at=DONE, status="done")
    kid = _node("x-kid", parent="x-donep", status="idea")
    out = _starvation_receipts([done, kid], None, True, None, set(), now, 21)
    assert out == [("x-kid", "dead-ancestor")]


def test_receipt_leaves_contained_and_in_review_classifications_alone():
    from fno.graph.cli import _starvation_receipts

    now = datetime.now(timezone.utc)
    done = _node("x-donep", completed_at=DONE, status="done")
    contained = _node("x-cont", parent="x-donep", contained_in="x-donep")
    planned = _node(
        "x-planned",
        parent="x-donep",
        plan_path="/p/x.md",
        pr_number=7,
        pr_url="https://github.com/o/r/pull/7",
    )
    out = _starvation_receipts(
        [done, contained, planned], None, True, None, set(), now, 21
    )
    reasons = dict(out)
    assert reasons.get("x-cont") != "dead-ancestor"
    assert "x-planned" not in reasons  # in review - handled, not starved


# ---------------------------------------------------------------------------
# CLI scaffolding (the test_contain.py pattern)
# ---------------------------------------------------------------------------


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """Fresh empty graph.json routed to tmp_path."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _read_entries(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def _by_id_file(g: Path) -> dict:
    return {e["id"]: e for e in _read_entries(g)}


def _seed_idea(g: Path, title: str, *extra: str) -> str:
    before = {e.get("id") for e in _read_entries(g)}
    r = runner.invoke(
        app,
        ["backlog", "idea", title, "--difficulty", "medium", "--separate", *extra],
        catch_exceptions=False,
    )
    assert r.exit_code == 0, r.output
    new = [e["id"] for e in _read_entries(g) if e.get("id") not in before]
    assert len(new) == 1, f"expected one new node, saw {new}"
    return new[0]


def _seed_stranded_family(g: Path) -> tuple[str, str, list[str]]:
    """grand(live) -> parent(live) -> two kids; returns (grand, parent, kids)."""
    grand = _seed_idea(g, "strand grand epic")
    parent = _seed_idea(g, "strand parent epic", "--parent", grand)
    kids = [_seed_idea(g, f"strand kid {i}", "--parent", parent) for i in range(2)]
    return grand, parent, kids


# ---------------------------------------------------------------------------
# Change 2: the close-time guard
# ---------------------------------------------------------------------------


def test_canonical_done_refuses_over_live_children(tmp_graph):
    _grand, parent, kids = _seed_stranded_family(tmp_graph)
    r = runner.invoke(
        app, ["backlog", "done", parent], catch_exceptions=False
    )
    assert r.exit_code == 1, r.output
    for kid in kids:
        assert kid in r.output
    rows = _by_id_file(tmp_graph)
    assert not rows[parent].get("completed_at")  # nothing closed
    for kid in kids:
        assert rows[kid]["parent"] == parent  # nobody moved


def test_canonical_done_force_reparents_to_nearest_live_ancestor(tmp_graph):
    grand, parent, kids = _seed_stranded_family(tmp_graph)
    r = runner.invoke(
        app,
        ["backlog", "done", parent, "--force", "--reason", "deliberate close"],
        catch_exceptions=False,
    )
    assert r.exit_code == 0, r.output
    rows = _by_id_file(tmp_graph)
    assert rows[parent]["completed_at"]
    for kid in kids:
        assert rows[kid]["parent"] == grand
        assert f"{kid} -> {grand}" in r.output


def test_rich_done_auto_reparents_with_a_receipt(tmp_graph):
    grand, parent, kids = _seed_stranded_family(tmp_graph)
    r = runner.invoke(
        app,
        ["backlog", "done", parent, "--note", "rich close"],
        catch_exceptions=False,
    )
    assert r.exit_code == 0, r.output
    rows = _by_id_file(tmp_graph)
    assert rows[parent]["completed_at"]
    for kid in kids:
        assert rows[kid]["parent"] == grand
    assert "re-parented 2 stranded child(ren)" in r.output


# ---------------------------------------------------------------------------
# Change 2/3: reconcile heals and previews
# ---------------------------------------------------------------------------


@pytest.fixture
def stranded_board(tmp_path, monkeypatch):
    """A done parent stranding one live child, written straight to disk."""
    import fno.graph._constants as gc
    import fno.graph.store as gs

    graph_path = tmp_path / "graph.json"
    entries = [
        _node("x-donep", completed_at=DONE, status="done"),
        _node("x-kid", parent="x-donep"),
    ]
    graph_path.write_text(json.dumps({"entries": entries}) + "\n")
    monkeypatch.setattr(gc, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gc, "LEDGER_JSON", tmp_path / "ledger.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", graph_path)
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph_path)
    monkeypatch.setattr("fno.paths.retro_pending_dir", lambda: tmp_path / "retro")

    # No PRs on the board: stub the drift scan so the leg can never reach gh
    # (what merged is not this test's subject).
    import fno.graph._reconcile as rec

    monkeypatch.setattr(rec, "scan_merge_drift", lambda entries, node_id=None, listings=None: [])
    return graph_path


def _reconcile(*args: str):
    from fno.graph.cli import cli

    return runner.invoke(cli, ["reconcile", *args], catch_exceptions=False)


def test_reconcile_dry_run_previews_the_reparent(stranded_board):
    r = _reconcile("--dry-run")
    assert r.exit_code == 0, r.output
    assert "Would re-parent 1 stranded child(ren)" in r.output
    assert "x-kid -> (none)" in r.output
    rows = _by_id_file(stranded_board)
    assert rows["x-kid"]["parent"] == "x-donep"  # preview mutated nothing


def test_reconcile_heals_and_carries_the_set_in_json(stranded_board):
    r = _reconcile("--json")
    assert r.exit_code == 0, r.output
    payload = json.loads(r.output)
    assert payload["reparented"] == [{"node_id": "x-kid", "parent": None}]
    rows = _by_id_file(stranded_board)
    assert rows["x-kid"]["parent"] is None


def test_stranded_next_receipts_caps_and_names_the_total():
    receipts = [(f"x-{i:04x}", "dead-ancestor") for i in range(12)]
    lines = _stranded_next_receipts(receipts)
    assert len(lines) == 11  # 10 node lines + 1 summary
    assert lines[0] == "stranded x-0000: dead-ancestor"
    assert "12 node(s) stranded under terminal parents (showing 10)" in lines[-1]
    assert "fno backlog reconcile" in lines[-1]
    # Non-strand reasons never fire on a winning dispatch.
    assert _stranded_next_receipts([("x-1", "plan-less")]) == []


def test_groom_parses_the_reconcile_leg_count():
    from fno.backlog.groom import _reconcile_leg_outcome

    stdout = (
        "Auto-closed 1 container epic(s) (all children complete): x-e\n"
        "re-parented 38 stranded child(ren) under terminal parents: x-a -> (none)\n"
    )
    assert _reconcile_leg_outcome(stdout) == "ok (re-parented 38)"
    assert _reconcile_leg_outcome("No merged-PR drift found.\n") == "ok (re-parented 0)"


# ---------------------------------------------------------------------------
# Change 3: `next` keeps its stdout contract and still names the stranded
# ---------------------------------------------------------------------------


def test_next_winner_stdout_stays_clean_stderr_names_the_stranded(
    tmp_graph, tmp_path
):
    plan = tmp_path / "winner.md"
    plan.write_text("---\nstatus: ready\nnode: x-winner\n---\n\n## Execution Strategy\n")
    entries = [
        _node("x-winner", plan_path=str(plan)),
        _node("x-donep", completed_at=DONE, status="done"),
        _node("x-kid", parent="x-donep", status="idea"),
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    r = runner.invoke(app, ["backlog", "next", "--all"], catch_exceptions=False)
    assert r.exit_code == 0, r.output
    # stdout is EXACTLY the winner JSON - the contract `_next_node` parses.
    picked = json.loads(r.stdout)
    assert picked["id"] == "x-winner"
    # stderr carries the capped strand receipt and the heal verb.
    assert "stranded x-kid: dead-ancestor" in r.stderr
    assert "1 node(s) stranded under terminal parents" in r.stderr
    assert "fno backlog reconcile" in r.stderr
