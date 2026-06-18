"""Tests for fno.evals.runner, fno.evals.history, and fno.evals.cli
(Task 2.1: fno evals run - workdir orchestration, history append, interrupts).

Matches the -k evals_run filter.

Covers:
- AC1-HP:   happy path: stub loop exits 0, TAP passes, one row written
- AC1-ERR:  missing loop script -> rejected upfront with nonzero exit
- AC1-ERR2: loop script exits 77 (harness-error) -> row written with harness-error reason
- AC1-UI:   header contains doctor line; summary table has isolation column
- AC1-EDGE: Budget termination + passing assertions recorded verbatim
- AC1-FR:   KeyboardInterrupt mid-task -> Interrupted row + group kill
- zero-assertion failure: assert.sh emits no TAP -> task fails, row still written
- unmatched --task: exit nonzero with no rows written
- one-row-per-task invariant: running 2 tasks writes exactly 2 rows
- history O_APPEND shape: append_row writes a single newline-terminated JSON line
- no_ship: settings written to workdir contain no_ship: true
"""
from __future__ import annotations

import json
import os
import signal
import stat
import textwrap
import time
from pathlib import Path
from typing import Generator
from unittest.mock import patch, MagicMock

import shutil

import pytest

# run_tasks shells out to git (init/add/commit inside eval workdirs); skip
# the module gracefully in environments without the git CLI (gemini review,
# PR #451). Workdirs are git-inited by the runner itself, so repo membership
# of the test CWD is irrelevant - only CLI availability matters.
pytestmark = pytest.mark.skipif(
    shutil.which("git") is None,
    reason="git CLI not available",
)


# ---------------------------------------------------------------------------
# Helpers shared across tests
# ---------------------------------------------------------------------------


def _write_fixture(
    base: Path,
    *,
    slug: str = "test-task",
    task_yaml: str | None = None,
    plan_md: str | None = None,
    assert_sh_content: str | None = None,
    test_file: str | None = None,
) -> Path:
    """Write a minimal valid fixture directory structure for run tests."""
    fx = base / slug
    fx.mkdir(parents=True)
    repo = fx / "repo"
    repo.mkdir()

    if task_yaml is None:
        task_yaml = (
            "title: Test Task\n"
            "tags: [test]\n"
            "budget_usd: 1.0\n"
            "max_iterations: 3\n"
            "timeout_secs: 60\n"
        )
    if plan_md is None:
        plan_md = "---\nstatus: ready\n---\n# Test Task\n## Goal\nDo the thing.\n"
    if assert_sh_content is None:
        assert_sh_content = "#!/usr/bin/env bash\necho 'ok check-passes'\n"
    if test_file is None:
        test_file = "def test_placeholder():\n    assert True\n"

    (fx / "task.yaml").write_text(task_yaml)
    (fx / "plan.md").write_text(plan_md)
    assert_sh = fx / "assert.sh"
    assert_sh.write_text(assert_sh_content)
    assert_sh.chmod(assert_sh.stat().st_mode | stat.S_IEXEC)
    (repo / "test_placeholder.py").write_text(test_file)
    return fx


def _write_stub_loop(
    tmp_path: Path,
    *,
    script_name: str = "stub-loop.sh",
    exit_code: int = 0,
    termination_reason: str | None = "DoneAdvisory",
    sleep_secs: float | None = None,
) -> Path:
    """Write a stub loop script that injects a termination event and exits.

    Args:
        exit_code: The exit code for the script.
        termination_reason: If set, writes a termination event to
            .fno/events.jsonl inside CWD. None skips the event.
        sleep_secs: If set, sleep this many seconds before exiting (for
            timeout/interrupt tests).
    """
    lines = ["#!/usr/bin/env bash", "set -e", "mkdir -p .fno"]

    if termination_reason is not None:
        event_json = json.dumps({
            "type": "termination",
            "data": {"reason": termination_reason},
            "ts": "2026-06-05T00:00:00Z",
        })
        lines.append(f"echo '{event_json}' >> .fno/events.jsonl")

    if sleep_secs is not None:
        lines.append(f"sleep {sleep_secs}")

    lines.append(f"exit {exit_code}")
    script = tmp_path / script_name
    script.write_text("\n".join(lines) + "\n")
    script.chmod(script.stat().st_mode | stat.S_IEXEC)
    return script


