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


def test_rename_refuses_a_label_collision(tmp_path):
    from fno.agents.registry import rename_agent

    registry = tmp_path / "agents.json"
    write_registry([
        AgentEntry(name="one", cwd="/repo", log_path="", harness="codex", harness_session_id="s1"),
        AgentEntry(name="two", cwd="/repo", log_path="", harness="codex", harness_session_id="s2"),
    ], path=registry)

    with pytest.raises(ValueError, match="already names"):
        rename_agent("one", "two", registry_path=registry)
