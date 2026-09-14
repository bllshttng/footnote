"""FNO_WORKTREE_POLICY: one env override above every config layer.

Lives under cli/tests to keep the source dir inside its line budget. The
autouse fixture mirrors test_worktree_policy_resolver's sole-config pin so
the ambient machine config cannot decide a case.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

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
    """FNO_CONFIG is the sole config candidate; tests rewrite it per case."""
    iso = tmp_path_factory.mktemp("iso") / "config.toml"
    iso.write_text("")
    monkeypatch.setenv("FNO_CONFIG", str(iso))
    monkeypatch.delenv("FNO_WORKTREE_POLICY", raising=False)
    yield


def test_env_never_outranks_config(tmp_path, monkeypatch):
    """AC2-HP: the env value wins over a config that names something else."""
    repo = _make_repo(tmp_path / "envwin")
    Path(os.environ["FNO_CONFIG"]).write_text(
        '[worktree]\npolicy = "external"\n', encoding="utf-8"
    )
    monkeypatch.setenv("FNO_WORKTREE_POLICY", "never")
    pol = resolve_worktree_policy(repo, "claude")
    assert pol.policy == "never"
    assert pol.source == "env"


def test_env_out_of_enum_refuses_fail_closed(tmp_path, monkeypatch):
    """AC2-ERR: a bad env value raises like a bad config value."""
    repo = _make_repo(tmp_path / "badenv")
    monkeypatch.setenv("FNO_WORKTREE_POLICY", "nonsense")
    with pytest.raises(WorktreePolicyError):
        resolve_worktree_policy(repo, "claude")


def test_env_empty_is_ignored(tmp_path, monkeypatch):
    """AC2-EDGE: unset or empty resolves byte-identical to today."""
    repo = _make_repo(tmp_path / "emptyenv")
    monkeypatch.setenv("FNO_WORKTREE_POLICY", "")
    pol = resolve_worktree_policy(repo, "claude")
    assert pol.policy == "harness-native"
    assert pol.source == "default"
    assert pol.degraded is False


def test_policy_receipt_names_source_and_degradation(tmp_path):
    """AC5-HP: `worktree policy` prints source= and the degraded clause."""
    from fno.worktree_cli.cli import app

    repo = _make_repo(tmp_path / "receipt")
    result = CliRunner().invoke(app, ["policy", "--repo", str(repo)])
    assert result.exit_code == 0
    lines = result.stdout.strip().splitlines()
    assert lines[0] == "external"
    assert any(line.startswith("base=") for line in lines)
    assert "source=default" in lines
    assert "requested=harness-native degraded=true" in lines


def test_policy_receipt_never_stays_one_word_plus_source(tmp_path):
    """A `never` receipt carries source= on a later line; line 1 stays bare."""
    from fno.worktree_cli.cli import app

    repo = _make_repo(tmp_path / "neverrepo")
    Path(os.environ["FNO_CONFIG"]).write_text(
        '[worktree]\npolicy = "never"\n', encoding="utf-8"
    )
    result = CliRunner().invoke(app, ["policy", "--repo", str(repo)])
    assert result.exit_code == 0
    lines = result.stdout.strip().splitlines()
    assert lines[0] == "never"
    assert "source=global" in lines
    assert not any(line.startswith("base=") for line in lines)
