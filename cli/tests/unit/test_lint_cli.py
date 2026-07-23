from __future__ import annotations

from pathlib import Path

from typer.testing import CliRunner

from fno import paths
from fno.lint_cli import app


runner = CliRunner()


def _clear_repo_root_cache() -> None:
    # resolve_repo_root() is @cache'd per process; clear it around tests that
    # pin FNO_REPO_ROOT so the env override is re-read.
    paths.resolve_repo_root.cache_clear()


def _write_provider(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")


def test_provider_stderr_merge_lint_flags_unjustified_merge(tmp_path: Path) -> None:
    providers = tmp_path / "providers"
    _write_provider(
        providers / "bad.py",
        """
import subprocess


def _run_bad():
    return subprocess.Popen(["bad"], stderr=subprocess.STDOUT)
""",
    )

    result = runner.invoke(
        app,
        ["provider-stderr-merge", "--providers-dir", str(providers)],
    )

    assert result.exit_code == 1
    assert "bad.py" in result.stderr
    assert "requires nearby" in result.stderr


def test_provider_stderr_merge_lint_accepts_locked_decision(tmp_path: Path) -> None:
    providers = tmp_path / "providers"
    _write_provider(
        providers / "codex_like.py",
        """
import subprocess


def _run_codex_like():
    # Locked Decision 12: this provider emits low-volume stderr and the
    # merged stream is parsed line-by-line by the same drainer.
    return subprocess.Popen(["codex"], stderr=subprocess.STDOUT)
""",
    )

    result = runner.invoke(
        app,
        ["provider-stderr-merge", "--providers-dir", str(providers)],
    )

    assert result.exit_code == 0
    assert "provider-stderr-merge: ok" in result.stdout


def test_provider_stderr_merge_lint_uses_explicit_dir_outside_repo(tmp_path: Path) -> None:
    providers = tmp_path / "providers"
    _write_provider(
        providers / "codex_like.py",
        """
import subprocess


def _run_codex_like():
    return subprocess.Popen(["codex"], stderr=subprocess.STDOUT)  # stderr=stdout: parsed by one drainer
""",
    )

    with runner.isolated_filesystem():
        result = runner.invoke(
            app,
            ["provider-stderr-merge", "--providers-dir", str(providers)],
        )

    assert result.exit_code == 0
    assert "provider-stderr-merge: ok" in result.stdout


def test_lint_cli_help_lists_promoted_flock_pattern() -> None:
    result = runner.invoke(app, ["--help"])

    assert result.exit_code == 0
    assert "flock-pattern" in result.stdout
    assert "provider-stderr-merge" in result.stdout


def test_spawn_paths_lint_rejects_non_allowlisted_session_shape(tmp_path: Path) -> None:
    source = tmp_path / "cli" / "src" / "fno" / "new_spawn.py"
    source.parent.mkdir(parents=True)
    source.write_text('cmd = ["claude", "--print", "prompt"]\n', encoding="utf-8")

    from fno.lint_cli import _spawn_shape_violations

    violations = _spawn_shape_violations(tmp_path)
    assert len(violations) == 1
    assert "new_spawn.py:1" in violations[0]
    assert "--print" in violations[0]


def test_spawn_paths_lint_rejects_spawn_flag_after_other_options(tmp_path: Path) -> None:
    source = tmp_path / "cli" / "src" / "fno" / "new_spawn.py"
    source.parent.mkdir(parents=True)
    source.write_text(
        'cmd = ["claude", "--model", "sonnet", "--print", "prompt"]\n',
        encoding="utf-8",
    )

    from fno.lint_cli import _spawn_shape_violations

    violations = _spawn_shape_violations(tmp_path)
    assert len(violations) == 1
    assert "new_spawn.py:1" in violations[0]
    assert "--print" in violations[0]


def test_spawn_paths_lint_allows_named_provider_file(tmp_path: Path) -> None:
    source = tmp_path / "cli" / "src" / "fno" / "agents" / "providers" / "claude.py"
    source.parent.mkdir(parents=True)
    source.write_text('cmd = ["claude", "--bg", "prompt"]\n', encoding="utf-8")

    from fno.lint_cli import _spawn_shape_violations

    assert _spawn_shape_violations(tmp_path) == []


# --------------------------------------------------------------------------- #
# flock-pattern: conform + degrade (ab-fd017698)
# --------------------------------------------------------------------------- #
def test_flock_pattern_degrades_when_script_absent(tmp_path: Path, monkeypatch) -> None:
    """US1 (AC1-HP/ERR/UI/EDGE/FR): with the lint script absent (a no-script env),
    the verb exits 2 with an actionable stderr message - never bash's 127 and
    never a Python traceback."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))  # empty dir -> no script
    _clear_repo_root_cache()
    try:
        result = runner.invoke(app, ["flock-pattern"])
    finally:
        _clear_repo_root_cache()

    assert result.exit_code == 2  # exit 2, not 127, not 0
    assert "flock-pattern" in result.stderr
    assert "lint scripts" in result.stderr  # names what is missing
    assert "Traceback" not in (result.stderr + result.stdout)


def test_flock_pattern_runs_script_when_present(tmp_path: Path, monkeypatch) -> None:
    """US3 (AC3-HP/ERR): when the script IS present the verb bash-execs it and
    preserves the script's own exit code unchanged."""
    (tmp_path / "scripts").mkdir()
    (tmp_path / "scripts" / "lint-flock-pattern.sh").write_text("#!/bin/bash\nexit 0\n")
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    _clear_repo_root_cache()

    calls: dict[str, list[str]] = {}

    class _Result:
        returncode = 7

    def _fake_run(argv, *a, **k):
        calls["argv"] = list(argv)
        return _Result()

    monkeypatch.setattr("fno.lint_cli.subprocess.run", _fake_run)
    try:
        result = runner.invoke(app, ["flock-pattern"])
    finally:
        _clear_repo_root_cache()

    assert result.exit_code == 7  # script's exit code preserved, not remapped
    assert calls["argv"][0] == "bash"
    assert calls["argv"][1].endswith("scripts/lint-flock-pattern.sh")


def test_flock_pattern_forwards_dispatch_path(tmp_path: Path, monkeypatch) -> None:
    """US3 (AC3-EDGE): the --dispatch-path override is forwarded to the script
    exactly as before the rooting/degrade change."""
    (tmp_path / "scripts").mkdir()
    (tmp_path / "scripts" / "lint-flock-pattern.sh").write_text("#!/bin/bash\nexit 0\n")
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    _clear_repo_root_cache()

    calls: dict[str, list[str]] = {}

    class _Result:
        returncode = 0

    def _fake_run(argv, *a, **k):
        calls["argv"] = list(argv)
        return _Result()

    monkeypatch.setattr("fno.lint_cli.subprocess.run", _fake_run)
    try:
        result = runner.invoke(
            app, ["flock-pattern", "--dispatch-path", "/tmp/dispatch.py"]
        )
    finally:
        _clear_repo_root_cache()

    assert result.exit_code == 0
    assert "/tmp/dispatch.py" in calls["argv"]
