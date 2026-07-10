"""Common shared types for fno state schemas."""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Annotated, List, Optional

from pydantic import AfterValidator

# -- Regex validators --

# Segment 2 may carry an optional 2-char provenance infix glued to the pid
# (driver codes mw/mt; provider codes cl/cx/gm/ag/hm/oc). The id stays 3
# dash-segments regardless.
_SESSION_ID_RE = re.compile(r"^\d{8}T\d{6}Z-[a-z]{0,2}\d+-[0-9a-f]{6}$")
# Codex exposes its durable thread identity as a canonical lowercase UUID.
_CODEX_THREAD_ID_RE = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
)
_ISO8601_RE = re.compile(r"^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}(:\d{2})?Z?)?$")

STATUS_VALUES = {"IN_PROGRESS", "COMPLETE", "BLOCKED"}


def _validate_session_id(v: str) -> str:
    if not (_SESSION_ID_RE.fullmatch(v) or _CODEX_THREAD_ID_RE.fullmatch(v)):
        raise ValueError(
            "session_id must match YYYYMMDDTHHMMSSZ-[infix]PID-{6hex} "
            f"or a canonical lowercase Codex thread UUID, got: {v!r}"
        )
    return v


def _validate_roadmap_id(v: str) -> str:
    # Liberal, config-free node-id grammar (legacy ab- and any configured
    # prefix/width). Imported lazily so this low-level schema module never
    # imports the graph package at module load.
    from fno.graph._constants import is_wellformed_node_id

    if not is_wellformed_node_id(v):
        raise ValueError(
            f"roadmap_id / graph_id must be a well-formed node id "
            f"(<prefix>-<4..8 hex>), got: {v!r}"
        )
    return v


def _validate_status(v: str) -> str:
    if v not in STATUS_VALUES:
        raise ValueError(
            f"status must be one of {sorted(STATUS_VALUES)}, got: {v!r}"
        )
    return v


SessionId = Annotated[str, AfterValidator(_validate_session_id)]
RoadmapId = Annotated[str, AfterValidator(_validate_roadmap_id)]
StatusStr = Annotated[str, AfterValidator(_validate_status)]


@dataclass
class AlignmentState:
    phases_since_check: int = 0
    check_interval: int = 2
    checks_performed: int = 0
    drift_detected: bool = False
    drift_details: Optional[str] = None
    consecutive_drifts: int = 0


@dataclass
class CheckpointState:
    latest_ref: Optional[str] = None
    latest_name: Optional[str] = None
    rollback_count: int = 0
    max_rollbacks: int = 3


@dataclass
class CircuitBreakerState:
    consecutive_same_error: int = 0
    last_error_signature: Optional[str] = None
    approaches_tried: List[str] = field(default_factory=list)
    tripped: bool = False
    trip_count: int = 0


@dataclass
class VerificationState:
    consecutive_failures: int = 0
    last_failure_phase: Optional[str] = None
    last_failure_error: Optional[str] = None
