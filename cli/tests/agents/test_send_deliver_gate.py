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
import threading
from pathlib import Path
from typing import Optional

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


# ---------------------------------------------------------------------------
# Helper: register live peers
# ---------------------------------------------------------------------------

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


def _register_gemini_peer(name: str = "gemini-agent") -> None:
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name=name,
            provider="gemini",
            cwd="/tmp",
            log_path="/tmp/gemini-agent.log",
            gemini_session_id="gemini-session-001",
            status="live",
        )
    ])


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


def _fake_daemon(sock_path: Path, responses: list[dict], received: list[dict]) -> None:
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(str(sock_path))
    srv.listen(1)
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
    t = threading.Thread(
        target=_fake_daemon,
        args=(sock_path, responses, received),
        daemon=True,
    )
    t.start()
    import time
    for _ in range(50):
        if sock_path.exists():
            break
        time.sleep(0.01)
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
    assert read_all_threads("codex-agent") == [], "hosted delivery must not queue durable"

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

    threads = read_all_threads("codex-agent")
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

    threads = read_all_threads("codex-agent")
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
    threads = read_all_threads("codex-agent")
    assert len(threads) == 1, "envelope must survive RPC error"


# ---------------------------------------------------------------------------
# Claude peer + switchboard (Group 2, Task 3.1): claude now probes the
# `agent.switchboard` RPC first; a non-stream-thread demotes to socket/MCP
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
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/claude-peer.log",
            claude_short_id="abcd1234",
            status="live",
        )
    ])

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)

    # The control.sock inject (mail-inject verb) is the socket-path successor; it
    # succeeds here so the demote falls through to it.
    inject_calls: list = []

    def _ok_inject(recipient: str, text: str) -> bool:
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
    assert rpc_calls[0]["method"] == "agent.switchboard"
    assert rpc_calls[0]["params"]["to"] == "claude-peer"


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
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/claude-stream.log",
            claude_short_id="abcd1234",
            claude_session_uuid="11111111-2222-3333-4444-555555555555",
            status="live",
        )
    ])

    from fno.agents.providers import claude as claude_mod

    send_calls: list = []
    monkeypatch.setattr(claude_mod, "send_to_session", lambda *a, **kw: send_calls.append(1))

    rpc_calls: list = []
    from fno.agents import dispatch as dispatch_mod

    # Pin OBSERVED mode (auto off) so this stays a single-hop delivery test;
    # the A2A relay loop is exercised by the 4.1 tests below.
    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (False, 6))

    def _mock_rpc(method: str, params: dict, **kwargs):
        rpc_calls.append({"method": method, "params": params})
        return {"delivered": True, "transport": "switchboard", "reply": "ack", "mirrored": True}

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
    assert rpc_calls[0]["method"] == "agent.switchboard"
    assert rpc_calls[0]["params"]["mirror"] is True, "observed mode mirrors into A"
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
        return {"delivered": True, "reply": "r1"}

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)
    assert dispatch_mod._switchboard_exchange("B", "A", "msg") is True
    assert len(calls) == 1
    assert calls[0]["mirror"] is True
    assert calls[0]["to"] == "B" and calls[0]["from"] == "A"


def test_switchboard_auto_is_nonblocking_kicks_off_detached_relay(monkeypatch) -> None:
    """ab-3bd520ab: auto=True drives B synchronously (hop 1 = the actual delivery)
    then KICKS OFF the relay in the background and returns immediately. The caller
    runs exactly ONE _daemon_rpc inline; the relay loop never runs in-process."""
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (True, 6))
    calls: list = []

    def _rpc(method, params, **kw):
        calls.append(params)
        return {"delivered": True, "reply": "r1"}

    kicked: list = []
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)
    monkeypatch.setattr(
        dispatch_mod,
        "_kickoff_background_relay",
        lambda to_name, from_name, seed, ceiling, mail_ctxs=None: kicked.append(
            (to_name, from_name, seed, ceiling, mail_ctxs)
        ),
    )
    assert dispatch_mod._switchboard_exchange("B", "A", "msg") is True
    assert len(calls) == 1, "only the first hop (drive B) runs inline; the relay is detached"
    assert calls[0]["to"] == "B" and calls[0]["mirror"] is False
    # No mail ctxs on this bare _switchboard_exchange call -> chat-style raw relay.
    assert kicked == [("B", "A", "r1", 6, None)], "the relay is handed off with B's reply as the seed"


