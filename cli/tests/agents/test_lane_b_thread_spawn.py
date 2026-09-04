"""The lane-B thread spawn (x-889a): a pane-less keeper hosts the thread.

A lane-B harness (transcript on disk, no daemon to hold the session) gets
its thread from fno holding the pty master itself: the spawn mints the
harness session id BEFORE launch, renders the harness's create argv
through the capability seam, and runs it under the SAME keeper the mux
hosts panes on. These tests pin the argv shape, the registry row, the
lane-A refusal, and - when a worker binary is resolvable - the real
keeper journey end to end against a stub provider.
"""
from __future__ import annotations

import inspect
import os
import shutil
import socket
import subprocess
import tempfile
import threading
import time
from pathlib import Path

import pytest

from fno.agents import dispatch as dispatch_mod
from fno.agents.dispatch import (
    DispatchAskError,
    _lane_b_thread_spawn,
    _mint_thread_session_id,
)
from fno.agents.harness_map import (
    DispatchResolveError,
    capabilities,
    render_session_argv,
    resolve_dispatch,
    thread_lane,
)
from fno.agents.harnesses.pi import pi_model, pi_provider
from fno.agents.registry import load_registry
from fno.paths_testing import use_tmpdir


def _fake_keeper(monkeypatch, tmp_path):
    """Stub the keeper launch + Identify so the spawn runs without a binary.

    Records the Popen argv and answers Identify with the session id the
    argv carries, the same read off the wire the real keeper does.
    """
    recorded: dict[str, object] = {}

    class _FakeProc:
        pid = 4242

        def kill(self) -> None:  # pragma: no cover - failure paths only
            recorded["killed"] = True

    def _fake_popen(argv, **kwargs):  # noqa: ANN001, ANN202
        recorded["argv"] = argv
        return _FakeProc()

    def _fake_identify(sock, timeout_sec=10.0):  # noqa: ANN001
        argv = list(recorded["argv"])  # type: ignore[arg-type]
        session_id = argv[argv.index("--pane-key") + 1]
        return {
            "v": 1,
            "keeper_pid": 4242,
            "child_pid": 555,
            "session_id": session_id,
            "argv": argv[argv.index("--") + 1 :],
            "cwd": str(tmp_path),
        }

    monkeypatch.setattr(dispatch_mod.subprocess, "Popen", _fake_popen)
    monkeypatch.setattr(dispatch_mod, "_keeper_identify", _fake_identify)
    monkeypatch.setattr(
        dispatch_mod, "_lane_b_worker_binary", lambda: Path("/fake/fno-agents-worker")
    )
    return recorded


@pytest.fixture
def lane_b_home(tmp_path, monkeypatch):
    """Isolated fno home; the provider renders as pi but never runs."""
    use_tmpdir(monkeypatch, tmp_path)
    return tmp_path


# ---------------------------------------------------------------------------
# AC4-ERR: the public dispatch surface still refuses
# ---------------------------------------------------------------------------

def test_agy_thread_dispatch_resolves_on_the_journey_backed_seat(lane_b_home) -> None:
    """agy's spawn claim reads native behind its own journey
    (test_agy_spawn_journey.py), so the keeper lane built here IS reachable
    through the public dispatch surface. agy participates in the loop
    natively (a Stop handler in its own hooks.json), so unlike pi it needs no
    installed extension for the autonomous `/target` template to resolve."""
    from fno.agents.harness_map import spawn_state, thread_seatable

    assert spawn_state("agy") == "native"
    assert thread_seatable("agy") is True
    resolved = resolve_dispatch(harness="agy", substrate="thread", command="agy --version")
    assert resolved["substrate"] == "thread"
    assert resolved["thread"] is True
    resolved_loop = resolve_dispatch(harness="agy", substrate="thread")
    assert resolved_loop["substrate"] == "thread"
    assert resolved_loop["loop_participation"] == "native"


def test_gemini_thread_dispatch_still_refuses_at_the_gate(lane_b_home) -> None:
    """The refusal this file used to assert through agy still has a subject.
    gemini carries a capability row and no seat, so nothing resolves onto a
    keeper lane nobody built for it. Its DEPRECATION gate fires before the
    substrate gate, so that is the message asserted - the point is that the
    row alone never buys the lane."""
    from fno.agents.harness_map import thread_seatable

    assert thread_seatable("gemini") is False
    with pytest.raises(DispatchResolveError) as exc_info:
        resolve_dispatch(harness="gemini", substrate="thread")
    assert "no maintained footnote dispatch lane" in str(exc_info.value)


def test_pi_thread_dispatch_resolves_on_the_journey_backed_bit(monkeypatch) -> None:
    """pi's spawn claim reads native behind its passing restart journey
    (test_thread_keeper_journey.py), so a one-shot dispatch resolves onto the
    lane this file builds. The autonomous `/target` template resolves too
    since x-43bd shipped pi's loop extension - the loop gate that used to
    refuse it here now passes (with the artifact installed; the install gate
    is asserted in test_harness_loop_participation.py)."""
    import fno.agents.harness_map as harness_map

    monkeypatch.setattr(harness_map, "_loop_extension_installed", lambda h: True)
    from fno.agents.harness_map import thread_seatable

    assert thread_seatable("pi") is True
    resolved = resolve_dispatch(harness="pi", substrate="thread", command="pi --version")
    assert resolved["substrate"] == "thread"
    assert resolved["thread"] is True
    resolved_loop = resolve_dispatch(harness="pi", substrate="thread")
    assert resolved_loop["substrate"] == "thread"
    assert resolved_loop["loop_participation"] == "extension"


