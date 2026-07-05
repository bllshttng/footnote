"""Unit tests for ``providers.codex.create`` JSONL parser + tee semantics.

Uses the captured fixture from Wave 1.0 as the source-of-truth replay
stream so the parser is exercised against the real codex 0.130.0
vocabulary, not hand-typed event strings.

Plan ACs covered:
- create() spawns `codex exec --json --cd <cwd> --skip-git-repo-check --sandbox <mode> <prompt>` with stdin=DEVNULL and stderr=subprocess.STDOUT
- JSONL parse loop captures session_id from _EVENT_TYPES["session"] events
- JSONL parse loop captures last_msg from _EVENT_TYPES["message"] events
- Tee writes every stdout line to output.jsonl in append mode, line-buffered
- On 0-event stream raise NoSessionIdError carrying the set of event-type names actually seen
"""
from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
from pathlib import Path
from typing import IO
from unittest.mock import MagicMock

import pytest

from fno.agents.providers import codex as codex_mod
from fno.agents.providers.codex import (
    CodexInvocationError,
    CodexResult,
    NoSessionIdError,
)


FIXTURE_PATH = (
    Path(__file__).parent / "fixtures" / "codex-jsonl-sample.jsonl"
)


def _read_fixture_lines() -> list[str]:
    """Return the captured JSONL stream as a list of lines."""
    return FIXTURE_PATH.read_text(encoding="utf-8").splitlines(keepends=True)


def _fixture_thread_id() -> str:
    """Extract the thread.started event's thread_id from the live fixture.

    The fixture is regenerated each time the smoke script runs (Wave 1.0),
    so unit tests cannot hardcode the value — they must read it.
    """
    for raw in _read_fixture_lines():
        stripped = raw.strip()
        if stripped.startswith("{") and '"type":"thread.started"' in stripped:
            event = json.loads(stripped)
            return event["thread_id"]
    raise RuntimeError(f"thread.started not found in fixture {FIXTURE_PATH}")


def _fixture_last_agent_message() -> str:
    """Extract the LAST agent_message text from the live fixture."""
    last = ""
    for raw in _read_fixture_lines():
        stripped = raw.strip()
        if not stripped.startswith("{"):
            continue
        event = json.loads(stripped)
        if (
            event.get("type") == "item.completed"
            and isinstance(event.get("item"), dict)
            and event["item"].get("type") == "agent_message"
        ):
            text = event["item"].get("text")
            if isinstance(text, str):
                last = text
    return last


class _FakePopen:
    """subprocess.Popen stand-in that replays a canned stdout stream.

    Constructor signature mirrors the real Popen's positional + keyword
    arguments so monkeypatching the module attribute works transparently.
    The fake captures argv / kwargs (for assertions) and exposes a
    readline-iterable stdout via :class:`_FakeStdout`.
    """

    def __init__(
        self,
        argv,
        stdin=None,
        stdout=None,
        stderr=None,
        text=False,
        bufsize=0,
        cwd=None,
        env=None,
        start_new_session=False,
    ):
        self.argv = list(argv)
        self.stdin_arg = stdin
        self.stdout_arg = stdout
        self.stderr_arg = stderr
        self.text_arg = text
        self.bufsize_arg = bufsize
        self.cwd_arg = cwd
        self.start_new_session = start_new_session
        self.pid = 99999  # fake PID for getpgid lookups in tests
        # Default: replay the captured fixture; tests override via class attr.
        # Explicit None check so an empty-list override behaves as "no lines",
        # not as "fall through to fixture".
        scripted = self.__class__._scripted_lines
        self._lines = scripted if scripted is not None else _read_fixture_lines()
        self._returncode = self.__class__._scripted_exit_code
        self.stdout = _FakeStdout(self._lines)
        self.terminated = False
        self.killed = False
        self.signals: list[int] = []

    # Class-level knobs so tests can swap behavior without subclassing.
    _scripted_lines: list[str] | None = None
    _scripted_exit_code: int = 0

    def wait(self, timeout=None):
        return self._returncode

    def poll(self):
        # Stdlib Popen.poll returns None if the process is still alive,
        # else its exit code. The fake completes synchronously, so we
        # return the cached exit code (which signals "already reaped").
        return self._returncode

    def terminate(self):
        self.terminated = True

    def kill(self):
        self.killed = True

    def send_signal(self, sig):
        self.signals.append(sig)

    @property
    def returncode(self):
        return self._returncode


