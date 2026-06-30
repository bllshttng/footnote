"""Tests for `fno pr status` verdict logic (x-8b64 G).

The non-trivial part is classifying a mixed statusCheckRollup: CheckRun entries
carry status+conclusion (conclusion empty until COMPLETED), StatusContext
entries carry only state. The Boundary cases: an in-progress check is *pending*
not red, and an empty rollup is *unknown* not red.
"""
from __future__ import annotations

from fno.pr import _status


def test_all_pass_is_green():
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"context": "legacy", "state": "SUCCESS"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "green"
    assert code == 0
    assert counts == {"total": 2, "pass": 2, "fail": 0, "pending": 0}


def test_any_failure_is_red():
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"name": "lint", "status": "COMPLETED", "conclusion": "FAILURE"},
    ]
    verdict, code, _ = _status.verdict_for(rollup)
    assert verdict == "red"
    assert code == 1


def test_in_progress_check_is_pending_not_red():
    """Boundary: a CheckRun with status=IN_PROGRESS has conclusion='' and must
    read as pending, never red."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"name": "build", "status": "IN_PROGRESS", "conclusion": ""},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "pending"
    assert code == 2
    assert counts["pending"] == 1


def test_status_context_pending_is_pending():
    rollup = [{"context": "deploy", "state": "PENDING"}]
    verdict, code, _ = _status.verdict_for(rollup)
    assert verdict == "pending"
    assert code == 2


def test_no_checks_is_unknown_not_red():
    """Boundary: a PR with no checks -> unknown, not red."""
    verdict, code, counts = _status.verdict_for([])
    assert verdict == "unknown"
    assert code == 3
    assert counts["total"] == 0


def test_failure_wins_over_pending():
    rollup = [
        {"name": "build", "status": "IN_PROGRESS", "conclusion": ""},
        {"name": "lint", "status": "COMPLETED", "conclusion": "FAILURE"},
    ]
    verdict, code, _ = _status.verdict_for(rollup)
    assert verdict == "red"
    assert code == 1


def test_run_status_emits_json_and_code(monkeypatch, capsys):
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: {
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        },
    )
    code = _status.run_status("42")
    assert code == 0
    import json

    out = json.loads(capsys.readouterr().out)
    assert out == {
        "pr": "42",
        "verdict": "green",
        "settled": True,
        "green": True,
        "pr_state": "OPEN",
        "checks": {"total": 1, "pass": 1, "fail": 0, "pending": 0},
    }


def test_run_status_fetch_failure_is_error(monkeypatch, capsys):
    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: None)
    code = _status.run_status("99")
    assert code == 4
    import json

    out = json.loads(capsys.readouterr().out)
    assert out["verdict"] == "error"
    assert out["settled"] is False
