"""Tests for Task 2.3: Python daemon-RPC wiring for _deliver_live + gate.

Covers AC4-ERR (Python side): codex/gemini delivery via daemon RPC,
daemon-down degradation to durable, and the durable-first invariant.
All existing test_send.py tests remain unchanged.
"""
from __future__ import annotations

import json
import os
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture(autouse=True)
def _bypass_a2a_confirm(monkeypatch):
    """ab-098967b4: these tests exercise the _switchboard_exchange relay logic
    directly, so bypass the US6 first-use confirm (which would otherwise
    downgrade auto->observed under pytest's no-TTY). The confirm itself is
    covered in test_a2a_confirm.py."""
    monkeypatch.setenv("FNO_A2A_NO_CONFIRM", "1")
    from fno.agents import dispatch as dispatch_mod

    # This suite isolates transport delivery after liveness has admitted the
    # recipient. Family-1 routing decisions have their own send tests.
    monkeypatch.setattr(
        dispatch_mod, "_registered_family1_state", lambda _entry: "working"
    )


# ---------------------------------------------------------------------------
# Helper: register live peers
# ---------------------------------------------------------------------------

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


def _register_gemini_peer(name: str = "gemini-agent") -> None:
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name=name,
            harness="gemini",
            cwd="/tmp",
            log_path="/tmp/gemini-agent.log",
            harness_session_id="gemini-session-001",
            status="live",
        )
    ])


def _sb_identity(name: str) -> dict:
    return {
        "harness": "claude",
        "session_id": f"session-{name}",
        "short_id": f"short-{name}",
        "created_at": f"created-{name}",
    }


def _sb_identities(*names: str) -> dict[str, dict]:
    return {name: _sb_identity(name) for name in names}


# ---------------------------------------------------------------------------
# Minimal fake daemon (4-byte-LE-u32 + JSON framing) using a short /tmp path
# ---------------------------------------------------------------------------

def _read_frame(conn: socket.socket) -> dict:
    header = b""
    while len(header) < 4:
        chunk = conn.recv(4 - len(header))
        if not chunk:
            raise EOFError("connection closed before frame header")
        header += chunk
    (length,) = struct.unpack_from("<I", header)
    data = b""
    while len(data) < length:
        chunk = conn.recv(length - len(data))
        if not chunk:
            raise EOFError("connection closed during frame body")
        data += chunk
    return json.loads(data.decode("utf-8"))


def _write_frame(conn: socket.socket, obj: dict) -> None:
    payload = json.dumps(obj).encode("utf-8")
    header = struct.pack("<I", len(payload))
    conn.sendall(header + payload)


def _fake_daemon(
    sock_path: Path,
    responses: list[dict],
    received: list[dict],
    ready: "threading.Event | None" = None,
) -> None:
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(str(sock_path))
    srv.listen(1)
    if ready is not None:
        ready.set()
    conn, _ = srv.accept()
    try:
        for resp in responses:
            req = _read_frame(conn)
            received.append(req)
            _write_frame(conn, resp)
    except Exception:
        pass
    finally:
        conn.close()
        srv.close()


def _start_fake_daemon(
    sock_path: Path,
    responses: list[dict],
) -> tuple[threading.Thread, list[dict]]:
    """Start the fake daemon in a background thread. Returns (thread, received_list)."""
    received: list[dict] = []
    # Readiness is signalled by the daemon thread after listen(), not inferred
    # from the socket file appearing: the file exists from bind(), one syscall
    # earlier, so an exists() poll can hand the client a socket that is not
    # accepting yet. Connect-probing instead would steal the single accept()
    # this fake serves. The old poll also capped at 500ms, which a loaded
    # full-suite run overran -- the flake this replaces.
    ready = threading.Event()
    t = threading.Thread(
        target=_fake_daemon,
        args=(sock_path, responses, received, ready),
        daemon=True,
    )
    t.start()
    ready.wait(timeout=10)
    return t, received


# ---------------------------------------------------------------------------
# AC4-ERR (Python): daemon returns delivered=true -> delivery becomes "hosted"
# Uses monkeypatching to avoid Unix socket path-length issues on macOS.
# ---------------------------------------------------------------------------

def test_deliver_live_codex_daemon_delivered_true(
    tmp_path: Path, monkeypatch
) -> None:
    """AC4-ERR (Python): daemon RPC delivered=true -> dispatch_send returns hosted."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_codex_peer()

    # Monkeypatch _daemon_rpc to simulate a successful daemon response.
    rpc_calls: list[dict] = []

    def _mock_rpc(method: str, params: dict, **kwargs):
        rpc_calls.append({"method": method, "params": params})
        if method == "agent.deliver" and params.get("name") == "codex-agent":
            return {"delivered": True, "transport": "pty"}
        return None

    from fno.agents import dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _mock_rpc)

    from fno.agents.dispatch import dispatch_send
    from fno.inbox.store import read_all_threads

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="codex-agent",
        message="hey codex via PTY",
        provider=None,
        cwd=cwd,
    )

    assert result.delivery == "hosted", (
        f"daemon delivered=true must produce delivery='hosted', got {result.delivery!r}"
    )
    assert result.msg_id.startswith("msg-")

    # Bus demotion (node x-1f23): a hosted delivery is self-recording (transcript),
    # NOT also queued durable.
    assert read_all_threads("00000001") == [], "hosted delivery must not queue durable"

    # The deliver RPC carried the <fno_mail>-wrapped turn (codex/gemini share the
    # envelope now), not the raw body.
    assert len(rpc_calls) == 1
    rpc = rpc_calls[0]
    assert rpc["method"] == "agent.deliver"
    assert rpc["params"]["name"] == "codex-agent"
    body = rpc["params"]["body"]
    assert body.startswith("<fno_mail ") and body.rstrip().endswith("</fno_mail>")
    assert "hey codex via PTY" in body


# ---------------------------------------------------------------------------
# AC4-ERR (Python): daemon returns delivered=false -> delivery becomes "durable"
# ---------------------------------------------------------------------------

def test_deliver_live_codex_daemon_delivered_false(
    tmp_path: Path, monkeypatch
) -> None:
    """AC4-ERR (Python): daemon RPC delivered=false -> queued durable."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_codex_peer()

    def _mock_rpc(method: str, params: dict, **kwargs):
        return {"delivered": False, "reason": "injection-gate-unverified"}

    from fno.agents import dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _mock_rpc)

    from fno.agents.dispatch import dispatch_send
    from fno.inbox.store import read_all_threads

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="codex-agent",
        message="hey queued",
        provider=None,
        cwd=cwd,
    )

    assert result.delivery == "durable"
    assert result.msg_id.startswith("msg-")

    threads = read_all_threads("deadbeef")
    assert len(threads) == 1


# ---------------------------------------------------------------------------
# AC4-ERR (Python): _daemon_rpc returns None (daemon unreachable) -> durable
# ---------------------------------------------------------------------------

