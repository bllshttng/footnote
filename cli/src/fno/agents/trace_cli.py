"""fno.agents.trace_cli — ``fno agents trace`` subcommand.

Task 3.3 from 2026-05-22-fno-agents-observability.md.

Reads ``~/.fno/events.jsonl`` (or path override) and prints an
interleaved timeline of dispatch lifecycle events for a single agent or
across all agents filtered by ``--request-id``.

Surface:

- ``fno agents trace <name>`` — show all events targeting ``<name>`` as
  the recipient (``to_name``), ordered by timestamp ascending.
- ``fno agents trace --all --request-id <id>`` — show every event tied
  to one logical request (joins across nested-agent chains).
- ``--json`` — emit one JSON object per row; otherwise human-readable.
- ``--limit N`` (default 200) — cap row count.
- ``--since <iso8601>`` — drop rows whose ``ts`` is earlier than ``since``.

Exit codes:
- 0 — success (may print "no events yet" when there are zero rows).
- 13 — agent name is not in the registry AND ``--all`` is not set.

The ``--follow`` (tail mode) and AC4-FR transport-demote markers are
deferred to a follow-up; the present MVP covers AC1-HP / AC1-ERR /
AC1-UI / AC1-EDGE / AC4-EDGE / AC4-UI / AC5-UI.
"""
from __future__ import annotations

import json
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

import typer

from fno import paths


def _parse_iso8601(s: str) -> datetime:
    """Parse an ISO8601 timestamp into a timezone-aware UTC datetime.

    Accepts the canonical ``YYYY-MM-DDTHH:MM:SSZ`` (the abi-stamped shape
    in events.jsonl) as well as ``+00:00`` offsets and fractional
    seconds. Naive timestamps (no tz) are assumed UTC so they compare
    correctly against the aware-UTC stamps the emitter produces.
    Raises ``ValueError`` on unparseable input.
    """
    raw = s.strip()
    if raw.endswith("Z"):
        # datetime.fromisoformat doesn't accept the Z suffix until 3.11+
        # in all forms; normalize to +00:00 for compatibility.
        raw = raw[:-1] + "+00:00"
    dt = datetime.fromisoformat(raw)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


REQUEST_ID_PREFIX_LEN = 8  # AC4-UI: 8-char prefix in default human output.


@dataclass(frozen=True)
class TraceResult:
    """Return shape for the testable trace pipeline (no Typer dep)."""

    exit_code: int
    output: str = ""
    stderr: str = ""


def _read_jsonl(path: Path) -> tuple[list[dict[str, Any]], int]:
    """Read all JSONL records from ``path``.

    Returns ``(records, malformed_count)``. Malformed lines (JSONDecodeError
    or non-dict payload) are silently skipped at the record level, but
    the count is surfaced to the caller so the trace CLI can warn that
    the events.jsonl is degraded (silent-failure-hunter HIGH 2).
    """
    if not path.exists():
        return [], 0
    records: list[dict[str, Any]] = []
    malformed = 0
    # errors="replace" so a single undecodable byte in events.jsonl
    # degrades to U+FFFD on the affected line (which then likely fails
    # JSONDecodeError and lands in the malformed counter) rather than
    # crashing the entire trace command. Same pattern as
    # parse_target_session. Codex P2 caught the prior strict-utf8
    # opening which would abort iteration mid-stream.
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                malformed += 1
                continue
            if isinstance(obj, dict):
                records.append(obj)
            else:
                malformed += 1
    return records, malformed


def _ev_kind(ev: dict[str, Any]) -> str:
    """Event type name: unified ``type``, else the retired flat ``kind``.

    ~/.fno/events.jsonl carries two shapes: the daemon EventEmitter's unified
    ``{type, source, data}`` envelope (x-2901) and the flat agents-audit
    envelope (append_agents_event, unchanged). Drop the ``kind`` fallback once
    the daemon fleet has restarted on the post-x-2901 binary.
    """
    return ev.get("type") or ev.get("kind") or ""


def _ev_field(ev: dict[str, Any], key: str) -> Any:
    """Read a payload field: unified ``data.<key>``, else flat ``<key>``.

    The daemon EventEmitter nests payload fields under ``data``; the flat
    agents-audit envelope keeps them top-level. Reading both renders every line.
    """
    data = ev.get("data")
    if isinstance(data, dict) and key in data:
        return data[key]
    return ev.get(key)


class _RegistryReadError(RuntimeError):
    """Surfaced when load_registry() fails for permission/schema/corruption reasons.

    Distinguishes a real registry-substrate failure (which the operator
    needs to see, not paper over) from a clean "agent not in registry"
    miss (which is the AC1-ERR exit 13 path). Codex P2 caught the prior
    blanket ``except Exception: return False`` that converted both into
    a misleading "not found" message.
    """


