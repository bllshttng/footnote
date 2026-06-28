"""Tests for fno.agents.dispatch.dispatch_send + cmd_send — Task 2.1.

Covers US3 AC3-HP / AC3-ERR / AC3-UI / AC3-EDGE / AC3-FR from the design doc.

Mocking strategy: all tests use use_tmpdir + write_registry to set up a
known-good registry state. Provider calls (send_to_session, mcp_channel_reachable)
are monkeypatched directly on the provider module to avoid real subprocess
overhead, following the pattern in test_dispatch_ask.py.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture(autouse=True)
def _no_real_mail_inject(monkeypatch):
    """Default the claude live-inject seam (node x-1f23) to 'not delivered' so no
    test shells out to a real fno-agents binary / daemon. The claude live path now
    runs `fno-agents mail-inject` over the daemon control.sock; tests that assert a
    HOSTED delivery override this with their own deterministic stub."""
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(
        dispatch_mod, "_mail_inject_claude", lambda recipient, text: False
    )


# ---------------------------------------------------------------------------
# Helper: write a registry entry for "red" (live claude peer)
# ---------------------------------------------------------------------------

def _register_claude_peer(name: str = "red", short_id: str = "abcd1234") -> None:
    """Write a single live claude AgentEntry into the registry."""
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name=name,
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/red.log",
            claude_short_id=short_id,
            status="live",
        )
    ])


def _register_codex_peer(name: str = "codex-agent") -> None:
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name=name,
            provider="codex",
            cwd="/tmp",
            log_path="/tmp/codex-agent.log",
            codex_session_id="deadbeef-0000-0000-0000-000000000001",
            status="live",
        )
    ])


def _sock_path(tmp_path: Path) -> str:
    """Return a path for a fake AF_UNIX socket."""
    return str(tmp_path / "agent.sock")


# ---------------------------------------------------------------------------
# Symbol surface
# ---------------------------------------------------------------------------

def test_dispatch_send_exported() -> None:
    """dispatch_send must be importable from fno.agents.dispatch."""
    from fno.agents import dispatch
    assert hasattr(dispatch, "dispatch_send"), "dispatch_send not found in dispatch module"


def test_dispatch_send_result_exported() -> None:
    """DispatchSendResult must be importable."""
    from fno.agents.dispatch import DispatchSendResult
    assert DispatchSendResult is not None


def test_kind_send_in_store() -> None:
    """Kind.SEND must be a member of fno.inbox.store.Kind."""
    from fno.inbox.store import Kind
    assert hasattr(Kind, "SEND"), "Kind.SEND not found in store.Kind enum"
    assert Kind.SEND.value == "send"


def test_cmd_send_registered() -> None:
    """'send' command must be registered on mail_app."""
    from fno.mail.cli import mail_app
    names = [c.name for c in mail_app.registered_commands]
    assert "send" in names, f"'send' not in mail_app commands: {names}"


# ---------------------------------------------------------------------------
# AC3-HP: live claude peer -> delivered (hosted), envelope in store
# ---------------------------------------------------------------------------

def test_dispatch_send_happy_path_live_claude(
    tmp_path: Path, monkeypatch
) -> None:
    """AC3-HP: live claude peer + live-inject success -> 'delivered (hosted)',
    exit 0. The turn is <fno_mail>-wrapped and injected over the control.sock; a
    hosted delivery is self-recording (transcript), so it is NOT also queued
    durable -- the bus is the fallback tier now (node x-1f23)."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    # MCP probe: return False so we reach the control.sock inject path.
    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)

    # The live-inject seam succeeds; capture what it was asked to inject.
    inject_calls: list[dict] = []

    def _ok_inject(recipient: str, text: str) -> bool:
        inject_calls.append({"recipient": recipient, "text": text})
        return True

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", _ok_inject)

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="red",
        message="FYI built the thing",
        provider=None,
        cwd=cwd,
        from_name="fno",
    )

    # stdout contract: "msg-<id> delivered (hosted)"
    assert result.msg_id.startswith("msg-"), f"Bad msg_id: {result.msg_id!r}"
    assert result.delivery == "hosted", f"Expected hosted, got {result.delivery!r}"

    # Exactly one live delivery attempt, carrying the paired <fno_mail> envelope.
    assert len(inject_calls) == 1
    injected = inject_calls[0]["text"]
    assert injected.startswith("<fno_mail "), f"not wrapped: {injected[:40]!r}"
    assert injected.rstrip().endswith("</fno_mail>")
    assert "FYI built the thing" in injected
    # Directed send -> the recipient's short id is stamped as the envelope `to`.
    assert 'to="abcd1234"' in injected, f"missing directed `to`: {injected[:80]!r}"

    # Bus demotion: a hosted delivery is NOT also written to the durable store.
    from fno.inbox.store import read_all_threads
    assert read_all_threads("red") == [], "hosted delivery must not queue durable"


