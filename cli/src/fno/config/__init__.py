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
import re
import tempfile
import tomllib
from functools import lru_cache
from pathlib import Path
from typing import Any, Literal, Optional, cast

import tomli_w
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
    bus_dir: Optional[str] = None
    loops_paused_json: Optional[str] = None
    observer_reports_dir: Optional[str] = None

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
        "bus_dir",
        "loops_paused_json",
        "observer_reports_dir",
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
    # Canonical-sync (x-47be): all three default to the feature-off state so a
    # fresh install does nothing. sync_command is the project's whole sync
    # incantation (run via `bash -lc` from the canonical checkout); sync_paths
    # gates it on the merged file list (empty = always run); auto_run lets
    # merge-detection dispatch the /fno:pr merged ritual. Footnote's own values
    # are a documented example, never engine defaults.
    sync_command: Optional[str] = None
    sync_paths: list[str] = Field(default_factory=list)
    auto_run: bool = False

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

    @field_validator("auto_run", mode="before")
    @classmethod
    def _coerce_auto_run(cls, v: object) -> bool:
        """Fail-safe to false on any non-boolean value.

        auto_run lets merge-detection dispatch a background /fno:pr merged
        ritual worker. Default off, and a scalar typo coerces to False rather
        than raising - false is the safe direction (never spawn an agent behind
        the maintainer's back on a malformed value).
        """
        if isinstance(v, bool):
            return v
        if isinstance(v, int):  # bool already handled above
            return v == 1
        if isinstance(v, str):
            return v.strip().lower() in {"1", "true", "yes", "on"}
        return False


class ResearchBlock(BaseModel):
    """`fno research` doc-deliverable settings (nested under 'config.research').

    `output_dir` is the landing area for the research `doc` deliverable: the
    brief (`<slug>.md`) and its evidence sidecar (`<slug>.sources.jsonl`) are
    written there. Unlike `post_merge.parking_lot_path` this is NOT repo-relative
    - it is a vault/output area (e.g. ~/c3po/raw/readyrule), so absolute and
    '~'-anchored paths are allowed. Left unset by default so a repo that has not
    opted in fails loud at the ship step rather than guessing a landing path
    (the parking_lot_path lesson, AC5).
    """

    model_config = ConfigDict(extra="ignore")

    output_dir: Optional[str] = None

    @field_validator("output_dir", mode="before")
    @classmethod
    def validate_output_dir(cls, v: object) -> object:
        if isinstance(v, str):
            _check_no_glob(v, "config.research.output_dir")
            _check_path_max(v, "config.research.output_dir")
        return v


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


# Reviewer names that have a `review_attestation` emit path (x-e703 Change 4).
# config.review.reviewers entries must resolve to one of these; a leading '/' is
# stripped first (so `/code-review` == `code-review`). Kept in sync with the
# emit surfaces in skills/review and the loop-check attestation read.
_RESOLVABLE_REVIEWERS = frozenset({"sigma", "code-review", "declare"})


