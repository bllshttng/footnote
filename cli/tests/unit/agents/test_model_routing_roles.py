"""Business-role compatibility tests for the existing model-routing lanes."""

from __future__ import annotations

from collections.abc import Callable

import pytest

from fno.agents import model_routing as mr
from fno.company.contracts import RoleRef
from fno.config import ConfigBlock, ModelRoutingBlock, SettingsModel
from fno.roles import (
    ResolvedRole,
    RoleResolutionBlocked,
    RoleResolutionReason,
    RoutingHint,
)


def _settings(**block_kwargs: object) -> SettingsModel:
    block = ModelRoutingBlock(**block_kwargs)  # type: ignore[arg-type]
    return SettingsModel(  # type: ignore[call-arg]
        config=ConfigBlock(model_routing=block)
    )


def _not_found(name: str) -> RoleResolutionBlocked:
    return RoleResolutionBlocked(
        role=RoleRef(id=name, function_id="test-function"),
        reason=RoleResolutionReason.NOT_FOUND,
        reference=name,
    )


def _resolved(*, provider: str | None = None, model: str | None = None) -> ResolvedRole:
    return ResolvedRole.model_construct(
        routing_projection=RoutingHint(provider=provider, model=model)
    )


OPENAI_PROVIDER = {
    "oai": {
        "protocol": "openai",
        "base_url": "https://example.test/v1",
        "api_key_env": "OPENAI_API_KEY",
    }
}


@pytest.mark.parametrize(
    ("role", "settings", "env"),
    [
        ("unknown", _settings(), {"ZAI_API_KEY": "zai-key"}),
        ("coordinate", _settings(), {"ZAI_API_KEY": "zai-key"}),
        (
            "build",
            _settings(roles={"build": "zai/glm-build"}),
            {"ZAI_API_KEY": "zai-key"},
        ),
        (
            "implement",
            _settings(roles={"implement": "zai/glm-unsafe"}),
            {"ZAI_API_KEY": "zai-key"},
        ),
        ("coordinate", _settings(enabled=False), {"ZAI_API_KEY": "zai-key"}),
        ("build", _settings(roles={"build": "malformed"}), {"ZAI_API_KEY": "zai-key"}),
        ("coordinate", _settings(), {}),
        ("build", _settings(roles={"build": "missing/model"}), {}),
        (
            "build",
            _settings(
                providers={
                    "wrong-protocol": {
                        "protocol": "openai",
                        "base_url": "https://example.test/v1",
                        "api_key_env": "OPENAI_API_KEY",
                    }
                },
                roles={"build": "wrong-protocol/model"},
            ),
            {"OPENAI_API_KEY": "openai-key"},
        ),
    ],
)
@pytest.mark.parametrize("resolver", [mr.resolve_route, mr.resolve_codex_route])
def test_ac_r4_compat_not_found_is_a_golden_legacy_delegate(
    resolver: Callable[..., object],
    role: str,
    settings: SettingsModel,
    env: dict[str, str],
) -> None:
    legacy_notices: list[str] = []
    bridge_notices: list[str] = []

    legacy = resolver(role, settings=settings, env=env, notice=legacy_notices.append)
    bridged = resolver(
        role,
        settings=settings,
        env=env,
        notice=bridge_notices.append,
        business_lookup=_not_found,
    )

    assert bridged == legacy
    assert bridge_notices == legacy_notices


