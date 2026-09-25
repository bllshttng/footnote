"""SessionStart hook: the plugin-root pointer never names a linked worktree.

The hook copy that runs can live in a linked worktree (a spawned worker's
feature worktree), so the parent of its script dir is NOT an installed
plugin. Writing that root to ~/.fno/plugin-root repoints every env-less
reader machine-wide at an unmerged branch the merge sweep later reaps.
A linked worktree has a .git FILE; a canonical checkout has a .git
DIRECTORY; an installed stage or plugin cache has none.
"""
from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[3]
HOOK = ROOT / "hooks" / "session-start.sh"
INSTALLED = "/installed/stage"


def _make_plugin_root(root: Path, git_shape: str | None) -> Path:
    """A plugin root whose hook is the real one reached through a symlink,
    so SCRIPT_DIR (cd + pwd, no -P) resolves inside root. git_shape: "file"
    (linked worktree), "dir" (canonical checkout), None (installed)."""
    (root / ".claude-plugin").mkdir(parents=True)
    (root / ".claude-plugin" / "plugin.json").write_text("{}")
    (root / "hooks").mkdir()
    (root / "hooks" / "session-start.sh").symlink_to(HOOK)
    if git_shape == "file":
        (root / ".git").write_text("gitdir: /nowhere\n")
    elif git_shape == "dir":
        (root / ".git").mkdir()
    return root


def _run_hook(root: Path, home: Path) -> None:
    bin_dir = home / "bin"
    bin_dir.mkdir(exist_ok=True)
    jq_dir = Path(shutil.which("jq")).parent
    env = {
        # No `fno` on PATH: the pointer write happens before the heal step,
        # and an absent fno keeps that step inert.
        "PATH": f"{bin_dir}:{jq_dir}:/usr/bin:/bin",
        "HOME": str(home),
        "FNO_HOME": str(home / ".fno"),
        "FNO_TEST_HERMETIC": "1",
        "CLAUDE_PROJECT_DIR": str(home),
    }
    subprocess.run(
        ["bash", str(root / "hooks" / "session-start.sh")],
        check=True,
        cwd=home,
        env=env,
        input="{}",
        text=True,
        stdout=subprocess.DEVNULL,
    )


@pytest.mark.parametrize(
    "git_shape,pointer_names_root",
    [
        ("file", False),  # linked worktree: never captured
        ("dir", True),  # canonical --plugin-dir checkout: still writes
        (None, True),  # installed stage: still writes
    ],
)
def test_pointer_gated_on_git_shape(tmp_path, git_shape, pointer_names_root):
    root = _make_plugin_root(tmp_path / "plugin", git_shape)
    home = tmp_path / "home"
    fno_home = home / ".fno"
    fno_home.mkdir(parents=True)
    (fno_home / "plugin-root").write_text(INSTALLED + "\n")
    _run_hook(root, home)
    ptr = fno_home / "plugin-root"
    if pointer_names_root:
        assert ptr.read_text().strip() == str(root)
    else:
        assert ptr.read_text().strip() == INSTALLED


def test_real_linked_worktree_never_writes_pointer(tmp_path):
    """The same guard proved on the shape a worker actually runs from:
    a real `git worktree add` checkout, not just a hand-made .git file."""
    canon = _make_plugin_root(tmp_path / "canon", None)
    subprocess.run(
        ["git", "-C", str(canon), "-c", "user.email=t@t", "-c", "user.name=t",
         "init", "-q"],
        check=True, capture_output=True,
    )
    subprocess.run(
        ["git", "-C", str(canon), "-c", "user.email=t@t", "-c", "user.name=t",
         "add", ".claude-plugin"],
        check=True, capture_output=True,
    )
    subprocess.run(
        ["git", "-C", str(canon), "-c", "user.email=t@t", "-c", "user.name=t",
         "commit", "-q", "-m", "init"],
        check=True, capture_output=True,
    )
    wt = tmp_path / "wt"
    subprocess.run(
        ["git", "-C", str(canon), "worktree", "add", "-q", str(wt)],
        check=True, capture_output=True,
    )
    (wt / "hooks").mkdir()
    (wt / "hooks" / "session-start.sh").symlink_to(HOOK)
    home = tmp_path / "home"
    fno_home = home / ".fno"
    fno_home.mkdir(parents=True)
    (fno_home / "plugin-root").write_text(INSTALLED + "\n")
    _run_hook(wt, home)
    assert (fno_home / "plugin-root").read_text().strip() == INSTALLED


def test_worktree_start_neither_flips_stamp_nor_repairs(tmp_path):
    """A worktree start must not flip the .worktree-hook-root stamp either,
    or every alternating worktree and installed start re-runs the repair."""
    root = _make_plugin_root(tmp_path / "plugin", "file")
    home = tmp_path / "home"
    fno_home = home / ".fno"
    fno_home.mkdir(parents=True)
    (fno_home / ".worktree-hook-root").write_text(INSTALLED + "\n")
    (home / ".claude").mkdir(parents=True)
    (home / ".claude" / "settings.json").write_text(
        '{"hooks": {"SessionEnd": ["worktree-remove.sh"]}}'
    )
    capture = tmp_path / "fno-argv"
    (home / "bin").mkdir(parents=True)
    fno = home / "bin" / "fno"
    fno.write_text(f'#!/usr/bin/env bash\nprintf \'%s\\n\' "$@" >> "{capture}"\n')
    fno.chmod(0o755)
    _run_hook(root, home)
    assert (fno_home / ".worktree-hook-root").read_text().strip() == INSTALLED
    assert not capture.exists(), "a worktree start must not run the repair"
