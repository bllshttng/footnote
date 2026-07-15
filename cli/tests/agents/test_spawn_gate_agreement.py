"""AC1-FR (x-bdf9): the Python spawn gate's slot count must agree with the Rust
gate on the SAME shared fixture, and the roster must never consume worker slots.

The Rust half lives in ``crates/fno-agents/src/spawn_gate.rs::
slot_count_agrees_with_python_gate_fixture``; both read
``fixtures/spawn_gate_slot_agreement.json``. If either gate's counting rule
drifts (e.g. re-adding the roster to the slot count), its own assertion fails.
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from fno.agents import spawn_gate
from fno.agents.registry import AgentEntry

FIXTURE = Path(__file__).parent / "fixtures" / "spawn_gate_slot_agreement.json"
DEAD_PID = 4194321  # 2**22+17: realistically never a live pid (mirrors Rust)


def _pid(sym) -> int | None:
    if sym == "self":
        return os.getpid()
    if sym == "dead":
        return DEAD_PID
    return None


@pytest.fixture(autouse=True)
def _isolated_world(tmp_path, monkeypatch):
    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims-root"))
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    yield


def _scenarios():
    data = json.loads(FIXTURE.read_text())
    return [(s["name"], s) for s in data["scenarios"]]


@pytest.mark.parametrize("name,sc", _scenarios(), ids=lambda v: v if isinstance(v, str) else "")
def test_slot_count_agrees_with_rust_gate_fixture(name, sc, tmp_path, monkeypatch):
    if not isinstance(sc, dict):
        return  # the id-only leg of parametrize
    # Materialize the roster the slot count must ignore.
    workers = {}
    for j, r in enumerate(sc["roster"]):
        short = r.get("short") or f"{0xAAAA0000 + j:08x}"
        w = {"sessionId": f"{short}-1-2-3-4"}
        p = _pid(r.get("pid"))
        if p is not None:
            w["pid"] = p
        workers[short] = w
    roster = {"proto": 1, "supervisorPid": 1, "workers": workers}
    (tmp_path / "daemon" / "roster.json").write_text(json.dumps(roster))

    # Materialize the registry via the real load path (census tags source=fno).
    rows = [
        AgentEntry(
            name=row["name"],
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/log",
            status=row["status"],
            pid=_pid(row.get("pid")),
            short_id=row.get("short_id") or "",
        )
        for row in sc["registry"]
    ]
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)

    got = spawn_gate.census().slot_count
    assert got == sc["expect_slot_count"], f"{name}: got {got}"


def test_slot_count_excludes_roster_while_count_includes_it(tmp_path, monkeypatch):
    """The union .count still sees the roster (display); .slot_count does not."""
    alive = os.getpid()
    roster = {
        "proto": 1,
        "supervisorPid": 1,
        "workers": {
            f"{i:08x}": {"sessionId": f"{i:08x}-1-2-3-4", "pid": alive}
            for i in range(1, 39)  # 38 claude-mem-style observer sessions
        },
    }
    (tmp_path / "daemon" / "roster.json").write_text(json.dumps(roster))
    rows = [
        AgentEntry(name="w1", provider="claude", cwd="/tmp", log_path="/l",
                   status="busy", pid=alive, short_id=""),
        AgentEntry(name="w2", provider="claude", cwd="/tmp", log_path="/l",
                   status="busy", pid=alive, short_id=""),
    ]
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
    c = spawn_gate.census()
    assert c.count == 40, "union still counts the 38 roster + 2 fno workers"
    assert c.slot_count == 2, "AC1-HP: only the 2 fno workers hold slots"


def test_run_gate_passes_under_cap_despite_large_roster(tmp_path, monkeypatch, capsys):
    """AC1-HP end to end: 38 roster + 2 fno workers, max_live=15 -> passes."""
    alive = os.getpid()
    roster = {
        "proto": 1, "supervisorPid": 1,
        "workers": {
            f"{i:08x}": {"sessionId": f"{i:08x}-1-2-3-4", "pid": alive}
            for i in range(1, 39)
        },
    }
    (tmp_path / "daemon" / "roster.json").write_text(json.dumps(roster))
    rows = [
        AgentEntry(name=f"w{i}", provider="claude", cwd="/tmp", log_path="/l",
                   status="busy", pid=alive, short_id="")
        for i in range(2)
    ]
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)

    class _A:
        max_live = 15
        min_free_gb = 0.0

    class _S:
        agents = _A()

    monkeypatch.setattr("fno.config.load_settings", lambda: _S())
    guard = spawn_gate.run_gate("newcomer", "bg")
    assert capsys.readouterr().err == "", "no queue line: slot 2 < cap 15"
    guard.release()
