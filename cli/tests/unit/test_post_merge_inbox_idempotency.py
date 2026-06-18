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
