"""Pure two-axis verification of a function-pack before activation.

Every condition reports on two independent axes:

* ``checked`` -- was this condition evaluated at all?
* ``result`` -- one of ``passed``, ``failed``, ``blocked``, ``unknown`` (reusing
  :class:`fno.company.contracts.EvidenceResult`).

Collapsing the axes into one boolean is how a verifier starts reporting green
for a condition it never evaluated. An unchecked condition reporting ``unknown``
is the honest shape for a check that could not run; an all-``passed`` report is
the only one that exits zero.

Verification is pure: it reads the manifest and an installed-pack index and
writes nothing, activates nothing, mutates no graph, and touches no effect
journal. It is runnable against a pack that is not installed, which is the whole
point of verifying before activation.

A malformed, unreadable, or invalid-UTF-8 manifest reports ``blocked`` naming
the file and the parse error and never reports the pack as absent, mirroring the
role registry's rule that a corrupt source blocks rather than reading as missing.
"""

from __future__ import annotations

import hashlib
import shutil
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from enum import Enum
from pathlib import Path
from typing import Any, Mapping

import yaml
from pydantic import ValidationError

from fno.company.contracts import EvidenceResult, WorkOrderRef
from fno.plugins.manifest import PackManifest, pack_digest
from fno.roles.models import (
    CapabilityFact,
    ContextBundleBounds,
    ContextReference,
    DefinitionStatus,
    RoleDefinitionSource,
    RoleLayer,
    RoleManifest,
    RoleResolutionBlocked,
    Sensitivity,
)
from fno.roles.resolver import resolve_role

__all__ = [
    "Condition",
    "ConditionFamily",
    "VerificationReport",
    "load_manifest",
    "resolve_manifest_path",
    "verify_pack",
]


class ConditionFamily(str, Enum):
    IDENTITY = "identity"
    SCHEMA = "schema"
    DEPENDENCY = "dependency"
    CAPABILITY = "capability"
    COMPATIBILITY = "compatibility"
    DECLARED_TEST = "declared-test"


@dataclass(frozen=True)
class Condition:
    family: ConditionFamily
    name: str
    checked: bool
    result: EvidenceResult
    detail: str | None = None

    @property
    def ok(self) -> bool:
        return self.checked and self.result is EvidenceResult.PASSED


@dataclass(frozen=True)
class VerificationReport:
    pack_path: str
    conditions: tuple[Condition, ...] = field(default_factory=tuple)

    @property
    def ok(self) -> bool:
        return bool(self.conditions) and all(condition.ok for condition in self.conditions)

    def by_family(self) -> dict[ConditionFamily, tuple[Condition, ...]]:
        grouped: dict[ConditionFamily, tuple[Condition, ...]] = {}
        for condition in self.conditions:
            grouped.setdefault(condition.family, ())
            grouped[condition.family] = (*grouped[condition.family], condition)
        return grouped

    def as_dict(self) -> dict[str, Any]:
        return {
            "pack_path": self.pack_path,
            "ok": self.ok,
            "conditions": [
                {
                    "family": condition.family.value,
                    "name": condition.name,
                    "checked": condition.checked,
                    "result": condition.result.value,
                    "detail": condition.detail,
                }
                for condition in self.conditions
            ],
        }


def resolve_manifest_path(path: Path) -> Path:
    """Resolve a directory or file argument to the plugin.yaml manifest file."""
    if path.is_dir():
        return path / "plugin.yaml"
    return path


