"""resolve_worktree_policy: the one location answer (x-f96e task 1.2).

Lives under cli/tests (not cli/src/fno) to keep the source dir inside its
line budget; imports the same public surface the src-tree tests use.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

from fno.worktree_paths import WorktreePolicyError, resolve_worktree_policy


def _make_repo(path: Path) -> Path:
    subprocess.run(["git", "init", "-b", "main", str(path)], check=True, capture_output=True)
    subprocess.run(["git", "config", "user.email", "t@t"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.name", "T"], cwd=path, check=True, capture_output=True)
    (path / "README.md").write_text("x\n")
    subprocess.run(["git", "add", "."], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "commit", "-m", "init"], cwd=path, check=True, capture_output=True)
    return path


@pytest.fixture(autouse=True)
def _sole_config(tmp_path_factory, monkeypatch):
    """FNO_CONFIG is the sole config candidate; an empty file = defaults only.

    No settings-cache clearing: resolve_worktree_policy reads the raw config
    per call, and a sibling test's stub of the settings loader must not
    decide whether these cache attributes exist.
    """
    iso = tmp_path_factory.mktemp("iso") / "config.toml"
    iso.write_text("")
    monkeypatch.setenv("FNO_CONFIG", str(iso))
    yield


# ----------------------------------------------------------------------
# resolve_worktree_policy: one location answer (x-f96e task 1.2)
# ----------------------------------------------------------------------
#
# The autouse fixture pins FNO_CONFIG to a file each test may rewrite; a
# pinned FNO_CONFIG makes the loader ignore repo-local config by design.


def _policy_repo(path: Path, config_text: str = "") -> Path:
    """A repo for policy resolution; config_text goes to FNO_CONFIG."""
    repo = _make_repo(path)
    (repo / ".fno").mkdir()
    if config_text:
        Path(os.environ["FNO_CONFIG"]).write_text(config_text, encoding="utf-8")
    return repo


def test_explicit_worktrees_base_alone_relocates(tmp_path):
    """AC4-HP: the key degrades harness-native to external, no second key."""
    base_dir = tmp_path / "wtbase"
    repo = _policy_repo(
        tmp_path / "relocated",
        f'[paths]\nworktrees_base = "{base_dir}"\n',
    )
    pol = resolve_worktree_policy(repo, "claude")
    assert pol.policy == "external"
    assert pol.base == base_dir
    assert pol.degraded is True
    assert pol.requested_policy == "harness-native"


def test_both_keys_unset_keeps_harness_native_claude_default(tmp_path):
    """AC5-HP: today's default is unchanged."""
    repo = _policy_repo(tmp_path / "defaulted")
    pol = resolve_worktree_policy(repo, "claude")
    assert pol.policy == "harness-native"
    assert pol.degraded is False


def test_never_policy_unchanged_by_an_explicit_base(tmp_path):
    """AC6-EDGE: `never` is not a location and an explicit base cannot flip it."""
    base_dir = tmp_path / "wtbase"
    repo = _policy_repo(
        tmp_path / "neverrepo",
        f'[worktree]\npolicy = "never"\n[paths]\nworktrees_base = "{base_dir}"\n',
    )
    pol = resolve_worktree_policy(repo, "claude")
    assert pol.policy == "never"


def test_explicit_external_policy_uses_the_configured_base(tmp_path):
    base_dir = tmp_path / "wtbase"
    repo = _policy_repo(
        tmp_path / "explicitext",
        f'[worktree]\npolicy = "external"\n[paths]\nworktrees_base = "{base_dir}"\n',
    )
    pol = resolve_worktree_policy(repo, "claude")
    assert pol.policy == "external"
    assert pol.base == base_dir
    assert pol.degraded is False


def test_repo_config_base_relocates_too(tmp_path):
    """Same key via the sole config file, exercising the merged-config read."""
    base_dir = tmp_path / "repo-base"
    repo = _policy_repo(
        tmp_path / "repobase",
        f'[paths]\nworktrees_base = "{base_dir}"\n',
    )
    pol = resolve_worktree_policy(repo, "claude")
    assert pol.policy == "external"
    assert pol.base == base_dir


def test_deprecated_conductor_key_relocates_with_note(tmp_path):
    repo = _policy_repo(
        tmp_path / "conductor",
        '[worktree]\nuse_conductor_canonical = true\n',
    )
    pol = resolve_worktree_policy(repo, "claude")
    assert pol.policy == "external"
    assert pol.base == (Path.home() / "conductor" / "workspaces").resolve()
    assert "DEPRECATED" in pol.note


def test_non_native_harness_degradation_keeps_fallback_base(tmp_path, monkeypatch):
    """A codex harness still degrades to the state-dir fallback, NOT to an
    explicitly configured base: that base is an external allocator choice
    and must not make an unsupported session look allocator-owned."""
    monkeypatch.setenv("HOME", str(tmp_path))
    base_dir = tmp_path / "wtbase"
    repo = _policy_repo(
        tmp_path / "codexrepo",
        f'[paths]\nworktrees_base = "{base_dir}"\n',
    )
    pol = resolve_worktree_policy(repo, "codex")
    assert pol.policy == "external"
    assert pol.base != base_dir
    assert pol.base == (tmp_path / ".fno" / "worktrees").resolve()


def test_out_of_enum_policy_still_refuses(tmp_path):
    repo = _policy_repo(
        tmp_path / "badpolicy",
        '[worktree]\npolicy = "sideways"\n',
    )
    with pytest.raises(WorktreePolicyError):
        resolve_worktree_policy(repo, "claude")
