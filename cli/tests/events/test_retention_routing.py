"""Retention-class routing at the Python write boundary.

The schema declares a retention class per event type; the write boundary must
honor it (node x-add3). An ephemeral row must never land in the durable
journal, and a durable/gate/unset row must never be diverted. A missing
sibling file is correct for the non-ephemeral classes: the file is created by
the first ephemeral write, not eagerly.
"""
from __future__ import annotations

import json
from pathlib import Path

from fno.events import EPHEMERAL_SUFFIX, _build, append_event


def _rows(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def test_ephemeral_event_lands_in_sibling_never_main(tmp_path: Path) -> None:
    main = tmp_path / "events.jsonl"
    event = _build(
        "mux_pane_counters",
        "test",
        {"session": "s1", "panes": []},
    )
    append_event(event, events_path=main)

    assert _rows(main) == [], "ephemeral row reached the durable journal"
    sibling = tmp_path / ("events.jsonl" + EPHEMERAL_SUFFIX)
    rows = _rows(sibling)
    assert [r["type"] for r in rows] == ["mux_pane_counters"], (
        "the gauge must be provably alive in the sibling, not merely absent "
        "from the main journal"
    )


def test_durable_event_lands_in_main_no_sibling(tmp_path: Path) -> None:
    main = tmp_path / "events.jsonl"
    event = _build(
        "operator_decision",
        "test",
        {"decision_id": "d-1", "decision": "kept durable"},
    )
    append_event(event, events_path=main)

    assert [r["type"] for r in _rows(main)] == ["operator_decision"]
    assert not (tmp_path / ("events.jsonl" + EPHEMERAL_SUFFIX)).exists()


def test_unset_and_gate_classes_stay_in_main(tmp_path: Path) -> None:
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
            "test",
            {"pr": 1, "coverage": "covered", "verdicts": [], "head_sha": "abc"},
        ),
        events_path=main,
    )

    types = [r["type"] for r in _rows(main)]
    assert types == ["control_plane_tick", "review_coverage"]
    assert not (tmp_path / ("events.jsonl" + EPHEMERAL_SUFFIX)).exists()


def test_ephemeral_suffix_is_the_declared_sibling_name() -> None:
    assert EPHEMERAL_SUFFIX == ".ephemeral"


def test_symlinked_journal_routes_to_the_resolved_space_sibling(tmp_path: Path) -> None:
    """A worktree journal symlinked into the repo space must route its
    ephemeral rows to that space's sibling, not a worktree-local file the
    symlink never covered."""
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

    assert _rows(main_space) == []
    assert not linked.with_name(linked.name + EPHEMERAL_SUFFIX).exists()
    rows = _rows(space / ("events.jsonl" + EPHEMERAL_SUFFIX))
    assert [r["type"] for r in rows] == ["human_touch"]
