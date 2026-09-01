"""The keeper lane's tests: a pure verdict table, seam-level discovery, and
three planted-keeper controls.

The controls plant a REAL ``fno-agents-worker --pane`` over a trivial child
(the exact shape of the seven orphans measured 2026-09-01) and assert the
planted PID BY NUMBER. A count is not a marker: "1 keeper was found" passes
against a different keeper, and a development machine can carry live orphans
of its own. Every planted process is cleaned up in a finally block; a test
that leaks a keeper is the leak this lane exists to collect.
"""
from __future__ import annotations

import os
import shutil
import socket
import subprocess
import tempfile
import time
from pathlib import Path

import pytest

from fno.agents import keeper_lane as kl
from fno.agents.orphans import REAP_MIN_AGE_S


# --- fixtures for the pure table --------------------------------------------


def _obs(**overrides) -> kl.KeeperObs:
    """One reapable-by-construction observation; each test overrides the arm
    it is about, so a probe cannot satisfy another arm's assertion."""
    base = dict(
        pid=4001,
        lane="pane",
        sock=Path("/tmp/fk.test/mux/panes/fk-1-1.sock"),
        session="fk-1",
        cwd="/repo/proj",
        age_s=REAP_MIN_AGE_S + 1,
        child_pids=(4002,),
        sock_state=kl.NO_LISTENER,
        claimed_by=None,
    )
    base.update(overrides)
    return kl.KeeperObs(**base)


def test_reapable_when_no_listener_unclaimed_and_old() -> None:
    verdict, _ = kl.keeper_verdict(_obs())
    assert verdict == kl.REAP


def test_absent_socket_reaps_the_measured_shape() -> None:
    """The measured orphans have zero sockets on disk: their servers cleaned
    the temp dirs and left the children. A socket-walk discovery would never
    have produced this candidate at all."""
    verdict, reason = kl.keeper_verdict(_obs(sock_state=kl.ABSENT))
    assert verdict == kl.REAP
    assert "absent" in reason


def test_silence_never_reaps() -> None:
    verdict, reason = kl.keeper_verdict(_obs(sock_state=kl.SILENT))
    assert verdict == kl.LEAVE
    assert "silent" in reason


def test_live_listener_never_reaps() -> None:
    verdict, reason = kl.keeper_verdict(_obs(sock_state=kl.LISTENER))
    assert verdict == kl.LEAVE
    assert "listener" in reason


def test_a_claimed_pid_is_named_and_left() -> None:
    verdict, reason = kl.keeper_verdict(_obs(claimed_by="worker-x"))
    assert verdict == kl.LEAVE
    assert "worker-x" in reason


def test_a_fresh_keeper_leaves_on_the_grace_arm() -> None:
    verdict, reason = kl.keeper_verdict(_obs(age_s=60.0))
    assert verdict == kl.LEAVE
    assert "grace" in reason


def test_no_declared_sock_is_unreadable_and_never_reaps() -> None:
    verdict, reason = kl.keeper_verdict(_obs(sock=None, sock_state=kl.UNREADABLE))
    assert verdict == kl.LEAVE
    assert "--sock" in reason


def test_an_unreadable_registry_leaves_every_keeper() -> None:
    """A failed registry read leaves ``claimed_by`` None; reading that None as
    unclaimed would reap a keeper a row may claim. The registry_ok arm is the
    fail-closed."""
    verdict, reason = kl.keeper_verdict(_obs(registry_ok=False))
    assert verdict == kl.LEAVE
    assert "registry unreadable" in reason


# --- discovery over injected rows -------------------------------------------


def _proc_row(pid: int, sock: str, *, lane_flag: str = "--pane") -> dict:
    return {
        "pid": pid,
        "ppid": 1,
        "name": "fno-agents-worker",
        "cmdline": [
            "/repo/crates/fno-agents/target/debug/fno-agents-worker",
            lane_flag, "--sock", sock, "--session", "fk-9", "--pane-key", "1",
            "--cwd", "/repo/proj", "--rows", "24", "--cols", "80",
            "--", "bash", "-c", "while IFS= read -r l; do echo GOT:$l; done",
        ],
        "uids": None,
        "create_time": time.time() - 7200,
        "cpu_times": None,
        "cwd": "/repo/proj",
    }


