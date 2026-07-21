"""Tests for boundary-reconcile detection (x-d0ad). tmp paths only -- never the
real Obsidian vault (``internal/`` may be absent in CI)."""
from __future__ import annotations

import os
from pathlib import Path

from fno.plan.boundary import BlockerVerdict, boundary_reconcile
from fno.plan import boundary


def _plan(p: Path, body: str, *, epoch: bool = False) -> Path:
    p.write_text(body, encoding="utf-8")
    if epoch:
        os.utime(p, (0, 0))  # 1970 -> older than any 2026 blocker
    return p


_DONE = {
    "id": "x-blk",
    "status": "done",
    "pr_number": 141,
    "completed_at": "2026-07-02T09:12:12+00:00",
}


def test_stale_when_plan_predates_blocker(tmp_path: Path) -> None:  # AC1-HP
    plan = _plan(tmp_path / "p.md", "# plan\n", epoch=True)
    node = {"id": "x-dep", "blocked_by": ["x-blk"]}
    [v] = boundary_reconcile(node, str(plan), [_DONE])
    assert v.verdict == "stale" and v.blocker_id == "x-blk" and v.pr_number == 141


def test_fresh_when_plan_newer(tmp_path: Path) -> None:  # AC2-HP
    plan = _plan(tmp_path / "p.md", "# plan\n")  # mtime = now, after 2026-07
    node = {"id": "x-dep", "blocked_by": ["x-blk"]}
    [v] = boundary_reconcile(node, str(plan), [_DONE])
    assert v.verdict == "fresh"


def test_marker_present_wins_over_mtime(tmp_path: Path) -> None:  # AC5/AC7 idempotent
    # Even with an ancient mtime, an existing landed-marker => reconciled.
    plan = _plan(tmp_path / "p.md", "# plan\n### x-blk landed (PR #141)\n", epoch=True)
    node = {"id": "x-dep", "blocked_by": ["x-blk"]}
    [v] = boundary_reconcile(node, str(plan), [_DONE])
    assert v.verdict == "reconciled"


def test_marker_matches_on_pr_number(tmp_path: Path) -> None:
    # Heading names the PR but not the id -> still reconciled (secondary key).
    plan = _plan(tmp_path / "p.md", "# plan\n### something landed (PR #141, merged)\n", epoch=True)
    node = {"id": "x-dep", "blocked_by": ["x-blk"]}
    [v] = boundary_reconcile(node, str(plan), [_DONE])
    assert v.verdict == "reconciled"


def test_independent_blockers_marker_does_not_mask_later(tmp_path: Path) -> None:
    # Marker for x-blk present, but x-late merged after and has no marker -> STALE.
    late = {"id": "x-late", "status": "done", "pr_number": 200, "completed_at": "2026-07-02T09:12:12+00:00"}
    plan = _plan(tmp_path / "p.md", "# plan\n### x-blk landed (PR #141)\n", epoch=True)
    node = {"id": "x-dep", "blocked_by": ["x-blk", "x-late"]}
    verdicts = {v.blocker_id: v.verdict for v in boundary_reconcile(node, str(plan), [_DONE, late])}
    assert verdicts == {"x-blk": "reconciled", "x-late": "stale"}


def test_marker_pr_number_no_substring_false_match() -> None:
    # A #20 heading must NOT satisfy the marker check for a blocker with PR #2.
    from fno.plan.boundary import _marker_present

    assert _marker_present("### x-other landed (PR #20)", "x-blk", 2) is False
    assert _marker_present("### z landed (PR #123)", "z-blk", 12) is False
    # exact PR match still works, even with trailing punctuation.
    assert _marker_present("### z landed (PR #12, merged)", "z-blk", 12) is True


def test_marker_id_no_substring_false_match() -> None:
    # `x-14` must NOT match a heading for `x-141` (hyphenated-id prefix).
    from fno.plan.boundary import _marker_present

    assert _marker_present("### x-141 landed (PR #9)", "x-14", None) is False
    assert _marker_present("### x-14 landed (PR #9)", "x-14", None) is True


def test_mtime_poisoned_after_any_marker_reads_stale(tmp_path: Path) -> None:
    # Reconciling x-blk (marker + mtime bumped to now) must NOT false-fresh a
    # different, un-markered blocker (x-late) that merged before the plan edit.
    late = {"id": "x-late", "status": "done", "pr_number": 200, "completed_at": "2026-06-01T00:00:00+00:00"}
    # mtime = now (NOT epoch): x-late would read fresh on mtime alone.
    plan = _plan(tmp_path / "p.md", "# plan\n### x-blk landed (PR #141)\n")
    node = {"id": "x-dep", "blocked_by": ["x-blk", "x-late"]}
    verdicts = {v.blocker_id: v.verdict for v in boundary_reconcile(node, str(plan), [_DONE, late])}
    assert verdicts == {"x-blk": "reconciled", "x-late": "stale"}


