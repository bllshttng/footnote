"""Unit smoke tests for the fno wrappers introduced in the consolidation pass:
fno tokens, fno codemap, fno worktree.

These verify only the wiring (subcommand registers, --help renders, the
canonical scripts get located, missing-script paths fail loudly). The heavy
behavior (token-burn analysis, AST/PageRank traversal, lifecycle git ops)
lives in the canonical scripts under scripts/diagnostics/, scripts/codemap/,
and scripts/lib/ and is exercised by their own callers.
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
CLI_DIR = REPO_ROOT / "cli"


def _run_abi(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["uv", "run", "fno-py", *args],
        cwd=CLI_DIR,
        capture_output=True,
        text=True,
        env={"PATH": __import__("os").environ.get("PATH", ""), "HOME": str(Path.home())},
    )


def test_abi_top_level_lists_demoted_verbs() -> None:
    """`fno help --all` exposes tokens, codemap, worktree.

    x-71b6 In-N-Out tiering hides these from the curated `fno --help`; the
    full-surface door lists them (they remain invocable either way).
    (`consolidation` was retired in x-71b6 - its audit re-homed to
    `fno lint stale-skill-refs`.)
    """
    result = _run_abi("help", "--all")
    assert result.returncode == 0, result.stdout + result.stderr
    out = result.stdout + result.stderr
    for verb in ("tokens", "codemap", "worktree"):
        assert verb in out, f"fno help --all missing '{verb}': {out[-1000:]}"


def test_abi_tokens_help_renders() -> None:
    result = _run_abi("tokens", "--help")
    assert result.returncode == 0, result.stdout + result.stderr
    assert "session" in result.stdout.lower() or "token" in result.stdout.lower()


def test_abi_codemap_help_renders() -> None:
    result = _run_abi("codemap", "--help")
    assert result.returncode == 0, result.stdout + result.stderr
    assert "codemap" in result.stdout.lower()
    assert "--tokens" in result.stdout


def test_abi_worktree_help_renders_subcommands() -> None:
    result = _run_abi("worktree", "--help")
    assert result.returncode == 0, result.stdout + result.stderr
    out = result.stdout + result.stderr
    for sub in ("status", "cleanup", "archive"):
        assert sub in out, f"fno worktree --help missing '{sub}': {out[-500:]}"


def test_canonical_scripts_exist_at_expected_paths() -> None:
    """The wrappers shell out to these paths; missing files would 404 at runtime."""
    assert (REPO_ROOT / "scripts" / "diagnostics" / "token-diagnose.py").is_file()
    assert (REPO_ROOT / "scripts" / "codemap" / "repogram.py").is_file()
    assert (REPO_ROOT / "scripts" / "codemap" / "db-schema.py").is_file()
    assert (REPO_ROOT / "scripts" / "lib" / "worktree-lifecycle.sh").is_file()


def test_abi_codemap_rejects_json_plus_db_schema() -> None:
    """Mixed-format combo is refused (Codex review P2): JSON output cannot
    accept the markdown db-schema appendix, so emit a clear error rather
    than silently producing an unparseable file."""
    result = _run_abi("codemap", "--json", "--db-schema")
    assert result.returncode == 2, (
        f"expected rc=2 for --json + --db-schema, got {result.returncode}\n"
        f"stdout: {result.stdout[-400:]}\n"
        f"stderr: {result.stderr[-400:]}"
    )
    assert "incompatible" in (result.stderr + result.stdout).lower()


def test_abi_worktree_status_runs() -> None:
    """worktree status delegates to scripts/lib/worktree-lifecycle.sh status."""
    result = _run_abi("worktree", "status")
    # Exit code may be 0 or non-zero depending on environment but should not crash
    # the wrapper itself. The script prints either a list or an empty header.
    assert result.returncode in (0, 1), (
        f"worktree status crashed: rc={result.returncode}\n"
        f"stdout: {result.stdout[-500:]}\n"
        f"stderr: {result.stderr[-500:]}"
    )
    assert "Worktrees" in result.stdout or result.stdout == "" or result.returncode == 0
