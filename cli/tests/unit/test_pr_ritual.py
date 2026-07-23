"""Tests for `fno pr ritual` (x-bbde) and its four absorbed bugs.

The legs shell existing fno verbs; a fake runner stands in for fno/gh/git so
every leg is exercised without a real backlog/graph/gh. Pure helpers
(``_canonical_root``, ``_parking_lot_path``) are tested directly, including the
real-git worktree path for x-fb99.
"""
from __future__ import annotations

import subprocess
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.pr import _ritual
from fno.pr._proc import Result


# --- fake runner ---------------------------------------------------------

class FakeRunner:
    """Records argv; returns canned Results keyed on the subcommand."""

    def __init__(self, *, diff_files=0, additions=0, deletions=0,
                 deferred=None, reconcile_closed=None, claim_rc=0,
                 spawn_rc=0, agent_rows=None, branch="feat/x"):
        self.calls: list[list[str]] = []
        self._diff = (diff_files, additions, deletions)
        self._deferred = deferred or []
        self._closed = reconcile_closed or []
        self._claim_rc = claim_rc
        self._spawn_rc = spawn_rc
        self._rows = agent_rows or []
        self._branch = branch

    def __call__(self, argv, *, cwd=None, timeout=None):
        self.calls.append(list(argv))
        head = argv[0]
        if head == "gh":
            if "list" in argv:
                return Result(0, '[{"number":7,"mergedAt":"2026-07-23T00:00:00Z"}]', "")
            if "view" in argv:
                return Result(0, '{"headRefName":"%s","changedFiles":%d,"additions":%d,"deletions":%d}'
                              % (self._branch, self._diff[0], self._diff[1], self._diff[2]), "")
            return Result(0, "{}", "")
        # fno-py <sub> ...
        sub = argv[1] if len(argv) > 1 else ""
        if sub == "claim" and "acquire" in argv:
            return Result(self._claim_rc, "acquired" if self._claim_rc == 0 else "held", "")
        if sub == "backlog" and "reconcile" in argv:
            import json
            return Result(0, json.dumps({"closed": [{"node_id": n} for n in self._closed]}), "")
        if sub == "backlog" and "find" in argv:
            import json
            return Result(0, json.dumps(self._deferred), "")
        if sub == "agents" and "list" in argv:
            import json
            return Result(0, json.dumps({"agents": self._rows}), "")
        if sub == "agents" and "spawn" in argv:
            return Result(self._spawn_rc, "spawned", "")
        if sub == "agents" and ("stop" in argv or "rm" in argv):
            return Result(0, "", "")
        return Result(0, "", "")


def _bare(tmp_path, runner, *, autonomous=False, pr=7, parking_lot=None,
          node_ids=None, self_reap=False):
    """A Ritual built without __init__'s git/config resolution (hermetic)."""
    r = object.__new__(_ritual.Ritual)
    pm = SimpleNamespace(sync_command=None, self_reap=self_reap,
                         parking_lot_path=parking_lot)
    r.ctx = _ritual._Ctx(
        pr=pr, autonomous=autonomous, canon=tmp_path, settings=None, pm=pm,
        project="", lane_project="", parking_lot=(tmp_path / parking_lot) if parking_lot else None,
        holder="postmerge:pr-holder:test",
        node_ids=list(node_ids or []),
    )
    r.runner = runner
    r.cwd = tmp_path
    return r


def _argv_sub(calls, sub):
    """First fno call argv whose fno subcommand == sub."""
    for c in calls:
        if len(c) > 1 and c[0] != "gh" and c[1] == sub:
            return c
    return None


# --- x-fb99: canonical root from a worktree ------------------------------

