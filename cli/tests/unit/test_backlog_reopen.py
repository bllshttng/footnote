"""`fno backlog reopen` - the inverse of `done`, and its refusals.

The verb is native (backlog/workflows.rs), so these scenarios drive the door
(the fno-agents binary against a sandboxed store), the way the golden
receipts do. The refusals are the point of the verb, so most of this file is
about the cases it declines rather than the case it permits. A correction
verb that permits everything is a hand-edit with a nicer name, and
hand-editing the graph is what the PreToolUse hook already forbids.

The wheel-level drive-audit receipt (`backlog_done_operator_initiated`)
moved into the binary with the verb; a sandbox has no space dir for the
event journal, so that tail is not door-observable and has no python
monkeypatch surface left to pin.
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
from pathlib import Path

import pytest

from tests.goldens._door import door, make_sandbox, warm, write_pr_stub
from fno.graph.store import read_graph_strict


def sandbox(tmp_path: Path, *entries: dict) -> Path:
    root = make_sandbox(tmp_path, list(entries))
    warm(root, entries[0]["id"] if entries else "ab-00000000")
    return root


def _write(root: Path, *entries: dict) -> None:
    seed_graph(root / "graph.json", json.dumps({"entries": list(entries)}))


def _read(root: Path) -> dict[str, dict]:
    return {e["id"]: e for e in read_graph_strict(root / "graph.json")}


def _node(nid: str, **over) -> dict:
    base = {
        "id": nid,
        "title": f"node {nid}",
        "slug": nid,
        "type": "feature",
        "status": "done",
        "completed_at": "2026-08-01T00:00:00+00:00",
        "domain": "code",
        "priority": "p2",
        "created_at": "2026-07-01T00:00:00+00:00",
    }
    base.update(over)
    return base


# -- the permitted case --


def test_a_node_closed_in_error_reopens(tmp_path):
    root = sandbox(tmp_path, _node("ab-11111111"))
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "closed by mistake"])
    assert code == 0, err
    node = _read(root)["ab-11111111"]
    assert node["completed_at"] is None
    assert node["reopened_reason"] == "closed by mistake"
    assert node["reopened_at"]


def test_the_status_recomputes_off_the_cleared_completion(tmp_path):
    """Clearing completed_at IS the status change; assert the POSITIVE status
    (`idea`, a plan-less node's underlying state), never `!= "done"`."""
    root = sandbox(tmp_path, _node("ab-11111111"))
    door(root, ["reopen", "ab-11111111", "--reason", "wrong"])
    assert _read(root)["ab-11111111"]["status"] == "idea"


def test_a_reopened_pr_bearing_node_reads_in_review(tmp_path):
    """Not a dispatchable state, and deliberately so: the node has a PR, and
    reopening records that its close was wrong, not that a worker should go."""
    root = sandbox(tmp_path, _node("ab-11111111", pr_number=7, pr_url="https://github.com/o/r/pull/7"))
    write_pr_stub(root, {7: "OPEN"})
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "early close"], path_prepend=str(root / "stubbin"))
    node = _read(root)["ab-11111111"]
    assert node["completed_at"] is None
    assert node["status"] == "in_review"
    assert node["pr_number"] == 7


# -- what it deliberately does not undo --


def test_merge_status_survives_a_reopen(tmp_path):
    """It records that GitHub confirmed a merge, which stays true after a
    reopen. Clearing it would erase a fact to express an opinion."""
    root = sandbox(tmp_path, _node("ab-11111111", merge_status="merged", pr_number=7, pr_url="https://github.com/o/r/pull/7"))
    write_pr_stub(root, {7: "MERGED"})
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "wrong", "--force"], path_prepend=str(root / "stubbin"))
    assert code == 0, err
    assert _read(root)["ab-11111111"]["merge_status"] == "merged"


def test_recorded_cost_survives_a_reopen(tmp_path):
    root = sandbox(tmp_path, _node("ab-11111111", cost_usd=4.25))
    door(root, ["reopen", "ab-11111111", "--reason", "wrong"])
    assert _read(root)["ab-11111111"]["cost_usd"] == 4.25


def test_reopen_does_not_resurrect_a_claim(tmp_path):
    """A holder with no lockfile behind it is worse than no holder."""
    root = sandbox(tmp_path, _node("ab-11111111"))
    door(root, ["reopen", "ab-11111111", "--reason", "wrong"])
    node = _read(root)["ab-11111111"]
    assert not node.get("locked_by")
    assert not node.get("claimed_at")


