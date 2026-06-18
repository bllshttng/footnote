"""SIGINT-handling tests for `fno agents logs --follow` (AC2-FR).

Two test layers:

1. Unit-level — ``read_logs`` catches ``KeyboardInterrupt`` from the
   follow loop and returns exit 0 with no traceback on stderr. This is
   the load-bearing behavior the contract promises.

2. Subprocess-level — spawns the actual ``python -m fno.cli``
   process, lets it enter follow mode, sends ``SIGINT``, asserts a
   clean exit code and empty stderr. Skipped on platforms where
   ``signal.SIGINT`` does not behave as expected (e.g. Windows). The
   subprocess test is more brittle than the unit test, but it's the
   only check that catches real cleanup bugs the harness might mask.
"""
from __future__ import annotations

import io
import os
import signal
import subprocess
import sys
import time
from pathlib import Path
from types import SimpleNamespace
from typing import IO

import pytest

from fno.paths_testing import use_tmpdir
from fno.agents.registry import AgentEntry, write_registry


def _codex(**kw) -> AgentEntry:
    base = dict(
        name="follow-target",
        provider="codex",
        cwd="/tmp",
        log_path="",
        codex_session_id="codex-followtest",
        created_at="2026-05-21T00:00:00Z",
        status="live",
        last_message_at=None,
    )
    base.update(kw)
    return AgentEntry(**base)


def test_read_logs_codex_follow_swallows_keyboard_interrupt(tmp_path, monkeypatch):
    """AC2-FR — KeyboardInterrupt during follow → exit 0, no traceback."""
    use_tmpdir(monkeypatch, tmp_path)
    log_file = tmp_path / "follow.jsonl"
    log_file.write_text('{"line": 1}\n', encoding="utf-8")
    write_registry([_codex(log_path=str(log_file))])

    # Patch the follow loop to raise KeyboardInterrupt on the first poll.
    from fno.agents import read as read_mod

    def _fake_follow(path, stdout, stderr, poll_interval=0.5):
        # Simulate the user pressing Ctrl-C during the polling wait.
        raise KeyboardInterrupt

    monkeypatch.setattr(read_mod, "_follow_jsonl", _fake_follow)

    stdout_buf, stderr_buf = io.StringIO(), io.StringIO()
    result = read_mod.read_logs(
        name="follow-target",
        tail=None,
        follow=True,
        stdout=stdout_buf,
        stderr=stderr_buf,
    )

    assert result.exit_code == 0
    assert "traceback" not in stderr_buf.getvalue().lower()
    # The initial tail should have flushed the one record before the loop.
    assert "{\"line\": 1}" in stdout_buf.getvalue()