def test_deliver_live_codex_daemon_unreachable(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC4-ERR (Python): daemon unreachable -> queued durable, stderr daemon-unreachable notice."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_codex_peer()

    # _daemon_rpc already prints to stderr and returns None on connection failure.
    # Monkeypatch to simulate what _daemon_rpc does on connection refused.
    import sys

    def _mock_rpc_unreachable(method: str, params: dict, **kwargs):
        print("fno-agents daemon unreachable; message queued durable", file=sys.stderr)
        return None

    from fno.agents import dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _mock_rpc_unreachable)

    from fno.agents.dispatch import dispatch_send
    from fno.inbox.store import read_all_threads

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="codex-agent",
        message="hey unreachable",
        provider=None,
        cwd=cwd,
    )

    assert result.delivery == "durable"
    assert result.msg_id.startswith("msg-")

    threads = read_all_threads("deadbeef")
    assert len(threads) == 1

    captured = capsys.readouterr()
    assert "daemon" in captured.err.lower() or "unreachable" in captured.err.lower(), (
        f"stderr must mention daemon/unreachable; got: {captured.err!r}"
    )


# ---------------------------------------------------------------------------
# AC4-ERR (Python): CLI output format for codex delivered via PTY
# ---------------------------------------------------------------------------

def test_cmd_send_codex_delivered_hosted_stdout(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """CLI: codex delivered via daemon -> stdout 'msg-<id> delivered (hosted)'."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_codex_peer()

    def _mock_rpc(method: str, params: dict, **kwargs):
        return {"delivered": True, "transport": "pty"}

    from fno.agents import dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _mock_rpc)

    from fno.mail.cli import mail_app

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = runner.invoke(
        mail_app,
        ["send", "codex-agent", "hello", "--cwd", str(cwd)],
    )
    assert result.exit_code == 0, (result.stdout or "") + (result.stderr or "")
    out = (result.stdout or "").strip()
    assert out.startswith("msg-"), f"stdout: {out!r}"
    assert "delivered (hosted)" in out, f"stdout: {out!r}"
    assert "queued" not in out


# ---------------------------------------------------------------------------
# Durable-first invariant: envelope always in store even on daemon errors
# ---------------------------------------------------------------------------

def test_deliver_live_codex_daemon_rpc_error_still_durable(
    tmp_path: Path, monkeypatch
) -> None:
    """Durable-first: _daemon_rpc returns None on error -> envelope still in store."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_codex_peer()

    def _mock_rpc_error(method: str, params: dict, **kwargs):
        # Simulate what _daemon_rpc does on RPC error: prints to stderr, returns None.
        import sys
        print("daemon RPC error: AgentNotFound", file=sys.stderr)
        return None

    from fno.agents import dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _mock_rpc_error)

    from fno.agents.dispatch import dispatch_send
    from fno.inbox.store import read_all_threads

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="codex-agent",
        message="error path",
        provider=None,
        cwd=cwd,
    )

    assert result.delivery == "durable"
    threads = read_all_threads("deadbeef")
    assert len(threads) == 1, "envelope must survive RPC error"


# ---------------------------------------------------------------------------
# Claude peer + switchboard (Group 2, Task 3.1): claude now probes the
# versioned switchboard RPC first; a non-stream-thread demotes to socket/MCP
# (purely additive over the 2.1 socket/MCP contract).
# ---------------------------------------------------------------------------

def test_deliver_live_claude_switchboard_demotes_to_socket(
    tmp_path: Path, monkeypatch
) -> None:
    """Claude peer that is NOT a live stream thread: the switchboard probe
    demotes and delivery falls through to the socket path (2.1 behavior)."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry
    write_registry([
        AgentEntry(
            name="claude-peer",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/claude-peer.log",
            short_id="abcd1234",
            status="live",
        ),
        AgentEntry(
            name="fno",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/fno.log",
            short_id="fno12345",
            harness_session_id="aaaaaaaa-2222-3333-4444-555555555555",
            status="live",
        ),
    ])

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)

    # The control.sock inject (mail-inject verb) is the socket-path successor; it
    # succeeds here so the demote falls through to it.
    inject_calls: list = []

    def _ok_inject(recipient: str, text: str, **_k) -> bool:
        inject_calls.append({"recipient": recipient, "text": text})
        return True

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", _ok_inject)

    rpc_calls: list = []

    def _mock_rpc(method: str, params: dict, **kwargs):
        rpc_calls.append({"method": method, "params": params})
        # B is not a live stream thread -> the daemon demotes.
        return {"delivered": False, "reason": "not-a-live-stream-thread"}

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _mock_rpc)

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="claude-peer",
        message="hi claude",
        provider=None,
        cwd=cwd,
    )

    assert result.delivery == "hosted"
    assert len(inject_calls) == 1, "demote must fall through to the control.sock inject"
    assert len(rpc_calls) == 1, "claude must probe the switchboard RPC"
    assert rpc_calls[0]["method"] == "agent.switchboard_v2"
    assert rpc_calls[0]["params"]["to"] == "claude-peer"
    assert rpc_calls[0]["params"]["recipient_identity"]["short_id"] == "abcd1234"


def test_deliver_live_claude_switchboard_delivered_skips_socket(
    tmp_path: Path, monkeypatch
) -> None:
    """Claude peer that IS a live stream thread: the switchboard delivers the
    turn and the socket/MCP path is skipped entirely."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry
    write_registry([
        AgentEntry(
            name="claude-stream",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/claude-stream.log",
            short_id="abcd1234",
            harness_session_id="11111111-2222-3333-4444-555555555555",
            status="live",
        ),
        AgentEntry(
            name="fno",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/fno.log",
            short_id="fno12345",
            harness_session_id="aaaaaaaa-2222-3333-4444-666666666666",
            status="live",
        ),
    ])

    from fno.agents.harnesses import claude as claude_mod

    send_calls: list = []
    monkeypatch.setattr(claude_mod, "send_to_session", lambda *a, **kw: send_calls.append(1))

    rpc_calls: list = []
    from fno.agents import dispatch as dispatch_mod

    # Pin OBSERVED mode (auto off) so this stays a single-hop delivery test;
    # the A2A relay loop is exercised by the 4.1 tests below.
    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (False, 6))

    def _mock_rpc(method: str, params: dict, **kwargs):
        rpc_calls.append({"method": method, "params": params})
        return {
            "delivered": True,
            "identity_verified": True,
            "transport": "switchboard",
            "reply": "ack",
            "mirrored": True,
        }

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _mock_rpc)

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="claude-stream",
        message="hi via switchboard",
        provider=None,
        cwd=cwd,
    )

    assert result.delivery == "hosted"
    assert len(rpc_calls) == 1
    assert rpc_calls[0]["method"] == "agent.switchboard_v2"
    assert rpc_calls[0]["params"]["mirror"] is True, "observed mode mirrors into A"
    assert (
        rpc_calls[0]["params"]["recipient_identity"]["session_id"]
        == "11111111-2222-3333-4444-555555555555"
    )
    assert rpc_calls[0]["params"]["from_identity"]["short_id"] == "fno12345"
    assert len(send_calls) == 0, "a delivered switchboard turn must skip the socket path"