def test_lane_b_spawn_wiring_names_every_wired_keeper_harness() -> None:
    """The public dispatch_spawn body reaches the keeper entry point, and the
    table it dispatches through carries every wired arm - cursor-agent
    (callee-minted), pi and grok (caller-assigned) and agy (callee-minted) -
    so no wiring is dropped by accident. Each row is asserted through the
    field that makes it a real arm rather than a stub: the refusal an
    operator reads when the lane has no one-shot form.
    """
    from fno.agents.harness_map import known_harnesses
    from fno.agents.keeper_thread import keeper_arm

    source = inspect.getsource(dispatch_mod.dispatch_spawn)
    assert "keeper_thread_spawn" in source
    rowed = {h for h in known_harnesses() if keeper_arm(h) is not None}
    assert rowed == {"cursor-agent", "pi", "grok", "agy"}
    for harness in sorted(rowed):
        arm = keeper_arm(harness)
        assert arm and arm.get("once_refusal"), f"{harness} declares no one-shot refusal"


# ---------------------------------------------------------------------------
# AC3-EDGE: lane A keeps its harness-owned path
# ---------------------------------------------------------------------------

def test_lane_a_harnesses_are_refused_by_the_lane_b_entry_point(
    lane_b_home, monkeypatch
) -> None:
    """claude and codex resolve to the attach lane, and the lane-B entry
    point refuses them: no keeper is ever created for lane A."""
    assert thread_lane("claude") == "attach"
    assert thread_lane("codex") == "attach"
    for harness in ("claude", "codex"):
        recorded = _fake_keeper(monkeypatch, lane_b_home)
        with pytest.raises(DispatchAskError) as exc_info:
            _lane_b_thread_spawn(name="wk-a", harness=harness, cwd=lane_b_home)
        assert exc_info.value.exit_code == 2
        assert "keeper-lane" in str(exc_info.value)
        assert "argv" not in recorded, "no keeper argv is built for lane A"
        assert not list(load_registry()), "no row is written for lane A"


# ---------------------------------------------------------------------------
# AC1-HP / AC2-HP: the spawn, the argv, the row
# ---------------------------------------------------------------------------

def test_lane_b_spawn_renders_the_contract_argv_and_registers_the_row(
    lane_b_home, monkeypatch
) -> None:
    """The minted id rides the rendered create argv AND the keeper argv, and
    the row carries the socket + the id the viewport's pi thread arm reads."""
    recorded = _fake_keeper(monkeypatch, lane_b_home)
    receipt = _lane_b_thread_spawn(name="wk-pi", harness="pi", cwd=lane_b_home)

    argv = list(recorded["argv"])  # type: ignore[arg-type]
    assert argv[0] == "/fake/fno-agents-worker"
    assert argv[1] == "--keeper", "the pane-less spelling is canonical"
    worker_tail = argv[argv.index("--") + 1 :]
    session_id = receipt["session_id"]
    # The contract's render plus the provider+model completion the pane lane's
    # build_pane_argv has always appended for pi (bare pi defaults to provider
    # google, and `--provider` without `--model` falls to Bedrock).
    expected_tail = [
        *render_session_argv("pi", "interactive_create", session_id),
        "--provider",
        pi_provider(),
        "--model",
        pi_model(),
    ]
    assert worker_tail == expected_tail, (
        "the provider argv is the contract's render plus the pinned "
        "provider/model, never hand-assembled"
    )
    assert argv[argv.index("--pane-key") + 1] == session_id
    assert argv[argv.index("--sock") + 1] == receipt["keeper_socket"]
    assert argv[argv.index("--cwd") + 1] == str(lane_b_home)

    entries = load_registry()
    assert [e.name for e in entries] == ["wk-pi"]
    row = entries[0]
    assert row.harness == "pi"
    assert row.host_mode == "interactive"
    assert row.harness_session_id == session_id
    assert row.messaging_socket_path == receipt["keeper_socket"]
    assert row.pid == 4242
    assert row.mux is None, "a thread row is pane-less: no mux ref"
    assert row.fno_id == session_id
    assert row.origin == "spawn"


def test_lane_b_spawn_renders_grok_argv_and_registers_the_row(
    lane_b_home, monkeypatch
) -> None:
    """grok's keeper arm (x-fd31): the minted caller-assigned uuid rides the
    create argv (the row's interactive_create render) and the keeper's
    --pane-key; the bypass, model and effort axes ride the arm, the same
    completion build_pane_argv's grok arm composes for the pane lane."""
    recorded = _fake_keeper(monkeypatch, lane_b_home)
    receipt = _lane_b_thread_spawn(
        name="wk-grok",
        harness="grok",
        cwd=lane_b_home,
        yolo=True,
        model="grok-4.6",
        effort="high",
    )

    argv = list(recorded["argv"])  # type: ignore[arg-type]
    assert argv[0] == "/fake/fno-agents-worker"
    assert argv[1] == "--keeper", "the pane-less spelling is canonical"
    worker_tail = argv[argv.index("--") + 1 :]
    session_id = receipt["session_id"]
    expected_tail = [
        *render_session_argv("grok", "interactive_create", session_id),
        "--always-approve",
        "--model",
        "grok-4.6",
        "--reasoning-effort",
        "high",
    ]
    assert worker_tail == expected_tail, (
        "the provider argv is the contract's render plus the axes the pane "
        "lane appends, never hand-assembled"
    )
    assert argv[argv.index("--pane-key") + 1] == session_id

    entries = load_registry()
    assert [e.name for e in entries] == ["wk-grok"]
    row = entries[0]
    assert row.harness == "grok"
    assert row.harness_session_id == session_id
    assert row.messaging_socket_path == receipt["keeper_socket"]
    assert row.mux is None, "a thread row is pane-less: no mux ref"


def test_grok_is_a_keeper_lane_harness() -> None:
    """The row's resume forms make grok a keeper-lane harness (interactive
    attach unsupported, resume supported), the same lane pi and cursor-agent
    resolve onto."""
    assert thread_lane("grok") == "keeper"


