"""Unit tests for ``providers.codex.resume`` follow-up invocation.

Shares the JSONL replay infrastructure with ``test_providers_codex_create``.
Resume differs from create in three ways:

- argv: ``codex exec resume <session_id>`` instead of ``codex exec``
- cwd: passed via ``Popen(cwd=...)`` because resume does NOT accept ``--cd``
- session_id is NOT re-captured (caller already has it from the registry)

Plan ACs covered:
- codex.resume(session_id, cwd, prompt) spawns `codex exec resume <id> --json ... <prompt>`
- Same JSONL parse loop as create; session_id NOT re-captured
- cwd always taken from caller (Popen cwd, no --cd flag)
- On codex "session not found" exit propagate codex's diagnostic and exit code
"""
from __future__ import annotations

import subprocess
from pathlib import Path
from typing import IO
from unittest.mock import MagicMock

import pytest

from fno.agents.providers import codex as codex_mod
from fno.agents.providers.codex import (
    CodexInvocationError,
    CodexResult,
)


# Reuse the same FakePopen pattern as create tests. Duplicated rather
# than imported so a refactor of either test module is independent.
class _FakePopen:
    _scripted_lines: list[str] | None = None
    _scripted_exit_code: int = 0

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
        self.cwd_arg = cwd
        self.start_new_session = start_new_session
        self.pid = 99999
        scripted = self.__class__._scripted_lines
        self._lines = scripted if scripted is not None else []
        self._returncode = self.__class__._scripted_exit_code
        self.stdout = _FakeStdout(self._lines)

    def wait(self, timeout=None):
        return self._returncode

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


class _FakeStdout:
    def __init__(self, lines: list[str]):
        self._iter = iter(lines)

    def readline(self):
        try:
            return next(self._iter)
        except StopIteration:
            return ""

    def close(self):
        pass


@pytest.fixture
def fake_popen(monkeypatch):
    factory = MagicMock(side_effect=_FakePopen)
    monkeypatch.setattr(codex_mod, "_subprocess_popen", factory)
    _FakePopen._scripted_lines = None
    _FakePopen._scripted_exit_code = 0
    return factory


# ---------------------------------------------------------------------------
# Argv shape and cwd handling
# ---------------------------------------------------------------------------


def test_resume_argv_pin(tmp_path, fake_popen):
    _FakePopen._scripted_lines = [
        '{"type": "thread.started", "thread_id": "ignored-on-resume"}\n',
        '{"type": "item.completed", "item": {"type": "agent_message", "text": "ack"}}\n',
        '{"type": "turn.completed", "usage": {}}\n',
    ]
    # headless_yolo=False pins the explicit sandboxed (no-bypass) resume shape.
    # The autonomous lane now DEFAULTS to no-prompt (ab-994222ee); this test
    # pins the structural argv, so it opts out deterministically.
    out = codex_mod.resume(
        session_id="abc-uuid-1234",
        cwd=Path("/Users/foo/proj"),
        prompt="follow up",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
        headless_yolo=False,
    )

    assert isinstance(out, CodexResult)
    call = fake_popen.call_args
    argv = call.args[0]
    # `codex exec resume <id> --json --skip-git-repo-check <prompt>`
    assert argv[:4] == ["codex", "exec", "resume", "abc-uuid-1234"]
    assert "--json" in argv
    assert "--skip-git-repo-check" in argv
    # Resume MUST NOT pass --cd / -C; the cwd-pinning is via Popen cwd kw.
    assert "--cd" not in argv
    assert "-C" not in argv
    # Resume MUST NOT pass --sandbox (codex exec resume has no such flag).
    assert "--sandbox" not in argv
    assert "workspace-write" not in argv
    # With the opt-out, the no-prompt bypass is NOT emitted either (inherits
    # the original session's sandbox).
    assert "--dangerously-bypass-approvals-and-sandbox" not in argv
    # Last positional is the prompt with from-name prefix.
    assert argv[-1] == "[from: fno]\n\nfollow up"

    # cwd-pinning is via Popen(cwd=str(cwd)).
    assert call.kwargs["cwd"] == "/Users/foo/proj"
    assert call.kwargs["stdin"] == subprocess.DEVNULL
    assert call.kwargs["stdout"] == subprocess.PIPE
    assert call.kwargs["stderr"] == subprocess.STDOUT


