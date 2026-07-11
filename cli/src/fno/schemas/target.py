"""Pydantic schema for target-state.md frontmatter."""
from __future__ import annotations

import warnings
from typing import Any, Dict, List, Optional

from pydantic import BaseModel, Field, field_validator, model_validator

from fno.schemas.common import (
    SessionId,
    RoadmapId,
    StatusStr,
)

# Module-level seen-set for once-per-session legacy coercion warnings.
# Key: (field_name, raw_string_value). Prevents flooding logs when many
# state files with legacy string gate values are loaded in one session.
_GATE_COERCION_WARNED: set[tuple[str, str]] = set()


class MergeFailureRecord(BaseModel):
    """Record written by pr-merge.sh for a failed merge attempt."""

    model_config = {"extra": "allow"}

    pr: int
    reason: str


class ConflictResolutionRecord(BaseModel):
    """Record written by rebase-resolve.sh for a resolved conflict.

    All fields are Optional with extra="allow" because the on-disk shape
    varies by invocation (files list, resolution_commits, etc.). Claude's
    Discretion #4 from the design doc: start permissive, tighten later.
    """

    model_config = {"extra": "allow"}

    pr: Optional[int] = None
    resolution: Optional[str] = None


class AlignmentModel(BaseModel):
    model_config = {"extra": "allow"}
    phases_since_check: int = 0
    check_interval: int = 2
    checks_performed: int = 0
    drift_detected: bool = False
    drift_details: Optional[str] = None
    consecutive_drifts: int = 0


class CheckpointModel(BaseModel):
    model_config = {"extra": "allow"}
    latest_ref: Optional[str] = None
    latest_name: Optional[str] = None
    rollback_count: int = 0
    max_rollbacks: int = 3


class CircuitBreakerModel(BaseModel):
    model_config = {"extra": "allow"}
    consecutive_same_error: int = 0
    last_error_signature: Optional[str] = None
    approaches_tried: List[str] = Field(default_factory=list)
    tripped: bool = False
    trip_count: int = 0


class VerificationModel(BaseModel):
    model_config = {"extra": "allow"}
    consecutive_failures: int = 0
    last_failure_phase: Optional[str] = None
    last_failure_error: Optional[str] = None