# ---------------------------------------------------------------------------
# Group 2, Task 4.1: A2A relay loop + config.agents.a2a toggle/ceiling
# ---------------------------------------------------------------------------

def test_switchboard_observed_single_hop(monkeypatch) -> None:
    """auto=False -> a single observed hop, mirror=True, no relay."""
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (False, 6))
    calls: list = []

    def _rpc(method, params, **kw):
        calls.append(params)
        return {"delivered": True, "identity_verified": True, "reply": "r1"}

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)
    assert dispatch_mod._switchboard_exchange(
        "B",
        "A",
        "msg",
        to_identity=_sb_identity("B"),
        from_identity=_sb_identity("A"),
    ) is True
    assert len(calls) == 1
    assert calls[0]["mirror"] is True
    assert calls[0]["to"] == "B" and calls[0]["from"] == "A"


def test_unanswered_confirmation_continues_as_observed_switchboard_hop(
    tmp_path, monkeypatch
) -> None:
    """A prompt timeout still reaches a terminal observed delivery."""
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.delenv("FNO_A2A_NO_CONFIRM", raising=False)
    from fno.agents import dispatch as dispatch_mod

    read_fd, write_fd = os.pipe()

    class _UnansweredTTY:
        def isatty(self):
            return True

        def fileno(self):
            return read_fd

        def readline(self):
            return os.read(read_fd, 4096).decode()

    class _TTYErr:
        def isatty(self):
            return True

        def write(self, _text):
            return None

        def flush(self):
            return None

    monkeypatch.setattr(dispatch_mod.sys, "stdin", _UnansweredTTY())
    monkeypatch.setattr(dispatch_mod.sys, "stderr", _TTYErr())
    real_gate = dispatch_mod._a2a_first_use_gate
    monkeypatch.setattr(
        dispatch_mod,
        "_a2a_first_use_gate",
        lambda auto, ceiling: real_gate(
            auto,
            ceiling,
            confirm_timeout_seconds=0.05,
        ),
    )
    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (True, 6))
    calls: list[dict] = []

    def _rpc(_method, params, **_kwargs):
        calls.append(params)
        return {"delivered": True, "identity_verified": True, "reply": "r1"}

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)
    started = time.monotonic()
    try:
        delivered = dispatch_mod._switchboard_exchange(
            "B",
            "A",
            "msg",
            to_identity=_sb_identity("B"),
            from_identity=_sb_identity("A"),
        )
    finally:
        os.close(write_fd)
        os.close(read_fd)

    assert time.monotonic() - started < 0.5
    assert delivered is True
    assert len(calls) == 1
    assert calls[0]["mirror"] is True
    assert not (tmp_path / ".fno" / ".a2a-confirmed").exists()
    assert not (tmp_path / "config.toml").exists()


def test_switchboard_auto_is_nonblocking_kicks_off_detached_relay(monkeypatch) -> None:
    """ab-3bd520ab: auto=True drives B synchronously (hop 1 = the actual delivery)
    then KICKS OFF the relay in the background and returns immediately. The caller
    runs exactly ONE _daemon_rpc inline; the relay loop never runs in-process."""
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (True, 6))
    calls: list = []

    def _rpc(method, params, **kw):
        calls.append(params)
        return {"delivered": True, "identity_verified": True, "reply": "r1"}

    kicked: list = []
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)
    monkeypatch.setattr(
        dispatch_mod,
        "_kickoff_background_relay",
        lambda to_name, from_name, seed, ceiling, mail_ctxs=None, **kwargs: kicked.append(
            (to_name, from_name, seed, ceiling, mail_ctxs, kwargs)
        ),
    )
    assert dispatch_mod._switchboard_exchange(
        "B",
        "A",
        "msg",
        to_identity=_sb_identity("B"),
        from_identity=_sb_identity("A"),
    ) is True
    assert len(calls) == 1, "only the first hop (drive B) runs inline; the relay is detached"
    assert calls[0]["to"] == "B" and calls[0]["mirror"] is False
    # No mail ctxs on this bare _switchboard_exchange call -> chat-style raw relay.
    assert kicked == [
        (
            "B",
            "A",
            "r1",
            6,
            None,
            {"recipient_identities": _sb_identities("B", "A")},
        )
    ], "the relay is handed off with B's reply as the seed"


def test_background_relay_first_fork_failure_drops_continuation_without_inline_wait(
    monkeypatch,
) -> None:
    """A resource-exhausted launcher cannot run the relay before the receipt."""
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(
        dispatch_mod.os,
        "fork",
        lambda: (_ for _ in ()).throw(OSError("fork unavailable")),
    )
    relay_calls: list[tuple] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_run_relay_loop",
        lambda *args, **kwargs: relay_calls.append((args, kwargs)),
    )
    stopped: list[tuple] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_emit_relay_stopped",
        lambda *args, **kwargs: stopped.append((args, kwargs)),
    )

    dispatch_mod._kickoff_background_relay(
        "B",
        "A",
        "reply",
        6,
        recipient_identities=_sb_identities("A", "B"),
    )

    assert relay_calls == []
    assert stopped[0][0][3] == "relay-detach-failed"


def test_background_relay_second_fork_failure_exits_intermediate_without_relay(
    monkeypatch,
) -> None:
    """The parent wait stays short when the grandchild cannot be created."""
    from fno.agents import dispatch as dispatch_mod

    forks = iter((0, OSError("grandchild unavailable")))

    def _fork():
        result = next(forks)
        if isinstance(result, OSError):
            raise result
        return result

    class _ChildExit(BaseException):
        pass

    monkeypatch.setattr(dispatch_mod.os, "fork", _fork)
    monkeypatch.setattr(dispatch_mod.os, "setsid", lambda: None)
    monkeypatch.setattr(
        dispatch_mod.os,
        "_exit",
        lambda _code: (_ for _ in ()).throw(_ChildExit()),
    )
    relay_calls: list[tuple] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_run_relay_loop",
        lambda *args, **kwargs: relay_calls.append((args, kwargs)),
    )
    stopped: list[tuple] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_emit_relay_stopped",
        lambda *args, **kwargs: stopped.append((args, kwargs)),
    )

    with pytest.raises(_ChildExit):
        dispatch_mod._kickoff_background_relay(
            "B",
            "A",
            "reply",
            6,
            recipient_identities=_sb_identities("A", "B"),
        )

    assert relay_calls == []
    assert stopped[0][0][3] == "relay-detach-failed"


def test_detach_stdio_reports_devnull_open_failure(monkeypatch) -> None:
    from fno.agents import dispatch as dispatch_mod

    with monkeypatch.context() as patch:
        patch.setattr(
            dispatch_mod.os,
            "open",
            lambda *_args, **_kwargs: (_ for _ in ()).throw(OSError("fd exhausted")),
        )
        assert dispatch_mod._detach_stdio() is False


