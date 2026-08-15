"""Tests for fno.agents.dispatch.dispatch_send + cmd_send — Task 2.1.

Covers US3 AC3-HP / AC3-ERR / AC3-UI / AC3-EDGE / AC3-FR from the design doc.

Mocking strategy: all tests use use_tmpdir + write_registry to set up a
known-good registry state. Provider calls (send_to_session, mcp_channel_reachable)
are monkeypatched directly on the provider module to avoid real subprocess
overhead, following the pattern in test_dispatch_ask.py.
"""
from __future__ import annotations

import fcntl
import json
import time
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
        dispatch_mod, "_mail_inject_claude", lambda recipient, text, **_k: False
    )
    monkeypatch.setattr(
        dispatch_mod, "_registered_family1_state", lambda _entry: "working"
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
            harness="claude",
            harness_session_id="abcd1234-1111-7222-8333-444455556666",
            cwd="/tmp",
            log_path="/tmp/red.log",
            short_id=short_id,
            status="live",
        )
    ])


def _register_codex_peer(name: str = "codex-agent") -> None:
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name=name,
            harness="codex",
            cwd="/tmp",
            log_path="/tmp/codex-agent.log",
            harness_session_id="deadbeef-0000-0000-0000-000000000001",
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
    from fno.agents.harnesses import claude as claude_mod

    # MCP probe: return False so we reach the control.sock inject path.
    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)

    # The live-inject seam succeeds; capture what it was asked to inject.
    inject_calls: list[dict] = []

    def _ok_inject(recipient: str, text: str, **_k) -> bool:
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
    # US1 / Locked Decision 8: the registered-agent live path carries the SAME
    # minted id as the receipt, so the recipient can reply --to it and a
    # bounded-duplicate is dedupable (codex P1 - _deliver_live is the 2nd choke
    # point, not just _name_lane_send).
    assert f'id="{result.msg_id}"' in injected, f"missing envelope id: {injected[:100]!r}"

    # Bus demotion: a hosted delivery is NOT also written to the durable store.
    from fno.inbox.store import read_all_threads
    assert read_all_threads("abcd1234") == [], "hosted delivery must not queue durable"


def test_cmd_send_happy_path_stdout_format(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """AC3-HP / AC3-UI: cmd_send stdout is exactly 'msg-<id> delivered (hosted)\\n', exit 0."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)
    # Live-inject succeeds -> hosted.
    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", lambda recipient, text, **_k: True)

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
        yield

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


@pytest.mark.parametrize("address", ["red", "abcd1234", "deadbeef"])
def test_dispatch_send_locks_canonical_registry_name(
    tmp_path: Path, monkeypatch, address: str
) -> None:
    """Name and transport/canonical addresses serialize on one lock file."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name="red",
            harness="claude",
            harness_session_id="019fb417-1111-7222-8333-4444deadbeef",
            cwd="/tmp",
            log_path="/tmp/red.log",
            short_id="abcd1234",
            status="live",
        )
    ])

    from contextlib import contextmanager
    from fno.agents import dispatch as dispatch_mod

    locked: list[str] = []

    @contextmanager
    def _record_lock(lock_name, *_args, **_kwargs):
        locked.append(lock_name)
        yield object()

    monkeypatch.setattr(dispatch_mod, "hold_agent_lock", _record_lock)
    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", lambda *_args, **_k: True)

    result = dispatch_mod.dispatch_send(
        name=address,
        message="hello",
        provider=None,
        cwd=tmp_path,
    )

    assert result.delivery == "hosted"
    assert locked == ["red"]


def test_dispatch_send_refuses_address_owner_change_under_lock(
    tmp_path: Path, monkeypatch
) -> None:
    """The post-lock resolution must still name the row whose lock is held."""
    use_tmpdir(monkeypatch, tmp_path)
    from contextlib import contextmanager
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.registry import AgentEntry, ResolvedAgent, write_registry

    red = AgentEntry(
        name="red",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/red.log",
        short_id="abcd1234",
        status="live",
    )
    blue = AgentEntry(
        name="blue",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/blue.log",
        short_id="beef1234",
        status="live",
    )
    write_registry([red, blue])
    calls = {"count": 0}

    def staged(_entries, _token):
        calls["count"] += 1
        return ResolvedAgent(
            entry=red if calls["count"] == 1 else blue,
            matched_by="name",
        )

    @contextmanager
    def unlocked(*_args, **_kwargs):
        yield object()

    monkeypatch.setattr(dispatch_mod, "resolve_registered_agent_across_sources", staged)
    monkeypatch.setattr(dispatch_mod, "hold_agent_lock", unlocked)
    monkeypatch.setattr(
        dispatch_mod,
        "_mail_inject_claude",
        lambda *_args: pytest.fail("delivery ran after address owner changed"),
    )

    with pytest.raises(dispatch_mod.DispatchAskError, match="changed from 'red' to 'blue'"):
        dispatch_mod.dispatch_send(
            name="abcd1234",
            message="hello",
            provider=None,
            cwd=tmp_path,
        )

    assert calls["count"] == 2


