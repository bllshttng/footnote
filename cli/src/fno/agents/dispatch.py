"""Provider selection and ask-dispatch orchestrator for fno agents.

Phase 1 surface:

- ``KNOWN_PROVIDERS`` — frozen tuple of supported provider names.
- ``is_provider_available(name)`` — wraps ``shutil.which`` for a single CLI.
- ``available_providers()`` — fan-out check for all known providers.
- ``select_provider(name, requested_provider)`` — registry-aware selection
  that catches the "wrong provider on follow-up" mistake before any
  subprocess fires.

US1 surface (this module):

- ``dispatch_ask(name, message, provider, cwd, timeout, lock_timeout)`` —
  orchestrates is_provider_available + per-agent flock + select_provider
  (INSIDE the flock per architecture step 3) + provider.bg_create +
  update_registry + events. Returns the parsed short-id on success.

The actual subprocess invocation per provider lives in
``fno.agents.harnesses.{claude,codex}``. Gemini is a legacy readable identity,
not a maintained Python dispatch provider.
"""

from __future__ import annotations

import contextvars
import os
import re
import select
import shutil
import subprocess
import sys
import time
from contextlib import contextmanager
from dataclasses import dataclass, field, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Iterator, Literal, Mapping, Optional

from fno import paths
from fno.agents import events
from fno.agents import rm_notice
from fno.agents.context import EventContext, build_context
from fno.agents.harness_map import DispatchResolveError, normalize_command
from fno.agents.lock import AgentLockTimeout, hold_agent_lock
from fno.agents.harnesses import KNOWN_PROVIDERS
from fno.agents.harnesses.base import ProviderResult, ReachabilityProbeError
from fno.agents.registry import (
    AgentEntry,
    AgentResolutionError,
    AgentStatus,
    RegistryVersionError,
    TERMINAL_STATUSES,
    load_registry,
    resolve_registered_agent_across_sources,
    update_registry,
)
from fno.agents.crown import calling_agent_row, crown_validation_error, grant_error
from fno.agents.whoami import is_caller_row
from fno.harness_identity import (
    canonical_handle,
    session_identity_key,
)

DispatchKind = Literal["create", "followup"]
RecipientIdentity = tuple[
    str,
    str,
    Optional[str],
    str,
    Optional[str],
    Optional[tuple[object, object]],
    str,
]
SwitchboardIdentity = dict[str, object]


def _recipient_identity_key(entry: AgentEntry) -> RecipientIdentity:
    """Snapshot every field that can select or replace a live recipient."""
    session_id = getattr(entry, "harness_session_id", None) or getattr(
        entry, "session_id", None
    )
    mux = entry.mux
    return (
        entry.name,
        entry.harness,
        session_identity_key(session_id) if session_id else None,
        entry.short_id,
        entry.mcp_channel_id,
        (
            (mux.get("session"), mux.get("pane_id"))
            if mux is not None
            else None
        ),
        entry.created_at,
    )


def _switchboard_identity(entry: AgentEntry) -> SwitchboardIdentity:
    """Identity fields the daemon must match before driving a named stream."""
    return {
        "harness": entry.harness,
        "session_id": entry.harness_session_id,
        "short_id": entry.short_id,
        "created_at": entry.created_at,
    }


def _update_registry_if_recipient_unchanged(
    name: str,
    expected_identity: RecipientIdentity,
    updater: Callable[[list[AgentEntry]], list[AgentEntry]],
    *,
    registry_path: Optional[Path] = None,
    registry_lock_timeout: Optional[float] = None,
    decline_reason: Optional[list[str]] = None,
) -> bool:
    """Apply a post-side-effect write only to the selected recipient.

    Exact-name lifecycle calls follow the current owner until the per-name lock
    selects a concrete row. Every call then carries that row's identity through
    the final registry-wide lock so a writer that does not honor the per-name
    lock cannot make a replacement inherit the registry mutation.

    `decline_reason`, when passed, gets one of `"row_removed"` (no row this
    name any more), `"duplicate_name"` (more than one row now shares the
    name, an ambiguous registry state distinct from either a clean removal or
    a restamp), or `"identity_changed"` (exactly one row exists but is not
    the one the caller resolved) appended on a declined write -- callers that
    need to tell those apart for telemetry (self-review finding: row-removed
    and identity-changed used to log as the same `RecipientIdentityChanged`,
    hiding a removed-entirely row behind the restamp-race label) read it
    after the call.
    """
    applied = False

    def _guarded(entries: list[AgentEntry]) -> list[AgentEntry]:
        nonlocal applied
        matches = [entry for entry in entries if entry.name == name]
        if (
            len(matches) != 1
            or _recipient_identity_key(matches[0]) != expected_identity
        ):
            if decline_reason is not None:
                if not matches:
                    decline_reason.append("row_removed")
                elif len(matches) > 1:
                    decline_reason.append("duplicate_name")
                else:
                    decline_reason.append("identity_changed")
            return entries
        applied = True
        return updater(entries)

    if registry_path is None:
        if registry_lock_timeout is None:
            update_registry(_guarded)
        else:
            update_registry(_guarded, lock_timeout=registry_lock_timeout)
    else:
        if registry_lock_timeout is None:
            update_registry(_guarded, path=registry_path)
        else:
            update_registry(
                _guarded,
                path=registry_path,
                lock_timeout=registry_lock_timeout,
            )
    return applied


# ---------------------------------------------------------------------------
# Dispatch-scoped context propagation (Task 2.1)
# ---------------------------------------------------------------------------
#
# ``dispatch_ask`` builds an ``EventContext`` once it knows the recipient
# provider (after ``select_provider``) and stashes it on this ContextVar
# so the helpers it calls can emit context-enriched events without
# threading ``ctx`` through every keyword-arg list. ContextVar is the
# right substrate because:
#
# - It is automatically isolated per-task / per-thread (no module-global
#   races between concurrent dispatch_ask calls in different threads).
# - The ``set(...)`` + ``reset(token)`` cycle ensures no leakage across
#   dispatches even when an exception unwinds the stack.
# - Test code can read it cheaply for assertion (or ignore it; helpers
#   that don't observe the contextvar fall back to legacy ``emit``).
_DISPATCH_CTX: contextvars.ContextVar[Optional[EventContext]] = contextvars.ContextVar(
    "fno_dispatch_ctx", default=None
)


def _emit_ev(kind: str, **data: Any) -> None:
    """Emit an event with the active dispatch ``EventContext`` if set.

    Falls back to legacy ``events.emit`` when ``_DISPATCH_CTX`` is unset
    so callers outside the dispatch_ask scope (or pre-migration code
    paths) still produce valid records.
    """
    ctx = _DISPATCH_CTX.get()
    if ctx is not None:
        events.emit_with_context(ctx, kind, **data)
    else:
        events.emit(kind, **data)


@dataclass(frozen=True)
class DispatchAskResult:
    """Return shape for :func:`dispatch_ask`.

    ``kind`` discriminates the two paths the auto-router takes:

    - ``"create"`` — agent name was new; ``short_id`` is the provider's
      newly-minted supervisor id (e.g. claude 8-hex). ``reply`` is None.
      The CLI prints ``<short_id>\\n`` per US1's contract.
    - ``"followup"`` — agent name existed; ``short_id`` is the existing
      registry entry's id, and ``reply`` carries the recipient's reply
      text. The CLI prints ``reply`` verbatim (no trailing newline
      added) per US2 AC2-HP.
    """

    kind: DispatchKind
    short_id: str
    reply: Optional[str] = None
    duration_ms: Optional[int] = None

    def __post_init__(self) -> None:
        if self.kind not in ("create", "followup"):
            raise ValueError(
                f"DispatchAskResult.kind must be 'create' or 'followup', got {self.kind!r}"
            )
        if self.kind == "followup" and self.reply is None:
            raise ValueError("DispatchAskResult.reply is required when kind='followup'")


class ProviderMismatchError(RuntimeError):
    """Raised when a follow-up ``ask`` passes a provider that disagrees with the registry."""


def _check_known_provider(name: str) -> None:
    if name not in KNOWN_PROVIDERS:
        raise ValueError(f"unknown provider {name!r}; supported: {', '.join(KNOWN_PROVIDERS)}")


def is_provider_available(name: str) -> bool:
    """Return True iff the named provider CLI is on PATH.

    Raises ``ValueError`` if ``name`` is not in :data:`KNOWN_PROVIDERS`.
    """
    _check_known_provider(name)
    return shutil.which(name) is not None


def available_providers() -> dict[str, bool]:
    """Return a {name: bool} availability map for every known provider."""
    return {name: shutil.which(name) is not None for name in KNOWN_PROVIDERS}


def select_provider(name: str, requested_provider: Optional[str]) -> str:
    """Select the provider for ``fno agents ask <name>``.

    Logic:
      - If ``requested_provider`` is given, validate it against
        :data:`KNOWN_PROVIDERS`.
      - If the agent already exists in the registry:
        - No request: return the recorded provider.
        - Request matches: return it.
        - Request mismatches: raise :class:`ProviderMismatchError` with a
          message that names the agent, recorded provider, and requested
          provider. This catches the mistaken-reuse failure mode that a
          silent "ignored" path would mask.
      - If the agent is new:
        - Request given: return it.
        - No request: raise ``ValueError`` because there is nothing to
          select for a brand-new agent.
    """
    if requested_provider is not None:
        _check_known_provider(requested_provider)

    existing = next(
        (entry for entry in load_registry() if entry.name == name),
        None,
    )

    if existing is not None:
        if requested_provider is None or requested_provider == existing.harness:
            return existing.harness
        raise ProviderMismatchError(
            f"agent {name!r} is provider={existing.harness}, "
            f"refusing to follow-up as provider={requested_provider}"
        )

    if requested_provider is None:
        raise ValueError(
            f"provider is required for new agent {name!r}; "
            f"pass --provider one of: {', '.join(KNOWN_PROVIDERS)}"
        )
    return requested_provider


# ---------------------------------------------------------------------------
# dispatch_ask — US1 orchestrator
# ---------------------------------------------------------------------------


# Absorbing states: once a row reaches one, no probe may move it and no identity
# may be backfilled onto it. Named once because reconcile tests it on three
# separate paths and a set that drifts between them reopens the resurrection bug.
_TERMINAL_AGENT_STATUSES = frozenset({"orphaned", "failed", "exited", "permanent_dead"})

_NAME_MAX_LEN = 128
_SHORT_ID_NAME_SHAPE = re.compile(r"^[0-9a-f]{8}$")
_DEFAULT_LOCK_TIMEOUT = 30.0

# Every refusal that wrote nothing says this, once. The bus-lock timeout
# already carries it, so a wrapper that appends it blindly says it twice.
_NO_ENVELOPE_CLAUSE = "no durable envelope was written"

# Floor for the post-timeout durable queue's grace window. A durable write
# needs the lock because only the lock proves the recipient row is committed,
# so a queue that cannot take it refuses instead of guessing.
_LOCK_TIMEOUT_QUEUE_GRACE_SECONDS = 2.0


def _queue_grace_seconds(lock_timeout: float) -> float:
    """Seconds the post-timeout queue waits for the flock before giving up.

    A flat floor loses to the contention it exists to catch. The holder owns
    the lock for its whole live-delivery attempt, budgeted at
    `_MAIL_INJECT_LIVENESS_SCALED_TIMEOUT_S`, so a caller that gave up at the
    default 30s is usually waiting on a holder still running toward 40s: it
    would retry for two seconds, miss by eight, and drop the very message it
    came here to save. The window therefore covers the INJECT budget, measured
    from what the caller already waited.

    It does NOT cover every holder. The same flock spans a synchronous
    switchboard hop, whose read budget is `_SWITCHBOARD_READ_TIMEOUT` (130s),
    so a sender queued behind an A2A hop still exhausts this window and takes
    the exit-11 arm. Sizing for 130s would make a routine `fno agents mail send`
    block over two minutes before saying anything, which is a worse trade than
    the loud refusal. Narrowing the flock to the registry mutation is the real
    fix; it is tracked separately, for the reason the block comment in
    `dispatch_send` gives.

    Capped at that same wait, because the cap is what keeps the rule honest
    for a caller who asked for a short one: a 0.1s `lock_timeout` means "do
    not wait", and spending 42s on the fallback would answer a question
    nobody asked. Past the ceiling the floor is all that is left, and all the
    identity-consistency read needs.

    The floor outranks the cap below two seconds, so a 0.1s caller still
    blocks 2.0s here. That is deliberate and the test pins it: the grace
    window's other job is the identity-consistency read, which a sub-second
    budget cannot do at all, and a fallback that cannot verify the recipient
    writes nothing. The cap governs the range where both jobs fit.
    """
    residual = _MAIL_INJECT_LIVENESS_SCALED_TIMEOUT_S + _LOCK_TIMEOUT_QUEUE_GRACE_SECONDS
    return max(
        _LOCK_TIMEOUT_QUEUE_GRACE_SECONDS,
        min(residual - lock_timeout, lock_timeout),
    )

# Receipt reason for a message the lock-timeout lane queued. Exported because
# the receipt formatter in fno.mail.cli must branch on it: a contended lock
# means the recipient was BUSY, and the generic "not live" copy would send the
# reader to resurrect a session that is working fine.
LOCK_TIMEOUT_REASON = "agent-lock-timeout"

_FROM_NAME_MAX_LEN = 128
_FROM_NAME_DEFAULT = "fno"
_FROM_NAME_FORBIDDEN_CHARS = frozenset('"<>&')
_DEFAULT_FOLLOWUP_TIMEOUT_SEC = 600.0

# x-c393: how recent an inside_leg report must be for a worker to count as
# "provably live" when a follow-up fails to route. Mirrors the Rust
# PROVABLY_LIVE_WINDOW_SECS; `fno agents reconcile` (the `claude logs` probe) is
# the eventual authority that orphans a genuinely dead worker.
_PROVABLY_LIVE_WINDOW_SEC = 3600.0


def _inside_leg_is_recent(
    inside_leg: Optional[dict],
    now_epoch: float,
    window_sec: float = _PROVABLY_LIVE_WINDOW_SEC,
) -> bool:
    """True when the row's ``inside_leg`` report is within ``window_sec`` of now.

    A live bg worker whose registry identity merely wasn't routable (the
    null-uuid gap, x-c393) still emits ``inside_leg`` reports, so a routing miss
    on such a row is a gap, not a death. An absent report or unparseable stamp
    is NOT recent (fail closed), so a genuinely dead / corrupt row still orphans.
    """
    if not isinstance(inside_leg, dict):
        return False
    stamp = inside_leg.get("received_at")
    if not isinstance(stamp, str) or not stamp:
        return False
    try:
        recv = datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
    except (ValueError, TypeError):
        return False
    # A future stamp (recv > now) is corrupt / clock-skewed, not recent: require
    # recv <= now so it cannot suppress orphaning (fail closed).
    return recv <= now_epoch and (now_epoch - recv) <= window_sec


def _current_inside_leg(name: str) -> Optional[dict]:
    """Read the row's CURRENT ``inside_leg``, not the pre-ask snapshot.

    The ask can run for up to the follow-up timeout; deciding orphan-vs-live off
    the row as it was BEFORE the send would miss a report that landed during it
    (codex P2). A fresh read right before the guard closes that window. Read
    failure -> ``None`` (fail closed: no liveness signal -> orphan as today).
    """
    try:
        for entry in load_registry():
            if entry.name == name:
                return entry.inside_leg
    except (OSError, RegistryVersionError):
        return None
    return None


class DispatchAskError(RuntimeError):
    """Raised by :func:`dispatch_ask` for any callable failure.

    Carries the exit code the CLI layer should propagate to the shell.
    """

    def __init__(self, message: str, *, exit_code: int) -> None:
        super().__init__(message)
        self.exit_code = exit_code


# Exit-code taxonomy (documented here for cross-language parity with Rust Task 1.3):
#   1  subprocess failure
#   2  usage / input validation
#   11 lock timeout
#   12 registry I/O
#   13 provider refused / orphan
#   14 provider CLI not on PATH
#   15 reply timeout
#   16 unknown agent name (agent must be created via spawn/host first)
UNKNOWN_AGENT_EXIT_CODE = 16


def _validate_inputs(
    name: str, message: str, from_name: str, *, name_is_address: bool = False
) -> None:
    """Reject inputs that fail the AC1-ERR / AC1-EDGE / AC2-ERR boundary checks.

    ``name_is_address`` marks a caller whose ``name`` is a TARGET to resolve, not
    a name to create. The short-id-shape rejection below guards against NAMING an
    agent like an id; applied to a send target it rejects the canonical mailbox
    handle itself, which is the exact string `whoami` advertises.
    """
    if not name:
        raise DispatchAskError("agent name must not be empty", exit_code=2)
    if "/" in name or "\\" in name or ".." in name:
        raise DispatchAskError(
            f"agent name must not contain path separators or '..': {name!r}",
            exit_code=2,
        )
    if len(name) > _NAME_MAX_LEN:
        raise DispatchAskError(
            f"name must be <={_NAME_MAX_LEN} chars (got {len(name)})",
            exit_code=2,
        )
    if _SHORT_ID_NAME_SHAPE.match(name) and not name_is_address:
        raise DispatchAskError(
            f"agent name {name!r} must not match short-id shape "
            f"^[0-9a-f]{{8}}$ (prevents name/id collision)",
            exit_code=2,
        )
    # Reject characters that would corrupt env-var injection
    # (FNO_AGENT_SELF=<name>) on subprocess spawn. NUL bytes cause
    # subprocess.run to raise ValueError; \n/\r split a meta value
    # across lines in downstream consumers; `=` breaks the env-key=value
    # shape. Tightened in response to sigma-review H4 catching a crash
    # path when a name like "a\x00b" landed in the registry and crashed
    # every subsequent dispatch.
    _forbidden_env_chars = ("\x00", "\n", "\r", "=")
    bad = next((ch for ch in _forbidden_env_chars if ch in name), None)
    if bad is not None:
        raise DispatchAskError(
            f"agent name {name!r} contains a forbidden character "
            f"({bad!r} would corrupt subprocess env injection)",
            exit_code=2,
        )
    if not message or not message.strip():
        raise DispatchAskError("message must be non-empty", exit_code=2)
    _validate_from_name(from_name)


def _validate_from_name(from_name: str) -> None:
    """AC2-ERR: from_name must be non-empty, <=128 chars, XML-attribute-safe."""
    if not from_name:
        raise DispatchAskError("from-name must not be empty", exit_code=2)
    if len(from_name) > _FROM_NAME_MAX_LEN:
        raise DispatchAskError(
            f"from-name must be <={_FROM_NAME_MAX_LEN} chars (got {len(from_name)})",
            exit_code=2,
        )
    if any(ch in _FROM_NAME_FORBIDDEN_CHARS for ch in from_name):
        raise DispatchAskError(
            'from-name must not contain XML-unsafe characters (", <, >, &)',
            exit_code=2,
        )


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _followup_path(
    *,
    name: str,
    message: str,
    cwd: Path,
    from_name: str,
    existing: AgentEntry,
    timeout_sec: float,
    lock_handle,  # type: ignore[no-untyped-def]
) -> DispatchAskResult:
    """Execute the US2 follow-up against an already-registered agent.

    Runs INSIDE the per-agent flock acquired by :func:`dispatch_ask`.

    Side effects:
      - Emits ``agent_followup_started`` then exactly one of
        ``agent_followup_done`` or ``agent_followup_failed``.
      - Updates the registry entry: bumps ``last_message_at`` to now and
        sets ``status="live"`` on success; sets ``status="orphaned"`` on
        orphan failures (preserves the field for observability).
      - On post-send registry-write OSError: detaches the flock so the
        next caller sees the manual-cleanup signal (mirrors US1 AC1-FR
        registry-write semantics).

    Raises:
        DispatchAskError: with the documented exit code per AC2 failure
            mode (1, 11, 12, 13, 15 mapped from provider errors).
    """
    short_id = existing.short_id
    if not short_id:
        raise DispatchAskError(
            f"registry entry {name!r} has no short id on file; cannot follow up. "
            f"Remove with 'fno agents rm {name}' and recreate.",
            exit_code=12,
        )

    from fno.agents.harnesses import claude as claude_mod
    from fno.agents.harnesses.base import ReachabilityProbeError

    _emit_ev(
        "agent_followup_started",
        name=name,
        provider=existing.harness,
        short_id=short_id,
    )

    # --- Phase 5 (US6) MCP route selection ---------------------------
    # If this is a claude agent that was created with --channels
    # fno (mcp_channel_id is non-null), probe the MCP sidecar
    # and prefer that backend. Three failure modes demote silently to
    # the US2 socket path: probe returns False, probe raises (sidecar
    # unreachable), or send raises MCPChannelSendError after a True
    # probe. Each demotion emits mcp_channel_demoted_to_socket with a
    # machine-stable reason discriminator (per spec AC1-ERR / AC3-HP).
    reply: Optional[str] = None
    backend = "socket"
    demote_reason: Optional[str] = None
    demote_event_kind: Optional[str] = None
    # x-e21e: the MCP sidecar lane drives a recipient turn without routing
    # through any shared injector, so a bus-only row must not take it. Demote
    # to the socket path, whose injector gate refuses loud by policy.
    if _delivery_policy_refusal(existing) == BUS_ONLY_POLICY:
        mcp_alive = False
        demote_reason = BUS_ONLY_POLICY
    elif existing.harness == "claude" and existing.mcp_channel_id:
        try:
            mcp_alive = claude_mod.mcp_channel_reachable(existing.mcp_channel_id, timeout=0.25)
        except ReachabilityProbeError as probe_exc:
            mcp_alive = False
            demote_reason = probe_exc.reason  # "mcp_channel_disconnected"
            # Probe-raise path (spec routing decision tree §4d) -> the
            # mcp_channel_unreachable event kind, distinct from §4c
            # (probe returned False, the demoted-to-socket path).
            demote_event_kind = events.KIND_MCP_CHANNEL_UNREACHABLE
        if mcp_alive:
            # Outer try/except is exclusively for MCPChannelSendError →
            # demote-to-socket. ProviderOrphanError and ProviderTimeoutError
            # raised by the MCP path use the SAME exception classes the
            # socket-path handler block below already maps to exit codes
            # 13 and 15 (spec AC1-ERR codex P1, PR #323). We catch them
            # here ONLY to set backend="mcp" on the event payload so the
            # forensic trail records which transport failed; then we
            # re-raise so the standard handler block runs.
            try:
                reply = claude_mod.ask_followup_via_mcp(
                    claude_short_id=short_id,
                    message=message,
                    cwd=cwd,
                    from_name=from_name,
                    timeout=timeout_sec,
                    mcp_channel_id=existing.mcp_channel_id,
                )
                backend = "mcp"
            except claude_mod.MCPChannelSendError as send_exc:
                # Probe-True but send failed (spec AC1-ERR).
                demote_reason = f"send_failed_post_probe:{send_exc.reason}"
                demote_event_kind = events.KIND_MCP_CHANNEL_DEMOTED_TO_SOCKET
            except claude_mod.ProviderOrphanError as orphan_exc:
                # x-c393: same provably-live guard as the socket path below --
                # a recent inside_leg report means a routing gap, not a death,
                # so skip the orphan stamp and report it as a routing gap.
                truth_routing_gap = orphan_exc.reason == "truth-live-inject-failed"
                if truth_routing_gap or _inside_leg_is_recent(
                    _current_inside_leg(name), time.time()
                ):
                    events.emit(
                        "agent_followup_failed",
                        stage="routing-gap",
                        name=name,
                        short_id=short_id,
                        backend="mcp",
                        reason=orphan_exc.reason,
                    )
                    raise DispatchAskError(
                        f"agent {name!r} is live but not currently routable "
                        f"(reason: {orphan_exc.reason}); message not delivered. "
                        f"Try 'claude attach {short_id}'",
                        exit_code=13,
                    ) from orphan_exc
                # Same exit code (13) + status="orphaned" stamp as the
                # socket-path orphan handler below. We do NOT fall back
                # to socket here — orphan means the session itself is
                # gone, not just the MCP channel. The socket path would
                # fail the same way.
                try:
                    update_registry(
                        _stamp_status(name, status="orphaned", last_message_at_preserve=True)
                    )
                except (OSError, RegistryVersionError) as stamp_exc:
                    print(
                        f"fno agents: warning: failed to mark {name!r} as orphaned: {stamp_exc}",
                        file=sys.stderr,
                    )
                events.emit(
                    "agent_followup_failed",
                    stage="orphan",
                    name=name,
                    short_id=short_id,
                    backend="mcp",
                    reason=orphan_exc.reason,
                )
                raise DispatchAskError(
                    f"agent {name!r} is not running via MCP (reason: {orphan_exc.reason})",
                    exit_code=13,
                ) from orphan_exc
            except claude_mod.ProviderTimeoutError as timeout_exc:
                # Same exit code (15) as the socket-path timeout handler.
                # Timeout means the send went out (over MCP) but the
                # reply never arrived in state.json — socket fallback
                # wouldn't help because reply-polling uses the same
                # state.json regardless of send transport.
                events.emit(
                    "agent_followup_failed",
                    stage="poll-timeout",
                    name=name,
                    short_id=short_id,
                    backend="mcp",
                    elapsed_sec=timeout_exc.elapsed_sec,
                )
                raise DispatchAskError(
                    f"message sent via MCP but no reply within "
                    f"{int(timeout_exc.elapsed_sec)}s. Try "
                    f"'fno agents logs {name}' to read the transcript.",
                    exit_code=15,
                ) from timeout_exc
        elif demote_reason is None:
            # Sidecar alive but reports no such channel id -> session is
            # definitively orphaned at the MCP layer. Socket fallback
            # may still work (the bg socket survives MCP teardown).
            # This is the §4c branch (probe False).
            demote_reason = "channel_not_registered"
            demote_event_kind = events.KIND_MCP_CHANNEL_DEMOTED_TO_SOCKET
        if demote_reason is not None:
            events.emit(
                demote_event_kind or events.KIND_MCP_CHANNEL_DEMOTED_TO_SOCKET,
                name=name,
                short_id=short_id,
                mcp_channel_id=existing.mcp_channel_id,
                reason=demote_reason,
            )
            print(
                f"fno agents: warning: MCP channel unavailable for {name!r} "
                f"({demote_reason}); falling back to socket",
                file=sys.stderr,
            )

    if reply is None:
        try:
            reply = claude_mod.ask_followup(
                claude_short_id=short_id,
                message=message,
                cwd=cwd,
                from_name=from_name,
                timeout=timeout_sec,
            )
            backend = "socket_after_mcp_demote" if demote_reason else "socket"
        except claude_mod.ProviderOrphanError as exc:
            # x-c393: a live worker whose row merely wasn't routable (a recent
            # inside_leg report) is a routing gap, not a death -- do NOT stamp
            # it orphaned (that misleads `fno agents list`). reconcile's
            # `claude logs` probe stays the authority that orphans a dead one.
            #
            # x-2681: "roster-live-inject-failed" means the control.sock fallback
            # delivery failed on a session that IS live in the daemon roster --
            # also a routing gap, never a death, so it takes the same no-stamp
            # branch (AC6-FR: a roster-live session is never stamped orphaned).
            truth_routing_gap = exc.reason in {
                "roster-live-inject-failed",
                "truth-live-inject-failed",
            }
            if truth_routing_gap or _inside_leg_is_recent(_current_inside_leg(name), time.time()):
                events.emit(
                    "agent_followup_failed",
                    stage="routing-gap",
                    name=name,
                    short_id=short_id,
                    reason=exc.reason,
                )
                raise DispatchAskError(
                    f"agent {name!r} is live but not currently routable "
                    f"(reason: {exc.reason}); message not delivered. "
                    f"Try 'claude attach {short_id}'",
                    exit_code=13,
                ) from exc
            # Stamp status=orphaned on the registry entry so US3 list shows
            # the dead session. Errors during this best-effort update should
            # NOT mask the original orphan: the user's primary signal is the
            # orphan, not a downstream write blip. But losing visibility into
            # the secondary failure breaks debuggability (status="live" /
            # status="orphaned" drift between `list` and `ask`), so the swallow
            # is observable via the events log + a stderr warning.
            try:
                update_registry(
                    _stamp_status(name, status="orphaned", last_message_at_preserve=True)
                )
            except (OSError, RegistryVersionError) as stamp_exc:
                print(
                    f"fno agents: warning: failed to mark {name!r} as orphaned: {stamp_exc}",
                    file=sys.stderr,
                )
                events.emit(
                    "agent_status_stamp_failed",
                    name=name,
                    short_id=short_id,
                    target_status="orphaned",
                    error=str(stamp_exc),
                    error_type=type(stamp_exc).__name__,
                )
            events.emit(
                "agent_followup_failed",
                stage="orphan",
                name=name,
                short_id=short_id,
                reason=exc.reason,
            )
            if exc.reason == "socket-null":
                hint = (
                    f". Run 'claude attach {short_id}' to wake the session, "
                    f"or 'fno agents rm {name}' to remove"
                )
            elif exc.reason == "not-found":
                hint = f". Run 'fno agents rm {name}' to clear the stale entry"
            elif exc.reason == "liveness-failed":
                hint = (
                    f". Socket exists but is unresponsive; try "
                    f"'claude attach {short_id}' or 'fno agents rm {name}'"
                )
            else:
                # Defensive: a future OrphanReason variant should surface
                # explicitly here, not fall back to no-hint generic text.
                hint = (
                    f". Inspect with 'fno agents logs {name}' or remove via 'fno agents rm {name}'"
                )
            raise DispatchAskError(
                f"agent {name!r} is not running (reason: {exc.reason}{'; session is suspended' if exc.reason == 'socket-null' else ''})"
                + hint,
                exit_code=13,
            ) from exc
        except claude_mod.ProviderSocketError as exc:
            events.emit(
                "agent_followup_failed",
                stage="send",
                name=name,
                short_id=short_id,
                reason="socket-error",
                error=str(exc),
                error_type=type(exc).__name__,
            )
            raise DispatchAskError(str(exc), exit_code=1) from exc
        except claude_mod.ProviderTimeoutError as exc:
            events.emit(
                "agent_followup_failed",
                stage="poll-timeout",
                name=name,
                short_id=short_id,
                elapsed_sec=exc.elapsed_sec,
            )
            raise DispatchAskError(
                f"message sent but no reply within {int(exc.elapsed_sec)}s. "
                f"Try 'fno agents logs {name}' to read the transcript.",
                exit_code=15,
            ) from exc

    # Reply extracted successfully — bump registry. On OSError, the
    # message has already been delivered; AC2-FR demands the lock stay
    # held and stdout NOT show the reply.
    #
    # ``last_message_at=_utc_now_iso`` (callable, no parens) defers the
    # timestamp into the registry-wide flock so concurrent followups
    # stay strictly monotonic.
    try:
        update_registry(
            _stamp_status(name, status="live", last_message_at=_utc_now_iso),
        )
    except (OSError, RegistryVersionError) as exc:
        events.emit(
            "agent_followup_failed",
            stage="registry-write",
            name=name,
            short_id=short_id,
            error=str(exc),
            error_type=type(exc).__name__,
        )
        lock_handle.detach()
        raise DispatchAskError(
            f"registry write failed: {exc}. NOTE: message was already delivered; do not retry.",
            exit_code=12,
        ) from exc

    # Contract guard FIRST: ``reply`` is the value stdout emits on
    # success. The provider adapter must return "" (not None) when the
    # recipient produced no text; a None here is a contract breach in
    # ``ask_followup`` that would otherwise crash the event emit below
    # with TypeError(len(NoneType)) before the guard fired (Gemini
    # review on PR #295).
    if reply is None:
        events.emit(
            "agent_followup_failed",
            stage="provider-contract",
            name=name,
            short_id=short_id,
        )
        raise DispatchAskError(
            f"internal error: provider returned None reply for {name!r}; "
            "expected string (possibly empty). This is a bug in the "
            "fno provider adapter.",
            exit_code=12,
        )
    _emit_ev(
        "agent_followup_done",
        stage="followup",
        name=name,
        provider=existing.harness,
        short_id=short_id,
        reply_chars=len(reply),
        backend=backend,
    )
    return DispatchAskResult(kind="followup", short_id=short_id, reply=reply)


def _stamp_status(
    name: str,
    *,
    status: AgentStatus,
    last_message_at: Optional[str | Callable[[], str]] = None,
    last_message_at_preserve: bool = False,
):
    """Build an ``update_registry`` updater that bumps status/last_message_at.

    ``last_message_at`` may be a literal ``str``/``None`` (resolved at
    construction time) OR a ``Callable[[], str]`` invoked INSIDE the
    updater closure — i.e. while the registry-wide flock is held. The
    callable form is how dispatch_ask paths defer the timestamp into
    the lock so concurrent followups stay strictly monotonic per atomic
    write. The pre-lock pattern (``last_message_at=_utc_now_iso()``)
    was a latent race: lock-loser could carry an earlier timestamp than
    lock-winner and the winner's atomic write would persist the earlier
    value (US4-gemini handoff lifecycle item).
    """

    def _updater(entries: list[AgentEntry]) -> list[AgentEntry]:
        if last_message_at_preserve:
            resolved_last: Optional[str] = None  # not used; preserve branch
        elif callable(last_message_at):
            resolved_last = last_message_at()
        else:
            resolved_last = last_message_at  # str or None

        # Use ``dataclasses.replace`` for the same reason ``reconcile_agents``
        # does: future AgentEntry fields are preserved automatically without
        # needing to re-list every constructor argument (Gemini PR #319 review,
        # consistent with the PR #317 Gemini-medium fix).
        out: list[AgentEntry] = []
        for entry in entries:
            if entry.name != name:
                out.append(entry)
                continue
            if last_message_at_preserve:
                out.append(replace(entry, status=status))
            else:
                out.append(replace(entry, status=status, last_message_at=resolved_last))
        return out

    return _updater


def _derive_log_path(name: str) -> Path:
    """Stable fno-side log path for `fno agents logs <name>` (US3 plumbing)."""
    return paths.state_dir() / "agents" / "logs" / f"{name}.log"


def _touch_log_path(name: str) -> Optional[Path]:
    """Create (or reuse) the log file a mint site is about to record as a
    registry row's ``log_path`` (x-7bcd AC4): a ``log_path`` pointing at
    nothing is a claim, not evidence, so the file must exist before the row
    does. Returns ``None`` on a failed create (disk full, EROFS, a
    permission error) instead of raising or returning a path nothing backs,
    mirroring ``claude_ask.rs``'s ``log_file_created`` gate, so a caller
    records this leg only when it is real. Shared by every mint site
    (dispatch.py, mux_spawn.py) so the mkdir+touch idiom lives in exactly
    one place.
    """
    log_path = _derive_log_path(name)
    try:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_path.touch(exist_ok=True)
    except OSError:
        return None
    return log_path


def _codex_output_path(name: str) -> Path:
    """Tee target for the codex provider's JSONL stream (Locked Decision 8).

    Per agent design: ``<state_dir>/agents/<name>/output.jsonl``. ``fno
    agents logs <name>`` reads the same file (US3).
    """
    return paths.state_dir() / "agents" / name / "output.jsonl"


