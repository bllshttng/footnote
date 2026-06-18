"""Tests for the Hermes Agent CLI adapter (Plan A3, ab-39195ebd).

Mirrors test_codex.py because the two adapters share the same
RuntimeAdapter Protocol shape. Differences from Codex:

- spawn_worker invokes ``hermes chat -q "<prompt>"`` (not ``codex exec``).
- In-session detection covers three env vars: CLAUDECODE_SESSION_ID,
  CODEX_SESSION_ID, and the speculative HERMES_SESSION_ID.
- health() uses ``hermes doctor`` (no ``--version`` flag is documented)
  and probes three candidate config-dir paths defensively.
- map_hermes_error walks Plan A's universal text rules first, then a
  Hermes-specific exit-code fallback (1=usage, 2=runtime, 3=auth).
"""
from __future__ import annotations

import os
import subprocess
from unittest import mock

from fno.adapters.hermes import (
    HermesCliAdapter,
    _BODY_EXCERPT_MAX_BYTES,
    _HERMES_DOCTOR_TIMEOUT_SECONDS,
    map_hermes_error,
)
from fno.adapters.providers.error_taxonomy import ErrorClass


# Helper: build a CompletedProcess for hermes doctor stubbing.
def _doctor_proc(returncode: int = 0, stdout: str = "ok", stderr: str = ""):
    return subprocess.CompletedProcess(
        args=["hermes", "doctor"],
        returncode=returncode,
        stdout=stdout,
        stderr=stderr,
    )


# ----------------------------------------------------------------------
# spawn_worker
# ----------------------------------------------------------------------


def test_spawn_worker_happy_path_returns_descriptor(monkeypatch):
    """AC1.1-HP: External spawn invokes `hermes chat -q "<prompt>"` and returns descriptor."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("HERMES_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 4242
    fake_proc.poll.return_value = None  # still running

    seen: dict = {}

    def fake_popen(cmd, **kwargs):
        seen["cmd"] = cmd
        return fake_proc

    with mock.patch("fno.adapters.hermes.subprocess.Popen", side_effect=fake_popen):
        with mock.patch("fno.adapters.hermes.time.sleep"):
            result = HermesCliAdapter().spawn_worker(prompt="echo test")

    assert seen["cmd"] == ["hermes", "chat", "-q", "echo test"]
    assert result["pid"] == 4242
    assert "worker_id" in result
    assert "started_at" in result
    assert result["action"] == "spawned"  # success path discriminator


def test_spawn_worker_in_session_via_claudecode_session_id(monkeypatch):
    """AC1.2-HP: CLAUDECODE_SESSION_ID triggers skill_dispatch_required sentinel."""
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "abc123")
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("HERMES_SESSION_ID", raising=False)

    with mock.patch("fno.adapters.hermes.subprocess.Popen") as mock_popen:
        result = HermesCliAdapter().spawn_worker(prompt="anything")

    assert result["action"] == "skill_dispatch_required"
    assert "worker_id" in result
    assert "next_step" in result
    mock_popen.assert_not_called()


def test_spawn_worker_in_session_via_codex_session_id(monkeypatch):
    """AC1.2-HP: CODEX_SESSION_ID also triggers the in-session sentinel."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.setenv("CODEX_SESSION_ID", "xyz789")
    monkeypatch.delenv("HERMES_SESSION_ID", raising=False)

    with mock.patch("fno.adapters.hermes.subprocess.Popen") as mock_popen:
        result = HermesCliAdapter().spawn_worker(prompt="anything")

    assert result["action"] == "skill_dispatch_required"
    mock_popen.assert_not_called()