def test_switchboard_auto_no_kickoff_when_first_reply_empty(monkeypatch) -> None:
    """B delivered but replied empty -> nothing to relay, so no background kickoff."""
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (True, 6))
    monkeypatch.setattr(
        dispatch_mod, "_daemon_rpc", lambda *a, **k: {"delivered": True, "reply": ""}
    )
    kicked: list = []
    monkeypatch.setattr(
        dispatch_mod, "_kickoff_background_relay", lambda *a: kicked.append(a)
    )
    assert dispatch_mod._switchboard_exchange("B", "A", "msg") is True
    assert kicked == [], "an empty first reply has no relay to run"


def test_relay_loop_bounded_by_ceiling(monkeypatch, capsys) -> None:
    """_run_relay_loop with an always-replying pair runs up to turn_ceiling total
    turns (the first hop already happened) and stops with 'loop ceiling reached',
    alternating A, B, A after the seed."""
    from fno.agents import dispatch as dispatch_mod

    calls: list = []

    def _rpc(method, params, **kw):
        calls.append(params)
        return {"delivered": True, "reply": "more"}

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)
    dispatch_mod._run_relay_loop("B", "A", "r1", 4)
    # ceiling=4 total; the first hop (drive B) is the caller's, so the loop runs 3.
    assert [c["to"] for c in calls] == ["A", "B", "A"]
    assert all(c["mirror"] is False for c in calls)
    assert "loop ceiling reached" in capsys.readouterr().err


def test_relay_loop_stops_on_empty_reply(monkeypatch, capsys) -> None:
    """A side that produces no reply ends the relay before the ceiling, with no
    'loop ceiling reached' notice."""
    from fno.agents import dispatch as dispatch_mod

    calls: list = []

    def _rpc(method, params, **kw):
        calls.append(params)
        return {"delivered": True, "reply": ""}  # A replies empty on the first relay hop

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)
    dispatch_mod._run_relay_loop("B", "A", "r1", 8)
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
    dispatch_mod._run_relay_loop("B", "A", "r1", 6)
    assert len(calls) == 1, "a single failed relay hop to A ends the exchange"


def test_switchboard_demote_when_first_hop_not_delivered(monkeypatch) -> None:
    """B not a live stream thread on the first hop -> None (caller demotes)."""
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (True, 6))
    monkeypatch.setattr(
        dispatch_mod,
        "_daemon_rpc",
        lambda *a, **k: {"delivered": False, "reason": "not-a-live-stream-thread"},
    )
    assert dispatch_mod._switchboard_exchange("B", "A", "msg") is None


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

    def _raw_fake_daemon(path: Path, resp_list: list, recv_list: list) -> None:
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(str(path))
        srv.listen(1)
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
    import time
    for _ in range(50):
        if sock_path.exists():
            break
        time.sleep(0.01)

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

    def _capturing_daemon(path: Path) -> None:
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(str(path))
        srv.listen(1)
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

    import threading
    t = threading.Thread(target=_capturing_daemon, args=(sock_path,), daemon=True)
    t.start()
    import time
    for _ in range(50):
        if sock_path.exists():
            break
        time.sleep(0.01)

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

    import time as _time

    t = _threading.Thread(target=_raw_server, daemon=True)
    t.start()
    for _ in range(100):
        if sock_path.exists():
            break
        _time.sleep(0.01)
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