def test_discovery_classifies_lane_and_session(monkeypatch) -> None:
    monkeypatch.setattr(
        kl, "iter_processes", lambda *a, **k: iter([_proc_row(4100, "/tmp/x.sock")])
    )
    monkeypatch.setattr(kl, "sock_state_of", lambda sock: kl.ABSENT)
    monkeypatch.setattr(kl, "REAP_MIN_AGE_S", 0.0)
    result = kl.discover(now_s=time.time())
    assert not result.broken
    assert [o.pid for o in result.observations] == [4100]
    obs = result.observations[0]
    assert obs.lane == "pane"
    assert obs.session == "fk-9"
    assert obs.sock_state == kl.ABSENT
    assert result.verdicts[4100][0] == kl.REAP


def test_a_thread_lane_keeper_is_recognized(monkeypatch) -> None:
    monkeypatch.setattr(
        kl,
        "iter_processes",
        lambda *a, **k: iter([_proc_row(4101, "/tmp/y.sock", lane_flag="--keeper")]),
    )
    monkeypatch.setattr(kl, "sock_state_of", lambda sock: kl.NO_LISTENER)
    monkeypatch.setattr(kl, "REAP_MIN_AGE_S", 0.0)
    result = kl.discover(now_s=time.time())
    assert result.observations[0].lane == "thread"
    assert result.verdicts[4101][0] == kl.REAP


def test_a_non_keeper_process_is_invisible(monkeypatch) -> None:
    row = _proc_row(4102, "/tmp/z.sock")
    row["cmdline"][0] = "/bin/cat"
    monkeypatch.setattr(kl, "iter_processes", lambda *a, **k: iter([row]))
    result = kl.discover(now_s=time.time())
    assert result.observations == []
    assert not result.broken


def test_a_row_claiming_the_pid_protects_the_keeper(monkeypatch) -> None:
    """A keeper whose pid appears as a registry row's ``keeper_child_pid`` is
    LEAVE, and the reason names the row (the plan's own acceptance). The
    hosted-child half of the claim net is exercised by the planted control
    with REAL children below."""
    monkeypatch.setattr(
        kl, "iter_processes", lambda *a, **k: iter([_proc_row(4103, "/tmp/c.sock")])
    )
    monkeypatch.setattr(kl, "sock_state_of", lambda sock: kl.ABSENT)
    monkeypatch.setattr(kl, "REAP_MIN_AGE_S", 0.0)

    class Row:
        name = "pi-thread"
        pid = None
        keeper_child_pid = 4103  # this keeper's own pid, claimed by a row

    import fno.agents.registry as registry

    monkeypatch.setattr(registry, "load_registry", lambda path=None: [Row()])
    result = kl.discover(now_s=time.time(), registry_path=Path("/fake/registry.json"))
    assert result.observations[0].claimed_by == "pi-thread"
    assert result.verdicts[4103][0] == kl.LEAVE
    assert "pi-thread" in result.verdicts[4103][1]


def test_a_damaged_registry_breaks_the_lane_loudly(monkeypatch) -> None:
    monkeypatch.setattr(
        kl, "iter_processes", lambda *a, **k: iter([_proc_row(4105, "/tmp/d.sock")])
    )

    import fno.agents.registry as registry

    def boom(path=None):
        raise ValueError("malformed JSON")

    monkeypatch.setattr(registry, "load_registry", boom)
    result = kl.discover(now_s=time.time())
    assert result.broken
    assert "registry unreadable" in result.broken_reason
    # Even with a positive socket death, nothing is reapable while the lane
    # cannot read the claims.
    assert result.reapable == []
    assert result.verdicts[4105][0] == kl.LEAVE


def test_enumeration_failure_withholds(monkeypatch) -> None:
    def boom(*a, **k):
        raise RuntimeError("psutil exploded")

    monkeypatch.setattr(kl, "iter_processes", boom)
    result = kl.discover(now_s=time.time())
    assert result.broken
    assert "enumeration failed" in result.broken_reason


def test_broken_lane_reaps_nothing(monkeypatch) -> None:
    result = kl.KeeperLaneResult(observations=[_obs(pid=4199)], broken_reason="x")
    result.verdicts[4199] = (kl.REAP, "n")
    assert result.reapable == []
    assert kl.reap_keepers(result) == ([], [])


# --- the socket probe --------------------------------------------------------


