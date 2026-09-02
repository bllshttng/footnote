"""Acceptance tests for plan stamp and graduate telemetry."""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.plan._stamp import build_parser, cmd_graduate, cmd_stamp


def _write_plan(path: Path, body: str) -> None:
    path.write_text(
        "---\nclaims: x-e94a\n" + body + "---\n\n# body\n",
        encoding="utf-8",
    )


def test_stamp_emits_after_the_plan_write(tmp_path: Path, monkeypatch) -> None:
    """AC8: the event carries the stamped subject and outcome."""
    plan = tmp_path / "plan.md"
    _write_plan(plan, "status: ready\n")
    events: list[dict] = []
    monkeypatch.setattr("fno.events.append_event", lambda event: events.append(event))

    args = build_parser().parse_args(
        ["stamp", "--plan-path", str(plan), "--session-id", "sess-1", "--url", "https://x/pull/1"]
    )
    assert cmd_stamp(args) == 0

    assert "status: in_review" in plan.read_text(encoding="utf-8")
    assert len(events) == 1
    assert events[0]["type"] == "plan_stamped"
    assert events[0]["data"]["plan_path"] == str(plan.resolve())
    assert events[0]["data"]["session_id"] == "sess-1"
    assert events[0]["data"]["outcome"] == "stamped"
    assert events[0]["data"]["node_id"] == "x-e94a"


def test_graduate_not_met_emits_and_leaves_status_unchanged(tmp_path: Path, monkeypatch) -> None:
    """AC9: a deliberate not-met result is distinguishable from no run."""
    plan = tmp_path / "plan.md"
    _write_plan(
        plan,
        "status: in_review\nexpected_url_count: 2\nurls: [https://x/pull/1]\nsession_ids: [sess-1]\n",
    )
    events: list[dict] = []
    monkeypatch.setattr("fno.events.append_event", lambda event: events.append(event))

    args = build_parser().parse_args(["graduate", "--plan-path", str(plan)])
    assert cmd_graduate(args) == 0

    assert "status: in_review" in plan.read_text(encoding="utf-8")
    assert events[-1]["type"] == "plan_graduated"
    assert events[-1]["data"]["outcome"] == "not_met"
    assert events[-1]["data"]["reason"]


def test_stamp_emit_failure_is_named_without_failing_successful_stamp(
    tmp_path: Path, monkeypatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """AC10: telemetry loss is visible but does not undo the durable stamp."""
    plan = tmp_path / "plan.md"
    _write_plan(plan, "status: ready\n")

    def fail(_event):
        raise OSError("journal locked")

    monkeypatch.setattr("fno.events.append_event", fail)
    args = build_parser().parse_args(
        ["stamp", "--plan-path", str(plan), "--session-id", "sess-1", "--url", "https://x/pull/1"]
    )

    assert cmd_stamp(args) == 0
    assert "status: in_review" in plan.read_text(encoding="utf-8")
    err = capsys.readouterr().err
    assert "plan_stamped" in err
    assert "journal locked" in err
