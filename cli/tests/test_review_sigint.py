"""Tests for SIGINT handler in the review orchestrator.

Phase 05: SIGINT reaps workers and exits 130.

Test groups:
- reap_unit: unit tests for _reap_workers helper
- subprocess: end-to-end subprocess test (sends real SIGINT to a child process)
- handler_restore: verifies prior SIGINT handler is restored after normal exit
"""

from __future__ import annotations

import os
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
from pathlib import Path

from fno.review.orchestrator import AGENT_NAMES


# ---------------------------------------------------------------------------
# Unit tests: _reap_workers
# ---------------------------------------------------------------------------

def _spawn_polite_child() -> int:
    """Spawn a child that sleeps forever but exits on SIGTERM."""
    proc = subprocess.Popen(
        [sys.executable, "-c", "import time; time.sleep(60)"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return proc.pid


def _spawn_stubborn_child() -> int:
    """Spawn a child that ignores SIGTERM (needs SIGKILL)."""
    code = textwrap.dedent("""\
        import signal, time
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        time.sleep(60)
    """)
    proc = subprocess.Popen(
        [sys.executable, "-c", code],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return proc.pid


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        # Process exists but we lack permission
        return True


def test_ac1_reap_sigterm_polite_child():
    """AC1-REAP-SIGTERM: A polite child exits on SIGTERM; no zombies."""
    from fno.review.orchestrator import _reap_workers

    pid = _spawn_polite_child()
    # Give child a moment to settle
    time.sleep(0.1)
    assert _pid_alive(pid), "child should be alive before reap"

    _reap_workers([pid], sigterm_grace=5.0)

    assert not _pid_alive(pid), "child should be dead after reap"


def test_ac1_reap_sigkill_stubborn_child():
    """AC1-REAP-SIGKILL: A stubborn child (ignores SIGTERM) is force-killed."""
    from fno.review.orchestrator import _reap_workers

    pid = _spawn_stubborn_child()
    time.sleep(0.2)  # Let child install SIGTERM ignore handler
    assert _pid_alive(pid), "stubborn child should be alive before reap"

    # Use a short grace so the test runs fast (0.5s)
    _reap_workers([pid], sigterm_grace=0.5)

    assert not _pid_alive(pid), "stubborn child should be dead after SIGKILL"


def test_ac1_missing_pid_no_error():
    """AC1-MISSING-PID: Reaping an already-dead pid raises no error."""
    from fno.review.orchestrator import _reap_workers

    # Spawn and immediately kill so we have a confirmed-dead pid
    proc = subprocess.Popen(
        [sys.executable, "-c", "import sys; sys.exit(0)"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    proc.wait()
    dead_pid = proc.pid

    # Should not raise
    _reap_workers([dead_pid], sigterm_grace=1.0)


# ---------------------------------------------------------------------------
# Subprocess test: SIGINT -> exit 130
# ---------------------------------------------------------------------------

def test_ac2_subprocess_sigint_exits_130_reaps_workers_preserves_scratchpad():
    """AC2-EXIT-130 + AC2-REAP-ALL + AC2-SCRATCHPAD-PRESERVED.

    Starts a child process that runs orchestrate_review_parallel with a fake
    runner that blocks forever while tracking a real sleep PID. Sends SIGINT
    to the child after the worker is ready, then asserts:
    - child exits with code 130
    - the tracked sleep PID is no longer alive
    - the scratchpad directory still exists on disk
    """
    with tempfile.TemporaryDirectory() as tmpdir:
        tmppath = Path(tmpdir)
        scratchpad = tmppath / "scratchpad"
        scratchpad.mkdir()
        ready_file = tmppath / "ready.flag"
        pid_file = tmppath / "worker.pid"

        # Python script run as child process.
        # The fake runner captures the sleep_proc pid into the orchestrator's
        # worker_pids list so the SIGINT handler has a real target to reap.
        child_script = textwrap.dedent(f"""\
            import subprocess
            import sys
            import time
            from pathlib import Path
            from fno.review.orchestrator import (
                orchestrate_review_parallel,
                WorkerOutcome,
            )
            from fno.review.orchestrator import AGENT_NAMES

            ready_file = Path({str(ready_file)!r})
            pid_file = Path({str(pid_file)!r})
            scratchpad = Path({str(scratchpad)!r})

            # Spawn a real long-lived process so SIGINT handler has a real PID target.
            # We pass worker_pids into orchestrate_review_parallel so the handler
            # can reap this PID on SIGINT.
            sleep_proc = subprocess.Popen(
                ["sleep", "60"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            tracked_pids = [sleep_proc.pid]
            pid_file.write_text(str(sleep_proc.pid))
            ready_file.touch()  # signal readiness to parent

            import asyncio

            async def fake_runner(agent, prompt, diff):
                # Block until killed
                await asyncio.sleep(120)
                return WorkerOutcome(agent=agent, ok=True)

            orchestrate_review_parallel(
                diff_context="test diff",
                prompts={{name: "prompt" for name in {list(AGENT_NAMES)!r}}},
                runner=fake_runner,
                scratchpad_path=scratchpad,
                worker_pids=tracked_pids,
            )
        """)

        # Write script to a temp file so it's importable
        script_file = tmppath / "child_script.py"
        script_file.write_text(child_script)

        # Launch child with PYTHONPATH pointing to cli/src
        cli_src = Path(__file__).parent.parent / "src"
        env = os.environ.copy()
        env["PYTHONPATH"] = str(cli_src)
        env.pop("CLAUDECODE_SESSION_ID", None)

        child = subprocess.Popen(
            [sys.executable, str(script_file)],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        # Wait for ready flag (up to 10s)
        deadline = time.monotonic() + 10.0
        while not ready_file.exists():
            if time.monotonic() > deadline:
                child.kill()
                child.wait()
                raise TimeoutError("child never wrote ready.flag")
            time.sleep(0.05)

        # Give child a moment to enter the orchestration loop
        time.sleep(0.3)

        # Read the worker PID before sending SIGINT
        sleep_pid = int(pid_file.read_text().strip())

        # Send SIGINT to the child
        os.kill(child.pid, signal.SIGINT)

        # Wait for exit (up to 8s)
        try:
            rc = child.wait(timeout=8)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
            raise AssertionError("child did not exit within 8s after SIGINT")

        # AC2-EXIT-130: exit code must be 130
        assert rc == 130, f"expected exit code 130, got {rc}"

        # AC2-REAP-ALL: sleep child should be dead
        time.sleep(0.2)  # brief grace for OS to reclaim
        assert not _pid_alive(sleep_pid), (
            f"sleep child (pid {sleep_pid}) still alive after SIGINT reap"
        )

        # AC2-SCRATCHPAD-PRESERVED: scratchpad directory still on disk
        assert scratchpad.exists(), "scratchpad was deleted by SIGINT handler (should be preserved)"


# ---------------------------------------------------------------------------
# Handler restore test
# ---------------------------------------------------------------------------

def test_ac2_handler_restored_after_normal_exit():
    """AC2-HANDLER-RESTORED: prior SIGINT handler is restored after normal orchestration."""
    from fno.review.orchestrator import (
        orchestrate_review_parallel,
        WorkerOutcome,
        AGENT_NAMES,
    )
    import asyncio

    # Record the current handler before calling orchestrate_review_parallel
    prior = signal.getsignal(signal.SIGINT)

    async def fast_runner(agent, prompt, diff):
        return WorkerOutcome(agent=agent, ok=True)

    orchestrate_review_parallel(
        diff_context="test diff",
        prompts={name: "prompt" for name in AGENT_NAMES},
        runner=fast_runner,
    )

    after = signal.getsignal(signal.SIGINT)
    assert after == prior, (
        f"SIGINT handler was not restored: before={prior!r}, after={after!r}"
    )
