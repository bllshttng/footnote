"""Tests for worktree_paths: worktree base + project_id."""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from fno.worktree_paths import (
    _validate_component,
    resolve_project_id,
)


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
    yield


# ----------------------------------------------------------------------
# _validate_component
# ----------------------------------------------------------------------


def test_validate_component_rejects_path_traversal():
    with pytest.raises(ValueError, match=r"\.\."):
        _validate_component("..", kind="project_id")
    with pytest.raises(ValueError, match=r"\.\."):
        _validate_component("..foo", kind="project_id")
    with pytest.raises(ValueError, match=r"\.\."):
        _validate_component("foo..bar", kind="project_id")


def test_validate_component_rejects_path_separator():
    with pytest.raises(ValueError):
        _validate_component("foo/bar", kind="project_id")
    with pytest.raises(ValueError):
        _validate_component("/abs", kind="name")


def test_validate_component_rejects_empty():
    with pytest.raises(ValueError):
        _validate_component("", kind="project_id")


# ----------------------------------------------------------------------
# resolve_project_id
# ----------------------------------------------------------------------


def _make_repo(path: Path) -> Path:
    subprocess.run(["git", "init", "-b", "main", str(path)], check=True, capture_output=True)
    subprocess.run(["git", "config", "user.email", "t@t"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.name", "T"], cwd=path, check=True, capture_output=True)
    (path / "README.md").write_text("x\n")
    subprocess.run(["git", "add", "."], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "commit", "-m", "init"], cwd=path, check=True, capture_output=True)
    return path


def test_resolve_project_id_reads_config_project_id(tmp_path):
    """config.project.id is the canonical location (the deprecation target)."""
    repo = _make_repo(tmp_path / "repo")
    (repo / ".fno").mkdir()
    (repo / ".fno" / "settings.yaml").write_text(
        "config:\n  project:\n    id: canonical-id\n",
        encoding="utf-8",
    )
    assert resolve_project_id(repo) == "canonical-id"


def test_resolve_project_id_config_beats_top_level(tmp_path):
    """Both present: config.project.id wins over the deprecated top-level alias."""
    repo = _make_repo(tmp_path / "repo")
    (repo / ".fno").mkdir()
    (repo / ".fno" / "settings.yaml").write_text(
        "project:\n  id: legacy-id\nconfig:\n  project:\n    id: canonical-id\n",
        encoding="utf-8",
    )
    assert resolve_project_id(repo) == "canonical-id"


def test_resolve_project_id_reads_config_string_shorthand(tmp_path):
    """Bare-string shorthand under config: `config:\\n  project: <id>` resolves."""
    repo = _make_repo(tmp_path / "repo")
    (repo / ".fno").mkdir()
    (repo / ".fno" / "settings.yaml").write_text(
        "config:\n  project: config-shorthand-id\n", encoding="utf-8"
    )
    assert resolve_project_id(repo) == "config-shorthand-id"


def test_resolve_project_id_falls_back_to_git_remote_basename(tmp_path):
    repo = _make_repo(tmp_path / "myrepo")
    subprocess.run(
        ["git", "remote", "add", "origin", "https://github.com/foo/bar-baz.git"],
        cwd=repo, check=True, capture_output=True,
    )
    assert resolve_project_id(repo) == "bar-baz"


def test_resolve_project_id_handles_scp_style_url_without_path(tmp_path):
    """Gemini MEDIUM PR #234: SCP-style URL ``git@host:repo.git`` (no `/`) must parse.

    Pre-fix the rsplit("/", 1) returned the whole string as basename and
    validation rejected it. The regex split on both `/` and `:` recovers
    the right basename.
    """
    repo = _make_repo(tmp_path / "myrepo")
    subprocess.run(
        ["git", "remote", "add", "origin", "git@example.com:reponame.git"],
        cwd=repo, check=True, capture_output=True,
    )
    assert resolve_project_id(repo) == "reponame"


def test_resolve_project_id_falls_back_to_repo_basename(tmp_path):
    repo = _make_repo(tmp_path / "myproject")
    # No settings.yaml, no remote
    assert resolve_project_id(repo) == "myproject"


def test_resolve_project_id_settings_beats_git_remote(tmp_path):
    repo = _make_repo(tmp_path / "wrong-name")
    subprocess.run(
        ["git", "remote", "add", "origin", "https://github.com/foo/also-wrong.git"],
        cwd=repo, check=True, capture_output=True,
    )
    (repo / ".fno").mkdir()
    (repo / ".fno" / "settings.yaml").write_text("project:\n  id: chosen\n")
    assert resolve_project_id(repo) == "chosen"


def test_resolve_project_id_rejects_unsafe_settings_value(tmp_path):
    repo = _make_repo(tmp_path / "repo")
    (repo / ".fno").mkdir()
    (repo / ".fno" / "settings.yaml").write_text("project:\n  id: '../escape'\n")
    with pytest.raises(ValueError):
        resolve_project_id(repo)
