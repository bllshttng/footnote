"""Pure deterministic resolution for immutable role and context snapshots."""

from __future__ import annotations

import itertools
import math
from datetime import datetime
from typing import Iterable, Sequence

from fno.company.contracts import RoleRef, WorkOrderRef
from fno.roles.models import (
    ApprovalFloor,
    AuthorityCeiling,
    CapabilityFact,
    ContextBundle,
    ContextBundleBounds,
    ContextReference,
    ContextSelector,
    DefinitionStatus,
    ManifestRoutingResolution,
    ResolvedRole,
    ResolvedSource,
    RoleDefinitionSource,
    RoleLayer,
    RoleManifest,
    RoleResolution,
    RoleResolutionBlocked,
    RoleResolutionReason,
    Sensitivity,
    canonical_digest as _digest,
)
from fno.roles.registry import DiscoveredRoleDefinition, RegistryError, RoleRegistry

_AUTHORITY_RANK = {
    AuthorityCeiling.ADVISORY: 0,
    AuthorityCeiling.INTERNAL: 1,
    AuthorityCeiling.EXTERNAL: 2,
}
_APPROVAL_RANK = {
    ApprovalFloor.NONE: 0,
    ApprovalFloor.PRINCIPAL: 1,
    ApprovalFloor.FOUNDER: 2,
}
_SENSITIVITY_RANK = {
    Sensitivity.PUBLIC: 0,
    Sensitivity.INTERNAL: 1,
    Sensitivity.SENSITIVE: 2,
    Sensitivity.RESTRICTED: 3,
}


def _delegation_keys(manifest: RoleManifest) -> set[tuple[str, str]]:
    return {(target.function_id, target.id) for target in manifest.delegation_targets}


def _selector_field(selector: ContextSelector, field: str | None = None) -> str:
    reference = f"context_selectors[{selector.reference}]"
    return f"{reference}.{field}" if field is not None else reference


def _context_overlay_violation(
    previous: RoleManifest,
    candidate: RoleManifest,
) -> str | None:
    options: list[tuple[int, ...]] = []
    for selector in candidate.context_selectors:
        same_kind = tuple(
            index
            for index, prior in enumerate(previous.context_selectors)
            if prior.kind is selector.kind
        )
        if not same_kind:
            return _selector_field(selector)
        compatible_identifier = tuple(
            index
            for index in same_kind
            if previous.context_selectors[index].identifier is None
            or previous.context_selectors[index].identifier == selector.identifier
        )
        if not compatible_identifier:
            return _selector_field(selector, "identifier")
        compatible_freshness = tuple(
            index
            for index in compatible_identifier
            if not previous.context_selectors[index].requires_freshness
            or selector.requires_freshness
        )
        if not compatible_freshness:
            return _selector_field(selector, "requires_freshness")
        compatible_sensitivity = tuple(
            index
            for index in compatible_freshness
            if _SENSITIVITY_RANK[selector.max_sensitivity]
            <= _SENSITIVITY_RANK[previous.context_selectors[index].max_sensitivity]
        )
        if not compatible_sensitivity:
            return _selector_field(selector, "max_sensitivity")
        options.append(compatible_sensitivity)

    matched_prior: dict[int, int] = {}

    def assign(candidate_index: int, seen: set[int]) -> bool:
        for prior_index in options[candidate_index]:
            if prior_index in seen:
                continue
            seen.add(prior_index)
            occupant = matched_prior.get(prior_index)
            if occupant is None or assign(occupant, seen):
                matched_prior[prior_index] = candidate_index
                return True
        return False

    for candidate_index, selector in enumerate(candidate.context_selectors):
        if not assign(candidate_index, set()):
            return _selector_field(selector)
    return None


