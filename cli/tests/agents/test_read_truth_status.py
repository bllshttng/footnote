"""US2 wiring: `fno agents list` overrides only the ambiguous harness Idle /
missing live_status with the fno-truth state, never a harness Working.

The resolver itself is tested against real claim + events files in
tests/test_truth_status.py; here the resolver is stubbed so the tests exercise
read.py's override rules precisely (AC1-HP, AC5-ERR, Locked Decision 1).
"""

from __future__ import annotations

import json

import pytest

from fno.agents import truth_status
from fno.agents.read import list_agents
from fno.agents.registry import AgentEntry, write_registry
from fno.paths_testing import use_tmpdir


def _claude(name, short_id="abc12345", **kw) -> AgentEntry:
    base = dict(
        name=name,
        provider="claude",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/x/output.jsonl",
        claude_short_id=short_id,
        created_at="2026-05-20T17:00:00Z",
        status="live",
    )
    base.update(kw)
    return AgentEntry(**base)


@pytest.fixture
def _patch_claude(monkeypatch):
    def _install(live_map):
        from fno.agents.providers import claude as claude_mod

        monkeypatch.setattr(
            claude_mod, "claude_agents_json", lambda timeout=3.0: (live_map, [])
        )

    return _install


@pytest.fixture(autouse=True)
def _no_events(monkeypatch):
    # The list path builds the index once; stub it so no real events file is read.
    monkeypatch.setattr(truth_status, "build_loop_check_index", lambda **_: {})


def _rows(monkeypatch, tmp_path):
    result = list_agents(json_out=True)
    return {r["name"]: r for r in json.loads(result.output)["agents"]}


def test_idle_target_worker_shows_working(tmp_path, monkeypatch, _patch_claude):
    """AC1-HP — harness Idle + a working resolver verdict -> Working row."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude("target-x-4a48-fleet-status", short_id="s1")])
    _patch_claude({"s1": {"live_status": "Idle"}})
    monkeypatch.setattr(
        truth_status,
        "resolve_truth_status",
        lambda nid, **_: {"state": "working", "last_loop_check_age_s": 120},
    )
    rows = _rows(monkeypatch, tmp_path)
    assert rows["target-x-4a48-fleet-status"]["live_status"] == "Working (loop 2m ago)"


def test_harness_working_never_overridden(tmp_path, monkeypatch, _patch_claude):
    """Locked Decision 1 — a harness Working is authoritative; resolver unused."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude("target-x-4a48-fleet-status", short_id="s1")])
    _patch_claude({"s1": {"live_status": "Working"}})

    def _boom(*a, **k):  # resolver must not be consulted
        raise AssertionError("resolver called despite harness Working")

    monkeypatch.setattr(truth_status, "resolve_truth_status", _boom)
    rows = _rows(monkeypatch, tmp_path)
    assert rows["target-x-4a48-fleet-status"]["live_status"] == "Working"


def test_non_target_name_unchanged(tmp_path, monkeypatch, _patch_claude):
    """A non-``target-`` name has no node id -> unknown -> harness value passes."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude("worker-frontend", short_id="s1")])
    _patch_claude({"s1": {"live_status": "Idle"}})
    rows = _rows(monkeypatch, tmp_path)
    assert rows["worker-frontend"]["live_status"] == "Idle"


def test_unknown_verdict_passes_harness_idle(tmp_path, monkeypatch, _patch_claude):
    """AC5-ERR — resolver unknown (missing signals) -> Idle unchanged."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude("target-x-4a48-fleet-status", short_id="s1")])
    _patch_claude({"s1": {"live_status": "Idle"}})
    monkeypatch.setattr(
        truth_status,
        "resolve_truth_status",
        lambda nid, **_: {"state": "unknown", "last_loop_check_age_s": None},
    )
    rows = _rows(monkeypatch, tmp_path)
    assert rows["target-x-4a48-fleet-status"]["live_status"] == "Idle"


def test_missing_harness_status_filled_by_stalled(tmp_path, monkeypatch, _patch_claude):
    """A missing harness status (shellout dropped the row) + stale claim ->
    Stalled is surfaced (the state Idle used to falsely imply)."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude("target-x-4a48-fleet-status", short_id="s1")])
    _patch_claude({})  # no live_status for s1 -> None
    monkeypatch.setattr(
        truth_status,
        "resolve_truth_status",
        lambda nid, **_: {"state": "stalled", "last_loop_check_age_s": None},
    )
    rows = _rows(monkeypatch, tmp_path)
    assert rows["target-x-4a48-fleet-status"]["live_status"] == "Stalled (claim stale)"
