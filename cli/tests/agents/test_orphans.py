"""The orphan sweep: attribution, the withheld verdict, and reap narrowness.

The live positive control proves the NAME arm, the CWD arm and the kill path
on every run. It cannot prove the uid arm, the census arm, or the reap age
gate, because the probe is ours, is not a census row, and is seconds old. Those
three are proven here against a fabricated process table, and that division is
stated in the sweep's own output so nobody reads the control as covering more
than it does.
"""
from __future__ import annotations

import collections
import os
import time
from pathlib import Path

import pytest

from fno.agents import orphans

CpuTimes = collections.namedtuple("CpuTimes", "user system")


def _proc(
    pid: int,
    *,
    name: str = "sleep",
    argv0: str | None = None,
    cwd: str | None = None,
    uid: int | None = None,
    ppid: int = 1,
    cpu: float = 0.0,
    age: float = 60.0,
) -> dict:
    return {
        "pid": pid,
        "ppid": ppid,
        "name": name,
        "cmdline": [argv0 if argv0 is not None else name, "30"],
        # Default to the RUNNING uid, never a literal. A hardcoded 501 passes on
        # a mac and matches nothing on a CI runner, so every finding vanishes
        # and the whole file goes green-on-nothing in one environment and red in
        # the other.
        "uids": (uid if uid is not None else os.getuid(),) * 3,
        "create_time": time.time() - age,
        "cpu_times": CpuTimes(cpu, 0.0),
        "cwd": cwd,
    }


@pytest.fixture
def table(monkeypatch):
    """Substitute the process enumeration and neutralise the live probes, so a
    test scan sees exactly the rows it declares."""
    rows: list[dict] = []
    monkeypatch.setattr(orphans, "_iter_processes", lambda: iter(rows))
    monkeypatch.setattr(orphans, "_spawn_probe", lambda *a, **k: None)
    monkeypatch.setattr(orphans, "_await_orphaned", lambda *a, **k: False)
    monkeypatch.setattr(orphans, "_repo_roots", lambda: ["/repo"])
    monkeypatch.setattr(orphans, "_census_pids", set)
    # NEVER let a test signal a real process. The fabricated probes are pinned
    # to pids 9001/9002, which are ordinary live pids on a developer machine,
    # and an unstubbed `_kill` sent them a real SIGTERM and a SIGKILL two
    # seconds later. A test suite that kills the developer's own processes is a
    # worse bug than anything it is testing. Individual tests re-stub `_kill`
    # to record calls; the default here is that nothing escapes.
    monkeypatch.setattr(orphans, "_kill", lambda pid: None)
    monkeypatch.setattr(orphans, "_pid_alive", lambda pid: False)
    return rows


#: The two pids `_scan_with_working_control` hands to the fake probes.
PROBE_PIDS = (9001, 9002)


def _reaped_pids(killed: list[int]) -> list[int]:
    """Kills that are not probe cleanup."""
    return [pid for pid in killed if pid not in PROBE_PIDS]


def _scan_with_working_control(monkeypatch, rows, **kwargs):
    """Run a scan whose two controls are declared present in the table.

    The probes are pinned to fixed pids so the fabricated table can carry a row
    for each, which keeps every test running through the SAME control gate the
    live command runs through rather than around it.
    """
    rows.insert(0, _proc(9001, name="sleep", argv0="fno-orphan-probe-test", cwd="/tmp/x"))
    rows.insert(1, _proc(9002, name="sleep", cwd="/repo"))
    pids = iter([9001, 9002])
    monkeypatch.setattr(orphans, "_spawn_probe", lambda *a, **k: next(pids))
    monkeypatch.setattr(orphans, "_await_orphaned", lambda *a, **k: True)
    return orphans.scan(**kwargs)


# ─── the measured correction ────────────────────────────────────────────────


def test_name_arm_reads_argv0_not_the_executable() -> None:
    """Measured 2026-08-13: after `exec -a fno-load-test sleep 20`, psutil
    name() is `sleep` and only argv[0] carries the rename. Attributing on
    name() made this arm unfirable, and the live probe is what caught it."""
    info = _proc(1, name="sleep", argv0="fno-load-test")
    assert orphans.display_name(info) == "fno-load-test"
    assert orphans.was_renamed(info) is True
    assert orphans._attribute(info, ["/repo"]) == "NAME"