def _codex_create_path(
    *,
    name: str,
    message: str,
    cwd: Path,
    from_name: str,
    yolo: bool,
    timeout_sec: float,
    lock_handle,
    role: Optional[str] = None,
    model: Optional[str] = None,
    effort: Optional[str] = None,
    add_dir: Optional[str] = None,
) -> DispatchAskResult:
    """Spawn a new codex agent under the per-agent flock.

    Mirrors the claude create path's contract: invokes the provider
    adapter, persists the new registry row, emits structured events.
    Failure modes map to exit codes per the Failure Modes section of
    the US4-codex design doc:

    - codex not on PATH                  -> 14 (caller checked earlier)
    - 0-event JSONL stream                -> 11 (NoSessionIdError)
    - non-zero exit, no captured reply    -> 1 (CodexInvocationError)
    - wall-clock timeout                  -> 15 (CodexTimeoutError)
    - registry write failure post-create  -> 12 (with cleanup hint)
    """
    from fno.agents.harnesses import codex as codex_mod
    from fno.agents.model_routing import RouteCompositionError

    output_path = _codex_output_path(name)

    try:
        result = codex_mod.create(
            cwd=cwd,
            prompt=message,
            from_name=from_name,
            yolo=yolo,
            output_path=output_path,
            timeout=timeout_sec,
            agent_self=name,
            role=role,
            reasoning_effort=effort,
            add_dir=add_dir,
        )
    except RouteCompositionError as exc:
        events.emit(
            "agent_ask_failed",
            stage="codex-route",
            name=name,
            provider="codex",
            role=role,
        )
        raise DispatchAskError(str(exc), exit_code=2) from exc
    except codex_mod.NoSessionIdError as exc:
        events.emit(
            "agent_ask_failed",
            stage="codex-no-session",
            name=name,
            provider="codex",
            types_seen=sorted(exc.types_seen),
        )
        raise DispatchAskError(str(exc), exit_code=11) from exc
    except codex_mod.CodexTimeoutError as exc:
        events.emit(
            "agent_ask_failed",
            stage="codex-timeout",
            name=name,
            provider="codex",
            timeout_sec=exc.timeout_sec,
        )
        raise DispatchAskError(
            f"codex create timed out after {exc.timeout_sec}s",
            exit_code=15,
        ) from exc
    except codex_mod.CodexInvocationError as exc:
        events.emit(
            "agent_ask_failed",
            stage="codex-subprocess",
            name=name,
            provider="codex",
            returncode=exc.exit_code,
        )
        # Propagate codex's exit code (or the provider's structured code
        # like 12 for tee-open EACCES or 127 for missing binary) instead
        # of collapsing to 1. Gemini PR #305 round 3 flagged the prior
        # collapse as losing structured error context.
        raise DispatchAskError(
            f"codex exited {exc.exit_code} (see {output_path} for details)",
            exit_code=exc.exit_code if exc.exit_code != 0 else 1,
        ) from exc

    session_id = result.session_id
    assert session_id is not None  # codex.create raises NoSessionIdError otherwise

    new_entry = AgentEntry(
        name=name,
        cwd=str(cwd),
        log_path=str(output_path),
        harness="codex",
        provider="openai",
        model=model,
        effort=effort,
        harness_session_id=session_id,
        # The THIRD Python path that mints a worker row, after the pane and bg
        # paths. Its Rust counterpart in codex_ask.rs stamps this, so leaving it
        # off here made one codex worker read "spawn" and another read absent
        # purely by which language created it.
        origin="spawn",
    )

    try:
        update_registry(lambda entries: entries + [new_entry])
    except (AgentResolutionError, OSError, RegistryVersionError) as exc:
        events.emit_spawn_failed(
            name=name, provider=new_entry.harness, reason=f"registry-write: {exc}"
        )
        events.emit(
            "agent_ask_failed",
            stage="registry-write",
            name=name,
            provider="codex",
            codex_session_id=session_id,
        )
        # Hold the lock to surface manual-cleanup signal to the next caller;
        # mirrors AC1-FR semantics on the claude path.
        lock_handle.detach()
        raise DispatchAskError(
            f"registry write failed: {exc}. "
            f"orphaned codex session: codex sessions are persisted to disk; "
            f"clean up via 'codex sessions rm {session_id}' if desired",
            exit_code=12,
        ) from exc

    # Spawn birth (x-8cd5 Wave 6): the codex create path is the third spawn
    # seam after _claude_create_path and mux_spawn, and was the one a death
    # could dangle from. Emit to the daemon lifecycle log with the parent edge.
    _cx_session, _cx_harness, _cx_cwd = _capture_parent_edge()
    events.emit_spawned(
        name=name,
        short_id=session_id,
        provider=new_entry.harness,
        spawned_by_session=_cx_session,
        spawned_by_harness=_cx_harness,
        spawned_by_cwd=_cx_cwd,
    )
    _emit_ev(
        "agent_ask_done",
        stage="dispatch",
        name=name,
        provider="codex",
        codex_session_id=session_id,
        duration_ms=result.duration_ms,
        yolo=yolo,
    )
    # Codex's create path RETURNS the reply on stdout (per AC1-HP). Since
    # we cannot stretch the DispatchAskResult.kind="create" contract (which
    # claude uses to print short_id\n on stdout), we route to kind="followup"
    # semantics: the CLI prints reply verbatim, no banner, no newline.
    return DispatchAskResult(
        kind="followup",
        short_id=session_id,
        reply=result.last_msg,
        duration_ms=result.duration_ms,
    )


def _codex_followup_path(
    *,
    name: str,
    message: str,
    from_name: str,
    existing: AgentEntry,
    yolo: bool,
    timeout_sec: float,
    lock_handle,
) -> DispatchAskResult:
    """Resume an existing codex session via `codex exec resume <id>`.

    Invariants:
      - cwd is taken from the registry's recorded ``existing.cwd`` (parent
        design domain pitfall: codex sessions are cwd-pinned). The
        call-time cwd is ignored.
      - codex_session_id is preserved (never re-minted, never overwritten).
      - last_message_at is bumped only on success.
    """
    from fno.agents.harnesses import codex as codex_mod

    session_id = existing.harness_session_id
    if not session_id:
        raise DispatchAskError(
            f"registry entry {name!r} has no harness_session_id; cannot follow up. "
            f"Remove with 'fno agents rm {name}' and recreate.",
            exit_code=11,
        )

    _emit_ev(
        "agent_followup_started",
        name=name,
        provider="codex",
        codex_session_id=session_id,
        yolo=yolo,
    )

    # AgentEntry.log_path and .cwd are non-Optional strings; falsy values
    # are a registry-corruption signal, not a recoverable case. Raise
    # rather than substitute a default path that would silently land
    # codex's tee in /tmp (or the conventional path for a DIFFERENT
    # agent name) and confuse downstream `fno agents logs <name>`.
    if not existing.log_path:
        raise DispatchAskError(
            f"registry entry {name!r} has empty log_path; run 'fno agents rm {name}' and recreate.",
            exit_code=11,
        )
    if not existing.cwd:
        raise DispatchAskError(
            f"registry entry {name!r} has empty cwd; "
            f"codex sessions are cwd-pinned and resume cannot proceed. "
            f"Run 'fno agents rm {name}' and recreate.",
            exit_code=11,
        )
    output_path = Path(existing.log_path)
    registered_cwd = Path(existing.cwd)

    try:
        result = codex_mod.resume(
            session_id=session_id,
            cwd=registered_cwd,
            prompt=message,
            from_name=from_name,
            yolo=yolo,
            output_path=output_path,
            timeout=timeout_sec,
        )
    except codex_mod.CodexTimeoutError as exc:
        events.emit(
            "agent_followup_failed",
            stage="codex-timeout",
            name=name,
            codex_session_id=session_id,
            timeout_sec=exc.timeout_sec,
        )
        raise DispatchAskError(
            f"codex follow-up timed out after {exc.timeout_sec}s",
            exit_code=15,
        ) from exc
    except codex_mod.CodexInvocationError as exc:
        events.emit(
            "agent_followup_failed",
            stage="codex-subprocess",
            name=name,
            codex_session_id=session_id,
            returncode=exc.exit_code,
        )
        # Propagate codex's exit code (or structured provider code like
        # 12 for tee-open EACCES) instead of collapsing to 1. Gemini
        # PR #305 round 3 flagged the prior collapse as losing context.
        raise DispatchAskError(
            f"codex resume exited {exc.exit_code} (see {output_path} for details). "
            f"If the session was lost, run 'fno agents rm {name}' then re-ask.",
            exit_code=exc.exit_code if exc.exit_code != 0 else 1,
        ) from exc

    # AC2-HP: bump last_message_at only on success.
    # Pass ``_utc_now_iso`` (callable, no parens) so the timestamp is
    # generated under the registry-wide flock — monotonic per atomic
    # write under concurrent followup.
    try:
        update_registry(
            _stamp_status(name, status="live", last_message_at=_utc_now_iso),
        )
    except (OSError, RegistryVersionError) as exc:
        events.emit(
            "agent_followup_failed",
            stage="registry-write",
            name=name,
            codex_session_id=session_id,
            error=str(exc),
            error_type=type(exc).__name__,
        )
        lock_handle.detach()
        raise DispatchAskError(
            f"registry write failed: {exc}. NOTE: message was already delivered; do not retry.",
            exit_code=12,
        ) from exc

    _emit_ev(
        "agent_followup_done",
        stage="followup",
        name=name,
        provider="codex",
        codex_session_id=session_id,
        reply_chars=len(result.last_msg or ""),
        yolo=yolo,
    )
    return DispatchAskResult(
        kind="followup",
        short_id=session_id,
        reply=result.last_msg or "",
        duration_ms=result.duration_ms,
    )


def _capture_parent_edge() -> tuple[Optional[str], Optional[str], Optional[str]]:
    """Capture the spawning session's ambient identity from environment variables.

    Returns ``(session_id, harness, cwd)`` — all three are strings or None.
    Precedence applies within one harness family; markers from two families
    attribute NOTHING (a foreign inherited marker must not be laundered into
    the parent record, x-b57a). Never raises; always returns a triple
    (missing fields degrade to None).

    Harness detection order (Task 2.2, x-30f6):
      CODEX_THREAD_ID        -> harness="codex"
      CLAUDE_CODE_SESSION_ID -> harness="claude"
      CODEX_SESSION_ID       -> harness="codex"
      GEMINI_SESSION_ID      -> harness="gemini"
      OPENCODE_SESSION_ID    -> harness="opencode"
    """
    # OWNED, not precedence (x-20f1): this triple is stamped onto the SPAWNED
    # row as its `spawned_by_*` edge, so an inherited marker records a stranger
    # as the parent for the life of that row. An ambiguous resolve records no
    # lineage rather than a wrong one.
    from fno.claims.self_identity import resolve_self_identity

    identity = resolve_self_identity()

    # $PWD may be unset (non-interactive shells, cron, daemonized procs); fall
    # back to os.getcwd(), which for a `fno agents spawn` subprocess is the
    # spawning session's cwd (inherited), so the parent cwd is always captured.
    parent_cwd: Optional[str] = (os.environ.get("PWD") or os.getcwd()).strip() or None

    return identity.session_id, identity.harness, parent_cwd


def _capture_spawn_trigger() -> Optional[str]:
    """The CAUSE of this spawn (x-42c5), distinct from :func:`_capture_parent_edge`.

    An automated dispatcher (today: think-spawn) sets ``FNO_SPAWN_TRIGGER`` in
    the subprocess env before shelling out to ``fno agents spawn``; a human
    running the command directly never sets it, so absence reads as "an
    operator asked for this." Never raises.

    Pops the var after reading it (one-shot, not just get): the create paths
    downstream (e.g. claude.py's ``bg_create``) build the *new* worker's own
    process env from a ``dict(os.environ)`` snapshot of this process, and
    ``scrub_ambient_identity`` does not know about this marker. Left in place,
    it would ride into the spawned session's environment and mislabel that
    session's own later spawns with this spawn's cause.
    """
    return (os.environ.pop("FNO_SPAWN_TRIGGER", "") or "").strip() or None


# Bounded lookup of the spawned supervisor's pid. Short by design: the sidecar is
# normally written by receipt time, so this only covers a race, and a wake must
# not stall behind it.
_PIN_LOOKUP_ATTEMPTS = 4
_PIN_LOOKUP_BACKOFF_S = 0.05

# `claude --bg` never ran, so no supervisor can exist and the claim is safe to
# free. Every other failure is ambiguous - a timeout (124) or an unparseable
# receipt may leave a half-created supervisor, which bg_create's own timeout
# comment calls out as the caller's to reconcile.
_NO_CHILD_POSSIBLE_EXIT = 127

# How long to guard a transcript whose possible writer we could not identify (a
# timeout or unparseable receipt leaves no short_id to resolve a pid from). Long
# enough to cover the orphan's startup, short enough that a false positive costs
# one delayed wake rather than a wedged session.
_UNKNOWN_ORPHAN_TTL_MS = 5 * 60 * 1000


def _claude_create_path(
    *,
    name: str,
    message: str,
    cwd: Path,
    chosen: str,
    timeout: Optional[int],
    yolo: bool,
    lock_handle,  # type: ignore[no-untyped-def]
    role: Optional[str] = None,
    route_env: Optional[Mapping[str, str]] = None,
    model: Optional[str] = None,
    permission_mode: Optional[str] = None,
    effort: Optional[str] = None,
    resume_session_id: Optional[str] = None,
    revive: bool = False,
    add_dir: Optional[str] = None,
    agent: Optional[str] = None,
    tools: Optional[str] = None,
    deny_tools: Optional[str] = None,
    account_env: Optional[Mapping[str, str]] = None,
    crown_level: Optional[int] = None,
    crown_scope: Optional[str] = None,
    route_provider: Optional[str] = None,
) -> DispatchAskResult:
    """Spawn a new claude agent under the per-agent flock.

    Extracted from the inline create block in :func:`dispatch_ask` so
    Task 1.2 (the new ``spawn`` verb) can call the same machinery without
    going through ``dispatch_ask``.  Runs INSIDE the per-agent flock.

    The CALLER emits ``agent_ask_started`` (dispatch_spawn does, under its
    dispatch context); this helper emits exactly one of ``agent_ask_done`` or
    ``agent_ask_failed``.  On registry-write failure, detaches the lock
    (AC1-FR) and surfaces the orphaned short_id in the error message.

    x-dfa4: ``--yolo`` maps to bypassPermissions for claude (was a no-op); an
    explicit ``permission_mode`` wins (the two are mutually exclusive upstream).
    """
    # x-dfa4: fold --yolo -> bypassPermissions; an explicit mode wins. Both unset
    # leaves the argv byte-identical to today (matches the Rust bg path).
    effective_mode = permission_mode or ("bypassPermissions" if yolo else None)

    from fno.agents.harnesses import claude as claude_mod
    from fno.harness_identity import claude_transport_short_id

    # x-9844 Lane 2 / x-7fef: every resume takes the session single-writer claim
    # here, so a concurrent resume of the same uuid (the residual window the
    # per-agent flock's name-scoped serialization leaves open, plus the
    # cross-name case) can't spawn a second supervisor onto one transcript.
    #
    # x-7fef: the claim is PINNED to the spawned supervisor's pid below and then
    # deliberately outlives this process. Releasing after the spawn (the old
    # lifetime) left the transcript guarded only by `session_is_live(uuid[:8])`,
    # which a revived supervisor defeats by registering under a NEW short id. A
    # supervisor-pinned claim needs no such lookup: a dead supervisor makes it
    # dead-pid and the next acquire reclaims it via stale recovery, while a live
    # one truthfully refuses a second writer. That also answers the old
    # "holding past spawn hoards the writer and blocks native attach" objection.
    writer_claim_holder: Optional[str] = None
    if resume_session_id:
        writer_claim_holder = f"revive:{os.getpid()}"
        try:
            claude_mod.acquire_session_writer_claim(
                session_uuid=resume_session_id,
                holder=writer_claim_holder,
                # Guard 1: refuse a transcript whose original bg supervisor is
                # still reachable. Carried in from wake_and_deliver's acquire so
                # moving the claim inward does not drop the probe.
                claude_short_id=claude_transport_short_id(resume_session_id),
            )
        except claude_mod.SessionWriterClaimError as exc:
            raise DispatchAskError(
                f"session {resume_session_id} is held by another writer; refusing "
                f"to open a second writer on one transcript ({exc})",
                exit_code=11,
            ) from exc
        except Exception as exc:
            # Fail closed on a claim-substrate fault (a corrupt claim file raises
            # ClaimCorrupted, which is neither SessionWriterClaimError nor the
            # OSError/RuntimeError wake_and_deliver catches). Left to propagate it
            # would abort `fno agents mail send` before the durable fallback queues the
            # message; exit 11 demotes to "writer possibly live" instead, which is
            # the safe answer when we cannot establish single-writer safety.
            raise DispatchAskError(
                f"cannot establish single-writer safety for session "
                f"{resume_session_id}: {exc}",
                exit_code=11,
            ) from exc

    def _release_writer_claim() -> None:
        """Free the claim. ONLY correct when no child can exist - once a
        supervisor is running it IS the writer, and the claim must say so."""
        if writer_claim_holder is None or not resume_session_id:
            return
        try:
            claude_mod.release_session_writer_claim(
                session_uuid=resume_session_id, holder=writer_claim_holder
            )
        except Exception:
            pass

    def _anchor_claim_for_unknown_orphan() -> None:
        """Keep guarding a possible supervisor whose pid we never learned.

        The TTL is what makes this guard real. The claim is currently pinned to
        this short-lived process, so it would read STALE the instant we exit and
        the next wake would reclaim it while the orphan is still writing. With a
        TTL, a dead pid reads SUSPECT instead, which acquire refuses until the
        window lapses. The project's own preference settles the trade-off: a
        missed wake demotes to durable, a second writer corrupts a transcript.
        """
        if writer_claim_holder is None or not resume_session_id:
            return
        try:
            claude_mod.acquire_session_writer_claim(
                session_uuid=resume_session_id,
                holder=writer_claim_holder,
                ttl_ms=_UNKNOWN_ORPHAN_TTL_MS,
            )
        except Exception:
            pass

    def _pin_claim_to_supervisor(spawned_short_id: str) -> bool:
        """Re-pin the claim from this process onto the spawned supervisor.

        A same-holder re-acquire rewrites the claim file with the given pid
        (claims/core.py resolution step 1), so no new primitive is needed. The
        sidecar `~/.claude/sessions/<pid>.json` is normally written by receipt
        time; the short retry covers the race where it is not yet visible, since
        every miss costs an unguarded orphan window.
        """
        if writer_claim_holder is None or not resume_session_id:
            return False
        for attempt in range(_PIN_LOOKUP_ATTEMPTS):
            try:
                locator = claude_mod.locate_session(spawned_short_id)
            except Exception:
                locator = None
            if locator is not None:
                try:
                    claude_mod.acquire_session_writer_claim(
                        session_uuid=resume_session_id,
                        holder=writer_claim_holder,
                        pid=locator.pid,
                    )
                    return True
                except Exception:
                    return False
            if attempt + 1 < _PIN_LOOKUP_ATTEMPTS:
                time.sleep(_PIN_LOOKUP_BACKOFF_S)
        return False

    # x-ae2d: materialize the route file BEFORE the supervisor exists. It does
    # mkdir + open + replace under the state dir, and doing it at row-write time
    # would put that I/O after the launch, where an OSError escapes uncaught and
    # strands a live supervisor with no registry row. Content-addressed, so this
    # is the same path bg_create resolves for itself moments later - including
    # the account overlay, else a composed spawn's row names a different file
    # than the worker launched with and a restore silently drops the account's
    # pinned env. Route-bearing rows only: an account-only file restores as "no
    # route" (or an incomplete unit the composition guard refuses), so stamping
    # it broke every --account worker's revive.
    from fno.agents.model_routing import route_settings_path_for

    route_settings_path = (
        route_settings_path_for(route_env, account_env) if route_env else None
    )

    # x-42c5, review fix: pop FNO_SPAWN_TRIGGER BEFORE bg_create, not after.
    # bg_create snapshots dict(os.environ) to build the NEW worker's own
    # persistent env; popping post-call is too late, the snapshot already
    # happened and the var would ride into that worker's environment (see
    # _capture_spawn_trigger's own docstring for what that mislabels).
    spawn_trigger = _capture_spawn_trigger()

    try:
        result: ProviderResult = claude_mod.bg_create(
            name=name,
            message=message,
            cwd=cwd,
            timeout=timeout,
            role=role,
            route_env=route_env,
            model=model,
            permission_mode=effective_mode,
            effort=effort,
            resume_session_id=resume_session_id,
            add_dir=add_dir,
            agent=agent,
            tools=tools,
            deny_tools=deny_tools,
            account_env=account_env,
        )
    except claude_mod.ProviderSubprocessError as exc:
        # Only a never-executed claude proves no supervisor exists. A timeout or
        # a non-zero exit may have left one running, and there is no short_id to
        # resolve its pid from - so the claim gets a TTL, which is the only thing
        # that keeps guarding it once this process exits.
        if exc.exit_code == _NO_CHILD_POSSIBLE_EXIT:
            _release_writer_claim()
        else:
            _anchor_claim_for_unknown_orphan()
        events.emit(
            "agent_ask_failed",
            stage="subprocess",
            name=name,
            provider=chosen,
            returncode=exc.exit_code,
        )
        raise DispatchAskError(exc.stderr, exit_code=1) from exc
    except claude_mod.ProviderParseError as exc:
        # claude exited 0 and only its receipt was unreadable, so a supervisor is
        # very likely running - and without a parsed short_id we cannot find its
        # pid. Same TTL anchor as the timeout case.
        _anchor_claim_for_unknown_orphan()
        events.emit(
            "agent_ask_failed",
            stage="parse",
            name=name,
            provider=chosen,
            short_id_raw=exc.stdout_head,
        )
        raise DispatchAskError(
            f"unable to parse short-id from claude --bg output: {exc.stdout_head}",
            exit_code=1,
        ) from exc

    short_id = result.session_id_out
    assert short_id is not None  # parse_short_id raises otherwise

    # x-7fef: re-pin the claim off this transient process and onto the spawned
    # supervisor, so the claim lives and dies with the writer it guards.
    pinned_to_supervisor = _pin_claim_to_supervisor(short_id)

    # Best-effort full session-UUID capture (ab-f1b0ccd1, AC1-HP): persist the
    # stream-json `--resume` target alongside the 8-hex short-id so the worker
    # is adoptable by the live stream-json switchboard lane. Runs after the receipt is
    # captured; a miss leaves the field None and never gates the launch.
    # x-9844 Fix 3: a revival preserves the resumed uuid (the identity being
    # continued) rather than re-resolving from the fresh short_id, so the
    # invariant "same conversation, new short_id" holds even if resolution slips.
    session_uuid = (
        resume_session_id if revive else claude_mod.resolve_session_uuid_at_spawn(short_id)
    )

    # A revival continues one conversation under a NEW handle, which is the
    # invariant directly above -- but it was never printed, so an operator
    # watching the old handle heard nothing while the new one did the work.
    # The sibling mail-revive fork already prints its lineage under "a fork is
    # never silent"; this is the same event on a different door. Same rule as
    # the rest of this node: never hand back a value without naming what it
    # continues.
    if revive and resume_session_id:
        print(
            f"fno agents spawn: resumed {canonical_handle(resume_session_id)} as "
            f"{short_id} (same conversation, new handle).\n"
            f"Watch the new one: fno agents peek {short_id}",
            file=sys.stderr,
        )

    # Capture the spawning session's ambient identity (Task 2.2, x-30f6).
    # Best-effort: never raises, degrades to (None, None, None) when absent.
    # spawn_trigger was already popped before bg_create above (x-42c5 ordering fix).
    spawned_by_session, spawned_by_harness, spawned_by_cwd = _capture_parent_edge()

    # Crown stamp (US9), same contract as the pane path: the grantor is the
    # spawning session captured just above, or "human" for a direct human spawn
    # with no session env. Never a caller-supplied value - a row that could name
    # its own grantor proves nothing.
    crown_grantor_val = (spawned_by_session or "human") if crown_level is not None else None

    # Registry write. Create the file the row records (x-7bcd AC4): a
    # log_path pointing at nothing is a claim, not evidence, and the
    # resolvable-handle guard only checks the field is non-empty, not that
    # the file exists.
    touched_log_path = _touch_log_path(name)
    from fno.agents.spawn_defaults import resolve_lane_vendor

    lane_provider = route_provider or resolve_lane_vendor(
        ["claude", *(["--model", model] if model else [])], harness="claude"
    )
    new_entry = AgentEntry(
        name=name,
        cwd=str(cwd),
        log_path=str(touched_log_path) if touched_log_path is not None else "",
        short_id=short_id,
        # Canonical identity at birth (x-ec59): a bg claude row is born routable
        # by name. A raced uuid-resolution miss leaves harness_session_id None;
        # reconcile / send-time heal backfills it.
        harness="claude",
        provider=lane_provider,
        model=model,
        effort=effort,
        harness_session_id=session_uuid,
        spawned_by_session=spawned_by_session,
        spawned_by_harness=spawned_by_harness,
        spawned_by_cwd=spawned_by_cwd,
        spawn_trigger=spawn_trigger,
        # The SAME stamp the pane path writes. Two Python paths mint a worker
        # row - pane and bg - and stamping only one would leave the reap lane
        # reading absent on every bg worker, which is a producer on one of N
        # paths: the field reads unpopulated rather than merely unreliable.
        origin="spawn",
        # x-ae2d: the route this launch got, so a relaunch can come back on it.
        # ROUTE only, never an account overlay: the account settings file omits
        # CLAUDE_CONFIG_DIR by construction (it cannot live in a file read FROM
        # that config), so recording it would promise a restore that silently
        # leaves the account behind. Resolved before the launch (above) so its
        # I/O cannot strand a live supervisor.
        route_settings_path=route_settings_path,
        crown_level=crown_level,
        crown_scope=crown_scope,
        crown_grantor=crown_grantor_val,
    )

    crown_declined = False
    crown_succeeded = False

    # x-9844 Fix 3: a revival REPLACES the existing exited same-name row in place
    # (never appends a duplicate name). The load-modify-write is atomic under
    # update_registry's own lock, so a concurrent reader sees the old exited row
    # or the new live row, never a torn/absent state.
    def _write(entries: list) -> list:
        nonlocal crown_declined, crown_succeeded
        entry = new_entry
        # One-live-crown guard (x-7685), inside the write lock so the check and
        # the stamp are atomic against a racing spawn. If a non-terminal row
        # already reigns over this scope, decline the crown and spawn UNCROWNED
        # rather than refuse the spawn: an uncrowned worker can still be given the
        # crown later, two live crowns over one scope cannot be undone.
        #
        # UNLESS the sitting king is the caller, which is SUCCESSION. An
        # abdicating king cannot hand off after it exits (a dead session spawns
        # nothing), so the handoff has to happen while it still reigns. The crown
        # moves in this one write: the caller's row is vacated as the heir is
        # stamped, so no reader sees two live crowns over the scope, and none sees
        # zero.
        #
        # A revive is checked the same way, with the row being replaced excluded:
        # a king reviving its own exited session must not be blocked by the
        # corpse it is about to overwrite.
        if crown_level is not None and crown_scope:
            contenders = [e for e in entries if not (revive and e.name == name)]
            holders = [
                e
                for e in contenders
                if e.crown_scope == crown_scope and e.status not in TERMINAL_STATUSES
            ]

            if holders and all(is_caller_row(h, spawned_by_session, spawned_by_harness) for h in holders):
                entries = [
                    replace(e, crown_level=None, crown_scope=None, crown_grantor=None)
                    if e.crown_scope == crown_scope and is_caller_row(e, spawned_by_session, spawned_by_harness)
                    else e
                    for e in entries
                ]
                crown_succeeded = True
            elif holders:
                entry = replace(
                    new_entry, crown_level=None, crown_scope=None, crown_grantor=None
                )
                crown_declined = True
        if revive:
            return [entry if e.name == name else e for e in entries]
        return entries + [entry]

    try:
        update_registry(_write)
        if crown_declined:
            print(
                f"spawn: crown declined (scope {crown_scope!r} already held by a "
                "live row); spawned uncrowned. The worker launched without a crown.",
                file=sys.stderr,
            )
        elif crown_succeeded:
            print(
                f"spawn: crown over {crown_scope!r} transferred from this session "
                f"to {name} (succession). You no longer hold it.",
                file=sys.stderr,
            )
    except (AgentResolutionError, OSError, ValueError, RegistryVersionError) as exc:
        # Birth's failure counterpart (x-8cd5 Wave 6): the supervisor launched
        # but no registry row names it, so without this the orphan's later
        # death would join no birth in the daemon log.
        events.emit_spawn_failed(
            name=name, provider=chosen, short_id=short_id, reason=f"registry-write: {exc}"
        )
        events.emit(
            "agent_ask_failed",
            stage="registry-write",
            name=name,
            provider=chosen,
            short_id=short_id,
        )
        # Hold the lock so the next caller sees "manual
        # cleanup needed" — AC1-FR registry-write semantics.
        # The same treatment applies if update_registry's
        # internal load_registry hits a RegistryVersionError
        # mid-cycle: the subprocess already created the
        # supervisor, so the orphan signal stays valid.
        lock_handle.detach()
        # x-7fef: do NOT release the writer claim here. The orphaned supervisor
        # is the transcript's writer even though no registry row names it, so a
        # supervisor-pinned claim is the only thing that stops a later wake from
        # opening a second one.
        #
        # This is also the last chance to pin it. If the first lookup lost the
        # sidecar race, retry now - the registry attempt bought time, and an
        # orphan guarded by a claim pinned to THIS exiting process is no guard at
        # all. Still unpinned, we keep the claim anyway rather than release: a
        # dead-pid claim is reclaimed by stale recovery, whereas releasing hands
        # the transcript to the next wake while the orphan is still writing.
        if not pinned_to_supervisor:
            pinned_to_supervisor = _pin_claim_to_supervisor(short_id)
            if not pinned_to_supervisor:
                _anchor_claim_for_unknown_orphan()
        held_note = (
            f" session writer claim session:{resume_session_id} is held for the "
            f"orphan (pid-pinned); later wakes of this session will refuse until "
            f"that supervisor exits."
            if pinned_to_supervisor
            else (
                f" session writer claim session:{resume_session_id} is held on a "
                f"{_UNKNOWN_ORPHAN_TTL_MS // 60000}m TTL (supervisor pid "
                f"unresolved); later wakes refuse until it lapses."
            )
        )
        raise DispatchAskError(
            f"registry write failed: {exc}. "
            f"orphaned supervisor session: claude rm {short_id} "
            f"(registry not updated).{held_note}",
            exit_code=12,
        ) from exc

    # x-7fef degrade: the supervisor pid was never resolved (sidecar race), so
    # the claim is still pinned to this exiting process and would go dead-pid on
    # exit anyway. Fall back to the pre-x-7fef lifetime - release and warn -
    # rather than leave a claim whose pid lies about who is writing.
    if writer_claim_holder is not None and resume_session_id and not pinned_to_supervisor:
        _release_writer_claim()
        print(
            f"warning: could not resolve supervisor pid for {short_id}; released "
            f"session:{resume_session_id} writer claim (transcript guarded only by "
            f"supervisor liveness)",
            file=sys.stderr,
        )

    # Spawn birth (x-30f6, x-8cd5 Wave 6): exactly one per successful create,
    # written to the daemon lifecycle log so it joins the death events the
    # daemon emits there (agent_orphan_reaped / agent_row_reaped / etc.).
    events.emit_spawned(
        name=name,
        short_id=short_id,
        provider=chosen,
        spawned_by_session=spawned_by_session,
        spawned_by_harness=spawned_by_harness,
        spawned_by_cwd=spawned_by_cwd,
    )

    # Done event.
    _emit_ev(
        "agent_ask_done",
        stage="dispatch",
        name=name,
        provider=chosen,
        short_id=short_id,
        duration_ms=result.duration_ms,
        yolo=yolo,
    )
    return DispatchAskResult(
        kind="create",
        short_id=short_id,
        duration_ms=result.duration_ms,
    )


def dispatch_ask(
    name: str,
    message: str,
    provider: Optional[str],
    cwd: Path,
    timeout: Optional[int] = None,
    lock_timeout: float = _DEFAULT_LOCK_TIMEOUT,
    from_name: str = _FROM_NAME_DEFAULT,
    yolo: bool = False,
) -> DispatchAskResult:
    """Dispatch an ``ask`` to an already-registered agent (follow-up only).

    ``ask`` never creates agents. Unknown names raise
    :data:`UNKNOWN_AGENT_EXIT_CODE` (16) pointing the caller at
    ``fno agents spawn``. Use ``spawn`` / ``host`` for initial creation.

    Orchestration:

    1. Validate name / message / from_name.
    2. Acquire per-agent flock (``hold_agent_lock``) with timeout.
    3. INSIDE the flock: load the registry; reject unknown names with
       exit 16 BEFORE calling ``select_provider`` (so unknown+no-provider
       gets exit 16, not exit 2). For existing names run ``select_provider``
       to catch provider-mismatch (still exit 2).
    4. Route existing names to the follow-up path: emit
       ``agent_followup_started``, invoke ``ask_followup``, bump
       ``last_message_at`` + ``status="live"`` via ``update_registry``,
       emit ``agent_followup_done``, return result with reply text.

    Returns:
        :class:`DispatchAskResult` with ``kind == "followup"`` only.
        (``kind == "create"`` is returned by ``_claude_create_path`` /
        ``_codex_create_path`` when called
        from the ``spawn`` verb; ``dispatch_ask`` itself never returns
        ``kind == "create"``.)

    Raises:
        DispatchAskError: every documented failure mode, with the exit
            code the caller should propagate.
    """
    # 1. Input validation.
    _validate_inputs(name=name, message=message, from_name=from_name)

    registry_path = paths.agents_registry_path()

    def _on_wait() -> None:
        print(
            f"Waiting for agent {name!r} lock...",
            file=sys.stderr,
            flush=True,
        )

    # 2. Per-agent flock + 3-onward inside the lock.
    try:
        with hold_agent_lock(
            name,
            registry_path,
            timeout=lock_timeout,
            on_wait=_on_wait,
        ) as lock_handle:
            # 3a. Read the registry under the lock so existing-name
            # detection and provider-selection see a consistent snapshot.
            # RegistryVersionError is a RuntimeError (not ValueError), so
            # it MUST be enumerated explicitly here - the schema-version
            # guard's whole point is to fail loud rather than silently
            # misread an alien shape.
            try:
                entries = load_registry()
            except (OSError, ValueError, RegistryVersionError) as exc:
                events.emit(
                    "agent_ask_failed",
                    stage="registry-read",
                    name=name,
                )
                raise DispatchAskError(
                    f"registry read failed: {exc}",
                    exit_code=12,
                ) from exc

            existing = next(
                (e for e in entries if e.name == name),
                None,
            )

            # 3b. Unknown-agent guard: ask never creates; spawn/host first.
            # This check precedes select_provider so that an unknown name
            # with no --provider gets exit 16 (unknown-agent), NOT exit 2
            # (provider-required). The spec mandates this ordering.
            if existing is None:
                events.emit(
                    "agent_ask_failed",
                    stage="unknown-name",
                    name=name,
                )
                raise DispatchAskError(
                    f"unknown agent {name!r}; spawn it first: "
                    f"fno agents spawn {name} --harness <harness>",
                    exit_code=UNKNOWN_AGENT_EXIT_CODE,
                )

            # 3c. Provider mismatch check for EXISTING agents. select_provider
            # raises ProviderMismatchError when a follow-up specifies the wrong
            # provider. It also validates the requested provider is in
            # KNOWN_PROVIDERS (ValueError on unknown provider name).
            # select_provider also calls load_registry internally; guard the
            # same OSError / RegistryVersionError class.
            try:
                chosen = select_provider(name=name, requested_provider=provider)
            except ProviderMismatchError as exc:
                raise DispatchAskError(str(exc), exit_code=2) from exc
            except ValueError as exc:
                raise DispatchAskError(str(exc), exit_code=2) from exc
            except (OSError, RegistryVersionError) as exc:
                events.emit(
                    "agent_ask_failed",
                    stage="registry-read",
                    name=name,
                )
                raise DispatchAskError(
                    f"registry read failed: {exc}",
                    exit_code=12,
                ) from exc

            # 3d. Build the dispatch context (EventContext) now that we
            # know the chosen provider, so the followup branch has one
            # request_id + caller_kind + from_name across started/done
            # event pairs (AC4-HP).
            #
            # Stashed on the module ContextVar so the followup helpers'
            # emits pick it up via _emit_ev without threading a new kwarg
            # through their long signatures. The try/finally resets the
            # token even when DispatchAskError unwinds the stack so ctx
            # cannot leak to a sibling dispatch on the same thread.
            ctx_for_dispatch = build_context(
                to_name=name,
                to_provider=chosen,
                transport="direct-cli",
                from_name_override=from_name,
            )
            ctx_token = _DISPATCH_CTX.set(ctx_for_dispatch)

            try:
                # 3e. Follow-up path — existing is always not-None here
                # (unknown-agent guard above exits early). Route to follow-up
                # under the same flock so two parallel asks for the same name
                # serialize end to end (AC2-EDGE concurrent ask same-name).
                if existing is not None:
                    # Mux-hosted agents (any provider) ride PaneSend, not the
                    # provider socket/MCP/worker follow-up lanes below (which
                    # key on a provider short_id a mux row lacks). Mirror
                    # _deliver_live's mux short-circuit before provider routing.
                    if existing.mux:
                        return _mux_followup_path(
                            name=name,
                            message=message,
                            from_name=from_name,
                            existing=existing,
                            lock_handle=lock_handle,
                        )
                    if yolo and existing.harness == "claude":
                        # AC3-ERR: --yolo is a no-op for the claude path
                        # (claude's --bg has no equivalent flag). Emit a
                        # single-line stderr note and continue normally.
                        print(
                            "--yolo has no effect for provider 'claude'",
                            file=sys.stderr,
                        )
                    if existing.harness == "claude":
                        return _followup_path(
                            name=name,
                            message=message,
                            cwd=cwd,
                            from_name=from_name,
                            existing=existing,
                            timeout_sec=(
                                float(timeout)
                                if timeout is not None
                                else _DEFAULT_FOLLOWUP_TIMEOUT_SEC
                            ),
                            lock_handle=lock_handle,
                        )
                    if existing.harness == "codex":
                        return _codex_followup_path(
                            name=name,
                            message=message,
                            from_name=from_name,
                            existing=existing,
                            yolo=yolo,
                            timeout_sec=(
                                float(timeout)
                                if timeout is not None
                                else _DEFAULT_FOLLOWUP_TIMEOUT_SEC
                            ),
                            lock_handle=lock_handle,
                        )
                    if existing.harness == "gemini":
                        raise DispatchAskError(
                            "provider 'gemini' is retired; route this work to agy "
                            "or a maintained claude/codex provider",
                            exit_code=2,
                        )
                    raise DispatchAskError(
                        f"follow-up for provider {existing.harness!r} is not implemented",
                        exit_code=2,
                    )
            finally:
                _DISPATCH_CTX.reset(ctx_token)

    except AgentLockTimeout as exc:
        events.emit(
            "agent_ask_failed",
            stage="lock-timeout",
            name=name,
        )
        raise DispatchAskError(
            f"lock timeout for agent {name!r} after {exc.timeout}s"
            f"{exc.holder_note()}",
            exit_code=11,
        ) from exc