def test_dispatch_send_refuses_same_name_identity_change_under_lock(
    tmp_path: Path, monkeypatch
) -> None:
    """Replacing one registry name with another session cannot redirect send."""
    use_tmpdir(monkeypatch, tmp_path)
    from contextlib import contextmanager
    from fno.agents import discover as discover_mod
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.registry import AgentEntry, ResolvedAgent, write_registry
    from fno.inbox.store import read_all_threads

    original = AgentEntry(
        name="red",
        harness="claude",
        harness_session_id="aaaaaaaa-1111-7222-8333-4444deadbeef",
        cwd="/tmp",
        log_path="/tmp/red.log",
        short_id="transport",
        status="live",
    )
    replacement = AgentEntry(
        name="red",
        harness="codex",
        harness_session_id="bbbbbbbb-1111-7222-8333-4444cafefeed",
        cwd="/tmp",
        log_path="/tmp/red-new.log",
        status="live",
    )
    write_registry([original])
    calls = {"count": 0}

    def staged(_entries, _token):
        calls["count"] += 1
        return ResolvedAgent(
            entry=original if calls["count"] == 1 else replacement,
            matched_by="name",
        )

    @contextmanager
    def unlocked(*_args, **_kwargs):
        yield object()

    monkeypatch.setattr(dispatch_mod, "resolve_registered_agent_across_sources", staged)
    monkeypatch.setattr(dispatch_mod, "hold_agent_lock", unlocked)
    monkeypatch.setattr(discover_mod, "discovery_address_matches", lambda *_a, **_k: [])
    monkeypatch.setattr(dispatch_mod, "_registered_family1_state", lambda _entry: "working")
    delivered: list[str] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_deliver_live",
        lambda entry, *_a, **_k: delivered.append(entry.harness) or True,
    )

    with pytest.raises(dispatch_mod.DispatchAskError, match="recipient identity changed"):
        dispatch_mod.dispatch_send(
            name="red",
            message="hello",
            provider=None,
            cwd=tmp_path,
        )

    assert calls["count"] == 2
    assert delivered == []
    assert read_all_threads("abcd1234") == []


@pytest.mark.parametrize(
    ("original_fields", "replacement_fields"),
    [
        (
            {"mcp_channel_id": "mcp-original"},
            {"mcp_channel_id": "mcp-replacement"},
        ),
        (
            {"mux": {"session": "main", "pane_id": 11}},
            {"mux": {"session": "main", "pane_id": 12}},
        ),
    ],
)
def test_dispatch_send_refuses_same_name_route_change_under_lock(
    tmp_path: Path,
    monkeypatch,
    original_fields: dict,
    replacement_fields: dict,
) -> None:
    """Replacing an MCP or mux route under one name cannot redirect send."""
    use_tmpdir(monkeypatch, tmp_path)
    from contextlib import contextmanager
    from fno.agents import discover as discover_mod
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.registry import AgentEntry, ResolvedAgent, write_registry
    from fno.inbox.store import read_all_threads

    original = AgentEntry(
        name="red",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/red.log",
        status="live",
        created_at="2026-07-30T10:00:00Z",
        **original_fields,
    )
    replacement = AgentEntry(
        name="red",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/red-new.log",
        status="live",
        created_at="2026-07-30T10:00:01Z",
        **replacement_fields,
    )
    write_registry([original])
    calls = {"count": 0}

    def staged(_entries, _token):
        calls["count"] += 1
        return ResolvedAgent(
            entry=original if calls["count"] == 1 else replacement,
            matched_by="name",
        )

    @contextmanager
    def unlocked(*_args, **_kwargs):
        yield object()

    monkeypatch.setattr(dispatch_mod, "resolve_registered_agent_across_sources", staged)
    monkeypatch.setattr(dispatch_mod, "hold_agent_lock", unlocked)
    monkeypatch.setattr(discover_mod, "discovery_address_matches", lambda *_a, **_k: [])
    monkeypatch.setattr(dispatch_mod, "_registered_family1_state", lambda _entry: "working")
    delivered: list[str] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_deliver_live",
        lambda entry, *_a, **_k: delivered.append(entry.name) or True,
    )

    with pytest.raises(dispatch_mod.DispatchAskError, match="recipient identity changed"):
        dispatch_mod.dispatch_send(
            name="red",
            message="hello",
            provider=None,
            cwd=tmp_path,
        )

    assert calls["count"] == 2
    assert delivered == []
    assert read_all_threads("abcd1234") == []


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
        yield

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

    from fno.agents.harnesses import claude as claude_mod
    from fno.agents.harnesses.claude import ProviderSocketError
    from fno.agents.harnesses._claude_session_registry import SessionLocator

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
            harness="claude",
            harness_session_id="abcd1234-1111-7222-8333-444455556666",
            cwd="/tmp",
            log_path="/tmp/red.log",
            short_id="abcd1234",
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