@pytest.fixture(autouse=True)
def _isolate_history(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Point evals_history() at a tmp file so tests never touch real state."""
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)
    yield


def _read_history_rows(path: Path) -> list[dict]:
    """Read all JSON rows from a history jsonl file."""
    if not path.exists():
        return []
    rows = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if line:
            rows.append(json.loads(line))
    return rows


# ---------------------------------------------------------------------------
# AC-HISTORY: history.append_row - O_APPEND single-line row shape
# ---------------------------------------------------------------------------


def test_evals_history_append_row_single_line(tmp_path: Path) -> None:
    """AC-HISTORY-HP: append_row writes a single newline-terminated JSON line."""
    from fno.evals.history import append_row

    path = tmp_path / "test-history.jsonl"
    row = {"ts": "2026-06-05T00:00:00Z", "task": "my-task", "passed": True}
    append_row(path, row)

    content = path.read_text()
    lines = content.splitlines()
    assert len(lines) == 1, f"Expected 1 line, got {len(lines)}: {content!r}"
    parsed = json.loads(lines[0])
    assert parsed == row


def test_evals_history_append_row_multiple(tmp_path: Path) -> None:
    """AC-HISTORY-HP: multiple append_row calls produce one row per line."""
    from fno.evals.history import append_row

    path = tmp_path / "test-history.jsonl"
    append_row(path, {"task": "a", "n": 1})
    append_row(path, {"task": "b", "n": 2})
    append_row(path, {"task": "c", "n": 3})

    rows = _read_history_rows(path)
    assert len(rows) == 3
    assert [r["task"] for r in rows] == ["a", "b", "c"]


def test_evals_history_append_row_creates_parent(tmp_path: Path) -> None:
    """AC-HISTORY-HP: append_row creates parent directories if needed."""
    from fno.evals.history import append_row

    path = tmp_path / "deep" / "nested" / "history.jsonl"
    append_row(path, {"task": "x"})
    assert path.exists()


# ---------------------------------------------------------------------------
# AC1-HP: happy path - stub loop exits 0 with DoneAdvisory termination
# ---------------------------------------------------------------------------


def test_evals_run_happy_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: stub loop exits 0, assert.sh emits ok, one row written with passed=True."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    fx = _write_fixture(golden, slug="happy-task")
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    history_path = paths_mod.evals_history()
    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="test-run",
        model="claude-test",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    rows = _read_history_rows(history_path)
    assert len(rows) == 1, f"Expected 1 row, got {len(rows)}"
    row = rows[0]
    assert row["task"] == "happy-task"
    assert row["termination_reason"] == "DoneAdvisory"
    assert row["passed"] is True
    assert row["total"] == 1
    assert "ts" in row
    assert row["driver"] == "claude-code"
    # Task 2.2: isolation is now computed ("clean" or "violated"), not hardcoded "unknown"
    assert row["isolation"] in ("clean", "violated", "unknown")


def test_evals_run_happy_path_workdir_cleaned(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: workdir is removed after a successful run (keep_workdir=False)."""
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="clean-task")
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    kept_dirs: list[Path] = []

    original_run = None

    def capture_workdir(
        task,
        workdir,
        *,
        loop_script,
        label,
        model,
        keep_workdir,
        **kwargs,
    ):
        kept_dirs.append(workdir)
        return original_run(
            task,
            workdir,
            loop_script=loop_script,
            label=label,
            model=model,
            keep_workdir=keep_workdir,
            **kwargs,
        )

    # Just verify the row says workdir_kept is None on success
    import fno.paths as paths_mod
    history_path = paths_mod.evals_history()
    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="test-clean",
        model="claude-test",
        keep_workdir=False,
        loop_script=stub_loop,
    )
    rows = _read_history_rows(history_path)
    assert rows[0]["workdir_kept"] is None


# ---------------------------------------------------------------------------
# AC1-ERR: missing loop script -> reject upfront, no rows written
# ---------------------------------------------------------------------------