class ReviewBlock(BaseModel):
    """External-review gate settings (nested under 'config.review').

    `github_apps` is the must-have-reviewed list of GitHub App bot logins
    consumed by the `fno-agents loop-check` review gate (control-plane step 2,
    ab-f1c5a9ed): a session terminates DonePRGreen only when every listed login
    has at least one completed review pass and no unaddressed blocking finding.

    Semantics (mirrors the Rust-side parser in loopcheck.rs):
      - key absent (None)  -> no review gate (PR + CI only); a fresh install
                              completes without hanging on a bot it never set up.
                              The effective Rust default is [] (cv-6537099f); the
                              old ["chatgpt-codex-connector"] docstring was wrong.
      - explicit []        -> declared no-review-gate path (PR + CI only),
                              mirroring ci.declared_none; never auto-detected
      - non-empty list     -> every listed login must pass

    `peers` are harness peers (codex/gemini/...) run locally that post a real PR
    review under `peer_identity` (a distinct machine account, not the author).
    Each entry is a provider scalar (`codex`) or a map (`{provider, identity,
    token_env}`). The gate is the union of `github_apps` and the resolved peer
    identities (loop-check stays login-based; per-peer coverage is the posting
    pipeline's job for a shared identity). Phase 1: `reviewers` (local
    attestation) is a follow-up node.

    `required_bots` is a legacy alias for `github_apps` (a straight rename).
    Existing configs are unchanged; if both are set, `github_apps` wins.

    Distinct from `config.external_reviewers` (the recognition/matching
    list): that list says which logins count as bots; `github_apps` says which
    logins are REQUIRED to have reviewed.
    """

    model_config = ConfigDict(extra="ignore")

    # The GATE: GitHub App bot logins that must have reviewed (canonical).
    github_apps: Optional[list[str]] = None
    # Legacy alias for github_apps; resolved into github_apps by the validator
    # below (github_apps wins if both set). Kept readable for back-compat.
    required_bots: Optional[list[str]] = None
    # Harness peers that run a CLI locally and post a real PR review under
    # peer_identity. Scalar (`codex`) or map (`{provider, identity, token_env}`).
    peers: list[Any] = Field(default_factory=list)  # str provider or {provider,...} map
    # The distinct login peers post under (must not be the author account) and
    # the env var holding that identity's PAT. Required whenever `peers` is set.
    peer_identity: Optional[str] = None
    peer_token_env: Optional[str] = None
    # Reviewer logins honored-if-present but NOT required (x-4baa): the gate
    # never waits for them (their absence never blocks - kills the App-bot
    # usage-limit wedge), but a blocking finding from one still holds the gate.
    optional_apps: list[str] = Field(default_factory=list)
    # Local-attestation reviewers (x-e703, Phase 2): skill/agent/command names
    # (sigma | /code-review | declare) that produce NO GitHub review object, so
    # loop-check accepts a head-pinned `review_attestation` event as gate
    # evidence instead of a login match. A leading '/' is stripped so
    # `/code-review` and `code-review` name the same reviewer. An unresolvable
    # name fails LOUD here (a typo must never silently become no-gate) and is
    # unsatisfiable at loop-check (Rust, fail closed).
    reviewers: list[str] = Field(default_factory=list)
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

    @field_validator("github_apps", "required_bots", mode="before")
    @classmethod
    def coerce_malformed_to_default(cls, v: object) -> object:
        """Coerce a bare scalar to a single-login list; fail closed on other junk.

        A bracket-less scalar (`github_apps: codex`, or a stray number) is a
        single-login gate, NOT a silent no-gate: a typo must still GATE on that
        login rather than fail OPEN (codex P1 on #205; parity with the Rust
        text reader, which singleton-izes any scalar). Only a genuinely
        un-listable value (a mapping, or a bare bool) degrades to "absent".
        """
        if v is None or isinstance(v, list):
            return v
        # bool is nonsense here (and str(True) != the Rust reader's "true"); the
        # Rust reader also rejects `{...}` mappings via scalar_as_singleton.
        if not isinstance(v, bool) and isinstance(v, (str, int, float)):
            return [str(v)]
        _LOG.warning(
            "settings.yaml: config.review.github_apps/required_bots is not a "
            "list or scalar login (%r); ignoring it",
            v,
        )
        return None

    @field_validator("optional_apps", mode="before")
    @classmethod
    def coerce_optional_apps(cls, v: object) -> object:
        """Accept a scalar or list; fail-safe a junk value to [] (no optional).

        A bare string (`optional_apps: chatgpt-codex-connector`) coerces to a
        one-item list. Unlike the required gate, degrading a malformed optional
        list to [] is safe: it only drops honored-if-present reviewers, never
        weakens a REQUIRED gate.
        """
        if v is None:
            return []
        if isinstance(v, list):
            return v
        # A scalar login coerces to a one-item list (parity with the Rust text
        # reader). bool / mapping is not a login -> [] (no optional).
        if not isinstance(v, bool) and isinstance(v, (str, int, float)):
            return [str(v)]
        _LOG.warning(
            "settings.yaml: config.review.optional_apps is not a list or scalar "
            "login (%r); ignoring it",
            v,
        )
        return []

    @field_validator("reviewers", mode="before")
    @classmethod
    def coerce_and_resolve_reviewers(cls, v: object) -> object:
        """Coerce scalar->list, strip a leading '/', and reject an unresolvable
        name loudly (AC2-ERR / AC3-ERR).

        The resolvable set is exactly the reviewers that have a
        `review_attestation` emit path in this repo (sigma / code-review /
        declare). A name outside it names no producer, so its gate entry could
        never be satisfied - raising at load beats a silent never-green gate.
        Unlike optional_apps, this fails CLOSED-and-LOUD rather than fail-safe:
        a reviewers typo is a mis-declared GATE, not a dropped optional.
        """
        if v is None:
            return []
        if isinstance(v, bool) or not isinstance(v, (str, list)):
            raise ValueError(
                f"config.review.reviewers must be a scalar or list of reviewer "
                f"names (got {v!r})"
            )
        raw = [v] if isinstance(v, str) else v
        cleaned: list[str] = []
        for entry in raw:
            if not isinstance(entry, str):
                raise ValueError(
                    f"config.review.reviewers entry must be a string (got {entry!r})"
                )
            name = entry.strip().lstrip("/")
            if name not in _RESOLVABLE_REVIEWERS:
                raise ValueError(
                    f"config.review.reviewers names an unresolvable reviewer "
                    f"{entry!r} (expected one of {sorted(_RESOLVABLE_REVIEWERS)}); "
                    f"a typo would leave the gate permanently unsatisfiable"
                )
            cleaned.append(name)
        return cleaned

    @field_validator("peers", mode="before")
    @classmethod
    def coerce_peers(cls, v: object) -> object:
        """Accept a scalar or a list of scalar-or-map entries; fail LOUD on a
        map entry missing `provider`.

        A bare string (`peers: codex`) coerces to a single-item list. A map
        entry must carry `provider` (the CLI to run); a missing `provider` is a
        loud config error, NOT a silent skip (a silently-dropped peer would
        fail open - the gate would go green without that reviewer).
        """
        if v is None:
            return []
        if isinstance(v, (str, dict)):
            v = [v]
        if not isinstance(v, list):
            raise ValueError(
                f"config.review.peers must be a scalar or list (got {v!r})"
            )
        for entry in v:
            if isinstance(entry, dict) and "provider" not in entry:
                raise ValueError(
                    "config.review.peers map entry is missing 'provider': "
                    f"{entry!r}"
                )
            if not isinstance(entry, (str, dict)):
                raise ValueError(
                    f"config.review.peers entry must be a string or map: {entry!r}"
                )
        return v

    @model_validator(mode="after")
    def resolve_github_apps_alias(self) -> "ReviewBlock":
        """`required_bots` is a legacy alias for `github_apps`; github_apps wins.

        Both readable afterwards (kept in sync) so old readers of
        `required_bots` and new readers of `github_apps` see the same value.
        """
        if self.github_apps is not None and self.required_bots is not None:
            _LOG.warning(
                "settings.yaml: both config.review.github_apps and the legacy "
                "config.review.required_bots are set; using github_apps",
            )
        elif self.github_apps is None and self.required_bots is not None:
            self.github_apps = self.required_bots
        # Keep the alias readable and consistent with the canonical field.
        self.required_bots = self.github_apps
        # A peer needs an identity to post under: its own (map `identity`) or
        # the shared `peer_identity`. Fail loud if any peer has neither - a
        # peers gate with no resolvable login can never clear (fail-closed
        # forever), so surface it at load rather than wedging the loop.
        if self.peers and not self.peer_identity:
            needs_shared = [
                e
                for e in self.peers
                if not (isinstance(e, dict) and e.get("identity"))
            ]
            if needs_shared:
                raise ValueError(
                    "config.review.peers has an entry with no posting identity "
                    "and config.review.peer_identity is unset; peers must post "
                    f"under a distinct machine account: {needs_shared!r}"
                )
        # A `claude` peer is only a real cross-model reviewer when it names a
        # model route (e.g. {provider: claude, model: "zai,glm-5.2"}): the claude
        # CLI is only transport, and the routed model (GLM) is genuinely distinct
        # from the Claude author. A bare `claude` peer (no route) IS the author's
        # own model, which defeats the "distinct model" trust invariant - reject
        # it at load, fail-closed, rather than let it masquerade as a peer.
        for e in self.peers:
            prov: object
            model: object
            if isinstance(e, str):
                prov, model = e, None
            elif isinstance(e, dict):
                prov, model = e.get("provider"), e.get("model")
            else:
                continue
            if not (isinstance(prov, str) and prov.strip().lower() == "claude"):
                continue
            # A claude peer is the author's own model UNLESS it names a route to
            # a genuinely different model (the claude CLI is only transport). A
            # non-empty `model` is not enough: it must parse as
            # `route_provider,route_model` AND not route back to the author's own
            # provider (anthropic/claude), or it defeats the distinct-model trust
            # invariant just as a bare claude peer would. Reject fail-closed at
            # load rather than let a same-model review masquerade as a peer.
            route = model.strip() if isinstance(model, str) else ""
            parts = [p.strip() for p in route.split(",")]
            if len(parts) != 2 or not parts[0] or not parts[1]:
                raise ValueError(
                    "config.review.peers has a claude peer with no valid model "
                    'route (need "route_provider,route_model", e.g. "zai,glm-5.2"); '
                    "a bare claude peer is the same model as the author, breaking "
                    f"the distinct-model trust invariant: {e!r}"
                )
            if parts[0].lower() in {"anthropic", "claude"}:
                raise ValueError(
                    "config.review.peers claude peer routes to the author's own "
                    f"provider ({parts[0]!r}), which is not a distinct model - it "
                    "breaks the distinct-model trust invariant; route to a "
                    f"different provider (e.g. zai): {e!r}"
                )
        return self

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
    # Dead-row GC grace window in SECONDS (x-b1aa). A finished agent-view row
    # stays visible this long after the daemon GC first observes its process gone,
    # before it is reaped. Default 3600 (1h). The Rust daemon + `fno agents reap`
    # read the same `config.agents.dead_row_grace` key (agents_config.rs), so a
    # non-default here changes both the automatic sweep and the manual verb.
    dead_row_grace: int = 3600
    codex: AgentProviderBlock = Field(default_factory=AgentProviderBlock)
    gemini: AgentProviderBlock = Field(default_factory=AgentProviderBlock)
    # Spawn-gate knobs (x-c5cc). All three coerce invalid values to their
    # defaults (fail-open, matching _coerce_max_concurrent): the gate is
    # protective infrastructure and a typo must never brick spawning.
    #   max_live    — cap on concurrent live worker processes (union of the fno
    #                 registry and claude's daemon roster). Spawn queues at cap.
    #   min_free_gb — available-RAM floor for spawn preflight; <= 0 disables.
    #   worker_qos  — utility (demote workers to background QoS) | off.
    max_live: int = 3
    min_free_gb: float = 4.0
    worker_qos: str = "utility"

    @field_validator("dead_row_grace")
    @classmethod
    def dead_row_grace_nonneg(cls, v: int) -> int:
        """Grace is a duration; a negative value is a config error (would reap
        instantly, defeating the visibility window)."""
        if v < 0:
            raise ValueError(
                f"config.agents.dead_row_grace must be >= 0 seconds; got {v}"
            )
        return v

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

    @field_validator("max_live", mode="before")
    @classmethod
    def _coerce_max_live(cls, v: object) -> object:
        """Drop a non-positive / non-int max_live to the default (3); never raise."""
        if isinstance(v, bool):
            return 3
        if isinstance(v, int) and v >= 1:
            return v
        if isinstance(v, str):
            try:
                n = int(v.strip())
            except ValueError:
                return 3
            return n if n >= 1 else 3
        return 3

    @field_validator("min_free_gb", mode="before")
    @classmethod
    def _coerce_min_free_gb(cls, v: object) -> object:
        """Coerce a non-numeric min_free_gb to the default (4.0); never raise.

        <= 0 is a VALID value (guard disabled), so only unparseable input
        falls back to the default.
        """
        if isinstance(v, bool):
            return 4.0
        if isinstance(v, (int, float)):
            return float(v)
        if isinstance(v, str):
            try:
                return float(v.strip())
            except ValueError:
                return 4.0
        return 4.0

    @field_validator("worker_qos", mode="before")
    @classmethod
    def _coerce_worker_qos(cls, v: object) -> object:
        """Any value outside utility|off coerces to the default (utility)."""
        if isinstance(v, str) and v.strip().lower() in ("utility", "off"):
            return v.strip().lower()
        return "utility"

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