class _FakeStdout:
    """Iterable stdin with .readline() conforming to subprocess.PIPE shape."""

    def __init__(self, lines: list[str]):
        self._iter = iter(lines)
        self.closed = False

    def readline(self):
        try:
            return next(self._iter)
        except StopIteration:
            return ""

    def close(self):
        self.closed = True


@pytest.fixture
def fake_popen(monkeypatch):
    """Install _FakePopen as the module-level _subprocess_popen."""
    factory = MagicMock(side_effect=_FakePopen)
    monkeypatch.setattr(codex_mod, "_subprocess_popen", factory)
    # Reset script state between tests.
    _FakePopen._scripted_lines = None
    _FakePopen._scripted_exit_code = 0
    return factory


# ---------------------------------------------------------------------------
# Happy path: fixture replay
# ---------------------------------------------------------------------------


def test_create_captures_session_id_and_last_message_from_fixture(
    tmp_path, fake_popen
):
    out = codex_mod.create(
        cwd=Path("/tmp"),
        prompt="echo hello",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
    )

    assert isinstance(out, CodexResult)
    assert out.exit_code == 0
    # Thread id is dynamic per smoke run; read it from the fixture.
    assert out.session_id == _fixture_thread_id()
    # Last agent_message in the fixture (whatever the model said last).
    assert out.last_msg == _fixture_last_agent_message()
    assert out.duration_ms >= 0


def test_create_tees_every_line_to_output_path(tmp_path, fake_popen):
    out_file = tmp_path / "agents" / "worker-X" / "output.jsonl"
    codex_mod.create(
        cwd=Path("/tmp"),
        prompt="prompt",
        from_name="fno",
        yolo=False,
        output_path=out_file,
    )

    assert out_file.exists()
    written = out_file.read_text(encoding="utf-8")
    # Every fixture line — including the non-JSON banner — must be tee'd.
    for raw in _read_fixture_lines():
        assert raw in written, f"missing line in tee: {raw!r}"


def test_create_tee_appends_across_invocations(tmp_path, fake_popen):
    out_file = tmp_path / "output.jsonl"
    out_file.parent.mkdir(parents=True, exist_ok=True)
    out_file.write_text("PRE-EXISTING LINE\n", encoding="utf-8")

    codex_mod.create(
        cwd=Path("/tmp"),
        prompt="prompt",
        from_name="fno",
        yolo=False,
        output_path=out_file,
    )

    body = out_file.read_text(encoding="utf-8")
    assert body.startswith("PRE-EXISTING LINE\n")
    # The captured stream lands AFTER the pre-existing content (append).
    assert "thread.started" in body