def test_evals_run_missing_loop_script(tmp_path: Path) -> None:
    """AC1-ERR: missing loop script -> nonzero exit, no history rows written."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks, RunnerError

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="err-task")
    missing_script = tmp_path / "nonexistent-loop.sh"

    with pytest.raises(RunnerError, match="loop script"):
        run_tasks(
            fixtures_dir=golden,
            task_slug=None,
            label="test",
            model="m",
            keep_workdir=False,
            loop_script=missing_script,
        )

    history_path = paths_mod.evals_history()
    rows = _read_history_rows(history_path)
    assert len(rows) == 0, "No rows should be written when loop script is missing"


def test_evals_run_non_executable_loop_script(tmp_path: Path) -> None:
    """AC1-ERR: non-executable loop script -> RunnerError, no rows written."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks, RunnerError

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="non-exec-task")
    script = tmp_path / "not-executable.sh"
    script.write_text("#!/bin/bash\nexit 0\n")
    # No execute bit set

    with pytest.raises(RunnerError, match="loop script"):
        run_tasks(
            fixtures_dir=golden,
            task_slug=None,
            label="test",
            model="m",
            keep_workdir=False,
            loop_script=script,
        )

    history_path = paths_mod.evals_history()
    assert len(_read_history_rows(history_path)) == 0


# ---------------------------------------------------------------------------
# AC1-ERR2: loop script exits 77 (harness-error) -> row written
# ---------------------------------------------------------------------------


def test_evals_run_loop_exit_77(tmp_path: Path) -> None:
    """AC1-ERR2: loop exits 77 (claude CLI missing) -> row with harness-error reason."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="harness-err-task")
    # exit 77 but no termination event
    stub_loop = _write_stub_loop(
        tmp_path,
        exit_code=77,
        termination_reason=None,
    )

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="harness-test",
        model="m",
        keep_workdir=True,
        loop_script=stub_loop,
    )

    history_path = paths_mod.evals_history()
    rows = _read_history_rows(history_path)
    assert len(rows) == 1
    row = rows[0]
    assert row["termination_reason"] == "harness-error"
    assert row["workdir_kept"] is not None  # kept on failure


# ---------------------------------------------------------------------------
# AC1-UI: header contains doctor line; summary has isolation column
# ---------------------------------------------------------------------------


def test_evals_run_header_has_doctor_line(tmp_path: Path, capsys: pytest.CaptureFixture) -> None:
    """AC1-UI: run output includes a doctor: line in the header."""
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="ui-task")
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="ui-test",
        model="m",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    captured = capsys.readouterr()
    output = captured.out + captured.err
    assert "doctor:" in output, f"Expected 'doctor:' in output:\n{output}"


def test_evals_run_summary_has_isolation_column(tmp_path: Path, capsys: pytest.CaptureFixture) -> None:
    """AC1-UI: summary table contains an 'isolation' column header."""
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="isolation-task")
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="iso-test",
        model="m",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    captured = capsys.readouterr()
    output = captured.out + captured.err
    assert "isolation" in output.lower(), f"Expected 'isolation' in summary:\n{output}"


# ---------------------------------------------------------------------------
# AC1-EDGE: Budget termination + passing assertions recorded verbatim
# ---------------------------------------------------------------------------


def test_evals_run_budget_termination_recorded(tmp_path: Path) -> None:
    """AC1-EDGE: Budget termination still records assertions and reason verbatim."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(
        golden,
        slug="budget-task",
        assert_sh_content="#!/usr/bin/env bash\necho 'ok budget-check-a'\necho 'ok budget-check-b'\n",
    )
    stub_loop = _write_stub_loop(tmp_path, termination_reason="Budget", exit_code=0)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="budget-test",
        model="m",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    history_path = paths_mod.evals_history()
    rows = _read_history_rows(history_path)
    assert len(rows) == 1
    row = rows[0]
    assert row["termination_reason"] == "Budget"
    assert row["total"] == 2
    assert row["passed"] is True
    assertions = row["assertions"]
    assert assertions["budget-check-a"] is True
    assert assertions["budget-check-b"] is True


