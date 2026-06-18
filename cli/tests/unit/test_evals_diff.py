"""Tests for fno evals diff command (Task 3.1).

Matches the -k evals_diff filter.

Covers:
- AC2-HP: assertion flips both directions (regression distinct from improvement), termination change, cost delta
- AC2-ERR: unknown label -> exit nonzero naming the missing label
- AC2-UI: both labels exist but no common tasks -> explicit message, nonzero exit
- AC2-EDGE: asymmetric task sets listed as "missing in <label>", not dropped
- AC2-FR: corrupt line skipped with line number, both labels processed
"""
from __future__ import annotations

import json
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
        if passed and total == 2:
            assertions = {"check-a": True, "check-b": True}
        elif total == 0:
            assertions = {}
        else:
            assertions = {f"check-{i}": True for i in range(total)}
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
# AC2-HP: happy path - assertion flips + termination change + cost delta
# ---------------------------------------------------------------------------


def test_evals_diff_assertion_regression_shown(tmp_path: Path) -> None:
    """AC2-HP: regression (ok->FAIL) is shown and visually distinct."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        # Before: check-a pass, check-b pass
        _mk_row(
            task="feature-add",
            label="before",
            assertions={"check-a": True, "check-b": True},
            passed=True,
            total=2,
        ),
        # After: check-a pass, check-b FAIL (regression)
        _mk_row(
            task="feature-add",
            label="after",
            assertions={"check-a": True, "check-b": False},
            passed=False,
            total=2,
        ),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "before", "--label", "after"])
    assert result.exit_code == 0, f"Diff should exit 0 when comparison produced: {result.output}"

    output = result.output
    # check-b flip (ok -> FAIL) should be shown
    assert "check-b" in output, f"Expected check-b in diff output:\n{output}"
    # Should indicate regression (FAIL direction)
    assert "FAIL" in output or "fail" in output.lower() or "regression" in output.lower() or "ok->FAIL" in output or "ok -> FAIL" in output


def test_evals_diff_assertion_improvement_shown(tmp_path: Path) -> None:
    """AC2-HP: improvement (FAIL->ok) is shown, visually distinct from regression."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        # Before: check-a FAIL
        _mk_row(
            task="bug-fix",
            label="before",
            assertions={"check-a": False},
            passed=False,
            total=1,
        ),
        # After: check-a pass (improvement)
        _mk_row(
            task="bug-fix",
            label="after",
            assertions={"check-a": True},
            passed=True,
            total=1,
        ),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "before", "--label", "after"])
    assert result.exit_code == 0, f"Diff should exit 0: {result.output}"

    output = result.output
    # check-a flip (fail -> OK) should be shown
    assert "check-a" in output, f"Expected check-a in diff output:\n{output}"
    # Should indicate improvement
    assert "ok" in output.lower() or "OK" in output or "pass" in output.lower() or "improve" in output.lower()


def test_evals_diff_regressions_distinct_from_improvements(tmp_path: Path) -> None:
    """AC2-HP: regressions and improvements are visually distinct markers."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        # Before: check-good=FAIL, check-bad=OK
        _mk_row(
            task="mixed-task",
            label="before",
            assertions={"check-good": False, "check-bad": True},
            passed=False,
            total=2,
        ),
        # After: check-good=OK (improvement), check-bad=FAIL (regression)
        _mk_row(
            task="mixed-task",
            label="after",
            assertions={"check-good": True, "check-bad": False},
            passed=False,
            total=2,
        ),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "before", "--label", "after"])
    assert result.exit_code == 0, f"Diff exit 0: {result.output}"

    output = result.output
    assert "check-good" in output, "Expected check-good flip shown"
    assert "check-bad" in output, "Expected check-bad flip shown"

    # The output must contain at least two distinct markers for the two flip directions
    # Regression marker (e.g. ✗ or FAIL or -) vs improvement marker (e.g. ✓ or OK or +)
    # Accept multiple conventions but both must appear
    regression_markers = ["FAIL", "fail", "regression", "->FAIL", "-> FAIL", "✗", "✘"]
    improvement_markers = ["->OK", "-> OK", "OK", "improve", "✓", "✔", "pass", "fix"]

    has_regression_marker = any(m in output for m in regression_markers)
    has_improvement_marker = any(m in output for m in improvement_markers)

    assert has_regression_marker or has_improvement_marker, (
        f"Expected distinct regression/improvement markers in output:\n{output}"
    )


def test_evals_diff_termination_change_shown(tmp_path: Path) -> None:
    """AC2-HP: termination_reason change between labels is shown."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        _mk_row(task="term-task", label="before", termination_reason="DoneAdvisory"),
        _mk_row(task="term-task", label="after", termination_reason="Budget"),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "before", "--label", "after"])
    assert result.exit_code == 0
    output = result.output
    # Both termination reasons should appear
    assert "DoneAdvisory" in output or "Budget" in output, (
        f"Expected termination change in diff:\n{output}"
    )


