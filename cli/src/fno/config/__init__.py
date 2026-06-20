"""Pydantic settings schema for the fno CLI.

Settings are DEEP-MERGED across every candidate file that exists, with
higher-priority files overriding lower-priority ones key-by-key (nested
dicts merge recursively; scalars and lists replace wholesale). Candidate
priority, highest first:
  1. $FNO_CONFIG env var (explicit path; when set, the ONLY candidate)
  2. <worktree_root>/.fno/settings.yaml  (project-local to this checkout)
  3. <canonical_root>/.fno/settings.yaml  (the main checkout's config,
     reached via the main worktree from `git worktree list`; lets a linked
     worktree read shared project config with zero per-worktree setup;
     deduped when 2 == 3)
  4. ~/.fno/settings.yaml  (per-user global; shared defaults)

A key absent from a higher-priority file falls through to the next file
down, so the per-user global can hold shared defaults (e.g.
config.obsidian.vault) while each project sets only its deltas (e.g.
config.post_merge.parking_lot_path). When no file exists, built-in defaults apply.
This mirrors the shell reader (scripts/lib/config.sh, per-key local->global
fallback) and the provider loader, both of which already merge project-local
over global.

Cache: load_settings() is cached per-process via functools.lru_cache.
Mid-process edits to settings.yaml do not take effect; the next
subprocess sees the new value.

Design decisions (locked in 2026-05-14-path-config.md):
  - extra='ignore' for forward compatibility (do NOT change to 'forbid')
  - Emit a startup WARNING for unknown keys (not an error)
  - Reject glob chars (*?[) at validation time, not at resolve time
  - Reject {vault} when obsidian.enabled is False
  - PATH_MAX = 4096 bytes enforced on state_dir / plans_dir
"""
from __future__ import annotations

import logging
import os
from functools import lru_cache
from pathlib import Path
from typing import Optional

import yaml
from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

_LOG = logging.getLogger(__name__)

# POSIX PATH_MAX
_PATH_MAX = 4096

# Shell glob characters to reject in path values
_GLOB_CHARS = frozenset("*?[")


def _check_no_glob(value: str, field_name: str) -> None:
    """Raise ValueError if value contains shell glob characters."""
    found = _GLOB_CHARS & set(value)
    if found:
        chars = ", ".join(sorted(repr(c) for c in found))
        raise ValueError(
            f"{field_name!r} contains glob character(s) {chars}; "
            "glob characters are not allowed in path values"
        )


def _check_path_max(value: str, field_name: str) -> None:
    """Raise ValueError if the UTF-8 encoded value exceeds PATH_MAX."""
    if len(value.encode("utf-8")) > _PATH_MAX:
        raise ValueError(
            f"{field_name!r} exceeds PATH_MAX ({_PATH_MAX} bytes): "
            f"{len(value.encode('utf-8'))} bytes"
        )


# ---------------------------------------------------------------------------
# Schema models
# ---------------------------------------------------------------------------


class PathsBlock(BaseModel):
    """Per-resource path overrides.

    Every field is optional; omitting it causes the resolver to derive
    the path from state_dir instead.
    """

    model_config = ConfigDict(extra="ignore")

    graph_json: Optional[str] = None
    ledger_json: Optional[str] = None
    briefs_dir: Optional[str] = None
    fleet_dir: Optional[str] = None
    postmortems_dir: Optional[str] = None
    worktrees_base: Optional[str] = None
    memory_dir: Optional[str] = None
    hook_logs_dir: Optional[str] = None
    inbox_dir: Optional[str] = None
    inbox_path: Optional[str] = None
    agents_registry_path: Optional[str] = None
    handoffs_dir: Optional[str] = None
    retro_pending_dir: Optional[str] = None
    evals_history: Optional[str] = None
    bus_dir: Optional[str] = None

    @field_validator(
        "graph_json",
        "ledger_json",
        "briefs_dir",
        "fleet_dir",
        "postmortems_dir",
        "worktrees_base",
        "memory_dir",
        "hook_logs_dir",
        "inbox_dir",
        "inbox_path",
        "agents_registry_path",
        "handoffs_dir",
        "retro_pending_dir",
        "evals_history",
        "bus_dir",
        mode="before",
    )
    @classmethod
    def reject_glob_chars(cls, v: object) -> object:
        if isinstance(v, str):
            _check_no_glob(v, "paths override")
        return v


class ObsidianBlock(BaseModel):
    """Obsidian vault integration settings."""

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False
    vault: Optional[str] = None

    @field_validator("vault", mode="before")
    @classmethod
    def reject_glob_chars(cls, v: object) -> object:
        if isinstance(v, str):
            _check_no_glob(v, "obsidian.vault")
        return v

    @model_validator(mode="after")
    def vault_required_when_enabled(self) -> "ObsidianBlock":
        """Reject enabled=True with no vault at schema load time, not resolve time."""
        if self.enabled and not self.vault:
            raise ValueError("obsidian.enabled is true but obsidian.vault is not set")
        return self


class ProjectBlock(BaseModel):
    """Project identity settings."""

    model_config = ConfigDict(extra="ignore")

    id: Optional[str] = None
    # Free-text statement of what this codebase is and why. Read at SessionStart
    # by hooks/inject-project-vision.sh for semantic grounding. Canonical home is
    # config.project.vision; the loader aliases a legacy top-level project.vision.
    vision: Optional[str] = None

    @model_validator(mode="before")
    @classmethod
    def coerce_string_shorthand(cls, v: object) -> object:
        """Accept `project: <id>` shorthand at parse time and coerce to {id: <id>}.

        Legacy settings.yaml files (including smoke fixtures) use the bare-string
        form. Preserve back-compat by coercing here before field validation runs.
        """
        if isinstance(v, str):
            return {"id": v}
        return v

    @field_validator("id", mode="before")
    @classmethod
    def validate_project_id(cls, v: object) -> object:
        import re

        if isinstance(v, str) and v and not re.fullmatch(r"[A-Za-z0-9._-]+", v):
            raise ValueError(
                f"project.id {v!r} contains invalid characters; "
                "only [A-Za-z0-9._-] are allowed"
            )
        return v


class BlueprintBlock(BaseModel):
    """Blueprint / epic-decomposition settings (nested under 'config.blueprint')."""

    model_config = ConfigDict(extra="ignore")

    max_prs_per_epic: int = 4

    @field_validator("max_prs_per_epic")
    @classmethod
    def max_prs_per_epic_positive(cls, v: int) -> int:
        """A decomposition ceiling below 1 can never produce a group node."""
        if v < 1:
            raise ValueError("config.blueprint.max_prs_per_epic must be >= 1")
        return v


class MaintainBlock(BaseModel):
    """`fno backlog maintain` settings (nested under 'config.backlog.maintain')."""

    model_config = ConfigDict(extra="ignore")

    staleness_days: int = 30
    max_failed_attempts: int = 3

    @field_validator("staleness_days")
    @classmethod
    def staleness_days_positive(cls, v: int) -> int:
        """An idea cannot be 'older than N days' for N < 1."""
        if v < 1:
            raise ValueError("config.backlog.maintain.staleness_days must be >= 1")
        return v

    @field_validator("max_failed_attempts")
    @classmethod
    def max_failed_attempts_positive(cls, v: int) -> int:
        """A consecutive-failure threshold below 1 would auto-defer every node
        on its first failure (or with zero failures), so N must be >= 1."""
        if v < 1:
            raise ValueError(
                "config.backlog.maintain.max_failed_attempts must be >= 1"
            )
        return v


