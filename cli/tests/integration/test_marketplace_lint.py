"""Integration tests for scripts/lint/no-cross-skill-runtime-calls.sh.

Covers all four ACs from Phase 3 of the skill-encapsulation refactor:
- AC1-HP: clean driver skill passes the lint
- AC2-ERR: Skill() call fails the lint
- AC3-UI: error message names file:line and recommends Read or Task/Agent
- AC4-EDGE: violations in references/*.md and agents/*.md are caught,
            not just SKILL.md
"""
from __future__ import annotations

import os
import subprocess
import textwrap
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[3]
LINT = REPO_ROOT / "scripts" / "lint" / "no-cross-skill-runtime-calls.sh"


_CLEAN_SKILL_FRONTMATTER = textwrap.dedent(
    """\
    ---
    name: target
    description: "clean fixture"
    requires:
      binaries:
        - "fno >= 0.1"
    ---
    Body content. No forbidden patterns.
    """
)


def _run(cmd: list[str], cwd: Path | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, cwd=cwd, check=False)


def _shelled_python_has_yaml() -> bool:
    """Does the bare ``python3`` this lint shells out to import yaml?

    The lint runs under the system interpreter, not the project venv, and its
    frontmatter parse requires PyYAML. CI's python3 carries it, so guarding
    here keeps a dev machine whose system interpreter lacks it from reading as
    a lint failure. Mirrors the same guard in test_skill_bundles.py.
    """
    return _run(["python3", "-c", "import yaml"]).returncode == 0


pytestmark = pytest.mark.skipif(
    not _shelled_python_has_yaml(),
    reason="system python3 lacks PyYAML, required by the marketplace lint (pip install pyyaml)",
)


def _make_skill(tmp_root: Path, name: str, skill_md: str) -> Path:
    skill_dir = tmp_root / "skills" / name
    skill_dir.mkdir(parents=True, exist_ok=True)
    (skill_dir / "SKILL.md").write_text(skill_md)
    return skill_dir


# ---------------------------------------------------------------------------
# AC1-HP: clean driver skill passes
# ---------------------------------------------------------------------------


def test_clean_driver_skill_passes(tmp_path):
    """A skill with no Skill() calls, no path escapes, fno declared in
    requires.binaries: lint exits 0."""
    _make_skill(tmp_path, "target", _CLEAN_SKILL_FRONTMATTER)
    result = _run(["bash", str(LINT), "--root", str(tmp_path)])
    assert result.returncode == 0, result.stdout + result.stderr
    assert "self-contained" in result.stdout


# ---------------------------------------------------------------------------
# AC2-ERR + AC3-UI: Skill() runtime call fails
# ---------------------------------------------------------------------------


def test_skill_runtime_call_fails(tmp_path):
    """A Skill() call in any *.md inside a driver skill fails the lint with
    a line-pinpoint error and a fix recommendation."""
    skill_md = _CLEAN_SKILL_FRONTMATTER.replace(
        "Body content. No forbidden patterns.",
        'Body content. Then we Skill("blueprint", "...") at runtime.',
    )
    _make_skill(tmp_path, "target", skill_md)
    result = _run(["bash", str(LINT), "--root", str(tmp_path)])
    assert result.returncode != 0
    # AC3-UI: file:line:match output for navigation
    assert "skills/target/SKILL.md:" in result.stderr
    # AC3-UI: fix recommendation
    assert "Read" in result.stderr
    assert "Task/Agent" in result.stderr


def test_skill_runtime_call_in_reference_md_fails(tmp_path):
    """AC4-EDGE: a Skill() call in references/*.md (not just SKILL.md) fails."""
    _make_skill(tmp_path, "target", _CLEAN_SKILL_FRONTMATTER)
    refs = tmp_path / "skills" / "target" / "references"
    refs.mkdir()
    (refs / "loop.md").write_text('Some text. Skill("target", "...") inline.\n')
    result = _run(["bash", str(LINT), "--root", str(tmp_path)])
    assert result.returncode != 0
    assert "skills/target/references/loop.md:" in result.stderr


def test_skill_runtime_call_in_agent_md_fails(tmp_path):
    """AC4-EDGE: a Skill() call in agents/*.md (subagent prompt) fails."""
    _make_skill(tmp_path, "target", _CLEAN_SKILL_FRONTMATTER)
    agents = tmp_path / "skills" / "target" / "agents"
    agents.mkdir()
    (agents / "worker.md").write_text('Body. Skill("target", "...") here.\n')
    result = _run(["bash", str(LINT), "--root", str(tmp_path)])
    assert result.returncode != 0
    assert "skills/target/agents/worker.md:" in result.stderr