# ---------------------------------------------------------------------------
# Task 1.2: spawn verb (US2 Python fallback runtime)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class SpawnResult:
    """Return shape for :func:`dispatch_spawn`.

    ``kind`` discriminates the two outcomes:

    - ``"created"`` -- persistent peer (claude plain spawn). ``short_id``
      is the provider's id; ``reply`` is None. The CLI emits the compact
      JSON receipt on stdout.
    - ``"once"`` -- ephemeral one-shot (codex --once). ``reply``
      carries the exchange output; ``short_id`` is the session/short id.
      The CLI prints ``reply`` verbatim on stdout and the teardown receipt
      on stderr.
    """

    kind: Literal["created", "once"]
    name: str
    provider: str
    short_id: str
    reply: Optional[str] = None
    effective_message: Optional[str] = None

    def __post_init__(self) -> None:
        # Convert the prose contract into a runtime trip-wire (sigma-review
        # type-design finding): the cross-field constraint is invisible to
        # the field types alone.
        if self.kind == "once" and self.reply is None:
            raise ValueError("SpawnResult kind='once' requires reply to be set")
        if self.kind == "created" and self.reply is not None:
            raise ValueError("SpawnResult kind='created' must have reply=None")


def validate_spawn_name(name: str) -> None:
    """Spawn-name rules, shared by the daemon/bg/one-shot path
    (:func:`dispatch_spawn`) and the mux-pane back half
    (``fno.agents.mux_spawn``) so the two can never drift (4a-G2 front-half
    reuse). Raises :class:`DispatchAskError` (exit 2) on every violation.
    """
    if not name:
        raise DispatchAskError("agent name must not be empty", exit_code=2)
    if "/" in name or "\\" in name or ".." in name:
        raise DispatchAskError(
            f"agent name must not contain path separators or '..': {name!r}",
            exit_code=2,
        )
    if len(name) > _NAME_MAX_LEN:
        raise DispatchAskError(
            f"name must be <={_NAME_MAX_LEN} chars (got {len(name)})",
            exit_code=2,
        )
    if _SHORT_ID_NAME_SHAPE.match(name):
        raise DispatchAskError(
            f"agent name {name!r} must not match short-id shape "
            f"^[0-9a-f]{{8}}$ (prevents name/id collision)",
            exit_code=2,
        )
    _forbidden_env_chars = ("\x00", "\n", "\r", "=")
    bad = next((ch for ch in _forbidden_env_chars if ch in name), None)
    if bad is not None:
        raise DispatchAskError(
            f"agent name {name!r} contains a forbidden character "
            f"({bad!r} would corrupt subprocess env injection)",
            exit_code=2,
        )


def _is_revival(
    existing: "AgentEntry", provider: str, resume_session_id: Optional[str]
) -> bool:
    """True iff spawning an existing same-name row with ``--resume`` is a revival,
    not a collision (x-9844 Fix 3).

    Gated on: the spawn carries ``--resume``, both the spawn and the row are
    claude, the row's own recorded ``claude_session_uuid`` equals the ``--resume``
    target, and the row's supervisor is NOT live. Liveness is a reality probe
    (``session_is_live``), never the registry ``status`` field, so a row whose
    supervisor is actually alive can never be revived into a second writer on one
    transcript. Every other same-name case (live row, uuid mismatch, no
    ``--resume``) stays fail-closed. The uuid check runs before the (heavier)
    liveness probe so the common mismatch never pays for a socket connect.
    """
    if not resume_session_id or provider != "claude":
        return False
    if getattr(existing, "harness", None) != "claude":
        return False
    if getattr(existing, "harness_session_id", None) != resume_session_id:
        return False
    from fno.agents.harnesses import claude as claude_mod

    short_id = getattr(existing, "short_id", "") or None
    if short_id:
        # A liveness-probe error fails SAFE toward "possibly live": never revive
        # (--resume) into what could be a second writer on one transcript. A
        # spurious collision refusal is retryable; a double writer is not. So a
        # probe crash refuses the revival, it does not wave it through.
        try:
            if claude_mod.session_is_live(short_id):
                return False
        except Exception:
            return False
    return True


def restore_route_for_relaunch(entry: "AgentEntry") -> Optional[Mapping[str, str]]:
    """The route a relaunch of ``entry`` must come back on, or ``None`` (x-ae2d).

    The claude relaunch door: a ``--resume`` revive starts a NEW supervisor, and
    its route comes only from the flags on THAT invocation, so a revive issued
    without ``-P``/``--route`` silently moves the work onto the default Anthropic
    account. Silent is the whole problem - the worker runs fine, bills the wrong
    vendor, and reports nothing.

    A faithful replay, not a re-resolution: the recorded file is read back and
    handed to the spawn as an explicit route, which ``resolve_spawn_route`` passes
    through unchanged. Re-resolving against today's config would relaunch the
    worker onto a route that may differ from the one it ran under, which is a
    behavior change wearing the word "resume".

    Raises:
        DispatchAskError: exit 2, when the row names a route file that is gone
            or unreadable. Refusing beats relaunching unrouted. Exit 2 is the
            code every other route-composition refusal already uses
            (``RouteCompositionError``); notably NOT 15, which the ask/followup
            lane documents as "reply timeout, the message WAS delivered" - the
            opposite claim, since this refusal starts nothing at all.
    """
    path = getattr(entry, "route_settings_path", None)
    if not path:
        return None
    from fno.agents.model_routing import RouteRestoreError, read_route_settings

    try:
        return read_route_settings(path)
    except RouteRestoreError as exc:
        raise DispatchAskError(
            f"agent {entry.name!r} was launched on the route recorded at {path}, "
            f"and it cannot be restored ({exc}). Refusing to relaunch it on the "
            f"default account; re-spawn with an explicit --route/-P to choose one.",
            exit_code=2,
        ) from exc


def _picked_headroom_note(account_id: str) -> str:
    """The picked account's worst window, for the receipt. Never raises."""
    try:
        from fno.adapters.providers.runtime_state import read_usage

        snap = read_usage(account_id)
        if snap is not None and snap.windows:
            worst = max(snap.windows, key=lambda w: w.used_pct)
            return f"{worst.label} {worst.used_pct:.0f}%"
    except Exception:  # noqa: BLE001 - a receipt detail must never break a spawn
        pass
    return "headroom unknown"


def _pick_account_env(
    *,
    role: Optional[str] = None,
    route_env: Optional[Mapping[str, str]] = None,
) -> Optional[Mapping[str, str]]:
    """Consult the picker for a spawn that named no account, or None.

    Advisory in every direction: opt-in via ``providers.quota.pick_on_launch``,
    and any refusal or failure returns None so the spawn proceeds exactly as it
    does today. The receipt is always printed, because a launch silently landing
    on a different account than the operator expects is a billing surprise.

    A ROUTED spawn is never picked for. Picking is a quota decision, and a
    routed worker sends its model traffic to the route's vendor, consuming no
    Anthropic account quota - there is nothing to pick for. An explicit
    ``--account`` still composes with a route (profile from the account,
    endpoint+auth+model from the vendor), but that is operator intent, never a
    picker decision.
    """
    picked = pick_account_id(role=role, route_env=route_env)
    if picked is None:
        return None
    try:
        from fno.agents.account_env import resolve_account_overlay

        return resolve_account_overlay(picked).env
    except Exception as exc:  # noqa: BLE001 - picking is advisory; never block a spawn
        print(f"account: default (pick unavailable: {exc})", file=sys.stderr)
        return None


def pick_account_id(
    *,
    role: Optional[str] = None,
    route_env: Optional[Mapping[str, str]] = None,
) -> Optional[str]:
    """The account id a spawn that named none should launch on, or None.

    THE picking decision, with two consumers: the in-process spawn seams turn it
    into an env overlay, and the runtime seam turns it into an injected
    ``--account`` flag so the Rust client inherits the same choice. One decision,
    so those two can never disagree about which account a worker is billing.

    Advisory in every direction: opt-in via ``providers.quota.pick_on_launch``,
    and any refusal or failure returns None so the spawn proceeds exactly as it
    does today. A routed spawn is never picked for: it bills the route's
    vendor, not an Anthropic account, so there is no quota to manage.
    """
    if route_env or role is not None:
        return None
    try:
        from fno.adapters.providers.cli import PICK_EXIT_NOT_ARMED, pick_account

        # `--if-armed` so the opt-in is read in exactly ONE place, the same call
        # the Rust loop makes. Checking `pick_on_launch` here as well would be a
        # second answer to one question, which is the shape this whole feature
        # exists to remove.
        verdict = pick_account(if_armed=True)
        if verdict.exit_code == PICK_EXIT_NOT_ARMED:
            return None  # off by default: say nothing, change nothing
        if verdict.account is None:
            print(
                f"account: default (pick unavailable: {verdict.reason})",
                file=sys.stderr,
            )
            return None
        print(
            f"account: {verdict.account} (picked, {_picked_headroom_note(verdict.account)})",
            file=sys.stderr,
        )
        return verdict.account
    except Exception as exc:  # noqa: BLE001 - picking is advisory; never block a spawn
        print(f"account: default (pick unavailable: {exc})", file=sys.stderr)
        return None


def note_quota_death(account_env: Optional[Mapping[str, str]], tail: str | None) -> None:
    """Record a cooldown when a worker died with a quota marker in its tail.

    The reactive half of quota survival: a snapshot can be up to
    ``probe_ttl_seconds`` stale, so without this the next pick would hand the
    successor the account that just died. Reuses the existing error taxonomy and
    health writer rather than adding a second classifier. Best-effort - a
    telemetry write must never turn a worker's death into a dispatch failure.
    """
    if not tail:
        return
    try:
        from fno.adapters.providers.error_taxonomy import (
            classify_error,
            reset_epoch_from,
        )
        from fno.adapters.providers.loader import effective_active
        from fno.adapters.providers.runtime_state import (
            record_reset_timezone,
            update_provider_health,
        )

        rule = classify_error(None, tail)
        if rule is None:
            return
        provider_id = _account_id_for_env(account_env) or effective_active()
        if provider_id:
            # The tail that proves the death usually also names when the window
            # reopens. Without it this wrote a seconds-scale backoff over a
            # multi-hour cap, and the next pick handed the successor the account
            # that had just refused.
            update_provider_health(
                provider_id, rule,
                resets_at=reset_epoch_from(
                    tail, record_reset_timezone(provider_id),
                ),
            )
    except Exception:  # noqa: BLE001 - never let a health write break teardown
        pass


def _account_id_for_env(account_env: Optional[Mapping[str, str]]) -> Optional[str]:
    """Which record an overlay pinned, by asking each what its overlay would be.

    Matching on ``config_dir`` alone missed two real shapes: an ``oauth_dir``
    record's overlay names its STAGED dir, not a config_dir, and an api_key
    record's overlay has no directory at all. Both would fall through to
    ``effective_active()`` and record a quota death against the wrong account -
    poisoning an account that is fine while leaving the dead one pickable.
    Comparing the resolved overlay covers every lane, and only runs when a worker
    has already died, so the extra resolutions are not on any hot path.
    """
    if not account_env:
        return None
    try:
        from fno.adapters.providers.loader import load_providers
        from fno.agents.account_env import resolve_account_overlay

        for record in load_providers().records:
            if record.harness != "claude":
                continue
            try:
                if dict(resolve_account_overlay(record.id).env) == dict(account_env):
                    return record.id
            except Exception:  # noqa: BLE001 - an unresolvable record is not a match
                continue
    except Exception:  # noqa: BLE001
        return None
    return None


def dispatch_spawn(
    name: str,
    message: str,
    provider: str,
    cwd: Path,
    once: bool = False,
    timeout: Optional[int] = None,
    lock_timeout: float = _DEFAULT_LOCK_TIMEOUT,
    from_name: str = _FROM_NAME_DEFAULT,
    yolo: bool = False,
    role: Optional[str] = None,
    route_env: Optional[Mapping[str, str]] = None,
    model: Optional[str] = None,
    permission_mode: Optional[str] = None,
    effort: Optional[str] = None,
    add_dir: Optional[str] = None,
    agent: Optional[str] = None,
    tools: Optional[str] = None,
    deny_tools: Optional[str] = None,
    headless: bool = False,
    output_format: Optional[str] = None,
    resume_session_id: Optional[str] = None,
    account_env: Optional[Mapping[str, str]] = None,
    crown_level: Optional[int] = None,
    crown_scope: Optional[str] = None,
    route_provider: Optional[str] = None,
    provider_gate: object | None = None,
) -> SpawnResult:
    """Orchestrate ``fno agents spawn``.

    Routing:

    1. Name validation (same rules as ask).
    2. Provider validation (required for spawn).
    3. Per-agent flock (``hold_agent_lock``) with timeout.
    4. INSIDE the flock:
       a. Collision check: if name already in registry -> exit 2.
       b. Dispatch by (provider, once):
          - claude + once=True           -> exit 2 (refused)
          - claude + once=False          -> ``_claude_create_path``; return compact JSON
          - codex + once=False           -> exit 13 (PTY daemon required)
          - codex + once=True            -> ``_codex_create_path``; teardown after
          - gemini                       -> refused as a retired provider

    Teardown (--once codex):
    - On success: remove the registry row created by the helper.
    - On teardown failure: loud stderr warning, row stays, exit 0 still.
    - On create failure: nonzero exit, no registry row (helpers only write
      registry after subprocess success -- this invariant is pinned by
      test_spawn_once_create_failure_no_registry_entry).

    Returns:
        :class:`SpawnResult`

    Raises:
        :class:`DispatchAskError`: every documented failure mode.
    """
    # 0. Launch-time headroom picking (x-7d45). An explicit --account always
    # wins and is never second-guessed; this only fills the gap when none was
    # given. It runs before the tier-remap check below so that check sees the
    # overlay the worker will actually launch with.
    #
    # This is ONE of the two Python spawn seams: `cmd_spawn` routes the default
    # `pane` substrate to `dispatch_spawn_pane` and never reaches here, so the
    # pane path calls the same helper itself. Two seams, one implementation -
    # putting it in cli.py instead would miss every in-process caller that
    # bypasses argument parsing.
    # A --resume spawn is never picked for, the same seam rule `_pick_account_at_seam`
    # applies to the CLI argv: the transcript being resumed lives under the config
    # dir it was created in, so a picked CLAUDE_CONFIG_DIR points at a directory
    # where that uuid does not exist. It also keeps the revive restore below
    # honest - any --account reaching it is one the operator actually typed.
    if account_env is None and provider == "claude" and not resume_session_id:
        account_env = _pick_account_env(role=role, route_env=route_env)

    launch_role = role
    resolved_providers: list[str] = []
    if provider == "claude" and (role is not None or route_env):
        from fno.agents.model_routing import (
            RouteCompositionError,
            resolve_spawn_route,
        )

        try:
            route_env = resolve_spawn_route(
                role,
                route_env,
                notice=lambda note: print(note, file=sys.stderr),
                resolved_provider=resolved_providers.append,
            )
        except RouteCompositionError as exc:
            raise DispatchAskError(str(exc), exit_code=2) from exc
        if resolved_providers:
            resolved_provider = resolved_providers[-1]
            if route_provider is not None and route_provider != resolved_provider:
                raise DispatchAskError(
                    f"resolved provider {resolved_provider!r} does not match supplied "
                    f"provider {route_provider!r}; refusing before dispatch",
                    exit_code=2,
                )
            route_provider = resolved_provider
        launch_role = None

    if provider == "claude" and route_env and not resolved_providers:
        raise DispatchAskError(
            "pre-resolved route has no bound model-provider identity; refusing "
            "because its provider cap cannot be evaluated; no worker launched",
            exit_code=2,
        )
    if provider == "claude" and route_env and route_provider is None:
        raise DispatchAskError(
            "resolved route has no model-provider axis; refusing to launch because "
            "its provider cap cannot be evaluated; no worker launched",
            exit_code=2,
        )
    spawn_substrate = "headless" if (once or headless) else "bg"
    from fno.agents.spawn_gate import consume_provider_admission

    if route_provider is not None and not (
        provider_gate is not None
        and consume_provider_admission(
            provider_gate, route_provider, name, spawn_substrate
        )
    ):
        raise DispatchAskError(
            f"provider {route_provider!r} has no matching admission token; "
            "refusing before dispatch; no worker launched",
            exit_code=2,
        )

    # 1. Name validation. spawn allows empty message (default "").
    # x: the tier-remap invariant must hold on every reachable spawn path, not
    # just the CLI seam -- an in-process caller passing model="opus" under a
    # foreign ANTHROPIC_DEFAULT_OPUS_MODEL would otherwise still launch a worker
    # that dies on its first turn. Fail closed before anything is created.
    from fno.agents.model_routing import (
        RouteCompositionError,
        check_spawn_tier_remap,
        emit_env_scrub_warning,
    )

    try:
        check_spawn_tier_remap(
            provider,
            model,
            role=launch_role,
            route_env=route_env,
            account_env=account_env,
        )
    except RouteCompositionError as exc:
        raise DispatchAskError(str(exc), exit_code=2) from exc
    # Same seam, same reason as the tier-remap check above: a permission-pinned
    # claude worker launched under CLAUDE_CODE_SUBPROCESS_ENV_SCRUB stalls on
    # approvals, so warn on every reachable path, not just the CLI seam.
    emit_env_scrub_warning(provider, permission_pinned=bool(permission_mode or yolo))

    # Crown eligibility, checked HERE rather than only at the CLI seam: this
    # function is the in-process entry point too, and only the claude bg branch
    # below reaches `_claude_create_path`, the one route that stamps the fields.
    # Every other route builds its AgentEntry elsewhere and would drop the crown
    # while reporting a successful spawn - a silently uncrowned king is the
    # failure this refusal exists to make impossible. Fail closed before anything
    # is created, so a refusal launches nothing and leaves the node dispatchable.
    crown_problem = crown_validation_error(crown_level, crown_scope)
    if crown_problem is not None:
        raise DispatchAskError(crown_problem, exit_code=2)
    if crown_level is not None:
        # Authorization, at the seam every caller reaches: you cannot hand down
        # authority you do not hold. Refuses BEFORE the launch rather than
        # declining after, because an unauthorized grant is an authority error,
        # not a race - nothing should exist as a result of it.
        grant_problem = grant_error(crown_scope or "", calling_agent_row())
        if grant_problem is not None:
            raise DispatchAskError(f"--crown: {grant_problem}", exit_code=2)
        if once or headless:
            raise DispatchAskError(
                "--crown needs a session that outlives the grant; a one-shot "
                "exits after one answer. Use the pane or bg substrate.",
                exit_code=2,
            )
        if provider != "claude":
            raise DispatchAskError(
                f"--crown on the bg substrate is claude-only; got provider "
                f"{provider!r}. Use --substrate pane, which maps every provider.",
                exit_code=2,
            )

    validate_spawn_name(name)
    _validate_from_name(from_name)

    # 2. Provider validation. _check_known_provider raises ValueError, which
    # cmd_spawn does not catch (it only catches DispatchAskError) -- wrap it
    # so an unknown --provider exits 2 cleanly instead of tracebacking.
    try:
        _check_known_provider(provider)
    except ValueError as exc:
        raise DispatchAskError(str(exc), exit_code=2) from exc

    effective_message: Optional[str] = None
    if message.strip().startswith(("/", "$fno:")):
        try:
            message = normalize_command(message, provider)
        except DispatchResolveError as exc:
            raise DispatchAskError(str(exc), exit_code=2) from exc
        effective_message = message

    from fno.agents.spawn_payload import enrich_spawn_payload

    message = enrich_spawn_payload(message)

    if output_format is not None and (
        provider != "claude" or not headless or output_format != "json"
    ):
        raise DispatchAskError(
            "--output-format supports only 'json' on claude headless spawns",
            exit_code=2,
        )

    # 3a. claude + --once -> refused immediately (before acquiring the lock,
    # since there is no state to protect).
    if provider == "claude" and once and not headless:
        raise DispatchAskError(
            "--once is not supported for provider 'claude' "
            "(claude peers are persistent bg threads; use plain spawn)",
            exit_code=2,
        )

    # 3b. codex plain spawn (no --once) in Python fallback -> exit 13.
    if provider == "codex" and not once:
        raise DispatchAskError(
            f"plain spawn for provider {provider!r} requires the fno-agents daemon "
            f"(Rust runtime); use --once for an ephemeral one-shot, or install the "
            f"fno-agents binary",
            exit_code=13,
        )

    registry_path = paths.agents_registry_path()

    def _on_wait() -> None:
        print(
            f"Waiting for agent {name!r} lock...",
            file=sys.stderr,
            flush=True,
        )

    # 3. Per-agent flock.
    try:
        with hold_agent_lock(
            name, registry_path, timeout=lock_timeout, on_wait=_on_wait
        ) as lock_handle:
            # 4a. Collision check INSIDE the flock.
            try:
                entries = load_registry()
            except (OSError, ValueError, RegistryVersionError) as exc:
                raise DispatchAskError(f"registry read failed: {exc}", exit_code=12) from exc
            if resume_session_id and getattr(entries, "complete", True) is not True:
                raise DispatchAskError(
                    "registry forward read is incomplete; refusing resume because a "
                    "recorded provider route may be invisible; no worker launched",
                    exit_code=12,
                )

            # Revive-in-place (x-9844 Fix 3): a --resume spawn whose target uuid
            # matches an EXITED same-name claude row is a revival, not a
            # collision - the row is updated in place below (new short_id, same
            # uuid) instead of refused. Every other same-name case stays
            # fail-closed (live row, uuid mismatch, no --resume).
            existing = next((e for e in entries if e.name == name), None)
            revive = existing is not None and _is_revival(existing, provider, resume_session_id)
            if existing is not None and not revive:
                raise DispatchAskError(
                    f"agent {name!r} already exists; "
                    f"use 'fno agents rm {name}' first or pick another name",
                    exit_code=2,
                )

            # x-ae2d: a revive relaunches the supervisor, so it must come back on
            # the route the row was born with unless this invocation resolved one
            # of its own. Raises (exit 2) when the recorded route is unrestorable.
            #
            # Keyed on the RESOLVED route, never on whether --role was mentioned.
            # `resolve_route` is fail-SAFE: a protected role, a disabled block, an
            # unconfigured provider, or a missing key all return None and leave
            # route_env unset. Skipping the restore because a role was NAMED would
            # therefore relaunch unrouted in exactly the case where the role
            # produced nothing - the silent default-account fallback this exists
            # to prevent. A role that DID resolve leaves route_env truthy, so it
            # still wins over the recorded route.
            # The source row is resolved by the TRANSCRIPT being resumed, not by
            # this spawn's name. A revive reuses the old name, but nothing stops
            # `spawn other-name --resume <uuid>` from relaunching the same
            # transcript under a fresh row - and that row is the one carrying the
            # route. Keying on `existing` alone would leave every renamed relaunch
            # silently unrouted, a guard on one of the two ways in.
            source_row = existing if revive else None
            if resume_session_id and source_row is None:
                source_row = next(
                    (
                        e
                        for e in entries
                        if getattr(e, "harness_session_id", None) == resume_session_id
                        and getattr(e, "route_settings_path", None)
                    ),
                    None,
                )
            if resume_session_id and source_row is not None and not route_env:
                restored_route = restore_route_for_relaunch(source_row)
                if restored_route:
                    restored_provider = getattr(source_row, "provider", None)
                    if not restored_provider:
                        raise DispatchAskError(
                            f"route recorded for {source_row.name!r} has no model-provider "
                            "axis in its registry row; refusing to relaunch because its "
                            "provider cap cannot be evaluated; no worker launched",
                            exit_code=2,
                        )
                    # An explicit --account COMPOSES with the restored route, the
                    # same way it composes with a flag-supplied one (x-5ed4): the
                    # route wins endpoint+auth+model as one unit through the
                    # settings file, and the account's CLAUDE_CONFIG_DIR rides the
                    # spawn env to select the per-account daemon. Nothing here
                    # refuses the pair - `fno agents spawn` does not either, and
                    # the picker that would otherwise inject an advisory
                    # --account already skips a --resume spawn on both seams, so
                    # an account reaching this point is one the operator typed.
                    #
                    # Through resolve_spawn_route, not assigned past it: that is
                    # THE composition decision, where an incomplete route (an
                    # endpoint without its own credential) is refused. A restored
                    # route that skipped it would be the one route in the system
                    # exempt from the check - a guard every other route pays and
                    # this one does not.
                    from fno.agents.model_routing import (
                        bind_route_provider,
                        resolve_spawn_route,
                    )

                    try:
                        route_env = resolve_spawn_route(
                            None,
                            bind_route_provider(restored_route, restored_provider),
                            intent=f"route recorded for {source_row.name!r}",
                            notice=lambda note: print(note, file=sys.stderr),
                        )
                    except RouteCompositionError as exc:
                        raise DispatchAskError(str(exc), exit_code=2) from exc
                    if route_provider is None:
                        raise DispatchAskError(
                            f"route recorded for {source_row.name!r} resolves provider "
                            f"{restored_provider!r}, but provider admission was not "
                            "evaluated before dispatch; refusing; no worker launched",
                            exit_code=2,
                        )
                    if route_provider != restored_provider:
                        raise DispatchAskError(
                            f"route recorded for {source_row.name!r} resolves provider "
                            f"{restored_provider!r}, but admission was evaluated for "
                            f"{route_provider!r}; refusing; no worker launched",
                            exit_code=2,
                        )
                    # Say so. The Rust `resume` door prints its restore, and a
                    # relaunch that silently changes destination is the failure
                    # shape this whole path exists to remove - a receipt that
                    # omits the restore is the same silence pointed the other way.
                    print(
                        f"route: restored from {source_row.route_settings_path} "
                        f"(recorded when {source_row.name!r} launched)",
                        file=sys.stderr,
                    )

            # 4a2. Build the dispatch context so the create helpers' emits
            # (agent_ask_started/agent_ask_done) carry the same request_id /
            # caller / from_name attribution the old dispatch_ask create
            # branch had (codex P2, PR #457). try/finally mirrors
            # dispatch_ask's 3c block so the ctx cannot leak to a sibling
            # dispatch on the same thread.
            ctx_for_dispatch = build_context(
                to_name=name,
                to_provider=provider,
                transport="direct-cli",
                from_name_override=from_name,
            )
            ctx_token = _DISPATCH_CTX.set(ctx_for_dispatch)
            try:
                # Started event (pairs with the helpers' agent_ask_done /
                # agent_ask_failed). Lived in dispatch_ask's routing before
                # Task 1.1 removed the create branch; restored here so the
                # spawn create keeps the started/done pair (codex P2 PR #457).
                _emit_ev(
                    "agent_ask_started",
                    name=name,
                    provider=provider,
                    yolo=yolo,
                )

                # 4b. claude plain spawn.
                if provider == "claude":
                    if headless:
                        from fno.agents.harnesses import claude as claude_mod

                        try:
                            result = claude_mod.headless_create(
                                message=message,
                                cwd=cwd,
                                timeout=timeout,
                                model=model,
                                permission_mode=permission_mode
                                or ("bypassPermissions" if yolo else None),
                                effort=effort,
                                add_dir=add_dir,
                                agent=agent,
                                tools=tools,
                                deny_tools=deny_tools,
                                output_format=output_format,
                                account_env=account_env,
                                route_env=route_env,
                                name=name,
                            )
                        except claude_mod.ProviderSubprocessError as exc:
                            # A quota death here is the freshest signal there is:
                            # write the cooldown so the NEXT pick avoids this
                            # account before its usage snapshot refreshes.
                            note_quota_death(account_env, exc.stderr)
                            _emit_ev(
                                "agent_ask_failed",
                                stage="claude-headless",
                                name=name,
                                provider="claude",
                                returncode=exc.exit_code,
                            )
                            raise DispatchAskError(str(exc), exit_code=exc.exit_code) from exc
                        _emit_ev(
                            "agent_ask_done",
                            stage="dispatch",
                            name=name,
                            provider="claude",
                            duration_ms=result.duration_ms,
                            yolo=yolo,
                        )
                        return SpawnResult(
                            kind="once",
                            name=name,
                            provider="claude",
                            short_id="",
                            reply=result.stdout,
                            effective_message=effective_message,
                        )
                    created = _claude_create_path(
                        name=name,
                        message=message,
                        cwd=cwd,
                        chosen="claude",
                        timeout=timeout,
                        yolo=yolo,
                        lock_handle=lock_handle,
                        role=launch_role,
                        route_env=route_env,
                        model=model,
                        permission_mode=permission_mode,
                        effort=effort,
                        resume_session_id=resume_session_id,
                        revive=revive,
                        add_dir=add_dir,
                        agent=agent,
                        tools=tools,
                        deny_tools=deny_tools,
                        account_env=account_env,
                        crown_level=crown_level,
                        crown_scope=crown_scope,
                        route_provider=route_provider,
                    )
                    return SpawnResult(
                        kind="created",
                        name=name,
                        provider="claude",
                        short_id=created.short_id,
                        effective_message=effective_message,
                    )

                # 4c. codex --once: create + exchange + teardown.
                if provider == "codex":
                    create_result = _codex_create_path(
                        name=name,
                        message=message or "hello",
                        cwd=cwd,
                        from_name=from_name,
                        yolo=yolo,
                        timeout_sec=(
                            float(timeout) if timeout is not None else _DEFAULT_FOLLOWUP_TIMEOUT_SEC
                        ),
                        lock_handle=lock_handle,
                        role=launch_role,
                        model=model,
                        effort=effort,
                        add_dir=add_dir,
                    )
                else:
                    raise DispatchAskError(
                        "provider 'gemini' is retired; route this work to agy "
                        "or a maintained claude/codex provider",
                        exit_code=2,
                    )

                session_or_short_id = create_result.short_id

                # Teardown: remove the registry row the create helper wrote.
                try:
                    update_registry(lambda es: [e for e in es if e.name != name])
                    # Teardown receipt on stderr (AC2-UI).
                    print(
                        f"once: {name} ({provider}/{session_or_short_id}) torn down",
                        file=sys.stderr,
                    )
                except (OSError, RegistryVersionError) as exc:
                    # AC2-FR: loud warning, row stays visible, exit 0 still.
                    print(
                        f"fno agents spawn: warning: teardown failed for {name!r} "
                        f"({provider}/{session_or_short_id}): {exc}. "
                        f"Peer leaked -- clean up via 'fno agents rm {name}'",
                        file=sys.stderr,
                    )

                return SpawnResult(
                    kind="once",
                    name=name,
                    provider=provider,
                    short_id=session_or_short_id,
                    reply=create_result.reply,
                    effective_message=effective_message,
                )
            finally:
                _DISPATCH_CTX.reset(ctx_token)

    except AgentLockTimeout as exc:
        raise DispatchAskError(
            f"lock timeout for agent {name!r} after {exc.timeout}s"
            f"{exc.holder_note()}",
            exit_code=11,
        ) from exc


# ---------------------------------------------------------------------------
# US4-lifecycle: stop / rm / reconcile / attach (write + read verbs)
# ---------------------------------------------------------------------------


_DEFAULT_CLAUDE_SHELLOUT_TIMEOUT = 30.0
_DEFAULT_CLAUDE_LOGS_TAIL_TIMEOUT = 10.0


@dataclass(frozen=True)
class StopResult:
    """Return shape for :func:`stop_agent`.

    ``claude_exit`` is the shellout's exit code on the claude path; ``None``
    for codex / gemini where stop is a synchronous no-op between asks.
    """

    name: str
    provider: str
    claude_exit: Optional[int] = None


@dataclass(frozen=True)
class RmResult:
    """Return shape for :func:`rm_agent`.

    ``registry_changed`` is False when the claude path refuses non-forcefully
    so the entry stayed in the registry. ``force`` reflects the caller's
    flag for forensic visibility downstream.
    """

    name: str
    provider: str
    claude_exit: Optional[int] = None
    force: bool = False
    registry_changed: bool = False


@dataclass(frozen=True)
class ReconcileResult:
    """Return shape for :func:`reconcile_agents`.

    Lists are JSON-friendly: each entry is a dict with ``name``, ``provider``,
    optional ``id`` (short_id or session_id), optional ``reason``. The
    CLI emits this verbatim under ``--json`` (Locked Decision 4 mirror).
    """

    scanned: int
    orphaned: list[dict] = field(default_factory=list)
    recovered: list[dict] = field(default_factory=list)
    skipped: list[dict] = field(default_factory=list)
    errors: list[dict] = field(default_factory=list)
    # Live rows whose null canonical harness_session_id reconcile healed from the
    # harness store (x-ec59). Empty list (not absent) distinguishes "ran, nothing
    # to heal" from "healed": each entry is {name, provider, harness_session_id}.
    backfilled: list[dict] = field(default_factory=list)


@dataclass(frozen=True)
class AttachResult:
    """Return shape for :func:`attach_agent`.

    ``exit_code`` mirrors claude's exit on detach. CLI propagates this.
    """

    name: str
    provider: str
    exit_code: int


def _validate_lifecycle_name(name: str) -> None:
    """Reject empty / path-traversal names for stop/rm/reconcile/attach.

    Mirrors :func:`_validate_inputs` but without the message / from_name
    checks - lifecycle verbs take a name and nothing else.
    """
    if not name:
        raise DispatchAskError("agent name must not be empty", exit_code=2)
    if "/" in name or "\\" in name or ".." in name:
        raise DispatchAskError(
            f"agent name must not contain path separators or '..': {name!r}",
            exit_code=2,
        )
    if len(name) > _NAME_MAX_LEN:
        raise DispatchAskError(
            f"name must be <={_NAME_MAX_LEN} chars (got {len(name)})",
            exit_code=2,
        )


def _resolve_lifecycle_target(
    token: str,
    *,
    registry_path: Optional[Path] = None,
) -> tuple[AgentEntry, Optional[RecipientIdentity]]:
    """Resolve a lifecycle token without discarding address-bound identity.

    A genuine miss falls through to the exact-name lookup so the downstream
    error remains ``agent {name!r} not found in registry``.
    Exact names intentionally follow the current owner of that name under the
    lock; full ids and short handles remain pinned to the selected session.
    Ambiguity and unavailable identity evidence are refusals: falling back could
    make a token that also happens to be a row name select that row and act on
    the wrong session."""
    from fno.agents.registry import AgentResolutionError, resolve_agent

    try:
        resolved = resolve_agent(token, path=registry_path)
        expected_identity = (
            None
            if resolved.matched_by == "name"
            else _recipient_identity_key(resolved.entry)
        )
        return resolved.entry, expected_identity
    except AgentResolutionError as exc:
        if exc.ambiguous or exc.unavailable:
            raise DispatchAskError(
                str(exc),
                exit_code=12 if exc.unavailable else 2,
            ) from exc
        return _resolve_registry_entry(token, registry_path=registry_path), None


def _resolve_registry_entry(name: str, *, registry_path: Optional[Path] = None) -> AgentEntry:
    """Load the registry and return the entry for ``name``.

    Raises :class:`DispatchAskError`(exit_code=2) when the entry is
    missing - the convention for lifecycle verbs (AC1-UI / AC2-UI /
    AC7-UI). Raises (exit_code=12) on registry-read failures so the
    operator gets a distinct "registry busted" signal.

    Args:
        name: agent name to look up.
        registry_path: optional override forwarded to ``load_registry``.
            Used by ``with_agent_lock_and_entry`` so a non-default
            registry override locks AND reads against the same file
            (Codex P2 on PR #317: previously the override only reached
            the lock path and the entry read silently fell through to
            the default registry).
    """
    try:
        entries = load_registry(registry_path)
    except (OSError, ValueError, RegistryVersionError) as exc:
        raise DispatchAskError(
            f"registry read failed: {exc}",
            exit_code=12,
        ) from exc
    matches = [entry for entry in entries if entry.name == name]
    if len(matches) > 1:
        candidates = ", ".join(
            f"{entry.harness_session_id or entry.short_id or '-'} ({entry.harness})"
            for entry in matches
        )
        raise DispatchAskError(
            f"registry name {name!r} is ambiguous across {len(matches)} rows: "
            f"{candidates}. Repair the duplicate registry rows before retrying.",
            exit_code=2,
        )
    if matches:
        return matches[0]
    raise DispatchAskError(
        f"agent {name!r} not found in registry",
        exit_code=2,
    )


