"""fno.agents.registry — JSON agent registry with atomic-rename + flocks.

Storage substrate for `fno agents`. Two lock scopes:

- **Per-agent flock** (``_agent_lock_path``): callers in dispatch.py hold
  this around a single agent's subprocess invocation so two ``ask`` calls
  for the same name serialize end-to-end (claude -bg + supervisor probe).
- **Registry-wide flock** (``_registry_lock_path``): held inside
  ``update_registry`` to make the load-modify-write cycle atomic across
  different agent names. Without it, two concurrent ``ask`` calls for
  DIFFERENT agents could both ``load_registry`` -> mutate -> ``write``
  and the loser's update would be lost (Codex review on PR #288 P1).

Use ``update_registry(name, updater)`` for any production read-modify-write;
``write_registry`` stays as the low-level primitive (also handy in tests).

``write_registry`` uses an atomic temp-file + ``os.replace`` so a kill -9
mid-write cannot corrupt the existing file. Schema version is bumped any
time the on-disk shape changes.

An OLDER on-disk schema is read transparently. A NEWER one is read
forward: this file is global to every agent on the machine, so refusing it
meant one process running ahead of the deployment bricked every deployed
reader at once. Above our own version, a row this fno cannot represent is
skipped rather than fatal, and the skip is announced. What makes that safe
is ``write_registry`` REFUSING while the on-disk schema is higher, since a
read that drops what it cannot see must never write those rows back.

Malformed JSON, a missing or non-integer ``schema_version``, non-dict
rows, and rows with no valid identity token all still surface as
``RegistryVersionError`` — callers handle alien shape through one
exception type. Reading forward covers a version gap, never a torn file.
"""
from __future__ import annotations

import contextlib
import fcntl
import json
import os
import re
import sys
import time
from dataclasses import asdict, dataclass, field, fields
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Iterator, Literal, Optional

from fno import paths
from fno.harness_identity import (
    canonical_handle,
    legacy_suffix_handle,
    session_handle_tier,
    session_identity_key,
    sync_harness_aliases,
)
from fno.time_budget import validate_timeout_budget

# registry.status is a projection of state.status (LD10), so it can be ANY
# AgentStatus variant. The daemon writes "live" on spawn and "exited" on child
# exit (retained until rm), and reconcile writes "orphaned". The earlier
# {live, orphaned} set was too narrow — it hard-errored every registry read
# once an exited row was present, bricking all Python `fno agents` commands
# until the row was rm'd via the Rust binary. This is the full snake_case
# AgentStatus vocabulary (mirrors crates/fno-agents/src/lib.rs AgentStatus and
# the status-v1 schema); it accepts every valid projected status while still
# rejecting garbage. Must stay in lockstep with the Rust strict reader
# (crates/fno-agents/src/client_verbs.rs::KNOWN_STATUSES).
AgentStatus = Literal[
    "spawning",
    "ready",
    "idle",
    "busy",
    "live",
    "restarting",
    "orphaned",
    "failed",
    "exited",
    "permanent_dead",
]
KNOWN_STATUSES = frozenset(
    {
        "spawning",
        "ready",
        "idle",
        "busy",
        "live",
        "restarting",
        "orphaned",
        "failed",
        "exited",
        "permanent_dead",
    }
)

# The statuses that mean "this row will never act again". Lives here, next to the
# vocabulary it is a subset of, because three call sites need the SAME answer and
# a drifted copy is a real defect: the one-live-crown guards (spawn --crown on
# both substrates, and `fno agents crown`) read "not terminal" as "still reigning",
# so a set that forgets a status mints a second crown over one scope.
TERMINAL_STATUSES = frozenset({"exited", "orphaned", "failed", "permanent_dead"})

# The crown's MEANING (the ladder, scope encoding, derivation, validation) lives
# in fno.agents.crown, not here: this module owns the three fields on the row and
# nothing about what they signify. Storage does not get to define authority.

# Valid host_mode values (interactive-drive node). A missing/null key coerces to
# "exec" in load_registry; any other concrete value is rejected like an alien
# status, so a typo ("intractive") cannot silently fall back to exec behavior.
# "attached" is an ADOPTED claude --bg session footnote drives over the daemon
# control.sock (G1 held-attach substrate, x-26df) -- its process is Claude's, not
# footnote's, so it is neither "exec" (one-shot) nor "interactive" (a
# footnote-spawned PTY worker); listed here so a row the Rust adopt path writes
# stays load_registry-readable from Python instead of bricking the registry.
KNOWN_HOST_MODES = frozenset({"exec", "interactive", "attached"})

# Single source of truth for "which stored field is a harness's resume
# target". Consumed by both AgentEntry.session_id (real entries) and
# resume_cli._session_id_for (duck-typed against test fakes), so the
# harness -> field mapping lives in exactly one place and cannot drift
# between the two. Keyed on the row's harness (x-8dfc): identity is one axis.
# At v10 (x-880e) the legacy per-provider id fields are gone, so codex/gemini
# resume off the canonical harness_session_id; claude still attaches by the
# 8-hex jobId in short_id (a distinct transport key, not removed).
HARNESS_SESSION_ID_FIELDS = {
    "claude": "short_id",
    "codex": "harness_session_id",
    "gemini": "harness_session_id",
    "opencode": "harness_session_id",
}

# The registry's legacy per-harness session-id keys (x-ec59). Distinct from the
# manifest's map (which uses claude_session_id): the registry's claude identity
# lives in claude_session_uuid. Passed to the shared sync_harness_aliases rule so
# canonical harness_session_id and these legacy fields stay in lockstep on load.
REGISTRY_LEGACY_SESSION_KEYS = {
    "claude": "claude_session_uuid",
    "codex": "codex_session_id",
    "gemini": "gemini_session_id",
}

# v4 (ab-a171ceb2) is the host_mode forward-compat bump. v5 (inside-out E3.1) is
# the same kind of bump for the additive `inside_leg` field: structurally
# identical to v4 (inside_leg is additive-optional, an absent key reads as None),
# but stamping v5 makes a pre-inside-leg reader (which accepts only {1,2,3,4})
# reject a v5 store instead of silently dropping the inside-leg report on
# write-back. Reads stay backward-compatible: load_registry accepts
# 1..=SCHEMA_VERSION. v6 (4a-G2) is the mux-ref bump; v7 (screen-manifest
# fallback authority) the same bump for the additive `screen_state` verdict.
# v8 (x-ec59) is the canonical-identity bump for `harness` / `harness_session_id`:
# every Python-authored row emits these keys, so a pre-v8 reader must REJECT the
# store (clean "upgrade fno") rather than accept the version and then TypeError on
# the unknown AgentEntry kwargs (the PR #364 brick) or silently drop the fields on
# a Rust read-modify-write. Same forward-compat rationale as the v4-v7 bumps.
# v9 removes `claude_short_id`: the claude jobId (a pure prefix of the session
# UUID) now lives in `short_id`, unifying the transport-key field across
# providers. Legacy rows backfill on load (see load_registry); a pre-v9 reader
# must reject a v9 store rather than drop the jobId on write-back.
# v10 (x-880e) removes the on-disk `provider` field and the legacy per-provider
# session-id trio (`codex_session_id`, `gemini_session_id`, `claude_session_uuid`):
# `harness` is the sole identity axis and `harness_session_id` the sole session
# id. A legacy row's `provider` back-fills `harness`, and each per-provider key
# back-fills `harness_session_id`, at load (the accept-on-read pattern) and the
# key dies there. A pre-v10 reader must reject a v10 store rather than mis-read
# a harness-only row.
# v11 (US9): additive crown fields (crown_level/crown_scope/crown_grantor).
# asdict emits them as null on every written row, so a pre-v11 reader must
# reject the store rather than TypeError on the unknown keys.
# v12 (x-ae2d): additive `route_settings_path` - the route-settings file a
# routed worker was LAUNCHED with, so a relaunch can re-apply it instead of
# silently coming back on the default account. Same additive-optional shape and
# same forward-compat rationale as v11: asdict emits the key on every written
# row, so a pre-v12 reader must reject the store rather than TypeError on it.
# v13 (x-0358): additive `fno_id` - the target run id of an adopted /target
# orphan, linking a revived session to its node. Set by the Rust adopt verb from
# the matched target manifest; None otherwise. Same additive-optional shape and
# forward-compat rationale as v12.
# v14 (x-e21e): additive `delivery_policy` - a recipient's mail delivery
# policy ("bus-only": never prompt-line inject, always durable bus). Same
# additive-optional shape and same forward-compat rationale as v12/v13: asdict
# emits the key on every written row, so a pre-v14 reader must reject the
# store rather than TypeError on the unknown kwarg.
# v15 restores `provider` with its literal model-provider meaning. It is
# intentionally distinct from `harness`: a routed worker can have
# harness="claude" and provider="zai", while "opencode" is valid on either
# axis. Rows from v1..v14 retain the legacy meaning where `provider` was a
# harness alias and are migrated only while reading those schema versions.
SCHEMA_VERSION = 15


