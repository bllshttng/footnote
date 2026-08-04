from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path

import pytest
import yaml

from fno.approvals.models import classify_effect
from fno.approvals.policy import ConfigAuthority
from fno.config import load_settings
from fno.company.contracts import FunctionRef, RoleRef, WorkOrderRef
from fno.plugins.activate import (
    ActivationRefusal,
    ActivationRefusalReason,
    activate,
    deactivate,
    unconfigured_required_artifacts,
)
from fno.plugins.manifest import (
    AssetDeclaration,
    CompatibilityRange,
    PackManifest,
    ScenarioDeclaration,
    pack_digest,
)
from fno.plugins.registry import PackRegistryStore
from fno.company.contracts import EvidenceResult
from fno.roles.models import (
    AuthorityCeiling,
    ContextBundleBounds,
    ContextKind,
    ContextSelector,
    DeliveryPolicy,
    ReviewPolicy,
    RoleDefinitionSource,
    RoleLayer,
    RoleManifest,
    RoleResolutionBlocked,
    Sensitivity,
)
from fno.roles.registry import discover_role_definitions
from fno.roles.resolver import resolve_role
from tests.unit.plugins.test_manifest import _full_pack, _materialize_declared_sources


def _store_and_root(tmp_path):
    root = tmp_path / "roles"
    root.mkdir()
    store = PackRegistryStore(tmp_path / "registry.json")
    return store, root


def _write_pack(tmp_path, manifest, name="growth-studio") -> Path:
    pack_dir = tmp_path / name
    pack_dir.mkdir(exist_ok=True)
    (pack_dir / "plugin.yaml").write_text(yaml.safe_dump(manifest.model_dump(mode="json")), encoding="utf-8")
    _materialize_declared_sources(pack_dir, manifest)
    return pack_dir


def _resolve_role(root: Path, role_id: str, function_id: str):
    records = discover_role_definitions(root=root)
    role_ref = next(r.role for r in records if r.role is not None and r.role.id == role_id)
    definitions = tuple(r.definition for r in records if r.definition is not None and r.role == role_ref)
    revision = definitions[0].snapshot_revision
    work_order = WorkOrderRef(node_id="wo-1", attempt_id="attempt-1", role_id=role_ref.id)
    return resolve_role(
        role=role_ref,
        definitions=definitions,
        capability_facts=(),
        context_catalog=(),
        work_order=work_order,
        clock=datetime.now(UTC),
        snapshot_revision=revision,
        bundle_bounds=ContextBundleBounds(max_references=32, max_bytes=10_000_000),
        unchecked_definitions=records,
    )


# AC3-HP: activation makes roles resolvable through the existing resolver.


def test_activation_makes_role_resolvable_from_plugin_layer(tmp_path):
    store, root = _store_and_root(tmp_path)
    pack_dir = _write_pack(tmp_path, _full_pack())
    outcome = activate(pack_dir, registry_store=store, role_root=root)
    assert outcome.already_active is False
    result = _resolve_role(root, "marketing", "growth-studio")
    assert not isinstance(result, RoleResolutionBlocked)
    plugin_sources = [s for s in result.source_chain if s.layer is RoleLayer.PLUGIN]
    assert plugin_sources
    assert plugin_sources[-1].disposition == "contributing"


def test_written_definition_is_a_role_definition_source_at_plugin_layer(tmp_path):
    store, root = _store_and_root(tmp_path)
    pack_dir = _write_pack(tmp_path, _full_pack())
    activate(pack_dir, registry_store=store, role_root=root)
    written = root / "plugin" / "growth-studio" / "marketing.json"
    definition = RoleDefinitionSource.model_validate(json.loads(written.read_text()))
    assert definition.layer is RoleLayer.PLUGIN
    assert definition.source_id == "plugin/growth-studio/marketing.json"
    assert definition.status.value == "valid"
    assert definition.manifest is not None


# AC2-SEC: activation grants no effect.


def test_activation_leaves_authority_byte_identical_and_effect_still_blocked(tmp_path):
    store, root = _store_and_root(tmp_path)
    pack_dir = _write_pack(tmp_path, _full_pack())
    before = dict(load_settings().approvals.authorized_principals)

    activate(pack_dir, registry_store=store, role_root=root)

    after = dict(load_settings().approvals.authorized_principals)
    assert before == after
    # The declared external-publication effect still requires approval and the
    # unconfigured authority still denies it: dispatch is blocked exactly as
    # before activation. classify_effect reads only the class.
    assert classify_effect("external.publication").value == "require_approval"
    authority = ConfigAuthority()  # unconfigured => nobody may approve
    assert authority.may_approve(principal_id="founder", effect_class="external.publication", destination="social-network") is False


