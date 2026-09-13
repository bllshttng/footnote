"""Tests for the project-journal day-boundary adapter."""

from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest


def test_day_boundary_builder_is_available_and_validates() -> None:
    from fno import events

    assert hasattr(events, "day_boundary")
    event = events.day_boundary(
        boundary_id="day-start-20260913-ab12",
        kind="start",
        cutoff="2026-09-13T08:00:00Z",
        prior_boundary_id=None,
        featured=["q-1"],
        completed=2,
        open_count=3,
        opened=1,
        closed=0,
        retractions=1,
    )
    assert event["type"] == "day_boundary"
    assert event["data"]["boundary_id"] == "day-start-20260913-ab12"
    events.validate(event)


def test_day_adapter_module_is_registered() -> None:
    assert importlib.util.find_spec("fno.outstanding.day") is not None


def test_index_failure_names_boundary_id(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno.outstanding import day

    captured: list[tuple[dict, object]] = []

    def fake_append(event: dict, events_path=None) -> None:
        captured.append((event, events_path))
        if len(captured) == 2:
            raise OSError("index unavailable")

    monkeypatch.setattr(day, "_run_native", lambda kind: {
        "boundary_id": "day-end-20260913-ab12",
        "kind": kind,
        "reused": False,
        "cutoff": "2026-09-13T18:00:00Z",
        "window": {"from": "2026-09-13T08:00:00Z", "to": "2026-09-13T18:00:00Z", "label": "since previous boundary"},
        "prior_boundary_id": "day-start-20260913-ab12",
        "completed": {"count": 0, "items": []},
        "questions": {"open": 0, "opened": 0, "closed": 0, "featured": []},
        "retractions": [],
    })
    monkeypatch.setattr(day, "append_event", fake_append)
    monkeypatch.setattr(day, "events_path", lambda root: root / "events.jsonl")
    monkeypatch.setattr(day, "questions_path", lambda: Path("questions.jsonl"))
    monkeypatch.setattr(day, "resolve_carveout_root", lambda: Path("project"))

    with pytest.raises(day.DayIndexWriteError, match="day-end-20260913-ab12"):
        day._record_boundary("end")
    assert len(captured) == 2
