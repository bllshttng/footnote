"""Tests for `fno do pr sync-canonical` (x-47be, task 1.2 / US2 + US5).

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
    def __init__(self, rc: int = 0, stdout: str = "", stderr: str = ""):
        self.rc = rc
        self.stdout = stdout
        self.stderr = stderr
        self.calls: list[tuple[str, str]] = []

    def __call__(self, command: str, cwd: str) -> Result:
        self.calls.append((command, cwd))
        return Result(returncode=self.rc, stdout=self.stdout, stderr=self.stderr)


def _run(canonical, **kw):
    kw.setdefault("settings", _settings("git pull && fno doctor update"))
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
        settings=_settings("git pull && fno doctor update", paths=["cli/**"]),
        gh_json=_gh_row(files=[{"path": "cli/src/fno/z.py"}]),
        shell_runner=shell,
    )
    assert rc == 0
    assert "synced" in capsys.readouterr().out
    assert len(shell.calls) == 1
    assert shell.calls[0][0] == "git pull && fno doctor update"
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


def test_failure_surfaces_command_and_captured_stderr(tmp_path, capsys):
    # The defect this fixes: a failing sync_command reported only an exit code
    # and discarded the command text, stdout, and stderr. A two-day outage was a
    # one-word typo (`git checkpout`) the tool had captured and threw away, so
    # the receipt must now carry the command that ran and its captured stderr.
    shell = _Shell(rc=1, stderr="git: 'checkpout' is not a git command")
    rc = _run(tmp_path, shell_runner=shell)
    assert rc == 1
    err = capsys.readouterr().err
    assert "exit 1" in err
    assert "git pull && fno doctor update" in err  # the command that ran
    assert "checkpout" in err               # the captured stderr
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


# --- Wave 4 (x-7930) AC-e2e: canonical-sync fires on merge, targets canonical --


def test_e2e_merge_syncs_canonical_not_worktree_and_dedups(tmp_path, capsys):
    """AC-e2e: a merged PR (manual or daemon-detected, no live session in the
    picture) runs sync_command IN the canonical, writes post-merge-synced/<sha>
    under the canonical, never touches the worktree, and a re-run is a no-op."""
    canonical = tmp_path / "canonical"
    canonical.mkdir()
    worktree = tmp_path / "worktree"  # must never be the sync cwd
    worktree.mkdir()

    shell = _Shell()
    rc = _run(canonical, shell_runner=shell)
    assert rc == 0

    # sync_command ran exactly once, in the canonical, never the worktree.
    assert len(shell.calls) == 1
    _cmd, cwd = shell.calls[0]
    assert Path(cwd) == canonical
    assert Path(cwd) != worktree

    # Marker landed under the canonical (keyed by merge SHA), not the worktree.
    marker = canonical / ".fno" / "post-merge-synced" / ("a" * 40)
    assert marker.exists()
    assert not (worktree / ".fno" / "post-merge-synced" / ("a" * 40)).exists()

    # Re-run (a second detection / re-tick) re-runs nothing.
    rc2 = _run(canonical, shell_runner=shell)
    assert rc2 == 0
    assert len(shell.calls) == 1  # unchanged: dedup by marker
    assert "already synced" in capsys.readouterr().out


def test_claim_key_is_canonical_wide_not_per_sha(tmp_path, monkeypatch):
    """Two different merges must contend for ONE lock.

    The claim's job is that two `fno agents restart`s never overlap in a checkout. A
    per-SHA key does not deliver that: a catch-up for one merge and a merge-time
    sync for another would take different locks and pull, update, and restart
    concurrently. Exactly-once-per-SHA is the marker's job, not the claim's.
    """
    from fno import claims

    keys: list[str] = []
    monkeypatch.setattr(
        claims, "acquire_claim", lambda key, holder, **_kw: keys.append(key)
    )
    monkeypatch.setattr(claims, "release_claim", lambda *a, **k: None)

    _run(tmp_path, shell_runner=_Shell())
    _run(
        tmp_path,
        gh_json=_gh_row(mergeCommit={"oid": "b" * 40}),
        shell_runner=_Shell(),
    )

    assert len(keys) == 2
    assert keys[0] == keys[1], "different SHAs must contend for the same lock"
    assert "a" * 40 not in keys[0] and "b" * 40 not in keys[1]


# --- x-adf9: _default_shell_runner detaches daemons + is bounded ---------

def test_default_shell_runner_captures_without_leaking_to_parent(tmp_path, capsys):
    # x-adf9: a `fno agents restart` in sync_command detaches a daemon that inherits
    # the runner's stdout and never closes it, wedging subprocess.run on wait.
    # Output is captured to temp FILES (not PIPE), so the child's output never
    # reaches the parent's stdout (a detached child cannot hold a pipe open and
    # the parent's wait() never blocks on EOF) - yet it is still recoverable on
    # the returned Result for the failure receipt.
    from fno.pr._sync_canonical import _default_shell_runner

    res = _default_shell_runner("echo LEAKED_MARKER_XADF9", str(tmp_path))
    assert res.returncode == 0
    assert "LEAKED_MARKER_XADF9" not in capsys.readouterr().out  # parent not polluted
    assert "LEAKED_MARKER_XADF9" in res.stdout  # ...but captured for the receipt


def test_default_shell_runner_is_bounded(tmp_path, capsys, monkeypatch):
    # x-adf9 backstop: a stuck sync_command returns 124 at the bound instead of
    # wedging the ritual forever.
    import fno.pr._sync_canonical as mod

    monkeypatch.setattr(mod, "_SYNC_COMMAND_TIMEOUT_S", 1.0)
    res = mod._default_shell_runner("sleep 30", str(tmp_path))
    assert res.returncode == 124
    assert "timed out" in capsys.readouterr().err.lower()


def test_default_shell_runner_captures_failing_output(tmp_path):
    # The real path (file capture, not PIPE): a failing command's stdout and
    # stderr land on the returned Result so the failure receipt can carry them.
    from fno.pr._sync_canonical import _default_shell_runner

    res = _default_shell_runner("echo on-stdout; echo on-stderr >&2; exit 3", str(tmp_path))
    assert res.returncode == 3
    assert "on-stdout" in res.stdout
    assert "on-stderr" in res.stderr


def test_default_shell_runner_bounds_captured_output(tmp_path):
    # A runaway sync_command must not load its full output into the watcher's
    # memory: only the tail is retained on the Result (the head is discarded),
    # even though the command emitted well past the capture budget.
    from fno.pr._sync_canonical import _default_shell_runner

    cmd = "for i in $(seq 1 2000); do echo line$i-END; done"
    res = _default_shell_runner(cmd, str(tmp_path))
    assert res.returncode == 0
    assert "line2000-END" in res.stdout  # tail retained
    assert "line1-END" not in res.stdout  # head discarded
    from fno.pr._sync_canonical import _CAPTURE_TAIL_CHARS
    assert len(res.stdout) <= _CAPTURE_TAIL_CHARS * 4  # bounded, not the full ~34KB
