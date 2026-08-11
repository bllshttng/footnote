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


# Harness identity resolution. Emitted when resolution was NOT a plain single-
# marker win, so a disagreement or refused collision is reconstructable from
# the event record alone: every marker present, the disposition chosen, the
# resolved harness, and the owning row when a candidate id was refused. The
# originating ambient-leak incident was unrecoverable (the process was gone
# before anyone looked); this record exists so the next one is not.
KIND_HARNESS_IDENTITY_RESOLVED = "harness_identity_resolved"


def emit_identity_resolution(owned: Any, *, path: Optional[Path] = None) -> None:
    """Record a non-trivial harness identity resolution (AC5-CON).

    ``owned`` is an ``OwnedHarnessIdentity`` whose resolution was not a plain
    single-marker win. Carries the resolved session id and every marker's value
    so the resolution (and a leak path) is reconstructable from this one record.
    Best-effort: the underlying ``emit()`` swallows OSError.
    """
    emit(
        KIND_HARNESS_IDENTITY_RESOLVED,
        path=path,
        disposition=owned.disposition,
        harness=owned.harness,
        session_id=owned.session_id,
        markers_present=[
            {"marker": marker, "harness": harness, "session_id": value}
            for marker, harness, value in owned.markers_present
        ],
        rejected=[dict(entry) for entry in owned.rejected],
    )


# ---------------------------------------------------------------------
# Spawn-lifecycle births (x-8cd5 Wave 6): deaths already land in the daemon's
# agent-lifecycle log (~/.fno/agents/events.jsonl) — agent_orphan_reaped,
# agent_row_reaped, agent_stopped, agent_removed. A birth that lands anywhere
# else splits the lineage tree across two files, so it is unreconstructible:
# the daemon records every way an agent can END and nothing about how it began.
# These helpers write the birth to the SAME log the daemon writes deaths to, so
# a parent->child->death tree is joinable from one file.
#
# Co-writing the daemon's log from Python is safe: single-line JSONL appends
# are atomic below PIPE_BUF, so concurrent writers (this process and the
# daemon) interleave only at line boundaries. The daemon's advisory flock is
# not held across writes, so not taking it here cannot deadlock or corrupt a
# line.
KIND_AGENT_SPAWNED = "agent_spawned"
KIND_AGENT_SPAWN_FAILED = "agent_spawn_failed"


def daemon_lifecycle_log() -> Path:
    """The daemon's agent-lifecycle log: where births and deaths are joinable."""
    return paths.agents_home_dir() / "events.jsonl"


def _emit_daemon_envelope(
    kind: str, data: dict[str, Any], *, source: str = "python"
) -> None:
    """Write one record in the daemon's unified envelope (x-2901) to the daemon
    lifecycle log.

    The Rust daemon nests the payload under ``data`` and stamps the kind as
    ``type`` (crates/fno-agents/src/events.rs). A Python birth that shared the
    file but used the flat ``{..., ts, kind}`` shape would not be joinable with
    a daemon death by one reader: ``rec["data"]["name"]`` works on the death and
    KeyErrors on the flat birth. Writing the same envelope from both sides is
    what makes the lineage tree reconstructable from a single file.
    Best-effort: OSError is swallowed (a failed log write must not break spawn).
    """
    record = {
        "ts": _utc_now_iso(),
        "type": kind,
        "source": source,
        "data": data,
    }
    line = json.dumps(record, separators=(",", ":")) + "\n"
    try:
        target = daemon_lifecycle_log()
        target.parent.mkdir(parents=True, exist_ok=True)
        with open(target, "a", encoding="utf-8") as fh:
            fh.write(line)
    except OSError as exc:
        print(f"fno agents: warning: daemon envelope {kind}: {exc}", file=sys.stderr)


def emit_spawned(
    *,
    name: str,
    short_id: Optional[str],
    provider: str,
    pid: Optional[int] = None,
    spawned_by_session: Optional[str] = None,
    spawned_by_harness: Optional[str] = None,
    spawned_by_cwd: Optional[str] = None,
) -> None:
    """Record one agent birth in the daemon lifecycle log (envelope format).

    Carries the parent edge the registry row already captures
    (spawned_by_session/harness/cwd), plus the child ``pid`` where the caller
    has it. The pane path in particular leaves the registry row's short_id
    empty (mux is its one live transport ref) and keys the row on pid, so the
    daemon death that reads that row carries pid, not the mux pane_id. A birth
    that recorded the pane_id as short_id could not join that death; recording
    pid (and leaving short_id empty to match the row) makes the birth join the
    death on ``data.name`` and, for the pane path, on ``data.pid``.
    Exactly one per successful create.
    """
    data: dict[str, object] = {
        "name": name,
        "short_id": short_id,
        "provider": provider,
        "spawned_by_session": spawned_by_session,
        "spawned_by_harness": spawned_by_harness,
        "spawned_by_cwd": spawned_by_cwd,
    }
    if pid is not None:
        data["pid"] = pid
    _emit_daemon_envelope(KIND_AGENT_SPAWNED, data)


def emit_spawn_failed(
    *,
    name: str,
    provider: Optional[str] = None,
    short_id: Optional[str] = None,
    reason: str = "",
) -> None:
    """Record a spawn attempt that did not produce a live row.

    The birth's failure counterpart: a spawn that exits via an error path still
    leaves a trace in the daemon log, so a name with a death but no birth is
    distinguishable from a name whose only event is a failed start.
    """
    _emit_daemon_envelope(
        KIND_AGENT_SPAWN_FAILED,
        {"name": name, "provider": provider, "short_id": short_id, "reason": reason},
    )


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

# Autonomous switchboard continuation lifecycle.
KIND_AGENT_RELAY_STOPPED = "agent_relay_stopped"
