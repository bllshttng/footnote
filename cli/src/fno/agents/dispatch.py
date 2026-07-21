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
``fno.agents.providers.{claude,codex,gemini}``. US1 ships the claude
adapter; codex / gemini land in US4.
"""

from __future__ import annotations

import contextvars
import os
import re
import shutil
import subprocess
import sys
import time
from contextlib import contextmanager
from dataclasses import dataclass, field, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Iterator, Literal, Mapping, Optional

DispatchKind = Literal["create", "followup"]

from fno import paths
from fno.agents import events
from fno.agents.context import EventContext, build_context
from fno.agents.lock import AgentLockTimeout, hold_agent_lock
from fno.agents.providers import KNOWN_PROVIDERS
from fno.agents.providers.base import ProviderResult, ReachabilityProbeError
from fno.agents.registry import (
    AgentEntry,
    RegistryVersionError,
    _agent_lock_path,
    load_registry,
    update_registry,
)
from fno.harness_identity import resolve_harness_identity


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
    "abi_dispatch_ctx", default=None
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


_NAME_MAX_LEN = 128
_SHORT_ID_NAME_SHAPE = re.compile(r"^[0-9a-f]{8}$")
_DEFAULT_LOCK_TIMEOUT = 30.0

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

    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers.base import ReachabilityProbeError

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
    if existing.harness == "claude" and existing.mcp_channel_id:
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
                if _inside_leg_is_recent(_current_inside_leg(name), time.time()):
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
            if exc.reason == "roster-live-inject-failed" or _inside_leg_is_recent(
                _current_inside_leg(name), time.time()
            ):
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
    status: str,
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
        # Resolve callable ``last_message_at`` HERE so the timestamp is
        # generated under the registry-wide flock held by
        # ``update_registry``.
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
    """Stable abi-side log path for `fno agents logs <name>` (US3 plumbing)."""
    return paths.state_dir() / "agents" / "logs" / f"{name}.log"


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
    from fno.agents.providers import codex as codex_mod

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
        harness_session_id=session_id,
    )

    try:
        update_registry(lambda entries: entries + [new_entry])
    except (OSError, RegistryVersionError) as exc:
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
    from fno.agents.providers import codex as codex_mod

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


def _gemini_output_path(name: str) -> Path:
    """Tee target for the gemini provider's JSON+stderr stream.

    Same shape as ``_codex_output_path`` so ``fno agents logs <name>``
    sees a uniform layout regardless of provider.
    """
    return paths.state_dir() / "agents" / name / "output.jsonl"


def _gemini_create_path(
    *,
    name: str,
    message: str,
    cwd: Path,
    from_name: str,
    yolo: bool,
    timeout_sec: float,
    lock_handle,
) -> DispatchAskResult:
    """Spawn a new gemini agent under the per-agent flock.

    Mirror of ``_codex_create_path``. Failure-to-exit-code map:

    - gemini not on PATH                 -> 14 (caller checked earlier)
    - missing session_id in JSON output  -> 11 (GeminiParseError)
    - non-zero exit / sigkill escalation -> exit code from provider
    - wall-clock timeout                 -> 15 (GeminiTimeoutError)
    - registry write failure post-create -> 12 (with cleanup hint)
    """
    from fno.agents.providers import gemini as gemini_mod

    output_path = _gemini_output_path(name)

    try:
        result = gemini_mod.create(
            cwd=cwd,
            prompt=message,
            from_name=from_name,
            yolo=yolo,
            output_path=output_path,
            timeout=timeout_sec,
            agent_self=name,
        )
    except gemini_mod.GeminiTimeoutError as exc:
        events.emit(
            "agent_ask_failed",
            stage="gemini-timeout",
            name=name,
            provider="gemini",
            timeout_sec=exc.timeout_sec,
        )
        raise DispatchAskError(
            f"gemini create timed out after {exc.timeout_sec}s",
            exit_code=15,
        ) from exc
    except gemini_mod.GeminiParseError as exc:
        events.emit(
            "agent_ask_failed",
            stage="gemini-parse",
            name=name,
            provider="gemini",
            raw_head=exc.raw_head,
        )
        raise DispatchAskError(
            f"gemini output parse failed: {exc} (see {output_path} for full bytes)",
            exit_code=11,
        ) from exc
    except gemini_mod.GeminiInvocationError as exc:
        events.emit(
            "agent_ask_failed",
            stage="gemini-subprocess",
            name=name,
            provider="gemini",
            returncode=exc.exit_code,
        )
        raise DispatchAskError(
            f"gemini exited {exc.exit_code} (see {output_path} for details)",
            exit_code=exc.exit_code if exc.exit_code != 0 else 1,
        ) from exc

    session_id = result.session_id
    assert session_id is not None  # gemini.create raises GeminiParseError otherwise

    new_entry = AgentEntry(
        name=name,
        cwd=str(cwd),
        log_path=str(output_path),
        harness="gemini",
        harness_session_id=session_id,
    )

    try:
        update_registry(lambda entries: entries + [new_entry])
    except (OSError, RegistryVersionError) as exc:
        events.emit(
            "agent_ask_failed",
            stage="registry-write",
            name=name,
            provider="gemini",
            gemini_session_id=session_id,
        )
        lock_handle.detach()
        raise DispatchAskError(
            f"registry write failed: {exc}. "
            f"orphaned gemini session: gemini sessions persist on disk; "
            f"clean up via 'gemini --delete-session <index>' if desired "
            f"(--list-sessions to find the index)",
            exit_code=12,
        ) from exc

    _emit_ev(
        "agent_ask_done",
        stage="dispatch",
        name=name,
        provider="gemini",
        gemini_session_id=session_id,
        duration_ms=result.duration_ms,
        yolo=yolo,
    )
    # gemini's create path RETURNS the reply on stdout — same routing as
    # codex (DispatchAskResult.kind="followup" so the CLI prints the
    # reply verbatim without a banner).
    return DispatchAskResult(
        kind="followup",
        short_id=session_id,
        reply=result.last_msg,
        duration_ms=result.duration_ms,
    )


def _gemini_followup_path(
    *,
    name: str,
    message: str,
    from_name: str,
    existing: AgentEntry,
    yolo: bool,
    timeout_sec: float,
    lock_handle,
) -> DispatchAskResult:
    """Resume an existing gemini session via ``gemini --resume <uuid>``.

    Invariants mirror the codex followup contract:

    - cwd is taken from the registry's recorded ``existing.cwd`` (Wave 2.0
      OQ1: gemini sessions are cwd-pinned; resume from a different cwd
      fails with "Invalid session identifier").
    - gemini_session_id is preserved (never re-minted).
    - last_message_at is bumped only on success.
    """
    from fno.agents.providers import gemini as gemini_mod

    session_id = existing.harness_session_id
    if not session_id:
        raise DispatchAskError(
            f"registry entry {name!r} has no harness_session_id; cannot follow "
            f"up. Remove with 'fno agents rm {name}' and recreate.",
            exit_code=11,
        )

    _emit_ev(
        "agent_followup_started",
        name=name,
        provider="gemini",
        gemini_session_id=session_id,
        yolo=yolo,
    )

    if not existing.log_path:
        raise DispatchAskError(
            f"registry entry {name!r} has empty log_path; run 'fno agents rm {name}' and recreate.",
            exit_code=11,
        )
    if not existing.cwd:
        raise DispatchAskError(
            f"registry entry {name!r} has empty cwd; "
            f"gemini sessions are cwd-pinned and resume cannot proceed. "
            f"Run 'fno agents rm {name}' and recreate.",
            exit_code=11,
        )
    output_path = Path(existing.log_path)
    registered_cwd = Path(existing.cwd)

    try:
        result = gemini_mod.resume(
            session_id=session_id,
            cwd=registered_cwd,
            prompt=message,
            from_name=from_name,
            yolo=yolo,
            output_path=output_path,
            timeout=timeout_sec,
        )
    except gemini_mod.GeminiTimeoutError as exc:
        events.emit(
            "agent_followup_failed",
            stage="gemini-timeout",
            name=name,
            gemini_session_id=session_id,
            timeout_sec=exc.timeout_sec,
        )
        raise DispatchAskError(
            f"gemini follow-up timed out after {exc.timeout_sec}s",
            exit_code=15,
        ) from exc
    except gemini_mod.GeminiParseError as exc:
        events.emit(
            "agent_followup_failed",
            stage="gemini-parse",
            name=name,
            gemini_session_id=session_id,
            raw_head=exc.raw_head,
        )
        raise DispatchAskError(
            f"gemini output parse failed: {exc} (see {output_path} for full bytes)",
            exit_code=11,
        ) from exc
    except gemini_mod.GeminiInvocationError as exc:
        events.emit(
            "agent_followup_failed",
            stage="gemini-subprocess",
            name=name,
            gemini_session_id=session_id,
            returncode=exc.exit_code,
        )
        raise DispatchAskError(
            f"gemini resume exited {exc.exit_code} (see {output_path} for details). "
            f"If the session was deleted (e.g. 'gemini --delete-session'), "
            f"run 'fno agents rm {name}' then re-ask.",
            exit_code=exc.exit_code if exc.exit_code != 0 else 1,
        ) from exc

    # Bump last_message_at via the callable form so the timestamp is
    # generated INSIDE the registry-wide flock (monotonic per atomic
    # write under concurrent followup).
    try:
        update_registry(
            _stamp_status(name, status="live", last_message_at=_utc_now_iso),
        )
    except (OSError, RegistryVersionError) as exc:
        events.emit(
            "agent_followup_failed",
            stage="registry-write",
            name=name,
            gemini_session_id=session_id,
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
        provider="gemini",
        gemini_session_id=session_id,
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
    The shared harness identity precedence applies when multiple vars are set.
    Never raises; always returns a triple (missing fields degrade to None).

    Harness detection order (Task 2.2, x-30f6):
      CODEX_THREAD_ID        -> harness="codex"
      CLAUDE_CODE_SESSION_ID -> harness="claude"
      CODEX_SESSION_ID       -> harness="codex"
      GEMINI_SESSION_ID      -> harness="gemini"
    """
    identity = resolve_harness_identity()

    # $PWD may be unset (non-interactive shells, cron, daemonized procs); fall
    # back to os.getcwd(), which for a `fno agents spawn` subprocess is the
    # spawning session's cwd (inherited), so the parent cwd is always captured.
    parent_cwd: Optional[str] = (os.environ.get("PWD") or os.getcwd()).strip() or None

    return identity.session_id, identity.harness, parent_cwd


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

    from fno.agents.providers import claude as claude_mod

    # x-9844 Lane 2: guard the detached revival's critical section with the
    # session single-writer claim so a concurrent revival of the same uuid (the
    # residual window the per-agent flock's name-scoped serialization leaves
    # open, plus the cross-name case) can't spawn a second supervisor onto one
    # transcript. Released right after the registry write below - the new
    # supervisor's own liveness is the ongoing guard, and holding it longer would
    # hoard the writer (the documented regression where footnote blocks native
    # attach).
    revive_claim_holder: Optional[str] = None
    if revive and resume_session_id:
        revive_claim_holder = f"revive:{os.getpid()}"
        try:
            claude_mod.acquire_session_writer_claim(
                session_uuid=resume_session_id, holder=revive_claim_holder
            )
        except claude_mod.SessionWriterClaimError as exc:
            raise DispatchAskError(
                f"session {resume_session_id} is held by another writer; refusing "
                f"to open a second writer on one transcript ({exc})",
                exit_code=11,
            ) from exc

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
        events.emit(
            "agent_ask_failed",
            stage="subprocess",
            name=name,
            provider=chosen,
            returncode=exc.exit_code,
        )
        raise DispatchAskError(exc.stderr, exit_code=1) from exc
    except claude_mod.ProviderParseError as exc:
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

    # Capture the spawning session's ambient identity (Task 2.2, x-30f6).
    # Best-effort: never raises, degrades to (None, None, None) when absent.
    spawned_by_session, spawned_by_harness, spawned_by_cwd = _capture_parent_edge()

    # Registry write.
    log_path = _derive_log_path(name)
    new_entry = AgentEntry(
        name=name,
        cwd=str(cwd),
        log_path=str(log_path),
        short_id=short_id,
        # Canonical identity at birth (x-ec59): a bg claude row is born routable
        # by name. A raced uuid-resolution miss leaves harness_session_id None;
        # reconcile / send-time heal backfills it.
        harness="claude",
        harness_session_id=session_uuid,
        spawned_by_session=spawned_by_session,
        spawned_by_harness=spawned_by_harness,
        spawned_by_cwd=spawned_by_cwd,
    )

    # x-9844 Fix 3: a revival REPLACES the existing exited same-name row in place
    # (never appends a duplicate name). The load-modify-write is atomic under
    # update_registry's own lock, so a concurrent reader sees the old exited row
    # or the new live row, never a torn/absent state.
    def _write(entries: list) -> list:
        if revive:
            return [new_entry if e.name == name else e for e in entries]
        return entries + [new_entry]

    try:
        update_registry(_write)
    except (OSError, RegistryVersionError) as exc:
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
        raise DispatchAskError(
            f"registry write failed: {exc}. "
            f"orphaned supervisor session: claude rm {short_id} "
            f"(registry not updated)",
            exit_code=12,
        ) from exc

    # Revival succeeded and the row is live: release the critical-section claim
    # so the new supervisor's own liveness (not a lingering lockfile) is the
    # ongoing single-writer guard. Idempotent, so a never-recorded claim is a
    # no-op.
    if revive_claim_holder is not None and resume_session_id:
        claude_mod.release_session_writer_claim(
            session_uuid=resume_session_id, holder=revive_claim_holder
        )

    # Spawn event (Task 2.2, x-30f6): exactly one per successful create.
    # Open schema — flattens onto the JSONL record alongside ts/kind.
    events.emit(
        "agent_spawned",
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
        ``_codex_create_path`` / ``_gemini_create_path`` when called
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
                    f"fno agents spawn {name} -p <provider>",
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
                        return _gemini_followup_path(
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
            f"lock timeout for agent {name!r} after {exc.timeout}s",
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
    - ``"once"`` -- ephemeral one-shot (codex/gemini --once). ``reply``
      carries the exchange output; ``short_id`` is the session/short id.
      The CLI prints ``reply`` verbatim on stdout and the teardown receipt
      on stderr.
    """

    kind: Literal["created", "once"]
    name: str
    provider: str
    short_id: str
    reply: Optional[str] = None

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
    from fno.agents.providers import claude as claude_mod

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
    resume_session_id: Optional[str] = None,
    account_env: Optional[Mapping[str, str]] = None,
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
          - codex/gemini + once=False    -> exit 13 (PTY daemon required)
          - codex + once=True            -> ``_codex_create_path``; teardown after
          - gemini + once=True           -> ``_gemini_create_path``; teardown after

    Teardown (--once codex/gemini):
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
    # 1. Name validation. spawn allows empty message (default "").
    validate_spawn_name(name)
    _validate_from_name(from_name)

    # 2. Provider validation. _check_known_provider raises ValueError, which
    # cmd_spawn does not catch (it only catches DispatchAskError) -- wrap it
    # so an unknown --provider exits 2 cleanly instead of tracebacking.
    try:
        _check_known_provider(provider)
    except ValueError as exc:
        raise DispatchAskError(str(exc), exit_code=2) from exc

    # 3a. claude + --once -> refused immediately (before acquiring the lock,
    # since there is no state to protect).
    if provider == "claude" and once and not headless:
        raise DispatchAskError(
            f"--once is not supported for provider 'claude' "
            f"(claude peers are persistent bg threads; use plain spawn)",
            exit_code=2,
        )

    # 3b. codex/gemini plain spawn (no --once) in Python fallback -> exit 13.
    if provider in ("codex", "gemini") and not once:
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

            # Revive-in-place (x-9844 Fix 3): a --resume spawn whose target uuid
            # matches an EXITED same-name claude row is a revival, not a
            # collision - the row is updated in place below (new short_id, same
            # uuid) instead of refused. Every other same-name case stays
            # fail-closed (live row, uuid mismatch, no --resume).
            existing = next((e for e in entries if e.name == name), None)
            revive = existing is not None and _is_revival(
                existing, provider, resume_session_id
            )
            if existing is not None and not revive:
                raise DispatchAskError(
                    f"agent {name!r} already exists; "
                    f"use 'fno agents rm {name}' first or pick another name",
                    exit_code=2,
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
                        from fno.agents.providers import claude as claude_mod

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
                                account_env=account_env,
                            )
                        except claude_mod.ProviderSubprocessError as exc:
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
                        )
                    result = _claude_create_path(
                        name=name,
                        message=message,
                        cwd=cwd,
                        chosen="claude",
                        timeout=timeout,
                        yolo=yolo,
                        lock_handle=lock_handle,
                        role=role,
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
                    )
                    return SpawnResult(
                        kind="created",
                        name=name,
                        provider="claude",
                        short_id=result.short_id,
                    )

                # 4c. codex/gemini --once: create + exchange + teardown.
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
                        role=role,
                        effort=effort,
                        add_dir=add_dir,
                    )
                else:
                    # gemini --once
                    create_result = _gemini_create_path(
                        name=name,
                        message=message or "hello",
                        cwd=cwd,
                        from_name=from_name,
                        yolo=yolo,
                        timeout_sec=(
                            float(timeout) if timeout is not None else _DEFAULT_FOLLOWUP_TIMEOUT_SEC
                        ),
                        lock_handle=lock_handle,
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
                )
            finally:
                _DISPATCH_CTX.reset(ctx_token)

    except AgentLockTimeout as exc:
        raise DispatchAskError(
            f"lock timeout for agent {name!r} after {exc.timeout}s",
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


def _canonical_agent_name(token: str, *, registry_path: Optional[Path] = None) -> str:
    """Translate a lifecycle token (name | 8-hex short | full session id) to the
    canonical registry name, so ``stop``/``rm`` address a session by any of the
    three forms (x-1b1e) while still locking + acting on the canonical name.

    Falls back to the token UNCHANGED on any resolution miss (unknown, ambiguous,
    unreadable registry), so the downstream name lookup raises its familiar
    ``agent {name!r} not found in registry`` error rather than a second variant -
    the not-found/exit-2 contract the lifecycle tests pin stays intact."""
    from fno.agents.registry import AgentResolutionError, resolve_agent

    try:
        return resolve_agent(token, path=registry_path).entry.name
    except AgentResolutionError:
        return token


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
    for entry in entries:
        if entry.name == name:
            return entry
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
    # passed out of this scope; we re-read post-lock below so any
    # concurrent rm/recreate between the two reads is absorbed. The
    # ``registry_path`` override (Codex P2 on PR #317) forwards to BOTH
    # the lock acquisition AND the registry read so a test or future
    # caller cannot accidentally lock one file while reading another.
    if registry_path is None:
        registry_path = paths.agents_registry_path()
    _resolve_registry_entry(name, registry_path=registry_path)
    with hold_agent_lock(name, registry_path, timeout=timeout, on_wait=on_wait) as lock_handle:
        # Post-lock re-read. If another process deleted the entry between
        # the pre-flock validation and the flock acquisition, this raises
        # the SAME DispatchAskError shape the pre-flock path would have,
        # propagated INSIDE the with-block so the caller's exit path is
        # the regular AgentLockTimeout/exception flow rather than the
        # missing-entry flow. The lock is released as the context
        # manager unwinds (AC2-ERR).
        existing = _resolve_registry_entry(name, registry_path=registry_path)
        yield (lock_handle, existing)


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
    # Accept any of the three address forms (x-1b1e): translate to the canonical
    # name before the flock, which keys on it.
    name = _canonical_agent_name(name)
    # Pre-flock fast-fail + capture provider for the lock-timeout event
    # payload. The authoritative load happens inside
    # ``with_agent_lock_and_entry`` below; this pre-read exists ONLY so
    # the AgentLockTimeout branch can name the provider in its event
    # emit. The lint script `scripts/lint-flock-pattern.sh` allows this
    # because we do NOT call ``hold_agent_lock`` directly in this function
    # body — the helper encapsulates the lock acquisition.
    pre_existing = _resolve_registry_entry(name)
    pre_provider = pre_existing.harness

    def _on_wait() -> None:
        print(f"Waiting for agent {name!r} lock...", file=sys.stderr, flush=True)

    try:
        with with_agent_lock_and_entry(name, timeout=lock_timeout, on_wait=_on_wait) as (
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

            short_id = existing.short_id
            if not short_id:
                raise DispatchAskError(
                    f"registry entry {name!r} has no short id on file; "
                    f"cannot stop. Run 'fno agents rm {name}' to clear.",
                    exit_code=12,
                )

            if not is_provider_available("claude"):
                raise DispatchAskError("claude CLI not on PATH", exit_code=14)

            from fno.agents.providers import claude as claude_mod

            try:
                exit_code, stderr_text = claude_mod.claude_stop(short_id, timeout=shellout_timeout)
            except FileNotFoundError as exc:
                # PATH check passed above but claude vanished mid-call; treat
                # the same as not-on-PATH to mirror US1's contract.
                raise DispatchAskError("claude CLI not on PATH", exit_code=14) from exc
            except subprocess.TimeoutExpired as exc:
                events.emit(
                    "agent_stopped",
                    name=name,
                    provider="claude",
                    claude_exit=None,
                    timed_out=True,
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

            events.emit(
                "agent_stopped",
                name=name,
                provider="claude",
                claude_exit=exit_code,
                short_id=short_id,
            )

            if exit_code != 0:
                if stderr_text:
                    sys.stderr.write(stderr_text)
                    if not stderr_text.endswith("\n"):
                        sys.stderr.write("\n")
                raise DispatchAskError(
                    f"claude stop {short_id} exited {exit_code}",
                    exit_code=1,
                )

            # Success: flip registry status to "orphaned" so
            # ``fno agents list`` reflects the stop immediately rather
            # than carrying the stale ``live`` value until the next
            # reconcile. ``last_message_at_preserve=True`` keeps the
            # historical timestamp — the stop doesn't invalidate it.
            # Per the handoff item, "orphaned" matches the post-reconcile
            # state, so this pre-empts the eventual reconcile without
            # introducing a new status value (no schema bump).
            try:
                update_registry(
                    _stamp_status(
                        name,
                        status="orphaned",
                        last_message_at_preserve=True,
                    ),
                )
            except (OSError, RegistryVersionError):
                # The stop subprocess already succeeded; a registry-write
                # failure here is logged via the same events stream the
                # caller already has open, but must not turn a successful
                # stop into a raised error. The next reconcile will pick
                # up the orphan state via the live reachability probe.
                events.emit(
                    "agent_stopped_status_write_failed",
                    name=name,
                    provider="claude",
                )

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
            f"lock timeout for agent {name!r} after {exc.timeout}s",
            exit_code=11,
        ) from exc


def _teardown_harness_session(
    existing: AgentEntry,
    *,
    name: str,
    force: bool,
) -> None:
    """Delete a non-claude agent's record from its own harness store.

    Record-only: the harness's index record goes, the conversation stays.
    Upholds the ordering invariant by raising before the caller touches
    the registry -- unless ``force``, which downgrades every failure to a
    stderr WARN naming the orphan so the operator can clean it later.

    An already-absent harness record is success, not an error: a manually
    cleaned store must not wedge ``fno agents rm``.

    opencode is registry-only because it has no record-only teardown at
    all; see :mod:`fno.agents.providers.opencode`.
    """
    harness = existing.harness
    sid = existing.harness_session_id

    def _fail(message: str, *, exit_code: int) -> None:
        # Emit on BOTH paths: a forced removal is exactly the case a later
        # registry-vs-store diff has to explain, so it must leave a greppable
        # record, not just a stderr line the operator saw once.
        events.emit(
            "agent_removed",
            name=name,
            provider=harness,
            force=force,
            registry_changed=force,
            teardown_error=message,
        )
        if not force:
            raise DispatchAskError(message, exit_code=exit_code)
        sys.stderr.write(
            f"WARN: {message}; --force given, removing registry only. "
            f"Orphan {harness} session record: {sid}\n"
        )

    if harness == "opencode":
        # No record-only teardown exists for opencode: removing the session
        # would take its child sessions and full message history with it.
        # Registry-only, and say so rather than implying nothing was left.
        from fno.agents.providers import opencode as opencode_mod

        if sid:
            print(opencode_mod.REGISTRY_ONLY_NOTE.format(sid=sid), flush=True)
        return

    if not sid:
        # Refuse rather than assume there is nothing to clean: the harness
        # record may well exist, and this row simply lost the id that
        # addresses it. Silently dropping the row would orphan it for good.
        _fail(
            f"registry entry has no {harness} session id on file; cannot "
            "tear down the harness record. Re-run with --force to drop the "
            "registry row anyway.",
            exit_code=12,
        )
        return

    if harness == "codex":
        from fno.agents.providers import codex as codex_mod

        try:
            removed = codex_mod.remove_session_index_entry(sid)
        except ValueError as exc:
            _fail(str(exc), exit_code=12)
            return
        except OSError as exc:
            _fail(f"codex session index rewrite failed: {exc}", exit_code=1)
            return
        print(
            f"torn down: codex session index entry {sid}"
            if removed
            else f"already gone: codex session index entry {sid}",
            flush=True,
        )
        return

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
    # Accept any of the three address forms (x-1b1e); lock on the canonical name.
    name = _canonical_agent_name(name)
    # Pre-flock fast-fail + capture provider for lock-timeout event
    # payload. See ``stop_agent`` for the lint-pattern rationale: the
    # body does NOT call ``hold_agent_lock`` directly — that lives inside
    # ``with_agent_lock_and_entry``, which the lint script allowlists.
    pre_existing = _resolve_registry_entry(name)
    pre_provider = pre_existing.harness

    def _on_wait() -> None:
        print(f"Waiting for agent {name!r} lock...", file=sys.stderr, flush=True)

    try:
        with with_agent_lock_and_entry(name, timeout=lock_timeout, on_wait=_on_wait) as (
            _lock_handle,
            existing,
        ):
            claude_exit: Optional[int] = None

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

                    from fno.agents.providers import claude as claude_mod

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
                _teardown_harness_session(
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

            try:
                update_registry(lambda entries: [e for e in entries if e.name != name])
            except (OSError, RegistryVersionError) as exc:
                events.emit(
                    "agent_removed",
                    name=name,
                    provider=existing.harness,
                    claude_exit=claude_exit,
                    force=force,
                    registry_changed=False,
                    error=str(exc),
                    error_type=type(exc).__name__,
                )
                raise DispatchAskError(
                    f"registry write failed: {exc}",
                    exit_code=12,
                ) from exc

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
            f"lock timeout for agent {name!r} after {exc.timeout}s",
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
    - **gemini**: skipped with ``reason=us4-gemini-not-shipped`` until
      US4-gemini lands (Locked Decision 11).

    Emits ``reconcile_done`` once at the end with the aggregate counts.
    """
    try:
        entries = load_registry()
    except (OSError, ValueError, RegistryVersionError) as exc:
        raise DispatchAskError(
            f"registry read failed: {exc}",
            exit_code=12,
        ) from exc

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
    pending_updates: dict[str, AgentEntry] = {}

    # Read codex's session index ONCE outside the loop so a registry with
    # N codex agents only pays the I/O cost once. Mirror the same one-shot
    # capability check for claude so a host without `claude` on PATH does
    # NOT mass-flip every claude row to orphaned (sigma-review C1 finding:
    # the false-orphan storm is the worst kind of silent failure — it
    # rewrites the registry on insufficient evidence).
    from fno.agents.providers import codex as codex_mod

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
        if entry.harness == "gemini":
            # Wave 3.3: gemini reachability via the cwd-pinned chats dir
            # at ~/.gemini/tmp/<cwd-basename>/chats/session-*-<short>.jsonl
            # (Wave 2.0 layout discovery).
            if not entry.harness_session_id:
                events.emit(
                    "agent_inconsistent",
                    name=entry.name,
                    provider="gemini",
                )
                errors.append(
                    {
                        "name": entry.name,
                        "provider": "gemini",
                        "id": None,
                        "reason": "missing-gemini-session-id",
                    }
                )
                continue
            if not entry.cwd:
                # Defensive: gemini sessions are cwd-pinned (Wave 2.0 OQ1).
                # An entry without a cwd cannot be probed deterministically.
                events.emit(
                    "agent_inconsistent",
                    name=entry.name,
                    provider="gemini",
                )
                errors.append(
                    {
                        "name": entry.name,
                        "provider": "gemini",
                        "id": entry.harness_session_id,
                        "reason": "missing-gemini-cwd",
                    }
                )
                continue

            from fno.agents.providers import gemini as gemini_mod

            try:
                reachable = gemini_mod.gemini_session_reachable(
                    entry.harness_session_id, Path(entry.cwd)
                )
            except ReachabilityProbeError as exc:
                # Tri-state inconclusive (AC8-FR): preserve status, route
                # to errors. Mirrors the codex / claude treatments — a
                # PermissionError on ~/.gemini/tmp/ or a missing chats
                # dir does NOT mass-flip every gemini agent to orphaned.
                events.emit(
                    "agent_inconsistent",
                    name=entry.name,
                    provider="gemini",
                    reason=exc.reason,
                )
                errors.append(
                    {
                        "name": entry.name,
                        "provider": "gemini",
                        "id": entry.harness_session_id,
                        "reason": f"gemini-probe-failed: {exc.reason}",
                    }
                )
                continue
            new_status = "live" if reachable else "orphaned"

        elif entry.harness == "codex":
            if codex_index_state != "ready":
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
                        "provider": "codex",
                        "id": entry.harness_session_id,
                        "reason": reason,
                    }
                )
                continue
            if not entry.harness_session_id:
                # Registry corruption: a codex row should always carry its
                # session id (US4-codex AC1-HP invariant). Surface but do
                # not mutate - mark as inconsistent for manual triage.
                events.emit(
                    "agent_inconsistent",
                    name=entry.name,
                    provider="codex",
                )
                errors.append(
                    {
                        "name": entry.name,
                        "provider": "codex",
                        "id": None,
                        "reason": "missing-codex-session-id",
                    }
                )
                continue

            reachable = entry.harness_session_id in known_codex_ids
            new_status = "live" if reachable else "orphaned"

        elif entry.harness == "claude":
            if not claude_path_present:
                # Mirror the codex-index-missing pattern: when claude is
                # not installed we cannot probe reachability, so we route
                # the entry to `errors` with status untouched. Anything
                # else would mass-flip every claude row to orphaned on a
                # host where claude was removed mid-day.
                errors.append(
                    {
                        "name": entry.name,
                        "provider": "claude",
                        "id": entry.short_id,
                        "reason": "claude-cli-not-on-path",
                    }
                )
                continue
            if not entry.short_id:
                events.emit(
                    "agent_inconsistent",
                    name=entry.name,
                    provider="claude",
                )
                errors.append(
                    {
                        "name": entry.name,
                        "provider": "claude",
                        "id": None,
                        "reason": "missing-claude-short-id",
                    }
                )
                continue

            from fno.agents.providers import claude as claude_mod

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
                        "provider": "claude",
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
                    pending_backfill[entry.name] = (entry.short_id, healed)
                    backfilled.append(
                        {
                            "name": entry.name,
                            "provider": "claude",
                            "harness_session_id": healed,
                        }
                    )

        else:
            errors.append(
                {
                    "name": entry.name,
                    "provider": entry.harness,
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
        pending_updates[entry.name] = replace(entry, status=new_status)

        change = {
            "name": entry.name,
            "provider": entry.harness,
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
    if pending_updates or pending_backfill:

        def _apply(current_entries: list[AgentEntry]) -> list[AgentEntry]:
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
                    updates["status"] = pending_updates[e.name].status
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
            failed_names = set(pending_updates.keys())
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
    future abi-owned supervisor) as the planned landing for cross-
    provider attach (Locked Decision 13).

    NO per-agent flock is acquired (Locked Decision 8b): attach holds
    the terminal for indefinite human time and locking would deadlock
    every concurrent stop / rm / ask. claude's own supervisor handles
    concurrent attach safety natively.
    """
    _validate_lifecycle_name(name)
    existing = _resolve_registry_entry(name)

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

    from fno.agents.providers import claude as claude_mod

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
# (1:1 mapping; see providers/claude.py module-level note). The value
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
        # providers/claude.py module note). A follow-up will swap in
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
# (SWITCHBOARD_TURN_TIMEOUT_MS=120s) so a real reply is not cut short. The
# demote/probe case answers in well under a second (a failed stream.ping), so
# normal sends to non-stream sessions are not slowed materially.
_SWITCHBOARD_READ_TIMEOUT = 130.0
# A SHORT connect timeout: every claude send now tries the switchboard first, so
# a DOWN/wedged daemon must not tax the common (non-stream) path — it should fail
# the connect fast and demote, rather than burn the 3s default before the
# existing MCP/socket path runs.
_SWITCHBOARD_CONNECT_TIMEOUT = 1.0


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


def _run_relay_loop(
    to_name: str,
    from_name: str,
    seed: str,
    ceiling: int,
    mail_ctxs: "Optional[dict[str, _MailCtx]]" = None,
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
        hop = _daemon_rpc(
            "agent.switchboard",
            {
                "to": target,
                "from": peer,
                # Wrap each continuation in the sending peer's <fno_mail> so the
                # relay turn carries provenance, not just the seed (node x-1f23).
                "body": _wrap_relay_body(cur, (mail_ctxs or {}).get(peer)),
                "mirror": False,
            },
            connect_timeout=_SWITCHBOARD_CONNECT_TIMEOUT,
            read_timeout=_SWITCHBOARD_READ_TIMEOUT,
        )
        if not isinstance(hop, dict) or hop.get("delivered") is not True:
            # peer is not a live stream thread (one-way) or a daemon hiccup;
            # the exchange ends here — B already received the original body.
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


def _detach_stdio() -> None:
    """Redirect fd 0/1/2 to /dev/null so a detached relay cannot wedge on (or
    spew to) a closed terminal."""
    import os

    try:
        devnull = os.open(os.devnull, os.O_RDWR)
    except OSError:
        # fd limits / permissions: nothing we can redirect to. The detached
        # grandchild proceeds without redirection rather than crashing on an
        # unbound `devnull` (gemini review).
        return
    for fd in (0, 1, 2):
        try:
            os.dup2(devnull, fd)
        except OSError:
            pass
    if devnull > 2:
        try:
            os.close(devnull)  # the dup2'd copies remain; don't leak the original
        except OSError:
            pass


def _kickoff_background_relay(
    to_name: str,
    from_name: str,
    seed: str,
    ceiling: int,
    mail_ctxs: "Optional[dict[str, _MailCtx]]" = None,
) -> None:
    """Run the A2A relay in a DETACHED background process so the caller returns
    immediately (ab-3bd520ab).

    The relay is autonomous — no human waits on it — so blocking the
    ``fno mail send`` caller for up to ``turn_ceiling × 130s`` was pure
    latency. The send's actual delivery (hop 1: B received the message) already
    happened synchronously in :func:`_switchboard_exchange`; this only continues
    the autonomous A<->B exchange. Double-fork + ``setsid`` so the relay outlives
    the short-lived CLI process and reparents to init (no zombie). A fork failure
    degrades to running the relay INLINE (blocking, but the turns still happen)
    rather than dropping them.
    """
    import os

    try:
        pid = os.fork()
    except OSError:
        _run_relay_loop(to_name, from_name, seed, ceiling, mail_ctxs)
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
        except OSError:
            grandchild = 0  # fork failed; run the relay in THIS child
        if grandchild > 0:
            os._exit(0)
        _detach_stdio()
        try:
            _run_relay_loop(to_name, from_name, seed, ceiling, mail_ctxs)
        except Exception:
            pass
    finally:
        # _exit (not sys.exit) so the child never runs atexit handlers or flushes
        # the parent's buffers a second time.
        os._exit(0)


def _a2a_first_use_gate(auto: bool, ceiling: int) -> bool:
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
        answer = sys.stdin.readline().strip().lower()
    except Exception:
        return False  # cannot read an answer -> conservative
    keep_on = answer in ("", "y", "yes")

    try:
        from fno.config.writer import set_config_value

        set_config_value("config.agents.a2a.auto", "true" if keep_on else "false", scope="global")
    except Exception:
        pass  # best-effort persist; the marker below still prevents re-asking.
    try:
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text("answered\n", encoding="utf-8")
    except OSError:
        pass
    return keep_on


def _switchboard_exchange(
    to_name: str,
    from_name: str,
    body: str,
    mail_ctxs: "Optional[dict[str, _MailCtx]]" = None,
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
    runs synchronously so the delivered/demote decision is exact. When
    ``config.agents.a2a.auto`` is True (the default) the bounded autonomous relay
    that follows (drive A with B's reply, then B with A's reply, ... up to
    ``config.agents.a2a.turn_ceiling`` total turns) is kicked off in a DETACHED
    background process and the caller returns ``True`` immediately (ab-3bd520ab) —
    it no longer blocks for up to ``turn_ceiling × 130s``. When ``auto`` is False,
    a single OBSERVED hop drives B and mirrors B's reply into A's view, with no
    autonomous relay.
    """
    auto, ceiling = _load_a2a_settings()
    # US6 (ab-098967b4): the first-use confirm gates the first autonomous hop.
    # On a no / headless / unconfirmed gate this downgrades to observed mode, so
    # the hop below runs as a single mirrored hop with no autonomous relay.
    auto = _a2a_first_use_gate(auto, ceiling)
    # First hop: drive B. In observed mode (auto off) ask the daemon to mirror
    # B's reply into A's view; in auto mode the relay's next hop injects it (so
    # mirror=False avoids a double-injection).
    sb = _daemon_rpc(
        "agent.switchboard",
        {"to": to_name, "from": from_name, "body": body, "mirror": not auto},
        connect_timeout=_SWITCHBOARD_CONNECT_TIMEOUT,
        read_timeout=_SWITCHBOARD_READ_TIMEOUT,
    )
    if sb is None or sb.get("delivered") is not True:
        return None  # not a live stream thread / daemon down -> caller demotes
    if not auto:
        return True  # observed: one hop, B's reply mirrored into A

    # A2A relay: kick off the remaining alternating hops in the background so the
    # caller is not blocked for the whole exchange. A self-send (from == to) or an
    # empty first reply has no relay to run.
    cur = sb.get("reply") or ""
    if ceiling > 1 and from_name != to_name and cur.strip():
        _kickoff_background_relay(to_name, from_name, cur, ceiling, mail_ctxs)
    return True


# Subprocess budget for the mail-inject verb. It polls the recipient transcript
# for ~10s (40 * 250ms) before reporting not-confirmed; give it headroom.
_MAIL_INJECT_TIMEOUT_S = 20.0


@dataclass(frozen=True)
class _MailCtx:
    """Sender identity stamped into the ``<fno_mail>`` envelope (node x-1f23)."""

    from_: str
    harness: str
    model: str
    node: Optional[str] = None
    to: Optional[str] = None


def _build_mail_ctx(
    from_name: str,
    from_session: Optional[str],
    provider_from: Optional[str],
    to: Optional[str] = None,
) -> _MailCtx:
    """Build the ``<fno_mail>`` sender context from the dispatch provenance.

    ``from`` is the sender's short 8-hex sessionId (or the bare ``from_name`` when
    the caller is unregistered). ``model`` is the invoking session's real model,
    resolved from its own transcript store (x-605c); an unresolvable model floors
    to ``"unknown"`` -- never fabricated.

    ``to`` and ``node`` are OPTIONAL envelope attributes (omitted when None).
    ``to`` is the recipient's short id -- set for a directed ``fno mail send`` so
    the recipient can tell a directed turn from a broadcast. ``node`` (the sender's
    backlog node) stays None: dispatch has no truthful source for it today."""
    from fno.agents.self_stamp import resolve_self_model
    from fno.mail.envelope import harness_for_provider

    from_ = from_session.split("-")[0] if from_session else from_name
    return _MailCtx(
        from_=from_,
        harness=harness_for_provider(provider_from),
        model=resolve_self_model(),
        to=to or None,
    )


def _mux_pane_send(entry: "AgentEntry", text: str) -> bool:
    """Live-inject to a mux-hosted agent via ``fno mux pane send``, holding
    the pane's writer claim around the text-then-CR burst. The claim is
    best-effort (an unclaimed pane refuses the acquire; send proceeds), but a
    failed send fails closed -> the caller's durable demotion.
    """
    mux = entry.mux or {}
    session = mux.get("session")
    pane_id = mux.get("pane_id")
    if not session or pane_id is None:
        return False
    fno_bin = os.environ.get("FNO_BIN") or "fno"
    pane = str(pane_id)

    def _run(args: list[str], stdin_text: Optional[str] = None) -> bool:
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
            return False
        if proc.returncode != 0:
            detail = (proc.stderr or "").strip()
            print(
                f"fno mux pane {args[0]} exited {proc.returncode}: {detail}",
                file=sys.stderr,
            )
            return False
        return True

    claimed = _run(["claim", pane, "--pid", str(os.getpid())])
    try:
        if not _run(["send", pane, "--stdin"], stdin_text=text):
            return False
        # PaneSend is bytes; the CR submit waits for the TUI to absorb the paste.
        time.sleep(0.3)
        return _run(["send", pane, "--text", "\r"])
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
    from fno.agents.providers.claude import build_cross_session_container

    mux = existing.mux or {}
    ref = f"{mux.get('session')}:{mux.get('pane_id')}"
    _emit_ev(
        "agent_followup_started",
        name=name,
        provider=existing.harness,
        short_id=ref,
    )
    wrapped = build_cross_session_container(message, from_name)
    if not _mux_pane_send(existing, wrapped):
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


def _mail_inject_claude(recipient: str, text: str) -> bool:
    """Inject ``text`` into a live claude session over the daemon ``control.sock``
    via the ``fno-agents mail-inject`` verb (G1 substrate, node x-1f23).

    Returns True only when the verb confirms the turn landed in the recipient
    transcript; any miss (binary absent, recipient not on the roster, not
    confirmed within the poll budget) returns False so the caller writes the
    durable fallback."""
    import json

    from fno.agents import rust_runtime

    binary = rust_runtime.resolve_installed_binary()
    if binary is None:
        return False
    try:
        proc = subprocess.run(
            [str(binary), "mail-inject", "--session", recipient],
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


def _mail_inject_codex(thread_id: str, text: str) -> bool:
    """Inject ``text`` into a live codex session over the app-server daemon socket
    via the ``fno-agents mail-inject --provider codex`` verb (US8, node x-d899).

    ``thread_id`` is the codex threadId (full UUID). Returns True only when the
    daemon accepts the turn; any miss (binary absent, no daemon socket, thread
    not attached) returns False so the caller writes the durable fallback. The
    codex app-server daemon only exists when the user runs it
    (``codex remote-control start``); absent it this is a clean no-op."""
    import json

    from fno.agents import rust_runtime

    binary = rust_runtime.resolve_installed_binary()
    if binary is None:
        return False
    try:
        proc = subprocess.run(
            [str(binary), "mail-inject", "--provider", "codex", "--session", thread_id],
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


def _deliver_live(
    entry: "AgentEntry",
    body: str,
    from_name: str,
    mail: "Optional[_MailCtx]" = None,
) -> bool:
    """Attempt a single fire-and-forget live delivery (live-inject-first; the
    caller writes the durable fallback when this returns False -- node x-1f23).

    Returns True on success, False when live delivery is not possible or fails
    (not live-reachable, socket error, daemon unreachable, etc.).

    When ``mail`` is set the body is wrapped in the paired ``<fno_mail>`` envelope
    so the recipient sees agent-to-agent structure and the delivered turn is
    self-recording (``grep <fno_mail>`` reconstructs a2a history). Every live
    transport below carries the same wrapped turn.

    For claude peers: the proven ``control.sock`` ``op:'reply'`` inject via the
    ``fno-agents mail-inject`` verb (G1, x-26df) is the live primitive for adopted
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
        )

    # Dual-run dispatch on the row's live ref (4a-G2): a mux-hosted agent gets
    # PaneSend; worker/bg rows keep the legacy lanes below until G4.
    if entry.mux:
        return _mux_pane_send(entry, wrapped)

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
        reason = result.get("reason", "unknown")
        print(
            f"fno-agents deliver demoted: {reason}; message queued durable",
            file=sys.stderr,
        )
        return False

    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers.base import ReachabilityProbeError

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
    if _switchboard_exchange(entry.name, from_name, wrapped, relay_ctxs):
        return True

    # MCP-channel probe (mirrors _followup_path :334-366).
    if entry.mcp_channel_id:
        try:
            mcp_alive = claude_mod.mcp_channel_reachable(entry.mcp_channel_id, timeout=0.25)
        except ReachabilityProbeError:
            mcp_alive = False
        if mcp_alive:
            try:
                # Fire-and-forget via the MCP sidecar: the send-only half of
                # ask_followup_via_mcp (build notification + push to channel),
                # WITHOUT its wait_for_reply poll - send never blocks for a
                # reply (codex #459 P2: the old call used nonexistent kwargs
                # and would also have blocked polling for a reply).
                from fno.mcp import build_channel_notification
                from fno.mcp import client as _mcp_client

                envelope = build_channel_notification(
                    content=wrapped,
                    meta={
                        "source": "fno",
                        "from_name": from_name,
                        "session_id": entry.mcp_channel_id,
                    },
                )
                _mcp_client.send_to_channel(entry.mcp_channel_id, envelope)
                return True
            except Exception:
                pass  # fall through to socket path

    # Live inject over control.sock (adopted `claude --bg`, the fno-agents
    # mail-inject verb, G1; node x-1f23). The claude PTY worker.sock lane retired
    # with daemon PTY hosting (x-f54c), so every live claude row now carries an
    # empty plain `short_id` -- the worker lane can no longer resolve a mail
    # recipient, leaving control.sock the sole live path (x-3dac). The mail-inject
    # verb resolves the handle itself via ClaudeRoster (accepts the full session
    # uuid or 8-hex short id) and returns False (-> durable) when not reachable.
    recipient = entry.harness_session_id or entry.short_id
    if not recipient:
        return False
    return _mail_inject_claude(recipient, wrapped)


def dispatch_send(
    name: str,
    message: str,
    provider: Optional[str],
    cwd: "Path",
    lock_timeout: float = _DEFAULT_LOCK_TIMEOUT,
    from_name: str = _FROM_NAME_DEFAULT,
) -> "DispatchSendResult":
    """Dispatch an async ``send`` to an already-registered agent.

    Live-inject-first (node x-1f23): live delivery is attempted FIRST and the
    durable inbox envelope is written ONLY when the recipient is not
    live-reachable or the live inject does not confirm. A confirmed live
    (``hosted``) send is self-recording in the transcript and is NOT also queued;
    the durable bus is the offline fallback tier. Both the live turn and the
    durable body carry the same ``<fno_mail>`` envelope.

    Orchestration:

    1. Validate name / message / from_name (same rules as dispatch_ask).
    2. Reject bodies over 1 MiB (exit 2) BEFORE any store write.
    3. Acquire per-agent flock (hold_agent_lock) with timeout (exit 11).
    4. INSIDE the flock:
       a. Load registry; unknown name -> exit 16 (same message as ask).
       b. Provider mismatch -> exit 2.
       c. Capture sender provenance + build the <fno_mail> ctx; generate msg_id.
       d. Attempt live delivery via _deliver_live (fire-and-forget).
       e. On non-hosted, write the durable fallback envelope (the <fno_mail>
          body), kind=send, recipient=name.
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

    # 2. Body size cap (exit 2 BEFORE any write).
    if len(message.encode("utf-8")) > _SEND_MAX_BODY_BYTES:
        raise DispatchAskError(
            f"message body exceeds maximum size "
            f"({_SEND_MAX_BODY_BYTES // 1024 // 1024} MiB); "
            f"got {len(message.encode('utf-8'))} bytes",
            exit_code=2,
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
            name,
            registry_path,
            timeout=lock_timeout,
            on_wait=_on_wait,
        ):
            # 4a. Load registry under the lock.
            try:
                entries = load_registry()
            except (OSError, ValueError, RegistryVersionError) as exc:
                events.emit(
                    "agent_send_failed",
                    stage="registry-read",
                    name=name,
                )
                raise DispatchAskError(
                    f"registry read failed: {exc}",
                    exit_code=12,
                ) from exc

            existing = next((e for e in entries if e.name == name), None)

            # 4a (cont). Unknown-agent guard: send never creates.
            if existing is None:
                events.emit(
                    "agent_send_failed",
                    stage="unknown-name",
                    name=name,
                )
                raise DispatchAskError(
                    f"unknown agent {name!r}; spawn it first: "
                    f"fno agents spawn {name} -p <provider>",
                    exit_code=UNKNOWN_AGENT_EXIT_CODE,
                )

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
            from fno.inbox.store import generate_msg_id, write_new_thread

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
            # A `fno mail send <name>` is always directed -> stamp the recipient's
            # short id as the envelope `to` (node x-1f23: optional, set when known).
            mail_ctx = _build_mail_ctx(
                from_name,
                from_session,
                provider_from,
                to=(existing.short_id or None),
            )
            msg_id = generate_msg_id()

            def _write_durable() -> None:
                """Write the durable FALLBACK envelope: the pending-queue for an
                offline recipient, or the recovery record when a live inject did
                not land. The jsonl bus is the fallback tier now, not a peer to the
                live path (node x-1f23). Drain-on-wake semantics are unchanged.

                The body is stored <fno_mail>-wrapped, the SAME envelope the live
                path injects, so a delivered message carries one consistent wire
                form everywhere and `grep <fno_mail>` reconstructs durable history
                too (codex peer P1). The wrapped body round-trips through the
                thread render unchanged (no unwrap, so mark_thread_read does not
                strip it); summaries surface the open tag, which identifies the
                message as a2a from its `from` sender."""
                durable_body = message
                if mail_ctx is not None:
                    from fno.mail.envelope import wrap_fno_mail

                    durable_body = wrap_fno_mail(
                        message,
                        from_=mail_ctx.from_,
                        harness=mail_ctx.harness,
                        model=mail_ctx.model,
                        node=mail_ctx.node,
                        to=mail_ctx.to,
                    )
                try:
                    write_new_thread(
                        recipient=name,
                        sender=from_name,
                        kind="send",
                        body=durable_body,
                        msg_id=msg_id,
                        to_kind="name",
                        provider_to=existing.harness,
                        provider_from=provider_from,
                        from_session=from_session,
                    )
                except (OSError, ValueError, RuntimeError) as exc:
                    events.emit(
                        "agent_send_failed",
                        stage="envelope-write",
                        name=name,
                    )
                    raise DispatchAskError(
                        f"durable envelope write failed: {exc}",
                        exit_code=12,
                    ) from exc

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

                delivery = "durable"
                demotion_notice: Optional[str] = None

                if existing.status == "live" and _deliver_live(
                    existing, message, from_name, mail_ctx
                ):
                    delivery = "hosted"
                else:
                    # Durable fallback: an offline recipient, or a live inject that
                    # did not confirm. Persist ONLY here so a CONFIRMED live turn is
                    # not also queued. At-most-once on the common path; a busy
                    # recipient whose injected turn is queued past the verb's confirm
                    # budget can still receive the durable copy too (bounded
                    # double-delivery -- see mail_inject.rs). Live-first also widens
                    # the crash-loss window vs the old durable-first; both are
                    # accepted tradeoffs of the live-inject-first design (node
                    # x-1f23). A live peer that fell through gets a demotion notice.
                    _write_durable()
                    if existing.status == "live":
                        demotion_notice = (
                            f"live delivery failed for {name!r}; message queued durable ({msg_id})"
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

            # 4f. Bump registry stamps (best-effort; not fatal if registry
            # write fails here since envelope is already durable).
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

                update_registry(_stamp, path=registry_path)
            except (OSError, ValueError, RegistryVersionError):
                pass  # envelope is durable; stamp failure is non-fatal

            return DispatchSendResult(msg_id=msg_id, delivery=delivery)

    except AgentLockTimeout as exc:
        events.emit(
            "agent_send_failed",
            stage="lock-timeout",
            name=name,
        )
        raise DispatchAskError(
            f"timed out waiting for agent {exc.name!r} lock (timeout={exc.timeout}s)",
            exit_code=11,
        ) from exc


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
        result = dispatch_send(
            name=res.recipient,
            message=message,
            provider=provider,
            cwd=cwd,
            lock_timeout=lock_timeout,
            from_name=from_name,
        )
        return replace(result, recipient=res.recipient, to_project=project)

    # No live peer: durable queue addressed to the project itself. The envelope
    # (and bus mirror) record to == project (to_kind=project); the next drain in
    # that project picks it up, EXCLUDING the sender (Group 1, ab-ba91b807). The
    # sender identity is best-effort - exclusion falls back to the from_ name.
    from fno.inbox.store import write_new_thread

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

    try:
        handle = write_new_thread(
            recipient=project,
            sender=from_name,
            kind="send",
            body=message,
            to_kind="project",
            from_session=from_session,
            provider_from=provider_from,
        )
    except (OSError, ValueError, RuntimeError) as exc:
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
