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
    create: float | None = None,
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
        # An explicit `create` pins the process identity across two fabricated
        # scans. Deriving it from `age` alone makes "the same process an hour
        # later" a DIFFERENT process, which is a test that cannot see a
        # start-time-keyed bug.
        "create_time": create if create is not None else time.time() - age,
        "cpu_times": CpuTimes(cpu, 0.0),
        "cwd": cwd,
    }


@pytest.fixture
def table(monkeypatch):
    """Substitute the process enumeration and neutralise the live probes, so a
    test scan sees exactly the rows it declares."""
    rows: list[dict] = []
    monkeypatch.setattr(orphans, "_iter_processes", lambda *a, **k: iter(rows))
    monkeypatch.setattr(orphans, "_spawn_probe", lambda *a, **k: None)
    monkeypatch.setattr(orphans, "_await_orphaned", lambda *a, **k: None)
    monkeypatch.setattr(orphans, "_repo_roots", lambda: ["/repo"])
    monkeypatch.setattr(orphans, "_census_pids", set)
    # NEVER let a test signal a real process. The fabricated probes are pinned
    # to pids 9001/9002, which are ordinary live pids on a developer machine,
    # and an unstubbed `_kill` sent them a real SIGTERM and a SIGKILL two
    # seconds later. A test suite that kills the developer's own processes is a
    # worse bug than anything it is testing. Individual tests re-stub `_kill`
    # to record calls; the default here is that nothing escapes.
    # True = our signal reached it, which is the ordinary case these tests mean.
    monkeypatch.setattr(orphans, "_kill", lambda pid: True)
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
    monkeypatch.setattr(orphans, "_await_orphaned", lambda *a, **k: 1)
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


def test_the_reaper_is_measured_not_assumed_to_be_pid_1(monkeypatch, table) -> None:
    """A Linux host with a child subreaper reparents orphans to it, not to 1.

    `systemd --user`, and anything else calling PR_SET_CHILD_SUBREAPER, becomes
    the reaper. A literal `== 1` made both probes fail there, so every hourly
    sweep printed `verdict withheld (scan-broken)` forever with no way to quiet
    it. Here the reaper is 4242 and the sweep works exactly as it does on macOS.
    """
    rows = table
    rows.insert(0, _proc(9001, name="sleep", argv0="fno-orphan-probe-t", cwd="/tmp/x", ppid=4242))
    rows.insert(1, _proc(9002, name="sleep", cwd="/repo", ppid=4242))
    rows.append(_proc(88, name="sleep", argv0="fno-load-x", cwd="/repo", ppid=4242))
    # Still at PID 1, so on this host it is NOT an orphan.
    rows.append(_proc(89, name="sleep", argv0="fno-load-y", cwd="/repo", ppid=1))
    pids = iter([9001, 9002])
    monkeypatch.setattr(orphans, "_spawn_probe", lambda *a, **k: next(pids))
    monkeypatch.setattr(orphans, "_await_orphaned", lambda *a, **k: 4242)

    result = orphans.scan()
    assert not result.broken, result.broken_reason
    assert [f.pid for f in result.findings] == [88]


def test_a_probe_that_never_reparents_withholds_the_verdict(monkeypatch, table) -> None:
    """With no reaper there is nothing to compare a ppid against.

    Counting against a guessed reaper is the absence trap; say the reaper is
    unknown instead.
    """
    monkeypatch.setattr(orphans, "_spawn_probe", lambda *a, **k: 9001)
    monkeypatch.setattr(orphans, "_await_orphaned", lambda *a, **k: None)

    result = orphans.scan()
    assert result.broken
    assert "reaper is unknown" in result.broken_reason
    assert result.findings == []


def test_an_out_of_enum_skip_probe_refuses(monkeypatch, table) -> None:
    """A falsifier that cannot fail is worse than no falsifier.

    An unrecognised value skipped nothing, so the scan came back healthy and an
    operator read that as proof the control works. `FNO_ORPHANS_SKIP_PROBE=NAME`
    is one shift key away from that.
    """
    result = orphans.scan(skip_probe="NAME")
    assert result.broken
    assert "not 'name' or 'cwd'" in result.broken_reason
    assert result.findings == []


