from __future__ import annotations

import os
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


def test_spawn_paths_lint_allows_named_harness_file(tmp_path: Path) -> None:
    source = tmp_path / "cli" / "src" / "fno" / "agents" / "harnesses" / "claude.py"
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


# --------------------------------------------------------------------------- #
# markdown style gate: a renamed doc is not authored prose
# --------------------------------------------------------------------------- #


def _git(repo: Path, *args: str) -> None:
    import subprocess

    subprocess.run(
        ["git", *args],
        cwd=str(repo),
        check=True,
        capture_output=True,
        text=True,
    )


def _repo_with_renamed_doc(tmp_path: Path) -> tuple[Path, str, str]:
    """A repo whose only change since 'base' is one pure `git mv` of a doc."""
    repo = tmp_path / "repo"
    (repo / "docs").mkdir(parents=True)
    _git(repo.parent, "init", "-q", "repo")
    _git(repo, "config", "user.email", "t@example.com")
    _git(repo, "config", "user.name", "t")
    old = "docs/old-name.md"
    new = "docs/new-name.md"
    # Prose that breaks rule 1 and rule 2, so a gate that reads it as added
    # cannot come back clean by accident.
    (repo / old).write_text(
        "This existing sentence is deliberately far longer than the twenty five "
        "word cap that the style checker enforces on ordinary prose lines; it "
        "also carries a semicolon.\n",
        encoding="utf-8",
    )
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "base")
    _git(repo, "branch", "base")
    _git(repo, "mv", old, new)
    _git(repo, "commit", "-qm", "rename")
    return repo, new, old


def test_added_line_nums_ignores_a_pure_rename(tmp_path: Path) -> None:
    from fno.lint_cli import _git_added_line_nums

    repo, new, old = _repo_with_renamed_doc(tmp_path)

    # Positive control: without the pre-rename path, the pathspec hides the
    # rename source and git reports the whole body as added. This asserts the
    # bug is reachable, so the fix below is not passing on an empty diff.
    assert _git_added_line_nums(new, "base", repo) != set()

    assert _git_added_line_nums(new, "base", repo, old) == set()


def test_style_gate_reports_no_violations_for_a_pure_rename(tmp_path: Path) -> None:
    import os

    from fno.lint_cli import _style_added_lines

    repo, _new, _old = _repo_with_renamed_doc(tmp_path)
    cwd = os.getcwd()
    os.chdir(repo)
    _clear_repo_root_cache()
    os.environ["FNO_REPO_ROOT"] = str(repo)
    try:
        violations, inspected, changed = _style_added_lines("base", None)
    finally:
        os.environ.pop("FNO_REPO_ROOT", None)
        _clear_repo_root_cache()
        os.chdir(cwd)

    assert changed == 1, "the renamed doc must still be reported as a changed file"
    assert inspected == 0, "a pure rename authors no lines"
    assert violations == []


def test_every_git_call_pins_the_rename_limit(tmp_path: Path, monkeypatch) -> None:
    """Deleting either pin must fail this test.

    Past ``diff.renameLimit`` git skips the exhaustive pass, warns on stderr,
    and exits 0. Measured on a real branch at ``renameLimit=1``: exit 0 and 9 of
    21 renames found, so a return-code guard sees nothing wrong. The earlier
    version of this test called its helper once with the UNLIMITED value and
    asserted unlimited detection works, which was never in doubt and stayed
    green with both pins deleted. This asserts the pin itself, on every call
    that resolves or reads a rename.
    """
    import subprocess as sp

    from fno.lint_cli import _style_added_lines

    repo, _new, _old = _repo_with_renamed_doc(tmp_path)
    seen: list[list[str]] = []
    real_run = sp.run

    def _spy(argv, *a, **k):
        if isinstance(argv, list) and argv and argv[0] == "git":
            seen.append(list(argv))
        return real_run(argv, *a, **k)

    monkeypatch.setattr("fno.lint_cli.subprocess.run", _spy)
    cwd = os.getcwd()
    os.chdir(repo)
    _clear_repo_root_cache()
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo))
    try:
        _style_added_lines("base", None)
    finally:
        _clear_repo_root_cache()
        os.chdir(cwd)

    diffs = [c for c in seen if "diff" in c]
    assert diffs, "positive control: no git diff call was observed at all"
    unpinned = [c for c in diffs if "diff.renameLimit=0" not in c]
    assert not unpinned, f"git diff calls missing the rename-limit pin: {unpinned}"
