"""Integration tests for WorktreeManager class in fno.worktree.

All tests use tmp_path so we never pollute real worktrees.

Test names follow the phase-02b acceptance criteria in the plan doc.
"""
from __future__ import annotations

import json
import subprocess
import time
from pathlib import Path
from unittest.mock import patch

import pytest

from fno.worktree import (
    Worktree,
    WorktreeError,
    WorktreeManager,
    WorktreeDiskPressureError,
    WorktreeStaleError,
)


# ---------------------------------------------------------------------------
# Shared fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def tmp_git_repo(tmp_path: Path) -> Path:
    """A minimal git repo with an initial commit on main.

    Pinned to -b main so tests that pass base_ref='main' work regardless
    of the host's init.defaultBranch setting.
    """
    subprocess.run(
        ["git", "init", "-b", "main", str(tmp_path)],
        check=True, capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=tmp_path, check=True, capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test User"],
        cwd=tmp_path, check=True, capture_output=True,
    )
    readme = tmp_path / "README.md"
    readme.write_text("# Test Repo\n")
    subprocess.run(["git", "add", "README.md"], cwd=tmp_path, check=True, capture_output=True)
    subprocess.run(
        ["git", "commit", "-m", "initial"],
        cwd=tmp_path, check=True, capture_output=True,
    )
    return tmp_path


@pytest.fixture
def wm(tmp_git_repo: Path) -> WorktreeManager:
    """WorktreeManager pointed at tmp_git_repo with a custom base_dir."""
    base_dir = tmp_git_repo / ".claude" / "worktrees"
    return WorktreeManager(repo_root=tmp_git_repo, base_dir=base_dir)


# ---------------------------------------------------------------------------
# Task 2b.2 - create()
# ---------------------------------------------------------------------------


def test_create_makes_worktree_with_unique_branch(wm: WorktreeManager, tmp_git_repo: Path):
    """create() produces a worktree dir and a branch named feature/{last8}."""
    node_id = "ab-12345678"
    wt = wm.create(node_id, base_ref="main")

    # Path should exist on disk
    assert wt.path.exists(), f"expected worktree at {wt.path}"
    # Branch should be feature/{last 8 chars}
    assert wt.branch == "feature/12345678"
    # The git worktree list should include the path
    result = subprocess.run(
        ["git", "worktree", "list", "--porcelain"],
        cwd=tmp_git_repo, capture_output=True, text=True,
    )
    assert str(wt.path) in result.stdout


def test_create_prunes_stale_registration(wm: WorktreeManager, tmp_git_repo: Path):
    """create() calls 'git worktree prune' before creating so stale registrations
    do not block a fresh create call.
    """
    # Simulate a stale registration: register a path with git then delete the dir
    stale_path = wm.base_dir / "ab-staleabc"
    wm.base_dir.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "worktree", "add", "-b", "feature/staleabc", str(stale_path), "main"],
        cwd=tmp_git_repo, check=True, capture_output=True,
    )
    # Remove the dir manually, leaving a stale registration
    import shutil
    shutil.rmtree(stale_path)

    # Now create a new worktree - prune must happen first so no stale error
    wt = wm.create("ab-newnode1", base_ref="main")
    assert wt.path.exists()


def test_create_refuses_below_2gb(wm: WorktreeManager):
    """create() raises WorktreeDiskPressureError when free disk < 2GB."""
    import shutil

    # Mock disk_usage to return tiny free space
    fake_usage = shutil.disk_usage.__class__  # just for typing
    with patch("shutil.disk_usage") as mock_du:
        mock_du.return_value = type("DU", (), {"free": 1 * 1024 ** 3, "total": 500 * 1024 ** 3})()
        with pytest.raises(WorktreeDiskPressureError):
            wm.create("ab-disktest", base_ref="main")


# ---------------------------------------------------------------------------
# Task 2b.3 - remove() and archive()
# ---------------------------------------------------------------------------


def test_remove_idempotent(wm: WorktreeManager, tmp_git_repo: Path):
    """remove() called twice does not raise on the second call."""
    wt = wm.create("ab-rm123456", base_ref="main")
    assert wt.path.exists()

    wm.remove(wt)
    assert not wt.path.exists()

    # Second remove must be a no-op, not an error
    wm.remove(wt)