def test_evals_run_noprogress_termination_recorded(tmp_path: Path) -> None:
    """AC1-EDGE: NoProgress termination still records assertions verbatim."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(
        golden,
        slug="noprogress-task",
        assert_sh_content="#!/usr/bin/env bash\necho 'not ok check-failed'\n",
    )
    stub_loop = _write_stub_loop(tmp_path, termination_reason="NoProgress", exit_code=1)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="noprog-test",
        model="m",
        keep_workdir=True,
        loop_script=stub_loop,
    )

    history_path = paths_mod.evals_history()
    rows = _read_history_rows(history_path)
    assert rows[0]["termination_reason"] == "NoProgress"
    assert rows[0]["assertions"]["check-failed"] is False
    assert rows[0]["passed"] is False


# ---------------------------------------------------------------------------
# AC1-FR: KeyboardInterrupt mid-task -> Interrupted row + group kill
# ---------------------------------------------------------------------------


def test_evals_run_keyboard_interrupt_writes_row(tmp_path: Path) -> None:
    """AC1-FR: KeyboardInterrupt during loop -> Interrupted row written, exit nonzero."""
    import fno.paths as paths_mod
    import fno.evals.runner as runner_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="interrupt-task")

    # A stub loop that sleeps - we'll patch _run_single_task to raise KeyboardInterrupt
    stub_loop = _write_stub_loop(
        tmp_path,
        sleep_secs=60,
        termination_reason=None,
        exit_code=0,
    )

    # Patch the loop invocation inside _run_single_task to simulate KeyboardInterrupt
    # by replacing Popen only when running the eval loop (not git subprocess calls).
    # We use a counter to allow the first N subprocess.Popen calls (git operations)
    # through and raise KeyboardInterrupt on the loop invocation.
    import subprocess as subprocess_mod

    real_popen = subprocess_mod.Popen
    call_count = [0]

    class _PatchedPopen:
        def __new__(cls, cmd, *args, **kwargs):
            call_count[0] += 1
            # The loop script call uses start_new_session=True; detect it
            if kwargs.get("start_new_session"):
                raise KeyboardInterrupt
            return real_popen(cmd, *args, **kwargs)

    with patch("subprocess.Popen", _PatchedPopen):
        with pytest.raises((SystemExit, KeyboardInterrupt)):
            run_tasks(
                fixtures_dir=golden,
                task_slug=None,
                label="interrupt-test",
                model="m",
                keep_workdir=False,
                loop_script=stub_loop,
            )

    history_path = paths_mod.evals_history()
    rows = _read_history_rows(history_path)
    assert len(rows) == 1, f"Expected 1 row after interrupt, got {len(rows)}"
    assert rows[0]["termination_reason"] == "Interrupted"


# ---------------------------------------------------------------------------
# Zero-assertion failure: assert.sh emits no TAP -> task fails, row still written
# ---------------------------------------------------------------------------


def test_evals_run_zero_assertions_is_failure(tmp_path: Path) -> None:
    """ZERO-ASSERT: assert.sh emits no TAP lines -> task fails (row written, passed=False)."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(
        golden,
        slug="zero-assert-task",
        assert_sh_content="#!/usr/bin/env bash\n# no assertions emitted\nexit 0\n",
    )
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="zero-test",
        model="m",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    history_path = paths_mod.evals_history()
    rows = _read_history_rows(history_path)
    assert len(rows) == 1
    row = rows[0]
    assert row["passed"] is False, "Zero assertions should be treated as failure"
    assert row["total"] == 0


# ---------------------------------------------------------------------------
# Unmatched --task: exit nonzero with no rows written
# ---------------------------------------------------------------------------


def test_evals_run_unmatched_task_slug(tmp_path: Path) -> None:
    """BOUNDARIES: --task with nonexistent slug -> RunnerError, no rows written."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks, RunnerError

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="real-task")
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    with pytest.raises(RunnerError, match="no-such-task"):
        run_tasks(
            fixtures_dir=golden,
            task_slug="no-such-task",
            label="test",
            model="m",
            keep_workdir=False,
            loop_script=stub_loop,
        )

    history_path = paths_mod.evals_history()
    assert len(_read_history_rows(history_path)) == 0


# ---------------------------------------------------------------------------
# One-row-per-task invariant: 2 tasks -> exactly 2 rows
# ---------------------------------------------------------------------------


def test_evals_run_two_tasks_two_rows(tmp_path: Path) -> None:
    """INVARIANT: running 2 tasks writes exactly 2 rows to history."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="task-alpha")
    _write_fixture(golden, slug="task-beta")
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="two-task-run",
        model="m",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    history_path = paths_mod.evals_history()
    rows = _read_history_rows(history_path)
    assert len(rows) == 2
    slugs = {r["task"] for r in rows}
    assert slugs == {"task-alpha", "task-beta"}