def test_completion_note_is_cleared_not_overwritten(tmp_path):
    """Load-bearing: the close cascade only writes its auto-closed note when
    this field is empty; leftover reopen prose would make an epic permanently
    unrecognizable as cascade-closed."""
    root = sandbox(
        tmp_path,
        _node("ab-11111111", completion_note="auto-closed: all children complete"),
    )
    door(root, ["reopen", "ab-11111111", "--reason", "wrong"])
    assert _read(root)["ab-11111111"]["completion_note"] is None


# -- refusals --


def test_a_merged_pr_refuses_the_reopen(tmp_path):
    """done's gate, inverted: it refuses when nothing merged, this when
    something did."""
    root = sandbox(tmp_path, _node("ab-11111111", pr_number=7, pr_url="https://github.com/o/r/pull/7"))
    write_pr_stub(root, {7: "MERGED"})
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "changed my mind"], path_prepend=str(root / "stubbin"))
    assert code == 3
    assert _read(root)["ab-11111111"]["completed_at"] is not None


def test_the_merged_refusal_names_the_remedy(tmp_path):
    root = sandbox(tmp_path, _node("ab-11111111", pr_number=7, pr_url="https://github.com/o/r/pull/7"))
    write_pr_stub(root, {7: "MERGED"})
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "changed my mind"], path_prepend=str(root / "stubbin"))
    assert "fno backlog idea" in err
    assert "--force" in err


def test_force_overrides_the_merged_refusal(tmp_path):
    root = sandbox(tmp_path, _node("ab-11111111", pr_number=7, pr_url="https://github.com/o/r/pull/7"))
    write_pr_stub(root, {7: "MERGED"})
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "closed the wrong node", "-F"], path_prepend=str(root / "stubbin"))
    assert code == 0, err
    assert _read(root)["ab-11111111"]["completed_at"] is None


def test_a_merged_additional_pr_refuses_even_when_the_primary_is_not(tmp_path):
    """The gate has to see every ref, the way done's does: a node can close on
    a merged additional_prs entry while its primary sits closed and unmerged."""
    root = sandbox(
        tmp_path,
        _node(
            "ab-11111111",
            pr_number=41,
            pr_url="https://github.com/o/r/pull/41",
            additional_prs=[{"number": 42, "url": "https://github.com/o/r/pull/42"}],
        ),
    )
    write_pr_stub(root, {41: "CLOSED", 42: "MERGED"})
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "x"], path_prepend=str(root / "stubbin"))
    assert code == 3, err
    assert _read(root)["ab-11111111"]["completed_at"] is not None


def test_a_forced_reopen_names_the_pr_that_produced_the_state(tmp_path):
    """The forced-reopen warning has to name the ref that actually merged,
    which is not necessarily the primary."""
    root = sandbox(
        tmp_path,
        _node(
            "ab-11111111",
            pr_number=41,
            pr_url="https://github.com/o/r/pull/41",
            additional_prs=[{"number": 42, "url": "https://github.com/o/r/pull/42"}],
        ),
    )
    write_pr_stub(root, {41: "CLOSED", 42: "MERGED"})
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "x", "-F"], path_prepend=str(root / "stubbin"))
    assert code == 0, err
    assert "pull/42" in err


def test_an_open_pr_does_not_block_a_reopen(tmp_path):
    """Only a MERGED PR is evidence the work shipped."""
    root = sandbox(tmp_path, _node("ab-11111111", pr_number=7, pr_url="https://github.com/o/r/pull/7"))
    write_pr_stub(root, {7: "OPEN"})
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "early close"], path_prepend=str(root / "stubbin"))
    assert code == 0, err


def test_a_gh_outage_leaves_the_node_done(tmp_path):
    """An unreachable gh is a missing answer, not a permitting one (exit 4,
    the retryable-outage slot)."""
    root = sandbox(tmp_path, _node("ab-11111111", pr_number=7, pr_url="https://github.com/o/r/pull/7"))
    write_pr_stub(root, {7: "OPEN"}, fail_stderr="gh: network unreachable")
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "x"], path_prepend=str(root / "stubbin"))
    assert code == 4
    assert _read(root)["ab-11111111"]["completed_at"] is not None


def test_a_routing_refusal_does_not_reopen_the_node(tmp_path):
    root = sandbox(
        tmp_path,
        _node("ab-route001", pr_number=1140, pr_url="https://github.com/o/r/pull/1140"),
    )
    write_pr_stub(
        root,
        {1140: "OPEN"},
        fail_stderr="[fno GraphQL reserve] use `fno do pr info 1140`; unconditional route refusal",
    )
    code, out, err = door(root, ["reopen", "ab-route001", "--reason", "x"], path_prepend=str(root / "stubbin"))
    assert code == 3
    assert "fno do pr info 1140 --repo o/r" in err
    assert "retryable once gh is available again" not in err
    assert _read(root)["ab-route001"]["completed_at"] is not None


