"""Positive coverage for the logical identity of pane-less thread workers."""
from __future__ import annotations

from types import SimpleNamespace

import pytest

from fno.agents.registry import AgentEntry


@pytest.fixture(autouse=True)
def _graph_with_target_node(monkeypatch):
    """Verb resolution loads the node record; default it to a planless low
    node so the probe resolves the target verb."""
    monkeypatch.setattr(
        "fno.graph.store.read_nodes_by_ids",
        lambda path, tokens: {"entries": [{"id": "x-bdb9", "difficulty": "low"}]},
    )


def _thread_row(**overrides) -> AgentEntry:
    values = {
        "name": "thread-worker",
        "cwd": "/repo",
        "log_path": "/tmp/thread-worker.log",
        "harness": "codex",
        "provider": "openai",
        "model": "gpt-5.6-sol",
        "effort": "high",
        "harness_session_id": "thread-session",
        "substrate": "thread",
        "fno_id": "thread-session",
        "mux": None,
        "host_mode": "interactive",
    }
    values.update(overrides)
    return AgentEntry(**values)


def test_resolve_target_coordinate_leaves_substrate_unspecified_until_worker_read(
    tmp_path, monkeypatch
):
    from fno.agents.retask import resolve_target_coordinate

    cfg = tmp_path / "config.toml"
    cfg.write_text("")
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    target = resolve_target_coordinate("x-bdb9", env={})

    assert target.substrate is None


def test_thread_viewport_resolver_uses_thread_identity_not_pane_zero(monkeypatch):
    from fno.agents import retask

    calls = []

    def run(command, **_kwargs):
        calls.append(command)
        if command[1:3] == ["mux", "thread"]:
            return SimpleNamespace(returncode=0, stdout="thread pane -> thread-worker\n", stderr="")
        return SimpleNamespace(
            returncode=0,
            stdout='[{"name":"thread-worker","fno_id":"thread-session","pane_id":993}]',
            stderr="",
        )

    monkeypatch.setattr(retask.subprocess, "run", run)
    monkeypatch.setenv("FNO_SESSION", "main")

    assert retask.resolve_thread_viewport(_thread_row()) == ("main", 993)
    # The door keys on the row name (portal_reach row_answers_key); the
    # fno_id rides the join below. The reach carries no baked placement:
    # no flag tunes the row's open portal, else portal 0 serves it.
    assert calls[0] == [
        "fno", "mux", "thread", "thread-worker", "--server", "main",
    ]
    assert calls[1][:5] == ["fno", "mux", "pane", "ls", "--server"]