def test_create_argv_shape_pin(tmp_path, fake_popen):
    # headless_yolo=False pins the explicit sandboxed argv shape. The autonomous
    # exec lane now DEFAULTS to no-prompt (ab-994222ee); the default-vs-opt-out
    # behavior is covered in test_providers_codex_argv.py. This test pins the
    # structural argv (flags + prompt position), so it opts out deterministically.
    codex_mod.create(
        cwd=Path("/tmp/work"),
        prompt="do this",
        from_name="orchestrator",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
        headless_yolo=False,
    )

    fake_popen.assert_called_once()
    # The constructor receives argv as positional kwargs[0] OR call_args[0].
    call_args = fake_popen.call_args
    argv = call_args.args[0]

    # Strict argv ordering: codex --ask-for-approval never exec --json
    # -C <cwd> --skip-git-repo-check --sandbox workspace-write <prompt>.
    # --ask-for-approval is a GLOBAL flag and MUST precede `exec` (codex
    # rejects it after `exec`); --sandbox is an `exec` flag and follows it.
    assert argv[:5] == ["codex", "--ask-for-approval", "never", "exec", "--json"]
    assert argv[5:7] == ["-C", "/tmp/work"]
    assert "--skip-git-repo-check" in argv
    assert "--sandbox" in argv
    assert "workspace-write" in argv
    assert "--dangerously-bypass-approvals-and-sandbox" not in argv
    # Regression (pr704): the global approval flag must come BEFORE `exec`.
    assert argv.index("--ask-for-approval") < argv.index("exec")
    # The prompt arg is the LAST positional and contains the from-name prefix.
    assert argv[-1] == "[from: orchestrator]\n\ndo this"

    # Locked Decision 11 + 12: stdin DEVNULL, stderr merged into stdout.
    assert call_args.kwargs["stdin"] == subprocess.DEVNULL
    assert call_args.kwargs["stdout"] == subprocess.PIPE
    assert call_args.kwargs["stderr"] == subprocess.STDOUT
    assert call_args.kwargs["text"] is True
    assert call_args.kwargs["bufsize"] == 1


def test_create_routed_openai_provider_injects_config_and_env(
    tmp_path, fake_popen, monkeypatch
):
    """Item 4 (x-db50): a routed role with an openai-protocol provider prepends
    the `-c` model_provider config (before `exec`) and injects the api key env."""
    from fno.agents import model_routing

    monkeypatch.setattr(
        model_routing,
        "resolve_codex_route",
        lambda role, **kw: model_routing.CodexRoute(
            env={"OPENAI_API_KEY": "oai-key"},
            config_args=[
                "-c",
                "model_providers.zai-openai={ base_url = 'https://z/v4', "
                "env_key = 'OPENAI_API_KEY', wire_api = 'chat' }",
                "-c",
                "model_provider='zai-openai'",
                "-c",
                "model='glm-5.2'",
            ],
        ),
    )
    codex_mod.create(
        cwd=Path("/tmp/work"),
        prompt="do this",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
        headless_yolo=False,
        role="tidy",
    )
    call_args = fake_popen.call_args
    argv = call_args.args[0]
    # The -c config flags are GLOBAL: they must precede `exec`.
    assert argv[0] == "codex"
    assert "-c" in argv and argv.index("-c") < argv.index("exec")
    assert any("model_provider='zai-openai'" in a for a in argv)
    assert any("model='glm-5.2'" in a for a in argv)
    # The api key rides the spawn env (codex reads it via env_key).
    assert call_args.kwargs["env"]["OPENAI_API_KEY"] == "oai-key"


def test_create_unrouted_leaves_argv_and_env_default(tmp_path, fake_popen, monkeypatch):
    """No role -> no codex route -> argv/env byte-identical to today (no -c,
    inherit parent env)."""
    codex_mod.create(
        cwd=Path("/tmp/work"),
        prompt="do this",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
        headless_yolo=False,
    )
    call_args = fake_popen.call_args
    argv = call_args.args[0]
    assert "-c" not in argv
    assert argv[:2] == ["codex", "--ask-for-approval"]
    # No agent_self, no route -> env is None (inherit parent unchanged).
    assert call_args.kwargs["env"] is None


def test_create_yolo_swaps_sandbox_for_dangerous_bypass(tmp_path, fake_popen):
    codex_mod.create(
        cwd=Path("/tmp"),
        prompt="ship it",
        from_name="fno",
        yolo=True,
        output_path=tmp_path / "output.jsonl",
    )
    argv = fake_popen.call_args.args[0]
    assert "--dangerously-bypass-approvals-and-sandbox" in argv
    assert "--sandbox" not in argv
    assert "workspace-write" not in argv


# ---------------------------------------------------------------------------
# Failure modes
# ---------------------------------------------------------------------------


def test_create_zero_event_stream_raises_no_session_id_error(tmp_path, fake_popen):
    _FakePopen._scripted_lines = []  # codex exits before emitting anything
    _FakePopen._scripted_exit_code = 1

    with pytest.raises(NoSessionIdError) as exc_info:
        codex_mod.create(
            cwd=Path("/tmp"),
            prompt="prompt",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
        )
    assert exc_info.value.types_seen == set()
    assert "did not emit session id" in str(exc_info.value)