def test_cmd_send_happy_path_stdout_format(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """AC3-HP / AC3-UI: cmd_send stdout is exactly 'msg-<id> delivered (hosted)\\n', exit 0."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)
    # Live-inject succeeds -> hosted.
    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", lambda recipient, text: True)

    from fno.mail.cli import mail_app

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = runner.invoke(
        mail_app,
        ["send", "red", "FYI built the thing", "--cwd", str(cwd)],
    )

    assert result.exit_code == 0, (result.stdout or "") + (result.stderr or "")
    out = (result.stdout or "").strip()
    # "msg-<id> delivered (hosted)"
    assert out.startswith("msg-"), f"stdout: {out!r}"
    assert "delivered (hosted)" in out, f"stdout: {out!r}"
    assert "queued" not in out, "stdout must not say 'queued' for a live delivery"


# ---------------------------------------------------------------------------
# AC3-ERR: lock-timeout -> loud stderr + nonzero (exit 11)
# ---------------------------------------------------------------------------

def test_dispatch_send_lock_timeout(tmp_path: Path, monkeypatch) -> None:
    """AC3-ERR: hold_agent_lock raises AgentLockTimeout -> DispatchAskError exit 11."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.lock import AgentLockTimeout

    # Make hold_agent_lock raise immediately
    from contextlib import contextmanager

    @contextmanager
    def _timeout_lock(*args, **kwargs):
        raise AgentLockTimeout(name="red", timeout=0.1)
        yield  # noqa: unreachable

    monkeypatch.setattr(dispatch_mod, "hold_agent_lock", _timeout_lock)

    from fno.agents.dispatch import DispatchAskError, dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_send(
            name="red",
            message="hello",
            provider=None,
            cwd=cwd,
        )
    assert exc_info.value.exit_code == 11


def test_cmd_send_lock_timeout_surfaces_on_stderr(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """AC3-ERR (CLI): lock timeout -> nonzero exit, stderr has message."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.lock import AgentLockTimeout
    from contextlib import contextmanager

    @contextmanager
    def _timeout_lock(*args, **kwargs):
        raise AgentLockTimeout(name="red", timeout=0.1)
        yield  # noqa: unreachable

    monkeypatch.setattr(dispatch_mod, "hold_agent_lock", _timeout_lock)

    from fno.mail.cli import mail_app

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = runner.invoke(
        mail_app,
        ["send", "red", "hello", "--cwd", str(cwd)],
    )
    assert result.exit_code != 0
    assert result.exit_code == 11
    stderr = result.stderr or ""
    # Some text on stderr about the failure
    assert len(stderr.strip()) > 0, "stderr must not be empty on lock timeout"


# ---------------------------------------------------------------------------
# AC3-UI: stdout distinguishes delivered vs queued
# ---------------------------------------------------------------------------

def test_dispatch_send_durable_queued_output(tmp_path: Path, monkeypatch) -> None:
    """AC3-UI: when peer is live but socket send fails, output says 'queued (durable)'."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers.claude import ProviderSocketError
    from fno.agents.providers._claude_session_registry import SessionLocator

    # locate_session succeeds but send_to_session fails
    monkeypatch.setattr(
        claude_mod, "locate_session",
        lambda short_id, home=None: SessionLocator(
            pid=12345, short_id=short_id,
            messaging_socket_path=_sock_path(tmp_path),
            jobs_dir=tmp_path / "jobs",
        ),
    )
    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)

    def _failing_send(sock_path: str, content: str, from_name: str) -> None:
        raise ProviderSocketError("connection refused")

    monkeypatch.setattr(claude_mod, "send_to_session", _failing_send)

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="red",
        message="FYI done",
        provider=None,
        cwd=cwd,
    )

    assert result.delivery == "durable", f"Expected durable, got {result.delivery!r}"
    assert result.msg_id.startswith("msg-")