class RegistryVersionError(RuntimeError):
    """Raised when a registry file's schema_version != SCHEMA_VERSION."""


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# Indirection so AC2-ERR can monkeypatch this symbol to simulate kill -9.
# Looked up via module attribute at call time, NOT closed over.
_json_dumps = json.dumps


@dataclass
class AgentEntry:
    """One row in the registry — a named agent session.

    Schema v2 (US2) adds ``status`` and ``last_message_at``:

    - ``status`` is ``"live"`` while the agent's messaging socket is
      reachable; flipped to ``"orphaned"`` by US2's follow-up path when
      ``locate_session`` or the 250 ms liveness probe fails.
    - ``last_message_at`` is the UTC ISO timestamp of the most recent
      successful follow-up send (bumped post-send, monotone per
      ``update_registry`` flock).

    Schema v3 (Phase 5 US6) adds ``mcp_channel_id``:

    - ``mcp_channel_id`` is the server-generated UUIDv4 the fno
      MCP sidecar uses to route inbound pokes to the session that was
      launched with ``--channels fno``. ``None`` for legacy
      (US2/socket-only) sessions; ``str`` for MCP-backed sessions. Only
      ``register_mcp_channel`` (dispatch.py) writes this field; no other
      code path mutates it (spec invariant).
    """

    name: str
    cwd: str
    log_path: str
    # Canonical harness identity (x-880e, v10). `provider` below is a separate
    # model-provider axis from v15 onward; neither value may be inferred from the
    # other because names such as "opencode" are valid on both axes.
    # ``harness_session_id`` (below) is the worker's own session id in its harness's
    # store (claude full UUID, codex thread id, gemini session id). load_registry
    # back-fills both from a legacy row's ``provider`` / per-provider keys on read.
    harness: str
    provider: Optional[str] = None
    created_at: str = field(default_factory=_utc_now_iso)
    status: AgentStatus = "live"
    last_message_at: Optional[str] = None
    mcp_channel_id: Optional[str] = None
    # host_mode: "exec" (one-shot, the default for every existing row) or
    # "interactive" (a long-lived drivable TUI hosted by the Rust daemon via
    # `fno agents host`/`promote`). load_registry coerces a missing key or an
    # explicit null to "exec" before constructing the entry, so a concrete mode
    # always reaches consumers (never None). The Rust RegistryEntry mirrors this
    # with #[serde(default, skip_serializing_if = "Option::is_none")], so a row
    # round-trips between the two languages: Rust omits the key for exec rows and
    # Python's coercion maps the absence back to "exec". [interactive-drive node]
    host_mode: Optional[str] = None
    # The worker's own session id in its harness's store (claude full UUID, codex
    # thread id, gemini session id) -- the canonical successor to the removed
    # per-provider session-id fields (x-880e). load_registry back-fills it from a
    # legacy row's per-provider key on read; the Rust RegistryEntry mirrors it.
    harness_session_id: Optional[str] = None
    # Spawn-time parent edge (Task 2.2, x-30f6). Ambient-captured from the
    # SPAWNING session's environment; never required of a caller. All three
    # default to None so pre-existing rows and callers that pass none of them
    # round-trip safely (additive-optional: the Rust crate has no
    # deny_unknown_fields, so it ignores these keys on read).
    #   spawned_by_session — the parent session id (CLAUDE_CODE_SESSION_ID /
    #                        CODEX_SESSION_ID / GEMINI_SESSION_ID, whichever
    #                        is set; claude takes precedence if multiple are).
    #   spawned_by_harness — "claude" | "codex" | "gemini"; None when no
    #                        session env var is present.
    #   spawned_by_cwd     — parent $PWD at spawn time.
    spawned_by_session: Optional[str] = None
    spawned_by_harness: Optional[str] = None
    spawned_by_cwd: Optional[str] = None
    # x-42c5: the CAUSE of the spawn, distinct from spawned_by_* above (which
    # identify WHO called `fno agents spawn`, not WHY). An automated dispatcher
    # sets FNO_SPAWN_TRIGGER before shelling out so the subprocess's own
    # environment carries the reason (ambient-captured here the same way
    # spawned_by_* is); a human-run `fno agents spawn` never sets it, so
    # None/absent means "an operator asked for this directly." Format is
    # "<dispatcher>:<reason>", e.g. "think_spawn:work-start" or
    # "think_spawn:conversational" - the only producer today is
    # fno.provenance.spawn_think._spawn_think_worker. Before x-42c5 the only
    # evidence for "did a birth trigger spawn this?" was a timestamp gap
    # between a node's created_at and a worker's registry row.
    spawn_trigger: Optional[str] = None
    # Registration origin: "operator" for a session a human started by hand (the
    # SessionStart register hook / ``fno agents register``); absent for a
    # spawn/host worker row. Additive-optional (None default) so pre-existing
    # rows and the Rust RegistryEntry round-trip losslessly, and absence reads
    # as worker -- the attended-miss escalation lane fails toward silence.
    origin: Optional[str] = None

    # ----------------------------------------------------------------------
    # Rust-daemon-only PTY fields (ab-b946b59c). A genuine daemon PTY row
    # (spawn/host/promote) carries a non-empty short_id/project_root + pid +
    # worker socket, etc. PR #364 made a *round-tripped Python* row omit these
    # (Rust's skip_serializing_if drops them when empty/None), but a real PTY
    # row in a MIXED registry still serializes them with values -- and the
    # earlier AgentEntry, lacking these init fields, made `AgentEntry(**row)`
    # raise TypeError, which load_registry maps to RegistryVersionError, bricking
    # EVERY Python `fno agents` read. Mirroring the fields here lets Python read
    # a Rust PTY row AND preserve it losslessly on write-back (asdict re-emits
    # them; the Rust struct's #[serde(default)] reads Python's values fine).
    #
    # short_id/project_root are Rust `String` (NOT Option), so they default to
    # "" -- emitting "short_id": null would fail Rust's deserialize (null is not
    # a String). The Option fields below emit null, which Rust reads as None.
    #
    # short_id is the provider's transport key (v9, x-1b1e): claude rows carry
    # the 8-hex jobId (`claude attach/logs <jobId>`, by construction the first 8
    # hex of the session UUID); daemon PTY rows carry the name-derived worker
    # socket key. The legacy `claude_short_id` field was removed at v9 --
    # load_registry backfills it into short_id on read and never writes it back.
    short_id: str = ""
    project_root: str = ""
    messaging_socket_path: Optional[str] = None
    cc_session_id: Optional[str] = None
    pid: Optional[int] = None
    pid_start_time: Optional[int] = None
    last_reconciled_at: Optional[str] = None
    # Latest inside-leg report for this row's claude pane (inside-out E3.1,
    # "contract v2"; mirrors the Rust `RegistryEntry.inside_leg` /
    # `InsideLegReport`). A lossless PASSTHROUGH: the daemon (Rust) is the sole
    # writer and owns all inside-leg behaviour (seq-drop, TTL aging, authority);
    # Python only custodies the blob so a row round-trips across the mixed-language
    # registry (X3 / ab-b946b59c). Kept as an opaque dict (not a typed dataclass)
    # because no Python consumer reads its fields yet; type it when one does.
    # None for every non-inside-leg row; asdict re-emits it (None -> null, which
    # Rust reads back as None). Additive-optional, gated by the v5 schema bump.
    inside_leg: Optional[dict] = None
    # Dead-row GC exit stamp (x-b1aa). ISO 8601 UTC set by the Rust daemon's GC
    # sweep the first tick it observes this row's process gone; anchors the
    # config.agents.dead_row_grace window before the row is reaped. Rust is the
    # sole writer; Python only custodies it so a row round-trips losslessly.
    # Additive-optional: an absent key reads as None and the Rust RegistryEntry
    # mirrors it with #[serde(default, skip_serializing_if=...)], so no schema bump.
    exited_at: Optional[str] = None
    # Mux hosting ref (4a-G2): ``{"session": <mux session>, "pane_id": <u64>}``
    # for an agent whose PTY is a mux pane (``fno agents spawn --substrate
    # pane``); ``None`` for daemon-worker, bg-thread, and headless rows. The
    # Python spawn back half writes it; the mux server's sideline reader and
    # ``fno mail`` live-inject dispatch on it (a row carries exactly ONE live
    # ref - mux XOR worker XOR bg - enforced by ``write_registry``). Mirrors
    # Rust ``RegistryEntry.mux: Option<MuxRef>`` (X3); gated by the v6 schema
    # bump so a pre-mux reader rejects instead of silently dropping the ref.
    mux: Optional[dict] = None
    # Latest screen-manifest verdict for this row's mux pane (v7, the fallback
    # rung of the badge lattice under the inside-leg hook): ``{"state", "rule",
    # "seq", "at", "ttl_ms"?}``. The Rust daemon's scrape sweep is the sole
    # writer and owns all behaviour (eligibility, write-on-change, clears);
    # Python only custodies the blob so a row round-trips across the
    # mixed-language registry, same X3 passthrough treatment as ``inside_leg``.
    # Gated by the v7 schema bump so a pre-v7 reader rejects instead of
    # silently dropping a stored verdict.
    screen_state: Optional[dict] = None
    # Crown fields (US9, KFAD squad court): who holds an orchestrator crown and
    # at what altitude. STAMPED BY THE SPAWN PATH, never self-declared - a
    # session cannot write a crown onto its own row; the grantor
    # (`crown_grantor`, the spawning session, or "human" for a direct human
    # spawn) is captured ambiently, the same provenance discipline as
    # harness-stamped mail identity. Crown liveness == this row's liveness (no
    # separate lifecycle). Rust's RegistryEntry mirrors all three as
    # additive-optional passthrough, so the daemon preserves them on write-back
    # (a Python-only field is dropped when the daemon re-serializes the row);
    # gated by the v11 schema bump so a pre-v11 reader rejects instead of
    # silently dropping a stored crown.
    crown_level: Optional[int] = None
    crown_scope: Optional[str] = None
    crown_grantor: Optional[str] = None
    # The PATH of the route-settings/<sha16>.json this CLAUDE worker was launched
    # with (x-ae2d, v12), or None for a worker that was never routed. Written by
    # the spawn seams only; read only by the relaunch paths, which re-apply it or
    # refuse. A path, never the contents: that file is 0600 and carries a live
    # ANTHROPIC_AUTH_TOKEN, while the registry has no such guarantee.
    #
    # Claude-only by construction: the file IS a claude `--settings` JSON, and a
    # codex route lives in `-c` config args rather than the env, so recording a
    # codex row's env here would promise a restore that lands on codex's default
    # provider holding the route's key. A non-claude row is left unrecorded.
    #
    # NOT an answer to "what is this worker running now" - a recorded value
    # reports the INTENDED route in exactly the case where a fallback happened,
    # so that question is read from the transcript (x-cf40) and never from here.
    # Rust's RegistryEntry mirrors it as additive-optional passthrough, or the
    # daemon would drop a Python-stamped path on its next read-modify-write.
    route_settings_path: Optional[str] = None
    # v13 (x-0358): the fno_id of the /target session an adopted orphan was
    # working, set by the Rust adopt verb from the matched target manifest. None
    # for every row not adopted from a target manifest. Identity-adjacent
    # linkage only; never read for liveness or ownership. Rust mirrors it as
    # additive-optional passthrough so the daemon's read-modify-write keeps it.
    fno_id: Optional[str] = None
    # v14 (x-e21e): this recipient's MAIL DELIVERY POLICY. ``"bus-only"`` means
    # mail to this session never prompt-line injects and always takes the
    # durable bus (the recipient surfaces it at its turn boundary via
    # ``fno mail notify-self``); ``None`` is the default injectable policy every
    # worker keeps. A DELIVERY-POLICY fact, never a liveness verdict - the same
    # distinction that renamed NOT_INJECTABLE off "not-live"
    # (crates/fno-agents/src/mail_inject.rs): a bus-only session may be alive
    # and mid-turn, it just belongs on the bus. Stamped by the session itself
    # (``fno agents register --delivery-policy bus-only``); Rust's RegistryEntry
    # mirrors it as additive-optional passthrough so the daemon's
    # read-modify-write keeps it.
    delivery_policy: Optional[str] = None

    @property
    def session_id(self) -> Optional[str]:
        """The harness-specific resume-target id.

        Resolves to whichever stored field the resume path consumes:
        ``short_id`` (``claude attach``) for a claude row that carries one,
        otherwise the canonical ``harness_session_id`` (``claude --resume``,
        ``codex resume <uuid>``). ``None`` only when the row carries neither a
        transport key nor a ``harness_session_id``.

        claude is the one harness whose transport key (``short_id``, the 8-hex
        jobId ``claude attach`` takes) is distinct from its canonical id: a
        pane/mux row carries a ``harness_session_id`` but no ``short_id`` by
        design (``_validate_single_live_ref`` enforces mux XOR worker XOR bg).
        Falling back to ``harness_session_id`` keeps such a row resumable
        instead of reporting "no session id" for a row that has one (x-b84f).

        The harness -> field mapping comes from the module-level
        :data:`HARNESS_SESSION_ID_FIELDS`, which ``resume_cli._session_id_for``
        also reads, so the two cannot drift. Keyed on ``harness`` (x-880e, the
        sole identity axis). As a ``@property`` this is excluded from ``asdict``
        serialization and never becomes an on-disk storage field.
        """
        field_name = HARNESS_SESSION_ID_FIELDS.get(self.harness)
        # `short_id` is a str defaulting to "" (never None); normalize the
        # empty transport key to None so callers keep their `is None` checks.
        transport = (getattr(self, field_name) or None) if field_name else None
        if transport:
            return transport
        # No transport key (a claude pane row): the canonical id is the
        # resume-target id.
        return self.harness_session_id or None

    @property
    def crown_label(self) -> Optional[str]:
        """Compact crown descriptor for display (``"L1 epic-x"``), or ``None``
        when this row holds no crown. The single formatter both ``fno whoami``
        and ``fno agents list``/``top`` render from, so the two cannot drift.
        Excluded from ``asdict`` (a ``@property``), so it never persists."""
        if self.crown_level is None:
            return None
        return f"L{self.crown_level} {self.crown_scope or '?'}"