def test_create_stream_with_other_events_but_no_session_warns_with_types(
    tmp_path, fake_popen
):
    # Locked Decision 14: warn-on-drift surfaces what we DID see.
    _FakePopen._scripted_lines = [
        '{"type": "turn.started"}\n',
        '{"type": "item.started", "item": {"type": "command_execution"}}\n',
        '{"type": "turn.completed", "usage": {}}\n',
    ]
    _FakePopen._scripted_exit_code = 0

    with pytest.raises(NoSessionIdError) as exc_info:
        codex_mod.create(
            cwd=Path("/tmp"),
            prompt="prompt",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
        )
    assert exc_info.value.types_seen == {
        "turn.started",
        "item.started",
        "turn.completed",
    }
    assert "turn.started" in str(exc_info.value)


def test_create_empty_thread_id_is_not_captured_as_session(tmp_path, fake_popen):
    # cv-dcd823ce: an EMPTY thread_id ("") must NOT be captured as the session
    # id. Capturing it would write codex_session_id="" to the registry and make
    # every later resume fail opaquely. The stream still recorded
    # "thread.started" in types_seen, so create fails closed with
    # NoSessionIdError (exit 11). (Mirrors codex_ask.rs's run_codex guard.)
    _FakePopen._scripted_lines = [
        '{"type": "thread.started", "thread_id": ""}\n',
        (
            '{"type": "item.completed", "item": '
            '{"type": "agent_message", "text": "ignored reply"}}\n'
        ),
        '{"type": "turn.completed", "usage": {}}\n',
    ]
    _FakePopen._scripted_exit_code = 0

    with pytest.raises(NoSessionIdError) as exc_info:
        codex_mod.create(
            cwd=Path("/tmp"),
            prompt="prompt",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
        )
    # The event type is still surfaced for forensics even though the id was empty.
    assert "thread.started" in exc_info.value.types_seen


def test_create_nonzero_exit_without_message_raises_invocation_error(
    tmp_path, fake_popen
):
    _FakePopen._scripted_lines = [
        '{"type": "thread.started", "thread_id": "abc-123"}\n',
        # No agent_message, no turn.completed; codex died mid-stream.
    ]
    _FakePopen._scripted_exit_code = 1

    with pytest.raises(CodexInvocationError) as exc_info:
        codex_mod.create(
            cwd=Path("/tmp"),
            prompt="prompt",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
        )
    assert exc_info.value.exit_code == 1


def test_create_nonzero_exit_with_captured_message_returns_result(
    tmp_path, fake_popen
):
    # If codex emitted a partial reply before exiting non-zero, we
    # surface what we have so the operator sees the assistant's words.
    _FakePopen._scripted_lines = [
        '{"type": "thread.started", "thread_id": "abc-123"}\n',
        (
            '{"type": "item.completed", "item": '
            '{"type": "agent_message", "text": "partial reply"}}\n'
        ),
    ]
    _FakePopen._scripted_exit_code = 2

    out = codex_mod.create(
        cwd=Path("/tmp"),
        prompt="prompt",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
    )
    assert out.exit_code == 2
    assert out.session_id == "abc-123"
    assert out.last_msg == "partial reply"


def test_create_missing_codex_binary_raises_invocation_127(
    tmp_path, monkeypatch
):
    def _raise_file_not_found(*args, **kwargs):
        raise FileNotFoundError("codex not found")

    monkeypatch.setattr(codex_mod, "_subprocess_popen", _raise_file_not_found)
    with pytest.raises(CodexInvocationError) as exc_info:
        codex_mod.create(
            cwd=Path("/tmp"),
            prompt="prompt",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
        )
    assert exc_info.value.exit_code == 127


