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
    # opus/sonnet/default stay on the role model; the background haiku tier
    # drops to the provider's cheaper glm-4.5-air (still the same zai provider).
    assert route["ANTHROPIC_MODEL"] == "glm-5.2"
    assert route["ANTHROPIC_DEFAULT_OPUS_MODEL"] == "glm-5.2"
    assert route["ANTHROPIC_DEFAULT_SONNET_MODEL"] == "glm-5.2"
    assert route["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-4.5-air"


@pytest.mark.parametrize(
    "role", ["coordinate", "tidy", "orient", "consolidate", "post-merge"]
)
def test_all_default_routed_roles_route_when_keyed(role: str) -> None:
    route = mr.resolve_route(role, settings=_settings(), env={"ZAI_API_KEY": "k"})
    assert route is not None
    assert route["ANTHROPIC_MODEL"] == "glm-5.2"


def test_post_merge_is_routable_not_protected() -> None:
    # Item 3: the post-merge ritual routes to GLM by default (judgment-light),
    # but must NOT be a protected role (it stays overridable / fail-safe).
    assert "post-merge" in mr.DEFAULT_ROUTED_ROLES
    assert "post-merge" not in mr.PROTECTED_ROLES


def test_post_merge_fails_safe_without_key() -> None:
    # No key -> primary Anthropic model (the ritual still fires, on Anthropic).
    assert mr.resolve_route("post-merge", settings=_settings(), env={}) is None


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
    assert mr._DEFAULT_PROVIDERS["zai"]["haiku_model"] == mr.DEFAULT_ZAI_HAIKU_MODEL
    assert mr.DEFAULT_ZAI_HAIKU_MODEL == "glm-4.5-air"


# ---------------------------------------------------------------------------
# Item 1: per-tier routed model (background haiku -> cheaper glm-4.5-air)
# ---------------------------------------------------------------------------


def test_zai_haiku_tier_defaults_to_cheaper_model() -> None:
    route = mr.resolve_route(
        "consolidate", settings=_settings(), env={"ZAI_API_KEY": "k"}
    )
    assert route is not None
    assert route["ANTHROPIC_MODEL"] == "glm-5.2"
    assert route["ANTHROPIC_DEFAULT_OPUS_MODEL"] == "glm-5.2"
    assert route["ANTHROPIC_DEFAULT_SONNET_MODEL"] == "glm-5.2"
    assert route["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-4.5-air"


def test_per_provider_haiku_override_wins_over_builtin_default() -> None:
    route = mr.resolve_route(
        "tidy",
        settings=_settings(providers={"zai": {"haiku_model": "glm-tiny"}}),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-tiny"
    # opus/sonnet/default unaffected by the haiku override.
    assert route["ANTHROPIC_MODEL"] == "glm-5.2"


def test_provider_without_haiku_override_keeps_role_model_on_haiku() -> None:
    # deepseek has no haiku_model, so the haiku tier keeps the role model
    # (no regression to an empty/invalid id).
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
    assert route["ANTHROPIC_MODEL"] == "deepseek-chat"
    assert route["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "deepseek-chat"


# ---------------------------------------------------------------------------
# Item 2: carry the [1m] 1M-context compact window automatically
# ---------------------------------------------------------------------------


def test_one_m_suffix_injects_compact_window() -> None:
    route = mr.resolve_route(
        "tidy",
        settings=_settings(roles={"tidy": "zai,glm-5.2[1m]"}),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["CLAUDE_CODE_AUTO_COMPACT_WINDOW"] == "1000000"


def test_non_one_m_model_injects_no_compact_window() -> None:
    route = mr.resolve_route(
        "consolidate", settings=_settings(), env={"ZAI_API_KEY": "k"}
    )
    assert route is not None
    assert "CLAUDE_CODE_AUTO_COMPACT_WINDOW" not in route


def test_extra_env_compact_window_wins_over_injection() -> None:
    route = mr.resolve_route(
        "tidy",
        settings=_settings(
            roles={"tidy": "zai,glm-5.2[1m]"},
            extra_env={"CLAUDE_CODE_AUTO_COMPACT_WINDOW": "500000"},
        ),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["CLAUDE_CODE_AUTO_COMPACT_WINDOW"] == "500000"