def test_lane_b_socket_follows_the_agents_home_the_sweep_derives_from(
    lane_b_home, monkeypatch, tmp_path
) -> None:
    """The socket lands where the Rust registry-side sweep reads it: with
    FNO_AGENTS_HOME set, both sides derive the threads dir from its parent,
    so a restart rebind cannot silently find nothing."""
    alt_root = tmp_path / "altstate"
    monkeypatch.setenv("FNO_AGENTS_HOME", str(alt_root / "agents"))
    recorded = _fake_keeper(monkeypatch, lane_b_home)
    receipt = _lane_b_thread_spawn(name="wk-home", harness="pi", cwd=lane_b_home)
    assert receipt["keeper_socket"] == str(alt_root / "mux" / "threads" / "wk-home.sock")
    assert receipt["keeper_socket"] in recorded["argv"]
    row = load_registry()[0]
    assert row.messaging_socket_path == receipt["keeper_socket"]


def test_lane_b_socket_keeps_a_symlinked_agents_home_spelling(
    lane_b_home, monkeypatch, tmp_path
) -> None:
    """A symlinked FNO_AGENTS_HOME keeps its literal spelling in the socket
    path: the Rust sweep matches the row's socket byte-for-byte against a
    dir derived from the raw --home string, so resolving through the link
    (macOS /var -> /private/var for every mkdtemp state root) would orphan
    the socket and every restart rebind would silently find nothing."""
    real = tmp_path / "realstate"
    real.mkdir()
    link = tmp_path / "linkstate"
    link.symlink_to(real)
    monkeypatch.setenv("FNO_AGENTS_HOME", str(link / "agents"))
    _fake_keeper(monkeypatch, lane_b_home)
    receipt = _lane_b_thread_spawn(name="wk-link", harness="pi", cwd=lane_b_home)
    assert receipt["keeper_socket"] == str(link / "mux" / "threads" / "wk-link.sock")
    row = load_registry()[0]
    assert row.messaging_socket_path == str(link / "mux" / "threads" / "wk-link.sock")


def test_lane_b_spawn_records_the_child_pid_the_restart_sweep_asserts(
    lane_b_home, monkeypatch
) -> None:
    """The row carries the keeper's CHILD pid out of the Identify reply: the
    daemon's registry-side keeper sweep asserts it unchanged across a restart,
    so a respawn wearing this row's name fails instead of recovering."""
    _fake_keeper(monkeypatch, lane_b_home)
    _lane_b_thread_spawn(name="wk-pi", harness="pi", cwd=lane_b_home)
    row = load_registry()[0]
    assert row.keeper_child_pid == 555, "the child pid rides the row, not just the receipt"


def test_lane_b_spawn_refuses_a_name_collision(lane_b_home, monkeypatch) -> None:
    """A second spawn under the same name is refused before any keeper
    launch - the collision check runs inside the per-agent flock."""
    _fake_keeper(monkeypatch, lane_b_home)
    _lane_b_thread_spawn(name="wk-pi", harness="pi", cwd=lane_b_home)
    recorded = _fake_keeper(monkeypatch, lane_b_home)
    with pytest.raises(DispatchAskError) as exc_info:
        _lane_b_thread_spawn(name="wk-pi", harness="pi", cwd=lane_b_home)
    assert exc_info.value.exit_code == 2
    assert "already exists" in str(exc_info.value)
    assert "argv" not in recorded
    assert len(load_registry()) == 1


def test_lane_b_spawn_without_worker_binary_exits_13(lane_b_home, monkeypatch) -> None:
    """No fno-agents runtime -> the same exit-13 shape as the codex lane."""
    monkeypatch.setattr(dispatch_mod, "_lane_b_worker_binary", lambda: None)
    with pytest.raises(DispatchAskError) as exc_info:
        _lane_b_thread_spawn(name="wk-nobin", harness="pi", cwd=lane_b_home)
    assert exc_info.value.exit_code == 13
    assert "--substrate pane" in str(exc_info.value)


def test_lane_b_spawn_kills_the_keeper_when_identify_disagrees(
    lane_b_home, monkeypatch
) -> None:
    """A keeper answering a session id other than the minted one is killed,
    never registered: the row must not point at a session fno did not mint.
    A LIVE keeper answering wrong is an identity bug; a keeper that already
    EXITED means our spawn never bound and the socket belongs to someone
    else - the error names the collision, not the identity."""
    recorded = _fake_keeper(monkeypatch, lane_b_home)

    class _LiveProc:
        pid = 4242

        def poll(self) -> None:
            return None  # alive: the answering keeper is ours

        def kill(self) -> None:
            recorded["killed"] = True

    def _wrong_identify(sock, timeout_sec=10.0):  # noqa: ANN001
        return {"v": 1, "keeper_pid": 4242, "child_pid": 555, "session_id": "other"}

    monkeypatch.setattr(dispatch_mod.subprocess, "Popen", lambda *a, **k: _LiveProc())
    monkeypatch.setattr(dispatch_mod, "_keeper_identify", _wrong_identify)
    with pytest.raises(DispatchAskError) as exc_info:
        _lane_b_thread_spawn(name="wk-wrong", harness="pi", cwd=lane_b_home)
    assert "expected the minted" in str(exc_info.value)
    assert recorded.get("killed") is True
    assert not list(load_registry())

    class _DeadProc(_LiveProc):
        def poll(self) -> int:
            return 2  # exited at the bind refusal: the socket is not ours

    recorded.clear()
    monkeypatch.setattr(dispatch_mod.subprocess, "Popen", lambda *a, **k: _DeadProc())
    with pytest.raises(DispatchAskError) as exc_info:
        _lane_b_thread_spawn(name="wk-collide", harness="pi", cwd=lane_b_home)
    assert "another keeper still holds" in str(exc_info.value)
    assert not list(load_registry())


def test_lane_b_spawn_wraps_a_registry_read_failure(lane_b_home, monkeypatch) -> None:
    """A corrupt registry surfaces as the exit-12 envelope every sibling
    spawn path uses, never a raw traceback."""

    def _boom():
        raise dispatch_mod.RegistryVersionError("schema ahead")

    _fake_keeper(monkeypatch, lane_b_home)
    monkeypatch.setattr(dispatch_mod, "load_registry", _boom)
    with pytest.raises(DispatchAskError) as exc_info:
        _lane_b_thread_spawn(name="wk-badreg", harness="pi", cwd=lane_b_home)
    assert exc_info.value.exit_code == 12
    assert "registry read failed" in str(exc_info.value)