def test_spawn_worker_in_session_via_hermes_session_id(monkeypatch):
    """AC1.2-HP: HERMES_SESSION_ID also triggers the in-session sentinel."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.setenv("HERMES_SESSION_ID", "hms-001")

    with mock.patch("fno.adapters.hermes.subprocess.Popen") as mock_popen:
        result = HermesCliAdapter().spawn_worker(prompt="anything")

    assert result["action"] == "skill_dispatch_required"
    mock_popen.assert_not_called()


def test_spawn_worker_empty_string_session_var_still_in_session(monkeypatch):
    """Fail-closed: an env var explicitly set to empty string still counts as in-session.

    A misconfigured outer runner that does `export HERMES_SESSION_ID=""` would
    otherwise shell-spawn a Hermes child, which is doubly dangerous because
    Hermes carries persistent memory by default.
    """
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.setenv("HERMES_SESSION_ID", "")

    with mock.patch("fno.adapters.hermes.subprocess.Popen") as mock_popen:
        result = HermesCliAdapter().spawn_worker(prompt="anything")

    assert result["action"] == "skill_dispatch_required"
    mock_popen.assert_not_called()


def test_spawn_worker_binary_missing(monkeypatch):
    """AC1.3-ERR: FileNotFoundError on Popen surfaces as spawn_failed."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("HERMES_SESSION_ID", raising=False)

    with mock.patch(
        "fno.adapters.hermes.subprocess.Popen",
        side_effect=FileNotFoundError("hermes"),
    ):
        result = HermesCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"
    assert "PATH" in result["error"]


def test_spawn_worker_early_crash_returns_diagnostics(monkeypatch):
    """AC1.4-EDGE: Process that crashes within poll window surfaces stderr."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("HERMES_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 9999
    fake_proc.poll.return_value = 3
    fake_proc.communicate.return_value = (b"", b"hermes setup not complete")

    with mock.patch("fno.adapters.hermes.subprocess.Popen", return_value=fake_proc):
        with mock.patch("fno.adapters.hermes.time.sleep"):
            result = HermesCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"
    assert result["returncode"] == 3
    assert "hermes setup not complete" in result["stderr"]
    assert len(result["stderr"]) <= 500


def test_spawn_worker_rc_zero_within_poll_window_is_early_exit(monkeypatch):
    """A process that exits 0 within the poll window is still a zombie risk."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("HERMES_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 1234
    fake_proc.poll.return_value = 0
    fake_proc.communicate.return_value = (b"early bye\n", b"")

    with mock.patch("fno.adapters.hermes.subprocess.Popen", return_value=fake_proc):
        with mock.patch("fno.adapters.hermes.time.sleep"):
            result = HermesCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"
    assert result.get("early_exit") is True
    assert result["returncode"] == 0


def test_spawn_worker_communicate_timeout_does_not_propagate(monkeypatch):
    """If communicate(timeout=2) hangs, kill the child and still return spawn_failed."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("HERMES_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 5678
    fake_proc.poll.return_value = 2
    fake_proc.communicate.side_effect = [
        subprocess.TimeoutExpired(cmd="hermes", timeout=2),
        (b"", b""),
    ]

    with mock.patch("fno.adapters.hermes.subprocess.Popen", return_value=fake_proc):
        with mock.patch("fno.adapters.hermes.time.sleep"):
            result = HermesCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"
    fake_proc.kill.assert_called_once()


def test_spawn_worker_communicate_timeout_twice_does_not_crash(monkeypatch):
    """Even if communicate hangs after kill, we still return spawn_failed safely."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("HERMES_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 9001
    fake_proc.poll.return_value = 2
    fake_proc.communicate.side_effect = [
        subprocess.TimeoutExpired(cmd="hermes", timeout=2),
        subprocess.TimeoutExpired(cmd="hermes", timeout=2),
    ]

    with mock.patch("fno.adapters.hermes.subprocess.Popen", return_value=fake_proc):
        with mock.patch("fno.adapters.hermes.time.sleep"):
            result = HermesCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"
    assert "communicate timed out" in result["stderr"]


