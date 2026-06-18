"""fno.agents.context — EventContext envelope + sender attribution.

Single source of truth for "who is calling, how do they appear in event
logs, and what request_id ties this dispatch together." Built once per
dispatch via ``build_context()``; frozen so the same context can be
emitted on entry AND exit without an intermediate race re-reading env or
target-state.

See spec: 2026-05-22-fno-agents-observability.md.

Caller-kind decision tree (locked):

1. ``FNO_AGENT_SELF`` env set -> ``nested_agent`` (parent injected it)
2. ``MCP_CHANNEL_INBOUND_POKE`` env set -> ``mcp_channel``
3. ``.fno/target-state.md`` exists at cwd with ``status: IN_PROGRESS``
   -> ``target_session``  (wired in Task 2.3; this module exposes the
   ``parse_target_session()`` helper now so AC5-ERR/AC5-EDGE are covered)
4. ``CRON_JOB`` env or ``INVOCATION_ID`` env (systemd) -> ``cron``
5. Default -> ``human_cli``

The env-var protocol is the cross-process bridge: each provider's spawn
path injects ``FNO_AGENT_SELF / _PROVIDER / _SESSION`` into the
spawned agent's environment so nested ``fno agents ask`` invocations
attribute back to the parent. We do NOT walk process ancestry — too
brittle across macOS / Linux differences (Locked Decision #3).
"""
from __future__ import annotations

import os
import sys
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

import yaml


# ---------------------------------------------------------------------------
# EventContext dataclass
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class EventContext:
    """Per-dispatch sender + recipient + correlation envelope.

    Frozen so the same context can be emitted on entry AND exit of a
    dispatch without re-reading env vars or target-state.md mid-flight
    (Locked Decision #9: race-safety).
    """

    # Sender (always populated)
    from_name: Optional[str]
    from_provider: Optional[str]
    from_session_id: Optional[str]
    from_cwd: str
    from_pid: int
    caller_kind: str  # one of: human_cli | nested_agent | target_session | mcp_channel | cron

    # Recipient (populated at dispatch time)
    to_name: str
    to_provider: str
    to_cwd: Optional[str]
    to_session_id: Optional[str]
    transport: str  # socket | mcp | direct-cli

    # Correlation
    request_id: str  # 32 lowercase hex chars (UUIDv4, dashes stripped)
    target_session_id: Optional[str]


# ---------------------------------------------------------------------------
# caller_kind_from_env — env-only decision tree
# ---------------------------------------------------------------------------


def caller_kind_from_env() -> str:
    """Resolve caller_kind from env vars only (no target-state.md read).

    Used by ``build_context()`` and also exposed for callers that want
    the env-only signal without paying the target-state file read.

    Returns one of: ``nested_agent | mcp_channel | cron | human_cli``.
    The ``target_session`` discriminator is layered on top by
    ``build_context()`` via ``parse_target_session()`` (Task 2.3).
    """
    if os.environ.get("FNO_AGENT_SELF"):
        return "nested_agent"
    if os.environ.get("MCP_CHANNEL_INBOUND_POKE"):
        return "mcp_channel"
    if os.environ.get("CRON_JOB") or os.environ.get("INVOCATION_ID"):
        return "cron"
    return "human_cli"


# ---------------------------------------------------------------------------
# parse_target_session — read .fno/target-state.md (stale-safe)
# ---------------------------------------------------------------------------


def parse_target_session(cwd: Path) -> Optional[str]:
    """Return ``target-state.md``'s ``session_id`` iff status is IN_PROGRESS.

    Stale state (status: COMPLETE / BLOCKED), missing file, missing
    fields, and malformed YAML all return ``None``. Corrupt YAML logs a
    one-line WARN to stderr (AC5-EDGE); the other degradations are
    silent because they are expected in projects that aren't running a
    target session.

    Args:
        cwd: directory containing ``.fno/target-state.md``.

    Returns:
        The ``session_id`` field if the state file is live (status:
        IN_PROGRESS) and well-formed; ``None`` otherwise.
    """
    state_path = Path(cwd) / ".fno" / "target-state.md"
    if not state_path.is_file():
        return None

    try:
        # errors="replace" so a non-UTF8 byte (BOM, latin-1 paste, stray
        # terminal control sequence) degrades to U+FFFD rather than
        # tearing down the whole dispatch with UnicodeDecodeError.
        # parse_target_session's contract promises "silent None on
        # degraded state"; without replace, this raised through
        # build_context (sigma-review HIGH 1).
        raw = state_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None

    # target-state.md is a markdown file with a YAML frontmatter block
    # bracketed by '---' lines. Extract the frontmatter; bail on
    # malformed shape.
    if not raw.startswith("---"):
        return None
    parts = raw.split("---", 2)
    if len(parts) < 3:
        return None
    front = parts[1]

    try:
        parsed = yaml.safe_load(front)
    except yaml.YAMLError as exc:
        print(
            f"fno agents: warn: corrupt target-state YAML at {state_path}: {exc}",
            file=sys.stderr,
        )
        return None

    if not isinstance(parsed, dict):
        return None
    if parsed.get("status") != "IN_PROGRESS":
        # AC5-ERR: COMPLETE / BLOCKED / anything else => no false provenance.
        return None
    session_id = parsed.get("session_id")
    if not isinstance(session_id, str) or not session_id:
        return None
    return session_id


