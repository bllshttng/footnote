"""Immutable, non-granting contracts for business-role resolution."""

from __future__ import annotations

import hashlib
import json
from datetime import datetime
from enum import Enum
from typing import Annotated, Self

from pydantic import (
    BaseModel,
    ConfigDict,
    Field,
    StringConstraints,
    field_validator,
    model_validator,
)

from fno.company.contracts import FunctionRef, RoleRef, WorkOrderRef

NonEmptyStr = Annotated[str, StringConstraints(strip_whitespace=True, min_length=1)]
Sha256Digest = Annotated[str, StringConstraints(pattern=r"^[0-9a-f]{64}$")]


def canonical_digest(domain: str, value: object) -> str:
    if hasattr(value, "model_dump"):
        value = value.model_dump(mode="json")  # type: ignore[union-attr]
    canonical = json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    )
    payload = f"fno/{domain}/v1\x00{canonical}".encode()
    return hashlib.sha256(payload).hexdigest()


def work_order_scope_matches(scope: WorkOrderRef | None, work_order: WorkOrderRef) -> bool:
    if scope is None:
        return True
    if scope.node_id != work_order.node_id or scope.attempt_id != work_order.attempt_id:
        return False
    if scope.role_id is not None and scope.role_id != work_order.role_id:
        return False
    if scope.principal_id is not None and scope.principal_id != work_order.principal_id:
        return False
    return True


class _RoleModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class RoleLayer(str, Enum):
    BUILT_IN = "built-in"
    PLUGIN = "plugin"
    COMPANY = "company"
    PROJECT = "project"
    PLAN = "plan"


class AuthorityCeiling(str, Enum):
    ADVISORY = "advisory"
    INTERNAL = "internal"
    EXTERNAL = "external"


class ApprovalFloor(str, Enum):
    NONE = "none"
    PRINCIPAL = "principal"
    FOUNDER = "founder"


class ContextKind(str, Enum):
    PLAN = "plan"
    BRIEF = "brief"
    GRAPH_NODE = "graph-node"
    ARTIFACT = "artifact"
    SOURCE = "source"
    CONFIG = "config"


class Sensitivity(str, Enum):
    PUBLIC = "public"
    INTERNAL = "internal"
    SENSITIVE = "sensitive"
    RESTRICTED = "restricted"


class DefinitionStatus(str, Enum):
    VALID = "valid"
    INVALID = "invalid"
    UNREADABLE = "unreadable"


class RoleResolutionReason(str, Enum):
    NOT_FOUND = "not_found"
    INVALID_MANIFEST = "invalid_manifest"
    INVALID_OVERLAY = "invalid_overlay"
    AUTHORITY_EXPANSION = "authority_expansion"
    MISSING_CAPABILITY = "missing_capability"
    MISSING_CONTEXT = "missing_context"
    STALE_CONTEXT = "stale_context"
    SENSITIVITY_REFUSED = "sensitivity_refused"
    CONTEXT_OUT_OF_SCOPE = "context_out_of_scope"
    UNREADABLE_CONTEXT = "unreadable_context"
    OVER_BUDGET = "over_budget"
    MIXED_REVISION = "mixed_revision"


class RoutingHint(_RoleModel):
    provider: NonEmptyStr | None = None
    model: NonEmptyStr | None = None

    @model_validator(mode="after")
    def _not_empty(self) -> Self:
        if self.provider is None and self.model is None:
            raise ValueError("routing_hint must name a provider or model")
        return self


class ReviewPolicy(_RoleModel):
    required: bool = False
    minimum_reviewers: int = Field(default=0, ge=0)
    required_review_kinds: tuple[NonEmptyStr, ...] = Field(default_factory=tuple)

    @model_validator(mode="after")
    def _required_count(self) -> Self:
        if not self.required and (self.minimum_reviewers or self.required_review_kinds):
            raise ValueError("review requirements require required=true")
        if self.required and self.minimum_reviewers < 1:
            raise ValueError("required review needs at least one reviewer")
        return self


class DeliveryPolicy(_RoleModel):
    required_evidence: tuple[NonEmptyStr, ...] = Field(default_factory=tuple)
    require_all_deliverables: bool = True


class ContextSelector(_RoleModel):
    kind: ContextKind
    identifier: NonEmptyStr | None = None
    max_sensitivity: Sensitivity = Sensitivity.INTERNAL
    requires_freshness: bool = False

    @property
    def reference(self) -> str:
        return f"{self.kind.value}:{self.identifier or '*'}"


