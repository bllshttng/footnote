"""Retention classification at the Python write boundary.

The schema declares a retention class per event type; the store now owns the
classification (node x-add3 routed journals; x-0915 moved the boundary into
events.db). An ephemeral row lands in the store carrying `ephemeral`, and no
sibling journal is ever created: the class is metadata, not a file route.
"""
from __future__ import annotations

import json
from pathlib import Path

from fno.events import EPHEMERAL_SUFFIX, _build, append_event
from fno.events.store_client import read_committed_lines, store_db_path


def _rows(path: Path) -> list[dict]:
    return [json.loads(line) for line in read_committed_lines(path) if line]


def _classes(path: Path) -> dict[str, str]:
    import sqlite3

    db = store_db_path(path)
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        return {
            row[0]: row[1]
            for row in conn.execute("SELECT type, retention_class FROM events")
        }
    finally:
        conn.close()


def test_ephemeral_event_lands_in_the_store_as_ephemeral(tmp_path: Path) -> None:
    main = tmp_path / "events.jsonl"
    event = _build(
        "mux_pane_counters",
        "test",
        {"session": "s1", "panes": []},
    )
    append_event(event, events_path=main)

    rows = _rows(main)
    assert [r["type"] for r in rows] == ["mux_pane_counters"], (
        "the gauge is stored, not dropped"
    )
    assert _classes(main)["mux_pane_counters"] == "ephemeral"
    # The class is store metadata now: the sibling journal is never created.
    assert not (tmp_path / ("events.jsonl" + EPHEMERAL_SUFFIX)).exists()


def test_durable_event_lands_in_main_no_sibling(tmp_path: Path) -> None:
    main = tmp_path / "events.jsonl"
    event = _build(
        "operator_decision",
        "test",
        {"decision_id": "d-1", "decision": "kept durable"},
    )
    append_event(event, events_path=main)

    assert [r["type"] for r in _rows(main)] == ["operator_decision"]
    assert _classes(main)["operator_decision"] == "durable"
    assert not (tmp_path / ("events.jsonl" + EPHEMERAL_SUFFIX)).exists()


def test_unset_and_gate_classes_stay_durable_and_gate(tmp_path: Path) -> None:
    main = tmp_path / "events.jsonl"
    append_event(
        _build(
            "control_plane_tick",
            "test",
            {"arm": True, "scheduler": "test", "acted": 0, "interval_s": 30},
        ),
        events_path=main,
    )
    append_event(
        _build(
            "review_coverage",
            "hook",
            {"pr": 1, "coverage": "covered", "verdicts": [], "head_sha": "abc"},
        ),
        events_path=main,
    )

    types = [r["type"] for r in _rows(main)]
    assert types == ["control_plane_tick", "review_coverage"]
    classes = _classes(main)
    assert classes["control_plane_tick"] == "durable"
    assert classes["review_coverage"] == "gate"
    assert not (tmp_path / ("events.jsonl" + EPHEMERAL_SUFFIX)).exists()


def test_ephemeral_suffix_is_the_declared_sibling_name() -> None:
    assert EPHEMERAL_SUFFIX == ".ephemeral"


def test_symlinked_journal_resolves_to_one_space_store(tmp_path: Path) -> None:
    """A worktree journal symlinked into the repo space must commit into that
    space's store, not a worktree-local database the symlink never covered."""
    space = tmp_path / "space"
    space.mkdir()
    main_space = space / "events.jsonl"
    main_space.touch()
    worktree_dir = tmp_path / "wt" / ".fno"
    worktree_dir.mkdir(parents=True)
    linked = worktree_dir / "events.jsonl"
    linked.symlink_to(main_space)

    event = _build(
        "human_touch",
        "backlog",
        {"graph_node_id": "x-1", "source": "merge", "resolution": "ok"},
    )
    append_event(event, events_path=linked)

    rows = _rows(main_space)
    assert [r["type"] for r in rows] == ["human_touch"]
    assert (space / "events.db").exists(), "the resolved space store holds the row"
    assert not linked.with_name(linked.name + EPHEMERAL_SUFFIX).exists()