class BacklogBlock(BaseModel):
    """Backlog hygiene settings (nested under 'config.backlog').

    ``id_prefix`` / ``id_hex_width`` configure the node-ID minting scheme
    (``<prefix><hex>``). Schema defaults are the LEGACY values - an absent
    ``id_prefix`` (``None``) falls back to ``ab-`` at the accessor, and the width
    defaults to 8 - so an existing ``config.backlog`` block with no id keys is
    byte-identical. The setup wizard OFFERS width 4 for new installs; that is the
    wizard's default, not the schema default (AC3-FR).
    """

    model_config = ConfigDict(extra="ignore")

    maintain: MaintainBlock = Field(default_factory=MaintainBlock)
    id_prefix: Optional[str] = None
    id_hex_width: int = 8

    @field_validator("id_prefix")
    @classmethod
    def validate_id_prefix(cls, v: Optional[str]) -> Optional[str]:
        """Lowercase, shape-check, normalize to a trailing '-', reject reserved.

        ``None`` passes through (means 'not configured' -> legacy fallback).
        A provided value must be a 1-7 char lowercase, letter-led token
        (``^[a-z][a-z0-9]{0,6}-?$``), is normalized to end in ``-``, and must
        not collide with a reserved sibling family (``cv-``/``fu-``/``tgt-``).
        """
        if v is None:
            return None
        import re as _re

        from fno.graph._constants import RESERVED_PREFIXES

        raw = v.strip().lower()
        if not _re.fullmatch(r"[a-z][a-z0-9]{0,6}-?", raw):
            raise ValueError(
                "config.backlog.id_prefix must be a lowercase token of 1-7 "
                "letters/digits (letter-led) with an optional trailing '-', "
                f"got: {v!r}"
            )
        normalized = raw if raw.endswith("-") else raw + "-"
        if normalized in RESERVED_PREFIXES:
            raise ValueError(
                "config.backlog.id_prefix must not collide with a reserved "
                f"family {sorted(RESERVED_PREFIXES)}, got: {v!r}"
            )
        return normalized

    @field_validator("id_hex_width")
    @classmethod
    def validate_id_hex_width(cls, v: int) -> int:
        """Hex width must be an integer in [4, 8] (4-hex stays collision-safe
        via mint-time retry; 8-hex is the legacy width)."""
        if not isinstance(v, int) or isinstance(v, bool) or not (4 <= v <= 8):
            raise ValueError(
                f"config.backlog.id_hex_width must be an integer in [4, 8], got: {v!r}"
            )
        return v


class PostMergeBlock(BaseModel):
    """Post-merge ritual settings (nested under 'config.post_merge').

    Drives the /fno:pr merged skill: where to write prose follow-up
    todos after a PR merges, and whether the ritual is enabled for this repo.

    `parking_lot_path` is repo-relative. The vault-area name does NOT equal the
    project name (e.g. example-pipeline -> internal/etl/backlog/parking-lot.md),
    so it must be set explicitly per repo and is never derived from the project
    name. Left unset by default so a repo that has not opted in resolves to an
    empty value and the skill fails loud rather than guessing a path.
    """

    model_config = ConfigDict(extra="ignore")

    parking_lot_path: Optional[str] = None
    enabled: bool = True
    self_reap: bool = False

    @field_validator("parking_lot_path", mode="before")
    @classmethod
    def validate_parking_lot_path(cls, v: object) -> object:
        if isinstance(v, str):
            _check_no_glob(v, "config.post_merge.parking_lot_path")
            _check_path_max(v, "config.post_merge.parking_lot_path")
            # parking_lot_path is documented as repo-relative. Enforce it at the
            # schema level so the skill's downstream bash guard is
            # defense-in-depth, not the sole barrier - the post-merge skill
            # joins this onto the repo root and writes to it, so an absolute
            # path or a '..' traversal would escape the repo.
            if v.startswith("/") or v.startswith("~"):
                raise ValueError(
                    "config.post_merge.parking_lot_path must be repo-relative "
                    f"(no leading '/' or '~'); got: {v!r}"
                )
            if ".." in v.split("/"):
                raise ValueError(
                    "config.post_merge.parking_lot_path must not contain a '..' "
                    f"path segment (no repo escape); got: {v!r}"
                )
        return v

    @field_validator("self_reap", mode="before")
    @classmethod
    def _coerce_self_reap(cls, v: object) -> bool:
        """Fail-safe to false on any non-boolean value.

        self_reap lets a finished /fno:pr merged background worker remove its
        own agent-view row (``claude rm <id>``) at the end of the ritual.
        Default off: the ritual prints the one-keystroke reap command instead,
        and only auto-removes when this is an explicit affirmative
        (``true``/``yes``/``on``/``1``). A scalar typo (``self_reap: banana``)
        coerces to False rather than raising, and false is the safe direction -
        auto-removing a row the operator still wanted is the costly mistake.
        """
        if isinstance(v, bool):
            return v
        if isinstance(v, int):  # bool already handled above
            return v == 1
        if isinstance(v, str):
            return v.strip().lower() in {"1", "true", "yes", "on"}
        return False


class CrossModelBlock(BaseModel):
    """Cross-model review opt-in (nested under 'config.review.cross_model').

    Gates the provider-rotated sigma-review panel (ab-6c8f4c61): when
    `enabled` is true the internal review panel may route correctness agents
    to a different provider (codex/gemini) than wrote the code, catching
    model-specific blind spots. Default False: existing all-claude review is
    byte-for-byte unchanged until the operator turns it on (Locked Decision 7).
    An explicit `config.review.agent_providers` map ALSO engages cross-model
    (the selector treats either signal as opt-in); this flag is the
    no-map-needed switch.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False

    @field_validator("enabled", mode="before")
    @classmethod
    def _coerce_enabled(cls, v: object) -> bool:
        """Fail-safe to disabled on a non-boolean value.

        Mirrors config.auto_continue.enabled: a scalar typo
        (`enabled: banana`) is operator error; coercing it to False keeps
        load_settings() succeeding for every OTHER consumer rather than
        raising. Only a clear affirmative enables; cross-model spends a second
        provider's quota, so false-enabled is the dangerous direction.
        """
        if isinstance(v, bool):
            return v
        if isinstance(v, int):  # bool already handled above
            return v == 1
        if isinstance(v, str):
            return v.strip().lower() in {"1", "true", "yes", "on"}
        return False


class ReviewBlock(BaseModel):
    """External-review gate settings (nested under 'config.review').

    `required_bots` is the must-have-reviewed list consumed by the
    `fno-agents loop-check` review gate (control-plane step 2, ab-f1c5a9ed):
    a session terminates DonePRGreen only when every listed bot has at least
    one completed review pass and no unaddressed blocking finding.

    Semantics (mirrors the Rust-side parser in loopcheck.rs):
      - key absent (None)  -> code default ["chatgpt-codex-connector"]
      - explicit []        -> declared no-review-gate path (PR + CI only),
                              mirroring ci.declared_none; never auto-detected
      - non-empty list     -> every listed bot must pass

    Distinct from `config.external_reviewers` (the recognition/matching
    list): that list says which logins count as bots; this one says which
    bots are REQUIRED to have reviewed.
    """

    model_config = ConfigDict(extra="ignore")

    required_bots: Optional[list[str]] = None
    # The INVOCATION list (Locked Decision 2): which AI reviewers /pr requests a
    # review from (gemini | codex | coderabbit | claude | none). Distinct from
    # required_bots (the GATE: which GitHub bot logins must have reviewed before
    # the ship gate goes green). Canonical home is config.review.external_reviewers;
    # the loader aliases the legacy config.external_reviewers (list) and the
    # singular config.external_reviewer (scalar) to it. Empty == external review
    # disabled (callers treat no entries / all-"none" as off). read by
    # skills/pr/scripts/list-reviewers.sh.
    external_reviewers: list[str] = Field(default_factory=list)
    # Per-agent provider routing for the cross-model review panel (ab-6c8f4c61).
    # Map of agent-name -> provider (claude | codex | gemini | alternate).
    # Default empty: the curated correctness-subset default is computed in the
    # T2.1 resolver, NOT baked here, so an empty map stays a faithful empty map.
    agent_providers: dict[str, str] = Field(default_factory=dict)
    cross_model: CrossModelBlock = Field(default_factory=CrossModelBlock)

    @field_validator("required_bots", mode="before")
    @classmethod
    def coerce_malformed_to_default(cls, v: object) -> object:
        """Fail closed to the code default on a non-list value (AC3-ERR).

        A scalar or mapping here is operator error; treating it as "absent"
        keeps the gate on its default rather than failing the whole settings
        load (the Rust reader does the same).
        """
        if v is None or isinstance(v, list):
            return v
        _LOG.warning(
            "settings.yaml: config.review.required_bots is not a list (%r); "
            "using the code default",
            v,
        )
        return None

    @field_validator("external_reviewers", mode="before")
    @classmethod
    def coerce_external_reviewers(cls, v: object) -> object:
        """Accept the legacy scalar form and fail-safe a malformed value to [].

        A bare string (`external_reviewers: gemini`) coerces to a single-item
        list for back-compat with the scalar `external_reviewer`. None or any
        non-list/non-str value degrades to [] (external review off) rather than
        failing the whole settings load.
        """
        if v is None:
            return []
        if isinstance(v, str):
            return [v]
        if isinstance(v, list):
            return v
        _LOG.warning(
            "settings.yaml: config.review.external_reviewers is not a list (%r); "
            "treating external review as disabled",
            v,
        )
        return []

    @field_validator("agent_providers", mode="before")
    @classmethod
    def coerce_agent_providers(cls, v: object) -> object:
        """Fail-safe to an empty map on a non-mapping value.

        A scalar or list here is operator error; degrading to {} keeps the
        rest of the settings load succeeding and leaves cross-model OFF (no
        map = no opt-in signal), rather than raising out of the whole load.
        Values are NOT validated here (an unknown provider literal is handled
        at resolution time with a warn+claude fallback, per Failure Modes).
        """
        if v is None:
            return {}
        if isinstance(v, dict):
            return v
        _LOG.warning(
            "settings.yaml: config.review.agent_providers is not a mapping "
            "(%r); ignoring it (cross-model stays off)",
            v,
        )
        return {}

    @field_validator("cross_model", mode="before")
    @classmethod
    def coerce_cross_model(cls, v: object) -> object:
        """Fail-safe: a non-mapping `cross_model:` degrades to defaults (OFF).

        Mirrors ConfigBlock._coerce_auto_continue: `cross_model: 42` (or a
        list, or null) cannot build the block; fall back to the default
        disabled block rather than raising out of the settings load. A dict
        passes through so the inner `enabled` coercer still runs.
        """
        if isinstance(v, (dict, CrossModelBlock)):
            return v
        return {}


class HandoffBlock(BaseModel):
    """Self-handoff settings (nested under 'config.target.handoff').

    Controls the sanctioned session-succession protocol (ab-534bcc55) that lets
    a /target session hand the rest of its pipeline to a fresh-context successor
    at pipeline boundaries instead of carrying earlier-phase context baggage.

    Locked Decisions 6-8 (plan sec "Locked Decisions"):
      6. Boundary-agnostic primitive, staged wiring; generation cap 4.
      7. Transcript-derived context probe is the only pressure source; probe
         failure = no handoff (fail-safe).
      8. used_pct_trigger default 50, boundary-only evaluation.

    Shell consumer: skills/target/scripts/handoff.sh reads these via
    get_config "target.handoff.*" (GENERATION_CAP, USED_PCT_TRIGGER,
    HANDOFF_ENABLED) with defaults matching these values exactly. Keep both
    in sync when changing defaults.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = True
    used_pct_trigger: int = 50
    generation_cap: int = 4

    @field_validator("used_pct_trigger")
    @classmethod
    def used_pct_trigger_range(cls, v: int) -> int:
        """Percentage must be 1-100; 0 would never trigger, >100 is nonsensical."""
        if not (1 <= v <= 100):
            raise ValueError(
                "config.target.handoff.used_pct_trigger must be in range 1-100; "
                f"got {v}"
            )
        return v

    @field_validator("generation_cap")
    @classmethod
    def generation_cap_positive(cls, v: int) -> int:
        """A cap below 1 would immediately refuse every handoff attempt."""
        if v < 1:
            raise ValueError(
                "config.target.handoff.generation_cap must be >= 1; "
                f"got {v}"
            )
        return v


