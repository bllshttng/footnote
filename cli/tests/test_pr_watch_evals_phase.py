"""The pr-watch tick's evals phase (x-ab72): fires when due, skips on the
gate, journals evals_stale with one notice per window, and never counts an
exit-0 run that appended no rows as a success."""
from __future__ import annotations

import json
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from fno.evals import history as _history
from fno.pr_watch import _evals_phase as ep

_NOW = datetime(2026, 9, 15, 12, 0, 0, tzinfo=timezone.utc)


class _Evals:
    def __init__(self, schedule: int = 7, stale: int = 7) -> None:
        self.schedule_days = schedule
        self.stale_days = stale


class _Settings:
    def __init__(self, evals: object | None = None) -> None:
        self.evals = evals if evals is not None else _Evals()


def _ts(days_ago: float) -> str:
    return (_NOW - timedelta(days=days_ago)).isoformat().replace("+00:00", "Z")


def _write_history(tmp_path: Path, *rows: dict) -> Path:
    hp = tmp_path / "evals-history.jsonl"
    for r in rows:
        _history.append_row(hp, r)
    return hp


def _reg_row(task_id: str, ts: str) -> dict:
    return {"ts": ts, "task_id": task_id, "tier": "regression", "pass": True,
            "reason": "", "duration_s": 1.0, "repeat_index": 0,
            "bank_rev": None, "worker_provider": None}


