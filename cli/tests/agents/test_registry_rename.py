from __future__ import annotations

import pytest

from fno.agents.registry import AgentEntry, resolve_agent, write_registry


def test_rename_changes_only_the_registry_label_and_preserves_session_identity(tmp_path):
    from fno.agents.registry import rename_agent

    registry = tmp_path / "agents.json"
    write_registry([
        AgentEntry(
            name="bp-old",
            cwd="/repo",
            log_path="",
            harness="codex",
            harness_session_id="session-1",
            mux={"session": "main", "pane_id": 12},
        )
    ], path=registry)

    renamed = rename_agent("bp-old", "target-new", registry_path=registry)

    assert renamed.name == "target-new"
    assert resolve_agent("target-new", path=registry).entry.harness_session_id == "session-1"
    assert resolve_agent("session-1", path=registry).entry.name == "target-new"
    assert resolve_agent("bp-old", path=registry).entry.name == "target-new"

    from fno.agents.registry import restamp_harness_session_id

    restamped = restamp_harness_session_id(
        name="bp-old", harness="codex", session_id="session-2", registry_path=registry
    )
    assert restamped is not None and restamped.name == "target-new"


def test_rename_refuses_a_label_collision(tmp_path):
    from fno.agents.registry import rename_agent

    registry = tmp_path / "agents.json"
    write_registry([
        AgentEntry(name="one", cwd="/repo", log_path="", harness="codex", harness_session_id="s1"),
        AgentEntry(name="two", cwd="/repo", log_path="", harness="codex", harness_session_id="s2"),
    ], path=registry)

    with pytest.raises(ValueError, match="already names"):
        rename_agent("one", "two", registry_path=registry)


def test_verified_tier_projection_updates_the_restamped_row_atomically(tmp_path):
    from fno.agents.registry import project_verified_tier

    registry = tmp_path / "agents.json"
    write_registry([
        AgentEntry(
            name="target-new",
            cwd="/repo",
            log_path="",
            harness="codex",
            harness_session_id="session-2",
            model="gpt-5.6-sol",
            effort="high",
        )
    ], path=registry)

    updated = project_verified_tier(
        "target-new", "session-2", model="gpt-5.6-luna", effort="xhigh", registry_path=registry
    )

    assert (updated.model, updated.effort) == ("gpt-5.6-luna", "xhigh")
    assert resolve_agent("session-2", path=registry).entry.model == "gpt-5.6-luna"
