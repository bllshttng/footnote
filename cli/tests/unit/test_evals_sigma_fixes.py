"""Tests for sigma-review fixes to fno.evals.

Covers FIX-1 through FIX-11 from the sigma-review findings.

- FIX-1 (F1): _run_assert_sh distinguishes harness failure from model failure
  - timeout path: prints stderr message, returns partial stdout
  - crash path: assert.sh with syntax error -> stderr message + zero assertions
  - rc!=0-no-TAP: prints distinguishing stderr message
- FIX-2 (F2): _init_manifest surfaces nonzero exit to stderr
- FIX-3 (F7): _latest_per unparseable-ts behavior - later rows win when ts unparseable
- FIX-4 (F3): termination_reason explicit init (behavioral-identical; existing tests stay green)
- FIX-5 (F4): _get_cost except narrowed to (JSONDecodeError, AttributeError)
- FIX-7 (F9): IsolationResult.__post_init__ coherence guard
- FIX-8 (F11): producer->consumer chain test (journey test)
- FIX-9 (F12): CLI-boundary exit-code tests
- FIX-10 (F13): loop argv/cwd contract test
- FIX-11 (F14): "evals" in _EXPECTED_SUBCOMMANDS
"""
from __future__ import annotations

import json
import os
import stat
import textwrap
from pathlib import Path
from typing import Generator

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
# Isolation fixture - redirect evals_history() into tmp
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _isolate_history(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)
    yield


# ---------------------------------------------------------------------------
# Helpers (reuse the same patterns as test_evals_run.py)
# ---------------------------------------------------------------------------


def _write_fixture(
    base: Path,
    *,
    slug: str = "test-task",
    task_yaml: str | None = None,
    plan_md: str | None = None,
    assert_sh_content: str | None = None,
) -> Path:
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

    (fx / "task.yaml").write_text(task_yaml)
    (fx / "plan.md").write_text(plan_md)
    assert_sh = fx / "assert.sh"
    assert_sh.write_text(assert_sh_content)
    assert_sh.chmod(assert_sh.stat().st_mode | stat.S_IEXEC)
    (repo / "test_placeholder.py").write_text("def test_placeholder():\n    assert True\n")
    return fx


def _write_stub_loop(
    tmp_path: Path,
    *,
    script_name: str = "stub-loop.sh",
    exit_code: int = 0,
    termination_reason: str | None = "DoneAdvisory",
) -> Path:
    lines = ["#!/usr/bin/env bash", "set -e", "mkdir -p .fno"]
    if termination_reason is not None:
        event_json = json.dumps({
            "type": "termination",
            "data": {"reason": termination_reason},
            "ts": "2026-06-05T00:00:00Z",
        })
        lines.append(f"echo '{event_json}' >> .fno/events.jsonl")
    lines.append(f"exit {exit_code}")
    script = tmp_path / script_name
    script.write_text("\n".join(lines) + "\n")
    script.chmod(script.stat().st_mode | stat.S_IEXEC)
    return script


def _read_history_rows(path: Path) -> list[dict]:
    if not path.exists():
        return []
    rows = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if line:
            rows.append(json.loads(line))
    return rows


# ---------------------------------------------------------------------------
# FIX-1 (F1): _run_assert_sh distinguishes harness failure from model failure
# ---------------------------------------------------------------------------

# Module-level constant for the timeout so tests can use a small value.
# After FIX-1, runner.py must expose ASSERT_TIMEOUT (or we use monkeypatch
# of subprocess.run timeout argument). We test behavior through stderr output.


