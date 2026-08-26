"""`fno backlog reopen` - the inverse of `done`, and its refusals.

The refusals are the point of the verb, so most of this file is about the cases
it declines rather than the case it permits. A correction verb that permits
everything is a hand-edit with a nicer name, and hand-editing the graph is what
the PreToolUse hook already forbids.

Graph fixture follows test_done.py: a temp graph.json routed through the
monkeypatchable `_constants` module, with gh stubbed so no test touches GitHub.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # Seam readers (guarded metadata/display reads) resolve paths.graph_json
    # at call time; pin the resolver to the same hermetic file.
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return g


@pytest.fixture(autouse=True)
def no_plan_projection(monkeypatch, request):
    """Plan projection writes real files; the graph is what most of these assert.

    Opt out with @pytest.mark.real_plan_projection. That escape exists because
    this fixture HID a P1: reopen cleared completed_at while the plan doc stayed
    stamped `done`, so dispatch kept refusing the node and the correction did
    nothing usable. Stubbing the projector is the "guard on one of N paths"
    trap wearing a test's clothes, so the path now has its own live test.
    """
    if "real_plan_projection" in request.keywords:
        return
    import fno.graph.cli as gcli

    monkeypatch.setattr(gcli, "_project_plans_from_graph", lambda *a, **k: None)


def _write(graph: Path, *entries: dict) -> None:
    graph.write_text(json.dumps({"entries": list(entries)}))


def _read(graph: Path) -> dict[str, dict]:
    return {e["id"]: e for e in json.loads(graph.read_text())["entries"]}


def _node(nid: str, **over) -> dict:
    base = {
        "id": nid,
        "title": f"node {nid}",
        "status": "done",
        "completed_at": "2026-08-01T00:00:00+00:00",
        "domain": "code",
        "priority": "p2",
        "created_at": "2026-07-01T00:00:00+00:00",
    }
    base.update(over)
    return base


def _stub_pr(monkeypatch, state: str):
    import fno.graph.cli as gcli
    from fno.graph._reconcile import PrMergeState

    monkeypatch.setattr(
        gcli,
        "_done_gh_query",
        lambda n, **kw: PrMergeState(
            number=n,
            state=state,
            url=f"https://github.com/o/r/pull/{n}",
            merged_at="2026-08-01T00:00:00Z" if state == "MERGED" else None,
        ),
    )


# -- the permitted case --


def test_a_node_closed_in_error_reopens(tmp_graph):
    _write(tmp_graph, _node("ab-11111111"))
    res = runner.invoke(
        app, ["backlog", "reopen", "ab-11111111", "--reason", "closed by mistake"]
    )
    assert res.exit_code == 0, res.output
    node = _read(tmp_graph)["ab-11111111"]
    assert node["completed_at"] is None
    assert node["reopened_reason"] == "closed by mistake"
    assert node["reopened_at"]


def test_the_status_recomputes_off_the_cleared_completion(tmp_graph):
    """Clearing completed_at IS the status change; nothing sets status directly.

    Asserts the POSITIVE status rather than `!= "done"`. An absence assertion
    passes on any of half a dozen states, including ones that would mean the
    reopen left the node somewhere nobody intended.
    """
    _write(tmp_graph, _node("ab-11111111"))
    runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "wrong"])
    # `idea`, the underlying state of a plan-less node with no PR - not merely
    # "something other than done".
    assert _read(tmp_graph)["ab-11111111"]["status"] == "idea"


def test_a_reopened_pr_bearing_node_reads_in_review(tmp_graph, monkeypatch):
    """Not a dispatchable state, and deliberately so.

    `recompute_statuses` derives `in_review` from a pr_number with no
    completed_at, which holds the node out of the dispatch pool. That is the
    honest reading: the node has a PR, and reopening records that its close was
    wrong, not that a fresh worker should pick it up. `pr_number` survives for
    the same reason `merge_status` does - it is a fact, not an opinion.
    """
    _stub_pr(monkeypatch, "OPEN")
    _write(tmp_graph, _node("ab-11111111", pr_number=7))
    runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "early close"])
    node = _read(tmp_graph)["ab-11111111"]
    assert node["completed_at"] is None
    assert node["status"] == "in_review"
    assert node["pr_number"] == 7


# -- what it deliberately does not undo --


def test_merge_status_survives_a_reopen(tmp_graph, monkeypatch):
    """It records that GitHub confirmed a merge, which stays true after a reopen.

    Clearing it would erase a fact in order to express an opinion.
    """
    _stub_pr(monkeypatch, "MERGED")
    _write(tmp_graph, _node("ab-11111111", merge_status="merged", pr_number=7))
    res = runner.invoke(
        app, ["backlog", "reopen", "ab-11111111", "--reason", "wrong", "--force"]
    )
    assert res.exit_code == 0, res.output
    assert _read(tmp_graph)["ab-11111111"]["merge_status"] == "merged"


def test_recorded_cost_survives_a_reopen(tmp_graph):
    _write(tmp_graph, _node("ab-11111111", cost_usd=4.25))
    runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "wrong"])
    assert _read(tmp_graph)["ab-11111111"]["cost_usd"] == 4.25


def test_reopen_does_not_resurrect_a_claim(tmp_graph):
    """A holder with no lockfile behind it is worse than no holder."""
    _write(tmp_graph, _node("ab-11111111"))
    runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "wrong"])
    node = _read(tmp_graph)["ab-11111111"]
    assert not node.get("locked_by")
    assert not node.get("claimed_at")


def test_completion_note_is_cleared_not_overwritten(tmp_graph):
    """Load-bearing: _cascade_close_parents only writes its note when this is empty.

    Leaving reopen prose here would make an epic permanently unrecognizable as
    cascade-closed, so the next reopen would leave it done under a live child.
    """
    _write(tmp_graph, _node("ab-11111111", completion_note="auto-closed: all children complete"))
    runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "wrong"])
    assert _read(tmp_graph)["ab-11111111"]["completion_note"] is None


# -- refusals --


def test_a_merged_pr_refuses_the_reopen(tmp_graph, monkeypatch):
    """done's gate, inverted: it refuses when nothing merged, this when something did."""
    _stub_pr(monkeypatch, "MERGED")
    _write(tmp_graph, _node("ab-11111111", pr_number=7))
    res = runner.invoke(
        app, ["backlog", "reopen", "ab-11111111", "--reason", "changed my mind"]
    )
    assert res.exit_code == 3
    assert _read(tmp_graph)["ab-11111111"]["completed_at"] is not None


