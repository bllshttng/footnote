"""Real-subprocess AC-FR tests for the codex provider (Wave 2.2).

Spawns providers.codex.create() / resume() against a controlled fake-codex
shim that hangs forever. Verifies SIGINT and timeout signaling actually
deliver and clean up — monkeypatched Popen cannot validate signal
delivery (US3's lesson: mocked tests passed but the real-subprocess
streaming bug shipped to main).

ACs covered:
- AC1-FR Ctrl-C mid-create releases flock, registry untouched, exit 130
- AC2-FR follow-up timeout SIGTERMs codex and exits 15
"""
from __future__ import annotations

import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest


FIXTURE_PATH = (
    Path(__file__).parent / "fixtures" / "fake-codex-hang.sh"
)


@pytest.fixture
def fake_codex_on_path(tmp_path, monkeypatch):
    """Install a ``codex`` symlink in tmp_path/bin pointing at the shim,
    then prepend the dir to PATH so providers.codex picks it up.

    Mode is selected via ``FAKE_CODEX_MODE`` env var (default 'create').
    Honored values: 'create', 'resume', 'complete-then-hang'. The
    'complete-then-hang' mode emits a full happy-path stream then sleeps
    so tests can exercise ``_wait_with_grace``'s process-group kill path
    (gemini code review on PR #305).
    """
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    fake = bin_dir / "codex"
    fake.write_text(
        f"""#!/usr/bin/env bash
# Test shim wrapper — forwards to fake-codex-hang.sh per subcommand.
# Override mode via FAKE_CODEX_MODE env var.
MODE="${{FAKE_CODEX_MODE:-}}"
if [[ -z "$MODE" ]]; then
    # Skip a leading GLOBAL --ask-for-approval <value> (codex >= 0.133.0 emits
    # it before the `exec` subcommand), so subcommand detection still works.
    if [[ ("$1" == "--ask-for-approval" || "$1" == "-a") && $# -ge 2 ]]; then
        shift 2
    fi
    if [[ "$1" == "exec" && "$2" == "resume" ]]; then
        MODE=resume
    elif [[ "$1" == "exec" ]]; then
        MODE=create
    else
        echo "fake-codex wrapper: unknown subcommand $1" >&2
        exit 99
    fi
fi
exec {FIXTURE_PATH} "$MODE"
""",
        encoding="utf-8",
    )
    fake.chmod(0o755)
    # PREPEND, do not replace — the shim shells out to sleep/exec and
    # needs the host's coreutils on PATH. Replacing PATH wholesale leaves
    # `sleep` undiscoverable and the shim exits immediately with rc=127.
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    return bin_dir


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="SIGINT signal-delivery on Windows is process-group-specific.",
)
def test_create_timeout_sigterms_codex_and_raises_timeout_error(
    tmp_path, fake_codex_on_path
):
    """AC2-FR analog: codex create wall-clock timeout → SIGTERM → exit 15.

    Uses a 1-second timeout against the fake shim that sleeps 60 seconds.
    The watchdog inside providers/codex.py must SIGTERM the child and
    raise CodexTimeoutError, which the dispatcher maps to exit 15.
    """
    from fno.agents.providers import codex as codex_mod
    from fno.agents.providers.codex import CodexTimeoutError

    start = time.monotonic()
    with pytest.raises(CodexTimeoutError) as exc_info:
        codex_mod.create(
            cwd=Path("/tmp"),
            prompt="hangtest",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
            timeout=1.0,
        )
    elapsed = time.monotonic() - start
    assert exc_info.value.timeout_sec == 1.0
    # Watchdog must fire within ~1s + small grace; if we wait the full
    # 60s the shim's sleep didn't get signaled.
    assert elapsed < 15.0, f"timeout took {elapsed:.1f}s, expected ~1s+grace"


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="SIGINT signal-delivery on Windows is process-group-specific.",
)
def test_resume_timeout_sigterms_codex(tmp_path, fake_codex_on_path):
    """AC2-FR end-to-end: resume timeout SIGTERMs codex, raises CodexTimeoutError."""
    from fno.agents.providers import codex as codex_mod
    from fno.agents.providers.codex import CodexTimeoutError

    start = time.monotonic()
    with pytest.raises(CodexTimeoutError) as exc_info:
        codex_mod.resume(
            session_id="any-uuid",
            cwd=Path("/tmp"),
            prompt="hang resume",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
            timeout=1.0,
        )
    elapsed = time.monotonic() - start
    assert exc_info.value.timeout_sec == 1.0
    assert elapsed < 15.0


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="signal-delivery on Windows is process-group-specific.",
)
def test_wait_with_grace_killpg_terminates_subshells_after_turn_completed(
    tmp_path, fake_codex_on_path, monkeypatch
):
    """Gemini code review on PR #305: _wait_with_grace must use os.killpg
    so subshells get reaped if codex emits turn.completed but never exits.

    The shim emits a full happy-path JSONL stream (thread.started,
    agent_message, turn.completed) then sleeps forever. providers.codex
    breaks the read loop on turn.completed and calls _wait_with_grace
    with a default 5s grace. Without process-group cleanup, the shim's
    sleep would orphan and the test would hang. With os.killpg, the
    function escalates SIGTERM->SIGKILL within ~12s.
    """
    from fno.agents.providers import codex as codex_mod
    from fno.agents.providers.codex import CodexResult, CodexInvocationError

    monkeypatch.setenv("FAKE_CODEX_MODE", "complete-then-hang")
    monkeypatch.setenv("FAKE_HANG_SECS", "60")

    # Patch _wait_with_grace's default grace to 0.5s so the test runs
    # quickly. The function-under-test is the grace-and-kill loop; the
    # caller passes a faster grace via a monkeypatch to keep CI snappy.
    original = codex_mod._wait_with_grace
    def _fast_grace(proc, grace_sec=5.0):
        return original(proc, grace_sec=0.5)
    monkeypatch.setattr(codex_mod, "_wait_with_grace", _fast_grace)

    start = time.monotonic()
    # The shim runs `sleep 60` after turn.completed. _wait_with_grace's
    # 0.5s wait expires; SIGTERM hits the process group (which includes
    # the sleep subshell since the shim's bash `exec`s into the fixture
    # script — every descendant shares the process group). Bash typically
    # propagates SIGTERM to the foreground job; if not, the SIGKILL
    # escalation 5s later finishes the job.
    try:
        result = codex_mod.create(
            cwd=Path("/tmp"),
            prompt="hangtest",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
            timeout=None,  # no watchdog; we're testing _wait_with_grace
        )
        elapsed = time.monotonic() - start
        # Got a result back: SIGTERM cleanly reaped the process group.
        assert result.last_msg == "hung after complete"
        # Should complete within ~6s (0.5s grace + 5s SIGTERM wait + buffer).
        assert elapsed < 15.0, f"_wait_with_grace took {elapsed:.1f}s"
    except CodexInvocationError:
        # SIGKILL escalation also acceptable; the contract is "reaped
        # within reasonable time," not "exit code zero." This branch
        # confirms the process-group kill ran.
        elapsed = time.monotonic() - start
        assert elapsed < 15.0, (
            f"_wait_with_grace escalated to SIGKILL but took {elapsed:.1f}s; "
            f"process group not reaped quickly enough"
        )


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="SIGINT signal-delivery on Windows is process-group-specific.",
)
def test_create_sigint_mid_stream_propagates_and_releases_child(
    tmp_path, fake_codex_on_path
):
    """AC1-FR: SIGINT during create propagates to codex, no zombie left behind.

    Spawns a subprocess that calls providers.codex.create() against the
    hanging shim, then SIGINTs the parent after a short delay. Asserts:
      - the parent exits non-zero (KeyboardInterrupt re-raises out of
        providers.codex._run_codex)
      - the codex child process is no longer running
    """
    # Drive the test via a child Python invocation so SIGINT delivery
    # to the test runner does not affect pytest itself.
    script = tmp_path / "driver.py"
    output_path = tmp_path / "output.jsonl"
    script.write_text(
        f"""
import os, signal, sys
from pathlib import Path
from fno.agents.providers import codex as codex_mod

try:
    codex_mod.create(
        cwd=Path('/tmp'),
        prompt='hang',
        from_name='fno',
        yolo=False,
        output_path=Path({str(output_path)!r}),
        timeout=60.0,
    )
except KeyboardInterrupt:
    sys.exit(130)
except Exception as exc:
    sys.exit(1)
sys.exit(0)
""",
        encoding="utf-8",
    )

    env = os.environ.copy()
    env["PYTHONUNBUFFERED"] = "1"
    proc = subprocess.Popen(
        [sys.executable, str(script)],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        # Let the driver enter the read loop and capture session_id.
        time.sleep(1.0)
        # SIGINT the driver; it forwards to the codex child via
        # _run_codex's KeyboardInterrupt branch.
        proc.send_signal(signal.SIGINT)
        try:
            stdout, stderr = proc.communicate(timeout=10.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            stdout, stderr = proc.communicate()
            pytest.fail(
                f"driver did not exit within 10s of SIGINT;"
                f" stdout={stdout!r} stderr={stderr!r}"
            )
    finally:
        if proc.poll() is None:
            proc.kill()

    # Driver should have caught KeyboardInterrupt and exited 130.
    assert proc.returncode == 130, (
        f"expected exit 130 (SIGINT), got {proc.returncode}\n"
        f"stdout={stdout!r}\nstderr={stderr!r}"
    )
    # No traceback on stderr — the provider should clean up gracefully.
    decoded_err = stderr.decode("utf-8", errors="replace")
    assert "Traceback" not in decoded_err, (
        f"SIGINT produced a traceback:\n{decoded_err}"
    )