def test_activation_refuses_on_failed_verification_and_writes_nothing(tmp_path):
    store, root = _store_and_root(tmp_path)
    bad = _full_pack().model_copy(
        update={"roles": (_full_pack().roles[0].model_copy(update={"default_topology": "fifth-shape"}),)}
    )
    pack_dir = _write_pack(tmp_path, bad)
    with pytest.raises(ActivationRefusal) as info:
        activate(pack_dir, registry_store=store, role_root=root)
    assert info.value.reason is ActivationRefusalReason.VERIFICATION_FAILED
    # nothing written
    assert not (root / "plugin").exists() or not any((root / "plugin").rglob("*.json"))


def test_activation_refuses_on_corrupt_manifest_and_writes_nothing(tmp_path):
    store, root = _store_and_root(tmp_path)
    pack_dir = tmp_path / "growth-studio"
    pack_dir.mkdir()
    (pack_dir / "plugin.yaml").write_text("id: broken\n  bad: indent\n - x\n", encoding="utf-8")
    with pytest.raises(ActivationRefusal) as info:
        activate(pack_dir, registry_store=store, role_root=root)
    assert info.value.reason is ActivationRefusalReason.VERIFICATION_FAILED
    assert not (root / "plugin").exists() or not any((root / "plugin").rglob("*.json"))


def test_activation_is_idempotent_for_same_digest(tmp_path):
    store, root = _store_and_root(tmp_path)
    pack_dir = _write_pack(tmp_path, _full_pack())
    first = activate(pack_dir, registry_store=store, role_root=root)
    second = activate(pack_dir, registry_store=store, role_root=root)
    assert first.already_active is False
    assert second.already_active is True
    assert first.receipt.pack_digest == second.receipt.pack_digest == pack_digest(_full_pack())


def test_registry_rejects_a_cross_namespace_receipt_claim(tmp_path):
    # A receipt may only own paths under its own plugin/<pack_id>/ namespace.
    # A claim on another pack's path is corruption and is rejected at save time,
    # so it can never be used to block or later delete that pack's role.
    from pydantic import ValidationError

    from fno.plugins.registry import ActivationReceipt

    store, _root = _store_and_root(tmp_path)
    impostor = ActivationReceipt(
        pack_id="impostor",
        pack_digest="f" * 64,
        resolved_version="9.9.9",
        written_paths=("plugin/growth-studio/marketing.json",),
        activated_at=datetime.now(UTC),
    )
    with pytest.raises((ValidationError, Exception)):
        store.record_activation(impostor)


def test_activation_refuses_an_unreceipted_existing_target(tmp_path):
    # A file already at a planned target with no receipt owning it is occupied.
    store, root = _store_and_root(tmp_path)
    pack_dir = _write_pack(tmp_path, _full_pack())
    (root / "plugin" / "growth-studio").mkdir(parents=True)
    (root / "plugin" / "growth-studio" / "marketing.json").write_text("{}", encoding="utf-8")
    with pytest.raises(ActivationRefusal) as info:
        activate(pack_dir, registry_store=store, role_root=root)
    assert info.value.reason is ActivationRefusalReason.PATH_OCCUPIED


# AC7-HP: deactivation removes only what the receipt recorded.


def test_deactivation_removes_only_receipted_paths_and_leaves_hand_written(tmp_path):
    store, root = _store_and_root(tmp_path)
    pack_dir = _write_pack(tmp_path, _full_pack())
    activate(pack_dir, registry_store=store, role_root=root)
    # A hand-written definition in the same plugin layer, not in any receipt.
    hand_written = root / "plugin" / "manual" / "curator.json"
    hand_written.parent.mkdir(parents=True, exist_ok=True)
    curator_role = RoleRef(id="curator", function_id="manual")
    curator_manifest = _full_pack().roles[0].model_copy(
        update={"role": curator_role, "function": FunctionRef(id="manual")}
    )
    hand_written.write_text(
        json.dumps(
            RoleDefinitionSource(
                layer=RoleLayer.PLUGIN,
                source_id="plugin/manual/curator.json",
                snapshot_revision="manual-1",
                role=curator_role,
                status="valid",
                manifest=curator_manifest,
            ).model_dump(mode="json")
        ),
        encoding="utf-8",
    )

    outcome = deactivate("growth-studio", registry_store=store, role_root=root)

    assert outcome.removed == ("plugin/growth-studio/marketing.json",)
    assert not (root / "plugin" / "growth-studio" / "marketing.json").exists()
    # hand-written definition survives and is still discovered
    assert hand_written.exists()
    discovered = discover_role_definitions(root=root)
    assert any(r.role is not None and r.role.id == "curator" for r in discovered)