# ---------------------------------------------------------------------------
# build_context — the factory
# ---------------------------------------------------------------------------


def _new_request_id() -> str:
    """UUIDv4 hex with dashes stripped — matches ``[a-f0-9]{32}`` (AC4-INVARIANT)."""
    return uuid.uuid4().hex


def build_context(
    *,
    to_name: str,
    to_provider: str,
    to_cwd: Optional[str] = None,
    to_session_id: Optional[str] = None,
    transport: str = "direct-cli",
    from_name_override: Optional[str] = None,
) -> EventContext:
    """Assemble an ``EventContext`` for a single dispatch.

    The caller_kind decision tree resolves once here; the resulting
    EventContext is frozen so subsequent emits cannot disagree about
    sender identity even if env vars or target-state.md change mid-flight.

    Args:
        to_name: Recipient agent name.
        to_provider: Recipient provider (claude | codex | gemini).
        to_cwd: Recipient's working directory (None on create-path).
        to_session_id: Recipient provider session id (None on create).
        transport: ``socket | mcp | direct-cli``.
        from_name_override: ``--from-name`` CLI flag value. Ignored when
            ``caller_kind`` is anything other than ``human_cli`` (env
            attribution outranks user override per Locked Decision #3).

    Returns:
        A frozen ``EventContext`` ready to pass to ``emit_with_context``.
    """
    # Full caller_kind decision tree (Task 2.3 wires target-state into
    # build_context with priority 3 per the locked spec):
    #
    #   1. FNO_AGENT_SELF env  -> nested_agent
    #   2. MCP_CHANNEL_INBOUND_POKE  -> mcp_channel
    #   3. target-state.md live       -> target_session  (NEW in 2.3)
    #   4. CRON_JOB / INVOCATION_ID  -> cron
    #   5. default                   -> human_cli
    #
    # nested_agent and mcp_channel short-circuit BEFORE the target-state
    # read; an explicit env attribution outranks a same-cwd target-state.
    # cron is checked AFTER target-state per the locked priority order;
    # caller_kind_from_env() returns "cron" first but this function
    # promotes "cron" to "target_session" when the state file is live.
    env_kind = caller_kind_from_env()

    target_sid: Optional[str]
    if env_kind in ("nested_agent", "mcp_channel"):
        kind = env_kind
        target_sid = None
    else:
        # env_kind is "cron" or "human_cli" — both can be overridden by
        # a live target-state.md (Locked Decision #10: "target_session_id
        # is stamped ONLY when target-state.md's status is IN_PROGRESS").
        # parse_target_session already enforces the IN_PROGRESS gate.
        target_sid = parse_target_session(Path.cwd())
        kind = "target_session" if target_sid is not None else env_kind

    from_name: Optional[str]
    from_provider: Optional[str]
    from_session_id: Optional[str]
    if kind == "nested_agent":
        from_name = os.environ.get("FNO_AGENT_SELF")
        from_provider = os.environ.get("FNO_AGENT_PROVIDER")
        from_session_id = os.environ.get("FNO_AGENT_SESSION")
    elif kind == "target_session":
        # from_name pinned to "target"; from_session_id mirrors the
        # target session id so a single read of either field tells the
        # observer "this dispatch came from target session <id>".
        from_name = "target"
        from_provider = None
        from_session_id = target_sid
    elif kind == "cron":
        from_name = "cron"
        from_provider = None
        from_session_id = None
    elif kind == "mcp_channel":
        # The envelope-meta-driven attribution is layered on by
        # channel_server when it invokes dispatch; here we just stamp
        # the placeholder so the caller can override post-construction
        # if needed. The default keeps from_name None so a downstream
        # consumer can distinguish 'mcp_channel, sender unknown' from
        # an explicit identity.
        from_name = None
        from_provider = None
        from_session_id = None
    else:
        # human_cli default
        from_name = from_name_override or "fno"
        from_provider = None
        from_session_id = None

    return EventContext(
        from_name=from_name,
        from_provider=from_provider,
        from_session_id=from_session_id,
        from_cwd=os.getcwd(),
        from_pid=os.getpid(),
        caller_kind=kind,
        to_name=to_name,
        to_provider=to_provider,
        to_cwd=to_cwd,
        to_session_id=to_session_id,
        transport=transport,
        request_id=_new_request_id(),
        target_session_id=target_sid,
    )