def test_a_real_fno_binary_is_not_claimed_by_the_name_arm() -> None:
    """`fno-agents-daemon` runs at PPID 1 in this repo right now. A bare
    fno-prefix test claims it, and `--reap` then kills the daemon. Requiring a
    genuine rename is what stops that."""
    daemon = _proc(
        2,
        name="fno-agents-daemon",
        argv0="/Users/x/.cargo/bin/fno-agents-daemon",
        cwd="/elsewhere",
    )
    assert orphans.was_renamed(daemon) is False
    assert orphans._attribute(daemon, ["/repo"]) is None


def test_daemon_found_by_cwd_is_reported_but_never_reaped(monkeypatch, table) -> None:
    table.append(
        _proc(
            42,
            name="fno-agents-daemon",
            argv0="/Users/x/.cargo/bin/fno-agents-daemon",
            cwd="/repo",
            age=90000,
        )
    )
    result = _scan_with_working_control(monkeypatch, table, reap=True)
    assert [f.pid for f in result.findings] == [42]
    assert result.reaped == []


# ─── attribution arms ───────────────────────────────────────────────────────


def test_cwd_arm_claims_an_ordinary_name_under_the_repo(monkeypatch, table) -> None:
    table.append(_proc(50, name="grep", cwd="/repo/.claude/worktrees/x-9f21"))
    result = _scan_with_working_control(monkeypatch, table)
    assert [(f.pid, f.arm) for f in result.findings] == [(50, "CWD")]


def test_external_worktree_path_is_claimed(monkeypatch, table) -> None:
    table.append(_proc(51, name="grep", cwd="/somewhere/.claude/worktrees/x-1"))
    result = _scan_with_working_control(monkeypatch, table)
    assert [f.pid for f in result.findings] == [51]


def test_unattributed_process_is_ignored(monkeypatch, table) -> None:
    table.append(_proc(52, name="Spotify", cwd="/Applications"))
    result = _scan_with_working_control(monkeypatch, table)
    assert result.findings == []


def test_foreign_uid_is_skipped(monkeypatch, table) -> None:
    """Not reachable by the live control: both probes are ours."""
    table.append(_proc(53, name="grep", cwd="/repo", uid=os.getuid() + 1))
    result = _scan_with_working_control(monkeypatch, table)
    assert result.findings == []


def test_census_worker_is_skipped(monkeypatch, table) -> None:
    """A `claude --bg` worker is detached to PPID 1 with a worktree cwd. Without
    this exclusion the first --reap takes down the fleet."""
    table.append(_proc(54, name="claude", cwd="/repo/.claude/worktrees/x-1"))
    monkeypatch.setattr(orphans, "_census_pids", lambda: {54})
    result = _scan_with_working_control(monkeypatch, table)
    assert result.findings == []
    assert result.census_excluded == 1


def test_no_cpu_floor(monkeypatch, table) -> None:
    """Specimen 3 burned no CPU. A floor would have hidden it."""
    table.append(_proc(55, name="grep", cwd="/repo", cpu=0.0))
    result = _scan_with_working_control(monkeypatch, table)
    assert [f.pid for f in result.findings] == [55]


# ─── the positive control ───────────────────────────────────────────────────


def test_missing_name_probe_withholds_the_verdict(monkeypatch, table) -> None:
    table.append(_proc(60, name="grep", cwd="/repo"))
    rows_with_cwd_probe_only = table
    rows_with_cwd_probe_only.insert(0, _proc(9002, name="sleep", cwd="/repo"))
    pids = iter([9001, 9002])
    monkeypatch.setattr(orphans, "_spawn_probe", lambda *a, **k: next(pids))
    monkeypatch.setattr(orphans, "_await_orphaned", lambda *a, **k: True)
    result = orphans.scan()  # _kill/_pid_alive stubbed by the `table` fixture
    assert result.broken
    assert "NAME" in (result.broken_reason or "")
    rendered = orphans.render(result)
    assert "verdict withheld (scan-broken)" in rendered
    assert "orphans:" not in rendered


def test_missing_cwd_probe_withholds_the_verdict(monkeypatch, table) -> None:
    table.insert(0, _proc(9001, name="sleep", argv0="fno-orphan-probe-test", cwd="/tmp/x"))
    pids = iter([9001, 9002])
    monkeypatch.setattr(orphans, "_spawn_probe", lambda *a, **k: next(pids))
    monkeypatch.setattr(orphans, "_await_orphaned", lambda *a, **k: True)
    result = orphans.scan()  # _kill/_pid_alive stubbed by the `table` fixture
    assert result.broken
    assert "CWD" in (result.broken_reason or "")
    assert "orphans:" not in orphans.render(result)


