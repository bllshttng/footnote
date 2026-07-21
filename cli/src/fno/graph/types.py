"""Type definitions for the fno graph module.

Contains Status/Priority enums and the Entry pydantic model.
"""
from __future__ import annotations

import math
from enum import Enum
from typing import Literal, Optional, Union

from pydantic import BaseModel, Field, computed_field, field_validator, model_validator


class Status(str, Enum):
    ready = "ready"
    design = "design"
    in_progress = "in_progress"
    blocked = "blocked"
    done = "done"
    superseded = "superseded"  # Fix 2: added to match _derive_status bare-string return
    idea = "idea"
    deferred = "deferred"
    in_review = "in_review"  # node carries an open, unmerged PR; held out of dispatch


class Priority(str, Enum):
    p0 = "p0"
    p1 = "p1"
    p2 = "p2"
    p3 = "p3"


# Lifecycle phases stamped into a node's append-only `sessions` list (x-b6e4).
# Exactly these four; review/plan-validation are deliberately excluded (Locked
# Decision 2). The single source of truth for phase validation in
# store.append_session_record and the `session add` CLI.
SESSION_PHASES: frozenset[str] = frozenset({"think", "blueprint", "do", "ship"})


# Re-export the canonical PRIORITY_ORDER from _constants so this module
# stays in sync without a parallel literal.
from fno.graph._constants import PRIORITY_ORDER  # noqa: E402,F401

import datetime as _dt


def _ts_now() -> str:
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _derive_status(data: dict) -> str:
    """Derive single-entry status from a dict of field values.

    Mirrors the single-entry portion of recompute_statuses. Cascade-wide
    checks (blocked_by sibling completed_at) stay in recompute_statuses
    because this function cannot see other entries.
    """
    if data.get("completed_at"):
        return "done"
    if data.get("superseded_by"):
        return "superseded"
    if data.get("deferred_at"):
        return "deferred"
    # Open, unmerged PR -> in_review (see recompute_statuses for the rationale).
    # Merge sets completed_at, so `done` above wins once the PR lands.
    if data.get("pr_number"):
        return "in_review"
    if data.get("blocked_by"):
        return "blocked"
    # locked_by-first; tolerate a raw pre-rename dict passed straight in (not via
    # the store normalize) that still carries only the legacy session_id.
    if data.get("locked_by") or data.get("session_id"):
        return "in_progress"
    if not data.get("plan_path"):
        return "idea"
    from fno.graph.ladder import is_design_stage

    if is_design_stage(data):
        return "design"
    return "ready"


