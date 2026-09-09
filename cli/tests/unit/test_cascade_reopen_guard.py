"""A deliberate reopen outranks the automatic close that would undo it.

Four close paths read a reopen, and one deliberate verb (``cmd_done``, with
its own --force plus --reason ladder) does not. The sweeps:

- `_cascade_close_parents` on a child close - guarded by
  `_reopen_outranks_child_closes`.
- `_sweep_close_done_epics` on reconcile - guarded through
  `_strandable_epic_ids`.
- reconcile's PR-merged close (`cmd_reconcile`'s closeable partition) and the
  contained merge cascade `_cascade_close_contained` - guarded by
  `_reopen_outranks_merge` / the same child-keyed predicate. The
  PR-merged leg reads no children, so the child-keyed guard alone never
  reached it: a container whose own PR shipped was re-closed on that evidence
  alone. Measured 2026-09-05 a node reopened with a written reason was
  re-closed by reconcile seven minutes later; measured 2026-09-09, twice
  inside two minutes on the PR-merged path.

`reopen` requires `--reason` because a close is evidenced by a merged PR while
a reopen is nothing but human judgment. An automatic sweep discarding that
judgment without a word is what these tests pin shut.

Every guard test here has a positive control asserting the SAME fixture closes
once the reopen is removed. Without it a green run cannot tell "the guard
works" from "this fixture never closed anyway".
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.graph._reconcile import (
    _merge_postdates_reopen,
    _reopen_outranks_child_closes,
    _reopen_outranks_merge,
)
from fno.graph.cli import (
    _cascade_close_contained,
    _cascade_close_parents,
    _strandable_contained_ids,
    _strandable_epic_ids,
)

CHILD_CLOSE = "2026-09-04T20:35:52+00:00"
BEFORE_CLOSE = "2026-09-04T19:00:00+00:00"
AFTER_CLOSE = "2026-09-05T04:17:40+00:00"
MERGED_AT = "2026-08-30T12:00:00Z"
BEFORE_MERGE = "2026-08-29T00:00:00+00:00"
AFTER_MERGE = "2026-08-31T00:00:00+00:00"


def _pair(*, reopened_at=None, child_closed=CHILD_CLOSE, parent_closed=None):
    """A parent with one done child, matching the shape that misfired."""
    parent = {"id": "p", "status": "ready"}
    if reopened_at is not None:
        parent["reopened_at"] = reopened_at
    if parent_closed is not None:
        parent["completed_at"] = parent_closed
    child = {"id": "c", "parent": "p", "completed_at": child_closed}
    return parent, child


def test_no_reopen_is_unaffected():
    parent, child = _pair()
    assert _reopen_outranks_child_closes(parent, [child]) is False


def test_reopen_after_every_child_close_holds():
    parent, child = _pair(reopened_at=AFTER_CLOSE)
    assert _reopen_outranks_child_closes(parent, [child]) is True


def test_reopen_before_the_last_child_close_is_stale():
    """It expires by itself: the parent genuinely completed after that call."""
    parent, child = _pair(reopened_at=BEFORE_CLOSE)
    assert _reopen_outranks_child_closes(parent, [child]) is False


def test_reopen_exactly_at_a_child_close_is_stale():
    parent, child = _pair(reopened_at=CHILD_CLOSE)
    assert _reopen_outranks_child_closes(parent, [child]) is False


def test_one_later_child_defeats_an_otherwise_valid_reopen():
    parent, child = _pair(reopened_at=AFTER_CLOSE)
    later = {"id": "c2", "parent": "p", "completed_at": "2026-09-05T09:00:00+00:00"}
    assert _reopen_outranks_child_closes(parent, [child, later]) is False


def test_unreadable_reopen_stamp_protects():
    """Ambiguity favours the human; see the helper's docstring."""
    parent, child = _pair(reopened_at="not-a-timestamp")
    assert _reopen_outranks_child_closes(parent, [child]) is True