# ---------------------------------------------------------------------------
# Shared identifier resolver (x-1b1e): every session-connecting `fno agents`
# verb accepts the registry name, full harness session id, explicit transport
# short id, canonical handle, or legacy prefix. This function is the single
# lookup choke point so no verb re-implements a name-only `.find`.
# ---------------------------------------------------------------------------

# Exactly eight lowercase hex characters, used only when deciding whether a
# Claude restamp may safely refresh a derived transport short id.
_DERIVED_SHORT_RE = re.compile(r"^[0-9a-f]{8}$")

_ACCEPTED_FORMS = (
    "accepted forms: name, canonical handle, transport short id, or full session id"
)


class AgentResolutionError(RuntimeError):
    """No entry, an ambiguous token, or an unreadable registry.

    ``exit_code`` defaults to 2 (the lifecycle name-not-found convention) for a
    caller that maps the error straight through (``raise typer.Exit(exc.exit_code)``,
    e.g. ``watch``). Verbs with their own convention still override it — resume
    reports 13, trace/stop/rm map through their existing not-found path — so this
    default is the fallback, not a universal choke point.

    ``ambiguous`` distinguishes "this token names several agents" from "no agent
    matches". ``unavailable`` means the evidence needed to decide could not be
    read. Only a genuine MISS may fall through to the harness-store fallback: a
    token the registry already refuses to disambiguate must keep refusing, or a
    store hit on one of the candidates would silently pick the winner the
    registry deliberately would not.
    """

    def __init__(
        self,
        message: str,
        *,
        exit_code: int = 2,
        ambiguous: bool = False,
        unavailable: bool = False,
    ) -> None:
        super().__init__(message)
        self.exit_code = exit_code
        self.ambiguous = ambiguous
        self.unavailable = unavailable


@dataclass
class ResolvedAgent:
    """The entry a token resolved to, plus which rule matched.

    ``worker_short_id`` is the transport handle a session-connecting verb
    shells out with (``claude attach/logs <short>`` etc.); ``None`` when the
    row recorded no short (a pre-heal claude row) so the verb can raise its own
    explicit "no short id on file" error instead of shelling an empty arg.
    """

    entry: AgentEntry
    matched_by: str  # "name" | "full_session_id" | "short_id" | handle compatibility

    @property
    def worker_short_id(self) -> Optional[str]:
        return self.entry.short_id or None


def _session_tier(entry: object, token: str) -> Optional[int]:
    """Delegate generated and legacy address comparison to the identity owner."""
    hsid = getattr(entry, "harness_session_id", None)
    return session_handle_tier(token, hsid) if hsid else None


def _one_or_ambiguous(hits: list, matched_by: str, token: str) -> ResolvedAgent:
    """Return the single matched entry, or raise on a real ambiguity.

    Dedup only repeated references to the same loaded row. A corrupt or legacy
    registry can contain two rows with one name but different sessions; treating
    the intended primary key as proof of identity would silently pick one.
    """
    distinct: list[Any] = []
    for entry in hits:
        if not any(entry is existing for existing in distinct):
            distinct.append(entry)
    if len(distinct) > 1:
        cands = ", ".join(
            f"{getattr(e, 'name', '?')} (short={getattr(e, 'short_id', '') or '-'}, "
            f"{getattr(e, 'harness', '?')})"
            for e in distinct
        )
        raise AgentResolutionError(
            f"token {token!r} is ambiguous across {len(distinct)} agents: "
            f"{cands}. Disambiguate with the name or full session id.",
            ambiguous=True,
        )
    return ResolvedAgent(entry=distinct[0], matched_by=matched_by)