def test_detach_stdio_reports_partial_dup_failure_and_closes_source(monkeypatch) -> None:
    from fno.agents import dispatch as dispatch_mod

    duplicated: list[int] = []
    closed: list[int] = []
    def _dup2(_source, destination):
        duplicated.append(destination)
        if destination == 1:
            raise OSError("stdout read only")

    with monkeypatch.context() as patch:
        patch.setattr(dispatch_mod.os, "open", lambda *_args, **_kwargs: 9)
        patch.setattr(dispatch_mod.os, "dup2", _dup2)
        patch.setattr(dispatch_mod.os, "close", lambda fd: closed.append(fd))
        assert dispatch_mod._detach_stdio() is False
    assert duplicated == [0, 1, 2]
    assert closed == [9]


def test_background_relay_stdio_detach_failure_exits_without_relay(monkeypatch) -> None:
    """A grandchild retaining capture pipes cannot withhold the CLI terminal."""
    from fno.agents import dispatch as dispatch_mod

    forks = iter((0, 0))

    class _ChildExit(BaseException):
        pass

    monkeypatch.setattr(dispatch_mod.os, "fork", lambda: next(forks))
    monkeypatch.setattr(dispatch_mod.os, "setsid", lambda: None)
    monkeypatch.setattr(
        dispatch_mod.os,
        "_exit",
        lambda _code: (_ for _ in ()).throw(_ChildExit()),
    )
    monkeypatch.setattr(dispatch_mod, "_detach_stdio", lambda: False)
    relay_calls: list[tuple] = []
    stopped: list[tuple] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_run_relay_loop",
        lambda *args, **kwargs: relay_calls.append((args, kwargs)),
    )
    monkeypatch.setattr(
        dispatch_mod,
        "_emit_relay_stopped",
        lambda *args, **kwargs: stopped.append((args, kwargs)),
    )

    with pytest.raises(_ChildExit):
        dispatch_mod._kickoff_background_relay(
            "B",
            "A",
            "reply",
            6,
            recipient_identities=_sb_identities("A", "B"),
        )

    assert relay_calls == []
    assert stopped[0][0][3] == "relay-stdio-detach-failed"


def test_real_relay_descendant_releases_captured_receipt_pipes(
    tmp_path, monkeypatch
) -> None:
    """A real double-forked relay cannot delay the hosted CLI-style receipt."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry
    from fno.bus.log import bus_log_path

    write_registry(
        [
            AgentEntry(
                name="red",
                harness="claude",
                harness_session_id="aaaaaaaa-1111-7222-8333-4444abcd1234",
                cwd=str(tmp_path),
                log_path=str(tmp_path / "red.log"),
                short_id="abcd1234",
                status="live",
            )
        ]
    )
    relay_marker = tmp_path / "relay-started"
    relay_release = tmp_path / "relay-release"
    relay_finished = tmp_path / "relay-finished"
    script = """
import os
import time
from pathlib import Path
from fno.agents import dispatch

dispatch._load_a2a_settings = lambda: (True, 6)
dispatch._registered_family1_state = lambda _entry: "working"

def run_relay(*_args, **_kwargs):
    Path(os.environ["FNO_TEST_RELAY_MARKER"]).write_text("started")
    release = Path(os.environ["FNO_TEST_RELAY_RELEASE"])
    deadline = time.monotonic() + 8
    while not release.exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    Path(os.environ["FNO_TEST_RELAY_FINISHED"]).write_text("finished")

dispatch._run_relay_loop = run_relay
dispatch._daemon_rpc = lambda *_args, **_kwargs: {
    "delivered": True,
    "identity_verified": True,
    "reply": "continue",
}

def deliver(entry, body, from_name, mail=None, sender_entry=None, reason_out=None, **_kw):
    return dispatch._switchboard_exchange(
        entry.name,
        from_name,
        body,
        to_identity={
            "harness": "claude",
            "session_id": entry.harness_session_id,
            "short_id": entry.short_id,
            "created_at": entry.created_at,
        },
        from_identity={
            "harness": "codex",
            "session_id": "sender-session",
            "short_id": "sender01",
            "created_at": "sender-created",
        },
    )

dispatch._deliver_live = deliver
result = dispatch.dispatch_send(
    name="red",
    message="hello",
    provider=None,
    cwd=Path.cwd(),
)
print(f"{result.msg_id} delivered ({result.delivery})")
"""
    env = os.environ.copy()
    env["FNO_TEST_RELAY_MARKER"] = str(relay_marker)
    env["FNO_TEST_RELAY_RELEASE"] = str(relay_release)
    env["FNO_TEST_RELAY_FINISHED"] = str(relay_finished)
    try:
        result = subprocess.run(
            [sys.executable, "-c", script],
            capture_output=True,
            text=True,
            env=env,
            timeout=8,
            check=False,
        )
    finally:
        relay_release.touch()

    assert result.returncode == 0, result.stdout + result.stderr
    assert len(result.stdout.splitlines()) == 1
    assert result.stdout.strip().endswith("delivered (hosted)")
    assert not bus_log_path().exists()
    marker_deadline = time.monotonic() + 2.0
    while time.monotonic() < marker_deadline:
        try:
            if relay_marker.read_text() == "started":
                break
        except OSError:
            pass
        time.sleep(0.01)
    assert relay_marker.read_text() == "started"
    assert not relay_finished.exists(), "relay finished before the CLI receipt returned"
    relay_release.write_text("release")
    finished_deadline = time.monotonic() + 2.0
    while time.monotonic() < finished_deadline:
        try:
            if relay_finished.read_text() == "finished":
                break
        except OSError:
            pass
        time.sleep(0.01)
    assert relay_finished.read_text() == "finished"


def test_switchboard_auto_no_kickoff_when_first_reply_empty(monkeypatch) -> None:
    """B delivered but replied empty -> nothing to relay, so no background kickoff."""
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (True, 6))
    monkeypatch.setattr(
        dispatch_mod,
        "_daemon_rpc",
        lambda *a, **k: {"delivered": True, "identity_verified": True, "reply": ""},
    )
    kicked: list = []
    monkeypatch.setattr(
        dispatch_mod, "_kickoff_background_relay", lambda *a: kicked.append(a)
    )
    assert dispatch_mod._switchboard_exchange(
        "B",
        "A",
        "msg",
        to_identity=_sb_identity("B"),
        from_identity=_sb_identity("A"),
    ) is True
    assert kicked == [], "an empty first reply has no relay to run"


def test_relay_loop_bounded_by_ceiling(monkeypatch, capsys) -> None:
    """_run_relay_loop with an always-replying pair runs up to turn_ceiling total
    turns (the first hop already happened) and stops with 'loop ceiling reached',
    alternating A, B, A after the seed."""
    from fno.agents import dispatch as dispatch_mod

    calls: list = []

    def _rpc(method, params, **kw):
        calls.append(params)
        return {"delivered": True, "identity_verified": True, "reply": "more"}

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)
    dispatch_mod._run_relay_loop(
        "B", "A", "r1", 4, recipient_identities=_sb_identities("A", "B")
    )
    # ceiling=4 total; the first hop (drive B) is the caller's, so the loop runs 3.
    assert [c["to"] for c in calls] == ["A", "B", "A"]
    assert all(c["mirror"] is False for c in calls)
    identities = _sb_identities("A", "B")
    assert all(c["recipient_identity"] == identities[c["to"]] for c in calls)
    assert "loop ceiling reached" in capsys.readouterr().err


def test_relay_loop_stops_on_empty_reply(monkeypatch, capsys) -> None:
    """A side that produces no reply ends the relay before the ceiling, with no
    'loop ceiling reached' notice."""
    from fno.agents import dispatch as dispatch_mod

    calls: list = []

    def _rpc(method, params, **kw):
        calls.append(params)
        return {
            "delivered": True,
            "identity_verified": True,
            "reply": "",
        }  # A replies empty on the first relay hop

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)
    dispatch_mod._run_relay_loop(
        "B", "A", "r1", 8, recipient_identities=_sb_identities("A", "B")
    )
    assert len(calls) == 1, "relay should stop when a side replies empty"
    assert "loop ceiling reached" not in capsys.readouterr().err


def test_relay_loop_one_way_when_peer_not_stream(monkeypatch) -> None:
    """The peer (A) is not a live stream thread: the first relay hop demotes and
    the exchange ends (B already received the original body via the caller's hop)."""
    from fno.agents import dispatch as dispatch_mod

    calls: list = []

    def _rpc(method, params, **kw):
        calls.append(params)
        return {"delivered": False, "reason": "not-a-live-stream-thread"}

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)
    emitted: list[tuple[str, dict]] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_emit_ev",
        lambda kind, **data: emitted.append((kind, data)),
    )
    dispatch_mod._run_relay_loop(
        "B", "A", "r1", 6, recipient_identities=_sb_identities("A", "B")
    )
    assert len(calls) == 1, "a single failed relay hop to A ends the exchange"
    assert emitted[0][0] == "agent_relay_stopped"
    assert emitted[0][1]["reason"] == "not-a-live-stream-thread"


