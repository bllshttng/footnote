"""fno.agents.events — JSONL event emitter for agent dispatch.

Phase 1 minimum: append one well-formed JSON line per call to
``~/.fno/events.jsonl`` (resolved via ``paths.state_dir()``). The
schema is open-ended — every call carries ``ts`` (ISO8601 UTC) and
``kind`` (string), plus arbitrary keyword data that flattens into the
top-level JSON object.

The existing project-level ``fno.events`` module is intentionally
NOT reused — it carries schema validation, mkdir-mutex locking, and
provenance bindings tied to target sessions, none of which are needed
for cross-CLI agent dispatch events. Keeping the agents emitter minimal
keeps the substrate decoupled.

Event kind constants
====================

Callers SHOULD reference ``KIND_*`` constants instead of inlining string
literals. The kinds are open-set (new code may emit new kinds without
touching this module) but the canonical names live here so a single
grep surfaces every event the dispatch/provider layers can produce.

Phase 5 (MCP channel + streaming) additions are grouped at the bottom.
"""
from __future__ import annotations

import json
import sys
from dataclasses import asdict
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Any, Optional

from fno import paths

if TYPE_CHECKING:
    from fno.agents.context import EventContext


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def emit(kind: str, *, path: Optional[Path] = None, **data: Any) -> None:
    """Append one well-formed JSON line to the agents events log.

    Telemetry emission is best-effort: an ``OSError`` (disk full,
    permission denied, parent dir unwritable) is logged to stderr and
    swallowed so a failed log write cannot break the primary dispatch.

    Args:
        kind: Event kind (e.g. ``agent_ask_started``, ``agent_ask_done``).
        path: Override the events file path. Defaults to
            ``paths.state_dir() / "events.jsonl"``.
        **data: Arbitrary keyword fields that flatten into the JSON object
            alongside ``ts`` and ``kind``.
    """
    target = path if path is not None else (paths.state_dir() / "events.jsonl")
    # Put ts and kind LAST so a stray data={"ts": ..., "kind": ...} kwarg
    # cannot overwrite the canonical fields. The dict's order-preserving
    # right-to-left merge gives the mandatory fields final say.
    record = {**data, "ts": _utc_now_iso(), "kind": kind}
    line = json.dumps(record, sort_keys=False, separators=(",", ":")) + "\n"
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        # 'a' mode is atomic for single writes <= PIPE_BUF (4096 on
        # macOS / Linux); a single JSONL record is well under that, so
        # concurrent emit() calls interleave at line boundaries.
        with open(target, "a", encoding="utf-8") as fh:
            fh.write(line)
    except OSError as exc:
        print(
            f"fno agents: warning: events.emit({kind!r}) to {target}: {exc}",
            file=sys.stderr,
        )


def emit_with_context(
    ctx: "EventContext",
    kind: str,
    *,
    path: Optional[Path] = None,
    **data: Any,
) -> None:
    """Append a JSONL record carrying every ``EventContext`` field.

    Flattens the 13 fields of ``ctx`` onto the record, then layers open
    ``**data`` kwargs on top (caller-overrides; Locked Decision #1), then
    delegates to ``emit()`` which pins ``ts``/``kind`` last.

    Delegating to ``emit`` (rather than writing the JSONL directly) means
    a test that monkeypatches ``events.emit`` to observe records will
    pick up emits routed through this function too — no per-test fork
    of the assertion strategy for migrated vs. legacy emit sites.

    Telemetry emission is best-effort: the underlying ``emit()`` swallows
    ``OSError`` and warns to stderr.

    Args:
        ctx: Per-dispatch sender + recipient + correlation envelope built
            via ``fno.agents.context.build_context``.
        kind: Event kind constant (e.g. ``agent_ask_started``).
        path: Override the events file path. Defaults to
            ``paths.state_dir() / "events.jsonl"``.
        **data: Arbitrary keyword fields. Overrides ctx fields when keys
            collide; ``ts`` and ``kind`` are still pinned last by emit().
    """
    ctx_dict = asdict(ctx)
    merged: dict[str, Any] = {**ctx_dict, **data}
    emit(kind, path=path, **merged)


# ---------------------------------------------------------------------
# Phase 5 — MCP channel + streaming event kinds
# ---------------------------------------------------------------------

# MCP channel lifecycle.
KIND_MCP_CHANNEL_REGISTERED = "mcp_channel_registered"
KIND_MCP_CHANNEL_UNREACHABLE = "mcp_channel_unreachable"
KIND_MCP_CHANNEL_DEMOTED_TO_SOCKET = "mcp_channel_demoted_to_socket"
KIND_MCP_CHANNEL_ENVELOPE_DRIFT = "mcp_channel_envelope_drift"
KIND_MCP_SERVER_UNREACHABLE = "mcp_server_unreachable"
KIND_AGENT_ASK_DONE = "agent_ask_done"  # extended in Phase 5 with backend=...

# Streaming surface.
KIND_AGENT_ASK_STREAMING_STARTED = "agent_ask_streaming_started"
KIND_AGENT_ASK_STREAMING_CHUNK = "agent_ask_streaming_chunk"
KIND_AGENT_ASK_STREAMING_COMPLETED = "agent_ask_streaming_completed"
KIND_AGENT_ASK_STREAMING_CANCELLED = "agent_ask_streaming_cancelled"
KIND_STREAMING_VIA_POLLING = "streaming_via_polling"
