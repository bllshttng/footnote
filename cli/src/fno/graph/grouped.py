"""The opt-in, human-readable view of a flat graph entry."""

from __future__ import annotations

import json
from collections.abc import Mapping

from fno.graph.store import CANONICAL_FIELD_ORDER
from fno.graph.types import Entry


GROUPED_FIELD_GROUPS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("Identity", ("id", "slug", "title", "type", "project")),
    ("Lifecycle", ("status", "priority", "rank", "size", "batch", "deferred_kind")),
    (
        "Timestamps",
        (
            "created_at",
            "touched_at",
            "locked_at",
            "completed_at",
            "deferred_at",
            "reopened_at",
            "queued_at",
        ),
    ),
    (
        "Reasons",
        ("details", "blocked_reason", "deferred_reason", "reopened_reason", "completion_note"),
    ),
    (
        "Hierarchy",
        ("parent", "children", "blocked_by", "related", "contained_in", "group_slug", "tasks"),
    ),
    ("Supersession", ("superseded_by", "supersedes", "supersession")),
    (
        "Provenance",
        (
            "source",
            "source_kind",
            "source_project",
            "source_session_id",
            "source_harness",
            "source_cwd",
            "source_node_id",
            "source_plan_path",
        ),
    ),
    (
        "Execution",
        (
            "locked_by",
            "locked_by_harness",
            "locked_by_harness_session",
            "session_id",
            "ownership_defect",
            "sessions",
            "dispatch_verb",
            "dispatch_brief",
        ),
    ),
    (
        "Delivery",
        (
            "plan_path",
            "pr_number",
            "pr_url",
            "additional_prs",
            "merge_status",
            "artifact_url",
            "collisions_acknowledged",
        ),
    ),
    ("Cost", ("cost_usd", "cost_sessions")),
    (
        "Content",
        ("has_brief", "roadmap_id", "vision_path", "think_output_path", "think_session_id"),
    ),
)

# These fields are part of the graph's current wire/schema surface but do not
# yet belong to a concept group. Naming them keeps the grouped view honest and
# makes a schema addition fail the assignment test until it is classified.
GROUPED_RESIDUAL_FIELDS: frozenset[str] = frozenset(
    {
        "__updated_at",
        "blocks_everything",
        "caused_by",
        "company_work",
        "compacted",
        "contract_version",
        "cwd",
        "decisions",
        "dep",
        "domain",
        "difficulty",
        "difficulty_history",
        "encounters",
        "fixes_pr",
        "model",
        "mission_active",
        "mission_from_msg_id",
        "mission_id",
        "mission_slug",
        "mission_wave",
        "orphan_ok",
        "points",
        "priority_history",
        "progress_notes",
        "public",
        "queued_reason",
        "reverted",
        "source_inbox_msg",
        "spawned_by_cwd",
        "spawned_by_harness",
        "spawned_by_session",
        "stub_against",
        "tags",
    }
)

# ``Entry`` owns typed fields; the canonical order and legacy set cover the
# forward-compatible graph keys that intentionally remain extras on Entry.
GROUPED_LEGACY_FIELDS: frozenset[str] = frozenset(
    {
        "__updated_at",
        "blocked_reason",
        "blocks_everything",
        "compacted",
        "decisions",
        "difficulty",
        "difficulty_history",
        "group_slug",
        "mission_active",
        "mission_from_msg_id",
        "mission_id",
        "mission_slug",
        "mission_wave",
        "orphan_ok",
        "points",
        "priority_history",
        "public",
        "reopened_at",
        "reopened_reason",
        "source_inbox_msg",
        "spawned_by_cwd",
        "spawned_by_harness",
        "spawned_by_session",
        "tags",
        "tasks",
        "think_output_path",
        "think_session_id",
    }
)

GROUPED_SCHEMA_FIELDS: frozenset[str] = (
    frozenset(CANONICAL_FIELD_ORDER) | frozenset(Entry.model_fields) | GROUPED_LEGACY_FIELDS
)
GROUPED_FIELD_NAMES = frozenset(field for _, fields in GROUPED_FIELD_GROUPS for field in fields)
GROUPED_ASSIGNED_FIELDS = GROUPED_FIELD_NAMES | GROUPED_RESIDUAL_FIELDS
GROUPED_FIELD_LABELS = {"source": "source (origin)"}


def _is_populated(value: object) -> bool:
    """Skip read-path defaults while retaining meaningful false/zero values."""
    if value is None:
        return False
    if isinstance(value, (str, bytes, list, tuple, dict, set, frozenset)):
        return bool(value)
    return True


def _display_value(value: object) -> str:
    if isinstance(value, (dict, list, tuple, set, frozenset)):
        return json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)
    if isinstance(value, bool):
        return str(value).lower()
    return str(value)


def render_grouped(entry: Mapping[str, object]) -> str:
    """Render populated entry fields in stable concept sections.

    Unknown keys are deliberately rendered in ``Residual`` in their original
    order. The flat mapping remains the source of truth and no key is dropped.
    """
    assigned = GROUPED_FIELD_NAMES
    sections: list[str] = []
    for heading, fields in GROUPED_FIELD_GROUPS:
        lines = [
            f"{GROUPED_FIELD_LABELS.get(field, field)}: {_display_value(entry[field])}"
            for field in fields
            if field in entry and _is_populated(entry[field])
        ]
        if lines:
            sections.extend([heading, *lines, ""])

    residual = [
        f"{GROUPED_FIELD_LABELS.get(field, field)}: {_display_value(value)}"
        for field, value in entry.items()
        if field not in assigned and _is_populated(value)
    ]
    if residual:
        sections.extend(["Residual", *residual, ""])
    return "\n".join(sections).rstrip()