def test_relay_loop_emits_stop_when_identity_proof_is_missing(monkeypatch) -> None:
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(
        dispatch_mod,
        "_daemon_rpc",
        lambda *a, **k: {"delivered": True, "reply": "unverified"},
    )
    emitted: list[tuple[str, dict]] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_emit_ev",
        lambda kind, **data: emitted.append((kind, data)),
    )

    turns = dispatch_mod._run_relay_loop(
        "B", "A", "r1", 6, recipient_identities=_sb_identities("A", "B")
    )

    assert turns == 1
    assert emitted == [
        (
            "agent_relay_stopped",
            {
                "target": "A",
                "peer": "B",
                "turn": 2,
                "turns_completed": 1,
                "reason": "identity-unverified",
            },
        )
    ]


def test_relay_loop_emits_stop_when_hop_raises(monkeypatch) -> None:
    from fno.agents import dispatch as dispatch_mod

    def _raise(*args, **kwargs):
        raise RuntimeError("daemon socket vanished")

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _raise)
    emitted: list[tuple[str, dict]] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_emit_ev",
        lambda kind, **data: emitted.append((kind, data)),
    )

    turns = dispatch_mod._run_relay_loop(
        "B", "A", "r1", 6, recipient_identities=_sb_identities("A", "B")
    )

    assert turns == 1
    assert emitted == [
        (
            "agent_relay_stopped",
            {
                "target": "A",
                "peer": "B",
                "turn": 2,
                "turns_completed": 1,
                "reason": "relay-hop-error",
                "error": "daemon socket vanished",
                "error_type": "RuntimeError",
            },
        )
    ]


def test_relay_loop_persists_stop_event(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(
        dispatch_mod,
        "_daemon_rpc",
        lambda *a, **k: {"delivered": False, "reason": "recipient-identity-changed"},
    )

    dispatch_mod._run_relay_loop(
        "B", "A", "r1", 6, recipient_identities=_sb_identities("A", "B")
    )

    records = [
        json.loads(line)
        for line in (tmp_path / ".fno/events.jsonl").read_text(encoding="utf-8").splitlines()
    ]
    assert records[-1]["kind"] == "agent_relay_stopped"
    assert records[-1]["target"] == "A"
    assert records[-1]["turn"] == 2
    assert records[-1]["reason"] == "recipient-identity-changed"


def test_switchboard_demote_when_first_hop_not_delivered(monkeypatch) -> None:
    """B not a live stream thread on the first hop -> None (caller demotes)."""
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (True, 6))
    monkeypatch.setattr(
        dispatch_mod,
        "_daemon_rpc",
        lambda *a, **k: {"delivered": False, "reason": "not-a-live-stream-thread"},
    )
    assert dispatch_mod._switchboard_exchange(
        "B",
        "A",
        "msg",
        to_identity=_sb_identity("B"),
        from_identity=_sb_identity("A"),
    ) is None


def test_switchboard_demotes_success_without_identity_proof(monkeypatch) -> None:
    """A success without the current daemon's identity proof fails closed."""
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (False, 6))
    monkeypatch.setattr(
        dispatch_mod,
        "_daemon_rpc",
        lambda *a, **k: {"delivered": True, "reply": "legacy daemon accepted unknown fields"},
    )
    assert dispatch_mod._switchboard_exchange(
        "B",
        "A",
        "msg",
        to_identity=_sb_identity("B"),
        from_identity=_sb_identity("A"),
    ) is None


def test_a2a_config_defaults_and_validation() -> None:
    """config.agents.a2a defaults (auto=True, ceiling=6); ceiling must be >= 1."""
    import pytest as _pytest
    from fno.config import A2aBlock, ConfigBlock

    blk = ConfigBlock()
    assert blk.agents.a2a.auto is True
    assert blk.agents.a2a.turn_ceiling == 6
    with _pytest.raises(Exception):
        A2aBlock(turn_ceiling=0)


# ---------------------------------------------------------------------------
# Gemini peer: routed through daemon RPC same as codex
# ---------------------------------------------------------------------------