def test_blank_reopen_stamp_is_no_reopen():
    parent, child = _pair(reopened_at="   ")
    assert _reopen_outranks_child_closes(parent, [child]) is False


def test_unreadable_child_close_does_not_defeat_protection():
    parent, child = _pair(reopened_at=AFTER_CLOSE, child_closed="garbage")
    assert _reopen_outranks_child_closes(parent, [child]) is True


def test_naive_and_z_suffixed_stamps_compare():
    """The stamps in the graph are written both ways; neither may crash."""
    parent, child = _pair(reopened_at="2026-09-05T04:17:40Z", child_closed="2026-09-04T20:35:52")
    assert _reopen_outranks_child_closes(parent, [child]) is True


def test_sweep_skips_a_reopened_parent():
    parent, child = _pair(reopened_at=AFTER_CLOSE)
    assert _strandable_epic_ids([parent, child]) == set()


def test_sweep_positive_control_same_fixture_closes_without_the_reopen():
    """Without this, a green guard test could just be an inert fixture."""
    parent, child = _pair()
    assert _strandable_epic_ids([parent, child]) == {"p"}


def test_cascade_skips_a_reopened_parent():
    parent, child = _pair(reopened_at=AFTER_CLOSE)
    assert _cascade_close_parents([parent, child], "c") == []
    assert parent.get("completed_at") is None


def test_cascade_positive_control_same_fixture_closes_without_the_reopen():
    parent, child = _pair()
    assert _cascade_close_parents([parent, child], "c") == ["p"]
    assert parent.get("completed_at")


def _contained_pair(*, reopened_at=None):
    """A contained child whose delivery unit already closed."""
    owner = {"id": "unit", "status": "done", "completed_at": CHILD_CLOSE}
    child = {"id": "c", "status": "in_progress", "contained_in": "unit"}
    if reopened_at is not None:
        child["reopened_at"] = reopened_at
    return owner, child


def test_contained_sweep_skips_a_reopened_child():
    owner, child = _contained_pair(reopened_at=AFTER_CLOSE)
    assert _strandable_contained_ids([owner, child]) == set()


def test_contained_sweep_positive_control_same_fixture_closes_without_the_reopen():
    owner, child = _contained_pair()
    assert _strandable_contained_ids([owner, child]) == {"c"}


def test_contained_reopen_before_the_owner_close_is_stale():
    owner, child = _contained_pair(reopened_at=BEFORE_CLOSE)
    assert _strandable_contained_ids([owner, child]) == {"c"}


# ---------------------------------------------------------------------------
# `_reopen_outranks_merge`: the merge-keyed twin
# ---------------------------------------------------------------------------


def _merged_node(*, reopened_at=None):
    """A node closed on its own merged PR - the leg that reads no children."""
    node = {"id": "n", "status": "in_review", "pr_number": 42}
    if reopened_at is not None:
        node["reopened_at"] = reopened_at
    return node


def test_reopen_after_the_merge_holds():
    assert _reopen_outranks_merge(_merged_node(reopened_at=AFTER_MERGE), MERGED_AT) is True


def test_merge_guard_positive_control_no_reopen_closes():
    assert _reopen_outranks_merge(_merged_node(), MERGED_AT) is False


def test_reopen_before_the_merge_is_stale():
    """It expires by itself: a later PR merging closes the node again."""
    assert _reopen_outranks_merge(_merged_node(reopened_at=BEFORE_MERGE), MERGED_AT) is False


def test_reopen_exactly_at_the_merge_is_stale():
    assert _reopen_outranks_merge(_merged_node(reopened_at=MERGED_AT), MERGED_AT) is False


def test_missing_merge_stamp_protects():
    """None on a reverse-mapped record when gh omits mergedAt - a real path."""
    assert _reopen_outranks_merge(_merged_node(reopened_at=BEFORE_MERGE), None) is True


def test_unreadable_merge_stamp_protects():
    assert _reopen_outranks_merge(_merged_node(reopened_at=BEFORE_MERGE), "garbage") is True


