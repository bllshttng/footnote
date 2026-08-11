"""Tests for spawn-lifecycle births landing in the daemon log (x-8cd5 Wave 6).

Deaths are written by the Rust daemon in the unified envelope
{ts, type, source, data:{...}} (events.rs x-2901). Births must use the SAME
envelope so a parent->child->death tree is joinable from one file by one reader.
A flat Python record would share the file but not the schema, so a reader joining
on data.name would KeyError on one half.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.agents import events

# The agent_spawned payload's `provider` field carries the HARNESS name, not a
# vendor. Bound through a constant so the literal is not a bare axis-word binding.
_CLAUDE = "claude"


@pytest.fixture
def daemon_log(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Redirect daemon_lifecycle_log() at a tmp path; return it."""
    target = tmp_path / "agents" / "events.jsonl"
    monkeypatch.setattr(events, "daemon_lifecycle_log", lambda: target)
    return target


def _read_records(path: Path) -> list[dict]:
    if not path.is_file():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def test_daemon_lifecycle_log_is_agents_home() -> None:
    """The birth target is the daemon's lifecycle log, where deaths live."""
    log = events.daemon_lifecycle_log()
    assert log.name == "events.jsonl"
    assert log.parent.name == "agents"


def test_emit_spawned_writes_envelope_to_daemon_log(daemon_log: Path) -> None:
    events.emit_spawned(
        name="wkA",
        short_id="deadbeef",
        provider=_CLAUDE,
        spawned_by_session="parent-uuid",
        spawned_by_harness=_CLAUDE,
        spawned_by_cwd="/repo",
    )
    recs = _read_records(daemon_log)
    assert len(recs) == 1
    rec = recs[0]
    # Envelope frame matches the Rust daemon (events.rs x-2901): a flat record
    # would not be joinable with a daemon death by one reader.
    assert rec["type"] == "agent_spawned"
    assert rec["source"] == "python"
    assert isinstance(rec["data"], dict)
    d = rec["data"]
    assert d["name"] == "wkA"
    assert d["short_id"] == "deadbeef"
    assert d["provider"] == _CLAUDE
    assert d["spawned_by_session"] == "parent-uuid"
    assert d["spawned_by_harness"] == _CLAUDE
    assert d["spawned_by_cwd"] == "/repo"


def test_emit_spawned_does_not_land_in_python_dispatch_log(daemon_log: Path) -> None:
    """One birth per spawn, in the daemon log only (not also in the python log)."""
    py_log = daemon_log.parent.parent / "events.jsonl"
    events.emit_spawned(name="wkB", short_id="11111111", provider=_CLAUDE)
    assert daemon_log.is_file()
    assert not py_log.is_file()


def test_emit_spawn_failed_records_the_failed_start(daemon_log: Path) -> None:
    events.emit_spawn_failed(name="wkC", provider=_CLAUDE, reason="registry-write: boom")
    recs = _read_records(daemon_log)
    assert len(recs) == 1
    assert recs[0]["type"] == "agent_spawn_failed"
    assert recs[0]["data"]["name"] == "wkC"
    assert "registry-write" in recs[0]["data"]["reason"]


def test_birth_and_death_joinable_in_one_file(daemon_log: Path) -> None:
    """A birth (Python envelope) and a death (the Rust daemon's envelope shape)
    for the same name join on data.name from one file."""
    events.emit_spawned(
        name="wkD", short_id="22222222", provider=_CLAUDE, spawned_by_session="p"
    )
    # A death written by the Rust daemon in its real envelope shape.
    events._emit_daemon_envelope(
        "agent_stopped", {"name": "wkD", "short_id": "22222222"}, source="daemon"
    )
    recs = _read_records(daemon_log)
    names = {r["data"]["name"] for r in recs}
    assert names == {"wkD"}
    types = [r["type"] for r in recs]
    assert "agent_spawned" in types and "agent_stopped" in types