# ---------------------------------------------------------------------------
# settings.yaml in workdir contains no_ship: true
# ---------------------------------------------------------------------------


def test_evals_run_workdir_settings_no_ship(tmp_path: Path) -> None:
    """VERIFY: workdir .fno/settings.yaml contains no_ship: true."""
    import yaml
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()

    # Capture the workdir path before cleanup
    written_settings: list[dict] = []

    _real_run_single = None

    import fno.evals.runner as runner_mod

    original_run_single = runner_mod._run_single_task  # will be set after import

    def spy_run_single(task, workdir, *, loop_script, label, model, keep_workdir, history_path, **kwargs):
        settings_path = workdir / ".fno" / "settings.yaml"
        # call the real impl first
        result = original_run_single(
            task,
            workdir,
            loop_script=loop_script,
            label=label,
            model=model,
            keep_workdir=True,  # keep so we can read it
            history_path=history_path,
            **kwargs,
        )
        if settings_path.exists():
            written_settings.append(yaml.safe_load(settings_path.read_text()))
        return result

    _write_fixture(golden, slug="no-ship-task")
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    # Run with keep_workdir=True so settings survive cleanup
    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="settings-test",
        model="m",
        keep_workdir=True,
        loop_script=stub_loop,
    )

    # The history row should record the workdir path since keep_workdir=True
    import fno.paths as paths_mod
    rows = _read_history_rows(paths_mod.evals_history())
    assert len(rows) == 1
    workdir_path = rows[0].get("workdir_kept")
    assert workdir_path is not None

    settings_file = Path(workdir_path) / ".fno" / "settings.yaml"
    assert settings_file.exists(), f"settings.yaml not found at {settings_file}"

    data = yaml.safe_load(settings_file.read_text())
    # no_ship lives in the config block
    config = data.get("config", data)  # support both flat and nested
    assert config.get("no_ship") is True or data.get("no_ship") is True, (
        f"no_ship: true not found in settings:\n{data}"
    )


# ---------------------------------------------------------------------------
# Row schema: required fields present
# ---------------------------------------------------------------------------