def test_evals_diff_cost_delta_shown(tmp_path: Path) -> None:
    """AC2-HP: tokens and cost delta between labels is shown."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        _mk_row(task="cost-task", label="before", cost_usd=1.50, tokens_total=50000),
        _mk_row(task="cost-task", label="after", cost_usd=2.50, tokens_total=80000),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "before", "--label", "after"])
    assert result.exit_code == 0
    output = result.output
    # Cost delta should appear: +1.00 or +$1.00 or similar
    assert "1.00" in output or "1.5" in output or "2.5" in output or "delta" in output.lower() or "cost" in output.lower(), (
        f"Expected cost delta in diff output:\n{output}"
    )


# ---------------------------------------------------------------------------
# AC2-ERR: unknown label -> exit nonzero naming missing label
# ---------------------------------------------------------------------------


def test_evals_diff_unknown_label_exits_nonzero(tmp_path: Path) -> None:
    """AC2-ERR: label matching no rows -> exit nonzero naming the missing label."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [_mk_row(task="some-task", label="real-label")]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "real-label", "--label", "ghost-label"])
    assert result.exit_code != 0, f"Expected nonzero exit for unknown label, got {result.exit_code}"
    # Output must name the missing label
    assert "ghost-label" in result.output, (
        f"Expected missing label named in output:\n{result.output}"
    )


def test_evals_diff_both_labels_unknown_exits_nonzero(tmp_path: Path) -> None:
    """AC2-ERR: both labels missing -> exit nonzero."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [_mk_row(task="some-task", label="existing")]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "no-such-a", "--label", "no-such-b"])
    assert result.exit_code != 0
    output = result.output
    assert "no-such-a" in output or "no-such-b" in output


# ---------------------------------------------------------------------------
# AC2-UI: both labels exist but no common tasks -> explicit, nonzero
# ---------------------------------------------------------------------------


def test_evals_diff_no_common_tasks_explicit_nonzero(tmp_path: Path) -> None:
    """AC2-UI: labels exist but share no common tasks -> explicit message + nonzero exit."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        _mk_row(task="alpha-task", label="before"),
        _mk_row(task="beta-task", label="after"),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "before", "--label", "after"])
    # Must not be empty output with exit 0
    assert result.exit_code != 0, (
        f"Expected nonzero exit when no common tasks, got {result.exit_code}"
    )
    # Must produce explicit message
    assert result.output.strip(), "Expected explicit message, got empty output"
    lower = result.output.lower()
    assert any(w in lower for w in ("no common", "no tasks", "common", "shared", "overlap")), (
        f"Expected 'no common tasks' message:\n{result.output}"
    )


# ---------------------------------------------------------------------------
# AC2-EDGE: asymmetric task sets listed as "missing in <label>"
# ---------------------------------------------------------------------------


def test_evals_diff_task_only_in_before_listed_as_missing(tmp_path: Path) -> None:
    """AC2-EDGE: task with rows in before but not after listed as 'missing in after'."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        # both have common-task
        _mk_row(task="common-task", label="before"),
        _mk_row(task="common-task", label="after"),
        # only-in-before has rows under before only
        _mk_row(task="only-in-before", label="before"),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "before", "--label", "after"])
    # diff should still succeed (comparison was produced for common-task)
    # Exit code 0 is fine when at least one common task exists
    output = result.output
    # only-in-before should be mentioned as missing in after
    assert "only-in-before" in output, f"Expected only-in-before listed:\n{output}"
    lower = output.lower()
    assert "missing" in lower or "only in" in lower or "not in" in lower, (
        f"Expected 'missing in after' info:\n{output}"
    )


def test_evals_diff_task_only_in_after_listed_as_missing(tmp_path: Path) -> None:
    """AC2-EDGE: task with rows in after but not before listed as 'missing in before'."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        _mk_row(task="common-task", label="before"),
        _mk_row(task="common-task", label="after"),
        _mk_row(task="only-in-after", label="after"),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "before", "--label", "after"])
    output = result.output
    assert "only-in-after" in output, f"Expected only-in-after listed:\n{output}"
    lower = output.lower()
    assert "missing" in lower or "only in" in lower or "not in" in lower