def test_the_merged_refusal_names_the_remedy(tmp_graph, monkeypatch):
    _stub_pr(monkeypatch, "MERGED")
    _write(tmp_graph, _node("ab-11111111", pr_number=7))
    res = runner.invoke(
        app, ["backlog", "reopen", "ab-11111111", "--reason", "changed my mind"]
    )
    assert "fno backlog idea" in res.output
    assert "--force" in res.output


def test_force_overrides_the_merged_refusal(tmp_graph, monkeypatch):
    _stub_pr(monkeypatch, "MERGED")
    _write(tmp_graph, _node("ab-11111111", pr_number=7))
    res = runner.invoke(
        app,
        ["backlog", "reopen", "ab-11111111", "--reason", "closed the wrong node", "-F"],
    )
    assert res.exit_code == 0, res.output
    assert _read(tmp_graph)["ab-11111111"]["completed_at"] is None


def test_a_merged_additional_pr_refuses_even_when_the_primary_is_not(tmp_graph, monkeypatch):
    """The gate has to see every ref, the way done's does.

    A node can close on a merged `additional_prs` entry while its primary
    pr_number sits closed and unmerged. Querying only the primary would permit
    exactly the case the refusal exists to catch, and record forced: false.
    """
    import fno.graph.cli as gcli
    from fno.graph._reconcile import PrMergeState

    states = {41: "CLOSED", 42: "MERGED"}

    def _by_number(n, **kw):
        return PrMergeState(
            number=n,
            state=states[n],
            url=f"https://github.com/o/r/pull/{n}",
            merged_at="2026-08-01T00:00:00Z" if states[n] == "MERGED" else None,
        )

    monkeypatch.setattr(gcli, "_done_gh_query", _by_number)
    _write(
        tmp_graph,
        _node(
            "ab-11111111",
            pr_number=41,
            pr_url="https://github.com/o/r/pull/41",
            additional_prs=[{"number": 42, "url": "https://github.com/o/r/pull/42"}],
        ),
    )
    res = runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "x"])
    assert res.exit_code == 3, res.output
    assert _read(tmp_graph)["ab-11111111"]["completed_at"] is not None


