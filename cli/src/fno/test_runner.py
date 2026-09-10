"""Bounded suite runner: one process group per run, killed whole on expiry.

Cargo execs the deps/ test binary in place, so killing cargo alone orphans
it - the measured shape: a deps binary at ppid 1 for 3h07m holding 227
zombies. Every ``fno doctor test`` suite runs through this module.

``run_suite_bounded`` prefers the native ``fno-agents test-run`` owner
(``crates/fno-agents/src/test_run.rs``): it admits under one machine-wide
``test:suite`` claim and ALWAYS attempts a group-kill after the run, not only
on timeout - the Python ``wait_or_kill_group`` below only killed the group on
``TimeoutExpired``/an exception, which is exactly how a clean cargo exit left
an orphaned ``deps/`` test binary running for 3h32m. ``kill_group``/
``wait_or_kill_group`` stay as the degraded fallback for a checkout with no
``fno-agents`` binary installed, and as the policy census's ``_run_bounded``
(a different, per-probe kill bound, never suite-admission-gated) still uses.
"""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import sys
from collections.abc import Mapping, Sequence


def test_timeout_seconds() -> int:
    """The wall-clock bound for one suite run: FNO_TEST_TIMEOUT_SECONDS wins,
    then ``config.test.timeout_seconds``, else 1800."""
    raw = os.environ.get("FNO_TEST_TIMEOUT_SECONDS")
    if raw:
        try:
            value = int(raw)
        except ValueError:
            pass
        else:
            if value > 0:
                return value
    try:
        from fno.config import load_settings

        value = int(load_settings().test.timeout_seconds)
    except Exception:  # noqa: BLE001 - a broken settings value degrades to default
        return 1800
    return value if value > 0 else 1800


def kill_group(proc: subprocess.Popen) -> None:
    """SIGKILL the run's whole process group, then reap the leader.

    ProcessLookupError is the TOCTOU window where the child exited between
    the timeout and getpgid; wait() reaps it. PermissionError is NOT caught:
    start_new_session makes us own the group so it is near-impossible, and
    if it ever surfaces a loud crash beats a wedged proc.wait() with no kill.
    """
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except ProcessLookupError:
        pass  # the group is already gone; nothing to kill
    proc.wait()


def wait_or_kill_group(proc: subprocess.Popen, timeout: int) -> tuple[int, bool]:
    """Wait up to ``timeout``; past it, kill the GROUP and reap.

    Returns ``(rc, killed)``. On any other exception (Ctrl-C included: the
    child runs in its own session and would otherwise outlive the SIGINT
    with all its grandchildren) the group is killed before re-raising.
    """
    try:
        return proc.wait(timeout=timeout), False
    except subprocess.TimeoutExpired:
        kill_group(proc)
        return (proc.returncode if proc.returncode is not None else 124), True
    except BaseException:
        kill_group(proc)
        raise


def _native_owner_binary() -> str | None:
    """Path to the ``fno-agents`` binary, or ``None`` if not installed."""
    return shutil.which("fno-agents")


def run_suite_bounded(cmd: Sequence[str], env: Mapping[str, str], timeout: int, **kw) -> int:
    """Run cmd through the native ``fno-agents test-run`` owner, which admits
    under the shared ``test:suite`` claim and ALWAYS group-kills after the
    run (success, failure, or timeout) - not only on timeout, the gap that
    let a clean-exit leader orphan a group-mate. Degrades to the pre-native
    Python group-kill (timeout only) when the binary is unavailable, rather
    than failing the suite."""
    native = _native_owner_binary()
    if native is None:
        proc = subprocess.Popen(cmd, env=env, start_new_session=True, **kw)
        rc, killed = wait_or_kill_group(proc, timeout)
        if killed:
            sys.stderr.write(
                f"fno doctor test: TIMEOUT after {timeout}s; process group killed\n"
            )
            return 124
        return rc

    claims_root = os.environ.get("FNO_CLAIMS_ROOT") or os.environ.get("HOME")
    native_cmd = [native, "test-run", "--timeout", str(timeout)]
    if claims_root:
        native_cmd += ["--claims-root", claims_root]
    native_cmd += ["--", *cmd]
    proc = subprocess.Popen(native_cmd, env=env, **kw)
    return proc.wait()
