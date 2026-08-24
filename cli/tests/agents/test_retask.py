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
    assert receipt["payload"]["target_command"] == "$fno:target --no-merge x-bdb9"
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


def test_default_target_vendor_preserves_the_registry_vendor_axis():
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = detect_retask(_row(provider="openai"), target, node="x-bdb9")

    assert receipt["outcome"] == "retask_ready"


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


def test_execute_retask_same_tier_orders_clear_rename_status_then_target():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    sends: list[tuple[str, bool]] = []
    tiers: list[tuple[str, str]] = []
    frames = iter([
        "› Ask Codex to do anything\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
    ])

    def send(text: str, submit: bool) -> bool:
        sends.append((text, submit))
        return True

    receipt = execute_retask(
        _row(screen_state={"state": "idle", "rule": "idle_prompt"}),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        send=send,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
        project_tier=lambda model, effort: tiers.append((model, effort)),
    )

    assert receipt == {
        "status": "retasked",
        "cleared": True,
        "session_restamped": True,
        "switch": "skipped_same_tier",
        "switch_verified": True,
        "target_submit_confirmed": True,
        "registry_name": "target-x-bdb9",
    }
    assert [text for text, _submit in sends] == [
        "/clear", "/status", "$fno:target --no-merge x-bdb9"
    ]
    assert tiers == [("gpt-5.6-sol", "high")]


def test_execute_retask_refuses_when_fresh_frame_fails_readiness():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = execute_retask(
        _row(screen_state={"state": "idle", "rule": "idle_prompt"}),
        target,
        node="x-bdb9",
        read_frame=lambda: "painted but busy",
        ready_frame=lambda _frame: False,
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "pane_not_idle"
    assert receipt["cleared"] is False
    assert receipt["target_submit_confirmed"] is False


def test_execute_retask_codex_menu_walk_verifies_each_target_before_submit():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-luna", effort="xhigh"),
        env={},
    )
    sends: list[tuple[str, bool]] = []
    tiers: list[tuple[str, str]] = []
    frames = iter([
        "› Ask Codex to do anything\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
        "Select Model and Effort\n› 1. gpt-5.6-sol (current)\n  3. gpt-5.6-luna\n",
        "Select Model and Effort\n  1. gpt-5.6-sol\n› 3. gpt-5.6-luna (current)\n",
        "Select Reasoning Level for gpt-5.6-luna\n› 2. Medium (default)\n  4. Extra high\n",
        "Select Reasoning Level for gpt-5.6-luna\n  2. Medium (default)\n› 4. Extra high\n",
        "Model: gpt-5.6-luna (reasoning xhigh, summaries auto)",
    ])

    def send(text: str, submit: bool) -> bool:
        sends.append((text, submit))
        return True

    receipt = execute_retask(
        _row(screen_state={"state": "idle", "rule": "idle_prompt"}),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        send=send,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
        project_tier=lambda model, effort: tiers.append((model, effort)),
    )

    assert receipt["status"] == "retasked"
    assert receipt["switch"] == "switched"
    assert receipt["switch_verified"] is True
    assert receipt["target_submit_confirmed"] is True
    assert sends[0] == ("/clear", True)
    assert sends[-1] == ("$fno:target --no-merge x-bdb9", True)
    assert ("/model", True) in sends
    assert ("", True) in sends
    assert any(text == "\x1b[B" and not submit for text, submit in sends)
    assert tiers == [("gpt-5.6-sol", "high"), ("gpt-5.6-luna", "xhigh")]


def test_execute_retask_claude_uses_direct_strategy_commands():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="claude", model="new-model", effort="xhigh"),
        env={},
    )
    sends: list[tuple[str, bool]] = []
    frames = iter([
        "ready",
        "Model: old-model (reasoning high, summaries auto)",
        "Model: new-model (reasoning xhigh, summaries auto)",
    ])

    receipt = execute_retask(
        _row(
            harness="claude",
            model="old-model",
            effort="high",
            screen_state={"state": "idle", "rule": "live_prompt_box"},
        ),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        send=lambda text, submit: (sends.append((text, submit)) or True),
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["status"] == "retasked"
    assert ("/model new-model", True) in sends
    assert ("/effort xhigh", True) in sends
    assert [text for text, _submit in sends if text.startswith("/model")] == ["/model new-model"]


def test_execute_retask_uses_verified_tier_when_target_axes_are_omitted():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    sends: list[tuple[str, bool]] = []
    frames = iter([
        "› Ask Codex to do anything\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
    ])
    receipt = execute_retask(
        _row(model=None, effort=None, screen_state={"state": "idle", "rule": "idle_prompt"}),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        send=lambda text, submit: (sends.append((text, submit)) or True),
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["status"] == "retasked"
    assert receipt["switch"] == "skipped_same_tier"


def test_run_retask_converts_mux_timeout_to_structured_refusal(monkeypatch):
    import fno.agents.retask as retask

    row = _row(screen_state={"state": "idle", "rule": "idle_prompt"})
    target = retask.RetaskCoordinate(
        harness="codex", provider=None, model="gpt-5.6-sol", effort="high",
        substrate="pane", permission_mode=None, route=None, account=None,
    )
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)

    def timeout(*_args, **_kwargs):
        raise retask.subprocess.TimeoutExpired("fno mux", 10)

    monkeypatch.setattr(retask.subprocess, "run", timeout)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "pane_read_timeout"
    assert receipt["target_submit_confirmed"] is False


def test_execute_retask_refuses_missing_positive_menu_row_before_target():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-luna", effort="xhigh"),
        env={},
    )
    frames = iter([
        "› Ask Codex to do anything\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
        "Select Model and Effort\n› 1. gpt-5.6-sol (current)\n",
    ])

    receipt = execute_retask(
        _row(screen_state={"state": "idle", "rule": "idle_prompt"}),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "model_row_missing"
    assert receipt["target_submit_confirmed"] is False


def test_execute_retask_refuses_unsupported_harness_before_clear():
    from fno.agents.retask import RetaskCoordinate, execute_retask

    target = RetaskCoordinate(
        harness="gemini", provider=None, model=None, effort=None,
        substrate="pane", permission_mode=None, route=None, account=None,
    )
    receipt = execute_retask(
        _row(harness="gemini", model=None, effort=None, screen_state={"state": "idle", "rule": "unsupported"}),
        target,
        node="x-bdb9",
        read_frame=lambda: "ready",
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "unsupported_switch_strategy"
    assert receipt["cleared"] is False
