"""Tests for the shared adapter helpers."""
from __future__ import annotations

import subprocess
from pathlib import Path
from unittest import mock

import pytest

from fno.adapters._shared import create_worktree


@pytest.fixture(autouse=True)
def _hermetic_worktree_config(tmp_path_factory, monkeypatch):
    """Isolate global settings so this repo's own config.paths.worktrees_base
    can't leak into these default-path-shape assertions. FNO_CONFIG
    short-circuits the loader to an empty file, so only defaults + HOME apply;
    project.id still resolves from each test's repo .fno/settings.yaml (read
    directly, not via load_settings)."""
    iso = tmp_path_factory.mktemp("iso") / "config.toml"
    iso.write_text("")
    monkeypatch.setenv("FNO_CONFIG", str(iso))
    from fno import config as _config
    from fno import paths as _paths
    _config.load_settings.cache_clear()
    _paths._settings.cache_clear()
    yield
    _config.load_settings.cache_clear()
    _paths._settings.cache_clear()


def _expected_path(tmp_path: Path, name: str, *, project_id: str = "fno") -> Path:
    """Helper: the canonical worktree path under a fake HOME."""
    return tmp_path / ".fno" / "worktrees" / f"{project_id}-{name}"


def test_create_worktree_returns_created_when_path_is_fresh(tmp_path, monkeypatch):
    """AC1.1-HP: A fresh worktree path triggers `git worktree add -b` and returns status=created."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HOME", str(tmp_path))

    calls: list = []

    def fake_run(cmd, **kwargs):
        calls.append({"cmd": cmd, "kwargs": kwargs})
        # show-ref returns 1 (branch does not exist); worktree add returns 0
        if cmd[:2] == ["git", "show-ref"]:
            return subprocess.CompletedProcess(cmd, returncode=1, stdout="", stderr="")
        return subprocess.CompletedProcess(cmd, returncode=0, stdout="", stderr="")

    with mock.patch("fno.adapters._shared.subprocess.run", side_effect=fake_run):
        result = create_worktree(name="test-extract", project_id="fno")

    expected = _expected_path(tmp_path, "test-extract")
    assert result == {
        "worktree_path": str(expected),
        "branch": "feature/test-extract",
        "status": "created",
    }
    # Second call (after show-ref probe) is the worktree add with -b
    add_call = calls[1]["cmd"]
    assert add_call[:3] == ["git", "worktree", "add"]
    assert "-b" in add_call
    assert "feature/test-extract" in add_call
    assert calls[1]["kwargs"].get("check") is True


def test_create_worktree_uses_existing_branch_when_show_ref_finds_it(tmp_path, monkeypatch):
    """Gemini MEDIUM fix: existing branch -> check it out rather than create a new one."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HOME", str(tmp_path))

    calls: list = []

    def fake_run(cmd, **kwargs):
        calls.append({"cmd": cmd, "kwargs": kwargs})
        # show-ref returns 0 (branch exists); worktree add returns 0
        if cmd[:2] == ["git", "show-ref"]:
            return subprocess.CompletedProcess(cmd, returncode=0, stdout="", stderr="")
        return subprocess.CompletedProcess(cmd, returncode=0, stdout="", stderr="")

    with mock.patch("fno.adapters._shared.subprocess.run", side_effect=fake_run):
        result = create_worktree(name="reused", project_id="fno")

    assert result["status"] == "created"
    add_call = calls[1]["cmd"]
    # When the branch already exists, the command must NOT include -b (which
    # would fail with "branch already exists") and must NOT include the base.
    expected = _expected_path(tmp_path, "reused")
    assert "-b" not in add_call
    assert add_call == ["git", "worktree", "add", str(expected), "feature/reused"]


def test_create_worktree_returns_already_exists_when_path_present(tmp_path, monkeypatch):
    """AC1.3-EDGE: Existing path short-circuits and no subprocess fires."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HOME", str(tmp_path))
    existing = _expected_path(tmp_path, "already-there")
    existing.mkdir(parents=True)

    with mock.patch("fno.adapters._shared.subprocess.run") as mock_run:
        result = create_worktree(name="already-there", project_id="fno")

    assert result["status"] == "already-exists"
    assert result["worktree_path"] == str(existing)
    assert result["branch"] == "feature/already-there"
    mock_run.assert_not_called()


def test_create_worktree_returns_already_exists_for_legacy_claude_worktrees_path(
    tmp_path, monkeypatch,
):
    """AC7 back-compat: a worktree at the OLD ``.claude/worktrees/{name}/`` is detected."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HOME", str(tmp_path))
    legacy = tmp_path / ".claude" / "worktrees" / "legacy"
    legacy.mkdir(parents=True)

    with mock.patch("fno.adapters._shared.subprocess.run") as mock_run:
        result = create_worktree(name="legacy", project_id="fno")

    assert result["status"] == "already-exists"
    assert result["worktree_path"] == str(legacy)
    assert result["branch"] == "feature/legacy"
    mock_run.assert_not_called()