def test_merge_guard_unreadable_reopen_stamp_protects():
    assert _reopen_outranks_merge(_merged_node(reopened_at="not-a-timestamp"), MERGED_AT) is True


def test_merge_guard_blank_reopen_stamp_is_no_reopen():
    assert _reopen_outranks_merge(_merged_node(reopened_at="   "), MERGED_AT) is False


def test_merge_guard_naive_and_z_suffixed_stamps_compare():
    node = _merged_node(reopened_at="2026-08-31T00:00:00Z")
    assert _reopen_outranks_merge(node, "2026-08-30T12:00:00") is True


# ---------------------------------------------------------------------------
# the contained merge cascade reads the same guard
# ---------------------------------------------------------------------------


def test_contained_cascade_skips_a_reopened_child():
    owner, child = _contained_pair(reopened_at=AFTER_CLOSE)
    assert _cascade_close_contained([owner, child], "unit") == []
    assert child.get("completed_at") is None


def test_contained_cascade_positive_control_same_fixture_closes_without_the_reopen():
    owner, child = _contained_pair()
    assert _cascade_close_contained([owner, child], "unit") == ["c"]
    assert child.get("completed_at")


def test_contained_cascade_reopen_before_the_owner_close_is_stale():
    owner, child = _contained_pair(reopened_at=BEFORE_CLOSE)
    assert _cascade_close_contained([owner, child], "unit") == ["c"]


def test_contained_cascade_reopen_between_merge_and_the_sweep_stamp_holds():
    """The owner's completed_at is reconcile's wall clock, not the close's
    evidence: a reopen made after the actual merge but before a delayed sweep
    holds when the merge stamp is the comparison basis."""
    owner = {"id": "unit", "status": "done", "completed_at": "2026-09-05T10:00:00+00:00"}
    child = {
        "id": "c",
        "status": "in_progress",
        "contained_in": "unit",
        "reopened_at": "2026-09-05T00:00:00+00:00",
    }
    assert _cascade_close_contained([owner, child], "unit", merged_at="2026-09-04T12:00:00Z") == []
    assert child.get("completed_at") is None


def test_contained_cascade_positive_control_same_child_closes_keyed_on_the_owner_stamp():
    """Without the merge stamp (self-heal sweep path) the owner's HISTORICAL
    stamp is the evidence, and the same reopen predating it is stale."""
    owner = {"id": "unit", "status": "done", "completed_at": "2026-09-05T10:00:00+00:00"}
    child = {
        "id": "c",
        "status": "in_progress",
        "contained_in": "unit",
        "reopened_at": "2026-09-05T00:00:00+00:00",
    }
    assert _cascade_close_contained([owner, child], "unit") == ["c"]


# ---------------------------------------------------------------------------
# `_merge_postdates_reopen`: the hold's own expiry
# ---------------------------------------------------------------------------


def _query_map(mapping):
    from fno.graph._reconcile import PrMergeState

    def query(number, **kw):
        state, merged_at = mapping[number]
        return PrMergeState(number=number, state=state, url=None, merged_at=merged_at)

    return query


def _ref_node(*, reopened_at=AFTER_MERGE):
    return {
        "id": "n",
        "pr_number": 42,
        "pr_url": "https://github.com/o/r/pull/42",
        "additional_prs": [{"number": 43, "url": "https://github.com/o/r/pull/43"}],
        "reopened_at": reopened_at,
    }


def test_a_later_additional_merge_expires_the_hold():
    node = _ref_node()
    query = _query_map({43: ("MERGED", "2026-09-02T00:00:00Z")})
    assert _merge_postdates_reopen(node, skip_pr=42, query=query) is True


def test_an_earlier_additional_merge_does_not_expire_the_hold():
    node = _ref_node()
    query = _query_map({43: ("MERGED", "2026-08-29T00:00:00Z")})
    assert _merge_postdates_reopen(node, skip_pr=42, query=query) is False


