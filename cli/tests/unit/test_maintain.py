"""Unit tests for the maintain legs (ab-9c144a4c).

Pure-function coverage of the six-leg sweep's detectors. The CLI-level
orchestration (apply under one lock, claimed-skip, health-history) is covered in
tests/integration/test_maintain_cli.py.

Filter: `python -m pytest tests/ -k maintain`
"""
from __future__ import annotations

from datetime import datetime, timedelta, timezone

from fno.graph import maintain as m


WS = {
    "fno": "/home/u/code/abilities",
    "etl": "/home/u/code/etl",
}


def _n(node_id: str, **over) -> dict:
    base = {"id": node_id, "title": node_id, "project": None, "cwd": None, "_status": "ready"}
    base.update(over)
    return base


# --- leg 1: re-scope -------------------------------------------------------


def test_rescope_project_null_cwd_maps_to_project():
    fixes = m.detect_rescope_fixes(
        [_n("ab-1", project=None, cwd="/home/u/code/abilities")], WS
    )
    assert len(fixes) == 1
    assert fixes[0].new_project == "fno"
    assert fixes[0].new_cwd == "/home/u/code/abilities"


def test_rescope_worktree_cwd_with_correct_project_fixes_cwd():
    fixes = m.detect_rescope_fixes(
        [_n("ab-2", project="fno", cwd="/home/u/conductor/workspaces/abilities/foo")],
        WS,
    )
    assert len(fixes) == 1
    assert fixes[0].new_project == "fno"
    assert fixes[0].new_cwd == "/home/u/code/abilities"


def test_rescope_unknown_project_name_cwd_maps_elsewhere():
    fixes = m.detect_rescope_fixes(
        [_n("ab-3", project="bogus", cwd="/home/u/code/etl")], WS
    )
    assert len(fixes) == 1
    assert fixes[0].new_project == "etl"


def test_rescope_project_null_conductor_worktree_repo_hint():
    fixes = m.detect_rescope_fixes(
        [_n("ab-4", project=None, cwd="/x/conductor/workspaces/etl/bar")], WS
    )
    assert len(fixes) == 1
    assert fixes[0].new_project == "etl"
    assert fixes[0].new_cwd == "/home/u/code/etl"


def test_rescope_project_null_harness_native_worktree_repo_hint():
    """Harness-native worktree layout <repo>/.claude/worktrees/<name> (x-33e9):
    the segment before .claude/worktrees/ is the repo hint."""
    fixes = m.detect_rescope_fixes(
        [_n("ab-4b", project=None, cwd="/home/u/code/etl/.claude/worktrees/bar")], WS
    )
    assert len(fixes) == 1
    assert fixes[0].new_project == "etl"
    assert fixes[0].new_cwd == "/home/u/code/etl"


def test_rescope_correct_node_is_noop():
    fixes = m.detect_rescope_fixes(
        [_n("ab-5", project="fno", cwd="/home/u/code/abilities")], WS
    )
    assert fixes == []


def test_rescope_unmappable_cwd_left_alone():
    fixes = m.detect_rescope_fixes(
        [_n("ab-6", project=None, cwd="/somewhere/unmapped")], WS
    )
    assert fixes == []


def test_rescope_empty_workspaces_yields_nothing():
    assert m.detect_rescope_fixes([_n("ab-7", project=None, cwd="/home/u/code/etl")], {}) == []


# --- leg 2: leak-prune -----------------------------------------------------


def test_is_temp_cwd_variants():
    # Carries a pytest/test marker -> a real leak.
    assert m.is_temp_cwd("/tmp/pytest-of-bob/pytest-3/x")
    assert m.is_temp_cwd("/private/var/folders/aa/bb/T/fno-test-home-xyz")
    assert m.is_temp_cwd("/Users/u/x/pytest-of-u/pytest-12")
    # Not a leak: a real cwd.
    assert not m.is_temp_cwd("/home/u/code/abilities")
    assert not m.is_temp_cwd(None)
    assert not m.is_temp_cwd("")
    # A legitimate checkout / scratch worktree under a bare temp ROOT (no
    # pytest marker) must NOT be pruned (codex P2 on PR #474): matching the
    # whole /tmp or /var/folders prefix would delete a real node.
    assert not m.is_temp_cwd("/tmp/my-scratch-project")
    assert not m.is_temp_cwd("/var/folders/aa/bb/T/some-worktree")


