"""Tests for skills/pr/scripts/inbox-has-pr.sh - the idempotency guard.

The post-merge ritual must be a no-op on re-run (BDD: "idempotent re-run").
The guard keys on a `<!-- post-merge:pr-<N> -->` marker so a second run for the
same PR skips writing a duplicate inbox section. Pure file inspection, no `fno`
dependency, so it is deterministic across CLI versions.

Node: ab-6eeb20ae.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

# repo_root/cli/tests/unit/this_file -> parents[3] == repo root
SCRIPT = (
    Path(__file__).resolve().parents[3]
    / "skills"
    / "pr"
    / "scripts"
    / "inbox-has-pr.sh"
)


def _run(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(SCRIPT), *args],
        capture_output=True,
        text=True,
    )


def test_script_exists_and_executable() -> None:
    assert SCRIPT.is_file(), f"helper missing at {SCRIPT}"


def test_absent_marker_exits_1(tmp_path: Path) -> None:
    """No marker for the PR -> safe to write (exit 1)."""
    inbox = tmp_path / "inbox.md"
    inbox.write_text("# Inbox\n\nsome unrelated prose\n", encoding="utf-8")
    assert _run(str(inbox), "123").returncode == 1


def test_present_marker_exits_0(tmp_path: Path) -> None:
    """Marker already present -> skip writing (exit 0)."""
    inbox = tmp_path / "inbox.md"
    inbox.write_text(
        "# Inbox\n\n<!-- post-merge:pr-123 -->\n## Post-merge follow-ups - PR #123\n",
        encoding="utf-8",
    )
    assert _run(str(inbox), "123").returncode == 0


def test_missing_inbox_file_exits_1(tmp_path: Path) -> None:
    """Inbox file does not exist yet -> safe to write (exit 1), no crash."""
    assert _run(str(tmp_path / "nope.md"), "123").returncode == 1


def test_missing_args_exits_2() -> None:
    """Usage error when args are missing."""
    assert _run().returncode == 2
    assert _run("only-one-arg").returncode == 2


def test_hash_prefixed_pr_normalized(tmp_path: Path) -> None:
    """'#123' and '123' are treated identically."""
    inbox = tmp_path / "inbox.md"
    inbox.write_text("<!-- post-merge:pr-123 -->\n", encoding="utf-8")
    assert _run(str(inbox), "#123").returncode == 0


def test_non_numeric_pr_exits_2(tmp_path: Path) -> None:
    """A non-numeric PR id is a usage error, not a silent miss."""
    inbox = tmp_path / "inbox.md"
    inbox.write_text("nothing\n", encoding="utf-8")
    assert _run(str(inbox), "abc").returncode == 2


def test_marker_does_not_false_match_other_pr(tmp_path: Path) -> None:
    """PR 12 must not match a marker for PR 123 (no substring false positive)."""
    inbox = tmp_path / "inbox.md"
    inbox.write_text("<!-- post-merge:pr-123 -->\n", encoding="utf-8")
    assert _run(str(inbox), "12").returncode == 1
    assert _run(str(inbox), "123").returncode == 0


def _git(*args: str, cwd: Path) -> None:
    subprocess.run(["git", *args], cwd=str(cwd), capture_output=True, text=True, check=True)


def test_canonical_root_derivation_finds_marker_across_cwd(tmp_path: Path) -> None:
    """AC2-HP / AC5-FR: the marker written at the CANONICAL parking-lot path is
    found when merged.md derives CANON_ROOT (git --git-common-dir) from a worktree
    cwd, whereas the OLD worktree-root join misses it - the regression x-071c closes.

    The failure precondition is a lane worktree WITHOUT the `internal/` symlink
    (a bg-healed lane): a worktree-root join lands on a nonexistent lane-local
    file, so the belt-and-braces sees no marker and re-runs the destructive ritual.
    """
    canon = tmp_path / "canon"
    canon.mkdir()
    _git("init", "-q", cwd=canon)
    _git("config", "user.email", "t@t", cwd=canon)
    _git("config", "user.name", "t", cwd=canon)
    _git("commit", "--allow-empty", "-qm", "init", cwd=canon)

    rel = "internal/parking-lot.md"
    (canon / "internal").mkdir()
    (canon / rel).write_text(
        "<!-- post-merge:pr-777 -->\n## Post-merge follow-ups - PR #777\n",
        encoding="utf-8",
    )

    # A linked worktree that does NOT carry the canonical `internal/` symlink.
    wt = tmp_path / "wt"
    _git("worktree", "add", "-q", str(wt), "-b", "feature/x", cwd=canon)
    assert not (wt / rel).exists()  # the failure precondition the fix addresses

    # merged.md's CANON_ROOT derivation, run FROM the worktree cwd. Mirrors the
    # skill: --git-common-dir may be relative (git < 2.31 lacks
    # --path-format=absolute), so resolve to absolute with cd/pwd - version-independent.
    derive = (
        'GCD="$(git rev-parse --git-common-dir)"; '
        'case "$GCD" in /*) ;; *) GCD="$(cd "$GCD" && pwd)" ;; esac; '
        'printf %s "$(dirname "$GCD")"'
    )
    canon_root = subprocess.run(
        ["bash", "-c", derive], cwd=str(wt), capture_output=True, text=True, check=True
    ).stdout.strip()
    assert Path(canon_root).resolve() == canon.resolve()

    # Canonical-derived path -> marker found (exit 0).
    assert _run(f"{canon_root}/{rel}", "777").returncode == 0
    # OLD worktree-root join -> marker missed (exit 1): the bug the fix removes.
    assert _run(f"{wt}/{rel}", "777").returncode == 1
