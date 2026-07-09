"""Tests for the US2 follow-up surface on providers.claude.

Task 2.3:
- send_to_session(sock_path, content, from_name) builds and writes the BG8 envelope
- liveness_probe(sock_path) -> True iff a 250ms connect succeeds
- wait_for_reply(jobs_dir, baseline, timeline_offset, timeout) polls state.json
- ask_followup(claude_short_id, message, cwd, from_name, timeout) orchestrates
  locate + probe + send + wait_for_reply
- Three new error classes: ProviderOrphanError, ProviderSocketError, ProviderTimeoutError

The tests mostly use a real ``socket.AF_UNIX`` server stub in a thread so
the protocol contract (envelope shape, newline framing) is verified
end-to-end without depending on real claude state.
"""
from __future__ import annotations

import json
import os
import socket
import sys
import tempfile
import threading
import uuid
from pathlib import Path
from typing import Any

import pytest


def _short_sock_path() -> str:
    """Return a unique AF_UNIX path under /tmp.

    macOS caps sun_path at 104 bytes; pytest's tmp_path under
    ``/private/var/folders/...`` regularly exceeds that. Tests that need a
    real bound socket use this helper instead of ``tmp_path / "sock"``.
    """
    return os.path.join("/tmp", f"abi-us2-{uuid.uuid4().hex[:12]}.sock")


# ---------------------------------------------------------------------------
# Symbol surface
# ---------------------------------------------------------------------------


def test_followup_module_exports() -> None:
    from fno.agents.providers import claude as claude_mod

    for sym in (
        "send_to_session",
        "liveness_probe",
        "wait_for_reply",
        "ask_followup",
        "ProviderOrphanError",
        "ProviderSocketError",
        "ProviderTimeoutError",
    ):
        assert hasattr(claude_mod, sym), f"missing symbol: {sym}"


# ---------------------------------------------------------------------------
# Helpers — Unix-socket stub server
# ---------------------------------------------------------------------------


# READINESS HANDSHAKE (read before adding a socket/timing test here)
# -------------------------------------------------------------------
# These tests stand up a real AF_UNIX listener to exercise the claude
# follow-up path. They must stay green even while a concurrent `cargo build`
# saturates every core (the observed flake trigger). The rule: NEVER use a
# fixed `time.sleep(...)` poll loop as a "the other side is ready now"
# barrier -- a budget sized for an idle box becomes a coin flip under load.
# Wait on an OBSERVED signal instead: `_UnixSocketServer` sets `listening`
# after bind+listen and `received_event` on the first bytes, so a test
# synchronizes on `.wait(...)` rather than on a sleep that may or may not be
# long enough. Test-local budgets may be widened as bounded defense-in-depth
# (generous enough for scheduling latency, short enough that a real hang
# still fails in seconds), but they are never the primary sync mechanism.
class _UnixSocketServer:
    """Tiny single-connection AF_UNIX stub.

    Accepts one connection, reads until EOF or until newline-terminated
    payload arrives, stashes the bytes for the test to assert against.

    Readiness is signalled explicitly rather than implied by call ordering:
    ``listening`` is set after bind+listen (so a client can wait for it before
    connecting), and ``received_event`` is set as soon as the first bytes
    arrive (so a test can wait for the send to land without a sleep poll).
    """

    def __init__(self, sock_path: str) -> None:
        self.sock_path = sock_path
        self.received: bytes = b""
        self.received_event = threading.Event()
        self.listening = threading.Event()
        self._srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._srv.bind(sock_path)
        # Generous backlog (not listen(1)): ask_followup makes two back-to-back
        # connects (liveness probe, then send). With a backlog of 1, the probe
        # can occupy the only queue slot, so under CPU saturation the send's
        # connect races a not-yet-drained backlog and is refused ([Errno 61]) --
        # the observed flake. A real `claude --bg` daemon listens with a normal
        # backlog, so listen(1) was an unfaithfully tiny stub, not a real limit.
        self._srv.listen(128)
        self._srv.settimeout(5.0)
        # bind+listen completed synchronously in __init__, so the listener is
        # accepting before any client connects (the accept backlog absorbs a
        # connect that races ahead of the accept() call -- no Errno 61 here).
        self.listening.set()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self) -> None:
        self._thread.start()

    def _run(self) -> None:
        # ask_followup connects TWICE: a liveness probe (connect-then-close,
        # zero bytes) and then the real send carrying the envelope. A direct
        # send_to_session test connects once with bytes. Loop over accepts and
        # treat the first connection that delivers bytes as the real send --
        # exactly like the Rust accept thread -- skipping a zero-byte probe
        # connection. The listener's 5s timeout bounds the wait if no send
        # ever arrives.
        while True:
            try:
                conn, _ = self._srv.accept()
            except (OSError, socket.timeout) as exc:
                # No further connection arrived within the listen timeout.
                # Surface the cause so a genuine never-sent failure is
                # distinguishable from a mid-recv fault (caught below) when
                # the test fails on the received_event barrier.
                print(f"_UnixSocketServer: accept ended: {exc!r}", file=sys.stderr)
                return
            conn.settimeout(5.0)
            got = b""
            try:
                with conn:
                    while True:
                        data = conn.recv(8192)
                        if not data:
                            break
                        got += data
            except (OSError, socket.timeout) as exc:
                # A recv fault on THIS connection (e.g. the liveness probe
                # sending an RST under load) must NOT kill the server thread,
                # or the real send that follows is never accepted and the test
                # flakes. Skip the faulted connection and keep accepting; log
                # the cause so it is not confused with "the send never arrived".
                print(f"_UnixSocketServer: recv aborted: {exc!r}", file=sys.stderr)
                continue
            if got:
                self.received += got
                # Signal the observed send so a test can wait on a real
                # event instead of polling .received with sleeps.
                self.received_event.set()
                return
            # zero-byte connection == liveness probe; wait for the real send

    def close(self) -> None:
        # Wait for the recv loop to drain via EOF from the client close,
        # THEN tear down the listener. Closing the listener first does not
        # interrupt the established conn's recv, so we'd race on it.
        self._thread.join(timeout=3.0)
        try:
            self._srv.close()
        except OSError:
            pass


