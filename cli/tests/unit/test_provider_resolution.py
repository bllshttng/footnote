"""Tests for review.provider_resolution - per-agent cross-model routing.

Covers the pure ``resolve_agent_provider`` (AC1/AC2/AC4/AC5 logic) plus the
thin I/O wrappers (ledger read, available-kinds derivation).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.review.provider_resolution import (
    DEFAULT_ALTERNATE_AGENTS,
    ResolvedProvider,
    available_provider_kinds,
    load_implementer_provider,
    resolve_agent_provider,
)

PANEL = [
    "code_reviewer",
    "silent_failure_hunter",
    "integration_test_analyzer",
    "ux_flow_tester",
    "multi_device_checker",
    "type_design_analyzer",
]


# ---- AC1-HP: curated default cross-models the correctness subset ----


@pytest.mark.parametrize("agent", sorted(DEFAULT_ALTERNATE_AGENTS))
def test_default_correctness_agents_resolve_to_alternate(agent: str) -> None:
    """Unset map + claude implementer + codex available -> correctness->codex."""
    res = resolve_agent_provider(
        agent,
        agent_providers={},
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        known_agents=PANEL,
    )
    assert res == ResolvedProvider(provider="codex", degraded=False, reason=None)


@pytest.mark.parametrize(
    "agent",
    ["integration_test_analyzer", "ux_flow_tester", "multi_device_checker"],
)
def test_default_non_correctness_agents_stay_claude(agent: str) -> None:
    res = resolve_agent_provider(
        agent,
        agent_providers={},
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        known_agents=PANEL,
    )
    assert res.provider == "claude"
    assert res.degraded is False


# ---- AC2-HP: operator override pins providers ----


def test_operator_map_pins_gemini() -> None:
    mapping = {"ux_flow_tester": "gemini", "type_design_analyzer": "gemini"}
    res = resolve_agent_provider(
        "ux_flow_tester",
        agent_providers=mapping,
        implementer_provider="claude",
        available_providers=["claude", "codex", "gemini"],
        known_agents=PANEL,
    )
    assert res.provider == "gemini"
    assert res.degraded is False


def test_unnamed_agent_with_set_map_stays_claude() -> None:
    """A map that does not name this agent -> claude (not the curated default)."""
    mapping = {"ux_flow_tester": "gemini"}
    res = resolve_agent_provider(
        "code_reviewer",  # would be `alternate` under the *unset* default
        agent_providers=mapping,
        implementer_provider="claude",
        available_providers=["claude", "codex", "gemini"],
        known_agents=PANEL,
    )
    assert res.provider == "claude"
    assert res.degraded is False


def test_explicit_claude_pin() -> None:
    res = resolve_agent_provider(
        "code_reviewer",
        agent_providers={"code_reviewer": "claude"},
        implementer_provider="codex",
        available_providers=["claude", "codex", "gemini"],
        known_agents=PANEL,
    )
    assert res == ResolvedProvider(provider="claude", degraded=False, reason=None)


# ---- AC4-EDGE: single-provider degradation ----


def test_alternate_degrades_to_claude_when_single_provider() -> None:
    res = resolve_agent_provider(
        "code_reviewer",
        agent_providers={},
        implementer_provider="claude",
        available_providers=["claude"],  # nothing differs from implementer
        known_agents=PANEL,
    )
    assert res.provider == "claude"
    assert res.degraded is True
    assert res.reason and "cross-model unavailable" in res.reason


# ---- AC5-FR / Invariant: implementer is excluded from `alternate` ----


def test_alternate_excludes_implementer_provider() -> None:
    """implementer=codex -> alternate must NOT pick codex; first differing wins."""
    res = resolve_agent_provider(
        "code_reviewer",
        agent_providers={"code_reviewer": "alternate"},
        implementer_provider="codex",
        available_providers=["codex", "gemini", "claude"],
        known_agents=PANEL,
    )
    assert res.provider == "gemini"  # first available that differs from codex
    assert res.degraded is False


def test_alternate_picks_claude_when_it_differs_from_implementer() -> None:
    """A codex implementer reviewed on claude is a real cross-model (not degraded)."""
    res = resolve_agent_provider(
        "code_reviewer",
        agent_providers={},
        implementer_provider="codex",
        available_providers=["claude", "codex"],
        known_agents=PANEL,
    )
    assert res.provider == "claude"
    assert res.degraded is False


# ---- Failure Modes: unknown provider literal -> warn + claude ----


def test_unknown_provider_literal_degrades_to_claude() -> None:
    res = resolve_agent_provider(
        "code_reviewer",
        agent_providers={"code_reviewer": "grok"},
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        known_agents=PANEL,
    )
    assert res.provider == "claude"
    assert res.degraded is True
    assert res.reason and "grok" in res.reason


def test_unknown_agent_key_warns_and_resolves(
    caplog: pytest.LogCaptureFixture,
) -> None:
    """A map key naming an unknown agent still resolves (warn, no crash)."""
    res = resolve_agent_provider(
        "not_a_real_agent",
        agent_providers={"not_a_real_agent": "codex"},
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        known_agents=PANEL,
    )
    # Named in the map -> honors the pin even though it's an unknown agent.
    assert res.provider == "codex"


# ---- I/O wrapper: load_implementer_provider ----


def _write_ledger(tmp_path: Path, entries: list[dict]) -> Path:
    p = tmp_path / "ledger.json"
    p.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    return p


def test_load_implementer_provider_reads_kind(tmp_path: Path) -> None:
    led = _write_ledger(
        tmp_path,
        [
            {"session_id": "s1", "provider_id": "codex"},
            {"session_id": "s2", "provider_id": "gemini"},
        ],
    )
    assert load_implementer_provider("s2", ledger_path=led) == "gemini"


def test_load_implementer_provider_latest_match_wins(tmp_path: Path) -> None:
    led = _write_ledger(
        tmp_path,
        [
            {"session_id": "s1", "provider_id": "codex"},
            {"session_id": "s1", "provider_id": "claude"},
        ],
    )
    assert load_implementer_provider("s1", ledger_path=led) == "claude"


def test_load_implementer_provider_absent_assumes_claude(tmp_path: Path) -> None:
    assert load_implementer_provider("nope", ledger_path=tmp_path / "missing.json") == "claude"


def test_load_implementer_provider_no_match_assumes_claude(tmp_path: Path) -> None:
    led = _write_ledger(tmp_path, [{"session_id": "other", "provider_id": "codex"}])
    assert load_implementer_provider("s1", ledger_path=led) == "claude"


# ---- I/O wrapper: available_provider_kinds ----


def test_available_kinds_empty_providers_is_claude_only(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from fno.adapters.providers.model import ProvidersConfig

    monkeypatch.setattr(
        "fno.adapters.providers.loader.load_providers",
        lambda repo_root=None: ProvidersConfig(records=[], active=None),
    )
    assert available_provider_kinds(is_locked_out=lambda _id: False) == ["claude"]


def test_available_kinds_includes_unlocked_codex(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

    records = [
        ProviderRecord(
            id="codex-pro",
            name="Codex Pro",
            cli="codex",
            auth="api_key",
            env={"OPENAI_API_KEY": "x"},
        ),
        ProviderRecord(
            id="gem-1",
            name="Gemini",
            cli="gemini",
            auth="api_key",
            env={"GEMINI_API_KEY": "x"},
        ),
    ]
    monkeypatch.setattr(
        "fno.adapters.providers.loader.load_providers",
        lambda repo_root=None: ProvidersConfig(records=records, active=None),
    )
    # gem-1 locked out -> excluded; codex-pro available.
    kinds = available_provider_kinds(is_locked_out=lambda rid: rid == "gem-1")
    assert kinds == ["claude", "codex"]
