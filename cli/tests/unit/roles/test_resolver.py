from __future__ import annotations

from datetime import UTC, datetime, timedelta

import pytest
from pydantic import ValidationError

from fno.company.contracts import FunctionRef, RoleRef, WorkOrderRef
from fno.roles import (
    AuthorityCeiling,
    CapabilityFact,
    ContextBundleBounds,
    ContextKind,
    ContextReference,
    ContextSelector,
    DefinitionStatus,
    DeliveryPolicy,
    RegistryError,
    ReviewPolicy,
    RoleDefinitionSource,
    RoleLayer,
    RoleManifest,
    RoleRegistry,
    RoleResolutionBlocked,
    RoleResolutionReason,
    RoutingHint,
    Sensitivity,
    resolve_role,
)

NOW = datetime(2026, 8, 2, 12, tzinfo=UTC)
REVISION = "snapshot-7"
ROLE = RoleRef(id="arbitrary-owner", function_id="arbitrary-function")
WORK_ORDER = WorkOrderRef(node_id="x-a8c0", attempt_id="attempt-1", role_id=ROLE.id)


def _manifest(**overrides: object) -> RoleManifest:
    values: dict[str, object] = {
        "role": ROLE,
        "function": FunctionRef(id=ROLE.function_id),
        "mission": "Produce a bounded, reviewable artifact.",
        "deliverable_kinds": ("brief",),
        "delegation_targets": (),
        "required_capabilities": ("connector.read",),
        "authority_ceiling": AuthorityCeiling.INTERNAL,
        "context_selectors": (
            ContextSelector(
                kind=ContextKind.BRIEF,
                identifier="company-brief",
                max_sensitivity=Sensitivity.SENSITIVE,
            ),
        ),
        "review_policy": ReviewPolicy(required=True, minimum_reviewers=1),
        "delivery_policy": DeliveryPolicy(required_evidence=("artifact-exists",)),
        "default_topology": "direct",
        "routing_hint": RoutingHint(provider="codex", model="gpt-5"),
    }
    values.update(overrides)
    return RoleManifest(**values)


def _source(
    layer: RoleLayer,
    source_id: str,
    *,
    manifest: RoleManifest | None = None,
    status: DefinitionStatus = DefinitionStatus.VALID,
    error: str | None = None,
    revision: str = REVISION,
) -> RoleDefinitionSource:
    return RoleDefinitionSource(
        layer=layer,
        source_id=source_id,
        snapshot_revision=revision,
        role=ROLE,
        manifest=manifest
        if manifest is not None
        else (_manifest() if status == DefinitionStatus.VALID else None),
        status=status,
        error=error,
    )


def _context(
    identifier: str = "company-brief",
    *,
    byte_size: int = 120,
    digest: str = "a" * 64,
    readable: bool = True,
    fresh_until: datetime | None = None,
    sensitivity: Sensitivity = Sensitivity.SENSITIVE,
    revision: str = REVISION,
) -> ContextReference:
    return ContextReference(
        kind=ContextKind.BRIEF,
        identifier=identifier,
        provenance=f"briefs/{identifier}.md",
        work_order_scope=WORK_ORDER,
        content_digest=digest,
        content_revision="content-v1",
        snapshot_revision=revision,
        fresh_until=fresh_until or NOW + timedelta(hours=1),
        sensitivity=sensitivity,
        byte_size=byte_size,
        readable=readable,
        unavailable_reason=None if readable else "permission denied",
    )


def _resolve(
    *,
    definitions: tuple[RoleDefinitionSource, ...] | None = None,
    capabilities: tuple[CapabilityFact, ...] | None = None,
    context: tuple[ContextReference, ...] | None = None,
):
    return resolve_role(
        role=ROLE,
        definitions=definitions or (_source(RoleLayer.BUILT_IN, "builtin/owner"),),
        capability_facts=capabilities
        if capabilities is not None
        else (
            CapabilityFact(
                capability="connector.read",
                available=True,
                source_id="runtime-capabilities",
                snapshot_revision=REVISION,
            ),
        ),
        context_catalog=context if context is not None else (_context(),),
        work_order=WORK_ORDER,
        clock=NOW,
        snapshot_revision=REVISION,
        bundle_bounds=ContextBundleBounds(max_references=2, max_bytes=512),
    )


def test_ac_r1_hp_resolution_is_deterministic_and_records_fixed_layer_order() -> None:
    definitions = tuple(
        _source(layer, f"{layer.value}/owner")
        for layer in reversed(tuple(RoleLayer))
    )

    first = _resolve(definitions=definitions)
    second = _resolve(definitions=tuple(reversed(definitions)))

    assert not isinstance(first, RoleResolutionBlocked)
    assert first == second
    assert tuple(source.layer for source in first.source_chain) == tuple(RoleLayer)
    assert [source.disposition for source in first.source_chain] == [
        "shadowed", "shadowed", "shadowed", "shadowed", "contributing"
    ]
    assert len(first.manifest_digest) == len(first.context_bundle.digest) == 64
    assert first.routing_projection == RoutingHint(provider="codex", model="gpt-5")