def test_a_dead_kill_path_withholds_the_verdict(monkeypatch, table) -> None:
    """`_kill` swallows every exception. Reading the kill control off "a probe
    was spawned" asserts an easier thing than a real kill does, so a broken
    kill path reported healthy while `--reap` no-opped and still printed
    `reaped: N`."""
    monkeypatch.setattr(orphans, "_kill", lambda pid: None)
    monkeypatch.setattr(orphans, "_pid_alive", lambda pid: True)
    result = _scan_with_working_control(monkeypatch, table)
    assert result.broken
    assert "kill" in (result.broken_reason or "")
    assert "orphans:" not in orphans.render(result)


def test_a_broken_scan_reaps_nothing(monkeypatch, table) -> None:
    table.append(_proc(61, name="sleep", argv0="fno-load-x1", age=3600))
    killed: list[int] = []
    monkeypatch.setattr(orphans, "_kill", killed.append)
    result = orphans.scan(reap=True)
    assert result.broken
    assert killed == []


def test_clean_scan_prints_the_control_above_the_count(monkeypatch, table) -> None:
    result = _scan_with_working_control(monkeypatch, table)
    lines = orphans.render(result).splitlines()
    assert not result.broken
    control = next(i for i, ln in enumerate(lines) if ln.startswith("control:"))
    count = next(i for i, ln in enumerate(lines) if ln.startswith("orphans:"))
    assert control < count
    assert lines[count] == "orphans: 0"


# ─── reap narrowness ────────────────────────────────────────────────────────


def test_reap_kills_an_old_renamed_fno_process(monkeypatch, table) -> None:
    table.append(_proc(70, name="sleep", argv0="fno-load-x1", age=1800))
    killed: list[int] = []
    monkeypatch.setattr(orphans, "_kill", killed.append)
    result = _scan_with_working_control(monkeypatch, table, reap=True)
    # The probes are killed too, on every run; that IS the live kill-path
    # control. Only non-probe kills are the reap under test here.
    assert _reaped_pids(killed) == [70]
    assert [f.pid for f in result.reaped] == [70]


def test_reap_spares_a_young_one(monkeypatch, table) -> None:
    table.append(_proc(71, name="sleep", argv0="fno-load-x1", age=60))
    killed: list[int] = []
    monkeypatch.setattr(orphans, "_kill", killed.append)
    result = _scan_with_working_control(monkeypatch, table, reap=True)
    assert _reaped_pids(killed) == []
    assert [f.pid for f in result.findings] == [71]


def test_reap_spares_an_unnamed_orphan(monkeypatch, table) -> None:
    """Tonight's 73 processes were named `yes`. Every orphan predating the
    guard is unnamed, so the reap can never touch it, and that is correct."""
    table.append(_proc(72, name="yes", cwd="/repo/.claude/worktrees/x-01ae", age=27000))
    killed: list[int] = []
    monkeypatch.setattr(orphans, "_kill", killed.append)
    result = _scan_with_working_control(monkeypatch, table, reap=True)
    assert _reaped_pids(killed) == []
    assert [f.pid for f in result.findings] == [72]
    assert "reaped: 0" in orphans.render(result)


# ─── the SessionStart quiet gate ────────────────────────────────────────────


def test_filter_new_is_loud_once_then_quiet(monkeypatch, table, tmp_path: Path) -> None:
    table.append(_proc(80, name="grep", cwd="/repo"))
    result = _scan_with_working_control(monkeypatch, table)
    seen = tmp_path / ".orphan-sweep-seen"
    assert orphans.filter_new(result, seen) is True
    assert orphans.filter_new(result, seen) is False


def test_filter_new_speaks_for_an_unseen_pid(monkeypatch, table, tmp_path: Path) -> None:
    seen = tmp_path / ".orphan-sweep-seen"
    seen.write_text("80:grep", encoding="utf-8")
    table.append(_proc(81, name="grep", cwd="/repo"))
    result = _scan_with_working_control(monkeypatch, table)
    assert orphans.filter_new(result, seen) is True
