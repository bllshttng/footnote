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


def _screen_verdict(
    *,
    matched: bool = True,
    rule_id: str | None = "idle_prompt",
    state: str | None = "idle",
) -> dict:
    return {"matched": matched, "rule_id": rule_id, "state": state}


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
        _row(screen_state=None),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        ready_frame=lambda _frame: _screen_verdict(),
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


def test_execute_retask_refuses_live_busy_even_when_cached_snapshot_is_idle():
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
        ready_frame=lambda _frame: _screen_verdict(rule_id="working", state="working"),
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "pane_not_idle"
    assert receipt["cleared"] is False
    assert receipt["target_submit_confirmed"] is False


def test_execute_retask_names_readable_unmatched_frame_as_unobserved():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    sends: list[tuple[str, bool]] = []
    receipt = execute_retask(
        _row(screen_state={"state": "idle", "rule": "idle_prompt"}),
        target,
        node="x-bdb9",
        read_frame=lambda: "painted but no known manifest rule",
        ready_frame=lambda _frame: _screen_verdict(matched=False, rule_id=None, state=None),
        send=lambda text, submit: sends.append((text, submit)) or True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "pane_state_unobserved"
    assert receipt["cleared"] is False
    assert sends == []


def test_execute_retask_names_missing_live_verdict_as_unobserved():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = execute_retask(
        _row(screen_state=None),
        target,
        node="x-bdb9",
        read_frame=lambda: "readable pane frame",
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "pane_state_unobserved"
    assert receipt["cleared"] is False


def test_execute_retask_fails_closed_on_legacy_boolean_live_verdict():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = execute_retask(
        _row(screen_state=None),
        target,
        node="x-bdb9",
        read_frame=lambda: "readable pane frame",
        ready_frame=lambda _frame: False,
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "pane_state_unobserved"
    assert receipt["cleared"] is False


def test_execute_retask_keeps_empty_frame_unreadable_distinct():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = execute_retask(
        _row(screen_state=None),
        target,
        node="x-bdb9",
        read_frame=lambda: "",
        ready_frame=lambda _frame: pytest.fail("empty frame must not be evaluated"),
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "pane_frame_unreadable"
    assert receipt["cleared"] is False


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
        ready_frame=lambda _frame: _screen_verdict(),
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
        ready_frame=lambda _frame: _screen_verdict(rule_id="live_prompt_box"),
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
        ready_frame=lambda _frame: _screen_verdict(),
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


def test_run_retask_passes_live_osc_title_to_manifest_evaluator(monkeypatch):
    import fno.agents.retask as retask

    row = _row(screen_state=None)
    target = retask.RetaskCoordinate(
        harness="codex", provider=None, model="gpt-5.6-sol", effort="high",
        substrate="pane", permission_mode=None, route=None, account=None,
    )
    sends: list[str] = []
    observed: dict[str, object] = {}
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(
        "fno.agents.mux_spawn._pane_osc_title",
        lambda *_args, **_kwargs: "⠋ Working",
    )

    def evaluate(_harness, _frame, _runner, *, osc_title=None, **_kwargs):
        observed["osc_title"] = osc_title
        if osc_title:
            return {"matched": True, "rule_id": "busy", "state": "working"}
        return {"matched": True, "rule_id": "idle_prompt", "state": "idle"}

    monkeypatch.setattr("fno.agents.mux_spawn._evaluate_manifest_screen", evaluate)

    def run(command, **_kwargs):
        if "send" in command:
            sends.append(command[command.index("--text") + 1])
        return SimpleNamespace(returncode=0, stdout="live frame", stderr="")

    monkeypatch.setattr(retask.subprocess, "run", run)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["reason"] == "pane_not_idle"
    assert observed == {"osc_title": "⠋ Working"}
    assert sends == []


def test_run_retask_refuses_claude_when_live_title_is_unavailable(monkeypatch):
    import fno.agents.retask as retask

    row = _row(harness="claude", screen_state=None)
    target = retask.RetaskCoordinate(
        harness="claude", provider=None, model="old-model", effort="high",
        substrate="pane", permission_mode=None, route=None, account=None,
    )
    sends: list[str] = []
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr("fno.agents.mux_spawn._pane_osc_title", lambda *_args: None)
    monkeypatch.setattr(
        "fno.agents.mux_spawn._evaluate_manifest_screen",
        lambda *_args, **_kwargs: {
            "matched": True,
            "rule_id": "live_prompt_box",
            "state": "idle",
        },
    )

    def run(command, **_kwargs):
        if "send" in command:
            sends.append(command[command.index("--text") + 1])
        return SimpleNamespace(returncode=0, stdout="live prompt box", stderr="")

    monkeypatch.setattr(retask.subprocess, "run", run)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["reason"] == "pane_state_unobserved"
    assert sends == []


def test_run_retask_timeout_mid_transaction_reports_the_true_pane_state(monkeypatch):
    """A transport death after /clear + restamp + rename must not claim the
    pane is untouched: the receipt names the cleared, renamed pane."""
    import fno.agents.retask as retask

    row = _row(screen_state={"state": "idle", "rule": "idle_prompt"})
    target = retask.RetaskCoordinate(
        harness="codex", provider=None, model="gpt-5.6-sol", effort="high",
        substrate="pane", permission_mode=None, route=None, account=None,
    )
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(
        retask,
        "load_registry",
        lambda **_kwargs: [SimpleNamespace(name=row.name, harness_session_id="new-session")],
    )
    monkeypatch.setattr(
        retask,
        "rename_agent",
        lambda *_args, **_kwargs: SimpleNamespace(name="target-x-bdb9"),
    )
    # Readiness normally shells to the Rust manifest engine; pin its verdict
    # so the only subprocess traffic is the pane reads/sends under test.
    monkeypatch.setattr(
        "fno.agents.mux_spawn._evaluate_manifest_screen",
        lambda *_args, **_kwargs: {"matched": True, "rule_id": "idle_prompt", "state": "idle"},
    )
    monkeypatch.setattr("fno.agents.mux_spawn._pane_osc_title", lambda *_args: None)

    calls = {"n": 0}

    def timeout_after_two(*_args, **_kwargs):
        # 1: initial frame read, 2: /clear send; die on the /status send.
        calls["n"] += 1
        if calls["n"] > 2:
            raise retask.subprocess.TimeoutExpired("fno mux", 15)
        return SimpleNamespace(returncode=0, stdout="frame")

    monkeypatch.setattr(retask.subprocess, "run", timeout_after_two)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "pane_send_timeout"
    assert receipt["cleared"] is True
    assert receipt["session_restamped"] is True
    assert receipt["registry_name"] == "target-x-bdb9"


def test_menu_delta_exact_match_beats_substring_and_shortest_wins():
    from fno.agents.retask import _menu_delta

    frame = "› 1. gpt-5.6-sol-mini\n  3. gpt-5.6-sol\n"
    assert _menu_delta(frame, "gpt-5.6-sol") == 2
    assert _menu_delta(frame, "sol") == 2
    assert _menu_delta(frame, "luna") is None


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
        ready_frame=lambda _frame: _screen_verdict(),
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