class RoleManifest(_RoleModel):
    role: RoleRef
    function: FunctionRef
    mission: NonEmptyStr
    deliverable_kinds: tuple[NonEmptyStr, ...]
    delegation_targets: tuple[RoleRef, ...] = Field(default_factory=tuple)
    required_capabilities: tuple[NonEmptyStr, ...] = Field(default_factory=tuple)
    authority_ceiling: AuthorityCeiling
    context_selectors: tuple[ContextSelector, ...] = Field(default_factory=tuple)
    review_policy: ReviewPolicy
    delivery_policy: DeliveryPolicy
    default_topology: NonEmptyStr
    approval_floor: ApprovalFloor = ApprovalFloor.NONE
    routing_hint: RoutingHint | None = None

    @model_validator(mode="after")
    def _identity_is_consistent(self) -> Self:
        if self.role.function_id != self.function.id:
            raise ValueError("role function_id must match function id")
        if len(set(self.deliverable_kinds)) != len(self.deliverable_kinds):
            raise ValueError("deliverable_kinds must be unique")
        if len(set(self.required_capabilities)) != len(self.required_capabilities):
            raise ValueError("required_capabilities must be unique")
        delegation_ids = {(item.id, item.function_id) for item in self.delegation_targets}
        if len(delegation_ids) != len(self.delegation_targets):
            raise ValueError("delegation_targets must be unique")
        return self


class RoleDefinitionSource(_RoleModel):
    layer: RoleLayer
    source_id: NonEmptyStr
    snapshot_revision: NonEmptyStr
    role: RoleRef
    manifest: RoleManifest | None = None
    status: DefinitionStatus = DefinitionStatus.VALID
    error: NonEmptyStr | None = None

    @model_validator(mode="after")
    def _status_matches_payload(self) -> Self:
        if self.status is DefinitionStatus.VALID:
            if self.manifest is None:
                raise ValueError("valid definition requires a manifest")
            if self.error is not None:
                raise ValueError("valid definition cannot carry an error")
            if self.manifest.role != self.role:
                raise ValueError("definition role must match manifest role")
        else:
            if self.manifest is not None:
                raise ValueError("invalid or unreadable definition cannot carry a manifest")
            if self.error is None:
                raise ValueError("invalid or unreadable definition requires an error")
        return self


class CapabilityFact(_RoleModel):
    capability: NonEmptyStr
    available: bool
    source_id: NonEmptyStr
    snapshot_revision: NonEmptyStr
    work_order_scope: WorkOrderRef | None = None


class ContextReference(_RoleModel):
    kind: ContextKind
    identifier: NonEmptyStr
    provenance: NonEmptyStr
    work_order_scope: WorkOrderRef | None = None
    content_digest: Sha256Digest | None = None
    content_revision: NonEmptyStr | None = None
    snapshot_revision: NonEmptyStr
    fresh_until: datetime | None = None
    sensitivity: Sensitivity
    byte_size: int = Field(ge=0)
    readable: bool
    unavailable_reason: NonEmptyStr | None = None

    @field_validator("fresh_until")
    @classmethod
    def _freshness_is_timezone_aware(cls, value: datetime | None) -> datetime | None:
        if value is not None and value.tzinfo is None:
            raise ValueError("fresh_until must be timezone-aware")
        return value

    @model_validator(mode="after")
    def _availability_is_honest(self) -> Self:
        if self.readable and self.unavailable_reason is not None:
            raise ValueError("readable context cannot have an unavailable_reason")
        if not self.readable and self.unavailable_reason is None:
            raise ValueError("unreadable context requires an unavailable_reason")
        # A digest/revision is a claim to have read the content. An unreadable
        # reference must carry neither (no fabricated digest); a readable one
        # must carry both. This is the honest-observation contract: digest
        # presence is validated only for content that was actually read.
        if self.readable and (self.content_digest is None or self.content_revision is None):
            raise ValueError("readable context requires content_digest and content_revision")
        if not self.readable and (self.content_digest is not None or self.content_revision is not None):
            raise ValueError("unreadable context cannot carry a content_digest or content_revision")
        return self


class ContextBundleBounds(_RoleModel):
    max_references: int = Field(ge=1)
    max_bytes: int = Field(ge=1)
    max_sensitivity: Sensitivity = Sensitivity.SENSITIVE
    max_combinations: int = Field(default=4096, ge=1)


class ResolvedSource(_RoleModel):
    layer: RoleLayer
    source_id: NonEmptyStr
    snapshot_revision: NonEmptyStr
    status: DefinitionStatus
    disposition: Annotated[str, StringConstraints(pattern=r"^(contributing|shadowed)$")]
    manifest_digest: Sha256Digest | None = None
    error: NonEmptyStr | None = None

    @model_validator(mode="after")
    def _resolved_source_is_valid(self) -> Self:
        if self.status is not DefinitionStatus.VALID:
            raise ValueError("resolved source must be valid")
        if self.manifest_digest is None:
            raise ValueError("resolved source requires a manifest_digest")
        if self.error is not None:
            raise ValueError("resolved source cannot carry an error")
        return self


