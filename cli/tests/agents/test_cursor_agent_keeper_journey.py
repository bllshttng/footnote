"""The cursor-agent keeper-lane journey (journey wk-cursor, lane B).

pi's journey (test_thread_keeper_journey.py) proved the LANE: a keeper-hosted
child survives the SIGKILL of both supervisors with pid, cwd and session id
unchanged, and the restart sweep re-binds the same row. That machinery is
harness-agnostic and is NOT re-proven here.

This journey proves what is cursor-agent-SPECIFIC, on the real binary:

1. CALLEE-MINTED ID: the row's `harness_session_id` is a chat id
   `create-chat` returned - never fno-minted, present before launch.
2. REMOTE STORE: the id appears in NO file under the cursor state root
   (~/.cursor is the real, authenticated state root; the positive control
   asserts the root itself is readable, so the absence is a measurement and
   not a pipeline loss).
3. PAINTER CONFIRM: mail lands through the keeper pty and is confirmed by
   the TUI painting the payload - the Rust lane's `Pty` confirm source, the
   only local evidence a remote-store harness can offer.
4. SURVIVE: a second process's turn recalls turn one's codeword, which is
   the cross-process recall the row's `callee-minted-read-back` binding
   claims.

Requires the worktree's Rust binaries (cargo build -p fno-agents) and a
logged-in cursor-agent; it SKIPS honestly otherwise and spends two real
model turns when it runs.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest

from fno.paths import paths

_REPO = Path(__file__).resolve().parents[3]
_CLI_SRC = _REPO / "cli" / "src"


def _crate_bin(crate: str, name: str) -> Path | None:
    target = _REPO / "crates" / crate / "target" / "debug" / name
    return target if target.is_file() else None


_worker_bin = _crate_bin("fno-agents", "fno-agents-worker")
_daemon_bin = _crate_bin("fno-agents", "fno-agents-daemon")
_client_bin = _crate_bin("fno-agents", "fno-agents")

if _worker_bin is None:
    pytest.skip("no fno-agents-worker binary; cargo build -p fno-agents first", allow_module_level=True)
if _daemon_bin is None:
    pytest.skip("no fno-agents-daemon binary; cargo build -p fno-agents first", allow_module_level=True)
if _client_bin is None:
    pytest.skip("no fno-agents client binary; cargo build -p fno-agents first", allow_module_level=True)

CURSOR_AGENT = os.environ.get("FNO_CURSOR_AGENT_BIN") or "cursor-agent"


def _cursor_authenticated() -> bool:
    """Assert the JSON field, never the exit code: `status` exits 0 while
    unauthenticated, so an exit-code check here is an absence with three
    explanations."""
    try:
        proc = subprocess.run(  # noqa: S603
            [CURSOR_AGENT, "status", "--format", "json"],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    try:
        return bool(json.loads(proc.stdout).get("isAuthenticated"))
    except ValueError:
        return False


if not _cursor_authenticated():
    pytest.skip("cursor-agent is not authenticated; the recall turns need it", allow_module_level=True)

CODEWORD = "FNO-CURSOR-7Q4X"
REPLY_ONE = "JRN-OK-601E"
PROMPT_ONE = f"Remember the codeword {CODEWORD}. Reply with exactly {REPLY_ONE} and nothing else."
PROMPT_TWO = (
    "What codeword were you asked to remember? Reply with exactly that word "
    "and nothing else."
)

_JOURNEY_NAME = "wk-cursor"
_SPAWN_SNIPPET = (
    "import json, sys\n"
    f"sys.path.insert(0, {str(_CLI_SRC)!r})\n"
    "from pathlib import Path\n"
    "from fno.agents.dispatch import _lane_b_thread_spawn\n"
    "receipt = _lane_b_thread_spawn(\n"
    f"    name={_JOURNEY_NAME!r}, harness='cursor-agent', cwd=Path({{cwd!r}})\n"
    ")\n"
    'print("RECEIVED-RECEIPT:" + json.dumps(receipt))\n'
)


def _ps_field(field: str, pid: int) -> str:
    out = subprocess.run(  # noqa: S603
        ["ps", "-o", f"{field}=", "-p", str(pid)],
        capture_output=True,
        text=True,
    )
    return out.stdout.strip()


def _alive(pid: int) -> bool:
    return bool(_ps_field("pid", pid))


def _child_cwd(pid: int) -> str | None:
    out = subprocess.run(  # noqa: S603
        ["lsof", "-a", "-p", str(pid), "-d", "cwd", "-Fn"],
        capture_output=True,
        text=True,
    )
    for line in out.stdout.splitlines():
        if line.startswith("n/"):
            return line[1:]
    return None


class _PtyTap:
    """Accumulate the keeper's Output frames from one connection.

    There is no screen to scrape on a pane-less keeper and no transcript on
    disk (the chat store is remote), so the pty stream is the only local
    evidence a turn produced output. The frame tags and lengths interleaved
    in the raw bytes are harmless to a substring read.
    """

    def __init__(self, sock: Path) -> None:
        import threading

        self._conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._conn.settimeout(2.0)
        self._conn.connect(str(sock))
        self.buf = bytearray()
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._drain, daemon=True)
        self._thread.start()

    def _drain(self) -> None:
        while not self._stop.is_set():
            try:
                chunk = self._conn.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                break
            if not chunk:
                break
            self.buf.extend(chunk)

    def wait_for_fresh(self, needle: str, start: int, timeout_s: float) -> bool:
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if needle.encode() in bytes(self.buf[start:]):
                return True
            time.sleep(0.5)
        return False

    def close(self) -> None:
        self._stop.set()
        self._thread.join(timeout=5)
        self._conn.close()


def _wait_cursor_ready(sock: Path, timeout_s: float = 90.0) -> int:
    """Wait for the idle composer marker the manifest pins; return the byte
    offset the marker appeared at, so later turns read only fresher bytes."""
    deadline = time.monotonic() + timeout_s
    buf = b""
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(2.0)
    try:
        conn.connect(str(sock))
        while time.monotonic() < deadline:
            try:
                chunk = conn.recv(4096)
            except socket.timeout:
                continue
            if not chunk:
                continue
            buf += chunk
            at = buf.find(b"Add a follow-up")
            if at >= 0:
                time.sleep(0.5)
                return len(buf)
    finally:
        conn.close()
    pytest.fail("cursor-agent's idle composer never painted: the TUI never became ready")


def _send_mail(text: str, session_id: str, monkeypatch, capsys) -> None:
    """One prompt through the real mail lane: the same name-lane choke point
    `fno agents mail send` runs, with the injector binary resolved to the
    worktree build. A not-confirmed landing fails HERE, naming the verb's
    reason - the pty confirm source is what this journey exists to prove."""
    from fno.agents.discover import DiscoveredSession
    from fno.mail import cli as mail_cli

    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    import fno.rust_binary as rust_binary_mod

    monkeypatch.setattr(rust_binary_mod, "resolve_installed_binary", lambda: _client_bin)
    resolved = DiscoveredSession(
        session_id=session_id,
        short_id="",
        handle=_JOURNEY_NAME,
        pid=0,
        cwd="",
        project=None,
        status="live",
        agent="cursor-agent",
    )
    mail_cli._name_lane_send(text, from_name="web", resolved=resolved)
    capsys.readouterr()


def _id_in_state_root(chat_id: str) -> tuple[bool, int]:
    """(found, files scanned) for the chat id under the cursor state root."""
    root = Path.home() / ".cursor"
    scanned = 0
    if not root.is_dir():
        return False, scanned
    for path in root.rglob("*"):
        if path.is_file() and path.stat().st_size < 50_000_000:
            scanned += 1
            try:
                if chat_id in path.read_text(encoding="utf-8", errors="ignore"):
                    return True, scanned
            except OSError:
                continue
    return False, scanned


def test_cursor_agent_keeper_journey(tmp_path, monkeypatch, capsys) -> None:
    # The id string is 36 chars; a full-tree grep of the real state root is
    # bounded work and the only honest store read this harness allows.
    short_state = Path(tempfile.mkdtemp(prefix="fno5c-"))
    settings = tmp_path / ".fno" / "settings.yaml"
    settings.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: {short_state}/\n",
        encoding="utf-8",
    )
    agents_home = short_state / "agents"
    monkeypatch.setenv("FNO_AGENTS_HOME", str(agents_home))
    monkeypatch.setenv("FNO_MUX_DIR", str(short_state / "mux"))
    monkeypatch.setenv("FNO_AGENTS_WORKER_BIN", str(_worker_bin))
    monkeypatch.setenv("FNO_AGENTS_IDLE_EXIT_SECS", "1800")
    monkeypatch.setenv("PATH", f"{_client_bin.parent}{os.pathsep}{os.environ['PATH']}")
    monkeypatch.setenv("TERM", os.environ.get("TERM") or "xterm-256color")

    journey_cwd = tmp_path / "journey"
    journey_cwd.mkdir()
    env = dict(os.environ)
    env["PYTHONPATH"] = str(_CLI_SRC) + os.pathsep + env.get("PYTHONPATH", "")
    daemon_popen: subprocess.Popen | None = None
    mux_popen: subprocess.Popen | None = None
    keeper_pid: int | None = None
    try:
        # ---- Step 1: spawn from a short-lived caller; the receipt's id must
        # be a chat id create-chat returned, not fno's uuid4 mint.
        spawn_cwd = str(journey_cwd)
        from fno.agents.harnesses.cursor_agent import create_chat as _mint_oracle

        oracle_id = _mint_oracle(spawn_cwd)
        proc = subprocess.run(  # noqa: S603
            [sys.executable, "-c", _SPAWN_SNIPPET.format(cwd=spawn_cwd)],
            env=env,
            capture_output=True,
            text=True,
            timeout=180,
        )
        marker = "RECEIVED-RECEIPT:"
        assert marker in proc.stdout, f"spawn failed: {proc.stderr}\n{proc.stdout}"
        receipt = json.loads(proc.stdout[proc.stdout.index(marker) + len(marker) :])
        session_id = receipt["session_id"]
        keeper_pid = int(receipt["keeper_pid"])
        child_pid = int(receipt["child_pid"])
        keeper_sock = Path(receipt["keeper_socket"])
        # The row's id is CALLEE-minted: a UUIDv4, distinct from the oracle
        # mint (two runs, two ids) and never equal to any fno-minted value.
        assert session_id != oracle_id, "the row reused the oracle's id"
        assert len(session_id) == 36 and session_id.count("-") == 4

        from fno.agents.dispatch import _keeper_identify

        identify = _keeper_identify(keeper_sock)
        assert identify["session_id"] == session_id
        assert identify["child_pid"] == child_pid

        ready_at = _wait_cursor_ready(keeper_sock)

        # ---- Step 2: the remote store. Positive control first: the state
        # root is real and readable, so the absence below is a measurement.
        root = Path.home() / ".cursor"
        assert root.is_dir() and any(root.iterdir()), "cursor state root unreadable"
        found, _scanned = _id_in_state_root(session_id)
        assert not found, "the chat id appeared on disk; the store is not remote"

        # ---- Step 3: first turn through the real mail lane; the painter
        # confirm proves the payload landed in the TUI.
        tap = _PtyTap(keeper_sock)
        try:
            _send_mail(PROMPT_ONE, session_id, monkeypatch, capsys)
            assert tap.wait_for_fresh(REPLY_ONE, max(ready_at, 0), 240.0), (
                f"the TUI never painted the model's {REPLY_ONE!r}; the turn "
                "never landed or the confirm source is blind"
            )

            # ---- Step 4: kill -9 BOTH supervisors. The lane proof lives in
            # pi's journey; here it bounds the restart claims that follow.
            daemon_popen = subprocess.Popen(  # noqa: S603
                [str(_daemon_bin), "--home", str(agents_home)],
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            _wait_sock_ready(agents_home / "supervisor.sock")
            mux_session = "journey-cursor"
            mux_sock = short_state / "mux" / f"{mux_session}.sock"
            mux_popen = subprocess.Popen(  # noqa: S603
                [str(_client_bin), "mux", "server", "--session", mux_session],
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            _wait_ready(mux_sock)
            keeper_ppid_before = _ps_field("ppid", keeper_pid)
            child_ppid_before = _ps_field("ppid", child_pid)
            os.kill(daemon_popen.pid, signal.SIGKILL)
            os.kill(mux_popen.pid, signal.SIGKILL)
            daemon_popen.wait(timeout=30)
            mux_popen.wait(timeout=30)
            daemon_popen = None
            mux_popen = None

            assert _alive(child_pid), "the child did not survive the kill"
            assert _alive(keeper_pid), "the keeper did not survive the kill"
            keeper_ppid = _ps_field("ppid", keeper_pid)
            assert keeper_ppid == "1", f"the keeper is hosted, not orphaned: ppid={keeper_ppid!r}"
            assert keeper_ppid_before == "1"
            assert _ps_field("ppid", child_pid) == child_ppid_before == str(keeper_pid)
            child_cwd = _child_cwd(child_pid)
            assert child_cwd is not None and os.path.realpath(child_cwd) == os.path.realpath(
                journey_cwd
            ), "the child's cwd changed across the kill"
            identify = _keeper_identify(keeper_sock)
            assert identify["session_id"] == session_id
            assert identify["child_pid"] == child_pid

            # ---- Step 5: restart both; the sweep must re-bind the same row.
            daemon_popen = subprocess.Popen(  # noqa: S603
                [str(_daemon_bin), "--home", str(agents_home)],
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            _wait_ready(agents_home / "supervisor.sock")
            mux_popen = subprocess.Popen(  # noqa: S603
                [str(_client_bin), "mux", "server", "--session", mux_session],
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            _wait_ready(mux_sock)
            from fno.agents.registry import load_registry

            deadline = time.monotonic() + 30
            row = None
            while time.monotonic() < deadline:
                entries = [e for e in load_registry() if e.name == _JOURNEY_NAME]
                row = entries[0] if entries else None
                if row is not None and row.keeper_child_pid == child_pid:
                    break
                time.sleep(0.5)
            assert row is not None, "the keeper row vanished across the restart"
            assert row.status == "live"
            assert row.keeper_child_pid == child_pid
            assert row.harness_session_id == session_id
            assert row.messaging_socket_path == str(keeper_sock)

            # ---- Step 6: SURVIVE. A fresh turn on the SAME remote chat
            # recalls turn one's codeword - the cross-process recall the
            # callee-minted-read-back binding claims. The needle is scanned
            # only in bytes newer than the prompt, so the envelope's own
            # paint cannot satisfy it.
            baseline = len(tap.buf)
            _send_mail(PROMPT_TWO, session_id, monkeypatch, capsys)
            assert tap.wait_for_fresh(CODEWORD, baseline, 240.0), (
                "a fresh process turn did not recall the codeword; the "
                "session binding is not proven"
            )
        finally:
            tap.close()
    finally:
        for popen in (daemon_popen, mux_popen):
            if popen is not None:
                popen.kill()
        # Tear the hosted thread down child-first: the keeper exits when its
        # child does; anything left is killed so the operator's machine keeps
        # no stray cursor-agent TUI from a test run.
        try:
            if _alive(child_pid):
                os.kill(child_pid, signal.SIGKILL)
            time.sleep(1.0)
            if _alive(keeper_pid or 0):
                os.kill(keeper_pid, signal.SIGKILL)
        except (NameError, OSError, ProcessLookupError):
            pass


def _wait_sock_ready(sock: Path, timeout_s: float = 60.0) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if sock.exists():
            probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                probe.connect(str(sock))
                probe.close()
                return
            except OSError:
                pass
        time.sleep(0.5)
    pytest.fail(f"{sock} never became connectable")
