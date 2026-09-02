"""Declared decisive fields and their behavioral coverage."""
from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class DecisiveField:
    field: str
    reader: str
    verdict: str
    test: str


DECISIVE_FIELDS = (
    DecisiveField(
        field="superseded_by",
        reader="fno.think_inspect._node_summary",
        verdict="is this candidate a live fold target",
        test="test_superseded_candidate_is_visible_to_the_consolidation_gate",
    ),
)
