from __future__ import annotations

import os
from pathlib import Path

import pytest
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
        violations, inspected, changed, unexplained = _style_added_lines("base", None)
    finally:
        os.environ.pop("FNO_REPO_ROOT", None)
        _clear_repo_root_cache()
        os.chdir(cwd)

    assert changed == 1, "the renamed doc must still be reported as a changed file"
    assert inspected == 0, "a pure rename authors no lines"
    assert violations == []
    # The absence guard fires on this number, not on `inspected`. A pure rename
    # is a legitimate zero, and the rename resolution above is what produces it,
    # so counting bare zeros made the fix trip the guard it shares a function
    # with: a rename-only PR inspected nothing and exited 1.
    assert unexplained == [], "a pure rename is an explained zero, not a broken scan"


def test_no_newline_marker_does_not_shift_added_line_numbers(tmp_path: Path) -> None:
    """`\\ No newline at end of file` is a note, never a line of the file.

    Counted as context it advances the position, so every added line after it
    is numbered one too high. The marker lands MID-hunk when the old file had
    no trailing newline and its last line is replaced, which is the shape that
    makes a real line escape rather than merely inventing a phantom one.
    """
    from fno.lint_cli import _git_added_line_nums

    repo = tmp_path / "repo"
    (repo / "docs").mkdir(parents=True)
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "t@t")
    _git(repo, "config", "user.name", "t")
    doc = repo / "docs" / "m.md"
    doc.write_text("Old last line.", encoding="utf-8")  # no trailing newline
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "base")
    _git(repo, "branch", "-f", "base")
    doc.write_text("New first line.\nSecond added.\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "replace the last line")

    nums = _git_added_line_nums("docs/m.md", "base", repo)
    assert nums == {1, 2}, (
        "added lines must be numbered against the NEW file; counting the "
        f"no-newline marker as context shifts them (got {sorted(nums)})"
    )


def test_an_added_line_beginning_with_plus_plus_is_not_read_as_a_header(
    tmp_path: Path,
) -> None:
    """Content that looks like a diff header must not shift line numbers.

    An added markdown line whose own text starts with `++` renders as `+++` in
    the diff. Matched by prefix it was swallowed as a file header AND did not
    advance the position, so every later added line in that file was numbered
    one too low: a real violation ships while a different line is checked.
    Repo docs carry fenced diff snippets, so this is reachable content.
    """
    from fno.lint_cli import _git_added_line_nums

    repo = tmp_path / "repo"
    (repo / "docs").mkdir(parents=True)
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "t@t")
    _git(repo, "config", "user.name", "t")
    doc = repo / "docs" / "d.md"
    doc.write_text("Intro line.\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "base")
    _git(repo, "branch", "-f", "base")
    doc.write_text(
        "Intro line.\n++ plus marker\nA line; with a semicolon.\n", encoding="utf-8"
    )
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "add a plus-prefixed line")

    nums = _git_added_line_nums("docs/d.md", "base", repo)
    assert nums == {2, 3}, (
        "both added lines must be numbered; swallowing the `++` line as a "
        f"header hides line 3, which carries a real violation (got {sorted(nums)})"
    )


def test_a_files_scope_matching_nothing_refuses(tmp_path: Path) -> None:
    """A caller scope that matches no changed file must not read as clean.

    `--files` resolves against the caller's cwd, so a caller standing anywhere
    but the repo root and passing a repo-relative path lands somewhere real,
    keys to a plausible prefix, and matches nothing. Exiting 0 there is the
    absence-read-as-success shape this whole change set exists to remove.
    """
    import os

    import typer

    from fno.lint_cli import _style_added_lines

    repo, _new, _old = _repo_with_renamed_doc(tmp_path)
    cwd = os.getcwd()
    os.chdir(repo)
    _clear_repo_root_cache()
    os.environ["FNO_REPO_ROOT"] = str(repo)
    try:
        with pytest.raises(typer.Exit) as exc:
            _style_added_lines("base", [Path("docs/nothing-here")])
    finally:
        os.environ.pop("FNO_REPO_ROOT", None)
        _clear_repo_root_cache()
        os.chdir(cwd)
    assert exc.value.exit_code == 2