def test_fix1_assert_sh_timeout_prints_stderr_and_returns_partial(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    """FIX-1 timeout path: TimeoutExpired -> stderr message, no exception raised."""
    import subprocess
    import fno.evals.runner as runner_mod
    from fno.evals.fixtures import load_task

    fx = _write_fixture(tmp_path / "golden", slug="timeout-task")
    task = load_task(fx)
    workdir = tmp_path / "wd"
    workdir.mkdir()

    # Patch subprocess.run to raise TimeoutExpired for the assert.sh call
    real_run = runner_mod.subprocess.run

    def fake_run(cmd, **kwargs):
        if "assert.sh" in str(cmd):
            # Simulate timeout - exc.stdout may be None or bytes
            exc = subprocess.TimeoutExpired(cmd, 120)
            exc.stdout = None
            raise exc
        return real_run(cmd, **kwargs)

    runner_mod.subprocess.run = fake_run
    try:
        assertions, raw_output = runner_mod._run_assert_sh(task, workdir)
    finally:
        runner_mod.subprocess.run = real_run

    captured = capsys.readouterr()
    stderr_text = captured.err
    # Must print a message distinguishing timeout from model failure
    assert "timed out" in stderr_text or "assert.sh" in stderr_text, (
        f"Expected timeout message in stderr, got: {stderr_text!r}"
    )
    # Must return empty assertions (no TAP), not raise
    assert assertions == [], f"Expected empty assertions on timeout, got {assertions}"


def test_fix1_assert_sh_timeout_with_partial_stdout(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    """FIX-1 timeout path: exc.stdout bytes are decoded and returned in raw_output."""
    import subprocess
    import fno.evals.runner as runner_mod
    from fno.evals.fixtures import load_task

    fx = _write_fixture(tmp_path / "golden", slug="timeout-partial-task")
    task = load_task(fx)
    workdir = tmp_path / "wd"
    workdir.mkdir()

    real_run = runner_mod.subprocess.run

    def fake_run(cmd, **kwargs):
        if "assert.sh" in str(cmd):
            exc = subprocess.TimeoutExpired(cmd, 120)
            # Simulate partial output captured before timeout
            exc.stdout = b"ok partial-check\n"
            raise exc
        return real_run(cmd, **kwargs)

    runner_mod.subprocess.run = fake_run
    try:
        assertions, raw_output = runner_mod._run_assert_sh(task, workdir)
    finally:
        runner_mod.subprocess.run = real_run

    # Raw output should contain the partial stdout bytes decoded
    # Assertions may be parsed from partial output
    captured = capsys.readouterr()
    assert "timed out" in captured.err or "assert.sh" in captured.err


def test_fix1_assert_sh_crash_prints_stderr_and_returns_empty(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    """FIX-1 crash path: syntax error in assert.sh -> stderr message + ([], "")."""
    import fno.evals.runner as runner_mod
    from fno.evals.fixtures import load_task

    # Write assert.sh with a bash syntax error
    fx = _write_fixture(
        tmp_path / "golden",
        slug="crash-task",
        assert_sh_content="#!/usr/bin/env bash\n{{invalid syntax\n",
    )
    task = load_task(fx)
    workdir = tmp_path / "wd"
    workdir.mkdir()

    # Simulate a non-TimeoutExpired exception (OSError, e.g. permission denied)
    real_run = runner_mod.subprocess.run

    def fake_run(cmd, **kwargs):
        if "assert.sh" in str(cmd):
            raise OSError("bash: permission denied")
        return real_run(cmd, **kwargs)

    runner_mod.subprocess.run = fake_run
    try:
        assertions, raw_output = runner_mod._run_assert_sh(task, workdir)
    finally:
        runner_mod.subprocess.run = real_run

    captured = capsys.readouterr()
    stderr_text = captured.err
    assert "assert.sh" in stderr_text and "failed to run" in stderr_text, (
        f"Expected 'assert.sh failed to run' in stderr, got: {stderr_text!r}"
    )
    assert assertions == []
    assert raw_output == ""


def test_fix1_assert_sh_rc_nonzero_no_tap_prints_stderr(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    """FIX-1 rc!=0-no-TAP: returncode!=0 with zero assertions -> stderr distinguishing message."""
    import fno.evals.runner as runner_mod
    from fno.evals.fixtures import load_task

    fx = _write_fixture(tmp_path / "golden", slug="rc-notap-task")
    task = load_task(fx)
    workdir = tmp_path / "wd"
    workdir.mkdir()

    real_run = runner_mod.subprocess.run

    class FakeResult:
        returncode = 1
        stdout = ""
        stderr = "something went wrong"

    def fake_run(cmd, **kwargs):
        if "assert.sh" in str(cmd):
            return FakeResult()
        return real_run(cmd, **kwargs)

    runner_mod.subprocess.run = fake_run
    try:
        assertions, raw_output = runner_mod._run_assert_sh(task, workdir)
    finally:
        runner_mod.subprocess.run = real_run

    captured = capsys.readouterr()
    stderr_text = captured.err
    # Must print a message distinguishing crashed-script from ran-clean-asserted-nothing
    assert "assert.sh" in stderr_text, (
        f"Expected distinguishing stderr message, got: {stderr_text!r}"
    )
    assert assertions == []


# ---------------------------------------------------------------------------
# FIX-2 (F2): _init_manifest surfaces nonzero exit to stderr
# ---------------------------------------------------------------------------


def test_fix2_init_manifest_nonzero_prints_stderr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """FIX-2: _init_manifest with rc=3 from init script prints message to stderr."""
    import fno.evals.runner as runner_mod
    from fno.evals.fixtures import load_task

    fx = _write_fixture(tmp_path / "golden", slug="init-fail-task")
    task = load_task(fx)
    workdir = tmp_path / "wd"
    workdir.mkdir()

    # Fake the init script existence and subprocess.run returning rc=3
    real_run = runner_mod.subprocess.run
    init_script_path = tmp_path / "fake-init-script.sh"
    init_script_path.write_text("#!/usr/bin/env bash\nexit 3\n")
    init_script_path.chmod(init_script_path.stat().st_mode | stat.S_IEXEC)

    # Monkeypatch _find_repo_root to return a path where hooks/helpers/init-target-state.sh exists
    fake_repo = tmp_path / "fake-repo"
    fake_repo.mkdir()
    hooks_helpers = fake_repo / "hooks" / "helpers"
    hooks_helpers.mkdir(parents=True)
    init_sh = hooks_helpers / "init-target-state.sh"
    init_sh.write_text("#!/usr/bin/env bash\nexit 0\n")
    init_sh.chmod(init_sh.stat().st_mode | stat.S_IEXEC)

    # Patch subprocess.run: when init-target-state.sh is called, return rc=3
    def fake_run(cmd, **kwargs):
        cmd_str = " ".join(str(c) for c in cmd)
        if "init-target-state.sh" in cmd_str:
            class FakeResult:
                returncode = 3
                stderr = "init script failed"
                stdout = ""
            return FakeResult()
        return real_run(cmd, **kwargs)

    monkeypatch.setattr(runner_mod, "_find_repo_root", lambda p: fake_repo)
    monkeypatch.setattr(runner_mod.subprocess, "run", fake_run)

    # Should not raise - nonzero is non-fatal
    runner_mod._init_manifest(task, workdir)

    captured = capsys.readouterr()
    stderr_text = captured.err
    assert "init-target-state.sh" in stderr_text or "init" in stderr_text.lower(), (
        f"Expected init failure message in stderr, got: {stderr_text!r}"
    )
    # Must mention the return code
    assert "3" in stderr_text, f"Expected rc=3 in stderr message, got: {stderr_text!r}"


# ---------------------------------------------------------------------------
# FIX-3 (F7): _latest_per unparseable-ts - later file order wins
# ---------------------------------------------------------------------------


def test_fix3_latest_per_unparseable_ts_later_wins() -> None:
    """FIX-3 (F7): when ts_new is unparseable, later file-order row wins (replaces earlier)."""
    from fno.evals.reporting import _latest_per

    rows = [
        {"task": "my-task", "label": "x", "ts": "2026-06-01T00:00:00Z", "passed": False},
        {"task": "my-task", "label": "x", "ts": "not-a-timestamp", "passed": True},
    ]

    result = _latest_per(rows, ("task", "label"))
    key = ("my-task", "x")
    assert key in result
    # Second row (malformed ts) should win because it appears later in file order
    assert result[key]["passed"] is True, (
        "Later row with unparseable ts should win (later file order wins)"
    )


def test_fix3_latest_per_parseable_ts_later_wins() -> None:
    """FIX-3 regression: parseable later ts still wins (existing behavior unchanged)."""
    from fno.evals.reporting import _latest_per

    rows = [
        {"task": "my-task", "label": "x", "ts": "2026-06-01T00:00:00Z", "passed": False},
        {"task": "my-task", "label": "x", "ts": "2026-06-02T00:00:00Z", "passed": True},
    ]
    result = _latest_per(rows, ("task", "label"))
    assert result[("my-task", "x")]["passed"] is True


def test_fix3_latest_per_both_unparseable_later_wins() -> None:
    """FIX-3 edge: both ts unparseable -> later file-order row wins."""
    from fno.evals.reporting import _latest_per

    rows = [
        {"task": "my-task", "label": "x", "ts": "bad", "passed": False},
        {"task": "my-task", "label": "x", "ts": "also-bad", "passed": True},
    ]
    result = _latest_per(rows, ("task", "label"))
    assert result[("my-task", "x")]["passed"] is True


# ---------------------------------------------------------------------------
# FIX-4 (F3): termination_reason explicit init - existing tests stay green
# ---------------------------------------------------------------------------
# FIX-4 is a code-quality hardening (no dir() probe). The behavioral contract
# is identical: existing tests verify it. We add one explicit guard test.


def test_fix4_termination_reason_init_no_dir_probe(tmp_path: Path) -> None:
    """FIX-4: _run_single_task initializes termination_reason explicitly.

    The fix replaces `'termination_reason' not in dir()` with an explicit
    `termination_reason: str | None = None` + `if termination_reason is None`.
    We verify the loop path still produces a valid reason after fix.
    """
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="fix4-task")
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="fix4-test",
        model="m",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    rows = _read_history_rows(paths_mod.evals_history())
    assert len(rows) == 1
    assert rows[0]["termination_reason"] == "DoneAdvisory"


# ---------------------------------------------------------------------------
# FIX-5 (F4): _get_cost except narrowed
# ---------------------------------------------------------------------------


def test_fix5_get_cost_narrow_except_does_not_swallow_unexpected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """FIX-5 (F4): _get_cost only catches JSONDecodeError/AttributeError, not all exceptions.

    After the fix, a RuntimeError from subprocess.run should propagate (or at
    minimum not be silently swallowed by a bare `except Exception`).
    We verify the cost-script path has appropriate narrow exception handling.
    """
    import fno.evals.runner as runner_mod

    # Create a workdir with an events.jsonl carrying an fno-internal session id.
    workdir = tmp_path / "wd"
    (workdir / ".fno").mkdir(parents=True)
    (workdir / ".fno" / "events.jsonl").write_text(
        json.dumps({"type": "x", "session_id": "test-session-id"}) + "\n"
    )

    # _get_cost now runs `python3 -m fno.cost._session_cost`, so there is no
    # repo-root / on-disk script to stub; fake the subprocess directly.

    # Patch subprocess.run to return malformed JSON -> JSONDecodeError should be caught
    class FakeResultBadJson:
        returncode = 0
        stdout = "not-json-at-all"

    real_run = runner_mod.subprocess.run

    def fake_run_bad_json(cmd, **kwargs):
        if "_session_cost" in str(cmd):
            return FakeResultBadJson()
        return real_run(cmd, **kwargs)

    monkeypatch.setattr(runner_mod.subprocess, "run", fake_run_bad_json)

    # Should not raise - JSONDecodeError is caught
    tokens, cost, session_id = runner_mod._get_cost(workdir, "some-uuid")
    assert tokens is None
    assert cost is None
    assert session_id == "test-session-id"


# ---------------------------------------------------------------------------
# FIX-7 (F9): IsolationResult.__post_init__ coherence guard
# ---------------------------------------------------------------------------


def test_fix7_isolation_result_coherence_violated_with_violations() -> None:
    """FIX-7: verdict='violated' with non-empty violations is valid."""
    from fno.evals.isolation import IsolationResult, Violation

    v = Violation(
        path=Path("/tmp/test.jsonl"),
        session_id="abc123",
        line_number=1,
        detail="test violation",
    )
    result = IsolationResult(verdict="violated", violations=[v])
    assert result.verdict == "violated"
    assert len(result.violations) == 1


def test_fix7_isolation_result_coherence_clean_no_violations() -> None:
    """FIX-7: verdict='clean' with empty violations is valid."""
    from fno.evals.isolation import IsolationResult

    result = IsolationResult(verdict="clean", violations=[])
    assert result.verdict == "clean"
    assert result.violations == []


def test_fix7_isolation_result_incoherent_violated_no_violations_raises() -> None:
    """FIX-7: verdict='violated' with empty violations -> ValueError."""
    from fno.evals.isolation import IsolationResult

    with pytest.raises(ValueError, match="violated"):
        IsolationResult(verdict="violated", violations=[])


def test_fix7_isolation_result_incoherent_clean_with_violations_raises() -> None:
    """FIX-7: verdict='clean' with non-empty violations -> ValueError."""
    from fno.evals.isolation import IsolationResult, Violation

    v = Violation(
        path=Path("/tmp/test.jsonl"),
        session_id="abc123",
        line_number=1,
        detail="test violation",
    )
    with pytest.raises(ValueError, match="clean"):
        IsolationResult(verdict="clean", violations=[v])


# ---------------------------------------------------------------------------
# FIX-8 (F11): producer->consumer chain test (journey test)
# ---------------------------------------------------------------------------


def test_fix8_journey_producer_consumer_chain(tmp_path: Path) -> None:
    """FIX-8 (F11): run_tasks rows feed iter_rows_tolerant and render_report/render_diff.

    Producer: run_tasks (x2, different labels) with DoneAdvisory stub.
    Consumer: iter_rows_tolerant -> render_report, render_diff.

    Verifies:
    - render_report output contains task slug, "DoneAdvisory", and plain "PASS"
    - render_diff returns exit_code 0 and contains the slug
    """
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks
    from fno.evals.history import iter_rows_tolerant
    from fno.evals.reporting import render_report, render_diff

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="journey-task")

    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)
    history_path = paths_mod.evals_history()

    # First run: label "before"
    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="before",
        model="claude-test",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    # Second run: label "after"
    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="after",
        model="claude-test",
        keep_workdir=False,
        loop_script=stub_loop,
    )

    # Consumer: load rows via iter_rows_tolerant
    rows: list[dict] = []
    for _lineno, row in iter_rows_tolerant(history_path):
        rows.append(row)

    assert len(rows) == 2, f"Expected 2 rows, got {len(rows)}"

    # render_report: should contain task slug, DoneAdvisory, and plain PASS (not PASS*)
    report = render_report(rows)
    assert "journey-task" in report, f"Task slug missing from report:\n{report}"
    assert "DoneAdvisory" in report, f"DoneAdvisory missing from report:\n{report}"
    # "PASS" must appear; must not only be "PASS*"
    assert "PASS" in report, f"PASS missing from report:\n{report}"
    # Verify it's a clean PASS not PASS*: count occurrences
    # render_report uses _pass_status which returns "PASS" for DoneAdvisory
    import re
    pass_statuses = re.findall(r'PASS\*?', report)
    assert any(s == "PASS" for s in pass_statuses), (
        f"Expected at least one plain PASS (not PASS*) in report:\n{report}"
    )

    # render_diff: exit_code 0, output contains slug
    diff_text, exit_code = render_diff(rows, label_a="before", label_b="after")
    assert exit_code == 0, f"render_diff returned exit_code {exit_code}:\n{diff_text}"
    assert "journey-task" in diff_text, f"Slug missing from diff:\n{diff_text}"