def test_follow_jsonl_detects_window_spanning_truncate(tmp_path):
    """Truncate-then-refill within a single poll window must surface, not emit garbage.

    Without cross-iteration last_size tracking, this case would be missed:
    a writer truncates the file AND refills past our previous read offset
    before our next stat call. fh.tell() <= st_size so the offset-based
    check is silent, but the underlying records are different.

    READINESS HANDSHAKE (was @pytest.mark.flaky_socket; de-quarantined).
    -----------------------------------------------------------------
    The old version ran ``_follow_jsonl`` in the main thread and had a
    writer thread ``time.sleep(0.4)`` before truncating, gambling that the
    reader had attached (opened + seeked-to-end) within that fixed budget.
    Under a saturating concurrent build the reader could attach AFTER the
    one-shot truncate, never observe a size drop, and poll forever -> hang.
    The fix removes the sleep barrier: we run ``_follow_jsonl`` in a thread
    and keep APPENDING a sentinel line until the reader echoes one to stdout.
    The loop-feed is robust against the reader's seek-to-EOF timing (a single
    append could land before the seek and be skipped; a repeated append
    always reaches a reader polling at EOF). The echo is proof the reader is
    attached and polling, so the subsequent truncate is GUARANTEED to land
    inside its follow loop. No fixed sleep is load-bearing; the budgets below
    are bounded defense-in-depth.
    """
    import io as _io
    import threading

    from fno.agents import read as read_mod

    log = tmp_path / "follow.jsonl"
    # Initial content the producer wrote before we attached (skipped: the
    # follower seeks to EOF, so these never reach stdout).
    log.write_text('{"line": "first"}\n' * 20, encoding="utf-8")

    stdout_buf = _io.StringIO()
    stderr_buf = _io.StringIO()

    follow_done = threading.Event()

    def _run_follower():
        read_mod._follow_jsonl(log, stdout_buf, stderr_buf, poll_interval=0.05)
        follow_done.set()

    follow_thread = threading.Thread(target=_run_follower, daemon=True)
    follow_thread.start()

    # OBSERVED readiness barrier: keep appending a sentinel until the reader
    # echoes one. Looping (not a single append) defeats the seek-to-EOF race -
    # whenever the follower finishes seeking, a later sentinel reaches it.
    sentinel = '{"line": "__reader_ready__"}\n'
    deadline = time.monotonic() + 5.0
    while "__reader_ready__" not in stdout_buf.getvalue():
        with log.open("a", encoding="utf-8") as fh:
            fh.write(sentinel)
        assert time.monotonic() < deadline, "follower never echoed the readiness sentinel"
        time.sleep(0.02)

    # Now truncate-in-place then refill smaller than the original offset.
    # Inode unchanged, so the rotation branch is silent; the size drop is
    # what the detector must catch. The reader is provably polling, so this
    # lands inside its follow loop and the next poll surfaces it.
    log.write_text('{"line": "rotated"}\n' * 3, encoding="utf-8")

    # _follow_jsonl returns once it detects the shrink. Bounded join: a
    # timeout here would itself be a real detection bug, not a flake.
    assert follow_done.wait(timeout=5.0), "follower did not detect the truncate within 5s"
    follow_thread.join(timeout=1.0)
    assert not follow_thread.is_alive(), "follower thread did not exit cleanly"
    msg = stderr_buf.getvalue().lower()
    assert "truncat" in msg or "shrank" in msg


def test_read_logs_codex_follow_open_race_returns_clean(tmp_path, monkeypatch):
    """Codex P2 — file deleted between tail read and _follow_jsonl open → clean exit.

    Simulates the open-time race: the tail read succeeds, then _follow_jsonl
    raises FileNotFoundError when it tries to open the path. Without the
    catch added in this commit, the operator would see a traceback;
    with it, they see a structured stderr note and exit_code=13.
    """
    import io as _io

    from fno.agents import read as read_mod

    use_tmpdir(monkeypatch, tmp_path)
    log_file = tmp_path / "follow.jsonl"
    log_file.write_text('{"line": 1}\n', encoding="utf-8")
    write_registry([_codex(log_path=str(log_file))])

    def _race_open(path, stdout, stderr, poll_interval=0.5):
        raise FileNotFoundError(f"log file removed mid-follow: {path}")

    monkeypatch.setattr(read_mod, "_follow_jsonl", _race_open)

    stdout_buf, stderr_buf = _io.StringIO(), _io.StringIO()
    result = read_mod.read_logs(
        name="follow-target",
        tail=None,
        follow=True,
        stdout=stdout_buf,
        stderr=stderr_buf,
    )

    assert result.exit_code == read_mod.EXIT_NOT_FOUND
    assert "traceback" not in stderr_buf.getvalue().lower()
    assert "disappeared" in stderr_buf.getvalue().lower()


def test_read_logs_codex_follow_permission_error_distinct_from_disappeared(
    tmp_path, monkeypatch
):
    """Gemini PR #301 — PermissionError is OSError but NOT a disappearance.

    Distinguish the two so an operator can tell "rotated away" (exit 13)
    from "real infrastructure problem" (exit 1) without reading stderr.
    """
    import io as _io

    from fno.agents import read as read_mod

    use_tmpdir(monkeypatch, tmp_path)
    log_file = tmp_path / "follow.jsonl"
    log_file.write_text('{"line": 1}\n', encoding="utf-8")
    write_registry([_codex(log_path=str(log_file))])

    def _perm_denied(path, stdout, stderr, poll_interval=0.5):
        raise PermissionError(f"open denied: {path}")

    monkeypatch.setattr(read_mod, "_follow_jsonl", _perm_denied)

    stdout_buf, stderr_buf = _io.StringIO(), _io.StringIO()
    result = read_mod.read_logs(
        name="follow-target",
        tail=None,
        follow=True,
        stdout=stdout_buf,
        stderr=stderr_buf,
    )

    assert result.exit_code == 1
    assert "traceback" not in stderr_buf.getvalue().lower()
    assert "failed to open" in stderr_buf.getvalue().lower()
    # Should NOT misleadingly say "disappeared".
    assert "disappeared" not in stderr_buf.getvalue().lower()