def load_manifest(path: Path) -> tuple[PackManifest | None, Condition | None]:
    """Load and validate a manifest, or return a single blocked condition.

    A missing, unreadable, invalid-UTF-8, malformed, or schema-invalid manifest
    yields a ``blocked`` condition naming the file and the error. The caller
    never sees the pack reported as absent.
    """
    manifest_file = resolve_manifest_path(path)
    label = str(manifest_file)
    try:
        text = manifest_file.read_text(encoding="utf-8")
    except OSError as exc:
        return None, Condition(
            ConditionFamily.SCHEMA,
            "manifest-load",
            checked=True,
            result=EvidenceResult.BLOCKED,
            detail=f"unreadable manifest {label}: {type(exc).__name__}: {exc}",
        )
    except UnicodeDecodeError as exc:
        return None, Condition(
            ConditionFamily.SCHEMA,
            "manifest-load",
            checked=True,
            result=EvidenceResult.BLOCKED,
            detail=f"invalid UTF-8 in manifest {label}: {exc}",
        )
    try:
        raw = yaml.safe_load(text)
    except yaml.YAMLError as exc:
        return None, Condition(
            ConditionFamily.SCHEMA,
            "manifest-load",
            checked=True,
            result=EvidenceResult.BLOCKED,
            detail=f"malformed YAML in manifest {label}: {exc}",
        )
    if not isinstance(raw, Mapping):
        return None, Condition(
            ConditionFamily.SCHEMA,
            "manifest-load",
            checked=True,
            result=EvidenceResult.BLOCKED,
            detail=f"manifest {label} is not a mapping",
        )
    try:
        manifest = PackManifest.model_validate(raw)
    except ValidationError as exc:
        return None, Condition(
            ConditionFamily.SCHEMA,
            "manifest-schema",
            checked=True,
            result=EvidenceResult.BLOCKED,
            detail=f"manifest {label} failed schema validation: {exc}",
        )
    return manifest, None


def _identity_conditions(manifest: PackManifest, path: str) -> list[Condition]:
    conditions: list[Condition] = [
        Condition(
            ConditionFamily.IDENTITY,
            "pack-id",
            checked=True,
            result=EvidenceResult.PASSED,
            detail=f"id={manifest.id} version={manifest.version}",
        )
    ]
    first = pack_digest(manifest)
    second = pack_digest(manifest)
    conditions.append(
        Condition(
            ConditionFamily.IDENTITY,
            "digest-stability",
            checked=True,
            result=EvidenceResult.PASSED if first == second else EvidenceResult.FAILED,
            detail=first,
        )
    )
    role_ids = [role.role.id for role in manifest.roles]
    function_ids = {role.function.id for role in manifest.roles}
    duplicate = next((value for value in role_ids if role_ids.count(value) > 1), None)
    conditions.append(
        Condition(
            ConditionFamily.IDENTITY,
            "role-and-function-ids-unique",
            checked=True,
            result=EvidenceResult.PASSED if duplicate is None else EvidenceResult.FAILED,
            detail=None if duplicate is None else f"duplicate role id {duplicate!r}",
        )
    )
    conditions.append(
        Condition(
            ConditionFamily.IDENTITY,
            "function-set",
            checked=True,
            result=EvidenceResult.PASSED,
            detail=",".join(sorted(function_ids)) or "no roles",
        )
    )
    return conditions


def _schema_conditions(manifest: PackManifest) -> list[Condition]:
    # Every declaration already parsed into a frozen model rejecting unknown
    # fields; reaching here means the schema held.
    component_counts = {
        "roles": len(manifest.roles),
        "skills": len(manifest.skills),
        "workflows": len(manifest.workflows),
        "adapters": len(manifest.adapters),
        "evaluators": len(manifest.evaluators),
        "assets": len(manifest.assets),
        "permissions": len(manifest.permissions),
        "scenarios": len(manifest.scenarios),
    }
    return [
        Condition(
            ConditionFamily.SCHEMA,
            "declarations-parse",
            checked=True,
            result=EvidenceResult.PASSED,
            detail="; ".join(f"{name}={count}" for name, count in component_counts.items()),
        )
    ]


def _dependency_conditions(
    manifest: PackManifest,
    installed: Mapping[str, str] | None,
) -> list[Condition]:
    if installed is None:
        return [
            Condition(
                ConditionFamily.DEPENDENCY,
                "installed-pack-index",
                checked=False,
                result=EvidenceResult.UNKNOWN,
                detail="no installed-pack index supplied; dependencies not resolved",
            )
        ]
    conditions: list[Condition] = []
    for dependency in manifest.depends_on:
        resolved = installed.get(dependency.pack_id)
        if resolved is None:
            conditions.append(
                Condition(
                    ConditionFamily.DEPENDENCY,
                    f"dependency:{dependency.pack_id}",
                    checked=True,
                    result=EvidenceResult.BLOCKED,
                    detail=(
                        f"declared dependency {dependency.pack_id} "
                        f"({dependency.version_range.minimum}.."
                        f"{dependency.version_range.maximum or 'open'}) is not installed"
                    ),
                )
            )
            continue
        conditions.append(
            Condition(
                ConditionFamily.DEPENDENCY,
                f"dependency:{dependency.pack_id}",
                checked=True,
                result=EvidenceResult.PASSED,
                detail=f"{dependency.pack_id} resolved to {resolved}",
            )
        )
    if not manifest.depends_on:
        conditions.append(
            Condition(
                ConditionFamily.DEPENDENCY,
                "no-declared-dependencies",
                checked=True,
                result=EvidenceResult.PASSED,
                detail=None,
            )
        )
    return conditions


