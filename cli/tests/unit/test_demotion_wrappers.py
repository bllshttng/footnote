"""Unit smoke tests for the fno wrappers introduced in the consolidation pass:
fno doctor codemap, fno worktree.

These verify only the wiring (subcommand registers, --help renders, the
canonical scripts get located, missing-script paths fail loudly). The heavy
behavior (AST/PageRank traversal, lifecycle git ops) lives in the canonical
scripts under scripts/diagnostics/ and scripts/lib/, and (for codemap) in
the engine shipped beside fno.codemap_cli; each is exercised by its own
callers. The former `fno tokens` wrapper was deleted by the verb audit;
`fno whoami context` / `fno whoami cost` cover the diagnosis.
"""
from __future__ import annotations

import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
CLI_DIR = REPO_ROOT / "cli"


def _run_fno(*args: str, extra_env: dict[str, str] | None = None) -> subprocess.CompletedProcess:
    env = {"PATH": __import__("os").environ.get("PATH", ""), "HOME": str(Path.home())}
    env.update(extra_env or {})
    return subprocess.run(
        ["uv", "run", "fno-py", *args],
        cwd=CLI_DIR,
        capture_output=True,
        text=True,
        env=env,
    )


def test_fno_top_level_lists_demoted_verbs() -> None:
    """codemap and worktree stay reachable as moved spellings.

    They used to render in the full-surface door `fno help --all`, but moved
    spellings render nowhere now (d-26002be8: discovered in their own
    subcommands; the x-6233 fold moved worktree under agents workspace).
    Reachability is proven by invoking each spelling's --help.
    """
    for verb in ("codemap", "worktree"):
        result = _run_fno(verb, "--help")
        assert result.returncode == 0, (
            f"fno {verb} --help exited {result.returncode}: "
            f"{result.stdout[-500:]}{result.stderr[-500:]}"
        )


def test_fno_codemap_help_renders() -> None:
    result = _run_fno("doctor", "codemap", "--help")
    assert result.returncode == 0, result.stdout + result.stderr
    assert "codemap" in result.stdout.lower()
    assert "--tokens" in result.stdout


def test_fno_worktree_help_renders_subcommands() -> None:
    result = _run_fno("worktree", "--help")
    assert result.returncode == 0, result.stdout + result.stderr
    out = result.stdout + result.stderr
    for sub in ("status", "cleanup", "archive"):
        assert sub in out, f"fno worktree --help missing '{sub}': {out[-500:]}"


def test_canonical_scripts_exist_at_expected_paths() -> None:
    """The wrappers shell out to these paths; missing files would 404 at runtime."""
    assert (REPO_ROOT / "scripts" / "lib" / "worktree-lifecycle.sh").is_file()


def test_codemap_engine_lives_inside_the_fno_package() -> None:
    """The codemap engine resolves from the fno INSTALLATION, not the target repo.

    This replaces an assertion on ``<repo>/scripts/codemap/repogram.py``, which
    held both before and after the fix (the suite runs inside footnote, where
    that path existed) and so constrained nothing. Asserting the engine is
    package-relative is what actually rules out the target-repo lookup.
    """
    import fno
    from fno.codemap_cli.cli import ENGINE_DIR

    package_root = Path(fno.__file__).resolve().parent
    assert ENGINE_DIR.is_relative_to(package_root), (
        f"codemap engine dir {ENGINE_DIR} is outside the fno package "
        f"{package_root}; it would not ship with an installed fno"
    )
    assert (ENGINE_DIR / "repogram.py").is_file()
    assert (ENGINE_DIR / "db-schema.py").is_file()
    assert (ENGINE_DIR / "queries").is_dir(), "repogram's bundled tree-sitter queries"


def test_fno_codemap_finds_its_engine_outside_the_footnote_checkout(tmp_path) -> None:
    """Regression: `fno doctor codemap` used to work only inside the footnote checkout.

    ``FNO_REPO_ROOT`` points repo resolution at a foreign repo, which is exactly
    what the live failure looked like (running from another project). The engine
    lookup must not follow it.

    The pass condition is "did NOT exit EXIT_NO_ENGINE", not "exited 0", because
    repogram needs heavy system-python deps a CI box legitimately lacks. Reaching
    EXIT_NO_INTERPRETER is itself PROOF of success: that check runs strictly
    after the engine-exists check, so the command could only get there by having
    already found the engine. An earlier version of this test asserted
    ``returncode != 2`` when both failures shared code 2, which made the two
    indistinguishable and turned a correct fix red on CI.
    """
    from fno.codemap_cli.cli import EXIT_NO_ENGINE, EXIT_NO_INTERPRETER

    foreign = tmp_path / "foreign-repo"
    foreign.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=foreign, check=True)
    (foreign / "mod.py").write_text("def hello():\n    return 1\n")

    result = _run_fno("doctor", "codemap", extra_env={"FNO_REPO_ROOT": str(foreign)})
    combined = result.stdout + result.stderr
    assert result.returncode != EXIT_NO_ENGINE, (
        "codemap resolved its engine from the analyzed repo instead of the fno "
        f"installation:\n{combined[-800:]}"
    )
    # Not a vacuous pass: the run must land on one of the two outcomes that
    # prove resolution happened, so a future refactor that drops the engine
    # lookup entirely (or dies some third way) still fails here.
    assert result.returncode in (0, EXIT_NO_INTERPRETER), (
        f"unexpected outcome rc={result.returncode}; expected success or the "
        f"missing-deps exit:\n{combined[-800:]}"
    )


