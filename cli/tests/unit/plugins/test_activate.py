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
)
from fno.plugins.manifest import pack_digest
from fno.plugins.registry import PackRegistryStore
from fno.roles.models import ContextBundleBounds, RoleDefinitionSource, RoleLayer, RoleResolutionBlocked
from fno.roles.registry import discover_role_definitions
from fno.roles.resolver import resolve_role
from tests.unit.plugins.test_manifest import _full_pack


def _store_and_root(tmp_path):
    root = tmp_path / "roles"
    root.mkdir()
    store = PackRegistryStore(tmp_path / "registry.json")
    return store, root


def _write_pack(tmp_path, manifest, name="growth-studio") -> Path:
    pack_dir = tmp_path / name
    pack_dir.mkdir(exist_ok=True)
    (pack_dir / "plugin.yaml").write_text(yaml.safe_dump(manifest.model_dump(mode="json")), encoding="utf-8")
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


def test_activation_refuses_when_a_different_pack_owns_the_path(tmp_path):
    store, root = _store_and_root(tmp_path)
    pack_a = _write_pack(tmp_path, _full_pack(), name="growth-studio")
    # A foreign pack already owns the exact path growth-studio would write.
    from fno.plugins.registry import ActivationReceipt

    foreign_digest = "f" * 64
    store.record_activation(
        ActivationReceipt(
            pack_id="impostor",
            pack_digest=foreign_digest,
            resolved_version="9.9.9",
            written_paths=("plugin/growth-studio/marketing.json",),
            activated_at=datetime.now(UTC),
        )
    )
    with pytest.raises(ActivationRefusal) as info:
        activate(pack_a, registry_store=store, role_root=root)
    assert info.value.reason is ActivationRefusalReason.DIFFERENT_PACK_OWNS_PATH


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