def test_deliver_live_gemini_daemon_delivered_true(
    tmp_path: Path, monkeypatch
) -> None:
    """Gemini peer: daemon RPC delivered=true -> delivery='hosted'."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_gemini_peer()

    def _mock_rpc(method: str, params: dict, **kwargs):
        return {"delivered": True, "transport": "pty"}

    from fno.agents import dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _mock_rpc)

    from fno.agents.dispatch import dispatch_send
    from fno.inbox.store import read_all_threads

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="gemini-agent",
        message="hey gemini",
        provider=None,
        cwd=cwd,
    )

    assert result.delivery == "hosted"
    # Bus demotion (node x-1f23): a hosted delivery is not also queued durable.
    assert read_all_threads("gemini-agent") == []


# ---------------------------------------------------------------------------
# Real socket test: framing correctness (uses a short /tmp path)
# ---------------------------------------------------------------------------

def test_daemon_rpc_real_socket_framing(monkeypatch) -> None:
    """_daemon_rpc speaks the correct 4-byte-LE-u32+JSON framing to a real socket."""
    import tempfile

    # Use a genuinely short path to avoid macOS 104-byte SUN_LEN limit.
    home_dir = Path(tempfile.mkdtemp(prefix="/tmp/fno"))
    sock_path = home_dir / "supervisor.sock"

    responses = [{"id": 1, "result": {"delivered": True, "transport": "pty"}}]
    _, received = _start_fake_daemon(sock_path, responses)

    # Override FNO_AGENTS_HOME so _daemon_rpc finds the socket.
    monkeypatch.setenv("FNO_AGENTS_HOME", str(home_dir))

    from fno.agents import dispatch as dispatch_mod
    result = dispatch_mod._daemon_rpc(
        "agent.deliver", {"name": "x", "body": "y", "from_name": "z"}
    )

    assert result is not None, "_daemon_rpc must return result dict on success"
    assert result.get("delivered") is True

    import shutil
    shutil.rmtree(str(home_dir), ignore_errors=True)


# ---------------------------------------------------------------------------
# F7a: Request-frame assertion - raw framing + RPC shape (Finding 7a)
# ---------------------------------------------------------------------------

def test_daemon_rpc_request_frame_shape(monkeypatch) -> None:
    """F7a: _daemon_rpc sends correct {id, method, params} JSON AND
    raw first-4-bytes decode as little-endian u32 matching the JSON byte length."""
    import tempfile

    home_dir = Path(tempfile.mkdtemp(prefix="/tmp/fno"))
    sock_path = home_dir / "supervisor.sock"

    # Capture raw bytes + parsed request
    raw_bytes_received: list[bytes] = []

    ready = threading.Event()

    def _raw_fake_daemon(path: Path, resp_list: list, recv_list: list) -> None:
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(str(path))
        srv.listen(1)
        ready.set()
        conn, _ = srv.accept()
        try:
            # Read the raw 4-byte header
            raw_header = b""
            while len(raw_header) < 4:
                chunk = conn.recv(4 - len(raw_header))
                if not chunk:
                    break
                raw_header += chunk
            raw_bytes_received.append(raw_header)
            (length,) = struct.unpack_from("<I", raw_header)

            # Read the body
            raw_body = b""
            while len(raw_body) < length:
                chunk = conn.recv(length - len(raw_body))
                if not chunk:
                    break
                raw_body += chunk
            raw_bytes_received.append(raw_body)

            req = json.loads(raw_body.decode("utf-8"))
            recv_list.append(req)

            resp = {"id": req.get("id", 1), "result": {"delivered": True, "transport": "pty"}}
            payload = json.dumps(resp).encode("utf-8")
            conn.sendall(struct.pack("<I", len(payload)) + payload)
        except Exception:
            pass
        finally:
            conn.close()
            srv.close()

    received: list[dict] = []
    t = threading.Thread(
        target=_raw_fake_daemon,
        args=(sock_path, [], received),
        daemon=True,
    )
    t.start()
    ready.wait(timeout=10)

    monkeypatch.setenv("FNO_AGENTS_HOME", str(home_dir))

    from fno.agents import dispatch as dispatch_mod
    dispatch_mod._daemon_rpc("agent.deliver", {"name": "foo", "body": "bar", "from_name": "baz"})

    t.join(timeout=2.0)

    import shutil
    shutil.rmtree(str(home_dir), ignore_errors=True)

    assert len(received) == 1, f"Expected 1 request, got {len(received)}"
    req = received[0]

    # Shape assertion: must have id (int), method (str), params (dict)
    assert isinstance(req.get("id"), int), f"id must be int, got {req.get('id')!r}"
    assert req.get("method") == "agent.deliver", f"method mismatch: {req.get('method')!r}"
    params = req.get("params", {})
    assert params.get("name") == "foo", f"params.name mismatch: {params.get('name')!r}"
    assert params.get("body") == "bar", f"params.body mismatch: {params.get('body')!r}"
    assert params.get("from_name") == "baz", f"params.from_name mismatch: {params.get('from_name')!r}"

    # Raw framing assertion: header is 4-byte LE u32 matching the JSON body length
    assert len(raw_bytes_received) == 2, "Expected raw header + body capture"
    raw_header = raw_bytes_received[0]
    raw_body = raw_bytes_received[1]
    assert len(raw_header) == 4, f"Header must be 4 bytes, got {len(raw_header)}"
    (declared_length,) = struct.unpack_from("<I", raw_header)
    assert declared_length == len(raw_body), (
        f"LE u32 header {declared_length} != actual JSON body length {len(raw_body)}"
    )


# ---------------------------------------------------------------------------
# F7c: Param-contract pin - Python client emits shape Rust can deserialize
# ---------------------------------------------------------------------------

def test_daemon_rpc_params_contract_pin(monkeypatch) -> None:
    """F7c: pin the Python client's emitted params shape:
    {name: str, body: str, from_name: str} with method="agent.deliver" and
    id as int. Fails if either side renames a field."""
    import tempfile

    home_dir = Path(tempfile.mkdtemp(prefix="/tmp/fno"))
    sock_path = home_dir / "supervisor.sock"

    captured_params: list[dict] = []

    import threading

    listening = threading.Event()

    def _capturing_daemon(path: Path) -> None:
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(str(path))
        srv.listen(1)
        # Signal AFTER listen(): bind() already created the socket file, so a
        # client that waits on the file can connect into the gap before anyone
        # is listening and get refused - captured nothing, no error raised.
        listening.set()
        conn, _ = srv.accept()
        try:
            header = b""
            while len(header) < 4:
                chunk = conn.recv(4 - len(header))
                if not chunk:
                    return
                header += chunk
            (length,) = struct.unpack_from("<I", header)
            body = b""
            while len(body) < length:
                chunk = conn.recv(length - len(body))
                if not chunk:
                    return
                body += chunk
            req = json.loads(body.decode("utf-8"))
            captured_params.append(req)
            resp = {"id": req.get("id", 1), "result": {"delivered": True, "transport": "pty"}}
            payload = json.dumps(resp).encode("utf-8")
            conn.sendall(struct.pack("<I", len(payload)) + payload)
        except Exception:
            pass
        finally:
            conn.close()
            srv.close()

    t = threading.Thread(target=_capturing_daemon, args=(sock_path,), daemon=True)
    t.start()
    assert listening.wait(timeout=10.0), "capturing daemon never reached listen()"

    monkeypatch.setenv("FNO_AGENTS_HOME", str(home_dir))

    from fno.agents import dispatch as dispatch_mod
    dispatch_mod._daemon_rpc(
        "agent.deliver",
        {"name": "target-agent", "body": "hello world", "from_name": "orchestrator"},
    )
    t.join(timeout=2.0)

    import shutil
    shutil.rmtree(str(home_dir), ignore_errors=True)

    assert len(captured_params) == 1, f"Expected 1 request captured, got {len(captured_params)}"
    req = captured_params[0]

    # Pin the full contract shape that Rust's handle_deliver expects to deserialize.
    # Any field rename on either side breaks this test.
    assert isinstance(req.get("id"), int), f"id must be int, got {req.get('id')!r}"
    assert req.get("method") == "agent.deliver", f"method must be agent.deliver, got {req.get('method')!r}"
    params = req.get("params", {})
    # These three keys are the canonical Rust-side param names from handle_deliver.
    assert "name" in params, f"params must contain 'name'; got keys: {list(params)}"
    assert "body" in params, f"params must contain 'body'; got keys: {list(params)}"
    assert "from_name" in params, f"params must contain 'from_name'; got keys: {list(params)}"
    assert params["name"] == "target-agent"
    assert params["body"] == "hello world"
    assert params["from_name"] == "orchestrator"
    # No unexpected extra keys that would fail Rust's strict param parsing.
    expected_keys = {"name", "body", "from_name"}
    extra = set(params.keys()) - expected_keys
    assert not extra, f"params contains unexpected keys: {extra}"


# ---------------------------------------------------------------------------
# Gemini PR #459 round 1: malformed-response robustness (never raise)
# ---------------------------------------------------------------------------

def test_daemon_rpc_non_dict_response_returns_none(monkeypatch) -> None:
    """A JSON-array response from the daemon demotes (None), never crashes."""
    import tempfile

    home_dir = Path(tempfile.mkdtemp(prefix="/tmp/fno"))
    sock_path = home_dir / "supervisor.sock"

    responses = [["not", "a", "dict"]]
    _start_fake_daemon(sock_path, responses)
    monkeypatch.setenv("FNO_AGENTS_HOME", str(home_dir))

    from fno.agents import dispatch as dispatch_mod
    result = dispatch_mod._daemon_rpc(
        "agent.deliver", {"name": "x", "body": "y", "from_name": "z"}
    )
    assert result is None

    import shutil
    shutil.rmtree(str(home_dir), ignore_errors=True)


def test_daemon_rpc_malformed_json_response_returns_none(monkeypatch) -> None:
    """Malformed JSON bytes from the daemon demote (None), never raise.

    json.JSONDecodeError is a ValueError subclass; the OSError-only catch
    crashed here before the gemini round-1 fix.
    """
    import struct as _struct
    import tempfile
    import threading as _threading

    home_dir = Path(tempfile.mkdtemp(prefix="/tmp/fno"))
    sock_path = home_dir / "supervisor.sock"

    def _raw_server() -> None:
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(str(sock_path))
        srv.listen(1)
        ready.set()
        conn, _ = srv.accept()
        try:
            _read_frame(conn)
            payload = b"{not json"
            conn.sendall(_struct.pack("<I", len(payload)) + payload)
        except Exception:
            pass
        finally:
            conn.close()
            srv.close()

    ready = _threading.Event()
    t = _threading.Thread(target=_raw_server, daemon=True)
    t.start()
    ready.wait(timeout=10)
    monkeypatch.setenv("FNO_AGENTS_HOME", str(home_dir))

    from fno.agents import dispatch as dispatch_mod
    result = dispatch_mod._daemon_rpc(
        "agent.deliver", {"name": "x", "body": "y", "from_name": "z"}
    )
    assert result is None

    import shutil
    shutil.rmtree(str(home_dir), ignore_errors=True)


def test_dispatch_send_stamp_valueerror_non_fatal(
    tmp_path: Path, monkeypatch
) -> None:
    """A ValueError from the registry stamp must not crash dispatch_send."""
    use_tmpdir(monkeypatch, tmp_path)
    _register_codex_peer()

    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(
        dispatch_mod, "_daemon_rpc", lambda *a, **k: None
    )

    def _boom(*args, **kwargs):
        raise ValueError("registry validation failed")

    monkeypatch.setattr(dispatch_mod, "update_registry", _boom)

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_mod.dispatch_send(
        name="codex-agent", message="hello", provider=None, cwd=cwd
    )
    assert result.delivery == "durable"


def test_cmd_gate_retired_prints_pointer(runner: CliRunner) -> None:
    """`fno agents gate` was retired at G4: the injection gate gated the deleted
    daemon PTY-inject lane. It must print a one-line pointer and exit non-zero
    rather than hit UnknownMethod on the removed agent.gate_check handler (codex
    P2 on PR #148)."""
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["gate", "codex", "--probe"])
    assert result.exit_code == 2
    assert "retired at G4" in result.output


