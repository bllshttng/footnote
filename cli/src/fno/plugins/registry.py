"""Installed-pack records, activation receipts, and conformance attribution.

The registry persists two things: which packs are installed (id, version,
digest, declared effect ceiling) and which are activated (a receipt naming the
exact role-layer paths the activation wrote). The receipt's written-path list is
what makes deactivation surgical: removing only the paths this receipt recorded
leaves a hand-written definition in the same layer untouched (AC7-HP).

It also records which pack digest declared which adapter conformance flags.
``approvals-and-effects.md`` states the approval store cannot verify adapter
conformance, so attribution makes a false declaration traceable after the fact;
this node owns the adapter registry, so the attribution lives here.

Concurrent mutations take a file lock so two packs racing for one definition
path serialize and the second refuses on digest ownership rather than
overwriting. The registry file is a dotfile at the role root, never inside the
``plugin`` layer directory the role registry walks.
"""

from __future__ import annotations

import json
import os
import tempfile
from datetime import datetime
from pathlib import Path
from typing import Self

from filelock import FileLock
from pydantic import BaseModel, ConfigDict, ValidationError, model_validator

from fno.company.contracts import NonEmptyStr
from fno.plugins.manifest import EffectDeclaration, PackManifest, pack_digest
from fno.roles.models import Sha256Digest
from fno.roles.registry import default_role_root

__all__ = [
    "ActivationReceipt",
    "ConformanceAttribution",
    "InstalledPack",
    "PackRegistry",
    "PackRegistryStore",
    "RegistryCorrupt",
    "conformance_for",
    "default_registry_path",
]


class RegistryCorrupt(Exception):
    """The registry file is present but unparseable; ownership cannot be trusted."""


def default_registry_path() -> Path:
    """The registry dotfile lives at the role root, outside the walked layers."""
    return default_role_root() / ".pack-registry.json"


class _RegistryModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class ConformanceAttribution(_RegistryModel):
    """One adapter conformance declaration attributed to one pack digest.

    Mirrors :class:`fno.plugins.manifest.AdapterConformance` plus the digest of
    the pack that declared it, so a conformance that later proves false is
    traceable to its source even though it could not be verified up front.
    """

    adapter_id: NonEmptyStr
    adapter_version: NonEmptyStr
    pack_digest: Sha256Digest
    remote_idempotency: bool = False
    reconciliation: bool = False


class ActivationReceipt(_RegistryModel):
    """Record of one activation: what was written, by which digest, when."""

    pack_id: NonEmptyStr
    pack_digest: Sha256Digest
    resolved_version: NonEmptyStr
    written_paths: tuple[str, ...]
    activated_at: datetime
    conformance: tuple[ConformanceAttribution, ...] = ()

    @model_validator(mode="after")
    def _clock_is_aware(self) -> Self:
        if self.activated_at.tzinfo is None or self.activated_at.utcoffset() is None:
            raise ValueError("activated_at must be timezone-aware")
        return self


class InstalledPack(_RegistryModel):
    """A pack recorded as installed (present on disk), independent of activation."""

    pack_id: NonEmptyStr
    resolved_version: NonEmptyStr
    pack_digest: Sha256Digest
    manifest_path: NonEmptyStr
    declared_effects: tuple[EffectDeclaration, ...] = ()