def test_open_blocker_skipped(tmp_path: Path) -> None:
    plan = _plan(tmp_path / "p.md", "# plan\n", epoch=True)
    node = {"id": "x-dep", "blocked_by": ["x-open"]}
    graph = [{"id": "x-open", "status": "ready", "pr_number": None}]
    assert boundary_reconcile(node, str(plan), graph) == []


def test_done_blocker_without_pr_skipped(tmp_path: Path) -> None:
    # A done doc/advisory node in blocked_by (no PR) is silently skipped.
    plan = _plan(tmp_path / "p.md", "# plan\n", epoch=True)
    node = {"id": "x-dep", "blocked_by": ["x-doc"]}
    graph = [{"id": "x-doc", "status": "done", "pr_number": None}]
    assert boundary_reconcile(node, str(plan), graph) == []


def test_no_plan_no_brief_returns_empty() -> None:  # boundary
    assert boundary_reconcile({"id": "x-bare"}, None, [_DONE]) == []


def test_anchor_stripped(tmp_path: Path) -> None:  # AC5-EDGE shared #group doc
    plan = _plan(tmp_path / "p.md", "# plan\n", epoch=True)
    node = {"id": "x-dep", "blocked_by": ["x-blk"]}
    [v] = boundary_reconcile(node, f"{plan}#group-g2", [_DONE])
    assert v.verdict == "stale"


def test_brief_fallback(tmp_path: Path, monkeypatch) -> None:  # AC6-EDGE
    brief = _plan(tmp_path / "x-dep.md", "# brief\n", epoch=True)
    monkeypatch.setattr(boundary, "_brief_path", lambda nid: brief)
    node = {"id": "x-dep", "has_brief": True, "blocked_by": ["x-blk"]}
    [v] = boundary_reconcile(node, None, [_DONE])
    assert v.verdict == "stale"


def test_reconcile_against_escape_hatch(tmp_path: Path) -> None:  # AC9-EDGE
    plan = _plan(
        tmp_path / "p.md",
        "---\nreconcile_against: [x-extra]\n---\n# plan\n",
        epoch=True,
    )
    node = {"id": "x-dep"}  # x-extra is NOT a blocker
    graph = [{"id": "x-extra", "status": "done", "pr_number": 99, "completed_at": "2026-07-02T00:00:00+00:00"}]
    [v] = boundary_reconcile(node, str(plan), graph)
    assert v.verdict == "stale" and v.blocker_id == "x-extra"


def test_reconcile_against_no_pr_is_unknown(tmp_path: Path) -> None:  # AC9 unknown clause
    plan = _plan(tmp_path / "p.md", "---\nreconcile_against: [x-doc]\n---\n# plan\n", epoch=True)
    graph = [{"id": "x-doc", "status": "done", "pr_number": None}]
    [v] = boundary_reconcile({"id": "x-dep"}, str(plan), graph)
    assert v.verdict == "unknown"


def test_reconcile_against_matches_slug(tmp_path: Path) -> None:  # Discretion #4
    plan = _plan(tmp_path / "p.md", "---\nreconcile_against: [my-slug]\n---\n# plan\n", epoch=True)
    graph = [{"id": "x-s", "slug": "my-slug", "status": "done", "pr_number": 5, "completed_at": "2026-07-02T00:00:00+00:00"}]
    [v] = boundary_reconcile({"id": "x-dep"}, str(plan), graph)
    assert v.verdict == "stale" and v.blocker_id == "x-s"


def test_unparseable_completed_at_is_unknown(tmp_path: Path) -> None:  # AC8-FR
    plan = _plan(tmp_path / "p.md", "# plan\n", epoch=True)
    node = {"id": "x-dep", "blocked_by": ["x-blk"]}
    graph = [{"id": "x-blk", "status": "done", "pr_number": 1, "completed_at": "not-a-date"}]
    [v] = boundary_reconcile(node, str(plan), graph)
    assert v.verdict == "unknown"


def test_detection_never_raises_on_bad_node() -> None:  # AC8-FR
    # A node that is not even a dict must not crash detection.
    v = boundary_reconcile(None, "whatever", [])  # type: ignore[arg-type]
    assert v == [] or (len(v) == 1 and v[0].verdict == "unknown")


def test_unreadable_plan_degrades_to_unknown(tmp_path: Path) -> None:  # AC8-FR
    # Path exists but is a directory -> read fails -> unknown per blocker.
    d = tmp_path / "plan_dir"
    d.mkdir()
    node = {"id": "x-dep", "blocked_by": ["x-blk"]}
    [v] = boundary_reconcile(node, str(d), [_DONE])
    assert v.verdict == "unknown"


def test_verdict_is_immutable() -> None:
    v = BlockerVerdict("x-1", "stale")
    assert v.verdict == "stale" and v.pr_number is None


def test_self_check_runs() -> None:
    boundary._self_check()
