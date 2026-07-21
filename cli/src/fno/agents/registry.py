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
time the on-disk shape changes; loading a registry with a different
``schema_version`` raises ``RegistryVersionError`` so fno refuses to
silently misread it. Malformed JSON, non-dict rows, and unknown providers
all surface as ``RegistryVersionError`` too — callers handle alien shape
through one exception type.
"""
from __future__ import annotations

import contextlib
import fcntl
import json
import os
import re
import sys
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Iterator, Literal, Optional

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

from fno import paths
from fno.harness_identity import canonical_handle, sync_harness_aliases

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
SCHEMA_VERSION = 10


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
    # Canonical identity axis (x-880e, v10). The harness name is the SOLE on-disk
    # identity; the legacy ``provider`` and per-provider session-id fields are gone.
    # ``harness_session_id`` (below) is the worker's own session id in its harness's
    # store (claude full UUID, codex thread id, gemini session id). load_registry
    # back-fills both from a legacy row's ``provider`` / per-provider keys on read.
    harness: str
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

    @property
    def session_id(self) -> Optional[str]:
        """The harness-specific resume-target id.

        Resolves to whichever stored field the resume path consumes:
        ``short_id`` (``claude attach``), ``codex_session_id``
        (``codex resume <uuid>``), or ``gemini_session_id``. ``None`` for
        unknown harnesses or when the id was never captured.

        The harness -> field mapping comes from the module-level
        :data:`HARNESS_SESSION_ID_FIELDS`, which ``resume_cli._session_id_for``
        also reads, so the two cannot drift. Keyed on ``harness`` (x-880e, the
        sole identity axis). As a ``@property`` this is excluded from ``asdict``
        serialization and never becomes an on-disk storage field.
        """
        field_name = HARNESS_SESSION_ID_FIELDS.get(self.harness)
        # `short_id` is a str defaulting to "" (never None); normalize the
        # empty transport key to None so callers keep their `is None` checks.
        return (getattr(self, field_name) or None) if field_name else None


# ---------------------------------------------------------------------------
# Shared identifier resolver (x-1b1e): every session-connecting `fno agents`
# verb accepts ONE of three address forms — the registry name/slug, the full
# harness_session_id, or an 8-hex short. This one function is the single lookup
# choke point so no verb re-implements a name-only `.find`.
# ---------------------------------------------------------------------------

# Exactly-8 lowercase hex, the `spawn --resume` convention (_SHORT_ID_RE, PR #397).
# A 7- or 9-char token is NOT a short; it falls through to name/full-id, then the
# not-found error.
_DERIVED_SHORT_RE = re.compile(r"^[0-9a-f]{8}$")

_ACCEPTED_FORMS = "accepted forms: name, 8-hex short id, or full session id"


class AgentResolutionError(RuntimeError):
    """No entry, an ambiguous token, or an unreadable registry.

    ``exit_code`` defaults to 2 (the lifecycle name-not-found convention) for a
    caller that maps the error straight through (``raise typer.Exit(exc.exit_code)``,
    e.g. ``watch``). Verbs with their own convention still override it — resume
    reports 13, trace/stop/rm map through their existing not-found path — so this
    default is the fallback, not a universal choke point.

    ``ambiguous`` distinguishes "this token names several agents" from "no agent
    matches". Both are resolution failures, but only a MISS may fall through to
    the harness-store fallback (x-9cc5): a token the registry already refuses to
    disambiguate must keep refusing, or a store hit on one of the candidates
    would silently pick the winner the registry deliberately would not.
    """

    def __init__(
        self, message: str, *, exit_code: int = 2, ambiguous: bool = False
    ) -> None:
        super().__init__(message)
        self.exit_code = exit_code
        self.ambiguous = ambiguous


@dataclass
class ResolvedAgent:
    """The entry a token resolved to, plus which rule matched.

    ``worker_short_id`` is the transport handle a session-connecting verb
    shells out with (``claude attach/logs <short>`` etc.); ``None`` when the
    row recorded no short (a pre-heal claude row) so the verb can raise its own
    explicit "no short id on file" error instead of shelling an empty arg.
    """

    entry: AgentEntry
    matched_by: str  # "name" | "full_session_id" | "short_id" | "derived_short"

    @property
    def worker_short_id(self) -> Optional[str]:
        return self.entry.short_id or None


def _full_session_ids(entry: object) -> list[str]:
    """The canonical full session id, lowercased (x-880e: the per-provider
    full-id fields are gone; harness_session_id is their single successor).

    ``getattr`` so the resolver core also accepts a duck-typed registry row
    (e.g. a test's SimpleNamespace)."""
    hsid = getattr(entry, "harness_session_id", None)
    return [hsid.lower()] if hsid else []


def _derived_short(entry: object) -> Optional[str]:
    """The canonical addressing short: the first 8 hex of harness_session_id
    (claude's jobId is built the same way, so for a claude row it equals the
    stored short_id). ``None`` for a row whose id is unresolved or non-hex
    (e.g. an opencode ``ses_...`` id), so the derived rule simply never fires."""
    hsid = getattr(entry, "harness_session_id", None)
    if not hsid:
        return None
    lead = hsid.split("-", 1)[0].lower()
    return lead if _DERIVED_SHORT_RE.match(lead) else None


def _one_or_ambiguous(hits: list, matched_by: str, token: str) -> ResolvedAgent:
    """Return the single matched entry, or raise on a real ambiguity.

    Dedups by ``name`` (the PK), so the SAME entry matching a tier via multiple
    rules is not ambiguous; two DISTINCT entries are (git's ambiguous-short-SHA
    behavior — never silently pick one)."""
    distinct = {getattr(e, "name", None): e for e in hits}
    if len(distinct) > 1:
        cands = ", ".join(
            f"{getattr(e, 'name', '?')} (short={getattr(e, 'short_id', '') or '-'}, "
            f"{getattr(e, 'harness', '?')})"
            for e in distinct.values()
        )
        raise AgentResolutionError(
            f"token {token!r} is ambiguous across {len(distinct)} agents: "
            f"{cands}. Disambiguate with the name or full session id.",
            ambiguous=True,
        )
    return ResolvedAgent(entry=next(iter(distinct.values())), matched_by=matched_by)


def resolve_agent_in(entries: list, token: str) -> ResolvedAgent:
    """The 4-rule matching core over an already-loaded entry list (the Rust
    ``find_agent_entry`` mirror). Precedence: exact name, exact full session id
    (case-insensitive), exact stored short_id (shape-agnostic), derived 8-hex
    prefix. Name wins first so a hex-shaped name is byte-stable.

    ``getattr``-based, so both real ``AgentEntry`` rows and duck-typed rows (a
    verb that injects its own registry loader) resolve identically. Raises
    :class:`AgentResolutionError` (exit 2) on empty/unknown/ambiguous."""
    token = (token or "").strip()
    if not token:
        raise AgentResolutionError(f"empty agent token; {_ACCEPTED_FORMS}")
    low = token.lower()

    named = [e for e in entries if getattr(e, "name", None) == token]
    if named:
        return _one_or_ambiguous(named, "name", token)

    by_full = [e for e in entries if low in _full_session_ids(e)]
    if by_full:
        return _one_or_ambiguous(by_full, "full_session_id", token)

    by_short = [e for e in entries if getattr(e, "short_id", None) == token]
    if by_short:
        return _one_or_ambiguous(by_short, "short_id", token)

    if _DERIVED_SHORT_RE.match(low):
        by_derived = [e for e in entries if _derived_short(e) == low]
        if by_derived:
            return _one_or_ambiguous(by_derived, "derived_short", token)

    raise AgentResolutionError(f"no agent matching {token!r}; {_ACCEPTED_FORMS}")


def resolve_agent(token: str, *, path: Optional[Path] = None) -> ResolvedAgent:
    """Resolve ``token`` to one registry entry, loading the registry first.

    Wraps :func:`resolve_agent_in`; a malformed/unreadable registry degrades to
    a clean :class:`AgentResolutionError`, never a traceback leaking to the
    verb. See ``resolve_agent_in`` for the matching rules.

    On a MISS ONLY, a session-shaped token falls through to the harness stores
    (x-9cc5): the registry is a cache of reality, so a real session with no
    roster row is adopted here rather than refused. The happy path pays nothing
    -- this is a hot seam (spawn dedup, mail), and a hit never reaches the probe.
    """
    try:
        entries = load_registry(path=path)
    except RegistryVersionError as exc:
        raise AgentResolutionError(
            f"registry unreadable ({exc}); cannot resolve {token!r}"
        ) from exc
    try:
        return resolve_agent_in(entries, token)
    except AgentResolutionError as exc:
        # A MISS may fall through; a registry the caller must disambiguate must
        # not. Otherwise a store hit on one of several matching rows would pick
        # the winner the registry deliberately refused to pick.
        if exc.ambiguous:
            raise
        entry = resolve_from_harness_store(token, registry_path=path)
        if entry is None:
            raise
        return ResolvedAgent(entry=entry, matched_by="harness_store")


def resolve_from_harness_store(
    token: str, *, registry_path: Optional[Path] = None
) -> Optional[AgentEntry]:
    """The registry-miss healer (x-9cc5), isolated so every resolution surface
    reaches it identically -- including ``resume``, which loads its own entries
    and so calls :func:`resolve_agent_in` rather than :func:`resolve_agent`.

    Returns ``None`` when no store knows the token, so the caller raises its own
    unchanged error. Propagates :class:`AgentResolutionError` on an ambiguous
    token: refusing to guess is the designed outcome, not a miss."""
    from fno.agents.store_fallback import heal_from_harness_store

    return heal_from_harness_store(token, registry_path=registry_path)


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


@contextlib.contextmanager
def _hold_registry_lock(registry_path: Path) -> Iterator[None]:
    """Block-acquire the registry-wide flock for the duration of the with-block."""
    lock_file = _registry_lock_path(registry_path)
    lock_file.parent.mkdir(parents=True, exist_ok=True)
    with open(lock_file, "w") as fh:
        fcntl.flock(fh, fcntl.LOCK_EX)
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


def write_registry(entries: list[AgentEntry], path: Optional[Path] = None) -> None:
    """Atomically write the registry to disk.

    Serialization happens before the temp file is opened so an exception in
    ``_json_dumps`` cannot corrupt the existing file. The encoded text is
    written to ``<path>.tmp`` and renamed into place via ``os.replace``.
    On a post-serialization failure (e.g. ENOSPC during ``write_text``),
    the orphan ``.tmp`` is unlinked so it doesn't accumulate on retry.
    """
    target = _registry_path(path)
    for e in entries:
        _validate_single_live_ref(e)
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


def load_registry(path: Optional[Path] = None) -> list[AgentEntry]:
    """Load the registry. Returns ``[]`` if the file does not exist.

    Every alien-shape failure mode raises ``RegistryVersionError`` so
    callers handle "this file looks wrong" through one exception type:
    invalid JSON, top-level not-a-dict, ``agents`` not-a-list, row
    not-a-dict, ``schema_version`` mismatch, an identity-less/corrupt-shape
    row, and unknown / missing AgentEntry fields all map to that one error.
    A future fno adding fields without bumping the schema_version must
    not silently corrupt the in-memory entry. Identity (provider/harness) is
    a shape check, not an enumeration (x-8dfc): one alien harness never bricks
    the shared read; dispatch capability is gated at the spawn/ask seam.
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
    if not (
        isinstance(on_disk_version, int)
        and 1 <= on_disk_version <= SCHEMA_VERSION
    ):
        raise RegistryVersionError(
            f"registry at {target} has schema_version={on_disk_version!r}, "
            f"this fno understands schema_version={SCHEMA_VERSION}. "
            "Upgrade or downgrade fno to match."
        )
    needs_v1_synthesis = on_disk_version == 1
    needs_v2_synthesis = on_disk_version <= 2

    agents_field = raw.get("agents", [])
    if not isinstance(agents_field, list):
        raise RegistryVersionError(
            f"registry at {target} 'agents' field is not a list "
            f"(got {type(agents_field).__name__})"
        )

    entries: list[AgentEntry] = []
    for index, row in enumerate(agents_field):
        if not isinstance(row, dict):
            raise RegistryVersionError(
                f"registry at {target} row {index} is not a JSON object "
                f"(got {type(row).__name__})"
            )
        provider = row.get("provider")
        harness = row.get("harness")
        # Identity is one axis (x-8dfc). The read tolerates ANY well-shaped
        # identity token so a single alien-harness row never bricks the shared
        # registry read (mail send, spawn-collision check, whoami all ride it);
        # "can THIS fno DISPATCH the row?" is enforced later at the spawn/ask
        # seam via KNOWN_PROVIDERS, not here. The corruption guard survives as a
        # shape check: at least one of provider/harness must be a valid token.
        if not (_is_identity_token(provider) or _is_identity_token(harness)):
            raise RegistryVersionError(
                f"registry at {target} row {index} has no valid identity token "
                f"(provider={provider!r}, harness={harness!r}); a row needs a "
                "non-empty lowercase provider or harness. "
                "Upgrade or downgrade fno to match."
            )
        # Divergence is loud, not fatal (x-8dfc): a writer bug stamping
        # provider != harness surfaces in the skew window instead of silently
        # after the v10 provider-field removal. harness wins for identity
        # (the backfill below leaves both in place; session_id keys on harness).
        if (
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
        if not _is_identity_token(row.get("harness")) and _is_identity_token(row.get("provider")):
            row = {**row, "harness": row["provider"]}
        # sync_harness_aliases reads the per-provider session keys still present in
        # the raw row and back-fills harness_session_id from the harness-matching
        # one (canonical wins on divergence). Runs BEFORE the pop below.
        row = sync_harness_aliases(dict(row), REGISTRY_LEGACY_SESSION_KEYS)
        # Drop the removed identity keys now that their values have back-filled
        # harness / harness_session_id, so they never reach AgentEntry(**row)
        # (which no longer defines them) and never round-trip through asdict.
        for _dead in ("provider", "codex_session_id", "gemini_session_id", "claude_session_uuid"):
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
        try:
            entries.append(AgentEntry(**row))
        except TypeError as exc:
            raise RegistryVersionError(
                f"registry at {target} row {index} has malformed shape "
                f"(unknown or missing fields): {exc}. "
                "Upgrade or downgrade fno to match."
            ) from exc
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
    row in place rather than appending a duplicate. A genuinely new
    session whose derived name collides with a different row gets a
    numeric suffix, so two sessions in one cwd stay addressable under
    distinct names (AC7-EDGE).

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
                    entry.short_id = short_id
                return entries
        base = name or canonical_handle(session_id)
        taken = {entry.name for entry in entries}
        chosen, suffix = base, 2
        while chosen in taken:
            chosen = f"{base}-{suffix}"
            suffix += 1
        fresh = AgentEntry(
            name=chosen,
            harness=provider,
            harness_session_id=session_id,
            cwd=cwd,
            log_path=log_path,
            status=_REGISTERED_STATUS,
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


def update_registry(
    updater: Callable[[list[AgentEntry]], list[AgentEntry]],
    path: Optional[Path] = None,
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
    with _hold_registry_lock(target):
        current = load_registry(path=target)
        new_entries = updater(list(current))
        write_registry(new_entries, path=target)
        return new_entries