def test_the_receipt_names_the_pr_that_produced_the_state(tmp_graph, monkeypatch):
    """pr_number and pr_state have to describe the same PR.

    The aggregate outcome can come from any ref, so recording refs[0] beside a
    MERGED read off additional_prs points an auditor at the wrong PR.
    """
    import fno.graph.cli as gcli
    from fno.graph._reconcile import PrMergeState

    states = {41: "CLOSED", 42: "MERGED"}
    monkeypatch.setattr(
        gcli,
        "_done_gh_query",
        lambda n, **kw: PrMergeState(
            number=n,
            state=states[n],
            url=f"https://github.com/o/r/pull/{n}",
            merged_at="2026-08-01T00:00:00Z" if states[n] == "MERGED" else None,
        ),
    )
    captured: dict = {}
    monkeypatch.setattr(
        "fno.events.append_event", lambda ev, **kw: captured.update(ev["data"])
    )
    _write(
        tmp_graph,
        _node(
            "ab-11111111",
            pr_number=41,
            pr_url="https://github.com/o/r/pull/41",
            additional_prs=[{"number": 42, "url": "https://github.com/o/r/pull/42"}],
        ),
    )
    res = runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "x", "-F"])
    assert res.exit_code == 0, res.output
    assert captured["pr_state"] == "MERGED"
    assert captured["pr_number"] == 42


def test_forced_is_false_when_no_refusal_was_bypassed(tmp_graph, monkeypatch):
    """`forced` claims the work is in main and was reopened anyway.

    Deriving it from the flag stamps that claim on an ordinary --force reopen of
    a node with an open PR, which is a false receipt on the field an auditor
    reads to find the risky reopens.
    """
    _stub_pr(monkeypatch, "OPEN")
    captured: dict = {}
    monkeypatch.setattr(
        "fno.events.append_event", lambda ev, **kw: captured.update(ev["data"])
    )
    _write(tmp_graph, _node("ab-11111111", pr_number=7))
    runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "x", "--force"])
    assert captured["forced"] is False


def test_forced_is_true_when_a_merged_refusal_was_bypassed(tmp_graph, monkeypatch):
    """Positive control for the test above."""
    _stub_pr(monkeypatch, "MERGED")
    captured: dict = {}
    monkeypatch.setattr(
        "fno.events.append_event", lambda ev, **kw: captured.update(ev["data"])
    )
    _write(tmp_graph, _node("ab-11111111", pr_number=7))
    runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "x", "--force"])
    assert captured["forced"] is True


def test_the_gate_carries_the_nodes_cwd(tmp_graph, monkeypatch):
    """A foreign-repo ref must resolve against its own repository.

    Without the node's cwd, `gh pr view N` runs in whatever checkout is current
    and answers about an unrelated PR #N.
    """
    import fno.graph.cli as gcli
    from fno.graph._reconcile import PrMergeState

    seen: dict = {}

    def _capture(n, **kw):
        seen.update(kw)
        return PrMergeState(number=n, state="OPEN", url=None, merged_at=None)

    monkeypatch.setattr(gcli, "_done_gh_query", _capture)
    _write(tmp_graph, _node("ab-11111111", pr_number=7, cwd="/repos/other"))
    runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "x"])
    assert seen.get("cwd") == "/repos/other"


def test_an_open_pr_does_not_block_a_reopen(tmp_graph, monkeypatch):
    """Only a MERGED PR is evidence the work shipped."""
    _stub_pr(monkeypatch, "OPEN")
    _write(tmp_graph, _node("ab-11111111", pr_number=7))
    res = runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "early close"])
    assert res.exit_code == 0, res.output


def test_a_gh_outage_leaves_the_node_done(tmp_graph, monkeypatch):
    """An unreachable gh is a missing answer, not a permitting one.

    Raises ReconcileError, which is what `query_pr_merge_state` raises and what
    the shared resolver catches. A stub raising a bare RuntimeError would test a
    path production never takes.
    """
    import fno.graph.cli as gcli
    from fno.graph._reconcile import ReconcileError

    def _boom(n, **kw):
        raise ReconcileError("gh: network unreachable")

    monkeypatch.setattr(gcli, "_done_gh_query", _boom)
    _write(tmp_graph, _node("ab-11111111", pr_number=7))
    res = runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "x"])
    assert res.exit_code == 4
    assert _read(tmp_graph)["ab-11111111"]["completed_at"] is not None


def test_a_routing_refusal_does_not_reopen_the_node(tmp_graph, monkeypatch):
    import fno.graph.cli as gcli
    from fno.graph._reconcile import ReconcileError

    def _refused(n, **kw):
        raise ReconcileError(
            "[fno GraphQL reserve] use `fno do pr info 1140`; unconditional route refusal"
        )

    monkeypatch.setattr(gcli, "_done_gh_query", _refused)
    _write(
        tmp_graph,
        _node(
            "ab-route001",
            pr_number=1140,
            pr_url="https://github.com/o/r/pull/1140",
        ),
    )
    res = runner.invoke(app, ["backlog", "reopen", "ab-route001", "--reason", "x"])

    assert res.exit_code == 3
    combined = res.output + (res.stderr or "")
    assert "fno do pr info 1140 --repo o/r" in combined
    assert "retryable once gh is available again" not in combined
    assert _read(tmp_graph)["ab-route001"]["completed_at"] is not None


