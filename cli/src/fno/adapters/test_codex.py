"""Tests for the Codex CLI adapter."""
from __future__ import annotations

import subprocess
from unittest import mock

import pytest

from fno.adapters.codex import (
    CodexCliAdapter,
    _version_at_least,
    map_codex_error,
)
from fno.adapters.providers.error_taxonomy import (
    ErrorClass,
)


# ----------------------------------------------------------------------
# spawn_worker
# ----------------------------------------------------------------------


def test_spawn_worker_happy_path_returns_descriptor(monkeypatch):
    """AC2.1-HP: External spawn invokes `codex exec [PROMPT]` and returns worker_id/pid/started_at."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 4242
    fake_proc.poll.return_value = None  # still running

    seen: dict = {}

    def fake_popen(cmd, **kwargs):
        seen["cmd"] = cmd
        return fake_proc

    with mock.patch("fno.adapters.codex.subprocess.Popen", side_effect=fake_popen):
        with mock.patch("fno.adapters.codex.time.sleep"):
            result = CodexCliAdapter().spawn_worker(prompt="echo test")

    assert seen["cmd"] == ["codex", "exec", "echo test"]
    assert result["pid"] == 4242
    assert "worker_id" in result
    assert "started_at" in result
    assert result["action"] == "spawned"  # success path discriminator


def test_spawn_worker_in_session_via_claudecode_session_id(monkeypatch):
    """AC2.2-HP: CLAUDECODE_SESSION_ID triggers the skill_dispatch_required sentinel."""
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "abc123")
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    with mock.patch("fno.adapters.codex.subprocess.Popen") as mock_popen:
        result = CodexCliAdapter().spawn_worker(prompt="anything")

    assert result["action"] == "skill_dispatch_required"
    assert "worker_id" in result
    assert "next_step" in result
    mock_popen.assert_not_called()


def test_spawn_worker_in_session_via_codex_session_id(monkeypatch):
    """AC2.3-HP: CODEX_SESSION_ID also triggers the in-session sentinel."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.setenv("CODEX_SESSION_ID", "xyz789")

    with mock.patch("fno.adapters.codex.subprocess.Popen") as mock_popen:
        result = CodexCliAdapter().spawn_worker(prompt="anything")

    assert result["action"] == "skill_dispatch_required"
    mock_popen.assert_not_called()


def test_spawn_worker_binary_missing(monkeypatch):
    """AC2.4-ERR: FileNotFoundError on Popen surfaces as spawn_failed."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    with mock.patch(
        "fno.adapters.codex.subprocess.Popen",
        side_effect=FileNotFoundError("codex"),
    ):
        result = CodexCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"
    assert "PATH" in result["error"]


def test_spawn_worker_early_crash_returns_diagnostics(monkeypatch):
    """AC2.5-EDGE: Process that crashes within poll window surfaces stderr in spawn_failed."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 9999
    fake_proc.poll.return_value = 2
    fake_proc.communicate.return_value = (b"", b"auth expired: please reauth")

    with mock.patch("fno.adapters.codex.subprocess.Popen", return_value=fake_proc):
        with mock.patch("fno.adapters.codex.time.sleep"):
            result = CodexCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"
    assert result["returncode"] == 2
    assert "auth expired" in result["stderr"]
    assert len(result["stderr"]) <= 500


def test_spawn_worker_rc_zero_within_poll_window_is_early_exit(monkeypatch):
    """A process that exits 0 within the poll window is still a zombie risk; surface as spawn_failed."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 1234
    fake_proc.poll.return_value = 0
    fake_proc.communicate.return_value = (b"early bye\n", b"")

    with mock.patch("fno.adapters.codex.subprocess.Popen", return_value=fake_proc):
        with mock.patch("fno.adapters.codex.time.sleep"):
            result = CodexCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"
    assert result.get("early_exit") is True
    assert result["returncode"] == 0


def test_spawn_worker_communicate_timeout_does_not_propagate(monkeypatch):
    """If communicate(timeout=2) hangs, kill the child and still return spawn_failed."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 5678
    fake_proc.poll.return_value = 2
    # First communicate call raises; second (after kill) returns empty.
    fake_proc.communicate.side_effect = [
        subprocess.TimeoutExpired(cmd="codex", timeout=2),
        (b"", b""),
    ]

    with mock.patch("fno.adapters.codex.subprocess.Popen", return_value=fake_proc):
        with mock.patch("fno.adapters.codex.time.sleep"):
            result = CodexCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"
    fake_proc.kill.assert_called_once()


