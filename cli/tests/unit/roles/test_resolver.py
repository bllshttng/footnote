from __future__ import annotations

import copy
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from pydantic import ValidationError

from fno.company.contracts import FunctionRef, RoleRef, WorkOrderRef
from fno.roles import (
    ApprovalFloor,
    AuthorityCeiling,
    CapabilityFact,
    ContextBundleBounds,
    ContextKind,
    ContextReference,
    ContextSelector,
    DefinitionStatus,
    DeliveryPolicy,
    RegistryError,
    ResolvedRole,
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
from fno.roles.models import canonical_digest

NOW = datetime(2026, 8, 2, 12, tzinfo=UTC)
REVISION = "snapshot-7"
ROLE = RoleRef(id="arbitrary-owner", function_id="arbitrary-function")
WORK_ORDER = WorkOrderRef(node_id="x-a8c0", attempt_id="attempt-1", role_id=ROLE.id)


def _manifest(*, role: RoleRef = ROLE, **overrides: object) -> RoleManifest:
    values: dict[str, object] = {
        "role": role,
        "function": FunctionRef(id=role.function_id),
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
    role: RoleRef = ROLE,
    manifest: RoleManifest | None = None,
    status: DefinitionStatus = DefinitionStatus.VALID,
    error: str | None = None,
    revision: str = REVISION,
) -> RoleDefinitionSource:
    return RoleDefinitionSource(
        layer=layer,
        source_id=source_id,
        snapshot_revision=revision,
        role=role,
        manifest=manifest
        if manifest is not None
        else (_manifest(role=role) if status == DefinitionStatus.VALID else None),
        status=status,
        error=error,
    )


def _context(
    identifier: str = "company-brief",
    *,
    work_order: WorkOrderRef = WORK_ORDER,
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
        work_order_scope=work_order,
        content_digest=digest if readable else None,
        content_revision="content-v1" if readable else None,
        snapshot_revision=revision,
        fresh_until=fresh_until or NOW + timedelta(hours=1),
        sensitivity=sensitivity,
        byte_size=byte_size,
        readable=readable,
        unavailable_reason=None if readable else "permission denied",
    )


def _resolve(
    *,
    role: RoleRef = ROLE,
    work_order: WorkOrderRef = WORK_ORDER,
    definitions: tuple[RoleDefinitionSource, ...] | None = None,
    capabilities: tuple[CapabilityFact, ...] | None = None,
    context: tuple[ContextReference, ...] | None = None,
    bounds: ContextBundleBounds | None = None,
):
    return resolve_role(
        role=role,
        definitions=definitions or (_source(RoleLayer.BUILT_IN, "builtin/owner", role=role),),
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
        context_catalog=context if context is not None else (_context(work_order=work_order),),
        work_order=work_order,
        clock=NOW,
        snapshot_revision=REVISION,
        bundle_bounds=bounds or ContextBundleBounds(max_references=2, max_bytes=512),
    )


def test_ac_r1_hp_resolution_is_deterministic_and_records_fixed_layer_order() -> None:
    definitions = tuple(
        _source(layer, f"{layer.value}/owner") for layer in reversed(tuple(RoleLayer))
    )

    first = _resolve(definitions=definitions)
    second = _resolve(definitions=tuple(reversed(definitions)))

    assert not isinstance(first, RoleResolutionBlocked)
    assert first == second
    assert tuple(source.layer for source in first.source_chain) == tuple(RoleLayer)
    assert [source.disposition for source in first.source_chain] == [
        "shadowed",
        "shadowed",
        "shadowed",
        "shadowed",
        "contributing",
    ]
    assert len(first.manifest_digest) == len(first.context_bundle.digest) == 64
    assert first.routing_projection == RoutingHint(provider="codex", model="gpt-5")


def _change_bundle_snapshot(payload: dict) -> None:
    bundle = payload["context_bundle"]
    bundle["snapshot_revision"] = "other-snapshot"
    for reference in bundle["references"]:
        reference["snapshot_revision"] = "other-snapshot"
    bundle["digest"] = canonical_digest(
        "context-bundle",
        {key: value for key, value in bundle.items() if key != "digest"},
    )


@pytest.mark.parametrize(
    ("mutation", "message"),
    [
        (
            lambda payload: payload.update(
                routing_projection={"provider": "other", "model": "other-model"}
            ),
            "routing_projection must match manifest",
        ),
        (
            lambda payload: payload.update(source_chain=[]),
            "at least 1 item",
        ),
        (
            _change_bundle_snapshot,
            "context bundle snapshot_revision must match resolved role",
        ),
        (
            lambda payload: payload.update(authority_ceiling="external"),
            "authority_ceiling must match manifest",
        ),
        (
            lambda payload: payload.update(manifest_digest="0" * 64),
            "manifest_digest must match manifest",
        ),
        (
            lambda payload: payload["context_bundle"].update(digest="0" * 64),
            "context bundle digest must match captured fields",
        ),
    ],
)
def test_resolved_role_rejects_contradictory_frozen_projection(
    mutation,
    message: str,
) -> None:
    resolved = _resolve()
    assert isinstance(resolved, ResolvedRole)
    payload = copy.deepcopy(resolved.model_dump(mode="json"))
    mutation(payload)

    with pytest.raises(ValidationError, match=message):
        ResolvedRole.model_validate(payload)


def test_context_bundle_accepts_matching_partial_work_order_scope() -> None:
    partial_scope = WorkOrderRef(
        node_id=WORK_ORDER.node_id,
        attempt_id=WORK_ORDER.attempt_id,
    )

    resolved = _resolve(context=(_context(work_order=partial_scope),))

    assert isinstance(resolved, ResolvedRole)
    assert resolved.context_bundle.references[0].work_order_scope == partial_scope


def test_fresh_context_requirement_rejects_missing_freshness_metadata() -> None:
    manifest = _manifest(
        context_selectors=(
            ContextSelector(
                kind=ContextKind.BRIEF,
                identifier="company-brief",
                max_sensitivity=Sensitivity.SENSITIVE,
                requires_freshness=True,
            ),
        )
    )
    context_without_freshness = _context().model_copy(update={"fresh_until": None})

    result = _resolve(
        definitions=(_source(RoleLayer.BUILT_IN, "builtin/owner", manifest=manifest),),
        context=(context_without_freshness,),
    )

    assert isinstance(result, RoleResolutionBlocked)
    assert result.reason is RoleResolutionReason.STALE_CONTEXT
    assert result.reference == "brief:company-brief"


def test_context_without_freshness_requirement_accepts_missing_metadata() -> None:
    context_without_freshness = _context().model_copy(update={"fresh_until": None})

    result = _resolve(context=(context_without_freshness,))

    assert not isinstance(result, RoleResolutionBlocked)


@pytest.mark.parametrize(
    ("capabilities", "context", "reason", "reference"),
    [
        ((), (_context(),), RoleResolutionReason.MISSING_CAPABILITY, "connector.read"),
        (
            (
                CapabilityFact(
                    capability="connector.read",
                    available=True,
                    source_id="runtime",
                    snapshot_revision=REVISION,
                ),
            ),
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


def test_capability_fact_from_another_principal_cannot_satisfy_the_attempt() -> None:
    work_order = WorkOrderRef(
        node_id="x-a8c0",
        attempt_id="attempt-1",
        principal_id="principal-a",
        role_id=ROLE.id,
    )
    other_principal = WorkOrderRef(
        node_id=work_order.node_id,
        attempt_id=work_order.attempt_id,
        principal_id="principal-b",
        role_id=ROLE.id,
    )

    result = _resolve(
        work_order=work_order,
        capabilities=(
            CapabilityFact(
                capability="connector.read",
                available=True,
                source_id="observed-for-principal-b",
                snapshot_revision=REVISION,
                work_order_scope=other_principal,
            ),
        ),
        context=(_context(work_order=work_order),),
    )

    assert result == RoleResolutionBlocked(
        role=ROLE,
        reason=RoleResolutionReason.MISSING_CAPABILITY,
        source_layer=RoleLayer.BUILT_IN,
        source_id="builtin/owner",
        reference="connector.read",
    )


def test_matching_principal_scoped_capability_fact_satisfies_the_attempt() -> None:
    work_order = WorkOrderRef(
        node_id="x-a8c0",
        attempt_id="attempt-1",
        principal_id="principal-a",
        role_id=ROLE.id,
    )

    result = _resolve(
        work_order=work_order,
        capabilities=(
            CapabilityFact(
                capability="connector.read",
                available=True,
                source_id="observed-for-principal-a",
                snapshot_revision=REVISION,
                work_order_scope=work_order,
            ),
        ),
        context=(_context(work_order=work_order),),
    )

    assert not isinstance(result, RoleResolutionBlocked)
    assert result.work_order == work_order


def test_ac_r5_con_captured_result_retains_digests_after_inputs_change() -> None:
    captured = _resolve()
    changed = _resolve(
        definitions=(
            _source(
                RoleLayer.BUILT_IN, "builtin/owner", manifest=_manifest(mission="Changed later.")
            ),
        ),
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
            _source(
                RoleLayer.PROJECT, "project/roles.toml", status=status, error="invalid document"
            ),
        )
    )

    assert isinstance(result, RoleResolutionBlocked)
    assert result.reason == RoleResolutionReason.INVALID_MANIFEST
    assert (result.source_layer, result.source_id, result.detail) == (
        RoleLayer.PROJECT,
        "project/roles.toml",
        "invalid document",
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
    assert tuple(reference.identifier for reference in first.context_bundle.references) == (
        "small-a",
    )


def test_context_candidate_search_is_bounded_before_cartesian_expansion() -> None:
    manifest = _manifest(
        context_selectors=(
            ContextSelector(kind=ContextKind.BRIEF),
            ContextSelector(kind=ContextKind.PLAN),
        )
    )
    briefs = tuple(
        _context(
            f"brief-{index}",
            byte_size=10,
            digest=f"{index + 1:x}" * 64,
            sensitivity=Sensitivity.INTERNAL,
        )
        for index in range(3)
    )
    plans = tuple(
        reference.model_copy(
            update={
                "kind": ContextKind.PLAN,
                "identifier": f"plan-{index}",
                "content_digest": f"{index + 4:x}" * 64,
            }
        )
        for index, reference in enumerate(briefs)
    )

    result = _resolve(
        definitions=(_source(RoleLayer.BUILT_IN, "builtin/owner", manifest=manifest),),
        context=briefs + plans,
        bounds=ContextBundleBounds(
            max_references=2,
            max_bytes=512,
            max_combinations=4,
        ),
    )

    assert isinstance(result, RoleResolutionBlocked)
    assert result.reason is RoleResolutionReason.OVER_BUDGET
    assert result.reference == "context_combinations"
    assert result.detail == "9 candidate combinations exceed limit 4"


def test_manifests_are_frozen_extra_forbidden_and_non_granting() -> None:
    manifest = _manifest()
    with pytest.raises(ValidationError, match="frozen"):
        manifest.mission = "mutated"
    for forbidden in (
        {"credentials": ("secret",)},
        {"granted_capabilities": ("connector.write",)},
        {"approval_receipts": ("receipt-1",)},
        {"approval_receipt_ids": ("receipt-1",)},
        {"approval_id": "approval-1"},
        {"effect": "send"},
        {"effect_id": "effect-1"},
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


def test_mixed_context_failures_report_the_first_precise_stable_reason() -> None:
    stale = _context(
        fresh_until=NOW - timedelta(seconds=1),
        digest="a" * 64,
    )
    over_sensitive = _context(
        sensitivity=Sensitivity.RESTRICTED,
        digest="b" * 64,
    )

    result = _resolve(context=(over_sensitive, stale))

    assert isinstance(result, RoleResolutionBlocked)
    assert result.reason is RoleResolutionReason.STALE_CONTEXT
    assert result.source_id == stale.provenance


def test_ac_r2_hp_all_layers_may_only_tighten_the_full_manifest() -> None:
    editor = RoleRef(id="editor", function_id=ROLE.function_id)
    analyst = RoleRef(id="analyst", function_id=ROLE.function_id)
    base = _manifest(
        deliverable_kinds=("brief", "post"),
        delegation_targets=(editor, analyst),
        required_capabilities=("connector.read", "draft.write"),
        authority_ceiling=AuthorityCeiling.EXTERNAL,
        context_selectors=(
            ContextSelector(kind=ContextKind.BRIEF, max_sensitivity=Sensitivity.SENSITIVE),
            ContextSelector(kind=ContextKind.PLAN, max_sensitivity=Sensitivity.INTERNAL),
        ),
        review_policy=ReviewPolicy(),
        delivery_policy=DeliveryPolicy(
            required_evidence=("artifact-exists",),
            require_all_deliverables=False,
        ),
        routing_hint=RoutingHint(provider="codex"),
    )
    plugin = base.model_copy(update={"authority_ceiling": AuthorityCeiling.INTERNAL})
    company = plugin.model_copy(
        update={
            "deliverable_kinds": ("brief",),
            "delegation_targets": (editor,),
            "required_capabilities": ("connector.read",),
        }
    )
    project = company.model_copy(
        update={
            "context_selectors": (
                ContextSelector(
                    kind=ContextKind.BRIEF,
                    identifier="company-brief",
                    max_sensitivity=Sensitivity.INTERNAL,
                ),
            )
        }
    )
    plan = project.model_copy(
        update={
            "review_policy": ReviewPolicy(
                required=True,
                minimum_reviewers=2,
                required_review_kinds=("legal",),
            ),
            "delivery_policy": DeliveryPolicy(
                required_evidence=("artifact-exists", "review-recorded"),
                require_all_deliverables=True,
            ),
            "routing_hint": RoutingHint(provider="zai-openai", model="glm-4.6"),
        }
    )
    definitions = tuple(
        _source(layer, f"{layer.value}/owner", manifest=manifest)
        for layer, manifest in zip(RoleLayer, (base, plugin, company, project, plan), strict=True)
    )

    result = _resolve(
        definitions=definitions,
        context=(_context(sensitivity=Sensitivity.INTERNAL),),
    )

    assert not isinstance(result, RoleResolutionBlocked)
    assert result.manifest == plan
    assert tuple(source.layer for source in result.source_chain) == tuple(RoleLayer)
    assert result.required_capabilities == ("connector.read",)
    assert result.authority_ceiling is AuthorityCeiling.INTERNAL


def test_ac_r2_sec_founder_approval_floor_cannot_become_autonomous() -> None:
    base = _manifest(
        deliverable_kinds=("public-publication",),
        approval_floor=ApprovalFloor.FOUNDER,
    )
    autonomous = base.model_copy(update={"approval_floor": ApprovalFloor.NONE})

    result = _resolve(
        definitions=(
            _source(RoleLayer.BUILT_IN, "builtin/publisher", manifest=base),
            _source(RoleLayer.PROJECT, "project/publisher", manifest=autonomous),
        )
    )

    assert isinstance(result, RoleResolutionBlocked)
    assert result.reason is RoleResolutionReason.AUTHORITY_EXPANSION
    assert result.reference == "approval_floor"


@pytest.mark.parametrize(
    ("base_overrides", "overlay_overrides", "reason", "reference"),
    [
        (
            {"authority_ceiling": AuthorityCeiling.INTERNAL},
            {"authority_ceiling": AuthorityCeiling.EXTERNAL},
            RoleResolutionReason.AUTHORITY_EXPANSION,
            "authority_ceiling",
        ),
        (
            {"deliverable_kinds": ("brief",)},
            {"deliverable_kinds": ("brief", "post")},
            RoleResolutionReason.INVALID_OVERLAY,
            "deliverable_kinds",
        ),
        (
            {"delegation_targets": ()},
            {"delegation_targets": (RoleRef(id="editor", function_id=ROLE.function_id),)},
            RoleResolutionReason.INVALID_OVERLAY,
            "delegation_targets",
        ),
        (
            {"required_capabilities": ("connector.read",)},
            {"required_capabilities": ("connector.read", "connector.write")},
            RoleResolutionReason.INVALID_OVERLAY,
            "required_capabilities",
        ),
        (
            {"review_policy": ReviewPolicy(required=True, minimum_reviewers=1)},
            {"review_policy": ReviewPolicy()},
            RoleResolutionReason.INVALID_OVERLAY,
            "review_policy.required",
        ),
        (
            {"review_policy": ReviewPolicy(required=True, minimum_reviewers=2)},
            {"review_policy": ReviewPolicy(required=True, minimum_reviewers=1)},
            RoleResolutionReason.INVALID_OVERLAY,
            "review_policy.minimum_reviewers",
        ),
        (
            {
                "review_policy": ReviewPolicy(
                    required=True,
                    minimum_reviewers=1,
                    required_review_kinds=("legal", "brand"),
                )
            },
            {
                "review_policy": ReviewPolicy(
                    required=True,
                    minimum_reviewers=1,
                    required_review_kinds=("legal",),
                )
            },
            RoleResolutionReason.INVALID_OVERLAY,
            "review_policy.required_review_kinds",
        ),
        (
            {"delivery_policy": DeliveryPolicy(required_evidence=("artifact", "review"))},
            {"delivery_policy": DeliveryPolicy(required_evidence=("artifact",))},
            RoleResolutionReason.INVALID_OVERLAY,
            "delivery_policy.required_evidence",
        ),
        (
            {
                "delivery_policy": DeliveryPolicy(
                    required_evidence=("artifact",),
                    require_all_deliverables=True,
                )
            },
            {
                "delivery_policy": DeliveryPolicy(
                    required_evidence=("artifact",),
                    require_all_deliverables=False,
                )
            },
            RoleResolutionReason.INVALID_OVERLAY,
            "delivery_policy.require_all_deliverables",
        ),
        (
            {"context_selectors": ()},
            {
                "context_selectors": (
                    ContextSelector(kind=ContextKind.BRIEF, identifier="company-brief"),
                )
            },
            RoleResolutionReason.INVALID_OVERLAY,
            "context_selectors[brief:company-brief]",
        ),
        (
            {
                "context_selectors": (
                    ContextSelector(kind=ContextKind.BRIEF, identifier="company-brief"),
                )
            },
            {"context_selectors": (ContextSelector(kind=ContextKind.BRIEF),)},
            RoleResolutionReason.INVALID_OVERLAY,
            "context_selectors[brief:*].identifier",
        ),
        (
            {
                "context_selectors": (
                    ContextSelector(
                        kind=ContextKind.BRIEF,
                        identifier="company-brief",
                        max_sensitivity=Sensitivity.INTERNAL,
                    ),
                )
            },
            {
                "context_selectors": (
                    ContextSelector(
                        kind=ContextKind.BRIEF,
                        identifier="company-brief",
                        max_sensitivity=Sensitivity.SENSITIVE,
                    ),
                )
            },
            RoleResolutionReason.INVALID_OVERLAY,
            "context_selectors[brief:company-brief].max_sensitivity",
        ),
        (
            {
                "context_selectors": (
                    ContextSelector(
                        kind=ContextKind.BRIEF,
                        identifier="company-brief",
                        requires_freshness=True,
                    ),
                )
            },
            {
                "context_selectors": (
                    ContextSelector(
                        kind=ContextKind.BRIEF,
                        identifier="company-brief",
                        requires_freshness=False,
                    ),
                )
            },
            RoleResolutionReason.INVALID_OVERLAY,
            "context_selectors[brief:company-brief].requires_freshness",
        ),
        (
            {"mission": "Produce one bounded artifact."},
            {"mission": "Own the whole company."},
            RoleResolutionReason.INVALID_OVERLAY,
            "mission",
        ),
        (
            {"default_topology": "direct"},
            {"default_topology": "squad"},
            RoleResolutionReason.INVALID_OVERLAY,
            "default_topology",
        ),
        (
            {"routing_hint": RoutingHint(provider="codex", model="gpt-5")},
            {"routing_hint": RoutingHint(model="gpt-5")},
            RoleResolutionReason.INVALID_OVERLAY,
            "routing_hint.provider",
        ),
        (
            {"routing_hint": RoutingHint(provider="codex", model="gpt-5")},
            {"routing_hint": RoutingHint(provider="codex")},
            RoleResolutionReason.INVALID_OVERLAY,
            "routing_hint.model",
        ),
    ],
)
def test_ac_r2_sec_forbidden_overlay_changes_fail_closed_at_exact_source_and_field(
    base_overrides: dict[str, object],
    overlay_overrides: dict[str, object],
    reason: RoleResolutionReason,
    reference: str,
) -> None:
    definitions = (
        _source(RoleLayer.BUILT_IN, "builtin/owner", manifest=_manifest(**base_overrides)),
        _source(RoleLayer.PLAN, "plans/launch.md", manifest=_manifest(**overlay_overrides)),
    )

    result = _resolve(definitions=definitions)

    assert result == RoleResolutionBlocked(
        role=ROLE,
        reason=reason,
        source_layer=RoleLayer.PLAN,
        source_id="plans/launch.md",
        reference=reference,
    )
    assert not hasattr(result, "manifest_digest")
    assert not hasattr(result, "source_chain")


def test_identity_mismatch_is_not_a_representable_overlay() -> None:
    with pytest.raises(ValidationError, match="role function_id must match function id"):
        _manifest(function=FunctionRef(id="other-function"))
    with pytest.raises(ValidationError, match="definition role must match manifest role"):
        _source(
            RoleLayer.PLAN,
            "plans/launch.md",
            manifest=_manifest(role=RoleRef(id="other-role", function_id=ROLE.function_id)),
        )


def test_same_layer_ambiguity_blocks_with_exact_layer_and_source() -> None:
    result = _resolve(
        definitions=(
            _source(RoleLayer.PLUGIN, "plugin-a/owner"),
            _source(RoleLayer.PLUGIN, "plugin-b/owner"),
        )
    )

    assert result == RoleResolutionBlocked(
        role=ROLE,
        reason=RoleResolutionReason.INVALID_OVERLAY,
        source_layer=RoleLayer.PLUGIN,
        source_id="plugin-a/owner",
        reference=ROLE.id,
        detail=(
            "ambiguous plugin definitions for "
            "arbitrary-function/arbitrary-owner: plugin-a/owner, plugin-b/owner"
        ),
    )


@pytest.mark.parametrize(
    "function_id",
    [
        "marketing",
        "communications",
        "design",
        "social",
        "support",
        "operations",
        "sales",
        "arbitrary-unknown-function",
    ],
)
def test_ac_r6_inv_tightening_enforcement_is_function_agnostic(
    function_id: str,
) -> None:
    role = RoleRef(id="owner", function_id=function_id)
    work_order = WorkOrderRef(
        node_id="x-a8c0",
        attempt_id="attempt-1",
        role_id=role.id,
    )
    base = _manifest(role=role, authority_ceiling=AuthorityCeiling.EXTERNAL)
    tightened = _manifest(role=role, authority_ceiling=AuthorityCeiling.INTERNAL)
    widened = _manifest(role=role, authority_ceiling=AuthorityCeiling.EXTERNAL)
    accepted = _resolve(
        role=role,
        work_order=work_order,
        definitions=(
            _source(
                RoleLayer.BUILT_IN,
                f"builtin/{function_id}",
                role=role,
                manifest=base,
            ),
            _source(
                RoleLayer.PROJECT,
                f"project/{function_id}",
                role=role,
                manifest=tightened,
            ),
        ),
        context=(_context(work_order=work_order),),
    )
    blocked = _resolve(
        role=role,
        work_order=work_order,
        definitions=(
            _source(
                RoleLayer.BUILT_IN,
                f"builtin/{function_id}",
                role=role,
                manifest=tightened,
            ),
            _source(
                RoleLayer.PROJECT,
                f"project/{function_id}",
                role=role,
                manifest=widened,
            ),
        ),
        context=(_context(work_order=work_order),),
    )

    assert not isinstance(accepted, RoleResolutionBlocked)
    assert accepted.authority_ceiling is AuthorityCeiling.INTERNAL
    assert blocked == RoleResolutionBlocked(
        role=role,
        reason=RoleResolutionReason.AUTHORITY_EXPANSION,
        source_layer=RoleLayer.PROJECT,
        source_id=f"project/{function_id}",
        reference="authority_ceiling",
    )


def test_core_role_resolution_has_no_function_name_semantics() -> None:
    roles_source = Path(__file__).parents[3] / "src" / "fno" / "roles"
    source = "\n".join(path.read_text() for path in roles_source.glob("*.py"))
    assert "_AUTHORITY_RANK" in source  # positive control for the source scan
    for function_name in (
        "marketing",
        "communications",
        "design",
        "social",
        "support",
        "operations",
        "sales",
    ):
        assert function_name not in source.casefold()
