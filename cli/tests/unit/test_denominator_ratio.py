"""The deliverables-1 ratio measurement (x-cbab, task 7).

Shipped as a TASK, not a note: a ``target_denominator`` event recorded at init +
a ``fno do target denominator-ratio`` command that reads it. The cheap
``--deliverables 1`` exit is the load-bearing bypass risk; this measures whether
it is reflexive (past ~80 percent, ``enumerated_scope`` needs widening).
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta
from pathlib import Path

from typer.testing import CliRunner


def _event(denominator: str, *, count=None, days_ago: int = 0) -> str:
    ts = (datetime.now() - timedelta(days=days_ago)).isoformat()
    data = {"denominator": denominator}
    if count is not None:
        data["count"] = count
    return json.dumps({"ts": ts, "type": "target_denominator", "source": "target", "data": data})


def test_target_denominator_event_validates():
    """The schema entry lands and the event validates."""
    from fno.events import _build

    ev = _build("target_denominator", "target", {"denominator": "deliverables", "count": 1})
    assert ev["type"] == "target_denominator"
    assert ev["data"]["denominator"] == "deliverables"


def test_record_denominator_choice_classifies_each_exit(monkeypatch, tmp_path):
    """init's helper emits one event per run, classified by which exit was taken."""
    from fno import target_cli

    (tmp_path / ".fno").mkdir()
    # init's cwd is the repo root; the helper writes to the cwd-relative default
    # events path, so chdir to the tmp repo to keep the test hermetic. The
    # journal needs pinning too: the hermetic sandbox sets FNO_EVENTS_PATH for
    # the whole pytest process, and it is checked ahead of the resolved root.
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))

    target_cli._record_denominator_choice("/x/p.md", None, None)  # plan
    target_cli._record_denominator_choice("", 1, None)  # deliverables:1
    target_cli._record_denominator_choice("", None, None)  # none

    lines = (tmp_path / ".fno" / "events.jsonl").read_text().strip().splitlines()
    denom = sorted(json.loads(ln)["data"]["denominator"] for ln in lines)
    assert denom == ["deliverables", "none", "plan"]


def test_ratio_command_flags_a_bypass(monkeypatch, tmp_path):
    """80% deliverables:1 -> the verdict names the bypass."""
    from fno.cli import app

    (tmp_path / ".fno").mkdir()
    events = [
        _event("plan"),
        _event("plan"),
        *[_event("deliverables", count=1) for _ in range(8)],  # 8 of 10 declared
    ]
    (tmp_path / ".fno" / "events.jsonl").write_text("\n".join(events) + "\n")
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda *a, **k: tmp_path)

    r = CliRunner().invoke(app, ["do", "target", "denominator-ratio", "--json"])
    assert r.exit_code == 0, r.output
    obj = json.loads(r.output)
    assert obj["deliverables_1_ratio_pct"] == 80.0
    assert obj["deliverables_1"] == 8
    assert obj["plan_backed"] == 2
    assert obj["verdict"].startswith("bypass")


def test_ratio_command_reports_healthy_below_threshold(monkeypatch, tmp_path):
    from fno.cli import app

    (tmp_path / ".fno").mkdir()
    events = [_event("plan"), _event("plan"), _event("deliverables", count=1)]
    (tmp_path / ".fno" / "events.jsonl").write_text("\n".join(events) + "\n")
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda *a, **k: tmp_path)

    obj = json.loads(CliRunner().invoke(app, ["do", "target", "denominator-ratio", "--json"]).output)
    assert obj["deliverables_1_ratio_pct"] == 33.3
    assert obj["verdict"] == "healthy"


def test_ratio_command_respects_the_window(monkeypatch, tmp_path):
    """Events older than --since-days are excluded (a stale spike must not read current)."""
    from fno.cli import app

    (tmp_path / ".fno").mkdir()
    events = [
        _event("deliverables", count=1, days_ago=40),  # outside the 28d window
        _event("plan"),
    ]
    (tmp_path / ".fno" / "events.jsonl").write_text("\n".join(events) + "\n")
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda *a, **k: tmp_path)

    obj = json.loads(
        CliRunner().invoke(app, ["do", "target", "denominator-ratio", "--json", "--since-days", "28"]).output
    )
    assert obj["plan_backed"] == 1
    assert obj["deliverables_declared"] == 0  # the 40-day-old one excluded


def test_ratio_command_handles_no_data(monkeypatch, tmp_path):
    from fno.cli import app

    (tmp_path / ".fno").mkdir()
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda *a, **k: tmp_path)

    obj = json.loads(CliRunner().invoke(app, ["do", "target", "denominator-ratio", "--json"]).output)
    assert obj["deliverables_1_ratio_pct"] is None
    assert "no declared-denominator" in obj["verdict"]