# ---------------------------------------------------------------------------
# Codex PR #459 round 1
# ---------------------------------------------------------------------------

def test_deliver_live_mcp_row_delivers_via_control_sock(
    tmp_path: Path, monkeypatch
) -> None:
    """The redundant MCP fast lane is retired (US5): a row with an mcp_channel_id
    delivers over the confirming control.sock lane, never the fire-and-forget
    send_to_channel push (which reported hosted on bytes-written, banned by LD4).
    """
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry
    write_registry([
        AgentEntry(
            name="claude-mcp",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/claude-mcp.log",
            short_id="abcd1234",
            status="live",
            mcp_channel_id="abcd1234",
        )
    ])

    from fno.mcp import client as mcp_client
    push_calls: list = []
    monkeypatch.setattr(
        mcp_client, "send_to_channel",
        lambda routing_key, envelope: push_calls.append((routing_key, envelope)),
    )

    from fno.agents import dispatch as dispatch_mod
    inject_calls: list = []

    def _ok_inject(recipient: str, text: str, **_k) -> bool:
        inject_calls.append(recipient)
        return True

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", _ok_inject)

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="claude-mcp", message="fyi built", provider=None, cwd=cwd
    )

    assert result.delivery == "hosted"
    assert len(push_calls) == 0, "the retired MCP fast lane must not fire"
    assert inject_calls == ["abcd1234"], "delivery rides the confirming control.sock lane"


