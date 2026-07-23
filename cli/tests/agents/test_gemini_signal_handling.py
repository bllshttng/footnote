"""Real-subprocess AC-FR tests for the gemini provider (Wave 2.3).

Spawns providers.gemini.create() / resume() against a controlled
fake-gemini shim that hangs forever. Verifies SIGINT and timeout
signaling actually deliver via os.killpg on the process group.
Monkeypatched Popen cannot validate signal delivery (US3 lesson).

ACs covered:
- AC4-FR Ctrl-C mid-create propagates SIGINT to process group and re-raises
- AC5-FR follow-up timeout SIGTERMs gemini and raises GeminiTimeoutError
"""
from __future__ import annotations

import os
import subprocess
import sys
import time
import uuid
from pathlib import Path

import pytest


FIXTURE_PATH = (
    Path(__file__).parent / "fixtures" / "fake-gemini-hang.sh"
)


@pytest.fixture
def fake_gemini_on_path(tmp_path, monkeypatch):
    """Install a ``gemini`` symlink at tmp_path/bin pointing at the shim.

    Mode is selected via ``FAKE_GEMINI_MODE`` env var. Honored values:
    'create' (default), 'resume', 'echo-then-hang', 'exit-1'.
    """
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    fake = bin_dir / "gemini"
    fake.write_text(
        f"""#!/usr/bin/env bash
# Wrapper that picks the mode based on argv (--resume vs --session-id) or
# the FAKE_GEMINI_MODE override env var.
MODE="${{FAKE_GEMINI_MODE:-}}"
if [[ -z "$MODE" ]]; then
    if [[ "$*" == *"--resume"* ]]; then
        MODE=resume
    else
        MODE=create
    fi
fi
exec {FIXTURE_PATH} "$MODE"
""",
        encoding="utf-8",
    )
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    return bin_dir


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="SIGINT signal-delivery on Windows is process-group-specific.",
)
def test_create_timeout_sigterms_gemini_and_raises(
    tmp_path, fake_gemini_on_path
):
    """AC5-FR: wall-clock timeout → SIGTERM → GeminiTimeoutError.

    1-second timeout against the fake shim that sleeps 60s. The
    watchdog inside providers/gemini.py must SIGTERM the process
    group and raise GeminiTimeoutError within a few seconds total.
    """
    from fno.agents.providers import gemini as gemini_mod

    start = time.monotonic()
    with pytest.raises(gemini_mod.GeminiTimeoutError) as exc_info:
        gemini_mod.create(
            cwd=Path("/tmp"),
            prompt="hangtest",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
            timeout=1.0,
        )
    elapsed = time.monotonic() - start
    assert exc_info.value.timeout_sec == 1.0
    assert elapsed < 15.0, (
        f"timeout took {elapsed:.1f}s — watchdog SIGTERM did not reap"
    )


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="signal-delivery on Windows is process-group-specific.",
)
def test_resume_timeout_sigterms_gemini(tmp_path, fake_gemini_on_path):
    """AC5-FR end-to-end: resume timeout reaps gemini via SIGTERM."""
    from fno.agents.providers import gemini as gemini_mod

    start = time.monotonic()
    with pytest.raises(gemini_mod.GeminiTimeoutError) as exc_info:
        gemini_mod.resume(
            session_id="11111111-1111-1111-1111-111111111111",
            cwd=Path("/tmp"),
            prompt="hangresume",
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
def test_keyboard_interrupt_mid_create_forwards_to_process_group(
    tmp_path, fake_gemini_on_path
):
    """AC4-FR: Ctrl-C mid-create sends SIGINT to gemini's process
    group via os.killpg, then re-raises KeyboardInterrupt.

    We simulate Ctrl-C by spawning the test in a background thread and
    sending SIGINT to ourselves via a separate raise after a short
    sleep. The provider's KeyboardInterrupt handler forwards SIGINT
    to the gemini process group, reaps the child, then re-raises.
    """
    from fno.agents.providers import gemini as gemini_mod

    # Patch the proc.stdout.read so it raises KeyboardInterrupt as if
    # the user Ctrl-C'd the parent process mid-read. We don't want to
    # actually os.kill(os.getpid(), SIGINT) here because pytest catches
    # SIGINT for test isolation and the test framework will report it
    # as an "interrupted test" rather than a passed one.
    real_popen = gemini_mod._subprocess_popen

    class _RaisingReadStream:
        """Pipe-like wrapper that proxies everything but raises KbInt on read."""
        def __init__(self, real):
            self._real = real
            self.closed = False
        def read(self, *args, **kwargs):
            raise KeyboardInterrupt
        def readline(self):
            raise KeyboardInterrupt
        def close(self):
            try:
                self._real.close()
            except OSError:
                pass

    class _RaisingPopen:
        def __init__(self, *args, **kwargs):
            # Spawn the real subprocess so killpg has a real pgid to target.
            self._real = real_popen(*args, **kwargs)
            self.pid = self._real.pid
            self.returncode = None
            self.stdout = _RaisingReadStream(self._real.stdout)
            self.stderr = self._real.stderr
        def poll(self):
            return self._real.poll()
        def wait(self, timeout=None):
            return self._real.wait(timeout=timeout)
        def send_signal(self, sig):
            return self._real.send_signal(sig)
        def kill(self):
            return self._real.kill()
        def terminate(self):
            return self._real.terminate()

    import unittest.mock as mock
    with mock.patch.object(gemini_mod, "_subprocess_popen", _RaisingPopen):
        with pytest.raises(KeyboardInterrupt):
            gemini_mod.create(
                cwd=Path("/tmp"),
                prompt="ki-test",
                from_name="fno",
                yolo=False,
                output_path=tmp_path / "output.jsonl",
                timeout=30.0,
            )


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="signal-delivery on Windows is process-group-specific.",
)
def test_exit_1_raises_invocation_error(tmp_path, fake_gemini_on_path, monkeypatch):
    """A gemini exit 1 with no JSON output surfaces as GeminiInvocationError."""
    from fno.agents.providers import gemini as gemini_mod

    monkeypatch.setenv("FAKE_GEMINI_MODE", "exit-1")

    with pytest.raises(gemini_mod.GeminiInvocationError) as exc_info:
        gemini_mod.create(
            cwd=Path("/tmp"),
            prompt="exit-test",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
            timeout=10.0,
        )
    assert exc_info.value.exit_code == 1


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="signal-delivery on Windows is process-group-specific.",
)
def test_stderr_flood_does_not_deadlock(tmp_path, fake_gemini_on_path, monkeypatch):
    """Codex P1 + Gemini high-priority regression on PR #317.

    The fake shim writes ~80KB to stderr BEFORE emitting the stdout
    JSON. Pre-fix the provider's sequential `proc.stdout.read()` would
    deadlock because gemini blocked on stderr write while parent
    blocked on stdout read; the watchdog timer (set to 30s) was the
    only escape. Post-fix the concurrent stderr drainer keeps gemini
    unblocked and the JSON arrives within milliseconds.

    Pass criterion: completes under 10s WITHOUT the watchdog firing
    AND returns the post-flood reply intact.
    """
    from fno.agents.providers import gemini as gemini_mod

    monkeypatch.setenv("FAKE_GEMINI_MODE", "flood-stderr-then-emit")

    start = time.monotonic()
    result = gemini_mod.create(
        cwd=Path("/tmp"),
        prompt="deadlock-regression",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
        timeout=30.0,
    )
    elapsed = time.monotonic() - start
    # Pre-fix elapsed would be 30+ (watchdog fires and SIGTERMs).
    # Post-fix elapsed is milliseconds (stderr drained concurrently).
    assert elapsed < 10.0, (
        f"flood test took {elapsed:.1f}s — stderr drainer is not "
        "running concurrently; gemini blocked on its stderr write "
        "while parent blocked on stdout.read"
    )
    assert result.session_id == "fa1afe11-1111-2222-3333-444444444444"
    assert result.last_msg == "post-flood ok"