def test_a_node_that_is_not_done_warns_and_changes_nothing(tmp_graph):
    """Idempotent in the safe direction, matching unsupersede's shape."""
    _write(tmp_graph, _node("ab-11111111", completed_at=None, status="ready"))
    before = tmp_graph.read_text()
    res = runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "x"])
    assert res.exit_code == 0
    assert "not done" in res.output
    assert tmp_graph.read_text() == before


def test_a_blank_reason_is_a_usage_error(tmp_graph):
    _write(tmp_graph, _node("ab-11111111"))
    res = runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "   "])
    assert res.exit_code == 2
    assert _read(tmp_graph)["ab-11111111"]["completed_at"] is not None


def test_an_unknown_node_is_not_found(tmp_graph):
    _write(tmp_graph)
    res = runner.invoke(app, ["backlog", "reopen", "ab-99999999", "--reason", "x"])
    assert res.exit_code == 1


def test_an_archived_node_is_named_as_archived_not_missing(tmp_graph, tmp_path, monkeypatch):
    """The absence has two explanations; the refusal has to pick the true one.

    Reporting "not found" for a node sitting readable in graph-archive.json is
    the same message a typo gets, and it sent the caller looking for a node they
    already had.
    """
    archive = tmp_path / "graph-archive.json"
    archive.write_text(json.dumps({"entries": [_node("ab-22222222")]}))
    import fno.graph._constants as gc

    # The constant, not the helper behind it: see test_backlog_unarchive.py for
    # why a helper patch survives in isolation and dies in the full suite.
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", archive)
    _write(tmp_graph)
    res = runner.invoke(app, ["backlog", "reopen", "ab-22222222", "--reason", "x"])
    assert res.exit_code == 4
    assert "archived" in res.output
    assert "unarchive" in res.output


def test_the_archived_refusal_survives_a_partial_id(tmp_graph, tmp_path, monkeypatch):
    """An exact compare here recreates the ambiguity the refusal exists to remove.

    `_find_node` resolves a short id against the working graph, so the archive
    probe has to resolve it the same way or `reopen ab-2222` reports "not found"
    for a node sitting readable in the archive.
    """
    archive = tmp_path / "graph-archive.json"
    archive.write_text(json.dumps({"entries": [_node("ab-22222222")]}))
    import fno.graph._constants as gc

    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", archive)
    _write(tmp_graph)
    res = runner.invoke(app, ["backlog", "reopen", "ab-2222", "--reason", "x"])
    assert res.exit_code == 4, res.output
    assert "archived" in res.output


# -- the cascade --


def test_an_auto_closed_epic_reopens_with_its_child(tmp_graph):
    """An epic is done exactly when its children are; one of them is open again."""
    _write(
        tmp_graph,
        _node("ab-e0000000", type="epic", completion_note="auto-closed: all children complete"),
        _node("ab-c0000000", parent="ab-e0000000"),
    )
    res = runner.invoke(app, ["backlog", "reopen", "ab-c0000000", "--reason", "wrong"])
    assert res.exit_code == 0, res.output
    nodes = _read(tmp_graph)
    assert nodes["ab-c0000000"]["completed_at"] is None
    assert nodes["ab-e0000000"]["completed_at"] is None
    assert "ab-e0000000" in res.output


def test_the_cascade_still_fires_for_a_partial_id(tmp_graph):
    """_find_node resolves `ab-c000` to the full id; the cascade walks full ids.

    Passing the argument straight through made the cascade find nothing for
    exactly the callers who typed the short form, leaving an auto-closed epic
    done over a live child - a silent wrong answer, not an error.
    """
    _write(
        tmp_graph,
        _node("ab-e0000000", type="epic", completion_note="auto-closed: all children complete"),
        _node("ab-c0000000", parent="ab-e0000000"),
    )
    res = runner.invoke(app, ["backlog", "reopen", "ab-c000", "--reason", "wrong"])
    assert res.exit_code == 0, res.output
    nodes = _read(tmp_graph)
    assert nodes["ab-c0000000"]["completed_at"] is None
    assert nodes["ab-e0000000"]["completed_at"] is None