# ---------------------------------------------------------------------------
# AC1-HP journey: the real keeper, a stub provider, no daemon anywhere
# ---------------------------------------------------------------------------


def _test_worker_bin() -> Path | None:
    """Prefer the worktree's cargo-built worker: the journey must run the
    code under test, not whatever deployed fno-agents happens to sit on
    PATH (a deployed binary can predate the --keeper lane)."""
    repo = Path(__file__).resolve().parents[3]
    for profile in ("debug", "release"):
        built = repo / "crates" / "fno-agents" / "target" / profile / "fno-agents-worker"
        if built.is_file() and os.access(built, os.X_OK):
            return built
    return dispatch_mod._lane_b_worker_binary()


_worker_bin = _test_worker_bin()


@pytest.mark.skipif(_worker_bin is None, reason="no fno-agents-worker binary resolvable")
def test_lane_b_journey_real_keeper_hosts_the_thread(lane_b_home, monkeypatch) -> None:
    """End to end: a real keeper hosts a stub pi carrying the minted
    --session-id, answers Identify with it, and its parent is the spawning
    process - never fno-agents-daemon and never the mux server."""
    # A keeper socket must fit AF_UNIX's 104-byte sun_path, and the pytest
    # basetemp does not: rewrite the isolated state root to a short tmp dir
    # (use_tmpdir's docstring invites overwriting the settings file).
    settings = lane_b_home / ".fno" / "settings.yaml"
    short_state = Path(
        tempfile.mkdtemp(prefix="fno-laneb-")
    )  # noqa: PTH103 - lifetime is this one journey test
    settings.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: {short_state}/\n",
        encoding="utf-8",
    )
    bin_dir = lane_b_home / "bin"
    bin_dir.mkdir(exist_ok=True)
    stub = bin_dir / "pi"
    stub.write_text("#!/bin/sh\nexec /usr/bin/tee /dev/null\n", encoding="utf-8")
    stub.chmod(0o755)
    # Prepend, never replace: ps/pgrep below still need the system PATH.
    monkeypatch.setenv("PATH", str(bin_dir) + os.pathsep + os.environ["PATH"])
    monkeypatch.setenv("FNO_AGENTS_WORKER_BIN", str(_worker_bin))

    receipt = _lane_b_thread_spawn(name="wk-journey", harness="pi", cwd=lane_b_home)
    keeper_pid = int(receipt["keeper_pid"])
    child_pid = int(receipt["child_pid"])

    def _ps_field(field: str, pid: int) -> str:
        out = subprocess.run(
            ["ps", "-o", f"{field}=", "-p", str(pid)],
            capture_output=True,
            text=True,
        )
        return out.stdout.strip()

    try:
        # The keeper's IDENTITY is already proven the strong way: the
        # Identify round trip in the receipt answered the minted session id,
        # and only fno-agents-worker speaks that protocol. A ps name check
        # adds nothing - Linux truncates both comm (15 chars) and args (80
        # cols headless) - so it is deliberately absent.
        keeper_ppid = _ps_field("ppid", keeper_pid)
        daemon_pids = {
            line.split()[0]
            for line in subprocess.run(
                ["pgrep", "-f", "fno-agents-daemon"], capture_output=True, text=True
            ).stdout.splitlines()
            if line.strip()
        }
        assert keeper_ppid not in daemon_pids, (
            f"the keeper's parent {keeper_ppid} must not be the daemon"
        )
        assert _ps_field("ppid", child_pid) == str(keeper_pid), (
            "the harness child is parented by the keeper"
        )
        # The row's socket is live: a fresh Identify over the ROW's field
        # answers the same minted session id.
        again = dispatch_mod._keeper_identify(Path(receipt["keeper_socket"]))
        assert again["session_id"] == receipt["session_id"]
        assert Path(receipt["keeper_socket"]).exists()
    finally:
        subprocess.run(["kill", str(keeper_pid)], capture_output=True)

    row = next(e for e in load_registry() if e.name == "wk-journey")
    assert row.messaging_socket_path == receipt["keeper_socket"]
    shutil.rmtree(short_state, ignore_errors=True)


# ---------------------------------------------------------------------------
# The stop path (PR 1332 review finding): a keeper row stops over its OWN
# socket, never worker_sock("")
# ---------------------------------------------------------------------------


def _spawn_fake_keeper(sock: Path, seen: dict, *, honor_kill: bool) -> threading.Thread:
    """A keeper stand-in with the real keeper's CONNECTION shape: every
    connection is served on its own thread and every frame read from it, so a
    liveness probe consuming one connection never eats another's Kill frame.
    honor_kill=True models the Kill contract (unlink, stop serving);
    honor_kill=False models a keeper that swallows Kill and stays up."""

    stop = threading.Event()

    def _serve_conn(conn: socket.socket) -> None:
        try:
            conn.settimeout(5)
            buf = b""
            while len(buf) < 5:
                chunk = conn.recv(5 - len(buf))
                if not chunk:
                    return
                buf += chunk
            seen.setdefault("frames", []).append(buf)
            if honor_kill and buf[0] == 3:
                seen["kill_frame"] = buf
                stop.set()
        except OSError:
            pass
        finally:
            try:
                conn.close()
            except OSError:
                pass

    def _serve() -> None:
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(str(sock))
        server.listen(8)
        server.settimeout(0.1)
        seen["ready"] = True
        try:
            while not stop.is_set():
                try:
                    conn, _ = server.accept()
                except TimeoutError:
                    continue
                except OSError:
                    break
                threading.Thread(target=_serve_conn, args=(conn,), daemon=True).start()
        finally:
            sock.unlink(missing_ok=True)
            server.close()

    thread = threading.Thread(target=_serve, daemon=True)
    thread.start()
    return thread, stop


