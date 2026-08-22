"""Business-role compatibility tests for the existing model-routing lanes."""

from __future__ import annotations

from collections.abc import Callable
import json
from pathlib import Path

import pytest

from fno.agents import model_routing as mr
from fno.company.contracts import FunctionRef, RoleRef
from fno.config import ConfigBlock, ModelRoutingBlock, SettingsModel
from fno.roles import (
    AuthorityCeiling,
    DeliveryPolicy,
    ResolvedRole,
    ReviewPolicy,
    RoleDefinitionSource,
    RoleLayer,
    RoleManifest,
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


def _write_business_role(
    root: Path,
    *,
    provider: str | None,
    model: str | None,
    role_id: str = "publisher",
    function_id: str = "communications",
    layer: RoleLayer = RoleLayer.COMPANY,
    revision: str = "snapshot-1",
    authority: AuthorityCeiling = AuthorityCeiling.INTERNAL,
) -> Path:
    role = RoleRef(id=role_id, function_id=function_id)
    source = RoleDefinitionSource(
        layer=layer,
        source_id=f"{layer.value}/{role_id}.json",
        snapshot_revision=revision,
        role=role,
        manifest=RoleManifest(
            role=role,
            function=FunctionRef(id=role.function_id),
            mission="Publish one bounded artifact.",
            deliverable_kinds=("brief",),
            authority_ceiling=authority,
            review_policy=ReviewPolicy(required=True, minimum_reviewers=1),
            delivery_policy=DeliveryPolicy(required_evidence=("artifact",)),
            default_topology="direct",
            routing_hint=(
                RoutingHint(provider=provider, model=model)
                if provider is not None or model is not None
                else None
            ),
        ),
    )
    path = root / layer.value / f"{role_id}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(source.model_dump(mode="json")), encoding="utf-8")
    return path


def _write_corrupt_role(root: Path) -> Path:
    path = root / "project" / "broken.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{not-json", encoding="utf-8")
    return path


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
    # The stamp rides with the codex lane too (x-c703): without it a routed
    # codex worker resolves provider "unknown" and ignores its subagent budget.
    assert route.env == {"OPENAI_API_KEY": "openai-key", "FNO_ROUTE_PROVIDER": "oai"}
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
def test_disabled_routing_not_found_remains_exact_legacy_none(
    resolver: Callable[..., object],
) -> None:
    settings = _settings(enabled=False)
    legacy_notices: list[str] = []
    bridge_notices: list[str] = []

    legacy = resolver("publisher", settings=settings, env={}, notice=legacy_notices.append)
    bridged = resolver(
        "publisher",
        settings=settings,
        env={},
        notice=bridge_notices.append,
        business_lookup=_not_found,
    )

    assert legacy is None
    assert bridged is None
    assert bridge_notices == legacy_notices


@pytest.mark.parametrize("resolver", [mr.resolve_route, mr.resolve_codex_route])
def test_disabled_routing_invalid_manifest_still_fails_closed(
    resolver: Callable[..., object],
) -> None:
    blocked = RoleResolutionBlocked(
        role=RoleRef(id="publisher", function_id="communications"),
        reason=RoleResolutionReason.INVALID_MANIFEST,
        source_id="company/roles.toml",
        reference="publisher",
    )

    with pytest.raises(mr.BusinessRoleResolutionBlockedError) as caught:
        resolver(
            "publisher",
            settings=_settings(enabled=False),
            env={},
            business_lookup=lambda _: blocked,
        )

    assert caught.value.result is blocked


@pytest.mark.parametrize("resolver", [mr.resolve_route, mr.resolve_codex_route])
def test_disabled_routing_resolved_business_role_remains_disabled(
    resolver: Callable[..., object],
) -> None:
    called = False

    def lookup(_: str) -> ResolvedRole:
        nonlocal called
        called = True
        return _resolved(provider="oai", model="gpt-business")

    assert (
        resolver(
            "publisher",
            settings=_settings(enabled=False, providers=OPENAI_PROVIDER),
            env={"OPENAI_API_KEY": "openai-key"},
            business_lookup=lookup,
        )
        is None
    )
    assert called is True


@pytest.mark.parametrize("resolver", [mr.resolve_route, mr.resolve_codex_route])
@pytest.mark.parametrize("role", sorted(mr.PROTECTED_ROLES))
def test_protected_business_names_validate_manifest_but_keep_primary_route(
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
    assert called is True


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


def test_default_production_lookup_projects_manifest_through_spawn_route(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_business_role(root, provider="zai", model="business-model")
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))
    monkeypatch.setenv("ZAI_API_KEY", "zai-key")
    monkeypatch.setattr(mr, "_routing_block", lambda settings: _settings().model_routing)

    route = mr.resolve_spawn_route("publisher")

    assert route is not None
    assert route["ANTHROPIC_BASE_URL"] == mr.DEFAULT_ZAI_BASE_URL
    assert route["ANTHROPIC_MODEL"] == "business-model"


