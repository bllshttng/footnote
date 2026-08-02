"""Ordered discovery records for role definitions."""

from __future__ import annotations

from collections import defaultdict
from typing import Sequence

from pydantic import Field

from fno.company.contracts import RoleRef
from fno.roles.models import RoleDefinitionSource, RoleLayer, _RoleModel

_LAYER_ORDER = {layer: index for index, layer in enumerate(RoleLayer)}


class RegistryError(ValueError):
    """The registry cannot choose between equal-precedence definitions."""


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