def _keeper_row(name: str, sock: Path, tmp: Path) -> None:
    from fno.agents.registry import AgentEntry, update_registry

    entry = AgentEntry(
        name=name,
        cwd=str(tmp),
        log_path=str(tmp / "keeper.log"),
        harness="pi",
        host_mode="interactive",
        harness_session_id="sess-keeper-stop",
        pid=4242,
        keeper_child_pid=555,
        messaging_socket_path=str(sock),
        origin="spawn",
    )
    update_registry(lambda entries: entries + [entry])


def test_stop_agent_kills_a_keeper_row_over_its_own_socket(lane_b_home) -> None:
    """The verb-level contract: stop sends the Kill frame down the ROW's
    socket, confirms it unreachable, and only then stamps the row Exited."""
    from fno.agents.dispatch import stop_agent

    # A keeper socket must fit AF_UNIX's 104-byte sun_path and the pytest
    # basetemp does not (the same rewrite the journey test below makes).
    short_state = Path(tempfile.mkdtemp(prefix="fno-laneb-"))
    sock = short_state / "mux" / "threads" / "wk-stoppy.sock"
    sock.parent.mkdir(parents=True, exist_ok=True)
    seen: dict[str, object] = {}
    thread, _stop = _spawn_fake_keeper(sock, seen, honor_kill=True)
    deadline = time.monotonic() + 5
    while "ready" not in seen and time.monotonic() < deadline:
        time.sleep(0.02)

    _keeper_row("wk-stoppy", sock, lane_b_home)
    result = stop_agent("wk-stoppy")
    thread.join(timeout=5)

    assert result.name == "wk-stoppy"
    assert seen.get("kill_frame") == b"\x03" + (0).to_bytes(4, "little"), seen.get(
        "frames"
    )
    row = next(e for e in load_registry() if e.name == "wk-stoppy")
    assert row.status == "exited", "the row goes terminal only after confirmation"
    assert row.exited_at
    assert not sock.exists()
    shutil.rmtree(short_state, ignore_errors=True)


def test_stop_agent_refuses_a_keeper_that_never_confirms(lane_b_home) -> None:
    """A keeper that swallows the Kill frame leaves the row non-terminal and
    the verb raises: reporting a stop over a live keeper is the zombie shape."""
    from fno.agents.dispatch import DispatchAskError, _stop_keeper_thread
    from fno.agents.registry import AgentEntry, load_registry, update_registry

    short_state = Path(tempfile.mkdtemp(prefix="fno-laneb-"))
    sock = short_state / "mux" / "threads" / "wk-stubborn.sock"
    sock.parent.mkdir(parents=True, exist_ok=True)
    seen: dict[str, object] = {}
    thread, stop = _spawn_fake_keeper(sock, seen, honor_kill=False)
    deadline = time.monotonic() + 5
    while "ready" not in seen and time.monotonic() < deadline:
        time.sleep(0.02)

    entry = AgentEntry(
        name="wk-stubborn",
        cwd=str(lane_b_home),
        log_path=str(lane_b_home / "keeper.log"),
        harness="pi",
        host_mode="interactive",
        harness_session_id="sess-stubborn",
        pid=4242,
        keeper_child_pid=555,
        messaging_socket_path=str(sock),
        origin="spawn",
    )
    update_registry(lambda entries: entries + [entry])

    try:
        with pytest.raises(DispatchAskError) as exc_info:
            _stop_keeper_thread("wk-stubborn", entry, str(sock), grace_s=0.5)
        assert exc_info.value.exit_code == 1
        assert "did not confirm shutdown" in str(exc_info.value)
        row = next(e for e in load_registry() if e.name == "wk-stubborn")
        assert row.status == "live", "a refused stop must not stamp the row terminal"
    finally:
        stop.set()
        thread.join(timeout=5)
        shutil.rmtree(short_state, ignore_errors=True)


def test_stop_agent_stops_a_keeper_that_dies_between_probe_and_kill(lane_b_home) -> None:
    """A keeper that answers the liveness probe and then dies (the SIGKILLed
    mid-stop shape) is a clean stop, not an error: the unreachability poll
    confirms the socket is gone before the row goes terminal."""
    from fno.agents.dispatch import _stop_keeper_thread
    from fno.agents.registry import AgentEntry, load_registry, update_registry

    short_state = Path(tempfile.mkdtemp(prefix="fno-laneb-", dir="/tmp"))
    sock = short_state / "mux" / "threads" / "wk-vanish.sock"
    sock.parent.mkdir(parents=True, exist_ok=True)
    seen: dict[str, object] = {}

    # The vanishing keeper: serve exactly one connection (the probe), then die
    # WITHOUT unlinking the socket path, the shape a SIGKILLed keeper leaves.
    # Closing a listening AF_UNIX socket never unlinks its bound path.
    def _serve() -> None:
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            server.bind(str(sock))
        except OSError as exc:
            seen["error"] = repr(exc)
            return
        server.listen(8)
        server.settimeout(5)
        seen["ready"] = True
        try:
            conn, _ = server.accept()
            conn.close()
        except OSError:
            pass
        finally:
            server.close()

    thread = threading.Thread(target=_serve, daemon=True)
    thread.start()
    deadline = time.monotonic() + 5
    while "ready" not in seen and "error" not in seen and time.monotonic() < deadline:
        time.sleep(0.02)
    assert "ready" in seen, f"the keeper never bound: {seen.get('error')}"

    entry = AgentEntry(
        name="wk-vanish",
        cwd=str(lane_b_home),
        log_path=str(lane_b_home / "keeper.log"),
        harness="pi",
        host_mode="interactive",
        harness_session_id="sess-vanish",
        pid=4242,
        keeper_child_pid=555,
        messaging_socket_path=str(sock),
        origin="spawn",
    )
    update_registry(lambda entries: entries + [entry])

    try:
        result = _stop_keeper_thread("wk-vanish", entry, str(sock), grace_s=5)
        assert result.name == "wk-vanish"
        row = next(e for e in load_registry() if e.name == "wk-vanish")
        assert row.status == "exited", "a keeper that died mid-stop is gone, not refused"
        assert row.exited_at
        assert not sock.exists(), "the stale socket file is reaped on the clean stop"
    finally:
        thread.join(timeout=5)
        shutil.rmtree(short_state, ignore_errors=True)