def test_a_read_failure_refuses_the_expiry_conclusion():
    from fno.graph._reconcile import ReconcileError

    node = _ref_node()

    def query(number, **kw):
        raise ReconcileError("gh pr view timed out")

    assert _merge_postdates_reopen(node, skip_pr=42, query=query) is False


def test_the_tripped_record_pr_is_skipped():
    """Its stamp already tripped the guard; only OTHER refs can expire it."""
    node = _ref_node()
    query = _query_map({42: ("MERGED", "2026-09-02T00:00:00Z")})
    assert _merge_postdates_reopen(node, skip_pr=42, query=query) is False


def test_expiry_scan_ignores_a_node_with_no_reopen():
    node = _ref_node(reopened_at=None)
    node.pop("reopened_at")
    query = _query_map({43: ("MERGED", "2026-09-02T00:00:00Z")})
    assert _merge_postdates_reopen(node, skip_pr=42, query=query) is False


# ---------------------------------------------------------------------------
# end to end: `fno backlog reconcile` holds the reopened node
# ---------------------------------------------------------------------------


@pytest.fixture
def routed(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Graph + ledger wired into the CLI; returns the graph path."""
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n', encoding="utf-8")
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.retro_pending_dir", lambda: tmp_path / "retro")
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return g


def _seed_merged_world(g: Path, tmp_path: Path, *, reopened_at) -> str:
    plan = tmp_path / "p.md"
    plan.write_text(
        "---\nnode: ab-reopen1\nstatus: ready\ncreated: 2026-08-09T00:00:00+00:00\n---\n\n# Plan\n",
        encoding="utf-8",
    )
    node = {
        "id": "ab-reopen1",
        "title": "node ab-reopen1",
        "domain": "code",
        "status": "in_review",
        "pr_number": 42,
        "pr_url": "https://github.com/o/r/pull/42",
        "plan_path": str(plan),
        "cost_usd": None,
        "cost_sessions": [],
        "created_at": "2026-08-09T00:00:00+00:00",
    }
    if reopened_at is not None:
        node["reopened_at"] = reopened_at
    g.write_text(json.dumps({"entries": [node]}, indent=2) + "\n", encoding="utf-8")
    return str(plan)


def _stub_scan(monkeypatch: pytest.MonkeyPatch, plan: str) -> None:
    import fno.graph._reconcile as rec

    def _scan(entries, node_id=None):
        return [
            rec.MergeDriftRecord(
                node_id="ab-reopen1",
                plan_path=plan,
                pr_number=42,
                pr_url="https://github.com/o/r/pull/42",
                pr_state="MERGED",
                merged_at=MERGED_AT,
            )
        ]

    monkeypatch.setattr(rec, "scan_merge_drift", _scan)


def _stub_gh_merged(monkeypatch: pytest.MonkeyPatch) -> None:
    import fno.graph._reconcile as rec
    from fno.graph._reconcile import PrMergeState

    monkeypatch.setattr(
        rec,
        "query_pr_merge_state",
        lambda n, **kw: PrMergeState(number=n, state="MERGED", url=None, merged_at=MERGED_AT),
    )


def test_reconcile_holds_a_reopen_postdating_the_merge(routed, tmp_path, monkeypatch):
    _stub_gh_merged(monkeypatch)
    plan = _seed_merged_world(routed, tmp_path, reopened_at=AFTER_MERGE)
    _stub_scan(monkeypatch, plan)
    from fno.graph.cli import cli

    r = CliRunner().invoke(cli, ["reconcile", "--json"])
    payload = json.loads(r.output)
    assert any(h["node_id"] == "ab-reopen1" for h in payload["reopen_held"]), r.output
    assert all(c.get("node_id") != "ab-reopen1" for c in payload["closed"])
    entry = next(e for e in json.loads(routed.read_text())["entries"] if e["id"] == "ab-reopen1")
    assert entry.get("completed_at") is None


def test_reconcile_positive_control_same_node_closes_without_the_reopen(
    routed, tmp_path, monkeypatch
):
    _stub_gh_merged(monkeypatch)
    plan = _seed_merged_world(routed, tmp_path, reopened_at=None)
    _stub_scan(monkeypatch, plan)
    from fno.graph.cli import cli

    r = CliRunner().invoke(cli, ["reconcile", "--json"])
    payload = json.loads(r.output)
    assert payload["reopen_held"] == []
    assert any(c.get("node_id") == "ab-reopen1" for c in payload["closed"]), r.output


def test_reconcile_dry_run_previews_no_close_and_names_the_held_reopen(
    routed, tmp_path, monkeypatch
):
    _stub_gh_merged(monkeypatch)
    plan = _seed_merged_world(routed, tmp_path, reopened_at=AFTER_MERGE)
    _stub_scan(monkeypatch, plan)
    from fno.graph.cli import cli

    r = CliRunner().invoke(cli, ["reconcile", "--dry-run"])
    # The held node is not in any would-close preview (an empty closeable
    # prints none at all) and the hold roll names it.
    assert "Would close" not in r.output, r.output
    assert "deliberate reopen postdates the merge" in r.output
    assert "reopened after PR #42 merged" in r.output


def test_reconcile_closes_when_a_later_additional_merge_expires_the_hold(
    routed, tmp_path, monkeypatch
):
    """The hold is not forever: the record stamps the FIRST merged ref, so a
    reopen postdating it holds only until another ref merges after the
    reopen - then the node genuinely completed again and closes."""
    import fno.graph._reconcile as rec
    from fno.graph._reconcile import PrMergeState

    def per_number(n, **kw):
        if n == 43:
            return PrMergeState(number=43, state="MERGED", url=None, merged_at="2026-09-02T00:00:00Z")
        return PrMergeState(number=n, state="MERGED", url=None, merged_at=MERGED_AT)

    monkeypatch.setattr(rec, "query_pr_merge_state", per_number)
    plan = _seed_merged_world(routed, tmp_path, reopened_at=AFTER_MERGE)
    entry = next(e for e in json.loads(routed.read_text())["entries"] if e["id"] == "ab-reopen1")
    entry["additional_prs"] = [{"number": 43, "url": "https://github.com/o/r/pull/43"}]
    routed.write_text(json.dumps({"entries": [entry]}, indent=2) + "\n", encoding="utf-8")
    _stub_scan(monkeypatch, plan)
    from fno.graph.cli import cli

    r = CliRunner().invoke(cli, ["reconcile", "--json"])
    payload = json.loads(r.output)
    assert payload["reopen_held"] == [], r.output
    assert any(c.get("node_id") == "ab-reopen1" for c in payload["closed"]), r.output


def test_locked_mutation_rechecks_the_reopen(routed, tmp_path, monkeypatch):
    """The closeable list is a pre-lock snapshot: a reopen landing between the
    scan and the locked transaction must hold there too, not just in the
    partition. Scripted predicate: pass 1 (partition) clear, pass 2 (locked
    mutation) tripped - the node must NOT close."""
    import fno.graph._reconcile as rec

    _stub_gh_merged(monkeypatch)
    plan = _seed_merged_world(routed, tmp_path, reopened_at=AFTER_MERGE)
    _stub_scan(monkeypatch, plan)

    calls = []

    def scripted(node, merged_at):
        calls.append(1)
        return len(calls) == 2

    monkeypatch.setattr(rec, "_reopen_outranks_merge", scripted)
    from fno.graph.cli import cli

    r = CliRunner().invoke(cli, ["reconcile", "--json"])
    payload = json.loads(r.output)
    assert len(calls) == 2, r.output
    assert all(c.get("node_id") != "ab-reopen1" for c in payload["closed"])
    assert any(h["node_id"] == "ab-reopen1" for h in payload["reopen_held"]), r.output
    entry = next(e for e in json.loads(routed.read_text())["entries"] if e["id"] == "ab-reopen1")
    assert entry.get("completed_at") is None
