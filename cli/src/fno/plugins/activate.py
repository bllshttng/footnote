"""Activation projects packaged roles into the plugin role layer, granting nothing.

Each packaged role is written as a full :class:`fno.roles.models.RoleDefinitionSource`
JSON document into ``<role root>/plugin/<pack id>/<role>.json``. That is the layer
``discover_role_definitions`` already walks, so a packaged role becomes resolvable
through the untouched ``resolve_role`` without a new discovery path, a new
resolver, or any change to ``registry.py`` or ``resolver.py``.

Activation grants nothing. It never writes ``config.approvals.authorized_principals``,
never mints a ``CapabilityFact``, and never touches the effect journal: the
projection is role definitions only. A pack that declares an external-publication
effect still cannot dispatch one, because the approval authority is unchanged and
capabilities arrive as independent work-order-scoped facts.

Writes are atomic (temp file plus ``os.replace``) so a concurrent discovery never
observes a partial JSON file. Activation refuses on any failed or blocked
verification condition, refuses to overwrite a path a different pack owns, is
idempotent for the same digest, and deactivates by removing only the paths its
own receipt recorded.
"""

from __future__ import annotations

import os
import tempfile
from datetime import UTC, datetime
from enum import Enum
from pathlib import Path

from fno.company.contracts import EvidenceResult
from fno.plugins.manifest import PackManifest, pack_digest
from fno.plugins.registry import (
    ActivationReceipt,
    PackRegistryStore,
    conformance_for,
)
from fno.plugins.verify import load_manifest, verify_pack
from fno.roles.models import DefinitionStatus, RoleDefinitionSource, RoleLayer
from fno.roles.registry import default_role_root

__all__ = [
    "ActivationOutcome",
    "ActivationRefusal",
    "ActivationRefusalReason",
    "DeactivationOutcome",
    "activate",
    "deactivate",
]


class ActivationRefusalReason(str, Enum):
    VERIFICATION_FAILED = "verification_failed"
    DIFFERENT_PACK_OWNS_PATH = "different_pack_owns_path"
    UNWRITABLE_LAYER = "unwritable_layer"


class ActivationRefusal(Exception):
    def __init__(self, reason: ActivationRefusalReason, detail: str) -> None:
        super().__init__(f"{reason.value}: {detail}")
        self.reason = reason
        self.detail = detail


def _revision_for(manifest: PackManifest) -> str:
    # A pack-content-addressed revision: the same pack always yields the same
    # revision, and a version bump (which changes the digest) changes it too.
    return f"pack:{pack_digest(manifest)[:16]}"


def _target_source_id(manifest: PackManifest, role_id: str) -> str:
    return f"plugin/{manifest.id}/{role_id}.json"


def _atomic_write_json(path: Path, payload: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, tmp_name = tempfile.mkstemp(dir=str(path.parent), prefix=f".{path.stem}-", suffix=".tmp")
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            handle.write(payload)
        os.replace(tmp_name, path)
    except BaseException:
        Path(tmp_name).unlink(missing_ok=True)
        raise


def _definition_document(
    manifest: PackManifest,
    role_manifest_index: int,
    *,
    revision: str,
) -> tuple[str, RoleDefinitionSource]:
    role_manifest = manifest.roles[role_manifest_index]
    source_id = _target_source_id(manifest, role_manifest.role.id)
    definition = RoleDefinitionSource(
        layer=RoleLayer.PLUGIN,
        source_id=source_id,
        snapshot_revision=revision,
        role=role_manifest.role,
        manifest=role_manifest,
        status=DefinitionStatus.VALID,
    )
    return source_id, definition


class ActivationOutcome:
    """The result of an activation: a receipt, or already-active for the same digest."""

    def __init__(self, receipt: ActivationReceipt, already_active: bool) -> None:
        self.receipt = receipt
        self.already_active = already_active


def activate(
    path: Path | str,
    *,
    registry_store: PackRegistryStore | None = None,
    role_root: Path | None = None,
) -> ActivationOutcome:
    """Verify then project a pack's roles into the plugin role layer.

    Grants nothing: writes role definitions only. Raises
    :class:`ActivationRefusal` on a failed/blocked verification condition or when
    a different pack owns a target path.
    """
    target = Path(path).expanduser()
    store = registry_store or PackRegistryStore()
    root = Path(role_root) if role_root is not None else default_role_root()

    manifest, load_failure = load_manifest(target)
    if load_failure is not None or manifest is None:
        detail = (load_failure.detail if load_failure else None) or "manifest could not be loaded"
        raise ActivationRefusal(ActivationRefusalReason.VERIFICATION_FAILED, detail)
    digest = pack_digest(manifest)

    existing = store.load().receipt_for(manifest.id)
    if existing is not None and existing.pack_digest == digest:
        return ActivationOutcome(receipt=existing, already_active=True)

    report = verify_pack(target, installed=store.installed_index())
    failing = [
        condition
        for condition in report.conditions
        if condition.result in (EvidenceResult.FAILED, EvidenceResult.BLOCKED)
    ]
    if failing:
        raise ActivationRefusal(
            ActivationRefusalReason.VERIFICATION_FAILED,
            "; ".join(f"{c.name}={c.result.value}" for c in failing),
        )

    revision = _revision_for(manifest)
    written: list[str] = []
    for index in range(len(manifest.roles)):
        source_id, definition = _definition_document(manifest, index, revision=revision)
        owner = store.owner_of_path(source_id)
        if owner is not None and owner[0] != manifest.id:
            raise ActivationRefusal(
                ActivationRefusalReason.DIFFERENT_PACK_OWNS_PATH,
                f"{source_id} is owned by pack {owner[0]}",
            )
        target_path = root / source_id
        try:
            _atomic_write_json(target_path, definition.model_dump_json())
        except OSError as exc:
            raise ActivationRefusal(
                ActivationRefusalReason.UNWRITABLE_LAYER,
                f"cannot write {target_path}: {exc}",
            ) from exc
        written.append(source_id)

    receipt = ActivationReceipt(
        pack_id=manifest.id,
        pack_digest=digest,
        resolved_version=manifest.version,
        written_paths=tuple(written),
        activated_at=datetime.now(UTC),
        conformance=conformance_for(manifest),
    )
    store.install(manifest, target)
    store.record_activation(receipt)
    return ActivationOutcome(receipt=receipt, already_active=False)


class DeactivationOutcome:
    def __init__(self, removed: tuple[str, ...], left_alone: tuple[str, ...]) -> None:
        self.removed = removed
        self.left_alone = left_alone


def deactivate(
    pack_id: str,
    *,
    registry_store: PackRegistryStore | None = None,
    role_root: Path | None = None,
) -> DeactivationOutcome:
    """Remove only the definition paths this pack's receipt recorded.

    A hand-written definition in the same plugin layer is never touched.
    """
    store = registry_store or PackRegistryStore()
    root = Path(role_root) if role_root is not None else default_role_root()
    receipt = store.remove_activation(pack_id)
    if receipt is None:
        return DeactivationOutcome(removed=(), left_alone=())
    removed: list[str] = []
    for source_id in receipt.written_paths:
        path = root / source_id
        if path.is_file():
            path.unlink()
            removed.append(source_id)
    # Report any receipted path that survives (e.g. a different pack reclaimed it
    # mid-flight) so the operator sees deactivation was not total there.
    left_alone = tuple(path for path in receipt.written_paths if path not in removed)
    return DeactivationOutcome(removed=tuple(removed), left_alone=left_alone)
