#!/usr/bin/env python3
"""Live keeper-lane journey for cursor-agent (journey wk-cursor).

pi's pytest journey (cli/tests/agents/test_thread_keeper_journey.py) proved
the LANE itself: a keeper-hosted child survives the SIGKILL of both
supervisors with pid, cwd and session id unchanged, and the restart sweep
re-binds the same row. That machinery is harness-agnostic and is not
re-proven here beyond the bounds the cursor step needs.

This script proves what is cursor-agent-SPECIFIC, on the real binary:

1. CALLEE-MINTED ID - the row's harness_session_id is a chat id
   `create-chat` returned, present before the child launched.
2. REMOTE STORE - the id appears in NO file under ~/.cursor (positive
   control: the state root itself is real and readable).
3. PAINTER CONFIRM - mail lands through the keeper pty and is confirmed by
   the TUI painting the payload: the Rust lane's Pty confirm source, the
   only local evidence a remote-store harness can offer.
4. SURVIVE - a fresh turn on the same remote chat recalls turn one's
   codeword: the cross-process recall the callee-minted-read-back binding
   claims.

Run it from a worktree that has built the Rust binaries:

    cargo build -p fno-agents
    uv run --project cli python cli/scripts/smoke/cursor-agent-keeper-journey.py

It runs OUTSIDE pytest on purpose: the test suite sandboxes $HOME, and
cursor-agent's credential lives in the real ~/.cursor/cli-config.json. The
script needs a logged-in cursor-agent and spends two real model turns.
Every step prints an SK-CURSOR marker; any failure exits nonzero after
naming the step.
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
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
CLI_SRC = REPO / "cli" / "src"

CODEWORD = "FNO-CURSOR-7Q4X"
REPLY_ONE = "JRN-OK-601E"
PROMPT_ONE = f"Remember the codeword {CODEWORD}. Reply with exactly {REPLY_ONE} and nothing else."
PROMPT_TWO = (
    "What codeword were you asked to remember? Reply with exactly that word "
    "and nothing else."
)
JOURNEY_NAME = "wk-cursor"
READY_MARKER = b"Add a follow-up"  # manifests/cursor-agent.toml idle_follow_up


def fail(step: str, detail: str) -> None:
    print(f"SK-CURSOR-FAIL step={step} detail={detail}")
    sys.exit(1)


def ok(step: str, detail: str = "") -> None:
    print(f"SK-CURSOR-OK step={step}" + (f" detail={detail}" if detail else ""), flush=True)


def crate_bin(crate: str, name: str) -> Path:
    target = REPO / "crates" / crate / "target" / "debug" / name
    if not target.is_file():
        fail("binaries", f"{target} missing; cargo build -p fno-agents first")
    return target


def cursor_authenticated() -> bool:
    # Assert the JSON field, never the exit code: `status` exits 0 while
    # unauthenticated, so an exit-code check here is an absence with three
    # explanations.
    proc = subprocess.run(
        ["cursor-agent", "status", "--format", "json"],
        capture_output=True, text=True, timeout=30,
    )
    try:
        return bool(json.loads(proc.stdout).get("isAuthenticated"))
    except ValueError:
        return False


class PtyTap:
    """Accumulate the keeper's Output frames from one connection.

    There is no screen to scrape on a pane-less keeper and no transcript on
    disk (the chat store is remote), so the pty stream is the only local
    evidence a turn produced output.
    """

    def __init__(self, sock: Path) -> None:
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


def wait_ready(sock: Path, timeout_s: float = 90.0) -> int:
    """Wait for the idle composer marker; return the byte offset it appeared
    at so later turns read only fresher bytes."""
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
            if READY_MARKER in buf:
                time.sleep(0.5)
                return len(buf)
    finally:
        conn.close()
    fail("ready", "cursor-agent's idle composer never painted; the TUI never became ready")
    return 0  # unreachable; fail() exits


def wait_sock_ready(sock: Path, timeout_s: float = 60.0) -> None:
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
    fail("supervisors", f"{sock} never became connectable")


def ps_field(field: str, pid: int) -> str:
    out = subprocess.run(["ps", "-o", f"{field}=", "-p", str(pid)],
                         capture_output=True, text=True)
    return out.stdout.strip()


def alive(pid: int) -> bool:
    return bool(ps_field("pid", pid))


def child_cwd(pid: int) -> str | None:
    out = subprocess.run(["lsof", "-a", "-p", str(pid), "-d", "cwd", "-Fn"],
                         capture_output=True, text=True)
    for line in out.stdout.splitlines():
        if line.startswith("n/"):
            return line[1:]
    return None


def id_in_state_root(chat_id: str) -> tuple[bool, int]:
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


def main() -> None:
    worker_bin = crate_bin("fno-agents", "fno-agents-worker")
    daemon_bin = crate_bin("fno-agents", "fno-agents-daemon")
    client_bin = crate_bin("fno-agents", "fno-agents")

    if not cursor_authenticated():
        fail("auth", "cursor-agent is not authenticated; run 'cursor-agent login' first")
    ok("auth")

    # The spawn runs from a short-lived caller so the keeper orphans to
    # launchd; both state roots move to a SHORT tempdir (AF_UNIX's 104-byte
    # sun_path cannot hold long temp paths).
    short_state = Path(tempfile.mkdtemp(prefix="fno5c-"))
    journey_cwd = Path(tempfile.mkdtemp(prefix="fno5c-journey-"))
    agents_home = short_state / "agents"
    env = dict(os.environ)
    env["FNO_AGENTS_HOME"] = str(agents_home)
    env["FNO_MUX_DIR"] = str(short_state / "mux")
    env["FNO_AGENTS_WORKER_BIN"] = str(worker_bin)
    env["FNO_AGENTS_IDLE_EXIT_SECS"] = "1800"
    env["PATH"] = f"{client_bin.parent}{os.pathsep}{env.get('PATH', '')}"
    env["PYTHONPATH"] = str(CLI_SRC) + os.pathsep + env.get("PYTHONPATH", "")
    env["TERM"] = env.get("TERM") or "xterm-256color"

    # The id mint oracle runs BEFORE the spawn: two runs, two distinct ids.
    from fno.agents.harnesses.cursor_agent import create_chat as mint_oracle

    oracle_id = mint_oracle(journey_cwd)

    spawn_snippet = (
        "import json, sys\n"
        f"sys.path.insert(0, {str(CLI_SRC)!r})\n"
        "from pathlib import Path\n"
        "from fno.agents.dispatch import _lane_b_thread_spawn\n"
        "receipt = _lane_b_thread_spawn(\n"
        f"    name={JOURNEY_NAME!r}, harness='cursor-agent', cwd=Path({str(journey_cwd)!r})\n"
        ")\n"
        'print("RECEIVED-RECEIPT:" + json.dumps(receipt))\n'
    )
    proc = subprocess.run(
        [sys.executable, "-c", spawn_snippet],
        env=env, capture_output=True, text=True, timeout=180,
    )
    marker = "RECEIVED-RECEIPT:"
    if marker not in proc.stdout:
        fail("spawn", repr(proc.stderr[-400:]))
    receipt = json.loads(proc.stdout[proc.stdout.index(marker) + len(marker):])
    session_id = receipt["session_id"]
    keeper_pid = int(receipt["keeper_pid"])
    child_pid = int(receipt["child_pid"])
    keeper_sock = Path(receipt["keeper_socket"])

    if session_id == oracle_id:
        fail("mint", "the row reused the oracle's id; the mint is not per-spawn")
    if not (len(session_id) == 36 and session_id.count("-") == 4):
        fail("mint", f"row id {session_id!r} is not a full UUID")
    ok("spawn", f"session={session_id} keeper={keeper_pid} child={child_pid}")

    from fno.agents.dispatch import _keeper_identify

    identify = _keeper_identify(keeper_sock)
    if identify["session_id"] != session_id or identify["child_pid"] != child_pid:
        fail("identify", repr(identify))
    ok("identify")

    ready_at = wait_ready(keeper_sock)
    ok("ready", f"painted_at_byte={ready_at}")

    # Remote store: positive control first, then the bounded absence.
    root = Path.home() / ".cursor"
    if not (root.is_dir() and any(root.iterdir())):
        fail("store-control", f"{root} is not a readable state root")
    found, scanned = id_in_state_root(session_id)
    if found:
        fail("store", f"the chat id appeared on disk in {scanned} files; the store is not remote")
    ok("store", f"absent_across={scanned}_files")

    daemon: subprocess.Popen | None = None
    mux: subprocess.Popen | None = None
    tap: PtyTap | None = None
    try:
        from fno.mail import cli as mail_cli
        from fno.agents.discover import DiscoveredSession
        import fno.rust_binary as rust_binary_mod

        rust_binary_mod.resolve_installed_binary = lambda: client_bin  # type: ignore[method-assign]
        resolved = DiscoveredSession(
            session_id=session_id, short_id="", handle=JOURNEY_NAME, pid=0,
            cwd="", project=None, status="live", agent="cursor-agent",
        )

        def send_mail(text: str) -> None:
            mail_cli._name_lane_send(text, from_name="web", resolved=resolved)

        tap = PtyTap(keeper_sock)
        send_mail(PROMPT_ONE)
        if not tap.wait_for_fresh(REPLY_ONE, ready_at, 240.0):
            fail("turn-one", f"the TUI never painted {REPLY_ONE!r}")
        ok("turn-one", f"painted={REPLY_ONE}")

        # Kill -9 BOTH supervisors (not SIGTERM: a graceful exit could spare
        # the child by a route that says nothing about the hangup).
        daemon = subprocess.Popen(
            [str(daemon_bin), "--home", str(agents_home)],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_sock_ready(agents_home / "supervisor.sock")
        mux_session = "journey-cursor"
        mux_sock = short_state / "mux" / f"{mux_session}.sock"
        mux = subprocess.Popen(
            [str(client_bin), "mux", "server", "--session", mux_session],
            env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_sock_ready(mux_sock)
        keeper_ppid_before = ps_field("ppid", keeper_pid)
        child_ppid_before = ps_field("ppid", child_pid)
        os.kill(daemon.pid, signal.SIGKILL)
        os.kill(mux.pid, signal.SIGKILL)
        daemon.wait(timeout=30)
        mux.wait(timeout=30)
        daemon = mux = None

        if not alive(child_pid):
            fail("survive-child", f"child pid {child_pid} died with the supervisors")
        if not alive(keeper_pid):
            fail("survive-keeper", f"keeper pid {keeper_pid} died with the supervisors")
        keeper_ppid = ps_field("ppid", keeper_pid)
        if keeper_ppid != "1":
            fail("orphan", f"the keeper is hosted, not orphaned: ppid={keeper_ppid!r}")
        if keeper_ppid_before != "1":
            fail("orphan-before", f"the keeper was already hosted at spawn: ppid={keeper_ppid_before!r}")
        if ps_field("ppid", child_pid) != child_ppid_before or child_ppid_before != str(keeper_pid):
            fail("child-parent", "the child's parent changed across the kill")
        cwd_now = child_cwd(child_pid)
        if cwd_now is None or os.path.realpath(cwd_now) != os.path.realpath(journey_cwd):
            fail("child-cwd", f"the child's cwd changed: {cwd_now!r}")
        identify = _keeper_identify(keeper_sock)
        if identify["session_id"] != session_id or identify["child_pid"] != child_pid:
            fail("identify-after-kill", repr(identify))
        ok("survive", "child, cwd, parent and identity unchanged across SIGKILL x2")

        # Restart both; the daemon start runs the registry-side keeper sweep.
        daemon = subprocess.Popen(
            [str(daemon_bin), "--home", str(agents_home)],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_sock_ready(agents_home / "supervisor.sock")
        mux = subprocess.Popen(
            [str(client_bin), "mux", "server", "--session", mux_session],
            env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_sock_ready(mux_sock)

        from fno.agents.registry import load_registry

        deadline = time.monotonic() + 30
        row = None
        while time.monotonic() < deadline:
            entries = [e for e in load_registry() if e.name == JOURNEY_NAME]
            row = entries[0] if entries else None
            if row is not None and row.keeper_child_pid == child_pid:
                break
            time.sleep(0.5)
        if row is None:
            fail("rebind", "the keeper row vanished across the restart")
        if row.status != "live":
            fail("rebind", f"the sweep left the row {row.status!r}")
        if row.keeper_child_pid != child_pid:
            fail("rebind", f"the sweep rebound a different child: {row.keeper_child_pid}")
        if row.harness_session_id != session_id or row.messaging_socket_path != str(keeper_sock):
            fail("rebind", "the rebound row lost its id or socket")
        ok("rebind", "same child, same socket, same session id")

        # SURVIVE: a fresh turn on the SAME remote chat recalls turn one's
        # codeword. The needle is scanned only in bytes newer than the
        # prompt, so the envelope's own paint cannot satisfy it.
        baseline = len(tap.buf)
        send_mail(PROMPT_TWO)
        if not tap.wait_for_fresh(CODEWORD, baseline, 240.0):
            fail("recall", "a fresh process turn did not recall the codeword")
        ok("recall", f"codeword={CODEWORD}")

    finally:
        for popen in (daemon, mux):
            if popen is not None:
                popen.kill()
        if tap is not None:
            tap.close()
        # Tear the hosted thread down child-first; the keeper exits when its
        # child does, and anything left is killed so the machine keeps no
        # stray cursor-agent TUI from a smoke run.
        try:
            if alive(child_pid):
                os.kill(child_pid, signal.SIGKILL)
            time.sleep(1.0)
            if alive(keeper_pid):
                os.kill(keeper_pid, signal.SIGKILL)
        except (OSError, ProcessLookupError):
            pass
        shutil.rmtree(short_state, ignore_errors=True)
        shutil.rmtree(journey_cwd, ignore_errors=True)

    print("SK-CURSOR-DONE journey=wk-cursor verdict=green")


if __name__ == "__main__":
    main()