def _overlay_violation(
    previous: RoleManifest,
    candidate: RoleManifest,
) -> tuple[RoleResolutionReason, str] | None:
    if candidate.role != previous.role:
        return RoleResolutionReason.INVALID_OVERLAY, "role"
    if candidate.function != previous.function:
        return RoleResolutionReason.INVALID_OVERLAY, "function"
    if _AUTHORITY_RANK[candidate.authority_ceiling] > _AUTHORITY_RANK[previous.authority_ceiling]:
        return RoleResolutionReason.AUTHORITY_EXPANSION, "authority_ceiling"
    if _APPROVAL_RANK[candidate.approval_floor] < _APPROVAL_RANK[previous.approval_floor]:
        return RoleResolutionReason.AUTHORITY_EXPANSION, "approval_floor"
    if not set(candidate.deliverable_kinds).issubset(previous.deliverable_kinds):
        return RoleResolutionReason.INVALID_OVERLAY, "deliverable_kinds"
    if not _delegation_keys(candidate).issubset(_delegation_keys(previous)):
        return RoleResolutionReason.INVALID_OVERLAY, "delegation_targets"
    if not set(candidate.required_capabilities).issubset(previous.required_capabilities):
        return RoleResolutionReason.INVALID_OVERLAY, "required_capabilities"
    if previous.review_policy.required and not candidate.review_policy.required:
        return RoleResolutionReason.INVALID_OVERLAY, "review_policy.required"
    if candidate.review_policy.minimum_reviewers < previous.review_policy.minimum_reviewers:
        return RoleResolutionReason.INVALID_OVERLAY, "review_policy.minimum_reviewers"
    if not set(previous.review_policy.required_review_kinds).issubset(
        candidate.review_policy.required_review_kinds
    ):
        return RoleResolutionReason.INVALID_OVERLAY, "review_policy.required_review_kinds"
    if not set(previous.delivery_policy.required_evidence).issubset(
        candidate.delivery_policy.required_evidence
    ):
        return RoleResolutionReason.INVALID_OVERLAY, "delivery_policy.required_evidence"
    if (
        previous.delivery_policy.require_all_deliverables
        and not candidate.delivery_policy.require_all_deliverables
    ):
        return RoleResolutionReason.INVALID_OVERLAY, "delivery_policy.require_all_deliverables"
    context_violation = _context_overlay_violation(previous, candidate)
    if context_violation is not None:
        return RoleResolutionReason.INVALID_OVERLAY, context_violation
    if candidate.mission != previous.mission:
        return RoleResolutionReason.INVALID_OVERLAY, "mission"
    if candidate.default_topology != previous.default_topology:
        return RoleResolutionReason.INVALID_OVERLAY, "default_topology"
    if previous.routing_hint is not None:
        for field in ("provider", "model"):
            if getattr(previous.routing_hint, field) is not None and (
                candidate.routing_hint is None or getattr(candidate.routing_hint, field) is None
            ):
                return RoleResolutionReason.INVALID_OVERLAY, f"routing_hint.{field}"
    return None


def _blocked(
    role: RoleRef,
    reason: RoleResolutionReason,
    *,
    source: RoleDefinitionSource | None = None,
    source_id: str | None = None,
    reference: str | None = None,
    detail: str | None = None,
) -> RoleResolutionBlocked:
    return RoleResolutionBlocked(
        role=role,
        reason=reason,
        source_layer=source.layer if source is not None else None,
        source_id=source.source_id if source is not None else source_id,
        reference=reference,
        detail=detail,
    )


def _same_work_order_scope(scope: WorkOrderRef | None, work_order: WorkOrderRef) -> bool:
    if scope is None:
        return True
    if scope.node_id != work_order.node_id or scope.attempt_id != work_order.attempt_id:
        return False
    if scope.role_id is not None and scope.role_id != work_order.role_id:
        return False
    if scope.principal_id is not None and scope.principal_id != work_order.principal_id:
        return False
    return True


def _capability_fact_applies(
    fact: CapabilityFact,
    *,
    work_order: WorkOrderRef,
    role: RoleRef,
) -> bool:
    scope = fact.work_order_scope
    if scope is None:
        return work_order.principal_id is None
    return (
        scope.node_id == work_order.node_id
        and scope.attempt_id == work_order.attempt_id
        and scope.principal_id == work_order.principal_id
        and scope.role_id == role.id
    )