def test_spawn_worker_os_error_surfaces_as_spawn_failed(monkeypatch):
    """OSError (E2BIG, EACCES, etc.) becomes spawn_failed rather than crashing the caller."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("HERMES_SESSION_ID", raising=False)

    with mock.patch(
        "fno.adapters.hermes.subprocess.Popen",
        side_effect=OSError("argument list too long"),
    ):
        result = HermesCliAdapter().spawn_worker(prompt="x" * 100)

    assert result["action"] == "spawn_failed"
    assert "argument list too long" in result["error"]


# ----------------------------------------------------------------------
# create_worktree (delegation)
# ----------------------------------------------------------------------


def test_create_worktree_delegates_to_shared(tmp_path, monkeypatch):
    """create_worktree on the hermes adapter calls _shared.create_worktree."""
    monkeypatch.chdir(tmp_path)
    sentinel = {
        "worktree_path": str(tmp_path / ".fno" / "worktrees" / "abi-x"),
        "branch": "feature/x",
        "status": "created",
    }

    with mock.patch(
        "fno.adapters.hermes._create_worktree", return_value=sentinel
    ) as mocked:
        result = HermesCliAdapter().create_worktree(name="x", base="main")

    mocked.assert_called_once_with(name="x", base="main")
    assert result is sentinel


# ----------------------------------------------------------------------
# call_api
# ----------------------------------------------------------------------


def test_call_api_success_path():
    """Returncode 0 returns immediately with stdout."""
    completed = subprocess.CompletedProcess(
        args=["hermes"], returncode=0, stdout="ok\n", stderr=""
    )
    with mock.patch("fno.adapters.hermes.subprocess.run", return_value=completed):
        result = HermesCliAdapter().call_api(command=["chat", "-q", "hi"])

    assert result["returncode"] == 0
    assert result["stdout"] == "ok\n"
    assert result["stderr"] == ""
    assert result["ok"] is True


def test_call_api_retries_on_sigkill_then_succeeds():
    """Returncode 137 (SIGKILL) triggers retry; success on retry returns rc=0."""
    rc137 = subprocess.CompletedProcess(args=["hermes"], returncode=137, stdout="", stderr="killed")
    rc0 = subprocess.CompletedProcess(args=["hermes"], returncode=0, stdout="ok", stderr="")

    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        side_effect=[rc137, rc0],
    ) as mock_run:
        with mock.patch("fno.adapters.hermes.time.sleep"):
            result = HermesCliAdapter().call_api(command=["chat", "-q", "x"], retries=3)

    assert result["returncode"] == 0
    assert mock_run.call_count == 2


def test_call_api_does_not_retry_on_usage_error():
    """Returncode 1 is non-retryable; only one subprocess.run call fires."""
    rc1 = subprocess.CompletedProcess(
        args=["hermes"], returncode=1, stdout="", stderr="unknown flag"
    )
    with mock.patch(
        "fno.adapters.hermes.subprocess.run", return_value=rc1
    ) as mock_run:
        result = HermesCliAdapter().call_api(command=["chat", "-q", "x"], retries=3)

    assert mock_run.call_count == 1
    assert result["returncode"] == 1
    assert "unknown flag" in result["stderr"]


def test_call_api_binary_missing_returns_127():
    """FileNotFoundError surfaces as returncode 127 (POSIX 'command not found')."""
    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        side_effect=FileNotFoundError("hermes"),
    ):
        result = HermesCliAdapter().call_api(command=["chat", "-q", "x"])

    assert result["returncode"] == 127
    assert "PATH" in result["stderr"]


def test_call_api_retries_on_negative_sigkill_returncode():
    """subprocess.run returns -9 for SIGKILL on POSIX. The retry check must
    normalize negative returncodes to shell-style 128+N before matching
    against _RETRYABLE_EXIT_CODES; otherwise SIGKILL-killed processes
    break out of the retry loop prematurely.

    Gemini-flagged HIGH on PR #249; same pattern as map_hermes_error's
    own normalization, applied at the call_api retry boundary.
    """
    rc_neg9 = subprocess.CompletedProcess(args=["hermes"], returncode=-9, stdout="", stderr="killed")
    rc0 = subprocess.CompletedProcess(args=["hermes"], returncode=0, stdout="ok", stderr="")

    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        side_effect=[rc_neg9, rc0],
    ) as mock_run:
        with mock.patch("fno.adapters.hermes.time.sleep"):
            result = HermesCliAdapter().call_api(command=["chat", "-q", "x"], retries=3)

    assert result["returncode"] == 0
    assert mock_run.call_count == 2


def test_call_api_os_error_returns_structured_error():
    """Permission denied, E2BIG, etc. surface as a structured envelope.

    Gemini-flagged MEDIUM on PR #249. Matches spawn_worker's existing
    OSError envelope so the surface area is consistent.
    """
    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        side_effect=OSError("argument list too long"),
    ):
        result = HermesCliAdapter().call_api(command=["chat", "-q", "x" * 100])

    assert result["returncode"] == -1
    assert "argument list too long" in result["stderr"]
    assert "hermes execution failed" in result["stderr"]
    assert result["stdout"] == ""


def test_call_api_exhausts_retries_returns_last_failure():
    """Retryable code that keeps failing exhausts retries and returns the final rc."""
    rc137 = subprocess.CompletedProcess(args=["hermes"], returncode=137, stdout="", stderr="killed")

    with mock.patch(
        "fno.adapters.hermes.subprocess.run", return_value=rc137
    ) as mock_run:
        with mock.patch("fno.adapters.hermes.time.sleep"):
            result = HermesCliAdapter().call_api(command=["chat", "-q", "x"], retries=2)

    assert mock_run.call_count == 3
    assert result["returncode"] == 137
    assert result["ok"] is False


def test_call_api_retries_exhausted_sets_ok_false():
    """AC-EDGE: call_api that hits non-retryable failure returns ok=False."""
    rc1 = subprocess.CompletedProcess(
        args=["hermes"], returncode=1, stdout="", stderr="usage error"
    )
    with mock.patch("fno.adapters.hermes.subprocess.run", return_value=rc1):
        result = HermesCliAdapter().call_api(command=["chat", "-q", "x"], retries=3)

    assert result["ok"] is False


# ----------------------------------------------------------------------
# spawn_worker action discrimination
# ----------------------------------------------------------------------


def test_spawn_worker_external_success_has_action_spawned(monkeypatch):
    """AC-HP: External spawn success returns action='spawned'."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("HERMES_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 7777
    fake_proc.poll.return_value = None

    with mock.patch("fno.adapters.hermes.subprocess.Popen", return_value=fake_proc):
        with mock.patch("fno.adapters.hermes.time.sleep"):
            result = HermesCliAdapter().spawn_worker(prompt="do something")

    assert result["action"] == "spawned"


