"""Unit smoke tests for the fno wrappers introduced in the consolidation pass:
fno tokens, fno codemap, fno worktree.

These verify only the wiring (subcommand registers, --help renders, the
canonical scripts get located, missing-script paths fail loudly). The heavy
behavior (token-burn analysis, AST/PageRank traversal, lifecycle git ops)
lives in the canonical scripts under scripts/diagnostics/ and scripts/lib/,
and (for codemap) in the engine shipped beside fno.codemap_cli; each is
exercised by its own callers.
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
    """`fno help --all` exposes tokens, codemap, worktree.

    x-71b6 In-N-Out tiering hides these from the curated `fno --help`; the
    full-surface door lists them (they remain invocable either way).
    (`consolidation` was retired in x-71b6 - its audit re-homed to
    `fno lint stale-skill-refs`.)
    """
    result = _run_fno("help", "--all")
    assert result.returncode == 0, result.stdout + result.stderr
    out = result.stdout + result.stderr
    for verb in ("tokens", "codemap", "worktree"):
        assert verb in out, f"fno help --all missing '{verb}': {out[-1000:]}"


def test_fno_tokens_help_renders() -> None:
    result = _run_fno("tokens", "--help")
    assert result.returncode == 0, result.stdout + result.stderr
    assert "session" in result.stdout.lower() or "token" in result.stdout.lower()


def test_fno_codemap_help_renders() -> None:
    result = _run_fno("codemap", "--help")
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
    assert (REPO_ROOT / "scripts" / "diagnostics" / "token-diagnose.py").is_file()
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
    """Regression: `fno codemap` used to work only inside the footnote checkout.

    ``FNO_REPO_ROOT`` points repo resolution at a foreign repo, which is exactly
    what the live failure looked like (running from another project). The engine
    lookup must not follow it. Asserted on the engine-missing error rather than
    on rc=0 because repogram needs heavy system-python deps (see ENGINE_DEPS)
    that CI may not have; a dep failure is a different failure, and this test is
    about resolution.
    """
    foreign = tmp_path / "foreign-repo"
    foreign.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=foreign, check=True)
    (foreign / "mod.py").write_text("def hello():\n    return 1\n")

    result = _run_fno("codemap", extra_env={"FNO_REPO_ROOT": str(foreign)})
    combined = result.stdout + result.stderr
    assert "repogram engine is missing" not in combined, (
        "codemap resolved its engine from the analyzed repo instead of the fno "
        f"installation:\n{combined[-800:]}"
    )
    assert result.returncode != 2, f"engine lookup failed:\n{combined[-800:]}"


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

    found, tried = _engine_python({"PATH": f"{bad}:{good}"})
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
    found, tried = _engine_python({"PATH": f"{broken}:{good}"})
    assert found == str(good / "python3"), f"broken shim was not skipped (tried {tried})"


def test_fno_codemap_rejects_json_plus_db_schema() -> None:
    """Mixed-format combo is refused (Codex review P2): JSON output cannot
    accept the markdown db-schema appendix, so emit a clear error rather
    than silently producing an unparseable file."""
    result = _run_fno("codemap", "--json", "--db-schema")
    assert result.returncode == 2, (
        f"expected rc=2 for --json + --db-schema, got {result.returncode}\n"
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