class ThinkSpawnBlock(BaseModel):
    """Context-carrying /think spawn settings (nested under 'config.think_spawn').

    The opt-in for node x-6a10: when enabled, the node-birth path (``fno backlog
    idea``) spawns/offers a context-carrying ``/think`` thread for a generated
    organic node, handing it the *resolved* origin transcript pointer (not a
    paraphrase) so a later pickup starts from ground truth.

    Default ``False`` (Locked Decision 2): shipping this changes nothing until
    the operator arms it; an absent key reads as off. The fail-safe posture
    mirrors ``config.auto_continue``: a malformed block degrades to disabled
    rather than failing the whole settings load - false-enabled (a background
    ``/think`` dispatched against the operator's intent) is the dangerous
    direction. The enable + presence + spawn logic lives in
    :mod:`fno.provenance.spawn_think`.

    ``max_per_run`` is the per-node-generation-run blast-radius cap (AC4-EDGE):
    a bulk run exceeding it skips the remainder and logs the truncation.
    ``idle_threshold_s`` is the discretionary activity-recency refinement
    (Claude's Discretion 1); ``0`` (default) disables it so the primary
    attended-vs-headless signal stands alone.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False
    max_per_run: int = 5
    idle_threshold_s: int = 0
    # --- A2 lifecycle triggers (x-122a) ---
    # Two non-birth dispatch moments, each behind its OWN sub-flag, default OFF
    # even when ``enabled`` is on (Open Question 1): ``on_work_start`` fires when
    # /target claims a node to work it; ``on_retro`` fires when ``fno backlog
    # done`` closes a node. ``daily_cap`` is the per-install per-day firehose
    # ceiling the broad A2 triggers require (Locked Decision 3); 0 disables it.
    on_work_start: bool = False
    on_retro: bool = False
    daily_cap: int = 20
    # B (x-5d51): how an attended session handles a born node. ``offer`` (default,
    # byte-for-byte x-6a10) prints a copy-pasteable handoff line; ``spawn`` opts
    # into a real bg /think dispatch. Fail-safe to ``offer`` so a garbage value
    # never auto-spawns against operator intent.
    attended: str = "offer"

    @field_validator("enabled", "on_work_start", "on_retro", mode="before")
    @classmethod
    def _coerce_enabled(cls, v: object) -> bool:
        """Fail-safe to disabled on any non-boolean value (AC4-ERR).

        Deliberately STRICT for a safety opt-in: only a clear affirmative
        (``true``/``yes``/``on``/``1``) enables; an ambiguous value fails safe
        to disabled, mirroring ``AutoContinueBlock._coerce_enabled``.
        """
        if isinstance(v, bool):
            return v
        if isinstance(v, int):  # bool already handled above
            return v == 1
        if isinstance(v, str):
            return v.strip().lower() in {"1", "true", "yes", "on"}
        return False

    @staticmethod
    def _nonneg_int_or(v: object, fallback: int) -> int:
        """Coerce ``v`` to a non-negative int, else ``fallback``. Never raises."""
        if isinstance(v, bool):
            return fallback  # a bool is not a meaningful bound
        if isinstance(v, int) and v >= 0:
            return v
        if isinstance(v, str) and v.strip().isdigit():
            return int(v.strip())
        return fallback

    @field_validator("max_per_run", mode="before")
    @classmethod
    def _coerce_max_per_run(cls, v: object) -> int:
        """Fail-safe: a garbage cap degrades to the default 5, never 0.

        Coercing garbage to 0 would silently disable every spawn; coercing to a
        small positive cap keeps the blast-radius guard meaningful while still
        bounding a spawn-storm (AC4-EDGE).
        """
        return cls._nonneg_int_or(v, 5)

    @field_validator("idle_threshold_s", mode="before")
    @classmethod
    def _coerce_idle_threshold(cls, v: object) -> int:
        """Fail-safe: a garbage threshold degrades to 0 (refinement off)."""
        return cls._nonneg_int_or(v, 0)

    @field_validator("daily_cap", mode="before")
    @classmethod
    def _coerce_daily_cap(cls, v: object) -> int:
        """Fail-safe: a garbage ceiling degrades to the default 20 (never 0).

        Coercing garbage to 0 would silently disable the firehose guard; the
        default keeps a meaningful per-day ceiling. An explicit 0 is honored
        (disables the ceiling) since it round-trips through ``_nonneg_int_or``.
        """
        return cls._nonneg_int_or(v, 20)

    @field_validator("attended", mode="before")
    @classmethod
    def _coerce_attended(cls, v: object) -> str:
        """Fail-safe: only an explicit ``spawn`` opts in; everything else => offer.

        Strict like ``_coerce_enabled``: the dangerous direction is an unintended
        auto-spawn, so an ambiguous/garbage value keeps the safe ``offer`` default.
        """
        if isinstance(v, str) and v.strip().lower() == "spawn":
            return "spawn"
        return "offer"


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


class RecoveryBlock(BaseModel):
    """Session auto-recovery watchdog settings (nested under 'config.recovery').

    Controls the Layer-2 watchdog (x-f47c) that rides the ``pr_watch`` launchd
    tick: it finds footnote-launched bg sessions which went idle-but-incomplete
    after an abnormal turn termination and re-injects a resume nudge. It only
    runs when ``pr_watch`` is installed (it shares that cadence) AND ``enabled``
    here is true; even then it is a no-op unless an idle-stale footnote bg
    session exists.

    Fields
    ------
    enabled:
        True to run the recovery sweep each pr_watch tick (default True; the
        target audience for pr_watch is exactly bg-session operators). Set
        false to keep pr_watch's PR polling without the resume nudges.
    idle_threshold_seconds:
        How stale a session's state.json must be before it is treated as
        idle-but-incomplete (default 900 = 15 min). This MUST exceed the
        longest expected single-tool runtime: a session mid-way through a long
        ``cargo build`` / test suite is busy but emits no turn events, so its
        state.json freezes while it is legitimately working — too low a value
        nudges a working session. 15 min clears that while still recovering a
        genuinely wedged session long before Claude Code's ~1h reaper abandons
        it. Tunable per workload (the motivating repro stalled ~3.5 min, but a
        human was watching; an autonomous watchdog is deliberately patient).
    max_nudges:
        Per-session cap on resume nudges before the watchdog gives up and emits
        ``recovery_capped`` (default 3) so a genuinely wedged session surfaces
        instead of looping forever.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = True
    idle_threshold_seconds: int = Field(default=900, gt=0)
    max_nudges: int = Field(default=3, ge=1)


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
    ``load_settings().health_monitor`` instead of re-loading the YAML (US2).
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


_DURATION_RE = re.compile(r"^\s*(\d+)\s*([smhd])\s*$")
_DURATION_UNITS = {"s": 1, "m": 60, "h": 3600, "d": 86400}


def _parse_duration_to_seconds(v: object) -> Optional[int]:
    """Parse a duration string (``"5m"``/``"30s"``/``"2h"``/``"1d"``) to seconds.

    A bare int/float is interpreted as seconds. Returns ``None`` for anything
    unparseable, zero, or negative, so callers can fail safe to disabled rather
    than spin a 0-sleep hot loop (active-backlog Boundaries).
    """
    if isinstance(v, bool):
        return None
    if isinstance(v, (int, float)):
        secs = int(v)
        return secs if secs > 0 else None
    if isinstance(v, str):
        s = v.strip()
        # A bare digit string ("300") is interpreted as seconds, matching the
        # bare-int path so `interval: "300"` and `interval: 300` agree (instead
        # of the quoted form silently disabling the feature).
        if s.isdigit():
            secs = int(s)
            return secs if secs > 0 else None
        m = _DURATION_RE.match(s)
        if not m:
            return None
        secs = int(m.group(1)) * _DURATION_UNITS[m.group(2)]
        return secs if secs > 0 else None
    return None


