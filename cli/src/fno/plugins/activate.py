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

Pack and role ids become path segments, so they are validated as single path
components and every resolved target is confirmed to stay under the plugin root:
a manifest id of ``../x`` cannot escape the role layer. Ownership preflight,
atomic per-file writes (temp plus ``os.replace``), stale-path reconciliation on
upgrade, and the receipt recording run under one registry lock so two concurrent
activations serialize and a partial activation never leaves unreceipted files.
Activation refuses on any failed or blocked verification condition, refuses to
overwrite a path a different pack owns, is idempotent for the same digest, and
deactivates by removing only the paths its own receipt recorded after they are
confirmed gone.
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
    RegistryCorrupt,
    conformance_for,
)
from fno.plugins.verify import load_manifest, verify_manifest
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
    INVALID_IDENTITY = "invalid_identity"
    DIFFERENT_PACK_OWNS_PATH = "different_pack_owns_path"
    UNWRITABLE_LAYER = "unwritable_layer"
    REGISTRY_CORRUPT = "registry_corrupt"


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


def _validate_path_component(value: str, kind: str) -> None:
    """Reject an id that is not a single safe path component.

    Pack and role ids become path segments under ``<root>/plugin/<pack>/<role>.json``.
    A value with a separator, a traversal literal, or a NUL could escape the
    plugin root or cross into another pack's namespace, so it is refused before
    any filesystem touch.
    """
    if (
        not value
        or "/" in value
        or "\\" in value
        or value in (".", "..")
        or "\x00" in value
    ):
        raise ActivationRefusal(
            ActivationRefusalReason.INVALID_IDENTITY,
            f"invalid {kind} {value!r}: must be a single path component",
        )


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
    :class:`ActivationRefusal` on a failed/blocked verification condition, an
    unsafe id, a path a different pack owns, an unwritable layer, or a corrupt
    registry.
    """
    target = Path(path).expanduser()
    store = registry_store or PackRegistryStore()
    root = Path(role_root) if role_root is not None else default_role_root()

    manifest, load_failure = load_manifest(target)
    if load_failure is not None or manifest is None:
        detail = (load_failure.detail if load_failure else None) or "manifest could not be loaded"
        raise ActivationRefusal(ActivationRefusalReason.VERIFICATION_FAILED, detail)
    digest = pack_digest(manifest)

    _validate_path_component(manifest.id, "pack id")
    for role_manifest in manifest.roles:
        _validate_path_component(role_manifest.role.id, "role id")

    # Verify the parsed manifest directly: no second read of the file between
    # verification and the projection, so the bytes verified are the bytes that
    # define the roles. UNKNOWN conditions are allowed (honest non-evaluation,
    # e.g. a pack with no benchmark scenarios); the plan gates activation on
    # *failed* conditions.
    base = target.parent if target.is_file() else target
    try:
        report = verify_manifest(manifest, installed=store.installed_index(), base=base)
    except RegistryCorrupt as exc:
        raise ActivationRefusal(ActivationRefusalReason.REGISTRY_CORRUPT, str(exc)) from exc
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
    plugin_root = (root / "plugin").resolve()
    # Resolve every target and prove it stays under the plugin root before any
    # write: a containment failure refuses up front rather than mid-activation.
    planned: list[tuple[str, Path, RoleDefinitionSource]] = []
    for index in range(len(manifest.roles)):
        source_id, definition = _definition_document(manifest, index, revision=revision)
        resolved = (root / source_id).resolve()
        try:
            resolved.relative_to(plugin_root)
        except ValueError as exc:
            raise ActivationRefusal(
                ActivationRefusalReason.INVALID_IDENTITY,
                f"{source_id} resolves outside the plugin role root",
            ) from exc
        planned.append((source_id, resolved, definition))

    try:
        with store.lock:
            registry = store.load()  # raises RegistryCorrupt
            for source_id, _resolved, _definition in planned:
                owner = registry.owner_of_path(source_id)
                if owner is not None and owner[0] != manifest.id:
                    raise ActivationRefusal(
                        ActivationRefusalReason.DIFFERENT_PACK_OWNS_PATH,
                        f"{source_id} is owned by pack {owner[0]}",
                    )
            existing = registry.receipt_for(manifest.id)
            already_active = existing is not None and existing.pack_digest == digest

            written: list[str] = []
            try:
                for source_id, resolved, definition in planned:
                    _atomic_write_json(resolved, definition.model_dump_json())
                    written.append(source_id)
            except OSError as exc:
                # Roll back every file this activation wrote so a failed write
                # never leaves discoverable roles without a receipt.
                for rollback_id in written:
                    (root / rollback_id).unlink(missing_ok=True)
                raise ActivationRefusal(
                    ActivationRefusalReason.UNWRITABLE_LAYER,
                    f"cannot write a definition file: {exc}",
                ) from exc

            # An upgrade whose new manifest drops roles must not leave the old
            # roles' files orphaned and undeactivatable: remove prior receipt
            # paths that the new set no longer carries.
            new_set = set(written)
            for prior in (existing.written_paths if existing is not None else ()):
                if prior in new_set:
                    continue
                prior_path = root / prior
                if prior_path.is_file():
                    prior_path.unlink()

            receipt = ActivationReceipt(
                pack_id=manifest.id,
                pack_digest=digest,
                resolved_version=manifest.version,
                written_paths=tuple(written),
                activated_at=datetime.now(UTC),
                conformance=conformance_for(manifest),
            )
            registry, _pack = store._install_locked(registry, manifest, target)
            registry = store._record_activation_locked(registry, receipt)
            store._save(registry)
            return ActivationOutcome(receipt=receipt, already_active=already_active)
    except RegistryCorrupt as exc:
        raise ActivationRefusal(ActivationRefusalReason.REGISTRY_CORRUPT, str(exc)) from exc


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

    The whole operation runs under one registry lock: ownership is read, files
    are unlinked, and the receipt is removed with no window for a concurrent
    activation to invalidate the snapshot. Each receipted path is removed only if
    it still resolves under the role root and its current owner is still this
    pack; a hand-written definition in the same plugin layer is never touched.
    """
    store = registry_store or PackRegistryStore()
    root = Path(role_root) if role_root is not None else default_role_root()
    root_resolved = root.resolve()

    def contained(source_id: str) -> bool:
        try:
            (root / source_id).resolve().relative_to(root_resolved)
            return True
        except (ValueError, RuntimeError):
            return False

    try:
        with store.lock:
            registry = store.load()
            receipt = registry.receipt_for(pack_id)
            if receipt is None:
                return DeactivationOutcome(removed=(), left_alone=())
            removed: list[str] = []
            left_alone: list[str] = []
            for source_id in receipt.written_paths:
                if not contained(source_id):
                    left_alone.append(source_id)
                    continue
                owner = registry.owner_of_path(source_id)
                if owner is not None and owner[0] != pack_id:
                    left_alone.append(source_id)
                    continue
                path = root / source_id
                if path.is_file():
                    path.unlink()
                removed.append(source_id)
            receipts = tuple(r for r in registry.receipts if r.pack_id != pack_id)
            store._save(registry.model_copy(update={"receipts": receipts}))
            return DeactivationOutcome(removed=tuple(removed), left_alone=tuple(left_alone))
    except RegistryCorrupt:
        return DeactivationOutcome(removed=(), left_alone=())
