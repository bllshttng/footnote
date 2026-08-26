"""orchestrator.emit_status_event - the do-phase task_done/blocked boundary emit.

The orchestrator shells `fno doctor event emit` (skills stay self-contained, never
import repo code). These tests cover the argv it builds and the non-fatal
contract: an emit failure logs and returns False, never raising.
"""
from __future__ import annotations

import importlib.util
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]


def _load_orch():
    spec = importlib.util.spec_from_file_location(
        "do_orchestrator", REPO / "skills/execute/orchestrator.py"
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
    assert argv[:6] == ["fno", "doctor", "event", "emit", "-t", "task_done"]
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


# -- task claim settlement at the boundary (x-09d7 group 3) --


def test_boundary_success_outcome_releases_done(monkeypatch) -> None:
    """task_done + SUCCESS/DONE_WITH_CONCERNS settles the claim to done."""
    orch = _load_orch()
    calls: list[list[str]] = []

    def fake_run(argv, **kw):
        calls.append(list(argv))
        return _Result(0)

    monkeypatch.setattr(orch.subprocess, "run", fake_run)
    assert orch.release_task_claim_at_boundary("x-t1", "1.1", "SUCCESS") is True
    assert orch.release_task_claim_at_boundary("x-t1", "1.1", "DONE_WITH_CONCERNS") is True
    for argv in calls:
        assert argv[:6] == ["fno", "backlog", "task", "update", "x-t1", "1.1"]
        assert argv[-2:] == ["--status", "done"]


def test_boundary_failed_or_blocked_gives_back_pending(monkeypatch) -> None:
    """FAILED and BLOCKED outcomes give the task back to pending so the next
    ready worker can claim it."""
    orch = _load_orch()
    calls: list[list[str]] = []

    def fake_run(argv, **kw):
        calls.append(list(argv))
        return _Result(0)

    monkeypatch.setattr(orch.subprocess, "run", fake_run)
    assert orch.release_task_claim_at_boundary("x-t1", "2.2", "FAILED") is True
    assert orch.release_task_claim_at_boundary("x-t1", "2.2", "BLOCKED") is True
    for argv in calls:
        assert argv[-2:] == ["--status", "pending"]


def test_an_empty_outcome_settles_nothing(monkeypatch) -> None:
    """The `blocked` branch hardcodes "FAILED", so an empty outcome can only
    reach here from a `task_done` whose optional --outcome was omitted.

    Treating that as a give-back returns a task that just committed to
    pending, and a peer claims and re-runs it. The positive control is the
    same instrument settling the healthy outcome one line down.
    """
    orch = _load_orch()
    calls: list[list[str]] = []

    def fake_run(argv, **kw):
        calls.append(list(argv))
        return _Result(0)

    monkeypatch.setattr(orch.subprocess, "run", fake_run)
    assert orch.release_task_claim_at_boundary("x-t1", "2.2", "") is False
    assert calls == [], "an unknown outcome must run no settle at all"
    assert orch.release_task_claim_at_boundary("x-t1", "2.2", "SUCCESS") is True
    assert calls[-1][-2:] == ["--status", "done"]


def test_boundary_settlement_is_nonfatal(monkeypatch) -> None:
    """A refused settle (e.g. give-back by a non-holder, exit 3) logs and
    returns False; it must never raise into the emit path."""
    orch = _load_orch()
    monkeypatch.setattr(orch.subprocess, "run", lambda *a, **k: _Result(3))
    assert orch.release_task_claim_at_boundary("x-t1", "1.1", "FAILED") is False

    def boom(*a, **k):
        raise FileNotFoundError()

    monkeypatch.setattr(orch.subprocess, "run", boom)
    assert orch.release_task_claim_at_boundary("x-t1", "1.1", "SUCCESS") is False


def test_unknown_outcome_settles_nothing(monkeypatch) -> None:
    """The outcome vocabulary is closed: an unrecognized spelling (lowercase
    success, PARTIAL) runs NO subprocess - mapping strays to the give-back
    would release a finished task's claim mid-flight."""
    orch = _load_orch()
    called: list[list[str]] = []

    def fake_run(argv, **kw):
        called.append(list(argv))
        return _Result(0)

    monkeypatch.setattr(orch.subprocess, "run", fake_run)
    assert orch.release_task_claim_at_boundary("x-t1", "1.1", "PARTIAL") is False
    assert orch.release_task_claim_at_boundary("x-t1", "1.1", "success") is False
    assert called == [], "an unknown outcome must not touch the claim"
    # Positive control: a known outcome through the same path runs.
    assert orch.release_task_claim_at_boundary("x-t1", "1.1", "SUCCESS") is True
    assert called and called[0][-2:] == ["--status", "done"]


def test_manifest_graph_node_id_fallback(tmp_path, monkeypatch) -> None:
    """The settle's node fallback reads graph_node_id from the manifest, so
    the documented bare `--emit-boundary task_done --task N.M` (no --node)
    still settles the claim taken at dispatch."""
    orch = _load_orch()
    manifest = tmp_path / "target-state.md"
    manifest.write_text(
        "session_id: 2782a6e1-aaaa-bbbb-cccc-dddddddddddd\n"
        "graph_node_id: x-t9\n"
        "plan_path: /nowhere.md\n",
        encoding="utf-8",
    )
    assert orch.manifest_graph_node_id(str(manifest)) == "x-t9"
    # Positive control for the miss case: an existing manifest WITHOUT the
    # field, plus a missing file, both yield "" (skip, never a wrong node).
    bare = tmp_path / "bare.md"
    bare.write_text("session_id: 2782a6e1-aaaa-bbbb-cccc-dddddddddddd\n", encoding="utf-8")
    assert orch.manifest_graph_node_id(str(bare)) == ""
    assert orch.manifest_graph_node_id(str(tmp_path / "nope.md")) == ""