def test_deliver_live_mcp_channel_id_is_the_recipient_fallback(
    tmp_path: Path, monkeypatch
) -> None:
    """No former MCP recipient is stranded by the lane retirement: a live row
    whose plain short_id was cleared (x-3dac) and which carries no
    harness_session_id still resolves a control.sock recipient via its
    mcp_channel_id -- which is the original roster-resolvable short_id, minted 1:1
    by register_mcp_channel."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry
    write_registry([
        AgentEntry(
            name="claude-mcp-only",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/claude-mcp-only.log",
            short_id="",                # cleared on a live row (x-3dac)
            status="live",
            mcp_channel_id="efgh5678",  # == the original short_id
        )
    ])

    from fno.agents import dispatch as dispatch_mod
    inject_calls: list = []

    def _ok_inject(recipient: str, text: str, **_k) -> bool:
        inject_calls.append(recipient)
        return True

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", _ok_inject)

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="claude-mcp-only", message="fyi built", provider=None, cwd=cwd
    )

    assert result.delivery == "hosted"
    assert inject_calls == ["efgh5678"], "recipient falls back to mcp_channel_id"


# ---------------------------------------------------------------------------
# node x-3dac: control.sock is the sole live inject lane. The claude PTY
# worker.sock lane retired with daemon PTY hosting (x-f54c), so a live claude
# recipient is driven over control.sock op:reply (or falls through to durable).
# ---------------------------------------------------------------------------

def test_deliver_live_claude_no_live_lane_queues_durable(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-EDGE: a recipient with no live worker.sock AND no live control.sock
    still queues durable (exit 0, delivery != hosted)."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry
    write_registry([
        AgentEntry(
            name="offline-claude",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/offline-claude.log",
            harness_session_id="bbbb0002-1111-2222-3333-444444444444",
            status="live",
        )
    ])

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)
    monkeypatch.setattr(
        dispatch_mod, "_daemon_rpc",
        lambda method, params, **kw: {"delivered": False, "reason": "not-a-live-stream-thread"},
    )
    # The control.sock inject misses -> durable fallback.
    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", lambda *a, **kw: False)

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="offline-claude", message="hello?", provider=None, cwd=cwd
    )
    assert result.delivery != "hosted", "no live lane -> durable fallback"


def test_deliver_live_claude_control_lane_delivers_with_envelope(
    tmp_path: Path, monkeypatch
) -> None:
    """A live claude recipient is reached over the control.sock lane (the sole
    live inject path after the PTY worker lane retired, x-3dac), and the injected
    turn carries the <fno_mail> envelope with an 8-hex `from` and no `session=`."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry
    write_registry([
        AgentEntry(
            name="sender",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/sender.log",
            harness_session_id="5e9de401-1111-2222-3333-444444444444",
            status="live",
        ),
        AgentEntry(
            name="adopted-bg",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/adopted-bg.log",
            harness_session_id="cccc0003-1111-2222-3333-444444444444",
            status="live",
        ),
    ])

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)
    monkeypatch.setattr(
        dispatch_mod, "_daemon_rpc",
        lambda method, params, **kw: {"delivered": False, "reason": "not-a-live-stream-thread"},
    )

    inject_calls: list = []

    def _ok_inject(recipient: str, text: str, **_k) -> bool:
        inject_calls.append({"recipient": recipient, "text": text})
        return True

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", _ok_inject)

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="adopted-bg", message="reach me on control", provider=None,
        cwd=cwd, from_name="sender",
    )

    assert result.delivery == "hosted", "live control.sock recipient delivers, not durable"
    assert len(inject_calls) == 1, "the control.sock lane is the sole live path"
    import re

    framed = inject_calls[0]["text"]
    assert re.match(r'^<fno_mail from="[0-9a-f]{8}"', framed), framed
    assert framed.rstrip().endswith("</fno_mail>"), framed
    assert "reach me on control" in framed
    assert "session=" not in framed


# ---------------------------------------------------------------------------
# node x-1f23: the autonomous relay continuations carry <fno_mail>, not just
# the seed (codex P2). Chat (no mail ctxs) stays raw.
# ---------------------------------------------------------------------------

def test_relay_loop_wraps_continuations_with_mail_ctxs(monkeypatch) -> None:
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import _MailCtx, _run_relay_loop

    calls: list = []

    def _rpc(method, params, **kw):
        calls.append(params)
        return {
            "delivered": True,
            "identity_verified": True,
            "reply": "",
        }  # empty reply ends the loop

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)

    ctxs = {
        "alice": _MailCtx(from_="aaaa1111", harness="claude-code", model="unknown", to="bbbb2222"),
        "bob": _MailCtx(from_="bbbb2222", harness="claude-code", model="unknown", to="aaaa1111"),
    }
    # seed = bob's reply; first continuation drives alice with bob's turn, so the
    # hop body is wrapped as BOB (the peer who just spoke).
    _run_relay_loop(
        "bob",
        "alice",
        "bob says hi",
        ceiling=3,
        mail_ctxs=ctxs,
        recipient_identities=_sb_identities("alice", "bob"),
    )
    assert len(calls) == 1
    body = calls[0]["body"]
    assert body.startswith('<fno_mail from="bbbb2222"'), body
    assert body.rstrip().endswith("</fno_mail>")
    assert "bob says hi" in body


def test_relay_loop_raw_without_mail_ctxs_chat_path(monkeypatch) -> None:
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import _run_relay_loop

    calls: list = []
    monkeypatch.setattr(
        dispatch_mod, "_daemon_rpc",
        lambda method, params, **kw: calls.append(params)
        or {"delivered": True, "identity_verified": True, "reply": ""},
    )
    # No mail ctxs (the chat path) -> body stays raw, no envelope.
    _run_relay_loop(
        "bob",
        "alice",
        "bob says hi",
        ceiling=3,
        recipient_identities=_sb_identities("alice", "bob"),
    )
    assert calls[0]["body"] == "bob says hi"


def test_mail_context_uses_canonical_sender_handle(monkeypatch) -> None:
    from fno.agents.dispatch import _build_mail_ctx

    monkeypatch.setattr("fno.agents.self_stamp.resolve_self_model", lambda: "unknown")
    context = _build_mail_ctx(
        "friendly-name",
        "019fb417-1111-7222-8333-444455556666",
        "codex",
    )
    assert context.from_ == "019fb417"


def test_first_hop_read_budget_covers_the_daemon_drive_ceiling() -> None:
    """The client must never abandon a turn the daemon is still driving.

    `agent.switchboard_v2` does not ack and return: the daemon drives B's whole
    turn before answering. A read budget below that ceiling makes the client give
    up on a body B already received, and `_deliver_live` then injects it again.
    The two numbers live in different languages, so pin them against each other
    rather than against a literal.
    """
    import re
    from pathlib import Path

    from fno.agents.dispatch import _SWITCHBOARD_FIRST_HOP_READ_TIMEOUT

    daemon = Path(__file__).resolve().parents[3] / "crates/fno-agents/src/daemon.rs"
    src = daemon.read_text(encoding="utf-8")
    turn_ms = re.search(r"const SWITCHBOARD_TURN_TIMEOUT_MS: u64 = ([\d_]+);", src)
    grace_s = re.search(r"const SWITCHBOARD_DRIVE_GRACE_S: u64 = ([\d_]+);", src)
    assert turn_ms and grace_s, "the daemon's switchboard budget constants moved"

    ceiling = int(turn_ms.group(1).replace("_", "")) / 1000 + int(grace_s.group(1))
    assert _SWITCHBOARD_FIRST_HOP_READ_TIMEOUT >= ceiling, (
        f"first-hop read budget {_SWITCHBOARD_FIRST_HOP_READ_TIMEOUT}s is below the "
        f"daemon's own drive ceiling {ceiling}s; a send would be delivered twice"
    )