class TargetState(BaseModel):
    """Schema for .fno/target-state.md frontmatter.

    Fields mirror the keys written by init-target-state.sh and the
    target stop hook. Optional fields default to None or their init values.
    Extra fields are allowed to handle future additions without hard failures.
    """

    model_config = {"extra": "allow"}

    @model_validator(mode="before")
    @classmethod
    def _backfill_fno_id(cls, data: Any) -> Any:
        """Back-fill fno_id from the legacy session_id key (one-release alias).

        The target-minted id is renamed fno_id; manifests dual-write both keys
        for one release and readers resolve fno_id-first. A pre-rename manifest
        carries only session_id, so populate fno_id from it here when absent.
        Mirrors the claude_transcript_id -> claude_session_id rename.
        """
        if isinstance(data, dict) and not data.get("fno_id") and data.get("session_id"):
            data = {**data, "fno_id": data["session_id"]}
        return data

    @field_validator("clean_passed", "goal_verification_passed", "browser_testing_passed", mode="before")
    @classmethod
    def _coerce_gate_field(cls, v: Any, info: Any) -> bool:
        """Coerce legacy string gate values to bool.

        Legacy writers emitted "passed" | "skipped" | "failed". New writers use
        True/False directly. A DeprecationWarning fires once per (field, value) pair
        per session via the module-level _GATE_COERCION_WARNED seen-set.
        """
        if isinstance(v, bool):
            return v
        # Fix 3: accept bare int (0/1) from YAML writers that emit numeric booleans.
        # bool is a subclass of int, so the bool branch above must come first -- which
        # it does. Bare ints fall through to this branch. No DeprecationWarning fires
        # because this is not a legacy-string shape; it is an alternate writer format.
        if isinstance(v, int):
            return bool(v)
        if v is None:
            return False
        if isinstance(v, str):
            field_name = info.field_name if info is not None else "unknown"
            if v == "passed":
                result = True
            elif v in ("skipped", "failed"):
                result = False
            else:
                raise ValueError(
                    f"TargetState.{field_name}: expected bool or legacy string "
                    f"'passed'/'skipped'/'failed', got {v!r}"
                )
            key = (field_name, v)
            if key not in _GATE_COERCION_WARNED:
                _GATE_COERCION_WARNED.add(key)
                warnings.warn(
                    f"TargetState.{field_name}: legacy string value {v!r} coerced to bool "
                    f"{result}; update writer",
                    DeprecationWarning,
                    stacklevel=2,
                )
            return result
        raise ValueError(
            f"TargetState gate field expected bool or legacy string, got {type(v).__name__!r}: {v!r}"
        )

    # Core status
    status: StatusStr = "IN_PROGRESS"
    current_phase: Optional[str] = None
    iteration: int = 1
    mode: Optional[str] = None
    size: Optional[str] = None

    # Plan / graph identity
    input: Optional[str] = None
    input_type: Optional[str] = None
    plan_path: Optional[str] = None
    graph_id: Optional[RoadmapId] = None

    # Provenance: strict 16-lowercase-hex nonce injected by the verify-child-promise
    # handshake (ab-c4acc10a). Pattern verified safe per Locked Decision #7: pre-
    # implementation grep across production state files confirmed zero non-conforming
    # values. Previously accepted via extra="allow"; now a modeled field.
    provenance_nonce: Optional[str] = Field(default=None, pattern=r"^[a-f0-9]{16}$")

    # fno_id is the canonical target-minted run id. session_id is a one-release
    # legacy MIRROR of fno_id (same value, back-filled by _backfill_fno_id),
    # kept only so pre-rename readers don't break; it is removed next release.
    # It is NOT the harness session - that lives in claude_session_id /
    # codex_thread_id (extra fields). Do not read session_id as "the session".
    fno_id: Optional[SessionId] = None
    session_id: Optional[SessionId] = None
    sessions: List[str] = Field(default_factory=list)
    owner_pid: Optional[int] = None
    owner_started_at: Optional[str] = None
    owner_cwd: Optional[str] = None

    # Provider config
    execution_mode: Optional[str] = None
    created_at: Optional[str] = None
    updated_at: Optional[str] = None
    cross_project: bool = False
    provider: Optional[str] = None
    provider_mode: Optional[str] = None
    provider_upgrade_reason: Optional[str] = None

    # Scratchpad / coordination
    scratchpad_path: Optional[str] = None
    coordinator_phase: Optional[str] = None
    session_start_context_loaded: bool = False
    domain: Optional[str] = None
    domain_phases: Optional[Dict[str, str]] = None

    # Skip flags
    no_external: bool = False
    no_docs: bool = False
    no_ship: bool = False
    no_verify: bool = False
    no_goals: bool = False
    no_browser: bool = False
    no_clean: bool = False
    no_how_to: bool = False
    no_research: bool = False
    no_verify_fresh: bool = False
    adversarial: bool = False
    has_ui: bool = False

    # Completion gates
    quality_check_passed: bool = False
    output_validated: bool = False
    ledger_updated: bool = False
    artifact_shipped: bool = False
    pr_number: Optional[int] = None
    # Gate fields: bool with legacy-string coercion (see _coerce_gate_field validator).
    # Old writers emitted "passed" | "skipped" | "failed"; new writers use True/False.
    clean_passed: bool = False
    external_review_passed: bool = False
    goal_verification_passed: bool = False
    docs_generated: bool = False
    browser_testing_passed: bool = False

    # Auto-merge
    auto_merge_enabled: bool = False
    auto_merge_approved: bool = False
    auto_merge_source: Optional[str] = None
    # Scalar int lists: sole writer is scripts/lib/pr-merge.sh which emits PR numbers.
    merged_prs: List[int] = Field(default_factory=list)
    merge_auto_queued: List[int] = Field(default_factory=list)
    # Record lists: sub-models use extra="allow" for legacy tolerance.
    merge_failed: List[MergeFailureRecord] = Field(default_factory=list)
    conflicts_resolved: List[ConflictResolutionRecord] = Field(default_factory=list)

    # Nested sub-models
    verification: VerificationModel = Field(default_factory=VerificationModel)
    circuit_breaker: CircuitBreakerModel = Field(default_factory=CircuitBreakerModel)
    alignment: AlignmentModel = Field(default_factory=AlignmentModel)
    checkpoint: CheckpointModel = Field(default_factory=CheckpointModel)