@contextmanager
def with_agent_lock_and_entry(
    name: str,
    *,
    registry_path: Optional[Path] = None,
    timeout: float = 30.0,
    on_wait: Optional[Callable[[], None]] = None,
    expected_identity: Optional[RecipientIdentity] = None,
) -> Iterator[tuple[object, AgentEntry]]:
    """Acquire per-agent flock AND re-load the registry entry under it.

    The lifecycle write verbs (``stop_agent`` / ``rm_agent``) used to
    open-code a two-step pattern: pre-flock ``_resolve_registry_entry``
    (for fast-fail + timeout-event payload), then ``hold_agent_lock``,
    then post-flock ``_resolve_registry_entry`` (to defeat the TOCTOU
    race a concurrent ``rm`` opens between the two reads). The dual-read
    is correct but easy to get wrong: a future contributor that forgets
    the post-lock re-read would silently operate on stale data.

    This context manager enforces the correct shape:

    - Pre-flock ``_resolve_registry_entry(name)`` validates the name
      (raises ``DispatchAskError(exit_code=2)`` if missing) but its
      result is intentionally NOT yielded; the post-lock re-read is the
      one callers MUST use.
    - ``hold_agent_lock`` is entered for the duration of the with-block.
    - Post-lock ``_resolve_registry_entry(name)`` re-fetches the entry
      under the lock; this is the value yielded.

    The return shape is a positional 2-tuple ``(lock_handle, existing)``
    (Locked Decision 4): callers destructure inline, and the tuple
    composes with ``contextlib.ExitStack`` if a future verb needs to
    lock multiple agents in one scope (AC2-EDGE).

    Args:
        name: Agent name to lock.
        registry_path: Override the registry path (test hook). Defaults
            to ``paths.agents_registry_path()``.
        timeout: Lock acquisition timeout (seconds). Propagates
            ``AgentLockTimeout`` on miss; callers decide how to format
            the timeout event.
        on_wait: Optional callback fired at the standard 1s
            blocked-acquire threshold by ``hold_agent_lock``.
        expected_identity: Recipient selected from the caller's original token.
            Both registry reads must still match it; a same-name replacement is
            refused before any lifecycle side effect.

    Yields:
        ``(lock_handle, existing)`` where ``existing`` is the AgentEntry
        re-fetched under the lock. ``lock_handle`` is the opaque handle
        from ``hold_agent_lock`` and is exposed only for ExitStack
        composition; most callers do not touch it.

    Raises:
        DispatchAskError(exit_code=2): pre-flock name validation failed
            (no registry entry for ``name``).
        DispatchAskError(exit_code=2): post-flock re-read found the
            entry was deleted between pre-flock validation and lock
            acquisition (rare but possible: another process ran ``rm``
            while we were blocked on the flock).
        AgentLockTimeout: lock could not be acquired within ``timeout``.
            Propagates to the caller verbatim so each lifecycle verb
            can emit its provider-tagged ``*_timeout`` event with the
            shape its tests expect.
    """
    # Pre-flock validation. The returned snapshot is intentionally NOT
    # passed out of this scope; we re-read post-lock below so a name-addressed
    # call observes the current owner, while an address-bound call compares the
    # snapshot with expected_identity and refuses a replacement. The
    # ``registry_path`` override (Codex P2 on PR #317) forwards to BOTH
    # the lock acquisition AND the registry read so a test or future
    # caller cannot accidentally lock one file while reading another.
    if registry_path is None:
        registry_path = paths.agents_registry_path()
    pre_existing = _resolve_registry_entry(name, registry_path=registry_path)
    if (
        expected_identity is not None
        and _recipient_identity_key(pre_existing) != expected_identity
    ):
        raise DispatchAskError(
            f"agent {name!r} recipient identity changed before lock acquisition; retry",
            exit_code=2,
        )
    with hold_agent_lock(name, registry_path, timeout=timeout, on_wait=on_wait) as lock_handle:
        # Post-lock re-read. If another process deleted the entry between
        # the pre-flock validation and the flock acquisition, this raises
        # the SAME DispatchAskError shape the pre-flock path would have,
        # propagated INSIDE the with-block so the caller's exit path is
        # the regular AgentLockTimeout/exception flow rather than the
        # missing-entry flow. The lock is released as the context
        # manager unwinds (AC2-ERR).
        existing = _resolve_registry_entry(name, registry_path=registry_path)
        if (
            expected_identity is not None
            and _recipient_identity_key(existing) != expected_identity
        ):
            raise DispatchAskError(
                f"agent {name!r} recipient identity changed while acquiring its lock; retry",
                exit_code=2,
            )
        yield (lock_handle, existing)


def _mark_stopped_orphaned(name: str, existing: AgentEntry) -> None:
    """Flip a stopped agent's registry status to ``orphaned``.

    So ``fno agents list`` reflects the stop immediately rather than
    carrying the stale ``live`` value until the next reconcile.
    ``last_message_at_preserve=True`` keeps the historical timestamp -- the
    stop doesn't invalidate it. "orphaned" matches the post-reconcile
    state, so this pre-empts the eventual reconcile without introducing a
    new status value (no schema bump).

    The process is already dead by the time this runs, so a registry-write
    failure is emitted to the events stream the caller already has open and
    swallowed: it must never turn a successful stop into a raised error.
    The next reconcile picks the orphan up via the live reachability probe.
    """
    try:
        decline_reason: list[str] = []
        status_written = _update_registry_if_recipient_unchanged(
            name,
            _recipient_identity_key(existing),
            _stamp_status(name, status="orphaned", last_message_at_preserve=True),
            decline_reason=decline_reason,
        )
        if not status_written:
            first_reason = decline_reason[0] if decline_reason else ""
            reason = (
                first_reason
                if first_reason in ("row_removed", "duplicate_name")
                else "recipient_identity_changed"
            )
            events.emit(
                "agent_stopped_status_write_failed",
                name=name,
                provider="claude",
                reason=reason,
            )
    except (OSError, RegistryVersionError):
        events.emit("agent_stopped_status_write_failed", name=name, provider="claude")


#: Seconds to wait for a signalled process to exit before escalating, and the
#: poll interval used to notice a clean exit inside that window. Mirrors the
#: 5s grace the daemon's ``stop_worker_confirmed`` allows a PTY worker.
_PID_STOP_GRACE_S = 5.0
_PID_STOP_POLL_S = 0.1


def _stop_by_pid(name: str, existing: AgentEntry) -> StopResult:
    """Stop an agent OUT OF BAND, by signalling its recorded pid.

    This is the arm for a session that cannot answer. ``claude stop`` asks the
    session to shut ITSELF down, which needs an API call, and the population
    that most needs stopping is the one whose API is gone: a provider-capped
    worker refuses once, holds a full session of context, and then burns the
    whole 30s shellout timeout before the caller raises exit 15 with the
    process still running. Nothing about that population lacks a transport id,
    which is why the old scoping of this function -- "the last resort after
    ``stop_agent`` finds no ``short_id``" -- meant it never ran for the exact
    workers it saves. A docstring that scopes a capability out of its real
    population is a capability nobody has.

    It is also still the arm for a row carrying a live process and no transport
    id at all, which the spawn receipt sometimes never yields; refusing there
    left the operator holding a running worker with no verb that addressed it,
    the duplicate-worker half of the wave-boundary handoff failure.

    Ownership is re-proved through ``_pid_alive`` immediately before every
    signal. It compares the recorded process-start token, so a pid recycled by
    an unrelated process is refused rather than killed, including a recycle
    inside the SIGTERM grace window. A row with no token at all is refused
    outright, and so is an unreadable verdict (``None`` -- no psutil, or a
    process this uid cannot inspect): never signal a pid whose incarnation
    cannot be proved.

    The probe and the signal remain two syscalls, so a recycle landing in that
    microsecond gap is not excluded -- closing it needs pidfd, which is neither
    portable here nor worth it against a window this size. The guarantee is
    "proved ours as late as possible", not "proved ours atomically".
    """
    import signal

    from fno.agents.spawn_gate import _pid_alive

    pid = existing.pid

    def _still_ours() -> bool:
        return _pid_alive(pid, existing.pid_start_time) is True

    def _confirmed_gone() -> bool:
        """True only when the process is KNOWN dead.

        ``_pid_alive`` returns ``None`` for "cannot tell" (no psutil, or a
        process this uid cannot inspect). Folding that into "gone" would report
        a clean stop over a process that is still running, so only an explicit
        ``False`` counts.
        """
        return _pid_alive(pid, existing.pid_start_time) is False

    # Require the incarnation token before signalling anything. Without it
    # ``_pid_alive`` degrades to bare liveness, which cannot distinguish our
    # worker from an unrelated process that inherited the pid. Good enough to
    # probe with, not good enough to kill on.
    if not pid or existing.pid_start_time is None or not _still_ours():
        raise DispatchAskError(
            f"registry entry {name!r} has no short id and no process this row "
            f"can prove it owns; there is nothing safe to stop. Note that "
            f"'fno agents rm {name}' clears the row but does NOT stop a "
            "running session.",
            exit_code=12,
        )

    def _signal(sig: int) -> None:
        """Send ``sig``, treating an already-exited process as success."""
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            pass
        except PermissionError as exc:
            raise DispatchAskError(
                f"not permitted to signal pid {pid} for agent {name!r}: {exc}",
                exit_code=13,
            ) from exc

    def _wait_gone() -> bool:
        deadline = time.monotonic() + _PID_STOP_GRACE_S
        while time.monotonic() < deadline:
            if _confirmed_gone():
                return True
            time.sleep(_PID_STOP_POLL_S)
        return _confirmed_gone()

    _signal(signal.SIGTERM)
    escalated = False
    # Re-prove ownership before escalating rather than inferring it from
    # _wait_gone's False: a pid recycled during the grace window must take no
    # SIGKILL, and _signal itself does not check.
    if not _wait_gone() and _still_ours():
        escalated = True
        _signal(signal.SIGKILL)
        _wait_gone()

    # Success demands a CONFIRMED death, not the absence of a positive liveness
    # answer. An unreadable probe means we do not know, and reporting a clean
    # stop there is the silent failure this whole path exists to avoid.
    if not _confirmed_gone():
        still_ours = _still_ours()
        reason = "survived_sigkill" if still_ours else "liveness_unconfirmed"
        events.emit(
            "agent_stop_error",
            name=name,
            provider=existing.harness,
            pid=pid,
            reason=reason,
        )
        detail = (
            "survived SIGTERM and SIGKILL"
            if still_ours
            else "could not be confirmed dead after SIGTERM and SIGKILL"
        )
        raise DispatchAskError(
            f"pid {pid} for agent {name!r} {detail}",
            exit_code=1,
        )

    events.emit(
        "agent_stopped",
        name=name,
        provider=existing.harness,
        claude_exit=None,
        pid=pid,
        stopped_by="pid",
        escalated=escalated,
    )
    _mark_stopped_orphaned(name, existing)
    print(f"stopped: {name} (pid {pid})", flush=True)
    return StopResult(name=name, provider=existing.harness, claude_exit=None)


def stop_agent(
    name: str,
    *,
    lock_timeout: float = _DEFAULT_LOCK_TIMEOUT,
    shellout_timeout: float = _DEFAULT_CLAUDE_SHELLOUT_TIMEOUT,
) -> StopResult:
    """Stop an agent's underlying session.

    claude: shells out to ``claude stop <short_id>``; surface its stderr
    verbatim on non-zero exit and propagate the exit code to the caller
    (AC1-ERR). On timeout, raise ``DispatchAskError(exit_code=15)``.

    codex / gemini: synchronous between asks (no persistent process to
    stop). Print an explanatory line to stderr and return cleanly. The
    registry is unchanged.

    Always emits ``agent_stopped`` with ``provider`` and ``claude_exit``
    (``null`` for codex/gemini) for forensic visibility.

    Raises:
        DispatchAskError: name validation, missing agent, claude not on
            PATH, claude shellout timeout, lock timeout.
    """
    _validate_lifecycle_name(name)
    # Resolve any address form once, then carry that identity through the
    # canonical-name flock. A same-name replacement cannot inherit this action.
    pre_existing, expected_identity = _resolve_lifecycle_target(name)
    name = pre_existing.name
    # Pre-flock fast-fail + capture provider for the lock-timeout event
    # payload. The authoritative load happens inside
    # ``with_agent_lock_and_entry`` below; this pre-read exists ONLY so
    # the AgentLockTimeout branch can name the provider in its event
    # emit. The lint script `scripts/lint-flock-pattern.sh` allows this
    # because we do NOT call ``hold_agent_lock`` directly in this function
    # body — the helper encapsulates the lock acquisition.
    pre_provider = pre_existing.harness

    def _on_wait() -> None:
        print(f"Waiting for agent {name!r} lock...", file=sys.stderr, flush=True)

    try:
        with with_agent_lock_and_entry(
            name,
            timeout=lock_timeout,
            on_wait=_on_wait,
            expected_identity=expected_identity,
        ) as (
            _lock_handle,
            existing,
        ):
            if existing.harness in ("codex", "gemini"):
                # Locked Decision 5: stop is a no-op between asks for the
                # synchronous providers. Emit the same event for symmetry
                # with the claude path so observability stays uniform.
                print(
                    f"{existing.harness} agents are synchronous; stop is a "
                    "no-op between asks. SIGINT an in-flight ask to "
                    "interrupt.",
                    file=sys.stderr,
                )
                events.emit(
                    "agent_stopped",
                    name=name,
                    provider=existing.harness,
                    claude_exit=None,
                )
                return StopResult(name=name, provider=existing.harness, claude_exit=None)

            if existing.harness != "claude":
                raise DispatchAskError(
                    f"stop for provider {existing.harness!r} is not implemented",
                    exit_code=2,
                )

            # `short_id` is the whole transport-id chain on this path: only
            # claude rows reach here, and `AgentEntry.session_id` is a property
            # that resolves to `short_id` for claude (HARNESS_SESSION_ID_FIELDS),
            # so there is no second id to try. A row with no transport id at all
            # goes straight to the signal arm. A row WITH one still reaches it,
            # by escalation, when the cooperative stop cannot land - see
            # `_escalate_to_pid` below.
            short_id = existing.short_id
            if not short_id:
                return _stop_by_pid(name, existing)

            def _escalate_to_pid() -> Optional[StopResult]:
                """The cooperative stop could not land: try the signal arm.

                No recovery mechanism may depend on the capped provider.
                ``claude stop`` needs the session to make an API call, so
                against a capped worker it can only ever time out. The pid arm
                needs no API: it re-proves the recorded process-start token
                immediately before every signal, allows a 5s grace, and refuses
                a row it cannot prove it owns.

                Returns None when there is nothing provable to signal, or when
                the signal arm itself refused. "Cannot tell" is not "gone", so
                the caller must raise on None rather than report a stop it did
                not achieve - and must not spawn a successor either, or one
                worktree gets two writers.
                """
                if not existing.pid or existing.pid_start_time is None:
                    return None
                try:
                    return _stop_by_pid(name, existing)
                except DispatchAskError as pid_exc:
                    print(
                        f"out-of-band stop for {name!r} also refused: {pid_exc}",
                        file=sys.stderr,
                    )
                    return None

            if not is_provider_available("claude"):
                raise DispatchAskError("claude CLI not on PATH", exit_code=14)

            from fno.agents.harnesses import claude as claude_mod

            try:
                exit_code, stderr_text = claude_mod.claude_stop(short_id, timeout=shellout_timeout)
            except FileNotFoundError as exc:
                # PATH check passed above but claude vanished mid-call; treat
                # the same as not-on-PATH to mirror US1's contract.
                raise DispatchAskError("claude CLI not on PATH", exit_code=14) from exc
            except subprocess.TimeoutExpired as exc:
                # The signature of a capped worker: it cannot answer, so the
                # cooperative request burns its whole timeout. Escalate BEFORE
                # emitting, so a successful signal leaves one honest
                # `agent_stopped` naming the pid arm rather than two events
                # disagreeing about whether the worker is gone.
                escalated = _escalate_to_pid()
                if escalated is not None:
                    return escalated
                events.emit(
                    "agent_stopped",
                    name=name,
                    provider="claude",
                    claude_exit=None,
                    timed_out=True,
                    stopped_by="shellout",
                )
                raise DispatchAskError(
                    f"claude stop timed out after {int(shellout_timeout)}s",
                    exit_code=15,
                ) from exc
            except OSError as exc:
                # Gemini medium: surface PermissionError / EIO as structured
                # DispatchAskError rather than a raw Python traceback. Mirrors
                # the catch on attach_agent and the new one on rm_agent.
                events.emit(
                    "agent_stopped",
                    name=name,
                    provider="claude",
                    claude_exit=None,
                    error=str(exc),
                    error_type=type(exc).__name__,
                )
                raise DispatchAskError(f"claude stop failed: {exc}", exit_code=1) from exc

            def _emit_shellout_stop() -> None:
                events.emit(
                    "agent_stopped",
                    name=name,
                    provider="claude",
                    claude_exit=exit_code,
                    short_id=short_id,
                    stopped_by="shellout",
                )

            if exit_code != 0:
                if stderr_text:
                    sys.stderr.write(stderr_text)
                    if not stderr_text.endswith("\n"):
                        sys.stderr.write("\n")
                escalated = _escalate_to_pid()
                if escalated is not None:
                    return escalated
                _emit_shellout_stop()
                raise DispatchAskError(
                    f"claude stop {short_id} exited {exit_code}",
                    exit_code=1,
                )

            _emit_shellout_stop()

            _mark_stopped_orphaned(name, existing)

            print(
                f"stopped: {name} ({short_id})",
                flush=True,
            )
            return StopResult(name=name, provider="claude", claude_exit=exit_code)
    except AgentLockTimeout as exc:
        events.emit(
            "agent_stopped", name=name, provider=pre_provider, claude_exit=None, lock_timeout=True
        )
        raise DispatchAskError(
            f"lock timeout for agent {name!r} after {exc.timeout}s"
            f"{exc.holder_note()}",
            exit_code=11,
        ) from exc


def _teardown_harness_session(
    existing: AgentEntry,
    *,
    name: str,
    force: bool,
) -> Optional[str]:
    """Delete a non-claude agent's record from its own harness store.

    Returns the teardown error message when ``force`` swallowed a failure,
    else None. The caller folds it into the ONE terminal ``agent_removed``
    event: emitting from in here would both duplicate that event and
    report a registry mutation that has not happened yet.

    Record-only: the harness's index record goes, the conversation stays.
    Upholds the ordering invariant by raising before the caller touches
    the registry -- unless ``force``, which downgrades every failure to a
    stderr WARN naming the orphan so the operator can clean it later.

    An already-absent harness record is success, not an error: a manually
    cleaned store must not wedge ``fno agents rm``.

    opencode is registry-only because it has no record-only teardown at
    all; see :mod:`fno.agents.harnesses.opencode`.
    """
    harness = existing.harness
    sid = existing.harness_session_id

    def _fail(message: str, *, exit_code: int) -> str:
        if not force:
            # Terminal here, so this IS the only event for this rm.
            events.emit(
                "agent_removed",
                name=name,
                provider=harness,
                force=False,
                registry_changed=False,
                teardown_error=message,
            )
            raise DispatchAskError(message, exit_code=exit_code)
        sys.stderr.write(
            f"WARN: {message}; --force given, removing registry only. "
            f"Orphan {harness} session record: {sid}\n"
        )
        return message

    if harness == "opencode":
        # No record-only teardown exists for opencode: removing the session
        # would take its child sessions and full message history with it.
        # Registry-only, and say so rather than implying nothing was left.
        from fno.agents.harnesses import opencode as opencode_mod

        if sid:
            print(opencode_mod.REGISTRY_ONLY_NOTE.format(sid=sid), flush=True)
        return None

    if not sid:
        # Refuse rather than assume there is nothing to clean: the harness
        # record may well exist, and this row simply lost the id that
        # addresses it. Silently dropping the row would orphan it for good.
        return _fail(
            f"registry entry has no {harness} session id on file; cannot "
            "tear down the harness record. Re-run with --force to drop the "
            "registry row anyway.",
            exit_code=12,
        )

    if harness == "codex":
        from fno.agents.harnesses import codex as codex_mod

        try:
            removed = codex_mod.remove_session_index_entry(sid)
        except ValueError as exc:
            return _fail(str(exc), exit_code=12)
        except OSError as exc:
            return _fail(f"codex session index rewrite failed: {exc}", exit_code=1)
        print(
            f"torn down: codex session index entry {sid}"
            if removed
            else f"already gone: codex session index entry {sid}",
            flush=True,
        )
        return None

    # Fail loud rather than fall off the end: the caller's harness tuple and
    # the arms above are two lists nothing ties together, and a silent return
    # would drop the registry row while leaving the session record behind --
    # the exact orphan this function exists to prevent.
    raise DispatchAskError(
        f"no teardown arm for harness {harness!r}",
        exit_code=2,
    )


def rm_agent(
    name: str,
    *,
    force: bool = False,
    lock_timeout: float = _DEFAULT_LOCK_TIMEOUT,
    shellout_timeout: float = _DEFAULT_CLAUDE_SHELLOUT_TIMEOUT,
) -> RmResult:
    """Remove an agent from the registry, and from claude's supervisor too.

    claude: shellout FIRST, registry mutation AFTER (Locked Decision 6
    ordering invariant). On non-forceful claude refusal, the registry is
    unchanged so the operator can address the underlying issue (e.g.
    uncommitted worktree state) and retry. ``--force`` overrides: the
    registry entry is removed even when ``claude rm`` fails, with a
    stderr WARN about the orphan supervisor session.

    codex / opencode: the harness's own session RECORD is torn down
    first (codex's ``session_index.jsonl`` entry, opencode's session via
    ``opencode session delete``), registry row after -- same ordering
    invariant, same ``--force`` override. Transcript files always stay
    (Locked Decision 1).

    gemini: registry-only; no teardown arm for a deprecated provider.

    Emits ``agent_removed`` with ``provider``, ``force``, ``claude_exit``
    fields.

    """
    _validate_lifecycle_name(name)
    # Resolve any address form once, then carry that identity through the
    # canonical-name flock. A same-name replacement cannot inherit this action.
    pre_existing, expected_identity = _resolve_lifecycle_target(name)
    name = pre_existing.name
    # Pre-flock fast-fail + capture provider for lock-timeout event
    # payload. See ``stop_agent`` for the lint-pattern rationale: the
    # body does NOT call ``hold_agent_lock`` directly — that lives inside
    # ``with_agent_lock_and_entry``, which the lint script allowlists.
    pre_provider = pre_existing.harness

    def _on_wait() -> None:
        print(f"Waiting for agent {name!r} lock...", file=sys.stderr, flush=True)

    try:
        with with_agent_lock_and_entry(
            name,
            timeout=lock_timeout,
            on_wait=_on_wait,
            expected_identity=expected_identity,
        ) as (
            _lock_handle,
            existing,
        ):
            claude_exit: Optional[int] = None
            # Non-None only when --force swallowed a teardown failure; rides
            # the terminal event so the forensic stream stays single and true.
            teardown_error: Optional[str] = None

            # The teardown below destroys the harness session record, which IS
            # the resume handle. Name it and name the verb that reverses it,
            # before anything is torn down. The interactive prompt lives at the
            # `rust_runtime` seam instead, because that is the one place both
            # runtimes pass through; this is the warning half only, so the
            # documented wedged-daemon escape hatch (`python -c "... rm_agent
            # (...)"` in the king brief's CLI reference) is not silent either.
            handle = rm_notice.resume_handle_for(existing)
            if handle is not None:
                sys.stderr.write(
                    rm_notice.resume_handle_notice(name, existing.harness, handle)
                )

            if existing.harness == "claude":
                short_id = existing.short_id
                if not short_id:
                    if not force:
                        # Help text promises --force can drop the orphan row,
                        # but the original code raised here unconditionally
                        # (Codex P1 finding). Honor the promise: without
                        # --force, refuse; with --force, fall through to
                        # the registry-only removal at the bottom.
                        raise DispatchAskError(
                            f"registry entry {name!r} has no short id on file; "
                            f"cannot rm via claude shellout. Re-run with --force "
                            "to drop the orphan registry entry.",
                            exit_code=12,
                        )
                    # --force on a corrupted row: skip the claude shellout,
                    # emit a forensic WARN, proceed to registry-only removal.
                    sys.stderr.write(
                        "WARN: registry entry has no short id on file; "
                        "--force given, removing registry row without "
                        "shelling out to claude.\n"
                    )
                    claude_exit = None
                else:
                    if not is_provider_available("claude"):
                        raise DispatchAskError("claude CLI not on PATH", exit_code=14)

                    from fno.agents.harnesses import claude as claude_mod

                    try:
                        claude_exit, stderr_text = claude_mod.claude_rm(
                            short_id, timeout=shellout_timeout
                        )
                    except FileNotFoundError as exc:
                        raise DispatchAskError("claude CLI not on PATH", exit_code=14) from exc
                    except subprocess.TimeoutExpired as exc:
                        events.emit(
                            "agent_removed",
                            name=name,
                            provider="claude",
                            claude_exit=None,
                            force=force,
                            timed_out=True,
                            registry_changed=False,
                        )
                        raise DispatchAskError(
                            f"claude rm timed out after {int(shellout_timeout)}s",
                            exit_code=15,
                        ) from exc
                    except OSError as exc:
                        # Gemini medium: surface as structured DispatchAskError
                        # not a raw traceback. Matches attach_agent's catch.
                        events.emit(
                            "agent_removed",
                            name=name,
                            provider="claude",
                            claude_exit=None,
                            force=force,
                            registry_changed=False,
                            error=str(exc),
                            error_type=type(exc).__name__,
                        )
                        raise DispatchAskError(f"claude rm failed: {exc}", exit_code=1) from exc

                    if claude_exit != 0:
                        if stderr_text:
                            sys.stderr.write(stderr_text)
                            if not stderr_text.endswith("\n"):
                                sys.stderr.write("\n")
                        if not force:
                            # Registry unchanged: AC2-ERR contract. Emit event
                            # for forensics so a downstream `fno agents list`
                            # vs claude-supervisor diff can be reconciled.
                            events.emit(
                                "agent_removed",
                                name=name,
                                provider="claude",
                                claude_exit=claude_exit,
                                force=False,
                                registry_changed=False,
                                short_id=short_id,
                            )
                            raise DispatchAskError(
                                f"claude rm {short_id} exited {claude_exit}",
                                exit_code=1,
                            )
                        # --force path: warn about the orphan supervisor and
                        # proceed to drop the registry row.
                        sys.stderr.write(
                            "WARN: claude rm failed but --force given; removing "
                            f"registry only. Orphan supervisor: claude rm "
                            f"{short_id} to clean later.\n"
                        )

            elif existing.harness in ("codex", "opencode"):
                teardown_error = _teardown_harness_session(
                    existing,
                    name=name,
                    force=force,
                )
            elif existing.harness != "gemini":
                raise DispatchAskError(
                    f"rm for provider {existing.harness!r} is not implemented",
                    exit_code=2,
                )
            # gemini: registry-only. No teardown arm -- the provider is
            # deprecated, so a speculative one would be untestable guesswork.

            decline_reason: list[str] = []
            try:
                registry_changed = _update_registry_if_recipient_unchanged(
                    name,
                    _recipient_identity_key(existing),
                    lambda entries: [e for e in entries if e.name != name],
                    decline_reason=decline_reason,
                )
            except (OSError, RegistryVersionError) as exc:
                events.emit(
                    "agent_removed",
                    name=name,
                    provider=existing.harness,
                    claude_exit=claude_exit,
                    force=force,
                    registry_changed=False,
                    teardown_error=teardown_error,
                    error=str(exc),
                    error_type=type(exc).__name__,
                )
                raise DispatchAskError(
                    f"registry write failed: {exc}",
                    exit_code=12,
                ) from exc
            if not registry_changed:
                row_removed = decline_reason and decline_reason[0] == "row_removed"
                events.emit(
                    "agent_removed",
                    name=name,
                    provider=existing.harness,
                    claude_exit=claude_exit,
                    force=force,
                    registry_changed=False,
                    teardown_error=teardown_error,
                    error=(
                        "resolved row removed entirely during harness removal"
                        if row_removed
                        else "recipient identity changed during harness removal"
                    ),
                    error_type=(
                        "RecipientRowRemoved"
                        if row_removed
                        else "RecipientIdentityChanged"
                    ),
                )
                raise DispatchAskError(
                    f"agent {name!r}: resolved to a row the registry does not hold, "
                    "or its recipient identity changed during rm; nothing was "
                    "removed, and any replacement row was retained. Re-read it "
                    "with `fno agents list --json` and rm by the exact `name` field.",
                    exit_code=12,
                )

            # Stdout "removed:" prints come AFTER update_registry succeeds so
            # a write failure cannot leave the operator with a misleading
            # confirmation. (Sigma-review C3 finding.)
            if existing.harness == "codex" and existing.harness_session_id:
                print(
                    f"removed: {name} (codex transcript files left on disk)",
                    flush=True,
                )
            else:
                print(f"removed: {name}", flush=True)

            events.emit(
                "agent_removed",
                name=name,
                provider=existing.harness,
                claude_exit=claude_exit,
                force=force,
                registry_changed=True,
                teardown_error=teardown_error,
            )
            return RmResult(
                name=name,
                provider=existing.harness,
                claude_exit=claude_exit,
                force=force,
                registry_changed=True,
            )
    except AgentLockTimeout as exc:
        # Symmetric with stop_agent's lock-timeout emit so forensics can
        # distinguish "rm refused at flock layer" from "operator never
        # ran rm" via events.jsonl alone. (Sigma-review #2 finding.)
        events.emit(
            "agent_removed",
            name=name,
            provider=pre_provider,
            claude_exit=None,
            force=force,
            registry_changed=False,
            lock_timeout=True,
        )
        raise DispatchAskError(
            f"lock timeout for agent {name!r} after {exc.timeout}s"
            f"{exc.holder_note()}",
            exit_code=11,
        ) from exc