def test_default_production_lookup_projects_manifest_through_codex_route(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_business_role(root, provider="oai", model="gpt-business")
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))

    route = mr.resolve_codex_route(
        "publisher",
        settings=_settings(providers=OPENAI_PROVIDER),
        env={"OPENAI_API_KEY": "openai-key"},
    )

    assert route is not None
    # The stamp rides with the codex lane too (x-c703): without it a routed
    # codex worker resolves provider "unknown" and ignores its subagent budget.
    assert route.env == {"OPENAI_API_KEY": "openai-key", "FNO_ROUTE_PROVIDER": "oai"}
    assert "model='gpt-business'" in " ".join(route.config_args)


@pytest.mark.parametrize("resolver", [mr.resolve_route, mr.resolve_codex_route])
def test_business_manifest_without_optional_routing_hint_uses_primary_route(
    resolver: Callable[..., object],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_business_role(root, provider=None, model=None)
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))

    assert resolver("publisher", settings=_settings(), env={}) is None


@pytest.mark.parametrize(
    ("resolver", "settings", "env"),
    [
        (
            mr.resolve_route,
            _settings(roles={"publisher": "zai/legacy-model"}),
            {"ZAI_API_KEY": "zai-key"},
        ),
        (
            mr.resolve_codex_route,
            _settings(
                providers=OPENAI_PROVIDER,
                roles={"publisher": "oai/legacy-model"},
            ),
            {"OPENAI_API_KEY": "openai-key"},
        ),
    ],
)
def test_business_manifest_without_routing_hint_ignores_same_name_legacy_route(
    resolver: Callable[..., object],
    settings: SettingsModel,
    env: dict[str, str],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_business_role(root, provider=None, model=None)
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))

    assert resolver("publisher", settings=settings, env=env) is None


def test_business_manifest_lookup_preserves_exact_mixed_case_role_identity(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_business_role(
        root,
        role_id="Publisher",
        provider="zai",
        model="business-model",
    )
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))

    route = mr.resolve_route(
        "Publisher",
        settings=_settings(),
        env={"ZAI_API_KEY": "zai-key"},
    )

    assert route is not None
    assert route["ANTHROPIC_MODEL"] == "business-model"


def test_codex_business_lookup_preserves_exact_mixed_case_role_identity(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_business_role(
        root,
        role_id="Publisher",
        provider="oai",
        model="gpt-business",
    )
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))

    route = mr.resolve_codex_route(
        "Publisher",
        settings=_settings(providers=OPENAI_PROVIDER),
        env={"OPENAI_API_KEY": "openai-key"},
    )

    assert route is not None
    assert "model='gpt-business'" in " ".join(route.config_args)


def test_default_lookup_uses_fixed_precedence_and_accepts_tightening_overlay(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_business_role(
        root,
        provider="zai",
        model="tightened-model",
        layer=RoleLayer.PROJECT,
        authority=AuthorityCeiling.INTERNAL,
    )
    _write_business_role(
        root,
        provider="zai",
        model="base-model",
        layer=RoleLayer.BUILT_IN,
        authority=AuthorityCeiling.EXTERNAL,
    )
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))

    result = mr._default_business_lookup("publisher")

    assert not isinstance(result, RoleResolutionBlocked)
    assert len(result.source_digest) == 64
    assert result.routing_projection == RoutingHint(provider="zai", model="tightened-model")


def test_default_lookup_blocks_mixed_revisions_and_authority_expansion(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_business_role(
        root,
        provider="zai",
        model="base-model",
        layer=RoleLayer.BUILT_IN,
        revision="snapshot-1",
    )
    _write_business_role(
        root,
        provider="zai",
        model="overlay-model",
        layer=RoleLayer.PROJECT,
        revision="snapshot-2",
        authority=AuthorityCeiling.EXTERNAL,
    )
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))

    mixed = mr._default_business_lookup("publisher")
    assert isinstance(mixed, RoleResolutionBlocked)
    assert mixed.reason is RoleResolutionReason.MIXED_REVISION

    overlay_path = root / RoleLayer.PROJECT.value / "publisher.json"
    raw = json.loads(overlay_path.read_text(encoding="utf-8"))
    raw["snapshot_revision"] = "snapshot-1"
    overlay_path.write_text(json.dumps(raw), encoding="utf-8")
    expanded = mr._default_business_lookup("publisher")
    assert isinstance(expanded, RoleResolutionBlocked)
    assert expanded.reason is RoleResolutionReason.AUTHORITY_EXPANSION
    assert expanded.source_layer is RoleLayer.PROJECT