class ContextBundle(_RoleModel):
    work_order: WorkOrderRef
    snapshot_revision: NonEmptyStr
    references: tuple[ContextReference, ...]
    total_bytes: int = Field(ge=0)
    digest: Sha256Digest

    @model_validator(mode="after")
    def _bundle_is_coherent(self) -> Self:
        if self.total_bytes != sum(reference.byte_size for reference in self.references):
            raise ValueError("context bundle total_bytes must match references")
        for reference in self.references:
            if reference.snapshot_revision != self.snapshot_revision:
                raise ValueError("context reference snapshot_revision must match bundle")
            if not work_order_scope_matches(reference.work_order_scope, self.work_order):
                raise ValueError("context reference work_order_scope must match bundle")
        digest_value = canonical_digest(
            "context-bundle",
            {
                "work_order": self.work_order.model_dump(mode="json"),
                "snapshot_revision": self.snapshot_revision,
                "references": [item.model_dump(mode="json") for item in self.references],
                "total_bytes": self.total_bytes,
            },
        )
        if self.digest != digest_value:
            raise ValueError("context bundle digest must match captured fields")
        return self


class ResolvedRole(_RoleModel):
    role: RoleRef
    work_order: WorkOrderRef
    snapshot_revision: NonEmptyStr
    source_chain: tuple[ResolvedSource, ...] = Field(min_length=1)
    manifest: RoleManifest
    manifest_digest: Sha256Digest
    context_bundle: ContextBundle
    required_capabilities: tuple[NonEmptyStr, ...]
    authority_ceiling: AuthorityCeiling
    approval_floor: ApprovalFloor = ApprovalFloor.NONE
    review_policy: ReviewPolicy
    delivery_policy: DeliveryPolicy
    routing_projection: RoutingHint | None = None

    @model_validator(mode="after")
    def _resolved_projection_is_coherent(self) -> Self:
        if self.role != self.manifest.role:
            raise ValueError("resolved role must match manifest role")
        if self.work_order.role_id is not None and self.work_order.role_id != self.role.id:
            raise ValueError("work_order role_id must match resolved role")
        if self.context_bundle.work_order != self.work_order:
            raise ValueError("context bundle work_order must match resolved role")
        if self.context_bundle.snapshot_revision != self.snapshot_revision:
            raise ValueError("context bundle snapshot_revision must match resolved role")
        if self.manifest_digest != canonical_digest("role-manifest", self.manifest):
            raise ValueError("manifest_digest must match manifest")
        if any(source.snapshot_revision != self.snapshot_revision for source in self.source_chain):
            raise ValueError("source snapshot_revision must match resolved role")
        dispositions = tuple(source.disposition for source in self.source_chain)
        if dispositions != ("shadowed",) * (len(self.source_chain) - 1) + ("contributing",):
            raise ValueError("source_chain must end with its sole contributing source")
        layers = tuple(source.layer for source in self.source_chain)
        if len(set(layers)) != len(layers) or layers != tuple(
            sorted(layers, key=tuple(RoleLayer).index)
        ):
            raise ValueError("source_chain must follow unique fixed layer order")
        manifest_fields = (
            (
                "required_capabilities",
                self.required_capabilities,
                self.manifest.required_capabilities,
            ),
            ("authority_ceiling", self.authority_ceiling, self.manifest.authority_ceiling),
            ("approval_floor", self.approval_floor, self.manifest.approval_floor),
            ("review_policy", self.review_policy, self.manifest.review_policy),
            ("delivery_policy", self.delivery_policy, self.manifest.delivery_policy),
            ("routing_projection", self.routing_projection, self.manifest.routing_hint),
        )
        for field, actual, expected in manifest_fields:
            if actual != expected:
                raise ValueError(f"{field} must match manifest")
        return self


class ManifestRoutingResolution(_RoleModel):
    """Routing-only projection of a validated business-role manifest chain."""

    role: RoleRef
    source_digest: Sha256Digest
    routing_projection: RoutingHint | None = None


class RoleResolutionBlocked(_RoleModel):
    role: RoleRef
    reason: RoleResolutionReason
    source_layer: RoleLayer | None = None
    source_id: NonEmptyStr | None = None
    reference: NonEmptyStr | None = None
    detail: NonEmptyStr | None = None


RoleResolution = ResolvedRole | RoleResolutionBlocked
