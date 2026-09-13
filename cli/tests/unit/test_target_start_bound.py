"""`fno do target start` bounds every stage instead of waiting forever.

On 2026-09-09 a cold start stalled at three different stages under machine
overload, with no bound anywhere: `subprocess.run` carried no timeout for
`worktree ensure` or `target init`, and the pre-ensure graph read was bounded
only per-`recv`, not per attempt. This file covers the fix: `run_bounded`
(AC1), the bounded setup-worktree.sh hook (AC2), the per-stage 124 exits
(AC3), and the end-to-end `faulthandler` watchdog (AC4).
"""
from __future__ import annotations

import subprocess
import sys
import time
from pathlib import Path

import psutil
import pytest
from typer.testing import CliRunner

from fno import target_cli
from fno._subprocess_util import run_bounded
from fno.target_cli import target_app
from fno.worktree import _run_setup_worktree_hook

from .test_target_start_receipt import _ordinary_start_stubs, _worktree_fixture

runner = CliRunner()


# --------------------------------------------------------------------------
# AC1: run_bounded kills the whole process group, and is a drop-in for the
# ordinary (non-timeout) case.
# --------------------------------------------------------------------------


def test_run_bounded_kills_the_whole_process_group_on_timeout(tmp_path: Path):
    pidfile = tmp_path / "grandchild.pid"
    with pytest.raises(subprocess.TimeoutExpired):
        run_bounded(
            ["bash", "-c", f'sleep 30 & echo $! > "{pidfile}"; wait'],
            timeout=1,
        )

    # Positive control: the grandchild really started, so a dead pid below
    # is proof of the kill, not proof the script never ran.
    assert pidfile.exists(), "grandchild never wrote its pid; nothing to prove the kill"
    pid = int(pidfile.read_text().strip())
    # A killed child can sit as a zombie until its new (post-killpg) parent
    # reaps it, so poll briefly rather than require instant disappearance.
    deadline = time.monotonic() + 5
    while True:
        try:
            status = psutil.Process(pid).status()
        except psutil.NoSuchProcess:
            status = None
        if status in (None, psutil.STATUS_ZOMBIE):
            break
        if time.monotonic() > deadline:
            pytest.fail(f"grandchild pid {pid} still alive (status={status!r}) after kill")
        time.sleep(0.05)


def test_run_bounded_returns_a_completed_process_like_subprocess_run():
    result = run_bounded(
        ["bash", "-c", "echo hi; exit 3"], timeout=5, capture_output=True, text=True
    )
    assert result.returncode == 3
    assert "hi" in result.stdout


# --------------------------------------------------------------------------
# AC2: the setup-worktree.sh hook is bounded at (default 120s, override-able).
# --------------------------------------------------------------------------


def _write_hook(tmp_path: Path, body: str) -> None:
    script = tmp_path / "scripts" / "setup" / "setup-worktree.sh"
    script.parent.mkdir(parents=True)
    script.write_text(f"#!/bin/bash\n{body}\n")
    script.chmod(0o755)


def test_setup_hook_returns_124_and_names_the_timeout_on_a_slow_script(tmp_path: Path):
    _write_hook(tmp_path, "sleep 30")
    started = time.monotonic()
    rc, tail = _run_setup_worktree_hook(tmp_path, tmp_path, timeout=1)
    assert time.monotonic() - started < 5
    assert rc == 124
    assert "exceeded 1s" in tail


def test_setup_hook_returns_unchanged_zero_on_a_quick_script(tmp_path: Path):
    _write_hook(tmp_path, "exit 0")
    rc, _tail = _run_setup_worktree_hook(tmp_path, tmp_path)
    assert rc == 0


# --------------------------------------------------------------------------
# AC3: a stage that exceeds its bound exits 124 and names the stage.
# --------------------------------------------------------------------------


def test_start_exits_124_naming_the_ensure_stage_and_never_runs_init(
    monkeypatch, tmp_path: Path
):
    canonical, wt = _worktree_fixture(tmp_path)
    _ordinary_start_stubs(monkeypatch, canonical, wt)
    calls: list[list[str]] = []

    def fake_run_bounded(cmd, **kwargs):
        calls.append(list(cmd))
        if "ensure" in cmd:
            raise subprocess.TimeoutExpired(cmd, 1)
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli, "run_bounded", fake_run_bounded)

    result = runner.invoke(target_app, ["start", "x-0b3f"])

    assert result.exit_code == 124
    assert "step: ensure" in result.output
    assert f"{target_cli._START_DEADLINE_S}s" in result.output
    assert not any("init" in c for c in calls), "init ran after ensure timed out"