def test_evals_run_row_schema_fields(tmp_path: Path) -> None:
    """VERIFY: history row contains all required schema fields."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="schema-task")
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="schema-label",
        model="test-model",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    history_path = paths_mod.evals_history()
    rows = _read_history_rows(history_path)
    row = rows[0]

    required_fields = [
        "ts", "task", "label", "abilities_sha", "installed_rev",
        "model", "driver", "termination_reason", "assertions",
        "passed", "total", "tokens_total", "cost_usd", "wall_secs",
        "iterations", "session_id", "transcript_path", "isolation",
        "workdir_kept",
    ]
    for field in required_fields:
        assert field in row, f"Missing required field: {field!r}"

    assert row["label"] == "schema-label"
    assert row["model"] == "test-model"
    assert row["driver"] == "claude-code"
    # Task 2.2: isolation is now computed ("clean" or "violated"), not hardcoded "unknown"
    assert row["isolation"] in ("clean", "violated", "unknown")


# ---------------------------------------------------------------------------
# --task filter: only the matching task runs
# ---------------------------------------------------------------------------


def test_evals_run_task_filter(tmp_path: Path) -> None:
    """VERIFY: --task slug runs only the matching fixture, not others."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="alpha")
    _write_fixture(golden, slug="beta")
    _write_fixture(golden, slug="gamma")
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    run_tasks(
        fixtures_dir=golden,
        task_slug="beta",
        label="filter-test",
        model="m",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    history_path = paths_mod.evals_history()
    rows = _read_history_rows(history_path)
    assert len(rows) == 1
    assert rows[0]["task"] == "beta"


# ---------------------------------------------------------------------------
# failing assertion recorded
# ---------------------------------------------------------------------------


def test_evals_run_failing_assertion_row(tmp_path: Path) -> None:
    """VERIFY: failing TAP assertion is recorded as False in assertions dict."""
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(
        golden,
        slug="fail-task",
        assert_sh_content="#!/usr/bin/env bash\necho 'ok check-a'\necho 'not ok check-b'\n",
    )
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="fail-test",
        model="m",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    history_path = paths_mod.evals_history()
    rows = _read_history_rows(history_path)
    row = rows[0]
    assert row["assertions"]["check-a"] is True
    assert row["assertions"]["check-b"] is False
    assert row["passed"] is False
    assert row["total"] == 2


# ---------------------------------------------------------------------------
# Smoke-sweep regressions (2026-06-05): migration sentinel, PYTHON env,
# FNO_SKIP_MIGRATION in the loop env. See docs/architecture/efficacy-evals.md.
# ---------------------------------------------------------------------------


def test_evals_run_workdir_settings_migration_sentinel(tmp_path: Path) -> None:
    """REGRESSION: _write_workdir_settings drops .path-migration-done.

    Without the sentinel, the first ``fno`` invocation inside the eval
    session runs _check_migration() and REWRITES the settings fragment,
    nulling the isolation overrides (observed in the first live smoke
    sweep).
    """
    from fno.evals.runner import _write_workdir_settings

    _write_workdir_settings(tmp_path)

    sentinel = tmp_path / ".fno" / ".path-migration-done"
    assert sentinel.exists(), "migration sentinel missing from workdir fragment"


def test_evals_run_assert_sh_receives_python_env(tmp_path: Path) -> None:
    """REGRESSION: assert.sh subprocess env carries PYTHON=sys.executable.

    Fixture assert.sh scripts invoke "${PYTHON:-python3}"; the ambient
    python3 may lack pytest, so the runner must inject the interpreter
    that runs it (which has pytest).
    """
    import sys as _sys

    from fno.evals.fixtures import load_task
    from fno.evals.runner import _run_assert_sh

    fx = _write_fixture(
        tmp_path / "golden",
        slug="python-env-task",
        assert_sh_content=(
            "#!/usr/bin/env bash\n"
            'if [ "${PYTHON:-}" = "' + _sys.executable + '" ]; then\n'
            "  echo 'ok python_env'\n"
            "else\n"
            "  echo 'not ok python_env'\n"
            "fi\n"
        ),
    )
    task = load_task(fx)
    workdir = tmp_path / "wd"
    workdir.mkdir()

    assertions, _output = _run_assert_sh(task, workdir)

    assert len(assertions) == 1
    assert assertions[0].name == "python_env"
    assert assertions[0].ok, "PYTHON env var not injected into assert.sh"


def test_evals_run_loop_env_skips_migration(tmp_path: Path) -> None:
    """REGRESSION: the loop subprocess env carries FNO_SKIP_MIGRATION=1."""
    from fno.evals.runner import run_tasks
    import fno.paths as paths_mod

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="env-probe-task")

    # Stub loop that records the env var, emits a termination event, exits 0.
    script = tmp_path / "env-probe-loop.sh"
    script.write_text(
        "#!/usr/bin/env bash\n"
        "set -e\n"
        "mkdir -p .fno\n"
        'echo "${FNO_SKIP_MIGRATION:-unset}" > abi-skip-migration.txt\n'
        "echo '" + json.dumps({
            "type": "termination",
            "data": {"reason": "DoneAdvisory"},
            "ts": "2026-06-05T00:00:00Z",
        }) + "' >> .fno/events.jsonl\n"
        "exit 0\n"
    )
    script.chmod(script.stat().st_mode | stat.S_IEXEC)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="env-probe",
        model="m",
        keep_workdir=True,
        loop_script=script,
    )

    rows = _read_history_rows(paths_mod.evals_history())
    assert len(rows) == 1
    workdir_kept = rows[0].get("workdir_kept")
    assert workdir_kept is not None
    probe = Path(workdir_kept) / "abi-skip-migration.txt"
    assert probe.exists(), "stub loop never ran or probe file missing"
    assert probe.read_text().strip() == "1", "FNO_SKIP_MIGRATION=1 not in loop env"