def _resolve_registry_name(token: str) -> "str | None":
    """Resolve a trace ``token`` (name | 8-hex short | full session id, x-1b1e)
    to its canonical registry name, or ``None`` when it matches nothing.

    Events key on the name, so the caller filters by the RESOLVED name — trace
    accepts any address form in parity with the Rust binary. ``None`` (unknown /
    ambiguous) makes the caller report the token not-found (exit 13).

    Raises:
        _RegistryReadError: if the registry is unreadable or malformed, so the
            caller can surface the distinct exit 12 instead of a misleading
            "agent not found".
    """
    try:
        from fno.agents.registry import (
            AgentResolutionError,
            RegistryVersionError,
            load_registry,
            resolve_agent_in,
        )
    except ImportError as exc:
        raise _RegistryReadError(f"registry module unavailable: {exc}") from exc
    try:
        entries = load_registry()
    except (OSError, ValueError, RegistryVersionError) as exc:
        raise _RegistryReadError(f"registry load failed: {exc}") from exc
    try:
        return resolve_agent_in(entries, token).entry.name
    except AgentResolutionError:
        return None


def _format_request_id(rid: Optional[str], json_mode: bool) -> str:
    """8-char prefix in default human output; 32-char full in --json (AC4-UI)."""
    if not rid:
        return ""
    if json_mode:
        return rid
    return rid[:REQUEST_ID_PREFIX_LEN]