def test_probe_states_off_the_filesystem(tmp_path) -> None:
    # macOS caps AF_UNIX paths at 104 bytes; pytest tmp dirs overflow it, so
    # the fixtures bind directly in the short POSIX /tmp.
    short = Path(tempfile.gettempdir()) / f"fk-lane-{os.getpid()}-a.sock"
    assert kl.sock_state_of(None) == kl.UNREADABLE
    assert kl.sock_state_of(short.parent / "gone.sock") == kl.ABSENT
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(str(short))
    srv.close()
    short.touch()  # the file outlives the dead bind, the listener does not
    try:
        assert kl.sock_state_of(short) == kl.NO_LISTENER
    finally:
        short.unlink(missing_ok=True)


def test_a_nonspeaking_listener_reads_silent(tmp_path) -> None:
    """A plain AF_UNIX socket that accepts and never speaks the frame protocol
    is SILENT - the wedged keeper's shape - and silence never reaps."""
    live = Path(tempfile.gettempdir()) / f"fk-lane-{os.getpid()}-b.sock"
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(str(live))
    srv.listen(1)
    try:
        assert kl.sock_state_of(live) == kl.SILENT
    finally:
        srv.close()
        live.unlink(missing_ok=True)


# --- the planted-keeper controls --------------------------------------------
#
# Three controls, one per arm, each asserting the planted pid BY NUMBER. They
# build the real keeper binary the way the pane journeys do
# (cli/tests/agents/test_spawn_pane.py) and skip when cargo is absent.


def _build_worker_bin() -> Path | None:
    cargo = shutil.which("cargo")
    if cargo is None:
        return None
    repo = Path(__file__).resolve().parents[3]
    worker_bin = repo / "crates" / "fno-agents" / "target" / "debug" / "fno-agents-worker"
    if worker_bin.is_file():
        return worker_bin
    build_env = {
        **__import__("os").environ,
        "CARGO_HOME": str(Path(cargo).parent.parent),
        "RUSTUP_HOME": str(Path(cargo).parent.parent.parent / ".rustup"),
    }
    built = subprocess.run(
        [cargo, "build", "--manifest-path",
         str(repo / "crates" / "fno-agents" / "Cargo.toml"),
         "--bin", "fno-agents-worker"],
        cwd=repo, env=build_env, text=True, capture_output=True,
    )
    if built.returncode != 0:
        pytest.skip(f"keeper binary build failed: {built.stderr[-400:]}")
    return worker_bin


def _spawn_planted_keeper(worker_bin: Path, tmp_path: Path) -> tuple[int, Path, subprocess.Popen, Path]:
    """Plant one real keeper over a `bash while-read` child, exactly the
    measured orphan shape, and wait for its Identify reply (a positive
    listener proof). Returns (pid, sock_path, popen, sock_parent).

    The socket lives in short POSIX /tmp, not the pytest tmp dir: macOS caps
    AF_UNIX paths at 104 bytes and the pytest tree overflows it. The returned
    parent is what a test deletes to produce the measured orphan shape."""
    sock_dir = Path(tempfile.gettempdir()) / f"fk-lane-{os.getpid()}" / "panes"
    sock = sock_dir / "fk-test-1.sock"
    sock_dir.mkdir(parents=True)
    proc = subprocess.Popen(
        [
            str(worker_bin), "--pane", "--sock", str(sock),
            "--session", "fk-test", "--pane-key", "1",
            "--cwd", str(tmp_path), "--rows", "24", "--cols", "80",
            "--", "bash", "-c", "while IFS= read -r l; do echo GOT:$l; done",
        ],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        stdin=subprocess.DEVNULL,
    )
    from fno.agents.dispatch import _keeper_identify

    try:
        _keeper_identify(sock, timeout_sec=15.0)  # raises unless a keeper answered
    except Exception:
        _kill_quietly(proc.pid)
        proc.wait(timeout=5)
        shutil.rmtree(sock_dir.parent, ignore_errors=True)
        raise
    return proc.pid, sock, proc, sock_dir.parent


def _kill_quietly(pid: int) -> None:
    import signal

    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass


def _planted_only_iter(pid: int):
    """Enumerate ONLY the planted pid through the real psutil: the control
    proves the lane against one specimen and can never kill a stray live
    orphan that happens to share the machine."""
    import psutil

    def _iter(*a, **k):
        proc = psutil.Process(pid)
        info = {
            "pid": proc.pid,
            "ppid": proc.ppid(),
            "name": proc.name(),
            "cmdline": proc.cmdline(),
            "uids": None,
            "create_time": proc.create_time(),
            "cpu_times": None,
            "cwd": None,
        }
        yield info

    return _iter


def _pid_alive(pid: int) -> bool:
    """Same zombie rule as the lane's ``_is_dead``: the planted keeper's
    parent (this test process) has not waited, so a group-killed keeper reads
    as killable-by-``os.kill(0)`` until the finally block waits it."""
    import psutil

    try:
        return psutil.Process(pid).status() != psutil.STATUS_ZOMBIE
    except psutil.NoSuchProcess:
        return False
    except psutil.Error:
        return True


@pytest.mark.skipif(shutil.which("cargo") is None,
                    reason="cargo not on PATH; this control drives the real keeper binary")
def test_control_orphan_named_in_dry_run_and_collected_under_apply_all(
    tmp_path, monkeypatch
) -> None:
    worker_bin = _build_worker_bin()
    if worker_bin is None:
        pytest.skip("keeper binary unavailable")
    planted_pid, sock, proc, sock_parent = _spawn_planted_keeper(worker_bin, tmp_path)
    child_pids = []
    try:
        import psutil

        child_pids = [c.pid for c in psutil.Process(planted_pid).children()]
        assert child_pids, "the planted keeper must host a live child"
        # The server-side socket owner "dies": delete the socket dir out from
        # under the live keeper, the measured orphan shape.
        shutil.rmtree(sock_parent)
        monkeypatch.setattr(kl, "iter_processes", _planted_only_iter(planted_pid))
        monkeypatch.setattr(kl, "REAP_MIN_AGE_S", 0.0)

        # DRY RUN: named, and nothing killed.
        dry = kl.discover(now_s=time.time())
        assert not dry.broken
        assert [o.pid for o in dry.reapable] == [planted_pid]
        assert _pid_alive(planted_pid)
        assert all(_pid_alive(c) for c in child_pids)

        # APPLY-ALL: exactly that pid, and exactly its hosted child, gone.
        keepers, children = kl.reap_keepers(dry)
        assert keepers == [planted_pid]
        assert children == child_pids
        assert not _pid_alive(planted_pid)
        assert all(not _pid_alive(c) for c in child_pids)
    finally:
        _kill_quietly(planted_pid)
        for c in child_pids:
            _kill_quietly(c)
        proc.wait(timeout=5)
        shutil.rmtree(sock_parent, ignore_errors=True)


@pytest.mark.skipif(shutil.which("cargo") is None,
                    reason="cargo not on PATH; this control drives the real keeper binary")
def test_control_live_listener_survives_both(tmp_path, monkeypatch) -> None:
    worker_bin = _build_worker_bin()
    if worker_bin is None:
        pytest.skip("keeper binary unavailable")
    planted_pid, sock, proc, sock_parent = _spawn_planted_keeper(worker_bin, tmp_path)
    try:
        monkeypatch.setattr(kl, "iter_processes", _planted_only_iter(planted_pid))
        monkeypatch.setattr(kl, "REAP_MIN_AGE_S", 0.0)
        result = kl.discover(now_s=time.time())
        assert not result.broken
        obs = result.observations[0]
        assert obs.sock_state == kl.LISTENER
        assert result.verdicts[planted_pid][0] == kl.LEAVE
        # Even the collector refuses: a live listener's pid survives.
        keepers, _children = kl.reap_keepers(result)
        assert keepers == []
        assert _pid_alive(planted_pid)
    finally:
        _kill_quietly(planted_pid)
        import psutil

        for c in psutil.Process(planted_pid).children():
            _kill_quietly(c.pid)
        proc.wait(timeout=5)
        shutil.rmtree(sock_parent, ignore_errors=True)


@pytest.mark.skipif(shutil.which("cargo") is None,
                    reason="cargo not on PATH; this control drives the real keeper binary")