def _candidate_key(reference: ContextReference) -> tuple[object, ...]:
    return (
        reference.byte_size,
        reference.kind.value,
        reference.identifier,
        reference.provenance,
        reference.content_digest,
    )


def _matching_candidates(
    selector: ContextSelector,
    catalog: Sequence[ContextReference],
) -> tuple[ContextReference, ...]:
    return tuple(
        reference
        for reference in catalog
        if reference.kind is selector.kind
        and (selector.identifier is None or reference.identifier == selector.identifier)
    )


def _context_failure(
    *,
    role: RoleRef,
    source: RoleDefinitionSource,
    selector: ContextSelector,
    candidates: Sequence[ContextReference],
    work_order: WorkOrderRef,
    clock: datetime,
    snapshot_revision: str,
    bounds: ContextBundleBounds,
) -> RoleResolutionBlocked | None:
    if not candidates:
        return _blocked(
            role,
            RoleResolutionReason.MISSING_CONTEXT,
            source=source,
            reference=selector.reference,
        )
    ordered = sorted(candidates, key=_candidate_key)
    checks = (
        (
            RoleResolutionReason.MIXED_REVISION,
            lambda item: item.snapshot_revision != snapshot_revision,
        ),
        (RoleResolutionReason.UNREADABLE_CONTEXT, lambda item: not item.readable),
        (
            RoleResolutionReason.CONTEXT_OUT_OF_SCOPE,
            lambda item: not _same_work_order_scope(item.work_order_scope, work_order),
        ),
        (
            RoleResolutionReason.STALE_CONTEXT,
            lambda item: (
                selector.requires_freshness
                and item.fresh_until is None
                or item.fresh_until is not None
                and item.fresh_until < clock
            ),
        ),
        (
            RoleResolutionReason.SENSITIVITY_REFUSED,
            lambda item: (
                _SENSITIVITY_RANK[item.sensitivity]
                > min(
                    _SENSITIVITY_RANK[selector.max_sensitivity],
                    _SENSITIVITY_RANK[bounds.max_sensitivity],
                )
            ),
        ),
    )

    def blocked_for(
        item: ContextReference,
        reason: RoleResolutionReason,
    ) -> RoleResolutionBlocked:
        detail: str | None = None
        if reason is RoleResolutionReason.UNREADABLE_CONTEXT:
            detail = item.unavailable_reason
        elif (
            reason is RoleResolutionReason.STALE_CONTEXT
            and selector.requires_freshness
            and item.fresh_until is None
        ):
            detail = "freshness metadata required"
        return _blocked(
            role,
            reason,
            source=source,
            source_id=item.provenance,
            reference=f"{item.kind.value}:{item.identifier}",
            detail=detail,
        ).model_copy(update={"source_layer": source.layer, "source_id": item.provenance})

    for reason, predicate in checks:
        if all(predicate(item) for item in ordered):
            return blocked_for(ordered[0], reason)
    for item in ordered:
        for reason, predicate in checks:
            if predicate(item):
                return blocked_for(item, reason)
    return None


def _eligible(
    selector: ContextSelector,
    candidates: Sequence[ContextReference],
    *,
    work_order: WorkOrderRef,
    clock: datetime,
    snapshot_revision: str,
    bounds: ContextBundleBounds,
) -> tuple[ContextReference, ...]:
    ceiling = min(
        _SENSITIVITY_RANK[selector.max_sensitivity],
        _SENSITIVITY_RANK[bounds.max_sensitivity],
    )
    return tuple(
        sorted(
            (
                item
                for item in candidates
                if item.snapshot_revision == snapshot_revision
                and item.readable
                and _same_work_order_scope(item.work_order_scope, work_order)
                and (not selector.requires_freshness or item.fresh_until is not None)
                and (item.fresh_until is None or item.fresh_until >= clock)
                and _SENSITIVITY_RANK[item.sensitivity] <= ceiling
            ),
            key=_candidate_key,
        )
    )