class PackRegistry(_RegistryModel):
    packs: tuple[InstalledPack, ...] = ()
    receipts: tuple[ActivationReceipt, ...] = ()

    def pack_by_id(self, pack_id: str) -> InstalledPack | None:
        return next((pack for pack in self.packs if pack.pack_id == pack_id), None)

    def receipt_for(self, pack_id: str) -> ActivationReceipt | None:
        return next((receipt for receipt in self.receipts if receipt.pack_id == pack_id), None)

    def owner_digest_of_path(self, written_path: str) -> str | None:
        """The pack digest whose receipt recorded this path, or None."""
        for receipt in self.receipts:
            if written_path in receipt.written_paths:
                return receipt.pack_digest
        return None

    def owner_of_path(self, written_path: str) -> tuple[str, str] | None:
        """The (pack_id, pack_digest) whose receipt recorded this path, or None."""
        for receipt in self.receipts:
            if written_path in receipt.written_paths:
                return (receipt.pack_id, receipt.pack_digest)
        return None

    @model_validator(mode="after")
    def _ownership_is_unambiguous(self) -> Self:
        # A registry with duplicate pack ids, duplicate receipts, or one path
        # owned by two receipts makes first-match ownership queries ambiguous, so
        # it fails validation and reads as corrupt rather than as a valid state.
        pack_ids = [pack.pack_id for pack in self.packs]
        if len(set(pack_ids)) != len(pack_ids):
            raise ValueError(f"duplicate installed pack id {pack_ids[0]!r}")
        receipt_ids = [receipt.pack_id for receipt in self.receipts]
        if len(set(receipt_ids)) != len(receipt_ids):
            raise ValueError(f"duplicate activation receipt for {receipt_ids[0]!r}")
        seen: dict[str, str] = {}
        for receipt in self.receipts:
            expected_prefix = f"plugin/{receipt.pack_id}/"
            for path in receipt.written_paths:
                # Reject non-normalized paths so an aliased entry like
                # plugin/a/../b/role.json cannot bypass the string uniqueness
                # check and shadow another pack's file.
                segments = path.split("/")
                if path.startswith("/") or "." in segments or ".." in segments or "" in segments:
                    raise ValueError(f"non-normalized receipt path {path!r}")
                # Bind each receipt path to its own pack namespace and a single
                # role .json, so a corrupt receipt cannot claim (and later
                # deactivate) another pack's role file.
                if not path.startswith(expected_prefix) or not path.endswith(".json"):
                    raise ValueError(
                        f"receipt path {path!r} is not under {expected_prefix} or not a .json"
                    )
                role_segment = path[len(expected_prefix):-len(".json")]
                if not role_segment or "/" in role_segment:
                    raise ValueError(f"receipt path {path!r} has an unsafe role segment")
                if path in seen:
                    raise ValueError(
                        f"path {path!r} owned by receipts {seen[path]!r} and {receipt.pack_id!r}"
                    )
                seen[path] = receipt.pack_id
        return self