def test_a_scope_naming_a_path_the_branch_renamed_away_is_allowed(
    tmp_path: Path,
) -> None:
    """The PRE-rename path is a legitimate scope for a rename diff.

    The existence check that catches a cwd mis-resolution also refused a path
    the branch itself moved, which is exactly the diff a rename PR asks about.
    Measured on this branch: `--files docs/providers/codex.md` exited 2 over a
    file this branch renamed. Absence in the working tree cannot decide it, so
    the base decides: present there means deleted or renamed since.
    """
    import os

    from fno.lint_cli import _style_added_lines

    repo, _new, old = _repo_with_renamed_doc(tmp_path)
    cwd = os.getcwd()
    os.chdir(repo)
    _clear_repo_root_cache()
    os.environ["FNO_REPO_ROOT"] = str(repo)
    try:
        violations, inspected, changed, unexplained = _style_added_lines(
            "base", [Path(old)]
        )
    finally:
        os.environ.pop("FNO_REPO_ROOT", None)
        _clear_repo_root_cache()
        os.chdir(cwd)

    # A pure rename authors nothing, so the honest answer is a clean zero
    # rather than a refusal. The sibling test above still proves a path absent
    # from BOTH sides refuses.
    assert (violations, inspected, unexplained) == ([], 0, [])
    assert changed >= 0


def test_the_guard_reaches_renamed_files(tmp_path: Path, monkeypatch) -> None:
    """A renamed-and-edited doc must still be covered by the instrument guard.

    `git --numstat` compresses a rename to `docs/{a.md => b.md}`, so keyed off
    that output the NEW path is never present and the guard silently cannot
    fire for any renamed file. That is precisely the class the rename
    resolution makes eligible for a zero-inspection result, so the one file
    type most likely to break the parser was the one the guard could not see.
    """
    import os

    from fno.lint_cli import _style_added_lines

    repo = tmp_path / "repo"
    (repo / "docs").mkdir(parents=True)
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "t@t")
    _git(repo, "config", "user.name", "t")
    (repo / "docs" / "a.md").write_text("One.\nTwo.\nThree.\nFour.\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "base")
    _git(repo, "branch", "-f", "base")
    _git(repo, "mv", "docs/a.md", "docs/b.md")
    (repo / "docs" / "b.md").write_text(
        "One.\nTwo.\nThree.\nFour.\nFive added.\n", encoding="utf-8"
    )
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "rename and edit")

    # The parser loses the authored line on the renamed path.
    monkeypatch.setattr("fno.lint_cli._git_added_line_nums", lambda *a, **k: set())

    cwd = os.getcwd()
    os.chdir(repo)
    _clear_repo_root_cache()
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo))
    try:
        _v, _inspected, _changed, unexplained = _style_added_lines("base", None)
    finally:
        _clear_repo_root_cache()
        os.chdir(cwd)

    assert unexplained == ["docs/b.md"], (
        "git counted an added line on the renamed path and the parser found "
        "none, so the guard must fire for renamed files too"
    )