def test_dispatch_send_offline_peer_queued(tmp_path: Path, monkeypatch) -> None:
    """AC3-UI: orphaned peer -> durable, output says 'queued (durable)'."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name="red",
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/red.log",
            claude_short_id="abcd1234",
            status="orphaned",
        )
    ])

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="red",
        message="FYI done",
        provider=None,
        cwd=cwd,
    )

    assert result.delivery == "durable"
    assert result.msg_id.startswith("msg-")


def test_cmd_send_queued_stdout_format(tmp_path: Path, monkeypatch, runner: CliRunner) -> None:
    """AC3-UI (CLI): durable path stdout is 'msg-<id> queued (durable)\\n'."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name="red",
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/red.log",
            claude_short_id="abcd1234",
            status="orphaned",
        )
    ])

    from fno.mail.cli import mail_app

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = runner.invoke(
        mail_app,
        ["send", "red", "hello", "--cwd", str(cwd)],
    )
    assert result.exit_code == 0, (result.stdout or "") + (result.stderr or "")
    out = (result.stdout or "").strip()
    assert out.startswith("msg-"), f"stdout: {out!r}"
    assert "queued (durable)" in out, f"stdout: {out!r}"
    assert "delivered" not in out, "stdout must not say 'delivered' for durable path"


# ---------------------------------------------------------------------------
# AC3-EDGE: 200KB body lands intact; >1MiB body is rejected before envelope
# ---------------------------------------------------------------------------

def test_dispatch_send_200kb_body_round_trip(tmp_path: Path, monkeypatch) -> None:
    """AC3-EDGE: 200KB body lands intact through the store write."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers._claude_session_registry import SessionLocator

    monkeypatch.setattr(
        claude_mod, "locate_session",
        lambda short_id, home=None: SessionLocator(
            pid=12345, short_id=short_id,
            messaging_socket_path=_sock_path(tmp_path),
            jobs_dir=tmp_path / "jobs",
        ),
    )
    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)
    monkeypatch.setattr(claude_mod, "send_to_session", lambda *a, **kw: None)

    body = "x" * (200 * 1024)  # 200KB

    from fno.agents.dispatch import dispatch_send
    from fno.inbox.store import read_all_threads

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="red",
        message=body,
        provider=None,
        cwd=cwd,
    )

    assert result.msg_id.startswith("msg-")
    threads = read_all_threads("red")
    assert len(threads) == 1
    stored_body = threads[0].messages[0].body
    assert stored_body == body, f"Round-trip mismatch: got {len(stored_body)} chars"


def test_dispatch_send_rejects_over_1mib_body(tmp_path: Path, monkeypatch) -> None:
    """AC3-EDGE: body > 1MiB -> exit 2 BEFORE any envelope is written."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents.dispatch import DispatchAskError, dispatch_send
    from fno.inbox.store import read_all_threads

    body = "x" * (1024 * 1024 + 1)  # 1MiB + 1 byte

    cwd = tmp_path / "work"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_send(
            name="red",
            message=body,
            provider=None,
            cwd=cwd,
        )

    assert exc_info.value.exit_code == 2
    # No envelope should have been written
    threads = read_all_threads("red")
    assert len(threads) == 0, f"No envelope should be written on body-size rejection"


# ---------------------------------------------------------------------------
# AC3-FR: peer dies between resolve and inject -> envelope still durable
# ---------------------------------------------------------------------------