def test_spawn_worker_in_session_returns_skill_dispatch_required(monkeypatch):
    """AC-HP: In-session spawn returns action='skill_dispatch_required'."""
    monkeypatch.setenv("HERMES_SESSION_ID", "hms-001")
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    result = HermesCliAdapter().spawn_worker(prompt="anything")

    assert result["action"] == "skill_dispatch_required"


def test_spawn_worker_missing_binary_returns_spawn_failed_action(monkeypatch):
    """AC-ERR: Missing hermes binary returns action='spawn_failed'."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("HERMES_SESSION_ID", raising=False)

    with mock.patch(
        "fno.adapters.hermes.subprocess.Popen",
        side_effect=FileNotFoundError("hermes"),
    ):
        result = HermesCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"


# ----------------------------------------------------------------------
# health
# ----------------------------------------------------------------------


def _patch_candidate_paths(
    monkeypatch,
    present: list[str],
    stale: list[str] | None = None,
):
    """Make os.path.isdir + os.path.lexists respond truthy per the given sets.

    - paths in `present`: isdir=True, lexists=True (real dirs)
    - paths in `stale`: isdir=False, lexists=True (file or broken symlink)
    - everything else: both False
    """
    expanded_present = {os.path.expanduser(p) for p in present}
    expanded_stale = {os.path.expanduser(p) for p in (stale or [])}

    def fake_isdir(p):
        return p in expanded_present

    def fake_lexists(p):
        return p in expanded_present or p in expanded_stale

    monkeypatch.setattr("fno.adapters.hermes.os.path.isdir", fake_isdir)
    monkeypatch.setattr("fno.adapters.hermes.os.path.lexists", fake_lexists)


def test_health_ok_with_xdg_config_dir(monkeypatch):
    """AC1.6-HP: hermes doctor exit 0 + ~/.config/hermes present -> ok=True."""
    _patch_candidate_paths(monkeypatch, ["~/.config/hermes"])

    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        return_value=_doctor_proc(returncode=0, stdout="hermes 2026.4.8 ready\n"),
    ):
        health = HermesCliAdapter().health()

    assert health.ok is True
    assert health.details["doctor_exit"] == 0
    assert health.details["config_dir"].endswith(".config/hermes")
    assert "2026.4.8" in health.details["doctor_stdout"]


def test_health_ok_with_dot_hermes_config_dir(monkeypatch):
    """Health passes when ~/.hermes is the present candidate."""
    _patch_candidate_paths(monkeypatch, ["~/.hermes"])

    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        return_value=_doctor_proc(returncode=0),
    ):
        health = HermesCliAdapter().health()

    assert health.ok is True
    assert health.details["config_dir"].endswith(".hermes")


def test_health_binary_missing_returns_specific_error(monkeypatch):
    """Missing binary -> ok=False with PATH error message."""
    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        side_effect=FileNotFoundError("hermes"),
    ):
        health = HermesCliAdapter().health()

    assert health.ok is False
    assert "PATH" in health.details["reason"]


def test_health_doctor_exit_nonzero(monkeypatch):
    """hermes doctor exits non-zero -> ok=False with exit code in error."""
    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        return_value=_doctor_proc(returncode=2, stderr="missing config"),
    ):
        health = HermesCliAdapter().health()

    assert health.ok is False
    assert "exited 2" in health.details["reason"]


def test_health_no_config_dir_lists_candidate_paths(monkeypatch):
    """AC1.7-ERR: doctor passes but no config dir -> ok=False with candidate paths in error."""
    _patch_candidate_paths(monkeypatch, [])  # none present, none stale

    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        return_value=_doctor_proc(returncode=0),
    ):
        health = HermesCliAdapter().health()

    assert health.ok is False
    msg = health.details["reason"]
    assert "config dir not found" in msg
    assert "~/.config/hermes" in msg
    assert "hermes setup" in msg


def test_health_config_path_exists_but_not_a_directory(monkeypatch):
    """A candidate path that exists as a file (or broken symlink) surfaces a distinct error.

    Without this distinction, an operator who accidentally created
    `~/.config/hermes` as a stray file (or whose `~/.hermes` is a broken
    symlink) sees the same "run hermes setup" hint that misses the real
    problem.
    """
    _patch_candidate_paths(monkeypatch, present=[], stale=["~/.config/hermes"])

    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        return_value=_doctor_proc(returncode=0),
    ):
        health = HermesCliAdapter().health()

    assert health.ok is False
    msg = health.details["reason"]
    assert "not a directory" in msg
    # The error message includes the expanded path so the operator sees the
    # exact filesystem location that needs cleanup.
    assert os.path.expanduser("~/.config/hermes") in msg


def test_health_os_error_returns_structured_error(monkeypatch):
    """hermes binary present but unexecutable (permission denied, etc.) -> ok=False.

    Gemini-flagged MEDIUM on PR #249. Without this catch, an OSError from
    subprocess.run would propagate to the caller and crash whatever code
    expected health() to return an AdapterHealth.
    """
    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        side_effect=OSError("[Errno 13] Permission denied"),
    ):
        health = HermesCliAdapter().health()

    assert health.ok is False
    assert "execution failed" in health.details["reason"]
    assert "Permission denied" in health.details["reason"]
    assert health.details["doctor_exit"] is None
    assert health.details["doctor_stdout"] is None
    assert health.details["doctor_stderr"] is None


def test_health_doctor_timeout(monkeypatch):
    """AC1.8-EDGE: hermes doctor hangs beyond _HERMES_DOCTOR_TIMEOUT_SECONDS -> ok=False."""
    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        side_effect=subprocess.TimeoutExpired(
            cmd="hermes doctor", timeout=_HERMES_DOCTOR_TIMEOUT_SECONDS,
        ),
    ):
        health = HermesCliAdapter().health()

    assert health.ok is False
    assert "timed out" in health.details["reason"]


def test_health_truncates_long_doctor_stdout(monkeypatch):
    """A massive doctor stdout is truncated in details."""
    _patch_candidate_paths(monkeypatch, ["~/.config/hermes"])

    huge = "x" * 2000
    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        return_value=_doctor_proc(returncode=0, stdout=huge),
    ):
        health = HermesCliAdapter().health()

    assert health.ok is True
    assert len(health.details["doctor_stdout"]) <= 500


def test_health_early_return_paths_include_doctor_keys(monkeypatch):
    """All early-return paths populate doctor_exit / doctor_stdout / doctor_stderr as None.

    AdapterHealth.details is a bag-type dict so a downstream consumer
    that reads `details["doctor_exit"]` on a binary-missing health report
    would otherwise hit a KeyError. Keeping the key set stable closes
    that gap without changing the base Protocol shape.
    """
    # FileNotFoundError path
    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        side_effect=FileNotFoundError("hermes"),
    ):
        health = HermesCliAdapter().health()
    assert health.ok is False
    assert health.details["doctor_exit"] is None
    assert health.details["doctor_stdout"] is None
    assert health.details["doctor_stderr"] is None

    # TimeoutExpired path
    with mock.patch(
        "fno.adapters.hermes.subprocess.run",
        side_effect=subprocess.TimeoutExpired(
            cmd="hermes doctor", timeout=_HERMES_DOCTOR_TIMEOUT_SECONDS,
        ),
    ):
        health = HermesCliAdapter().health()
    assert health.ok is False
    assert health.details["doctor_exit"] is None
    assert health.details["doctor_stdout"] is None
    assert health.details["doctor_stderr"] is None


# ----------------------------------------------------------------------
# map_hermes_error
# ----------------------------------------------------------------------


def test_map_hermes_error_rate_limit_text_rule():
    """AC3.1-HP: stderr 'rate limit exceeded' -> PROVIDER_4XX_QUOTA + swap."""
    err = map_hermes_error(returncode=2, stderr="rate limit exceeded")
    assert err.error_class is ErrorClass.PROVIDER_4XX_QUOTA
    assert err.triggers_swap is True
    assert err.raw_exit_code == 2
    assert err.raw_status is None
    assert err.body_excerpt == "rate limit exceeded"


def test_map_hermes_error_sigkill_fallback():
    """AC3.2-EDGE: returncode 137 with empty stderr -> PARSER_ERROR + no swap."""
    err = map_hermes_error(returncode=137, stderr="")
    assert err.error_class is ErrorClass.PARSER_ERROR
    assert err.triggers_swap is False
    assert err.raw_exit_code == 137


def test_map_hermes_error_exit_2_unavailable_hint():
    """Exit 2 stderr containing 'unavailable' triggers PROVIDER_5XX + swap.

    Note: the plan's AC3.3 anticipated a 'overloaded' rule scenario but the
    universal 'overloaded' rule fires BEFORE the exit-code fallback and
    maps to PROVIDER_4XX_QUOTA. The exit-2 server-side-hint fallback only
    fires when the universal rules return None. This test pins the
    documented exit-code fallback for 'unavailable' specifically.
    """
    err = map_hermes_error(returncode=2, stderr="upstream provider unavailable")
    assert err.error_class is ErrorClass.PROVIDER_5XX
    assert err.triggers_swap is True


def test_map_hermes_error_auth_exit_3():
    """AC3.4-EDGE: returncode 3 with auth-shaped stderr -> PROVIDER_4XX_AUTH + swap."""
    err = map_hermes_error(returncode=3, stderr="authentication failed")
    assert err.error_class is ErrorClass.PROVIDER_4XX_AUTH
    assert err.triggers_swap is True


def test_map_hermes_error_exit_0_defensive():
    """AC3.5-FR: returncode 0 with empty stderr -> UNKNOWN + no swap."""
    err = map_hermes_error(returncode=0, stderr="")
    assert err.error_class is ErrorClass.UNKNOWN
    assert err.triggers_swap is False
    assert err.raw_exit_code == 0


def test_map_hermes_error_body_excerpt_truncated():
    """stderr longer than _BODY_EXCERPT_MAX_BYTES is truncated in body_excerpt."""
    long_err = "x" * 500
    err = map_hermes_error(returncode=1, stderr=long_err)
    assert len(err.body_excerpt) == _BODY_EXCERPT_MAX_BYTES


def test_map_hermes_error_overloaded_text_rule():
    """'overloaded' is a Plan A text rule -> PROVIDER_4XX_QUOTA + swap."""
    err = map_hermes_error(returncode=2, stderr="model overloaded, try later")
    assert err.error_class is ErrorClass.PROVIDER_4XX_QUOTA
    assert err.triggers_swap is True


def test_map_hermes_error_no_credentials_maps_to_auth():
    """'no credentials' Plan A text rule maps to PROVIDER_4XX_AUTH + swap."""
    err = map_hermes_error(returncode=1, stderr="no credentials configured for openai")
    assert err.error_class is ErrorClass.PROVIDER_4XX_AUTH
    assert err.triggers_swap is True


def test_map_hermes_error_usage_exit_1_no_hint_is_parser_error():
    """Exit 1 with stderr that doesn't match any rule -> PARSER_ERROR."""
    err = map_hermes_error(returncode=1, stderr="unknown subcommand 'nonsense'")
    assert err.error_class is ErrorClass.PARSER_ERROR
    assert err.triggers_swap is False


