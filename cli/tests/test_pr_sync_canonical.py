"""Tests for `fno pr sync-canonical` (x-47be, task 1.2 / US2 + US5).

Every branch of run_sync_canonical is exercised via dependency injection: no
real gh, git, shell, or canonical filesystem. Claims are redirected to a tmp
root so the single-flight lock never touches the real .fno/claims.
"""
from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace
from typing import Optional

import pytest

from fno.pr._proc import Result
from fno.pr._sync_canonical import run_sync_canonical


@pytest.fixture(autouse=True)
def _isolate_claims(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))


def _settings(command: Optional[str] = None, paths=None):
    return SimpleNamespace(
        post_merge=SimpleNamespace(sync_command=command, sync_paths=paths or [])
    )


def _git_origin(url: str = "git@github.com:owner/repo.git"):
    def runner(cmd, cwd=None, **kw):
        return Result(returncode=0, stdout=url + "\n", stderr="")
    return runner


def _gh_row(**overrides):
    row = {
        "state": "MERGED",
        "mergeCommit": {"oid": "a" * 40},
        "files": [{"path": "cli/src/fno/x.py"}],
        "url": "https://github.com/owner/repo/pull/7",
    }
    row.update(overrides)
    return lambda args, cwd: row


class _Shell:
    def __init__(self, rc: int = 0):
        self.rc = rc
        self.calls: list[tuple[str, str]] = []

    def __call__(self, command: str, cwd: str) -> int:
        self.calls.append((command, cwd))
        return self.rc


def _run(canonical, **kw):
    kw.setdefault("settings", _settings("git pull && fno update"))
    kw.setdefault("canonical_root", canonical)
    kw.setdefault("runner", _git_origin())
    kw.setdefault("gh_json", _gh_row())
    return run_sync_canonical(7, **kw)


def test_unconfigured_is_noop(tmp_path, capsys):
    shell = _Shell()
    rc = _run(tmp_path, settings=_settings(None), shell_runner=shell)
    assert rc == 0
    assert "not configured" in capsys.readouterr().out
    assert shell.calls == []


def test_not_merged_skips(tmp_path, capsys):
    shell = _Shell()
    rc = _run(tmp_path, gh_json=_gh_row(state="OPEN"), shell_runner=shell)
    assert rc == 0
    assert "not merged" in capsys.readouterr().out
    assert shell.calls == []


def test_already_synced_skips(tmp_path, capsys):
    marker = tmp_path / ".fno" / "post-merge-synced" / ("a" * 40)
    marker.parent.mkdir(parents=True)
    marker.touch()
    shell = _Shell()
    rc = _run(tmp_path, shell_runner=shell)
    assert rc == 0
    assert "already synced" in capsys.readouterr().out
    assert shell.calls == []


def test_lock_held_skips(tmp_path, capsys, monkeypatch):
    from fno import claims

    def _raise(*a, **k):
        raise claims.ClaimHeldByOther("other", 1, "host", "post-merge-sync:x")

    monkeypatch.setattr(claims, "acquire_claim", _raise)
    shell = _Shell()
    rc = _run(tmp_path, shell_runner=shell)
    assert rc == 0
    assert "in progress elsewhere" in capsys.readouterr().out
    assert shell.calls == []


def test_path_gate_skip_writes_marker(tmp_path, capsys):
    shell = _Shell()
    rc = _run(
        tmp_path,
        settings=_settings("git pull", paths=["cli/**", "crates/**"]),
        gh_json=_gh_row(files=[{"path": "skills/pr/x.md"}, {"path": "docs/y.md"}]),
        shell_runner=shell,
    )
    assert rc == 0
    out = capsys.readouterr().out
    assert "no buildable change" in out
    assert shell.calls == []  # sync_command NOT run
    assert (tmp_path / ".fno" / "post-merge-synced" / ("a" * 40)).exists()


def test_path_gate_match_runs(tmp_path, capsys):
    shell = _Shell(rc=0)
    rc = _run(
        tmp_path,
        settings=_settings("git pull && fno update", paths=["cli/**"]),
        gh_json=_gh_row(files=[{"path": "cli/src/fno/z.py"}]),
        shell_runner=shell,
    )
    assert rc == 0
    assert "synced" in capsys.readouterr().out
    assert len(shell.calls) == 1
    assert shell.calls[0][0] == "git pull && fno update"
    assert (tmp_path / ".fno" / "post-merge-synced" / ("a" * 40)).exists()


def test_empty_paths_always_runs(tmp_path, capsys):
    shell = _Shell(rc=0)
    rc = _run(tmp_path, settings=_settings("make install", paths=[]), shell_runner=shell)
    assert rc == 0
    assert len(shell.calls) == 1


def test_failure_leaves_no_marker(tmp_path, capsys):
    shell = _Shell(rc=3)
    rc = _run(tmp_path, shell_runner=shell)
    assert rc == 3
    assert "failed" in capsys.readouterr().err
    assert not (tmp_path / ".fno" / "post-merge-synced" / ("a" * 40)).exists()


def test_wrong_repo_guard_skips(tmp_path, capsys):
    shell = _Shell()
    # gh returns a PR url in a DIFFERENT repo than the resolved canonical origin.
    rc = _run(
        tmp_path,
        gh_json=_gh_row(url="https://github.com/someoneelse/fork/pull/7"),
        shell_runner=shell,
    )
    assert rc == 0
    assert "wrong repo" in capsys.readouterr().err
    assert shell.calls == []


def test_repo_slug_compare_is_case_insensitive(tmp_path, capsys):
    """GitHub slugs are case-insensitive: a casing mismatch must NOT refuse."""
    shell = _Shell(rc=0)
    rc = _run(
        tmp_path,
        runner=_git_origin("git@github.com:Owner/Repo.git"),
        gh_json=_gh_row(url="https://github.com/owner/repo/pull/7"),
        shell_runner=shell,
    )
    assert rc == 0
    assert "wrong repo" not in capsys.readouterr().err
    assert len(shell.calls) == 1  # proceeded to sync, not refused


def test_no_origin_skips(tmp_path, capsys):
    def no_origin(cmd, cwd=None, **kw):
        return Result(returncode=1, stdout="", stderr="no origin")

    shell = _Shell()
    rc = _run(tmp_path, runner=no_origin, shell_runner=shell)
    assert rc == 0
    assert "no resolvable origin" in capsys.readouterr().err
    assert shell.calls == []


def test_no_merge_commit_skips(tmp_path, capsys):
    shell = _Shell()
    rc = _run(tmp_path, gh_json=_gh_row(mergeCommit=None), shell_runner=shell)
    assert rc == 0
    assert "no merge commit" in capsys.readouterr().out
    assert shell.calls == []