def test_dispatch_send_demotion_preserves_envelope(tmp_path: Path, monkeypatch) -> None:
    """AC3-FR: peer resolves live but the inject does not land -> envelope durable,
    no retry. The live inject failing partway must never lose the message; the
    durable fallback is the recovery record (node x-1f23)."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)

    inject_attempt_count = [0]

    def _fail_inject(recipient: str, text: str) -> bool:
        inject_attempt_count[0] += 1
        return False

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", _fail_inject)

    from fno.agents.dispatch import dispatch_send
    from fno.inbox.store import read_all_threads

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="red",
        message="important message",
        provider=None,
        cwd=cwd,
    )

    # Durable fallback, not a hard failure
    assert result.delivery == "durable"
    assert result.msg_id.startswith("msg-")

    # Exactly ONE attempt, no retry storm
    assert inject_attempt_count[0] == 1, f"Expected 1 inject attempt, got {inject_attempt_count[0]}"

    # Envelope is in the store (survived the failed inject)
    threads = read_all_threads("red")
    assert len(threads) == 1, f"Envelope must survive inject failure; got {len(threads)} threads"
    assert "important message" in threads[0].messages[0].body


# ---------------------------------------------------------------------------
# Unknown agent -> exit 16, no envelope written
# ---------------------------------------------------------------------------

def test_dispatch_send_unknown_agent(tmp_path: Path, monkeypatch) -> None:
    """Unknown agent name -> exit 16, no envelope written (mirrors ask behavior)."""
    use_tmpdir(monkeypatch, tmp_path)
    # Empty registry - no agents registered

    from fno.agents.dispatch import (
        DispatchAskError,
        UNKNOWN_AGENT_EXIT_CODE,
        dispatch_send,
    )
    from fno.inbox.store import read_all_threads

    cwd = tmp_path / "work"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_send(
            name="blue",
            message="hi",
            provider=None,
            cwd=cwd,
        )

    assert exc_info.value.exit_code == UNKNOWN_AGENT_EXIT_CODE
    # Error message byte-identical to ask's unknown-agent error
    msg = str(exc_info.value)
    assert "unknown agent" in msg
    assert "spawn it first" in msg
    assert "blue" in msg

    # No envelope written
    threads = read_all_threads("blue")
    assert len(threads) == 0


def test_cmd_send_unknown_agent_exit16(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """CLI: unknown agent -> exit 16."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.mail.cli import mail_app
    from fno.agents.dispatch import UNKNOWN_AGENT_EXIT_CODE

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = runner.invoke(
        mail_app,
        ["send", "blue", "hi", "--cwd", str(cwd)],
    )
    assert result.exit_code == UNKNOWN_AGENT_EXIT_CODE


# ---------------------------------------------------------------------------
# Codex/gemini peer -> durable (inject seam not yet wired)
# ---------------------------------------------------------------------------

def test_dispatch_send_codex_peer_queued_durable(tmp_path: Path, monkeypatch) -> None:
    """Codex peer -> queued (durable) via the not-yet-wired injection seam."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_codex_peer()

    from fno.agents.dispatch import dispatch_send
    from fno.inbox.store import read_all_threads

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="codex-agent",
        message="hey codex",
        provider=None,
        cwd=cwd,
    )

    assert result.delivery == "durable"
    assert result.msg_id.startswith("msg-")

    # Envelope is in the store
    threads = read_all_threads("codex-agent")
    assert len(threads) == 1


# ---------------------------------------------------------------------------
# Events: agent_send_started / agent_send_done emitted
# ---------------------------------------------------------------------------

def test_dispatch_send_emits_send_events(tmp_path: Path, monkeypatch) -> None:
    """dispatch_send emits agent_send_started and agent_send_done events."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers._claude_session_registry import SessionLocator

    monkeypatch.setattr(
        claude_mod, "locate_session",
        lambda short_id, home=None: SessionLocator(
            pid=12345, short_id=short_id,
            messaging_socket_path=_sock_path(tmp_path),
            jobs_dir=tmp_path / "jobs",
        ),
    )
    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)
    monkeypatch.setattr(claude_mod, "send_to_session", lambda *a, **kw: None)

    from fno import paths
    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    dispatch_send(
        name="red",
        message="test event emission",
        provider=None,
        cwd=cwd,
    )

    events_log = paths.state_dir() / "events.jsonl"
    assert events_log.exists(), "events.jsonl must be written"
    body = events_log.read_text(encoding="utf-8")

    assert "agent_send_started" in body, "agent_send_started not in events"
    assert "agent_send_done" in body, "agent_send_done not in events"

    # Verify the done event has a delivery field
    for line in body.splitlines():
        record = json.loads(line)
        if record.get("kind") == "agent_send_done":
            assert "delivery" in record, "agent_send_done must carry 'delivery' field"
            assert record["delivery"] in ("hosted", "durable")
            break
    else:
        pytest.fail("agent_send_done event not found in events.jsonl")


# ---------------------------------------------------------------------------
# F1 (sigma HIGH): envelope write failure -> exit 12, agent_send_failed event
# ---------------------------------------------------------------------------