class BlastConfig(BaseModel):
    """Blast-radius router settings (nested under 'config.target.blast', x-518f).

    A deterministic blast read at `/target` init modulates the size profile
    BEFORE the immutable manifest is written: a high-blast surface can only
    raise ceremony (a non-overridable floor at `M`), and low-blast work is
    downgraded to the fast path only when the operator did not pin a size.

    Default OFF (footnote convention): disabled is byte-for-byte today's
    behavior. A malformed block fails safe to disabled, mirroring
    `config.auto_merge` / `config.auto_continue`; a single bad glob in
    `high_blast_globs` is skipped, not raised (the classifier owns that).

    Knobs:
      enabled          - whole-feature opt-in.
      downgrade        - when False, only the high-blast floor applies
                         (safety-only mode; no token-saving downgrades).
      reuse_loc_manifest - include the loc-ratchet control-plane globs
                         (scripts/ci/loc-ratchet-manifest.yaml) in the map.
      high_blast_globs - per-project extension of the general default list.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False
    downgrade: bool = True
    reuse_loc_manifest: bool = True
    high_blast_globs: list[str] = Field(default_factory=list)


class TargetDefaultsBlock(BaseModel):
    """Session-input defaults (nested under 'config.target.defaults').

    These are NOT pipeline config: they are the default *values* a /target
    session starts with when the CLI flag / env override is absent. They are
    enumerated here so they appear in the generated docs as "session-input
    defaults", but the runtime values are still resolved from the size profile
    and TARGET_NO_* env overrides at session init - this block only documents
    the fallbacks. `budget_cap` was intentionally NOT modeled (Locked Decision
    10): cost accounting is not yet a dependable control.
    """

    model_config = ConfigDict(extra="ignore")

    no_external: bool = False
    no_docs: bool = False
    max_iterations: int = 40

    @field_validator("max_iterations", mode="before")
    @classmethod
    def max_iterations_positive(cls, v: object) -> object:
        """Degrade a non-positive / malformed ceiling to the default 40.

        This is a session-input default, not a hard constraint, and the block's
        theme is graceful degradation (gemini review): a bad value warns and
        falls back rather than breaking the whole settings load.
        """
        ok = False
        if isinstance(v, (int, str)) and not isinstance(v, bool):
            try:
                ok = int(v) >= 1
            except (TypeError, ValueError):
                ok = False
        if not ok:
            _LOG.warning(
                "config.target.defaults.max_iterations=%r invalid; using default 40",
                v,
            )
            return 40
        return v


class TargetConfig(BaseModel):
    """Target pipeline settings (nested under 'config.target').

    `dedupe_dead_duplicates` (ab-f6625d1c, Component B) gates the stop hook's
    opt-in cleanup of provably-dead duplicate sibling target-state.md files.
    When two worktrees of one repo carry an IN_PROGRESS state bound to the same
    claude_transcript_id, enabling this lets the stop hook rename the
    provably-dead sibling to `*.superseded` (recoverable) after a transcript-id
    backfill - but only when it is not the live worktree, its owner_pid is dead,
    and it is older than the current state. Default OFF: the non-destructive
    resolution-time tiebreak (Component A) alone fixes the p1 false-orphan; this
    is an incremental hygiene layer.
    """

    model_config = ConfigDict(extra="ignore")

    dedupe_dead_duplicates: bool = False
    # Whether a node reaching `ready` (via /blueprint) auto-launches a bg /target
    # worker. Read by skills/blueprint/scripts/autolaunch-on-ready.sh. Default OFF.
    auto_launch_on_blueprint: bool = False
    handoff: HandoffBlock = Field(default_factory=HandoffBlock)
    blast: BlastConfig = Field(default_factory=BlastConfig)
    defaults: TargetDefaultsBlock = Field(default_factory=TargetDefaultsBlock)

    @field_validator("blast", mode="before")
    @classmethod
    def coerce_blast(cls, v: object) -> object:
        """Fail-safe: a non-mapping `config.target.blast` degrades to defaults (OFF).

        Mirrors ConfigBlock._coerce_cross_model so a scalar typo like
        `blast: true` cannot raise out of `load_settings`/`SettingsModel`
        construction (the disabled-by-default promise must hold for every
        consumer, not only the local `_load_blast_cfg` catch). A dict passes
        through so BlastConfig's own field coercion still runs.
        """
        if isinstance(v, (dict, BlastConfig)):
            return v
        return {}


class EvalsBlock(BaseModel):
    """Evals-runner settings (nested under 'config.evals').

    `staleness_days` controls when `fno evals report` prints a staleness
    warning.  When the newest history row is older than this many days,
    report prints an explicit warning so an operator knows the suite has
    not run recently.  Default 14 days (Open Question 2 from the design
    doc); set to a lower value in CI or a higher value for infrequent
    manual sweeps.
    """

    model_config = ConfigDict(extra="ignore")

    staleness_days: int = 14

    @field_validator("staleness_days")
    @classmethod
    def staleness_days_positive(cls, v: int) -> int:
        """A staleness window of 0 or less is nonsensical."""
        if v < 1:
            raise ValueError(
                "config.evals.staleness_days must be >= 1; "
                f"got {v}"
            )
        return v


class A2aBlock(BaseModel):
    """Agent-to-agent switchboard settings (nested under 'config.agents.a2a').

    Governs the stream-json session-to-session switchboard (epic ab-d3a1ae3e,
    Group 2). `auto` is the A2A toggle: when true (the default), a switchboard
    `send A->B` runs the bounded literal-injection relay (B's reply becomes a
    user turn in A, A's reply relays back to B, ...) up to `turn_ceiling` total
    turns, then stops with a visible "loop ceiling reached". When false it falls
    back to OBSERVED mode: B is driven once and its reply is mirrored into A's
    view, with no autonomous relay.

    `turn_ceiling` is a HARD correctness bound (a runaway A<->B exchange burns
    plan credit); it applies regardless of `auto` and must be >= 1.
    """

    model_config = ConfigDict(extra="ignore")

    auto: bool = True
    turn_ceiling: int = 6

    @field_validator("turn_ceiling")
    @classmethod
    def ceiling_is_positive(cls, v: int) -> int:
        """A non-positive ceiling would disable the safety bound; reject it."""
        if v < 1:
            raise ValueError(
                f"config.agents.a2a.turn_ceiling must be >= 1; got {v}"
            )
        return v


class AgentProviderBlock(BaseModel):
    """Per-provider agent-runtime settings (nested under 'config.agents.<provider>').

    `headless_yolo` (ab-994222ee, redefined by the bounded-posture amendment)
    selects FULL yolo vs the BOUNDED posture for an autonomous (headless,
    MODE==exec) codex/gemini worker. Both postures never prompt (no hang); they
    differ on the sandbox:

      - False (default) -> BOUNDED: sandboxed AND never-prompt. codex
        `--sandbox workspace-write --ask-for-approval never`; gemini
        `--approval-mode yolo --sandbox`. No hang, and the worker cannot roam
        outside the workspace.
      - True -> FULL YOLO: unsandboxed bypass. codex
        `--dangerously-bypass-approvals-and-sandbox`; gemini bare `--yolo`. The
        explicit opt-in for an operator who genuinely wants no sandbox.

    The default is False (bounded) because it kills the headless hang WITHOUT
    dropping the sandbox - strictly safer than the full bypass. A malformed
    block fails safe to bounded. Only the autonomous exec lane consults it; an
    interactive `host`/`drive` launch and claude (yolo is a no-op) are never
    affected. (The old "sandboxed-but-prompting" meaning of `false` is removed;
    it was strictly worse than bounded - sandboxed AND hangs.)
    """

    model_config = ConfigDict(extra="ignore")

    headless_yolo: bool = False


class AgentsBlock(BaseModel):
    """Agent-runtime settings (nested under 'config.agents').

    `confirm` drives the /fno:agent spawn-verb confirm gate
    (ab-27541df5; namespace moved from `config.dispatch.confirm` to
    `config.agents.confirm` in ab-f1b0ccd1). It selects when the
    billed-launch confirm prompt is shown:

      - always: confirm every billed build (the pre-amendment behavior)
      - auto:   skip the confirm only for a resolved node-id build on a
                caveat-free lane; confirm free-form features, codex/gemini
                exec builds, --yolo, and merge grants
      - never:  skip the confirm; any caveats print as warnings alongside
                the receipt

    Model default is "auto" so a skill reading `config.agents.confirm`
    resolves even when settings.yaml has no agents block. An invalid value
    raises a ValidationError (the read fails); the skill's read path treats
    any failed read as "always" - less capability never means less safety
    (degrade toward the confirm, never toward a silent launch).
    """

    model_config = ConfigDict(extra="ignore")

    a2a: A2aBlock = Field(default_factory=A2aBlock)
    confirm: str = "auto"
    codex: AgentProviderBlock = Field(default_factory=AgentProviderBlock)
    gemini: AgentProviderBlock = Field(default_factory=AgentProviderBlock)

    @field_validator("confirm")
    @classmethod
    def confirm_is_known(cls, v: str) -> str:
        """A typo must fail the read, never silently behave as `never`."""
        allowed = {"always", "auto", "never"}
        if v not in allowed:
            raise ValueError(
                "config.agents.confirm must be one of always|auto|never; "
                f"got {v!r}"
            )
        return v

    @field_validator("codex", "gemini", mode="before")
    @classmethod
    def _coerce_provider_block(cls, v: object) -> object:
        """Fail-safe: a non-mapping provider block degrades to defaults.

        ``config.agents.gemini: banana`` (a scalar, list, or null) cannot
        build the block; rather than raise out of the whole settings load,
        fall back to the default (headless_yolo=True, hang-safe). A mapping
        passes through so an explicit ``headless_yolo: false`` opt-out still
        takes effect. Mirrors ``ConfigBlock._coerce_auto_continue``.
        """
        if isinstance(v, (dict, AgentProviderBlock)):
            return v
        return {}


class AutoContinueBlock(BaseModel):
    """Merge-triggered auto-continue settings (nested under 'config.auto_continue').

    The opt-in for merge-triggered auto-continue (node ab-3cd195b6): when
    enabled, a merge-detector (``fno backlog reconcile`` / the /pr merged skill)
    dispatches a fresh background ``/target no-merge`` worker for the next
    now-unblocked backlog node after a PR merges, so a merge-gated epic walks
    itself group-by-group with no manual re-invocation.

    Default ``False`` (Locked Decision 3): shipping this changes nothing until
    the operator arms it. Precedence mirrors ``config.auto_merge``: a malformed
    block degrades to disabled (fail-safe) rather than failing the settings
    load, so one operator typo can never break every CLI invocation. The
    enable-resolution chain (env override > campaign-arm marker > this block)
    lives in :func:`fno.backlog.advance.auto_continue_enabled`.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False

    @field_validator("enabled", mode="before")
    @classmethod
    def _coerce_enabled(cls, v: object) -> bool:
        """Fail-safe to disabled on any non-boolean value (AC2-ERR).

        A scalar typo (``enabled: banana``) is operator error; coercing it to
        False here keeps load_settings() succeeding for every OTHER consumer
        rather than raising a ValidationError that breaks the whole load.

        Deliberately STRICT for a safety opt-in: only a clear affirmative
        (``true``/``yes``/``on``/``1``) enables. An ambiguous value (``2``, a
        list, a mapping) fails safe to disabled rather than guessing on, because
        false-enabled (a background worker dispatched against the operator's
        intent) is the dangerous direction here, not false-disabled.
        """
        if isinstance(v, bool):
            return v
        if isinstance(v, int):  # bool already handled above
            return v == 1
        if isinstance(v, str):
            return v.strip().lower() in {"1", "true", "yes", "on"}
        return False


def _coerce_affirmative(v: object, default: bool) -> bool:
    """Map a settings value to a bool with the bash get_config truth table.

    The bash auto_merge helpers read each flag with ``get_config <key> <dflt>``
    and then test ``[[ "$value" == "true" ]]``. So an ABSENT key takes the
    field default, but a PRESENT non-affirmative value behaves as false. This
    helper is the ``before`` coercer half: it only runs when the key is present,
    so it returns True solely on a clear affirmative and False otherwise,
    matching ``== "true"`` for every present value. The field's own default
    (passed as ``default`` here only for documentation symmetry) covers the
    absent case via pydantic, where the validator never fires.
    """
    if isinstance(v, bool):
        return v
    if isinstance(v, int):  # bool already handled above
        return v == 1
    if isinstance(v, str):
        return v.strip().lower() in {"1", "true", "yes", "on"}
    return False


class AutoMergeBlock(BaseModel):
    """Auto-merge settings (nested under 'config.auto_merge').

    The typed reader for what the bash ``scripts/lib/config.sh`` auto_merge
    helpers used to parse (``is_auto_merge_allowed_for`` / ``get_auto_merge_*``).
    The ``fno pr`` port (ab-d4c98550) reads these via :func:`load_settings`
    instead of re-parsing settings.yaml in a subprocess, so the 4-tier
    precedence + caching live in one place.

    Validation mirrors the bash exactly: an invalid ``merge_strategy`` falls
    back to ``merge``, an invalid ``conflict_resolution`` to ``opus``, an
    invalid ``remediation`` to ``attempt`` (the bash printed a warning and used
    the same fallback). A malformed block degrades to defaults (auto-merge OFF)
    rather than failing the whole settings load - false-enabled is the dangerous
    direction for a merge opt-in.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False
    merge_strategy: str = "merge"
    delete_branch_on_merge: bool = True
    require_checks_pass: bool = True
    conflict_resolution: str = "opus"
    allowed_invokers: list[str] = Field(default_factory=list)
    remediation: str = "attempt"

    @field_validator("enabled", mode="before")
    @classmethod
    def _coerce_enabled(cls, v: object) -> bool:
        return _coerce_affirmative(v, default=False)

    @field_validator("delete_branch_on_merge", "require_checks_pass", mode="before")
    @classmethod
    def _coerce_flag(cls, v: object) -> bool:
        return _coerce_affirmative(v, default=True)

    @field_validator("merge_strategy", mode="before")
    @classmethod
    def _coerce_strategy(cls, v: object) -> str:
        if isinstance(v, str) and v.strip() in {"merge", "squash", "rebase"}:
            return v.strip()
        return "merge"

    @field_validator("conflict_resolution", mode="before")
    @classmethod
    def _coerce_conflict_resolution(cls, v: object) -> str:
        if isinstance(v, str) and v.strip() in {"opus", "fail"}:
            return v.strip()
        return "opus"

    @field_validator("remediation", mode="before")
    @classmethod
    def _coerce_remediation(cls, v: object) -> str:
        if isinstance(v, str) and v.strip() in {"attempt", "verify_only"}:
            return v.strip()
        return "attempt"

    @field_validator("allowed_invokers", mode="before")
    @classmethod
    def _coerce_allowed_invokers(cls, v: object) -> object:
        """Accept a YAML list or a bare string; degrade junk to [] (all allowed).

        An empty list means "no restriction" (the bash treated an empty
        allowed_invokers as all-allowed). A single bare string is wrapped to a
        one-element list so ``allowed_invokers: target`` resolves sanely.
        """
        if v is None:
            return []
        if isinstance(v, str):
            s = v.strip()
            return [s] if s else []
        if isinstance(v, (list, tuple)):
            return [str(x).strip() for x in v if str(x).strip()]
        return []

    def is_allowed_for(self, invoker: str) -> bool:
        """Port of ``is_auto_merge_allowed_for``: enabled AND invoker permitted.

        Disabled -> never allowed. Enabled with an empty allowed_invokers ->
        all invokers allowed. Enabled with a non-empty list -> membership.
        """
        if not self.enabled:
            return False
        if not self.allowed_invokers:
            return True
        return invoker in self.allowed_invokers


class PrWatchBlock(BaseModel):
    """PR-state watcher settings (nested under 'config.pr_watch').

    Controls the global launchd watcher that polls open-PR backlog nodes
    and fires headless /fno:pr check / /fno:pr merged.

    Fields
    ------
    enabled:
        True to activate the watcher (default False; operator opt-in).
    interval_seconds:
        ``StartInterval`` for the LaunchAgent plist (default 600 = 10 min).
    retries:
        Maximum consecutive dispatch failures before a PR is parked
        (default 3).
    max_age_days:
        PRs older than this many days are parked without dispatch (default 14).
    model:
        The claude model used for headless skill fires (default haiku-4-5;
        cheap mechanical task).
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False
    interval_seconds: int = Field(default=600, gt=0)
    retries: int = Field(default=3, ge=1)
    max_age_days: int = Field(default=14, ge=1)
    model: str = Field(default="claude-haiku-4-5", min_length=1)


class LogsBlock(BaseModel):
    """Append-log rotation caps (nested under 'config.logs').

    ``convo_signals_max_mb`` bounds ``.fno/convo-signals.jsonl`` - the largest
    unbounded append log in ``.fno/``. It is read defensively by
    ``hooks/convo-signal-capture.sh`` (which caps the project + global signals
    logs after each append; an absent or malformed value degrades to the
    default there, never raising). This schema entry exists so the knob is
    discoverable and ``fno config set logs.convo_signals_max_mb <N>`` validates
    instead of rejecting an 'unknown key'.
    """

    model_config = ConfigDict(extra="ignore")

    convo_signals_max_mb: int = 5

    @field_validator("convo_signals_max_mb")
    @classmethod
    def convo_signals_max_mb_positive(cls, v: int) -> int:
        """A rotation cap below 1 MB would truncate the log on every append."""
        if v < 1:
            raise ValueError("config.logs.convo_signals_max_mb must be >= 1")
        return v


class HealthThresholdsBlock(BaseModel):
    """Backlog-health breach thresholds (config.health_monitor.thresholds).

    Defaults lifted verbatim from the former
    ``health_monitor.py:DEFAULT_CONFIG["thresholds"]`` (US2 converges that
    private loader onto this block).
    """

    model_config = ConfigDict(extra="ignore")

    idea_pile_depth: int = 25
    stale_ready_days: int = 30
    failure_prone_attempts: int = 2
    collision_count: int = 3
    project_cwd_mismatch: int = 0

    @model_validator(mode="before")
    @classmethod
    def _sanitize(cls, v: object) -> object:
        """Drop any non-integer or negative value so the field default applies.

        A malformed threshold must never break load_settings(); it degrades to
        the modeled default for that key (with a WARNING), matching the old
        health_monitor per-key fallback.
        """
        if not isinstance(v, dict):
            return v
        out = dict(v)
        for key in (
            "idea_pile_depth",
            "stale_ready_days",
            "failure_prone_attempts",
            "collision_count",
            "project_cwd_mismatch",
        ):
            if key not in out:
                continue
            raw_val = out[key]
            if isinstance(raw_val, bool):
                ok = False
            else:
                try:
                    num = int(raw_val)
                    ok = num >= 0
                except (TypeError, ValueError):
                    ok = False
            if not ok:
                _LOG.warning(
                    "config.health_monitor.thresholds.%s=%r invalid; using default",
                    key,
                    raw_val,
                )
                out.pop(key)
        return out


class HealthNotificationsBlock(BaseModel):
    """Backlog-health notification routing (config.health_monitor.notifications)."""

    model_config = ConfigDict(extra="ignore")

    surfaces: list[str] = Field(default_factory=lambda: ["terminal"])
    discord_channel: Optional[str] = None
    webhook_url: Optional[str] = None
    throttle_minutes: int = 60


class HealthHistoryBlock(BaseModel):
    """Backlog-health history retention (config.health_monitor.history).

    ``path`` defaults to None and is resolved at runtime via paths.state_dir().
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = True
    path: Optional[str] = None
    retain_days: int = 90


class HealthMonitorBlock(BaseModel):
    """Backlog health monitoring (config.health_monitor).

    Promoted from the private ``health_monitor.py:DEFAULT_CONFIG`` so the model
    is the single source of truth (US1) and ``health_monitor.py`` can read
    ``load_settings().config.health_monitor`` instead of re-loading the YAML (US2).
    Sub-blocks use ``extra="ignore"`` + fail-safe coercers so a malformed value
    degrades to defaults rather than breaking ``load_settings()`` (Failure Modes).
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = True
    thresholds: HealthThresholdsBlock = Field(default_factory=HealthThresholdsBlock)
    notifications: HealthNotificationsBlock = Field(
        default_factory=HealthNotificationsBlock
    )
    history: HealthHistoryBlock = Field(default_factory=HealthHistoryBlock)

    @field_validator("thresholds", "notifications", "history", mode="before")
    @classmethod
    def coerce_subblock(cls, v: object) -> object:
        """Fail-safe: a non-mapping sub-block degrades to its defaults.

        A scalar typo (``thresholds: 3``) must not raise out of load_settings();
        an empty/non-dict value falls back to the default sub-block.
        """
        if isinstance(v, dict):
            return v
        return {}


class CollisionThresholdsBlock(BaseModel):
    """Collision severity scoring thresholds (config.collision.severity_thresholds).

    Defaults lifted verbatim from the former
    ``collision.py:DEFAULT_THRESHOLDS`` (US2 converges that private loader here).
    """

    model_config = ConfigDict(extra="ignore")

    high_count: float = 3
    high_ratio: float = 0.5
    medium_count: float = 2
    medium_ratio: float = 0.25

    @model_validator(mode="before")
    @classmethod
    def _sanitize(cls, v: object) -> object:
        """Drop any non-numeric or negative value so the field default applies.

        Matches the old collision._load_thresholds per-key fallback: a bad
        threshold warns and falls back to default rather than breaking load.
        """
        if not isinstance(v, dict):
            return v
        out = dict(v)
        for key in ("high_count", "high_ratio", "medium_count", "medium_ratio"):
            if key not in out:
                continue
            raw_val = out[key]
            if isinstance(raw_val, bool):
                ok = False
            else:
                try:
                    num = float(raw_val)
                    ok = num >= 0
                except (TypeError, ValueError):
                    ok = False
            if not ok:
                _LOG.warning(
                    "config.collision.severity_thresholds.%s=%r is not numeric "
                    "or is negative; using default",
                    key,
                    raw_val,
                )
                out.pop(key)
            else:
                out[key] = num
        return out


class CollisionBlock(BaseModel):
    """File-collision detection settings (config.collision)."""

    model_config = ConfigDict(extra="ignore")

    severity_thresholds: CollisionThresholdsBlock = Field(
        default_factory=CollisionThresholdsBlock
    )

    @field_validator("severity_thresholds", mode="before")
    @classmethod
    def coerce_severity_thresholds(cls, v: object) -> object:
        """Fail-safe: a non-mapping value degrades to default thresholds."""
        if isinstance(v, dict):
            return v
        return {}


class WorkspaceProjectEntry(BaseModel):
    """One project within a workspace (work.workspaces.<slug>.projects[]).

    Field names match what the bash ``get_workspace`` reader and the
    cross-project pipeline expect; ``extra="ignore"`` keeps any extra per-project
    keys a user added.
    """

    model_config = ConfigDict(extra="ignore")

    name: Optional[str] = None
    path: Optional[str] = None
    type: Optional[str] = None
    stack: list[str] = Field(default_factory=list)
    package_manager: Optional[str] = None


class WorkspaceEntry(BaseModel):
    """One named workspace (work.workspaces.<slug>)."""

    model_config = ConfigDict(extra="ignore")

    projects: list[WorkspaceProjectEntry] = Field(default_factory=list)


class WorkBlock(BaseModel):
    """Workspace / project topology map (TOP-LEVEL ``work:`` in settings.yaml).

    Stored at the top level (NOT under ``config:``), matching the live file and
    the bash ``get_workspace`` reader (``scripts/lib/config.sh`` reads
    ``.work.workspaces.*``). Load-bearing for project/cwd resolution in
    ``fno backlog maintain`` / ``health``. The plan's "config.work" phrasing was
    corrected to top-level to match ground truth and keep promotion non-breaking.
    """

    model_config = ConfigDict(extra="ignore")

    workspaces: dict[str, WorkspaceEntry] = Field(default_factory=dict)

    @field_validator("workspaces", mode="before")
    @classmethod
    def coerce_workspaces(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``work.workspaces`` degrades to {}."""
        if isinstance(v, dict):
            return v
        return {}


class ModelProvider(BaseModel):
    """One secondary model provider for role-based routing (z.ai, DeepSeek, ...).

    ``protocol`` is how a worker talks to it: a ``claude --bg`` worker speaks the
    Anthropic Messages API, so only ``anthropic``-protocol providers are usable
    for the claude lane (use the vendor's Anthropic-compatible endpoint, e.g.
    ``https://api.z.ai/api/anthropic`` or ``https://api.deepseek.com/anthropic``,
    NOT its OpenAI ``/v4`` path). The API key is read from the process env var
    named by ``api_key_env`` (falling back to ``api_key_file``); it never lives
    in settings.yaml. ``zai`` is built in by default; list a provider here to
    override it or to add another (e.g. ``deepseek``).
    """

    model_config = ConfigDict(extra="ignore")

    protocol: str = "anthropic"
    base_url: str = ""
    api_key_env: str = ""
    api_key_file: Optional[str] = None


class ModelRoutingBlock(BaseModel):
    """Role-based per-spawn model routing (config.model_routing in settings.yaml).

    Routes auxiliary coordination roles (coordinate / tidy / orient /
    consolidate) to a secondary provider (z.ai GLM by default) at spawn time
    while production roles stay on the primary Anthropic model. Keys live in env
    vars / .env files named per provider, never here. See
    fno.agents.model_routing.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = True
    providers: dict[str, ModelProvider] = Field(default_factory=dict)
    roles: dict[str, str] = Field(default_factory=dict)
    extra_env: dict[str, str] = Field(default_factory=dict)

    @field_validator("providers", "roles", "extra_env", mode="before")
    @classmethod
    def _coerce_mapping(cls, v: object) -> object:
        """Fail-safe: a non-mapping providers/roles/extra_env degrades to {}."""
        if isinstance(v, dict):
            return v
        return {}


class ConfigBlock(BaseModel):
    """Top-level config block (nested under 'config:' in settings.yaml)."""

    model_config = ConfigDict(extra="ignore")

    state_dir: str = "~/.fno/"
    plans_dir: str = ".fno/plans/"
    paths: PathsBlock = Field(default_factory=PathsBlock)
    obsidian: ObsidianBlock = Field(default_factory=ObsidianBlock)
    project: ProjectBlock = Field(default_factory=ProjectBlock)
    blueprint: BlueprintBlock = Field(default_factory=BlueprintBlock)
    backlog: BacklogBlock = Field(default_factory=BacklogBlock)
    post_merge: PostMergeBlock = Field(default_factory=PostMergeBlock)
    review: ReviewBlock = Field(default_factory=ReviewBlock)
    target: TargetConfig = Field(default_factory=TargetConfig)
    evals: EvalsBlock = Field(default_factory=EvalsBlock)
    agents: AgentsBlock = Field(default_factory=AgentsBlock)
    auto_continue: AutoContinueBlock = Field(default_factory=AutoContinueBlock)
    auto_merge: AutoMergeBlock = Field(default_factory=AutoMergeBlock)
    logs: LogsBlock = Field(default_factory=LogsBlock)
    pr_watch: PrWatchBlock = Field(default_factory=PrWatchBlock)
    health_monitor: HealthMonitorBlock = Field(default_factory=HealthMonitorBlock)
    collision: CollisionBlock = Field(default_factory=CollisionBlock)
    work: WorkBlock = Field(default_factory=WorkBlock)
    model_routing: ModelRoutingBlock = Field(default_factory=ModelRoutingBlock)

    @field_validator("model_routing", mode="before")
    @classmethod
    def _coerce_model_routing(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``model_routing:`` degrades to defaults.

        Mirrors ``_coerce_auto_merge``: a scalar/list/null cannot build the
        block; fall back to defaults rather than raising out of the whole
        settings load. A dict passes through so the inner coercers still run.
        """
        if isinstance(v, (dict, ModelRoutingBlock)):
            return v
        return {}

    @field_validator("logs", mode="before")
    @classmethod
    def _coerce_logs(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``logs:`` degrades to defaults.

        Mirrors ``_coerce_auto_merge``: ``logs: 42`` (or a list, or null)
        cannot build the block; fall back to defaults rather than raising out
        of the whole settings load. A dict passes through so the inner field
        validator still runs.
        """
        if isinstance(v, (dict, LogsBlock)):
            return v
        return {}

    @field_validator("pr_watch", mode="before")
    @classmethod
    def _coerce_pr_watch(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``pr_watch:`` degrades to defaults (disabled).

        Mirrors ``_coerce_auto_merge``: ``pr_watch: 42`` (or a list, or null)
        cannot build the block; fall back to the default disabled block rather
        than raising out of the whole settings load. A dict passes through so
        the inner field coercers still run.
        """
        if isinstance(v, (dict, PrWatchBlock)):
            return v
        return {}

    @field_validator("health_monitor", mode="before")
    @classmethod
    def _coerce_health_monitor(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``health_monitor:`` degrades to defaults."""
        if isinstance(v, (dict, HealthMonitorBlock)):
            return v
        return {}

    @field_validator("collision", mode="before")
    @classmethod
    def _coerce_collision(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``collision:`` degrades to defaults."""
        if isinstance(v, (dict, CollisionBlock)):
            return v
        return {}

    @field_validator("work", mode="before")
    @classmethod
    def _coerce_work(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``work:`` degrades to an empty workspace map."""
        if isinstance(v, (dict, WorkBlock)):
            return v
        return {}

    @field_validator("auto_merge", mode="before")
    @classmethod
    def _coerce_auto_merge(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``auto_merge:`` degrades to defaults (OFF).

        Mirrors ``_coerce_auto_continue``: ``auto_merge: 42`` (or a list, or
        null) cannot build the block; fall back to the default disabled block
        rather than raising out of the whole settings load. A dict passes
        through so the inner field coercers still run.
        """
        if isinstance(v, (dict, AutoMergeBlock)):
            return v
        return {}

    @field_validator("auto_continue", mode="before")
    @classmethod
    def _coerce_auto_continue(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``auto_continue:`` degrades to defaults.

        ``auto_continue: 42`` (or a list, or null) cannot build the block;
        rather than raise out of the whole settings load, fall back to the
        default disabled block (AC2-ERR). A dict passes through so the inner
        ``enabled`` coercer still runs.
        """
        if isinstance(v, (dict, AutoContinueBlock)):
            return v
        return {}

    @field_validator("state_dir", "plans_dir", mode="before")
    @classmethod
    def validate_path_fields(cls, v: object) -> object:
        if isinstance(v, str):
            _check_no_glob(v, "path field")
            _check_path_max(v, "path field")
        return v

    @model_validator(mode="after")
    def validate_template_references(self) -> "ConfigBlock":
        """Reject {vault} when obsidian is disabled; flag {project} early."""
        path_values: list[tuple[str, str]] = [
            ("config.state_dir", self.state_dir),
            ("config.plans_dir", self.plans_dir),
        ]
        # Add paths overrides
        for field_name in PathsBlock.model_fields:
            val = getattr(self.paths, field_name)
            if val is not None:
                path_values.append((f"config.paths.{field_name}", val))

        for location, value in path_values:
            # Parse template variables using the same lookbehind/lookahead pattern
            # as paths._TEMPLATE_VAR so {{ escape sequences are not matched.
            import re

            for match in re.finditer(r"(?<!\{)\{([^{}]+)\}(?!\})", value):
                var = match.group(1)
                if var == "vault":
                    if not self.obsidian.enabled:
                        raise ValueError(
                            f"{location!r} uses {{vault}} but obsidian.enabled is false. "
                            "Either set obsidian.enabled: true or rewrite the path."
                        )
                elif var == "project":
                    # Deferred validation: only error if BOTH conditions hold:
                    # not in a git repo AND no project.id. We check project.id here;
                    # the git repo check happens in _resolve() at call time.
                    pass  # validated lazily in _resolve()

        return self


class SettingsModel(BaseModel):
    """Root settings model. Mirrors the shape of settings.yaml."""

    model_config = ConfigDict(extra="ignore")

    # schema_version is the ONLY top-level key (it versions the file format,
    # not behavior). Every setting lives under `config:` - including `work` and
    # `project`, which were top-level historically. The loader aliases legacy
    # top-level `work:` / `project:` into `config.work` / `config.project`
    # (see _alias_legacy_keys), so existing files keep working with no migration.
    schema_version: int = 1
    config: ConfigBlock = Field(default_factory=ConfigBlock)


# ---------------------------------------------------------------------------
# Loader
# ---------------------------------------------------------------------------


def _global_settings_path() -> Path:
    """Resolve the per-user global settings.yaml path.

    Returns ``Path(FNO_GLOBAL_SETTINGS_PATH)`` when that environment variable
    is set to a non-empty value (use ``/dev/null`` to disable the global
    candidate in test isolation), otherwise the default
    ``~/.fno/settings.yaml``.

    This hook exists so unit tests pinning ``repo_root=tmp_path`` cannot leak
    the developer's real ``~/.fno/settings.yaml`` into assertions that
    expect an empty global config.

    Empty-string env var (e.g. ``FNO_GLOBAL_SETTINGS_PATH=``) is treated as
    "unset" rather than ``Path("")`` (which resolves to the CWD and would
    silently bypass the global config). An operator that genuinely wants to
    point at the CWD must say so explicitly: ``FNO_GLOBAL_SETTINGS_PATH=.``.
    """
    env = os.environ.get("FNO_GLOBAL_SETTINGS_PATH")
    if env:
        return Path(env)
    return Path.home() / ".fno" / "settings.yaml"


def _candidate_paths() -> list[Path]:
    """Return the ordered list of settings file candidates.

    Order:
      1. $FNO_CONFIG env var (explicit path, short-circuits)
      2. <worktree_root>/.fno/settings.yaml (project-local to this
         checkout, anchored to the repo root via git toplevel / FNO_REPO_ROOT,
         not cwd, so running `fno` from a subdirectory still finds it - and a
         worktree-local override still wins).
      3. <canonical_root>/.fno/settings.yaml (the main checkout's config,
         resolved via the main worktree from `git worktree list`). From a
         linked worktree, .fno/
         is a per-worktree real dir that may not carry settings.yaml, so without
         this a worktree fell straight through to global config and reported
         project-local keys (e.g. config.post_merge.parking_lot_path) empty. Deduped
         when canonical == worktree (i.e. running from the main checkout).
         Config climbs to canonical on purpose; session state does not - see
         fno.paths.resolve_canonical_repo_root.
      4. ~/.fno/settings.yaml (per-user global; honors
         $FNO_GLOBAL_SETTINGS_PATH override for test isolation)
    """
    env_path = os.environ.get("FNO_CONFIG")
    if env_path:
        return [Path(env_path)]

    # Resolve project-local candidates from the repo root, not cwd.
    # Lazy import to avoid circular dependency (paths imports config).
    try:
        from fno.paths import resolve_canonical_repo_root, resolve_repo_root
        repo_root = resolve_repo_root()
        canonical_root = resolve_canonical_repo_root()
    except (ImportError, ModuleNotFoundError):
        repo_root = Path.cwd()
        canonical_root = repo_root

    candidates = [repo_root / ".fno" / "settings.yaml"]
    canonical_candidate = canonical_root / ".fno" / "settings.yaml"
    if canonical_candidate not in candidates:
        candidates.append(canonical_candidate)
    candidates.append(_global_settings_path())
    return candidates


# Module-level variable recording the path that load_settings() actually read.
# None until the first successful load_settings() call.
# Exposed via loaded_from() so paths.config_file() can return the actual
# path without re-deriving from state_dir (Finding 3 fix).
_loaded_from: Optional[Path] = None


def loaded_from() -> Optional[Path]:
    """Return the Path that load_settings() actually read, or None if not loaded yet."""
    return _loaded_from


def _load_raw(path: Path) -> tuple[dict[str, object], bool]:
    """Load a YAML file and return (data, parse_succeeded).

    Returns ({}, False) on any OS or YAML error so callers can fall through
    to the next candidate. Logs a WARNING on parse failure so the user knows
    their settings.yaml was not applied.

    Returns (data, True) when the file parsed successfully (even if the dict
    is empty, i.e. the file was blank or contained only ``null``).
    """
    try:
        data = yaml.safe_load(path.read_text(encoding="utf-8"))
        return (data if isinstance(data, dict) else {}, True)
    except (OSError, yaml.YAMLError) as exc:
        _LOG.warning(
            "settings.yaml at %s failed to parse: %s; using defaults",
            path,
            exc,
        )
        return ({}, False)


def _warn_unknown_keys(data: dict[str, object], model: type[BaseModel], prefix: str = "") -> None:
    """Emit a DEBUG-level WARNING for keys not in the model's field set.

    Only emits when the FNO_DEBUG environment variable is set (any non-empty
    value).  This keeps the default UX quiet while still letting power users
    see the detail with ``FNO_DEBUG=1 fno ...``.
    """
    if not os.environ.get("FNO_DEBUG"):
        return
    known = set(model.model_fields.keys())
    for key in data:
        qualified = f"{prefix}.{key}" if prefix else key
        if key not in known:
            _LOG.warning(
                "settings.yaml: unknown key %r (ignored for forward compatibility)",
                qualified,
            )
        else:
            # Recurse into nested dicts if the field is itself a BaseModel
            sub_value = data[key]
            field_info = model.model_fields[key]
            annotation = field_info.annotation
            # For Optional[X] the annotation may be a Union; unwrap it
            args = getattr(annotation, "__args__", ())
            inner = None
            for arg in args:
                if arg is not type(None) and isinstance(arg, type) and issubclass(arg, BaseModel):
                    inner = arg
                    break
            if inner is None and isinstance(annotation, type) and issubclass(annotation, BaseModel):
                inner = annotation
            if inner is not None and isinstance(sub_value, dict):
                _warn_unknown_keys(sub_value, inner, prefix=qualified)


def _deep_merge(
    base: dict[str, object], override: dict[str, object]
) -> dict[str, object]:
    """Recursively merge ``override`` onto ``base``; ``override`` wins.

    Nested dicts merge key-by-key; everything else (scalars, lists, None)
    replaces wholesale. A project-level list such as ``external_reviewers``
    fully replaces the global list rather than concatenating, which keeps the
    merge predictable (no accidental duplicate or stale entries). Returns a new
    dict; neither input is mutated.
    """
    result = dict(base)
    for key, value in override.items():
        existing = result.get(key)
        if isinstance(existing, dict) and isinstance(value, dict):
            result[key] = _deep_merge(existing, value)
        elif isinstance(existing, dict) and value is None:
            # An empty override block parses as None (e.g. a bare `config:` line
            # with nothing indented under it). Do NOT let it overwrite an existing
            # dict, which would null out a nested model block (config/project/...)
            # and fail Pydantic validation (Gemini HIGH, PR #409).
            continue
        else:
            result[key] = value
    return result


def _alias_legacy_keys(raw: dict[str, object]) -> dict[str, object]:
    """Bridge legacy key locations onto their canonical modeled paths (US3).

    Mutates and returns ``raw`` (the merged settings dict) in place so that an
    existing settings.yaml keeps working after promotion:

    - ``config.external_reviewers`` (legacy list) and ``config.external_reviewer``
      (legacy scalar) -> ``config.review.external_reviewers``. The canonical
      ``config.review.external_reviewers`` wins if already present; else the
      legacy list; else the legacy scalar (as a single-item list).
    - top-level ``project.vision`` -> ``config.project.vision`` (canonical wins
      if already set).

    A one-time deprecation WARNING is emitted per legacy path that was actually
    used. No-ops cleanly when no legacy key is present (AC4-EDGE). Pre-existing
    unknown/extra keys are left untouched (the model's ``extra="ignore"`` handles
    them exactly as before).
    """
    if not isinstance(raw, dict):
        return raw

    config = raw.get("config")
    if not isinstance(config, dict):
        config = {}

    # --- review.external_reviewers ---------------------------------------
    review = config.get("review")
    review = review if isinstance(review, dict) else {}
    canonical_present = "external_reviewers" in review
    legacy_list = config.get("external_reviewers")
    legacy_scalar = config.get("external_reviewer")

    resolved_reviewers: object = None
    if canonical_present:
        resolved_reviewers = review.get("external_reviewers")
    elif legacy_list is not None:
        resolved_reviewers = legacy_list
        _LOG.warning(
            "settings.yaml: 'config.external_reviewers' is deprecated; "
            "use 'config.review.external_reviewers' instead."
        )
    elif legacy_scalar is not None:
        resolved_reviewers = legacy_scalar
        _LOG.warning(
            "settings.yaml: 'config.external_reviewer' (scalar) is deprecated; "
            "use the list 'config.review.external_reviewers' instead."
        )

    if resolved_reviewers is not None and not canonical_present:
        review["external_reviewers"] = resolved_reviewers
        config["review"] = review
        raw["config"] = config

    # --- top-level project.* -> config.project.* -------------------------
    # The whole top-level `project` block is deprecated (id + vision); lift any
    # field config.project hasn't already set. canonical (config.project) wins.
    top_project = raw.get("project")
    if isinstance(top_project, dict):
        cfg_project = config.get("project")
        cfg_project = cfg_project if isinstance(cfg_project, dict) else {}
        lifted = False
        for fld in ("id", "vision"):
            if top_project.get(fld) is not None and cfg_project.get(fld) is None:
                cfg_project[fld] = top_project[fld]
                lifted = True
        if lifted:
            config["project"] = cfg_project
            raw["config"] = config
            _LOG.warning(
                "settings.yaml: the top-level 'project' block is deprecated; "
                "use 'config.project' instead."
            )

    # --- top-level work -> config.work ------------------------------------
    top_work = raw.get("work")
    if isinstance(top_work, dict) and not isinstance(config.get("work"), dict):
        config["work"] = top_work
        raw["config"] = config
        _LOG.warning(
            "settings.yaml: the top-level 'work' block is deprecated; "
            "use 'config.work' instead."
        )

    return raw


@lru_cache(maxsize=1)
def load_settings() -> SettingsModel:
    """Load, deep-merge, and cache the settings for the lifetime of this process.

    Every existing candidate is read and deep-merged, highest priority winning
    key-by-key: $FNO_CONFIG (when set, the only candidate) ->
    <worktree>/.fno/settings.yaml -> <canonical>/.fno/settings.yaml
    -> ~/.fno/settings.yaml -> built-in defaults. See _candidate_paths for
    the canonical (main worktree from `git worktree list`) step that lets a
    linked worktree read shared config. A key absent from a higher-priority file
    falls through to the next file down, so global can hold shared defaults
    while each project sets only its deltas.

    Raises ValidationError on invalid values (glob chars, PATH_MAX, etc.).
    Emits WARNING for unknown keys.
    """
    global _loaded_from

    # Collect every candidate that exists and parses, in priority order
    # (project-local highest, global lowest). Files that fail to parse are
    # skipped (a WARNING is already emitted by _load_raw) so a corrupt
    # higher-priority file still falls through to a valid lower-priority one.
    layers: list[tuple[Path, dict[str, object]]] = []
    for candidate in _candidate_paths():
        if candidate.is_file():
            parsed, ok = _load_raw(candidate)
            if ok:
                layers.append((candidate.resolve(), parsed))

    # Deep-merge lowest priority first so the highest-priority file wins per
    # key. config.obsidian.vault can come from global while
    # config.post_merge.parking_lot_path comes from the project file.
    # Alias legacy keys PER LAYER, before merging, so a higher-priority file's
    # legacy value still wins over a lower-priority file's canonical value (and
    # vice-versa). Aliasing only the merged result would let a low-priority
    # canonical key mask a high-priority legacy key.
    raw: dict[str, object] = {}
    for _path, parsed in reversed(layers):
        raw = _deep_merge(raw, _alias_legacy_keys(parsed))

    # _loaded_from records the PRIMARY (highest-priority) file present, for
    # `fno config doctor` and paths.config_file(). With layering there is no
    # single source; the highest-priority file is the most meaningful anchor
    # (Finding 3: paths.config_file must agree with the loader, not re-derive).
    _loaded_from = layers[0][0] if layers else None

    # Warn about unknown top-level and nested keys BEFORE model construction
    # so the message appears even if validation later raises.
    # The recursive walker handles nested blocks (config, paths, etc.) automatically;
    # there is no need for an additional explicit nested call (which caused duplicate emission).
    _warn_unknown_keys(raw, SettingsModel)

    return SettingsModel.model_validate(raw)


def settings_from_files(paths: list[Path]) -> SettingsModel:
    """Build a validated SettingsModel from explicit files, highest priority first.

    The single model-backed loader for callers that must read config from
    specific files rather than the cached candidate discovery of
    ``load_settings`` (e.g. tests, or a module given explicit project/user
    paths). Files are deep-merged with the SAME ``_deep_merge`` + legacy
    aliasing the main loader uses, so there is exactly one schema and one set
    of defaults (US2: no per-module DEFAULT_CONFIG / private merge).

    ``paths`` is ordered highest-priority first; missing or unparseable files
    are skipped.
    """
    layers: list[dict[str, object]] = []
    for candidate in paths:
        if candidate and Path(candidate).is_file():
            parsed, ok = _load_raw(Path(candidate))
            if ok:
                layers.append(parsed)
    raw: dict[str, object] = {}
    # Merge lowest priority first so the first (highest-priority) file wins.
    # Alias per-layer (see load_settings) so precedence holds across legacy/canonical.
    for parsed in reversed(layers):
        raw = _deep_merge(raw, _alias_legacy_keys(parsed))
    return SettingsModel.model_validate(raw)


def load_settings_for_repo(repo_root: Path) -> SettingsModel:
    """Load settings for a specific repo root, merging with the global settings.

    Uncached (unlike ``load_settings``): used by the global PR-state watcher to
    read per-repo config (e.g. ``config.review.required_bots``) for each
    candidate PR's repository, without polluting the process-level cache.

    Merge order (highest to lowest priority):
      <repo_root>/.fno/settings.yaml -> ~/.fno/settings.yaml -> built-in defaults.
    """
    layers: list[tuple[Path, dict[str, object]]] = []

    candidates = [
        repo_root / ".fno" / "settings.yaml",
        _global_settings_path(),
    ]
    for candidate in candidates:
        if candidate.is_file():
            parsed, ok = _load_raw(candidate)
            if ok:
                layers.append((candidate.resolve(), parsed))

    raw: dict[str, object] = {}
    for _path, parsed in reversed(layers):
        raw = _deep_merge(raw, _alias_legacy_keys(parsed))
    return SettingsModel.model_validate(raw)


def agents_headless_yolo(provider: str) -> bool:
    """Resolve ``config.agents.<provider>.headless_yolo`` (bounded-posture amendment).

    Returns whether an autonomous (headless, MODE==exec) worker for ``provider``
    should use FULL yolo (``True``) instead of the BOUNDED posture (``False``,
    the default). Both never prompt; full yolo additionally drops the sandbox.
    Only codex and gemini carry the knob; any other provider (claude is
    unaffected) resolves to ``False``.

    Degrades to ``False`` (bounded) on ANY read failure (missing block,
    validation error, unexpected exception): bounded is the hang-safe default
    (it never prompts), so a typo can never re-introduce the headless hang AND
    never silently drops the sandbox into a full bypass.
    """
    try:
        block = getattr(load_settings().config.agents, provider, None)
    except Exception:
        return False
    if block is None:
        return False
    return bool(getattr(block, "headless_yolo", False))
