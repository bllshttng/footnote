"""orchestrator.emit_status_event - the do-phase task_done/blocked boundary emit.

The orchestrator shells `fno event emit` (skills stay self-contained, never
import repo code). These tests cover the argv it builds and the non-fatal
contract: an emit failure logs and returns False, never raising.
"""
from __future__ import annotations

import importlib.util
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]


def _load_orch():
    spec = importlib.util.spec_from_file_location(
        "do_orchestrator", REPO / "skills/do/orchestrator.py"
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class _Result:
    def __init__(self, rc: int = 0) -> None:
        self.returncode = rc
        self.stderr = b""


def test_emit_status_event_builds_argv(monkeypatch) -> None:
    orch = _load_orch()
    captured: dict = {}

    def fake_run(argv, **kw):
        captured["argv"] = argv
        return _Result(0)

    monkeypatch.setattr(orch.subprocess, "run", fake_run)
    ok = orch.emit_status_event(
        "task_done", run="R1", node="prj-0001", task="2.1", outcome="SUCCESS",
        data={"commit": "abc"},
    )
    assert ok is True
    argv = captured["argv"]
    assert argv[:5] == ["fno", "event", "emit", "-t", "task_done"]
    for flag, val in (("--run", "R1"), ("--node", "prj-0001"), ("--task", "2.1"), ("--outcome", "SUCCESS")):
        assert flag in argv and val in argv


def test_emit_status_event_omits_empty_flags(monkeypatch) -> None:
    orch = _load_orch()
    captured: dict = {}

    def fake_run(argv, **kw):
        captured["argv"] = argv
        return _Result(0)

    monkeypatch.setattr(orch.subprocess, "run", fake_run)
    orch.emit_status_event("blocked", run="R1", data={"reason": "x"})
    argv = captured["argv"]
    assert "--node" not in argv  # empty -> omitted
    assert "--outcome" not in argv


def test_emit_status_event_nonfatal_when_fno_missing(monkeypatch) -> None:
    orch = _load_orch()

    def boom(*a, **k):
        raise FileNotFoundError()

    monkeypatch.setattr(orch.subprocess, "run", boom)
    assert orch.emit_status_event("blocked", run="R1", data={"reason": "x"}) is False


def test_emit_status_event_nonfatal_on_reject(monkeypatch) -> None:
    orch = _load_orch()
    monkeypatch.setattr(orch.subprocess, "run", lambda *a, **k: _Result(1))
    assert orch.emit_status_event("task_done", run="R1", outcome="PARTIAL") is False