def test_engine_python_skips_interpreters_missing_the_deps(tmp_path) -> None:
    """The engine interpreter is probed, not assumed to be the first python3.

    Machines commonly carry several pythons (pyenv, homebrew, ~/.local/bin) with
    the heavy engine deps in exactly one. Running the first on PATH produced a
    bare "Missing dependency: pip install networkx" and no codemap.
    """
    import os

    from fno.codemap_cli.cli import _engine_python

    bad, good = tmp_path / "bad", tmp_path / "good"
    for d in (bad, good):
        d.mkdir()
    (bad / "python3").write_text("#!/bin/sh\nexit 1\n")
    (good / "python3").write_text("#!/bin/sh\nexit 0\n")
    for d in (bad, good):
        os.chmod(d / "python3", 0o755)

    found, tried = _engine_python({"PATH": os.pathsep.join([str(bad), str(good)])})
    assert found == str(good / "python3"), f"picked {found}, tried {tried}"
    assert str(bad / "python3") in tried

    found, tried = _engine_python({"PATH": str(bad)})
    assert found is None, "an interpreter without the deps must not be selected"
    assert tried == [str(bad / "python3")], "the error must be able to name what it tried"

    # A candidate that cannot be exec'd at all (broken shim: missing interpreter
    # in the shebang) costs us that candidate, not the whole command.
    broken = tmp_path / "broken"
    broken.mkdir()
    (broken / "python3").write_text("#!/nonexistent/interpreter\n")
    os.chmod(broken / "python3", 0o755)
    found, tried = _engine_python({"PATH": os.pathsep.join([str(broken), str(good)])})
    assert found == str(good / "python3"), f"broken shim was not skipped (tried {tried})"


def test_engine_python_parses_path_the_way_the_platform_writes_it(monkeypatch) -> None:
    """PATH is split on os.pathsep and Windows executables carry .exe.

    Codex P2 on PR 700. Splitting on a hardcoded ":" does not merely miss
    entries on Windows, it tears "C:\\Python\\python.exe" into "C" and
    "\\Python\\python.exe", so every entry is mangled rather than one being
    skipped; and probing extensionless names finds nothing there regardless.
    """
    import os

    from fno.codemap_cli.cli import _interpreter_names, _system_python_env

    assert "python3.exe" in _interpreter_names() or os.name != "nt"

    monkeypatch.setattr(os, "name", "nt")
    names = _interpreter_names()
    assert names[:2] == ("python3.exe", "python.exe"), f"windows names first: {names}"
    assert "python3" in names, "the bare names stay as a fallback"

    # The venv strip must rebuild PATH with the platform separator, not ":".
    monkeypatch.setattr(os, "name", os.name)
    venv = "/tmp/venv-under-test"
    monkeypatch.setenv("VIRTUAL_ENV", venv)
    monkeypatch.setenv("PATH", os.pathsep.join([f"{venv}/bin", "/usr/bin"]))
    env = _system_python_env()
    assert env["PATH"] == "/usr/bin", f"venv entry not stripped cleanly: {env['PATH']!r}"
    assert "VIRTUAL_ENV" not in env


def test_codemap_failure_exit_codes_stay_distinct() -> None:
    """Guards the guard: three failures, three codes.

    The engine-resolution test tells "engine missing" from "deps missing" by
    exit code alone. Collapsing any two back onto one number would not fail that
    test - it would silently strip its ability to discriminate, which is exactly
    how it passed review and then went red on CI. Assert the separation itself.
    """
    from fno.codemap_cli.cli import EXIT_NO_ENGINE, EXIT_NO_INTERPRETER, EXIT_USAGE

    codes = (EXIT_USAGE, EXIT_NO_ENGINE, EXIT_NO_INTERPRETER)
    assert len(set(codes)) == 3, f"codemap failure exit codes must stay distinct: {codes}"
    assert all(c != 0 for c in codes), "a failure must never exit 0"


def test_fno_codemap_rejects_json_plus_db_schema() -> None:
    """Mixed-format combo is refused (Codex review P2): JSON output cannot
    accept the markdown db-schema appendix, so emit a clear error rather
    than silently producing an unparseable file."""
    from fno.codemap_cli.cli import EXIT_USAGE

    result = _run_fno("doctor", "codemap", "--json", "--db-schema")
    assert result.returncode == EXIT_USAGE, (
        f"expected rc={EXIT_USAGE} for --json + --db-schema, got {result.returncode}\n"
        f"stdout: {result.stdout[-400:]}\n"
        f"stderr: {result.stderr[-400:]}"
    )
    assert "incompatible" in (result.stderr + result.stdout).lower()


def test_fno_worktree_status_runs() -> None:
    """worktree status delegates to scripts/lib/worktree-lifecycle.sh status."""
    result = _run_fno("worktree", "status")
    # Exit code may be 0 or non-zero depending on environment but should not crash
    # the wrapper itself. The script prints either a list or an empty header.
    assert result.returncode in (0, 1), (
        f"worktree status crashed: rc={result.returncode}\n"
        f"stdout: {result.stdout[-500:]}\n"
        f"stderr: {result.stderr[-500:]}"
    )
    assert "Worktrees" in result.stdout or result.stdout == "" or result.returncode == 0
