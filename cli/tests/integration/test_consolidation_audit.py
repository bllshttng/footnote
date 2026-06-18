"""Integration tests for `fno consolidation audit`.

These exercise the bash audit script via the Python wrapper so we get the
whole call path under test. They run the real script against a temporary
worktree to avoid coupling to the main repo's current cleanup state.
"""
from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[3]
AUDIT_SCRIPT = REPO_ROOT / "scripts" / "ci" / "check-no-stale-skill-refs.sh"


def _make_tiny_repo(tmp_path: Path) -> Path:
    """Build a minimal repo skeleton the audit script can scan."""
    subprocess.run(["git", "init", "-q", "-b", "main"], cwd=tmp_path, check=True)
    # Match the scan paths the script expects.
    for sub in ("hooks", "scripts", "agents", "skills", ".claude-plugin", "cli/src/fno"):
        (tmp_path / sub).mkdir(parents=True, exist_ok=True)
    # Copy the audit script into the same relative location so resolve_repo_root works.
    audit_dest = tmp_path / "scripts" / "ci" / "check-no-stale-skill-refs.sh"
    audit_dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy(AUDIT_SCRIPT, audit_dest)
    audit_dest.chmod(0o755)
    subprocess.run(["git", "add", "-A"], cwd=tmp_path, check=True)
    subprocess.run(
        ["git", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"],
        cwd=tmp_path,
        check=True,
    )
    return tmp_path


def _run_audit(repo: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", "scripts/ci/check-no-stale-skill-refs.sh"],
        cwd=repo,
        capture_output=True,
        text=True,
    )


def test_audit_passes_on_clean_tree(tmp_path: Path) -> None:
    """An empty scan surface has no stale refs; audit exits 0."""
    repo = _make_tiny_repo(tmp_path)
    result = _run_audit(repo)
    assert result.returncode == 0, result.stdout + result.stderr
    assert "AUDIT PASS" in result.stdout


def test_audit_fails_on_planted_stale_ref(tmp_path: Path) -> None:
    """Plant a hook that sources a removed skill; audit must fail."""
    repo = _make_tiny_repo(tmp_path)
    bad = repo / "hooks" / "fake-hook.sh"
    bad.write_text("#!/usr/bin/env bash\nsource skills/distill/scripts/foo.sh\n")
    result = _run_audit(repo)
    assert result.returncode == 1, result.stdout + result.stderr
    assert "distill" in result.stdout
    assert "AUDIT FAIL" in result.stdout


def test_audit_ignores_allowlisted_paths(tmp_path: Path) -> None:
    """A reference inside an allowlisted prose folder does NOT fail the audit."""
    repo = _make_tiny_repo(tmp_path)
    # internal/fno/reason/ is allowlisted; the path lives outside the
    # default scan paths so even raw greps shouldn't see it, but document it
    # explicitly here so a future refactor doesn't quietly drop the rule.
    reason_doc = repo / "internal" / "fno" / "reason" / "history.md"
    reason_doc.parent.mkdir(parents=True, exist_ok=True)
    reason_doc.write_text("Once upon a time /distill was a skill.\n")
    result = _run_audit(repo)
    assert result.returncode == 0, result.stdout + result.stderr


def test_audit_self_reference_in_script_is_allowlisted(tmp_path: Path) -> None:
    """The audit script names the skills it audits; that is not a stale ref."""
    repo = _make_tiny_repo(tmp_path)
    # The script copied into the tmp repo names every retired skill in its
    # arrays. Without the self-reference allowlist this would always fail.
    result = _run_audit(repo)
    assert result.returncode == 0, result.stdout + result.stderr


def test_audit_ignores_bare_word_matches(tmp_path: Path) -> None:
    """Bare-word occurrences of a skill name in prose lists do NOT trigger the audit.

    Example: the impeccable-stages baseline list contains the bare token
    `distill` as a stage name, unrelated to the removed `/distill` skill. The
    audit's pattern must discriminate against this kind of false positive.
    """
    repo = _make_tiny_repo(tmp_path)
    benign = repo / "skills" / "impeccable" / "SKILL.md"
    benign.parent.mkdir(parents=True, exist_ok=True)
    benign.write_text("Known stages: craft, polish, distill, extract, shape.\n")
    result = _run_audit(repo)
    assert result.returncode == 0, result.stdout + result.stderr


def test_audit_ignores_artifact_paths(tmp_path: Path) -> None:
    """`.fno/codemap.md` is the output file - the name survives demotion.

    The codemap skill demotes to `fno codemap` but the artifact at
    `.fno/codemap.md` keeps its name. Path references with a file
    extension (`.md`, `.sh`, `.py`) after the skill name are artifact paths,
    not skill references, and must not trigger the audit.
    """
    repo = _make_tiny_repo(tmp_path)
    doc = repo / "skills" / "blueprint" / "SKILL.md"
    doc.parent.mkdir(parents=True, exist_ok=True)
    doc.write_text(
        "Read `.fno/codemap.md` if it exists.\n"
        "Also try /tmp/codemap.txt as a fallback.\n"
    )
    result = _run_audit(repo)
    assert result.returncode == 0, result.stdout + result.stderr


def test_audit_catches_slash_command_form(tmp_path: Path) -> None:
    """`/distill` slash-command reference in a hook must fail the audit."""
    repo = _make_tiny_repo(tmp_path)
    hook = repo / "hooks" / "promote.sh"
    hook.write_text("#!/usr/bin/env bash\necho 'run /distill on output'\n")
    result = _run_audit(repo)
    assert result.returncode == 1, result.stdout + result.stderr
    assert "distill" in result.stdout


def test_audit_rejects_malformed_skill_name(tmp_path: Path) -> None:
    """Sanity check: if someone wrote a regex meta in CUT_SKILLS the script bails."""
    repo = _make_tiny_repo(tmp_path)
    script = repo / "scripts" / "ci" / "check-no-stale-skill-refs.sh"
    text = script.read_text()
    # Inject a malformed name into the CUT_SKILLS array.
    text = text.replace(
        'CUT_SKILLS=(distill megaspec tower-play tower-watch copy-this)',
        'CUT_SKILLS=("bad.name" megaspec tower-play tower-watch copy-this)',
    )
    script.write_text(text)
    result = _run_audit(repo)
    assert result.returncode == 2, result.stdout + result.stderr
    assert "malformed skill name" in result.stderr


@pytest.mark.skipif(
    not (REPO_ROOT / "cli" / "pyproject.toml").exists(),
    reason="run from the cli workspace",
)
def test_abi_consolidation_audit_wrapper_matches_bash() -> None:
    """`fno consolidation audit` exits with the same code as the bash script."""
    env = {**os.environ}
    cli_dir = REPO_ROOT / "cli"
    bash_proc = subprocess.run(
        ["bash", "scripts/ci/check-no-stale-skill-refs.sh"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        env=env,
    )
    abi_proc = subprocess.run(
        ["uv", "run", "fno", "consolidation", "audit"],
        cwd=cli_dir,
        capture_output=True,
        text=True,
        env=env,
    )
    assert bash_proc.returncode == abi_proc.returncode, (
        f"bash={bash_proc.returncode}, fno={abi_proc.returncode}\n"
        f"bash stdout: {bash_proc.stdout[-500:]}\n"
        f"fno stdout: {abi_proc.stdout[-500:]}"
    )