def test_activation_refuses_path_traversal_pack_id(tmp_path):
    store, root = _store_and_root(tmp_path)
    pack = _full_pack().model_copy(update={"id": "../escape"})
    pack_dir = tmp_path / "p"
    pack_dir.mkdir()
    (pack_dir / "plugin.yaml").write_text(yaml.safe_dump(pack.model_dump(mode="json")), encoding="utf-8")
    with pytest.raises(ActivationRefusal) as info:
        activate(pack_dir, registry_store=store, role_root=root)
    assert info.value.reason is ActivationRefusalReason.INVALID_IDENTITY


def test_activation_refuses_on_corrupt_registry(tmp_path):
    store, root = _store_and_root(tmp_path)
    store.path.parent.mkdir(parents=True, exist_ok=True)
    store.path.write_text("{ not valid json", encoding="utf-8")
    pack_dir = _write_pack(tmp_path, _full_pack())
    with pytest.raises(ActivationRefusal) as info:
        activate(pack_dir, registry_store=store, role_root=root)
    assert info.value.reason is ActivationRefusalReason.REGISTRY_CORRUPT


def test_upgrade_removes_stale_role_files(tmp_path):
    store, root = _store_and_root(tmp_path)
    base_role = _full_pack().roles[0]
    second_role = base_role.model_copy(update={"role": RoleRef(id="second", function_id="growth-studio")})
    v1 = _full_pack().model_copy(update={"version": "0.1.0", "roles": (base_role, second_role)})
    v2 = _full_pack().model_copy(update={"version": "0.2.0", "roles": (base_role,)})
    activate(_write_pack(tmp_path, v1, name="gs-v1"), registry_store=store, role_root=root)
    assert (root / "plugin" / "growth-studio" / "second.json").exists()
    activate(_write_pack(tmp_path, v2, name="gs-v2"), registry_store=store, role_root=root)
    assert (root / "plugin" / "growth-studio" / "marketing.json").exists()
    assert not (root / "plugin" / "growth-studio" / "second.json").exists()


# AC19-ERR: activation names unconfigured required artifacts without refusing.


def _artifact_pack() -> PackManifest:
    role = RoleManifest(
        role=RoleRef(id="marketing", function_id="growth-studio"),
        function=FunctionRef(id="growth-studio"),
        mission="draft grounded in verified product truth",
        deliverable_kinds=("campaign-plan",),
        authority_ceiling=AuthorityCeiling.INTERNAL,
        context_selectors=(
            ContextSelector(
                kind=ContextKind.ARTIFACT,
                identifier="product-truth",
                max_sensitivity=Sensitivity.PUBLIC,
            ),
        ),
        review_policy=ReviewPolicy(required=True, minimum_reviewers=1),
        delivery_policy=DeliveryPolicy(required_evidence=("factual-review",)),
        default_topology="pipeline",
        approval_floor="founder",
    )
    return PackManifest(
        id="growth-studio",
        version="0.1.0",
        footnote_compat=CompatibilityRange(minimum="0.3.0"),
        roles=(role,),
        assets=(AssetDeclaration(id="product-truth", source="assets/product-truth.md"),),
        scenarios=(
            ScenarioDeclaration(id="smoke", command="true", recorded_result=EvidenceResult.PASSED),
        ),
    )


def test_unconfigured_required_artifacts_names_missing_with_example() -> None:
    missing = unconfigured_required_artifacts(_artifact_pack(), configured=set())
    assert len(missing) == 1
    assert missing[0].identifier == "product-truth"
    assert missing[0].example_source == "assets/product-truth.md"


def test_unconfigured_required_artifacts_empty_when_configured() -> None:
    missing = unconfigured_required_artifacts(_artifact_pack(), configured={"product-truth"})
    assert missing == ()


def test_activation_reports_unconfigured_artifacts_and_writes_only_role_layer(tmp_path):
    store, root = _store_and_root(tmp_path)
    pack_dir = _write_pack(tmp_path, _artifact_pack())
    # Nothing configured: activation still succeeds (exits zero = no refusal).
    outcome = activate(pack_dir, registry_store=store, role_root=root, configured_artifacts=set())
    assert [item.identifier for item in outcome.unconfigured_artifacts] == ["product-truth"]
    # Every written path is a role-layer definition under plugin/<pack>/.
    for written in outcome.receipt.written_paths:
        assert written.startswith("plugin/growth-studio/")
    plugin_root = root / "plugin" / "growth-studio"
    assert (plugin_root / "marketing.json").exists()
    # No stray file outside the role-layer namespace.
    assert {p.relative_to(root).as_posix() for p in plugin_root.glob("*.json")} == {
        "plugin/growth-studio/marketing.json"
    }


def test_activation_silent_on_required_artifacts_when_all_configured(tmp_path):
    store, root = _store_and_root(tmp_path)
    pack_dir = _write_pack(tmp_path, _artifact_pack())
    outcome = activate(
        pack_dir,
        registry_store=store,
        role_root=root,
        configured_artifacts={"product-truth"},
    )
    assert outcome.unconfigured_artifacts == ()