def test_spawn_worker_communicate_timeout_twice_does_not_crash(monkeypatch):
    """Even if communicate hangs after kill, we still return spawn_failed safely."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 9001
    fake_proc.poll.return_value = 2
    fake_proc.communicate.side_effect = [
        subprocess.TimeoutExpired(cmd="codex", timeout=2),
        subprocess.TimeoutExpired(cmd="codex", timeout=2),
    ]

    with mock.patch("fno.adapters.codex.subprocess.Popen", return_value=fake_proc):
        with mock.patch("fno.adapters.codex.time.sleep"):
            result = CodexCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"
    assert "communicate timed out" in result["stderr"]


def test_spawn_worker_os_error_surfaces_as_spawn_failed(monkeypatch):
    """OSError (e.g., E2BIG, EACCES) becomes spawn_failed rather than crashing the caller."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    with mock.patch(
        "fno.adapters.codex.subprocess.Popen",
        side_effect=OSError("argument list too long"),
    ):
        result = CodexCliAdapter().spawn_worker(prompt="x" * 100)

    assert result["action"] == "spawn_failed"
    assert "argument list too long" in result["error"]


# ----------------------------------------------------------------------
# create_worktree (delegation)
# ----------------------------------------------------------------------


def test_create_worktree_delegates_to_shared(tmp_path, monkeypatch):
    """create_worktree on the codex adapter calls _shared.create_worktree."""
    monkeypatch.chdir(tmp_path)
    expected_path = str(tmp_path / ".fno" / "worktrees" / "fno-x")
    sentinel = {
        "worktree_path": expected_path,
        "branch": "feature/x",
        "status": "created",
    }

    with mock.patch(
        "fno.adapters.codex._create_worktree", return_value=sentinel
    ) as mocked:
        result = CodexCliAdapter().create_worktree(name="x", base="main")

    mocked.assert_called_once_with(name="x", base="main")
    assert result is sentinel


# ----------------------------------------------------------------------
# call_api
# ----------------------------------------------------------------------


def test_call_api_success_path():
    """Returncode 0 returns immediately with stdout."""
    completed = subprocess.CompletedProcess(
        args=["codex"], returncode=0, stdout="ok\n", stderr=""
    )
    with mock.patch("fno.adapters.codex.subprocess.run", return_value=completed):
        result = CodexCliAdapter().call_api(command=["exec", "--model", "gpt", "hi"])

    assert result["returncode"] == 0
    assert result["stdout"] == "ok\n"
    assert result["stderr"] == ""
    assert result["ok"] is True


def test_call_api_retries_on_sigkill_then_succeeds():
    """Returncode 137 (SIGKILL) triggers retry; success on retry returns rc=0."""
    rc137 = subprocess.CompletedProcess(args=["codex"], returncode=137, stdout="", stderr="killed")
    rc0 = subprocess.CompletedProcess(args=["codex"], returncode=0, stdout="ok", stderr="")

    with mock.patch(
        "fno.adapters.codex.subprocess.run",
        side_effect=[rc137, rc0],
    ) as mock_run:
        with mock.patch("fno.adapters.codex.time.sleep"):
            result = CodexCliAdapter().call_api(command=["exec", "x"], retries=3)

    assert result["returncode"] == 0
    assert mock_run.call_count == 2


def test_call_api_does_not_retry_on_usage_error():
    """Returncode 1 is non-retryable; only one subprocess.run call fires."""
    rc1 = subprocess.CompletedProcess(
        args=["codex"], returncode=1, stdout="", stderr="invalid flag"
    )
    with mock.patch(
        "fno.adapters.codex.subprocess.run", return_value=rc1
    ) as mock_run:
        result = CodexCliAdapter().call_api(command=["exec", "x"], retries=3)

    assert mock_run.call_count == 1
    assert result["returncode"] == 1
    assert "invalid flag" in result["stderr"]


def test_call_api_binary_missing_returns_127():
    """FileNotFoundError surfaces as returncode 127 (POSIX 'command not found')."""
    with mock.patch(
        "fno.adapters.codex.subprocess.run",
        side_effect=FileNotFoundError("codex"),
    ):
        result = CodexCliAdapter().call_api(command=["exec", "x"])

    assert result["returncode"] == 127
    assert "PATH" in result["stderr"]


def test_call_api_exhausts_retries_returns_last_failure():
    """Retryable code that keeps failing exhausts retries and returns the final rc."""
    rc137 = subprocess.CompletedProcess(args=["codex"], returncode=137, stdout="", stderr="killed")

    with mock.patch(
        "fno.adapters.codex.subprocess.run", return_value=rc137
    ) as mock_run:
        with mock.patch("fno.adapters.codex.time.sleep"):
            result = CodexCliAdapter().call_api(command=["exec", "x"], retries=2)

    # retries=2 means up to 3 attempts; all fail
    assert mock_run.call_count == 3
    assert result["returncode"] == 137
    assert result["ok"] is False