def test_read_logs_codex_follow_handles_disappearing_log(tmp_path, monkeypatch):
    """When the log file vanishes mid-follow, the poller exits with a stderr note."""
    use_tmpdir(monkeypatch, tmp_path)
    log_file = tmp_path / "follow.jsonl"
    log_file.write_text("\n", encoding="utf-8")
    write_registry([_codex(log_path=str(log_file))])

    from fno.agents import read as read_mod

    def _fake_follow(path, stdout, stderr, poll_interval=0.5):
        # Simulate the log file disappearing during polling.
        stderr.write(f"log file disappeared: {path}\n")

    monkeypatch.setattr(read_mod, "_follow_jsonl", _fake_follow)

    stdout_buf, stderr_buf = io.StringIO(), io.StringIO()
    result = read_mod.read_logs(
        name="follow-target",
        tail=None,
        follow=True,
        stdout=stdout_buf,
        stderr=stderr_buf,
    )

    # Either 0 or EXIT_OK is acceptable here; the contract is "clean exit
    # with a stderr note, not a traceback".
    assert result.exit_code == 0
    assert "log file disappeared" in stderr_buf.getvalue()


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="SIGINT signal-delivery on Windows is process-group-specific.",
)
def test_subprocess_follow_clean_sigint_exit(tmp_path, monkeypatch):
    """AC2-FR end-to-end — spawn the CLI, send SIGINT, assert clean exit.

    Uses the codex provider path because:
    - It does not shell out to a real ``claude`` binary (which isn't on
      PATH in CI).
    - The poll loop is in our own code, so SIGINT cleanup is our
      responsibility (not delegated to a subprocess).

    The test prepares a codex registry entry whose log_path exists (so
    the verb doesn't take the "US4 not yet shipped" exit-13 branch),
    spawns fno via ``python -m fno.cli``, lets it enter the
    follow loop, sends ``SIGINT``, and asserts the process exits with
    code 0 and no traceback on stderr.
    """
    use_tmpdir(monkeypatch, tmp_path)
    log_file = tmp_path / "follow.jsonl"
    log_file.write_text('{"seed": true}\n', encoding="utf-8")
    write_registry([_codex(log_path=str(log_file))])

    # Pass FNO_CONFIG through the env so the spawned process
    # resolves to the same tmp registry.
    env = os.environ.copy()
    env["FNO_CONFIG"] = str(tmp_path / ".fno" / "settings.yaml")
    # PYTHONUNBUFFERED so the seed line flushes before we SIGINT.
    env["PYTHONUNBUFFERED"] = "1"

    # Run as ``python -m fno.cli`` rather than relying on a
    # console-scripts entry point that may not be installed in CI.
    cmd = [
        sys.executable,
        "-m",
        "fno.cli",
        "agents",
        "logs",
        "follow-target",
        "--follow",
    ]
    proc = subprocess.Popen(
        cmd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=str(tmp_path),
    )
    try:
        # Give the follow loop ~1s to enter the poll waiting state.
        time.sleep(1.0)
        proc.send_signal(signal.SIGINT)
        try:
            stdout, stderr = proc.communicate(timeout=5.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            stdout, stderr = proc.communicate()
            pytest.fail(
                f"follow did not exit within 5s of SIGINT;"
                f" stdout={stdout!r} stderr={stderr!r}"
            )
    finally:
        if proc.poll() is None:
            proc.kill()

    assert proc.returncode == 0, (
        f"expected clean SIGINT exit, got returncode={proc.returncode}\n"
        f"stdout={stdout!r}\nstderr={stderr!r}"
    )
    decoded_err = stderr.decode("utf-8", errors="replace")
    assert "traceback" not in decoded_err.lower(), (
        f"SIGINT produced a traceback:\n{decoded_err}"
    )