def test_a_deletion_only_edit_is_an_explained_zero(tmp_path: Path) -> None:
    """A trimmed doc must not fail the gate.

    This test previously asserted the OPPOSITE, and it was wrong on purpose
    rather than by accident: it encoded the author's misreading that any
    zero-inspected file is suspicious. Making a test fail proves it is
    connected to the code. It never proves it asserts the right thing.
    """
    import os

    from fno.lint_cli import _style_added_lines

    repo = tmp_path / "repo"
    (repo / "docs").mkdir(parents=True)
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "t@t")
    _git(repo, "config", "user.name", "t")
    trim = repo / "docs" / "trim.md"
    trim.write_text("One line.\nTwo line.\n", encoding="utf-8")
    (repo / "docs" / "normal.md").write_text("Alpha.\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "base")
    _git(repo, "branch", "-f", "base")
    trim.write_text("One line.\n", encoding="utf-8")
    (repo / "docs" / "normal.md").write_text("Alpha.\nBeta.\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "trim one, extend the other")

    cwd = os.getcwd()
    os.chdir(repo)
    _clear_repo_root_cache()
    os.environ["FNO_REPO_ROOT"] = str(repo)
    try:
        _v, inspected, changed, unexplained = _style_added_lines("base", None)
    finally:
        os.environ.pop("FNO_REPO_ROOT", None)
        _clear_repo_root_cache()
        os.chdir(cwd)

    assert changed == 2 and inspected == 1, "positive control: one line was added"
    assert unexplained == [], "a deletion-only edit authors nothing and is explained"


def test_style_gate_still_fails_when_the_parser_loses_added_lines(
    tmp_path: Path, monkeypatch
) -> None:
    """The absence guard must survive the fix that narrowed it.

    Narrowing a guard is where guards die quietly. The case it still has to
    catch is the only one that ever meant trouble: git counted added lines for
    a path and this parser returned none, so the instrument failed.
    """
    import os

    from fno.lint_cli import _style_added_lines

    repo = tmp_path / "repo"
    (repo / "docs").mkdir(parents=True)
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "t@t")
    _git(repo, "config", "user.name", "t")
    doc = repo / "docs" / "d.md"
    doc.write_text("One line.\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "base")
    _git(repo, "branch", "-f", "base")
    # A genuine authored line, so git's own count for this path is non-zero.
    doc.write_text("One line.\nTwo line.\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "author a line")

    # The parser loses the line git counted. That disagreement, and only that,
    # is what the guard exists to catch.
    monkeypatch.setattr("fno.lint_cli._git_added_line_nums", lambda *a, **k: set())

    cwd = os.getcwd()
    os.chdir(repo)
    _clear_repo_root_cache()
    os.environ["FNO_REPO_ROOT"] = str(repo)
    try:
        _v, inspected, changed, unexplained = _style_added_lines("base", None)
    finally:
        os.environ.pop("FNO_REPO_ROOT", None)
        _clear_repo_root_cache()
        os.chdir(cwd)

    assert changed == 1 and inspected == 0
    assert unexplained == ["docs/d.md"], (
        "the guard must name the file, since a bare count is not investigable"
    )


def test_the_guard_catches_a_partial_parser_loss_not_only_a_total_one(
    tmp_path: Path, monkeypatch
) -> None:
    """Losing SOME added lines is the same instrument failure as losing all.

    The emptiness test this replaces caught only a total loss. One hunk header
    failing the `\\+(\\d+)` search leaves the returned set non-empty, so the
    file passed the guard, the unread lines shipped unstyled, and the receipt
    reported the smaller number as though it were the whole job. Git's exact
    count was already in hand.
    """
    import os

    from fno.lint_cli import _style_added_lines

    repo = tmp_path / "repo"
    (repo / "docs").mkdir(parents=True)
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "t@t")
    _git(repo, "config", "user.name", "t")
    doc = repo / "docs" / "d.md"
    doc.write_text("One line.\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "base")
    _git(repo, "branch", "-f", "base")
    doc.write_text("One line.\nTwo line.\nThree line.\n", encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "author two lines")

    # Git counts two added lines; the parser returns one. Non-empty, and wrong.
    monkeypatch.setattr("fno.lint_cli._git_added_line_nums", lambda *a, **k: {2})

    cwd = os.getcwd()
    os.chdir(repo)
    _clear_repo_root_cache()
    os.environ["FNO_REPO_ROOT"] = str(repo)
    try:
        _v, inspected, _changed, unexplained = _style_added_lines("base", None)
    finally:
        os.environ.pop("FNO_REPO_ROOT", None)
        _clear_repo_root_cache()
        os.chdir(cwd)

    assert inspected == 1, "the receipt reports what was actually read"
    assert unexplained == ["docs/d.md"], (
        "one line read out of two counted is an instrument failure, "
        "and the old emptiness test called it clean"
    )


@pytest.mark.parametrize(
    "marker", ["--name-only", "-U0", "--name-status", "--numstat", "rev-parse"]
)
def test_a_failing_git_diff_is_not_reported_as_a_clean_tree(
    tmp_path: Path, monkeypatch, marker: str
) -> None:
    """A git failure must exit 2, never look like "nothing changed".

    Every diff call answers with stdout alone, and empty stdout is also the
    clean result, so an unchecked return code turns any git error into a green
    gate. Measured before the fix: a malformed pathspec made git exit 128 while
    the gate exited 0 reporting zero changed files.

    Parameterized per CALL, not per outcome. Failing all three at once passed
    with two of the three guards deleted, because the surviving guard raised
    first and the assertion could not tell which one did. One failure injected
    at a time is what pins each site.
    """
    import os
    import subprocess as sp

    import typer

    from fno.lint_cli import _style_added_lines

    repo, _new, _old = _repo_with_renamed_doc(tmp_path)
    real_run = sp.run

    def _fail_diffs(argv, *a, **k):
        if isinstance(argv, list) and marker in argv:
            return sp.CompletedProcess(argv, 128, "", "fatal: bad revision")
        return real_run(argv, *a, **k)

    monkeypatch.setattr("fno.lint_cli.subprocess.run", _fail_diffs)
    cwd = os.getcwd()
    os.chdir(repo)
    _clear_repo_root_cache()
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo))
    try:
        with pytest.raises(typer.Exit) as exc:
            _style_added_lines("base", None)
    finally:
        _clear_repo_root_cache()
        os.chdir(cwd)
    assert exc.value.exit_code == 2


def test_relative_files_paths_resolve_against_the_repo_root(tmp_path: Path) -> None:
    """A caller-relative --files path must reach the same files from anywhere.

    Every git call here runs from the repo root while the caller's paths are
    relative to the caller's cwd. Measured before the fix: `--files ../docs`
    from `cli/` matched nothing and the gate exited 0 over 27 changed files.
    """
    import os

    from fno.lint_cli import _style_added_lines

    repo, _new, _old = _repo_with_renamed_doc(tmp_path)
    (repo / "sub").mkdir()

    def _run(cwd_dir: Path, scope: Path):
        cwd = os.getcwd()
        os.chdir(cwd_dir)
        _clear_repo_root_cache()
        os.environ["FNO_REPO_ROOT"] = str(repo)
        try:
            return _style_added_lines("base", [scope])
        finally:
            os.environ.pop("FNO_REPO_ROOT", None)
            _clear_repo_root_cache()
            os.chdir(cwd)

    from_root = _run(repo, Path("docs"))
    from_sub = _run(repo / "sub", Path("../docs"))
    assert from_root[2] > 0, "positive control: the scope must match files at all"
    assert from_sub[2] == from_root[2], "the same scope must resolve from any cwd"


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

    from fno.lint_cli import _pinned_diff_argv

    diffs = [c for c in seen if "diff" in c]
    assert diffs, "positive control: no git diff call was observed at all"
    # Pin the MECHANISM, not the outcome. The previous version accepted either
    # `diff.renames=true` or `--find-renames` at each site, which asserts that
    # detection is on somehow and permits three sites to spell it three ways.
    # It also false-flags a correct call using `-M`, git's own alias for
    # `--find-renames`. Requiring the shared prefix means one thing decides how,
    # and a fourth site cannot quietly invent a fourth spelling.
    prefix = _pinned_diff_argv()
    unowned = [c for c in diffs if c[: len(prefix)] != prefix]
    assert not unowned, (
        f"git diff calls not built by _pinned_diff_argv: {unowned}\n"
        f"expected every call to start with {prefix}"
    )
    # And no site re-specifies detection by flag, which is how a second spelling
    # gets back in beside the config pin.
    by_flag = [c for c in diffs if "--find-renames" in c or "-M" in c]
    assert not by_flag, f"rename detection respelled as a flag: {by_flag}"
