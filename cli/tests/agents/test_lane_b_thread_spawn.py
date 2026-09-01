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
import subprocess
import tempfile
from pathlib import Path

import pytest

from fno.agents import dispatch as dispatch_mod
from fno.agents.dispatch import DispatchAskError, _lane_b_thread_spawn
from fno.agents.harness_map import (
    DispatchResolveError,
    capabilities,
    render_session_argv,
    resolve_dispatch,
    thread_lane,
)
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

def test_pi_thread_dispatch_still_refuses_at_the_gate(lane_b_home) -> None:
    """pi's `thread` capability row stays false, so resolve_dispatch refuses
    the thread substrate - the lane built here is not reachable through the
    public dispatch surface."""
    assert capabilities("pi")["thread"] is False
    with pytest.raises(DispatchResolveError) as exc_info:
        resolve_dispatch(harness="pi", substrate="thread")
    assert "substrate 'thread' is unsupported on harness 'pi'" in str(exc_info.value)


def test_lane_b_spawn_is_not_wired_into_dispatch_spawn() -> None:
    """The explicit internal entry point is the ONLY caller surface: the
    public dispatch_spawn body does not reference it."""
    source = inspect.getsource(dispatch_mod.dispatch_spawn)
    assert "_lane_b_thread_spawn" not in source


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
    assert worker_tail == render_session_argv("pi", "interactive_create", session_id), (
        "the provider argv is the contract's render, never hand-assembled"
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
    assert row.origin == "spawn"


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