def test_dispatch_send_stale_orphaned_status_uses_live_family1(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name="red", harness="claude", cwd="/tmp", log_path="/tmp/red.log",
            short_id="abcd1234", status="orphaned",
        )
    ])
    monkeypatch.setattr(
        dispatch_mod, "_registered_family1_state", lambda _entry: "watching"
    )
    monkeypatch.setattr(dispatch_mod, "_deliver_live", lambda *a, **k: True)

    result = dispatch_mod.dispatch_send(
        name="red", message="ping", provider=None, cwd=tmp_path
    )

    assert result.delivery == "hosted"


@pytest.mark.parametrize("state", ["done", "stalled"])
def test_dispatch_send_nonlive_family1_never_attempts_live_delivery(
    tmp_path: Path, monkeypatch, state: str
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(
        dispatch_mod, "_registered_family1_state", lambda _entry: state
    )
    attempts: list = []
    monkeypatch.setattr(
        dispatch_mod, "_deliver_live", lambda *a, **k: attempts.append(a) or True
    )

    result = dispatch_mod.dispatch_send(
        name="red", message="ping", provider=None, cwd=tmp_path
    )

    assert result.delivery == "durable"
    assert attempts == []


def test_dispatch_send_unknown_family1_attempts_confirmable_transport(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(
        dispatch_mod, "_registered_family1_state", lambda _entry: "unknown"
    )
    attempts: list = []
    monkeypatch.setattr(
        dispatch_mod, "_deliver_live", lambda *a, **k: attempts.append(a) or True
    )

    result = dispatch_mod.dispatch_send(
        name="red", message="ping", provider=None, cwd=tmp_path
    )

    assert result.delivery == "hosted"
    assert len(attempts) == 1


def test_cmd_send_queued_stdout_format(tmp_path: Path, monkeypatch, runner: CliRunner) -> None:
    """AC3-UI (CLI): durable path stdout is 'msg-<id> queued (durable)\\n'."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name="red",
            harness="claude",
            harness_session_id="abcd1234-1111-7222-8333-444455556666",
            cwd="/tmp",
            log_path="/tmp/red.log",
            short_id="abcd1234",
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

    from fno.agents.harnesses import claude as claude_mod
    from fno.agents.harnesses._claude_session_registry import SessionLocator

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
    threads = read_all_threads("abcd1234")
    assert len(threads) == 1
    stored_body = threads[0].messages[0].body
    # The durable body is <fno_mail>-wrapped now (node x-1f23); the 200KB message
    # round-trips intact inside the paired envelope.
    assert stored_body.startswith("<fno_mail "), stored_body[:40]
    assert stored_body.rstrip().endswith("</fno_mail>")
    inner = stored_body.split("\n", 1)[1].rsplit("\n", 1)[0]
    assert inner == body, f"Round-trip mismatch: got {len(inner)} chars"


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
    threads = read_all_threads("abcd1234")
    assert len(threads) == 0, "No envelope should be written on body-size rejection"


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
    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)

    inject_attempt_count = [0]

    def _fail_inject(recipient: str, text: str, **_k) -> bool:
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
    threads = read_all_threads("abcd1234")
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
    threads = read_all_threads("deadbeef")
    assert len(threads) == 1


# ---------------------------------------------------------------------------
# Events: agent_send_started / agent_send_done emitted
# ---------------------------------------------------------------------------

def test_dispatch_send_emits_send_events(tmp_path: Path, monkeypatch) -> None:
    """dispatch_send emits agent_send_started and agent_send_done events."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents.harnesses import claude as claude_mod
    from fno.agents.harnesses._claude_session_registry import SessionLocator

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


def test_dispatch_send_reports_registry_stamp_failure_after_hosted_delivery(
    tmp_path: Path,
    monkeypatch,
    capsys,
) -> None:
    """A post-delivery stamp failure stays successful but cannot be silent."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents import dispatch as dispatch_mod
    from fno.inbox.store import read_all_threads

    monkeypatch.setattr(dispatch_mod, "_registered_family1_state", lambda _entry: "working")
    monkeypatch.setattr(dispatch_mod, "_deliver_live", lambda *_args, **_kwargs: True)
    monkeypatch.setattr(
        dispatch_mod,
        "update_registry",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(ValueError("corrupt registry")),
    )
    captured: list[tuple[str, dict]] = []
    monkeypatch.setattr(
        dispatch_mod.events,
        "emit",
        lambda kind, **data: captured.append((kind, data)),
    )

    result = dispatch_mod.dispatch_send(
        name="red",
        message="hello",
        provider=None,
        cwd=tmp_path,
    )

    assert result.delivery == "hosted"
    assert read_all_threads("abcd1234") == []
    assert any(
        kind == "agent_send_failed"
        and data.get("stage") == "registry-write"
        and data.get("delivery") == "hosted"
        for kind, data in captured
    )
    stderr = capsys.readouterr().err
    assert "registry stamp failed after hosted delivery" in stderr
    assert "do not retry" in stderr


def test_dispatch_send_registry_stamp_lock_is_bounded_after_hosted_delivery(
    tmp_path: Path,
    monkeypatch,
    capsys,
) -> None:
    """A completed hosted send cannot wait forever before returning its receipt."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno import paths
    from fno.agents import dispatch as dispatch_mod
    from fno.agents import registry as registry_mod

    monkeypatch.setattr(dispatch_mod, "_deliver_live", lambda *_args, **_kwargs: True)
    registry_path = paths.agents_registry_path()
    lock_path = registry_mod._registry_lock_path(registry_path)
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    holder = open(lock_path, "w")
    fcntl.flock(holder.fileno(), fcntl.LOCK_EX)
    started = time.monotonic()
    try:
        result = dispatch_mod.dispatch_send(
            name="red",
            message="hello",
            provider=None,
            cwd=tmp_path,
            registry_stamp_timeout_seconds=0.05,
        )
    finally:
        fcntl.flock(holder.fileno(), fcntl.LOCK_UN)
        holder.close()

    assert time.monotonic() - started < 2.0
    assert result.delivery == "hosted"
    stderr = capsys.readouterr().err
    assert "registry stamp failed after hosted delivery" in stderr
    assert "do not retry" in stderr


def test_dispatch_send_does_not_stamp_recipient_restamped_during_delivery(
    tmp_path: Path,
    monkeypatch,
    capsys,
) -> None:
    """A delivered send cannot stamp a replacement row selected after delivery."""
    use_tmpdir(monkeypatch, tmp_path)
    original_id = "aaaaaaaa-1111-7222-8333-4444deadbeef"
    replacement_id = "bbbbbbbb-1111-7222-8333-4444cafefeed"

    from fno.agents import dispatch as dispatch_mod
    from fno.agents import registry as registry_mod

    registry_mod.write_registry([
        registry_mod.AgentEntry(
            name="victim",
            harness="claude",
            harness_session_id=original_id,
            short_id="transportA",
            cwd="/tmp",
            log_path="/tmp/victim.log",
            status="orphaned",
        )
    ])
    monkeypatch.setattr(
        dispatch_mod, "_registered_family1_state", lambda _entry: "working"
    )

    def restamp_during_delivery(*_args, **_kwargs):
        registry_mod.restamp_harness_session_id(
            name="victim",
            harness="claude",
            session_id=replacement_id,
        )
        return True

    monkeypatch.setattr(dispatch_mod, "_deliver_live", restamp_during_delivery)
    captured: list[tuple[str, dict]] = []
    monkeypatch.setattr(
        dispatch_mod.events,
        "emit",
        lambda kind, **data: captured.append((kind, data)),
    )

    result = dispatch_mod.dispatch_send(
        name="victim",
        message="hello",
        provider=None,
        cwd=tmp_path,
    )

    assert result.delivery == "hosted"
    rows = registry_mod.load_registry()
    assert len(rows) == 1
    assert rows[0].harness_session_id == replacement_id
    assert rows[0].status == "orphaned"
    assert rows[0].last_message_at is None
    assert any(
        kind == "agent_send_failed"
        and data.get("stage") == "registry-write"
        and data.get("reason") == "recipient_identity_changed"
        for kind, data in captured
    )
    stderr = capsys.readouterr().err
    assert "recipient identity changed" in stderr
    assert "do not retry" in stderr


def test_dispatch_send_queues_to_selected_session_when_live_miss_restamps(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """A live miss queues to A's canonical mailbox even when A restamps to B."""
    use_tmpdir(monkeypatch, tmp_path)
    original_id = "aaaaaaaa-1111-7222-8333-4444deadbeef"
    replacement_id = "bbbbbbbb-1111-7222-8333-4444cafefeed"

    from fno.agents import dispatch as dispatch_mod
    from fno.agents import registry as registry_mod
    from fno.harness_identity import canonical_handle
    from fno.inbox.store import read_all_threads

    registry_mod.write_registry([
        registry_mod.AgentEntry(
            name="victim",
            harness="claude",
            harness_session_id=original_id,
            short_id="transportA",
            cwd="/tmp",
            log_path="/tmp/victim.log",
            status="live",
        )
    ])
    monkeypatch.setattr(
        dispatch_mod, "_registered_family1_state", lambda _entry: "working"
    )

    def restamp_then_miss(*_args, **_kwargs):
        registry_mod.restamp_harness_session_id(
            name="victim",
            harness="claude",
            session_id=replacement_id,
        )
        return False

    monkeypatch.setattr(dispatch_mod, "_deliver_live", restamp_then_miss)

    result = dispatch_mod.dispatch_send(
        name=original_id,
        message="secret for A",
        provider=None,
        cwd=tmp_path,
    )

    assert result.delivery == "durable"
    original_threads = read_all_threads(canonical_handle(original_id))
    assert len(original_threads) == 1
    assert original_threads[0].messages[0].body.endswith("secret for A\n</fno_mail>")
    assert f'to="{canonical_handle(original_id)}"' in original_threads[0].messages[0].body
    assert read_all_threads(canonical_handle(replacement_id)) == []
    assert read_all_threads("victim") == []
    row = registry_mod.load_registry()[0]
    assert row.harness_session_id == replacement_id
    assert row.last_message_at is None


def test_dispatch_send_refuses_durable_fallback_without_full_session_id(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """A mutable name or transport key is not a durable session address."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents import dispatch as dispatch_mod
    from fno.agents import registry as registry_mod
    from fno.inbox.store import read_all_threads

    registry_mod.write_registry([
        registry_mod.AgentEntry(
            name="legacy",
            harness="claude",
            short_id="transportA",
            cwd="/tmp",
            log_path="/tmp/legacy.log",
            status="orphaned",
        )
    ])
    monkeypatch.setattr(
        dispatch_mod, "_registered_family1_state", lambda _entry: "done"
    )

    with pytest.raises(dispatch_mod.DispatchAskError, match="no full harness session id") as exc:
        dispatch_mod.dispatch_send(
            name="legacy",
            message="do not misaddress",
            provider=None,
            cwd=tmp_path,
        )

    assert exc.value.exit_code == 12
    assert read_all_threads("legacy") == []
    assert read_all_threads("transportA") == []


# ---------------------------------------------------------------------------
# F1 (sigma HIGH): envelope write failure -> exit 12, agent_send_failed event
# ---------------------------------------------------------------------------

def test_dispatch_send_envelope_write_oserror_exit12(tmp_path: Path, monkeypatch) -> None:
    """F1: write_new_thread raises OSError -> DispatchAskError exit 12, agent_send_failed emitted."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents.harnesses import claude as claude_mod
    from fno.agents.harnesses._claude_session_registry import SessionLocator

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


def test_dispatch_send_bus_lock_timeout_is_explicit_exit12(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-ERR: a pre-append timeout is loud and never resembles a receipt."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.bus.log import BusLockTimeout
    from fno.inbox import store as store_mod

    timeout = BusLockTimeout(tmp_path / "messages.jsonl.lock", 5.0)
    monkeypatch.setattr(
        store_mod,
        "write_new_thread",
        lambda **_kw: (_ for _ in ()).throw(timeout),
    )

    from fno.agents.dispatch import DispatchAskError, dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as exc_info:
        dispatch_send(name="red", message="hello", provider=None, cwd=cwd)

    text = str(exc_info.value)
    assert exc_info.value.exit_code == 12
    assert "bus lock timeout" in text
    assert "no durable envelope was written" in text
    assert "delivered" not in text
    assert "queued (durable)" not in text


def test_cmd_send_real_bus_lock_timeout_has_no_success_receipt(
    tmp_path: Path,
    monkeypatch,
    runner: CliRunner,
) -> None:
    """The command boundary preserves the real canonical-lock failure."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents import dispatch as dispatch_mod
    from fno.bus import log as bus_log
    from fno.inbox.store import read_all_threads
    from fno.mail.cli import mail_app

    monkeypatch.setattr(dispatch_mod, "_deliver_live", lambda *_args, **_kwargs: False)
    monkeypatch.setattr(bus_log, "_LOCK_TIMEOUT_SECONDS", 0.05)
    monkeypatch.setattr(bus_log, "_LOCK_POLL_SECONDS", 0.005)
    lock_path = bus_log._lock_path()
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    holder = open(lock_path, "w")
    fcntl.flock(holder.fileno(), fcntl.LOCK_EX)
    try:
        result = runner.invoke(mail_app, ["send", "red", "hello"])
    finally:
        fcntl.flock(holder.fileno(), fcntl.LOCK_UN)
        holder.close()

    assert result.exit_code == 12
    output = result.stdout + (result.stderr or "")
    assert "bus lock timeout" in output
    assert "no durable envelope was written" in output
    assert "delivered" not in result.stdout
    assert "queued (durable)" not in result.stdout
    assert not bus_log.bus_log_path().exists()
    assert read_all_threads("abcd1234") == []


def test_dispatch_send_alias_lock_contention_falls_back_before_delivery(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """Discovery's UX lock cannot withhold a peer-send terminal."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents import discover as discover_mod
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod, "_deliver_live", lambda *_args, **_kwargs: False)
    monkeypatch.setattr(discover_mod, "_ALIAS_LOCK_TIMEOUT_SECONDS", 0.05)
    monkeypatch.setattr(discover_mod, "_ALIAS_LOCK_POLL_SECONDS", 0.005)
    name_map = discover_mod.default_name_map_path()
    lock_path = name_map.with_suffix(name_map.suffix + ".lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    with open(lock_path, "w") as holder:
        fcntl.flock(holder.fileno(), fcntl.LOCK_EX)
        started = time.monotonic()
        result = dispatch_mod.dispatch_send(
            name="red",
            message="hello",
            provider=None,
            cwd=tmp_path,
        )

    assert time.monotonic() - started < 2.0
    assert result.delivery == "durable"


@pytest.mark.parametrize("timeout", [float("inf"), float("nan"), -0.1])
def test_dispatch_send_rejects_nonterminating_registry_stamp_timeout(
    tmp_path: Path,
    monkeypatch,
    timeout: float,
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents import dispatch as dispatch_mod
    from fno.bus.log import bus_log_path

    delivered: list[bool] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_deliver_live",
        lambda *_args, **_kwargs: delivered.append(True) or True,
    )

    with pytest.raises(ValueError, match="finite and non-negative"):
        dispatch_mod.dispatch_send(
            name="red",
            message="hello",
            provider=None,
            cwd=tmp_path,
            registry_stamp_timeout_seconds=timeout,
        )

    assert delivered == []
    assert not bus_log_path().exists()


@pytest.mark.parametrize("timeout", [float("inf"), float("nan"), -0.1])
def test_dispatch_send_rejects_nonterminating_agent_lock_timeout(
    tmp_path: Path,
    monkeypatch,
    timeout: float,
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents import dispatch as dispatch_mod
    from fno.bus.log import bus_log_path

    delivered: list[bool] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_deliver_live",
        lambda *_args, **_kwargs: delivered.append(True) or True,
    )

    with pytest.raises(ValueError, match="finite and non-negative"):
        dispatch_mod.dispatch_send(
            name="red",
            message="hello",
            provider=None,
            cwd=tmp_path,
            lock_timeout=timeout,
        )

    assert delivered == []
    assert not bus_log_path().exists()


def test_dispatch_send_envelope_write_valueerror_exit12(tmp_path: Path, monkeypatch) -> None:
    """F1: write_new_thread raises ValueError -> DispatchAskError exit 12."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents.harnesses import claude as claude_mod
    from fno.agents.harnesses._claude_session_registry import SessionLocator

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

    from fno.agents.harnesses import claude as claude_mod
    from fno.agents.harnesses._claude_session_registry import SessionLocator

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

def test_us2_send_by_handle_is_session_addressed(runner, tmp_path, monkeypatch):
    """x-605c US3: a bare <handle> that is a live discovered claude session is
    delivered TO THAT SESSION (live-inject first, durable floor to its canonical
    handle) -- NOT re-routed to a project. Project anycast stays explicit via
    --to-project (Locked Decision 3)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import write_registry

    write_registry([])  # empty -> dispatch_send raises unknown-agent (exit 16)

    from fno.agents import discover as discover_mod
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.discover import DiscoveredSession

    fake = DiscoveredSession(
        session_id="uuid-tgt", short_id="tgt00001", handle="fno-tgt00001",
        pid=123, cwd="/x/fno", project="fno", status="idle",
    )
    monkeypatch.setattr(discover_mod, "resolve_or_suggest", lambda h, **kw: (fake, []))
    # Force the live-inject miss so the send deterministically writes the floor.
    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", lambda *_a, **_k: False)

    # The removed re-route must NOT fire: a project send is now a hard failure.
    def _boom(*_a, **_kw):  # pragma: no cover - asserts the path is dead
        raise AssertionError("claude->project re-route must not fire (LD3)")

    monkeypatch.setattr(dispatch_mod, "dispatch_send_to_project", _boom)

    from fno.mail.cli import mail_app

    res = runner.invoke(
        mail_app, ["send", "fno-tgt00001", "does advance() resolve cwd?"]
    )
    assert res.exit_code == 0, res.output
    assert "queued (durable)" in res.output
    # Addressed to the canonical handle, not a project.
    assert "uuid-tgt" in res.output


@pytest.mark.parametrize("truth_state", ["working", "unknown"])
def test_registered_handle_colliding_with_discovery_sends_nothing(
    runner, tmp_path, monkeypatch, truth_state
):
    """A registry-first hit cannot hide a different live canonical owner."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import discover as discover_mod
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.discover import DiscoveredSession
    from fno.agents.registry import AgentEntry, write_registry
    from fno.inbox.store import read_all_threads
    from fno.mail.cli import mail_app

    registered_sid = "aaaaaaaa-1111-7222-8333-4444deadbeef"
    live_sid = "bbbbbbbb-1111-7222-8333-4444deadbeef"
    write_registry(
        [
            AgentEntry(
                name="deadbeef",
                cwd="/wrong",
                log_path="/tmp/wrong.log",
                harness="claude",
                harness_session_id=registered_sid,
                short_id="transport",
                status="live",
            )
        ]
    )
    live = DiscoveredSession(
        session_id=live_sid,
        short_id="deadbeef",
        handle="deadbeef",
        pid=123,
        cwd="/right",
        project="right",
        status="idle",
        agent="claude",
        truth_state=truth_state,
    )
    monkeypatch.setattr(
        discover_mod, "discover_live_sessions", lambda **_kwargs: [live]
    )
    monkeypatch.setattr(dispatch_mod, "_registered_family1_state", lambda _entry: "working")

    injected: list[str] = []

    def capture_wrong_injection(entry, *_args, **_kwargs):
        injected.append(entry.harness_session_id or entry.name)
        return True

    monkeypatch.setattr(dispatch_mod, "_deliver_live", capture_wrong_injection)

    result = runner.invoke(mail_app, ["send", "deadbeef", "must not guess"])

    assert result.exit_code == 2
    assert "ambiguous" in result.output
    assert "delivered" not in result.output and "queued" not in result.output
    assert injected == []
    assert read_all_threads("deadbeef") == []


def test_legacy_registry_row_does_not_collide_with_its_live_projection(
    runner, tmp_path, monkeypatch
):
    """A Claude row whose only session identity is short_id remains sendable."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import discover as discover_mod
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.discover import DiscoveredSession
    from fno.agents.registry import AgentEntry, write_registry
    from fno.inbox.store import read_all_threads
    from fno.mail.cli import mail_app

    write_registry(
        [
            AgentEntry(
                name="legacy",
                cwd="/same",
                log_path="/tmp/same.log",
                harness="claude",
                harness_session_id=None,
                short_id="deadbeef",
                status="live",
            )
        ]
    )
    live = DiscoveredSession(
        session_id="deadbeef",
        short_id="deadbeef",
        handle="deadbeef",
        pid=123,
        cwd="/same",
        project="same",
        status="idle",
        agent="claude",
        truth_state="working",
        name="legacy",
        identity_provisional=True,
    )
    monkeypatch.setattr(
        discover_mod, "discover_live_sessions", lambda **_kwargs: [live]
    )
    monkeypatch.setattr(dispatch_mod, "_registered_family1_state", lambda _entry: "working")
    delivered: list[str] = []

    def capture_delivery(entry, *_args, **_kwargs):
        delivered.append(entry.name)
        return True

    monkeypatch.setattr(dispatch_mod, "_deliver_live", capture_delivery)

    result = runner.invoke(mail_app, ["send", "deadbeef", "same owner"])

    assert result.exit_code == 0
    assert "delivered (hosted)" in result.output
    assert delivered == ["legacy"]
    assert read_all_threads("legacy") == []


def test_legacy_pseudo_id_does_not_hide_foreign_canonical_owner(
    runner, tmp_path, monkeypatch
):
    """A short-only legacy self cannot take full-id precedence over a peer."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import discover as discover_mod
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.discover import DiscoveredSession
    from fno.agents.registry import AgentEntry, write_registry
    from fno.inbox.store import read_all_threads
    from fno.mail.cli import mail_app

    write_registry(
        [
            AgentEntry(
                name="legacy",
                cwd="/legacy",
                log_path="/tmp/legacy.log",
                harness="claude",
                harness_session_id=None,
                short_id="deadbeef",
                status="live",
            )
        ]
    )
    legacy = DiscoveredSession(
        session_id="deadbeef",
        short_id="deadbeef",
        handle="deadbeef",
        pid=123,
        cwd="/legacy",
        project="legacy",
        status="idle",
        agent="claude",
        truth_state="working",
        name="legacy",
        identity_provisional=True,
    )
    foreign = DiscoveredSession(
        session_id="bbbbbbbb-1111-7222-8333-4444deadbeef",
        short_id="deadbeef",
        handle="deadbeef",
        pid=456,
        cwd="/foreign",
        project="foreign",
        status="idle",
        agent="claude",
        truth_state="unknown",
    )
    monkeypatch.setattr(
        discover_mod,
        "discover_live_sessions",
        lambda **_kwargs: [legacy, foreign],
    )
    monkeypatch.setattr(dispatch_mod, "_registered_family1_state", lambda _entry: "working")
    delivered: list[str] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_deliver_live",
        lambda entry, *_a, **_k: delivered.append(entry.name) or True,
    )

    result = runner.invoke(mail_app, ["send", "deadbeef", "must not guess"])

    assert result.exit_code == 2
    assert "ambiguous" in result.output
    assert "delivered" not in result.output and "queued" not in result.output
    assert delivered == []
    assert read_all_threads("legacy") == []


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


def test_dispatch_send_durable_stamps_wake_daemon_owner(tmp_path: Path, monkeypatch) -> None:
    """US6: a registered-agent send whose live inject misses writes its durable
    fallback stamped owner=wake-daemon on the bus, so the sweep classifies it by
    the terminal model instead of blanket age semantics."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno.agents.harnesses import claude as claude_mod
    from fno.agents.harnesses.claude import ProviderSocketError
    from fno.agents.harnesses._claude_session_registry import SessionLocator

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
    from fno.bus.log import iter_messages

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(name="red", message="FYI done", provider=None, cwd=cwd)

    assert result.delivery == "durable"
    envs = [m for m in iter_messages(warn=False) if m.id == result.msg_id]
    assert len(envs) == 1
    assert envs[0].meta.get("owner") == "wake-daemon"
    assert envs[0].meta.get("ttl_at")  # derived from the owner class


# ---------------------------------------------------------------------------
# x-b281: an agent-lock timeout must queue the message, never lose it
# ---------------------------------------------------------------------------


def test_dispatch_send_agent_lock_timeout_queues_durable(
    tmp_path: Path, monkeypatch
) -> None:
    """A real held flock times the send out, and the message still lands.

    The positive marker is the thread on the bus. Asserting only the non-zero
    exit would pass on a send that wrote nothing, which is the defect.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer()

    from fno import paths
    from fno.agents.dispatch import DispatchAskError, dispatch_send
    from fno.agents.registry import _agent_lock_path
    from fno.inbox.store import read_all_threads

    lock_path = _agent_lock_path("red", paths.agents_registry_path())
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    with open(lock_path, "a") as holder:
        fcntl.flock(holder.fileno(), fcntl.LOCK_EX)
        try:
            with pytest.raises(DispatchAskError) as exc_info:
                dispatch_send(
                    name="red",
                    message="hello",
                    provider=None,
                    cwd=tmp_path,
                    lock_timeout=0.2,
                )
        finally:
            fcntl.flock(holder.fileno(), fcntl.LOCK_UN)

    assert exc_info.value.exit_code == 11
    text = str(exc_info.value)
    assert "timed out waiting for agent 'red' lock" in text
    assert "message queued durable as msg-" in text

    threads = read_all_threads("abcd1234")
    assert len(threads) == 1, f"the message must survive the timeout: {threads}"
    assert "hello" in threads[0].messages[0].body


def test_dispatch_send_agent_lock_timeout_without_durable_address_says_so(
    tmp_path: Path, monkeypatch
) -> None:
    """A timeout that cannot queue names the loss instead of implying a receipt."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name="red",
            harness="claude",
            harness_session_id=None,
            cwd="/tmp",
            log_path="/tmp/red.log",
            short_id="abcd1234",
            status="live",
        )
    ])

    from fno import paths
    from fno.agents.dispatch import DispatchAskError, dispatch_send
    from fno.agents.registry import _agent_lock_path
    from fno.bus.log import bus_log_path

    lock_path = _agent_lock_path("red", paths.agents_registry_path())
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    with open(lock_path, "a") as holder:
        fcntl.flock(holder.fileno(), fcntl.LOCK_EX)
        try:
            with pytest.raises(DispatchAskError) as exc_info:
                dispatch_send(
                    name="red",
                    message="hello",
                    provider=None,
                    cwd=tmp_path,
                    lock_timeout=0.2,
                )
        finally:
            fcntl.flock(holder.fileno(), fcntl.LOCK_UN)

    assert exc_info.value.exit_code == 12
    assert "no durable envelope was written" in str(exc_info.value)
    assert not bus_log_path().exists()