def reconcile_agents(
    *,
    claude_logs_timeout: float = _DEFAULT_CLAUDE_LOGS_TAIL_TIMEOUT,
    codex_session_index_path: Optional[Path] = None,
) -> ReconcileResult:
    """Walk the registry, sync statuses against provider reality, report.

    Read-mostly: each entry's status flip goes through ``update_registry``'s
    atomic load+filter+write cycle. No per-agent flock (Locked Decision 8):
    concurrent reconcile + ask is safe because ask mutates ``last_message_at``
    via the same atomic cycle and last-writer-wins on the timestamp; the
    status field updated by reconcile is independent.

    For each entry:

    - **claude**: ``claude logs <short_id> --tail 1`` (10s timeout) decides
      reachability. Exit 0 → live; anything else → orphaned.
    - **codex**: presence in ``~/.codex/session_index.jsonl`` decides
      reachability. Missing index → skip with an ``errors`` entry, leave
      status untouched (AC3-EDGE: fresh install must NOT trigger false
      orphan flags).
    - **gemini**: legacy registry rows are warned and skipped because the
      Python provider adapter is retired; new work routes to agy.

    Emits ``reconcile_done`` once at the end with the aggregate counts.
    """
    try:
        entries = load_registry()
    except (OSError, ValueError, RegistryVersionError) as exc:
        raise DispatchAskError(
            f"registry read failed: {exc}",
            exit_code=12,
        ) from exc

    entry_by_name = {entry.name: entry for entry in entries}

    def _vendor_for_name(name: str) -> Optional[str]:
        entry = entry_by_name.get(name)
        return entry.provider if entry is not None else None

    def _harness_for_name(name: str) -> Optional[str]:
        entry = entry_by_name.get(name)
        return entry.harness if entry is not None else None

    orphaned: list[dict] = []
    recovered: list[dict] = []
    skipped: list[dict] = []
    errors: list[dict] = []
    backfilled: list[dict] = []
    # name -> (probed short_id, resolved harness_session_id) for a live row
    # whose canonical id never landed (x-ec59). Folded into the SAME batched
    # update_registry write as the status flips, so no new write cycle or lock
    # scope appears. The probed short_id is retained so the write only stamps a row
    # that STILL matches what we probed: a slow reconcile can race a rm + same-name
    # re-register, and stamping by name alone would put the old row's claude uuid
    # onto a replacement (possibly codex/gemini) row and misroute its mail.
    pending_backfill: dict[str, tuple[Optional[str], str]] = {}
    # name -> (pid, mux ref, resolved Codex thread id). Unlike the Claude
    # backfill above, every field is rechecked under the registry write lock:
    # the pane process is the identity authority and a same-name replacement
    # must never inherit its rollout.
    pending_codex_backfill: dict[
        str,
        tuple[Optional[int], Optional[int], dict, int, Optional[int], str],
    ] = {}

    # ``pending_updates`` accumulates per-name status flips across the
    # probe loop; at the end we apply ALL of them via a SINGLE
    # ``update_registry`` call. The dict[str, AgentEntry] shape (vs
    # list[tuple]) makes last-writer-wins explicit when the same name
    # could appear twice (Locked Decision 5; reconcile shouldn't probe
    # the same name twice today, but a future stale-cache or duplicate
    # row would silently collapse here instead of writing twice).
    #
    # Atomicity contract (AC3-ERR / Locked Decision 1): SIGINT mid-loop
    # discards ``pending_updates`` because the propagated KeyboardInterrupt
    # bypasses the post-loop ``update_registry`` call. The on-disk
    # registry mtime never changes.
    pending_updates: dict[str, tuple[AgentEntry, AgentStatus]] = {}

    # Read codex's session index ONCE outside the loop so a registry with
    # N codex agents only pays the I/O cost once. Mirror the same one-shot
    # capability check for claude so a host without `claude` on PATH does
    # NOT mass-flip every claude row to orphaned (sigma-review C1 finding:
    # the false-orphan storm is the worst kind of silent failure — it
    # rewrites the registry on insufficient evidence).
    from fno.agents.harnesses import codex as codex_mod

    # Tri-state per-codex-side capability: True (readable + present),
    # False (file missing — fresh install), None (file present but
    # unreadable — permission/device error). Codex P1 finding on
    # PR #315: lumping "unreadable" with "fresh install" would
    # mass-orphan every codex agent on a host with a permission glitch.
    codex_index_state: Optional[str] = None  # "ready" | "missing" | "unreadable"
    known_codex_ids: set[str] = set()
    claude_path_present: Optional[bool] = None
    for entry in entries:
        if entry.harness == "codex" and codex_index_state is None:
            # Probing the index path can raise PermissionError on hosts
            # where the parent directory is unreadable. Without this
            # catch a codex-local permission glitch would abort the
            # entire reconcile_agents call (Codex P1 round-4 finding
            # on PR #315). Treat any stat-time OSError as
            # "unreadable" — same operator outcome as a load-time
            # codex ReachabilityProbeError: route codex agents to errors.
            try:
                index_present = codex_mod.session_index_exists(
                    session_index_path=codex_session_index_path
                )
            except OSError as exc:
                codex_index_state = "unreadable"
                sys.stderr.write(
                    f"WARN: codex session index path unreadable: {exc}; "
                    "codex agents will be skipped (no reachability data)\n"
                )
            else:
                if not index_present:
                    codex_index_state = "missing"
                    missing_path = (
                        codex_session_index_path or codex_mod.default_session_index_path()
                    )
                    sys.stderr.write(
                        f"WARN: codex session index missing at {missing_path}; "
                        "codex agents will be skipped (no reachability data)\n"
                    )
                else:
                    try:
                        known_codex_ids = codex_mod.load_known_session_ids(
                            session_index_path=codex_session_index_path
                        )
                        codex_index_state = "ready"
                    except ReachabilityProbeError as exc:
                        # Catch the lifted base class so any codex-side probe
                        # error routes through the same path. The ``provider``
                        # attribute is "codex" by construction; the reason
                        # carries the underlying OSError detail.
                        codex_index_state = "unreadable"
                        sys.stderr.write(
                            f"WARN: codex session index unreadable: {exc.reason}; "
                            "codex agents will be skipped (no reachability data)\n"
                        )
        if entry.harness == "claude" and claude_path_present is None:
            claude_path_present = is_provider_available("claude")
            if not claude_path_present:
                sys.stderr.write(
                    "WARN: claude CLI not on PATH; claude agents will be "
                    "skipped (no reachability data — statuses will NOT be "
                    "flipped to orphaned)\n"
                )

    for entry in entries:
        new_status: AgentStatus
        if entry.harness == "gemini":
            sys.stderr.write(
                f"WARN: agent {entry.name!r} references retired provider 'gemini'; "
                "skipping reachability probe (route new work to agy)\n"
            )
            errors.append(
                {
                    "name": entry.name,
                    "provider": entry.provider, "harness": entry.harness,
                    "id": entry.harness_session_id,
                    "reason": "retired-provider",
                }
            )
            continue
        elif entry.harness == "codex":
            # An id-less persistent pane can be healthy while Codex is still
            # creating its rollout. Heal it from the pane's own process tree;
            # the session index is not needed for this correlation.
            if not entry.harness_session_id and entry.mux:
                if entry.status in _TERMINAL_AGENT_STATUSES:
                    continue

                from fno.agents.mux_spawn import (
                    _codex_session_id_for_pid,
                    _lookup_child_pid,
                    _mux_pane_alive,
                )
                from fno.agents.spawn_gate import _pid_alive, _process_start_time

                pane_state = _mux_pane_alive(entry.mux)
                if pane_state is None:
                    errors.append(
                        {
                            "name": entry.name,
                            "provider": entry.provider, "harness": entry.harness,
                            "id": None,
                            "reason": "mux-pane-liveness-unavailable",
                        }
                    )
                    continue
                probe_pid = entry.pid
                probe_start = entry.pid_start_time
                # An incarnation token is what makes a recorded pid trustworthy:
                # without one `_pid_alive` degrades to bare existence, so a
                # recycled pid could bind a stranger's rollout onto this row. Rows
                # written before this stamping have no token, and they are exactly
                # the population this heal exists to repair - so re-derive the
                # pane's real child and require agreement. `_mux_pane_alive` proves
                # the PANE is up, never that this pid is still its child.
                confirmed = probe_start is not None
                if pane_state is not False and not confirmed:
                    live_pid = _lookup_child_pid(
                        str(entry.mux["session"]),
                        int(entry.mux["pane_id"]),
                        subprocess.run,
                    )
                    if live_pid is not None and probe_pid in (None, live_pid):
                        probe_pid = live_pid
                        probe_start = _process_start_time(probe_pid)
                        confirmed = True
                    elif probe_pid is None:
                        errors.append(
                            {
                                "name": entry.name,
                                "provider": entry.provider, "harness": entry.harness,
                                "id": None,
                                "reason": "codex-pane-pid-pending",
                            }
                        )
                        continue

                pid_state = (
                    _pid_alive(probe_pid, probe_start)
                    if pane_state is True and probe_pid is not None
                    else False
                )
                if pid_state is None:
                    errors.append(
                        {
                            "name": entry.name,
                            "provider": entry.provider, "harness": entry.harness,
                            "id": None,
                            "reason": "codex-process-incarnation-unavailable",
                        }
                    )
                    continue
                if pid_state is True:
                    assert probe_pid is not None
                    # Live, but we could not prove this pid is the pane's child.
                    # Never stamp an identity on that; a DEAD unconfirmed pid still
                    # takes the orphan path below, which binds nothing.
                    if not confirmed:
                        errors.append(
                            {
                                "name": entry.name,
                                "provider": entry.provider, "harness": entry.harness,
                                "id": None,
                                "reason": "codex-pane-pid-unconfirmed",
                            }
                        )
                        continue
                    probe_failed = False
                    try:
                        healed = _codex_session_id_for_pid(probe_pid)
                    except Exception as exc:  # noqa: BLE001 -- stays pending, never guesses
                        # The correlator already absorbs every EXPECTED failure
                        # (gone / access-denied / no rollout open) and returns None
                        # for them, so anything landing here is unexpected. Keep the
                        # row pending, but never render it as the benign "codex has
                        # not opened its rollout yet" case with the cause dropped -
                        # that reads as "still starting" forever.
                        healed = None
                        probe_failed = True
                        sys.stderr.write(
                            f"WARN: codex identity probe failed for {entry.name} "
                            f"(pid {probe_pid}): {exc}\n"
                        )
                    if healed:
                        # Scan the rows AND the ids this same pass already decided
                        # to stamp. Two id-less rows pointing at one pane both see
                        # the other as `harness_session_id=None`, so a rows-only
                        # check clears both and stamps the duplicate identity this
                        # guard exists to prevent.
                        duplicate = any(
                            other.name != entry.name
                            and other.harness_session_id == healed
                            for other in entries
                        ) or any(
                            claimed[-1] == healed
                            for name, claimed in pending_codex_backfill.items()
                            if name != entry.name
                        )
                        if duplicate:
                            if entry.status != "spawning":
                                pending_updates[entry.name] = (entry, "spawning")
                            errors.append(
                                {
                                    "name": entry.name,
                                    "provider": entry.provider, "harness": entry.harness,
                                    "id": None,
                                    "reason": "duplicate-codex-session-id",
                                }
                            )
                        else:
                            pending_codex_backfill[entry.name] = (
                                entry.pid,
                                entry.pid_start_time,
                                dict(entry.mux),
                                probe_pid,
                                probe_start,
                                healed,
                            )
                        continue

                    if entry.status != "spawning":
                        pending_updates[entry.name] = (entry, "spawning")
                    errors.append(
                        {
                            "name": entry.name,
                            "provider": entry.provider, "harness": entry.harness,
                            "id": None,
                            "reason": (
                                "codex-session-probe-failed"
                                if probe_failed
                                else "codex-session-id-pending"
                            ),
                        }
                    )
                    continue

                # The pane process is gone. Preserve terminal rows above; a
                # nonterminal pending/live row follows the existing orphan path.
                new_status = "orphaned"
            elif entry.status in {"failed", "exited", "permanent_dead"}:
                continue
            elif not entry.harness_session_id and entry.status == "orphaned":
                continue
            elif not entry.harness_session_id:
                events.emit(
                    "agent_inconsistent",
                    name=entry.name,
                    provider="codex",
                )
                errors.append(
                    {
                        "name": entry.name,
                        "provider": entry.provider, "harness": entry.harness,
                        "id": None,
                        "reason": "missing-codex-session-id",
                    }
                )
                continue
            elif codex_index_state != "ready":
                # AC3-EDGE: cannot probe codex reachability; report as
                # error but do NOT flip status. The reason discriminator
                # distinguishes "fresh install" (operator action: ignore)
                # from "permission glitch" (operator action: fix perms).
                if codex_index_state == "unreadable":
                    reason = "codex-session-index-unreadable"
                else:
                    reason = "codex-session-index-missing"
                errors.append(
                    {
                        "name": entry.name,
                        "provider": entry.provider, "harness": entry.harness,
                        "id": entry.harness_session_id,
                        "reason": reason,
                    }
                )
                continue
            else:
                reachable = entry.harness_session_id in known_codex_ids
                new_status = "live" if reachable else "orphaned"

        elif entry.harness == "claude":
            # EVERY claude PANE row reconciles from the pane, with or without a
            # session id. A mux row's short_id is deliberately empty (one live ref
            # per row), so the `claude logs` probe below - which is keyed on
            # short_id - can never reach one; without this arm a pane row falls to
            # `missing-claude-short-id`, which only reports and never changes
            # status, so a dead pane holds its name against every future spawn of
            # that name, forever.
            #
            # Gating this on `not harness_session_id` was the earlier shape and it
            # was the AGENTS.md path-uniqueness trap in miniature: it covered the
            # pane only until its worker restamped, and a pane that then died was
            # unreachable by any arm. The pane is the reachability oracle for a
            # pane-hosted row for its whole life, not just before it has an id.
            #
            # Deliberately ahead of the claude-on-PATH guard: this probes the mux
            # and the pid, never the claude CLI, so a host where claude was
            # removed can still retire a provably dead pane.
            if entry.mux:
                if entry.status in _TERMINAL_AGENT_STATUSES:
                    continue

                from fno.agents.mux_spawn import _lookup_child_pid, _mux_pane_alive
                from fno.agents.spawn_gate import _pid_alive, _process_start_time

                pane_state = _mux_pane_alive(entry.mux)
                if pane_state is None:
                    errors.append(
                        {
                            "name": entry.name,
                            "provider": entry.provider, "harness": entry.harness,
                            "id": None,
                            "reason": "mux-pane-liveness-unavailable",
                        }
                    )
                    continue
                probe_pid = entry.pid
                probe_start = entry.pid_start_time
                # `_mux_pane_alive` proves the PANE is up, never that the recorded
                # pid is still its child. Without an incarnation token `_pid_alive`
                # degrades to bare existence, so re-derive the pane's real child
                # before trusting a live answer. Unlike the codex arm this never
                # stamps an identity from the pid, so an unconfirmed pid only ever
                # keeps the row waiting - it can never bind a stranger.
                # ALWAYS re-derive the pane's current child, even when the row
                # carries an incarnation token. That token proves only that the
                # stored pid's incarnation is alive; it says nothing about which
                # pane that process now belongs to. A mux restart can hand
                # `(session, pane_id)` to a different child while the original is
                # still running, and then `_mux_pane_alive` and `_pid_alive` are
                # both true about DIFFERENT processes - which preserves the row as
                # live and points delivery at the stranger's pane.
                confirmed = False
                if pane_state is not False:
                    live_pid = _lookup_child_pid(
                        str(entry.mux["session"]),
                        int(entry.mux["pane_id"]),
                        subprocess.run,
                    )
                    if live_pid is not None and probe_pid in (None, live_pid):
                        probe_pid = live_pid
                        # Keep a stored incarnation token: it is stronger than a
                        # fresh read, which cannot see a pid recycled since.
                        if probe_start is None:
                            probe_start = _process_start_time(probe_pid)
                        confirmed = True
                # A live pane with no usable pid is INCONCLUSIVE, never dead.
                # `_lookup_child_pid` is best-effort, so folding its miss into
                # `False` would orphan a healthy worker on absent evidence - and
                # `orphaned` is terminal here while a later restamp only promotes
                # `spawning`, so that mistake is permanent. Stay pending, exactly
                # as the codex arm does with `codex-pane-pid-pending`.
                if pane_state is True and probe_pid is None:
                    errors.append(
                        {
                            "name": entry.name,
                            "provider": entry.provider, "harness": entry.harness,
                            "id": None,
                            "reason": "claude-pane-pid-pending",
                        }
                    )
                    continue
                # An uncorrelated pid may not be this pane's at all. A legacy row
                # carries no incarnation token, so `_pid_alive` degrades to bare
                # existence there; a mux restart can hand `(session, pane_id)` to
                # a different child while the recorded pid has been RECYCLED and
                # is alive. Trusting that keeps the row `live` and points
                # name-based delivery at a stranger. Only a DEAD pane is decided
                # without correlation, since nothing can be bound to it.
                if pane_state is True and not confirmed:
                    errors.append(
                        {
                            "name": entry.name,
                            "provider": entry.provider, "harness": entry.harness,
                            "id": None,
                            "reason": "claude-pane-pid-unconfirmed",
                        }
                    )
                    continue
                pid_state = (
                    _pid_alive(probe_pid, probe_start)
                    if pane_state is True and probe_pid is not None
                    else False
                )
                if pid_state is None:
                    errors.append(
                        {
                            "name": entry.name,
                            "provider": entry.provider, "harness": entry.harness,
                            "id": None,
                            "reason": "claude-process-incarnation-unavailable",
                        }
                    )
                    continue
                if pid_state is True:
                    # The pane is up. Leave the status alone: a row still owed
                    # its restamp is correctly `spawning`, and one that already
                    # got it was promoted to `live` by the restamp itself. Either
                    # way this pass has nothing to correct.
                    continue
                new_status = "orphaned"

            elif not claude_path_present:
                # Mirror the codex-index-missing pattern: when claude is
                # not installed we cannot probe reachability, so we route
                # the entry to `errors` with status untouched. Anything
                # else would mass-flip every claude row to orphaned on a
                # host where claude was removed mid-day.
                errors.append(
                    {
                        "name": entry.name,
                        "provider": entry.provider, "harness": entry.harness,
                        "id": entry.short_id,
                        "reason": "claude-cli-not-on-path",
                    }
                )
                continue
            elif not entry.short_id:
                events.emit(
                    "agent_inconsistent",
                    name=entry.name,
                    provider="claude",
                )
                errors.append(
                    {
                        "name": entry.name,
                        "provider": entry.provider, "harness": entry.harness,
                        "id": None,
                        "reason": "missing-claude-short-id",
                    }
                )
                continue
            else:
                from fno.agents.harnesses import claude as claude_mod

                # Phase 5: MCP-backed claude agents probe via the sidecar
                # instead of `claude logs`. Same tri-state contract:
                # True/False/raise. Socket-only agents (mcp_channel_id is
                # None) keep the legacy claude_logs_reachable path.
                # NOTE: probe_label is assigned BEFORE the probe call so a
                # ReachabilityProbeError from the probe still has the
                # label in scope for the error route.
                probe_label = (
                    "claude-mcp-probe-failed" if entry.mcp_channel_id else "claude-probe-failed"
                )
                try:
                    if entry.mcp_channel_id:
                        reachable = claude_mod.mcp_channel_reachable(entry.mcp_channel_id, timeout=0.25)
                    else:
                        reachable = claude_mod.claude_logs_reachable(
                            entry.short_id, timeout=claude_logs_timeout
                        )
                except ReachabilityProbeError as exc:
                    # Catch the lifted base class (US4-gemini Wave 1.1) so
                    # both the claude-side timeout/OSError probe error and the
                    # Phase 5 ``mcp_channel_disconnected`` probe error are routed
                    # identically. Probe inconclusive -> preserve status,
                    # route to errors with a per-provider reason
                    # discriminator. Mirrors the codex-side
                    # codex-session-index-unreadable treatment so transient
                    # CLI slowness or sidecar I/O hiccups don't mass-orphan
                    # healthy agents (Codex P1 round-5 on PR #315).
                    events.emit(
                        "agent_inconsistent",
                        name=entry.name,
                        provider="claude",
                        reason=exc.reason,
                    )
                    errors.append(
                        {
                            "name": entry.name,
                            "provider": entry.provider, "harness": entry.harness,
                            "id": entry.short_id,
                            "reason": f"{probe_label}: {exc.reason}",
                        }
                    )
                    continue
                new_status = "live" if reachable else "orphaned"

                # US4 heal (x-ec59): a live claude row whose canonical id never landed
                # (the uuid resolution raced at spawn) is unroutable-but-live. Resolve
                # it from claude's own store -- the same jsonl the liveness probe just
                # read -- and fold the write into reconcile's single batched cycle. A
                # miss leaves it null (the durable queue stays the floor); never fatal.
                if reachable and not entry.harness_session_id and entry.short_id:
                    try:
                        healed = claude_mod.resolve_session_uuid(entry.short_id)
                    except Exception:  # noqa: BLE001 — a resolver error is a tolerated miss
                        healed = None
                    if healed:
                        # Queue only. The `backfilled` claim is made AFTER the write,
                        # against the names the write actually stamped: the under-lock
                        # guard can refuse this row (same-name rm + re-register), and
                        # claiming success here reported a heal that never landed.
                        pending_backfill[entry.name] = (entry.short_id, healed)

        else:
            errors.append(
                {
                    "name": entry.name,
                    "provider": entry.provider, "harness": entry.harness,
                    "id": None,
                    "reason": f"unknown-provider-{entry.harness}",
                }
            )
            continue

        if entry.status == new_status:
            continue  # no change; do not write

        # Status drifted — queue the updated entry for the batched
        # single-cycle write at the end of the loop. ``dataclasses.replace``
        # preserves every other field automatically (Gemini medium on
        # PR #317), which is more robust against future AgentEntry
        # schema additions than manual field-by-field reconstruction.
        pending_updates[entry.name] = (entry, new_status)

        change = {
            "name": entry.name,
            "provider": entry.provider, "harness": entry.harness,
            # Codex P2 on PR #317: include gemini_session_id so reconcile
            # records carry an identifier for every provider. Pre-fix
            # gemini agents flipped between live/orphaned with "id": null
            # which rendered as "?" in human output and broke follow-up
            # tooling.
            "id": (entry.short_id or entry.harness_session_id),
        }
        if new_status == "orphaned":
            orphaned.append(change)
        else:
            recovered.append(change)

    # Single atomic write for ALL queued flips (AC3-HP: at most one
    # update_registry call per reconcile). Empty pending_updates
    # short-circuits with no write at all (AC3-UI). On disk-write
    # failure, every queued change moves from orphaned/recovered into
    # errors so the operator sees a single coherent failure rather than
    # a partial split. The all-or-nothing atomicity is enforced by
    # update_registry's own atomic-rename semantics — the closure is
    # pure, so an OSError mid-write leaves the registry untouched.
    if pending_updates or pending_backfill or pending_codex_backfill:

        codex_backfill_applied: set[str] = set()
        status_updates_applied: set[str] = set()
        codex_ids_claimed: set[str] = set()
        claude_backfill_applied: set[str] = set()

        def _apply(current_entries: list[AgentEntry]) -> list[AgentEntry]:
            # Reset per call: update_registry may re-run _apply against a fresh
            # snapshot after lock contention, and carrying a previous attempt's
            # names over would report a row as applied that this attempt skipped.
            codex_backfill_applied.clear()
            status_updates_applied.clear()
            codex_ids_claimed.clear()
            claude_backfill_applied.clear()
            # Build the new entries from the CURRENT (under-lock) entries,
            # overriding only the ``status`` field from pending_updates.
            # Pre-fix this returned ``pending_updates.get(e.name, e)`` which
            # substituted the entire snapshot AgentEntry captured at probe
            # time — silently losing any ``last_message_at`` bump that
            # dispatch_ask wrote during the probe loop (US4-gemini handoff:
            # concurrent reconcile + ask data loss).
            out: list[AgentEntry] = []
            for e in current_entries:
                updates: dict = {}
                if e.name in pending_updates:
                    probed, target_status = pending_updates[e.name]
                    if (
                        e.harness == probed.harness
                        and e.pid == probed.pid
                        and e.pid_start_time == probed.pid_start_time
                        and e.mux == probed.mux
                        and e.harness_session_id == probed.harness_session_id
                        and e.short_id == probed.short_id
                        and e.status == probed.status
                    ):
                        updates["status"] = target_status
                        status_updates_applied.add(e.name)
                if e.name in pending_backfill:
                    probed_short, hsid = pending_backfill[e.name]
                    # Only stamp a row that STILL matches the row we probed: a
                    # same-name rm+re-register during the probe loop would put this
                    # claude uuid onto a replacement row (misrouting its mail).
                    if e.harness == "claude" and e.short_id == probed_short:
                        # Canonical wins: set harness_session_id; the legacy
                        # claude uuid is synced from it on the next load's backfill.
                        updates["harness_session_id"] = hsid
                        updates["harness"] = e.harness
                        claude_backfill_applied.add(e.name)
                if e.name in pending_codex_backfill:
                    (
                        expected_pid,
                        expected_start,
                        probed_mux,
                        resolved_pid,
                        resolved_start,
                        hsid,
                    ) = pending_codex_backfill[e.name]
                    # `codex_ids_claimed` carries ids stamped EARLIER IN THIS APPLY,
                    # which `current_entries` cannot show because it is a consistent
                    # pre-update snapshot. Without it two rows racing for one pane
                    # both read "nobody owns this id" and both take it.
                    duplicate = hsid in codex_ids_claimed or any(
                        other.name != e.name
                        and other.harness_session_id == hsid
                        for other in current_entries
                    )
                    # ONE guard, branch inside. These were two near-identical eight
                    # conjunct conditions differing only in `duplicate`; a term added
                    # to one and not the other would silently drop the update and
                    # report it as a race.
                    if (
                        e.harness == "codex"
                        and e.pid == expected_pid
                        and e.pid_start_time == expected_start
                        and e.mux == probed_mux
                        and not e.harness_session_id
                        and e.status not in _TERMINAL_AGENT_STATUSES
                    ):
                        if duplicate:
                            updates["status"] = "spawning"
                        else:
                            updates["harness_session_id"] = hsid
                            updates["status"] = "live"
                            updates["pid"] = resolved_pid
                            updates["pid_start_time"] = resolved_start
                            codex_ids_claimed.add(hsid)
                            codex_backfill_applied.add(e.name)
                out.append(replace(e, **updates) if updates else e)
            return out

        try:
            update_registry(_apply)
        except (OSError, RegistryVersionError) as exc:
            # Re-classify every queued change as a write failure. Move
            # them out of orphaned/recovered into errors so callers don't
            # see a recovered/orphaned record that never actually committed.
            # A backfill that never committed must not claim it healed either.
            write_error = f"registry-write-failed: {exc}"
            failed_names = set(pending_updates.keys()) | set(pending_codex_backfill.keys())
            for change in list(orphaned):
                if change["name"] in failed_names:
                    orphaned.remove(change)
                    errors.append({**change, "reason": write_error})
            for change in list(recovered):
                if change["name"] in failed_names:
                    recovered.remove(change)
                    errors.append({**change, "reason": write_error})
            for change in list(backfilled):
                backfilled.remove(change)
                errors.append({**change, "id": None, "reason": write_error})
            for name in pending_backfill:
                errors.append(
                    {
                        "name": name,
                        "provider": _vendor_for_name(name), "harness": _harness_for_name(name),
                        "id": None,
                        "reason": write_error,
                    }
                )
            for name in pending_codex_backfill:
                errors.append(
                    {
                        "name": name,
                        "provider": _vendor_for_name(name), "harness": _harness_for_name(name),
                        "id": None,
                        "reason": write_error,
                    }
                )
        else:
            for name, (_probed_short, hsid) in pending_backfill.items():
                if name in claude_backfill_applied:
                    backfilled.append(
                        {
                            "name": name,
                            "provider": _vendor_for_name(name), "harness": _harness_for_name(name),
                            "harness_session_id": hsid,
                        }
                    )
                else:
                    errors.append(
                        {
                            "name": name,
                            "provider": _vendor_for_name(name), "harness": _harness_for_name(name),
                            "id": None,
                            "reason": "claude-session-id-backfill-raced",
                        }
                    )
            for name, (_epid, _estart, _mux, _pid, _start, hsid) in pending_codex_backfill.items():
                if name in codex_backfill_applied:
                    backfilled.append(
                        {
                            "name": name,
                            "provider": _vendor_for_name(name), "harness": _harness_for_name(name),
                            "harness_session_id": hsid,
                        }
                    )
                else:
                    errors.append(
                        {
                            "name": name,
                            "provider": _vendor_for_name(name), "harness": _harness_for_name(name),
                            "id": None,
                            "reason": "codex-session-id-backfill-raced",
                        }
                    )
            raced_updates = set(pending_updates) - status_updates_applied
            for change in list(orphaned):
                if change["name"] in raced_updates:
                    orphaned.remove(change)
            for change in list(recovered):
                if change["name"] in raced_updates:
                    recovered.remove(change)
            for name in raced_updates:
                probed, _target = pending_updates[name]
                errors.append(
                    {
                        "name": name,
                        "provider": probed.provider, "harness": probed.harness,
                        "id": probed.short_id or probed.harness_session_id,
                        "reason": "registry-status-update-raced",
                    }
                )

    events.emit(
        "reconcile_done",
        scanned=len(entries),
        orphaned=len(orphaned),
        recovered=len(recovered),
        skipped=len(skipped),
        errors=len(errors),
        backfilled=len(backfilled),
    )
    return ReconcileResult(
        scanned=len(entries),
        orphaned=orphaned,
        recovered=recovered,
        skipped=skipped,
        errors=errors,
        backfilled=backfilled,
    )


def attach_agent(name: str) -> AttachResult:
    """Interactive attach to a running agent session (claude only).

    claude: shells out to ``claude attach <short_id>`` with inherited
    stdio. The claude TUI takes over the terminal until the operator
    detaches. fno's exit code mirrors claude's.

    codex / gemini: exit 13 with a message pointing at Phase 6 (the
    future fno-owned supervisor) as the planned landing for cross-
    provider attach (Locked Decision 13).

    NO per-agent flock is acquired (Locked Decision 8b): attach holds
    the terminal for indefinite human time and locking would deadlock
    every concurrent stop / rm / ask. claude's own supervisor handles
    concurrent attach safety natively.
    """
    _validate_lifecycle_name(name)
    # Resolve to the ENTRY, not just the canonical name: when the harness-store
    # heal (x-9cc5) synthesizes a row it could not persist, re-reading the
    # registry by name would miss it and report not-found - defeating the
    # best-effort recovery in exactly the registry-unwritable case it exists for.
    # A genuine miss falls back to today's exact-name lookup, preserving the
    # familiar not-found/exit-2 contract. Ambiguous or unavailable identity
    # evidence must refuse before any attach side effect.
    from fno.agents.registry import AgentResolutionError, resolve_agent

    try:
        resolved = resolve_agent(name)
    except AgentResolutionError as exc:
        if exc.ambiguous or exc.unavailable:
            raise DispatchAskError(
                str(exc),
                exit_code=12 if exc.unavailable else 2,
            ) from exc
        existing = _resolve_registry_entry(name)
    except (OSError, RegistryVersionError) as exc:
        raise DispatchAskError(
            f"registry read failed: {exc}",
            exit_code=12,
        ) from exc
    else:
        existing, name = resolved.entry, resolved.entry.name

    if existing.harness in ("codex", "gemini"):
        sys.stderr.write(
            f"{existing.harness} agents are one-shot; no persistent "
            "session to attach to. Use 'fno agents logs "
            f"{name} --follow' for live output. Cross-provider attach is "
            "planned for the Phase 6 supervisor.\n"
        )
        # Forensic event so an `events.jsonl` audit can correlate
        # "why did this attach attempt fail" against operator activity.
        # (Sigma-review C4 finding: silent on the refused path before.)
        events.emit(
            "agent_attach_refused",
            name=name,
            provider=existing.harness,
            reason="one-shot-provider-no-persistent-session",
        )
        return AttachResult(name=name, provider=existing.harness, exit_code=13)

    if existing.harness != "claude":
        raise DispatchAskError(
            f"attach for provider {existing.harness!r} is not implemented",
            exit_code=2,
        )

    short_id = existing.short_id
    if not short_id:
        raise DispatchAskError(
            f"registry entry {name!r} has no short id on file; cannot attach.",
            exit_code=12,
        )

    if not is_provider_available("claude"):
        raise DispatchAskError("claude CLI not on PATH", exit_code=14)

    from fno.agents.harnesses import claude as claude_mod

    try:
        exit_code = claude_mod.claude_attach(short_id)
    except FileNotFoundError as exc:
        raise DispatchAskError("claude CLI not on PATH", exit_code=14) from exc
    except OSError as exc:
        # PermissionError / EIO / other subprocess errors should surface
        # as a clean DispatchAskError, not a raw Python traceback to the
        # operator's terminal (sigma-review H5 finding).
        events.emit(
            "agent_attached",
            name=name,
            provider="claude",
            short_id=short_id,
            claude_exit=None,
            error=str(exc),
            error_type=type(exc).__name__,
        )
        raise DispatchAskError(f"claude attach failed: {exc}", exit_code=1) from exc

    events.emit(
        "agent_attached",
        name=name,
        provider="claude",
        short_id=short_id,
        claude_exit=exit_code,
    )
    return AttachResult(name=name, provider="claude", exit_code=exit_code)


# =====================================================================
# Phase 5 (US6) — register_mcp_channel write verb
# =====================================================================
#
# Locked Decision 11 says channel registration happens at session-create
# time only. ``register_mcp_channel(name)`` is the write verb the create
# path calls (after a successful bg-claude spawn but BEFORE the user
# sees a "ready" signal) to assign an mcp_channel_id to the AgentEntry.
#
# The write uses ``with_agent_lock_and_entry`` so the entry is read
# under the per-agent flock AND the registry-wide flock; concurrent
# create-or-ask calls against the same name therefore serialize on the
# per-agent lock and the rename is atomic.
#
# Design note: ``mcp_channel_id`` currently equals the claude jobId (``short_id``)
# (1:1 mapping; see harnesses/claude.py module-level note). The value
# is generated here at registration time so a future UUIDv4 swap is a
# one-line change.


def register_mcp_channel(
    name: str,
    *,
    registry_path: Optional[Path] = None,
) -> str:
    """Assign an ``mcp_channel_id`` to an existing claude agent.

    Idempotent on the server side: calling twice for the same name
    returns the existing ``mcp_channel_id`` without allocating a fresh
    one (per spec invariant "registration is idempotent on the server
    side").

    Args:
        name: agent name (must already exist in the registry).
        registry_path: optional override forwarded to the lock + read.

    Returns:
        The assigned ``mcp_channel_id`` (today this equals the agent's
        ``short_id``; in a follow-up it will be a UUIDv4
        generated here).

    Raises:
        DispatchAskError(exit_code=2): agent name not found, or entry
            has no ``short_id`` (cannot generate an mcp id for
            a non-Claude or pre-create entry).
    """
    with with_agent_lock_and_entry(name, registry_path=registry_path) as (
        _lock_handle,
        entry,
    ):
        if entry.harness != "claude":
            raise DispatchAskError(
                f"register_mcp_channel: agent {name!r} provider is "
                f"{entry.harness!r}; MCP channel backend is Claude-only "
                "this release",
                exit_code=2,
            )
        if not entry.short_id:
            raise DispatchAskError(
                f"register_mcp_channel: agent {name!r} has no "
                "short id on file; cannot derive mcp_channel_id",
                exit_code=12,
            )
        # Idempotent: if already set, return the existing value.
        if entry.mcp_channel_id:
            events.emit(
                events.KIND_MCP_CHANNEL_REGISTERED,
                name=name,
                short_id=entry.short_id,
                mcp_channel_id=entry.mcp_channel_id,
                idempotent=True,
            )
            return entry.mcp_channel_id

        # Today the mcp_channel_id IS the claude jobId in short_id (1:1; see
        # harnesses/claude.py module note). A follow-up will swap in
        # uuid.uuid4().hex here without a schema change.
        new_id = entry.short_id

        from dataclasses import replace

        def _set_mcp_id(entries: list[AgentEntry]) -> list[AgentEntry]:
            out: list[AgentEntry] = []
            for e in entries:
                if e.name == name:
                    out.append(replace(e, mcp_channel_id=new_id))
                else:
                    out.append(e)
            return out

        try:
            update_registry(_set_mcp_id, path=registry_path)
        except (OSError, RegistryVersionError) as exc:
            # Spec AC1-ROLLBACK: callers who already spawned bg-claude
            # need a single exception class to match so they can SIGTERM
            # the PGID and clean up. Surfacing the raw OSError directly
            # would force every caller to handle two exception shapes.
            events.emit(
                "mcp_channel_register_failed",
                name=name,
                short_id=entry.short_id,
                error=str(exc),
                error_type=type(exc).__name__,
            )
            raise DispatchAskError(
                f"register_mcp_channel: registry write failed for "
                f"{name!r}: {exc}. The agent's bg-claude spawn (if any) "
                "may need to be SIGTERM'd by the caller (AC1-ROLLBACK).",
                exit_code=12,
            ) from exc
        events.emit(
            events.KIND_MCP_CHANNEL_REGISTERED,
            name=name,
            short_id=entry.short_id,
            mcp_channel_id=new_id,
            idempotent=False,
        )
        return new_id


# ---------------------------------------------------------------------------
# G2 Task 2.1 — send verb (async, durable-first)
# ---------------------------------------------------------------------------

#: Body size cap enforced before any envelope write (AC3-EDGE).
_SEND_MAX_BODY_BYTES = 1024 * 1024  # 1 MiB


@dataclass
class DispatchSendResult:
    """Return shape for :func:`dispatch_send`.

    ``msg_id``   The envelope id written to the store (``msg-<8hex>``).
    ``delivery`` ``"hosted"`` if live socket/MCP delivery succeeded;
                 ``"durable"`` if the peer was offline, non-claude, or
                 injection failed and the message was queued durable.
    """

    msg_id: str
    delivery: str  # "hosted" | "durable"
    # The live lane's own cause when delivery demoted to durable (node x-1904):
    # the claude control.sock vocabulary (not-confirmed / attach-failed / ...),
    # a codex RPC reason, or a mux token. None when no live attempt ran (the
    # recipient was asleep, so durable was written upfront with no live miss).
    reason: Optional[str] = None
    # Set by the --to-project anycast path (resolve_to_project): the registry
    # name the project resolved to (when one live peer), and the destination
    # project (for the durable-queue and resolved-recipient stdout lines).
    recipient: Optional[str] = None
    to_project: Optional[str] = None


def _daemon_rpc(
    method: str,
    params: dict,
    *,
    connect_timeout: float = 3.0,
    read_timeout: float = 5.0,
) -> Optional[dict]:
    """Send one JSON-RPC request to the daemon and return the result dict.

    Uses the 4-byte little-endian u32 length-prefix framing defined in
    crates/fno-agents/src/protocol.rs:

        <u32 LE length> <UTF-8 JSON>

    The daemon socket is resolved exactly as the Rust client does: read
    ``FNO_AGENTS_HOME`` env var; if absent, use ``$HOME/.fno/agents/``;
    the supervisor socket is ``supervisor.sock`` inside that directory.

    Returns the ``result`` field dict on success; returns None on any
    transport error (socket absent / refused / timeout) or when the daemon
    returns an ``error`` response.  NEVER raises (callers demote to durable
    on any falsy return).

    Exactly one attempt, no retry.
    """
    import json
    import os
    import socket
    import struct

    # Resolve the supervisor socket path using the same env-var logic as Rust.
    agents_home = os.environ.get("FNO_AGENTS_HOME")
    if agents_home:
        sock_path = Path(agents_home) / "supervisor.sock"
    else:
        home = Path(os.path.expanduser("~"))
        sock_path = home / ".fno" / "agents" / "supervisor.sock"

    # Frame the request.
    req_id = 1
    payload = json.dumps(
        {"id": req_id, "method": method, "params": params},
        ensure_ascii=True,
        sort_keys=False,
    ).encode("utf-8")
    frame = struct.pack("<I", len(payload)) + payload

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.settimeout(connect_timeout)
        try:
            sock.connect(str(sock_path))
        except (FileNotFoundError, ConnectionRefusedError, OSError):
            print(
                "fno-agents daemon unreachable; message queued durable",
                file=sys.stderr,
            )
            return None

        sock.settimeout(read_timeout)
        sock.sendall(frame)

        # Read the 4-byte length prefix.
        header = b""
        while len(header) < 4:
            chunk = sock.recv(4 - len(header))
            if not chunk:
                print("daemon closed connection unexpectedly", file=sys.stderr)
                return None
            header += chunk
        (length,) = struct.unpack_from("<I", header)

        # Guard against absurd lengths (mirrors protocol.rs MAX_FRAME_BYTES).
        if length > 16 * 1024 * 1024:
            print(f"daemon returned oversized frame ({length} bytes)", file=sys.stderr)
            return None

        # Read the JSON body.
        data = b""
        while len(data) < length:
            chunk = sock.recv(length - len(data))
            if not chunk:
                print("daemon closed connection mid-frame", file=sys.stderr)
                return None
            data += chunk

        resp = json.loads(data.decode("utf-8"))
        if not isinstance(resp, dict):
            print(
                "daemon returned invalid JSON-RPC response shape",
                file=sys.stderr,
            )
            return None
        if "error" in resp:
            err = resp["error"]
            print(
                f"daemon RPC error: {err.get('message', err)}",
                file=sys.stderr,
            )
            return None
        return resp.get("result")

    except (OSError, ValueError) as exc:
        # ValueError covers json.JSONDecodeError / UnicodeDecodeError from a
        # malformed daemon response; the docstring contract is NEVER raise.
        print(f"daemon socket error: {exc}", file=sys.stderr)
        return None
    finally:
        sock.close()


# read_timeout exceeds the daemon's per-turn ceiling
# (SWITCHBOARD_TURN_TIMEOUT_MS=120s) plus its 5s grace, so a real reply is never
# cut short and the client never abandons a turn the daemon is still driving.
# The detached background relay continuation (_relay_worker_loop, x-1f23) runs
# off-thread and wants the full ceiling for its own reason: a genuine multi-hop
# stream exchange. The first hop needs the same number for the reason below.
_SWITCHBOARD_READ_TIMEOUT = 130.0
# The FIRST hop's read budget. SYNCHRONOUS -- the sender blocks on it -- so a
# tighter value than the relay's is tempting, and a 15s one shipped here. It is
# unsafe, and the ceiling is not ours to pick: `agent.switchboard_v2` does not
# ack and return. `handle_switchboard` drives B's WHOLE turn before it answers,
# bounded by SWITCHBOARD_TURN_TIMEOUT_MS (120s) + SWITCHBOARD_DRIVE_GRACE_S (5s).
# Reading for less than 125s abandons a turn the daemon is still driving: the
# body already reached B, `_switchboard_exchange` returns None, and
# `_deliver_live` falls through and injects the SAME body a second time under a
# receipt that says durable. Most real model turns run past 15s, so that was the
# common case, not the edge.
#
# Capping the daemon instead (`timeout_ms` in params) is worse: a drive that
# misses its deadline stamps B orphaned, so a short cap marks every slow-but-
# healthy peer dead.
#
# The complaint behind the 15s is real -- a wedged daemon held a sender for the
# full budget, twice killed by hand at 90s and 120s. The fix belongs on the
# daemon side (ack first, drive after), not in a client read cap. Until that
# lands, the sender pays the daemon's own ceiling. The common non-stream case
# still fast-fails in about a second: every pre-drive check is bounded, the
# stream probe at STREAM_PROBE_TIMEOUT_S = 2s.
_SWITCHBOARD_FIRST_HOP_READ_TIMEOUT = _SWITCHBOARD_READ_TIMEOUT
# A SHORT connect timeout: every claude send now tries the switchboard first, so
# a DOWN/wedged daemon must not tax the common (non-stream) path — it should fail
# the connect fast and demote, rather than burn the 3s default before the
# existing MCP/socket path runs.
_SWITCHBOARD_CONNECT_TIMEOUT = 1.0
# A pre-identity daemon rejects this verb before it can act on the message body.
_SWITCHBOARD_RPC_METHOD = "agent.switchboard_v2"


def _load_a2a_settings() -> tuple[bool, int]:
    """Read ``(auto, turn_ceiling)`` from ``config.agents.a2a``.

    A failed / malformed read degrades SAFELY to OBSERVED mode (``auto=False``)
    so a broken settings file never starts an autonomous A<->B relay. The
    ceiling still applies and stays positive.
    """
    try:
        from fno.config import load_settings

        a2a = load_settings().agents.a2a
        return bool(a2a.auto), max(1, int(a2a.turn_ceiling))
    except Exception:
        return (False, 6)


def _wrap_relay_body(cur: str, ctx: "Optional[_MailCtx]") -> str:
    """Wrap a relay hop body in the peer's ``<fno_mail>`` envelope, or return it
    raw when no context is supplied (an unwrapped hop) (node x-1f23). The stream-json
    switchboard injects a whole turn, so this uses the paired multiline form, not
    the relay single-line PTY variant."""
    if ctx is None:
        return cur
    from fno.mail.envelope import wrap_fno_mail

    return wrap_fno_mail(
        cur,
        from_=ctx.from_,
        harness=ctx.harness,
        model=ctx.model,
        node=ctx.node,
        to=ctx.to,
    )