# ---------------------------------------------------------------------------
# send_to_session
# ---------------------------------------------------------------------------


def test_send_to_session_writes_documented_envelope() -> None:
    """AF_UNIX write contains the BG8 envelope JSON + trailing newline."""
    from fno.agents.providers.claude import send_to_session

    sock_path = _short_sock_path()
    server = _UnixSocketServer(sock_path)
    server.start()

    try:
        send_to_session(sock_path, "do the thing", "fno")
    finally:
        server.close()
        try:
            os.unlink(sock_path)
        except FileNotFoundError:
            pass

    raw = server.received
    assert raw.endswith(b"\n"), "envelope must be newline-terminated"

    envelope = json.loads(raw.rstrip(b"\n").decode("utf-8"))
    assert envelope["type"] == "user"
    assert envelope["priority"] == "next"

    content = envelope["message"]["content"]
    assert content.startswith("<cross-session-message from-name=\"fno\">\n")
    assert content.endswith("\n</cross-session-message>")
    assert "do the thing" in content


def test_send_to_session_escapes_xml_unsafe_from_name() -> None:
    """from_name with XML-active chars gets html-escaped before the attribute."""
    from fno.agents.providers.claude import send_to_session

    sock_path = _short_sock_path()
    server = _UnixSocketServer(sock_path)
    server.start()

    try:
        # Note: this test verifies escape; per AC2-ERR validation rejects
        # XML-unsafe input at the dispatch layer BEFORE this is called.
        # send_to_session must still produce a safe envelope if reached.
        send_to_session(sock_path, "msg", "Alice & Bob")
    finally:
        server.close()
        try:
            os.unlink(sock_path)
        except FileNotFoundError:
            pass

    envelope = json.loads(server.received.rstrip(b"\n").decode("utf-8"))
    content = envelope["message"]["content"]
    assert "from-name=\"Alice &amp; Bob\"" in content
    assert "Alice & Bob\"" not in content  # raw ampersand never reaches attr