def test_create_skips_non_json_banner_lines(tmp_path, fake_popen):
    # The fixture's line 1 is "Reading additional input from stdin...";
    # the parser must NOT crash on it (it skips lines not starting with '{').
    out = codex_mod.create(
        cwd=Path("/tmp"),
        prompt="prompt",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
    )
    # If banner-line handling crashed we'd never get here.
    assert out.session_id == _fixture_thread_id()


def test_create_does_not_treat_error_item_as_fatal(tmp_path, fake_popen):
    # The fixture contains an item.completed with item.type=error (the
    # codex_hooks deprecation warning). This is a SOFT error: codex still
    # exits 0 and produces a normal reply. The parser must NOT treat it
    # as fatal; the test passes iff create() returns happily.
    out = codex_mod.create(
        cwd=Path("/tmp"),
        prompt="prompt",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
    )
    assert out.exit_code == 0
    assert out.last_msg == "hello"


def test_create_tolerates_malformed_json_line(tmp_path, fake_popen):
    # codex could emit a Rust panic mid-stream that lands on stderr-merged
    # stdout. The parser must skip it for control flow but still tee it.
    _FakePopen._scripted_lines = [
        '{"type": "thread.started", "thread_id": "abc"}\n',
        "not json at all\n",
        '{"type": "item.completed", "item": {"type": "agent_message", "text": "ok"}}\n',
        '{"type": "turn.completed", "usage": {}}\n',
    ]
    _FakePopen._scripted_exit_code = 0

    out_file = tmp_path / "output.jsonl"
    out = codex_mod.create(
        cwd=Path("/tmp"),
        prompt="prompt",
        from_name="fno",
        yolo=False,
        output_path=out_file,
    )
    assert out.session_id == "abc"
    assert out.last_msg == "ok"
    # Malformed line is tee'd anyway for forensics.
    assert "not json at all" in out_file.read_text(encoding="utf-8")


def test_create_tee_open_eacces_raises_invocation_error_exit_12(
    tmp_path, monkeypatch
):
    """silent-failure-hunter row 2 regression: _open_tee failure maps to
    a structured CodexInvocationError (exit 12), not a raw OSError."""
    # Point the output file at a directory we can't write under.
    # Force PermissionError by monkeypatching mkdir to raise it.
    from pathlib import Path as _P

    def _raise_eacces(self, *args, **kwargs):
        raise PermissionError(13, "Permission denied")

    monkeypatch.setattr(_P, "mkdir", _raise_eacces)

    with pytest.raises(CodexInvocationError) as exc_info:
        codex_mod.create(
            cwd=Path("/tmp"),
            prompt="msg",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "agents" / "worker-X" / "output.jsonl",
        )
    assert exc_info.value.exit_code == 12


def test_create_tee_per_errno_warn_recurs_for_distinct_modes(
    tmp_path, fake_popen, capsys
):
    """silent-failure-hunter row 1 regression: a recurring failure mode
    warns ONCE; a DIFFERENT failure mode warns separately. The old
    code used a boolean latch that went permanently silent after the
    first OSError."""
    # Stub tee write to raise alternating errno values.
    written: list[str] = []

    class _BrokenTee:
        def __init__(self):
            self.calls = 0
            self.closed = False

        def write(self, raw):
            self.calls += 1
            if self.calls in (1, 2):
                raise OSError(28, "No space left on device")  # ENOSPC
            if self.calls in (3, 4):
                raise OSError(13, "Permission denied")        # EACCES
            written.append(raw)

        def flush(self):
            pass

        def close(self):
            self.closed = True

    broken = _BrokenTee()
    monkeypatch_open = lambda *a, **kw: broken
    # Patch _open_tee to return our broken tee.
    import fno.agents.providers.codex as cm

    original = cm._open_tee
    try:
        cm._open_tee = lambda path: broken
        codex_mod.create(
            cwd=Path("/tmp"),
            prompt="msg",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
        )
    finally:
        cm._open_tee = original

    captured = capsys.readouterr()
    # The stream has many lines; ENOSPC should appear exactly ONCE
    # (calls 1+2 same mode), EACCES should ALSO appear exactly ONCE
    # (calls 3+4 same mode). The fixture has more lines so write()
    # was called multiple times.
    enospc_warns = captured.err.count("No space left on device")
    eacces_warns = captured.err.count("Permission denied")
    assert enospc_warns == 1, f"expected 1 ENOSPC warn, got {enospc_warns}: {captured.err!r}"
    assert eacces_warns == 1, f"expected 1 EACCES warn, got {eacces_warns}: {captured.err!r}"


