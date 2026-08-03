"""Ordered discovery records for role definitions."""

from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass
import json
import os
from pathlib import Path
from typing import Any, Sequence

from pydantic import Field, ValidationError

from fno.company.contracts import RoleRef
from fno.roles.models import (
    DefinitionStatus,
    RoleDefinitionSource,
    RoleLayer,
    _RoleModel,
)

_LAYER_ORDER = {layer: index for index, layer in enumerate(RoleLayer)}


class RegistryError(ValueError):
    """The registry cannot choose between equal-precedence definitions."""


class DiscoveryInputError(ValueError):
    """An explicit discovery source has malformed syntax."""


@dataclass(frozen=True)
class DiscoveredRoleDefinition:
    """One source-attributed discovery result, including invalid documents."""

    layer: RoleLayer
    source_id: str
    status: DefinitionStatus
    role: RoleRef | None
    raw_definition: Any
    definition: RoleDefinitionSource | None
    error: str | None = None


class RoleRegistry(_RoleModel):
    records: tuple[RoleDefinitionSource, ...] = Field(default_factory=tuple)

    def definitions_for(self, role: RoleRef) -> tuple[RoleDefinitionSource, ...]:
        matches = tuple(record for record in self.records if record.role == role)
        by_layer: dict[RoleLayer, list[RoleDefinitionSource]] = defaultdict(list)
        for record in matches:
            by_layer[record.layer].append(record)
        for layer in RoleLayer:
            records = by_layer[layer]
            if len(records) > 1:
                source_ids = ", ".join(sorted(record.source_id for record in records))
                raise RegistryError(
                    f"ambiguous {layer.value} definitions for "
                    f"{role.function_id}/{role.id}: {source_ids}"
                )
        return ordered_definitions(matches)


def ordered_definitions(
    definitions: Sequence[RoleDefinitionSource],
) -> tuple[RoleDefinitionSource, ...]:
    """Return the fixed precedence order independent of discovery order."""
    return tuple(
        sorted(
            definitions,
            key=lambda record: (_LAYER_ORDER[record.layer], record.source_id),
        )
    )


def _source_id(path: Path, root: Path) -> str:
    try:
        return path.relative_to(root).as_posix()
    except ValueError:
        return str(path)


def _invalid_definition(
    *,
    layer: RoleLayer,
    source_id: str,
    raw: object,
    error: str,
) -> tuple[RoleRef | None, RoleDefinitionSource | None]:
    if not isinstance(raw, dict):
        return None, None
    try:
        role = RoleRef.model_validate(raw.get("role"))
    except ValidationError:
        return None, None
    revision = raw.get("snapshot_revision")
    if not isinstance(revision, str) or not revision.strip():
        return role, None
    return role, RoleDefinitionSource(
        layer=layer,
        source_id=source_id,
        snapshot_revision=revision,
        role=role,
        status=DefinitionStatus.INVALID,
        error=error,
    )


def _read_definition(path: Path, *, layer: RoleLayer, root: Path) -> DiscoveredRoleDefinition:
    source_id = _source_id(path, root)
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        return DiscoveredRoleDefinition(
            layer=layer,
            source_id=source_id,
            status=DefinitionStatus.UNREADABLE,
            role=None,
            raw_definition=None,
            definition=None,
            error=f"{type(exc).__name__}: {exc}",
        )
    except UnicodeDecodeError as exc:
        return DiscoveredRoleDefinition(
            layer=layer,
            source_id=source_id,
            status=DefinitionStatus.INVALID,
            role=None,
            raw_definition=None,
            definition=None,
            error=f"{type(exc).__name__}: {exc}",
        )
    try:
        raw: object = json.loads(text)
    except json.JSONDecodeError as exc:
        return DiscoveredRoleDefinition(
            layer=layer,
            source_id=source_id,
            status=DefinitionStatus.INVALID,
            role=None,
            raw_definition=None,
            definition=None,
            error=f"{type(exc).__name__}: {exc}",
        )
    try:
        definition = RoleDefinitionSource.model_validate(raw)
        if definition.layer is not layer:
            raise ValueError(
                f"document layer {definition.layer.value!r} does not match "
                f"discovery layer {layer.value!r}"
            )
        if definition.source_id != source_id:
            raise ValueError(
                f"document source_id {definition.source_id!r} does not match "
                f"source truth {source_id!r}"
            )
    except (ValidationError, ValueError) as exc:
        error = str(exc)
        role, invalid = _invalid_definition(
            layer=layer,
            source_id=source_id,
            raw=raw,
            error=error,
        )
        return DiscoveredRoleDefinition(
            layer=layer,
            source_id=source_id,
            status=DefinitionStatus.INVALID,
            role=role,
            raw_definition=raw,
            definition=invalid,
            error=error,
        )
    return DiscoveredRoleDefinition(
        layer=layer,
        source_id=source_id,
        status=definition.status,
        role=definition.role,
        raw_definition=raw,
        definition=definition,
        error=definition.error,
    )


def _explicit_paths(specs: Sequence[str]) -> tuple[tuple[RoleLayer, Path], ...]:
    parsed: list[tuple[RoleLayer, Path]] = []
    for spec in specs:
        layer_text, separator, path_text = spec.partition("=")
        if not separator or not path_text:
            raise DiscoveryInputError("--source must use layer=path")
        try:
            layer = RoleLayer(layer_text)
        except ValueError as exc:
            choices = ", ".join(item.value for item in RoleLayer)
            raise DiscoveryInputError(
                f"unknown role layer {layer_text!r}; expected one of {choices}"
            ) from exc
        path = Path(path_text).expanduser()
        if path.is_dir():
            parsed.extend((layer, item) for item in sorted(path.rglob("*.json")))
        else:
            parsed.append((layer, path))
    return tuple(parsed)


def discover_role_definitions(
    *,
    root: Path | None = None,
    source_specs: Sequence[str] = (),
) -> tuple[DiscoveredRoleDefinition, ...]:
    """Read sorted JSON definitions from the conventional layered role root."""
    resolved_root = (
        root
        if root is not None
        else Path(os.environ.get("FNO_ROLES_ROOT", Path.cwd() / ".fno" / "roles"))
    ).expanduser()
    candidates: list[tuple[RoleLayer, Path]] = []
    for layer in RoleLayer:
        layer_root = resolved_root / layer.value
        if layer_root.is_dir():
            candidates.extend((layer, path) for path in sorted(layer_root.rglob("*.json")))
    candidates.extend(_explicit_paths(source_specs))
    unique = {
        (layer, path.resolve()): (layer, path)
        for layer, path in candidates
    }
    records = (
        _read_definition(path, layer=layer, root=resolved_root)
        for layer, path in unique.values()
    )
    return tuple(
        sorted(
            records,
            key=lambda item: (_LAYER_ORDER[item.layer], item.source_id),
        )
    )
