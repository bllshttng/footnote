"""
Unit tests for cli/scripts/check-prompt-drift.sh

Tests invoke the shell script against a temporary directory tree that mirrors
the real layout (cli/src/fno/review/prompts/ + agents/) so the unit
tests remain hermetic and fast.
"""

import os
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
SCRIPT = REPO_ROOT / "cli" / "scripts" / "check-prompt-drift.sh"


def _make_tree(tmp_path: Path, prompts: dict, agents: dict) -> Path:
    """
    Build a minimal temp tree:
      <root>/cli/src/fno/review/prompts/<name>.md
      <root>/agents/<name>.md

    prompts: {filename_stem: body_text}  (underscore-named, no .md)
    agents:  {filename_stem: body_text}  (hyphen-named, no .md)
    """
    root = tmp_path / "repo"
    prompt_dir = root / "cli" / "src" / "fno" / "review" / "prompts"
    agent_dir = root / "agents"
    prompt_dir.mkdir(parents=True)
    agent_dir.mkdir(parents=True)

    for stem, body in prompts.items():
        (prompt_dir / f"{stem}.md").write_text(body)
    for stem, body in agents.items():
        (agent_dir / f"{stem}.md").write_text(body)

    return root


def _run(root: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(SCRIPT), "--root", str(root)],
        capture_output=True,
        text=True,
    )


# Shared minimal prompt bodies used across tests.
_FM = "---\nname: test-agent\nmodel: opus\n---\n"
_BODY = "You are a test agent.\n\nDo the thing.\n"
_FULL_CLI = _FM + _BODY
_FULL_AGENT = _FM + _BODY


class TestClean:
    """AC1-CLEAN: script exits 0 when bundled prompts match plugin agents."""

    def test_single_matching_pair_exits_zero(self, tmp_path):
        root = _make_tree(
            tmp_path,
            prompts={"my_agent": _FULL_CLI},
            agents={"my-agent": _FULL_AGENT},
        )
        result = _run(root)
        assert result.returncode == 0, (
            f"Expected exit 0 but got {result.returncode}.\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        )

    def test_multiple_matching_pairs_exit_zero(self, tmp_path):
        root = _make_tree(
            tmp_path,
            prompts={
                "silent_failure_hunter": _FULL_CLI,
                "code_reviewer": _FULL_CLI,
            },
            agents={
                "silent-failure-hunter": _FULL_AGENT,
                "code-reviewer": _FULL_AGENT,
            },
        )
        result = _run(root)
        assert result.returncode == 0, (
            f"Expected exit 0 but got {result.returncode}.\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        )

    def test_empty_prompt_dir_exits_zero(self, tmp_path):
        """No prompts = nothing to compare = clean."""
        root = _make_tree(tmp_path, prompts={}, agents={})
        result = _run(root)
        assert result.returncode == 0


class TestBodyDrift:
    """AC1-FLAGS-DRIFT: script exits non-zero and prints DRIFT + diff on body mismatch."""

    def test_drift_exits_nonzero(self, tmp_path):
        cli_body = _FM + "You are a test agent.\n\nDo the thing.\n"
        agent_body = _FM + "You are a test agent.\n\nDo something different.\n"
        root = _make_tree(
            tmp_path,
            prompts={"my_agent": cli_body},
            agents={"my-agent": agent_body},
        )
        result = _run(root)
        assert result.returncode != 0, (
            "Expected non-zero exit on drift but got 0.\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        )

    def test_drift_output_contains_drift_header(self, tmp_path):
        cli_body = _FM + "Old content.\n"
        agent_body = _FM + "New content.\n"
        root = _make_tree(
            tmp_path,
            prompts={"my_agent": cli_body},
            agents={"my-agent": agent_body},
        )
        result = _run(root)
        combined = result.stdout + result.stderr
        assert "DRIFT:" in combined, (
            f"Expected 'DRIFT:' in output but got:\n{combined}"
        )

    def test_drift_output_contains_diff_lines(self, tmp_path):
        cli_body = _FM + "Line A only in CLI.\n"
        agent_body = _FM + "Line B only in agent.\n"
        root = _make_tree(
            tmp_path,
            prompts={"my_agent": cli_body},
            agents={"my-agent": agent_body},
        )
        result = _run(root)
        combined = result.stdout + result.stderr
        # Unified diff lines start with + or -
        assert "-Line B only in agent." in combined or "+Line A only in CLI." in combined, (
            f"Expected diff content in output but got:\n{combined}"
        )

    def test_frontmatter_diff_alone_is_not_drift(self, tmp_path):
        """Frontmatter differences should NOT trigger drift - only body matters."""
        cli_body = "---\nname: my-agent\nmodel: opus\ncolor: green\n---\nSame body.\n"
        agent_body = "---\nname: my-agent\nmodel: haiku\n---\nSame body.\n"
        root = _make_tree(
            tmp_path,
            prompts={"my_agent": cli_body},
            agents={"my-agent": agent_body},
        )
        result = _run(root)
        assert result.returncode == 0, (
            "Frontmatter-only diff should not trigger drift but got exit "
            f"{result.returncode}.\nstdout: {result.stdout}\nstderr: {result.stderr}"
        )

    def test_cli_context_prefix_block_is_ignored(self, tmp_path):
        """The <!-- cli-context-prefix --> block on the CLI side is stripped before compare."""
        prefix_block = (
            "<!-- cli-context-prefix -->\n"
            "This is CLI-specific context only.\n"
            "<!-- /cli-context-prefix -->\n"
        )
        cli_body = _FM + prefix_block + "Shared body content.\n"
        agent_body = _FM + "Shared body content.\n"
        root = _make_tree(
            tmp_path,
            prompts={"my_agent": cli_body},
            agents={"my-agent": agent_body},
        )
        result = _run(root)
        assert result.returncode == 0, (
            "cli-context-prefix block should be stripped before compare, "
            f"but got exit {result.returncode}.\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        )


class TestOrphan:
    """AC1-FLAGS-ORPHAN: script exits non-zero and prints ORPHAN when no matching agent exists."""

    def test_orphan_exits_nonzero(self, tmp_path):
        root = _make_tree(
            tmp_path,
            prompts={"bogus_agent": _FULL_CLI},
            agents={},  # no matching agent
        )
        result = _run(root)
        assert result.returncode != 0, (
            "Expected non-zero exit for orphan prompt but got 0.\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        )

    def test_orphan_output_contains_orphan_header(self, tmp_path):
        root = _make_tree(
            tmp_path,
            prompts={"bogus_agent": _FULL_CLI},
            agents={},
        )
        result = _run(root)
        combined = result.stdout + result.stderr
        assert "ORPHAN:" in combined, (
            f"Expected 'ORPHAN:' in output but got:\n{combined}"
        )

    def test_orphan_and_clean_together_exits_nonzero(self, tmp_path):
        """One orphan + one clean pair = still exits non-zero."""
        root = _make_tree(
            tmp_path,
            prompts={
                "good_agent": _FULL_CLI,
                "orphan_agent": _FULL_CLI,
            },
            agents={
                "good-agent": _FULL_AGENT,
                # no orphan-agent
            },
        )
        result = _run(root)
        assert result.returncode != 0