# ---------------------------------------------------------------------------
# AC7-ERR (epic ab-0d05a9b7): cluster routers obey the same guard as drivers
# ---------------------------------------------------------------------------


def test_cluster_router_skill_call_fails(tmp_path):
    """A Skill() call in a cluster router (e.g. /review) fails the lint -
    proves the guard covers the router+mode skills, not just the drivers."""
    skill_md = _CLEAN_SKILL_FRONTMATTER.replace(
        "Body content. No forbidden patterns.",
        'Body. Then Skill("sigma-review", "...") at runtime.',
    )
    _make_skill(tmp_path, "review", skill_md)
    result = _run(["bash", str(LINT), "--root", str(tmp_path)])
    assert result.returncode != 0
    assert "skills/review/SKILL.md:" in result.stderr


# ---------------------------------------------------------------------------
# AC2-ERR: path-escape failure
# ---------------------------------------------------------------------------


def test_shared_path_escape_fails(tmp_path):
    """A `../../_shared/X.md` link in a driver skill fails the lint."""
    skill_md = _CLEAN_SKILL_FRONTMATTER.replace(
        "Body content. No forbidden patterns.",
        "See [auto-merge](../../_shared/auto-merge.md).",
    )
    _make_skill(tmp_path, "target", skill_md)
    result = _run(["bash", str(LINT), "--root", str(tmp_path)])
    assert result.returncode != 0
    assert "cross-skill path escape" in result.stderr
    assert "bundled references/ or agents/" in result.stderr


def test_sibling_skill_path_escape_fails(tmp_path):
    """A `../../<sibling-skill>/X.md` link fails the lint (catches future
    Skill folders that try to reach into each other directly)."""
    skill_md = _CLEAN_SKILL_FRONTMATTER.replace(
        "Body content. No forbidden patterns.",
        "See [worker](../../sibling/worker.md).",
    )
    _make_skill(tmp_path, "target", skill_md)
    result = _run(["bash", str(LINT), "--root", str(tmp_path)])
    assert result.returncode != 0
    assert "cross-skill path escape" in result.stderr


# ---------------------------------------------------------------------------
# Missing requires.binaries.fno fails
# ---------------------------------------------------------------------------


def test_missing_requires_block_fails(tmp_path):
    """A driver skill without a requires.binaries block fails the lint."""
    skill_md = textwrap.dedent(
        """\
        ---
        name: target
        description: "no requires block"
        ---
        body
        """
    )
    _make_skill(tmp_path, "target", skill_md)
    result = _run(["bash", str(LINT), "--root", str(tmp_path)])
    assert result.returncode != 0
    assert "does not declare 'fno'" in result.stderr


def test_missing_abi_in_requires_fails(tmp_path):
    """A requires.binaries: block that lists other binaries but not fno
    fails the lint."""
    skill_md = textwrap.dedent(
        """\
        ---
        name: target
        description: "fno missing"
        requires:
          binaries:
            - "gh >= 2.0"
            - "git >= 2.30"
        ---
        body
        """
    )
    _make_skill(tmp_path, "target", skill_md)
    result = _run(["bash", str(LINT), "--root", str(tmp_path)])
    assert result.returncode != 0
    assert "does not declare 'fno'" in result.stderr


def test_requires_with_abi_passes(tmp_path):
    """The exact format target/megawalk/megatron use passes the lint."""
    _make_skill(tmp_path, "target", _CLEAN_SKILL_FRONTMATTER)
    result = _run(["bash", str(LINT), "--root", str(tmp_path)])
    assert result.returncode == 0


# ---------------------------------------------------------------------------
# AC5-EDGE: real repo state passes the lint (regression-defense)
# ---------------------------------------------------------------------------


def test_real_repo_state_passes_lint():
    """After Phases 4/5/6, the real fno repo state must pass. If this
    test fails, a contributor regressed the marketplace-readiness invariant."""
    result = _run(["bash", str(LINT)])
    assert result.returncode == 0, result.stdout + result.stderr


# ---------------------------------------------------------------------------
# Multiple driver skills in fixture: lint covers each independently
# ---------------------------------------------------------------------------


def test_lint_reports_all_violating_skills(tmp_path):
    """When multiple driver skills violate, the lint reports each (doesn't
    stop on the first failure)."""
    bad_skill = _CLEAN_SKILL_FRONTMATTER.replace(
        "Body content. No forbidden patterns.",
        'Body. Skill("target", "...") here.',
    )
    _make_skill(tmp_path, "target", bad_skill)
    _make_skill(tmp_path, "megawalk", bad_skill)
    result = _run(["bash", str(LINT), "--root", str(tmp_path)])
    assert result.returncode != 0
    assert "skills/target" in result.stderr
    assert "skills/megawalk" in result.stderr