def test_control_registry_claimed_keeper_survives_both(tmp_path, monkeypatch) -> None:
    worker_bin = _build_worker_bin()
    if worker_bin is None:
        pytest.skip("keeper binary unavailable")
    planted_pid, sock, proc, sock_parent = _spawn_planted_keeper(worker_bin, tmp_path)
    child_pids = []
    try:
        import psutil

        child_pids = [c.pid for c in psutil.Process(planted_pid).children()]
        # Kill the listener's reachability WITHOUT killing the keeper: delete
        # the socket dir so the socket arm reads positively dead, then let the
        # registry claim arm refuse.
        shutil.rmtree(sock_parent)
        monkeypatch.setattr(kl, "iter_processes", _planted_only_iter(planted_pid))
        monkeypatch.setattr(kl, "REAP_MIN_AGE_S", 0.0)

        class Row:
            name = "live-worker"
            pid = None
            keeper_child_pid = child_pids[0] if child_pids else None

        import fno.agents.registry as registry

        monkeypatch.setattr(registry, "load_registry", lambda path=None: [Row()])
        result = kl.discover(now_s=time.time())
        assert not result.broken
        assert result.observations[0].claimed_by == "live-worker"
        assert result.verdicts[planted_pid][0] == kl.LEAVE
        keepers, _children = kl.reap_keepers(result)
        assert keepers == []
        assert _pid_alive(planted_pid)
        assert all(_pid_alive(c) for c in child_pids)
    finally:
        _kill_quietly(planted_pid)
        for c in child_pids:
            _kill_quietly(c)
        proc.wait(timeout=5)
        shutil.rmtree(sock_parent, ignore_errors=True)


# --- the verb's flag contract ----------------------------------------------


def test_apply_all_kills_through_the_real_command(tmp_path, monkeypatch) -> None:
    """The flag contract, through the typer command: --apply names and never
    kills; --apply-all collects. The lane is scoped to one planted specimen
    via the enumeration seam, so no live orphan shares the run."""
    worker_bin = _build_worker_bin()
    if worker_bin is None:
        pytest.skip("keeper binary unavailable")
    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    planted_pid, sock, proc, sock_parent = _spawn_planted_keeper(worker_bin, tmp_path)
    child_pids = []
    try:
        import psutil

        child_pids = [c.pid for c in psutil.Process(planted_pid).children()]
        shutil.rmtree(sock_parent)
        monkeypatch.setattr(kl, "iter_processes", _planted_only_iter(planted_pid))
        monkeypatch.setattr(kl, "REAP_MIN_AGE_S", 0.0)
        runner = CliRunner()

        dry = runner.invoke(agents_app, ["watchdog", "--only", "keeper"])
        assert dry.exit_code == 0, dry.output
        assert str(planted_pid) in dry.output
        assert _pid_alive(planted_pid)

        apply_only = runner.invoke(
            agents_app, ["watchdog", "--only", "keeper", "--apply"]
        )
        assert apply_only.exit_code == 0, apply_only.output
        assert str(planted_pid) in apply_only.output
        assert _pid_alive(planted_pid)

        apply_all = runner.invoke(
            agents_app, ["watchdog", "--only", "keeper", "--apply-all"]
        )
        assert apply_all.exit_code == 0, apply_all.output
        assert f"reaped 1 keeper(s), {len(child_pids)} hosted child(ren)" in apply_all.output
        assert not _pid_alive(planted_pid)
        assert all(not _pid_alive(c) for c in child_pids)
    finally:
        _kill_quietly(planted_pid)
        for c in child_pids:
            _kill_quietly(c)
        proc.wait(timeout=5)
        shutil.rmtree(sock_parent, ignore_errors=True)


def test_json_payload_carries_pid_lane_verdict_reason(tmp_path, monkeypatch) -> None:
    import json as _json

    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    monkeypatch.setattr(
        kl, "iter_processes", lambda *a, **k: iter([_proc_row(4200, "/tmp/j.sock")])
    )
    monkeypatch.setattr(kl, "sock_state_of", lambda sock: kl.ABSENT)
    monkeypatch.setattr(kl, "REAP_MIN_AGE_S", 0.0)
    result = CliRunner().invoke(agents_app, ["watchdog", "--only", "keeper", "--json"])
    assert result.exit_code == 0, result.output
    payload = _json.loads(result.output)
    assert payload["lane"] == "keeper"
    row = next(k for k in payload["keepers"] if k["pid"] == 4200)
    assert row["lane"] == "pane"
    assert row["verdict"] == kl.REAP
    assert row["reason"]
