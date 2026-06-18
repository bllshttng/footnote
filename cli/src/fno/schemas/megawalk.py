"""Pydantic schema for megawalk-state.md frontmatter.

Merged in 2026-05-06 megatron->megawalk rename: the simpler MegawalkState
base (skill-only) and the richer MegatronState extension (CLI walker /
campaign-level) collapsed into a single MegawalkState that covers both
surfaces. Defaults make the additional fields optional, so older skill
state files round-trip cleanly.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional

from pydantic import BaseModel, Field

from fno.schemas.common import RoadmapId


@dataclass
class InFlightNode:
    """Represents a graph node currently being driven by the walker."""

    node_id: str
    worktree_path: str
    current_phase: str
    started_at: str
    pr_number: Optional[int]
    review_iteration: int
    last_phase_transition_ts: str
    # Parked fields: set briefly between help-detected and node pruned from in_flight_nodes.
    # Real persistent park state lives on the graph node (deferred_at / deferred_reason).
    parked_at: Optional[str] = None
    parked_reason: Optional[str] = None
    parked_evidence: Optional[str] = None
    # Phase attempt tracking: counts how many times each phase has been entered
    # for this node. Re-entry (flailing fix loop) increments the counter.
    # Populated by Phase 02 stuck detection from per-node events.jsonl.
    # Host process death diagnostics (host_pid, host_exit_code, host_stderr_tail)
    # live on the node_process_died event in .fno/megawalk-events.jsonl,
    # not on this struct. subprocess.run does not expose pid post-exit, so a
    # field here would be permanently None until a future Popen migration.
    attempts_per_phase: Dict[str, int] = field(default_factory=dict)


@dataclass
class PendingPR:
    """Represents a PR that is queued for merge once deps are done."""

    node_id: str
    pr_number: int
    depends_on_nodes: List[str]
    latest_commit_sha: str
    # Phase 03: verification metadata. Optional with defaults so existing
    # queue entries from before this phase ships continue to deserialize cleanly.
    last_verified_at: Optional[str] = None              # ISO timestamp string
    last_verified_state: Optional[str] = None           # "OPEN" | "CLOSED" | "MERGED"
    last_verified_mergeable: Optional[bool] = None
    last_verified_review_decision: Optional[str] = None  # "APPROVED" | "CHANGES_REQUESTED" | "REVIEW_REQUIRED"


class MegawalkState(BaseModel):
    """Schema for .fno/megawalk-state.md frontmatter.

    Megawalk state covers both the in-conversation /megawalk skill and the
    headless `fno megawalk` CLI walker. The status vocabulary is broader
    than target's to accommodate walker modes (LOOPING, PAUSED, IDLE) in
    addition to the canonical IN_PROGRESS/COMPLETE/BLOCKED.
    """

    model_config = {"extra": "allow"}

    # Core loop status (broader than TargetState's StatusStr)
    status: str = "LOOPING"
    roadmap_id: Optional[RoadmapId] = None

    # Cost tracking
    total_cost_usd: float = 0.0
    budget_cap_usd: Optional[float] = None
    avg_task_cost: float = 50.0

    # Failure tracking
    consecutive_failures: int = 0
    tasks_completed_this_session: int = 0

    # Timestamps
    created_at: Optional[str] = None
    updated_at: Optional[str] = None

    # Session info
    session_id: Optional[str] = None
    sessions: List[str] = Field(default_factory=list)

    # Misc loop metadata
    loop_iteration: int = 0
    extra_modifiers: List[str] = Field(default_factory=list)

    # Epic-scope walk (C2, ab-facfaade): when set, selection is restricted
    # to this epic node's transitive children until the subtree drains.
    epic_id: Optional[str] = None

    # Campaign-level orchestration
    campaign_id: Optional[str] = None
    tick_count: int = 0
    last_reality_check_at: Optional[str] = None
    phases_completed: List[str] = Field(default_factory=list)
    current_campaign_phase: Optional[str] = None
    reality_check_interval: int = 5

    # Walker phase 04 fields
    pid: Optional[int] = None
    started_at: Optional[str] = None
    last_poll_ts: Optional[str] = None
    parallel_cap: int = 1
    in_flight_nodes: List[Any] = Field(default_factory=list)
    pause_reason: Optional[str] = None
    block_reason: Optional[str] = None
    review_iteration_cap: int = 5
    review_poll_minutes_min: int = 4
    review_poll_minutes_max: int = 8
    gh_rate_limit_state: Optional[Dict[str, Any]] = None
    pending_pr_queue: List[Any] = Field(default_factory=list)

    # Legacy fields kept for backward compat
    tasks_completed: int = 0
