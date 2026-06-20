"""Unit tests for role-based model routing (x-d2fe).

Covers the resolver contract from the plan's acceptance criteria, on the
provider-registry schema (config.model_routing.providers / roles / extra_env):

- AC1-HP  routed role + valid key -> provider base_url + token + model (all tiers)
- AC2-HP  production role (implement) -> None (primary model untouched)
- AC2-INV no role -> None (regression guard: behaves as today)
- AC3-HP  config roles map changes the model / provider for a role
- AC4-FR  no key -> None + one-line notice (fail-safe, never raises)
- guard   protected roles never route, even via config
- multi   a second provider (deepseek) routes via its own Anthropic endpoint
- proto   a non-anthropic-protocol provider is skipped for the claude lane
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional

import pytest

from fno.agents import model_routing as mr
from fno.config import ConfigBlock, ModelRoutingBlock, SettingsModel


def _settings(**block_kwargs: object) -> SettingsModel:
    """Build a SettingsModel carrying a model_routing block for tests."""
    return SettingsModel(
        config=ConfigBlock(model_routing=ModelRoutingBlock(**block_kwargs))
    )


def _collector() -> tuple[list[str], "object"]:
    notes: list[str] = []
    return notes, notes.append


# ---------------------------------------------------------------------------
# AC1-HP: routed spawn -> secondary provider (z.ai default), all model tiers
# ---------------------------------------------------------------------------


def test_consolidate_routes_to_zai_anthropic_endpoint() -> None:
    route = mr.resolve_route(
        "consolidate", settings=_settings(), env={"ZAI_API_KEY": "zk-secret"}
    )
    assert route is not None
    # The Anthropic-compatible endpoint (a claude worker speaks Anthropic).
    assert route["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/anthropic"
    assert route["ANTHROPIC_AUTH_TOKEN"] == "zk-secret"
    # All tiers set so the whole worker (incl. background haiku) stays on GLM.
    assert route["ANTHROPIC_MODEL"] == "glm-5.2"
    assert route["ANTHROPIC_DEFAULT_OPUS_MODEL"] == "glm-5.2"
    assert route["ANTHROPIC_DEFAULT_SONNET_MODEL"] == "glm-5.2"
    assert route["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-5.2"


@pytest.mark.parametrize("role", ["coordinate", "tidy", "orient", "consolidate"])
def test_all_default_routed_roles_route_when_keyed(role: str) -> None:
    route = mr.resolve_route(role, settings=_settings(), env={"ZAI_API_KEY": "k"})
    assert route is not None
    assert route["ANTHROPIC_MODEL"] == "glm-5.2"


def test_role_is_case_and_space_insensitive() -> None:
    route = mr.resolve_route(
        "  Consolidate ", settings=_settings(), env={"ZAI_API_KEY": "k"}
    )
    assert route is not None


# ---------------------------------------------------------------------------
# AC2: production roles + no-role are untouched
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("role", ["implement", "review-verdict"])
def test_production_roles_return_none(role: str) -> None:
    assert (
        mr.resolve_route(role, settings=_settings(), env={"ZAI_API_KEY": "k"})
        is None
    )


@pytest.mark.parametrize("role", [None, "", "   "])
def test_no_role_returns_none(role: Optional[str]) -> None:
    assert mr.resolve_route(role, settings=_settings(), env={"ZAI_API_KEY": "k"}) is None


def test_unknown_role_returns_none() -> None:
    assert (
        mr.resolve_route("compile", settings=_settings(), env={"ZAI_API_KEY": "k"})
        is None
    )


# ---------------------------------------------------------------------------
# AC3: config roles map (per-role provider,model)
# ---------------------------------------------------------------------------


def test_roles_map_changes_model_for_a_role() -> None:
    route = mr.resolve_route(
        "tidy",
        settings=_settings(roles={"tidy": "zai,glm-4.7"}),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["ANTHROPIC_MODEL"] == "glm-4.7"


def test_disabled_block_returns_none_even_for_routed_role() -> None:
    assert (
        mr.resolve_route(
            "tidy", settings=_settings(enabled=False), env={"ZAI_API_KEY": "k"}
        )
        is None
    )


def test_extra_env_is_merged_and_can_override_a_tier() -> None:
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(
            extra_env={
                "API_TIMEOUT_MS": "3000000",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.7",
            }
        ),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["API_TIMEOUT_MS"] == "3000000"
    # extra_env is merged last, so it wins over the per-role model for that tier.
    assert route["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-4.7"
    assert route["ANTHROPIC_MODEL"] == "glm-5.2"


# ---------------------------------------------------------------------------
# Multi-provider: a second provider (deepseek) routes via its own endpoint
# ---------------------------------------------------------------------------


def test_second_provider_routes_via_its_own_anthropic_endpoint() -> None:
    route = mr.resolve_route(
        "tidy",
        settings=_settings(
            providers={
                "deepseek": {
                    "protocol": "anthropic",
                    "base_url": "https://api.deepseek.com/anthropic",
                    "api_key_env": "DEEPSEEK_API_KEY",
                }
            },
            roles={"tidy": "deepseek,deepseek-chat"},
        ),
        env={"DEEPSEEK_API_KEY": "dsk"},
    )
    assert route is not None
    assert route["ANTHROPIC_BASE_URL"] == "https://api.deepseek.com/anthropic"
    assert route["ANTHROPIC_AUTH_TOKEN"] == "dsk"
    assert route["ANTHROPIC_MODEL"] == "deepseek-chat"


def test_provider_entry_can_override_builtin_zai_base_url() -> None:
    route = mr.resolve_route(
        "tidy",
        settings=_settings(
            providers={"zai": {"base_url": "https://api.z.ai/api/coding/paas/v4"}},
        ),
        env={"ZAI_API_KEY": "k"},
    )
    # The built-in zai protocol/api_key_env survive; only base_url is overridden.
    assert route is not None
    assert route["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/coding/paas/v4"


def test_unknown_provider_in_roles_falls_back() -> None:
    notes, sink = _collector()
    route = mr.resolve_route(
        "tidy",
        settings=_settings(roles={"tidy": "mystery,model-x"}),
        env={"ZAI_API_KEY": "k"},
        notice=sink,
    )
    assert route is None
    assert notes


def test_non_anthropic_protocol_provider_skipped_for_claude_lane() -> None:
    notes, sink = _collector()
    route = mr.resolve_route(
        "tidy",
        settings=_settings(
            providers={
                "oai": {
                    "protocol": "openai",
                    "base_url": "https://api.z.ai/api/coding/paas/v4",
                    "api_key_env": "ZAI_API_KEY",
                }
            },
            roles={"tidy": "oai,glm-5.2"},
        ),
        env={"ZAI_API_KEY": "k"},
        notice=sink,
    )
    assert route is None
    assert any("protocol" in n for n in notes)


# ---------------------------------------------------------------------------
# AC4-FR: fail-safe fallback (no key -> None + notice, never raises)
# ---------------------------------------------------------------------------


def test_missing_key_falls_back_with_notice() -> None:
    notes, sink = _collector()
    route = mr.resolve_route("coordinate", settings=_settings(), env={}, notice=sink)
    assert route is None
    assert len(notes) == 1
    assert "coordinate" in notes[0]


def test_missing_key_never_raises_without_notice() -> None:
    assert mr.resolve_route("coordinate", settings=_settings(), env={}) is None


def test_env_file_supplies_key_when_process_env_absent(tmp_path: Path) -> None:
    envf = tmp_path / "modelkit.env"
    envf.write_text("# comment\nZAI_API_KEY=from-file\n", encoding="utf-8")
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(providers={"zai": {"api_key_file": str(envf)}}),
        env={},
    )
    assert route is not None
    assert route["ANTHROPIC_AUTH_TOKEN"] == "from-file"


def test_process_env_wins_over_env_file(tmp_path: Path) -> None:
    envf = tmp_path / "modelkit.env"
    envf.write_text("ZAI_API_KEY=from-file\n", encoding="utf-8")
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(providers={"zai": {"api_key_file": str(envf)}}),
        env={"ZAI_API_KEY": "from-process"},
    )
    assert route is not None
    assert route["ANTHROPIC_AUTH_TOKEN"] == "from-process"


def test_env_file_tolerates_export_and_spaces(tmp_path: Path) -> None:
    envf = tmp_path / "modelkit.env"
    envf.write_text("export ZAI_API_KEY = spaced-and-exported\n", encoding="utf-8")
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(providers={"zai": {"api_key_file": str(envf)}}),
        env={},
    )
    assert route is not None
    assert route["ANTHROPIC_AUTH_TOKEN"] == "spaced-and-exported"


def test_missing_env_file_is_not_fatal() -> None:
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(providers={"zai": {"api_key_file": "/no/such/file.env"}}),
        env={},
    )
    assert route is None


def test_custom_key_env_var_name() -> None:
    route = mr.resolve_route(
        "orient",
        settings=_settings(providers={"zai": {"api_key_env": "MY_GLM_KEY"}}),
        env={"MY_GLM_KEY": "alt"},
    )
    assert route is not None
    assert route["ANTHROPIC_AUTH_TOKEN"] == "alt"


# ---------------------------------------------------------------------------
# Hard quality guard: protected roles never route, even via config
# ---------------------------------------------------------------------------


def test_config_cannot_route_protected_role() -> None:
    route = mr.resolve_route(
        "implement",
        settings=_settings(roles={"implement": "zai,glm-5.2"}),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is None


def test_protected_roles_constant_is_locked() -> None:
    assert "implement" in mr.PROTECTED_ROLES
    assert "review-verdict" in mr.PROTECTED_ROLES


def test_config_defaults_match_module_constants() -> None:
    # Drift guard: the built-in zai endpoint + default model must agree with the
    # module fallback constants.
    assert mr._DEFAULT_PROVIDERS["zai"]["base_url"] == mr.DEFAULT_ZAI_BASE_URL
    assert mr.DEFAULT_ZAI_BASE_URL == "https://api.z.ai/api/anthropic"
    assert mr.DEFAULT_SECONDARY_MODEL == "glm-5.2"
