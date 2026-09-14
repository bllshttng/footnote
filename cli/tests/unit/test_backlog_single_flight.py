"""One-in-flight gate tests for `fno backlog advance` / `reconcile` (x-ef2c).

The measured shape: three concurrent `advance --epic` from three different
parents and three concurrent reconcile trees, the oldest twelve minutes, at
load 458 against a gate of 120. The contract under test: the SECOND
invocation for an in-flight scope reports `held`, runs nothing, and exits 0;
the work itself still completes; a scope never wedges shut.

Claim isolation mirrors test_advance.py: FNO_CLAIMS_ROOT / FNO_REPO_ROOT /
FNO_EVENTS_PATH pinned per test, so no gate state reaches a real store.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.backlog import advance as adv
from fno.backlog.single_flight import (
    Flight,
    acquire_flight,
    advance_flight_key,
    reconcile_flight_key,
)
from fno.claims.core import acquire_claim, claim_status
from fno.claims.io import claims_root_for
from fno.cli import app
from fno.rust_binary import find_dev_binary

runner = CliRunner()

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


@pytest.fixture
def iso(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Pin claims + repo root + events under tmp_path, and point the gate's
    binary at THIS checkout's build (see test_advance.iso)."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_AUTO_CONTINUE", raising=False)
    events_path = tmp_path / ".fno" / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(events_path))
    dev = find_dev_binary()
    if dev is None:
        pytest.skip("compiled fno-agents binary not present")
    monkeypatch.setenv("FNO_AGENTS_BIN", str(dev))
    return tmp_path


def _advance_result(decision: str = "disabled") -> SimpleNamespace:
    # notes/json_receipt/render: newer AdvanceResult contract; a dev binary
    # present (FNO_AGENTS_BIN) routes --json through json_receipt(), which
    # CI's binary-less skip never reaches.
    return SimpleNamespace(
        decision=decision,
        event="",
        reason="",
        node_id=None,
        short_id=None,
        notes=[],
        json_receipt=lambda: {"decision": decision},
        render=lambda: [decision],
    )


# ---------------------------------------------------------------------------
# AC1: the second invocation for an in-flight scope is held and runs nothing
# ---------------------------------------------------------------------------


def test_second_advance_reports_held_and_runs_nothing(iso, monkeypatch):
    """The x-ef2c VERIFY: fire the arm twice inside one run; the second
    reports held rather than starting a second run."""
    acquire_claim(advance_flight_key(None), "a-previous-run", ttl_ms=600_000)

    def _must_not_run(*_a, **_k):
        raise AssertionError("a held advance must not run the selection")

    monkeypatch.setattr(adv, "advance", _must_not_run)
    monkeypatch.setattr(adv, "advance_dependents", _must_not_run)

    result = runner.invoke(app, ["backlog", "advance", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["held"] is True
    assert payload["requests"] >= 1, "the held count is the positive marker"
    assert payload["holder"] == "a-previous-run"
    assert payload["decision"] == "held", "the --json receipt shape holds"


def test_second_epic_advance_reports_held(iso, monkeypatch):
    """Same contract for the daemon's converge entry (`--epic`), and a held
    receipt is never a retirement: the CLI exits 0 with `held: true`."""
    acquire_claim(advance_flight_key("x-epic-a"), "a-converge", ttl_ms=600_000)

    ran = []
    monkeypatch.setattr(
        "fno.backlog.advance.run_advance_epic",
        lambda *a, **k: ran.append(a),
    )

    result = runner.invoke(app, ["backlog", "advance", "--epic", "x-epic-a", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["held"] is True
    assert ran == []


def test_second_reconcile_reports_held(iso, monkeypatch):
    """The SessionStart/merge/groom arms all fire this verb; a second full
    sweep while one runs stands down with the held receipt."""
    acquire_claim(
        reconcile_flight_key(node=None, pr_number=None), "a-sweep", ttl_ms=600_000
    )

    ran = []
    monkeypatch.setattr(
        "fno.graph.cli._reconcile_once", lambda **k: ran.append(k)
    )

    result = runner.invoke(app, ["backlog", "reconcile", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["held"] is True
    assert ran == []


def test_refusal_fires_even_while_the_scope_is_held(iso, monkeypatch):
    """A bad invocation is refused before the gate: `--node` + `--pr-number`
    must still exit 2 while another sweep holds the scope."""
    acquire_claim(
        reconcile_flight_key(node=None, pr_number=None), "a-sweep", ttl_ms=600_000
    )
    result = runner.invoke(
        app, ["backlog", "reconcile", "--node", "ab-2222aaaa", "--pr-number", "7"]
    )
    assert result.exit_code == 2
    assert "held" not in result.output


# ---------------------------------------------------------------------------
# AC2: the work still completes - a held tick never drops it
# ---------------------------------------------------------------------------


def test_gate_releases_so_the_next_run_is_not_held(iso, monkeypatch):
    """Free scope: the run executes, the gate releases, and an immediate
    second run executes too (no stack, no wedge)."""
    calls = []
    monkeypatch.setattr(
        adv, "advance", lambda *a, **k: calls.append(1) or _advance_result()
    )

    first = runner.invoke(app, ["backlog", "advance", "--json"])
    second = runner.invoke(app, ["backlog", "advance", "--json"])
    assert first.exit_code == 0, first.output
    assert second.exit_code == 0, second.output
    assert json.loads(first.stdout).get("held") is None
    assert json.loads(second.stdout).get("held") is None
    assert calls == [1, 1]
    key = advance_flight_key(None)
    state = claim_status(key, root=claims_root_for(key))["state"]
    assert state == "free"


def test_gate_unavailable_fails_open(iso, monkeypatch):
    """No fno-agents binary (or one older than the verb): the gate is
    unavailable and the verb proceeds ungated, the pre-gate behavior."""
    monkeypatch.setenv("FNO_AGENTS_BIN", "/nonexistent/fno-agents")
    calls = []
    monkeypatch.setattr(
        adv, "advance", lambda *a, **k: calls.append(1) or _advance_result()
    )

    result = runner.invoke(app, ["backlog", "advance", "--json"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout).get("held") is None
    assert calls == [1], "the run went ahead without the gate"


def test_a_dead_holder_does_not_wedge_the_scope(iso):
    """A holder whose process died is reclaimed on the pid probe, not the
    TTL: the next acquirer takes the scope and runs."""
    key = advance_flight_key(None)
    child = subprocess.Popen(["true"])
    child.wait()
    acquire_claim(key, "dead-run", ttl_ms=600_000, pid=child.pid)

    gate = acquire_flight(key, scope="advance")
    assert gate is not None and not gate.held, "a dead holder must never read as held"
    gate.release()


# ---------------------------------------------------------------------------
# Scopes: two copies of the SAME scope are the defect; distinct scopes are
# distinct work over the same store and never hold each other
# ---------------------------------------------------------------------------


def test_distinct_epics_hold_distinct_gates(iso):
    first = acquire_flight(advance_flight_key("x-epic-a"), scope="advance --epic a")
    other = acquire_flight(advance_flight_key("x-epic-b"), scope="advance --epic b")
    same = acquire_flight(advance_flight_key("x-epic-a"), scope="advance --epic a")
    assert first is not None and not first.held
    assert other is not None and not other.held
    assert same is not None and same.held
    first.release()
    other.release()


def test_pr_scoped_reconcile_does_not_queue_behind_a_full_sweep(iso):
    """A merge's own closure (`--pr-number`) is distinct work: it must not
    queue behind an unrelated twelve-minute graph sweep."""
    full = acquire_flight(
        reconcile_flight_key(node=None, pr_number=None), scope="reconcile"
    )
    pr = acquire_flight(
        reconcile_flight_key(node=None, pr_number=123), scope="reconcile pr 123"
    )
    same = acquire_flight(
        reconcile_flight_key(node=None, pr_number=None), scope="reconcile"
    )
    assert full is not None and not full.held
    assert pr is not None and not pr.held
    assert same is not None and same.held
    full.release()
    pr.release()


def test_dry_run_reconcile_is_never_gated(iso, monkeypatch):
    """--dry-run mutates nothing, so it stays readable while a real sweep
    runs: the operator inspects exactly when the graph is busiest."""
    acquire_claim(
        reconcile_flight_key(node=None, pr_number=None), "a-sweep", ttl_ms=600_000
    )
    ran = []
    monkeypatch.setattr("fno.graph.cli._reconcile_once", lambda **k: ran.append(k))
    result = runner.invoke(app, ["backlog", "reconcile", "--dry-run"])
    assert result.exit_code == 0, result.output
    assert ran, "a dry run must bypass the gate entirely"


# ---------------------------------------------------------------------------
# Held-stop: --stop is a control action and never queues behind the drain
# ---------------------------------------------------------------------------


def test_epic_stop_bypasses_the_gate(iso, monkeypatch):
    acquire_claim(advance_flight_key("x-epic-a"), "a-converge", ttl_ms=600_000)
    ran = []
    monkeypatch.setattr(
        "fno.backlog.advance.run_advance_epic",
        lambda *a, **k: ran.append(k),
    )
    result = runner.invoke(app, ["backlog", "advance", "--epic", "x-epic-a", "--stop"])
    assert result.exit_code == 0, result.output
    assert ran, "deactivating a mission must never be held by its own drain"


# ---------------------------------------------------------------------------
# x-626f: a live holder carries its own budget, dumps its stack on SIGUSR1,
# and dies with an opted-in parent. The specimen shape: a LIVE holder parked
# at 0.0 pct CPU on a blocking read, which no pid probe can call dead.
# ---------------------------------------------------------------------------

_SRC = str(Path(__file__).resolve().parents[2] / "src")


def _short_sock_dir() -> Path:
    """A short-lived dir with a socket path UNDER the 104-char AF_UNIX cap
    (pytest's tmp_path is far over it on macOS)."""
    import tempfile

    return Path(tempfile.mkdtemp(prefix="x626f-"))

# A reconcile whose work blocks forever reading a unix socket that accepted
# but never replies - the AC1-HP blocking read, in ~15 lines.
_CHILD_BLOCK_ON_SOCKET = """
import socket, sys
from fno.backlog.single_flight import reconcile_gate

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sys.argv[1])
srv.listen(1)
c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
c.connect(sys.argv[1])
conn, _ = srv.accept()

def once():
    conn.recv(1)  # the silent-socket read: blocks forever

reconcile_gate(dry_run=False, node=None, json_out=False, pr_number=None, once=once)
"""

# The same blocker as a SESSION LEADER with its own sleep child: the budget
# watchdog's group kill must take the descendant with the holder (the codex
# P1 on this PR), not only the Python process.
_CHILD_LEADER_WITH_CHILD = """
import os, socket, subprocess, sys
os.setsid()
sleeper = subprocess.Popen(["sleep", "98766"])
with open(sys.argv[2], "w") as fh:
    fh.write(str(sleeper.pid))
from fno.backlog.single_flight import reconcile_gate

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sys.argv[1])
srv.listen(1)
c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
c.connect(sys.argv[1])
conn, _ = srv.accept()

def once():
    conn.recv(1)

reconcile_gate(dry_run=False, node=None, json_out=False, pr_number=None, once=once)
"""

# Spawns the blocker with FNO_DIE_WITH_PARENT naming ITSELF, then lives until
# killed - the child's real parent in everything but the env var it passed.
_INTERMEDIATE_PARENT = """
import os, subprocess, sys, time
env = dict(os.environ, FNO_DIE_WITH_PARENT=str(os.getpid()))
proc = subprocess.Popen([sys.executable, "-c", sys.argv[1], sys.argv[2]], env=env)
with open(sys.argv[3], "w") as fh:
    fh.write(str(proc.pid))
time.sleep(120)
"""


def _child_env(iso: Path, extra: dict) -> dict:
    env = dict(
        os.environ,
        FNO_CLAIMS_ROOT=str(iso),
        FNO_REPO_ROOT=str(iso),
        FNO_EVENTS_PATH=str(iso / ".fno" / "events.jsonl"),
        PYTHONPATH=_SRC + os.pathsep + os.environ.get("PYTHONPATH", ""),
    )
    dev = find_dev_binary()
    if dev is not None:
        env["FNO_AGENTS_BIN"] = str(dev)
    env.update(extra)
    return env


def _wait_for_flight_held(key: str, iso: Path, proc: "subprocess.Popen | None" = None, timeout: float = 8.0) -> None:
    from fno.claims.io import claim_path

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if claim_path(key, root=iso).exists():
            return
        if proc is not None and proc.poll() is not None:
            break
        time.sleep(0.1)
    stderr = proc.stderr.read()[:500] if proc is not None and proc.stderr else b""
    raise AssertionError(f"child never acquired the flight {key} (rc={proc.poll() if proc else 'n/a'}); stderr: {stderr}")


def _pid_gone(pid: int) -> bool:
    """True when the pid is gone OR a reaped-pending zombie (a grandchild we
    can never waitpid)."""
    out = subprocess.run(
        ["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True
    )
    stat = out.stdout.strip()
    return not stat or stat[0] == "Z"


def test_budget_trips_a_live_blocked_holder(iso):
    """AC1-HP: budget 2s elapses while the holder blocks on a silent socket;
    the process exits 124, the flight reads free, and the stack file names
    the blocking frame."""
    key = reconcile_flight_key(node=None, pr_number=None)
    sock = _short_sock_dir() / "s.sock"
    proc = subprocess.Popen(
        [sys.executable, "-c", _CHILD_BLOCK_ON_SOCKET, str(sock)],
        env=_child_env(iso, {"FNO_FLIGHT_BUDGET_S": "2"}),
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        _wait_for_flight_held(key, iso, proc)
        assert proc.wait(timeout=8) == 124, f"child did not self-trip: {proc.stderr.read()[:400]}"
        stack_file = iso / ".fno" / "flight" / f"stack-{proc.pid}.txt"
        assert stack_file.exists(), "the watchdog must leave the stack file"
        # dump_traceback names the FRAME's function ("in once"), not the line text
        assert b"once" in stack_file.read_bytes(), "the stack must name the blocking frame"
        assert claim_status(key, root=claims_root_for(key))["state"] == "free"
    finally:
        if proc.poll() is None:
            proc.kill()


def test_die_with_parent_exits_the_orphan(iso):
    """AC1-ERR positive: the named parent is SIGKILLed; the child exits within
    4 seconds and its flight reads free."""
    key = reconcile_flight_key(node=None, pr_number=None)
    sock = _short_sock_dir() / "s.sock"
    pidfile = iso / "child.pid"
    intermediate = subprocess.Popen(
        [sys.executable, "-c", _INTERMEDIATE_PARENT, _CHILD_BLOCK_ON_SOCKET, str(sock), str(pidfile)],
        env=_child_env(iso, {}),
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    child_pid = None
    try:
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline and not pidfile.exists():
            time.sleep(0.1)
        child_pid = int(pidfile.read_text())
        _wait_for_flight_held(key, iso)  # child is a grandchild; no Popen handle
        os.kill(intermediate.pid, signal.SIGKILL)
        intermediate.wait(timeout=5)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and not _pid_gone(child_pid):
            time.sleep(0.2)
        if not _pid_gone(child_pid):
            os.kill(child_pid, signal.SIGKILL)
            raise AssertionError("orphaned child did not exit within 5s of its parent's death")
        assert claim_status(key, root=claims_root_for(key))["state"] == "free"
    finally:
        if intermediate.poll() is None:
            intermediate.kill()
        if child_pid is not None:
            try:
                os.kill(child_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def test_die_with_parent_unset_keeps_the_orphan_running(iso):
    """AC1-ERR negative: without the opt-in, the child outlives its parent
    (the detached reconcile-throttle shape stays legal)."""
    key = reconcile_flight_key(node=None, pr_number=None)
    sock = _short_sock_dir() / "s.sock"
    proc = subprocess.Popen(
        [sys.executable, "-c", _CHILD_BLOCK_ON_SOCKET, str(sock)],
        env=_child_env(iso, {}),
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        _wait_for_flight_held(key, iso, proc)
        time.sleep(2.5)
        assert proc.poll() is None, "unset FNO_DIE_WITH_PARENT must never trip the watchdog"
    finally:
        if proc.poll() is None:
            proc.kill()


def test_budget_takes_the_subtree_when_session_leader(iso):
    """The codex P1: a holder blocked in a subprocess must not orphan it on
    the trip. As a session leader the group kill takes the descendant too."""
    key = reconcile_flight_key(node=None, pr_number=None)
    sock = _short_sock_dir() / "lead.sock"
    pidfile = iso / "sleeper.pid"
    proc = subprocess.Popen(
        [sys.executable, "-c", _CHILD_LEADER_WITH_CHILD, str(sock), str(pidfile)],
        env=_child_env(iso, {"FNO_FLIGHT_BUDGET_S": "2"}),
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        _wait_for_flight_held(key, iso, proc)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and not pidfile.exists():
            time.sleep(0.1)
        time.sleep(0.3)  # let the sleep child spawn
        rc = proc.wait(timeout=8)
        # the pending SIGKILL and the os._exit(124) fallback race; both prove
        # the trip fired
        assert rc in (124, 137, -9), f"the leader holder must die on the trip (rc={rc})"
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                os.kill(int(pidfile.read_text()), 0)
            except ProcessLookupError:
                break
            time.sleep(0.2)
        else:
            raise AssertionError("the sleep descendant must die with the holder")
        assert claim_status(key, root=claims_root_for(key))["state"] == "free"
    finally:
        if proc.poll() is None:
            proc.kill()


def test_sigusr1_dumps_the_stack_and_the_holder_survives(iso):
    """AC1-EDGE: SIGUSR1 writes a traceback into the stack file; the holder
    keeps running."""
    key = reconcile_flight_key(node=None, pr_number=None)
    sock = _short_sock_dir() / "s.sock"
    proc = subprocess.Popen(
        [sys.executable, "-c", _CHILD_BLOCK_ON_SOCKET, str(sock)],
        env=_child_env(iso, {}),
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        _wait_for_flight_held(key, iso, proc)
        time.sleep(0.5)  # let the child pass register(); default SIGUSR1 would kill it
        os.kill(proc.pid, signal.SIGUSR1)
        time.sleep(1.0)
        assert proc.poll() is None, "SIGUSR1 must not kill the holder"
        stack_file = iso / ".fno" / "flight" / f"stack-{proc.pid}.txt"
        assert stack_file.exists(), "SIGUSR1 must leave the stack file"
        assert b"once" in stack_file.read_bytes(), "the dump must name the blocking frame"
    finally:
        if proc.poll() is None:
            proc.kill()