class PackRegistryStore:
    """Lock-guarded, atomically-written persistence for the pack registry."""

    def __init__(self, path: Path | None = None) -> None:
        self.path = Path(path) if path is not None else default_registry_path()
        self.lock = FileLock(str(self.path) + ".lock")

    def load(self) -> PackRegistry:
        """Load the registry, raising :class:`RegistryCorrupt` if it is present but unparseable.

        A missing file is a fresh install and returns an empty registry. A file
        that exists but is corrupt must NOT read as empty: doing so would discard
        every ownership and receipt record, letting activation overwrite existing
        definitions. Fail closed so the corruption is surfaced and fixed.
        """
        try:
            text = self.path.read_text(encoding="utf-8")
        except FileNotFoundError:
            return PackRegistry()
        except (UnicodeDecodeError, OSError) as exc:
            raise RegistryCorrupt(f"registry at {self.path} is unreadable: {exc}") from exc
        try:
            raw = json.loads(text)
        except json.JSONDecodeError as exc:
            raise RegistryCorrupt(f"registry at {self.path} is not valid JSON: {exc}") from exc
        try:
            return PackRegistry.model_validate(raw)
        except ValidationError as exc:
            raise RegistryCorrupt(f"registry at {self.path} failed schema validation: {exc}") from exc

    def load_or_empty(self) -> PackRegistry:
        """Read-only display load: an empty registry on corruption, never a raise."""
        try:
            return self.load()
        except RegistryCorrupt:
            return PackRegistry()

    def installed_index(self) -> dict[str, str]:
        """pack_id -> resolved_version, for the verifier's dependency family.

        Fails closed on a corrupt registry so a dependency-free pack cannot verify
        green against an index that could not be trusted.
        """
        return {pack.pack_id: pack.resolved_version for pack in self.load().packs}

    def owner_digest_of_path(self, written_path: str) -> str | None:
        return self.load().owner_digest_of_path(written_path)

    def owner_of_path(self, written_path: str) -> tuple[str, str] | None:
        return self.load().owner_of_path(written_path)

    def peek_receipt(self, pack_id: str) -> ActivationReceipt | None:
        """Read a receipt without removing it (for ordered deactivation)."""
        return self.load().receipt_for(pack_id)

    def install(self, manifest: PackManifest, manifest_path: Path) -> InstalledPack:
        """Record (or refresh) a pack as installed and return its record."""
        pack = InstalledPack(
            pack_id=manifest.id,
            resolved_version=manifest.version,
            pack_digest=pack_digest(manifest),
            manifest_path=str(manifest_path),
            declared_effects=manifest.permissions,
        )
        with self.lock:
            registry = self.load()
            packs = tuple(p for p in registry.packs if p.pack_id != pack.pack_id)
            self._save(registry.model_copy(update={"packs": (*packs, pack)}))
        return pack

    def record_activation(self, receipt: ActivationReceipt) -> None:
        with self.lock:
            self._save(self._record_activation_locked(self.load(), receipt))

    def remove_activation(self, pack_id: str) -> ActivationReceipt | None:
        with self.lock:
            registry = self.load()
            receipt = registry.receipt_for(pack_id)
            if receipt is None:
                return None
            receipts = tuple(r for r in registry.receipts if r.pack_id != pack_id)
            self._save(registry.model_copy(update={"receipts": receipts}))
            return receipt

    # -- lock-held internals (caller already holds self.lock) --------------

    @staticmethod
    def _install_locked(registry: PackRegistry, manifest: PackManifest, manifest_path: Path) -> tuple[PackRegistry, InstalledPack]:
        pack = InstalledPack(
            pack_id=manifest.id,
            resolved_version=manifest.version,
            pack_digest=pack_digest(manifest),
            manifest_path=str(manifest_path),
            declared_effects=manifest.permissions,
        )
        packs = tuple(p for p in registry.packs if p.pack_id != pack.pack_id)
        return registry.model_copy(update={"packs": (*packs, pack)}), pack

    @staticmethod
    def _record_activation_locked(registry: PackRegistry, receipt: ActivationReceipt) -> PackRegistry:
        receipts = tuple(r for r in registry.receipts if r.pack_id != receipt.pack_id)
        return registry.model_copy(update={"receipts": (*receipts, receipt)})

    def _save(self, registry: PackRegistry) -> None:
        # Re-validate before persisting: the locked mutators build the next state
        # with model_copy, which bypasses validators, so reconstruct to run the
        # uniqueness and path-normalization checks before the bytes land.
        validated = PackRegistry.model_validate(registry.model_dump(mode="json"))
        payload = json.dumps(validated.model_dump(mode="json"), sort_keys=True, indent=2)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        descriptor, tmp_name = tempfile.mkstemp(
            dir=str(self.path.parent), prefix=".pack-registry-", suffix=".tmp"
        )
        try:
            with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
                handle.write(payload)
            os.replace(tmp_name, self.path)
        except BaseException:
            Path(tmp_name).unlink(missing_ok=True)
            raise


def conformance_for(manifest: PackManifest) -> tuple[ConformanceAttribution, ...]:
    """Project a pack's adapter conformance declarations into attributable records."""
    digest = pack_digest(manifest)
    return tuple(
        ConformanceAttribution(
            adapter_id=adapter.conformance.adapter_id,
            adapter_version=adapter.conformance.adapter_version,
            pack_digest=digest,
            remote_idempotency=adapter.conformance.remote_idempotency,
            reconciliation=adapter.conformance.reconciliation,
        )
        for adapter in manifest.adapters
    )