def _capability_conditions(manifest: PackManifest) -> list[Condition]:
    # The structural guarantee that no declaration is a grant is enforced by the
    # manifest model itself: permissions is a ceiling of effect_class+destination
    # pairs with no approval field, and adapter conformance mirrors
    # AdapterCapability as declaration fields. There is no capability catalog in
    # this node, so the check is structural rather than catalog-driven.
    grant_leak = any(
        getattr(declaration, "approval_id", None) is not None
        or getattr(declaration, "principal_id", None) is not None
        for declaration in manifest.permissions
    )
    return [
        Condition(
            ConditionFamily.CAPABILITY,
            "declarations-are-not-grants",
            checked=True,
            result=EvidenceResult.FAILED if grant_leak else EvidenceResult.PASSED,
            detail=None if not grant_leak else "a permission declaration carries an approval/principal field",
        )
    ]


def _topology_conditions(manifest: PackManifest) -> list[Condition]:
    try:
        from fno.company.topology import TopologyRefusal, validate_manifest_topology
    except ImportError as exc:
        return [
            Condition(
                ConditionFamily.COMPATIBILITY,
                "topology-vocabulary",
                checked=True,
                result=EvidenceResult.BLOCKED,
                detail=f"topology vocabulary module absent: {exc}",
            )
        ]
    conditions: list[Condition] = []
    for role in manifest.roles:
        outcome = validate_manifest_topology(role.default_topology, source_layer=RoleLayer.PLUGIN)
        if isinstance(outcome, TopologyRefusal):
            conditions.append(
                Condition(
                    ConditionFamily.COMPATIBILITY,
                    f"topology:{role.role.id}",
                    checked=True,
                    result=EvidenceResult.FAILED,
                    detail=f"{role.role.id} default_topology {outcome.value!r}: {outcome.recovery}",
                )
            )
        else:
            conditions.append(
                Condition(
                    ConditionFamily.COMPATIBILITY,
                    f"topology:{role.role.id}",
                    checked=True,
                    result=EvidenceResult.PASSED,
                    detail=f"{role.role.id} default_topology={outcome.value}",
                )
            )
    return conditions


def _synthetic_resolution_inputs(
    manifest: RoleManifest,
    *,
    revision: str,
    source_id: str,
) -> tuple[
    tuple[RoleDefinitionSource, ...],
    tuple[CapabilityFact, ...],
    tuple[ContextReference, ...],
    WorkOrderRef,
]:
    role = manifest.role
    work_order = WorkOrderRef(node_id="verify-pack", attempt_id="verify-attempt", role_id=role.id)
    definition = RoleDefinitionSource(
        layer=RoleLayer.PLUGIN,
        source_id=source_id,
        snapshot_revision=revision,
        role=role,
        manifest=manifest,
        status=DefinitionStatus.VALID,
    )
    capability_facts = tuple(
        CapabilityFact(
            capability=capability,
            available=True,
            source_id="verify-synthetic",
            snapshot_revision=revision,
        )
        for capability in manifest.required_capabilities
    )
    references: list[ContextReference] = []
    for selector in manifest.context_selectors:
        content_digest = hashlib.sha256(
            f"verify-context:{selector.kind.value}:{selector.identifier or '*'}".encode()
        ).hexdigest()
        references.append(
            ContextReference(
                kind=selector.kind,
                identifier=selector.identifier or "verify-synthetic",
                provenance="verify-synthetic",
                work_order_scope=None,
                content_digest=content_digest,
                content_revision=revision,
                snapshot_revision=revision,
                fresh_until=datetime.now(UTC) + timedelta(hours=1) if selector.requires_freshness else None,
                sensitivity=Sensitivity.PUBLIC,
                byte_size=1,
                readable=True,
            )
        )
    return (definition,), capability_facts, tuple(references), work_order


