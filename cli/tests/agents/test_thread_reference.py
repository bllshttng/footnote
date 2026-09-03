"""Positive coverage for the logical identity of pane-less thread workers."""
from __future__ import annotations

from types import SimpleNamespace

from fno.agents.registry import AgentEntry


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


def _settings(provider: str = "codex") -> SimpleNamespace:
    defaults = SimpleNamespace(
        provider=provider,
        model="gpt-5.6-sol",
        effort="high",
        substrate="",
        permission_mode="",
        route="",
        account="",
        pane_group="",
        lanes=[],
    )
    return SimpleNamespace(
        agents=SimpleNamespace(
            defaults=defaults,
            profiles={"target": SimpleNamespace(**vars(defaults))},
            max_lanes={},
        ),
        model_routing=None,
    )


def test_detect_retask_reads_thread_identity_without_a_mux_pane():
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate("x-bdb9", settings=_settings(), env={})
    receipt = detect_retask(_thread_row(), target, node="x-bdb9")

    assert receipt["outcome"] == "retask_ready"
    assert receipt["payload"]["mux"] is None
    assert receipt["payload"]["thread_id"] == "thread-session"
    assert receipt["payload"]["target"]["substrate"] == "thread"


def test_resolve_target_coordinate_leaves_substrate_unspecified_until_worker_read():
    from fno.agents.retask import resolve_target_coordinate

    target = resolve_target_coordinate("x-bdb9", settings=_settings(), env={})

    assert target.substrate is None


def test_thread_identity_missing_refuses_by_name():
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate("x-bdb9", settings=_settings(), env={})
    receipt = detect_retask(_thread_row(fno_id=None), target, node="x-bdb9")

    assert receipt == {"outcome": "refused", "reason": "worker_has_no_thread_ref"}


def test_zero_mux_sentinel_is_not_a_thread_reference():
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate("x-bdb9", settings=_settings(), env={})
    receipt = detect_retask(
        _thread_row(mux={"session": "main", "pane_id": 0}),
        target,
        node="x-bdb9",
    )

    assert receipt == {"outcome": "refused", "reason": "worker_has_no_thread_ref"}


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
    assert calls[0] == ["fno", "mux", "thread", "--session", "main", "thread-session"]
    assert calls[1][:5] == ["fno", "mux", "pane", "ls", "--session"]