def test_dispatch_send_envelope_write_oserror_exit12(tmp_path: Path, monkeypatch) -> None:
    """F1: write_new_thread raises OSError -> DispatchAskError exit 12, agent_send_failed emitted."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers._claude_session_registry import SessionLocator

    monkeypatch.setattr(
        claude_mod, "locate_session",
        lambda short_id, home=None: SessionLocator(
            pid=12345, short_id=short_id,
            messaging_socket_path=str(tmp_path / "agent.sock"),
            jobs_dir=tmp_path / "jobs",
        ),
    )
    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)
    monkeypatch.setattr(claude_mod, "send_to_session", lambda *a, **kw: None)

    from fno.inbox import store as store_mod
    monkeypatch.setattr(store_mod, "write_new_thread", lambda **kw: (_ for _ in ()).throw(OSError("disk full")))

    captured: list = []
    from fno.agents import events as events_mod
    orig_emit = events_mod.emit
    def capture_emit(kind, **kw):
        captured.append((kind, kw))
        orig_emit(kind, **kw)
    monkeypatch.setattr(events_mod, "emit", capture_emit)

    from fno.agents.dispatch import DispatchAskError, dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_send(name="red", message="hello", provider=None, cwd=cwd)

    assert exc_info.value.exit_code == 12, f"Expected exit 12, got {exc_info.value.exit_code}"
    assert "envelope-write" in str(exc_info.value) or "envelope write" in str(exc_info.value).lower()

    failed_events = [e for e in captured if e[0] == "agent_send_failed"]
    assert failed_events, "agent_send_failed must be emitted on OSError"
    assert any(e[1].get("stage") == "envelope-write" for e in failed_events), (
        f"agent_send_failed must carry stage='envelope-write'; got {failed_events}"
    )


def test_dispatch_send_envelope_write_valueerror_exit12(tmp_path: Path, monkeypatch) -> None:
    """F1: write_new_thread raises ValueError -> DispatchAskError exit 12."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers._claude_session_registry import SessionLocator

    monkeypatch.setattr(
        claude_mod, "locate_session",
        lambda short_id, home=None: SessionLocator(
            pid=12345, short_id=short_id,
            messaging_socket_path=str(tmp_path / "agent.sock"),
            jobs_dir=tmp_path / "jobs",
        ),
    )
    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)
    monkeypatch.setattr(claude_mod, "send_to_session", lambda *a, **kw: None)

    from fno.inbox import store as store_mod
    monkeypatch.setattr(store_mod, "write_new_thread", lambda **kw: (_ for _ in ()).throw(ValueError("suffix exhausted")))

    from fno.agents.dispatch import DispatchAskError, dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_send(name="red", message="hello", provider=None, cwd=cwd)

    assert exc_info.value.exit_code == 12


# ---------------------------------------------------------------------------
# F2 (sigma MEDIUM): send events must carry context envelope fields
# ---------------------------------------------------------------------------

def test_dispatch_send_events_carry_context_envelope(tmp_path: Path, monkeypatch) -> None:
    """F2: agent_send_started/done events must carry request_id/from_name/caller_kind
    via the EventContext envelope (mirrors dispatch_ask's AC4-HP)."""
    use_tmpdir(monkeypatch, tmp_path)
    # build_context honors from_name_override ONLY in human_cli mode; pin the
    # env so caller_kind_from_env() does not resolve to "cron" (GitHub Actions
    # sets INVOCATION_ID, which would pin from_name to "cron" and ignore the
    # override). Clearing all four discriminator keys makes the override path
    # deterministic regardless of the host environment.
    for _var in ("FNO_AGENT_SELF", "MCP_CHANNEL_INBOUND_POKE", "CRON_JOB", "INVOCATION_ID"):
        monkeypatch.delenv(_var, raising=False)
    _register_claude_peer()

    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers._claude_session_registry import SessionLocator

    monkeypatch.setattr(
        claude_mod, "locate_session",
        lambda short_id, home=None: SessionLocator(
            pid=12345, short_id=short_id,
            messaging_socket_path=str(tmp_path / "agent.sock"),
            jobs_dir=tmp_path / "jobs",
        ),
    )
    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)
    monkeypatch.setattr(claude_mod, "send_to_session", lambda *a, **kw: None)

    captured: list = []
    from fno.agents import events as events_mod
    orig_emit = events_mod.emit
    def capture_emit(kind, **kw):
        captured.append((kind, dict(kw)))
        orig_emit(kind, **kw)
    monkeypatch.setattr(events_mod, "emit", capture_emit)

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    dispatch_send(name="red", message="envelope test", provider=None, cwd=cwd, from_name="tester")

    started = [e for e in captured if e[0] == "agent_send_started"]
    done = [e for e in captured if e[0] == "agent_send_done"]

    assert started, "agent_send_started not captured"
    assert done, "agent_send_done not captured"

    for label, evs in (("started", started), ("done", done)):
        payload = evs[0][1]
        assert "request_id" in payload, f"agent_send_{label} lacks request_id"
        assert payload.get("from_name") == "tester", (
            f"agent_send_{label} from_name mismatch: {payload.get('from_name')!r}"
        )

    # started and done must share the same request_id
    import re
    REQUEST_ID_RE = re.compile(r"^[a-f0-9]{32}$")
    rid_started = started[0][1]["request_id"]
    rid_done = done[0][1]["request_id"]
    assert rid_started == rid_done, f"request_id mismatch: {rid_started!r} vs {rid_done!r}"
    assert REQUEST_ID_RE.match(rid_started), f"request_id format bad: {rid_started!r}"


