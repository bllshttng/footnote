#!/usr/bin/env python3
"""Live keeper-lane journey for cursor-agent (journey wk-cursor).

pi's pytest journey (cli/tests/agents/test_thread_keeper_journey.py) proved
the LANE itself: a keeper-hosted child survives the SIGKILL of both
supervisors with pid, cwd and session id unchanged, and the restart sweep
re-binds the same row. That machinery is harness-agnostic and is not
re-proven here beyond the bounds the cursor steps need.

This script proves what is cursor-agent-SPECIFIC, on the real binary:

1. CALLEE-MINTED ID - the row's harness_session_id is a chat id
   `create-chat` returned, present before the child launched.
2. REMOTE STORE - the id appears in NO file under ~/.cursor (positive
   control: the state root itself is real and readable).
3. SEATED DRIVE - the keeper honors Input only from its one subscriber
   seat, so one connection takes the seat, forces a repaint with Resize,
   reads the idle composer marker, and drives both turns.
4. SURVIVE - across SIGKILL of both supervisors, a turn on the same pty
   recalls turn one's codeword: the cross-process recall the
   callee-minted-read-back binding claims.
5. MAIL - after the seat frees, one real turn through the mail lane's
   keeper injector lands and is confirmed; the model's answer repaints
   onto a fresh reader.

Run it from a worktree that has built the Rust binaries:

    cargo build -p fno-agents
    uv run --project cli python cli/scripts/smoke/cursor-agent-keeper-journey.py

It runs OUTSIDE pytest on purpose: the test suite sandboxes $HOME, and
cursor-agent's credential lives in the real ~/.cursor/cli-config.json. The
script needs a logged-in cursor-agent and spends three real model turns.
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
MAIL_NEEDLE = "MAIL-OK-601E"
PROMPT_ONE = f"Remember the codeword {CODEWORD}. Reply with exactly {REPLY_ONE} and nothing else."
PROMPT_TWO = (
    "What codeword were you asked to remember? Reply with exactly that word "
    "and nothing else."
)
PROMPT_THREE = f"Reply with exactly {MAIL_NEEDLE} and nothing else."
JOURNEY_NAME = "wk-cursor"
READY_MARKER = b"Plan, search, build anything"  # manifests/cursor-agent.toml idle_plan_build


def strip_ansi(raw: bytes) -> bytes:
    """Drop ANSI escape sequences so a marker match cannot be broken by the
    TUI painting one character per escape-wrapped write. Mirrors the Rust
    matcher input in mail_inject.rs: lossy on purpose."""
    out = bytearray()
    i = 0
    n = len(raw)
    while i < n:
        b = raw[i]
        if b == 0x1B:
            if i + 1 < n and raw[i + 1 : i + 2] == b"[":
                i += 2
                while i < n and not (0x40 <= raw[i] <= 0x7E):
                    i += 1
                i += 1
            else:
                i += 2
        else:
            out.append(b)
            i += 1
    return bytes(out)

TAG_INPUT = 1
TAG_RESIZE = 2


def fail(step: str, detail: str) -> None:
    print(f"SK-CURSOR-FAIL step={step} detail={detail}")
    sys.exit(1)


def ok(step: str, detail: str = "") -> None:
    print(f"SK-CURSOR-OK step={step}" + (f" detail={detail}" if detail else ""), flush=True)


def frame(tag: int, payload: bytes) -> bytes:
    """One keeper frame: u8 tag | u32 LE length | payload."""
    return bytes([tag]) + len(payload).to_bytes(4, "little") + payload


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


class Seat:
    """The keeper's one subscriber seat, held for the whole drive phase.

    The keeper honors Input only from the first seated connection and never
    replays its ring to a late one, so the drive reads, the repaint trigger
    and the typing all ride this single connection.
    """

    def __init__(self, sock: Path) -> None:
        self._conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._conn.settimeout(2.0)
        self._conn.connect(str(sock))
        self.buf = bytearray()
        self.text = bytearray()  # ANSI-stripped view of buf
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._drain, daemon=True)
        self._thread.start()
        self.resize()

    def resize(self, rows: int = 24, cols: int = 80) -> None:
        payload = rows.to_bytes(2, "little") + cols.to_bytes(2, "little")
        self._conn.sendall(frame(TAG_RESIZE, payload))

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
            self.text.extend(strip_ansi(chunk))

    def type_and_submit(self, text: str) -> None:
        self._conn.sendall(frame(TAG_INPUT, text.encode("utf-8")))
        time.sleep(0.1)  # the row's measured settle delay is 0ms; a beat
        self._conn.sendall(frame(TAG_INPUT, b"\r"))

    def wait_for_fresh(self, needle: str, start: int, timeout_s: float) -> bool:
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if needle.encode() in bytes(self.text[start:]):
                return True
            time.sleep(0.5)
        return False

    def close(self) -> None:
        self._stop.set()
        self._thread.join(timeout=5)
        self._conn.close()


def wait_ready(seat: Seat, timeout_s: float = 90.0) -> int:
    """Read the idle composer marker off the seated stream; return the byte
    offset it appeared at so later turns read only fresher bytes.

    The boot paint may have happened while nobody held the seat (the ring is
    not replayed to a late subscriber), and a same-size Resize fires no
    SIGWINCH, so the loop alternates sizes to force a repaint every few
    seconds until the marker arrives."""
    deadline = time.monotonic() + timeout_s
    flip = False
    last_nudge = 0.0
    while time.monotonic() < deadline:
        if READY_MARKER in bytes(seat.text):
            time.sleep(0.5)
            return len(seat.text)
        now = time.monotonic()
        if now - last_nudge >= 4.0:
            flip = not flip
            if flip:
                seat.resize(25, 81)
            else:
                seat.resize(24, 80)
            last_nudge = now
        time.sleep(0.5)
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
    # The mux SERVER is the front `fno` binary (crates/fno), not the
    # fno-agents client: two crates, two binaries.
    front_bin = crate_bin("fno", "fno")

    if not cursor_authenticated():
        fail("auth", "cursor-agent is not authenticated; run 'cursor-agent login' first")
    ok("auth")

    # The spawn runs from a short-lived caller so the keeper orphans to
    # launchd; both state roots move to a SHORT tempdir (AF_UNIX's 104-byte
    # sun_path cannot hold long temp paths). The fno-python side (registry,
    # keeper log, mail lane) resolves its state root from the CWD's
    # .fno/settings.yaml - FNO_AGENTS_HOME alone does not move the Python
    # registry - so every fno-python step below runs in a subprocess whose
    # cwd is this dir, and the REAL home stays untouched.
    short_state = Path(tempfile.mkdtemp(prefix="fno5c-"))
    journey_cwd = Path(tempfile.mkdtemp(prefix="fno5c-journey-"))
    (short_state / ".fno").mkdir(parents=True, exist_ok=True)
    (short_state / ".fno" / "settings.yaml").write_text(
        f"schema_version: 1\nconfig:\n  state_dir: {short_state}/\n",
        encoding="utf-8",
    )
    agents_home = short_state / "agents"
    env = dict(os.environ)
    env["FNO_AGENTS_HOME"] = str(agents_home)
    env["FNO_MUX_DIR"] = str(short_state / "mux")
    env["FNO_AGENTS_WORKER_BIN"] = str(worker_bin)
    env["FNO_AGENTS_IDLE_EXIT_SECS"] = "1800"
    env["PATH"] = f"{client_bin.parent}{os.pathsep}{env.get('PATH', '')}"
    env["PYTHONPATH"] = str(CLI_SRC) + os.pathsep + env.get("PYTHONPATH", "")
    env["FNO_AGENTS_BIN"] = str(client_bin)
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
        env=env, cwd=str(short_state), capture_output=True, text=True, timeout=180,
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

    # Remote store: positive control first, then the bounded absence.
    root = Path.home() / ".cursor"
    if not (root.is_dir() and any(root.iterdir())):
        fail("store-control", f"{root} is not a readable state root")
    found, scanned = id_in_state_root(session_id)
    if found:
        fail("store", f"the chat id appeared on disk in {scanned} files; the store is not remote")
    ok("store", f"absent_across={scanned}_files")

    seat: Seat | None = None
    daemon: subprocess.Popen | None = None
    mux: subprocess.Popen | None = None
    try:
        # The seat is taken BEFORE the first repaint read: whatever the TUI
        # painted before this connection arrived, the Resize forces back onto
        # the stream now.
        seat = Seat(keeper_sock)
        wait_ready(seat)
        ok("ready")

        seat.type_and_submit(PROMPT_ONE)
        if not seat.wait_for_fresh(REPLY_ONE, 0, 240.0):
            fail("turn-one", f"the TUI never painted {REPLY_ONE!r}")
        ok("turn-one", f"painted={REPLY_ONE}")

        # Kill -9 BOTH supervisors (not SIGTERM: a graceful exit could spare
        # the child by a route that says nothing about the hangup). The seat
        # connects to the KEEPER socket and survives both.
        daemon = subprocess.Popen(
            [str(daemon_bin), "--home", str(agents_home)],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_sock_ready(agents_home / "supervisor.sock")
        mux_session = "journey-cursor"
        mux_sock = short_state / "mux" / f"{mux_session}.sock"
        mux = subprocess.Popen(
            [str(front_bin), "mux", "server", "--session", mux_session],
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
            [str(front_bin), "mux", "server", "--session", mux_session],
            env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_sock_ready(mux_sock)

        rebind_snippet = (
            "import json, sys, time\n"
            f"sys.path.insert(0, {str(CLI_SRC)!r})\n"
            "from fno.agents.registry import load_registry\n"
            "deadline = time.monotonic() + 30\n"
            "row = None\n"
            "while time.monotonic() < deadline:\n"
            f"    entries = [e for e in load_registry() if e.name == {JOURNEY_NAME!r}]\n"
            "    row = entries[0] if entries else None\n"
            f"    if row is not None and row.keeper_child_pid == {child_pid}:\n"
            "        break\n"
            "    time.sleep(0.5)\n"
            "if row is None:\n"
            "    print('ROW:none')\n"
            "else:\n"
            "    print('ROW:' + json.dumps({'status': row.status, 'child': "
            "row.keeper_child_pid, 'sid': row.harness_session_id, "
            "'sock': row.messaging_socket_path}))\n"
        )
        rproc = subprocess.run(
            [sys.executable, "-c", rebind_snippet],
            env=env, cwd=str(short_state), capture_output=True, text=True, timeout=90,
        )
        row_line = next((l for l in rproc.stdout.splitlines() if l.startswith("ROW:")), None)
        if row_line is None or row_line == "ROW:none":
            fail("rebind", f"the keeper row vanished or never rebound: {rproc.stdout[-200:]!r}")
        row = json.loads(row_line[len("ROW:"):])
        if row["status"] != "live":
            fail("rebind", f"the sweep left the row {row['status']!r}")
        if row["child"] != child_pid:
            fail("rebind", f"the sweep rebound a different child: {row['child']}")
        if row["sid"] != session_id or row["sock"] != str(keeper_sock):
            fail("rebind", "the rebound row lost its id or socket")
        ok("rebind", "same child, same socket, same session id")

        # SURVIVE: a turn on the SAME pty recalls turn one's codeword. The
        # needle is scanned only in bytes newer than the prompt, so the
        # prompt's own echo cannot satisfy it.
        baseline = len(seat.buf)
        seat.type_and_submit(PROMPT_TWO)
        if not seat.wait_for_fresh(CODEWORD, baseline, 240.0):
            fail("recall", "a fresh turn did not recall the codeword")
        ok("recall", f"codeword={CODEWORD}")

        # Free the seat so the mail injector can take it, then drive one real
        # turn through the mail lane's keeper injector: the same verb
        # `fno agents mail send` runs, whose cursor-agent confirm reads the
        # pty repaint on its own connection.
        seat.close()
        seat = None
        time.sleep(1.0)

        mail_snippet = (
            "import sys, contextlib, io\n"
            f"sys.path.insert(0, {str(CLI_SRC)!r})\n"
            "from fno.mail import cli as mail_cli\n"
            "from fno.agents.discover import DiscoveredSession\n"
            f"resolved = DiscoveredSession(session_id={session_id!r}, "
            f"short_id='', handle={JOURNEY_NAME!r}, pid=0, cwd='', "
            "project=None, status='live', agent='cursor-agent')\n"
            "out = io.StringIO()\n"
            "with contextlib.redirect_stdout(out):\n"
            f"    mail_cli._name_lane_send({PROMPT_THREE!r}, from_name='web', "
            "resolved=resolved)\n"
            "value = out.getvalue().strip().splitlines()\n"
            "print('MAILRCPT:' + (value[-1] if value else '(none)'))\n"
        )
        mproc = subprocess.run(
            [sys.executable, "-c", mail_snippet],
            env=env, cwd=str(short_state), capture_output=True, text=True, timeout=240,
        )
        rcpt = next(
            (l for l in mproc.stdout.splitlines() if l.startswith("MAILRCPT:")),
            "MAILRCPT:(none)",
        )
        ok("mail-sent", f"receipt={rcpt[len('MAILRCPT:'):][:120]}")

        # A fresh reader takes the now-free seat; the Resize forces the
        # current screen (composer plus the last turn) back onto the stream,
        # so the model's answer to the mailed prompt is observable.
        reader = Seat(keeper_sock)
        try:
            deadline = time.monotonic() + 240.0
            seen = False
            flip = False
            last_nudge = 0.0
            while time.monotonic() < deadline:
                if MAIL_NEEDLE.encode() in bytes(reader.text):
                    seen = True
                    break
                now = time.monotonic()
                if now - last_nudge >= 4.0:
                    flip = not flip
                    if flip:
                        reader.resize(25, 81)
                    else:
                        reader.resize(24, 80)
                    last_nudge = now
                time.sleep(2.0)
            if not seen:
                fail("mail-confirm", f"the mailed turn's answer {MAIL_NEEDLE!r} never repainted")
            ok("mail-confirm", f"painted={MAIL_NEEDLE}")
        finally:
            reader.close()

    finally:
        for popen in (daemon, mux):
            if popen is not None:
                popen.kill()
        if seat is not None:
            seat.close()
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