def test_call_api_retries_exhausted_sets_ok_false():
    """AC-EDGE: call_api that exhausts all retries returns ok=False."""
    rc1 = subprocess.CompletedProcess(
        args=["codex"], returncode=1, stdout="", stderr="perm error"
    )
    with mock.patch("fno.adapters.codex.subprocess.run", return_value=rc1):
        result = CodexCliAdapter().call_api(command=["exec", "x"], retries=3)

    assert result["ok"] is False


# ----------------------------------------------------------------------
# spawn_worker action discrimination
# ----------------------------------------------------------------------


def test_spawn_worker_external_success_has_action_spawned(monkeypatch):
    """AC-HP: External spawn success returns action='spawned'."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    fake_proc = mock.MagicMock()
    fake_proc.pid = 7777
    fake_proc.poll.return_value = None

    with mock.patch("fno.adapters.codex.subprocess.Popen", return_value=fake_proc):
        with mock.patch("fno.adapters.codex.time.sleep"):
            result = CodexCliAdapter().spawn_worker(prompt="do something")

    assert result["action"] == "spawned"


def test_spawn_worker_in_session_returns_skill_dispatch_required(monkeypatch):
    """AC-HP: In-session spawn returns action='skill_dispatch_required'."""
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "abc")
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    result = CodexCliAdapter().spawn_worker(prompt="anything")

    assert result["action"] == "skill_dispatch_required"


def test_spawn_worker_missing_binary_returns_spawn_failed_action(monkeypatch):
    """AC-ERR: Missing codex binary returns action='spawn_failed'."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    with mock.patch(
        "fno.adapters.codex.subprocess.Popen",
        side_effect=FileNotFoundError("codex"),
    ):
        result = CodexCliAdapter().spawn_worker(prompt="hi")

    assert result["action"] == "spawn_failed"


# ----------------------------------------------------------------------
# health
# ----------------------------------------------------------------------


def _ver_proc(stdout: str, returncode: int = 0):
    return subprocess.CompletedProcess(
        args=["codex", "--version"], returncode=returncode, stdout=stdout, stderr=""
    )


def test_health_ok_with_oauth(monkeypatch, tmp_path):
    """codex on PATH + recent version + auth.json present -> ok=True with auth_status=oauth."""
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)
    fake_home = tmp_path / "codex_home"
    fake_home.mkdir()
    (fake_home / "auth.json").write_text("{}")
    monkeypatch.setattr(
        "fno.adapters.codex.os.path.expanduser",
        lambda p: str(fake_home) if p == "~/.codex" else p,
    )

    with mock.patch(
        "fno.adapters.codex.subprocess.run",
        return_value=_ver_proc("codex-cli 0.117.0\n"),
    ):
        health = CodexCliAdapter().health()

    assert health.ok is True
    assert health.details["auth_status"] == "oauth"
    assert "0.117.0" in health.details["version"]


def test_health_ok_with_api_key(monkeypatch, tmp_path):
    """codex on PATH + recent version + OPENAI_API_KEY set -> ok=True with auth_status=api_key."""
    monkeypatch.setenv("OPENAI_API_KEY", "sk-test")
    fake_home = tmp_path / "no_codex"  # does not exist
    monkeypatch.setattr(
        "fno.adapters.codex.os.path.expanduser",
        lambda p: str(fake_home) if p == "~/.codex" else p,
    )

    with mock.patch(
        "fno.adapters.codex.subprocess.run",
        return_value=_ver_proc("0.117.0"),
    ):
        health = CodexCliAdapter().health()

    assert health.ok is True
    assert health.details["auth_status"] == "api_key"


def test_health_binary_missing_returns_specific_error(monkeypatch):
    """Missing binary -> ok=False with PATH error message."""
    with mock.patch(
        "fno.adapters.codex.subprocess.run",
        side_effect=FileNotFoundError("codex"),
    ):
        health = CodexCliAdapter().health()

    assert health.ok is False
    assert "PATH" in health.details["reason"]


def test_health_version_too_old(monkeypatch):
    """version below MIN_CODEX_VERSION -> ok=False with version error."""
    with mock.patch(
        "fno.adapters.codex.subprocess.run",
        return_value=_ver_proc("codex-cli 0.50.0"),
    ):
        health = CodexCliAdapter().health()

    assert health.ok is False
    assert "too old" in health.details["reason"]


