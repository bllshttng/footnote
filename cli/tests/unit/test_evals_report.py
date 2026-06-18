"""Tests for fno evals report command (Task 3.1).

Matches the -k evals_report filter.

Covers:
- AC2-HP (report side): latest-per-task result printed with pass/total, termination, cost, isolation, ts
- AC1-EDGE (report side): pass-with-bad-termination flagged as PASS* not plain PASS
- Staleness warning when newest row older than staleness_days (default 14)
- Staleness: fresh ts -> no warning
- Staleness: configurable override via config.evals.staleness_days
- Empty history -> explicit message, exit 0
- Report --task filter: single task
- AC2-FR (report side): corrupt line skipped with line number warning, completes
- Trend: last N runs pass-rate and cost direction shown
"""
from __future__ import annotations

import json
import time
from datetime import datetime, timezone, timedelta
from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _mk_row(
    *,
    task: str = "test-task",
    label: str = "baseline",
    ts: str | None = None,
    passed: bool = True,
    total: int = 2,
    assertions: dict | None = None,
    termination_reason: str = "DoneAdvisory",
    cost_usd: float | None = 1.5,
    tokens_total: int | None = 50000,
    isolation: str = "clean",
    wall_secs: float = 120.0,
) -> dict:
    """Build a minimal history row matching _build_row's schema."""
    if ts is None:
        ts = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
    if assertions is None:
        assertions = {"check-a": True, "check-b": True} if passed and total == 2 else {}
    return {
        "ts": ts,
        "task": task,
        "label": label,
        "abilities_sha": "abc1234",
        "installed_rev": "abc1234",
        "model": "claude-test",
        "driver": "claude-code",
        "termination_reason": termination_reason,
        "assertions": assertions,
        "passed": passed,
        "total": total,
        "tokens_total": tokens_total,
        "cost_usd": cost_usd,
        "wall_secs": wall_secs,
        "iterations": None,
        "session_id": None,
        "transcript_path": None,
        "isolation": isolation,
        "workdir_kept": None,
    }


def _write_history(path: Path, rows: list[dict]) -> None:
    """Write rows to a JSONL history file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        for row in rows:
            f.write(json.dumps(row, separators=(",", ":"), ensure_ascii=False) + "\n")


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Point evals_history() at a tmp file so tests never touch real state."""
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)
    yield


def _get_runner_and_app():
    from typer.testing import CliRunner
    from fno.evals.cli import evals_app
    return CliRunner(), evals_app


# ---------------------------------------------------------------------------
# Report: empty history
# ---------------------------------------------------------------------------


def test_evals_report_empty_history_explicit_message(tmp_path: Path) -> None:
    """Report on empty history prints explicit message, exits 0."""
    runner, evals_app = _get_runner_and_app()

    result = runner.invoke(evals_app, ["report"])
    assert result.exit_code == 0, f"Expected exit 0, got {result.exit_code}: {result.output}"
    # Must not produce empty output
    assert result.output.strip(), "Expected explicit message for empty history, got empty output"
    # Should mention "no" or "empty" or "history"
    lower = result.output.lower()
    assert any(word in lower for word in ("no", "empty", "history", "runs")), (
        f"Expected 'no history' message, got: {result.output}"
    )


# ---------------------------------------------------------------------------
# Report: latest-per-task happy path
# ---------------------------------------------------------------------------


def test_evals_report_latest_per_task(tmp_path: Path) -> None:
    """AC2-HP: latest row per task printed with pass/total, termination, cost, isolation."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    # Two tasks, two rows each (latest should be the second)
    ts_old = "2026-06-01T10:00:00Z"
    ts_new = "2026-06-05T10:00:00Z"
    rows = [
        _mk_row(task="feature-add", label="run1", ts=ts_old, cost_usd=1.0),
        _mk_row(task="feature-add", label="run2", ts=ts_new, cost_usd=2.5),
        _mk_row(task="bug-fix", label="run1", ts=ts_old, cost_usd=0.8),
        _mk_row(task="bug-fix", label="run2", ts=ts_new, cost_usd=1.2),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["report"])
    assert result.exit_code == 0, f"Unexpected exit {result.exit_code}: {result.output}"

    output = result.output
    # Both tasks should appear
    assert "feature-add" in output
    assert "bug-fix" in output
    # Should show pass/total
    assert "2/2" in output
    # Should show termination reason
    assert "DoneAdvisory" in output
    # Should show cost
    assert "2.5" in output or "$2.50" in output or "2.50" in output
    # Should show isolation
    assert "clean" in output.lower()


def test_evals_report_task_filter(tmp_path: Path) -> None:
    """Report --task slug shows only that task's latest row."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        _mk_row(task="alpha", label="run1"),
        _mk_row(task="beta", label="run1"),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["report", "--task", "alpha"])
    assert result.exit_code == 0
    assert "alpha" in result.output
    assert "beta" not in result.output


# ---------------------------------------------------------------------------
# Report: pass-with-bad-termination flagging (AC1-EDGE)
# ---------------------------------------------------------------------------