def test_map_hermes_error_runtime_exit_2_no_hint_is_unknown():
    """Exit 2 with stderr that doesn't match any rule or hint -> UNKNOWN."""
    err = map_hermes_error(returncode=2, stderr="unexpected output 'foo'")
    assert err.error_class is ErrorClass.UNKNOWN
    assert err.triggers_swap is False


def test_map_hermes_error_sigterm_is_parser_error():
    """SIGTERM (143) maps to PARSER_ERROR, no swap."""
    err = map_hermes_error(returncode=143, stderr="")
    assert err.error_class is ErrorClass.PARSER_ERROR
    assert err.triggers_swap is False


def test_map_hermes_error_timeout_is_parser_error():
    """coreutils timeout exit (124) maps to PARSER_ERROR."""
    err = map_hermes_error(returncode=124, stderr="")
    assert err.error_class is ErrorClass.PARSER_ERROR
    assert err.triggers_swap is False


def test_map_hermes_error_unknown_exit_is_unknown():
    """An unknown exit code (e.g. 99) without any text-rule match -> UNKNOWN, no swap."""
    err = map_hermes_error(returncode=99, stderr="some unmapped failure")
    assert err.error_class is ErrorClass.UNKNOWN
    assert err.triggers_swap is False
    assert err.raw_exit_code == 99