def test_send_to_session_raises_socket_error_on_connect_refused(tmp_path: Path) -> None:
    """A non-existent socket path -> ProviderSocketError with underlying error preserved."""
    from fno.agents.providers.claude import (
        ProviderSocketError,
        send_to_session,
    )

    bogus = str(tmp_path / "does-not-exist.sock")
    with pytest.raises(ProviderSocketError):
        send_to_session(bogus, "x", "fno")


def test_send_to_session_passes_500kb_message() -> None:
    """No argv-style limit; 500KB message rides the socket fine."""
    from fno.agents.providers.claude import send_to_session

    sock_path = _short_sock_path()
    server = _UnixSocketServer(sock_path)
    server.start()

    big = "x" * (500 * 1024)
    try:
        send_to_session(sock_path, big, "fno")
    finally:
        server.close()
        try:
            os.unlink(sock_path)
        except FileNotFoundError:
            pass

    envelope = json.loads(server.received.rstrip(b"\n").decode("utf-8"))
    assert big in envelope["message"]["content"]


# ---------------------------------------------------------------------------
# liveness_probe
# ---------------------------------------------------------------------------


def test_liveness_probe_true_when_socket_accepts() -> None:
    from fno.agents.providers.claude import liveness_probe

    sock_path = _short_sock_path()
    server = _UnixSocketServer(sock_path)
    server.start()

    try:
        assert liveness_probe(sock_path) is True
    finally:
        server.close()
        try:
            os.unlink(sock_path)
        except FileNotFoundError:
            pass


def test_liveness_probe_false_when_socket_absent(tmp_path: Path) -> None:
    from fno.agents.providers.claude import liveness_probe

    bogus = str(tmp_path / "missing.sock")
    assert liveness_probe(bogus) is False


# ---------------------------------------------------------------------------
# wait_for_reply
# ---------------------------------------------------------------------------


def _write_state(jobs_dir: Path, *, state: str, updated_at: str,
                 result: Any = None) -> None:
    jobs_dir.mkdir(parents=True, exist_ok=True)
    output: dict[str, Any] = {}
    if result is not None:
        output["result"] = result
    (jobs_dir / "state.json").write_text(
        json.dumps({"state": state, "updatedAt": updated_at, "output": output}),
        encoding="utf-8",
    )


def test_wait_for_reply_returns_output_result_when_state_transitions(tmp_path: Path,
                                                                      monkeypatch) -> None:
    """state.updated_at > baseline AND state in terminal set -> return result."""
    from fno.agents.providers.claude import wait_for_reply

    monkeypatch.setenv("HOME", str(tmp_path))
    jobs_dir = tmp_path / "jobs" / "abc"
    _write_state(jobs_dir, state="completed", updated_at="T2",
                 result="OK, done")

    reply = wait_for_reply(
        jobs_dir, baseline_updated_at="T1", timeline_offset=0,
        timeout=2.0, poll_interval=0.05,
    )
    assert reply == "OK, done"


def test_wait_for_reply_excludes_stale_pre_baseline_content(tmp_path: Path) -> None:
    """If updated_at == baseline, the poll must NOT return the stale result."""
    from fno.agents.providers.claude import (
        ProviderTimeoutError,
        wait_for_reply,
    )

    jobs_dir = tmp_path / "jobs" / "abc"
    _write_state(jobs_dir, state="completed", updated_at="T1",
                 result="previous reply")

    with pytest.raises(ProviderTimeoutError):
        wait_for_reply(
            jobs_dir, baseline_updated_at="T1", timeline_offset=0,
            timeout=0.3, poll_interval=0.05,
        )


def test_wait_for_reply_falls_back_to_timeline_when_result_empty(tmp_path: Path) -> None:
    """output.result empty/None -> read_timeline_tail fallback."""
    from fno.agents.providers.claude import wait_for_reply

    jobs_dir = tmp_path / "jobs" / "fallback"
    jobs_dir.mkdir(parents=True)
    (jobs_dir / "timeline.jsonl").write_text(
        json.dumps({"at": "T2", "state": "completed", "text": "from timeline"}) + "\n",
        encoding="utf-8",
    )
    _write_state(jobs_dir, state="completed", updated_at="T2", result=None)

    reply = wait_for_reply(
        jobs_dir, baseline_updated_at="T1", timeline_offset=0,
        timeout=2.0, poll_interval=0.05,
    )
    assert reply == "from timeline"