def test_create_worktree_raises_with_stderr_in_message_when_git_fails(tmp_path, monkeypatch):
    """git failure surfaces as RuntimeError carrying the captured stderr text."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HOME", str(tmp_path))

    def fake_run(cmd, **kwargs):
        # show-ref is the first call; let it pass with "branch missing".
        if cmd[:2] == ["git", "show-ref"]:
            return subprocess.CompletedProcess(cmd, returncode=1, stdout="", stderr="")
        raise subprocess.CalledProcessError(
            returncode=128, cmd=cmd, stderr="fatal: not a git repository"
        )

    with mock.patch("fno.adapters._shared.subprocess.run", side_effect=fake_run):
        with pytest.raises(RuntimeError) as exc_info:
            create_worktree(name="will-fail", project_id="fno")

    assert "not a git repository" in str(exc_info.value)
    assert "128" in str(exc_info.value)


def test_create_worktree_runtime_error_message_safe_when_stderr_empty(tmp_path, monkeypatch):
    """No captured stderr still produces a useful error message (not bare exit code)."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HOME", str(tmp_path))

    def fake_run(cmd, **kwargs):
        if cmd[:2] == ["git", "show-ref"]:
            return subprocess.CompletedProcess(cmd, returncode=1, stdout="", stderr="")
        raise subprocess.CalledProcessError(returncode=128, cmd=cmd, stderr="")

    with mock.patch("fno.adapters._shared.subprocess.run", side_effect=fake_run):
        with pytest.raises(RuntimeError) as exc_info:
            create_worktree(name="will-fail", project_id="fno")

    assert "no stderr" in str(exc_info.value)


def test_create_worktree_honors_custom_base_on_fresh_branch(tmp_path, monkeypatch):
    """Custom base branch is passed through verbatim to `git worktree add -b` when branch is fresh."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HOME", str(tmp_path))
    seen: list = []

    def fake_run(cmd, **kwargs):
        seen.append(cmd)
        # show-ref returns 1 -> branch does not exist -> -b path
        if cmd[:2] == ["git", "show-ref"]:
            return subprocess.CompletedProcess(cmd, returncode=1, stdout="", stderr="")
        return subprocess.CompletedProcess(cmd, returncode=0, stdout="", stderr="")

    with mock.patch("fno.adapters._shared.subprocess.run", side_effect=fake_run):
        create_worktree(name="branchy", base="release/x", project_id="fno")

    add_call = seen[1]
    assert add_call[-1] == "release/x"
    assert "-b" in add_call


def test_create_worktree_ignores_base_when_branch_already_exists(tmp_path, monkeypatch):
    """When the branch already exists, `base` is not used (checking out the existing branch)."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HOME", str(tmp_path))
    seen: list = []

    def fake_run(cmd, **kwargs):
        seen.append(cmd)
        # show-ref returns 0 -> branch exists -> check it out without -b
        return subprocess.CompletedProcess(cmd, returncode=0, stdout="", stderr="")

    with mock.patch("fno.adapters._shared.subprocess.run", side_effect=fake_run):
        create_worktree(name="branchy", base="release/x", project_id="fno")

    add_call = seen[1]
    assert "-b" not in add_call
    assert "release/x" not in add_call
    assert add_call[-1] == "feature/branchy"


def test_create_worktree_path_shape_uses_project_prefix(tmp_path, monkeypatch):
    """AC1: canonical path is ~/.fno/worktrees/{proj}-{name}/."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HOME", str(tmp_path))

    def fake_run(cmd, **kwargs):
        if cmd[:2] == ["git", "show-ref"]:
            return subprocess.CompletedProcess(cmd, returncode=1, stdout="", stderr="")
        return subprocess.CompletedProcess(cmd, returncode=0, stdout="", stderr="")

    with mock.patch("fno.adapters._shared.subprocess.run", side_effect=fake_run):
        result = create_worktree(name="my-feat", project_id="webapp")

    expected = tmp_path / ".fno" / "worktrees" / "webapp-my-feat"
    assert result["worktree_path"] == str(expected)


def test_create_worktree_resolves_project_id_from_settings(tmp_path, monkeypatch):
    """AC2: project_id resolves from .fno/settings.yaml when not passed."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HOME", str(tmp_path))
    # Make CWD a git repo with .fno/settings.yaml so resolve_project_id works
    subprocess.run(["git", "init", "-b", "main", str(tmp_path)], check=True, capture_output=True)
    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "settings.yaml").write_text(
        "project:\n  id: declared\n", encoding="utf-8"
    )

    def fake_run(cmd, **kwargs):
        if cmd[:2] == ["git", "show-ref"]:
            return subprocess.CompletedProcess(cmd, returncode=1, stdout="", stderr="")
        return subprocess.CompletedProcess(cmd, returncode=0, stdout="", stderr="")

    with mock.patch("fno.adapters._shared.subprocess.run", side_effect=fake_run):
        result = create_worktree(name="autopid")

    expected = tmp_path / ".fno" / "worktrees" / "declared-autopid"
    assert result["worktree_path"] == str(expected)