def test_default_lookup_blocks_ambiguous_role_function_identity(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_business_role(
        root,
        provider="zai",
        model="communications-model",
        layer=RoleLayer.BUILT_IN,
        function_id="communications",
    )
    _write_business_role(
        root,
        provider="zai",
        model="sales-model",
        layer=RoleLayer.COMPANY,
        function_id="sales",
    )
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))

    ambiguous = mr._default_business_lookup("publisher")

    assert isinstance(ambiguous, RoleResolutionBlocked)
    assert ambiguous.reason is RoleResolutionReason.INVALID_MANIFEST
    assert "multiple functions" in (ambiguous.detail or "")


@pytest.mark.parametrize("resolver", [mr.resolve_route, mr.resolve_codex_route])
def test_default_production_lookup_blocks_corrupt_sources_even_when_disabled(
    resolver: Callable[..., object],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_corrupt_role(root)
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))

    with pytest.raises(mr.BusinessRoleResolutionBlockedError) as caught:
        resolver("publisher", settings=_settings(enabled=False), env={})

    assert caught.value.result.reason is RoleResolutionReason.INVALID_MANIFEST
    assert caught.value.result.source_layer is RoleLayer.PROJECT
    assert caught.value.result.source_id == "project/broken.json"
    assert "JSONDecodeError" in (caught.value.result.detail or "")


def test_default_production_lookup_blocks_corrupt_source_at_spawn_seam(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_corrupt_role(root)
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))
    monkeypatch.setattr(mr, "_routing_block", lambda settings: _settings().model_routing)

    with pytest.raises(mr.BusinessRoleResolutionBlockedError):
        mr.resolve_spawn_route("publisher")


def test_default_production_lookup_blocks_unreadable_layer_at_spawn_seam(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    unreadable_layer = root / RoleLayer.PROJECT.value
    unreadable_layer.mkdir(parents=True)
    from fno.roles import registry

    real_walk = registry.os.walk

    def deny_project_walk(directory, **kwargs):
        if Path(directory) == unreadable_layer:
            kwargs["onerror"](
                PermissionError(13, "Permission denied", str(unreadable_layer))
            )
            return iter(())
        return real_walk(directory, **kwargs)

    monkeypatch.setattr(registry.os, "walk", deny_project_walk)
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))
    monkeypatch.setattr(mr, "_routing_block", lambda settings: _settings().model_routing)

    with pytest.raises(mr.BusinessRoleResolutionBlockedError) as caught:
        mr.resolve_spawn_route("coordinate")

    assert caught.value.result.reason is RoleResolutionReason.INVALID_MANIFEST
    assert caught.value.result.source_layer is RoleLayer.PROJECT
    assert caught.value.result.source_id == "project"
    assert "PermissionError" in (caught.value.result.detail or "")


def test_default_production_lookup_blocks_invalid_role_root_at_spawn_seam(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    root.write_text("not a directory", encoding="utf-8")
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))
    monkeypatch.setattr(mr, "_routing_block", lambda settings: _settings().model_routing)

    with pytest.raises(mr.BusinessRoleResolutionBlockedError) as caught:
        mr.resolve_spawn_route("coordinate")

    assert caught.value.result.reason is RoleResolutionReason.INVALID_MANIFEST
    assert caught.value.result.source_layer is None
    assert caught.value.result.source_id == str(root)
    assert "not a directory" in (caught.value.result.detail or "")


def test_default_production_lookup_blocks_missing_explicit_role_root(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "missing-roles"
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))
    monkeypatch.setattr(mr, "_routing_block", lambda settings: _settings().model_routing)

    with pytest.raises(mr.BusinessRoleResolutionBlockedError) as caught:
        mr.resolve_spawn_route("coordinate")

    assert caught.value.result.reason is RoleResolutionReason.INVALID_MANIFEST
    assert caught.value.result.source_id == str(root)
    assert "does not exist" in (caught.value.result.detail or "")


def test_business_role_refusals_share_route_composition_error_contract() -> None:
    assert issubclass(mr.BusinessRoleResolutionBlockedError, mr.RouteCompositionError)
    assert issubclass(mr.BusinessRoleRoutingProjectionError, mr.RouteCompositionError)


@pytest.mark.parametrize("resolver", [mr.resolve_route, mr.resolve_codex_route])
def test_default_lookup_blocks_corrupt_sources_for_protected_roles(
    resolver: Callable[..., object],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    root = tmp_path / "roles"
    _write_corrupt_role(root)
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))

    with pytest.raises(mr.BusinessRoleResolutionBlockedError):
        resolver("implement", settings=_settings(), env={})


def test_spawn_paths_share_the_guarded_routing_seams() -> None:
    agents_root = Path(__file__).parents[3] / "src" / "fno" / "agents"
    expected = {
        "cli.py": "resolve_spawn_route",
        "dispatch.py": "resolve_spawn_route",
        "mux_spawn.py": "resolve_spawn_route",
        "harnesses/claude.py": "resolve_spawn_route",
        "harnesses/codex.py": "resolve_codex_route",
    }

    for relative, positive_control in expected.items():
        assert positive_control in (agents_root / relative).read_text()