def _emit_relay_stopped(
    target: str,
    peer: str,
    turns_completed: int,
    reason: str,
    *,
    error: Optional[BaseException | str] = None,
) -> None:
    data: dict[str, object] = {
        "target": target,
        "peer": peer,
        "turn": turns_completed + 1,
        "turns_completed": turns_completed,
        "reason": reason,
    }
    if error is not None:
        data["error"] = str(error)
        if isinstance(error, BaseException):
            data["error_type"] = type(error).__name__
    _emit_ev(events.KIND_AGENT_RELAY_STOPPED, **data)


def _run_relay_loop(
    to_name: str,
    from_name: str,
    seed: str,
    ceiling: int,
    mail_ctxs: "Optional[dict[str, _MailCtx]]" = None,
    *,
    recipient_identities: "Mapping[str, SwitchboardIdentity]",
) -> int:
    """Drive the bounded A2A relay AFTER the first hop (B already replied
    ``seed``). Alternate driving A then B with each other's reply — the drive IS
    the literal injection into the target — up to ``ceiling`` total turns
    (counting the first hop), stopping with a visible "loop ceiling reached". A
    side that is not a live stream thread ends the relay.

    Returns the total number of turns driven (counting the caller's first hop),
    so a synchronous driver can report the terminal state.
    Existing callers (:func:`_kickoff_background_relay`, the inline fallback)
    ignore the return, so this is additive.

    Pure orchestration over ``_daemon_rpc`` (no forking here), so it is callable
    both inline (tests, fork-unavailable fallback) and from the detached
    background process kicked off by :func:`_kickoff_background_relay`.
    """
    cur = seed
    target, peer = from_name, to_name  # next: drive A (from) with B's reply
    turns = 1  # the first hop (drive B) already happened in the caller
    while turns < ceiling and cur.strip():
        target_identity = recipient_identities.get(target)
        if target_identity is None:
            _emit_relay_stopped(target, peer, turns, "recipient-identity-missing")
            break
        try:
            hop = _daemon_rpc(
                _SWITCHBOARD_RPC_METHOD,
                {
                    "to": target,
                    "from": peer,
                    # Wrap each continuation in the sending peer's <fno_mail> so the
                    # relay turn carries provenance, not just the seed (node x-1f23).
                    "body": _wrap_relay_body(cur, (mail_ctxs or {}).get(peer)),
                    "mirror": False,
                    "recipient_identity": target_identity,
                },
                connect_timeout=_SWITCHBOARD_CONNECT_TIMEOUT,
                read_timeout=_SWITCHBOARD_READ_TIMEOUT,
            )
        except Exception as exc:
            _emit_relay_stopped(target, peer, turns, "relay-hop-error", error=exc)
            break
        if (
            not isinstance(hop, dict)
            or hop.get("delivered") is not True
            or hop.get("identity_verified") is not True
        ):
            if not isinstance(hop, dict):
                reason = "invalid-response"
            elif hop.get("delivered") is not True:
                reason = str(hop.get("reason") or "not-delivered")
            else:
                reason = "identity-unverified"
            error = hop.get("error") if isinstance(hop, dict) else None
            _emit_relay_stopped(target, peer, turns, reason, error=error)
            break
        turns += 1
        cur = hop.get("reply") or ""
        target, peer = peer, target
    if turns >= ceiling:
        print(
            f"fno-agents switchboard: loop ceiling reached ({ceiling} turns)",
            file=sys.stderr,
        )
    return turns


def _detach_stdio() -> bool:
    """Detach every standard fd, returning false if any still stays inherited."""
    import os

    try:
        devnull = os.open(os.devnull, os.O_RDWR)
    except OSError:
        return False
    detached = True
    for fd in (0, 1, 2):
        try:
            os.dup2(devnull, fd)
        except OSError:
            detached = False
    if devnull > 2:
        try:
            os.close(devnull)  # the dup2'd copies remain; don't leak the original
        except OSError:
            pass
    return detached


def _kickoff_background_relay(
    to_name: str,
    from_name: str,
    seed: str,
    ceiling: int,
    mail_ctxs: "Optional[dict[str, _MailCtx]]" = None,
    *,
    recipient_identities: "Mapping[str, SwitchboardIdentity]",
) -> None:
    """Run the A2A relay in a DETACHED background process so the caller returns
    immediately (ab-3bd520ab).

    The relay is autonomous — no human waits on it — so blocking the
    ``fno agents mail send`` caller for up to ``turn_ceiling × 130s`` was pure
    latency. The send's actual delivery (hop 1: B received the message) already
    happened synchronously in :func:`_switchboard_exchange`; this only continues
    the autonomous A<->B exchange. Double-fork + ``setsid`` so the relay outlives
    the short-lived CLI process and reparents to init (no zombie). A fork failure
    stops only this optional continuation and stays visible; hop one is already
    delivered, so running inline would suppress its receipt and make retry unsafe.
    """
    import os

    try:
        pid = os.fork()
    except OSError as exc:
        _emit_relay_stopped(
            from_name,
            to_name,
            1,
            "relay-detach-failed",
            error=exc,
        )
        return
    if pid > 0:
        # Parent: reap the intermediate child (it exits at once) and return.
        try:
            os.waitpid(pid, 0)
        except OSError:
            pass
        return
    # Intermediate child: detach into a new session, fork the grandchild that
    # actually runs the relay, then exit so the grandchild reparents to init.
    try:
        os.setsid()
        try:
            grandchild = os.fork()
        except OSError as exc:
            _emit_relay_stopped(
                from_name,
                to_name,
                1,
                "relay-detach-failed",
                error=exc,
            )
            return
        if grandchild > 0:
            os._exit(0)
        if not _detach_stdio():
            _emit_relay_stopped(
                from_name,
                to_name,
                1,
                "relay-stdio-detach-failed",
            )
            return
        try:
            _run_relay_loop(
                to_name,
                from_name,
                seed,
                ceiling,
                mail_ctxs,
                recipient_identities=recipient_identities,
            )
        except Exception as exc:
            _emit_relay_stopped(
                from_name,
                to_name,
                1,
                "relay-process-error",
                error=exc,
            )
    finally:
        # _exit (not sys.exit) so the child never runs atexit handlers or flushes
        # the parent's buffers a second time.
        os._exit(0)


_A2A_CONFIRM_TIMEOUT_SECONDS = 5.0
_A2A_CONFIG_LOCK_TIMEOUT_SECONDS = 1.0


def _a2a_first_use_gate(
    auto: bool,
    ceiling: int,
    *,
    confirm_timeout_seconds: float = _A2A_CONFIRM_TIMEOUT_SECONDS,
    config_lock_timeout_seconds: float = _A2A_CONFIG_LOCK_TIMEOUT_SECONDS,
) -> bool:
    """First-use confirm for the autonomous a2a relay (US6, ab-098967b4).

    Returns the EFFECTIVE ``auto`` after gating. Only the autonomous relay
    (``auto=True``) is gated; observed mode (``auto=False``, incl. the
    malformed-config fail-safe) needs no confirm and passes through.

    The first time the relay would fire its first autonomous hop, the user is
    asked once and the answer is persisted (a host marker + the settings value),
    so it never re-asks (AC6-FR). The prompt names the turn ceiling and that the
    relay draws plan credit (AC6-UI).

    Headless / no-TTY (Locked Decision 7 / F4): the relay NEVER inherits
    ``auto:true`` unattended — the conservative fallback (autonomous relay OFF,
    i.e. a single observed hop) applies regardless of the configured default,
    the decision is logged, and the caller is never blocked. The fallback is a
    per-run decision and is NOT persisted, so a later interactive run still asks.
    """
    import os

    # Test seam: relay-logic tests exercise auto=True directly and bypass the
    # confirm. Never set in production.
    if os.environ.get("FNO_A2A_NO_CONFIRM"):
        return auto
    if not auto:
        return False

    from fno import paths

    marker = paths.state_dir() / ".a2a-confirmed"
    if marker.exists():
        return True  # answered once already; honor the persisted setting.

    interactive = sys.stdin.isatty() and sys.stderr.isatty()
    if not interactive:
        sys.stderr.write(
            "fno-agents a2a: no TTY to confirm autonomous relay; applying the "
            "conservative fallback (autonomous relay OFF, single observed hop). "
            "Run `fno config set config.agents.a2a.auto true` to opt in.\n"
        )
        sys.stderr.flush()
        return False

    sys.stderr.write(
        f"\na2a auto-relay is ON: an A<->B send runs up to {ceiling} autonomous "
        "turns, which draws plan credit.\nKeep auto-relay on? [Y/n] "
    )
    sys.stderr.flush()
    try:
        from fno.time_budget import validate_timeout_budget

        validate_timeout_budget(
            confirm_timeout_seconds,
            label="a2a confirmation",
        )
        ready, _, _ = select.select(
            [sys.stdin],
            [],
            [],
            confirm_timeout_seconds,
        )
        if not ready:
            raise TimeoutError
        raw_answer = sys.stdin.readline()
        if raw_answer == "":
            raise EOFError
        answer = raw_answer.strip().lower()
    except Exception:
        sys.stderr.write(
            "\nfno-agents a2a: confirmation timed out or could not be read; "
            "applying the conservative fallback (autonomous relay OFF, single "
            "observed hop).\n"
        )
        sys.stderr.flush()
        return False
    keep_on = answer in ("", "y", "yes")

    try:
        from fno.config.writer import set_config_value

        set_config_value(
            "config.agents.a2a.auto",
            "true" if keep_on else "false",
            scope="global",
            lock_timeout=config_lock_timeout_seconds,
        )
    except Exception as exc:
        sys.stderr.write(
            f"\nfno-agents a2a: could not persist confirmation ({exc}); "
            "applying the conservative fallback (autonomous relay OFF, single "
            "observed hop).\n"
        )
        sys.stderr.flush()
        return False
    try:
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text("answered\n", encoding="utf-8")
    except OSError as exc:
        sys.stderr.write(
            f"\nfno-agents a2a: could not write confirmation marker ({exc}); "
            "applying the conservative fallback (autonomous relay OFF, single "
            "observed hop).\n"
        )
        sys.stderr.flush()
        return False
    return keep_on


def _switchboard_exchange(
    to_name: str,
    from_name: str,
    body: str,
    mail_ctxs: "Optional[dict[str, _MailCtx]]" = None,
    *,
    to_identity: SwitchboardIdentity,
    from_identity: "Optional[SwitchboardIdentity]" = None,
) -> Optional[bool]:
    """Drive a stream-json switchboard exchange (Group 2, Tasks 3.1 + 4.1).

    ``mail_ctxs`` (node x-1f23) maps each endpoint name to its ``<fno_mail>``
    sender context. When set (the mail-send path), every autonomous relay
    continuation is wrapped so later peer turns keep provenance, not just the
    seed. An unwrapped hop passes None, so the raw path stays unchanged.

    Returns ``True`` when the turn(s) were delivered via the switchboard, or
    ``None`` when B is not a live stream thread / the daemon is unreachable (the
    caller then demotes to the MCP/socket path).

    The FIRST hop (drive B with ``body``) is the actual ``send A->B`` delivery and
    runs synchronously so the delivered/demote decision is exact. Its read
    budget is ``_SWITCHBOARD_FIRST_HOP_READ_TIMEOUT``, which must stay above the
    daemon's own drive ceiling; see that constant for why a tighter one delivers
    the message twice. When ``config.agents.a2a.auto`` is True (the default) the
    bounded autonomous relay that follows (drive A with B's reply, then B with
    A's reply, ... up to ``config.agents.a2a.turn_ceiling`` total turns) is
    kicked off in a DETACHED background process and the caller returns
    ``True`` immediately (ab-3bd520ab) — it no longer blocks for up to
    ``turn_ceiling × 130s``, so the relay hop keeps the full ceiling: nothing
    is waiting on it. When ``auto`` is False, a single OBSERVED hop drives B
    and mirrors B's reply into A's view, with no autonomous relay.
    """
    auto, ceiling = _load_a2a_settings()
    # US6 (ab-098967b4): the first-use confirm gates the first autonomous hop.
    # On a no / headless / unconfirmed gate this downgrades to observed mode, so
    # the hop below runs as a single mirrored hop with no autonomous relay.
    auto = _a2a_first_use_gate(auto, ceiling)
    # First hop: drive B. In observed mode (auto off) ask the daemon to mirror
    # B's reply into A's view; in auto mode the relay's next hop injects it (so
    # mirror=False avoids a double-injection).
    mirror = not auto and from_identity is not None
    params: dict[str, object] = {
        "to": to_name,
        "from": from_name,
        "body": body,
        "mirror": mirror,
        "recipient_identity": to_identity,
    }
    if from_identity is not None:
        params["from_identity"] = from_identity
    sb = _daemon_rpc(
        _SWITCHBOARD_RPC_METHOD,
        params,
        connect_timeout=_SWITCHBOARD_CONNECT_TIMEOUT,
        read_timeout=_SWITCHBOARD_FIRST_HOP_READ_TIMEOUT,
    )
    if (
        sb is None
        or sb.get("delivered") is not True
        or sb.get("identity_verified") is not True
    ):
        return None  # not a live stream thread / daemon down -> caller demotes
    if not auto:
        return True  # observed: one hop, B's reply mirrored into A

    # A2A relay: kick off the remaining alternating hops in the background so the
    # caller is not blocked for the whole exchange. A self-send (from == to) or an
    # empty first reply has no relay to run.
    cur = sb.get("reply") or ""
    if (
        ceiling > 1
        and from_name != to_name
        and cur.strip()
        and from_identity is not None
    ):
        _kickoff_background_relay(
            to_name,
            from_name,
            cur,
            ceiling,
            mail_ctxs,
            recipient_identities={
                to_name: to_identity,
                from_name: from_identity,
            },
        )
    return True


# Subprocess budget for the mail-inject verb. It polls the recipient transcript
# for ~10s (40 * 250ms) before reporting not-confirmed; give it headroom.
_MAIL_INJECT_TIMEOUT_S = 20.0

# Liveness-scaled confirm budget (node x-1904, change 2). The enqueue record is
# written at submit time, not at turn end, so a healthy busy recipient confirms
# in well under a second -- the default 10s budget already exists only to cover
# recipients the daemon successfully attached to, i.e. ones already proven
# alive. What it currently does wrong is convert "the CR has not landed yet
# because the recipient is deep in a long tool call" into a live-miss, handing
# the message to a queue nobody drains. When the SENDER's own liveness signal
# (`_registered_family1_state`) independently reports the recipient mid-turn,
# raise the poll budget to ~30s (120 * 250ms) so a long tool call has room to
# yield back to the prompt before we give up; any other recipient keeps the
# unscaled default. A live recipient that still has not enqueued within the
# raised budget is genuinely wedged, a different fact from "busy" (change 4
# reports it as such via the reason token). Not a fixed sleep: the poll still
# exits the instant the enqueue lands.
_MAIL_INJECT_LIVENESS_SCALED_ATTEMPTS = 120
_MAIL_INJECT_LIVENESS_SCALED_TIMEOUT_S = 40.0

# Rust mux pane exit code for a guarded send the server refused because the
# recipient pane's turn is not takeable (crates/fno/src/mux_cli.rs
# EXIT_TARGET_NOT_IDLE). The delivery ladder reads it as turn-not-taken -> a
# stalled demotion to the durable floor, never a hosted receipt (US4, LD4).
_MUX_EXIT_TARGET_NOT_IDLE = 15

# Wake spawns key on the target session uuid, not on a fresh agent name: spawn
# dedup scopes NAME, so two senders waking one session must derive the same name
# to collide on its flock. Prefixed because a bare 8-hex name is refused.
_WAKE_NAME_PREFIX = "wake-"


@dataclass(frozen=True)
class _MailCtx:
    """Sender identity stamped into the ``<fno_mail>`` envelope (node x-1f23)."""

    from_: str
    harness: str
    model: str
    node: Optional[str] = None
    to: Optional[str] = None
    # This message's own bus msg-id (US1). Rendered as the additive `id` attr on
    # both the live inject and the durable fallback so a registered-agent send is
    # reply-correlatable and dedupable like the name-lane path. None on paths that
    # do not carry a minted id (relay hops), keeping the envelope byte-identical.
    id: Optional[str] = None


def _build_mail_ctx(
    from_name: str,
    from_session: Optional[str],
    provider_from: Optional[str],
    to: Optional[str] = None,
    id: Optional[str] = None,
) -> _MailCtx:
    """Build the ``<fno_mail>`` sender context from the dispatch provenance.

    ``from`` is the sender's canonical session handle (or the bare ``from_name`` when
    the caller is unregistered). ``model`` is the invoking session's real model,
    resolved from its own transcript store (x-605c); an unresolvable model floors
    to ``"unknown"`` -- never fabricated.

    ``to`` and ``node`` are OPTIONAL envelope attributes (omitted when None).
    ``to`` is the recipient's short id -- set for a directed ``fno agents mail send`` so
    the recipient can tell a directed turn from a broadcast. ``node`` (the sender's
    backlog node) stays None: dispatch has no truthful source for it today."""
    from fno.agents.self_stamp import resolve_self_model
    from fno.harness_identity import canonical_handle
    from fno.mail.envelope import harness_for_provider

    from_ = canonical_handle(from_session) if from_session else from_name
    return _MailCtx(
        from_=from_,
        harness=harness_for_provider(provider_from),
        model=resolve_self_model(),
        to=to or None,
        id=id or None,
    )


# Poll budget for the mux lane's content confirm (node x-1904, change 3),
# matched to the claude control.sock lane's default (crates/fno-agents/src/
# mail_inject.rs DEFAULT_ATTEMPTS/DEFAULT_INTERVAL_MS): 40 * 250ms = 10s. Kept
# in parity so neither keystroke lane is structurally more patient than the
# other for the same "did the paste land" question.
_MUX_CONFIRM_ATTEMPTS = 40
_MUX_CONFIRM_INTERVAL_S = 0.25


def _mux_recipient_transcript(entry: "AgentEntry") -> Optional[Path]:
    """Locate the mux recipient's OWN claude transcript by session uuid, the
    confirm target for :func:`_mux_pane_send`'s ``confirm`` mode (node x-1904).

    Reuses the resolver `fno.doctor._find_transcript_for` already used for the
    self-diagnostic surface rather than writing a second transcript-by-uuid
    walk (the Rust control.sock lane's own mirror is `find_transcript` in
    `crates/fno-agents/src/claude_drive.rs`). None when the entry carries no
    resolvable full session uuid, or no matching transcript file exists --
    both fail the confirm closed, never open.
    """
    from fno.doctor import _find_transcript_for

    session_id = entry.harness_session_id or entry.session_id
    if not session_id:
        return None
    return _find_transcript_for(session_id)


def _mux_content_confirm(
    transcript: Path,
    marker: str,
    since_byte: int,
    *,
    attempts: int = _MUX_CONFIRM_ATTEMPTS,
    interval_s: float = _MUX_CONFIRM_INTERVAL_S,
) -> bool:
    """Poll ``transcript`` for ``marker`` in lines appended after ``since_byte``
    (node x-1904, change 3): content, not growth, mirroring the claude
    control.sock lane's ``confirm_content_after``/``escaped_marker`` pair
    (``crates/fno-agents/src/mail_inject.rs``) so both keystroke lanes confirm
    delivery the same way. ``marker`` is escaped the same way ``json.dumps``
    would embed it in a JSON string field, since a claude transcript line
    stores the turn's text JSON-encoded -- matching Rust's
    ``serde_json::to_string`` + quote-strip. An empty escaped marker (an empty
    ``text``) never confirms; a submitted turn is recorded verbatim, an unsent
    paste records nothing, so a busy recipient's unrelated transcript growth
    never carries our marker by accident.
    """
    import json

    # ensure_ascii=False to match Rust's `serde_json::to_string`, which leaves
    # non-ASCII literal. With the default the marker's accented or emoji
    # characters become \uXXXX escapes that a claude transcript never carries,
    # and the confirm could never match.
    escaped = json.dumps(marker, ensure_ascii=False)[1:-1]
    if not escaped:
        return False
    needle = escaped.encode("utf-8")
    for _ in range(max(attempts, 1)):
        try:
            with transcript.open("rb") as fh:
                fh.seek(since_byte)
                for raw_line in fh:
                    if needle in raw_line:
                        return True
        except OSError:
            pass
        time.sleep(interval_s)
    return False


