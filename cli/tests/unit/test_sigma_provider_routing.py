"""Tests for the /review sigma cross-model routing seam (x-7137 US3).

Covers the shared resolver (``resolve_panel_providers``), the accessor the
skill consumes (``panel_provider_routing``), the ``fno review --print-providers``
flag, and the "one resolution path, no drift" invariant: the skill's routing
equals what the ``build_review_runner`` panel resolves for the same inputs.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.review import provider_resolution as pr
from fno.review.orchestrator import AGENT_NAMES
from fno.worker import review as review_mod

CORRECTNESS = ("code_reviewer", "silent_failure_hunter", "type_design_analyzer")


# ---- resolve_panel_providers: the single resolution path ----


def test_resolve_panel_explicit_pin_routes_only_named_agent() -> None:
    """An explicit codex pin routes that agent to codex; others stay claude."""
    resolved = pr.resolve_panel_providers(
        list(AGENT_NAMES),
        agent_providers={"code_reviewer": "codex"},
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        known_agents=AGENT_NAMES,
    )
    assert resolved["code_reviewer"].provider == "codex"
    assert resolved["code_reviewer"].degraded is False
    for agent in AGENT_NAMES:
        if agent != "code_reviewer":
            assert resolved[agent].provider == "claude"


def test_resolve_panel_alternate_degrades_when_no_other_provider() -> None:
    """Empty map + only claude available -> correctness subset degrades to claude."""
    resolved = pr.resolve_panel_providers(
        list(AGENT_NAMES),
        agent_providers={},
        implementer_provider="claude",
        available_providers=["claude"],
        known_agents=AGENT_NAMES,
    )
    for agent in CORRECTNESS:
        assert resolved[agent].provider == "claude"
        assert resolved[agent].degraded is True  # cross-model unavailable


# ---- panel_provider_routing: accessor behind --print-providers ----


def test_routing_empty_when_cross_model_off(monkeypatch: pytest.MonkeyPatch) -> None:
    """Cross-model OFF (no enabled, empty map) -> {} (all-claude, no routing)."""
    monkeypatch.setattr(
        review_mod, "_read_cross_model_config", lambda: ({}, False)
    )
    assert review_mod.panel_provider_routing(None) == {}


def test_routing_resolves_when_engaged(monkeypatch: pytest.MonkeyPatch) -> None:
    """Engaged config resolves every panel agent through the shared resolver."""
    monkeypatch.setattr(
        review_mod,
        "_read_cross_model_config",
        lambda: ({"silent_failure_hunter": "gemini"}, True),
    )
    monkeypatch.setattr(pr, "load_implementer_provider", lambda _sid: "claude")
    monkeypatch.setattr(pr, "available_provider_kinds", lambda: ["claude", "gemini"])

    routing = review_mod.panel_provider_routing("sess-1")
    assert set(routing) == set(AGENT_NAMES)
    assert routing["silent_failure_hunter"].provider == "gemini"


def test_routing_matches_panel_resolution_no_drift(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The skill's routing == what build_review_runner resolves (no drift)."""
    agent_providers = {"code_reviewer": "codex"}
    implementer, available = "claude", ["claude", "codex", "gemini"]

    monkeypatch.setattr(
        review_mod, "_read_cross_model_config", lambda: (agent_providers, True)
    )
    monkeypatch.setattr(pr, "load_implementer_provider", lambda _sid: implementer)
    monkeypatch.setattr(pr, "available_provider_kinds", lambda: available)

    skill_routing = {
        a: rp.provider for a, rp in review_mod.panel_provider_routing("s").items()
    }

    # What the panel resolves for the same inputs (the dispatch source of truth).
    base_prompts = {name: f"P:{name}" for name in AGENT_NAMES}
    panel_resolved = pr.resolve_panel_providers(
        list(base_prompts),
        agent_providers=agent_providers,
        implementer_provider=implementer,
        available_providers=available,
        known_agents=AGENT_NAMES,
    )
    panel_routing = {a: rp.provider for a, rp in panel_resolved.items()}

    assert skill_routing == panel_routing


# ---- the CLI flag ----


def test_print_providers_flag_off_emits_empty_json(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """`fno review --print-providers` with cross-model OFF prints `{}` and exits 0."""
    monkeypatch.setattr(
        review_mod, "_read_cross_model_config", lambda: ({}, False)
    )
    result = CliRunner().invoke(app, ["review", "--print-providers"])
    assert result.exit_code == 0
    assert json.loads(result.stdout.strip()) == {}


def test_print_providers_flag_emits_routing_json(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The flag serializes provider/degraded/reason per agent."""
    monkeypatch.setattr(
        review_mod,
        "_read_cross_model_config",
        lambda: ({"code_reviewer": "codex"}, True),
    )
    monkeypatch.setattr(pr, "load_implementer_provider", lambda _sid: "claude")
    monkeypatch.setattr(pr, "available_provider_kinds", lambda: ["claude", "codex"])

    result = CliRunner().invoke(app, ["review", "--print-providers"])
    assert result.exit_code == 0
    routing = json.loads(result.stdout.strip())
    assert routing["code_reviewer"]["provider"] == "codex"
    assert set(routing) == set(AGENT_NAMES)


# ---- session resolution parity with the panel (Gemini HIGH: no drift) ----


def test_resolve_session_id_prefers_explicit_then_state(tmp_path) -> None:
    state = tmp_path / "target-state.md"
    state.write_text("---\nsession_id: FROM-STATE\n---\n", encoding="utf-8")
    # explicit wins
    assert review_mod.resolve_session_id("EXPLICIT", state) == "EXPLICIT"
    # falls back to the state file
    assert review_mod.resolve_session_id(None, state) == "FROM-STATE"
    # neither -> None (caller defaults; routing -> claude implementer)
    assert review_mod.resolve_session_id(None, tmp_path / "missing.md") is None


def test_print_providers_resolves_session_from_state(
    tmp_path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """--print-providers without --session-id reads session from state, like the
    panel run, so the implementer-provider (excluded by `alternate`) matches."""
    state = tmp_path / "target-state.md"
    state.write_text("---\nsession_id: SESS-FROM-STATE\n---\n", encoding="utf-8")
    monkeypatch.setattr(
        review_mod,
        "_read_cross_model_config",
        lambda: ({"code_reviewer": "codex"}, True),
    )
    seen: dict[str, str] = {}

    def _fake_impl(sid: str) -> str:
        seen["sid"] = sid
        return "claude"

    monkeypatch.setattr(pr, "load_implementer_provider", _fake_impl)
    monkeypatch.setattr(pr, "available_provider_kinds", lambda: ["claude", "codex"])

    result = CliRunner().invoke(
        app, ["review", "--print-providers", "--state", str(state)]
    )
    assert result.exit_code == 0
    assert seen["sid"] == "SESS-FROM-STATE"
