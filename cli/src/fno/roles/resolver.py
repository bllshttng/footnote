"""Pure deterministic resolution for immutable role and context snapshots."""

from __future__ import annotations

import hashlib
import itertools
import json
from datetime import datetime
from typing import Iterable, Sequence

from fno.company.contracts import RoleRef, WorkOrderRef
from fno.roles.models import (
    AuthorityCeiling,
    CapabilityFact,
    ContextBundle,
    ContextBundleBounds,
    ContextReference,
    ContextSelector,
    DefinitionStatus,
    ResolvedRole,
    ResolvedSource,
    RoleDefinitionSource,
    RoleLayer,
    RoleManifest,
    RoleResolution,
    RoleResolutionBlocked,
    RoleResolutionReason,
    Sensitivity,
)
from fno.roles.registry import RegistryError, RoleRegistry

_AUTHORITY_RANK = {
    AuthorityCeiling.ADVISORY: 0,
    AuthorityCeiling.INTERNAL: 1,
    AuthorityCeiling.EXTERNAL: 2,
}
_SENSITIVITY_RANK = {
    Sensitivity.PUBLIC: 0,
    Sensitivity.INTERNAL: 1,
    Sensitivity.SENSITIVE: 2,
    Sensitivity.RESTRICTED: 3,
}


def _digest(domain: str, value: object) -> str:
    if hasattr(value, "model_dump"):
        value = value.model_dump(mode="json")
    canonical = json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    )
    payload = f"fno/{domain}/v1\x00{canonical}".encode()
    return hashlib.sha256(payload).hexdigest()


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
            lambda item: item.fresh_until is not None and item.fresh_until < clock,
        ),
        (
            RoleResolutionReason.SENSITIVITY_REFUSED,
            lambda item: _SENSITIVITY_RANK[item.sensitivity]
            > min(
                _SENSITIVITY_RANK[selector.max_sensitivity],
                _SENSITIVITY_RANK[bounds.max_sensitivity],
            ),
        ),
    )
    for reason, predicate in checks:
        if all(predicate(item) for item in ordered):
            item = ordered[0]
            return _blocked(
                role,
                reason,
                source=source,
                source_id=item.provenance,
                reference=f"{item.kind.value}:{item.identifier}",
                detail=item.unavailable_reason if reason is RoleResolutionReason.UNREADABLE_CONTEXT else None,
            ).model_copy(update={"source_layer": source.layer, "source_id": item.provenance})
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
    registry = RoleRegistry(records=tuple(definitions))
    try:
        ordered = registry.definitions_for(role)
    except RegistryError as exc:
        matches = [item for item in definitions if item.role == role]
        source = sorted(matches, key=lambda item: (tuple(RoleLayer).index(item.layer), item.source_id))[0]
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
        if _AUTHORITY_RANK[next_manifest.authority_ceiling] > _AUTHORITY_RANK[manifest.authority_ceiling]:
            return _blocked(
                role,
                RoleResolutionReason.AUTHORITY_EXPANSION,
                source=source,
                reference="authority_ceiling",
            )
        manifest = next_manifest
    highest_source = valid_sources[-1]

    fact_by_capability = {
        fact.capability: fact for fact in sorted(capability_facts, key=lambda item: (item.capability, item.source_id))
    }
    for capability in manifest.required_capabilities:
        fact = fact_by_capability.get(capability)
        if fact is None or not fact.available:
            return _blocked(
                role,
                RoleResolutionReason.MISSING_CAPABILITY,
                source=highest_source,
                reference=capability,
            )
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
        review_policy=manifest.review_policy,
        delivery_policy=manifest.delivery_policy,
        routing_projection=manifest.routing_hint,
    )
