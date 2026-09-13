"""The dispatcher's pin for a spawn into an undeclared foreign repo.

`undeclared_dispatch_pin` is the whole decision; the spawn body only merges
its answer into the provenance overlay. Tests run it directly against real
temp repos, passing the dirs a dispatch actually holds: the worker's cwd and
the caller's cwd.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from fno.worktree_paths import undeclared_dispatch_pin


def _make_repo(path: Path, config_text: str = "") -> Path:
    subprocess.run(["git", "init", "-b", "main", str(path)], check=True, capture_output=True)
    subprocess.run(["git", "config", "user.email", "t@t"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.name", "T"], cwd=path, check=True, capture_output=True)
    (path / "README.md").write_text("x\n")
    subprocess.run(["git", "add", "."], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "commit", "-m", "init"], cwd=path, check=True, capture_output=True)
    if config_text:
        (path / ".fno").mkdir()
        (path / ".fno" / "config.toml").write_text(config_text, encoding="utf-8")
    return path


@pytest.fixture(autouse=True)
def _sole_config(tmp_path_factory, monkeypatch):
    """No global config leak; tests that need repo config rely on the repo."""
    monkeypatch.setenv(
        "FNO_GLOBAL_SETTINGS_PATH", str(tmp_path_factory.mktemp("iso") / "none.yaml")
    )
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.delenv("FNO_WORKTREE_POLICY", raising=False)
    yield


def test_pin_fires_for_undeclared_foreign_repo(tmp_path):
    """AC3-HP: no policy anywhere in the chain pins `never` for the child."""
    caller = _make_repo(tmp_path / "caller")
    target = _make_repo(tmp_path / "target")
    pin = undeclared_dispatch_pin(target, caller, "claude")
    assert pin == {"FNO_WORKTREE_POLICY": "never"}


def test_pin_silent_when_target_declares_its_own_policy(tmp_path):
    """AC3-ERR: the target's own config governs; no variable is exported."""
    caller = _make_repo(tmp_path / "caller")
    target = _make_repo(
        tmp_path / "declared", '[worktree]\npolicy = "harness-native"\n'
    )
    assert undeclared_dispatch_pin(target, caller, "claude") == {}


def test_pin_silent_for_the_callers_own_repo(tmp_path):
    """A dispatch inside the caller's own repo changes nothing."""
    repo = _make_repo(tmp_path / "own")
    assert undeclared_dispatch_pin(repo, repo, "claude") == {}


def test_pin_silent_for_a_worktree_of_the_callers_own_repo(tmp_path):
    """Identity is the common dir: the caller's worktree spawning into its
    own canonical checkout is the same repo, not a foreign target."""
    repo = _make_repo(tmp_path / "own")
    wt = tmp_path / "wt"
    subprocess.run(
        ["git", "-C", str(repo), "worktree", "add", "-q", str(wt), "-b", "side"],
        check=True, capture_output=True,
    )
    assert undeclared_dispatch_pin(repo, wt, "claude") == {}


def test_pin_silent_outside_git(tmp_path):
    """A non-git target has no ceremony to pin; the ambient default applies."""
    caller = _make_repo(tmp_path / "caller")
    plain = tmp_path / "plain"
    plain.mkdir()
    assert undeclared_dispatch_pin(plain, caller, "claude") == {}
    assert undeclared_dispatch_pin(caller, plain, "claude") == {}


def test_pin_silent_when_operator_env_already_decided(tmp_path, monkeypatch):
    """An explicit ambient FNO_WORKTREE_POLICY outranks the dispatcher pin."""
    caller = _make_repo(tmp_path / "caller")
    target = _make_repo(tmp_path / "target")
    monkeypatch.setenv("FNO_WORKTREE_POLICY", "external")
    assert undeclared_dispatch_pin(target, caller, "claude") == {}
