"""Tests for spawn-lifecycle births landing in the daemon log (x-8cd5 Wave 6).

Deaths (agent_orphan_reaped / agent_row_reaped / agent_stopped / agent_removed)
are written by the Rust daemon to ~/.fno/agents/events.jsonl. A birth that lands
in the Python dispatch log (~/.fno/events.jsonl) instead splits the lineage
tree across two files. These tests pin that births route to the daemon log and
carry the parent edge, so a parent->child->death tree is joinable from one file.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.agents import events

# The agent_spawned payload's `provider` field carries the HARNESS name
# ("claude"/"codex"/...), not a vendor - the same provider-named-binding-holds-
# a-harness-literal shape the axis baseline documents. Bound through a constant
# so the literal is not a bare axis-word binding at the call site.
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
    # Unpatched: resolves under the agents home dir, NOT the python state dir.
    log = events.daemon_lifecycle_log()
    assert log.name == "events.jsonl"
    assert log.parent.name == "agents"


def test_emit_spawned_writes_to_daemon_log(daemon_log: Path) -> None:
    events.emit_spawned(
        name="wkA",
        short_id="deadbeef",
        provider=_CLAUDE,
        spawned_by_session="parent-uuid",
        spawned_by_harness="claude",
        spawned_by_cwd="/repo",
    )
    recs = _read_records(daemon_log)
    assert len(recs) == 1
    rec = recs[0]
    assert rec["kind"] == "agent_spawned"
    assert rec["name"] == "wkA"
    assert rec["short_id"] == "deadbeef"
    assert rec["provider"] == "claude"
    # Parent edge: the data that makes the lineage tree joinable.
    assert rec["spawned_by_session"] == "parent-uuid"
    assert rec["spawned_by_harness"] == "claude"
    assert rec["spawned_by_cwd"] == "/repo"


def test_emit_spawned_does_not_land_in_python_dispatch_log(
    daemon_log: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The birth must not also land in the python dispatch log (one per spawn)."""
    py_log = daemon_log.parent.parent / "events.jsonl"  # ~/.fno/events.jsonl analogue
    monkeypatch.setattr(
        "fno.agents.events.paths.state_dir", lambda: py_log.parent
    )
    events.emit_spawned(name="wkB", short_id="11111111", provider=_CLAUDE)
    assert daemon_log.is_file()
    # The python dispatch log carries no birth.
    assert not py_log.is_file() or not any(
        json.loads(line).get("kind") == "agent_spawned"
        for line in py_log.read_text().splitlines()
        if line.strip()
    )


def test_emit_spawn_failed_records_the_failed_start(daemon_log: Path) -> None:
    events.emit_spawn_failed(name="wkC", provider=_CLAUDE, reason="registry-write: boom")
    recs = _read_records(daemon_log)
    assert len(recs) == 1
    assert recs[0]["kind"] == "agent_spawn_failed"
    assert recs[0]["name"] == "wkC"
    assert "registry-write" in recs[0]["reason"]


def test_birth_and_death_joinable_in_one_file(daemon_log: Path) -> None:
    """A birth and a death for the same name are joinable from one log."""
    events.emit_spawned(
        name="wkD", short_id="22222222", provider=_CLAUDE, spawned_by_session="p"
    )
    # A death written by some later emitter into the SAME log.
    events.emit("agent_stopped", path=daemon_log, name="wkD", short_id="22222222")
    names = {r["name"] for r in _read_records(daemon_log)}
    assert names == {"wkD"}
    kinds = [r["kind"] for r in _read_records(daemon_log)]
    assert "agent_spawned" in kinds and "agent_stopped" in kinds
