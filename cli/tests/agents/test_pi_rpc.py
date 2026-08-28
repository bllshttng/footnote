"""The pi rpc lane: strict JSONL framing, held-open stdin, and honest receipts.

Three traps are pinned here, and each has already cost somebody a wrong answer
on some harness:

1. **A generic line reader is not protocol-compliant.** pi's rpc framing makes
   LF the only record delimiter. Node's ``readline`` also splits on U+2028 and
   U+2029, which are legal inside a JSON string, and Python's text-mode line
   iteration has the same defect. A splitter that breaks there corrupts a
   record whose CONTENT happens to carry one.
2. **rpc mode exits on stdin EOF, mid-turn, with status 0.** A prompt fed from
   a file yielded five events and stopped at the user's own ``message_end``;
   the assistant never spoke and the exit code still read success. So the
   driver settles on the POSITIVE ``agent_settled`` event and refuses to treat
   a stream that merely ended as a turn that completed.
3. **``success: true`` is a receipt about ACCEPTANCE.** Failures after
   acceptance arrive through the event stream, never as a second response for
   the same request id, so a receipt claiming the agent acted is a receipt that
   lies.
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest

_TEST_DIR = Path(__file__).resolve().parent
_SRC = _TEST_DIR.parents[1] / "src"
if str(_SRC) not in sys.path:
    sys.path.insert(0, str(_SRC))

from fno.agents.harnesses.pi import (  # noqa: E402
    SETTLED_EVENT,
    PiRpcSession,
    attach_argv,
    iter_jsonl,
    prompt_command,
    receipt_for_response,
    rpc_argv,
    steer_command,
)


def test_AC3_HP_framing_splits_on_LF_only():
    """A U+2028 inside a JSON string is CONTENT, not a record boundary."""
    payload = {"type": "message_end", "message": "line one line two"}
    stream = iter([json.dumps(payload).encode("utf-8") + b"\n"])
    events = list(iter_jsonl(stream))
    assert events == [payload], events


def test_framing_strips_an_optional_trailing_CR_and_survives_split_chunks():
    first = b'{"type": "agent_start"}\r\n{"type": "agent_se'
    second = b'ttled"}\r\n'
    events = list(iter_jsonl(iter([first, second])))
    assert [e["type"] for e in events] == ["agent_start", SETTLED_EVENT]


def test_framing_skips_a_human_readable_notice_without_aborting_the_turn():
    """pi prints "creating a new session with that id" on the same stream.

    One unparseable line must not kill a live turn, so it is skipped rather
    than raised.
    """
    stream = iter(
        [
            b"No project session found with id 'fno-1'; creating a new session with that id.\n",
            b'{"type": "agent_settled"}\n',
        ]
    )
    events = list(iter_jsonl(stream))
    assert [e["type"] for e in events] == [SETTLED_EVENT]


class _FakeStdin:
    def __init__(self) -> None:
        self.written: list[bytes] = []
        self.closed = False

    def write(self, data: bytes) -> None:
        assert not self.closed, "stdin was closed mid-turn; rpc mode exits on EOF"
        self.written.append(data)

    def flush(self) -> None:
        pass

    def close(self) -> None:
        self.closed = True


class _FakeStdout:
    def __init__(self, chunks: list[bytes]) -> None:
        self._chunks = list(chunks)

    def read1(self, _size: int) -> bytes:
        return self._chunks.pop(0) if self._chunks else b""


class _FakeProc:
    def __init__(self, chunks: list[bytes]) -> None:
        self.stdin = _FakeStdin()
        self.stdout = _FakeStdout(chunks)
        self.stderr = None

    def wait(self, timeout: float | None = None) -> int:
        return 0

    def kill(self) -> None:
        pass


def _session(chunks: list[bytes]) -> PiRpcSession:
    session = PiRpcSession("s-1", "/repo")
    session.proc = _FakeProc(chunks)  # type: ignore[assignment]
    return session


def test_AC3_HP_a_turn_settles_on_the_typed_event():
    session = _session(
        [
            b'{"type": "agent_start"}\n',
            b'{"type": "message_end", "message": {"role": "assistant"}}\n',
            b'{"type": "agent_settled"}\n',
        ]
    )
    events = session.run_turn("hello")
    assert events[-1]["type"] == SETTLED_EVENT
    sent = json.loads(session.proc.stdin.written[0])  # type: ignore[union-attr]
    assert sent == {"type": "prompt", "message": "hello", "id": "fno-1"}
    assert not session.proc.stdin.closed  # type: ignore[union-attr]


def test_AC3_EDGE_a_stream_that_merely_ends_is_not_a_completed_turn():
    """The EOF trap: five events, no assistant reply, exit 0, and it is NOT done.

    Asserting the absence of an error here would report the exact failure this
    harness produces silently.
    """
    session = _session(
        [
            b'{"type": "agent_start"}\n',
            b'{"type": "message_start", "message": {"role": "user"}}\n',
            b'{"type": "message_end", "message": {"role": "user"}}\n',
        ]
    )
    with pytest.raises(RuntimeError, match="without 'agent_settled'"):
        session.run_turn("hello")


def test_AC4_HP_mail_injection_is_a_steer_not_a_keystroke():
    """Mail to a live pi worker rides pi's own mid-turn injection.

    A steer is delivered after the current assistant turn finishes its tool
    calls and BEFORE the next LLM call, which is exactly the semantics fno's
    mail wants and what typing into a pane only approximates.
    """
    assert steer_command("stop and do this") == {
        "type": "steer",
        "message": "stop and do this",
    }
    session = _session([])
    session.steer("mid-turn note", msg_id="mail-7")
    sent = json.loads(session.proc.stdin.written[0])  # type: ignore[union-attr]
    assert sent == {"type": "steer", "message": "mid-turn note", "id": "mail-7"}


def test_a_mid_turn_prompt_must_name_a_streaming_behavior():
    """pi returns an error for a prompt sent mid-stream with no behaviour, so
    the builder makes the choice explicit rather than defaulting."""
    assert prompt_command("x")["type"] == "prompt"
    assert "streamingBehavior" not in prompt_command("x")
    assert prompt_command("x", streaming="steer")["streamingBehavior"] == "steer"


def test_AC4_EDGE_the_receipt_says_accepted_and_never_acted_on():
    accepted = receipt_for_response(
        {"type": "response", "command": "steer", "success": True}
    )
    assert "accepted" in accepted
    assert "not yet acted on" in accepted
    refused = receipt_for_response(
        {"type": "response", "command": "steer", "success": False, "error": "no run"}
    )
    assert "REFUSED" in refused and "no run" in refused


def test_both_lane_argvs_pin_provider_and_model_and_differ_only_in_mode(monkeypatch):
    """Trap 2: `--provider openai-codex` without `--model` resolves to a Bedrock
    model and dies naming an expired AWS SSO session."""
    monkeypatch.delenv("FNO_PI_PROVIDER", raising=False)
    monkeypatch.delenv("FNO_PI_MODEL", raising=False)
    driving = rpc_argv("s-1")
    watching = attach_argv("s-1")
    assert driving[:3] == ["pi", "--mode", "rpc"]
    assert "--mode" not in watching, watching
    for argv in (driving, watching):
        assert "--session-id" in argv and "s-1" in argv
        assert argv[argv.index("--provider") + 1] == "openai-codex"
        assert argv[argv.index("--model") + 1] == "gpt-5.5"


def test_the_two_crates_build_the_same_attach_argv():
    """Parity with ``fno_agents::pi::pi_attach_argv`` and
    ``fno::agents_view::pi_attach_argv``.

    Three implementations exist because `fno` never links `fno-agents` and
    Python never links either. The Rust pair is pinned against each other by
    ``the_pi_attach_argv_is_identical_in_both_crates``; this pins the Python
    one to the same literal, so a drift in any of the three fails somewhere.
    """
    assert attach_argv("01a04546-28b2-7a41-ae4c-892bbeb8e295") == [
        "pi",
        "--session-id",
        "01a04546-28b2-7a41-ae4c-892bbeb8e295",
        "--provider",
        "openai-codex",
        "--model",
        "gpt-5.5",
    ]