def test_health_no_auth_at_all(monkeypatch, tmp_path):
    """Neither auth.json nor OPENAI_API_KEY -> ok=False with auth error."""
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)
    fake_home = tmp_path / "no_codex"
    monkeypatch.setattr(
        "fno.adapters.codex.os.path.expanduser",
        lambda p: str(fake_home) if p == "~/.codex" else p,
    )

    with mock.patch(
        "fno.adapters.codex.subprocess.run",
        return_value=_ver_proc("0.117.0"),
    ):
        health = CodexCliAdapter().health()

    assert health.ok is False
    assert "no auth" in health.details["reason"]


def test_health_version_timeout(monkeypatch):
    """codex --version hanging beyond timeout -> ok=False with timeout error."""
    with mock.patch(
        "fno.adapters.codex.subprocess.run",
        side_effect=subprocess.TimeoutExpired(cmd="codex --version", timeout=10),
    ):
        health = CodexCliAdapter().health()

    assert health.ok is False
    assert "timed out" in health.details["reason"]


def test_health_empty_auth_file_treated_as_no_auth(monkeypatch, tmp_path):
    """An empty ~/.codex/auth.json is not real auth; without OPENAI_API_KEY -> ok=False."""
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)
    fake_home = tmp_path / "codex_home"
    fake_home.mkdir()
    (fake_home / "auth.json").write_text("")  # empty file
    monkeypatch.setattr(
        "fno.adapters.codex.os.path.expanduser",
        lambda p: str(fake_home) if p == "~/.codex" else p,
    )

    with mock.patch(
        "fno.adapters.codex.subprocess.run",
        return_value=_ver_proc("0.117.0"),
    ):
        health = CodexCliAdapter().health()

    assert health.ok is False
    assert "no auth" in health.details["reason"]


def test_health_unparseable_version_distinct_error(monkeypatch):
    """An unrecognised version string surfaces a 'could not parse' message, not 'too old'."""
    with mock.patch(
        "fno.adapters.codex.subprocess.run",
        return_value=_ver_proc("future-version-style"),
    ):
        health = CodexCliAdapter().health()

    assert health.ok is False
    assert "could not parse" in health.details["reason"]
    assert "too old" not in health.details["reason"]


def test_health_version_nonzero_exit(monkeypatch):
    """codex --version exits non-zero -> ok=False with exit code info."""
    bad = subprocess.CompletedProcess(
        args=["codex", "--version"], returncode=2, stdout="", stderr="broken install"
    )
    with mock.patch("fno.adapters.codex.subprocess.run", return_value=bad):
        health = CodexCliAdapter().health()

    assert health.ok is False
    assert "exited 2" in health.details["reason"]


# ----------------------------------------------------------------------
# map_codex_error
# ----------------------------------------------------------------------


def test_map_codex_error_rate_limit_text_rule():
    """AC4.1-HP: stderr 'rate limit exceeded' -> PROVIDER_4XX_QUOTA + triggers_swap=True."""
    err = map_codex_error(returncode=1, stderr="rate limit exceeded")
    assert err.error_class is ErrorClass.PROVIDER_4XX_QUOTA
    assert err.triggers_swap is True
    assert err.raw_exit_code == 1
    assert err.raw_status is None
    assert err.body_excerpt == "rate limit exceeded"


def test_map_codex_error_sigkill_fallback():
    """AC4.2-EDGE: returncode 137 with empty stderr -> PARSER_ERROR + no swap."""
    err = map_codex_error(returncode=137, stderr="")
    assert err.error_class is ErrorClass.PARSER_ERROR
    assert err.triggers_swap is False
    assert err.raw_exit_code == 137


def test_map_codex_error_exit_2_server_side_hint():
    """AC4.3-EDGE: exit 2 with 'internal server error' in stderr -> PROVIDER_5XX + swap."""
    err = map_codex_error(
        returncode=2, stderr="internal server error: model unavailable"
    )
    assert err.error_class is ErrorClass.PROVIDER_5XX
    assert err.triggers_swap is True


def test_map_codex_error_exit_0_defensive():
    """AC4.4-FR: returncode 0 with empty stderr -> UNKNOWN + no swap (defensive)."""
    err = map_codex_error(returncode=0, stderr="")
    assert err.error_class is ErrorClass.UNKNOWN
    assert err.triggers_swap is False
    assert err.raw_exit_code == 0


def test_map_codex_error_body_excerpt_truncated_to_256_bytes():
    """AC4.5-EDGE: stderr longer than 256 bytes is truncated in body_excerpt."""
    long_err = "x" * 500
    err = map_codex_error(returncode=1, stderr=long_err)
    assert len(err.body_excerpt) == 256