def _git(cwd, *args):
    env = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "a@b.c",
           "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "a@b.c"}
    full = {"PATH": "/usr/bin:/bin", **env}
    subprocess.run(["git", *args], cwd=str(cwd), check=True, env=full,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def test_canonical_root_resolves_from_worktree(tmp_path):
    # x-fb99: from a worktree cwd, the canonical root is the MAIN worktree,
    # not the worktree itself. A bare --show-toplevel would return the worktree.
    main = tmp_path / "main"
    main.mkdir()
    _git(main, "init", "-q", "-b", "main")
    (main / "f").write_text("x")
    _git(main, "add", "-A")
    _git(main, "commit", "-qm", "init")
    wt = tmp_path / "wt"
    _git(main, "worktree", "add", "-q", str(wt), "-b", "feature")
    assert _ritual._canonical_root(wt).resolve() == main.resolve()


def test_parking_lot_path_joins_canonical_and_rejects_escape(tmp_path):
    pm = SimpleNamespace(parking_lot_path="internal/etl/backlog/parking-lot.md")
    assert _ritual._parking_lot_path(tmp_path, pm) == tmp_path / "internal/etl/backlog/parking-lot.md"
    assert _ritual._parking_lot_path(tmp_path, SimpleNamespace(parking_lot_path=None)) is None
    # x-fb99 backstop: a stale installed fno that accepted an absolute / '..'
    # path must not let the join escape the canonical root.
    assert _ritual._parking_lot_path(tmp_path, SimpleNamespace(parking_lot_path="/etc/passwd")) is None
    assert _ritual._parking_lot_path(tmp_path, SimpleNamespace(parking_lot_path="../sibling")) is None
    assert _ritual._parking_lot_path(tmp_path, SimpleNamespace(parking_lot_path="a/../../b")) is None


# --- x-c4ff: legs call the real verbs (no dangling references) -----------

def test_leg_skill_diff_calls_real_verb(tmp_path, capsys):
    # x-c4ff: the skill-diff leg calls the existing `skill-diff reconcile`,
    # not a nonexistent `fno skill-diff`. Dangling reference = this fails.
    runner = FakeRunner()
    r = _bare(tmp_path, runner)
    r.leg_skill_diff()
    sub = _argv_sub(runner.calls, "skill-diff")
    assert sub is not None and "reconcile" in sub
    rec = [l for l in capsys.readouterr().out.splitlines() if l.startswith("step=skill-diff")]
    assert rec and "status=ok" in rec[0]


def test_leg_sync_canonical_calls_real_verb(tmp_path, capsys):
    # x-c4ff: the canonical-sync leg calls the existing `pr sync-canonical`.
    runner = FakeRunner()
    pm = SimpleNamespace(sync_command="git pull", self_reap=False, parking_lot_path=None)
    r = _bare(tmp_path, runner)
    r.ctx.pm = pm
    r.leg_sync_canonical()
    sub = _argv_sub(runner.calls, "pr")
    assert sub is not None and "sync-canonical" in sub


def test_sync_canonical_skipped_when_unconfigured(tmp_path, capsys):
    runner = FakeRunner()
    r = _bare(tmp_path, runner)  # pm.sync_command = None
    r.leg_sync_canonical()
    rec = [l for l in capsys.readouterr().out.splitlines() if l.startswith("step=sync-canonical")]
    assert rec and "status=skipped" in rec[0] and "not configured" in rec[0]


# --- x-0d66: advance leg bounded + progress lines ------------------------

def test_advance_stream_is_bounded(tmp_path, capsys, monkeypatch):
    # x-0d66: a hung advance must be killed at the bound, not wedge the ritual.
    monkeypatch.setattr(_ritual, "fno_py_cmd", lambda: ["bash", "-lc"])
    r = _bare(tmp_path, FakeRunner(), node_ids=["fno-abc1"])
    r._stream("advance", ["sleep 30"], 1.0)
    out = capsys.readouterr().out
    assert "step=advance status=failed" in out
    assert "timeout" in out


def test_advance_stream_emits_progress(tmp_path, capsys, monkeypatch):
    # x-0d66: progress lines surface partial-dispatch state instead of silence.
    monkeypatch.setattr(_ritual, "fno_py_cmd", lambda: ["bash", "-lc"])
    r = _bare(tmp_path, FakeRunner())
    r._stream("advance", ["echo dispatched-x; echo dispatched-y"], 5.0)
    out = capsys.readouterr().out
    assert "  advance: dispatched-x" in out
    assert "step=advance status=ok" in out


# --- AC3: a failing leg is loud -----------------------------------------

def test_failing_leg_records_failure_and_exit(tmp_path, capsys):
    # AC3-ERR: a non-zero exit surfaces as status=failed and run() exits 1.
    class _FailSync(FakeRunner):
        def __call__(self, argv, *, cwd=None, timeout=None):
            super().__call__(argv, cwd=cwd, timeout=timeout)
            if len(argv) > 1 and argv[1] == "pr" and "sync-canonical" in argv:
                return Result(3, "sync failed: boom", "")
            return super().__call__(argv, cwd=cwd, timeout=timeout) if False else self._last

    runner = FakeRunner()
    r = _bare(tmp_path, runner)

    # Simulate the sync leg failing by calling _leg directly with a runner
    # variant that returns non-zero for sync-canonical.
    class _R:
        def __init__(self, inner):
            self._inner = inner

        def __call__(self, argv, *, cwd=None, timeout=None):
            self._inner.calls.append(list(argv))
            if len(argv) > 1 and argv[1] == "pr" and "sync-canonical" in argv:
                return Result(3, "sync failed: boom", "")
            return FakeRunner.__call__(self._inner, argv, cwd=cwd, timeout=timeout)

    r.runner = _R(runner)
    pm = SimpleNamespace(sync_command="git pull", self_reap=False, parking_lot_path=None)
    r.ctx.pm = pm
    r.leg_sync_canonical()
    out = capsys.readouterr().out
    assert "step=sync-canonical status=failed" in out
    assert "exit=3" in out


def test_run_exits_nonzero_when_a_leg_fails(tmp_path, capsys, monkeypatch):
    # AC3 end-to-end: reconcile failure -> exit 1, every later leg still runs.
    class _FailReconcile(FakeRunner):
        def __call__(self, argv, *, cwd=None, timeout=None):
            self.calls.append(list(argv))
            sub = argv[1] if len(argv) > 1 and argv[0] != "gh" else ""
            if sub == "backlog" and "reconcile" in argv and "session" not in argv:
                return Result(1, "corrupt graph", "")
            return FakeRunner.__call__(self, argv, cwd=cwd, timeout=timeout)

    monkeypatch.setattr(_ritual, "fno_py_cmd", lambda: ["true"])  # no real fno-py needed
    runner = _FailReconcile()
    r = _bare(tmp_path, runner)
    # stub the claim runner path: acquire_mutex uses self.runner too
    r.runner = runner
    rc = r.run()
    out = capsys.readouterr().out
    assert rc == 1
    assert "step=reconcile status=failed" in out
    # later legs still ran and printed receipts
    assert "step=judgment" in out
    assert "step=reap-rows" in out


# --- AC2: empty inputs spawn nothing; non-empty spawns headless ----------

def test_judgment_autonomous_empty_skips(tmp_path, capsys):
    runner = FakeRunner(diff_files=0)  # below bar, no deferrals
    r = _bare(tmp_path, runner, autonomous=True)
    r.leg_judgment()
    out = capsys.readouterr().out
    assert "step=judgment status=skipped" in out
    assert "reason=no-inputs" in out or "diff-below-bar" in out
    # no spawn
    assert not any(c[1] == "agents" and "spawn" in c for c in runner.calls if len(c) > 1)


def test_judgment_autonomous_nonempty_spawns_headless(tmp_path, capsys):
    runner = FakeRunner(diff_files=14, additions=300, deletions=20)
    r = _bare(tmp_path, runner, autonomous=True, parking_lot="internal/x/parking-lot.md")
    r.leg_judgment()
    out = capsys.readouterr().out
    assert "step=judgment status=ok" in out
    assert "spawned headless" in out
    # exactly one headless spawn, substrate=headless (never bg)
    spawns = [c for c in runner.calls if len(c) > 1 and c[1] == "agents" and "spawn" in c]
    assert len(spawns) == 1
    assert "--substrate" in spawns[0] and "headless" in spawns[0]
    assert "bg" not in spawns[0]


def test_judgment_attended_defers_to_skill(tmp_path, capsys):
    # An attended run never spawns; the skill body does judgment inline.
    runner = FakeRunner(diff_files=50, additions=900, deletions=100)
    r = _bare(tmp_path, runner, autonomous=False, parking_lot="internal/x/parking-lot.md")
    r.leg_judgment()
    out = capsys.readouterr().out
    assert "deferred-to-skill" in out
    assert not any(len(c) > 1 and c[1] == "agents" and "spawn" in c for c in runner.calls)


# --- AC1/AC5: archive leg (found / inside-worktree / missing script) ----

def test_archive_defers_when_run_inside_worktree(tmp_path, capsys, monkeypatch):
    # AC5-EDGE: never self-remove; defer to the standing sweep with a named receipt.
    runner = FakeRunner(branch="feature/x")
    r = _bare(tmp_path, runner)
    monkeypatch.setattr(r, "_find_worktree", lambda branch: str(r.cwd))
    r.leg_archive()
    out = capsys.readouterr().out
    assert "step=archive status=skipped" in out
    assert "cleanup --merged --apply" in out


def test_archive_runs_script_when_worktree_found(tmp_path, capsys, monkeypatch):
    # AC1-HP: a found worktree for the merged branch is archived.
    runner = FakeRunner(branch="feature/x")
    r = _bare(tmp_path, runner)
    wt = tmp_path / "wt"; wt.mkdir()
    (tmp_path / "scripts" / "setup").mkdir(parents=True)
    (tmp_path / "scripts" / "setup" / "archive-worktree.sh").write_text("#!/bin/sh\nexit 0\n")
    monkeypatch.setattr(r, "_find_worktree", lambda branch: str(wt))
    r.leg_archive()
    out = capsys.readouterr().out
    assert "step=archive status=ok" in out
    assert "archived" in out
    # the archive script was invoked with --yes, never --force
    archive_calls = [c for c in runner.calls if c[:1] == ["bash"] and "archive-worktree.sh" in " ".join(c)]
    assert archive_calls and "--yes" in archive_calls[0]
    assert "--force" not in archive_calls[0]


def test_archive_skips_when_no_worktree(tmp_path, capsys, monkeypatch):
    runner = FakeRunner(branch="feature/x")
    r = _bare(tmp_path, runner)
    monkeypatch.setattr(r, "_find_worktree", lambda branch: None)
    r.leg_archive()
    out = capsys.readouterr().out
    assert "step=archive status=skipped" in out


# --- AC4: idempotency / mutex -------------------------------------------

def test_mutex_held_stops_clean(tmp_path, capsys):
    # AC4-FR / concurrency: if another runner owns the mutex, stop at status=skipped.
    runner = FakeRunner(claim_rc=1)
    r = _bare(tmp_path, runner)
    won = r.acquire_mutex()
    assert won is False
    out = capsys.readouterr().out
    assert "step=mutex status=skipped" in out
    assert "already-held" in out
    assert not r.ctx.owns_claim


def test_mutex_released_on_success(tmp_path):
    runner = FakeRunner(claim_rc=0)
    r = _bare(tmp_path, runner)
    r.acquire_mutex()
    assert r.ctx.owns_claim
    r.release_mutex()
    assert not r.ctx.owns_claim
    # a release call was made
    assert any(len(c) > 1 and c[1] == "claim" and "release" in c for c in runner.calls)