def test_ac_r4_compat_spawn_route_delegates_not_found_without_drift(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("ZAI_API_KEY", "zai-key")
    monkeypatch.setattr(mr, "_routing_block", lambda settings: _settings().model_routing)

    legacy_notices: list[str] = []
    bridge_notices: list[str] = []
    legacy = mr.resolve_spawn_route("coordinate", notice=legacy_notices.append)
    bridged = mr.resolve_spawn_route(
        "coordinate", notice=bridge_notices.append, business_lookup=_not_found
    )

    assert bridged == legacy
    assert bridge_notices == legacy_notices


def test_resolved_business_role_changes_only_claude_provider_and_model() -> None:
    settings = _settings(
        providers={
            "deepseek": {
                "protocol": "anthropic",
                "base_url": "https://deepseek.test/anthropic",
                "api_key_env": "DEEPSEEK_API_KEY",
            }
        },
        roles={"build": "zai/legacy-model"},
        extra_env={"ROUTE_MARKER": "preserved"},
    )

    route = mr.resolve_route(
        "build",
        settings=settings,
        env={"ZAI_API_KEY": "zai-key", "DEEPSEEK_API_KEY": "deepseek-key"},
        business_lookup=lambda _: _resolved(provider="deepseek", model="business-model"),
    )

    assert route is not None
    assert route["ANTHROPIC_BASE_URL"] == "https://deepseek.test/anthropic"
    assert route["ANTHROPIC_AUTH_TOKEN"] == "deepseek-key"
    assert route["ANTHROPIC_MODEL"] == "business-model"
    assert route["ROUTE_MARKER"] == "preserved"
    assert not any(
        word in " ".join(route).lower()
        for word in ("manifest", "capability", "authority", "context", "approval", "delivery")
    )


def test_resolved_business_role_routes_the_codex_lane() -> None:
    route = mr.resolve_codex_route(
        "publisher",
        settings=_settings(providers=OPENAI_PROVIDER),
        env={"OPENAI_API_KEY": "openai-key"},
        business_lookup=lambda _: _resolved(provider="oai", model="gpt-business"),
    )

    assert route is not None
    assert route.env == {"OPENAI_API_KEY": "openai-key"}
    assert "model='gpt-business'" in " ".join(route.config_args)


def test_incomplete_projection_composes_only_with_a_deterministic_legacy_target() -> None:
    route = mr.resolve_route(
        "build",
        settings=_settings(roles={"build": "zai/legacy-model"}),
        env={"ZAI_API_KEY": "zai-key"},
        business_lookup=lambda _: _resolved(model="business-model"),
    )

    assert route is not None
    assert route["ANTHROPIC_MODEL"] == "business-model"


def test_incomplete_projection_without_legacy_target_names_the_missing_field() -> None:
    with pytest.raises(mr.BusinessRoleRoutingProjectionError, match="missing model"):
        mr.resolve_route(
            "publisher",
            settings=_settings(),
            env={"ZAI_API_KEY": "zai-key"},
            business_lookup=lambda _: _resolved(provider="zai"),
        )


def test_invalid_business_manifest_is_distinct_from_routing_fallback() -> None:
    blocked = RoleResolutionBlocked(
        role=RoleRef(id="publisher", function_id="communications"),
        reason=RoleResolutionReason.INVALID_MANIFEST,
        source_id="company/roles.toml",
        reference="publisher",
        detail="invalid document",
    )

    with pytest.raises(mr.BusinessRoleResolutionBlockedError) as caught:
        mr.resolve_route(
            "publisher",
            settings=_settings(),
            env={},
            business_lookup=lambda _: blocked,
        )

    assert caught.value.result is blocked
    assert caught.value.result.reason is RoleResolutionReason.INVALID_MANIFEST


@pytest.mark.parametrize("resolver", [mr.resolve_route, mr.resolve_codex_route])
@pytest.mark.parametrize("role", sorted(mr.PROTECTED_ROLES))
def test_protected_business_names_short_circuit_before_lookup(
    resolver: Callable[..., object], role: str
) -> None:
    called = False

    def lookup(_: str) -> ResolvedRole:
        nonlocal called
        called = True
        return _resolved(provider="oai", model="unsafe")

    assert (
        resolver(
            role,
            settings=_settings(providers=OPENAI_PROVIDER, roles={role: "oai/unsafe"}),
            env={"OPENAI_API_KEY": "openai-key"},
            business_lookup=lookup,
        )
        is None
    )
    assert called is False


def test_manifest_existence_guard_distinguishes_not_found_from_resolved() -> None:
    settings = _settings(
        providers={
            "deepseek": {
                "protocol": "anthropic",
                "base_url": "https://deepseek.test/anthropic",
                "api_key_env": "DEEPSEEK_API_KEY",
            }
        },
        roles={"build": "zai/legacy-model"},
    )
    env = {"ZAI_API_KEY": "zai-key", "DEEPSEEK_API_KEY": "deepseek-key"}

    missing = mr.resolve_route("build", settings=settings, env=env, business_lookup=_not_found)
    exists = mr.resolve_route(
        "build",
        settings=settings,
        env=env,
        business_lookup=lambda _: _resolved(provider="deepseek", model="business-model"),
    )

    assert missing is not None and exists is not None
    assert missing["ANTHROPIC_BASE_URL"] == mr.DEFAULT_ZAI_BASE_URL
    assert missing["ANTHROPIC_MODEL"] == "legacy-model"
    assert exists["ANTHROPIC_BASE_URL"] == "https://deepseek.test/anthropic"
    assert exists["ANTHROPIC_MODEL"] == "business-model"


def test_explicit_peer_route_remains_separate_from_business_lookup() -> None:
    route = mr.resolve_explicit_route(
        "zai", "peer-model", settings=_settings(), env={"ZAI_API_KEY": "zai-key"}
    )

    assert route is not None
    assert route["ANTHROPIC_MODEL"] == "peer-model"