def test_a_node_that_is_not_done_warns_and_changes_nothing(tmp_path):
    """Idempotent in the safe direction, matching unsupersede's shape."""
    root = sandbox(tmp_path, _node("ab-11111111", completed_at=None, status="ready"))
    before = read_graph_strict(root / "graph.json")
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "x"])
    assert code == 0
    assert "not done" in err
    assert read_graph_strict(root / "graph.json") == before


def test_a_blank_reason_is_a_usage_error(tmp_path):
    root = sandbox(tmp_path, _node("ab-11111111"))
    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "   "])
    assert code == 2
    assert _read(root)["ab-11111111"]["completed_at"] is not None


def test_an_unknown_node_is_not_found(tmp_path):
    root = sandbox(tmp_path)
    code, out, err = door(root, ["reopen", "ab-99999999", "--reason", "x"])
    assert code == 1


def test_an_archived_node_is_named_as_archived_not_missing(tmp_path):
    """The absence has two explanations; the refusal has to pick the true one."""
    root = sandbox(tmp_path)
    (root / "graph-archive.json").write_text(json.dumps({"entries": [_node("ab-22222222")]}))
    code, out, err = door(root, ["reopen", "ab-22222222", "--reason", "x"])
    assert code == 4
    assert "archived" in err
    assert "unarchive" in err


def test_the_archived_refusal_survives_a_partial_id(tmp_path):
    """An exact compare here recreates the ambiguity the refusal exists to
    remove: `reopen ab-2222` must not report "not found" for a node sitting
    readable in the archive."""
    root = sandbox(tmp_path)
    (root / "graph-archive.json").write_text(json.dumps({"entries": [_node("ab-22222222")]}))
    code, out, err = door(root, ["reopen", "ab-2222", "--reason", "x"])
    assert code == 4, err
    assert "archived" in err


# -- the cascade --


def test_an_auto_closed_epic_reopens_with_its_child(tmp_path):
    """An epic is done exactly when its children are; one of them is open again."""
    root = sandbox(
        tmp_path,
        _node("ab-e0000000", type="epic", completion_note="auto-closed: all children complete"),
        _node("ab-c0000000", parent="ab-e0000000"),
    )
    code, out, err = door(root, ["reopen", "ab-c0000000", "--reason", "wrong"])
    assert code == 0, err
    nodes = _read(root)
    assert nodes["ab-c0000000"]["completed_at"] is None
    assert nodes["ab-e0000000"]["completed_at"] is None
    assert "ab-e0000000" in out


def test_the_cascade_still_fires_for_a_partial_id(tmp_path):
    """The partial id resolves to the full id; the cascade walks full ids.
    Passing the argument straight through left an auto-closed epic done over
    a live child - a silent wrong answer, not an error."""
    root = sandbox(
        tmp_path,
        _node("ab-e0000000", type="epic", completion_note="auto-closed: all children complete"),
        _node("ab-c0000000", parent="ab-e0000000"),
    )
    code, out, err = door(root, ["reopen", "ab-c000", "--reason", "wrong"])
    assert code == 0, err
    nodes = _read(root)
    assert nodes["ab-c0000000"]["completed_at"] is None
    assert nodes["ab-e0000000"]["completed_at"] is None


def test_an_epic_closed_on_its_own_evidence_is_left_done_and_named(tmp_path):
    """Silently reopening it would discard a judgment this verb never made."""
    root = sandbox(
        tmp_path,
        _node("ab-e0000000", type="epic", completion_note="closed by operator after review"),
        _node("ab-c0000000", parent="ab-e0000000"),
    )
    code, out, err = door(root, ["reopen", "ab-c0000000", "--reason", "wrong"])
    assert code == 0, err
    nodes = _read(root)
    assert nodes["ab-c0000000"]["completed_at"] is None
    assert nodes["ab-e0000000"]["completed_at"] is not None
    assert "ab-e0000000" in out
    assert "own evidence" in out