def test_outside_a_repo_the_sweep_withholds_rather_than_claim_cwd(
    monkeypatch, table
) -> None:
    """No git root means the CWD arm has nowhere to look.

    Falling back to os.getcwd() made every PPID-1 process under $HOME a
    finding. Inventing territory to keep a control green is the absence trap in
    a different hat.
    """
    monkeypatch.setattr(orphans, "_repo_roots", list)

    result = orphans.scan()
    assert result.broken
    assert "no git root" in result.broken_reason
    assert result.findings == []
    assert "verdict withheld (scan-broken)" in orphans.render(result)


def test_a_probe_that_died_on_its_own_does_not_certify_the_kill_path(
    monkeypatch, table
) -> None:
    """The control must prove WE ended the probe, not that it is gone.

    Reading only "it is gone now" lets a completely broken `_kill` report a
    healthy kill arm against any short-lived probe, and `--reap` then runs
    behind a control that never tested anything.
    """
    monkeypatch.setattr(orphans, "_kill", lambda pid: False)  # signal never landed
    monkeypatch.setattr(orphans, "_pid_alive", lambda pid: False)  # but it is gone

    result = _scan_with_working_control(monkeypatch, table)
    assert result.broken, "a kill nobody made cannot certify the kill path"
    assert "kill arm FAILED" in result.broken_reason
    assert "verdict withheld (scan-broken)" in orphans.render(result)


def test_a_process_that_died_on_its_own_is_not_credited_as_reaped(
    monkeypatch, table
) -> None:
    """`reaped:` counts signals WE sent, not processes that happen to be gone.

    A pid that exits between the scan and the reap makes `os.kill` raise, and
    the liveness re-check then sees a dead process. Crediting that prints a
    receipt for a signal nobody sent, which is the lie this module refuses.
    """
    table.append(
        _proc(
            77,
            name="sleep",
            argv0="fno-load-gone",
            cwd="/repo",
            age=orphans.REAP_MIN_AGE_S + 60,
        )
    )
    monkeypatch.setattr(orphans, "_kill", lambda pid: False)  # ESRCH: already gone

    result = _scan_with_working_control(monkeypatch, table, reap=True)
    assert [f.pid for f in result.findings] == [77], "still a finding"
    assert result.reaped == [], "no signal landed, so no reap to claim"


def test_our_own_daemon_is_counted_not_listed(monkeypatch, table) -> None:
    """PPID 1 is this daemon's normal state, so its presence carries no signal.

    Reported hourly it is noise nobody can act on, and every restart mints a
    fresh seen-key that speaks again. Counted instead, never dropped in
    silence, and never reaped either way.
    """
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
    assert [f.pid for f in result.findings] == []
    assert result.daemons_excluded == 1
    assert "1 own-daemon" in orphans.render(result)
    assert result.reaped == []


def test_a_daemon_lookalike_is_still_reported(monkeypatch, table) -> None:
    """The exclusion matches argv[0] exactly, so a near-name stays a finding.

    A prefix test here would hand anyone a way to opt out of the sweep by
    naming their process `fno-agents-daemon-<anything>`.
    """
    table.append(
        _proc(
            43,
            name="fno-agents-daemon-load",
            argv0="/Users/x/.cargo/bin/fno-agents-daemon-load",
            cwd="/repo",
            age=90000,
        )
    )
    result = _scan_with_working_control(monkeypatch, table, reap=True)
    assert [f.pid for f in result.findings] == [43]
    assert result.daemons_excluded == 0
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
    monkeypatch.setattr(orphans, "_await_orphaned", lambda *a, **k: 1)
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
    monkeypatch.setattr(orphans, "_await_orphaned", lambda *a, **k: 1)
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
    monkeypatch.setattr(orphans, "_kill", _recording_kill(killed))
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
def _recording_kill(killed: list[int]):
    """Record the pid AND report that the signal landed.

    `list.append` returns None, and the reap now credits only a kill it
    actually delivered, so a bare `killed.append` stub silently meant "the
    signal never landed" and every reap assertion went empty.
    """

    def _kill(pid: int) -> bool:
        killed.append(pid)
        return True

    return _kill