def test_wait_for_reply_times_out_when_no_transition(tmp_path: Path) -> None:
    from fno.agents.providers.claude import (
        ProviderTimeoutError,
        wait_for_reply,
    )

    jobs_dir = tmp_path / "jobs" / "still"
    _write_state(jobs_dir, state="running", updated_at="T0", result=None)

    with pytest.raises(ProviderTimeoutError) as exc_info:
        wait_for_reply(
            jobs_dir, baseline_updated_at="T0", timeline_offset=0,
            timeout=0.3, poll_interval=0.05,
        )
    # carries elapsed_sec for stderr formatting
    assert hasattr(exc_info.value, "elapsed_sec")
    assert exc_info.value.elapsed_sec >= 0.3


def test_wait_for_reply_waits_for_terminal_state(tmp_path: Path) -> None:
    """Even with updatedAt > baseline, non-terminal state must not exit early."""
    from fno.agents.providers.claude import (
        ProviderTimeoutError,
        wait_for_reply,
    )

    jobs_dir = tmp_path / "jobs" / "tool"
    _write_state(jobs_dir, state="running", updated_at="T9", result="partial")

    with pytest.raises(ProviderTimeoutError):
        wait_for_reply(
            jobs_dir, baseline_updated_at="T0", timeline_offset=0,
            timeout=0.3, poll_interval=0.05,
        )


def test_wait_for_reply_handles_state_transition_to_needs_input(tmp_path: Path) -> None:
    """needs-input is in the terminal-exit set."""
    from fno.agents.providers.claude import wait_for_reply

    jobs_dir = tmp_path / "jobs" / "qa"
    _write_state(jobs_dir, state="needs-input", updated_at="T2",
                 result="What's the file path?")

    reply = wait_for_reply(
        jobs_dir, baseline_updated_at="T1", timeline_offset=0,
        timeout=2.0, poll_interval=0.05,
    )
    assert reply == "What's the file path?"


# ---------------------------------------------------------------------------
# ask_followup orchestrator
# ---------------------------------------------------------------------------


def _setup_session_file(tmp_path: Path, pid: int, short_id: str,
                        sock_path: str) -> None:
    sessions = tmp_path / ".claude" / "sessions"
    sessions.mkdir(parents=True, exist_ok=True)
    (sessions / f"{pid}.json").write_text(
        json.dumps({
            "messagingSocketPath": sock_path,
            "jobId": short_id,
            "kind": "bg",
            "sessionId": f"s-{pid}",
            "cwd": "/tmp",
        }),
        encoding="utf-8",
    )