def test_the_reopen_warning_stamps_a_marker_on_the_evidence_closed_parent(tmp_path):
    """The stderr warning is gone the moment its terminal closes; the marker
    survives it, naming the child and when."""
    root = sandbox(
        tmp_path,
        _node("ab-e0000000", type="epic", completion_note="closed by operator after review"),
        _node("ab-c0000000", parent="ab-e0000000"),
    )
    code, out, err = door(root, ["reopen", "ab-c0000000", "--reason", "wrong"])
    assert code == 0, err
    epic = _read(root)["ab-e0000000"]
    assert epic["reopen_warning"] == {"child": "ab-c0000000", "at": epic["reopen_warning"]["at"]}
    assert epic["reopen_warning"]["at"]


def test_the_marker_clears_when_the_parent_is_reopened_directly(tmp_path):
    root = sandbox(
        tmp_path,
        _node(
            "ab-e0000000", type="epic", completion_note="closed by operator after review",
            reopen_warning={"child": "ab-c0000000", "at": "2026-08-01T00:00:00+00:00"},
        ),
        _node("ab-c0000000", parent="ab-e0000000", completed_at=None, status="in_progress"),
    )
    code, out, err = door(root, ["reopen", "ab-e0000000", "--reason", "correcting"])
    assert code == 0, err
    epic = _read(root)["ab-e0000000"]
    assert epic["completed_at"] is None
    assert "reopen_warning" not in epic


def test_the_marker_clears_when_the_reopened_child_closes_again(tmp_path):
    """The regression companion to the completion_note re-annotate test: the
    marker must not outlive the state it describes. Drives the shared close
    helpers directly - they stay wheel-level, owned by the reconcile sweep."""
    from fno.graph.cli import _apply_completion_fields, _cascade_close_parents

    root = sandbox(
        tmp_path,
        _node(
            "ab-e0000000", type="epic", completion_note="closed by operator after review",
            reopen_warning={"child": "ab-c0000000", "at": "2026-08-01T00:00:00+00:00"},
        ),
        _node("ab-c0000000", parent="ab-e0000000", completed_at=None, status="in_progress"),
    )
    live = list(read_graph_strict(root / "graph.json"))
    child = next(e for e in live if e["id"] == "ab-c0000000")
    _apply_completion_fields(child)
    _cascade_close_parents(live, "ab-c0000000")
    epic = next(e for e in live if e["id"] == "ab-e0000000")
    assert "reopen_warning" not in epic
    # The parent was closed on its own evidence, so re-closing its child must
    # not also re-close the parent - only the marker clears.
    assert epic["completed_at"] is not None


def test_an_already_open_child_reopen_writes_no_marker(tmp_path):
    """A no-op reopen (the node is not done) never reaches the cascade, so no
    marker is written."""
    root = sandbox(
        tmp_path,
        _node("ab-e0000000", type="epic", completion_note="closed by operator after review"),
        _node("ab-c0000000", parent="ab-e0000000", completed_at=None, status="in_progress"),
    )
    code, out, err = door(root, ["reopen", "ab-c0000000", "--reason", "wrong"])
    assert code == 0, err
    assert "nothing to reopen" in err
    epic = _read(root)["ab-e0000000"]
    assert "reopen_warning" not in epic


def test_the_cascade_climbs_more_than_one_level(tmp_path):
    auto = "auto-closed: all children complete"
    root = sandbox(
        tmp_path,
        _node("ab-e1000000", type="epic", completion_note=auto),
        _node("ab-e2000000", type="epic", parent="ab-e1000000", completion_note=auto),
        _node("ab-c0000000", parent="ab-e2000000"),
    )
    door(root, ["reopen", "ab-c0000000", "--reason", "wrong"])
    nodes = _read(root)
    assert nodes["ab-e1000000"]["completed_at"] is None
    assert nodes["ab-e2000000"]["completed_at"] is None


def test_a_close_reopen_close_cycle_re_annotates_the_epic(tmp_path):
    """The regression the completion_note clear exists to prevent.

    If reopen left prose in completion_note, the close cascade would skip its
    `auto-closed:` write on the second close (it only writes when the field is
    empty), and a second reopen would leave the epic done under a live child.
    """
    from fno.graph.cli import _apply_completion_fields, _cascade_close_parents

    root = sandbox(
        tmp_path,
        _node("ab-e0000000", type="epic", completion_note="auto-closed: all children complete"),
        _node("ab-c0000000", parent="ab-e0000000"),
    )
    code, out, err = door(root, ["reopen", "ab-c0000000", "--reason", "wrong"])
    assert code == 0, err

    # Re-close the child the way `done` does, then let the cascade run.
    live = list(read_graph_strict(root / "graph.json"))
    child = next(e for e in live if e["id"] == "ab-c0000000")
    _apply_completion_fields(child)
    _cascade_close_parents(live, "ab-c0000000")
    epic = next(e for e in live if e["id"] == "ab-e0000000")
    assert epic["completed_at"] is not None
    assert str(epic["completion_note"]).startswith("auto-closed:")