def test_map_codex_error_overloaded_text_rule_via_classify_error():
    """'overloaded' is a Plan A text rule and must trigger swap as PROVIDER_4XX_QUOTA."""
    err = map_codex_error(returncode=2, stderr="model overloaded, try later")
    assert err.error_class is ErrorClass.PROVIDER_4XX_QUOTA
    assert err.triggers_swap is True


def test_map_codex_error_no_credentials_maps_to_auth():
    """'no credentials' Plan A text rule maps to PROVIDER_4XX_AUTH + swap."""
    err = map_codex_error(returncode=1, stderr="no credentials configured for openai")
    assert err.error_class is ErrorClass.PROVIDER_4XX_AUTH
    assert err.triggers_swap is True


def test_map_codex_error_exit_2_no_hint_is_unknown():
    """Exit 2 with stderr that doesn't match any rule or hint -> UNKNOWN."""
    err = map_codex_error(returncode=2, stderr="unexpected output 'foo'")
    assert err.error_class is ErrorClass.UNKNOWN
    assert err.triggers_swap is False


def test_map_codex_error_sigterm_is_parser_error():
    """SIGTERM (143) maps to PARSER_ERROR, no swap."""
    err = map_codex_error(returncode=143, stderr="")
    assert err.error_class is ErrorClass.PARSER_ERROR
    assert err.triggers_swap is False


def test_map_codex_error_timeout_is_parser_error():
    """coreutils timeout exit (124) maps to PARSER_ERROR."""
    err = map_codex_error(returncode=124, stderr="")
    assert err.error_class is ErrorClass.PARSER_ERROR
    assert err.triggers_swap is False


def test_map_codex_error_unknown_exit_is_unknown():
    """An unknown exit code (e.g. 99) without any text-rule match -> UNKNOWN, no swap."""
    err = map_codex_error(returncode=99, stderr="some unmapped failure")
    assert err.error_class is ErrorClass.UNKNOWN
    assert err.triggers_swap is False
    assert err.raw_exit_code == 99


def test_map_codex_error_negative_sigkill_normalizes_to_137():
    """Python subprocess returns -9 for SIGKILL; normalize to shell-style 137 -> PARSER_ERROR."""
    err = map_codex_error(returncode=-9, stderr="")
    assert err.error_class is ErrorClass.PARSER_ERROR
    assert err.triggers_swap is False
    # raw_exit_code captures the normalized value (137), not the raw -9
    assert err.raw_exit_code == 137


def test_map_codex_error_negative_sigterm_normalizes_to_143():
    """Python subprocess returns -15 for SIGTERM; normalize to 143 -> PARSER_ERROR."""
    err = map_codex_error(returncode=-15, stderr="")
    assert err.error_class is ErrorClass.PARSER_ERROR
    assert err.triggers_swap is False
    assert err.raw_exit_code == 143


def test_map_codex_error_non_int_returncode_is_unknown_not_raise():
    """Non-int returncode is a programmer error but the helper must not raise."""
    err = map_codex_error(returncode="oops", stderr="")  # type: ignore[arg-type]
    assert err.error_class is ErrorClass.UNKNOWN
    assert err.triggers_swap is False
    assert err.raw_exit_code is None


def test_map_codex_error_clamps_huge_stderr_blob():
    """A 1MB stderr blob is truncated to _STDERR_MAX_BYTES before processing."""
    huge = "x" * (200_000)  # 200KB - over the 64KB cap
    err = map_codex_error(returncode=1, stderr=huge)
    # body_excerpt should still be 256 bytes
    assert len(err.body_excerpt) == 256


def test_map_codex_error_returns_normalized_with_provider_5xx_when_unavailable():
    """Exit 2 stderr containing 'unavailable' triggers PROVIDER_5XX + swap."""
    err = map_codex_error(returncode=2, stderr="service unavailable")
    assert err.error_class is ErrorClass.PROVIDER_5XX
    assert err.triggers_swap is True


# ----------------------------------------------------------------------
# _version_at_least
# ----------------------------------------------------------------------


@pytest.mark.parametrize(
    "actual,minimum,expected",
    [
        ("codex-cli 0.117.0", "0.117.0", True),
        ("0.117.0", "0.117.0", True),
        ("0.117.1", "0.117.0", True),
        ("0.117.0", "0.117.1", False),
        ("0.116.99", "0.117.0", False),
        ("1.0.0", "0.117.0", True),
        ("not a version at all", "0.117.0", False),
        ("0.117", "0.117.0", False),  # incomplete version string
    ],
)
def test_version_at_least_table(actual, minimum, expected):
    assert _version_at_least(actual, minimum) is expected