def test_create_promotes_error_item_message_when_no_agent_message(tmp_path, fake_popen):
    """silent-failure-hunter row 4 regression: a soft-error item with NO
    agent_message previously left CodexResult.last_msg=""; now it's
    promoted so the caller sees the error context."""
    _FakePopen._scripted_lines = [
        '{"type": "thread.started", "thread_id": "sid"}\n',
        (
            '{"type": "item.completed", "item": '
            '{"type": "error", "message": "config deprecation warning"}}\n'
        ),
        '{"type": "turn.completed", "usage": {}}\n',
    ]
    _FakePopen._scripted_exit_code = 0

    out = codex_mod.create(
        cwd=Path("/tmp"),
        prompt="msg",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
    )
    # No agent_message arrived but the error item's message is now
    # surfaced via last_msg.
    assert out.last_msg == "config deprecation warning"
    assert out.exit_code == 0


def test_create_agent_message_takes_precedence_over_error_item(tmp_path, fake_popen):
    """If both agent_message and error item arrive, agent_message wins."""
    _FakePopen._scripted_lines = [
        '{"type": "thread.started", "thread_id": "sid"}\n',
        (
            '{"type": "item.completed", "item": '
            '{"type": "error", "message": "soft warning"}}\n'
        ),
        (
            '{"type": "item.completed", "item": '
            '{"type": "agent_message", "text": "real reply"}}\n'
        ),
        '{"type": "turn.completed", "usage": {}}\n',
    ]
    _FakePopen._scripted_exit_code = 0

    out = codex_mod.create(
        cwd=Path("/tmp"),
        prompt="msg",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
    )
    assert out.last_msg == "real reply"


def test_create_watchdog_ignored_when_proc_already_exited(tmp_path, fake_popen, monkeypatch):
    """Codex PR #305 round 4 (P2) regression: _on_timeout must check
    proc.poll() before marking timed_out. Without the guard, a watchdog
    that fires concurrent with normal process exit would raise
    CodexTimeoutError on a successful run.
    """
    # The fake Popen's poll() returns the exit code (non-None) so the
    # subprocess is "already exited" by the time _on_timeout runs. We
    # force-trigger _on_timeout via a near-zero timeout and assert the
    # function returns a success result rather than CodexTimeoutError.
    out = codex_mod.create(
        cwd=Path("/tmp"),
        prompt="msg",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
        timeout=0.001,  # watchdog fires immediately
    )
    # If the guard was missing, this assertion would never run because
    # CodexTimeoutError would have been raised. The success here proves
    # _on_timeout saw proc.poll() != None and returned early.
    assert out.exit_code == 0
    assert out.session_id == _fixture_thread_id()


def test_create_sigkill_escalation_never_reports_success(tmp_path, fake_popen, monkeypatch):
    """silent-failure-hunter row 3 regression: a force-killed run with a
    partial agent_message previously surfaced as success. Now SIGKILL
    escalation always raises CodexInvocationError."""
    _FakePopen._scripted_lines = [
        '{"type": "thread.started", "thread_id": "sid"}\n',
        (
            '{"type": "item.completed", "item": '
            '{"type": "agent_message", "text": "partial reply"}}\n'
        ),
    ]
    _FakePopen._scripted_exit_code = 0

    # Stub _wait_with_grace to report SIGKILL escalation.
    monkeypatch.setattr(
        codex_mod,
        "_wait_with_grace",
        lambda proc, grace_sec=5.0: (0, True),  # exit 0 but force-killed
    )

    with pytest.raises(CodexInvocationError) as exc_info:
        codex_mod.create(
            cwd=Path("/tmp"),
            prompt="msg",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
        )
    # SIGKILL with exit 0 -> exit 1 (we force a non-zero code for the
    # dispatcher's exit-code propagation).
    assert exc_info.value.exit_code == 1