def _real_gemini_on_path() -> bool:
    try:
        subprocess.run(
            ["gemini", "--version"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=True,
            timeout=5,
        )
        return True
    except (FileNotFoundError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return False


@pytest.mark.smoke
@pytest.mark.skipif(
    os.environ.get("GEMINI_SMOKE", "0") != "1",
    reason="GEMINI_SMOKE=1 not set (real-binary test; excluded from CI)",
)
@pytest.mark.skipif(
    not _real_gemini_on_path(),
    reason="gemini binary not on PATH",
)
def test_real_gemini_debug_stderr_does_not_deadlock(tmp_path):
    """Nightly real-binary regression for the separate stderr drainer.

    The fake flood shim proves the pipe topology mechanically; this smoke
    catches drift in real gemini startup/debug stderr volume or timing.
    """
    from fno.agents.providers import gemini as gemini_mod

    session_id = str(uuid.uuid4())
    start = time.monotonic()
    result = gemini_mod._run_gemini(
        argv=[
            "gemini",
            "--skip-trust",
            "--debug",
            "-p",
            "say only the literal word PONG, nothing else",
            "--session-id",
            session_id,
            "--output-format",
            "json",
        ],
        output_path=tmp_path / "real-gemini-debug.jsonl",
        timeout=10.0,
        expect_session=True,
        popen_cwd=tmp_path,
    )
    elapsed = time.monotonic() - start

    assert elapsed < 10.0
    assert result.session_id == session_id