class Entry(BaseModel):
    """A single feature graph node, matching the graph.json schema exactly."""

    id: str
    parent: Optional[str] = None
    # Derived inverse-of-parent index: compact summaries of direct children,
    # each {id, title, project, status}. Recomputed on every write by
    # store.canonicalize_entries, so it is read-mostly here -- declared so
    # model_dump round-trips it and old graph.json entries (no children key)
    # parse without migration.
    children: list[dict] = Field(default_factory=list)
    # Title-derived human handle (ab-f82e8083). Additive: leads display, but
    # `id` stays the canonical key. Assigned once when a node is first persisted
    # (store.ensure_slugs) and immutable thereafter; null until backfilled.
    slug: Optional[str] = None
    title: Optional[str] = None
    type: str = "feature"
    project: Optional[str] = None
    cwd: Optional[str] = None
    priority: str = "p2"
    # Optional curated board rank. Nullable float so `--before`/`--after`
    # insert at a midpoint without renumbering siblings; null = unranked,
    # ordered after the (priority, created_at) fallback within a lane.
    rank: Optional[float] = None
    domain: str = "code"
    blocked_by: list[str] = Field(default_factory=list)
    # Lock owner. locked_by is canonical; session_id is the one-release mirror
    # (_normalize_lock_fields keeps them equal). locked_by_harness* record the
    # holder's provider + harness-session UUID (US6).
    locked_by: Optional[str] = None
    locked_by_harness: Optional[str] = None
    locked_by_harness_session: Optional[str] = None
    session_id: Optional[str] = None
    claimed_at: Optional[str] = None
    completed_at: Optional[str] = None
    # Defer state. Mutually exclusive with completed_at by cascade
    # convention: done > deferred. recompute_statuses enforces that
    # ordering, and cmd_done clears these fields when transitioning
    # from deferred to done.
    deferred_at: Optional[str] = None
    deferred_reason: Optional[str] = None
    has_brief: bool = False
    roadmap_id: Optional[str] = None
    vision_path: Optional[str] = None
    details: Optional[str] = None
    cost_usd: Optional[float] = None
    cost_sessions: list[dict] = Field(default_factory=list)
    size: Optional[str] = None
    # Optional per-node model pin (x-571f). A dispatcher appends `--model <m>`
    # to the worker spawn it builds; null = provider default (no behavior
    # change). Validated as a single non-whitespace token at write time; the
    # no-whitespace rule protects MODEL_FLAG shell word-splitting in the loop
    # drivers. Exact name `model` is legal on a BaseModel (only `model_*` is a
    # protected pydantic namespace).
    model: Optional[str] = None
    batch: Optional[str] = None
    # Per-node dispatch overrides (US3). dispatch_verb picks the worker command
    # (`<verb> {id}`, allowlist-validated at resolve, not write); dispatch_brief
    # rides TARGET_BRIEF env into cold-start. Both null = the built-in
    # `/target no-merge {id}` default. Old graph.json entries parse without them.
    dispatch_verb: Optional[str] = None
    dispatch_brief: Optional[str] = None
    plan_path: Optional[str] = None
    pr_number: Optional[int] = None
    pr_url: Optional[str] = None
    # Follow-up PRs shipped against the same node (e.g. wrap-up + review-fix
    # PRs after the primary). Each entry: {"number": int, "url": str|None,
    # "note": str|None}. Primary stays in pr_number/pr_url. Empty by default
    # so old graph.json entries parse without migration.
    additional_prs: list[dict] = Field(default_factory=list)
    merge_status: Optional[str] = None
    # Causal links (W4 telemetry, x-f063): survival math needs a fix node to
    # point back at what it fixes and a reverted ship to stop counting as a
    # survival. Writers: `backlog update --caused-by/--fixes-pr/--reverted`
    # (manual), retro-harvest node creation (auto caused_by), reconcile's
    # best-effort revert stamp.
    caused_by: Optional[str] = None
    fixes_pr: Optional[int] = None
    reverted: bool = False
    artifact_url: Optional[str] = None
    completion_note: Optional[str] = None
    created_at: Optional[str] = None
    # superseded_by is an extra field carried by superseded nodes; declared
    # here so computed_field can reference it cleanly without going through
    # model_extra.
    superseded_by: Optional[str] = None

    # Parent-edge provenance (x-30f6). Declared first-class (typed, default
    # None) so model_dump round-trips them and they validate as strings. The
    # store applies the same defaults on the raw-dict read path; the older
    # source_* fields stay extras (extra="allow") and are intentionally not
    # promoted here to keep this change surgical.
    #   source_node_id      backlog node -> the origin node it was spawned from
    #   source_harness      harness of source_session_id: claude|codex|gemini
    #   source_cwd          originating SESSION cwd (transcript-resolver key;
    #                       distinct from the node's durable `cwd` project root)
    #   source_plan_path    plan_path of the originating session, if any
    #   spawned_by_session  parent session id ambient at node birth / spawn
    #   spawned_by_harness  claude | codex | gemini (from env)
    #   spawned_by_cwd      parent cwd, for the transcript-path slug resolver
    source_harness: Optional[str] = None
    source_cwd: Optional[str] = None
    source_node_id: Optional[str] = None
    source_plan_path: Optional[str] = None
    spawned_by_session: Optional[str] = None
    spawned_by_harness: Optional[str] = None
    spawned_by_cwd: Optional[str] = None

    # Append-only lifecycle provenance (x-b6e4): one {phase, harness, session_id,
    # at} record per phase boundary a session crossed. Unique per
    # (phase, harness, session_id); the same session may appear across phases and
    # a takeover appends another entry for the same phase. Written only through
    # store.append_session_record. Empty on legacy nodes.
    sessions: list[dict] = Field(default_factory=list)

    model_config = {"extra": "allow"}

    @field_validator("rank", mode="before")
    @classmethod
    def _validate_rank(cls, v: object) -> Optional[float]:
        """Reject bool / inf / NaN ranks at the model boundary (ab-6603350c).

        Live render/verb paths already guard these (``_rank_band`` finite-check
        + bool exclusion, ``_is_ranked`` bool exclusion) and the raw-dict path
        dominates, so this is defensive hardening for the cases that *do*
        construct an ``Entry``: a non-finite or bool rank fails loudly here
        rather than silently degrading to "unranked" downstream. ``None``
        (unranked) and ordinary finite numbers pass through. Runs in
        ``before`` mode so a bool is caught before pydantic coerces it to 1.0.
        """
        if v is None:
            return None
        if isinstance(v, bool):
            raise ValueError("rank must be a real number, not a bool")
        try:
            f = float(v)
        except (TypeError, ValueError, OverflowError) as exc:
            raise ValueError(f"rank must be a finite number, got {v!r}") from exc
        if not math.isfinite(f):
            raise ValueError(f"rank must be finite, got {v!r}")
        return f

    @model_validator(mode="before")
    @classmethod
    def _check_status_drift(cls, data: object) -> object:
        """Drop persisted status; emit graph_status_drift event if it differs
        from the computed value so forensic audit can track legacy graph.json.

        Best-effort: event emit failures are swallowed so deserialization
        never crashes on a telemetry error.
        """
        if not isinstance(data, dict):
            return data
        # `_status` is the pre-rename key; a row read outside _apply_graph_defaults
        # still carries it.
        persisted = data.pop("status", None)
        legacy = data.pop("_status", None)
        if persisted is None:
            persisted = legacy
        if persisted is None:
            return data

        computed = _derive_status(data)
        if persisted != computed:
            # Fix 1 (Locked Decision #2): suppress the known single-entry-vs-cascade
            # approximation gap. recompute_statuses resolves nodes whose blocked_by
            # siblings are all completed to "ready"/"in_progress" and writes that to disk.
            # On reload, _derive_status still sees a non-empty blocked_by list and
            # returns "blocked" -- a single-entry approximation, not a real drift.
            # Emitting graph_status_drift here would be a false positive; the cascade
            # in recompute_statuses is authoritative for that case.
            if computed == "blocked" and persisted in {"ready", "design", "in_progress", "idea"}:
                return data

            try:
                from fno.events import append_event  # local import avoids circularity

                event = {
                    "ts": _ts_now(),
                    "type": "graph_status_drift",
                    "source": "migration",
                    "data": {
                        "entry_id": data.get("id", ""),
                        "persisted": persisted,
                        "computed": computed,
                    },
                }
                append_event(event)
            except (OSError, ImportError):
                # Fix 4: narrowed from broad except Exception. OSError covers transient
                # filesystem failures writing events.jsonl; ImportError covers installs
                # where the fno.events module is absent. Real bugs (ValidationError,
                # SchemaUnavailableError, KeyError, AttributeError) must propagate loudly.
                pass
        return data

    @computed_field
    @property
    def status(self) -> str:
        """Derive entry status from single-entry fields.

        Precedence (mirrors recompute_statuses single-entry portion):
          completed_at set    -> "done"
          superseded_by set   -> "superseded"
          deferred_at set     -> "deferred"
          pr_number set       -> "in_review"
          non-empty blocked_by -> "blocked"
              (single-entry cannot verify sibling completed_at -- just
               a non-empty list is the best approximation here;
               cascade-wide open-blocker check stays in recompute_statuses)
          locked_by set       -> "in_progress"
          no plan_path        -> "idea"
          plan says design    -> "design"
          else                -> "ready"
        """
        return _derive_status({
            "completed_at": self.completed_at,
            "superseded_by": self.superseded_by,
            "deferred_at": self.deferred_at,
            "pr_number": self.pr_number,
            "blocked_by": self.blocked_by,
            # locked_by-first; fall back to the legacy session_id mirror in case
            # this Entry was built from a pre-rename node not yet normalized.
            "locked_by": self.locked_by or self.session_id,
            "plan_path": self.plan_path,
            # Needed to resolve a repo-relative plan_path for the design probe.
            "cwd": self.cwd,
        })