# -- the plan doc, projected for real --


def test_the_plan_doc_comes_off_terminal_done(tmp_path):
    """The P1 the stubbed fixture hid: clearing completed_at in the graph
    while the plan stays stamped `done` leaves dispatch refusing the node.
    The projector is forward-only, so reopen forces the plan off terminal the
    way unsupersede does."""
    root = sandbox(tmp_path, _node("ab-11111111"))
    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: done\ndone_at: 2026-08-01T00:00:00Z\n---\n\n# a plan\n")
    _write(root, _node("ab-11111111", plan_path=str(plan)))

    code, out, err = door(root, ["reopen", "ab-11111111", "--reason", "wrong"])
    assert code == 0, err
    assert "status: done" not in plan.read_text()


def test_an_untouched_plan_stays_put_when_the_node_was_not_done(tmp_path):
    """Positive control: the forced write happens on the reopen, not on every
    call."""
    root = sandbox(tmp_path, _node("ab-11111111", completed_at=None))
    plan = tmp_path / "plan.md"
    original = "---\nstatus: done\ndone_at: 2026-08-01T00:00:00Z\n---\n\n# a plan\n"
    plan.write_text(original)
    _write(root, _node("ab-11111111", completed_at=None, plan_path=str(plan)))

    door(root, ["reopen", "ab-11111111", "--reason", "x"])
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
    """Positive control: the schema really does validate, so the test above
    means something. A reopen with no reason is a state change nobody can
    account for."""
    from fno.events import _build

    with pytest.raises(Exception):
        _build("backlog_reopened", "backlog", {"node_id": "ab-11111111"})


# -- update projects difficulty (restored from the x-dd1f branch; main moved
# its own difficulty coverage to test_graph_status.py) --


@pytest.mark.real_plan_projection
def test_update_difficulty_reaches_the_plan_doc(tmp_path):
    """`--difficulty` is a mirrored key, so the edit must trigger the projection
    like `--priority` does; before this it changed the graph and left the doc."""
    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: ready\ncreated: 2026-05-05\n---\n\n# a plan\n")
    root = sandbox(tmp_path, _node("ab-11111111", status="ready", completed_at=None, plan_path=str(plan)))

    code, out = _native_update(root, "ab-11111111", "--difficulty", "high")
    assert code == 0, out
    assert "difficulty: high" in plan.read_text()


@pytest.mark.real_plan_projection
def test_update_difficulty_null_clears_the_plan_doc(tmp_path):
    """The explicit clear reaches the doc even on a row that never held the key."""
    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: ready\ncreated: 2026-05-05\ndifficulty: high\n---\n\n# a plan\n")
    root = sandbox(tmp_path, _node("ab-11111111", status="ready", completed_at=None, plan_path=str(plan)))

    code, out = _native_update(root, "ab-11111111", "--difficulty", "null")
    assert code == 0, out
    assert "difficulty" not in plan.read_text()


@pytest.mark.real_plan_projection
def test_update_priority_keeps_a_persisted_null_band_off_the_doc(tmp_path):
    """Rows minted before the intake fix still store difficulty: null; a
    mirrored edit on them must not delete the band the doc authored since."""
    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: ready\ncreated: 2026-05-05\ndifficulty: medium\n---\n\n# a plan\n")
    root = sandbox(tmp_path, _node("ab-11111111", status="ready", completed_at=None, difficulty=None, plan_path=str(plan)))

    code, out = _native_update(root, "ab-11111111", "--priority", "p1")
    assert code == 0, out
    assert "difficulty: medium" in plan.read_text()


def _native_update(root: Path, *args: str):
    """The update leaf answers natively; drive the dev binary over the sandbox
    the fixture seeded (in-process monkeypatches cannot reach a subprocess)."""
    import os as _os
    import subprocess as _sp

    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    proc = _sp.run(
        [str(binary), "backlog", "update", *args],
        capture_output=True,
        text=True,
        timeout=60,
        env={
            "PATH": _os.environ["PATH"],
            "HOME": str(root),
            "FNO_CONFIG": str(root / "config.toml"),
            "FNO_GLOBAL_SETTINGS_PATH": "/dev/null",
            "FNO_TRACKER_BACKEND": "graph",
            "FNO_CLAIMS_ROOT": str(root / "claims"),
        },
        cwd=str(root),
    )
    return proc.returncode, proc.stdout + proc.stderr