def _mux_pane_send(
    entry: "AgentEntry",
    text: str,
    *,
    guarded: bool = True,
    sender: Optional[str] = None,
    confirm: bool = False,
) -> bool:
    """Live-inject to a mux-hosted agent via ``fno mux pane send``.

    When ``guarded``, the paste rides the server-side turn-taken interlock: a
    pane whose recipient is mid-turn refuses with EXIT_TARGET_NOT_IDLE and this
    returns False -- a ``stalled`` demotion to the caller's durable floor --
    rather than swallowing the bytes and letting the sender report ``hosted``
    (Locked Decision 4: hosted-on-bytes-written is banned). A guarded send does
    NOT hold the pane's writer claim: the server guard refuses any pane whose
    claim a live pid holds ("busy: relay"), so holding our own claim would
    self-block every guarded send; the atomic server-side idle check is itself
    the interleave protection for the paste. No caller opts into this branch any
    more (node x-1904): the guard was `rerun_allowed`, borrowed from the rerun
    verb, and a busy recipient enqueues an injected paste rather than corrupting
    a composer (measured, not inferred -- see the doc comment on
    `rerun_allowed` in `crates/fno/src/server.rs`), so refusing before any byte
    was written vetoed exactly the delivery this transport can make. Left in
    place (not deleted) as a real capability of the underlying `fno mux pane
    send --guarded` verb, which the rerun caller still legitimately wants.

    ``guarded=False`` is the raw channel the writer-claim holder owns; it holds
    the claim across the text-then-CR burst so no other writer interleaves. The
    claim is best-effort (an unclaimed pane refuses the acquire; send proceeds),
    but a failed send fails closed -> durable.

    ``confirm`` (node x-1904, mail-delivery default): the mux lane had no
    confirm at all before this -- the busy-veto stood in for one, wrongly,
    since it refused before any byte was written rather than checking whether
    the byte landed. When set, a bytes-written success from the unguarded paste
    is not enough: poll the recipient's OWN transcript for the injected turn's
    content (mirrors the claude control.sock lane's
    ``confirm_content_after``/``escaped_marker`` pair in
    ``crates/fno-agents/src/mail_inject.rs``, so both lanes confirm the same
    way -- content, not growth). No confirmable transcript, or the marker never
    lands within the poll budget, both report False; a confirm that "passes" on
    an unreadable transcript is the false-positive shape the pitfalls corpus
    warns against. Ignored when ``guarded`` (a guarded send's idle check was
    itself standing in for a confirm, and this flag governs the unguarded path
    replacing it), and ignored for a non-claude recipient, which has no
    ~/.claude/projects transcript to confirm against.
    """
    mux = entry.mux or {}
    session = mux.get("session")
    pane_id = mux.get("pane_id")
    if not session or pane_id is None:
        return False
    # x-e21e: the entry IS the row, so the bus-only gate reads it directly --
    # a bus-only recipient never gets a pane paste, same as the control.sock
    # and codex lanes.
    if _delivery_policy_refusal(entry) == BUS_ONLY_POLICY:
        return False
    from fno.agents.harness_map import capabilities

    harness = getattr(entry, "harness", "") or ""
    input_caps = capabilities(harness)
    submit_keys = input_caps["submit_keys"]
    if submit_keys == ["unsupported"]:
        # Name the TABLE and the KEY, not just the layer that did not run. The
        # old wording ("no pinned submit contract") plus the caller's
        # "[mux-send-failed]" receipt read as a broken transport, and two agents
        # plus an operator spent a night on a codex pane before anyone checked
        # whether something had DECLARED the lane unavailable. The transport was
        # never tried.
        print(
            f"mux pane delivery refused: harness {harness!r} declares "
            f"submit_keys = [\"unsupported\"] in "
            f"cli/src/fno/agents/harness_capabilities.toml, so the pane lane is "
            f"refused before it is tried. This is a capability declaration, not "
            f"a transport failure; the message falls back to the durable queue.",
            file=sys.stderr,
        )
        return False
    submit_bytes = {
        "enter": "\r", "tab": "\t", "left": "\x1b[D", "right": "\x1b[C",
        "up": "\x1b[A", "down": "\x1b[B", "esc": "\x1b",
    }
    try:
        submit_text = [submit_bytes.get(key, key) for key in submit_keys]
        enter_delay_s = input_caps["send_keys_enter_delay_ms"] / 1000
    except (KeyError, TypeError):
        return False
    # Audit floor: an UNWRAPPED payload (neither the <fno_mail> a2a envelope nor
    # the <cross-session-message> peer-follow-up container) leaves no agent-authored
    # marker in the recipient transcript, so record it in the ledger. Both wrapped
    # forms carry their own marker, so excluding only <fno_mail> would log every
    # routine peer follow-up as a false raw-inject. The mux pane lane never reaches
    # the Rust mail-inject binary, so this site is mandatory, not decorative.
    # Emitted AFTER the send with the transport's own answer: an emit-before-send
    # leaves a phantom record asserting an injection that a stalled pane or an
    # absent `fno mux` never performed. Best-effort -- a write failure never
    # breaks or fails the send.
    audit_unwrapped = not text.lstrip().startswith(
        ("<fno_mail", "<cross-session-message")
    )

    def _audit_raw_inject(confirmed: bool) -> None:
        if not audit_unwrapped:
            return
        try:
            from fno.events import agent_raw_inject, append_event

            # Write the CANONICAL {type, source, data} envelope to the SAME log
            # the Rust mail-inject binary uses (~/.fno/agents/events.jsonl). That
            # file is canonical-shape only; the flat {kind, ...} emitter would put
            # a second shape in one file and a consumer reading data.target_session
            # (where schema.yaml says it lives) would silently miss every mux
            # record -- the exact audit gap this event exists to close.
            append_event(
                agent_raw_inject(
                    target_session=getattr(entry, "harness_session_id", "") or "",
                    payload=text[:512],
                    harness=getattr(entry, "harness", "") or "",
                    lane="mux-pane",
                    target_cwd=getattr(entry, "cwd", None),
                    sender=sender,
                    confirmed=confirmed,
                    source="daemon",
                ),
                events.daemon_lifecycle_log(),
                lock_timeout_seconds=2,
            )
        except Exception:
            pass

    fno_bin = os.environ.get("FNO_BIN") or "fno"
    pane = str(pane_id)

    def _run(args: list[str], stdin_text: Optional[str] = None) -> int:
        """Run one ``fno mux pane`` verb; return its exit code (-1 on spawn
        failure). A non-zero code's stderr detail is surfaced, never swallowed."""
        try:
            proc = subprocess.run(
                [fno_bin, "mux", "pane", *args, "--session", str(session)],
                input=stdin_text,
                capture_output=True,
                text=True,
                timeout=_MAIL_INJECT_TIMEOUT_S,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            print(f"fno mux pane {args[0]} failed: {exc}", file=sys.stderr)
            return -1
        if proc.returncode != 0:
            detail = (proc.stderr or "").strip()
            print(
                f"fno mux pane {args[0]} exited {proc.returncode}: {detail}",
                file=sys.stderr,
            )
        return proc.returncode

    def _paste_then_submit() -> bool:
        # PaneSend is bytes; the CR submit waits for the TUI to absorb the paste.
        send_args = ["send", pane, "--stdin"]
        if guarded:
            send_args.append("--guarded")
        rc = _run(send_args, stdin_text=text)
        if rc != 0:
            if rc == _MUX_EXIT_TARGET_NOT_IDLE:
                # Turn not taken: the recipient is mid-turn, so the paste never
                # landed. Name the stall; the caller demotes to the durable floor.
                print(
                    f"mux pane {pane} stalled: recipient turn not taken; demoting to durable",
                    file=sys.stderr,
                )
            return False
        # The CR is unguarded: the guarded paste already proved the pane idle, and
        # guarding the submit could strand a pasted-but-unsent prompt.
        time.sleep(enter_delay_s)
        return all(_run(["send", pane, "--text", key]) == 0 for key in submit_text)

    if guarded:
        sent = _paste_then_submit()
        _audit_raw_inject(sent)
        return sent

    # Baseline BEFORE the paste (not after): the confirm below scans only lines
    # appended past this offset, so it never matches something already in the
    # transcript when we started.
    #
    # The transcript confirm is a CLAUDE-lane capability: the resolver reads
    # ~/.claude/projects only, so a mux-hosted codex/opencode/gemini pane has no
    # transcript to confirm against and every landed paste would report a miss --
    # a false durable demotion, and a duplicate once the recipient drains the
    # durable copy. Those panes keep the bytes-written verdict.
    confirm = confirm and (getattr(entry, "harness", "") or "") == "claude"
    confirm_transcript = _mux_recipient_transcript(entry) if confirm else None
    confirm_baseline: Optional[int] = None
    if confirm_transcript is not None:
        try:
            confirm_baseline = confirm_transcript.stat().st_size
        except OSError:
            confirm_transcript = None

    claimed = _run(["claim", pane, "--pid", str(os.getpid())]) == 0
    try:
        sent = _paste_then_submit()
        if sent and confirm:
            # Bytes-written alone is Locked-Decision-4 banned as a hosted
            # verdict; confirm by content against the recipient's own
            # transcript, never optimistically on an unreadable one.
            # Strip first: a leading newline would make the first line empty,
            # and an empty marker never confirms (a landed paste read as a miss).
            marker = text.strip().split("\n", 1)[0]
            sent = confirm_transcript is not None and confirm_baseline is not None and (
                _mux_content_confirm(confirm_transcript, marker, confirm_baseline)
            )
        _audit_raw_inject(sent)
        return sent
    finally:
        if claimed:
            _run(["release", pane])


def _mux_followup_path(
    *,
    name: str,
    message: str,
    from_name: str,
    existing: "AgentEntry",
    lock_handle,  # type: ignore[no-untyped-def]
) -> DispatchAskResult:
    """Follow-up delivery to a mux-hosted agent (any provider).

    A mux row's PTY is a mux pane, not a provider socket / MCP / worker lane,
    so the legacy provider follow-up paths (which key on short_id /
    codex_session_id / gemini_session_id) cannot reach it and raise exit 12.
    Deliver over PaneSend instead -- the same claim->text->CR->release burst
    _deliver_live uses for live mail. PaneSend is fire-and-forget: there is no
    captured reply, so the result carries an empty reply and a stderr note.

    The body rides the SAME cross-session-message container the socket (claude)
    and PTY (codex/gemini) follow-up paths use, so a peer / nested-agent message
    lands as an attributed peer turn rather than bare operator input (the PTY
    delivery contract in docs/architecture/fno-agents-deliver-gate.md).
    """
    from fno.agents.harnesses.claude import build_cross_session_container
    from fno.mail.envelope import ForgedEnvelopeError

    mux = existing.mux or {}
    ref = f"{mux.get('session')}:{mux.get('pane_id')}"
    _emit_ev(
        "agent_followup_started",
        name=name,
        provider=existing.harness,
        short_id=ref,
    )
    try:
        wrapped = build_cross_session_container(message, from_name)
    except ForgedEnvelopeError as exc:
        raise DispatchAskError(str(exc), exit_code=1) from exc
    # Peer follow-up is the writer-claim holder's own raw channel: it has no
    # durable floor to demote to, so it keeps the unguarded send (the turn-taken
    # interlock is the mail-delivery lane's guarantee, not this one -- US4 scope).
    if not _mux_pane_send(existing, wrapped, guarded=False):
        events.emit(
            "agent_followup_failed",
            stage="mux-send",
            name=name,
            short_id=ref,
            reason="pane-send-failed",
        )
        raise DispatchAskError(
            f"mux pane send to {name!r} failed; the pane may be gone. "
            f"Check 'fno mux ls' or 'fno agents logs {name}'.",
            exit_code=1,
        )
    # Message delivered. Bump registry under the held flock; on OSError the
    # send already landed, so keep the lock and do not retry (AC2-FR parity
    # with the claude follow-up path).
    try:
        update_registry(
            _stamp_status(name, status="live", last_message_at=_utc_now_iso),
        )
    except (OSError, RegistryVersionError) as exc:
        events.emit(
            "agent_followup_failed",
            stage="registry-write",
            name=name,
            short_id=ref,
            error=str(exc),
            error_type=type(exc).__name__,
        )
        lock_handle.detach()
        raise DispatchAskError(
            f"registry write failed: {exc}. NOTE: message was already delivered; do not retry.",
            exit_code=12,
        ) from exc
    _emit_ev(
        "agent_followup_done",
        stage="followup",
        name=name,
        provider=existing.harness,
        short_id=ref,
        reply_chars=0,
        backend="mux",
    )
    print(
        f"delivered to mux pane {ref} (fire-and-forget; no reply captured)",
        file=sys.stderr,
    )
    return DispatchAskResult(kind="followup", short_id=ref, reply="")


def mail_inject_probe(recipient: str) -> tuple[bool, str]:
    """Ask the ``fno-agents mail-inject --probe`` verb whether an injection path to
    ``recipient`` EXISTS, without injecting anything.

    Returns ``(injectable, reason)``. The probe resolves through the same
    ``resolve_target`` the real send uses, so it cannot say yes where the send
    would say no. It answers whether a PATH exists, never whether a turn will
    land: the recipient's prompt line may still refuse a mid-turn paste.

    Degrades to ``(False, "probe-unavailable")`` when the binary is missing or the
    call fails, so a caller gating advice on this never claims a path it could not
    measure.
    """
    import json

    from fno import rust_binary

    binary = rust_binary.resolve_installed_binary()
    if binary is None:
        return False, "probe-unavailable"
    try:
        proc = subprocess.run(
            [str(binary), "mail-inject", "--probe", "--session", recipient],
            capture_output=True,
            text=True,
            timeout=_MAIL_INJECT_TIMEOUT_S,
        )
    except (OSError, subprocess.SubprocessError):
        return False, "probe-unavailable"
    try:
        out = json.loads(proc.stdout.strip())
        return bool(out.get("injectable")), str(out.get("reason") or "unknown")
    except (ValueError, AttributeError):
        return False, "probe-unavailable"


#: The one delivery-policy value that forbids prompt-line injection (x-e21e).
#: A DELIVERY-POLICY fact, never a liveness verdict: a bus-only session may be
#: alive and mid-turn, it just belongs on the durable bus. Kept as a literal
#: (compared everywhere) because the registry field is an open
#: ``Optional[str]``; a second value would graduate to an enum then.
BUS_ONLY_POLICY = "bus-only"


def _hold_lapsed_for(entry) -> bool:
    """True when ``entry``'s ``bus-only`` flag no longer holds mail (x-481e).

    Busy mode arms the flag with a clock (``fno.mail.hold``). A row with no
    clock under any of its addresses is not a busy-mode hold at all - it is a
    policy stamped by ``fno agents register --delivery-policy bus-only``, which
    has no clock by construction - so it never lapses and the refusal stands
    exactly as it did before this function existed. Only a timed hold expires.

    The address list comes from ``hold.addresses``, the same rule the writers
    use, rather than a second copy here. A copy that omitted the canonical
    handle looked correct on claude, where the handle IS the ``short_id``, and
    made this check unable to find a codex hold at all: every writer keys by the
    first-eight, and none of that row's other addresses is the first-eight.

    Pure read. It never mutates the registry, so it cannot deadlock a caller
    already holding the registry lock; the stale flag is tidied by the release
    path and by ``fno agents mail notify-self``.
    """
    try:
        from fno.mail import hold as _hold

        if _hold.read_any(entry) is None:
            return False
        return _hold.lapsed(entry)
    except Exception:  # noqa: BLE001 - the gate never raises, and never lifts a hold it could not read
        return False
    return False


def _delivery_policy_refusal(target) -> Optional[str]:
    """:data:`BUS_ONLY_POLICY` when ``target``'s registry row says its mail
    belongs on the durable bus; ``None`` otherwise (no row, no policy, or an
    unreadable registry).

    The gate every shared injector consults BEFORE any transport call, so the
    no-paste guarantee holds on every reachable lane (name/reply, job, project,
    raw, dispatch, ask, annotate) rather than on whichever lane remembered to
    check. Accepts the target in whatever form the lane holds: a registry
    ``AgentEntry``, or an id/handle token matched against ``harness_session_id``,
    ``short_id``, and ``name``. Unresolvable reads as no-policy -- failing open
    here fails toward today's behavior (live delivery to workers), never toward
    stranding a worker's mail on a registry hiccup.

    Never raises."""
    try:
        if target is None:
            return None
        # Two branches, and the expiry check belongs on BOTH. A caller holding
        # an AgentEntry never reaches the registry loop below, so a self-heal
        # on one branch is decorative on the other (dispatch.py:576, :5741 and
        # :6773 all pass an entry).
        if hasattr(target, "delivery_policy"):
            if getattr(target, "delivery_policy", None) == BUS_ONLY_POLICY:
                return None if _hold_lapsed_for(target) else BUS_ONLY_POLICY
            return None
        entries = load_registry()
    except Exception:  # noqa: BLE001 - a registry read failure never blocks delivery
        return None
    for entry in entries:
        if (
            getattr(entry, "delivery_policy", None) == BUS_ONLY_POLICY
            and target in (entry.harness_session_id, entry.short_id, entry.name)
        ):
            return None if _hold_lapsed_for(entry) else BUS_ONLY_POLICY
    return None


def _mail_inject_claude(
    recipient: str,
    text: str,
    *,
    sender: Optional[str] = None,
    reason_out: Optional[list] = None,
    liveness_scaled: bool = False,
) -> bool:
    """Inject ``text`` into a live claude session over the daemon ``control.sock``
    via the ``fno-agents mail-inject`` verb (G1 substrate, node x-1f23).

    Returns True only when the verb confirms the turn landed in the recipient
    transcript; any miss (binary absent, recipient not on the roster, not
    confirmed within the poll budget) returns False so the caller writes the
    durable fallback.

    ``reason_out`` (node x-1904), when a non-empty list, receives the verb's own
    reason token (not-confirmed / attach-failed / io-error / no-transcript /
    not-injectable / unsafe-text) so a durable demotion receipt can name WHY the
    live lane missed instead of a generic live-miss. It is a side-channel rather
    than a second return value so the many callers and test mocks that read this
    as a plain bool are unaffected. A missing binary, subprocess failure, or
    unparseable stdout names that boundary too, so the receipt never silently
    reverts to a bare live-miss at the Python edge.

    ``liveness_scaled`` (node x-1904, change 2): pass the raised confirm budget
    (``_MAIL_INJECT_LIVENESS_SCALED_ATTEMPTS``) when the caller's OWN liveness
    signal already reports the recipient mid-turn, so a long tool call gets room
    to yield back to the prompt before the confirm gives up. The unscaled
    default otherwise (a recipient we cannot independently prove busy stays on
    the tight budget, converting a wedged send to durable quickly).

    ``sender`` is the invoking session's mail handle, forwarded to the binary's
    audit event. Only the UNWRAPPED lanes need it: a wrapped envelope carries
    its own ``from`` in the transcript, an unwrapped one has nowhere else to
    record who fired it."""
    import json

    from fno import rust_binary

    def _record(reason: str) -> None:
        if reason_out is not None:
            reason_out.append(reason)

    # x-e21e: a bus-only recipient never gets a prompt-line paste, on any lane
    # that routes through this injector. Refused BEFORE the binary, the roster,
    # and the socket: no transport call at all.
    if _delivery_policy_refusal(recipient) == BUS_ONLY_POLICY:
        _record(BUS_ONLY_POLICY)
        return False

    binary = rust_binary.resolve_installed_binary()
    if binary is None:
        _record("no-binary")
        return False
    from fno.agents.harness_map import capabilities

    enter_delay_ms = capabilities("claude")["send_keys_enter_delay_ms"]
    argv = [
        str(binary), "mail-inject", "--session", recipient,
        "--enter-delay-ms", str(enter_delay_ms),
    ]
    if sender:
        argv += ["--sender", sender]
    timeout = _MAIL_INJECT_TIMEOUT_S
    if liveness_scaled:
        argv += ["--attempts", str(_MAIL_INJECT_LIVENESS_SCALED_ATTEMPTS)]
        timeout = _MAIL_INJECT_LIVENESS_SCALED_TIMEOUT_S
    try:
        proc = subprocess.run(
            argv,
            input=text,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError):
        _record("probe-unavailable")
        return False
    try:
        out = json.loads(proc.stdout.strip())
        delivered = bool(out.get("delivered"))
        _record(str(out.get("reason") or "unknown"))
        return delivered
    except (ValueError, AttributeError):
        _record("unreadable")
        return False


# Rung-2 (x-eea5 1.1) probe budget: a revived session needs a moment to bind its
# control.sock before the rung-1 inject probe can land. Two short attempts bound
# the wait; a miss falls through to the fork rung, which still delivers the mail.
_RESPAWN_REINJECT_ATTEMPTS = 2
_RESPAWN_REINJECT_DELAY_S = 1.0
# F5: TTL for the rung-2 single-writer guard. Covers the respawn (30s timeout) +
# inject probe window with margin; a crash auto-expires so the claim never strands.
_RESPAWN_GUARD_TTL_MS = 120_000


def _roster_entry_for_session(session_uuid: str) -> Optional["AgentEntry"]:
    """The registry row whose ``harness_session_id`` is ``session_uuid``, or None.

    Best-effort: an unreadable/missing registry returns None so rung 2 degrades
    cleanly to the fork rung rather than blocking mail on registry state.
    """
    try:
        loaded = load_registry()
        if getattr(loaded, "complete", True) is not True:
            raise RegistryVersionError(
                "registry forward read is incomplete; routed wake cannot be classified"
            )
        for entry in loaded:
            if getattr(entry, "harness_session_id", None) == session_uuid:
                return entry
    except OSError:
        return None
    return None


def _respawn_claude_session(short_id: str) -> int:
    """Shell the public ``claude respawn <shortid>`` verb - the identity-
    PRESERVING revival (same uuid, one roster row), the opposite of the
    identity-breaking ``--bg --resume`` fork. Returns the honest subprocess exit
    code; claude absent or a timeout return non-zero so the caller falls through.
    """
    try:
        proc = subprocess.run(
            ["claude", "respawn", short_id],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return 127
    return proc.returncode


def _lineage_seed_prefix(root_uuid: str) -> str:
    """The one-line lineage marker prefixed to a fork's seed prompt (x-eea5 1.2).

    Carries the ROOT uuid durably in the transcript so the fork's own fence can
    resolve its lineage. A registry field was the plan's first choice but is
    NON-durable: the Rust daemon re-serializes registry rows and silently drops
    Python-only fields (state.rs:75), so it would be a lying field - the exact
    anti-pattern this node removes. The transcript is outside that loop.

    ``root_uuid`` is the uuid the fork resumed from. The common mail-wake case
    forks from the original, so the immediate parent IS the root; a fork-of-fork
    carries its immediate parent (a documented tail; deep chains would need root
    resolution across transcripts).
    """
    ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    from fno.harness_identity import canonical_handle

    short_root = canonical_handle(root_uuid or "")
    return (
        f"[lineage: forked from {short_root} at {ts}; "
        f"you are the claim-holding incarnation of {short_root}]"
    )


def _acquire_rung2_guard(session_uuid: str, short: str) -> Optional[str]:
    """Take ``session:<uuid>`` for the rung-2 revive window (F5), or None.

    Two concurrent wakes of one exited session would both respawn and inject the
    same mail (double delivery, competing writers). The single-writer claim
    serializes them: the loser's acquire fails and it falls through to the fork
    rung, which claims/pins exactly as rung 3 does. Pinned to this pid with a TTL
    so a crash auto-expires instead of stranding. Returns the holder to release,
    or None when another incarnation holds the claim."""
    from fno.agents.harnesses.claude import (
        SessionWriterClaimError,
        acquire_session_writer_claim,
    )

    holder = f"revive:{os.getpid()}"
    try:
        acquire_session_writer_claim(
            session_uuid=session_uuid,
            holder=holder,
            claude_short_id=short,
            pid=os.getpid(),
            ttl_ms=_RESPAWN_GUARD_TTL_MS,
        )
        return holder
    except SessionWriterClaimError:
        return None


def _release_rung2_guard(session_uuid: str, holder: Optional[str]) -> None:
    """Release the rung-2 guard claim. Idempotent (silent no-op if not held)."""
    if not holder:
        return
    from fno.agents.harnesses.claude import release_session_writer_claim

    release_session_writer_claim(session_uuid=session_uuid, holder=holder)


def _stamp_revived_live(entry: AgentEntry) -> None:
    """Make a successful identity-preserving respawn countable before admission ends."""
    decline_reason: list[str] = []
    applied = _update_registry_if_recipient_unchanged(
        entry.name,
        _recipient_identity_key(entry),
        _stamp_status(entry.name, status="live", last_message_at_preserve=True),
        decline_reason=decline_reason,
    )
    if not applied:
        reason_phrase = {
            "row_removed": "was removed entirely",
            "duplicate_name": "now has a duplicate name",
        }.get(decline_reason[0] if decline_reason else "", "changed")
        raise RegistryVersionError(
            f"registry row {entry.name!r} {reason_phrase} during identity-preserving respawn"
        )


def _revived_roster_pid(short_id: str) -> Optional[int]:
    """Return the revived worker's positive roster PID when readable."""
    try:
        from fno.agents.harnesses._claude_session_registry import roster_sessions

        for row in roster_sessions():
            if row.get("short_id") != short_id:
                continue
            pid = int(row.get("pid") or 0)
            if pid > 0:
                return pid
    except (OSError, TypeError, ValueError):
        pass
    return None


def wake_and_deliver(
    session_uuid: str, wrapped: str, *, cwd: Optional[Path] = None
) -> tuple[bool, str]:
    """Revive an asleep claude session with ``wrapped`` as its waking prompt.

    Asleep is a resumable state, not voicemail: the mail IS the prompt that
    brings the session back, and it returns as an attachable bg thread rather
    than a one-shot. Returns ``(True, short_id)`` naming the revived thread, or
    ``(False, reason)`` where the reason is a lane-failure token the sender's
    receipt prints verbatim.

    Rides the existing revive-in-place spawn substrate rather than shelling
    ``claude -r`` by hand. ``claude -p`` is never reachable from here -- a
    one-shot cannot host the multi-turn session the recipient resumes into.

    The name is derived from the uuid so concurrent wakes of one session collide
    on the same flock: spawn dedup scopes NAME, so a fresh random name per wake
    would defeat the serialization this depends on. It is prefixed rather than
    bare hex because a bare 8-hex name is refused as an id/name collision. That
    flock plus the same-name collision check is what serializes two senders --
    the second wake finds the first's row live and is refused as
    ``wake-already-in-flight``.

    The uuid-scoped single-writer claim lives in ``_claude_create_path`` (x-7fef),
    not here: it is taken for every resume, pinned to the SPAWNED supervisor's
    pid, and outlives this process. Holding it here instead would pin liveness to
    the short-lived ``fno agents mail send`` process, so the claim would guard only the
    probe->spawn window and go reclaimable the moment this command exits.

    That claim is NOT redundant with the substrate's own fail-safe. That one
    lives inside ``_is_revival``, which runs only when a same-name row ALREADY
    exists -- and a wake derives a fresh name, so the FIRST wake of a session
    would skip it entirely. This rung also fires whenever the inject probe
    returned False, which happens for reasons unrelated to being asleep (the
    runtime binary absent, a subprocess error, an unconfirmed poll budget), so
    the target may well be live.

    A claim is taken rather than a bare liveness probe because a probe is not
    atomic: a daemon adoption or a differently-named ``--resume`` could acquire
    ``session:<uuid>`` between the check and the spawn, and we would start a
    second writer on one transcript anyway.
    """
    if not session_uuid:
        return False, "no-session-uuid"

    from fno.harness_identity import claude_transport_short_id, canonical_handle

    # Rung 2 (x-eea5 1.1): an exited-but-rostered session revives IN PLACE via
    # `claude respawn <shortid>` (identity-preserving: same uuid, one roster
    # row), then the rung-1 inject probe re-runs against the revived session.
    # A respawn miss (claude absent, non-zero) or an inject that still does not
    # land in the probe budget falls through to the fork rung (rung 3) so the
    # mail is never dropped. The x-7fef single-writer claim still guards rung 3.
    try:
        entry = _roster_entry_for_session(session_uuid)
    except (RegistryVersionError, ValueError):
        return False, "registry-incomplete"

    spawn_name = f"{_WAKE_NAME_PREFIX}{canonical_handle(session_uuid)}"
    route_provider = (
        getattr(entry, "provider", None)
        if entry is not None and getattr(entry, "route_settings_path", None)
        else None
    )
    from fno.agents.spawn_gate import GateRefused, run_gate

    gate = None
    revived_reservation = False
    if route_provider is not None:
        try:
            gate = run_gate(spawn_name, "bg", route_provider=route_provider)
        except GateRefused as exc:
            return False, f"spawn-exit-{exc.code}"

    try:
        if entry is not None and getattr(entry, "status", None) == "exited":
            short = (
                getattr(entry, "short_id", None)
                or getattr(entry, "name", "")
                or claude_transport_short_id(session_uuid)
            )
            # F5: take session:<uuid> so two concurrent wakes of one exited session
            # don't both respawn+inject (double delivery / competing writers). Another
            # incarnation holding it -> skip rung 2 and let the fork rung claim/pin.
            guard = _acquire_rung2_guard(session_uuid, short)
            if guard is not None:
                try:
                    if gate is not None:
                        # Establish durable provider evidence BEFORE respawn.
                        # A claim-store failure therefore creates no worker. The
                        # row name makes both global and provider counts dedupe
                        # the reservation once registry+roster evidence catches up.
                        gate.retain_revived_worker(
                            short,
                            worker_name=entry.name,
                            worker_pid=os.getpid(),
                            positive_marker="claude-respawn-pending",
                        )
                        revived_reservation = True
                    if _respawn_claude_session(short) == 0:
                        if gate is not None:
                            # Rebind the already-durable reservation to the
                            # revived process when the roster exposes its PID.
                            gate.retain_revived_worker(
                                short,
                                worker_name=entry.name,
                                worker_pid=_revived_roster_pid(short),
                            )
                        try:
                            _stamp_revived_live(entry)
                        except Exception as stamp_error:
                            from fno.agents.harnesses.claude import claude_stop

                            try:
                                stop_code, stop_detail = claude_stop(short)
                            except Exception as stop_error:
                                if gate is not None:
                                    gate.release_gate_mutex()
                                    gate = None
                                raise RuntimeError(
                                    f"revived worker {short} could not be recorded "
                                    "or stopped; provider reservation retained"
                                ) from stop_error
                            if stop_code != 0 and gate is not None:
                                # The live worker could not be made countable or
                                # stopped. Release only the serialization mutex;
                                # retaining its provider reservation makes later
                                # matching counts refuse instead of failing open.
                                gate.release_gate_mutex()
                                gate = None
                                raise RuntimeError(
                                    f"revived worker {short} could not be recorded "
                                    f"or stopped ({stop_detail}); provider reservation "
                                    "retained"
                                ) from stamp_error
                            if gate is not None:
                                gate.release()
                                gate = None
                                revived_reservation = False
                            raise RuntimeError(
                                f"revived worker {short} was stopped because its live "
                                "registry stamp failed"
                            ) from stamp_error
                        for _attempt in range(_RESPAWN_REINJECT_ATTEMPTS):
                            if _mail_inject_claude(session_uuid, wrapped):
                                return True, short
                            time.sleep(_RESPAWN_REINJECT_DELAY_S)
                        # The respawn already created a live worker. A fork here
                        # would spend this one admission twice; durable mail can
                        # retry delivery without creating another incarnation.
                        return False, "respawn-inject-unconfirmed"
                    if gate is not None and revived_reservation:
                        gate.release_worker_reservation()
                        revived_reservation = False
                finally:
                    try:
                        _release_rung2_guard(session_uuid, guard)
                    except Exception as exc:
                        print(
                            f"rung-2 writer claim release failed for {short}: {exc}",
                            file=sys.stderr,
                        )
                # A failed respawn created no worker, so the fork rung may use
                # the admission that is still held.

        result = dispatch_spawn(
            name=spawn_name,
            message=_lineage_seed_prefix(session_uuid) + "\n" + wrapped,
            provider="claude",
            cwd=cwd or Path.cwd(),
            resume_session_id=session_uuid,
            route_provider=route_provider,
            provider_gate=gate,
        )
        short = (
            getattr(result, "short_id", None)
            or getattr(result, "name", "")
            or "unknown"
        )
        # Rung 3 forked a new incarnation (x-eea5 1.2): make it loud. The receipt
        # names both the new handle and the old lineage, and the seed prompt above
        # carried the lineage prefix. A fork is never silent.
        print(
            f"forked new incarnation {short} from lineage "
            f"{canonical_handle(session_uuid)}",
            file=sys.stderr,
        )
        return True, short
    except GateRefused as exc:
        return False, f"spawn-exit-{exc.code}"
    except DispatchAskError as exc:
        # Exit 11 is the writer claim refusing: another writer holds the
        # transcript, so the session is not actually asleep. Exit 2 is the name
        # collision, which for a uuid-derived name means a concurrent wake won
        # the race. Both are honest "do not wake" answers, and the caller
        # re-probes the now-live session before demoting.
        if exc.exit_code == 11:
            return False, "writer-possibly-live"
        if exc.exit_code == 2:
            return False, "wake-already-in-flight"
        return False, f"spawn-exit-{exc.exit_code}"
    except (OSError, RuntimeError) as exc:
        return False, f"spawn-error-{type(exc).__name__}"
    finally:
        if gate is not None:
            if revived_reservation:
                gate.release_gate_mutex()
            else:
                gate.release()


def wake_drain_agent(
    session_uuid: str, *, cwd: Optional[Path] = None
) -> tuple[bool, str]:
    """Wake an asleep-but-resumable claude session to drain its OWN inbox (US9,
    rung 3 of the inbox-daemon ladder). The inbox daemon calls this for a
    heads-up addressed to a session with no live turn boundary of its own - the
    mail would otherwise pile durable forever, which is the wall this rung
    removes.

    A thin wrapper over ``wake_and_deliver``: waking to drain IS delivering a
    waking prompt, so the concurrency guarantee comes for free. The name is
    derived from the uuid (never the envelope msg-id), so two concurrent wakes
    collide on one flock and the single-writer claim refuses the second - one
    revival, not two writers on one transcript. Rides the revive-in-place
    substrate rather than a one-shot ``claude -p`` because only the persistent
    substrate holds that claim; a headless one-shot could not make concurrent
    wakes collapse. Returns ``wake_and_deliver``'s ``(delivered, reason)``.
    """
    return wake_and_deliver(
        session_uuid,
        "You were woken to drain unread fno agents mail addressed to you. "
        "Run `fno agents mail drain-self` to process it, then stop.",
        cwd=cwd,
    )


def wake_if_asleep_claude(token: str) -> tuple[bool, Optional[str]]:
    """Resolve ``token`` to a resumable-but-asleep claude session and wake it to
    drain its own inbox (US9). Returns ``(True, short_id)`` on a revival, else
    ``(False, None)`` - the token is a project name, a non-claude/ambiguous
    token, or the wake refused (the session is actually live, or another wake is
    in flight). Best-effort: a resolver or spawn error never raises.

    Shared by the send-time heads-up path (``mail send <handle> --kind heads-up``,
    the reachable trigger for a handle-addressed note) and the drain daemon rung.
    ``resolve_reachable`` is liveness-blind, so an asleep session resolves here
    even though it is absent from every live listing.
    """
    from fno.agents import discover as discover_mod

    # A bus-only recipient declines the wake from every caller (the send-time
    # heads-up and the inbox drain daemon rung alike): waking revives a second
    # writer on a session that declared the durable bus its one lane.
    if _delivery_policy_refusal(token) == BUS_ONLY_POLICY:
        return False, None

    try:
        reachable, _ambiguous = discover_mod.resolve_reachable(token)
    except Exception:  # noqa: BLE001 - a resolver failure is not a delivery failure
        return False, None
    if reachable is None or reachable.agent != "claude":
        return False, None
    cwd = Path(reachable.cwd) if reachable.cwd else None
    try:
        delivered, detail = wake_drain_agent(reachable.session_id, cwd=cwd)
    except (OSError, RuntimeError):
        return False, None
    return (True, detail) if delivered else (False, None)


def _mail_inject_codex(
    thread_id: str, text: str, *, reason_out: Optional[list] = None
) -> bool:
    """Inject ``text`` into a live codex session over the app-server daemon socket
    via the ``fno-agents mail-inject --harness codex`` verb (US8, node x-d899).

    ``thread_id`` is the codex threadId (full UUID). Returns True only when the
    daemon accepts the turn; any miss (binary absent, no daemon socket, thread
    not attached) returns False so the caller writes the durable fallback. The
    codex app-server daemon only exists when the user runs it
    (``codex app-server daemon start``); absent it this is a clean no-op.

    ``reason_out`` (x-e21e), when a non-empty list, receives the live lane's
    cause on a miss -- the same side-channel contract as
    :func:`_mail_inject_claude`, so a bus-only refusal names itself in the
    caller's receipt instead of reading as a generic live-miss."""
    import json

    from fno import rust_binary

    # x-e21e: same injector-level gate as the claude lane; see
    # _delivery_policy_refusal.
    if _delivery_policy_refusal(thread_id) == BUS_ONLY_POLICY:
        if reason_out is not None:
            reason_out.append(BUS_ONLY_POLICY)
        return False

    binary = rust_binary.resolve_installed_binary()
    if binary is None:
        return False
    try:
        proc = subprocess.run(
            [str(binary), "mail-inject", "--harness", "codex", "--session", thread_id],
            input=text,
            capture_output=True,
            text=True,
            timeout=_MAIL_INJECT_TIMEOUT_S,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    try:
        return bool(json.loads(proc.stdout.strip()).get("delivered"))
    except (ValueError, AttributeError):
        return False


def _review_start_codex(
    thread_id: str,
    target: str,
    *,
    audit_payload: str | None = None,
    audit_sender: str | None = None,
    audit_target_cwd: str | None = None,
) -> dict[str, object]:
    """Start an inline Codex review and preserve its structured outcome receipt."""
    import json

    from fno import rust_binary

    binary = rust_binary.resolve_installed_binary()
    if binary is None:
        return {"delivered": False, "reason": "binary-not-found"}
    try:
        argv = [
            str(binary),
            "review-start",
            "--session",
            thread_id,
            "--target",
            target,
            "--delivery",
            "inline",
        ]
        for flag, value in (
            ("--audit-payload", audit_payload),
            ("--audit-sender", audit_sender),
            ("--audit-target-cwd", audit_target_cwd),
        ):
            if value is not None:
                argv.extend((flag, value))
        proc = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=_MAIL_INJECT_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        return {"delivered": False, "reason": "not-confirmed"}
    except OSError:
        return {"delivered": False, "reason": "spawn-failed"}
    try:
        receipt = json.loads(proc.stdout.strip())
    except (ValueError, AttributeError):
        # Exit 2 with no stdout is the binary's usage arm: a deployed binary
        # predating this PR's flags rejects the invocation there. Point the
        # operator at the binary, not the daemon.
        if proc.returncode == 2 and not proc.stdout.strip():
            return {"delivered": False, "reason": "stale-binary"}
        return {"delivered": False, "reason": "rpc-error"}
    if not isinstance(receipt, dict):
        return {"delivered": False, "reason": "rpc-error"}
    delivered = receipt.get("delivered")
    if delivered is True:
        turn_id = receipt.get("turn_id")
        review_thread_id = receipt.get("review_thread_id")
        if (
            proc.returncode == 0
            and isinstance(turn_id, str)
            and bool(turn_id)
            and isinstance(review_thread_id, str)
            and bool(review_thread_id)
        ):
            return receipt
        return {"delivered": False, "reason": "not-confirmed"}
    if delivered is False:
        reason = receipt.get("reason")
        if isinstance(reason, str) and reason:
            return receipt
    return {"delivered": False, "reason": "rpc-error"}


def keystroke_lane(entry: "AgentEntry") -> tuple[str, bool]:
    """The live delivery lane for a registry row and whether it is a KEYSTROKE
    lane (a prompt-line path where the REPL's slash parser runs before the
    model), mirroring ``_deliver_live``'s routing order EXACTLY: mux first, then
    harness. A predicate that disagrees with the real router is worse than none.

    A raw slash payload fires a verb only on a keystroke lane. The codex/gemini/
    opencode daemon lanes answer False (``agent.deliver`` / ``turn/start`` submit
    a turn to the model with no TUI prompt line, so the slash never reaches a
    parser); the mux pane paste and claude's ``control.sock`` (the --raw inject
    lane) are keystrokes. The claude answer models the control.sock door ``--raw``
    uses, not every claude sub-lane (a stream-json switchboard peer is a different
    lane this predicate does not classify).
    """
    if entry.mux:
        return ("mux-pane", True)
    if entry.harness != "claude":
        return (f"{entry.harness or 'unknown'}-daemon", False)
    return ("control.sock", True)


def _deliver_live(
    entry: "AgentEntry",
    body: str,
    from_name: str,
    mail: "Optional[_MailCtx]" = None,
    sender_entry: "Optional[AgentEntry]" = None,
    reason_out: "Optional[list]" = None,
    family1_state: Optional[str] = None,
) -> bool:
    """Attempt a single fire-and-forget live delivery (live-inject-first; the
    caller writes the durable fallback when this returns False -- node x-1f23).

    ``reason_out`` (node x-1904), when a non-empty list, receives the live
    lane's own cause (the claude control.sock vocabulary from
    :func:`_mail_inject_claude`, the codex daemon RPC reason, or a mux token) so
    a durable demotion receipt can name WHY the live lane missed instead of a
    generic live-miss. A side-channel, not a second return value, so callers
    and test mocks that read this as a plain bool are unaffected.

    ``family1_state`` (node x-1904, change 2) is the caller's ALREADY-COMPUTED
    :func:`_registered_family1_state` classification for ``entry`` -- passed in
    rather than recomputed here, since ``dispatch_send`` already resolves it
    before calling this function and a second call would re-read the recipient
    transcript for no new information. ``"working"`` (mid-turn, per
    :func:`_registered_family1_state`) scales the claude control.sock confirm
    budget so a long tool call has room to yield before we give up.

    When ``mail`` is set the body is wrapped in the paired ``<fno_mail>`` envelope
    so the recipient sees agent-to-agent structure and the delivered turn is
    self-recording (``grep <fno_mail>`` reconstructs a2a history). Every live
    transport below carries the same wrapped turn.

    For claude peers: the ``control.sock`` inject via the ``fno-agents
    mail-inject`` verb (G1, x-26df) is the live primitive for adopted
    ``claude --bg`` sessions, replacing the dead per-worker messaging socket; the
    switchboard / MCP fast lanes still apply first for stream-json / MCP-routed
    peers.

    For codex/gemini peers: the daemon ``agent.deliver`` RPC, now carrying the
    ``<fno_mail>`` envelope. Daemon-down or any failure demotes to durable with a
    stderr notice; the durable envelope the caller writes is the recovery record.
    """
    wrapped = body
    if mail is not None:
        from fno.mail.envelope import wrap_fno_mail

        wrapped = wrap_fno_mail(
            body,
            from_=mail.from_,
            harness=mail.harness,
            model=mail.model,
            node=mail.node,
            to=mail.to,
            id=mail.id,
        )

    # Dual-run dispatch on the row's live ref (4a-G2): a mux-hosted agent gets
    # PaneSend; worker/bg rows keep the legacy lanes below until G4.
    def _record(reason: str) -> None:
        if reason_out is not None:
            reason_out.append(reason)

    # The bus-only policy bounds EVERY live transport below, not only the three
    # shared injectors: the switchboard and daemon-RPC lanes drive a recipient
    # turn without routing through any of them.
    if _delivery_policy_refusal(entry) == BUS_ONLY_POLICY:
        _record(BUS_ONLY_POLICY)
        return False

    if entry.mux:
        mux_delivered = _mux_pane_send(entry, wrapped, guarded=False, confirm=True)
        if not mux_delivered:
            _record("mux-send-failed")
        return mux_delivered

    # Route key is the canonical harness, legacy provider as fallback (x-ec59):
    # an unknown harness with no inject lane (e.g. opencode) falls through to the
    # daemon deliver RPC by name and demotes to durable cleanly (never a KeyError).
    route_harness = entry.harness
    if route_harness != "claude":
        # Route codex/gemini through the daemon deliver RPC (now <fno_mail>-wrapped).
        result = _daemon_rpc(
            "agent.deliver",
            {
                "name": entry.name,
                "body": wrapped,
                "from_name": from_name,
            },
        )
        if result is None:
            # _daemon_rpc already printed to stderr.
            return False
        if result.get("delivered") is True:
            return True
        # delivered=false: print the demotion reason to stderr.
        reason = str(result.get("reason") or "unknown")
        print(
            f"fno-agents deliver demoted: {reason}; message queued durable",
            file=sys.stderr,
        )
        _record(reason)
        return False

    # Group 2 (Task 3.1): both-endpoints-live switchboard fast lane. When B is a
    # held stream-json thread the daemon drives a turn against it and (the A2A
    # default, Task 4.1 gates it by config) mirrors B's reply back into A. The
    # daemon is authoritative: it probes B's worker socket, so a claude peer that
    # is NOT a live stream thread returns delivered=false / "not-a-live-stream-
    # thread" and we fall through to the MCP/socket path below. This is purely
    # additive — today's behavior is unchanged whenever the lane does not apply
    # (demote, daemon-unreachable=None, or any non-delivered result).
    #
    # The exchange (single observed hop, or the bounded A2A relay when
    # config.agents.a2a.auto is on) is in _switchboard_exchange. It returns True
    # when delivered via the switchboard, or None to demote to the MCP/socket
    # path below (B not a live stream thread, or daemon unreachable).
    # node x-1f23: provenance for the autonomous relay continuations. The sender's
    # ctx wraps A's turns; the recipient's ctx (from/to swapped) wraps B's. None
    # when there is no mail envelope, leaving the relay raw (an unwrapped hop
    # never reaches _deliver_live, unaffected).
    relay_ctxs = None
    if mail is not None:
        from fno.mail.envelope import harness_for_provider

        relay_ctxs = {from_name: mail}
        # Only wrap the recipient's relay turns when it has a resolvable short id;
        # otherwise leave that side raw rather than emit <fno_mail from=""> (codex
        # peer P2). mail.to is the recipient short resolved in dispatch_send.
        if mail.to:
            relay_ctxs[entry.name] = _MailCtx(
                from_=mail.to,
                harness=harness_for_provider(entry.harness),
                model="unknown",
                to=mail.from_,
            )
    if _switchboard_exchange(
        entry.name,
        from_name,
        wrapped,
        relay_ctxs,
        to_identity=_switchboard_identity(entry),
        from_identity=(
            _switchboard_identity(sender_entry) if sender_entry is not None else None
        ),
    ):
        return True

    # Live inject over control.sock (adopted `claude --bg`, the fno-agents
    # mail-inject verb, G1; node x-1f23). This is the SOLE claude live lane: the
    # PTY worker.sock lane retired with daemon PTY hosting (x-f54c, x-3dac), and
    # the redundant MCP-channel fast lane retired here (US5) because it reported
    # hosted on an unconfirmed bytes-written push (Locked Decision 4) while
    # reaching no peer the control.sock lane cannot. The mail-inject verb resolves
    # the handle itself via ClaudeRoster (accepts the full session uuid or 8-hex
    # short id) and confirms transcript growth before reporting delivered.
    #
    # Recipient resolution guarantees no former MCP recipient is stranded:
    # mcp_channel_id is minted 1:1 from short_id by its sole producer
    # (register_mcp_channel), so it IS a roster-resolvable id. Live rows can carry
    # an empty plain `short_id` (x-3dac), so mcp_channel_id is the load-bearing
    # fallback for an MCP-registered row whose short_id field was since cleared.
    recipient = entry.harness_session_id or entry.short_id or entry.mcp_channel_id
    if not recipient:
        _record("no-recipient")
        return False
    return _mail_inject_claude(
        recipient,
        wrapped,
        reason_out=reason_out,
        liveness_scaled=family1_state == "working",
    )


def _registered_family1_state(entry: "AgentEntry") -> str:
    """Classify a registered recipient without trusting its stored lifecycle."""
    from types import SimpleNamespace

    from fno.agents.session_truth import resolve_session_truth

    session_id = (
        entry.harness_session_id
        or entry.cc_session_id
        or entry.session_id
        or entry.short_id
    )
    known = SimpleNamespace(
        agent=entry.harness,
        session_id=session_id,
        cwd=entry.cwd,
    )
    result = resolve_session_truth(
        entry.name, resolve=lambda _handle: (known, [])
    )
    return str(result.get("state") or "unknown")


def _queue_durable_fallback(
    entry: "AgentEntry",
    message: str,
    from_name: str,
    entries: "list[AgentEntry]",
    *,
    msg_id: Optional[str] = None,
    reason: Optional[str] = None,
    mail_ctx: "Optional[_MailCtx]" = None,
) -> "tuple[str, str]":
    """Write the <fno_mail> envelope to the durable bus.

    Returns ``(msg_id, durable_recipient)``. The recipient rides back so a
    caller's receipt line names the handle this actually wrote to, without
    re-deriving it from an Optional field the refusal below already narrowed.

    ``mail_ctx`` is the envelope the caller already built. Pass it whenever one
    exists: rebuilding it here would re-run ``resolve_self_model``, a transcript
    scan, and a live send that then falls back would stamp the durable copy with
    a model resolved separately from the one the live turn carried.

    Raises DispatchAskError(12) when the row has no harness_session_id, or
    when the bus write fails; both messages say that no durable envelope
    was written, so a caller cannot mistake the failure for a receipt.
    """
    from fno.inbox.store import DurableOwner, generate_msg_id, write_new_thread
    from fno.mail.envelope import wrap_fno_mail

    durable_recipient = (
        canonical_handle(entry.harness_session_id)
        if entry.harness_session_id
        else None
    )
    if durable_recipient is None:
        events.emit(
            "agent_send_failed",
            stage="durable-address",
            name=entry.name,
            msg_id=msg_id,
            reason="missing_harness_session_id",
            caller_reason=reason,
        )
        raise DispatchAskError(
            f"cannot queue durable mail for {entry.name!r}: registry row has "
            "no full harness session id; no durable envelope was written",
            exit_code=12,
        )

    msg_id = msg_id or generate_msg_id()
    sender_entry = next((e for e in entries if e.name == from_name), None)
    from_session = provider_from = None
    if sender_entry is not None:
        provider_from = sender_entry.harness
        # Defensive getattr so a partial / future entry that lacks one of
        # these fields degrades to None rather than crashing the send.
        from_session = (
            getattr(sender_entry, "harness_session_id", None)
            or getattr(sender_entry, "short_id", None)
        )
    if mail_ctx is None:
        mail_ctx = _build_mail_ctx(
            from_name,
            from_session,
            provider_from,
            # Never the short_id fallback the caller-built ctx uses: the
            # refusal above already proved durable_recipient is not None here.
            to=durable_recipient,
            id=msg_id,
        )
    # `_build_mail_ctx` returns a `_MailCtx`, never None, and the branch above
    # fills one whenever the caller passed none, so the envelope is always
    # wrapped from here on. The guard that used to stand here read as a real
    # unwrapped-body path and there is none.
    durable_body = wrap_fno_mail(
        message,
        from_=mail_ctx.from_,
        harness=mail_ctx.harness,
        model=mail_ctx.model,
        node=mail_ctx.node,
        to=mail_ctx.to,
        id=mail_ctx.id,
    )
    from fno import style as _style

    try:
        write_new_thread(
            recipient=durable_recipient,
            sender=mail_ctx.from_,
            kind="send",
            body=durable_body,
            msg_id=msg_id,
            to_kind="session",
            provider_to=entry.harness,
            provider_from=provider_from,
            from_session=from_session,
            owner=DurableOwner.WAKE_DAEMON.value,
            # Count the raw body, not the wire wrapper: Rule 7 and the rolling
            # budget read the same string, so the row must too.
            word_count=_style.word_count(message),
        )
    except (OSError, ValueError, RuntimeError) as exc:
        events.emit(
            "agent_send_failed",
            stage="envelope-write",
            name=entry.name,
            msg_id=msg_id,
            caller_reason=reason,
        )
        # The clause has to appear, and appear once. The bus-lock timeout this
        # most often wraps already ends with it, so appending unconditionally
        # stuttered "...; no durable envelope was written; no durable envelope
        # was written" at the one moment the reader is deciding whether to
        # re-send.
        detail = str(exc)
        tail = "" if _NO_ENVELOPE_CLAUSE in detail else f"; {_NO_ENVELOPE_CLAUSE}"
        raise DispatchAskError(
            f"durable envelope write failed: {detail}{tail}",
            exit_code=12,
        ) from exc
    return msg_id, durable_recipient


def _stamp_after_delivery(
    name: str,
    identity: "RecipientIdentity",
    delivery: str,
    msg_id: str,
    *,
    registry_path: "Path",
    registry_lock_timeout: float,
) -> None:
    """Bump `last_message_at` (and `status` for a hosted send) after delivery.

    Delivery is already complete when this runs, so a failure here cannot make
    the send retryable, but it must stay visible. Shared by the normal path and
    the lock-timeout queue so a durable success is booked the same way in both.
    """
    try:

        def _stamp(entries_list: "list[AgentEntry]") -> "list[AgentEntry]":
            out = []
            for e in entries_list:
                if e.name == name:
                    updates: dict = {"last_message_at": _utc_now_iso()}
                    if delivery == "hosted":
                        updates["status"] = "live"
                    out.append(replace(e, **updates))
                else:
                    out.append(e)
            return out

        decline_reason: list[str] = []
        stamp_written = _update_registry_if_recipient_unchanged(
            name,
            identity,
            _stamp,
            registry_path=registry_path,
            registry_lock_timeout=registry_lock_timeout,
            decline_reason=decline_reason,
        )
        if not stamp_written:
            row_removed = decline_reason and decline_reason[0] == "row_removed"
            reason_text = "row removed entirely" if row_removed else "recipient identity changed"
            print(
                f"registry stamp failed after {delivery} delivery for "
                f"{name!r}: {reason_text}; delivery succeeded; "
                "do not retry",
                file=sys.stderr,
            )
            try:
                events.emit(
                    "agent_send_failed",
                    stage="registry-write",
                    name=name,
                    msg_id=msg_id,
                    delivery=delivery,
                    reason="row_removed" if row_removed else "recipient_identity_changed",
                    error=f"{reason_text} after delivery",
                    error_type="RecipientRowRemoved" if row_removed else "RecipientIdentityChanged",
                )
            except (OSError, ValueError):
                pass  # stderr already carries the non-retryable degradation
    except (OSError, ValueError, RegistryVersionError) as exc:
        print(
            f"registry stamp failed after {delivery} delivery for "
            f"{name!r}: {exc}; delivery succeeded; do not retry",
            file=sys.stderr,
        )
        try:
            events.emit(
                "agent_send_failed",
                stage="registry-write",
                name=name,
                msg_id=msg_id,
                delivery=delivery,
                error=str(exc),
                error_type=type(exc).__name__,
            )
        except (OSError, ValueError):
            pass  # stderr already carries the non-retryable degradation


def _reserve_send_budget(
    *,
    sender: str,
    recipient: str,
    message: str,
    msg_id: str,
    enforce: bool,
):
    """Reserve one canonical pair before any outward send side effect."""
    from fno import style
    from fno.mail import budget

    try:
        return budget.reserve(
            sender=sender,
            recipient=recipient,
            words=style.word_count(message),
            msg_id=msg_id,
            enforce=enforce,
        )
    except budget.BudgetRefused as exc:
        raise DispatchAskError(
            f"refused: rolling word budget for {exc.pair}: {exc.marker()}",
            exit_code=1,
        ) from exc
    except budget.BudgetUnavailable as exc:
        raise DispatchAskError(f"refused: {exc}", exit_code=1) from exc


def dispatch_send(
    name: str,
    message: str,
    provider: Optional[str],
    cwd: "Path",
    lock_timeout: float = _DEFAULT_LOCK_TIMEOUT,
    from_name: str = _FROM_NAME_DEFAULT,
    *,
    registry_stamp_timeout_seconds: float = 1.0,
    budget_enforce: bool = True,
) -> "DispatchSendResult":
    """Dispatch an async ``send`` to an already-registered agent.

    Live-inject-first (node x-1f23): live delivery is attempted FIRST and the
    durable inbox envelope is written ONLY when the recipient is not
    live-reachable or the live inject does not confirm. A confirmed live
    (``hosted``) send is self-recording in the transcript and is NOT also queued;
    its bus row is audit-only, while the durable bus remains the offline fallback
    tier. Both the live turn and the bus record carry the same ``<fno_mail>``
    envelope.

    Orchestration:

    1. Validate name / message / from_name (same rules as dispatch_ask).
    2. Reject bodies over 1 MiB (exit 2) BEFORE any store write.
    3. Resolve the address to its registry primary key, then acquire that
       per-agent flock (hold_agent_lock) with timeout. A timeout retries the
       acquire on a short grace window and queues the message durable when it
       wins (delivery="durable", reason=LOCK_TIMEOUT_REASON, exit 0); only
       sustained contention, which leaves the recipient unverified, exits 11
       with nothing written.
    4. INSIDE the flock:
       a. Reload and re-resolve; unknown or changed identity refuses.
       b. Provider mismatch -> exit 2.
       c. Capture sender provenance + build the <fno_mail> ctx; generate msg_id.
       d. Attempt live delivery via _deliver_live (fire-and-forget).
       e. On non-hosted, write the durable fallback envelope (the <fno_mail>
          body), kind=send, addressed to the selected session's canonical handle.
       f. Emit agent_send_started / agent_send_done (delivery field).
       g. Bump last_message_at + status stamps via update_registry.
    5. Return DispatchSendResult(msg_id, delivery).

    Raises:
        DispatchAskError: every documented failure mode.  send never
            creates agents; unknown names get exit 16 identical to ask.
    """
    # 1. Input validation (reuses ask's _validate_inputs). `name` here is a
    # target to resolve: a bare short-id is the canonical mailbox handle, so it
    # must reach registry lookup (and then handle resolution on a miss) rather
    # than being refused as a badly-shaped name.
    _validate_inputs(
        name=name, message=message, from_name=from_name, name_is_address=True
    )

    from fno.time_budget import validate_timeout_budget

    validate_timeout_budget(
        registry_stamp_timeout_seconds,
        label="post-delivery registry stamp",
    )

    # 2. Body size cap (exit 2 BEFORE any write).
    if len(message.encode("utf-8")) > _SEND_MAX_BODY_BYTES:
        raise DispatchAskError(
            f"message body exceeds maximum size "
            f"({_SEND_MAX_BODY_BYTES // 1024 // 1024} MiB); "
            f"got {len(message.encode('utf-8'))} bytes",
            exit_code=2,
        )

    registry_path = paths.agents_registry_path()
    requested_name = name

    def _load_and_resolve_target(
        expected_identity: Optional[RecipientIdentity] = None,
    ) -> tuple[list[AgentEntry], AgentEntry]:
        try:
            entries = load_registry(registry_path)
        except (OSError, ValueError, RegistryVersionError) as exc:
            events.emit(
                "agent_send_failed",
                stage="registry-read",
                name=requested_name,
            )
            raise DispatchAskError(
                f"registry read failed: {exc}",
                exit_code=12,
            ) from exc
        try:
            existing = resolve_registered_agent_across_sources(
                entries, requested_name
            ).entry
            resolved_identity = _recipient_identity_key(existing)
            if (
                expected_identity is not None
                and resolved_identity != expected_identity
            ):
                raise DispatchAskError(
                    f"agent address {requested_name!r} changed from "
                    f"{expected_identity[0]!r} to {existing.name!r} while acquiring "
                    "its lock; recipient identity changed from "
                    f"{expected_identity!r} to {resolved_identity!r}; retry the send",
                    exit_code=2,
                )
            from fno.agents.discover import discovery_address_matches

            registry_id = resolved_identity[2]
            registry_key = (resolved_identity[1], registry_id)
            live_foreign = {
                (session.agent, session_identity_key(session.session_id)): session
                for session in discovery_address_matches(
                    requested_name, registry_path=registry_path
                )
                if (
                    session.agent,
                    session_identity_key(session.session_id),
                )
                != registry_key
            }
            if live_foreign:
                candidates = [
                    f"{registry_id or existing.name} "
                    f"({existing.harness}, registry name={existing.name})",
                    *(
                        f"{session.session_id} ({session.agent}, live discovery)"
                        for session in sorted(
                            live_foreign.values(),
                            key=lambda value: (value.agent, value.session_id),
                        )
                    ),
                ]
                raise AgentResolutionError(
                    f"token {requested_name!r} is ambiguous across "
                    f"{len(candidates)} sessions: {', '.join(candidates)}. "
                    "Disambiguate with the full session id.",
                    ambiguous=True,
                )
        except AgentResolutionError as exc:
            events.emit(
                "agent_send_failed",
                stage=(
                    "ambiguous-address"
                    if exc.ambiguous
                    else "identity-unavailable"
                    if exc.unavailable
                    else "unknown-name"
                ),
                name=requested_name,
            )
            if exc.ambiguous:
                raise DispatchAskError(str(exc), exit_code=2) from exc
            if exc.unavailable:
                raise DispatchAskError(str(exc), exit_code=12) from exc
            raise DispatchAskError(
                f"unknown agent {requested_name!r}; spawn it first: "
                f"fno agents spawn {requested_name} --harness <harness>",
                exit_code=UNKNOWN_AGENT_EXIT_CODE,
            ) from exc
        return entries, existing

    # Resolve before locking so every address form serializes on the registry
    # primary key. The second resolution under that lock closes the read/lock
    # race and refuses if the address changed owners while we waited.
    _, initial = _load_and_resolve_target()
    canonical_name = initial.name
    canonical_identity = _recipient_identity_key(initial)

    def _on_wait() -> None:
        print(
            f"Waiting for agent {canonical_name!r} lock...",
            file=sys.stderr,
            flush=True,
        )

    # 3. Per-agent flock. Confirmed (node x-1904, change 6): this `with` block
    # spans the ENTIRE rest of the send, including the live-delivery attempt
    # below -- not only the registry read/identity-check mutation it exists to
    # guard (the "second resolution under that lock closes the read/lock race"
    # comment a few lines down). A live-delivery attempt at a busy recipient
    # can now take up to `_MAIL_INJECT_LIVENESS_SCALED_TIMEOUT_S` (40s, change
    # 2's own budget raise) or the switchboard's 130s relay ceiling, so a
    # second sender addressing the SAME busy recipient serializes behind the
    # whole first attempt rather than just the identity check -- the specimen
    # measured delivering this plan's own report ("Waiting for agent
    # 'king-footnote-g3' lock..." with no progress until killed). Narrowing
    # this to cover only the registry mutation is a real fix but a nontrivial
    # restructuring of a concurrency-sensitive 300-line function under time
    # pressure is the wrong place to guess; filed as a carveout rather than
    # rushed here.
    try:
        with hold_agent_lock(
            canonical_name,
            registry_path,
            timeout=lock_timeout,
            on_wait=_on_wait,
        ):
            entries, existing = _load_and_resolve_target(canonical_identity)
            if existing.name != canonical_name:
                raise DispatchAskError(
                    f"agent address {requested_name!r} changed from "
                    f"{canonical_name!r} to {existing.name!r} while acquiring "
                    "its lock; retry the send",
                    exit_code=2,
                )
            selected_identity = _recipient_identity_key(existing)
            # Live routing and events use the registry primary key, never the
            # caller's alias. Durable routing below binds to this selected
            # session's canonical handle so later name reuse cannot inherit it.
            name = canonical_name

            # 4b. Provider mismatch check (mirrors dispatch_ask).
            try:
                select_provider(name=name, requested_provider=provider)
            except ProviderMismatchError as exc:
                raise DispatchAskError(str(exc), exit_code=2) from exc
            except ValueError as exc:
                raise DispatchAskError(str(exc), exit_code=2) from exc
            except (OSError, RegistryVersionError) as exc:
                events.emit(
                    "agent_send_failed",
                    stage="registry-read",
                    name=name,
                )
                raise DispatchAskError(
                    f"registry read failed: {exc}",
                    exit_code=12,
                ) from exc

            # 4c. Capture sender provenance for the <fno_mail> envelope and the
            # durable fallback record (node x-1f23). Sender identity is
            # best-effort: an unregistered caller leaves from_session None and
            # exclusion falls back to the always-present from_ name. from_model is
            # NOT set on the durable envelope (AgentEntry has no model field; we do
            # not fabricate one -- LD11 forward-compat).
            from fno.inbox.store import generate_msg_id

            sender_entry = next((e for e in entries if e.name == from_name), None)
            from_session = provider_from = None
            if sender_entry is not None:
                provider_from = sender_entry.harness
                # Defensive getattr so a partial / future entry that lacks one of
                # these fields degrades to None rather than crashing the send.
                from_session = (
                    getattr(sender_entry, "harness_session_id", None)
                    or getattr(sender_entry, "short_id", None)
                )
            # A `fno agents mail send <name>` is always directed -> stamp the selected
            # session's canonical handle as the envelope `to`. A transport short
            # id is retained only for hosted delivery when the legacy row has no
            # full session id; such a row cannot safely receive durable mail.
            # Mint the id BEFORE building the ctx so the SAME id rides the live
            # inject, the durable fallback, AND the durable thread record (US1 /
            # Locked Decision 8: both _name_lane_send and _deliver_live carry it).
            msg_id = generate_msg_id()
            durable_recipient = (
                canonical_handle(existing.harness_session_id)
                if existing.harness_session_id
                else None
            )
            budget_recipient = durable_recipient or existing.short_id or canonical_name
            mail_ctx = _build_mail_ctx(
                from_name,
                from_session,
                provider_from,
                to=(durable_recipient or existing.short_id or None),
                id=msg_id,
            )
            reservation = _reserve_send_budget(
                sender=mail_ctx.from_,
                recipient=budget_recipient,
                message=message,
                msg_id=msg_id,
                enforce=budget_enforce,
            )

            live_attempted = False

            def _write_durable() -> None:
                """Write the durable FALLBACK envelope: the pending-queue for an
                offline recipient, or the recovery record when a live inject did
                not land. The jsonl bus is the fallback tier now, not a peer to the
                live path (node x-1f23). Drain-on-wake semantics are unchanged.

                Delegates to :func:`_queue_durable_fallback`, the same helper the
                lock-timeout handler below uses, so a message queued from inside
                the lock and one queued after failing to acquire it share one
                envelope-construction and error path."""
                try:
                    _queue_durable_fallback(
                        existing,
                        message,
                        from_name,
                        entries,
                        msg_id=msg_id,
                        mail_ctx=mail_ctx,
                    )
                except Exception:
                    if not live_attempted:
                        from fno.mail import budget

                        budget.release(reservation)
                    raise

            # 4d/4e. Live-inject-first, durable fallback. The context stash ensures
            # started/done share one request_id + caller attribution (mirrors the
            # dispatch_ask pattern introduced in PR #457).
            ctx_for_dispatch = build_context(
                to_name=name,
                to_provider=existing.harness,
                transport="direct-cli",
                from_name_override=from_name,
            )
            ctx_token = _DISPATCH_CTX.set(ctx_for_dispatch)

            try:
                _emit_ev(
                    "agent_send_started",
                    name=name,
                    provider=existing.harness,
                    msg_id=msg_id,
                )

                family1_state = _registered_family1_state(existing)
                family1_live = family1_state in {"working", "watching", "your-move"}
                # Unknown is hands-off, not dead. A registered peer still has a
                # confirmable transport, so try it once and let delivery's ack
                # decide; failure falls through to the durable bus.
                family1_attemptable = family1_live or family1_state == "unknown"

                # W3 write-ahead: a recipient we will not attempt live is asleep,
                # so it cannot drain during a live window, and there is no live
                # window to crash in. Write its durable placeholder BEFORE
                # anything else so a sender crash before the recipient wakes does
                # not lose the message. A recipient we WILL attempt live stays
                # live-first: it can drain at its next SessionStart while the live
                # turn is still in flight, before the id lands in its transcript,
                # so W2 cannot skip a write-ahead placeholder there and the
                # message would double-deliver.
                if durable_recipient is not None and not family1_attemptable:
                    _write_durable()
                delivery = "durable"
                demotion_notice: Optional[str] = None
                live_miss_reason: Optional[str] = None

                _live_delivered = False
                _live_reason: list = []
                # x-e21e: the row's own policy names the durable queue's cause
                # even when no live rung was attemptable (an idle registered
                # leader), so the receipt never reads as a live-miss.
                _bus_only = (
                    _delivery_policy_refusal(existing) == BUS_ONLY_POLICY
                )
                if family1_attemptable:
                    live_attempted = True
                    _live_delivered = _deliver_live(
                        existing,
                        message,
                        from_name,
                        mail_ctx,
                        sender_entry=sender_entry,
                        reason_out=_live_reason,
                        family1_state=family1_state,
                    )
                    if _live_delivered:
                        delivery = "hosted"
                        from fno.bus.log import record_hosted_delivery
                        from fno.mail.envelope import wrap_fno_mail

                        hosted_body = wrap_fno_mail(
                            message,
                            from_=mail_ctx.from_,
                            harness=mail_ctx.harness,
                            model=mail_ctx.model,
                            node=mail_ctx.node,
                            to=mail_ctx.to,
                            id=mail_ctx.id,
                        )
                        from fno import style as _hstyle

                        _hosted_words = _hstyle.word_count(message)
                        try:
                            record_hosted_delivery(
                                msg_id=msg_id,
                                sender=mail_ctx.from_,
                                recipient=durable_recipient or existing.short_id,
                                body=hosted_body,
                                provider_from=provider_from,
                                provider_to=existing.harness,
                                from_session=from_session,
                                from_model=mail_ctx.model,
                                to_kind="session",
                                word_count=_hosted_words,
                            )
                        except Exception as exc:  # noqa: BLE001 - delivery already succeeded
                            print(
                                "delivery succeeded; outbox record failed; "
                                f"do not retry: {exc}",
                                file=sys.stderr,
                            )
                    else:
                        live_miss_reason = _live_reason[0] if _live_reason else None
                if _bus_only:
                    live_miss_reason = BUS_ONLY_POLICY
                if not _live_delivered and (durable_recipient is None or family1_attemptable):
                    # Live-first fallback: an attemptable recipient whose live
                    # attempt missed, or a recipient with no durable address
                    # (which raises durable-address exit 12 inside _write_durable).
                    _write_durable()

                if delivery == "durable" and family1_live:
                    why = f" ({live_miss_reason})" if live_miss_reason else ""
                    demotion_notice = (
                        f"live delivery failed for {name!r}{why}; message queued durable ({msg_id})"
                    )

                _emit_ev(
                    "agent_send_done",
                    name=name,
                    provider=existing.harness,
                    msg_id=msg_id,
                    delivery=delivery,
                )
            finally:
                _DISPATCH_CTX.reset(ctx_token)

            if demotion_notice:
                print(demotion_notice, file=sys.stderr)

            # 4f. Bump registry stamps (shared with the lock-timeout queue).
            _stamp_after_delivery(
                name,
                selected_identity,
                delivery,
                msg_id,
                registry_path=registry_path,
                registry_lock_timeout=registry_stamp_timeout_seconds,
            )

            return DispatchSendResult(msg_id=msg_id, delivery=delivery, reason=live_miss_reason)

    except AgentLockTimeout as exc:
        # INVARIANT, and it is load-bearing: this handler guards the whole
        # `with` body, not only the acquire, and the body now ends in a
        # durable queue that returns exit 0. That is safe ONLY because no
        # callee inside the block takes a per-agent flock - not _deliver_live,
        # _switchboard_exchange, _mux_pane_send, _registered_family1_state,
        # _queue_durable_fallback or _stamp_after_delivery. Add a nested
        # acquire and a timeout AFTER a confirmed hosted delivery lands here,
        # queues the same message a second time, and prints a durable receipt
        # for one that already arrived. Narrow this `try` to the acquire
        # before adding one.
        #
        # A durable write needs a VERIFIED recipient, and ONLY the lock
        # verifies one. An unlocked re-read cannot: the contender may be a
        # same-name reclaim that holds the flock and has not committed its
        # replacement row yet, so the read returns the OLD identity and the
        # "identity unchanged" check passes vacuously. Unchanged has two
        # explanations there - it really is, or the change is not visible yet -
        # and queuing on that reading strands the message in the dead session's
        # mailbox. So the queue takes the lock too, on a short grace window
        # that asks "did the holder just finish?" rather than waiting again.
        #
        # The locked path rebinds `name` to the registry primary key before it
        # emits or stamps anything; this path never entered that block, so it
        # must do the same. Everything below keys on the name: a stamp keyed to
        # the caller's alias matches no row, so it silently skips
        # `last_message_at` AND reports the miss as "recipient identity
        # changed" - a false failure on a send that succeeded.
        name = canonical_name
        grace_seconds = _queue_grace_seconds(exc.timeout)
        # Reassigned under the lock once the row is resolved: a bus-only row
        # queues by design, not by deferral, and must not wear this lane's
        # reason. Declared here because the receipt is built after the block.
        queue_reason = LOCK_TIMEOUT_REASON
        try:
            with hold_agent_lock(
                canonical_name,
                registry_path,
                timeout=grace_seconds,
                # The grace window makes a default send block for 42s, not 30.
                # Without the callback the extra 12s is silent, so the command
                # reads as hung right after it announced it was waiting.
                on_wait=_on_wait,
            ):
                # Every resolution that feeds a write happens here, under the
                # lock, through the same call the normal path uses. An owner
                # change now REFUSES (exit 2) rather than guessing which
                # session the caller meant.
                timeout_entries, timeout_entry = _load_and_resolve_target(
                    canonical_identity
                )

                # Provider mismatch refuses BEFORE the queue. The locked path
                # checks this and exits 2 without delivering, so lock
                # contention must not turn a refused send into a delivered one.
                try:
                    select_provider(
                        name=timeout_entry.name, requested_provider=provider
                    )
                except ProviderMismatchError as mismatch:
                    raise DispatchAskError(str(mismatch), exit_code=2) from mismatch
                except ValueError as bad_provider:
                    # An unknown --harness is a usage error on the locked path
                    # too. Lock timing must not change which exception the CLI
                    # sees.
                    raise DispatchAskError(
                        str(bad_provider), exit_code=2
                    ) from bad_provider
                except (OSError, RegistryVersionError) as unreadable:
                    events.emit("agent_send_failed", stage="registry-read", name=name)
                    raise DispatchAskError(
                        f"registry read failed: {unreadable}",
                        exit_code=12,
                    ) from unreadable

                # A queued message is a delivered outcome, so it books like
                # one: started/done and the registry stamp all fire, exactly as
                # the normal durable path does. Emitting only agent_send_failed
                # here left a successful send reading as a failure in the event
                # trail and left last_message_at stale, which agent display and
                # project anycast rank on.
                #
                # All of it stays INSIDE the lock, like the normal path: a stamp
                # written after release can lose a same-name reclaim race and
                # print "recipient identity changed" over a send that succeeded.
                ctx_for_timeout = build_context(
                    to_name=name,
                    to_provider=timeout_entry.harness,
                    transport="direct-cli",
                    from_name_override=from_name,
                )
                # A legacy row with no full harness session id has no durable
                # ADDRESS: hosted delivery is its only lane, and this path
                # never ran one. Queuing anyway raises exit 12 "cannot queue
                # durable mail ... no full harness session id", which reads as
                # a permanent registry defect and stops the caller dead. The
                # truth is narrower and recoverable, and it is what exit 11
                # said before this lane existed: the lock was busy, live is
                # the only lane this row has, retry.
                if not timeout_entry.harness_session_id:
                    events.emit(
                        "agent_send_failed",
                        stage="durable-address",
                        name=name,
                        reason="missing_harness_session_id",
                        caller_reason=LOCK_TIMEOUT_REASON,
                    )
                    raise DispatchAskError(
                        f"live delivery to {name!r} was not attempted (its "
                        f"agent lock was busy for {exc.timeout}s, and this "
                        "queue holds it now), and its registry row carries no "
                        "full harness session id, so it has no durable "
                        f"address; {_NO_ENVELOPE_CLAUSE}; retry the send",
                        exit_code=11,
                    ) from exc

                # It does NOT retry the live lane from here, and that is a
                # decision rather than an omission - five review passes have
                # read it as one, so it is written down. The block holds a
                # verified row and could inject. It does not, because an
                # inject here runs to the same 40s budget WHILE HOLDING this
                # flock, on a send that has already waited out one holder: it
                # would push a routine send past 80s and make the next sender
                # time out on us, which is the contention that caused this bug.
                # The sender is not stranded either way - the message is
                # durable and the receipt names withdraw-then-resend. Moving
                # live delivery in here belongs with narrowing the flock, and
                # is tracked with it.
                #
                # A bus-only row's queue is its DESIGNED lane, not a deferral.
                # Reporting it as a lock timeout hands the sender the recovery
                # ladder - withdraw, then retry live - for a message that is
                # correctly queued, against a row whose own policy forbids the
                # live retry. The normal path branches here; a grace-path queue
                # that skipped the check mirrored this arm's own wrong-cause
                # defect onto a different row.
                queue_reason = (
                    BUS_ONLY_POLICY
                    if _delivery_policy_refusal(timeout_entry) == BUS_ONLY_POLICY
                    else LOCK_TIMEOUT_REASON
                )

                # Mint the id before the started event so started and done name
                # the same message, as they do on the normal path.
                from fno.inbox.store import generate_msg_id

                msg_id = generate_msg_id()
                sender_entry = next(
                    (entry for entry in timeout_entries if entry.name == from_name),
                    None,
                )
                from_session = provider_from = None
                if sender_entry is not None:
                    provider_from = sender_entry.harness
                    from_session = (
                        getattr(sender_entry, "harness_session_id", None)
                        or getattr(sender_entry, "short_id", None)
                    )
                timeout_recipient = canonical_handle(
                    timeout_entry.harness_session_id
                )
                timeout_mail_ctx = _build_mail_ctx(
                    from_name,
                    from_session,
                    provider_from,
                    to=timeout_recipient,
                    id=msg_id,
                )
                reservation = _reserve_send_budget(
                    sender=timeout_mail_ctx.from_,
                    recipient=timeout_recipient,
                    message=message,
                    msg_id=msg_id,
                    enforce=budget_enforce,
                )
                ctx_token = _DISPATCH_CTX.set(ctx_for_timeout)
                try:
                    _emit_ev(
                        "agent_send_started",
                        name=name,
                        provider=timeout_entry.harness,
                        msg_id=msg_id,
                    )
                    try:
                        msg_id, _durable_to = _queue_durable_fallback(
                            timeout_entry,
                            message,
                            from_name,
                            timeout_entries,
                            msg_id=msg_id,
                            reason=queue_reason,
                            mail_ctx=timeout_mail_ctx,
                        )
                    except Exception:
                        from fno.mail import budget

                        budget.release(reservation)
                        raise
                    _emit_ev(
                        "agent_send_done",
                        name=name,
                        provider=timeout_entry.harness,
                        msg_id=msg_id,
                        delivery="durable",
                        reason=queue_reason,
                    )
                finally:
                    _DISPATCH_CTX.reset(ctx_token)

                _stamp_after_delivery(
                    name,
                    _recipient_identity_key(timeout_entry),
                    "durable",
                    msg_id,
                    registry_path=registry_path,
                    registry_lock_timeout=registry_stamp_timeout_seconds,
                )
        except AgentLockTimeout as still_held:
            # Sustained contention: no verified recipient, so nothing is
            # written. Loud and nonzero beats a message delivered to whoever
            # used to own the name. The requirement was "queue durable OR exit
            # nonzero and say so"; this is the second arm.
            events.emit(
                "agent_send_failed",
                stage="lock-timeout",
                name=name,
                reason="unverified_recipient",
            )
            # Name the holder that blocked the QUEUE, not the one that blocked
            # the first acquire. They differ whenever the original holder
            # released and a third process took the lock inside the grace
            # window, and reporting the first one points at a process that no
            # longer owns anything. Both waits are named for the same reason:
            # the caller blocked for lock_timeout AND the grace window, so the
            # first alone understates the block and reads as a tuning knob
            # that did not do what it says.
            raise DispatchAskError(
                f"timed out waiting for agent {exc.name!r} lock "
                f"(timeout={exc.timeout}s + {grace_seconds}s queue grace)"
                f"{still_held.holder_note()}; "
                "recipient identity could not be verified, so no durable "
                "envelope was written; retry the send",
                exit_code=11,
            ) from still_held

        # The message is queued, so this is a durable SUCCESS, not a failure.
        # cmd_send's stdout contract is one receipt line and exit 0 for every
        # durable outcome; a nonzero exit here would both break that and make
        # a retry-on-failure caller enqueue the same message twice. The live
        # lane's cause rides back as the receipt's reason, and the holder goes
        # to stderr the way a live-miss demotion notice does.
        # Not for a bus-only row: its queue is the designed destination, so a
        # "live delivery deferred" notice would report a miss that never was.
        if queue_reason == LOCK_TIMEOUT_REASON:
            # Past tense throughout, and deliberately. Reaching this line
            # proves the holder RELEASED, because the grace acquire only wins
            # once it does, so a present-tense "lock busy ... held by pid P"
            # names an owner that no longer owns it - and `_pid_is_alive`
            # cannot refuse that pid, which is usually still running. The
            # exit-11 arm below reads `still_held` for the same reason.
            print(
                f"live delivery deferred for {name!r}: "
                f"lock was busy for {exc.timeout}s{exc.holder_note(past=True)}",
                file=sys.stderr,
            )
        return DispatchSendResult(
            msg_id=msg_id,
            delivery="durable",
            reason=queue_reason,
        )


# ---------------------------------------------------------------------------
# Project-destination addressing (anycast) - Group 3 Task 3.3 (US6)
# ---------------------------------------------------------------------------
# Project/cwd is demoted from address to resolver. `send --to-project X` (and
# `ask --to-project`) resolves over the registry: cwd->project mapping plus the
# config.inbox.peers `project:` hint. Rule: exactly one live peer -> deliver
# live; none -> durable queue to project X; many -> error listing the live
# candidates unless `--any` breaks the tie (most recent last_message_at wins,
# lexicographic registry name as the final tiebreak). One log underneath.

AMBIGUOUS_PROJECT_EXIT_CODE = 17


@dataclass
class ProjectResolution:
    """Outcome of resolving a project name to a delivery target.

    Exactly one of three outcomes holds: live (``recipient`` set), durable
    (``durable``), or ambiguous (``ambiguous``). ``__post_init__`` enforces the
    mutual exclusivity so an illegal combination (e.g. a recipient AND
    ambiguous) fails loudly at construction rather than silently mis-routing.
    """

    recipient: Optional[str]  # the single live peer to deliver to, else None
    live_candidates: list[str]  # all live peer names in the project (sorted)
    durable: bool  # True when no live peer -> durable queue
    ambiguous: bool  # True when >1 live peer and no --any

    def __post_init__(self) -> None:
        active = (self.recipient is not None) + self.durable + self.ambiguous
        if active != 1:
            raise ValueError(
                "ProjectResolution must encode exactly one outcome; got "
                f"recipient={self.recipient!r}, durable={self.durable}, "
                f"ambiguous={self.ambiguous}"
            )
        if self.ambiguous and len(self.live_candidates) < 2:
            raise ValueError("ambiguous resolution requires >=2 live candidates")


def _entry_projects(entry: "AgentEntry", peer_projects: dict[str, str]) -> set[str]:
    """Return every project a registry entry serves.

    The registry cwd->project mapping is authoritative; the
    `config.inbox.peers.<name>.project` hint only ADDS an association, it never
    replaces the cwd mapping. So an entry serves BOTH its cwd-resolved project
    and any hinted project: a stale or extra hint can never hide a live peer
    from its actual cwd project. Returns the (possibly empty) set of project
    names this entry is a candidate for.
    """
    projects: set[str] = set()
    if entry.cwd:
        try:
            from fno.inbox.store import (
                ProjectIdentificationError,
                resolve_project,
            )

            projects.add(resolve_project(Path(entry.cwd)))
        except ProjectIdentificationError:
            pass
        except Exception:  # noqa: BLE001 - a bad cwd must not abort resolution
            pass
    hinted = peer_projects.get(entry.name)
    if hinted:
        projects.add(hinted)
    return projects


def resolve_to_project(
    project: str,
    *,
    any_: bool = False,
    registry_path: "Optional[Path]" = None,
) -> ProjectResolution:
    """Resolve a destination project to a single delivery target.

    Registry cwd->project mapping is authoritative; the `config.inbox.peers`
    `project:` hint only adds associations and degrades to {} (never raises)
    on a missing/malformed config, so resolution always works off the registry
    alone (AC6-FR).
    """
    try:
        from fno.inbox.settings import read_peer_projects

        # read_peer_projects already degrades to {} on a malformed config shape
        # (with its own stderr warning), so this outer guard only catches an
        # UNEXPECTED error in the hint path - log it rather than silently
        # masking a real bug as "no hints".
        peer_projects = read_peer_projects()
    except Exception as exc:  # noqa: BLE001 - the hint is best-effort; never fatal
        print(
            f"warning: --to-project peer hint unavailable ({type(exc).__name__}: "
            f"{exc}); resolving over the registry cwd mapping alone",
            file=sys.stderr,
        )
        peer_projects = {}

    try:
        entries = load_registry(registry_path) if registry_path else load_registry()
    except (OSError, ValueError, RegistryVersionError) as exc:
        raise DispatchAskError(f"registry read failed: {exc}", exit_code=12) from exc

    # Candidate = any entry that serves this project (cwd mapping OR hint),
    # deduped by name.
    candidates: dict[str, "AgentEntry"] = {}
    for e in entries:
        if project in _entry_projects(e, peer_projects):
            candidates[e.name] = e

    live = [e for e in candidates.values() if e.status == "live"]
    live_names = sorted(e.name for e in live)

    if not live:
        return ProjectResolution(recipient=None, live_candidates=[], durable=True, ambiguous=False)
    if len(live) == 1:
        return ProjectResolution(
            recipient=live[0].name,
            live_candidates=live_names,
            durable=False,
            ambiguous=False,
        )
    if not any_:
        return ProjectResolution(
            recipient=None,
            live_candidates=live_names,
            durable=False,
            ambiguous=True,
        )
    # --any tiebreak: most recent last_message_at, then lexicographic name.
    max_ts = max((e.last_message_at or "") for e in live)
    tied = sorted(
        (e for e in live if (e.last_message_at or "") == max_ts),
        key=lambda e: e.name,
    )
    return ProjectResolution(
        recipient=tied[0].name,
        live_candidates=live_names,
        durable=False,
        ambiguous=False,
    )


def dispatch_send_to_project(
    project: str,
    message: str,
    *,
    provider: Optional[str] = None,
    cwd: "Path",
    from_name: str = _FROM_NAME_DEFAULT,
    any_: bool = False,
    lock_timeout: float = _DEFAULT_LOCK_TIMEOUT,
    budget_enforce: bool = True,
) -> "DispatchSendResult":
    """Async send addressed to a project (anycast over the registry).

    One live peer -> live send to it (records the resolved recipient). None ->
    durable queue addressed to the project (picked up at that project's next
    drain). Many -> error listing the live candidates, delivering to none,
    unless ``any_`` breaks the tie deterministically.
    """
    # Validate message + from_name (the project name is validated by the
    # resolver / store recipient check, not the agent-name rule).
    _validate_inputs(name="placeholder", message=message, from_name=from_name)

    # Body size cap BEFORE any write, matching dispatch_send (both delivery
    # paths - live by-name and durable-to-project - share the ceiling).
    if len(message.encode("utf-8")) > _SEND_MAX_BODY_BYTES:
        raise DispatchAskError(
            f"message body exceeds maximum size "
            f"({_SEND_MAX_BODY_BYTES // 1024 // 1024} MiB); "
            f"got {len(message.encode('utf-8'))} bytes",
            exit_code=2,
        )

    res = resolve_to_project(project, any_=any_)

    if res.ambiguous:
        listing = ", ".join(res.live_candidates)
        raise DispatchAskError(
            f"--to-project {project!r} is ambiguous: {len(res.live_candidates)} "
            f"live peers ({listing}); pass --any to break the tie or address one "
            f"by name. Delivered to none.",
            exit_code=AMBIGUOUS_PROJECT_EXIT_CODE,
        )

    if res.recipient is not None:
        # Exactly one live peer (or --any winner): deliver live by name.
        # Omit the default kwarg so existing in-process adapters that implement
        # the pre-budget dispatch signature keep working. The exception lane
        # passes False explicitly because it changes enforcement.
        budget_kwargs = {} if budget_enforce else {"budget_enforce": False}
        result = dispatch_send(
            name=res.recipient,
            message=message,
            provider=provider,
            cwd=cwd,
            lock_timeout=lock_timeout,
            from_name=from_name,
            **budget_kwargs,
        )
        return replace(result, recipient=res.recipient, to_project=project)

    # No live peer: durable queue addressed to the project itself. The envelope
    # (and bus mirror) record to == project (to_kind=project); the next drain in
    # that project picks it up, EXCLUDING the sender (Group 1, ab-ba91b807). The
    # sender identity is best-effort - exclusion falls back to the from_ name.
    from fno.inbox.store import DurableOwner, generate_msg_id, write_new_thread

    from_session = provider_from = None
    try:
        from fno.agents.registry import load_registry as _load_reg

        _se = next((e for e in _load_reg() if e.name == from_name), None)
        if _se is not None:
            provider_from = _se.harness
            from_session = (
                getattr(_se, "harness_session_id", None)
                or getattr(_se, "short_id", None)
            )
    except Exception:  # noqa: BLE001 - sender identity is best-effort
        pass

    from fno import style as _pstyle
    from fno.mail import budget

    msg_id = generate_msg_id()
    reservation = _reserve_send_budget(
        sender=from_name,
        recipient=project,
        message=message,
        msg_id=msg_id,
        enforce=budget_enforce,
    )

    try:
        handle = write_new_thread(
            recipient=project,
            sender=from_name,
            kind="send",
            body=message,
            msg_id=msg_id,
            to_kind="project",
            from_session=from_session,
            provider_from=provider_from,
            word_count=_pstyle.word_count(message),
            # US6: an explicit --to-project note deliberately chose the durable
            # project-inbox lane; the project's own drain owns it.
            owner=DurableOwner.INBOX_DRAIN.value,
        )
    except (OSError, ValueError, RuntimeError) as exc:
        budget.release(reservation)
        events.emit("agent_send_failed", stage="durable-write", name=project)
        raise DispatchAskError(
            f"durable envelope write failed for project {project!r}: {exc}",
            exit_code=12,
        ) from exc

    _emit_ev(
        "agent_send_done",
        name=project,
        provider=provider or "",
        msg_id=handle.thread_id,
        delivery="durable",
    )
    return DispatchSendResult(
        msg_id=handle.thread_id,
        delivery="durable",
        recipient=None,
        to_project=project,
    )
