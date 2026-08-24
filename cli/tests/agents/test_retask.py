from __future__ import annotations

from dataclasses import replace
from types import SimpleNamespace

import pytest

from fno.agents.registry import AgentEntry


def _row(**overrides) -> AgentEntry:
    values = {
        "name": "bp-xbdb9-retask",
        "cwd": "/repo",
        "log_path": "",
        "harness": "codex",
        "provider": None,
        "model": "gpt-5.6-sol",
        "effort": "high",
        "harness_session_id": "old-session",
        "mux": {"session": "main", "pane_id": 12},
    }
    values.update(overrides)
    return AgentEntry(**values)


def _settings(**target):
    defaults = SimpleNamespace(
        provider="", model="", effort="", substrate="", permission_mode="",
        route="", account="", pane_group="", lanes=[],
    )
    profile = SimpleNamespace(**{**vars(defaults), **target})
    return SimpleNamespace(
        agents=SimpleNamespace(defaults=defaults, profiles={"target": profile}, max_lanes={}),
        model_routing=None,
    )


def test_same_tier_builds_target_payload_without_executable_switch_commands():
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = detect_retask(_row(), target, node="x-bdb9")

    assert receipt["outcome"] == "retask_ready"
    assert receipt["payload"]["target_command"] == "$fno:target x-bdb9"
    assert receipt["payload"]["switch"] == {"required": False}
    assert receipt["payload"]["execution"] == {"mode": "read_only_plan"}


def test_tier_mismatch_builds_mechanism_neutral_switch_pending_payload():
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-luna", effort="xhigh"),
        env={},
    )
    receipt = detect_retask(_row(), target, node="x-bdb9")

    assert receipt["outcome"] == "switch_pending"
    assert receipt["payload"]["switch"] == {
        "required": True,
        "from": {"model": "gpt-5.6-sol", "effort": "high"},
        "to": {"model": "gpt-5.6-luna", "effort": "xhigh"},
        "mechanism": "pending_operator_decision",
    }
    assert receipt["payload"]["execution"] == {"mode": "read_only_plan"}


@pytest.mark.parametrize(
    ("target_override", "reason"),
    [
        ({"harness": "claude"}, "harness"),
        ({"provider": "zai", "route": "zai/glm-5.3", "model": "glm-5.3"}, "provider"),
        ({"substrate": "bg"}, "substrate"),
        ({"permission_mode": "yolo"}, "permission_mode"),
        ({"account": "work"}, "account"),
    ],
)
def test_incompatible_axis_requires_spawn_before_any_payload(target_override, reason):
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    target = replace(target, **target_override)
    receipt = detect_retask(_row(), target, node="x-bdb9")

    assert receipt == {"outcome": "spawn_required", "reason": reason}


def test_non_mux_worker_is_refused_without_a_target_payload():
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    receipt = detect_retask(_row(mux=None), target, node="x-bdb9")

    assert receipt == {"outcome": "refused", "reason": "worker_has_no_mux_ref"}


@pytest.mark.parametrize(
    ("row_override", "reason"),
    [
        ({"status": "stopped"}, "worker_not_live"),
        ({"harness_session_id": None}, "worker_has_no_session_id"),
    ],
)
def test_unusable_worker_is_refused_with_a_named_positive_verdict(row_override, reason):
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    receipt = detect_retask(_row(**row_override), target, node="x-bdb9")

    assert receipt == {"outcome": "refused", "reason": reason}


def test_explicit_model_and_effort_override_target_profile():
    from fno.agents.retask import resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        model="gpt-5.6-luna",
        effort="xhigh",
        env={},
    )

    assert target.model == "gpt-5.6-luna"
    assert target.effort == "xhigh"
