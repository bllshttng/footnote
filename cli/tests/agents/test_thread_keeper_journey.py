"""The restart journey (x-61bc): a keeper-hosted pi thread survives the death
of both of its supervisors, unchanged, and keeps conversing.

This is the epic x-16b7 acceptance proof. A thread row appearing in a roster
proves nothing - the row appears today. The proof is a journey a person could
not fake: a live keeper-hosted pi thread with its child pid recorded, then
BOTH supervisors (the mux server and ``fno-agents-daemon``) killed with
SIGKILL, and then the named pid still alive, its parents unchanged and never
a supervisor, its cwd unchanged, its session id unchanged, pi's own session
store having gained NOTHING, and the conversation continuing across the
restart with a second prompt answered from memory of the first.

RUN OF RECORD, 2026-09-01, pi 0.84.2/0.84.3 on a live openai-codex
subscription, debug builds of fno-agents + the Rust front from the x-61bc
worktree: **RED at step 7, and that is the finding.** Steps 1-6 pass: the
keeper-hosted pi thread spawns, paints, survives BOTH supervisors killed with
SIGKILL, keeps its child pid, cwd and session id, and mints nothing. The
conversation step fails: the pasted prompt reaches pi's composer (rendered)
but pi does not submit it on Enter in ANY python-driven pty - bare
``pty.fork``, the keeper's pty, and a logging relay inside tmux all reproduce;
the same bytes submit inside real tmux, and ``pi -p`` answers instantly. The
defect is in pi's TUI/terminal handshake, not in fno's lane. The gate flip is
therefore WITHHELD (this test never passed; AC3-ERR is unsatisfied), and this
file is the acceptance instrument for whoever closes the gap. Never weaken the
step-7 assertions to turn this green.

The live test is opt-in (``FNO_PI_LIVE=1``) because it spends real
subscription tokens and needs this machine's pi credentials. It is never to
be deleted: the plan's kill_criteria names ``proof_test_deleted`` because a
worker stuck on this proof deleting the test that demands it - or flipping
``thread = true`` to make a suite go green - is the exact failure this epic
exists to prevent. If no real terminal is reachable, the journey STOPS and
reports; a green unit suite proves the frame protocol and nothing about
survival.
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

from fno.agents.harnesses.pi import lookup_sessions, pi_sessions_root
from fno.agents.registry import load_registry
from fno.paths_testing import use_tmpdir

LIVE = os.environ.get("FNO_PI_LIVE") == "1"
PI_ON_PATH = (
    subprocess.run(["which", "pi"], capture_output=True).returncode == 0  # noqa: S603,S607
)

_TEST_DIR = Path(__file__).resolve().parent
_REPO = _TEST_DIR.parents[2]
_CLI_SRC = _REPO / "cli" / "src"


def _crate_bin(crate: str, name: str) -> Path | None:
    """Prefer the worktree's cargo-built binary: the journey must run the code
    under test, not a deployed binary that can predate the keeper lane."""
    for profile in ("debug", "release"):
        built = _REPO / "crates" / crate / "target" / profile / name
        if built.is_file() and os.access(built, os.X_OK):
            return built
    return None


_worker_bin = _crate_bin("fno-agents", "fno-agents-worker")
_daemon_bin = _crate_bin("fno-agents", "fno-agents-daemon")
_client_bin = _crate_bin("fno-agents", "fno-agents")
_front_bin = _crate_bin("fno", "fno")

_SKIP = ""
if not LIVE:
    _SKIP = "live pi journey spends subscription tokens; set FNO_PI_LIVE=1 to run"
elif not PI_ON_PATH:
    _SKIP = "pi is not on PATH"
elif _worker_bin is None:
    _SKIP = "no fno-agents-worker binary; cargo build -p fno-agents first"
elif _daemon_bin is None:
    _SKIP = "no fno-agents-daemon binary; cargo build -p fno-agents first"
elif _client_bin is None:
    _SKIP = "no fno-agents client binary; cargo build -p fno-agents first"
elif _front_bin is None:
    _SKIP = "no rust front binary; cargo build -p fno first"

CODEWORD = "PIKEEPER"
PROMPT_ONE = f"Remember the codeword {CODEWORD}. Reply with just: OK"
PROMPT_TWO = "What codeword were you asked to remember? Reply with just the word."

_JOURNEY_NAME = "wk-x61bc"
_SPAWN_SNIPPET = (
    "import json, sys\n"
    f"sys.path.insert(0, {str(_CLI_SRC)!r})\n"
    "from pathlib import Path\n"
    "from fno.agents.dispatch import _lane_b_thread_spawn\n"
    "receipt = _lane_b_thread_spawn(\n"
    f"    name={_JOURNEY_NAME!r}, harness='pi', cwd=Path({{cwd!r}})\n"
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


def _wait_pi_ready(sock: Path, timeout_s: float = 90.0) -> bool:
    """Read keeper Output frames until pi's own status bar has painted.

    The `(sub)` subscription tag and the `(auto)` compaction tag render only
    once the TUI is up and the model session is wired; pi's first frame alone
    proves nothing about readiness, which is exactly how a paste gets
    swallowed. The pane lane reads the same bar as its idle marker
    (manifests/pi.toml `idle_status_bar`); the keeper lane has no screen to
    scrape, so the same fact is read off the pty stream itself. The two tags
    are matched independently so ANSI escapes between them cannot mask either.
    """
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
            if b"(sub)" in buf and b"(auto)" in buf:
                time.sleep(0.5)
                return True
    finally:
        conn.close()
    return False


def _assistant_texts(cwd: Path, session_id: str) -> list[str]:
    """The assistant texts pi has recorded for one (cwd, session id) pair."""
    lookup = lookup_sessions(cwd, session_id)
    texts: list[str] = []
    for path in lookup.files:
        for line in path.read_text(encoding="utf-8").splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if row.get("type") != "message":
                continue
            message = row.get("message") or {}
            if message.get("role") != "assistant":
                continue
            if message.get("stopReason") == "error":
                raise AssertionError(
                    f"pi errored on a turn: {message.get('errorMessage')!r}"
                )
            texts.extend(
                part.get("text", "")
                for part in (message.get("content") or [])
                if isinstance(part, dict) and part.get("type") == "text"
            )
    return texts


def _wait_for_answer(
    cwd: Path, session_id: str, baseline: int, needle: str, timeout_s: float
) -> str:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        texts = _assistant_texts(cwd, session_id)
        fresh = [t for t in texts if needle in t]
        if len(texts) > baseline and fresh:
            return fresh[-1]
        time.sleep(2.0)
    texts = _assistant_texts(cwd, session_id)
    raise AssertionError(
        f"no assistant reply containing {needle!r} within {timeout_s:.0f}s; "
        f"assistant texts seen: {texts!r}"
    )


def _send_mail(text: str, session_id: str, monkeypatch, capsys) -> None:
    """One prompt through the real mail lane: the same name-lane choke point
    `fno agents mail send` runs, with the injector binary resolved to the
    worktree build (a deployed binary can predate the keeper lane). A
    not-confirmed landing fails HERE, naming the verb's reason, never later
    as a mysterious missing answer."""
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
        agent="pi",
    )
    mail_cli._name_lane_send(text, from_name="web", resolved=resolved)
    out = capsys.readouterr().out
    assert "delivered (hosted)" in out, f"mail never confirmed a landing: {out}"


def _sessions_snapshot(cwd: Path) -> dict[str, list[str]]:
    """pi's own session store: the root listing plus the journey cwd's own
    listing, before and after the restart, compared byte for byte."""
    from fno.agents.harnesses.pi import session_dir

    root = pi_sessions_root()
    directory = session_dir(cwd)
    return {
        "root": sorted(p.name for p in root.iterdir()) if root.is_dir() else [],
        "cwd": sorted(p.name for p in directory.iterdir()) if directory.is_dir() else [],
    }


def _wait_ready(sock_path: Path, timeout_s: float = 30.0) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if sock_path.exists():
            probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            probe.settimeout(1.0)
            try:
                probe.connect(str(sock_path))
                return
            except OSError:
                pass
            finally:
                probe.close()
        time.sleep(0.1)
    raise AssertionError(f"{sock_path} never came up within {timeout_s:.0f}s")


@pytest.mark.skipif(bool(_SKIP), reason=_SKIP)
def test_AC1_HP_the_restart_journey_on_a_real_pi_thread(tmp_path, monkeypatch, capsys) -> None:
    """The seven-step journey, end to end, on a real terminal."""
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "inbox"))
    # The real HOME, because pi's credential lives under it and the hermetic
    # sandbox has none: pi comes up unable to reach a model and its status bar
    # never paints. The same move test_pi_journey.py makes for the same reason;
    # the cwd and state stay isolated, so the real home is only READ for
    # credentials and pi's own session store.
    user = os.environ.get("USER", "")
    real_home = next(
        (
            candidate
            for candidate in (Path("/Users") / user, Path("/home") / user, Path("/root"))
            if candidate.is_dir()
        ),
        None,
    )
    assert real_home is not None, "the live pi journey could not locate the real HOME"
    monkeypatch.setenv("HOME", str(real_home))
    monkeypatch.setenv("USERPROFILE", str(real_home))

    # A keeper socket must fit AF_UNIX's 104-byte sun_path and the pytest
    # basetemp does not: move the whole isolated state root to a shorter dir,
    # flat (the resolved /private prefix alone eats 8 bytes on macOS). It
    # stays under TMPDIR - the hermetic guard's allowed root - so the sandbox
    # still covers every write. FNO_AGENTS_HOME then makes all four parties
    # agree on one root: the Python registry (state_dir()/agents/registry.json),
    # the daemon's home (root/registry.json), the keeper socket
    # (home.parent()/mux/threads/), and the daemon-start keeper sweep that
    # reads the same directory.
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
    monkeypatch.setenv("PATH", f"{_front_bin.parent}{os.pathsep}{os.environ['PATH']}")
    # The hosted pi is a real TUI on a real pty: give it a terminal type the
    # bg harness may not carry.
    monkeypatch.setenv("TERM", os.environ.get("TERM") or "xterm-256color")

    journey_cwd = tmp_path / "journey"
    journey_cwd.mkdir()
    session_name = _JOURNEY_NAME
    daemon_popen: subprocess.Popen | None = None
    mux_popen: subprocess.Popen | None = None
    keeper_pid: int | None = None
    try:
        # ---- Step 1: spawn pi as a lane-B thread from a short-lived caller.
        # The caller must EXIT so the keeper orphans to launchd: the AC's
        # "parent is 1" is a statement about the spawner being gone, and a
        # keeper parented by the live test process could prove nothing about
        # who hosts it.
        env = dict(os.environ)
        env["PYTHONPATH"] = str(_CLI_SRC) + os.pathsep + env.get("PYTHONPATH", "")
        spawn_cwd = str(journey_cwd)
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

        # Positive control: the keeper behind the row answers Identify with
        # the minted identity and the child pid that rode the reply.
        from fno.agents.dispatch import _keeper_identify

        identify = _keeper_identify(keeper_sock)
        assert identify["session_id"] == session_id
        assert identify["child_pid"] == child_pid

        # The hosted TUI is really up before the first paste goes in.
        assert _wait_pi_ready(keeper_sock), (
            "pi's status bar never painted: the hosted TUI never became ready"
        )

        # ---- Step 2: snapshot pi's session store.
        snapshot = _sessions_snapshot(journey_cwd)

        # ---- First turn, before anything dies: the codeword goes in through
        # the mail lane and a real model answer comes back.
        baseline = len(_assistant_texts(journey_cwd, session_id))
        _send_mail(PROMPT_ONE, session_id, monkeypatch, capsys)
        _wait_for_answer(journey_cwd, session_id, baseline, "OK", 180.0)

        # ---- Step 3: start both supervisors, then kill -9 BOTH. Not
        # SIGTERM: a graceful exit could spare the child by a route that says
        # nothing about the hangup.
        daemon_popen = subprocess.Popen(  # noqa: S603
            [str(_daemon_bin), "--home", str(agents_home)],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        _wait_ready(agents_home / "supervisor.sock")
        mux_session = "journey-x61bc"
        mux_sock = short_state / "mux" / f"{mux_session}.sock"
        mux_popen = subprocess.Popen(  # noqa: S603
            [str(_front_bin), "mux", "server", "--session", mux_session],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        _wait_ready(mux_sock)
        supervisor_pids = {daemon_popen.pid, mux_popen.pid}

        keeper_ppid_before = _ps_field("ppid", keeper_pid)
        child_ppid_before = _ps_field("ppid", child_pid)

        os.kill(daemon_popen.pid, signal.SIGKILL)
        os.kill(mux_popen.pid, signal.SIGKILL)
        daemon_popen.wait(timeout=30)
        mux_popen.wait(timeout=30)
        daemon_popen = None
        mux_popen = None

        # ---- Step 4: the recorded child pid is alive, parented by the SAME
        # keeper, whose own parent is launchd (pid 1) - never a supervisor.
        assert _alive(child_pid), f"child pid {child_pid} did not survive the kill"
        assert _alive(keeper_pid), f"keeper pid {keeper_pid} did not survive the kill"
        keeper_ppid = _ps_field("ppid", keeper_pid)
        assert keeper_ppid == "1", (
            f"the keeper must be orphaned to launchd, not hosted: ppid={keeper_ppid!r}"
        )
        assert keeper_ppid_before == "1", (
            f"the keeper was already hosted at spawn time: ppid={keeper_ppid_before!r}"
        )
        assert keeper_ppid not in {str(p) for p in supervisor_pids}
        child_ppid = _ps_field("ppid", child_pid)
        assert child_ppid == child_ppid_before == str(keeper_pid), (
            f"the child's parent changed across the kill: {child_ppid_before!r} -> {child_ppid!r}"
        )
        child_cwd = _child_cwd(child_pid)
        assert child_cwd is not None and os.path.realpath(child_cwd) == os.path.realpath(
            journey_cwd
        ), f"the child's cwd changed: {child_cwd!r}"

        # The surviving keeper still answers for the SAME session id.
        identify = _keeper_identify(keeper_sock)
        assert identify["session_id"] == session_id
        assert identify["child_pid"] == child_pid

        # ---- Step 5: restart both. The daemon start runs the registry-side
        # keeper sweep, and the row must come back bound to the SAME child.
        daemon_popen = subprocess.Popen(  # noqa: S603
            [str(_daemon_bin), "--home", str(agents_home)],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        _wait_ready(agents_home / "supervisor.sock")
        mux_popen = subprocess.Popen(  # noqa: S603
            [str(_front_bin), "mux", "server", "--session", mux_session],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        _wait_ready(mux_sock)

        deadline = time.monotonic() + 30
        row = None
        while time.monotonic() < deadline:
            entries = [e for e in load_registry() if e.name == session_name]
            row = entries[0] if entries else None
            if row is not None and row.keeper_child_pid == child_pid:
                break
            time.sleep(0.5)
        assert row is not None, "the keeper row vanished across the restart"
        assert row.status == "live", f"the sweep left the row {row.status!r}"
        assert row.keeper_child_pid == child_pid, (
            f"the sweep rebound a different child: {row.keeper_child_pid}"
        )
        assert row.harness_session_id == session_id
        assert row.messaging_socket_path == str(keeper_sock)
        assert os.path.realpath(row.cwd) == os.path.realpath(journey_cwd)
        events = agents_home / "events.jsonl"
        rebound = False
        if events.exists():
            for line in events.read_text(encoding="utf-8").splitlines():
                if not line.strip():
                    continue
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if (
                    event.get("type") == "keeper_row_rebound"
                    and event.get("data", {}).get("name") == session_name
                ):
                    rebound = True
                    break
        assert rebound, "the daemon start never emitted keeper_row_rebound"

        # ---- Step 6: pi's session store gained NOTHING. A new file is a
        # minted session and fails the epic.
        after = _sessions_snapshot(journey_cwd)
        assert after["cwd"] == snapshot["cwd"], (
            f"the journey cwd's session listing changed across the restart: "
            f"{snapshot['cwd']} -> {after['cwd']}"
        )
        assert after["root"] == snapshot["root"], (
            f"pi's session store root changed across the restart: "
            f"{snapshot['root']} -> {after['root']}"
        )

        # ---- Step 7: the conversation continues - the second prompt is
        # answered FROM MEMORY of the first. A live pid hosting a wedged
        # harness is not a survival.
        baseline = len(_assistant_texts(journey_cwd, session_id))
        _send_mail(PROMPT_TWO, session_id, monkeypatch, capsys)
        answer = _wait_for_answer(journey_cwd, session_id, baseline, CODEWORD, 180.0)
        assert CODEWORD in answer

        # ---- Cleanup, dogfooding the recovered PR 1332 finding's fix: the
        # stop arm drives the SAME socket the journey proved survives, and the
        # child goes with it. A lane that cannot be stopped is the leak class
        # that nearly cost the operator the machine.
        from fno.agents.dispatch import stop_agent

        result = stop_agent(session_name)
        assert result.name == session_name
        deadline = time.monotonic() + 10
        while _alive(child_pid) and time.monotonic() < deadline:
            time.sleep(0.2)
        assert not _alive(child_pid), "the child outlived a confirmed keeper stop"
        assert not keeper_sock.exists()
        row = next(e for e in load_registry() if e.name == session_name)
        assert row.status == "exited"
    finally:
        # Cleanup through the NEW stop arm when it exists (dogfood); SIGKILL
        # the private supervisors regardless.
        try:
            if keeper_pid and _alive(keeper_pid):
                os.kill(keeper_pid, signal.SIGKILL)
        except OSError:
            pass
        for popen in (daemon_popen, mux_popen):
            if popen is not None and popen.poll() is None:
                popen.kill()
                popen.wait(timeout=30)
        shutil.rmtree(short_state, ignore_errors=True)