def _select_bundle(
    *,
    role: RoleRef,
    manifest: RoleManifest,
    source: RoleDefinitionSource,
    catalog: Sequence[ContextReference],
    work_order: WorkOrderRef,
    clock: datetime,
    snapshot_revision: str,
    bounds: ContextBundleBounds,
) -> ContextBundle | RoleResolutionBlocked:
    candidate_sets: list[tuple[ContextReference, ...]] = []
    for selector in manifest.context_selectors:
        candidates = _matching_candidates(selector, catalog)
        failure = _context_failure(
            role=role,
            source=source,
            selector=selector,
            candidates=candidates,
            work_order=work_order,
            clock=clock,
            snapshot_revision=snapshot_revision,
            bounds=bounds,
        )
        eligible = _eligible(
            selector,
            candidates,
            work_order=work_order,
            clock=clock,
            snapshot_revision=snapshot_revision,
            bounds=bounds,
        )
        if not eligible:
            if failure is not None:
                return failure
            return _blocked(
                role,
                RoleResolutionReason.MISSING_CONTEXT,
                source=source,
                reference=selector.reference,
            )
        candidate_sets.append(eligible)

    combinations: Iterable[tuple[ContextReference, ...]]
    combination_count = math.prod(len(candidates) for candidates in candidate_sets)
    if combination_count > bounds.max_combinations:
        return _blocked(
            role,
            RoleResolutionReason.OVER_BUDGET,
            source=source,
            reference="context_combinations",
            detail=(
                f"{combination_count} candidate combinations exceed limit {bounds.max_combinations}"
            ),
        )
    combinations = itertools.product(*candidate_sets) if candidate_sets else ((),)
    valid: list[tuple[tuple[object, ...], tuple[ContextReference, ...]]] = []
    for combination in combinations:
        unique = {
            (
                item.kind.value,
                item.identifier,
                item.provenance,
                item.content_digest,
            ): item
            for item in combination
        }
        references = tuple(
            sorted(
                unique.values(),
                key=lambda item: (
                    item.kind.value,
                    item.identifier,
                    item.provenance,
                    item.content_digest,
                ),
            )
        )
        total_bytes = sum(item.byte_size for item in references)
        if len(references) > bounds.max_references or total_bytes > bounds.max_bytes:
            continue
        stable_refs = tuple(
            (item.kind.value, item.identifier, item.provenance, item.content_digest)
            for item in references
        )
        valid.append(((total_bytes, len(references), stable_refs), references))
    if not valid:
        return _blocked(
            role,
            RoleResolutionReason.OVER_BUDGET,
            source=source,
            reference="context_bundle",
        )
    _, references = min(valid, key=lambda item: item[0])
    total_bytes = sum(item.byte_size for item in references)
    digest_value = _digest(
        "context-bundle",
        {
            "work_order": work_order.model_dump(mode="json"),
            "snapshot_revision": snapshot_revision,
            "references": [item.model_dump(mode="json") for item in references],
            "total_bytes": total_bytes,
        },
    )
    return ContextBundle(
        work_order=work_order,
        snapshot_revision=snapshot_revision,
        references=references,
        total_bytes=total_bytes,
        digest=digest_value,
    )