# ---------------------------------------------------------------------------
# Rust routing guard: 'send' must NOT be in RUST_CLIENT_VERBS
# ---------------------------------------------------------------------------

def test_send_not_in_rust_client_verbs() -> None:
    """'send' must not be in RUST_CLIENT_VERBS (Python owns send in G2)."""
    from fno.agents.rust_runtime import RUST_CLIENT_VERBS
    assert "send" not in RUST_CLIENT_VERBS, (
        "'send' must NOT be in RUST_CLIENT_VERBS - Python owns this verb in Group 2"
    )


# ---------------------------------------------------------------------------
# US2 (ab-098967b4): send by discovered live-session handle
# ---------------------------------------------------------------------------

def test_us2_send_by_handle_routes_to_project(runner, tmp_path, monkeypatch):
    """AC2-HP: a bare <handle> that is a live discovered session (not a
    registered agent) resolves to its project and rides the --to-project bus."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import write_registry

    write_registry([])  # empty -> dispatch_send raises unknown-agent (exit 16)

    from fno.agents import discover as discover_mod
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.discover import DiscoveredSession

    fake = DiscoveredSession(
        session_id="uuid-tgt", short_id="tgt00001", handle="fno-tgt00001",
        pid=123, cwd="/x/abilities", project="fno", status="idle",
    )
    monkeypatch.setattr(discover_mod, "resolve_or_suggest", lambda h, **kw: (fake, []))

    captured: dict = {}

    class _Res:
        delivery = "durable"
        msg_id = "msg-42"
        recipient = None

    def fake_to_project(project, message, **kw):
        captured["project"] = project
        captured["message"] = message
        return _Res()

    monkeypatch.setattr(dispatch_mod, "dispatch_send_to_project", fake_to_project)

    from fno.mail.cli import mail_app

    res = runner.invoke(
        mail_app, ["send", "fno-tgt00001", "does advance() resolve cwd?"]
    )
    assert res.exit_code == 0, res.output
    assert captured["project"] == "fno"
    assert captured["message"] == "does advance() resolve cwd?"
    assert "msg-42" in res.output
    assert "fno-tgt00001" in res.output  # handle echoed in context


def test_us2_unknown_handle_errors_with_suggestions(runner, tmp_path, monkeypatch):
    """AC2-ERR: an unknown handle errors with the closest live handles, sending
    nothing (dispatch_send_to_project is never called)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import write_registry

    write_registry([])

    from fno.agents import discover as discover_mod
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(
        discover_mod, "resolve_or_suggest",
        lambda h, **kw: (None, ["fno-think001", "fno-tgt00001"]),
    )

    def _boom(*a, **k):  # must NOT be called
        raise AssertionError("must not send on an unknown handle")

    monkeypatch.setattr(dispatch_mod, "dispatch_send_to_project", _boom)

    from fno.mail.cli import mail_app

    res = runner.invoke(mail_app, ["send", "nope", "hi"])
    assert res.exit_code != 0
    assert "Closest live sessions" in res.output
    assert "fno-think001" in res.output
