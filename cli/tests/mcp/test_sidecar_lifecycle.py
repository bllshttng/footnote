"""End-to-end sidecar lifecycle: spawn real subprocess + drive via client.

Wave 1.1 + AC3-MIDBIND integration coverage. Spawning the real sidecar
process catches the AC3-MIDBIND stale-socket bug that unit tests missed
(the inner ``_prepare_socket_path`` retry only matters when a real
``bind()`` is being attempted, which only the real subprocess does).

Skipped on CI without permissions; gated only on local-fs Unix-socket
support (every supported fno platform).
"""
from __future__ import annotations

import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest


@pytest.fixture
def short_home(tmp_path: Path):
    """Yield a SHORT-path HOME directory.

    macOS AF_UNIX has a 104-character path ceiling and pytest's default
    ``tmp_path`` is well past it (~150 chars). Allocate the test home
    under ``/tmp`` so the sidecar socket fits.
    """
    home = Path(tempfile.mkdtemp(prefix="abi-mcp-", dir="/tmp"))
    try:
        yield home
    finally:
        shutil.rmtree(home, ignore_errors=True)


def _spawn_sidecar(home_dir: Path) -> tuple[subprocess.Popen, Path]:
    """Spawn the sidecar subprocess against a tmp socket path.

    Overrides HOME so fno.paths.state_dir() resolves under
    tmp_path. Returns the Popen + the resolved socket path.

    macOS-friendly: ``tmp_path.resolve()`` collapses the
    ``/var -> /private/var`` symlink so the test's expected path
    matches the realpath the sidecar logs.
    """
    home = home_dir.resolve()
    env = dict(os.environ)
    env["HOME"] = str(home)
    env.pop("XDG_RUNTIME_DIR", None)
    # Make sure paths.state_dir() points under tmp by removing any
    # project-local settings override.
    env.pop("FNO_CONFIG_DIR", None)
    # cwd MUST be under home so project-local .fno/settings.yaml
    # from the fno checkout doesn't get auto-discovered and pin
    # the state_dir to the checkout's path.
    # Redirect stderr/stdout to files (rather than PIPEs) so the
    # subprocess can't block on a full pipe buffer and we can read
    # the logs out-of-band when a test asserts.
    stderr_log = home / "sidecar-stderr.log"
    stdout_log = home / "sidecar-stdout.log"
    home.mkdir(parents=True, exist_ok=True)
    stderr_log.touch()
    stdout_log.touch()
    stderr_fp = open(stderr_log, "w", encoding="utf-8")
    stdout_fp = open(stdout_log, "w", encoding="utf-8")
    proc = subprocess.Popen(
        [sys.executable, "-m", "fno.mcp.sidecar"],
        stdin=subprocess.DEVNULL,
        stdout=stdout_fp,
        stderr=stderr_fp,
        env=env,
        cwd=str(home),
        start_new_session=True,
    )
    # Close the parent's copies; the child has dup'd fds.
    stderr_fp.close()
    stdout_fp.close()
    # Attach the log paths to the proc so test failures can include them.
    proc._stderr_log = stderr_log  # type: ignore[attr-defined]
    proc._stdout_log = stdout_log  # type: ignore[attr-defined]
    sock_path = home / ".fno" / "sidecar.sock"
    return proc, sock_path


def _read_stderr(proc: subprocess.Popen) -> str:
    """Read the sidecar's stderr log (file-backed)."""
    log_path = getattr(proc, "_stderr_log", None)
    if log_path is None:
        return "(no stderr capture configured)"
    try:
        return log_path.read_text("utf-8", errors="replace")[:4096]
    except OSError as exc:
        return f"(could not read {log_path}: {exc})"