def test_reap_kills_an_old_renamed_fno_process(monkeypatch, table) -> None:
    table.append(_proc(70, name="sleep", argv0="fno-load-x1", age=1800))
    killed: list[int] = []
    monkeypatch.setattr(orphans, "_kill", _recording_kill(killed))
    result = _scan_with_working_control(monkeypatch, table, reap=True)
    # The probes are killed too, on every run; that IS the live kill-path
    # control. Only non-probe kills are the reap under test here.
    assert _reaped_pids(killed) == [70]
    assert [f.pid for f in result.reaped] == [70]


def test_reap_spares_a_young_one(monkeypatch, table) -> None:
    table.append(_proc(71, name="sleep", argv0="fno-load-x1", age=60))
    killed: list[int] = []
    monkeypatch.setattr(orphans, "_kill", _recording_kill(killed))
    result = _scan_with_working_control(monkeypatch, table, reap=True)
    assert _reaped_pids(killed) == []
    assert [f.pid for f in result.findings] == [71]


def test_reap_spares_an_unnamed_orphan(monkeypatch, table) -> None:
    """Tonight's 73 processes were named `yes`. Every orphan predating the
    guard is unnamed, so the reap can never touch it, and that is correct."""
    table.append(_proc(72, name="yes", cwd="/repo/.claude/worktrees/x-01ae", age=27000))
    killed: list[int] = []
    monkeypatch.setattr(orphans, "_kill", _recording_kill(killed))
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


def test_seen_key_ignores_age(monkeypatch, table, tmp_path: Path) -> None:
    """The bug the first two filter_new tests could not see.

    They reused one ScanResult inside one second. Keying on an age BUCKET made
    the key advance every minute, so a long-lived daemon read as new at every
    hourly sweep and reprinted at every session start forever. The key must
    depend on the process's START time, which is fixed, never on how long it
    has been running, which is not.

    Asserted on the key directly rather than through two fabricated scans: a
    second scan with a different create_time is a DIFFERENT process, so that
    route cannot express "the same process, later" at all.
    """
    born = time.time() - 600
    common = dict(
        pid=90, name="node", exe_name="node", renamed=False, cmdline="node",
        cwd="/repo", cpu_seconds=0.0, start_time=born, arm="CWD",
    )
    young = orphans.Finding(age_seconds=600, **common)
    old = orphans.Finding(age_seconds=600 + 3600, **common)
    assert orphans._seen_key(young) == orphans._seen_key(old)

    seen = tmp_path / ".orphan-sweep-seen"
    table.append(_proc(90, name="node", cwd="/repo", create=born))
    result = _scan_with_working_control(monkeypatch, table)
    assert orphans.filter_new(result, seen) is True
    assert orphans.filter_new(result, seen) is False


def test_a_name_with_a_space_still_goes_quiet(monkeypatch, table, tmp_path: Path) -> None:
    """The seen-file is newline-joined, so it must be read by LINES.

    Read by whitespace, a key carrying an argv[0] like `Chrome Helper` broke
    into three tokens and never matched itself again, so exactly the long-lived
    third-party process `--quiet-unless-new` exists to silence reprinted at
    every session start forever. macOS argv[0] with a space is routine.
    """
    seen = tmp_path / ".orphan-sweep-seen"
    table.append(_proc(93, name="Chrome Helper", cwd="/repo", create=time.time() - 900))
    result = _scan_with_working_control(monkeypatch, table)
    assert [f.name for f in result.findings] == ["Chrome Helper"]
    assert orphans.filter_new(result, seen) is True
    assert orphans.filter_new(result, seen) is False