def test_detect_temp_leaks():
    entries = [
        _n("ab-good", cwd="/home/u/code/abilities"),
        _n("ab-leak", cwd="/tmp/pytest-of-x/pytest-1/proj"),
    ]
    assert m.detect_temp_leaks(entries) == ["ab-leak"]


# --- leg 3: dedup ----------------------------------------------------------


def test_detect_dup_groups_idea_only():
    entries = [
        _n("ab-a", title="Fix the thing", _status="idea"),
        _n("ab-b", title="fix the  thing!", _status="idea"),  # normalizes same
        _n("ab-c", title="Fix the thing", _status="ready"),   # ready -> ignored
        _n("ab-d", title="Unrelated", _status="idea"),
    ]
    groups = m.detect_dup_groups(entries)
    assert len(groups) == 1
    assert set(groups[0]) == {"ab-a", "ab-b"}


# --- leg 4: drain stale ----------------------------------------------------


def test_detect_stale_ideas_strictly_older_than():
    now = datetime(2026, 6, 8, tzinfo=timezone.utc)
    old = (now - timedelta(days=40)).isoformat()
    exactly = (now - timedelta(days=30)).isoformat()
    fresh = (now - timedelta(days=5)).isoformat()
    entries = [
        _n("ab-old", _status="idea", created_at=old),
        _n("ab-edge", _status="idea", created_at=exactly),  # exactly N -> NOT stale
        _n("ab-fresh", _status="idea", created_at=fresh),
        _n("ab-ready", _status="ready", created_at=old),    # not an idea
        _n("ab-nots", _status="idea"),                       # no created_at
    ]
    stale = m.detect_stale_ideas(entries, 30, now=now)
    assert [s.node_id for s in stale] == ["ab-old"]
    assert stale[0].age_days == 40


# --- leg 5: cap Now --------------------------------------------------------


def test_now_overflow():
    entries = [_n(f"ab-{i}", col="Now") for i in range(3)] + [_n("ab-x", col="Next")]

    def col(e):
        return e.get("col")

    assert m.now_overflow(entries, 2, col) == (3, 2)
    assert m.now_overflow(entries, 3, col) is None


# --- config block ----------------------------------------------------------


def test_maintain_config_default_staleness():
    from fno.config import ConfigBlock

    assert ConfigBlock().backlog.maintain.staleness_days == 30


def test_maintain_config_custom_staleness():
    from fno.config import BacklogBlock

    assert BacklogBlock(maintain={"staleness_days": 7}).maintain.staleness_days == 7


def test_maintain_config_rejects_non_positive_staleness():
    import pytest
    from pydantic import ValidationError

    from fno.config import MaintainBlock

    with pytest.raises(ValidationError):
        MaintainBlock(staleness_days=0)


def test_maintain_config_default_max_failed_attempts():
    from fno.config import ConfigBlock

    assert ConfigBlock().backlog.maintain.max_failed_attempts == 3


def test_maintain_config_custom_max_failed_attempts():
    from fno.config import BacklogBlock

    assert (
        BacklogBlock(maintain={"max_failed_attempts": 5}).maintain.max_failed_attempts
        == 5
    )


def test_maintain_config_rejects_non_positive_max_failed_attempts():
    import pytest
    from pydantic import ValidationError

    from fno.config import MaintainBlock

    with pytest.raises(ValidationError):
        MaintainBlock(max_failed_attempts=0)


# --- failure-streak helper (ab-5b7cf63a / #34, task 1.2) -------------------

from fno.graph import failure as f  # noqa: E402


def _fail(nid: str) -> dict:
    return {"type": "node_failed", "data": {"unit_id": nid}}


def _parked(nid: str) -> dict:
    return {"type": "node_closed", "data": {"unit_id": nid, "close": "parked"}}


def _closed(nid: str) -> dict:
    return {"type": "node_closed", "data": {"unit_id": nid, "close": "closed"}}


def _refused(nid: str) -> dict:
    return {"type": "node_closed", "data": {"unit_id": nid, "close": "refused"}}


def _undefer(nid: str) -> dict:
    # Flat agents-emitter envelope (fno.agents.events.emit shape).
    return {"kind": "node_undeferred", "unit_id": nid}