def test_stop_agent_routes_a_cursor_thread_through_the_keeper_kill(
    lane_b_home, monkeypatch
) -> None:
    """A keeper-hosted cursor-agent thread row must take the Kill-frame arm.

    The pane arm finds no mux pane on a thread row: it would mark the row
    orphaned while the keeper and the hosted TUI still run. Routing through
    _stop_keeper_thread is the only stop that reaches the child, with the
    worker-server census reaped after the teardown."""
    from fno.agents.harnesses import cursor_agent as cursor_agent_mod
    from fno.agents.registry import AgentEntry, load_registry, update_registry

    # A SHORT bind root: AF_UNIX paths cap at ~104 bytes, and the default
    # pytest tmpdir can exceed it, which would make bind fail and this test
    # pass vacuously. The test asserts the bind and the accepted frames, so
    # neither failure mode is silent.
    short_state = Path(tempfile.mkdtemp(prefix="fno-laneb-cursor-", dir="/tmp"))
    sock = short_state / "mux" / "threads" / "wk-cursor-stop.sock"
    sock.parent.mkdir(parents=True, exist_ok=True)
    seen: dict[str, object] = {}

    # The keeper serves the liveness probe, then reads the Kill frame off a
    # second connection before exiting - the real protocol, asserted.
    def _serve() -> None:
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(str(sock))
        server.listen(8)
        server.settimeout(5)
        seen["ready"] = True
        try:
            conn, _ = server.accept()
            conn.close()
            conn2, _ = server.accept()
            seen["accepted"] = True
            frame = b""
            while len(frame) < 5:
                chunk = conn2.recv(5 - len(frame))
                if not chunk:
                    break
                frame += chunk
            seen["kill_frame"] = frame
            conn2.close()
        except OSError as exc:
            seen["error"] = repr(exc)
        finally:
            server.close()

    thread = threading.Thread(target=_serve, daemon=True)
    thread.start()
    deadline = time.monotonic() + 5
    while "ready" not in seen and time.monotonic() < deadline:
        time.sleep(0.02)
    assert "ready" in seen, "the keeper never bound; the test cannot run"
    assert "error" not in seen, f"keeper socket error: {seen.get('error')}"

    entry = AgentEntry(
        name="wk-cursor-stop",
        cwd=str(lane_b_home),
        log_path=str(lane_b_home / "keeper.log"),
        harness="cursor-agent",
        host_mode="interactive",
        harness_session_id="fadad56b-8008-45f5-b809-f9fab7074534",
        pid=4242,
        keeper_child_pid=555,
        messaging_socket_path=str(sock),
        origin="spawn",
    )
    update_registry(lambda entries: entries + [entry])

    reap_sizes: list[int] = []

    def _fake_reap(handles):
        reap_sizes.append(len(list(handles)))
        return 1

    monkeypatch.setattr(
        cursor_agent_mod, "capture_detached_worker_servers", lambda owner_pid, owner_pid_start_time: ()
    )
    monkeypatch.setattr(cursor_agent_mod, "reap_detached_worker_servers", _fake_reap)

    try:
        result = dispatch_mod.stop_agent("wk-cursor-stop")
        assert result.name == "wk-cursor-stop"
        assert seen.get("accepted"), "no Kill connection ever reached the keeper"
        assert seen.get("kill_frame") == b"\x03" + (0).to_bytes(
            4, "little"
        ), f"wrong frame: {seen.get('kill_frame')!r}"
        row = next(e for e in load_registry() if e.name == "wk-cursor-stop")
        assert row.status == "exited", "the Kill arm went terminal, not the pane arm's orphaned"
        assert reap_sizes == [0], "the census runs against the live keeper, then reaps after"
        assert not sock.exists(), "the stale socket file is reaped on the clean stop"
    finally:
        thread.join(timeout=5)
        shutil.rmtree(short_state, ignore_errors=True)


# ---------------------------------------------------------------------------
# cursor-agent: the callee-minted keeper harness
# ---------------------------------------------------------------------------

def test_lane_b_cursor_agent_mints_through_create_chat(
    lane_b_home, monkeypatch
) -> None:
    """The row's id is CALLEE-minted: create-chat returns it before launch,
    and it is never fno's uuid4."""
    _fake_keeper(monkeypatch, lane_b_home)
    minted = "fadad56b-8008-45f5-b809-f9fab7074534"

    from fno.agents.harnesses import cursor_agent

    monkeypatch.setattr(
        cursor_agent, "create_chat", lambda cwd: minted
    )
    receipt = _lane_b_thread_spawn(
        name="wk-cursor", harness="cursor-agent", cwd=lane_b_home
    )
    assert receipt["session_id"] == minted
    row = next(e for e in load_registry() if e.name == "wk-cursor")
    assert row.harness == "cursor-agent"
    assert row.harness_session_id == minted