class ActiveBacklogConfig(BaseModel):
    """Active backlog dispatcher settings (nested under 'config.active_backlog').

    The opt-in for the always-on backlog drain daemon (node x-c070): when
    enabled for a project, the per-user supervisor daemon continuously claims
    ready backlog nodes for that project and dispatches them one at a time
    through the existing megawalk loop primitive, sleeping between drains with
    an event nudge for low latency.

    Default OFF (footnote convention): a disabled block is byte-for-byte
    today's behavior. The fail-safe posture mirrors ``config.auto_continue`` /
    ``config.target.blast``: a malformed block degrades to disabled rather than
    raising out of the whole settings load, a bad scalar field is dropped to its
    default (never raised), and an invalid ``interval`` (zero, negative, or
    unparseable) disables the feature rather than spinning a 0-sleep hot loop
    (Boundaries).

    Fields
    ------
    enabled:
        ``True`` to drain every project, or a per-project map
        ``{<project>: bool}`` to scope the daemon to specific projects.
        Default ``False``. A scalar typo fails safe to disabled; in a map, only
        a clear affirmative (``true``/``yes``/``on``/``1``) enables a project.
    interval:
        Poll-floor cadence as a duration string (``"5m"``, ``"30s"``, ``"2h"``,
        ``"1d"``) or a bare integer (seconds). Default ``"5m"``. The poll floor
        is the correctness guarantee; the event nudge is a latency optimization
        layered on top.
    failure_limit:
        Consecutive dispatch failures before a node is parked (the circuit
        breaker). Default 3. Reset to zero only on a successful close.
    max_concurrent:
        In-flight nodes per project per tick. Default 1 (serial, v1). Defined
        now so v2 parallelism needs no config migration; v1 asserts == 1.
    mission:
        Optional mission id; when set, the daemon drains only that mission's
        nodes and never drifts into the general backlog.
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool | dict[str, bool] = False
    interval: str = "5m"
    failure_limit: int = Field(default=3, ge=1)
    max_concurrent: int = Field(default=1, ge=1)
    mission: Optional[str] = None

    @field_validator("enabled", mode="before")
    @classmethod
    def _coerce_enabled(cls, v: object) -> object:
        """Fail-safe coercion for ``enabled`` (bool or per-project map).

        A bool passes through. A mapping is coerced per-value with the strict
        affirmative truth table (only ``true``/``yes``/``on``/``1`` enable a
        project; an ambiguous value disables that project, never guesses on).
        Any other scalar (``enabled: banana``) fails safe to ``False`` - the
        dangerous direction for an autonomous-dispatch opt-in is false-enabled.
        """
        if isinstance(v, bool):
            return v
        if isinstance(v, dict):
            return {str(k): _coerce_affirmative(val, False) for k, val in v.items()}
        return _coerce_affirmative(v, False)

    @field_validator("interval", mode="before")
    @classmethod
    def _coerce_interval(cls, v: object) -> object:
        """Drop a non-string/non-int interval to the default; never raise.

        Whether the value parses to a *positive* duration is decided by the
        consumer accessors (:meth:`interval_seconds` / :meth:`is_enabled_for`),
        which fail closed on a bad interval rather than raising (Boundaries).
        """
        if isinstance(v, bool):
            return "5m"
        if isinstance(v, str):
            return v
        if isinstance(v, (int, float)):
            return f"{int(v)}s"
        return "5m"

    @field_validator("failure_limit", mode="before")
    @classmethod
    def _coerce_failure_limit(cls, v: object) -> object:
        """Drop a non-positive-int failure_limit to the default (3); never raise."""
        if isinstance(v, bool):
            return 3
        if isinstance(v, int) and v >= 1:
            return v
        if isinstance(v, str):
            try:
                n = int(v.strip())
            except ValueError:
                return 3
            return n if n >= 1 else 3
        return 3

    @field_validator("max_concurrent", mode="before")
    @classmethod
    def _coerce_max_concurrent(cls, v: object) -> object:
        """Drop a non-positive-int max_concurrent to the default (1); never raise."""
        if isinstance(v, bool):
            return 1
        if isinstance(v, int) and v >= 1:
            return v
        if isinstance(v, str):
            try:
                n = int(v.strip())
            except ValueError:
                return 1
            return n if n >= 1 else 1
        return 1

    def interval_seconds(self) -> Optional[int]:
        """Parsed poll-floor in seconds, or ``None`` if the interval is invalid."""
        return _parse_duration_to_seconds(self.interval)

    def is_enabled_for(self, project: Optional[str]) -> bool:
        """Whether the daemon should drain ``project``.

        Fail-closed: an invalid interval disables the feature entirely (no
        0-sleep hot loop). ``enabled: true`` enables every project; a
        per-project map enables only its truthy keys.
        """
        if self.interval_seconds() is None:
            return False
        en = self.enabled
        if isinstance(en, dict):
            if project is None:
                return False
            return bool(en.get(project, False))
        return bool(en)

    def enabled_projects(self) -> list[str]:
        """The explicitly-enabled project names (per-project map mode only)."""
        if isinstance(self.enabled, dict):
            return [p for p, on in self.enabled.items() if on]
        return []

    def any_enabled(self) -> bool:
        """Whether the feature is on for >=1 project (and the interval is valid)."""
        if self.interval_seconds() is None:
            return False
        if isinstance(self.enabled, dict):
            return any(self.enabled.values())
        return bool(self.enabled)


class ParallelBlock(BaseModel):
    """Parallel-mode dispatch settings (nested under 'config.parallel').

    Parallel mode (epic x-42d5) runs up to ``max_lanes`` independent bg
    worktree lanes concurrently, one per distinct backlog domain, to compress
    wall-clock time. The cap is the sole cost-bound lever (design Locked
    Decision #3): it trades CI minutes for throughput, never the reverse.

    ``max_lanes`` is deliberately SHARED config, NOT in the per-worktree
    ``settings.local.yaml`` override allowlist (:data:`WORKTREE_LOCAL_KEYS`,
    Locked Decision #10): a per-lane cap is meaningless - every lane must see
    one global ceiling or the cap does not bound anything. The allowlist is an
    exact-match frozenset of ``{parking_lot_path, project.id}``, so this key is
    excluded by construction; do NOT add ``config.parallel.max_lanes`` to it.

    Fields
    ------
    max_lanes:
        Max concurrent lanes. ``0`` disables parallel (sequential); ``1`` is
        today's single-lane path; ``>=2`` opts into parallelism. Default ``1``
        (footnote convention: a new feature is OFF until the operator turns it
        up; the wiring that consumes this lands in later groups). Negative
        values fail safe to the default rather than raising.
    """

    model_config = ConfigDict(extra="ignore")

    max_lanes: int = Field(default=1, ge=0)

    @field_validator("max_lanes", mode="before")
    @classmethod
    def _coerce_max_lanes(cls, v: object) -> object:
        """Drop a negative / non-int max_lanes to the default (1); never raise.

        0 (sequential) and 1 (today) are both valid and preserved; only a
        negative or unparseable value fails safe to 1 (the dangerous direction
        for a cost-bound cap is an accidental huge fan-out, so an ambiguous
        value collapses to the conservative single lane).
        """
        if isinstance(v, bool):
            return 1
        if isinstance(v, int):
            return v if v >= 0 else 1
        if isinstance(v, str):
            try:
                n = int(v.strip())
            except ValueError:
                return 1
            return n if n >= 0 else 1
        return 1


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
    # Cheaper model for the background (haiku) tier so judgment-light background
    # traffic runs cheap while opus/sonnet stay on the role model. Unset (None)
    # keeps the role model on every tier. The built-in zai provider defaults it
    # to glm-4.5-air; set it here to override or to give another provider a
    # cheap background model.
    haiku_model: Optional[str] = None
    # Codex/OpenAI-lane only (protocol == "openai"): the codex wire protocol for
    # this provider's endpoint. Third-party OpenAI-compatible endpoints (e.g.
    # z.ai's paas/v4) speak Chat Completions -> "chat"; leave unset to default
    # to "chat" when routing a codex-lane spawn. Ignored on the anthropic lane.
    wire_api: Optional[str] = None


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


_LOOP_LEVELS = ("report", "assisted", "unattended")


class LoopEntry(BaseModel):
    """One named loop's level (``config.loops.<name>``, x-ce71).

    ``report`` (observe only) is the safest default, so a malformed or
    unrecognized level fails safe to it rather than raising - a standing
    loop must never silently upgrade its own autonomy from a config typo.
    """

    model_config = ConfigDict(extra="ignore")

    level: Literal["report", "assisted", "unattended"] = "report"
    # Aggregate per-run spend ceiling (x-57a5's observer harness is the first
    # consumer: config.loops.observer_harness.budget_usd_per_run). Generic on
    # LoopEntry rather than observer-specific so any future loop with its own
    # spend can reuse the same key. A non-positive/malformed value fails safe
    # to the default rather than silently uncapping a loop's spend.
    budget_usd_per_run: float = 30.0

    @field_validator("level", mode="before")
    @classmethod
    def _coerce_level(cls, v: object) -> str:
        if isinstance(v, str) and v.strip().lower() in _LOOP_LEVELS:
            return v.strip().lower()
        _LOG.warning("config.loops.<name>.level=%r invalid; using default 'report'", v)
        return "report"

    @field_validator("budget_usd_per_run", mode="before")
    @classmethod
    def _coerce_budget(cls, v: object) -> float:
        try:
            f = float(v)  # type: ignore[arg-type]
        except (TypeError, ValueError):
            f = -1.0
        if f <= 0:
            _LOG.warning("config.loops.<name>.budget_usd_per_run=%r invalid; using default 30.0", v)
            return 30.0
        return f


class BatchBlock(BaseModel):
    """Batch-lane settings (nested under 'config.batch', x-8cae).

    The auto-target runner can coalesce N same-domain ready nodes onto one
    branch and open ONE PR per batch instead of one-per-node, cutting GitHub
    Actions runs ~N× (the cost driver is PR *volume*, not bad merges).

    Default OFF (footnote opt-in convention): with `enabled=false` the selection
    path is byte-for-byte today's one-PR-per-node behavior. A malformed
    `enabled` fails safe to disabled (mirrors config.auto_merge / cross_model) —
    false-enabled is the dangerous direction because a bad batch could pool
    unrelated work into one PR.

    Knobs:
      enabled   - whole-feature opt-in.
      max_nodes - nodes per batch before it closes (domain boundary + size/p0
                  can close it sooner). Non-positive/malformed → default 3.
      max_loc   - optional cumulative-diff LOC ceiling (None = off).
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = False
    max_nodes: int = 3
    max_loc: Optional[int] = None

    @field_validator("enabled", mode="before")
    @classmethod
    def _coerce_enabled(cls, v: object) -> bool:
        """Fail-safe to disabled on a non-boolean value (see cross_model)."""
        if isinstance(v, bool):
            return v
        if isinstance(v, int):  # bool already handled above
            return v == 1
        if isinstance(v, str):
            return v.strip().lower() in {"1", "true", "yes", "on"}
        return False

    @field_validator("max_nodes", mode="before")
    @classmethod
    def _coerce_max_nodes(cls, v: object) -> object:
        """Degrade a non-positive / malformed ceiling to the default 3."""
        if isinstance(v, (int, str)) and not isinstance(v, bool):
            try:
                if int(v) >= 1:
                    return int(v)
            except (TypeError, ValueError):
                pass
        _LOG.warning("config.batch.max_nodes=%r invalid; using default 3", v)
        return 3

    @field_validator("max_loc", mode="before")
    @classmethod
    def _coerce_max_loc(cls, v: object) -> object:
        """Degrade a non-positive / malformed ceiling to None (off).

        `should_close` treats max_loc truthily (`if max_loc and cum_loc > ...`),
        so a stray `max_loc: -1` / `0` would close every batch after the first
        node — silently defeating the feature. A non-int-coercible value
        (`max_loc: "lots"`) would raise ValidationError out of the whole
        settings load. Both degrade to None (off), logged, matching max_nodes.
        """
        if v is None:
            return None
        if isinstance(v, (int, str)) and not isinstance(v, bool):
            try:
                if int(v) >= 1:
                    return int(v)
            except (TypeError, ValueError):
                pass
        _LOG.warning("config.batch.max_loc=%r invalid; disabling (None)", v)
        return None


class BranchBlock(BaseModel):
    """Dispatch branch naming (nested under 'config.branch', x-ff83 W3).

    ``prefix`` is the leading segment of a dispatched worktree branch:
    ``<prefix>/<slug>-<node>`` (e.g. ``fno/plan-docs-...-x-ff83``). A legible
    branch that round-trips back to its node beats the opaque ``feature/<hex>``.
    """

    model_config = ConfigDict(extra="ignore")

    prefix: str = "fno"

    @field_validator("prefix")
    @classmethod
    def prefix_ref_safe(cls, v: str) -> str:
        """A ref prefix must be a non-empty, git-ref-safe path segment."""
        v = (v or "").strip().strip("/")
        if not v or ".." in v or any(c in v for c in " ~^:?*[\\"):
            raise ValueError(
                "config.branch.prefix must be a non-empty git-ref-safe segment"
            )
        return v


class MuxBlock(BaseModel):
    """fno mux (terminal multiplexer) settings (nested under 'config.mux').

    ``shell_integration`` (x-b63b) controls whether the mux auto-injects the
    OSC 133 block-marker snippet into the shells it spawns, so command-block
    capture works with zero user config and WITHOUT touching the user's global
    shell rc. The injection happens ONLY in mux-spawned pane shells (temp
    ``ZDOTDIR`` / ``--rcfile``), never in ``~/.zshrc`` / ``~/.bashrc``. Consumed
    by the Rust mux via ``FNO_MUX_SHELL_INTEGRATION``; absent env reads as
    ``mux-panes`` (the Rust default is on).

    ``notify_on_blocked`` / ``notify_on_done`` (x-dd84) fire an OS notification
    when a badge enters blocked / done. The Rust daemon reads these straight from
    settings.yaml (``agents_config.rs``, the same split-brain as
    ``config.agents.dead_row_grace``); modeling them here keeps every mux key
    discoverable via ``fno config get/set``, the wizard, and the generated docs.

    Fields
    ------
    shell_integration:
        ``mux-panes`` (default): inject into mux-spawned pane shells only.
        ``off``: never inject (the manual ``fno mux shell-init`` eval still
        works). Any other value coerces to ``mux-panes`` - only ``off`` is off,
        matching the Rust ``integration_disabled`` semantics.
    """

    model_config = ConfigDict(extra="ignore")

    shell_integration: str = "mux-panes"
    # Fire an OS notification when a badge ENTERS `blocked` (any authority: hook
    # report or screen-manifest verdict). Episode-gated: once per blocked spell.
    notify_on_blocked: bool = True
    # Also notify on a terminal `done` hook transition. Off by default.
    notify_on_done: bool = False
    # Catch-up digest on attach (x-4e2d): when a client attaches to a session it
    # last left more than `attach_digest_threshold_min` ago, render a
    # "while you were gone" overlay (fold of events + ledger) instead of raw
    # scrollback. Read straight from settings.yaml by the interactive Rust mux
    # client (no Python launcher on the attach path), same split-brain as
    # notify_on_blocked.
    attach_digest: bool = True
    # >= 1: a zero/negative threshold would make the overlay fire on every
    # attach (and the Rust reader parses it as u64, silently rejecting negatives
    # to the default), so pin the floor at 1 minute here.
    attach_digest_threshold_min: int = Field(default=10, ge=1)
    # Focus-follows-mouse over coding panes (x-a496): hovering a pane makes it the
    # keyboard focus after a short settle. Read straight from settings.yaml by the
    # interactive Rust client (same split-brain as attach_digest); modeled here so
    # the off-switch is discoverable via `fno config get/set`.
    hover_focus: bool = True

    @field_validator("shell_integration", mode="before")
    @classmethod
    def _coerce_shell_integration(cls, v: object) -> object:
        """Only ``off`` disables; anything else is ``mux-panes``.

        YAML 1.1 (PyYAML's default) parses an unquoted ``off``/``no``/``false``
        as boolean ``False``, so ``shell_integration: off`` reaches here as
        ``False``, not ``"off"`` - treat that as off too, else the user's
        disable is silently ignored.
        """
        if v is False:
            return "off"
        return "off" if isinstance(v, str) and v.strip() == "off" else "mux-panes"


class ConfigBlock(BaseModel):
    """Top-level config block (nested under 'config:' in settings.yaml)."""

    model_config = ConfigDict(extra="ignore")

    state_dir: str = "~/.fno/"
    plans_dir: str = ".fno/plans/"
    branch: BranchBlock = Field(default_factory=BranchBlock)
    paths: PathsBlock = Field(default_factory=PathsBlock)
    obsidian: ObsidianBlock = Field(default_factory=ObsidianBlock)
    project: ProjectBlock = Field(default_factory=ProjectBlock)
    blueprint: BlueprintBlock = Field(default_factory=BlueprintBlock)
    backlog: BacklogBlock = Field(default_factory=BacklogBlock)
    batch: BatchBlock = Field(default_factory=BatchBlock)
    post_merge: PostMergeBlock = Field(default_factory=PostMergeBlock)
    research: ResearchBlock = Field(default_factory=ResearchBlock)
    review: ReviewBlock = Field(default_factory=ReviewBlock)
    target: TargetConfig = Field(default_factory=TargetConfig)
    agents: AgentsBlock = Field(default_factory=AgentsBlock)
    auto_continue: AutoContinueBlock = Field(default_factory=AutoContinueBlock)
    think_spawn: ThinkSpawnBlock = Field(default_factory=ThinkSpawnBlock)
    active_backlog: ActiveBacklogConfig = Field(default_factory=ActiveBacklogConfig)
    parallel: ParallelBlock = Field(default_factory=ParallelBlock)
    auto_merge: AutoMergeBlock = Field(default_factory=AutoMergeBlock)
    pr_watch: PrWatchBlock = Field(default_factory=PrWatchBlock)
    recovery: RecoveryBlock = Field(default_factory=RecoveryBlock)
    health_monitor: HealthMonitorBlock = Field(default_factory=HealthMonitorBlock)
    collision: CollisionBlock = Field(default_factory=CollisionBlock)
    work: WorkBlock = Field(default_factory=WorkBlock)
    model_routing: ModelRoutingBlock = Field(default_factory=ModelRoutingBlock)
    mux: MuxBlock = Field(default_factory=MuxBlock)
    loops: dict[str, LoopEntry] = Field(default_factory=dict)

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

    @field_validator("mux", mode="before")
    @classmethod
    def _coerce_mux(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``mux:`` degrades to defaults.

        Mirrors ``_coerce_model_routing``: ``mux: off`` (a scalar, list, or
        null) cannot build the block; fall back to defaults rather than raising
        out of the whole settings load (which would break every `fno` command).
        A dict passes through so the inner ``shell_integration`` coercer runs.
        """
        if isinstance(v, (dict, MuxBlock)):
            return v
        return {}

    @field_validator("branch", mode="before")
    @classmethod
    def _coerce_branch(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``branch:`` degrades to defaults (prefix fno)."""
        if isinstance(v, (dict, BranchBlock)):
            return v
        return {}

    @field_validator("loops", mode="before")
    @classmethod
    def _coerce_loops(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``loops:`` degrades to {}, and a malformed
        per-loop entry (e.g. ``loops: {my-loop: assisted}`` or ``null`` instead
        of ``{level: ...}``) is dropped rather than raising - a config typo
        must never crash settings load for the whole project; the dropped
        loop just defaults to level "report" via absence."""
        if not isinstance(v, dict):
            return {}
        coerced = {}
        for name, entry in v.items():
            if isinstance(entry, (dict, LoopEntry)):
                coerced[name] = entry
            else:
                _LOG.warning(
                    "config.loops.%s=%r is not a mapping; dropping (defaults to level 'report')",
                    name, entry,
                )
        return coerced

    @field_validator("batch", mode="before")
    @classmethod
    def _coerce_batch(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``batch:`` degrades to defaults (disabled).

        Mirrors ``_coerce_auto_merge``: ``batch: 42`` (or a list, or null)
        cannot build the block; fall back to the default disabled block rather
        than raising out of the whole settings load (which would break every
        `fno` command, not just batching). A dict passes through so the inner
        field coercers still run.
        """
        if isinstance(v, (dict, BatchBlock)):
            return v
        return {}

    @field_validator("recovery", mode="before")
    @classmethod
    def _coerce_recovery(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``recovery:`` degrades to defaults.

        ``recovery: true`` (or a list, or null) cannot build the block; fall
        back to defaults rather than raising out of the whole settings load.
        A dict passes through so field defaults/validators still apply.
        """
        if isinstance(v, (dict, RecoveryBlock)):
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

    @field_validator("think_spawn", mode="before")
    @classmethod
    def _coerce_think_spawn(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``think_spawn:`` degrades to defaults (OFF).

        Mirrors ``_coerce_auto_continue``: ``think_spawn: 42`` (or a list, or
        null) cannot build the block; fall back to the default disabled block
        (AC4-ERR) rather than raising out of the whole settings load. A dict
        passes through so the inner coercers still run.
        """
        if isinstance(v, (dict, ThinkSpawnBlock)):
            return v
        return {}

    @field_validator("active_backlog", mode="before")
    @classmethod
    def _coerce_active_backlog(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``active_backlog:`` degrades to defaults (OFF).

        Mirrors ``_coerce_auto_continue``: ``active_backlog: 42`` (or a list, or
        null) cannot build the block; fall back to the default disabled block
        rather than raising out of the whole settings load. A dict passes
        through so the inner field coercers still run.
        """
        if isinstance(v, (dict, ActiveBacklogConfig)):
            return v
        return {}

    @field_validator("parallel", mode="before")
    @classmethod
    def _coerce_parallel(cls, v: object) -> object:
        """Fail-safe: a non-mapping ``parallel:`` degrades to defaults (max_lanes=1).

        Mirrors ``_coerce_active_backlog``: a scalar/list/null cannot build the
        block; fall back to the default (sequential) rather than raising out of
        the whole settings load. A dict passes through so ``_coerce_max_lanes``
        still runs.
        """
        if isinstance(v, (dict, ParallelBlock)):
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


def _unwrap_config_dict(raw: dict[str, object]) -> dict[str, object]:
    """Normalize a settings dict to the FLAT canonical shape.

    Config fields live at the top level now (flat config.toml). Legacy files
    nest every setting under a ``config:`` key; lift those to the top level so
    both shapes validate into the same flat model. Top-level siblings the model
    ignores (``worktree``) and ``schema_version`` are preserved; a ``config``
    block key wins over a stray top-level key of the same name (canonical beats
    legacy). No-ops on an already-flat dict.
    """
    if not isinstance(raw, dict):
        return raw
    cfg = raw.get("config")
    # Accept a ConfigBlock instance too (e.g. SettingsModel(config=ConfigBlock(...))
    # in tests), not just a parsed dict. exclude_unset preserves which fields were
    # explicitly set, so partial-override semantics (e.g. a provider entry that
    # overrides only base_url, keeping the built-in api_key_env) survive.
    if isinstance(cfg, BaseModel):
        cfg = cfg.model_dump(exclude_unset=True)
    if not isinstance(cfg, dict):
        return raw
    rest = {k: v for k, v in raw.items() if k != "config"}
    return _deep_merge(rest, cfg)


class SettingsModel(ConfigBlock):
    """Root settings model. Config fields live at the TOP level (flat).

    Historically every setting nested under a ``config:`` key; the file is now
    flat (``config.toml`` with top-level blocks). ``SettingsModel`` inherits
    ``ConfigBlock`` so ``settings.review`` / ``settings.project`` resolve
    directly. The legacy ``config:``-wrapped shape still loads via the
    ``_unwrap`` before-validator (so ``SettingsModel(config={...})`` and old
    YAML both work).
    """

    # schema_version versions the file format, not behavior.
    schema_version: int = 1

    @model_validator(mode="before")
    @classmethod
    def _unwrap(cls, data: object) -> object:
        if isinstance(data, dict):
            return _unwrap_config_dict(data)
        return data


# Keys a per-worktree `.fno/config.local.toml` may override on top of the
# shared (symlinked) config.toml. setup-worktree.sh symlinks config.toml
# from canonical into every worktree so backlog/ledger config stays coherent,
# which also forces these collision-prone keys to be shared. The local file is
# the one file kept per-worktree; it may diverge ONLY on this allowlist. A key
# outside it in the local file is ignored (with a warning) so a local file can
# never silently fork shared config. Dotted, FLAT (post-unwrap) paths - the
# config.local.toml is flat, so no `config.` prefix.
WORKTREE_LOCAL_KEYS: frozenset[str] = frozenset(
    {
        "post_merge.parking_lot_path",
        "project.id",
    }
)


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


# ---------------------------------------------------------------------------
# One-shot yaml -> flat-toml migration (Locked Decision #4: the load-time
# safety net for the hard cut). This module is the ONLY place PyYAML survives
# at load time - a legacy settings.yaml is converted to a flat config.toml
# exactly once, then every reader (Python, Rust, shell) sees config.toml.
# ---------------------------------------------------------------------------


def _strip_none(data: object) -> object:
    """Recursively drop None-valued keys. TOML cannot represent null, and the
    loader treats an absent key as its default (== None), so stripping None is
    lossless and keeps ``tomli_w`` from ever choking on an unserializable value.
    """
    if isinstance(data, dict):
        return {k: _strip_none(v) for k, v in data.items() if v is not None}
    if isinstance(data, list):
        return [_strip_none(v) for v in data]
    return data


def _atomic_write_toml(target: Path, data: dict[str, object]) -> None:
    """Write ``data`` as flat TOML to ``target`` via temp file + ``os.replace``.

    Writes THROUGH a symlink to its real target so a worktree link (pointing at
    the canonical config) is preserved rather than clobbered by the rename. The
    temp file lands in the real target's directory so the rename is atomic (same
    filesystem); on any failure the temp file is unlinked and the original is
    left intact.
    """
    if target.is_symlink():
        target = Path(os.path.realpath(target))
    target.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_str = tempfile.mkstemp(
        dir=str(target.parent), prefix=f".{target.name}.tmp.", suffix=".part"
    )
    tmp = Path(tmp_str)
    try:
        with os.fdopen(fd, "wb") as f:
            clean = cast("dict[str, Any]", _strip_none(data))
            f.write(tomli_w.dumps(clean).encode("utf-8"))
        os.replace(str(tmp), str(target))
    except BaseException:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise


def _flat_from_yaml_file(path: Path) -> dict[str, object]:
    """Read a legacy settings.yaml and return its FLAT (unwrapped) dict.

    Raises ``yaml.YAMLError`` on a malformed file so the caller can leave the
    original in place rather than convert junk.
    """
    raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    if not isinstance(raw, dict):
        return {}
    return _unwrap_config_dict(raw)


def _migrate_yaml_to_toml(
    yaml_path: Path, toml_name: str = "config.toml"
) -> Optional[Path]:
    """One-shot convert a legacy YAML config to a flat TOML sibling.

    Idempotent: returns the toml path (no rewrite) when the toml already exists,
    so a second migrator - or a re-run - is a no-op (AC3-ERR). Atomic + crash
    safe: the toml is written temp+rename, and the yaml is deleted ONLY after the
    toml is durable, so an interrupt leaves a readable yaml and no partial toml
    (AC3-FR / AC4-FR). Converges under concurrency: two racers either short
    circuit on the existing toml or write identical content.

    ``yaml_path`` may be a symlink (a worktree's link to the canonical config);
    the toml is written next to the *real* file so every linked worktree
    resolves the same config.toml.
    """
    real_yaml = (
        Path(os.path.realpath(yaml_path)) if yaml_path.is_symlink() else yaml_path
    )
    toml_path = real_yaml.with_name(toml_name)
    if toml_path.exists():
        return toml_path
    if not real_yaml.is_file():
        return None
    try:
        data = _flat_from_yaml_file(real_yaml)
    except yaml.YAMLError:
        _LOG.warning(
            "config migrate: %s is malformed; leaving it in place", real_yaml
        )
        return None
    _atomic_write_toml(toml_path, data)
    try:
        real_yaml.unlink()
    except FileNotFoundError:
        pass  # a concurrent migrator already removed it
    return toml_path


def run_config_migration(
    locations: Optional[list[Path]] = None,
) -> list[tuple[Path, str]]:
    """Convert every legacy settings.yaml (+ its settings.local.yaml) in the
    candidate chain to a flat config.toml, returning ``(toml_path, action)`` per
    file for the CLI to report. ``action`` is ``migrated`` | ``already-migrated``
    | ``absent``. The explicit ``fno setup migrate-config`` verb over the same
    one-shot conversion the loader runs, so a deployed install converts on demand
    (comments are NOT carried across the round-trip). Idempotent + atomic.
    """
    out: list[tuple[Path, str]] = []
    seen: set[str] = set()
    for yaml_path in locations if locations is not None else _settings_yaml_locations():
        if yaml_path.name != "settings.yaml":
            continue
        real = (
            Path(os.path.realpath(yaml_path)) if yaml_path.is_symlink() else yaml_path
        )
        for src, toml_name in (
            (yaml_path, "config.toml"),
            (real.with_name("settings.local.yaml"), "config.local.toml"),
        ):
            toml_path = real.with_name(toml_name)
            if str(toml_path) in seen:
                continue
            seen.add(str(toml_path))
            # settings.local.yaml is a real per-worktree file; skip a symlinked one.
            if toml_name == "config.local.toml" and (
                not src.is_file() or src.is_symlink()
            ):
                continue
            if toml_path.exists():
                out.append((toml_path, "already-migrated"))
            elif _migrate_yaml_to_toml(src, toml_name) is not None:
                out.append((toml_path, "migrated"))
            else:
                out.append((toml_path, "absent"))
    return out


def read_config_flat(path: Path) -> dict[str, object]:
    """Read a single config file (config.toml -> TOML by suffix, else YAML) and
    return its FLAT dict (a legacy ``config:`` wrapper unwrapped). Returns {} on a
    missing or unparseable file. For the handful of consumers that read the
    config file DIRECTLY - the work.workspaces topology map, project detection -
    instead of through the cached ``load_settings`` (they need global-only reads,
    no per-process cache, or run at bootstrap).
    """
    data, ok = _load_raw(path)
    return _unwrap_config_dict(data) if ok else {}


def config_read_candidates(paths: list[Path]) -> list[Path]:
    """config.toml-first read candidates for a list of settings.yaml locations
    (public alias of ``_prefer_toml`` for direct-file readers)."""
    return _prefer_toml(paths)


def _ensure_migrated(locations: list[Path]) -> None:
    """Convert every legacy settings.yaml (+ its settings.local.yaml) in
    ``locations`` to a flat config.toml, once. The load-time hard-cut safety net:
    an unmigrated install is transparently converted on first config load, then
    read as TOML. NOT a steady-state dual-read - the fast path is the
    config.toml-exists short-circuit inside ``_migrate_yaml_to_toml``.

    Skipped entirely when ``$FNO_CONFIG`` pins an explicit path: an explicitly
    handed file is read as-is (``_load_raw`` still parses YAML by suffix), never
    migrated out from under the caller.
    """
    if os.environ.get("FNO_CONFIG"):
        return
    for yaml_path in locations:
        if yaml_path.name != "settings.yaml":
            continue
        _migrate_yaml_to_toml(yaml_path)
        local_yaml = yaml_path.with_name("settings.local.yaml")
        if local_yaml.is_file() and not local_yaml.is_symlink():
            _migrate_yaml_to_toml(local_yaml, "config.local.toml")


def _settings_yaml_locations() -> list[Path]:
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
    return _apply_search_ceiling(candidates)


def _apply_search_ceiling(candidates: list[Path]) -> list[Path]:
    """Drop candidates that resolve outside $FNO_CONFIG_SEARCH_ROOT.

    No-op when unset (all real usage). Tests set it to their tmpdir roots so the
    git-derived canonical candidate can never reach the developer's real checkout
    (repo_root/canonical climb via ``git worktree list``, which no HOME redirect
    can bound). Value is an os.pathsep-separated list because two legitimate test
    roots exist: the pytest basetemp and the redirected HOME.
    """
    raw = os.environ.get("FNO_CONFIG_SEARCH_ROOT")
    if not raw:
        return candidates
    roots: list[Path] = []
    for r in raw.split(os.pathsep):
        if not r:
            continue
        try:
            roots.append(Path(r).resolve())
        except OSError:
            pass
    if not roots:
        return candidates
    # resolve() is filesystem I/O (symlink walk) and can raise OSError on a
    # symlink loop / permission wall; config load is on every command's startup
    # path, so resolve each candidate once and degrade (drop) rather than crash.
    kept: list[Path] = []
    for c in candidates:
        try:
            resolved = c.resolve()
        except OSError:
            continue
        if any(resolved.is_relative_to(r) for r in roots):
            kept.append(c)
    return kept


def _candidate_paths() -> list[Path]:
    """Ordered read candidates for the loader, config.toml-first.

    Runs the one-shot yaml->toml auto-migrate over the settings locations, then
    returns each location's ``config.toml`` (with the legacy ``settings.yaml``
    kept as a passthrough fallback for the rare case migrate could not convert,
    e.g. a malformed file). After a successful migrate only the config.toml
    exists at each location, so the read is effectively toml-only.
    """
    locations = _settings_yaml_locations()
    _ensure_migrated(locations)
    return _prefer_toml(locations)


def _prefer_toml(paths: list[Path]) -> list[Path]:
    """For each ``settings.yaml`` candidate, try its ``config.toml`` sibling first.

    Adds the new flat-TOML file as a higher-priority read candidate wherever the
    legacy YAML was a candidate, so a ``config.toml`` wins per-key while an
    existing ``settings.yaml`` still loads. Env-pinned non-YAML paths (e.g.
    ``/dev/null`` for test isolation) get no sibling and pass through untouched.
    """
    out: list[Path] = []
    for p in paths:
        if p.name == "settings.yaml":
            toml = p.with_name("config.toml")
            if toml not in out:
                out.append(toml)
        if p not in out:
            out.append(p)
    # Bound the final list too: direct-file readers (e.g. _intake's
    # project<->path map) reach this via config_read_candidates without going
    # through _settings_yaml_locations, and a cwd-relative candidate resolves
    # through a worktree symlink to the canonical checkout (the leak this fixes).
    return _apply_search_ceiling(out)


# Module-level variable recording the path that load_settings() actually read.
# None until the first successful load_settings() call.
# Exposed via loaded_from() so paths.config_file() can return the actual
# path without re-deriving from state_dir (Finding 3 fix).
_loaded_from: Optional[Path] = None


def loaded_from() -> Optional[Path]:
    """Return the Path that load_settings() actually read, or None if not loaded yet."""
    return _loaded_from


def _load_raw(path: Path) -> tuple[dict[str, object], bool]:
    """Load a settings file and return (data, parse_succeeded).

    Parses TOML for a ``.toml`` suffix (config.toml), YAML otherwise
    (settings.yaml). Returns ({}, False) on any OS or parse error so callers
    can fall through to the next candidate. Logs a WARNING on parse failure so
    the user knows their config was not applied.

    Returns (data, True) when the file parsed successfully (even if the dict
    is empty, i.e. the file was blank).
    """
    try:
        text = path.read_text(encoding="utf-8")
        if path.suffix == ".toml":
            data = tomllib.loads(text)
        else:
            data = yaml.safe_load(text)
        return (data if isinstance(data, dict) else {}, True)
    except (OSError, UnicodeDecodeError, yaml.YAMLError, tomllib.TOMLDecodeError) as exc:
        _LOG.warning(
            "config file at %s failed to parse: %s; using defaults",
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


def _flatten_leaf_paths(
    data: dict[str, object], prefix: str = ""
) -> list[tuple[str, object]]:
    """Yield (dotted_path, value) for every leaf (non-dict) in a nested dict."""
    out: list[tuple[str, object]] = []
    for key, value in data.items():
        # Coerce non-string keys (YAML allows int/bool/None keys, e.g. `1: x`)
        # so the dotted path is always a str - otherwise sorted()/join() over the
        # ignored-keys warning list would TypeError and crash config loading.
        key_str = str(key)
        path = f"{prefix}.{key_str}" if prefix else key_str
        if isinstance(value, dict):
            out.extend(_flatten_leaf_paths(value, path))
        else:
            out.append((path, value))
    return out


def _worktree_local_override(local_raw: dict[str, object]) -> dict[str, object]:
    """Filter a settings.local.yaml dict down to the WORKTREE_LOCAL_KEYS allowlist.

    Returns a nested override dict containing ONLY allowlisted leaf paths, to be
    deep-merged on top of the shared settings. Any non-allowlisted leaf is
    dropped and reported in one WARNING (stderr), so a per-worktree local file
    can never silently fork a shared key (backlog graph, ledger, reviewers).
    """
    override: dict[str, object] = {}
    ignored: list[str] = []
    for path, value in _flatten_leaf_paths(local_raw):
        if path in WORKTREE_LOCAL_KEYS:
            *parents, leaf = path.split(".")
            cursor = override
            for parent in parents:
                nxt = cursor.get(parent)
                if not isinstance(nxt, dict):
                    nxt = {}
                    cursor[parent] = nxt
                cursor = nxt
            cursor[leaf] = value
        else:
            ignored.append(path)
    if ignored:
        _LOG.warning(
            "config.local.toml: ignoring non-worktree-local key(s): %s. "
            "Only %s may be overridden per-worktree; other keys stay shared.",
            ", ".join(sorted(ignored)),
            ", ".join(sorted(WORKTREE_LOCAL_KEYS)),
        )
    return override


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
    had_config = isinstance(config, dict)
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
    # Only a genuinely legacy file (a `config:` block present) treats top-level
    # `project` as the deprecated location. In the flat canonical shape there is
    # no `config:` block and top-level `project` IS canonical - skip silently so
    # a flat config.toml never draws a spurious deprecation warning.
    top_project = raw.get("project")
    if had_config and isinstance(top_project, dict):
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
    # As with `project`, only alias when a legacy `config:` block is present;
    # flat-shape top-level `work` is canonical.
    top_work = raw.get("work")
    if had_config and isinstance(top_work, dict) and not isinstance(config.get("work"), dict):
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

    # Per-worktree local override (x-cbce). Layer an optional real (non-symlinked)
    # config.local.toml, sitting in the same .fno/ as the primary config.toml,
    # on top of the merged config - but ONLY for WORKTREE_LOCAL_KEYS. This is the
    # one file setup-worktree.sh never symlinks, so sibling worktrees can diverge
    # on parking_lot_path / project.id while everything else stays shared via the
    # symlinked config.toml. Absent (or symlinked) file => no-op, behavior
    # unchanged. A symlinked local file is skipped: it would defeat the whole
    # point (re-sharing the collision-prone keys across worktrees).
    candidates = _candidate_paths()
    if candidates:
        local_path = candidates[0].parent / "config.local.toml"
        if local_path.is_file() and not local_path.is_symlink():
            local_parsed, ok = _load_raw(local_path)
            if ok:
                override = _worktree_local_override(local_parsed)
                if override:
                    raw = _deep_merge(raw, override)

    # _loaded_from records the PRIMARY (highest-priority) file present, for
    # `fno config doctor` and paths.config_file(). With layering there is no
    # single source; the highest-priority file is the most meaningful anchor
    # (Finding 3: paths.config_file must agree with the loader, not re-derive).
    _loaded_from = layers[0][0] if layers else None

    # Flatten the legacy config:-wrapped shape to the canonical top-level shape
    # before warning/validation so unknown-key warnings key off real block names
    # (the model is flat; a residual `config` key would look "unknown").
    raw = _unwrap_config_dict(raw)

    # Warn about unknown top-level and nested keys BEFORE model construction
    # so the message appears even if validation later raises.
    # The recursive walker handles nested blocks (paths, review, etc.) automatically;
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
      <repo_root>/.fno/config.toml -> ~/.fno/config.toml -> built-in defaults.
    """
    layers: list[tuple[Path, dict[str, object]]] = []

    locations = [
        repo_root / ".fno" / "settings.yaml",
        _global_settings_path(),
    ]
    _ensure_migrated(locations)
    candidates = _prefer_toml(locations)
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
        block = getattr(load_settings().agents, provider, None)
    except Exception:
        return False
    if block is None:
        return False
    return bool(getattr(block, "headless_yolo", False))
