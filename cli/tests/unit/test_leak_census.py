"""Real-process tests for the leak census (x-ec81 change 2).

Every case spawns a REAL process tree - never a fabricated table - because
the unit under test is a live-process reaper, and a fake table proves the
fake. Each orphan's real ppid is passed as ``reaper`` so the census's
orphan-only cwd read (orphans.iter_processes reads cwd for
``ppid in reaper_set(reaper)`` only) sees exactly the trees this test made,
under any subreaper arrangement Linux provides.
"""
from __future__ import annotations

import os
import subprocess
import time

from tests._leak_census import reap_rooted

_SLEEP = 60  # seconds; every spawned process is killed by the test or reaper


def _spawn_rooted(cwd, command: str) -> subprocess.Popen:
    proc = subprocess.Popen(
        ["/bin/sh", "-c", command],
        cwd=str(cwd),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    # Give the shell a moment to fork its children so the tree is real
    # before the census reads it.
    time.sleep(0.3)
    return proc


def test_reap_rooted_takes_the_whole_tree(tmp_path):
    """An orphan shell rooted under the root owns a descendant rooted
    elsewhere ('/'); the descendant comes back too, and everything is
    terminal within 5 s."""
    leaf = tmp_path / "leaf"
    leaf.mkdir()
    # Two background children: one sleep inheriting the leaf cwd, one
    # subshell that cds to / before exec'ing sleep. The /-rooted sleep is
    # NOT under the root, so the tree capture - not the cwd match - is what
    # reaches it.
    shell = _spawn_rooted(
        leaf,
        f"/bin/sleep {_SLEEP} & (cd / && exec /bin/sleep {_SLEEP}) & wait",
    )
    try:
        assert shell.poll() is None

        started = time.monotonic()
        rows = reap_rooted([str(tmp_path)], reaper=os.getpid())
        elapsed = time.monotonic() - started

        pids = {r["pid"] for r in rows}
        assert shell.pid in pids
        # The /-rooted descendant: captured through the tree, so its report
        # row carries no matched cwd of its own.
        assert any(
            r["root_pid"] == shell.pid and r["cwd"] is None for r in rows
        ), rows
        assert all(r["terminal"] for r in rows), rows
        assert elapsed < 5, elapsed
        assert shell.poll() is not None
    finally:
        shell.kill()
        shell.wait()


def test_reap_rooted_leaves_processes_outside_the_root(tmp_path):
    """Positive control: an orphan rooted in a SIBLING of the root survives
    the call. A census that killed it would be sweeping by adjacency, not
    ownership."""
    root_area = tmp_path / "root-area"
    root_area.mkdir()
    outside = tmp_path / "outside"
    outside.mkdir()
    inner = root_area / "inner"
    inner.mkdir()
    inside = _spawn_rooted(inner, f"exec /bin/sleep {_SLEEP}")
    control = _spawn_rooted(outside, f"exec /bin/sleep {_SLEEP}")
    try:
        rows = reap_rooted([str(root_area)], reaper=os.getpid())

        assert {r["pid"] for r in rows} == {inside.pid}
        assert rows[0]["terminal"] is True
        assert control.poll() is None
    finally:
        control.kill()
        inside.kill()
        control.wait()
        inside.wait()


def test_match_component_reaps_garbage_only(tmp_path):
    """With match_component='garbage-', a process under root/garbage-x/ is
    reaped and one under root/pytest-9/ is untouched - the shape of the real
    start-of-session sweep against a live sibling session."""
    garbage = tmp_path / "garbage-abc"
    garbage.mkdir()
    live = tmp_path / "pytest-9"
    live.mkdir()
    in_garbage = _spawn_rooted(garbage, f"exec /bin/sleep {_SLEEP}")
    in_live = _spawn_rooted(live, f"exec /bin/sleep {_SLEEP}")
    try:
        rows = reap_rooted(
            [str(tmp_path)], match_component="garbage-", reaper=os.getpid()
        )

        assert {r["pid"] for r in rows} == {in_garbage.pid}
        assert rows[0]["terminal"] is True
        assert in_live.poll() is None
    finally:
        in_live.kill()
        in_garbage.kill()
        in_live.wait()
        in_garbage.wait()


def test_reap_rooted_reports_cmdline_and_cwd(tmp_path):
    """The report carries what the failure message needs: the pid, its cwd
    (which names the leaking test dir) and its cmdline."""
    leaf = tmp_path / "leaf"
    leaf.mkdir()
    proc = _spawn_rooted(leaf, f"exec /bin/sleep {_SLEEP}")
    try:
        rows = reap_rooted([str(tmp_path)], reaper=os.getpid())

        assert len(rows) == 1
        row = rows[0]
        assert row["pid"] == proc.pid
        assert row["cwd"] == str(leaf)
        assert row["cmdline"] and "sleep" in " ".join(row["cmdline"])
    finally:
        proc.kill()
        proc.wait()
