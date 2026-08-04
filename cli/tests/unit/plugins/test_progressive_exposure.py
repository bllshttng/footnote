from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

import yaml

from fno.company.contracts import FunctionRef, RoleRef, WorkOrderRef
from fno.plugins.activate import activate
from fno.plugins.manifest import AssetDeclaration, CompatibilityRange, PackManifest
from fno.plugins.registry import PackRegistryStore
from fno.roles.models import (
    AuthorityCeiling,
    CapabilityFact,
    ContextBundleBounds,
    DeliveryPolicy,
    ReviewPolicy,
    RoleManifest,
    RoleResolutionBlocked,
    RoleResolutionReason,
)
from fno.roles.registry import discover_role_definitions
from fno.roles.resolver import resolve_role


def _role(role_id: str, function_id: str, capability: str) -> RoleManifest:
    return RoleManifest(
        role=RoleRef(id=role_id, function_id=function_id),
        function=FunctionRef(id=function_id),
        mission=f"perform {function_id} work",
        deliverable_kinds=("draft",),
        required_capabilities=(capability,),
        authority_ceiling=AuthorityCeiling.INTERNAL,
        review_policy=ReviewPolicy(required=True, minimum_reviewers=1),
        delivery_policy=DeliveryPolicy(required_evidence=("review",)),
        default_topology="loop",
    )


def _pack(pack_id: str, role: RoleManifest) -> PackManifest:
    return PackManifest(
        id=pack_id,
        version="0.1.0",
        footnote_compat=CompatibilityRange(minimum="0.3.0"),
        roles=(role,),
        assets=(AssetDeclaration(id=f"{pack_id}-asset", source=f"assets/{pack_id}-asset.md"),),
    )


def _write_pack(tmp_path: Path, pack: PackManifest) -> Path:
    from tests.unit.plugins.test_manifest import _materialize_declared_sources

    pack_dir = tmp_path / pack.id
    pack_dir.mkdir(exist_ok=True)
    (pack_dir / "plugin.yaml").write_text(yaml.safe_dump(pack.model_dump(mode="json")), encoding="utf-8")
    _materialize_declared_sources(pack_dir, pack)
    return pack_dir


def _resolve(root: Path, role_id: str, function_id: str, facts, work_order_override=None):
    records = discover_role_definitions(root=root)
    role_ref = RoleRef(id=role_id, function_id=function_id)
    definitions = tuple(r.definition for r in records if r.definition is not None and r.role == role_ref)
    revision = definitions[0].snapshot_revision
    work_order = work_order_override or WorkOrderRef(node_id="wo-1", attempt_id="attempt-1", role_id=role_id)
    return resolve_role(
        role=role_ref,
        definitions=definitions,
        capability_facts=facts,
        context_catalog=(),
        work_order=work_order,
        clock=datetime.now(UTC),
        snapshot_revision=revision,
        bundle_bounds=ContextBundleBounds(max_references=32, max_bytes=10_000_000),
        unchecked_definitions=records,
    )


def _fact(capability: str, revision: str, *, role_id: str, node_id="wo-1", attempt_id="attempt-1") -> CapabilityFact:
    return CapabilityFact(
        capability=capability,
        available=True,
        source_id="fact-source",
        snapshot_revision=revision,
        work_order_scope=WorkOrderRef(node_id=node_id, attempt_id=attempt_id, role_id=role_id),
    )


def test_installing_two_packs_does_not_expose_capabilities_globally(tmp_path):
    root = tmp_path / "roles"
    root.mkdir()
    store = PackRegistryStore(tmp_path / "registry.json")
    role_a = _role("role-a", "fn-a", "cap-a")
    role_b = _role("role-b", "fn-b", "cap-b")
    activate(_write_pack(tmp_path, _pack("pack-a", role_a)), registry_store=store, role_root=root)
    activate(_write_pack(tmp_path, _pack("pack-b", role_b)), registry_store=store, role_root=root)

    records = discover_role_definitions(root=root)
    revision_a = next(
        r.definition.snapshot_revision for r in records if r.role and r.role.id == "role-a"
    )

    # role-a resolves only when a cap-a fact scoped to role-a's attempt is present.
    blocked_without = _resolve(root, "role-a", "fn-a", ())
    assert isinstance(blocked_without, RoleResolutionBlocked)
    assert blocked_without.reason is RoleResolutionReason.MISSING_CAPABILITY

    resolved_with = _resolve(root, "role-a", "fn-a", (_fact("cap-a", revision_a, role_id="role-a"),))
    assert not isinstance(resolved_with, RoleResolutionBlocked)

    # A cap-a fact scoped to a different work order does not satisfy role-a
    # being resolved under wo-1.
    blocked_other_order = _resolve(
        root,
        "role-a",
        "fn-a",
        (_fact("cap-a", revision_a, role_id="role-a", node_id="wo-other"),),
    )
    assert isinstance(blocked_other_order, RoleResolutionBlocked)
    assert blocked_other_order.reason is RoleResolutionReason.MISSING_CAPABILITY

    # A cap-a fact scoped to role-b does not satisfy role-a, even in the same attempt.
    blocked_other_role = _resolve(root, "role-a", "fn-a", (_fact("cap-a", revision_a, role_id="role-b"),))
    assert isinstance(blocked_other_role, RoleResolutionBlocked)
    assert blocked_other_role.reason is RoleResolutionReason.MISSING_CAPABILITY

    # role-b needs cap-b, never cap-a; pack-a being installed gives it nothing.
    revision_b = next(
        r.definition.snapshot_revision for r in records if r.role and r.role.id == "role-b"
    )
    blocked_b_needs_cap_b = _resolve(root, "role-b", "fn-b", (_fact("cap-a", revision_b, role_id="role-b"),))
    assert isinstance(blocked_b_needs_cap_b, RoleResolutionBlocked)
    assert blocked_b_needs_cap_b.reason is RoleResolutionReason.MISSING_CAPABILITY
    resolved_b = _resolve(root, "role-b", "fn-b", (_fact("cap-b", revision_b, role_id="role-b"),))
    assert not isinstance(resolved_b, RoleResolutionBlocked)