def test_ask_followup_happy_path(tmp_path: Path, monkeypatch) -> None:
    """locate -> probe -> send -> wait_for_reply -> reply string.

    De-flaked and re-enabled in CI (was @pytest.mark.flaky_socket): the
    send-landed barrier is now an observed `threading.Event` instead of a
    fixed `for _ in range(40): time.sleep(0.05)` poll, so it does not depend
    on the recipient being scheduled inside a sleep budget under load.
    """
    from fno.agents.providers.claude import ask_followup

    monkeypatch.setenv("HOME", str(tmp_path))
    short_id = "abc12345"
    sock_path = _short_sock_path()
    _setup_session_file(tmp_path, pid=11, short_id=short_id, sock_path=sock_path)
    jobs_dir = tmp_path / ".claude" / "jobs" / short_id

    # Start a stub server that accepts the send, then we update state.json
    # post-send to simulate the recipient processing the message. The listener
    # is bound+listening before we launch the client (server.listening is set
    # in __init__), so the connect cannot be refused.
    server = _UnixSocketServer(sock_path)
    server.start()
    assert server.listening.wait(timeout=5.0), "stub never reached listen()"

    # Run ask_followup in a background thread so we can update state.json
    # after the send completes. Test-local budgets are widened as bounded
    # defense-in-depth (10s/15s vs the production-realistic 3s) so a
    # late-scheduled thread under a saturating build is still observed, while
    # a genuine hang still fails in seconds rather than minutes.
    result_holder: dict[str, Any] = {}

    def _runner() -> None:
        try:
            result_holder["reply"] = ask_followup(
                claude_short_id=short_id,
                message="please add validation",
                cwd=Path("/tmp"),
                from_name="fno",
                timeout=10.0,
                poll_interval=0.05,
            )
        except BaseException as exc:  # noqa: BLE001
            result_holder["error"] = exc

    runner = threading.Thread(target=_runner)
    runner.start()

    # Wait for the send to land via an observed event (not a sleep poll),
    # then write the terminal state.json wait_for_reply is polling for.
    assert server.received_event.wait(timeout=10.0), "send was never observed by the stub"
    _write_state(jobs_dir, state="completed", updated_at="POSTSEND",
                 result="validation added")

    runner.join(timeout=15.0)
    server.close()
    try:
        os.unlink(sock_path)
    except FileNotFoundError:
        pass

    assert not runner.is_alive(), "ask_followup runner did not finish"
    # LOAD-BEARING: _runner stashes any exception in result_holder["error"]
    # rather than raising it, and received_event fires before wait_for_reply
    # runs (it is set on the send, which precedes the poll). So a post-send
    # failure (e.g. wait_for_reply regressing) is caught HERE, not at the
    # barrier above. Do not drop this assertion.
    assert "error" not in result_holder, result_holder.get("error")
    assert result_holder["reply"] == "validation added"


def test_ask_followup_orphan_when_session_missing(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.providers.claude import (
        ProviderOrphanError,
        ask_followup,
    )

    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / ".claude" / "sessions").mkdir(parents=True, exist_ok=True)

    with pytest.raises(ProviderOrphanError) as exc_info:
        ask_followup(
            claude_short_id="missing!",
            message="m",
            cwd=Path("/tmp"),
            from_name="fno",
            timeout=1.0,
            poll_interval=0.05,
        )
    assert exc_info.value.reason == "not-found"


def test_ask_followup_orphan_when_socket_null(tmp_path: Path, monkeypatch) -> None:
    """Session matches but messagingSocketPath=null -> orphan with reason=socket-null."""
    from fno.agents.providers.claude import (
        ProviderOrphanError,
        ask_followup,
    )

    monkeypatch.setenv("HOME", str(tmp_path))
    sessions = tmp_path / ".claude" / "sessions"
    sessions.mkdir(parents=True, exist_ok=True)
    (sessions / "20.json").write_text(
        json.dumps({
            "messagingSocketPath": None,
            "jobId": "abc12345",
            "kind": "bg",
        }),
        encoding="utf-8",
    )

    with pytest.raises(ProviderOrphanError) as exc_info:
        ask_followup(
            claude_short_id="abc12345",
            message="m",
            cwd=Path("/tmp"),
            from_name="fno",
            timeout=1.0,
            poll_interval=0.05,
        )
    assert exc_info.value.reason == "socket-null"


def test_ask_followup_orphan_when_liveness_fails(tmp_path: Path, monkeypatch) -> None:
    """Session entry has socket path that doesn't connect -> orphan with reason=liveness-failed."""
    from fno.agents.providers.claude import (
        ProviderOrphanError,
        ask_followup,
    )

    monkeypatch.setenv("HOME", str(tmp_path))
    short_id = "deadbef0"
    bogus_sock = str(tmp_path / "no-server.sock")
    _setup_session_file(tmp_path, pid=33, short_id=short_id, sock_path=bogus_sock)

    with pytest.raises(ProviderOrphanError) as exc_info:
        ask_followup(
            claude_short_id=short_id,
            message="m",
            cwd=Path("/tmp"),
            from_name="fno",
            timeout=1.0,
            poll_interval=0.05,
        )
    assert exc_info.value.reason == "liveness-failed"


# ---------------------------------------------------------------------------
# Task 1.1 - ask_followup_via_mcp decoupled from the (dead) messaging socket
# ---------------------------------------------------------------------------