def test_a_concurrent_sweeps_probe_is_not_an_orphan(monkeypatch, table) -> None:
    """Two sweeps inside one 30s probe lifetime is likely, because worktrees of
    one repo share the hourly stamp. Neither may report the other's control."""
    table.append(
        _proc(91, name="sleep", argv0="fno-orphan-probe-beef", cwd="/tmp/other")
    )
    # Both arms, not just the NAME one. The other sweep's CWD probe is the one
    # that leaks: it sits in the repo root at PPID 1, which is exactly the shape
    # the CWD arm reports, so an unmarked one was listed as an orphan.
    table.append(
        _proc(92, name="sleep", argv0="orphan-probe-cwd-beef", cwd="/repo")
    )
    result = _scan_with_working_control(monkeypatch, table)
    assert result.findings == []


def test_an_unreadable_census_breaks_the_scan(monkeypatch, table) -> None:
    """An empty exclusion set reads as "no live workers" and hands every
    `claude --bg` process to a reap that SessionStart runs unattended."""
    table.append(_proc(92, name="claude", cwd="/repo", age=3600))
    monkeypatch.setattr(orphans, "_census_pids", lambda: None)
    result = _scan_with_working_control(monkeypatch, table, reap=True)
    assert result.broken
    assert "census" in (result.broken_reason or "")
    assert result.reaped == []
    assert "orphans:" not in orphans.render(result)


def test_a_broken_scan_does_not_mark_its_findings_reported(
    monkeypatch, table, tmp_path: Path
) -> None:
    """A withheld finding is not a reported finding.

    `render` prints no list on a broken scan, so recording those findings as
    seen let one census failure silence a real orphan on every healthy sweep
    afterwards - the exact silence this module exists to make impossible.
    """
    table.append(_proc(95, name="grep", cwd="/repo"))
    seen = tmp_path / ".orphan-sweep-seen"

    monkeypatch.setattr(orphans, "_census_pids", lambda: None)
    broken = _scan_with_working_control(monkeypatch, table)
    assert broken.broken
    assert orphans.filter_new(broken, seen) is True
    assert "orphans:" not in orphans.render(broken)

    monkeypatch.setattr(orphans, "_census_pids", lambda: set())
    healthy = _scan_with_working_control(monkeypatch, table)
    assert not healthy.broken
    assert orphans.filter_new(healthy, seen) is True


def test_a_report_run_claims_no_reap(monkeypatch, table) -> None:
    """Without `--reap` no kill was attempted, so `reaped: 0` would be a lie
    about a signal that was never sent."""
    table.append(_proc(96, name="grep", cwd="/repo"))
    result = _scan_with_working_control(monkeypatch, table)
    assert "reaped" not in orphans.render(result)


def test_filter_new_speaks_for_an_unseen_pid(monkeypatch, table, tmp_path: Path) -> None:
    seen = tmp_path / ".orphan-sweep-seen"
    seen.write_text("80:grep", encoding="utf-8")
    table.append(_proc(81, name="grep", cwd="/repo"))
    result = _scan_with_working_control(monkeypatch, table)
    assert orphans.filter_new(result, seen) is True


def test_session_start_appends_outstanding_then_orphan_sweep() -> None:
    """Both blocks survive, in order, when another branch rewrites the hook.

    A concurrent rewrite of session-start.sh merges clean while dropping or
    reordering either block, so this names both appends and their order
    instead of counting sections. The sweep speaks after outstanding because
    an unharvested carve-out outranks a stale process.
    """
    body = (
        Path(__file__).resolve().parents[3] / "hooks" / "session-start.sh"
    ).read_text(encoding="utf-8")

    for call in ('append_section "$outstanding_content"',
                 'append_section "$orphan_content"'):
        assert body.count(call) == 1, f"session-start.sh lost {call!r}"

    assert body.index('append_section "$outstanding_content"') < body.index(
        'append_section "$orphan_content"'
    ), "outstanding must be appended before the orphan sweep"
    assert "fno agents orphans --reap --quiet-unless-new" in body
