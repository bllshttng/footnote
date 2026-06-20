"""Unit tests for role-based model routing (x-d2fe).

Covers the resolver contract from the plan's acceptance criteria:

- AC1-HP  cheap role + valid key -> z.ai base_url + token + glm-5.2
- AC2-HP  production role (implement) -> None (default model untouched)
- AC2-INV no role -> None (regression guard: behaves as today)
- AC3-HP  config override changes the model for a cheap role
- AC4-FR  no z.ai key -> None + one-line notice (fail-safe, never raises)
- guard   protected roles never resolve cheap, even via config override
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
# AC1-HP: cheap spawn routes to z.ai
# ---------------------------------------------------------------------------


def test_consolidate_with_key_routes_to_zai_glm() -> None:
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(),
        env={"ZAI_API_KEY": "zk-secret"},
    )
    assert route is not None
    assert route["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/coding/paas/v4"
    assert route["ANTHROPIC_AUTH_TOKEN"] == "zk-secret"
    assert route["ANTHROPIC_MODEL"] == "glm-5.2"


@pytest.mark.parametrize("role", ["coordinate", "tidy", "orient", "consolidate"])
def test_all_cheap_roles_route_when_keyed(role: str) -> None:
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
    # An unrecognized role is not cheap: fall back to the default model.
    assert (
        mr.resolve_route("compile", settings=_settings(), env={"ZAI_API_KEY": "k"})
        is None
    )


# ---------------------------------------------------------------------------
# AC3: config override
# ---------------------------------------------------------------------------


def test_override_changes_model_for_cheap_role() -> None:
    route = mr.resolve_route(
        "tidy",
        settings=_settings(overrides={"tidy": "zai,glm-4.5-air"}),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["ANTHROPIC_MODEL"] == "glm-4.5-air"


def test_default_model_is_configurable() -> None:
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(default_model="glm-5.2-pro"),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["ANTHROPIC_MODEL"] == "glm-5.2-pro"


def test_base_url_is_configurable() -> None:
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(zai_base_url="https://example.test/api/anthropic"),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["ANTHROPIC_BASE_URL"] == "https://example.test/api/anthropic"


def test_per_role_override_beats_default_model() -> None:
    # default_model sets the baseline; an explicit override still wins per role.
    route = mr.resolve_route(
        "tidy",
        settings=_settings(
            default_model="glm-5.2", overrides={"tidy": "zai,glm-4.5-air"}
        ),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["ANTHROPIC_MODEL"] == "glm-4.5-air"


def test_config_defaults_match_module_constants() -> None:
    # Drift guard: the schema defaults and the module fallback constants must agree.
    block = ModelRoutingBlock()
    assert block.zai_base_url == mr.DEFAULT_ZAI_BASE_URL
    assert block.default_model == mr.DEFAULT_CHEAP_MODEL


def test_disabled_block_returns_none_even_for_cheap_role() -> None:
    assert (
        mr.resolve_route(
            "tidy", settings=_settings(enabled=False), env={"ZAI_API_KEY": "k"}
        )
        is None
    )


def test_configurable_key_env_var_name() -> None:
    route = mr.resolve_route(
        "orient",
        settings=_settings(zai_key_env="MY_GLM_KEY"),
        env={"MY_GLM_KEY": "alt"},
    )
    assert route is not None
    assert route["ANTHROPIC_AUTH_TOKEN"] == "alt"


# ---------------------------------------------------------------------------
# AC4-FR: fail-safe fallback (no key -> None + notice, never raises)
# ---------------------------------------------------------------------------


def test_missing_key_falls_back_with_notice() -> None:
    notes, sink = _collector()
    route = mr.resolve_route(
        "coordinate", settings=_settings(), env={}, notice=sink
    )
    assert route is None
    assert len(notes) == 1
    assert "coordinate" in notes[0]


def test_missing_key_never_raises_without_notice() -> None:
    # No notice callback supplied: must still degrade quietly to None.
    assert mr.resolve_route("coordinate", settings=_settings(), env={}) is None


def test_env_file_supplies_key_when_process_env_absent(tmp_path: Path) -> None:
    envf = tmp_path / "modelkit.env"
    envf.write_text("# comment\nZAI_API_KEY=from-file\n", encoding="utf-8")
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(zai_env_file=str(envf)),
        env={},
    )
    assert route is not None
    assert route["ANTHROPIC_AUTH_TOKEN"] == "from-file"


def test_process_env_wins_over_env_file(tmp_path: Path) -> None:
    envf = tmp_path / "modelkit.env"
    envf.write_text("ZAI_API_KEY=from-file\n", encoding="utf-8")
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(zai_env_file=str(envf)),
        env={"ZAI_API_KEY": "from-process"},
    )
    assert route is not None
    assert route["ANTHROPIC_AUTH_TOKEN"] == "from-process"


def test_env_file_tolerates_export_and_spaces(tmp_path: Path) -> None:
    envf = tmp_path / "modelkit.env"
    envf.write_text("export ZAI_API_KEY = spaced-and-exported\n", encoding="utf-8")
    route = mr.resolve_route(
        "consolidate", settings=_settings(zai_env_file=str(envf)), env={}
    )
    assert route is not None
    assert route["ANTHROPIC_AUTH_TOKEN"] == "spaced-and-exported"


def test_missing_env_file_is_not_fatal() -> None:
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(zai_env_file="/no/such/file.env"),
        env={},
    )
    assert route is None  # no key anywhere -> fail-safe None


# ---------------------------------------------------------------------------
# Hard quality guard: protected roles never cheap, even via override
# ---------------------------------------------------------------------------


def test_override_cannot_route_protected_role_cheap() -> None:
    # A malicious/erroneous config that tries to send `implement` to GLM
    # must be refused: the verdict/diff model is never cheap.
    route = mr.resolve_route(
        "implement",
        settings=_settings(overrides={"implement": "zai,glm-5.2"}),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is None


def test_protected_roles_constant_is_locked() -> None:
    assert "implement" in mr.PROTECTED_ROLES
    assert "review-verdict" in mr.PROTECTED_ROLES


def test_unwired_provider_in_override_falls_back() -> None:
    # Only the z.ai lane is wired in v1; an override naming another cheap
    # provider degrades to the default model rather than erroring.
    notes, sink = _collector()
    route = mr.resolve_route(
        "tidy",
        settings=_settings(overrides={"tidy": "gemini,flash"}),
        env={"ZAI_API_KEY": "k"},
        notice=sink,
    )
    assert route is None
    assert notes  # a notice was emitted