def resolve_role(
    *,
    role: RoleRef,
    definitions: Sequence[RoleDefinitionSource],
    capability_facts: Sequence[CapabilityFact],
    context_catalog: Sequence[ContextReference],
    work_order: WorkOrderRef,
    clock: datetime,
    snapshot_revision: str,
    bundle_bounds: ContextBundleBounds,
    unchecked_definitions: Sequence[DiscoveredRoleDefinition] = (),
) -> RoleResolution:
    """Resolve one role without reads, writes, grants, or implicit fallback."""
    if clock.tzinfo is None:
        raise ValueError("clock must be timezone-aware")
    if work_order.role_id is not None and work_order.role_id != role.id:
        return _blocked(
            role,
            RoleResolutionReason.INVALID_MANIFEST,
            reference="work_order.role_id",
            detail="work order role does not match requested role",
        )
    for record in unchecked_definitions:
        if (
            record.definition is None
            and record.status is not DefinitionStatus.VALID
            and (record.role is None or record.role == role)
        ):
            return RoleResolutionBlocked(
                role=role,
                reason=RoleResolutionReason.INVALID_MANIFEST,
                source_layer=record.layer,
                source_id=record.source_id,
                reference=role.id,
                detail=record.error or "source identity could not be validated",
            )
    registry = RoleRegistry(records=tuple(definitions))
    try:
        ordered = registry.definitions_for(role)
    except RegistryError as exc:
        matches = [item for item in definitions if item.role == role]
        source = sorted(
            matches, key=lambda item: (tuple(RoleLayer).index(item.layer), item.source_id)
        )[0]
        return _blocked(
            role,
            RoleResolutionReason.INVALID_OVERLAY,
            source=source,
            reference=role.id,
            detail=str(exc),
        )
    if not ordered:
        return _blocked(role, RoleResolutionReason.NOT_FOUND, reference=role.id)
    for source in ordered:
        if source.snapshot_revision != snapshot_revision:
            return _blocked(
                role,
                RoleResolutionReason.MIXED_REVISION,
                source=source,
                reference=source.snapshot_revision,
            )
        if source.status is not DefinitionStatus.VALID:
            return _blocked(
                role,
                RoleResolutionReason.INVALID_MANIFEST,
                source=source,
                reference=source.role.id,
                detail=source.error,
            )
    valid_sources = tuple(source for source in ordered if source.manifest is not None)
    manifest = valid_sources[0].manifest
    assert manifest is not None
    for source in valid_sources[1:]:
        next_manifest = source.manifest
        assert next_manifest is not None
        violation = _overlay_violation(manifest, next_manifest)
        if violation is not None:
            reason, reference = violation
            return _blocked(
                role,
                reason,
                source=source,
                reference=reference,
            )
        manifest = next_manifest
    highest_source = valid_sources[-1]

    for capability in manifest.required_capabilities:
        matching_facts = tuple(
            fact
            for fact in sorted(
                capability_facts,
                key=lambda item: (item.capability, item.source_id),
            )
            if fact.capability == capability
            and fact.available
            and _capability_fact_applies(fact, work_order=work_order, role=role)
        )
        if not matching_facts:
            return _blocked(
                role,
                RoleResolutionReason.MISSING_CAPABILITY,
                source=highest_source,
                reference=capability,
            )
        current_facts = tuple(
            fact for fact in matching_facts if fact.snapshot_revision == snapshot_revision
        )
        fact = current_facts[0] if current_facts else matching_facts[0]
        if fact.snapshot_revision != snapshot_revision:
            return _blocked(
                role,
                RoleResolutionReason.MIXED_REVISION,
                source_id=fact.source_id,
                reference=capability,
            )

    bundle = _select_bundle(
        role=role,
        manifest=manifest,
        source=highest_source,
        catalog=context_catalog,
        work_order=work_order,
        clock=clock,
        snapshot_revision=snapshot_revision,
        bounds=bundle_bounds,
    )
    if isinstance(bundle, RoleResolutionBlocked):
        return bundle

    source_chain = tuple(
        ResolvedSource(
            layer=source.layer,
            source_id=source.source_id,
            snapshot_revision=source.snapshot_revision,
            status=source.status,
            disposition="contributing" if source is highest_source else "shadowed",
            manifest_digest=_digest("role-source", source.manifest),
        )
        for source in valid_sources
    )
    return ResolvedRole(
        role=role,
        work_order=work_order,
        snapshot_revision=snapshot_revision,
        source_chain=source_chain,
        manifest=manifest,
        manifest_digest=_digest("role-manifest", manifest),
        context_bundle=bundle,
        required_capabilities=manifest.required_capabilities,
        authority_ceiling=manifest.authority_ceiling,
        approval_floor=manifest.approval_floor,
        review_policy=manifest.review_policy,
        delivery_policy=manifest.delivery_policy,
        routing_projection=manifest.routing_hint,
    )