def resolve_agent_in(entries: list, token: str) -> ResolvedAgent:
    """The matching core over an already-loaded entry list (the Rust mirror).

    A full session id is explicit and resolves first. Every shorter address form
    shares one namespace: exact name, stored transport short id, canonical
    handle, and legacy prefix matches are unioned before uniqueness is decided.
    UUID-family identity matching is case-insensitive; OpenCode identity matching
    preserves case.

    ``getattr``-based, so both real ``AgentEntry`` rows and duck-typed rows (a
    verb that injects its own registry loader) resolve identically. Raises
    :class:`AgentResolutionError` (exit 2) on empty/unknown/ambiguous."""
    token = (token or "").strip()
    if not token:
        raise AgentResolutionError(f"empty agent token; {_ACCEPTED_FORMS}")
    by_full = [e for e in entries if _session_tier(e, token) == 0]
    if by_full:
        return _one_or_ambiguous(by_full, "full_session_id", token)

    categories = (
        ("name", [e for e in entries if getattr(e, "name", None) == token]),
        ("short_id", [e for e in entries if getattr(e, "short_id", None) == token]),
        ("canonical_handle", [e for e in entries if _session_tier(e, token) == 1]),
        ("legacy_suffix", [e for e in entries if _session_tier(e, token) == 2]),
    )
    hits = [entry for _matched_by, category in categories for entry in category]
    if hits:
        matched_by = next(matched_by for matched_by, category in categories if category)
        return _one_or_ambiguous(hits, matched_by, token)

    raise AgentResolutionError(f"no agent matching {token!r}; {_ACCEPTED_FORMS}")


def resolve_agent(token: str, *, path: Optional[Path] = None) -> ResolvedAgent:
    """Resolve ``token`` to one registry entry, loading the registry first.

    Wraps :func:`resolve_agent_in`; a malformed/unreadable registry becomes a
    typed unavailable :class:`AgentResolutionError`, never a traceback leaking
    to the verb. See ``resolve_agent_in`` for the matching rules.

    A full session id and an ordinary name resolve from the registry directly.
    Every session-shaped short token is checked against the harness stores too:
    the registry is a cache of reality, so a store-only session must participate
    in the same ambiguity decision. A registry miss may then adopt one unique
    store hit (x-9cc5).
    """
    try:
        entries = load_registry(path=path)
    except RegistryVersionError as exc:
        raise AgentResolutionError(
            f"registry unreadable ({exc}); cannot resolve {token!r}",
            unavailable=True,
        ) from exc
    return resolve_agent_across_sources(entries, token, path=path)


def resolve_agent_across_sources(
    entries: list, token: str, *, path: Optional[Path] = None
) -> ResolvedAgent:
    """Resolve one token against a registry snapshot and every harness store."""
    try:
        return resolve_registered_agent_across_sources(entries, token)
    except AgentResolutionError as exc:
        # A MISS may fall through; a registry the caller must disambiguate must
        # not. Otherwise a store hit on one of several matching rows would pick
        # the winner the registry deliberately refused to pick.
        if exc.ambiguous or exc.unavailable:
            raise
        entry = resolve_from_harness_store(token, registry_path=path)
        if entry is None:
            raise
        return ResolvedAgent(entry=entry, matched_by="harness_store")


def resolve_registered_agent_across_sources(entries: list, token: str) -> ResolvedAgent:
    """Resolve a registry row while checking store-only collision candidates.

    A registry miss stays a miss and never adopts a store-only session. Registry-
    gated read and delivery verbs use this to share the all-source ambiguity rule
    without changing their established miss contract.
    """
    resolved = resolve_agent_in(entries, token)
    resolved = _ensure_unique_across_aliases(resolved, token)
    return _ensure_unique_across_stores(resolved, token)


def _ensure_unique_across_aliases(
    resolved: ResolvedAgent, token: str
) -> ResolvedAgent:
    """Union a registry hit with persisted friendly aliases before selecting."""
    if resolved.matched_by == "full_session_id":
        return resolved

    from fno.agents.discover import _alias_to_session_ids

    alias_ids, read_ok = _alias_to_session_ids(token, None)
    if not read_ok:
        raise AgentResolutionError(
            f"persisted alias map unreadable; cannot resolve {token!r} uniquely",
            unavailable=True,
        )

    entry = resolved.entry
    registry_id = getattr(entry, "harness_session_id", None)
    registry_identity = session_identity_key(registry_id) if registry_id else None
    foreign = sorted(
        sid
        for sid in set(alias_ids)
        if registry_identity is None or session_identity_key(sid) != registry_identity
    )
    if not foreign:
        return resolved

    candidates = [
        f"{registry_id or entry.name} ({entry.harness}, registry name={entry.name})",
        *(f"{sid} (persisted alias)" for sid in foreign),
    ]
    raise AgentResolutionError(
        f"token {token!r} is ambiguous across {len(candidates)} sessions: "
        f"{', '.join(candidates)}. Disambiguate with the full session id.",
        ambiguous=True,
    )


def _ensure_unique_across_stores(
    resolved: ResolvedAgent, token: str
) -> ResolvedAgent:
    """Union one registry hit with store-only candidates before selecting it."""
    from fno.agents.store_fallback import complete_store_hits, is_session_shaped

    if resolved.matched_by == "full_session_id" or not is_session_shaped(token):
        return resolved

    entry = resolved.entry
    registry_id = getattr(entry, "harness_session_id", None)
    registry_key = (
        entry.harness,
        session_identity_key(registry_id) if registry_id else None,
    )
    foreign = {
        (hit.harness, session_identity_key(hit.session_id)): hit
        for hit in complete_store_hits(token)
        if (hit.harness, session_identity_key(hit.session_id)) != registry_key
    }
    if not foreign:
        return resolved

    registry_id = registry_id or entry.name
    candidates = [
        f"{registry_id} ({entry.harness}, registry name={entry.name})",
        *(
            f"{hit.session_id} ({hit.harness}, harness store)"
            for hit in sorted(foreign.values(), key=lambda h: (h.harness, h.session_id))
        ),
    ]
    raise AgentResolutionError(
        f"token {token!r} is ambiguous across {len(candidates)} sessions: "
        f"{', '.join(candidates)}. Disambiguate with the full session id.",
        ambiguous=True,
    )


def resolve_from_harness_store(
    token: str, *, registry_path: Optional[Path] = None,
    scope_cwd: Optional[str] = None, cross_project: bool = False,
) -> Optional[AgentEntry]:
    """The registry-miss healer (x-9cc5), isolated so every resolution surface
    reaches it identically -- including ``resume``, which loads its own entries
    and so calls :func:`resolve_agent_in` rather than :func:`resolve_agent`.

    Returns ``None`` when no store knows the token, so the caller raises its own
    unchanged error. Propagates :class:`AgentResolutionError` on an ambiguous
    token: refusing to guess is the designed outcome, not a miss.

    ``scope_cwd``/``cross_project`` carry the project-confinement contract
    through to :func:`heal_from_harness_store`; the default (process cwd, no
    override) confines adoption to the caller's project. ``resume`` bypasses this
    healer entirely, so it is uncovered by design."""
    from fno.agents.store_fallback import heal_from_harness_store

    return heal_from_harness_store(
        token, registry_path=registry_path,
        scope_cwd=scope_cwd, cross_project=cross_project,
    )


def _registry_path(path: Optional[Path]) -> Path:
    if path is not None:
        return path
    return paths.agents_registry_path()


def _agent_lock_path(name: str, registry_path: Path) -> Path:
    """Return the flock file for a given agent name under registry's directory.

    Lock files live under ``<registry-dir>/locks/<name>.lock``. Caller is
    responsible for opening the file and calling ``fcntl.flock``; this
    function only computes the path. Name is rejected if it contains
    path separators or ``..``.
    """
    if "/" in name or "\\" in name or ".." in name:
        raise ValueError(
            f"agent name must not contain path separators or '..': {name!r}"
        )
    return registry_path.parent / "locks" / f"{name}.lock"


def _registry_lock_path(registry_path: Path) -> Path:
    """Return the registry-wide flock file alongside the registry."""
    return registry_path.parent / "locks" / "_registry.lock"


class RegistryLockTimeout(TimeoutError):
    """The registry-wide lock stayed contended past a caller's budget."""


@contextlib.contextmanager
def _hold_registry_lock(
    registry_path: Path,
    *,
    timeout: Optional[float] = None,
    poll_seconds: float = 0.02,
) -> Iterator[None]:
    """Acquire the registry-wide flock, optionally within a caller budget."""
    lock_file = _registry_lock_path(registry_path)
    lock_file.parent.mkdir(parents=True, exist_ok=True)
    with open(lock_file, "w") as fh:
        if timeout is None:
            fcntl.flock(fh, fcntl.LOCK_EX)
        else:
            validate_timeout_budget(
                timeout,
                label="registry lock",
                poll=poll_seconds,
            )
            deadline = time.monotonic() + timeout
            while True:
                try:
                    fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise RegistryLockTimeout(
                            f"registry lock timeout after {timeout:g}s at {lock_file}"
                        )
                    time.sleep(min(poll_seconds, remaining))
        try:
            yield
        finally:
            fcntl.flock(fh, fcntl.LOCK_UN)