def test_create_closes_tee_fh_on_exception_path(tmp_path, fake_popen):
    """Gemini PR #305 round 2: tee_fh must close on every exit path, not
    just the happy path. Force CodexInvocationError by configuring a
    non-zero exit + empty stream, then assert the tee handle is closed.
    """
    _FakePopen._scripted_lines = []
    _FakePopen._scripted_exit_code = 1

    closed: list[bool] = []
    real_open_tee = codex_mod._open_tee

    def _tracking_open_tee(path):
        fh = real_open_tee(path)
        original_close = fh.close

        def _close_and_track():
            closed.append(True)
            return original_close()

        fh.close = _close_and_track
        return fh

    codex_mod._open_tee = _tracking_open_tee
    try:
        with pytest.raises(NoSessionIdError):
            codex_mod.create(
                cwd=Path("/tmp"),
                prompt="msg",
                from_name="fno",
                yolo=False,
                output_path=tmp_path / "output.jsonl",
            )
    finally:
        codex_mod._open_tee = real_open_tee

    assert closed, "tee_fh.close() was not called on the exception path"


def test_create_closes_tee_fh_on_keyboard_interrupt(tmp_path, monkeypatch):
    """Gemini PR #305 round 2 regression: KeyboardInterrupt mid-_parse_stream
    must still close tee_fh via the outer try/finally."""
    closed: list[bool] = []

    class _InterruptingStdout:
        def readline(self):
            raise KeyboardInterrupt

        def close(self):
            pass

    class _FakeInterruptingPopen:
        def __init__(self, *a, **kw):
            self.stdout = _InterruptingStdout()
            self.pid = 99999
            self._returncode = 0

        def wait(self, timeout=None):
            return 0

        def poll(self):
            return self._returncode

        def terminate(self):
            pass

        def kill(self):
            pass

        def send_signal(self, sig):
            pass

        @property
        def returncode(self):
            return self._returncode

    monkeypatch.setattr(codex_mod, "_subprocess_popen", _FakeInterruptingPopen)

    real_open_tee = codex_mod._open_tee

    def _tracking_open_tee(path):
        fh = real_open_tee(path)
        original_close = fh.close

        def _close_and_track():
            closed.append(True)
            return original_close()

        fh.close = _close_and_track
        return fh

    monkeypatch.setattr(codex_mod, "_open_tee", _tracking_open_tee)

    # Patch os.killpg / os.getpgid since the fake Popen doesn't actually
    # create a process group.
    monkeypatch.setattr(codex_mod.os, "getpgid", lambda pid: pid)
    monkeypatch.setattr(codex_mod.os, "killpg", lambda pgid, sig: None)

    with pytest.raises(KeyboardInterrupt):
        codex_mod.create(
            cwd=Path("/tmp"),
            prompt="msg",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
        )

    assert closed, "tee_fh.close() was not called when KeyboardInterrupt fired"


def test_create_breaks_on_turn_completed_ignoring_post_complete_lines(
    tmp_path, fake_popen
):
    # If codex emits more events AFTER turn.completed, the parser must
    # break out of the read loop and NOT update last_msg from them.
    _FakePopen._scripted_lines = [
        '{"type": "thread.started", "thread_id": "sid-1"}\n',
        '{"type": "item.completed", "item": {"type": "agent_message", "text": "first"}}\n',
        '{"type": "turn.completed", "usage": {}}\n',
        # Trailing event with a different message — must be ignored.
        '{"type": "item.completed", "item": {"type": "agent_message", "text": "AFTER_COMPLETE"}}\n',
    ]
    _FakePopen._scripted_exit_code = 0

    out = codex_mod.create(
        cwd=Path("/tmp"),
        prompt="prompt",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
    )
    assert out.last_msg == "first"
    assert "AFTER_COMPLETE" not in out.last_msg