def test_ask_followup_via_mcp_polls_jobs_dir_without_a_session_file(
    tmp_path: Path, monkeypatch
) -> None:
    """The MCP path derives the jobs-dir directly from the short-id and does NOT
    require a live session file / socket. An IDLE session (no
    ~/.claude/sessions/<pid>.json, so locate_session returns None) must still
    deliver + poll a reply rather than raising ProviderOrphanError - the send
    routes via the sidecar (not the socket) and the reply is read from the
    jobs-dir, neither of which needs the socket."""
    import fno.mcp as mcp_pkg
    from fno.mcp import client as mcp_client
    from fno.agents.providers.claude import ask_followup_via_mcp

    monkeypatch.setenv("HOME", str(tmp_path))
    short_id = "7c5dcf5d"
    # The jobs-dir exists (the session ran) but state.json is absent at baseline
    # capture, so the poll baseline is None. NO session file is written, so
    # locate_session() would return None here (socket-null / idle). The mocked
    # send simulates the recipient replying by writing the terminal state.
    jobs_dir = tmp_path / ".claude" / "jobs" / short_id
    jobs_dir.mkdir(parents=True)

    sent: dict[str, object] = {}

    def _fake_build(content, meta):  # noqa: ANN001
        return {"content": content, "meta": meta}

    def _fake_send(routing_key, envelope):  # noqa: ANN001
        sent["routing_key"] = routing_key
        sent["envelope"] = envelope
        # Recipient receives the message and replies (writes terminal state).
        _write_state(jobs_dir, state="completed", updated_at="T2", result="hi from B")

    monkeypatch.setattr(mcp_pkg, "build_channel_notification", _fake_build)
    monkeypatch.setattr(mcp_client, "send_to_channel", _fake_send)

    reply = ask_followup_via_mcp(
        claude_short_id=short_id,
        message="ping",
        cwd=tmp_path,
        from_name="fno",
        timeout=1.0,
        poll_interval=0.05,
    )

    assert reply == "hi from B"
    assert sent["routing_key"] == short_id  # routed by short-id, no socket lookup


def test_ask_followup_via_mcp_orphans_when_jobs_dir_absent(
    tmp_path: Path, monkeypatch
) -> None:
    """A genuinely-nonexistent session (no jobs-dir => never ran / typo) still
    raises ProviderOrphanError so a bad id fails fast instead of polling
    forever."""
    from fno.agents.providers.claude import ProviderOrphanError, ask_followup_via_mcp

    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / ".claude" / "jobs").mkdir(parents=True)  # no per-job dir

    with pytest.raises(ProviderOrphanError):
        ask_followup_via_mcp(
            claude_short_id="ffffffff",
            message="ping",
            cwd=tmp_path,
            from_name="fno",
            timeout=1.0,
            poll_interval=0.05,
        )


# ---------------------------------------------------------------------------
# x-2681: ask-lane control.sock fallback (socket-null + roster-live)
# ---------------------------------------------------------------------------

_FB_UUID = "abc12345-1111-2222-3333-444455556666"


def _write_socket_null_session(tmp_path: Path, short_id: str) -> None:
    sessions = tmp_path / ".claude" / "sessions"
    sessions.mkdir(parents=True, exist_ok=True)
    (sessions / "20.json").write_text(
        json.dumps({
            "messagingSocketPath": None,
            "jobId": short_id,
            "kind": "bg",
            "sessionId": _FB_UUID,
        }),
        encoding="utf-8",
    )


def _write_roster(tmp_path: Path, session_uuid: str) -> None:
    daemon = tmp_path / ".claude" / "daemon"
    daemon.mkdir(parents=True, exist_ok=True)
    (daemon / "roster.json").write_text(
        json.dumps({"workers": {"w": {"sessionId": session_uuid}}}),
        encoding="utf-8",
    )