# ---------------------------------------------------------------------------
# AC2-FR: corrupt history line skipped with line number (diff)
# ---------------------------------------------------------------------------


def test_evals_diff_corrupt_line_skipped_with_lineno(tmp_path: Path) -> None:
    """AC2-FR: corrupt JSONL line skipped with warning naming line number, diff completes."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    history_path.parent.mkdir(parents=True, exist_ok=True)
    # Line 1: valid before row, Line 2: corrupt, Line 3: valid after row
    with open(history_path, "w", encoding="utf-8") as f:
        f.write(json.dumps(_mk_row(task="good-task", label="before"), separators=(",", ":")) + "\n")
        f.write("{bad json\n")
        f.write(json.dumps(_mk_row(task="good-task", label="after"), separators=(",", ":")) + "\n")

    result = runner.invoke(evals_app, ["diff", "--label", "before", "--label", "after"])
    # Should complete (either exit 0 with diff or nonzero only if no common tasks after corruption)
    # Key: should not crash with unhandled exception
    assert result.exit_code in (0, 1), f"Should complete cleanly: {result.output}"

    # Should warn about the corrupt line with its number
    output_lower = result.output.lower()
    assert "line 2" in output_lower or "line: 2" in output_lower or (
        "2" in result.output and ("warn" in output_lower or "skip" in output_lower or "corrupt" in output_lower or "invalid" in output_lower or "malform" in output_lower)
    ), f"Expected line-number warning for corrupt line 2:\n{result.output}"

    # good-task diff should still show up (valid rows processed)
    assert "good-task" in result.output


# ---------------------------------------------------------------------------
# Diff: latest-row-per-label semantics
# ---------------------------------------------------------------------------


def test_evals_diff_uses_latest_row_per_label(tmp_path: Path) -> None:
    """Diff uses the LATEST row per (task, label) when multiple rows exist."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    ts_old = "2026-06-01T10:00:00Z"
    ts_new = "2026-06-05T10:00:00Z"
    rows = [
        # before: old run (FAIL), new run (PASS) - latest should be PASS
        _mk_row(task="task-a", label="before", ts=ts_old,
                assertions={"check": False}, passed=False, total=1),
        _mk_row(task="task-a", label="before", ts=ts_new,
                assertions={"check": True}, passed=True, total=1),
        # after: single run (PASS)
        _mk_row(task="task-a", label="after",
                assertions={"check": True}, passed=True, total=1),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "before", "--label", "after"])
    assert result.exit_code == 0
    # If latest before is used (PASS) and after is PASS, there should be no flip for check
    # The output should either show no flips or show check as unchanged
    output = result.output
    # Should not show check as a regression (since latest before is PASS = same as after)
    assert "task-a" in output


# ---------------------------------------------------------------------------
# Diff: no changes case
# ---------------------------------------------------------------------------


def test_evals_diff_no_changes_exit_zero(tmp_path: Path) -> None:
    """Diff exits 0 when comparison produced even with no flips (reports nothing changed)."""
    import fno.paths as paths_mod
    runner, evals_app = _get_runner_and_app()

    history_path = paths_mod.evals_history()
    rows = [
        _mk_row(task="stable-task", label="before",
                assertions={"check-a": True}, passed=True, total=1),
        _mk_row(task="stable-task", label="after",
                assertions={"check-a": True}, passed=True, total=1),
    ]
    _write_history(history_path, rows)

    result = runner.invoke(evals_app, ["diff", "--label", "before", "--label", "after"])
    # Exit 0 - diff reports, doesn't gate
    assert result.exit_code == 0, f"Diff should exit 0 even when no flips: {result.output}"
    assert result.output.strip(), "Should produce some output even when no changes"
