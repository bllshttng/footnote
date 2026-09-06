"""Coverage for the king board's surviving Python surfaces (x-25b8)."""

from __future__ import annotations

import pytest

from fno.king.scope import compile_scope_ids, scope_undelivered


def test_epic_scope_compiles_to_the_root_and_descendants():
    entries = [
        {"id": "x-root", "type": "epic", "project": "fno"},
        {"id": "x-child", "parent": "x-root", "project": "fno"},
        {"id": "x-grand", "parent": "x-child", "project": "fno"},
        {"id": "x-other", "project": "fno"},
    ]
    ids = compile_scope_ids("x-root", entries, resolve=lambda _: (2, "x-root"))
    assert ids == {"x-root", "x-child", "x-grand"}


def test_project_scope_compiles_to_the_project_union():
    entries = [
        {"id": "f-1", "project": "fdata"},
        {"id": "e-1", "project": "etl"},
    ]
    assert compile_scope_ids("fdata", entries, resolve=lambda _: (1, "fdata")) == {"f-1"}
    assert compile_scope_ids(
        "fdata,etl", entries, resolve=lambda _: (0, "fdata,etl")
    ) == {"f-1", "e-1"}


def test_an_epic_set_compiles_to_the_union_of_both_roots():
    """A rung-2 crown over two epics is a set: the king's board must see the
    nodes under EITHER member, not just the ones under the first."""
    entries = [
        {"id": "x-a", "type": "epic", "project": "fno"},
        {"id": "x-a1", "parent": "x-a", "project": "fno"},
        {"id": "x-b", "type": "epic", "project": "fno"},
        {"id": "x-b1", "parent": "x-b", "project": "fno"},
        {"id": "x-o", "project": "fno"},
    ]
    ids = compile_scope_ids(
        "x-a,x-b", entries, resolve=lambda _: (2, "x-a,x-b")
    )
    assert ids == {"x-a", "x-a1", "x-b", "x-b1"}


def test_every_member_of_an_epic_set_must_be_an_epic():
    entries = [
        {"id": "x-a", "type": "epic", "project": "fno"},
        {"id": "x-n", "type": "feature", "project": "fno"},
    ]
    with pytest.raises(ValueError):
        compile_scope_ids("x-a,x-n", entries, resolve=lambda _: (2, "x-a,x-n"))


def test_a_non_epic_root_is_refused():
    entries = [{"id": "x-root", "type": "feature"}]
    with pytest.raises(ValueError):
        compile_scope_ids("x-root", entries, resolve=lambda _: (2, "x-root"))


# --- the drain read (the reign goal's own count) -------------------------------


def test_scope_undelivered_counts_only_nodes_not_done_or_superseded():
    entries = [
        {"id": "x-root", "type": "epic"},
        {
            "id": "x-done",
            "parent": "x-root",
            "status": "done",
            "completed_at": "2026-09-06T00:00:00Z",
        },
        {"id": "x-sup", "parent": "x-root", "superseded_by": "x-done"},
        {"id": "x-driven", "parent": "x-root", "status": "in_review", "pr": 1497},
        {"id": "x-blocked", "parent": "x-root", "status": "blocked"},
        {"id": "x-deferred", "parent": "x-root", "status": "deferred"},
        {"id": "x-outside", "status": "in_progress"},
    ]
    assert scope_undelivered("x-root", entries, resolver=lambda _: (2, "x-root")) == 4


def test_a_fully_driven_scope_still_counts_as_undelivered():
    # The 2026-09-06 incident shape: every row had a driver, the actionable
    # board read empty, nothing had shipped. Assignment is not delivery.
    entries = [
        {"id": "x-root", "type": "epic"},
        {"id": "x-a", "parent": "x-root", "status": "in_progress", "pr": 1497},
    ]
    assert (
        scope_undelivered("x-root", entries, resolver=lambda _: (2, "x-root")) == 2
    )


def test_scope_undelivered_raises_on_an_uncompilable_scope():
    # A reader that would END a reign on zero must see the failure, not read
    # it as drained; the wake caller catches this itself.
    entries = [{"id": "x-root", "type": "feature"}]
    with pytest.raises(ValueError):
        scope_undelivered("x-root", entries, resolver=lambda _: (2, "x-root"))


# --- the human board renderer ------------------------------------------------


def _queue(**overrides):
    q = {
        "name": "undispatched",
        "source": "fno backlog undispatched --json",
        "status": "ok",
        "error": "",
        "count": 4,
        "actionable": True,
        "note": "one worker per node",
        "verb": "/fno:target",
    }
    q.update(overrides)
    return q


def _render_lines(capsys, board, max_rows=25):
    from fno.king.cli import _render

    _render(board, max_rows)
    return capsys.readouterr().out.splitlines()


def test_render_elides_past_max_rows_and_says_so(capsys):
    rows = [{"id": f"x-{i}"} for i in range(4)]
    board = {"actionable": 4, "warnings": [], "queues": [_queue(rows=rows)]}
    lines = _render_lines(capsys, board, max_rows=2)
    shown = [line for line in lines if "x-" in line]
    assert len(shown) == 2
    assert any("... 2 more not shown" in line for line in lines)


def test_render_names_an_unreadable_queue_and_prints_warnings(capsys):
    from fno.king.cli import _render

    q = _queue(status="unreadable", error="graph unreadable", rows=[])
    board = {"actionable": 1, "warnings": ["stalled_holder: capped"], "queues": [q]}
    _render(board, 25)
    captured = capsys.readouterr()
    assert any("UNREADABLE" in line and "graph unreadable" in line for line in captured.out.splitlines())
    assert captured.err.strip() == "warning: stalled_holder: capped"