def test_streak_zero_with_no_events():
    assert f.consecutive_failures("ab-x", []) == 0


def test_streak_counts_consecutive_failures():
    events = [_fail("ab-x"), _fail("ab-x"), _fail("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 3


def test_streak_parked_close_counts_as_failure():
    events = [_fail("ab-x"), _parked("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 2


def test_streak_only_counts_target_node():
    events = [_fail("ab-x"), _fail("ab-y"), _fail("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 2


def test_streak_reset_on_success_close():
    # AC4-EDGE: two failures then a success ship -> streak 0.
    events = [_fail("ab-x"), _fail("ab-x"), _closed("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 0
    # ... and a later failure after the success counts from the boundary.
    events2 = events + [_fail("ab-x")]
    assert f.consecutive_failures("ab-x", events2) == 1


def test_streak_reset_on_undefer_event():
    # AC5-FR: undefer is a reset boundary; one fresh failure after it -> 1.
    events = [_fail("ab-x"), _fail("ab-x"), _fail("ab-x"), _undefer("ab-x"), _fail("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 1


def test_streak_refused_close_ignored():
    # A dispatch refusal neither counts nor resets the streak.
    events = [_fail("ab-x"), _refused("ab-x"), _fail("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 2


def test_streak_malformed_event_skipped(tmp_path):
    # AC2-ERR: a truncated/non-JSON line is skipped, valid lines still read.
    log = tmp_path / "events.jsonl"
    log.write_text(
        '{"type":"node_failed","data":{"unit_id":"ab-x"}}\n'
        '{"type":"node_failed","data":{"unit_id":"ab-x"\n'  # truncated
        "not json at all\n"
        '{"type":"node_failed","data":{"unit_id":"ab-x"}}\n'
    )
    events = f.read_events(log)
    assert len(events) == 2  # two well-formed lines survive
    assert f.consecutive_failures("ab-x", events) == 2


def test_read_events_absent_file_is_empty(tmp_path):
    assert f.read_events(tmp_path / "nope.jsonl") == []


def test_stranded_dependents_maps_auto_failure_deferred():
    entries = [
        _n("ab-block", _status="deferred", deferred_at="t",
           deferred_reason="auto-failure: 3 consecutive failed attempts"),
        _n("ab-dep1", _status="blocked", blocked_by=["ab-block"]),
        _n("ab-dep2", _status="blocked", blocked_by=["ab-block"]),
    ]
    stranded = f.stranded_dependents(entries)
    assert set(stranded["ab-block"]) == {"ab-dep1", "ab-dep2"}


def test_stranded_dependents_ignores_manual_defer():
    # A hand-deferred blocker (no auto-failure sentinel) is NOT strand-reported.
    entries = [
        _n("ab-block", _status="deferred", deferred_at="t",
           deferred_reason="parked by hand"),
        _n("ab-dep1", _status="blocked", blocked_by=["ab-block"]),
    ]
    assert f.stranded_dependents(entries) == {}


def test_stranded_dependents_omits_blocker_with_no_dependents():
    entries = [
        _n("ab-block", _status="deferred", deferred_at="t",
           deferred_reason="auto-failure: 4 consecutive failed attempts"),
    ]
    assert f.stranded_dependents(entries) == {}


# --- auto-defer detector (maintain.py, task 2.1) ---------------------------


def test_detect_failure_defers_threshold_boundary():
    # N-1 must not trigger; >= N must (Boundaries).
    events = [_fail("ab-x"), _fail("ab-x")]
    node = _n("ab-x")  # _status ready
    assert m.detect_failure_defers([node], events, 3) == []
    cands = m.detect_failure_defers([node], events + [_fail("ab-x")], 3)
    assert [(c.node_id, c.streak) for c in cands] == [("ab-x", 3)]


def test_detect_failure_defers_skips_non_ready_and_deferred():
    events = [_fail("ab-x")] * 5
    assert m.detect_failure_defers([_n("ab-x", _status="idea")], events, 3) == []
    assert (
        m.detect_failure_defers(
            [_n("ab-x", _status="deferred", deferred_at="t")], events, 3
        )
        == []
    )


def test_detect_failure_defers_event_for_absent_node_noops():
    events = [_fail("ab-ghost")] * 5
    assert m.detect_failure_defers([_n("ab-real")], events, 3) == []