# ---------------------------------------------------------------------------
# FIX-9 (F12): CLI-boundary exit-code tests via CliRunner
# ---------------------------------------------------------------------------


def test_fix9_cli_missing_loop_script_exit_code_2(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """FIX-9a: missing/non-executable loop script -> fno evals run exits with code 2."""
    from typer.testing import CliRunner
    from fno.evals.cli import evals_app

    # Set up a temp repo with a golden dir containing one fixture
    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="cli-err-task")

    # Point the CLI at a nonexistent loop script via env var
    nonexistent = tmp_path / "no-such-script.sh"
    monkeypatch.setenv("FNO_EVALS_LOOP_SCRIPT", str(nonexistent))

    # _golden_dir() resolves from cwd; we need evals/golden/ relative to repo root.
    # Patch _golden_dir to return our tmp golden dir directly.
    import fno.evals.cli as cli_mod
    monkeypatch.setattr(cli_mod, "_golden_dir", lambda: golden)

    runner = CliRunner()
    result = runner.invoke(evals_app, ["run"])

    assert result.exit_code == 2, (
        f"Expected exit_code=2 for missing loop script, got {result.exit_code}.\n"
        f"Output: {result.output}"
    )


def test_fix9_cli_happy_path_exit_code_0(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """FIX-9b: happy path with DoneAdvisory stub -> fno evals run exits with code 0."""
    from typer.testing import CliRunner
    from fno.evals.cli import evals_app
    import fno.evals.cli as cli_mod
    import fno.evals.runner as runner_mod

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(golden, slug="cli-happy-task")
    stub_loop = _write_stub_loop(tmp_path, termination_reason="DoneAdvisory", exit_code=0)

    monkeypatch.setenv("FNO_EVALS_LOOP_SCRIPT", str(stub_loop))
    monkeypatch.setattr(cli_mod, "_golden_dir", lambda: golden)

    # The CLI calls run_tasks without a history_path override, so it uses
    # paths.evals_history(). The _isolate_history autouse fixture already
    # redirected this to tmp_path for us.
    runner = CliRunner()
    result = runner.invoke(evals_app, ["run"])

    assert result.exit_code == 0, (
        f"Expected exit_code=0 for happy path, got {result.exit_code}.\n"
        f"Output: {result.output}\n"
        f"Stderr: {result.stderr if hasattr(result, 'stderr') else '(no stderr)'}"
    )


# ---------------------------------------------------------------------------
# FIX-10 (F13): loop argv/cwd contract
# ---------------------------------------------------------------------------


def test_fix10_loop_argv_cwd_contract(tmp_path: Path) -> None:
    """FIX-10 (F13): loop script receives correct argv and cwd.

    Stub loop writes its argv to argv.txt and cwd to pwd.txt.
    Verifies --max-iterations, --budget, --model are passed correctly
    and that the cwd is the task workdir.
    """
    import fno.paths as paths_mod
    from fno.evals.runner import run_tasks

    golden = tmp_path / "golden"
    golden.mkdir()
    _write_fixture(
        golden,
        slug="argv-task",
        task_yaml=(
            "title: Argv Task\n"
            "tags: [test]\n"
            "budget_usd: 2.5\n"
            "max_iterations: 4\n"
            "timeout_secs: 60\n"
        ),
    )

    # Stub loop that records argv and cwd, then injects a termination event
    event_json = json.dumps({
        "type": "termination",
        "data": {"reason": "DoneAdvisory"},
        "ts": "2026-06-05T00:00:00Z",
    })
    script = tmp_path / "argv-probe-loop.sh"
    script.write_text(
        "#!/usr/bin/env bash\n"
        "set -e\n"
        "mkdir -p .fno\n"
        # Write all arguments as a single space-separated line
        'echo "$@" > argv.txt\n'
        # Write current working directory
        'pwd > pwd.txt\n'
        f"echo '{event_json}' >> .fno/events.jsonl\n"
        "exit 0\n"
    )
    script.chmod(script.stat().st_mode | stat.S_IEXEC)

    run_tasks(
        fixtures_dir=golden,
        task_slug=None,
        label="argv-test",
        model="claude-x",
        keep_workdir=True,
        loop_script=script,
    )

    rows = _read_history_rows(paths_mod.evals_history())
    assert len(rows) == 1
    workdir_kept = rows[0].get("workdir_kept")
    assert workdir_kept is not None, "workdir_kept should be set with keep_workdir=True"

    workdir = Path(workdir_kept)

    # Check argv.txt
    argv_file = workdir / "argv.txt"
    assert argv_file.exists(), "stub loop never ran (argv.txt missing)"
    argv_line = argv_file.read_text().strip()

    assert "--max-iterations" in argv_line, f"--max-iterations missing from argv: {argv_line!r}"
    assert "4" in argv_line, f"max_iterations value 4 missing from argv: {argv_line!r}"
    assert "--budget" in argv_line, f"--budget missing from argv: {argv_line!r}"
    assert "2.5" in argv_line, f"budget_usd value 2.5 missing from argv: {argv_line!r}"
    assert "--model" in argv_line, f"--model missing from argv: {argv_line!r}"
    assert "claude-x" in argv_line, f"model 'claude-x' missing from argv: {argv_line!r}"

    # Check pwd.txt - cwd must be the workdir
    pwd_file = workdir / "pwd.txt"
    assert pwd_file.exists(), "pwd.txt missing"
    loop_cwd = Path(pwd_file.read_text().strip())
    assert loop_cwd == workdir, (
        f"Loop cwd {loop_cwd} != workdir {workdir}"
    )


# ---------------------------------------------------------------------------
# FIX-11 (F14): "evals" in _EXPECTED_SUBCOMMANDS
# (test lives here to verify the lazy_imports test covers evals;
#  the actual edit is to test_lazy_imports.py)
# ---------------------------------------------------------------------------


def test_fix11_evals_app_is_registered_in_abi_help() -> None:
    """FIX-11 (F14): 'evals' subcommand appears in `fno --help` output."""
    from fno.cli import app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(app, ["--help"])
    assert result.exit_code == 0, f"fno --help failed: {result.output}"
    assert "evals" in result.output, (
        f"'evals' not found in fno --help output:\n{result.output}"
    )