def _wait_for_socket(sock_path: Path, *, timeout: float = 5.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if sock_path.exists():
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(0.3)
            try:
                s.connect(str(sock_path))
                s.sendall(b'{"op": "ping"}\n')
                data = s.recv(4096)
                s.close()
                if data:
                    return True
            except (ConnectionRefusedError, socket.timeout, OSError):
                pass
        time.sleep(0.05)
    return False


def _rpc(sock_path: Path, payload: dict) -> dict:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(2.0)
    s.connect(str(sock_path))
    s.sendall((json.dumps(payload) + "\n").encode("utf-8"))
    buf = bytearray()
    while True:
        chunk = s.recv(4096)
        if not chunk:
            break
        buf.extend(chunk)
        if b"\n" in buf:
            break
    s.close()
    line, _, _ = buf.partition(b"\n")
    return json.loads(line.decode("utf-8"))


class TestSidecarLifecycle:
    def test_lazy_start_and_ping(self, short_home: Path) -> None:
        proc, sock_path = _spawn_sidecar(short_home)
        try:
            assert _wait_for_socket(sock_path, timeout=5.0), (
                f"sidecar did not bind within 5s.\n"
                f"sock_path={sock_path}\n"
                f"alive={proc.poll() is None}\n"
                f"stderr:\n{_read_stderr(proc)}"
            )
            resp = _rpc(sock_path, {"op": "ping"})
            assert resp["ok"] is True
            assert isinstance(resp["pid"], int)
            assert resp["pid"] == proc.pid
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    def test_status_with_zero_channels(self, short_home: Path) -> None:
        proc, sock_path = _spawn_sidecar(short_home)
        try:
            assert _wait_for_socket(sock_path, timeout=5.0)
            resp = _rpc(sock_path, {"op": "status"})
            assert resp["ok"] is True
            assert resp["channels_registered"] == 0
            assert resp["channels"] == []
        finally:
            proc.terminate()
            proc.wait(timeout=5)

    def test_ac3_midbind_stale_socket_recovery(self, short_home: Path) -> None:
        """AC3-MIDBIND: a stale socket file (no process listening) is
        unlinked + rebound by a fresh sidecar without retry storm."""
        home = short_home.resolve()
        sock_path = home / ".fno" / "sidecar.sock"
        sock_path.parent.mkdir(parents=True, exist_ok=True)
        # Create a stale socket file (no listener behind it).
        sock_path.touch()
        assert sock_path.exists()

        proc, _ = _spawn_sidecar(short_home)
        try:
            # Despite the stale file, the sidecar should bind cleanly
            # within 5s. This was the failing case before the fix:
            # the retry loop was theater + start_unix_server had no
            # stale-recovery wrapper.
            assert _wait_for_socket(sock_path, timeout=5.0), (
                "sidecar failed to recover from a stale socket file"
            )
            resp = _rpc(sock_path, {"op": "ping"})
            assert resp["ok"] is True
            assert resp["pid"] == proc.pid
        finally:
            proc.terminate()
            proc.wait(timeout=5)

    def test_second_sidecar_detects_live_first_and_exits(
        self, short_home: Path
    ) -> None:
        """Spec: only one sidecar per user. The second invocation must
        detect the live first and exit 0 with a stderr note."""
        proc1, sock_path = _spawn_sidecar(short_home)
        try:
            assert _wait_for_socket(sock_path, timeout=5.0)
            # Spawn the second sidecar with the same HOME.
            env = dict(os.environ)
            env["HOME"] = str(short_home.resolve())
            env.pop("XDG_RUNTIME_DIR", None)
            proc2 = subprocess.run(
                [sys.executable, "-m", "fno.mcp.sidecar"],
                env=env,
                cwd=str(short_home.resolve()),
                capture_output=True,
                text=True,
                timeout=5,
            )
            assert proc2.returncode == 0
            assert "already running" in proc2.stderr
        finally:
            proc1.terminate()
            proc1.wait(timeout=5)

    def test_sigterm_flushes_state_to_disk(self, short_home: Path) -> None:
        """AC4-UI prerequisite: SIGTERM persists channel-registration
        table to ~/.fno/sidecar-state.json before exit."""
        proc, sock_path = _spawn_sidecar(short_home)
        home = short_home.resolve()
        try:
            assert _wait_for_socket(sock_path, timeout=5.0)
            # Send SIGTERM; the signal handler flushes state.
            proc.terminate()
            proc.wait(timeout=5)
            state_path = home / ".fno" / "sidecar-state.json"
            assert state_path.exists(), (
                f"sidecar-state.json missing after SIGTERM (stderr: "
                f"{_read_stderr(proc)})"
            )
            payload = json.loads(state_path.read_text("utf-8"))
            assert payload["pid"] == proc.pid
            assert payload["channels"] == []
        finally:
            if proc.poll() is None:
                proc.kill()


class TestSidecarBadRequest:
    def test_unknown_op_returns_bad_request(self, short_home: Path) -> None:
        proc, sock_path = _spawn_sidecar(short_home)
        try:
            assert _wait_for_socket(sock_path, timeout=5.0)
            resp = _rpc(sock_path, {"op": "nonsense"})
            assert resp["ok"] is False
            assert resp["reason"] == "bad_request"
        finally:
            proc.terminate()
            proc.wait(timeout=5)

    def test_send_to_unregistered_channel(self, short_home: Path) -> None:
        proc, sock_path = _spawn_sidecar(short_home)
        try:
            assert _wait_for_socket(sock_path, timeout=5.0)
            resp = _rpc(
                sock_path,
                {
                    "op": "send_to_channel",
                    "session_id": "ghost-id",
                    "envelope": {
                        "jsonrpc": "2.0",
                        "method": "notifications/claude/channel",
                        "params": {"content": "hi", "meta": {}},
                    },
                },
            )
            assert resp["ok"] is False
            assert resp["reason"] == "channel_not_registered"
        finally:
            proc.terminate()
            proc.wait(timeout=5)


def _read_one_frame(sock: socket.socket, *, timeout: float = 2.0) -> dict:
    """Read one newline-delimited JSON frame off a live push connection.

    Bounded by ``timeout`` (AC1-FR): a non-delivery raises a clear
    ``AssertionError`` naming the missing round-trip rather than hanging
    the suite forever — the exact silent-failure mode this test guards.
    """
    sock.settimeout(timeout)
    buf = bytearray()
    try:
        while b"\n" not in buf:
            chunk = sock.recv(4096)
            if not chunk:
                break
            buf.extend(chunk)
    except socket.timeout:
        raise AssertionError(
            f"no deliver frame arrived within {timeout}s: the "
            f"register/send_to_channel round-trip did not deliver"
        )
    line, _, _ = buf.partition(b"\n")
    assert line, "push connection closed without a deliver frame"
    return json.loads(line.decode("utf-8"))


class TestSidecarChannelDelivery:
    """The register -> send_to_channel -> deliver HIT case.

    The existing suite only covered the miss case
    (``test_send_to_unregistered_channel``). The delivery path has real
    production callers (``agents/dispatch.py``,
    ``agents/providers/claude.py``) yet no test asserted a payload
    actually crosses the socket — the coverage gap that let a
    deliver-nothing regression ship silently.
    """

    def _register(self, sock_path: Path, session_id: str) -> socket.socket:
        """Open a persistent connection and register a push channel on it.

        Returns the still-open socket (push mode); the caller keeps it
        open across the peer's ``send_to_channel`` and tears it down.
        """
        conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        conn.settimeout(2.0)
        conn.connect(str(sock_path))
        conn.sendall(
            (
                json.dumps(
                    {
                        "op": "register_channel",
                        "session_id": session_id,
                        "channel_name": "test-channel",
                        "pid": os.getpid(),
                    }
                )
                + "\n"
            ).encode("utf-8")
        )
        ack = _read_one_frame(conn)
        assert ack == {"ok": True}, f"register_channel failed: {ack}"
        return conn

    def test_send_to_registered_channel_delivers(self, short_home: Path) -> None:
        """AC1-HP: a registered channel receives the pushed envelope,
        byte-equal including meta keys."""
        from fno.mcp.channel import build_channel_notification

        proc, sock_path = _spawn_sidecar(short_home)
        conn_a = None
        try:
            assert _wait_for_socket(sock_path, timeout=5.0)
            # Connection A registers and STAYS OPEN (push mode). Closing
            # it first would silently drop the delivery route.
            conn_a = self._register(sock_path, "sess-hit")

            envelope = build_channel_notification(content="hi", meta={"k": "v"})
            # Connection B (fresh, single-shot) sends to the same session.
            resp = _rpc(
                sock_path,
                {"op": "send_to_channel", "session_id": "sess-hit", "envelope": envelope},
            )
            assert resp == {"ok": True}, f"send_to_channel failed: {resp}"

            frame = _read_one_frame(conn_a)
            assert frame == {"op": "deliver", "envelope": envelope}
            # Value equality on meta, not just shape (channel.py's smoke
            # pins shape and ignores content/meta values).
            assert frame["envelope"]["params"]["meta"] == {"k": "v"}
        finally:
            if conn_a is not None:
                conn_a.close()
            proc.terminate()
            proc.wait(timeout=5)

    def test_empty_meta_envelope_round_trips(self, short_home: Path) -> None:
        """AC1-EDGE: an envelope built with meta=None (collapsing to {})
        round-trips with meta preserved as an empty dict."""
        from fno.mcp.channel import build_channel_notification

        proc, sock_path = _spawn_sidecar(short_home)
        conn_a = None
        try:
            assert _wait_for_socket(sock_path, timeout=5.0)
            conn_a = self._register(sock_path, "sess-edge")

            envelope = build_channel_notification(content="hi", meta=None)
            resp = _rpc(
                sock_path,
                {"op": "send_to_channel", "session_id": "sess-edge", "envelope": envelope},
            )
            assert resp == {"ok": True}

            frame = _read_one_frame(conn_a)
            assert frame["op"] == "deliver"
            assert frame["envelope"]["params"]["meta"] == {}
        finally:
            if conn_a is not None:
                conn_a.close()
            proc.terminate()
            proc.wait(timeout=5)