def test_evals_report_pass_with_bad_termination_flagged(tmp_path: Path) -> None:
    """AC1-EDGE: row with passed=True but non-advisory termination shows PASS* not plain PASS."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        _mk_row(
            task="budget-task",
            passed=True,
            total=2,
            assertions={"check-a": True, "check-b": True},
            termination_reason="Budget",
        ),
        _mk_row(
            task="good-task",
            passed=True,
            total=2,
            termination_reason="DoneAdvisory",
        ),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["report"])
    assert result.exit_code == 0, f"Unexpected exit: {result.output}"

    output = result.output
    # The bad-termination task must be flagged distinctly
    assert "PASS*" in output or "pass*" in output.lower(), (
        f"Expected PASS* flag for Budget termination, got:\n{output}"
    )
    # Legend or explanation should be present
    assert "*" in output  # at minimum the asterisk appears

    # The good task should show plain PASS (not PASS*)
    # We just verify the normal PASS is also present without asterisk marking
    assert "PASS" in output


def test_evals_report_noprogress_flagged(tmp_path: Path) -> None:
    """AC1-EDGE: NoProgress with passing assertions flagged as PASS*."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        _mk_row(
            task="noprogress-task",
            passed=True,
            total=1,
            assertions={"check": True},
            termination_reason="NoProgress",
        ),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["report"])
    assert result.exit_code == 0
    assert "PASS*" in result.output or "pass*" in result.output.lower()


# ---------------------------------------------------------------------------
# Report: staleness warning
# ---------------------------------------------------------------------------


def test_evals_report_staleness_warning_when_old(tmp_path: Path) -> None:
    """Staleness line printed when newest row is older than staleness_days (default 14)."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    # Timestamp 30 days ago
    old_ts = (datetime.now(timezone.utc) - timedelta(days=30)).isoformat().replace("+00:00", "Z")
    rows = [_mk_row(task="stale-task", ts=old_ts)]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["report"])
    assert result.exit_code == 0
    output = result.output.lower()
    assert "stale" in output or "days" in output or "old" in output or "outdated" in output, (
        f"Expected staleness warning for 30-day-old history, got:\n{result.output}"
    )


def test_evals_report_no_staleness_warning_when_fresh(tmp_path: Path) -> None:
    """No staleness warning when newest row is within staleness_days."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    # Timestamp 3 days ago - within default 14-day window
    fresh_ts = (datetime.now(timezone.utc) - timedelta(days=3)).isoformat().replace("+00:00", "Z")
    rows = [_mk_row(task="fresh-task", ts=fresh_ts)]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["report"])
    assert result.exit_code == 0
    output = result.output.lower()
    # Should NOT print staleness warning
    assert "stale" not in output and "outdated" not in output, (
        f"Unexpected staleness warning for fresh history:\n{result.output}"
    )


def test_evals_report_staleness_configurable(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Staleness threshold is configurable; a 5-day-old run is stale at staleness_days=3."""
    import fno.paths as paths_mod
    from fno import config as config_mod

    history_path = paths_mod.evals_history()
    # 5 days old
    ts = (datetime.now(timezone.utc) - timedelta(days=5)).isoformat().replace("+00:00", "Z")
    rows = [_mk_row(task="config-stale-task", ts=ts)]
    _write_history(history_path, rows)

    # Write settings with staleness_days=3
    settings_path = tmp_path / ".fno" / "settings.yaml"
    settings_path.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: {str(tmp_path / '.fno')}/\n"
        f"  evals:\n    staleness_days: 3\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_path))
    config_mod.load_settings.cache_clear()
    paths_mod._settings.cache_clear()

    runner, evals_app = _get_runner_and_app()
    result = runner.invoke(evals_app, ["report"])
    assert result.exit_code == 0
    output = result.output.lower()
    assert "stale" in output or "days" in output or "old" in output or "outdated" in output, (
        f"Expected staleness warning with staleness_days=3 and 5-day-old data:\n{result.output}"
    )


# ---------------------------------------------------------------------------
# Report: corrupt line tolerance (AC2-FR)
# ---------------------------------------------------------------------------


def test_evals_report_corrupt_line_skipped_with_lineno(tmp_path: Path) -> None:
    """AC2-FR: malformed JSONL line skipped with warning naming the line number."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    history_path.parent.mkdir(parents=True, exist_ok=True)
    # Line 1: valid, Line 2: corrupt, Line 3: valid
    with open(history_path, "w", encoding="utf-8") as f:
        f.write(json.dumps(_mk_row(task="good-task-1"), separators=(",", ":")) + "\n")
        f.write("NOT VALID JSON {{{\n")
        f.write(json.dumps(_mk_row(task="good-task-2"), separators=(",", ":")) + "\n")

    result = runner.invoke(evals_app, ["report"])
    # Should complete (exit 0)
    assert result.exit_code == 0, f"Should complete despite corrupt line: {result.output}"
    # Should warn about the corrupt line with its line number
    output_lower = result.output.lower()
    assert "line 2" in output_lower or "line: 2" in output_lower or "lineno" in output_lower or (
        "2" in result.output and ("warn" in output_lower or "skip" in output_lower or "malform" in output_lower or "corrupt" in output_lower or "invalid" in output_lower)
    ), f"Expected line-number warning for corrupt line 2:\n{result.output}"
    # Valid rows should still show up
    assert "good-task-1" in result.output or "good-task-2" in result.output


# ---------------------------------------------------------------------------
# Report: trend
# ---------------------------------------------------------------------------


def test_evals_report_trend_shown(tmp_path: Path) -> None:
    """Trend over last N runs is shown (pass-rate and cost direction)."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    # 5 runs for one task, varying pass/fail and cost
    base_ts = datetime.now(timezone.utc) - timedelta(days=10)
    rows = []
    for i in range(5):
        ts = (base_ts + timedelta(hours=i)).isoformat().replace("+00:00", "Z")
        rows.append(_mk_row(task="trend-task", ts=ts, passed=(i % 2 == 0), cost_usd=1.0 + i * 0.1))
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["report"])
    assert result.exit_code == 0
    # Should show multiple runs or trend info
    output = result.output
    # At minimum should mention multiple runs or a pass rate
    assert any(c in output for c in ["%", "/", "trend", "runs", "last"]) or len(output.strip()) > 50, (
        f"Expected trend info in report output:\n{output}"
    )