def resolve_manifest_routing(
    role_id: str,
    records: Sequence[DiscoveredRoleDefinition],
) -> ManifestRoutingResolution | RoleResolutionBlocked:
    """Validate discovered manifests and return only their routing projection."""
    candidates = tuple(
        record for record in records if record.role is not None and record.role.id == role_id
    )
    functions = sorted({record.role.function_id for record in candidates if record.role})
    role = RoleRef(
        id=role_id,
        function_id=functions[0] if len(functions) == 1 else "unavailable",
    )
    unchecked = next(
        (
            record
            for record in records
            if record.role is None and record.status is not DefinitionStatus.VALID
        ),
        None,
    )
    if unchecked is not None:
        return RoleResolutionBlocked(
            role=role,
            reason=RoleResolutionReason.INVALID_MANIFEST,
            source_layer=unchecked.layer,
            source_id=unchecked.source_id,
            reference=role_id,
            detail=unchecked.error or "source identity could not be validated",
        )
    if len(functions) > 1:
        source = candidates[0]
        return RoleResolutionBlocked(
            role=role,
            reason=RoleResolutionReason.INVALID_MANIFEST,
            source_layer=source.layer,
            source_id=source.source_id,
            reference=role_id,
            detail=(f"role {role_id!r} exists in multiple functions: " + ", ".join(functions)),
        )
    if not candidates:
        return RoleResolutionBlocked(
            role=role,
            reason=RoleResolutionReason.NOT_FOUND,
            reference=role_id,
        )
    missing_definition = next(
        (record for record in candidates if record.definition is None),
        None,
    )
    if missing_definition is not None:
        return RoleResolutionBlocked(
            role=role,
            reason=RoleResolutionReason.INVALID_MANIFEST,
            source_layer=missing_definition.layer,
            source_id=missing_definition.source_id,
            reference=role_id,
            detail=missing_definition.error or "source could not be validated",
        )
    definitions = tuple(record.definition for record in candidates if record.definition is not None)
    registry = RoleRegistry(records=definitions)
    try:
        ordered = registry.definitions_for(role)
    except RegistryError as exc:
        candidate_source = candidates[0]
        return RoleResolutionBlocked(
            role=role,
            reason=RoleResolutionReason.INVALID_OVERLAY,
            source_layer=candidate_source.layer,
            source_id=candidate_source.source_id,
            reference=role_id,
            detail=str(exc),
        )
    revision = ordered[0].snapshot_revision
    manifest: RoleManifest | None = None
    for definition_source in ordered:
        if definition_source.snapshot_revision != revision:
            return _blocked(
                role,
                RoleResolutionReason.MIXED_REVISION,
                source=definition_source,
                reference=definition_source.snapshot_revision,
            )
        if (
            definition_source.status is not DefinitionStatus.VALID
            or definition_source.manifest is None
        ):
            return _blocked(
                role,
                RoleResolutionReason.INVALID_MANIFEST,
                source=definition_source,
                reference=role_id,
                detail=definition_source.error,
            )
        if manifest is not None:
            violation = _overlay_violation(manifest, definition_source.manifest)
            if violation is not None:
                reason, reference = violation
                return _blocked(
                    role,
                    reason,
                    source=definition_source,
                    reference=reference,
                )
        manifest = definition_source.manifest
    assert manifest is not None
    return ManifestRoutingResolution(
        role=role,
        source_digest=_digest(
            "role-routing-sources",
            [source.model_dump(mode="json") for source in ordered],
        ),
        routing_projection=manifest.routing_hint,
    )