def test_ask_followup_control_sock_fallback_delivers_and_returns_reply(
    tmp_path: Path, monkeypatch,
) -> None:
    """AC1-HP/AC2-HP: socket-null + roster-live -> control.sock inject, then the
    reply is collected from the bg jobs-dir."""
    import fno.agents.dispatch as dispatch_mod
    from fno.agents.providers.claude import ask_followup

    monkeypatch.setenv("HOME", str(tmp_path))
    short_id = "abc12345"
    _write_socket_null_session(tmp_path, short_id)
    _write_roster(tmp_path, _FB_UUID)
    jobs_dir = tmp_path / ".claude" / "jobs" / short_id
    _write_state(jobs_dir, state="running", updated_at="T1", result=None)

    captured: dict[str, Any] = {}

    def _fake_inject(recipient: str, text: str) -> bool:
        captured["recipient"] = recipient
        captured["text"] = text
        # The recipient processes the injected turn and replies.
        _write_state(jobs_dir, state="completed", updated_at="T2",
                     result="fallback reply")
        return True

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", _fake_inject)

    reply = ask_followup(
        claude_short_id=short_id, message="ping?", cwd=Path("/tmp"),
        from_name="peer", timeout=2.0, poll_interval=0.05,
    )
    assert reply == "fallback reply"
    assert captured["recipient"] == short_id
    # The question is wrapped as a peer turn, not raw operator input.
    assert "<cross-session-message" in captured["text"]
    assert "ping?" in captured["text"]


def test_ask_followup_control_sock_fallback_not_delivered_raises_distinct_reason(
    tmp_path: Path, monkeypatch,
) -> None:
    """AC6-FR: roster-live but the inject did not confirm -> a distinct reason
    (roster-live-inject-failed), NOT socket-null, so dispatch never orphan-stamps
    a live session."""
    import fno.agents.dispatch as dispatch_mod
    from fno.agents.providers.claude import ProviderOrphanError, ask_followup

    monkeypatch.setenv("HOME", str(tmp_path))
    short_id = "abc12345"
    _write_socket_null_session(tmp_path, short_id)
    _write_roster(tmp_path, _FB_UUID)

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", lambda r, t: False)

    with pytest.raises(ProviderOrphanError) as exc_info:
        ask_followup(
            claude_short_id=short_id, message="m", cwd=Path("/tmp"),
            from_name="peer", timeout=1.0, poll_interval=0.05,
        )
    assert exc_info.value.reason == "roster-live-inject-failed"


def test_ask_followup_socket_null_not_in_roster_stays_orphan(
    tmp_path: Path, monkeypatch,
) -> None:
    """AC3-ERR: socket-null AND absent from the roster -> today's socket-null
    orphan, and NO control.sock inject is attempted."""
    import fno.agents.dispatch as dispatch_mod
    from fno.agents.providers.claude import ProviderOrphanError, ask_followup

    monkeypatch.setenv("HOME", str(tmp_path))
    short_id = "abc12345"
    _write_socket_null_session(tmp_path, short_id)  # no roster.json written

    calls = {"n": 0}

    def _counting_inject(recipient: str, text: str) -> bool:
        calls["n"] += 1
        return True

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", _counting_inject)

    with pytest.raises(ProviderOrphanError) as exc_info:
        ask_followup(
            claude_short_id=short_id, message="m", cwd=Path("/tmp"),
            from_name="peer", timeout=1.0, poll_interval=0.05,
        )
    assert exc_info.value.reason == "socket-null"
    assert calls["n"] == 0, "dead-in-roster session must not attempt control.sock"


def test_ask_followup_not_found_never_falls_back_even_if_rostered(
    tmp_path: Path, monkeypatch,
) -> None:
    """Locked Decision 5: not-found is genuinely dead and never falls back,
    even if a same-short session happens to sit in the roster."""
    import fno.agents.dispatch as dispatch_mod
    from fno.agents.providers.claude import ProviderOrphanError, ask_followup

    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / ".claude" / "sessions").mkdir(parents=True, exist_ok=True)  # no match
    _write_roster(tmp_path, _FB_UUID)

    calls = {"n": 0}
    monkeypatch.setattr(
        dispatch_mod, "_mail_inject_claude",
        lambda r, t: calls.__setitem__("n", calls["n"] + 1) or True,
    )

    with pytest.raises(ProviderOrphanError) as exc_info:
        ask_followup(
            claude_short_id="abc12345", message="m", cwd=Path("/tmp"),
            from_name="peer", timeout=1.0, poll_interval=0.05,
        )
    assert exc_info.value.reason == "not-found"
    assert calls["n"] == 0