def _validate_single_live_ref(entry: AgentEntry) -> None:
    """One-live-ref invariant (4a-G2, mirrors Rust ``validate_single_live_ref``).

    A row carrying the ``mux`` ref must not ALSO carry a transport identity
    (non-empty ``short_id``: a worker-socket key or, since v9, a ``claude
    --bg`` jobId) - a double-ref row would make consumers dispatch one
    agent down two substrates. Scoped to mux rows only; pre-existing
    worker/bg field combinations are untouched.
    """
    if entry.mux is None:
        return
    if entry.short_id:
        raise ValueError(
            f"registry row {entry.name!r} carries a mux ref alongside a "
            f"worker/bg ref; a row holds exactly one live ref (mux XOR worker XOR bg)"
        )


def _read_raw_registry(target: Path) -> Optional[dict]:
    """Best-effort parse of the on-disk registry, or ``None`` when it is
    missing, unreadable, or not a JSON object.

    Shared by ``_refuse_write_over_newer_schema`` and ``_existing_row_names``
    so a write pays for this read once, not twice.
    """
    try:
        raw = json.loads(target.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return None
    if not isinstance(raw, dict):
        return None
    return raw


def _refuse_write_over_newer_schema(raw: Optional[dict], target: Path) -> None:
    """Refuse to overwrite a registry written by a newer fno.

    This is the half of read-forward that protects the file. ``load_registry``
    drops fields above its own schema, so entries read from a newer store are
    incomplete by construction, and writing them back would erase every field
    this fno cannot see -- for every agent on the machine, not just this one.

    Only a readable, higher integer version blocks. A missing or unparseable
    file is not a newer writer, and refusing there would leave a torn registry
    unrepairable by the very command meant to rewrite it.
    """
    if raw is None:
        return
    on_disk = raw.get("schema_version")
    if isinstance(on_disk, int) and on_disk > SCHEMA_VERSION:
        raise RegistryVersionError(
            f"refusing to write registry at {target}: on-disk schema_version="
            f"{on_disk} is newer than the schema_version={SCHEMA_VERSION} this "
            "fno understands, and writing would drop the fields it cannot see. "
            "Upgrade fno to match."
        )


def _existing_row_names(raw: Optional[dict]) -> set[str]:
    """Names already on disk, from the same read ``_refuse_write_over_newer_schema``
    uses -- so the new-vs-existing split for the resolvable-handle invariant
    (x-7bcd) costs no extra I/O."""
    if raw is None:
        return set()
    agents = raw.get("agents")
    if not isinstance(agents, list):
        return set()
    return {a["name"] for a in agents if isinstance(a, dict) and isinstance(a.get("name"), str)}


def _has_resolvable_handle(
    *,
    pid: Optional[int] = None,
    pid_start_time: Optional[float] = None,
    log_path: Optional[str] = None,
    harness: Optional[str] = None,
    harness_session_id: Optional[str] = None,
) -> bool:
    """The three-leg resolvable-handle predicate (x-7bcd), factored out of
    ``_validate_resolvable_handle`` so a mint site that must decide WHETHER a
    fallback handle is needed (``mux_spawn.py``) can call the same source of
    truth instead of re-deriving the leg logic inline, where a future change
    to a leg's definition could otherwise drift between the two copies.
    """
    leg1 = pid is not None and pid_start_time is not None
    leg2 = bool(log_path)
    leg3 = bool(harness) and bool(harness_session_id)
    return leg1 or leg2 or leg3


def _validate_resolvable_handle(entry: AgentEntry) -> None:
    """The resolvable-handle invariant (x-7bcd, mirrors Rust
    ``validate_resolvable_handle``): at creation, every registry row carries
    at least one handle an outside observer can resolve without asking the
    worker anything. Any one of three legs satisfies it: (1) ``pid`` +
    ``pid_start_time``, when the writer owns the process; (2) a non-empty
    ``log_path``, when the writer has created the file it records (file
    existence is enforced at the mint site, not here); (3) ``harness`` +
    ``harness_session_id``, when the writer owns neither a pid nor a log
    file. Scoped to NEW rows only by the caller (AC3-FR); this function does
    no I/O and makes no new-vs-existing distinction itself.
    """
    if _has_resolvable_handle(
        pid=entry.pid,
        pid_start_time=entry.pid_start_time,
        log_path=entry.log_path,
        harness=entry.harness,
        harness_session_id=entry.harness_session_id,
    ):
        return
    raise ValueError(
        f"registry row {entry.name!r} carries no resolvable handle: needs one of "
        "(pid + pid_start_time), log_path, or (harness + harness_session_id)"
    )


def write_registry(entries: list[AgentEntry], path: Optional[Path] = None) -> None:
    """Atomically write the registry to disk.

    Serialization happens before the temp file is opened so an exception in
    ``_json_dumps`` cannot corrupt the existing file. The encoded text is
    written to ``<path>.tmp`` and renamed into place via ``os.replace``.
    On a post-serialization failure (e.g. ENOSPC during ``write_text``),
    the orphan ``.tmp`` is unlinked so it doesn't accumulate on retry.
    """
    target = _registry_path(path)
    raw = _read_raw_registry(target)
    _refuse_write_over_newer_schema(raw, target)
    existing = _existing_row_names(raw)
    for e in entries:
        _validate_single_live_ref(e)
        if e.name not in existing:
            _validate_resolvable_handle(e)
    payload = {
        "schema_version": SCHEMA_VERSION,
        "agents": [asdict(e) for e in entries],
    }
    # Bare-name call resolves via module globals at call time, so
    # ``monkeypatch.setattr(reg_module, "_json_dumps", ...)`` works.
    text = _json_dumps(payload, indent=2, sort_keys=False)
    target.parent.mkdir(parents=True, exist_ok=True)
    tmp = target.with_suffix(target.suffix + ".tmp")
    try:
        tmp.write_text(text, encoding="utf-8")
        os.replace(tmp, target)
    except OSError:
        tmp.unlink(missing_ok=True)
        raise


#: Constructor keys of ``AgentEntry``, derived rather than listed so a new
#: field never has to be remembered here. Used only on the read-forward path,
#: to drop keys a newer writer added after this fno was built.
_INIT_FIELD_NAMES = frozenset(f.name for f in fields(AgentEntry) if f.init)


def _is_identity_token(value: object) -> bool:
    """A well-shaped registry identity token (provider or harness): a
    non-empty, all-lowercase, whitespace-free string.

    The relaxed load-gate corruption guard (x-8dfc) that replaced the
    KNOWN/READABLE_PROVIDERS enumeration: the read no longer bricks on an
    alien harness (it degrades to durable routing, x-ec59 posture), and
    dispatch capability is gated separately at the spawn/ask seam. This still
    rejects genuine corruption -- empty, non-string, or whitespace-bearing
    identity. Mirrors Rust ``client_verbs::is_identity_token``.
    """
    return (
        isinstance(value, str)
        and value != ""
        and value == value.lower()
        and not any(c.isspace() for c in value)
    )


# A row in one of these statuses represents a session that still owns its
# identity: a harness_session_id held here is provably NOT another acquiring
# session's. Terminal/exit statuses (orphaned, failed, exited, permanent_dead)
# release ownership, so an id last held by an exited row is free to claim.
_OWNERSHIP_LIVE_STATUSES = frozenset(
    {"spawning", "ready", "idle", "busy", "live", "restarting"}
)


def row_owning_session_id(
    session_id: str, registry_path: Optional[Path] = None
) -> Optional[str]:
    """Name of an active registry row whose ``harness_session_id`` is
    ``session_id``, or None.

    Two live sessions cannot share a harness session id, so a hit proves a
    candidate id is NOT this session's - the cause-agnostic detector that would
    have caught the ambient-leak incident regardless of how the marker entered
    the environment. Only rows in an ownership-live status count; an exited
    row's id is free.

    Degrade-safe by contract (AC4-ERR): an absent, unreadable, or alien-shape
    registry returns None (cannot prove a collision) rather than raising, so an
    unreadable registry never blocks init. Callers that must know whether the
    check ran inspect the returned owner against None after a successful read.
    """
    if not session_id:
        return None
    from fno.harness_identity import session_identity_key

    needle = session_identity_key(session_id)
    try:
        entries = load_registry(registry_path)
    except Exception:
        # Unreadable / wrong-schema / absent: cannot prove a collision.
        return None
    for entry in entries:
        if entry.status not in _OWNERSHIP_LIVE_STATUSES:
            continue
        candidate = getattr(entry, "harness_session_id", None)
        if candidate and session_identity_key(candidate) == needle:
            return entry.name
    return None


def load_registry(path: Optional[Path] = None) -> list[AgentEntry]:
    """Load the registry. Returns ``[]`` if the file does not exist.

    Damage raises ``RegistryVersionError``, so callers handle "this file
    looks wrong" through one exception type: invalid JSON, top-level
    not-a-dict, ``agents`` not-a-list, a missing or non-integer
    ``schema_version``, row not-a-dict, and an identity-less row.

    A schema NEWER than ours is not damage and is read forward. Rows this
    fno cannot represent are skipped and announced; the rest still resolve.
    ``write_registry`` refuses while the on-disk schema is higher, which is
    what stops a partial read from being written back over everyone else's
    fields. AT or BELOW our own schema, an unknown field or an unknown
    ``status`` / ``host_mode`` stays fatal, because there it means a writer
    bug rather than a version gap.

    Harness identity is a shape check, not an enumeration: one
    alien harness never bricks the shared read, and dispatch capability is
    gated at the spawn/ask seam.
    """
    target = _registry_path(path)
    if not target.exists():
        return []

    try:
        raw = json.loads(target.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, UnicodeDecodeError) as exc:
        raise RegistryVersionError(
            f"registry at {target} is malformed JSON: {exc}"
        ) from exc

    if not isinstance(raw, dict):
        raise RegistryVersionError(
            f"registry at {target} top-level is not a JSON object "
            f"(got {type(raw).__name__})"
        )

    on_disk_version = raw.get("schema_version")
    # Older schemas are read transparently: missing fields are
    # synthesized in memory with default values, and the next
    # write_registry persists the current shape. The on-disk file
    # is NOT mutated by load. Accepted: v1 (lacks status +
    # last_message_at + mcp_channel_id), v2 (lacks mcp_channel_id),
    # v3 (adds mcp_channel_id), and v4 (host_mode forward-compat bump;
    # structurally identical to v3). The accepted set spans 1..=SCHEMA_VERSION
    # so a bump never drops back-compat reads (ab-a171ceb2); the synthesis
    # flags below key off ABSOLUTE version numbers, not SCHEMA_VERSION-relative
    # offsets, so future bumps don't silently mis-trigger v1/v2 synthesis.
    # Anything outside the range raises RegistryVersionError.
    if not (isinstance(on_disk_version, int) and on_disk_version >= 1):
        raise RegistryVersionError(
            f"registry at {target} has schema_version={on_disk_version!r}, "
            f"this fno understands schema_version={SCHEMA_VERSION}. "
            "Upgrade or downgrade fno to match."
        )
    # READ FORWARD. This store is global to every agent on the machine, so a
    # process running ahead of the deployment used to brick every deployed
    # reader at once: mail died fleet-wide, with no announcement and a symptom
    # that surfaced far from the cause. A newer writer is now read, not refused.
    #
    # Two things make that safe, and neither is optional.
    #   - write_registry REFUSES while the on-disk schema is higher, because
    #     reading forward drops fields this reader cannot see and a write from
    #     that state would erase rows it never knew about.
    #   - every degraded read announces itself below. Silence is the real trap:
    #     it makes a partial row indistinguishable from a complete one, so a
    #     routing or liveness decision taken on a truncated row leaves no trace.
    read_forward = on_disk_version > SCHEMA_VERSION
    if read_forward:
        print(
            f"fno agents: registry at {target} is schema_version="
            f"{on_disk_version}, ahead of the schema_version={SCHEMA_VERSION} "
            "this fno understands. Reading the fields it knows and ignoring the "
            "rest; writes are refused until this fno is upgraded. Rows may be "
            "incomplete.",
            file=sys.stderr,
        )
    needs_v1_synthesis = on_disk_version == 1
    needs_v2_synthesis = on_disk_version <= 2
    legacy_provider_semantics = on_disk_version < 15

    agents_field = raw.get("agents", [])
    if not isinstance(agents_field, list):
        raise RegistryVersionError(
            f"registry at {target} 'agents' field is not a list "
            f"(got {type(agents_field).__name__})"
        )

    entries: list[AgentEntry] = []
    skipped_rows: list[tuple[int, str]] = []
    for index, row in enumerate(agents_field):
        # Under read-forward ANY row-level refusal degrades to a skipped row
        # rather than taking the whole shared read down. Wrapping the body,
        # rather than guarding each check, is deliberate: a refusal added here
        # later is covered without anyone remembering to guard it. Added KEYS
        # were only half the problem -- a newer writer that widens the status or
        # host_mode enum, or drops a field that is required today, still bricked
        # every reader on the machine, which is the failure this file exists to
        # stop. At or below our own schema every one of these stays fatal, where
        # it means a writer bug rather than a version gap.
        try:
            if not isinstance(row, dict):
                raise RegistryVersionError(
                    f"registry at {target} row {index} is not a JSON object "
                    f"(got {type(row).__name__})"
                )
            provider = row.get("provider")
            harness = row.get("harness")
            # Before v15 `provider` was a harness alias. At v15 and later it is
            # a separate model-provider axis, so only `harness` can satisfy the
            # row identity requirement. Both axes accept any well-shaped token;
            # dispatch capability is enforced later at the spawn/ask seam.
            valid_legacy_alias = legacy_provider_semantics and _is_identity_token(provider)
            if not (_is_identity_token(harness) or valid_legacy_alias):
                raise RegistryVersionError(
                    f"registry at {target} row {index} has no valid identity token "
                    f"(provider={provider!r}, harness={harness!r}); a row needs a "
                    "non-empty lowercase harness. "
                    "Upgrade or downgrade fno to match."
                )
            if provider is not None and not _is_identity_token(provider):
                raise RegistryVersionError(
                    f"registry at {target} row {index} has invalid provider={provider!r}; "
                    "provider must be a non-empty lowercase token or null."
                )
            # Divergence was a writer warning only while both keys represented
            # the harness axis. In v15, differing values are the normal routed
            # shape (for example harness=claude, provider=zai).
            if (
                legacy_provider_semantics
                and
                _is_identity_token(provider)
                and _is_identity_token(harness)
                and provider != harness
            ):
                print(
                    f"fno agents: warning: registry row {row.get('name')!r} has "
                    f"provider={provider!r} and harness={harness!r} (diverged); "
                    "harness wins for identity",
                    file=sys.stderr,
                )
            if needs_v1_synthesis:
                row = {**row, "status": "live", "last_message_at": None}
            if needs_v2_synthesis and "mcp_channel_id" not in row:
                # v2 → v3 synthesis: socket-only agents have no MCP channel.
                row = {**row, "mcp_channel_id": None}
            # host_mode: absent key OR explicit null reads as "exec". Version-
            # independent (the additive field is handled by absence, not a schema
            # bump) so a Rust-written exec row (which omits the key) and any
            # pre-host_mode row both materialize a concrete "exec" mode. An explicit
            # "interactive" passes through unchanged. [interactive-drive node]
            if row.get("host_mode") is None:
                row = {**row, "host_mode": "exec"}
            elif row["host_mode"] not in KNOWN_HOST_MODES:
                raise RegistryVersionError(
                    f"registry at {target} row {index} has host_mode="
                    f"{row['host_mode']!r}; known values: "
                    f"{sorted(KNOWN_HOST_MODES)}. "
                    "Upgrade or downgrade fno to match."
                )
            # v2 entries carry an explicit status — guard against alien
            # values landing in-memory via a tampered registry file. v1
            # synthesis above pins "live" so it always passes.
            if row.get("status", "live") not in KNOWN_STATUSES:
                raise RegistryVersionError(
                    f"registry at {target} row {index} has status="
                    f"{row.get('status')!r}; known values: "
                    f"{sorted(KNOWN_STATUSES)}. "
                    "Upgrade or downgrade fno to match."
                )
            # Accept-on-read backfill (x-880e, v10): the removed identity keys
            # (provider + the per-provider session-id trio) populate the canonical
            # harness / harness_session_id and then die, so a legacy row round-trips
            # losslessly and asdict never re-emits them. harness adopts provider when
            # absent OR truthy-but-corrupt (whitespace/uppercase); the gate above
            # guarantees at least one of provider/harness is a valid token, so the
            # healed harness is always valid.
            if (
                legacy_provider_semantics
                and not _is_identity_token(row.get("harness"))
                and _is_identity_token(row.get("provider"))
            ):
                row = {**row, "harness": row["provider"]}
            # sync_harness_aliases reads the per-provider session keys still present in
            # the raw row and back-fills harness_session_id from the harness-matching
            # one (canonical wins on divergence). Runs BEFORE the pop below.
            row = sync_harness_aliases(dict(row), REGISTRY_LEGACY_SESSION_KEYS)
            # Drop the removed identity keys now that their values have back-filled
            # harness / harness_session_id, so they never reach AgentEntry(**row)
            # (which no longer defines them) and never round-trip through asdict.
            dead_keys = ["codex_session_id", "gemini_session_id", "claude_session_uuid"]
            if legacy_provider_semantics:
                dead_keys.append("provider")
            for _dead in dead_keys:
                row.pop(_dead, None)
            # v9 backfill (x-1b1e): the removed `claude_short_id` is accepted on
            # READ only -- a legacy row's jobId moves into `short_id` (the unified
            # transport key) and the key dies here, so asdict never re-emits it.
            # A conflicting pair keeps `short_id` (the drift this removal kills)
            # and warns once, never silently prefers the legacy value.
            legacy_short = row.pop("claude_short_id", None)
            if legacy_short:
                existing_short = row.get("short_id")
                if not existing_short:
                    row["short_id"] = legacy_short
                elif existing_short != legacy_short:
                    print(
                        f"fno agents: warning: registry row {row.get('name')!r} "
                        f"carries short_id={existing_short!r} and legacy "
                        f"claude_short_id={legacy_short!r}; keeping short_id",
                        file=sys.stderr,
                    )
            # `session_id` is a computed @property on AgentEntry, not an init field.
            # A Rust PTY row may serialize it (Rust skips it when None, so this only
            # fires for a row that recorded one); passing it to AgentEntry(**row)
            # would TypeError. Drop it -- Python recomputes it from harness +
            # harness_session_id (the identical projection Rust uses), so nothing
            # recoverable is lost, and asdict re-omits it on write-back. (ab-b946b59c)
            if "session_id" in row:
                row = {k: v for k, v in row.items() if k != "session_id"}
            # An unknown key is a writer bug AT or BELOW our schema, and stays fatal
            # there. Above it, an unknown key is just a field added after this fno
            # was built, so drop it rather than refuse the whole shared read. The
            # write refusal below is what keeps the dropped fields from being lost.
            if read_forward:
                row = {k: v for k, v in row.items() if k in _INIT_FIELD_NAMES}
            try:
                entries.append(AgentEntry(**row))
            except TypeError as exc:
                raise RegistryVersionError(
                    f"registry at {target} row {index} has malformed shape "
                    f"(unknown or missing fields): {exc}. "
                    "Upgrade or downgrade fno to match."
                ) from exc
        except Exception as exc:  # noqa: BLE001 - see below
            # Catching Exception, not just RegistryVersionError, is the point.
            # "A row this fno cannot represent" is the contract, and a newer
            # writer that turns `status` into a structured value reaches
            # `value in KNOWN_STATUSES` with a dict and raises TypeError, which
            # is not a RegistryVersionError and used to escape this handler and
            # take the whole shared read down -- the same fleet-wide brick, by a
            # third door. Any failure to build a row from a newer store is that
            # row's problem, never every other agent's.
            #
            # The type is reported below so a genuine bug in this loop is still
            # diagnosable rather than silently absorbed, and at or below our own
            # schema nothing is caught at all.
            if not read_forward:
                raise
            skipped_rows.append((index, type(exc).__name__))
            continue
    if skipped_rows:
        detail = ", ".join(f"{i} ({why})" for i, why in skipped_rows)
        print(
            f"fno agents: registry at {target}: skipped row(s) {detail} this fno "
            f"cannot represent at schema_version={on_disk_version}. Those agents "
            "are invisible to this process until it is upgraded.",
            file=sys.stderr,
        )
    return entries


def register_existing_session(
    *,
    provider: str,
    session_id: str,
    cwd: str,
    name: Optional[str] = None,
    log_path: str = "",
    short_id: str = "",
    status: Optional[AgentStatus] = None,
    origin: Optional[str] = None,
    delivery_policy: Optional[str] = None,
    registry_path: Optional[Path] = None,
) -> AgentEntry:
    """Register an operator-started session so peers can address it by name.

    The bus epic's spawn/host paths create registry rows; this is the
    missing seam for a session a human started by hand (e.g. a ``claude``
    SessionStart hook). After registration a peer can ``fno mail send``
    to the row's name; with no live transport the send demotes to the
    durable queue, which the session's own inbox-wake hook surfaces (US7).

    Idempotent on ``(provider, session_id)``: re-registering the same
    session (the hook re-fires after a resume/compaction) refreshes the
    row in place rather than appending a duplicate. A genuinely new session
    whose generated canonical handle names the SAME session as another row is
    refused rather than assigned an order-dependent numeric address; a
    first-eight overlap with a different session is the time-prefixed codex
    same-window shape and registers (the generated name suffixes when the name
    itself is already taken). Explicitly supplied friendly names retain their
    existing suffix behavior.

    Raises on registry I/O failure or bad input; the SessionStart caller
    (``register_session.main``) fails open and emits a warning event
    (AC7-ERR), so a locked/unwritable registry never blocks session start.
    """
    # Re-keyed on the shared harness mapping (x-8dfc); the parameter is still
    # the caller's provider (== harness on every current row), so the message
    # names what was passed.
    if provider not in HARNESS_SESSION_ID_FIELDS:
        raise ValueError(
            f"unknown provider for registration: {provider!r}; "
            f"known: {sorted(HARNESS_SESSION_ID_FIELDS)}"
        )
    if not session_id:
        raise ValueError("session_id must be non-empty")
    session_field = HARNESS_SESSION_ID_FIELDS[provider]

    # A hand-started session has NO live messaging transport (no daemon PTY,
    # no bg jobId/socket): a peer cannot inject into it. Registering it "live"
    # would make `resolve_to_project` pick it as an anycast target, and the
    # default-send live path would then dead-letter the durable fallback under
    # inbox/<agent-name>/ - which the session's own inbox-wake hook never reads
    # (it scans inbox/<project>/). So register as "idle": discoverable in
    # `fno agents list`, excluded from live anycast, so `send --to-project`
    # queues durable to the PROJECT inbox the session actually drains. Reliable
    # by-name live delivery to operator sessions waits on the deferred transport
    # (cv-d54ddd45).
    #
    # ``status`` overrides that default for a caller with better information:
    # the harness-store fallback (x-9cc5) adopts a row it only knows EXISTS, so
    # it registers "orphaned". Neither value is live, so neither reaches live
    # anycast or a lane cap.
    _REGISTERED_STATUS: AgentStatus = status or "idle"

    def _updater(entries: list[AgentEntry]) -> list[AgentEntry]:
        def _address_is_taken(
            token: str,
            *,
            exclude: Optional[AgentEntry] = None,
            same_session_only: bool = False,
        ) -> bool:
            # same_session_only: the token is MINTED from the session id (the
            # generated canonical handle), so it is taken only by the SAME
            # session (tier 0). A first-eight overlap with a different session
            # is the time-prefixed codex same-window shape - read-side
            # ambiguity, not a registration wall.
            def _row_takes(entry: AgentEntry) -> bool:
                if entry is exclude:
                    return False
                if not same_session_only and (
                    getattr(entry, "name", None) == token
                    or getattr(entry, "short_id", None) == token
                ):
                    return True
                tier = _session_tier(entry, token)
                return tier is not None and (not same_session_only or tier == 0)

            return any(_row_takes(entry) for entry in entries)

        for entry in entries:
            # Keyed on harness_session_id, the canonical id every row carries --
            # `session_field` is `short_id` for claude, which a caller may set to
            # the 8-hex transport key rather than the session id we match on.
            if entry.harness == provider and entry.harness_session_id == session_id:
                # Same session re-registering: refresh, do not duplicate.
                #
                # An EXPLICIT status never demotes a live row. The harness-store
                # healer resolves against a miss, then upserts under the lock; a
                # registration landing in that window (a `/fno-me`, a spawn) would
                # otherwise be overwritten with the healer's weaker "orphaned" and
                # dropped from live routing. A caller passing no status keeps the
                # old unconditional refresh, so `/fno-me` behaves exactly as before.
                if status is None or entry.status != "live":
                    entry.status = _REGISTERED_STATUS
                entry.cwd = cwd
                if log_path:
                    entry.log_path = log_path
                if short_id:
                    if short_id != entry.short_id and _address_is_taken(
                        short_id, exclude=entry
                    ):
                        raise AgentResolutionError(
                            f"transport short id {short_id!r} collision while "
                            f"refreshing session {session_id!r}; use the full session id",
                            ambiguous=True,
                        )
                    entry.short_id = short_id
                # Only restamp origin when the caller passed one: the harness-store
                # healer refreshes rows without it, and a blind `entry.origin = None`
                # would clobber an operator stamp a re-firing hook must preserve.
                if origin is not None:
                    entry.origin = origin
                # Same preserve-when-silent discipline for the delivery policy:
                # the SessionStart hook re-fires register without this kwarg
                # after every resume/compaction, and a blind overwrite would
                # silently revert a bus-only recipient to injectable. "off" is
                # the explicit clear (None means the caller said nothing).
                if delivery_policy is not None:
                    entry.delivery_policy = (
                        None if delivery_policy == "off" else delivery_policy
                    )
                return entries
        generated = canonical_handle(session_id)
        if _address_is_taken(generated, same_session_only=True):
            raise AgentResolutionError(
                f"canonical handle {generated!r} collision while registering session "
                f"{session_id!r}; use the full session id directly",
                ambiguous=True,
            )
        if short_id and _address_is_taken(short_id):
            raise AgentResolutionError(
                f"transport short id {short_id!r} collision while registering session "
                f"{session_id!r}; use the full session id",
                ambiguous=True,
            )

        base = name or generated
        chosen, suffix = base, 2
        while _address_is_taken(chosen):
            chosen = f"{base}-{suffix}"
            suffix += 1
        fresh = AgentEntry(
            name=chosen,
            harness=provider,
            harness_session_id=session_id,
            cwd=cwd,
            log_path=log_path,
            status=_REGISTERED_STATUS,
            origin=origin,
            delivery_policy=(
                None if delivery_policy in (None, "off") else delivery_policy
            ),
        )
        setattr(fresh, session_field, session_id)
        if short_id:
            # After the setattr: for claude, session_field IS short_id, and the
            # caller's transport key (the 8-hex jobId `claude attach` wants) must
            # win over the full UUID that setattr just wrote there.
            fresh.short_id = short_id
        entries.append(fresh)
        return entries

    persisted = update_registry(_updater, path=registry_path)
    for entry in persisted:
        if entry.harness == provider and entry.harness_session_id == session_id:
            return entry
    # update_registry returns the persisted entries list (the updater's
    # output), so the row must be present; a miss means the upsert dropped it.
    raise RuntimeError(
        f"registration for {provider} session {session_id!r} did not persist"
    )


def restamp_harness_session_id(
    *,
    name: str,
    harness: str,
    session_id: str,
    registry_path: Optional[Path] = None,
) -> Optional[AgentEntry]:
    """Re-point a spawned worker's row at the session id its harness now uses.

    A harness may REPLACE the session id footnote passed at spawn. A claude
    worker launched as ``claude --session-id <uuid>`` has been observed
    continuing under a different uuid ~35s in, carrying its transcript across
    (identical message uuids on both sides, so a rename with carry-over, not a
    fork into two live sessions). The row then records an id that addresses
    nothing: peek/attach/resume and every mail send keyed on it miss a worker
    that is very much alive.

    Keyed on ``name`` -- the registry PK, minted by footnote at spawn and handed
    to the worker as ``FNO_AGENT_SELF``. It is the one identity on the row the
    harness cannot re-mint, so it is the only safe key here.
    ``register_existing_session`` keys its upsert on ``harness_session_id``
    instead and therefore MISSES a re-minted worker outright, appending a second
    row for one worker rather than correcting the first.

    Returns the updated entry, or ``None`` when there was nothing to do: no row
    under that name, a harness mismatch, or an id that already matches.

    KNOWN LIMIT, deliberately not solved here. If a spawn ever produces two
    descendants that are BOTH live, they inherit one ``FNO_AGENT_SELF`` and the
    row ends up naming whichever restamped last. That is still strictly better
    than the status quo it replaces -- a row pinned to the birth id addresses
    NONE of them, where this addresses one -- so the fix stands on its own, but
    it is not fork ownership. No such case has been observed: in the run that
    reported this, the birth id went silent 35 seconds in while its successor
    ran for three hours, with a 13/13 identical message prefix, so it was a
    rename with carry-over. A genuine flap would not be silent either: every
    correction emits ``session_id_restamped`` with the row name and the id it
    moved to, so two live descendants read off events.jsonl as one name
    alternating between two ids.
    """
    if not name or not session_id or not harness:
        return None

    restamped: list[AgentEntry] = []

    def _updater(entries: list[AgentEntry]) -> list[AgentEntry]:
        for entry in entries:
            if entry.name != name or entry.harness != harness:
                continue
            if entry.harness_session_id == session_id:
                return entries  # already current: no write, no event
            stale = entry.harness_session_id or ""
            entry.harness_session_id = session_id
            # A row parked at `spawning` was waiting for exactly this: an id it
            # could not learn at spawn time. The worker has now named itself, so
            # it is addressable and the transition is complete. Without this the
            # row is stuck at `spawning` for its whole life on any route where
            # the restamp is the ONLY path to an id (a happy-hosted claude pane
            # cannot pin one, since happy discards it), and a permanently
            # `spawning` row is the same lie as a permanently `live` one, just
            # in the other direction. Only `spawning` is promoted: every other
            # status is owned by something that knows more than this hook does.
            if entry.status == "spawning":
                entry.status = "live"
            # claude addresses by the 8-hex jobId in short_id, which is the
            # session uuid's leading segment (HARNESS_SESSION_ID_FIELDS maps
            # claude -> short_id). Re-derive it only when the stored short was
            # itself derived that way, or absent: a short that does NOT match
            # the stale uuid's prefix is an independent transport key we have
            # no basis to rewrite.
            #
            # NEVER on a mux row. `_validate_single_live_ref` enforces mux XOR
            # worker XOR bg, so filling the deliberately-empty short_id of a
            # pane-hosted row makes write_registry raise, the caller's fail-open
            # except swallow it, and the id change never persist -- a restamp
            # that no-ops on exactly the pane-spawned shape that reported this.
            # Correcting harness_session_id is enough for a mux row anyway:
            # resolve_agent matches the full id and the short DERIVED from it,
            # neither of which reads the stored short_id.
            if harness == "claude" and entry.mux is None:
                lead = session_id.split("-", 1)[0].lower()
                stale_lead = stale.split("-", 1)[0].lower()
                if _DERIVED_SHORT_RE.match(lead) and entry.short_id in ("", stale_lead):
                    entry.short_id = lead
            restamped.append(entry)
            return entries
        return entries

    update_registry(_updater, path=registry_path)
    return restamped[0] if restamped else None


def update_registry(
    updater: Callable[[list[AgentEntry]], list[AgentEntry]],
    path: Optional[Path] = None,
    *,
    lock_timeout: Optional[float] = None,
) -> list[AgentEntry]:
    """Atomically load -> apply ``updater`` -> write the registry.

    Holds the registry-wide flock for the full cycle so concurrent
    invocations for DIFFERENT agent names cannot stomp each other's
    updates. ``updater`` receives the current entries list and must
    return the new list to persist (typically by appending, replacing,
    or filtering the existing entries).

    Returns the freshly-persisted entries.

    Phase 1 callers are tests + Phase 2 dispatch. ``write_registry``
    remains the low-level primitive for cases that already hold the
    lock (test fixtures, repair tooling).
    """
    target = _registry_path(path)
    with _hold_registry_lock(target, timeout=lock_timeout):
        current = load_registry(path=target)
        before = {entry.name: _identity_signature(entry) for entry in current}
        new_entries = updater(list(current))
        _validate_changed_identities(before, new_entries)
        write_registry(new_entries, path=target)
        return new_entries


def _identity_signature(entry: AgentEntry) -> tuple[str, str, str, str]:
    """Fields whose mutation can change what token addresses a registry row."""
    return (
        entry.name,
        entry.short_id or "",
        entry.harness or "",
        entry.harness_session_id or "",
    )


def _validate_changed_identities(
    before: dict[str, tuple[str, str, str, str]], entries: list[AgentEntry]
) -> None:
    """Reject a newly minted address that shadows any existing row.

    Historical legacy-prefix collisions remain readable and resolve as
    ambiguity. Tokens come in two kinds. Chosen tokens (the row name and an
    explicit transport short id) may not shadow another row's name, short id,
    or any session address tier. Minted tokens (derived from the harness-minted
    session id) shadow only the SAME session: a tier-0 full-id match. A
    first-eight overlap between two DIFFERENT sessions is not a collision -
    codex ids are time-prefixed, so sessions started in one window share their
    first eight, and resolution already fails closed on the shared short asking
    for the full id.
    """

    def _matches(token: str, other: AgentEntry, *, same_session_only: bool) -> bool:
        if not same_session_only and (
            token == other.name or (other.short_id and token == other.short_id)
        ):
            return True
        tier = _session_tier(other, token)
        if same_session_only:
            return tier == 0
        return tier is not None

    for index, candidate in enumerate(entries):
        if before.get(candidate.name) == _identity_signature(candidate):
            continue
        sid = candidate.harness_session_id or ""
        chosen_tokens = {candidate.name}
        if candidate.short_id:
            chosen_tokens.add(candidate.short_id)
        minted_tokens: set[str] = set()
        if sid:
            minted_tokens.update((sid, canonical_handle(sid)))
        legacy = legacy_suffix_handle(sid) if sid else ""
        for other_index, other in enumerate(entries):
            if index == other_index:
                continue
            collision = next(
                (
                    token
                    for token in chosen_tokens
                    if _matches(token, other, same_session_only=False)
                ),
                None,
            ) or next(
                (
                    token
                    for token in minted_tokens
                    if _matches(token, other, same_session_only=True)
                ),
                None,
            )
            if collision is None and legacy and _matches(
                legacy, other, same_session_only=True
            ):
                collision = legacy
            if collision is not None:
                raise AgentResolutionError(
                    f"registry identity {collision!r} for new or changed row "
                    f"{candidate.name!r} collides with row {other.name!r}; use a "
                    "different name or the full session id",
                    ambiguous=True,
                )