@pytest.mark.parametrize(
    ("capabilities", "context", "reason", "reference"),
    [
        ((), (_context(),), RoleResolutionReason.MISSING_CAPABILITY, "connector.read"),
        (
            (CapabilityFact(capability="connector.read", available=True, source_id="runtime", snapshot_revision=REVISION),),
            (),
            RoleResolutionReason.MISSING_CONTEXT,
            "brief:company-brief",
        ),
    ],
)
def test_ac_r3_err_missing_requirements_fail_closed_with_source_attribution(
    capabilities: tuple[CapabilityFact, ...],
    context: tuple[ContextReference, ...],
    reason: RoleResolutionReason,
    reference: str,
) -> None:
    result = _resolve(capabilities=capabilities, context=context)

    assert result == RoleResolutionBlocked(
        role=ROLE,
        reason=reason,
        source_layer=RoleLayer.BUILT_IN,
        source_id="builtin/owner",
        reference=reference,
    )


def test_ac_r5_con_captured_result_retains_digests_after_inputs_change() -> None:
    captured = _resolve()
    changed = _resolve(
        definitions=(_source(RoleLayer.BUILT_IN, "builtin/owner", manifest=_manifest(mission="Changed later.")),),
        context=(_context(digest="b" * 64),),
    )

    assert not isinstance(captured, RoleResolutionBlocked)
    assert not isinstance(changed, RoleResolutionBlocked)
    assert captured.manifest_digest != changed.manifest_digest
    assert captured.context_bundle.digest != changed.context_bundle.digest
    assert captured.manifest_digest == _resolve().manifest_digest
    assert captured.context_bundle.digest == _resolve().context_bundle.digest


@pytest.mark.parametrize("status", [DefinitionStatus.INVALID, DefinitionStatus.UNREADABLE])
def test_corrupt_or_unreadable_layer_remains_visible_and_blocks(status: DefinitionStatus) -> None:
    result = _resolve(
        definitions=(
            _source(RoleLayer.BUILT_IN, "builtin/owner"),
            _source(RoleLayer.PROJECT, "project/roles.toml", status=status, error="invalid document"),
        )
    )

    assert isinstance(result, RoleResolutionBlocked)
    assert result.reason == RoleResolutionReason.INVALID_MANIFEST
    assert (result.source_layer, result.source_id, result.detail) == (
        RoleLayer.PROJECT, "project/roles.toml", "invalid document"
    )


def test_registry_refuses_same_layer_duplicates_but_preserves_records() -> None:
    sources = (
        _source(RoleLayer.PLUGIN, "plugin-a/owner"),
        _source(RoleLayer.PLUGIN, "plugin-b/owner"),
    )
    registry = RoleRegistry(records=sources)

    assert registry.records == sources
    with pytest.raises(RegistryError, match="ambiguous plugin definitions"):
        registry.definitions_for(ROLE)


def test_smallest_bundle_selection_is_stable_under_catalog_reordering() -> None:
    manifest = _manifest(
        context_selectors=(
            ContextSelector(kind=ContextKind.BRIEF, max_sensitivity=Sensitivity.SENSITIVE),
        )
    )
    contexts = (
        _context("large", byte_size=300, digest="c" * 64),
        _context("small-b", byte_size=40, digest="d" * 64),
        _context("small-a", byte_size=40, digest="e" * 64),
    )
    definitions = (_source(RoleLayer.BUILT_IN, "builtin/owner", manifest=manifest),)

    first = _resolve(definitions=definitions, context=contexts)
    second = _resolve(definitions=definitions, context=tuple(reversed(contexts)))

    assert not isinstance(first, RoleResolutionBlocked)
    assert first == second
    assert tuple(reference.identifier for reference in first.context_bundle.references) == ("small-a",)


def test_manifests_are_frozen_extra_forbidden_and_non_granting() -> None:
    manifest = _manifest()
    with pytest.raises(ValidationError, match="frozen"):
        manifest.mission = "mutated"
    for forbidden in (
        {"credentials": ("secret",)},
        {"granted_capabilities": ("connector.write",)},
        {"approval_id": "approval-1"},
        {"effects": ("send",)},
        {"evidence_verdict": "passed"},
        {"graph_authority": "create"},
        {"delivery_evaluation": "complete"},
    ):
        with pytest.raises(ValidationError, match="Extra inputs are not permitted"):
            RoleManifest(**_manifest().model_dump(), **forbidden)


def test_unreadable_stale_sensitive_and_mixed_revision_context_fail_closed() -> None:
    cases = (
        (_context(readable=False), RoleResolutionReason.UNREADABLE_CONTEXT),
        (_context(fresh_until=NOW - timedelta(seconds=1)), RoleResolutionReason.STALE_CONTEXT),
        (_context(sensitivity=Sensitivity.RESTRICTED), RoleResolutionReason.SENSITIVITY_REFUSED),
        (_context(revision="snapshot-8"), RoleResolutionReason.MIXED_REVISION),
    )
    for context, reason in cases:
        result = _resolve(context=(context,))
        assert isinstance(result, RoleResolutionBlocked)
        assert result.reason == reason
        assert result.source_id == context.provenance
