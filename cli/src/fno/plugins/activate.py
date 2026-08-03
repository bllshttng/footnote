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
components and every resolved target (and every prior receipt path removed on
upgrade) is confirmed to stay under the plugin root. Verification runs against
the locked registry snapshot, ownership is preflighted, definitions are staged to
temp files and only swapped into place once every stage succeeds (so a write
failure preserves the prior roles), and the install-plus-receipt commit is one
locked transaction. Deactivation runs under the same lock, removes only paths the
receipt recorded under the root, and retains the receipt if any path could not be
removed.
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
    PATH_OCCUPIED = "path_occupied"
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
    """Reject an id that is not a single safe path component."""
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


def _contained_under(source_id: str, root: Path, anchor: Path) -> bool:
    """True when ``root / source_id`` resolves under ``anchor``."""
    try:
        (root / source_id).resolve().relative_to(anchor.resolve())
        return True
    except (ValueError, RuntimeError):
        return False


def _stage_and_swap(planned: list[tuple[str, Path, RoleDefinitionSource]]) -> None:
    """Stage every definition to a temp file, then swap all into place.

    A staging failure cleans up only the temp files; prior role definitions are
    untouched until every stage succeeds and the swaps are applied.
    """
    staged: list[tuple[Path, Path]] = []
    try:
        for _source_id, target, definition in planned:
            target.parent.mkdir(parents=True, exist_ok=True)
            descriptor, tmp_name = tempfile.mkstemp(
                dir=str(target.parent), prefix=f".{target.stem}-stage-", suffix=".tmp"
            )
            with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
                handle.write(definition.model_dump_json())
            staged.append((target, Path(tmp_name)))
    except OSError as exc:
        for _target, tmp in staged:
            tmp.unlink(missing_ok=True)
        raise ActivationRefusal(
            ActivationRefusalReason.UNWRITABLE_LAYER,
            f"cannot stage a definition file: {exc}",
        ) from exc
    for target, tmp in staged:
        os.replace(tmp, target)


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

    Grants nothing: writes role definitions only. Raises :class:`ActivationRefusal`
    on a failed/blocked verification condition, an unsafe id, an occupied or
    differently-owned path, an unwritable layer, or a corrupt registry.
    """
    target = Path(path).expanduser()
    store = registry_store or PackRegistryStore()
    root = Path(role_root) if role_root is not None else default_role_root()
    base = target.parent if target.is_file() else target

    manifest, load_failure = load_manifest(target)
    if load_failure is not None or manifest is None:
        detail = (load_failure.detail if load_failure else None) or "manifest could not be loaded"
        raise ActivationRefusal(ActivationRefusalReason.VERIFICATION_FAILED, detail)
    digest = pack_digest(manifest)

    _validate_path_component(manifest.id, "pack id")
    for role_manifest in manifest.roles:
        _validate_path_component(role_manifest.role.id, "role id")

    revision = _revision_for(manifest)
    plugin_root = root / "plugin"
    # Resolve every target and prove it stays under the plugin root before any
    # write: a containment failure refuses up front rather than mid-activation.
    planned: list[tuple[str, Path, RoleDefinitionSource]] = []
    for index in range(len(manifest.roles)):
        source_id, definition = _definition_document(manifest, index, revision=revision)
        if not _contained_under(source_id, root, plugin_root):
            raise ActivationRefusal(
                ActivationRefusalReason.INVALID_IDENTITY,
                f"{source_id} resolves outside the plugin role root",
            )
        planned.append((source_id, (root / source_id), definition))

    try:
        with store.lock:
            registry = store.load()  # raises RegistryCorrupt
            existing_receipt = registry.receipt_for(manifest.id)
            already_active = (
                existing_receipt is not None and existing_receipt.pack_digest == digest
            )
            # Verify against the locked snapshot so a concurrent pack update
            # cannot invalidate the dependency versions between verify and commit.
            installed = {p.pack_id: p.resolved_version for p in registry.packs}
            report = verify_manifest(manifest, installed=installed, base=base)
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

            existing = registry.receipt_for(manifest.id)
            for source_id, resolved, _definition in planned:
                owner = registry.owner_of_path(source_id)
                if owner is not None and owner[0] != manifest.id:
                    raise ActivationRefusal(
                        ActivationRefusalReason.DIFFERENT_PACK_OWNS_PATH,
                        f"{source_id} is owned by pack {owner[0]}",
                    )
                if owner is None and resolved.exists():
                    raise ActivationRefusal(
                        ActivationRefusalReason.PATH_OCCUPIED,
                        f"{source_id} exists but no receipt owns it",
                    )

            _stage_and_swap(planned)
            written = [source_id for source_id, _resolved, _definition in planned]

            # An upgrade whose new manifest drops roles must not leave the old
            # roles' files orphaned: remove prior receipt paths the new set no
            # longer carries, each confirmed to stay under the plugin root.
            new_set = set(written)
            for prior in (existing.written_paths if existing is not None else ()):
                if prior in new_set:
                    continue
                if not _contained_under(prior, root, plugin_root):
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

    Runs under one registry lock. Each receipted path is removed only if it
    resolves under the plugin root and its current owner is still this pack; the
    receipt is dropped only when every path was removed, otherwise a residual
    receipt is kept for the paths that remain so they keep an owner. Raises
    :class:`RegistryCorrupt` if the registry cannot be trusted.
    """
    store = registry_store or PackRegistryStore()
    root = Path(role_root) if role_root is not None else default_role_root()
    plugin_root = root / "plugin"

    with store.lock:
        registry = store.load()  # raises RegistryCorrupt
        receipt = registry.receipt_for(pack_id)
        if receipt is None:
            return DeactivationOutcome(removed=(), left_alone=())
        removed: list[str] = []
        left_alone: list[str] = []
        for source_id in receipt.written_paths:
            if not _contained_under(source_id, root, plugin_root):
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
        if left_alone:
            residual = receipt.model_copy(update={"written_paths": tuple(left_alone)})
            receipts = tuple(r if r.pack_id != pack_id else residual for r in registry.receipts)
        else:
            receipts = tuple(r for r in registry.receipts if r.pack_id != pack_id)
        store._save(registry.model_copy(update={"receipts": receipts}))
        return DeactivationOutcome(removed=tuple(removed), left_alone=tuple(left_alone))