def test_deliver_live_claude_mcp_send_only_no_reply_wait(
    tmp_path: Path, monkeypatch
) -> None:
    """codex #459 P2: a reachable MCP channel delivers via the send-only
    sidecar push (build_channel_notification + send_to_channel), never the
    blocking ask_followup_via_mcp, and never falls through to the socket."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry
    write_registry([
        AgentEntry(
            name="claude-mcp",
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/claude-mcp.log",
            claude_short_id="abcd1234",
            status="live",
            mcp_channel_id="abcd1234",
        )
    ])

    from fno.agents.providers import claude as claude_mod
    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: True)

    socket_calls: list = []
    monkeypatch.setattr(
        claude_mod, "send_to_session", lambda *a, **kw: socket_calls.append(1)
    )

    ask_mcp_calls: list = []
    monkeypatch.setattr(
        claude_mod, "ask_followup_via_mcp",
        lambda *a, **kw: ask_mcp_calls.append(1),
    )

    from fno.mcp import client as mcp_client
    push_calls: list = []
    monkeypatch.setattr(
        mcp_client, "send_to_channel",
        lambda routing_key, envelope: push_calls.append((routing_key, envelope)),
    )

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="claude-mcp", message="fyi built", provider=None, cwd=cwd
    )

    assert result.delivery == "hosted"
    assert len(push_calls) == 1, "MCP sidecar push must be used"
    assert push_calls[0][0] == "abcd1234"
    assert len(socket_calls) == 0, "socket path must not fire when MCP delivers"
    assert len(ask_mcp_calls) == 0, "blocking ask_followup_via_mcp must NOT be used for send"


def test_deliver_live_claude_mcp_error_falls_back_to_socket(
    tmp_path: Path, monkeypatch
) -> None:
    """MCP sidecar failure falls through to the socket path (demotion chain)."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry
    write_registry([
        AgentEntry(
            name="claude-mcp2",
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/claude-mcp2.log",
            claude_short_id="efgh5678",
            status="live",
            mcp_channel_id="efgh5678",
        )
    ])

    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: True)

    from fno.mcp import client as mcp_client

    def _boom(routing_key, envelope):
        raise mcp_client.MCPSidecarUnreachable("sidecar gone")

    monkeypatch.setattr(mcp_client, "send_to_channel", _boom)

    # The control.sock inject (mail-inject verb) is the socket-path successor.
    from fno.agents import dispatch as dispatch_mod

    inject_calls: list = []

    def _ok_inject(recipient: str, text: str) -> bool:
        inject_calls.append({"recipient": recipient, "text": text})
        return True

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", _ok_inject)

    from fno.agents.dispatch import dispatch_send

    cwd = tmp_path / "work"
    cwd.mkdir()
    result = dispatch_send(
        name="claude-mcp2", message="fyi built", provider=None, cwd=cwd
    )

    assert result.delivery == "hosted"
    assert len(inject_calls) == 1, "control.sock inject must fire on sidecar failure"


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
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/offline-claude.log",
            claude_session_uuid="bbbb0002-1111-2222-3333-444444444444",
            status="live",
        )
    ])

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

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
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/sender.log",
            claude_session_uuid="5e9de401-1111-2222-3333-444444444444",
            status="live",
        ),
        AgentEntry(
            name="adopted-bg",
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/adopted-bg.log",
            claude_session_uuid="cccc0003-1111-2222-3333-444444444444",
            status="live",
        ),
    ])

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", lambda *a, **kw: False)
    monkeypatch.setattr(
        dispatch_mod, "_daemon_rpc",
        lambda method, params, **kw: {"delivered": False, "reason": "not-a-live-stream-thread"},
    )

    inject_calls: list = []

    def _ok_inject(recipient: str, text: str) -> bool:
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
        return {"delivered": True, "reply": ""}  # empty reply ends the loop

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)

    ctxs = {
        "alice": _MailCtx(from_="aaaa1111", harness="claude-code", model="unknown", to="bbbb2222"),
        "bob": _MailCtx(from_="bbbb2222", harness="claude-code", model="unknown", to="aaaa1111"),
    }
    # seed = bob's reply; first continuation drives alice with bob's turn, so the
    # hop body is wrapped as BOB (the peer who just spoke).
    _run_relay_loop("bob", "alice", "bob says hi", ceiling=3, mail_ctxs=ctxs)
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
        lambda method, params, **kw: calls.append(params) or {"delivered": True, "reply": ""},
    )
    # No mail ctxs (the chat path) -> body stays raw, no envelope.
    _run_relay_loop("bob", "alice", "bob says hi", ceiling=3)
    assert calls[0]["body"] == "bob says hi"
