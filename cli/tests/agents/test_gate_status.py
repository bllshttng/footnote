"""The read-only capacity probe behind `fno agents gate-status`.

The stop hook asks this instead of waking the model on work no session can
dispatch. Behavioral contract: every capped lane at cap refuses; one lane with
room accepts; a global floor refuses; a broken reading is unknown, never
saturation; and the probe is silent and side-effect free."""
from __future__ import annotations

import os

import pytest

from fno.agents import spawn_gate
from fno.agents.registry import AgentEntry

KING = "aaaaaaaa-1111-2222-3333-444455556666"


@pytest.fixture(autouse=True)
def _isolated_world(tmp_path, monkeypatch):
    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims-root"))
    monkeypatch.setenv("FNO_THINK_SPAWN", "0")
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    monkeypatch.setenv("FNO_CC_DAEMON_RV_ROOT", str(tmp_path / "no-farm"))
    yield


def _row(name, pid, *, spawned_by=None):
    return AgentEntry(
        name=name,
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/l",
        status="live",
        pid=pid,
        spawned_by_session=spawned_by,
    )


def _settings():
    class _A:
        max_live = 30
        min_free_gb = 0.0
        max_load_per_cpu = 0.0
        provider_limits = {"zai": 2, "codex": 2}

    class _S:
        agents = _A()

    return _S()


def _census(monkeypatch, rows):
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
    return spawn_gate.census()


def _wire(
    monkeypatch,
    rows,
    *,
    ram=8.0,
    lanes=None,
    lane_error=None,
    settings=None,
):
    alive = os.getpid()
    c = _census(monkeypatch, rows)
    monkeypatch.setattr("fno.config.load_settings", lambda: settings or _settings())
    monkeypatch.setattr(spawn_gate, "census", lambda: c)
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda: type("I", (), {"session_id": KING, "harness": "claude"})(),
    )
    monkeypatch.setattr(spawn_gate, "available_ram_gb", lambda: ram)
    monkeypatch.setattr(
        spawn_gate,
        "provider_live_count",
        lambda provider, counted=None: (
            lane_error(provider) if lane_error else lanes[provider]
        ),
    )
    return c


def test_every_lane_at_cap_refuses_with_lanes_named(monkeypatch):
    alive = os.getpid()
    rows = [
        _row("z1", alive),
        _row("z2", alive),
        _row("c1", alive),
        _row("c2", alive),
    ]
    _wire(monkeypatch, rows, lanes={"zai": 2, "codex": 2})
    verdict = spawn_gate.probe_capacity()
    assert verdict["verdict"] == "refused"
    assert verdict["reason"] == "provider_cap"
    assert verdict["lanes"] == {
        "zai": {"cap": 2, "live": 2},
        "codex": {"cap": 2, "live": 2},
    }
    assert "zai 2/2" in verdict["message"]
    assert "codex 2/2" in verdict["message"]


def test_one_lane_below_cap_accepts(monkeypatch):
    alive = os.getpid()
    rows = [
        _row("z1", alive),
        _row("z2", alive),
        _row("c1", alive),
    ]
    _wire(monkeypatch, rows, lanes={"zai": 2, "codex": 1})
    verdict = spawn_gate.probe_capacity()
    assert verdict["verdict"] == "accepted"
    assert verdict["lanes"] == {
        "zai": {"cap": 2, "live": 2},
        "codex": {"cap": 2, "live": 1},
    }


def test_ram_floor_refuses(monkeypatch):
    class _A:
        max_live = 30
        min_free_gb = 1.0
        max_load_per_cpu = 0.0
        provider_limits = {"zai": 2, "codex": 2}

    class _S:
        agents = _A()

    alive = os.getpid()
    rows = [_row("z1", alive)]
    _wire(
        monkeypatch,
        rows,
        lanes={"zai": 0, "codex": 0},
        ram=0.5,
        settings=_S(),
    )
    verdict = spawn_gate.probe_capacity()
    assert verdict["verdict"] == "refused"
    assert verdict["reason"] == "ram_floor"
    assert verdict["available_gb"] == 0.5


def test_lane_count_failure_is_unknown_never_saturation(monkeypatch):
    alive = os.getpid()
    rows = [_row("z1", alive)]

    def _boom(provider):
        raise RuntimeError("ps exploded")

    _wire(monkeypatch, rows, lane_error=_boom)
    verdict = spawn_gate.probe_capacity()
    assert verdict["verdict"] == "unknown"


def test_probe_is_silent_and_raises_nothing(monkeypatch, capsys):
    alive = os.getpid()
    rows = [_row("z1", alive)]
    _wire(monkeypatch, rows, lanes={"zai": 0, "codex": 0})
    verdict = spawn_gate.probe_capacity()
    captured = capsys.readouterr()
    assert captured.out == ""
    assert captured.err == ""
    assert verdict["verdict"] == "accepted"


def test_registry_schema_ahead_refuses(monkeypatch):
    alive = os.getpid()
    rows = [_row("z1", alive)]
    _wire(monkeypatch, rows, lanes={"zai": 0, "codex": 0})
    monkeypatch.setattr(
        "fno.agents.registry._registry_path",
        lambda p: "/tmp/registry.json",
    )
    monkeypatch.setattr(
        "fno.agents.registry._read_raw_registry",
        lambda target: {"schema_version": 9999},
    )
    verdict = spawn_gate.probe_capacity()
    assert verdict["verdict"] == "refused"
    assert verdict["reason"] == "registry_schema"