def test_lane_b_cursor_agent_keeper_argv_carries_trust_and_grant(
    lane_b_home, monkeypatch
) -> None:
    """The keeper tail is the declared form (--trust rides it) plus the
    state-root grant the row's argv-add-dir cell declares - never a worktree
    flag."""
    recorded = _fake_keeper(monkeypatch, lane_b_home)
    minted = "0f9e63ed-861d-4f9f-8efa-3e40c5e01266"

    from fno.agents import dispatch as d

    monkeypatch.setattr(
        d, "_mint_thread_session_id", lambda harness, cwd, requested=None: minted
    )
    _lane_b_thread_spawn(
        name="wk-cursor-argv", harness="cursor-agent", cwd=lane_b_home
    )
    argv = list(recorded["argv"])  # type: ignore[arg-type]
    tail = argv[argv.index("--") + 1 :]
    assert tail[:3] == ["cursor-agent", "--resume", minted]
    assert tail.count("--trust") == 1, "the declared form carries it once, never duplicated"
    assert "--add-dir" in tail, "the computed state-root grant rides the keeper argv"
    assert not any(t in {"-w", "--worktree", "--worktree-base"} for t in tail)


def test_lane_b_cursor_agent_refuses_a_truncated_resume_id(lane_b_home) -> None:
    """spawn --resume with a head-8 handle: the pinned refusal names the
    condition, because the wrong value has an obvious source."""
    with pytest.raises(DispatchAskError) as caught:
        _mint_thread_session_id("cursor-agent", lane_b_home, requested="74db359a")
    message = str(caught.value)
    assert "74db359a" in message
    assert "8 hex characters" in message
    assert "an fno session handle, not a chat id" in message


# ---------------------------------------------------------------------------
# agy: the second callee-minted keeper harness
# ---------------------------------------------------------------------------

def test_lane_b_agy_mints_through_a_print_mode_turn(lane_b_home, monkeypatch) -> None:
    """agy's id is CALLEE-minted: the print-mode envelope returns it before
    launch, and it is never fno's uuid4."""
    _fake_keeper(monkeypatch, lane_b_home)
    minted = "c5661b28-bcba-4690-8b2e-4a4a88541e8c"

    from fno.agents.harnesses import agy

    monkeypatch.setattr(agy, "create_conversation", lambda cwd, **kw: minted)
    receipt = _lane_b_thread_spawn(name="wk-agy", harness="agy", cwd=lane_b_home)

    assert receipt["session_id"] == minted
    row = next(e for e in load_registry() if e.name == "wk-agy")
    assert row.harness == "agy"
    assert row.harness_session_id == minted


def test_lane_b_agy_keeper_argv_matches_the_pane_lane_completion(
    lane_b_home, monkeypatch
) -> None:
    """The keeper tail is the declared `--conversation` form plus the axes
    build_pane_argv's agy arm appends. The two lanes host the same TUI, so a
    disagreement here is a worker that launches differently depending on which
    substrate asked for it."""
    recorded = _fake_keeper(monkeypatch, lane_b_home)
    minted = "3b0a7e21-4c55-4f0e-9a2c-8de1f4a90b77"

    from fno.agents import dispatch as d

    monkeypatch.setattr(
        d, "_mint_thread_session_id", lambda harness, cwd, requested=None: minted
    )
    _lane_b_thread_spawn(
        name="wk-agy-argv",
        harness="agy",
        cwd=lane_b_home,
        model="gemini-3-pro",
        effort="high",
    )
    argv = list(recorded["argv"])  # type: ignore[arg-type]
    tail = argv[argv.index("--") + 1 :]
    assert tail[:3] == ["agy", "--conversation", minted]
    # An unattended keeper has nobody to answer a tool approval, so the bypass
    # is not optional here.
    assert "--dangerously-skip-permissions" in tail
    assert tail[tail.index("--effort") + 1] == "high"
    assert tail[tail.index("--model") + 1] == "gemini-3-pro"
    assert "--add-dir" in tail, "the computed state-root grant rides the keeper argv"
    assert "-p" not in tail and "--print" not in tail, (
        "print mode exits after one turn; a keeper thread must host the TUI"
    )
    assert argv[argv.index("--pane-key") + 1] == minted


def test_lane_b_agy_trusts_the_cwd_before_the_keeper_launches(
    lane_b_home, monkeypatch
) -> None:
    """A folder agy does not trust puts a modal in front of the composer, and
    the keeper has nobody to answer it. The upsert runs on the launch path, not
    only inside the mint, so a caller-supplied resume id gets it too."""
    _fake_keeper(monkeypatch, lane_b_home)
    trusted: list = []

    from fno.agents import mux_spawn

    monkeypatch.setattr(
        mux_spawn, "_ensure_agy_folder_trusted", lambda cwd: trusted.append(cwd) or True
    )
    _lane_b_thread_spawn(
        name="wk-agy-trust",
        harness="agy",
        cwd=lane_b_home,
        resume_session_id="8e2c0b41-19a7-4c3d-b0f5-6d7e2a3b9c10",
    )
    assert lane_b_home in trusted


def test_lane_b_agy_refuses_a_truncated_resume_id(lane_b_home) -> None:
    """agy resumes by EXACT id, so a partial one silently opens a different
    conversation under a name fno believes is occupied."""
    with pytest.raises(DispatchAskError) as caught:
        _mint_thread_session_id("agy", lane_b_home, requested="c5661b28")
    assert "c5661b28" in str(caught.value)
    assert "not a full UUID" in str(caught.value)


def test_agy_is_a_keeper_lane_harness() -> None:
    """The row's resume forms make agy a keeper-lane harness (interactive
    attach unsupported, resume supported), the lane pi, grok and cursor-agent
    resolve onto."""
    assert thread_lane("agy") == "keeper"


# ---------------------------------------------------------------------------
# The seed submit's modal answer must not desync the frame decoder
# ---------------------------------------------------------------------------