def test_an_epic_closed_on_its_own_evidence_is_left_done_and_named(tmp_graph):
    """Silently reopening it would discard a judgment this verb never made."""
    _write(
        tmp_graph,
        _node("ab-e0000000", type="epic", completion_note="closed by operator after review"),
        _node("ab-c0000000", parent="ab-e0000000"),
    )
    res = runner.invoke(app, ["backlog", "reopen", "ab-c0000000", "--reason", "wrong"])
    assert res.exit_code == 0, res.output
    nodes = _read(tmp_graph)
    assert nodes["ab-c0000000"]["completed_at"] is None
    assert nodes["ab-e0000000"]["completed_at"] is not None
    assert "ab-e0000000" in res.output
    assert "own evidence" in res.output


def test_the_cascade_climbs_more_than_one_level(tmp_graph):
    auto = "auto-closed: all children complete"
    _write(
        tmp_graph,
        _node("ab-e1000000", type="epic", completion_note=auto),
        _node("ab-e2000000", type="epic", parent="ab-e1000000", completion_note=auto),
        _node("ab-c0000000", parent="ab-e2000000"),
    )
    runner.invoke(app, ["backlog", "reopen", "ab-c0000000", "--reason", "wrong"])
    nodes = _read(tmp_graph)
    assert nodes["ab-e1000000"]["completed_at"] is None
    assert nodes["ab-e2000000"]["completed_at"] is None


def test_a_close_reopen_close_cycle_re_annotates_the_epic(tmp_graph):
    """The regression the completion_note clear exists to prevent.

    If reopen left prose in completion_note, _cascade_close_parents would skip
    its `auto-closed:` write on the second close (it only writes when the field
    is empty), and a second reopen would leave the epic done under a live child.
    """
    from fno.graph.cli import _apply_completion_fields, _cascade_close_parents

    entries = [
        _node("ab-e0000000", type="epic", completion_note="auto-closed: all children complete"),
        _node("ab-c0000000", parent="ab-e0000000"),
    ]
    _write(tmp_graph, *entries)
    runner.invoke(app, ["backlog", "reopen", "ab-c0000000", "--reason", "wrong"])

    # Re-close the child the way `done` does, then let the cascade run.
    live = list(json.loads(tmp_graph.read_text())["entries"])
    child = next(e for e in live if e["id"] == "ab-c0000000")
    _apply_completion_fields(child)
    _cascade_close_parents(live, "ab-c0000000")
    epic = next(e for e in live if e["id"] == "ab-e0000000")
    assert epic["completed_at"] is not None
    assert str(epic["completion_note"]).startswith("auto-closed:")


# -- the plan doc, projected for real --


@pytest.mark.real_plan_projection
def test_the_plan_doc_comes_off_terminal_done(tmp_graph, tmp_path):
    """The P1 the stubbed fixture hid.

    Clearing completed_at in the graph while the plan stays stamped `done`
    leaves dispatch refusing the node: the verb reports success and nothing the
    operator can use has changed. The projector is forward-only, so reopen has
    to force the plan off terminal the way unsupersede does.
    """
    plan = tmp_path / "plan.md"
    plan.write_text(
        "---\nstatus: done\ndone_at: 2026-08-01T00:00:00Z\n---\n\n# a plan\n"
    )
    _write(tmp_graph, _node("ab-11111111", plan_path=str(plan)))

    res = runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "wrong"])
    assert res.exit_code == 0, res.output
    assert "status: done" not in plan.read_text()


@pytest.mark.real_plan_projection
def test_an_untouched_plan_stays_put_when_the_node_was_not_done(tmp_graph, tmp_path):
    """Positive control: the forced write happens on the reopen, not on every call."""
    plan = tmp_path / "plan.md"
    original = "---\nstatus: done\ndone_at: 2026-08-01T00:00:00Z\n---\n\n# a plan\n"
    plan.write_text(original)
    _write(tmp_graph, _node("ab-11111111", completed_at=None, plan_path=str(plan)))

    runner.invoke(app, ["backlog", "reopen", "ab-11111111", "--reason", "x"])
    assert plan.read_text() == original


# -- the event --


def test_the_reopen_event_validates_against_the_live_schema():
    from fno.events import backlog_reopened

    event = backlog_reopened(
        node_id="ab-11111111",
        reason="closed in error",
        forced=True,
        pr_number=7,
        pr_state="MERGED",
        cascade_reopened=["ab-e0000000"],
    )
    assert event["type"] == "backlog_reopened"
    assert event["data"]["reason"] == "closed in error"
    assert event["data"]["cascade_reopened"] == "ab-e0000000"


def test_the_event_schema_requires_a_reason():
    """Positive control: the schema really does validate, so the test above means
    something. A reopen with no reason is a state change nobody can account for."""
    from fno.events import _build

    with pytest.raises(Exception):
        _build("backlog_reopened", "backlog", {"node_id": "ab-11111111"})