def test_start_exits_124_naming_the_init_stage_and_claim_recovery(
    monkeypatch, tmp_path: Path
):
    canonical, wt = _worktree_fixture(tmp_path)
    _ordinary_start_stubs(monkeypatch, canonical, wt)

    def fake_run_bounded(cmd, **kwargs):
        if "ensure" in cmd:
            return subprocess.CompletedProcess(
                cmd, 0, stdout=str(wt), stderr=f"worktree ensure: worktree at {wt}"
            )
        if "init" in cmd:
            raise subprocess.TimeoutExpired(cmd, 1)
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli, "run_bounded", fake_run_bounded)

    result = runner.invoke(target_app, ["start", "x-0b3f"])

    assert result.exit_code == 124
    assert "step: init" in result.output
    assert "claim state is unknown" in result.output.lower()
    assert "fno agents claim status node:x-0b3f" in result.output
    assert "fno do target start x-0b3f" in result.output


def test_start_codex_native_bounds_its_own_init_call_too(monkeypatch):
    """`_start_codex_native` runs a second, separate `target init` subprocess
    for a Codex Desktop-owned worktree -- a sibling of the ensure/init calls
    bounded above, missed by the plan's own answerer count. Given a deadline
    (always supplied by its one real caller, `_start_body`), it must exit 124
    the same way instead of leaving an unbounded, unkillable orphan."""

    def fake_run_bounded(cmd, **kwargs):
        raise subprocess.TimeoutExpired(cmd, 1)

    monkeypatch.setattr(target_cli, "run_bounded", fake_run_bounded)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_resolve_node_model", lambda *a, **k: (None, "none"))
    monkeypatch.setattr(target_cli, "_prepare_codex_native_branch", lambda *a: "main")

    with pytest.raises(target_cli.typer.Exit) as excinfo:
        target_cli._start_codex_native(
            canonical=Path("/repo"),
            cwd=Path("/repo/wt"),
            node="x-1",
            plan_path=None,
            size=None,
            model=None,
            harness=None,
            beastmode=False,
            no_merge=False,
            deadline=time.monotonic() + 100,
        )
    assert excinfo.value.exit_code == 124


# --------------------------------------------------------------------------
# AC4: the end-to-end faulthandler watchdog.
# --------------------------------------------------------------------------

_AC4_HP_SCRIPT = """
import sys, time
from typer.testing import CliRunner
from fno import target_cli
from fno.target_cli import target_app

target_cli._START_DEADLINE_S = 2

def _git_out_stub(cwd, *args):
    if args == ("rev-parse", "--show-toplevel"):
        return "/tmp/ac4-hp-does-not-need-a-real-repo"
    return None

target_cli._is_linked_worktree = lambda cwd: False
target_cli._git_out = _git_out_stub
target_cli._resolve_fno_cmd = lambda: ["fno"]

def _graph_entries_or_none(*a, **kw):
    time.sleep(60)

target_cli._graph_entries_or_none = _graph_entries_or_none

runner = CliRunner()
runner.invoke(target_app, ["start", "x-0b3f"])
"""


def test_watchdog_dumps_the_stalled_frame_and_exits_within_the_grace_window():
    started = time.monotonic()
    proc = subprocess.run(
        [sys.executable, "-c", _AC4_HP_SCRIPT], capture_output=True, text=True, timeout=30
    )
    elapsed = time.monotonic() - started

    assert proc.returncode != 0
    assert elapsed < 15
    assert "Timeout (0:00:02)!" in proc.stderr
    # The frame name is the positive marker that the dump named the stalled
    # function; an absent dump would fail this instead of passing it.
    assert "_graph_entries_or_none" in proc.stderr


def test_watchdog_is_cancelled_when_start_returns_early(monkeypatch):
    monkeypatch.setattr(target_cli, "_START_DEADLINE_S", 2)
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *a: None)

    result = runner.invoke(target_app, ["start", "x-0b3f"])
    assert result.exit_code == 1

    # If the watchdog had not been cancelled, it would _exit(1) the whole
    # pytest process at the 2s mark; reaching this assertion at all is the
    # proof that it was.
    time.sleep(3)
    assert True