def _serve_frames(sock_path: Path, chunks: list[bytes]) -> threading.Thread:
    """A fake keeper: accept one connection, write ``chunks`` in order, record
    every Input frame the client sends, and ECHO each one back the way a real
    TUI repaints a submitted line - that repaint is the seed's landing proof.
    """
    received: list[bytes] = []
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(sock_path))
    server.listen(1)

    def _run() -> None:
        conn, _ = server.accept()
        try:
            for chunk in chunks:
                conn.sendall(chunk)
                time.sleep(0.15)
            deadline = time.monotonic() + 10
            conn.settimeout(0.3)
            pending = bytearray()
            while time.monotonic() < deadline:
                try:
                    data = conn.recv(65536)
                except socket.timeout:
                    continue
                if not data:
                    break
                received.append(data)
                pending.extend(data)
                while len(pending) >= 5:
                    length = int.from_bytes(pending[1:5], "little")
                    if len(pending) < 5 + length:
                        break
                    payload = bytes(pending[5 : 5 + length])
                    del pending[: 5 + length]
                    if payload != b"\r":
                        conn.sendall(_frame(1, payload))
        finally:
            conn.close()
            server.close()

    thread = threading.Thread(target=_run, daemon=True)
    thread.start()
    thread.received = received  # type: ignore[attr-defined]
    return thread


def _frame(tag: int, payload: bytes) -> bytes:
    return bytes([tag]) + len(payload).to_bytes(4, "little") + payload


def test_answering_a_modal_keeps_the_frame_decoder_in_sync() -> None:
    """The modal answer must not drop either buffer.

    `raw_pending` can hold the HEAD of a partial frame. Clearing it makes the
    next recv parse a tag and length out of mid-payload, and one garbage length
    over the 1 MiB cap breaks the decode loop for good - a wedge that reads
    exactly like a TUI that never painted its composer. `text` matters for a
    quieter reason: agy repaints the composer right behind the trust dialog, so
    the marker can arrive in the SAME recv as the modal.

    The stream here is split mid-frame on purpose: the modal frame's payload is
    delivered in two writes, and the composer marker rides the second.
    """
    from fno.agents.dispatch import _keeper_seed_submit

    # AF_UNIX caps sun_path at 104 bytes and the pytest basetemp does not fit,
    # the same short-path move the keeper spawn itself makes.
    short = Path(tempfile.mkdtemp(prefix="fnok-"))
    sock_path = short / "k.sock"
    modal = _frame(1, b"Do you trust the contents of this project?\n")
    ready = _frame(1, b"? for shortcuts")
    # The split point is the whole point: the modal frame arrives COMPLETE (so
    # the regex matches and the answer fires) carrying the first three bytes of
    # the NEXT frame behind it. Those three bytes are what `raw_pending` holds
    # when the answer fires, and dropping them makes the marker's tail parse as
    # a header. A split inside the modal frame instead would leave the buffer
    # empty at that moment and the bug would be a no-op - which is how this
    # test first passed against the defect it exists to catch.
    server = _serve_frames(sock_path, [modal + ready[:3], ready[3:]])

    _keeper_seed_submit(
        name="wk-modal",
        session_id="7a64a981-b33c-4e94-861f-bb66a13c9f01",
        sock=sock_path,
        message="hello",
        ready_marker=b"? for shortcuts",
        clear_modal=(r"trust (?:this )?folder|do you trust", b"\r"),
    )
    server.join(timeout=10)

    shutil.rmtree(short, ignore_errors=True)
    sent = b"".join(server.received)  # type: ignore[attr-defined]
    # The modal answer AND the seed both landed: a decoder that desynced on the
    # partial frame would never have seen the marker and would have raised.
    assert sent.count(_frame(1, b"\r")) >= 1, "the modal was never answered"
    assert b"hello" in sent, "the seed never reached the composer"


def test_a_callee_minted_row_with_no_mint_is_refused_not_fabricated(monkeypatch) -> None:
    """The fallback for a row fno has no mint for is its own UUIDv4, and a
    callee-minted harness never adopts one: the keeper would launch on an id
    the harness does not know, and Identify would read that fabricated id back
    off the argv and report it as truth. grok stands in for the next row to
    declare the strategy before its mint is written."""
    import fno.agents.harness_map as harness_map
    from fno.agents.keeper_thread import mint_session_id

    real = harness_map.capabilities
    monkeypatch.setattr(
        harness_map,
        "capabilities",
        lambda h: {**real(h), "session_binding": {"strategy": "callee-minted-read-back"}},
    )
    with pytest.raises(DispatchAskError) as caught:
        mint_session_id("grok", Path("/tmp"), None)
    assert "callee-minted-read-back" in str(caught.value)
    assert "never adopts" in str(caught.value)
    # The measured rows are unaffected: grok really declares caller-assigned.
    monkeypatch.undo()
    assert mint_session_id("grok", Path("/tmp"), "abc") == "abc"


def test_a_modal_arriving_with_the_marker_is_still_answered() -> None:
    """One repaint can deliver the composer paint and the dialog over it.

    The marker is then already in the buffer when the modal matches, so a loop
    that tests the marker FIRST breaks out with the dialog unanswered. The seed
    goes to a TUI showing the modal: its submit CR answers the dialog, the
    payload is swallowed, and the spawn returns a live registry row with no
    orders. The modal check runs first for exactly this stream.
    """
    from fno.agents.dispatch import _keeper_seed_submit

    short = Path(tempfile.mkdtemp(prefix="fnok-"))
    sock_path = short / "k.sock"
    modal = _frame(1, b"Do you trust the contents of this project?\n")
    ready = _frame(1, b"? for shortcuts")
    # ONE chunk, both complete: the marker is matchable the first time the loop
    # looks, which is what makes the ordering load-bearing.
    server = _serve_frames(sock_path, [ready + modal])

    _keeper_seed_submit(
        name="wk-modal-same-recv",
        session_id="0d1a5c33-6b90-4d02-9f77-2a1c5e88b410",
        sock=sock_path,
        message="hello",
        ready_marker=b"? for shortcuts",
        clear_modal=(r"trust (?:this )?folder|do you trust", b"\r"),
    )
    server.join(timeout=10)

    shutil.rmtree(short, ignore_errors=True)
    sent = b"".join(server.received)  # type: ignore[attr-defined]
    answer = sent.index(_frame(1, b"\r"))
    assert b"hello" in sent, "the seed never reached the composer"
    assert answer < sent.index(b"hello"), "the seed was pasted before the modal was answered"