def test_map_hermes_error_negative_sigkill_normalizes_to_137():
    """Python subprocess returns -9 for SIGKILL; normalize to shell-style 137 -> PARSER_ERROR."""
    err = map_hermes_error(returncode=-9, stderr="")
    assert err.error_class is ErrorClass.PARSER_ERROR
    assert err.triggers_swap is False
    assert err.raw_exit_code == 137


def test_map_hermes_error_negative_sigterm_normalizes_to_143():
    """Python subprocess returns -15 for SIGTERM; normalize to 143 -> PARSER_ERROR."""
    err = map_hermes_error(returncode=-15, stderr="")
    assert err.error_class is ErrorClass.PARSER_ERROR
    assert err.triggers_swap is False
    assert err.raw_exit_code == 143


def test_map_hermes_error_non_int_returncode_is_unknown_not_raise():
    """Non-int returncode is a programmer error but the helper must not raise."""
    err = map_hermes_error(returncode="oops", stderr="")  # type: ignore[arg-type]
    assert err.error_class is ErrorClass.UNKNOWN
    assert err.triggers_swap is False
    assert err.raw_exit_code is None


def test_map_hermes_error_clamps_huge_stderr_blob():
    """A 200KB stderr blob is truncated to _STDERR_MAX_BYTES before processing."""
    huge = "x" * 200_000
    err = map_hermes_error(returncode=1, stderr=huge)
    assert len(err.body_excerpt) == _BODY_EXCERPT_MAX_BYTES


# ----------------------------------------------------------------------
# Adapter-name invariant
# ----------------------------------------------------------------------


def test_adapter_name_is_hermes():
    """name is 'hermes' (matches the provider-record `cli:` value)."""
    assert HermesCliAdapter().name == "hermes"


def test_adapter_implements_runtime_protocol():
    """Structural Protocol check - all four primitives present and callable."""
    adapter = HermesCliAdapter()
    assert callable(adapter.spawn_worker)
    assert callable(adapter.create_worktree)
    assert callable(adapter.call_api)
    assert callable(adapter.health)