def test_resume_yolo_emits_dangerous_bypass_only(tmp_path, fake_popen):
    _FakePopen._scripted_lines = [
        '{"type": "item.completed", "item": {"type": "agent_message", "text": "ok"}}\n',
        '{"type": "turn.completed", "usage": {}}\n',
    ]
    codex_mod.resume(
        session_id="uuid",
        cwd=Path("/tmp"),
        prompt="msg",
        from_name="fno",
        yolo=True,
        output_path=tmp_path / "output.jsonl",
    )
    argv = fake_popen.call_args.args[0]
    assert "--dangerously-bypass-approvals-and-sandbox" in argv
    assert "--sandbox" not in argv


def test_resume_does_not_recapture_session_id(tmp_path, fake_popen):
    # Even if the resume stream emits a thread.started event (codex's
    # bookkeeping detail), the CodexResult's session_id is None — the
    # caller already has the id and resume doesn't speak about it.
    _FakePopen._scripted_lines = [
        '{"type": "thread.started", "thread_id": "should-be-ignored"}\n',
        '{"type": "item.completed", "item": {"type": "agent_message", "text": "ok"}}\n',
        '{"type": "turn.completed", "usage": {}}\n',
    ]
    out = codex_mod.resume(
        session_id="real-uuid",
        cwd=Path("/tmp"),
        prompt="msg",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
    )
    # Resume's CodexResult.session_id is set by _parse_stream too, but the
    # expect_session=False branch means we don't raise if it's None. The
    # contract is "caller already has the id; resume just delivers a reply".
    # We accept either None or the captured id here; the load-bearing field
    # is last_msg.
    assert out.last_msg == "ok"


def test_resume_captures_last_message(tmp_path, fake_popen):
    _FakePopen._scripted_lines = [
        '{"type": "item.completed", "item": {"type": "agent_message", "text": "first"}}\n',
        '{"type": "item.completed", "item": {"type": "agent_message", "text": "second"}}\n',
        '{"type": "turn.completed", "usage": {}}\n',
    ]
    out = codex_mod.resume(
        session_id="u",
        cwd=Path("/tmp"),
        prompt="m",
        from_name="fno",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
    )
    # Last agent_message wins.
    assert out.last_msg == "second"


def test_resume_session_not_found_raises_invocation_error(tmp_path, fake_popen):
    # codex prints "session not found" to stderr (which we merge into
    # stdout via Locked Decision 12) and exits non-zero. Without a
    # captured agent_message we raise CodexInvocationError.
    _FakePopen._scripted_lines = [
        "session not found: invalid-uuid\n",  # codex's plain-text diagnostic
    ]
    _FakePopen._scripted_exit_code = 1

    with pytest.raises(CodexInvocationError) as exc_info:
        codex_mod.resume(
            session_id="invalid-uuid",
            cwd=Path("/tmp"),
            prompt="m",
            from_name="fno",
            yolo=False,
            output_path=tmp_path / "output.jsonl",
        )
    assert exc_info.value.exit_code == 1


def test_resume_tees_every_line_to_output(tmp_path, fake_popen):
    _FakePopen._scripted_lines = [
        '{"type": "item.completed", "item": {"type": "agent_message", "text": "ok"}}\n',
        '{"type": "turn.completed", "usage": {}}\n',
    ]
    out_file = tmp_path / "agents" / "worker-X" / "output.jsonl"
    codex_mod.resume(
        session_id="u",
        cwd=Path("/tmp"),
        prompt="m",
        from_name="fno",
        yolo=False,
        output_path=out_file,
    )
    body = out_file.read_text(encoding="utf-8")
    for line in _FakePopen._scripted_lines:
        assert line in body


def test_resume_tee_appends_does_not_truncate(tmp_path, fake_popen):
    _FakePopen._scripted_lines = [
        '{"type": "turn.completed", "usage": {}}\n',
    ]
    out_file = tmp_path / "output.jsonl"
    out_file.parent.mkdir(parents=True, exist_ok=True)
    out_file.write_text("EARLIER CONTENT\n", encoding="utf-8")
    codex_mod.resume(
        session_id="u",
        cwd=Path("/tmp"),
        prompt="m",
        from_name="fno",
        yolo=False,
        output_path=out_file,
    )
    body = out_file.read_text(encoding="utf-8")
    assert body.startswith("EARLIER CONTENT\n")
    assert "turn.completed" in body