@pytest.fixture()
def autonomy_on(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("fno.config.autonomy_master_enabled", lambda: True)


def _run(settings, tmp_path, history, *, emit_rows, gate=(True, ""),
         runner=None, events_path=None, notify=None, budget=7200.0, emit=None):
    if runner is None:
        def runner(cmd, timeout_s):
            return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")
    if emit is None:
        emit = lambda k, d: emit_rows.append((k, d))  # noqa: E731
    return ep.run_evals_phase(
        settings, emit=emit,
        budget_left_s=budget, fno_bin="fno-py-bin",
        history_path=history, events_path=events_path, notify=notify,
        runner=runner, now=_NOW,
    )


def test_fires_when_newest_regression_run_past_window(
    tmp_path, monkeypatch, autonomy_on
) -> None:
    monkeypatch.setattr(ep, "_gate_open", lambda: (True, ""))
    history = _write_history(
        tmp_path, _reg_row("regression-cli-help-smoke", _ts(9)),
    )

    emitted: list[tuple[str, dict]] = []

    def runner(cmd: list[str], timeout_s: int) -> subprocess.CompletedProcess:
        assert cmd == ["fno-py-bin", "doctor", "evals", "run",
                       "--tier", "regression", "-y"]
        assert timeout_s == int(7200.0 - 60)
        for tid in ("regression-cli-help-smoke", "regression-loop-check-suite",
                    "regression-context-pitfalls-reachable"):
            _history.append_row(history, _reg_row(tid, _ts(0.0)))
        return subprocess.CompletedProcess(cmd, 0, stdout="done.", stderr="")

    row = _run(_Settings(), tmp_path, history, emit_rows=emitted, runner=runner)

    assert row["acted"] == 1 and row["skip_reason"] is None
    kinds = [k for k, _ in emitted]
    assert kinds == [ep.EVALS_SCHEDULED_RUN]
    data = emitted[0][1]
    assert data["task_count"] == 3
    assert data["passes"] == 3
    assert data["window_days"] == 7


def test_skips_when_fresh(tmp_path, monkeypatch, autonomy_on) -> None:
    monkeypatch.setattr(ep, "_gate_open", lambda: (True, ""))
    history = _write_history(tmp_path, _reg_row("regression-cli-help-smoke", _ts(2)))
    emitted: list[tuple[str, dict]] = []
    row = _run(_Settings(), tmp_path, history, emit_rows=emitted)
    assert row == {"acted": 0, "skip_reason": "fresh", "detail": "age 2d <= 7d window"}
    assert emitted == []


def test_skips_on_refusing_gate_and_journals_stale(
    tmp_path, monkeypatch, autonomy_on
) -> None:
    monkeypatch.setattr(ep, "_gate_open", lambda: (False, "load"))
    history = _write_history(tmp_path, _reg_row("regression-cli-help-smoke", _ts(15)))
    emitted: list[tuple[str, dict]] = []
    notices: list[str] = []
    events_file = tmp_path / "events.jsonl"

    def _emit(kind: str, data: dict) -> None:
        # Mirror production: the emit appends its envelope to the journal.
        emitted.append((kind, data))
        envelope = {"ts": _ts(0.0), "type": kind, "source": "daemon", "data": data}
        with open(events_file, "a", encoding="utf-8") as fh:
            fh.write(json.dumps(envelope) + "\n")

    def _notify(title: str, message: str) -> None:
        notices.append(message)

    row = _run(_Settings(), tmp_path, history, emit_rows=emitted,
               events_path=events_file, notify=_notify, emit=_emit)

    assert row["acted"] == 0 and row["skip_reason"] == "load"
    kinds = [k for k, _ in emitted]
    assert kinds == [ep.EVALS_STALE]
    data = emitted[0][1]
    assert data["reason"] == "gate"
    assert data["age_days"] == pytest.approx(15.0, abs=0.01)
    assert len(notices) == 1
    # The journal row exists (append-only envelope with ts).
    lines = events_file.read_text(encoding="utf-8").splitlines()
    assert any(json.loads(l)["type"] == ep.EVALS_STALE for l in lines)


def test_notice_deduped_within_window_by_journal(
    tmp_path, monkeypatch, autonomy_on
) -> None:
    monkeypatch.setattr(ep, "_gate_open", lambda: (False, "load"))
    history = _write_history(tmp_path, _reg_row("regression-cli-help-smoke", _ts(15)))
    events_file = tmp_path / "events.jsonl"
    prior = {"ts": _ts(1), "type": ep.EVALS_STALE, "source": "daemon",
             "data": {"reason": "gate"}}
    events_file.write_text(json.dumps(prior) + "\n", encoding="utf-8")
    notices: list[str] = []

    row = _run(_Settings(), tmp_path, history, emit_rows=[],
               events_path=events_file, notify=lambda t, m: notices.append(m))

    assert row["acted"] == 0
    assert notices == []  # a prior evals_stale row inside the window absorbs it


def test_exit_zero_without_rows_is_not_a_success(
    tmp_path, monkeypatch, autonomy_on
) -> None:
    monkeypatch.setattr(ep, "_gate_open", lambda: (True, ""))
    history = _write_history(tmp_path, _reg_row("regression-cli-help-smoke", _ts(15)))
    emitted: list[tuple[str, dict]] = []

    def runner(cmd: list[str], timeout_s: int) -> subprocess.CompletedProcess:
        return subprocess.CompletedProcess(cmd, 0, stdout="done.", stderr="")

    row = _run(_Settings(), tmp_path, history, emit_rows=emitted, runner=runner)

    assert row["acted"] == 0 and row["skip_reason"] == "no_rows"
    assert emitted[0][0] == ep.EVALS_STALE
    assert emitted[0][1]["reason"] == "no_rows"


def test_schedule_zero_disables(tmp_path, autonomy_on) -> None:
    emitted: list[tuple[str, dict]] = []
    history = _write_history(tmp_path, _reg_row("r", _ts(15)))
    row = _run(_Settings(evals=_Evals(schedule=0)), tmp_path, history,
               emit_rows=emitted)
    assert row["skip_reason"] == "evals_off"
    assert emitted == []


def test_settings_stub_without_evals_block_reads_unarmed(
    tmp_path, autonomy_on
) -> None:
    class _Bare:
        pass

    emitted: list[tuple[str, dict]] = []
    row = _run(_Bare(), tmp_path, tmp_path / "none.jsonl", emit_rows=emitted)
    assert row["skip_reason"] == "evals_off"
    assert emitted == []


def test_young_bank_past_stale_but_inside_schedule_skips_quietly(
    tmp_path, monkeypatch, autonomy_on
) -> None:
    # Gate refuses while the bank is only 10d old (past 2x stale is 14d):
    # a quiet skip, no evals_stale row, no notice.
    monkeypatch.setattr(ep, "_gate_open", lambda: (False, "fleet_full"))
    history = _write_history(tmp_path, _reg_row("regression-cli-help-smoke", _ts(10)))
    emitted: list[tuple[str, dict]] = []
    notices: list[str] = []
    row = _run(_Settings(), tmp_path, history, emit_rows=emitted,
               notify=lambda t, m: notices.append(m))
    assert row["skip_reason"] == "fleet_full"
    assert emitted == []
    assert notices == []