def test_evals_run_cost_lookup_uses_transcript_uuid(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """REGRESSION: session-cost.py is called with the transcript UUID.

    session-cost.py resolves transcripts by Claude transcript UUID (the
    .jsonl filename stem), not by the fno-internal session id from
    events.jsonl.  The first smoke sweep passed the wrong id and every
    row carried null tokens/cost.
    """
    import fno.evals.runner as runner_mod

    # Workdir with an events.jsonl carrying an fno-internal id.
    workdir = tmp_path / "wd"
    (workdir / ".fno").mkdir(parents=True)
    (workdir / ".fno" / "events.jsonl").write_text(
        json.dumps({"type": "x", "session_id": "20260606T000000Z-1-abc"}) + "\n"
    )

    captured_cmds: list[list[str]] = []

    class _FakeResult:
        returncode = 0
        stdout = json.dumps({"cost_usd": 1.23, "tokens": {"total": 456}})

    real_run = runner_mod.subprocess.run

    def fake_run(cmd, **kwargs):
        # Only fake the _session_cost invocation; let git etc. through
        # (_find_repo_root shells out to git rev-parse).
        if any("_session_cost" in str(c) for c in cmd):
            captured_cmds.append([str(c) for c in cmd])
            return _FakeResult()
        return real_run(cmd, **kwargs)

    monkeypatch.setattr(runner_mod.subprocess, "run", fake_run)

    uuid = "02cc0ffe-352d-4224-bca8-84eafe2feabc"
    tokens, cost, session_id = runner_mod._get_cost(workdir, uuid)

    assert tokens == 456
    assert cost == 1.23
    assert session_id == "20260606T000000Z-1-abc"
    assert captured_cmds, "session-cost.py was never invoked"
    assert uuid in captured_cmds[0], (
        f"expected transcript UUID in cmd, got: {captured_cmds[0]}"
    )

    # Without a transcript UUID the lookup is skipped entirely.
    captured_cmds.clear()
    tokens, cost, session_id = runner_mod._get_cost(workdir, None)
    assert tokens is None and cost is None
    assert session_id == "20260606T000000Z-1-abc"
    assert not captured_cmds, "session-cost.py must not be called without a UUID"


def test_evals_run_loop_env_scrubs_config_pins(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """REGRESSION (codex P2, PR #451): FNO_CONFIG / FNO_REPO_ROOT
    inherited from the operator's shell must NOT reach the loop subprocess.

    Either var pins fno.config resolution away from the eval
    workdir's settings fragment, silently bypassing the preventive
    isolation layer.
    """
    from fno.evals.runner import run_tasks

    monkeypatch.setenv("FNO_CONFIG", "/somewhere/else/settings.yaml")
    monkeypatch.setenv("FNO_REPO_ROOT", "/somewhere/else")

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="env-scrub-task")

    # NOTE: history_path is injected explicitly. With FNO_CONFIG set,
    # the runner process itself resolves paths.evals_history() through the
    # pinned (nonexistent) config -> defaults -> the REAL global history
    # file. That resolution pinning is exactly why the subprocess env must
    # be scrubbed; explicit injection keeps this test hermetic.
    history_path = tmp_path / "env-scrub-history.jsonl"

    script = tmp_path / "env-scrub-loop.sh"
    script.write_text(
        "#!/usr/bin/env bash\n"
        "set -e\n"
        "mkdir -p .fno\n"
        'echo "${FNO_CONFIG:-unset}" > fno-config.txt\n'
        'echo "${FNO_REPO_ROOT:-unset}" > abi-repo-root.txt\n'
        "echo '" + json.dumps({
            "type": "termination",
            "data": {"reason": "DoneAdvisory"},
            "ts": "2026-06-05T00:00:00Z",
        }) + "' >> .fno/events.jsonl\n"
        "exit 0\n"
    )
    script.chmod(script.stat().st_mode | stat.S_IEXEC)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="env-scrub",
        model="m",
        keep_workdir=True,
        loop_script=script,
        history_path=history_path,
    )

    rows = _read_history_rows(history_path)
    assert len(rows) == 1
    wd = Path(rows[0]["workdir_kept"])
    assert (wd / "fno-config.txt").read_text().strip() == "unset", (
        "FNO_CONFIG leaked into the loop subprocess env"
    )
    assert (wd / "abi-repo-root.txt").read_text().strip() == "unset", (
        "FNO_REPO_ROOT leaked into the loop subprocess env"
    )