def trace_logic(
    *,
    name: Optional[str],
    request_id: Optional[str] = None,
    all_agents: bool = False,
    json_out: bool = False,
    limit: int = 200,
    since: Optional[str] = None,
    events_path: Optional[Path] = None,
    registry_check: bool = True,
) -> TraceResult:
    """Pure-function trace pipeline; Typer command wraps this for I/O.

    Args:
        name: Recipient agent name to filter on. ``None`` is only valid
            in combination with ``all_agents=True`` (see AC1-ERR exit 13).
        request_id: Filter rows by exact ``request_id``. Useful with
            ``all_agents=True`` to follow a single logical request
            across nested agents.
        all_agents: Drop the ``to_name=<name>`` filter (cross-agent view).
        json_out: Emit JSON-per-line rather than human-readable rows.
        limit: Cap row count (default 200).
        since: ISO8601 lower bound on ``ts``.
        events_path: Override the default events.jsonl path (tests).
        registry_check: When False, skip the registry membership check
            for ``name`` (tests that don't seed a registry).

    Returns:
        :class:`TraceResult` with ``exit_code`` + captured output text.
    """
    if events_path is None:
        events_path = paths.state_dir() / "events.jsonl"

    # Enforce the command contract: name is required UNLESS --all is set.
    # Without this guard, `fno agents trace` (no args) skipped the
    # recipient filter and returned events for every agent, contradicting
    # the help text. Codex P2 caught the silent fall-through.
    if name is None and not all_agents:
        return TraceResult(
            exit_code=2,
            stderr=(
                "fno agents trace: agent NAME is required unless --all is set\n"
            ),
        )

    # Parse --since into a comparable datetime so non-canonical ISO8601
    # variants (timezone offsets, fractional seconds) compare correctly
    # against the record's ts. Falls back to raw-string compare only when
    # the user explicitly passes a non-ISO string; in that case we
    # degrade-open with a stderr warn rather than fail. Codex P2 caught
    # the prior raw-string filter dropping/including events incorrectly.
    since_dt: Optional[datetime] = None
    since_warn: str = ""
    if since is not None:
        try:
            since_dt = _parse_iso8601(since)
        except ValueError:
            since_warn = (
                f"fno agents trace: warn: --since {since!r} did not parse "
                f"as ISO8601; falling back to raw-string compare\n"
            )

    # AC1-ERR: gate on registry membership unless --all, resolving the token to
    # its canonical name (x-1b1e) so the event filter below matches regardless of
    # the address form. Surface registry read failures distinctly (exit 12) so
    # the operator sees the real cause instead of a misleading "agent not found".
    resolved_name = name
    if name is not None and not all_agents and registry_check:
        try:
            resolved_name = _resolve_registry_name(name)
        except _RegistryReadError as exc:
            return TraceResult(
                exit_code=12,
                stderr=f"fno agents trace: {exc}\n",
            )
        if resolved_name is None:
            return TraceResult(
                exit_code=13,
                stderr=f"fno agents trace: agent {name!r} not found in registry\n",
            )

    events, malformed_count = _read_jsonl(events_path)

    # Filter: by name (when not --all), by request_id, by since.
    def _matches(ev: dict[str, Any]) -> bool:
        if not all_agents and resolved_name is not None:
            # to_name (from EventContext) is the canonical recipient;
            # legacy emits also carry `name` for back-compat. Filter by the
            # resolved canonical name so a short/uuid token matches too.
            recipient = _ev_field(ev, "to_name") or _ev_field(ev, "name")
            if recipient != resolved_name:
                return False
        if request_id is not None:
            if _ev_field(ev, "request_id") != request_id:
                return False
        if since is not None:
            ts = ev.get("ts", "")
            if since_dt is not None:
                # Datetime compare — robust to format variation.
                try:
                    ev_dt = _parse_iso8601(ts)
                    if ev_dt < since_dt:
                        return False
                except ValueError:
                    # Event ts not parseable — keep it (degrade-open).
                    pass
            else:
                # Raw-string fallback (user passed non-ISO --since).
                if ts < since:
                    return False
        return True

    filtered = [e for e in events if _matches(e)]
    # Sort ascending by ts (events.jsonl is append-order which IS ts-
    # order for a single producer; but tests may stitch arbitrary
    # fixtures and concurrent producers may interleave).
    filtered.sort(key=lambda e: e.get("ts", ""))

    # Compute orphans over the FULL filtered set BEFORE applying the
    # limit. Without this, a started/done pair straddling the limit
    # boundary (done at index >= limit, dropped) would falsely flag
    # the surviving started as orphaned. AC4-EDGE.
    seen_done = {
        _ev_field(e, "request_id")
        for e in filtered
        if _ev_kind(e).endswith("_done") and _ev_field(e, "request_id")
    }
    orphan_rids: set[str] = set()
    if not json_out:
        for e in filtered:
            kind = _ev_kind(e)
            rid = _ev_field(e, "request_id")
            if kind.endswith("_started") and rid and rid not in seen_done:
                orphan_rids.add(rid)

    # Apply limit AFTER sort + orphan detection so the cap is "first
    # 200 chronologically", and orphan detection isn't biased by the
    # window boundary.
    filtered = filtered[:limit]

    if not filtered:
        # AC1-EDGE: zero events → message + exit 0
        out = "no events yet\n"
        err = since_warn
        if malformed_count:
            err += (
                f"fno agents trace: skipped {malformed_count} malformed "
                f"line(s) in {events_path}\n"
            )
        return TraceResult(exit_code=0, output=out, stderr=err)

    # Synthesize target header (AC5-UI): if any row carries
    # target_session_id, mention it once at the top.
    header_lines: list[str] = []
    rsids = sorted({_ev_field(e, "target_session_id") for e in filtered if _ev_field(e, "target_session_id")})
    if rsids and not json_out:
        header_lines.append(f"target_session: {', '.join(str(r) for r in rsids)}")

    lines: list[str] = list(header_lines)
    for ev in filtered:
        if json_out:
            lines.append(json.dumps(ev, sort_keys=False, separators=(",", ":")))
        else:
            ts = ev.get("ts", "")
            kind = _ev_kind(ev)
            recipient = _ev_field(ev, "to_name") or _ev_field(ev, "name") or "?"
            sender = _ev_field(ev, "from_name") or "?"
            rid = _format_request_id(_ev_field(ev, "request_id"), json_mode=False)
            ck = _ev_field(ev, "caller_kind") or "-"
            row = f"{ts}  {kind}  {sender} -> {recipient}  rid={rid}  caller={ck}"
            lines.append(row)
            if kind.endswith("_started") and ev.get("request_id") in orphan_rids:
                lines.append("                                          no _done received")

    err = since_warn
    if malformed_count:
        err += (
            f"fno agents trace: skipped {malformed_count} malformed "
            f"line(s) in {events_path}\n"
        )
    return TraceResult(exit_code=0, output="\n".join(lines) + "\n", stderr=err)


def cmd_trace(
    name: Optional[str] = typer.Argument(
        None,
        help="Agent name (recipient to_name). Omit with --all to see every agent.",
    ),
    request_id: Optional[str] = typer.Option(
        None, "--request-id",
        help="Filter to a single logical request (32 hex chars; joins across agents).",
    ),
    all_agents: bool = typer.Option(
        False, "--all", "-A",
        help="Drop the to_name=<name> filter; useful with --request-id.",
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J",
        help="Emit one JSON object per row (machine output).",
    ),
    limit: int = typer.Option(
        200, "--limit",
        help="Cap row count (chronologically earliest N).",
    ),
    since: Optional[str] = typer.Option(
        None, "--since",
        help="ISO8601 lower bound on event ts.",
    ),
) -> None:
    """Trace an agent's dispatch lifecycle from events.jsonl."""
    result = trace_logic(
        name=name,
        request_id=request_id,
        all_agents=all_agents,
        json_out=json_out,
        limit=limit,
        since=since,
    )
    if result.stderr:
        sys.stderr.write(result.stderr)
    if result.output:
        sys.stdout.write(result.output)
        sys.stdout.flush()
    if result.exit_code != 0:
        raise typer.Exit(code=result.exit_code)