def test_archive_preserves_dir(wm: WorktreeManager, tmp_git_repo: Path):
    """archive() moves the worktree to .archived/ and writes a reason file."""
    wt = wm.create("ab-arch1234", base_ref="main")
    assert wt.path.exists()

    archived_path = wm.archive(wt, reason="stuck")

    # Original dir is gone
    assert not wt.path.exists(), "original worktree dir should be gone after archive"
    # Archived copy exists
    assert archived_path.exists(), f"archived dir should exist at {archived_path}"
    # Reason file is present
    reason_file = archived_path / ".archive-reason.txt"
    assert reason_file.exists(), "reason file must be written inside archive dir"
    assert "stuck" in reason_file.read_text()


# ---------------------------------------------------------------------------
# Task 2b.5 - list_orphaned()
# ---------------------------------------------------------------------------


def test_list_orphaned_finds_unregistered(wm: WorktreeManager, tmp_git_repo: Path):
    """A worktree dir with no matching node in graph is listed as orphaned."""
    wt = wm.create("ab-orp12345", base_ref="main")
    assert wt.path.exists()

    # Graph has no entry for this node_id
    graph: dict = {}
    orphans = wm.list_orphaned(graph)
    node_ids = [o.node_id for o in orphans]
    assert "ab-orp12345" in node_ids, f"expected ab-orp12345 in orphans, got {node_ids}"


def test_list_orphaned_finds_done(wm: WorktreeManager, tmp_git_repo: Path):
    """A worktree whose graph node is _status:done is listed as orphaned."""
    wt = wm.create("ab-done5678", base_ref="main")
    assert wt.path.exists()

    graph = {
        "ab-done5678": {"_status": "done", "title": "finished node"},
    }
    orphans = wm.list_orphaned(graph)
    node_ids = [o.node_id for o in orphans]
    assert "ab-done5678" in node_ids, f"expected ab-done5678 in orphans, got {node_ids}"


# ---------------------------------------------------------------------------
# Task 2b.4 - is_stuck()
# ---------------------------------------------------------------------------


def test_is_stuck_true_when_quiet(wm: WorktreeManager, tmp_git_repo: Path):
    """is_stuck returns True when git is clean and no recent progress entries."""
    wt = wm.create("ab-stuck001", base_ref="main")

    # Ensure the progress file is absent (no activity)
    progress_file = wt.path / ".fno" / "agent-progress.jsonl"
    # Don't create it - absence means no progress

    result = wm.is_stuck(wt, threshold_minutes=20)
    assert result is True, "should be stuck when git clean + no progress file"


def test_is_stuck_false_with_recent_progress(wm: WorktreeManager, tmp_git_repo: Path):
    """is_stuck returns False when a progress entry was written recently."""
    wt = wm.create("ab-stuck002", base_ref="main")

    # Write a progress entry timestamped now
    progress_dir = wt.path / ".fno"
    progress_dir.mkdir(parents=True, exist_ok=True)
    progress_file = progress_dir / "agent-progress.jsonl"
    now_ts = time.time()
    progress_file.write_text(
        json.dumps({"ts": now_ts, "phase": "implement", "msg": "working"}) + "\n"
    )

    result = wm.is_stuck(wt, threshold_minutes=20)
    assert result is False, "should not be stuck with a recent progress entry"


def test_is_stuck_false_with_active_git_changes(wm: WorktreeManager, tmp_git_repo: Path):
    """is_stuck returns False when new git changes appear since the last poll."""
    wt = wm.create("ab-stuck003", base_ref="main")

    # First poll: working tree is clean - snapshot is taken
    # We need to prime the snapshot as if a prior poll happened
    # by calling is_stuck once (will return True, no changes yet)
    # Then add a file to trigger a new change
    _ = wm.is_stuck(wt, threshold_minutes=20)  # prime snapshot

    # Now make a change in the worktree
    (wt.path / "new_file.txt").write_text("some change\n")

    # Second call: git status changed since last snapshot -> not stuck
    result = wm.is_stuck(wt, threshold_minutes=20)
    assert result is False, "should not be stuck when git status changed since last poll"


def test_is_stuck_handles_lsp_autosave(wm: WorktreeManager, tmp_git_repo: Path):
    """is_stuck treats 'git status non-empty but unchanged' as quiet (LSP autosave).

    An LSP server may continuously autosave a file, keeping git status non-empty
    but producing the exact same output on every poll. This should still count as
    quiet/stuck since no real work is happening.
    """
    wt = wm.create("ab-stuck004", base_ref="main")

    # Create an untracked file to make git status non-empty
    autosave_file = wt.path / "lsp_autosave.py"
    autosave_file.write_text("# autosaved content\n")

    # Prime the snapshot by calling is_stuck once
    _ = wm.is_stuck(wt, threshold_minutes=20)

    # File content doesn't change (same autosave) - git status unchanged
    # is_stuck must treat this as quiet (stuck), not as activity
    result = wm.is_stuck(wt, threshold_minutes=20)
    assert result is True, (
        "LSP autosave (non-empty but unchanged git status) must be treated as quiet"
    )