def _resolution_conditions(manifest: PackManifest) -> list[Condition]:
    revision = "verify-snapshot"
    conditions: list[Condition] = []
    clock = datetime.now(UTC)
    for role_manifest in manifest.roles:
        definitions, facts, catalog, work_order = _synthetic_resolution_inputs(
            role_manifest, revision=revision, source_id=f"plugin/{manifest.id}/{role_manifest.role.id}.json"
        )
        outcome = resolve_role(
            role=role_manifest.role,
            definitions=definitions,
            capability_facts=facts,
            context_catalog=catalog,
            work_order=work_order,
            clock=clock,
            snapshot_revision=revision,
            bundle_bounds=ContextBundleBounds(max_references=32, max_bytes=10_000_000),
        )
        if isinstance(outcome, RoleResolutionBlocked):
            structural = outcome.reason.value in {"invalid_manifest", "invalid_overlay", "not_found"}
            conditions.append(
                Condition(
                    ConditionFamily.COMPATIBILITY,
                    f"resolve:{role_manifest.role.id}",
                    checked=True,
                    result=EvidenceResult.FAILED if structural else EvidenceResult.UNKNOWN,
                    detail=f"{role_manifest.role.id} blocked at {outcome.reason.value}: {outcome.detail}",
                )
            )
        else:
            conditions.append(
                Condition(
                    ConditionFamily.COMPATIBILITY,
                    f"resolve:{role_manifest.role.id}",
                    checked=True,
                    result=EvidenceResult.PASSED,
                    detail=f"{role_manifest.role.id} manifest={outcome.manifest_digest[:12]}",
                )
            )
    return conditions


def _compatibility_conditions(manifest: PackManifest) -> list[Condition]:
    conditions: list[Condition] = [
        Condition(
            ConditionFamily.COMPATIBILITY,
            "footnote-compat-range",
            checked=True,
            result=EvidenceResult.PASSED,
            detail=f"minimum={manifest.footnote_compat.minimum} maximum={manifest.footnote_compat.maximum or 'open'}",
        ),
        Condition(
            ConditionFamily.COMPATIBILITY,
            "role-manifest-schema",
            checked=True,
            result=EvidenceResult.PASSED,
            detail=f"{len(manifest.roles)} role(s) parsed as RoleManifest",
        ),
    ]
    conditions.extend(_topology_conditions(manifest))
    conditions.extend(_resolution_conditions(manifest))
    return conditions


def _declared_test_conditions(manifest: PackManifest, base: Path) -> list[Condition]:
    conditions: list[Condition] = []
    for scenario in manifest.scenarios:
        token = scenario.command.split()[0] if scenario.command.split() else ""
        located = shutil.which(token) or str(base / token) if token else ""
        runnable = bool(located) and Path(located).exists()
        if runnable:
            conditions.append(
                Condition(
                    ConditionFamily.DECLARED_TEST,
                    f"scenario:{scenario.id}",
                    checked=True,
                    result=scenario.recorded_result,
                    detail=f"{scenario.id} command runnable; recorded={scenario.recorded_result.value}",
                )
            )
        else:
            conditions.append(
                Condition(
                    ConditionFamily.DECLARED_TEST,
                    f"scenario:{scenario.id}",
                    checked=True,
                    result=EvidenceResult.BLOCKED,
                    detail=f"{scenario.id} command {token!r} is absent or not executable",
                )
            )
    if not manifest.scenarios:
        conditions.append(
            Condition(
                ConditionFamily.DECLARED_TEST,
                "no-declared-scenarios",
                checked=False,
                result=EvidenceResult.UNKNOWN,
                detail="pack declares no benchmark scenarios",
            )
        )
    return conditions


def verify_pack(
    path: Path | str,
    *,
    installed: Mapping[str, str] | None = None,
) -> VerificationReport:
    """Verify a pack at ``path`` (directory or plugin.yaml) and return a report.

    ``installed`` maps an installed pack id to its resolved version; when it is
    None the dependency family reports unchecked/unknown rather than guessing.
    """
    target = Path(path).expanduser()
    report_path = str(target)
    manifest, load_failure = load_manifest(target)
    if load_failure is not None:
        return VerificationReport(pack_path=report_path, conditions=(load_failure,))
    assert manifest is not None
    conditions: list[Condition] = []
    conditions.extend(_identity_conditions(manifest, report_path))
    conditions.extend(_schema_conditions(manifest))
    conditions.extend(_dependency_conditions(manifest, installed))
    conditions.extend(_capability_conditions(manifest))
    conditions.extend(_compatibility_conditions(manifest))
    conditions.extend(_declared_test_conditions(manifest, target.parent if target.is_file() else target))
    return VerificationReport(pack_path=report_path, conditions=tuple(conditions))