# ---------------------------------------------------------------------------
# BUG-MR-003: canonical worktree wiring
# ---------------------------------------------------------------------------


class TestCanonicalWorktreeWiring:
    """Regression: WorktreeManager must honor worktree.use_conductor_canonical
    and invoke setup-worktree.sh after `git worktree add`.

    Without the latter, megawalk worktrees have no symlink to canonical
    `.fno/` state, so target gates can't see backlog mutations from
    sibling worktrees, codemap is stale, inbox drain breaks.
    """

    def test_use_conductor_canonical_flag_routes_default_base_dir(self, tmp_path):
        """When config.worktree.use_conductor_canonical=true is set in
        .fno/settings.yaml under repo_root, the default base_dir is
        ~/conductor/workspaces/<repo_root.name>.
        """
        from fno.worktree import WorktreeManager
        repo_root = tmp_path / "my-repo"
        (repo_root / ".fno").mkdir(parents=True)
        (repo_root / ".fno" / "settings.yaml").write_text(
            "config:\n  worktree:\n    use_conductor_canonical: true\n"
        )
        wm = WorktreeManager(repo_root=repo_root)
        assert wm.base_dir == Path.home() / "conductor" / "workspaces" / "my-repo"

    def test_flag_absent_keeps_local_default(self, tmp_path):
        from fno.worktree import WorktreeManager
        repo_root = tmp_path / "other-repo"
        repo_root.mkdir()
        # No .fno/settings.yaml at all
        wm = WorktreeManager(repo_root=repo_root)
        assert wm.base_dir == repo_root / ".claude" / "worktrees"

    def test_flag_false_keeps_local_default(self, tmp_path):
        from fno.worktree import WorktreeManager
        repo_root = tmp_path / "false-repo"
        (repo_root / ".fno").mkdir(parents=True)
        (repo_root / ".fno" / "settings.yaml").write_text(
            "config:\n  worktree:\n    use_conductor_canonical: false\n"
        )
        wm = WorktreeManager(repo_root=repo_root)
        assert wm.base_dir == repo_root / ".claude" / "worktrees"

    def test_worktrees_base_routes_default_base_dir(self, tmp_path, monkeypatch):
        """config.paths.worktrees_base set -> {base}/{repo_root.name} (x-33e9)."""
        monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")  # isolate global
        from fno.worktree import WorktreeManager
        repo_root = tmp_path / "based-repo"
        (repo_root / ".fno").mkdir(parents=True)
        (repo_root / ".fno" / "settings.yaml").write_text(
            "config:\n  paths:\n    worktrees_base: " + str(tmp_path / "wtroot") + "\n"
        )
        wm = WorktreeManager(repo_root=repo_root)
        assert wm.base_dir == tmp_path / "wtroot" / "based-repo"

    def test_worktrees_base_expands_tilde(self, tmp_path, monkeypatch):
        """A leading ~ in worktrees_base expands to $HOME."""
        monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
        from fno.worktree import WorktreeManager
        repo_root = tmp_path / "tilde-repo"
        (repo_root / ".fno").mkdir(parents=True)
        (repo_root / ".fno" / "settings.yaml").write_text(
            "config:\n  paths:\n    worktrees_base: ~/myworktrees\n"
        )
        wm = WorktreeManager(repo_root=repo_root)
        assert wm.base_dir == Path.home() / "myworktrees" / "tilde-repo"

    def test_worktrees_base_wins_over_conductor_flag(self, tmp_path, monkeypatch):
        """worktrees_base takes precedence over the deprecated conductor flag."""
        monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
        from fno.worktree import WorktreeManager
        repo_root = tmp_path / "both-repo"
        (repo_root / ".fno").mkdir(parents=True)
        (repo_root / ".fno" / "settings.yaml").write_text(
            "config:\n"
            "  paths:\n    worktrees_base: " + str(tmp_path / "wtroot") + "\n"
            "  worktree:\n    use_conductor_canonical: true\n"
        )
        wm = WorktreeManager(repo_root=repo_root)
        assert wm.base_dir == tmp_path / "wtroot" / "both-repo"

    def test_conductor_flag_read_from_top_level_worktree_block(self, tmp_path, monkeypatch):
        """Real settings.yaml stores the flag top-level (worktree:), which the bash
        hook reads; the walker must agree (codex P1 on PR #67)."""
        monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
        from fno.worktree import WorktreeManager
        repo_root = tmp_path / "toplevel-repo"
        (repo_root / ".fno").mkdir(parents=True)
        (repo_root / ".fno" / "settings.yaml").write_text(
            "worktree:\n  use_conductor_canonical: true\n"  # top-level, not config.worktree
        )
        wm = WorktreeManager(repo_root=repo_root)
        assert wm.base_dir == Path.home() / "conductor" / "workspaces" / "toplevel-repo"

    def test_worktrees_base_read_from_global_settings(self, tmp_path, monkeypatch):
        """A GLOBALLY-set worktrees_base (the maintainer's setup) is honored even
        when the repo has no local setting (gemini HIGH on PR #67)."""
        global_file = tmp_path / "global-settings.yaml"
        global_file.write_text(
            "config:\n  paths:\n    worktrees_base: " + str(tmp_path / "gbase") + "\n"
        )
        monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(global_file))
        from fno.worktree import WorktreeManager
        repo_root = tmp_path / "global-repo"
        (repo_root / ".fno").mkdir(parents=True)  # no local worktrees_base
        wm = WorktreeManager(repo_root=repo_root)
        assert wm.base_dir == tmp_path / "gbase" / "global-repo"

    def test_malformed_config_falls_back_to_local_default(self, tmp_path, monkeypatch):
        """A non-dict config block must not raise (gemini HIGH: dict guards)."""
        monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
        from fno.worktree import WorktreeManager
        repo_root = tmp_path / "bad-repo"
        (repo_root / ".fno").mkdir(parents=True)
        (repo_root / ".fno" / "settings.yaml").write_text("config: not-a-dict\n")
        wm = WorktreeManager(repo_root=repo_root)
        assert wm.base_dir == repo_root / ".claude" / "worktrees"

    def test_create_invokes_setup_worktree_hook_when_script_exists(self, tmp_git_repo, monkeypatch):
        """After `git worktree add`, create() shells out to
        scripts/setup/setup-worktree.sh if present, passing CANONICAL +
        WORKTREE so it links shared state without depending on cwd.
        """
        from fno.worktree import WorktreeManager

        # Plant a fake setup-worktree.sh that records the env vars it received.
        script_dir = tmp_git_repo / "scripts" / "setup"
        script_dir.mkdir(parents=True, exist_ok=True)
        marker = tmp_git_repo / "setup-hook-marker.txt"
        (script_dir / "setup-worktree.sh").write_text(
            f"#!/usr/bin/env bash\n"
            f"echo \"CANONICAL=${{CANONICAL}}\" >> {marker}\n"
            f"echo \"WORKTREE=${{WORKTREE}}\" >> {marker}\n"
            f"echo \"PWD=$(pwd)\" >> {marker}\n"
        )
        (script_dir / "setup-worktree.sh").chmod(0o755)

        base_dir = tmp_git_repo / ".claude" / "worktrees"
        wm = WorktreeManager(repo_root=tmp_git_repo, base_dir=base_dir)
        wt = wm.create("ab-hookwire", base_ref="main")

        assert marker.exists(), "setup-worktree.sh should have been invoked"
        contents = marker.read_text()
        assert f"CANONICAL={tmp_git_repo}" in contents
        assert f"WORKTREE={wt.path}" in contents
        assert f"PWD={wt.path}" in contents

    def test_create_silent_when_setup_script_absent(self, wm):
        """No scripts/setup/setup-worktree.sh under repo_root -> create
        succeeds, no error raised, no script invoked.
        """
        wt = wm.create("ab-noscrip1", base_ref="main")
        # Nothing to assert positively beyond "create returned" - just that
        # absence of the hook does not raise.
        assert wt.path.exists()

    def test_create_logs_but_does_not_raise_when_script_fails(self, tmp_git_repo, capsys):
        from fno.worktree import WorktreeManager

        script_dir = tmp_git_repo / "scripts" / "setup"
        script_dir.mkdir(parents=True, exist_ok=True)
        # Failing script: exit 17 with a clear marker.
        (script_dir / "setup-worktree.sh").write_text(
            "#!/usr/bin/env bash\necho 'symlink wiring failed' >&2\nexit 17\n"
        )
        (script_dir / "setup-worktree.sh").chmod(0o755)

        base_dir = tmp_git_repo / ".claude" / "worktrees"
        wm = WorktreeManager(repo_root=tmp_git_repo, base_dir=base_dir)
        # create() must not raise even though the hook returned 17.
        wt = wm.create("ab-failhk01", base_ref="main")
        assert wt.path.exists()
        captured = capsys.readouterr()
        assert "exited 17" in captured.err, captured.err
