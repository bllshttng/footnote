"""fno.agents.format — pure JSON + table renderers for `fno agents`.

Pure functions that take canonical row dicts and return strings. The CLI
layer picks the renderer based on TTY / --json. No I/O, no shell-outs,
no registry mutations — the renderers are deterministic in their inputs.

The canonical row shape is documented in
``internal/fno/specs/2026-05-20-fno-agents-us3-list-logs.md``.
``serialize_entry`` produces that shape from a :class:`AgentEntry`; the
shape is stable across providers so JSON consumers can rely on the key
set (AC3-HP).
"""
from __future__ import annotations

import json as _json
import shutil
from typing import Optional

from fno.agents.registry import AgentEntry

# Bumped when the JSON output shape changes in a breaking way.
# Distinct from registry SCHEMA_VERSION (storage substrate).
# v2 (ab-098967b4): adds the additive ``discovered_sessions`` /
# ``discovered_count`` keys for the P1 live-session lane.
JSON_SCHEMA_VERSION = 2


def serialize_entry(entry: AgentEntry, live_status: Optional[str]) -> dict:
    """Produce the canonical dict shape for one agent.

    Returns the same key set for every provider so JSON consumers can
    iterate a list of agents without per-provider branching (AC3-HP).
    ``short_id`` is the provider transport key (claude jobId or daemon
    worker key; null when absent). ``session_id`` is the unified,
    provider-resolving resume-target id: ``short_id`` for claude, ``codex_session_id``
    for codex, ``gemini_session_id`` for gemini. It surfaces the codex
    resume UUID — the argument ``codex resume`` / ``fno agents resume``
    consume — which was previously stored but invisible in list output.

    ``live_status`` is the orthogonal "what is claude's supervisor saying
    right now" signal. It is ``None`` for non-Claude entries and for
    Claude entries when the ``claude agents --json`` shellout failed or
    omitted the entry.
    """
    return {
        "name": entry.name,
        "provider": entry.provider,
        "short_id": entry.short_id or None,
        "session_id": entry.session_id,
        "cwd": entry.cwd,
        "created_at": entry.created_at,
        "last_message_at": entry.last_message_at,
        "status": entry.status,
        "live_status": live_status,
        "log_path": entry.log_path,
    }


def render_json(
    rows: list[dict],
    filters_applied: dict,
    discovered: Optional[list[dict]] = None,
) -> str:
    """Render the canonical JSON object.

    Pretty-printed with ``indent=2`` for human inspection; jq round-trips
    cleanly either way (AC3-UI). ``discovered`` is the P1 live-session lane
    (host-local, un-adopted Claude Code sessions); always present so a
    consumer can distinguish "no discovered sessions" from "an older shape".
    """
    discovered = discovered or []
    payload = {
        "agents": rows,
        "count": len(rows),
        "discovered_sessions": discovered,
        "discovered_count": len(discovered),
        "filters_applied": filters_applied,
        "schema_version": JSON_SCHEMA_VERSION,
    }
    return _json.dumps(payload, indent=2, sort_keys=False)


# --- Human table rendering ---------------------------------------------------
#
# Column layout (in order): NAME, PROVIDER, STATUS, LIVE, LAST MESSAGE, CWD.
# Width auto-sizes to terminal columns. STATUS and LIVE never truncate —
# they are short-text columns. NAME and CWD truncate with right-aligned
# ellipsis if needed; LAST MESSAGE is rendered as a relative-time string
# from ``last_message_at``.

_HEADERS = ("NAME", "PROVIDER", "STATUS", "LIVE", "LAST MESSAGE", "CWD")
_HOME_PREFIX_PLACEHOLDER = "~"


def _terminal_width(fallback: int = 120) -> int:
    """Best-effort terminal width detection; falls back when no TTY."""
    try:
        return shutil.get_terminal_size((fallback, 24)).columns
    except OSError:
        return fallback


def _relative_time(iso_ts: Optional[str]) -> str:
    """Render an ISO-8601 UTC timestamp as a short relative-time token.

    Returns ``"-"`` for None (legacy v1 entries / never-messaged agents).
    Output format examples: ``17:30:12 (2m)``, ``17:00:00 (32m)``,
    ``yesterday``. The renderer prefers wall-clock + delta so the human
    reading the table can correlate with logs and grafana boards.
    """
    if not iso_ts:
        return "-"
    from datetime import datetime, timezone

    try:
        when = datetime.strptime(iso_ts, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=timezone.utc
        )
    except ValueError:
        # ISO with fractional seconds or +00:00 offsets — best-effort.
        try:
            when = datetime.fromisoformat(iso_ts.replace("Z", "+00:00"))
        except ValueError:
            return iso_ts  # fall back to raw string; better than crash
    now = datetime.now(timezone.utc)
    delta = now - when
    seconds = int(delta.total_seconds())
    if seconds < 0:
        # Clock skew or future-stamped entries — show wall-clock only.
        return when.strftime("%H:%M:%S")
    if seconds < 60:
        relative = f"{seconds}s"
    elif seconds < 3600:
        relative = f"{seconds // 60}m"
    elif seconds < 86400:
        relative = f"{seconds // 3600}h"
    else:
        relative = f"{seconds // 86400}d"
    return f"{when.strftime('%H:%M:%S')} ({relative})"


def _collapse_home(cwd: str) -> str:
    """Replace the user's ``$HOME`` prefix with ``~`` for display.

    Handles three cases:
    - ``cwd`` equals home exactly → returns ``~``.
    - ``cwd`` starts with ``home + path-separator`` → returns ``~/rest``.
    - Otherwise → returns ``cwd`` unchanged.

    The trailing-separator check uses ``os.sep`` rather than a hardcoded
    ``/`` so the renderer behaves correctly on non-POSIX hosts. The
    falsy ``home`` guard covers the rare case where ``os.path.expanduser``
    returns the literal ``~`` (no $HOME set, no passwd entry).
    """
    import os

    home = os.path.expanduser("~")
    if not home or home == "~":
        return cwd
    if cwd == home:
        return _HOME_PREFIX_PLACEHOLDER
    prefix = home if home.endswith(os.sep) else home + os.sep
    if cwd.startswith(prefix):
        return _HOME_PREFIX_PLACEHOLDER + cwd[len(home):]
    return cwd


def _truncate(text: str, width: int) -> str:
    """Right-aligned ellipsis truncation."""
    if width <= 0:
        return ""
    if len(text) <= width:
        return text
    if width <= 1:
        return text[:width]
    return text[: width - 1] + "…"


def render_table(
    rows: list[dict],
    terminal_width: Optional[int] = None,
    discovered: Optional[list[dict]] = None,
) -> str:
    """Render the human-table view.

    Layout is column-aligned with single-space separators. Empty registry
    still emits the header row so an automated consumer can detect "the
    command ran, zero results" rather than "the command crashed silently".

    When ``discovered`` is non-empty, a visually distinct "DISCOVERED LIVE
    SESSIONS" section is appended below the registered-agents table (AC1-UI),
    so live un-adopted sessions never blend into the registry rows.
    """
    width = terminal_width or _terminal_width()

    # Compute display fields once, then size columns from the actual data.
    display_rows = []
    for row in rows:
        live = row.get("live_status") or "-"
        last_msg = _relative_time(row.get("last_message_at"))
        cwd = _collapse_home(row.get("cwd") or "")
        display_rows.append(
            {
                "name": row.get("name") or "-",
                "provider": row.get("provider") or "-",
                "status": row.get("status") or "-",
                "live": live,
                "last_message": last_msg,
                "cwd": cwd,
            }
        )

    # Column widths: max(header, longest value), bounded by terminal width.
    # NAME and CWD are the truncation candidates if the row overflows.
    min_widths = {
        "name": len("NAME"),
        "provider": len("PROVIDER"),
        "status": len("STATUS"),
        "live": len("LIVE"),
        "last_message": len("LAST MESSAGE"),
        "cwd": len("CWD"),
    }
    col_widths = dict(min_widths)
    for r in display_rows:
        for key in col_widths:
            col_widths[key] = max(col_widths[key], len(str(r[key])))

    # Pad widths produce total row width; check overflow and truncate
    # NAME / CWD if necessary. The 5 single-space separators contribute 5.
    pad_total = sum(col_widths.values()) + 5
    if pad_total > width:
        overflow = pad_total - width
        # Take from CWD first, then NAME.
        cwd_shrink = min(overflow, col_widths["cwd"] - min_widths["cwd"])
        col_widths["cwd"] -= cwd_shrink
        overflow -= cwd_shrink
        if overflow > 0:
            name_shrink = min(overflow, col_widths["name"] - min_widths["name"])
            col_widths["name"] -= name_shrink

    def _format_row(values: list[str]) -> str:
        keys = ["name", "provider", "status", "live", "last_message", "cwd"]
        cells = []
        for key, val in zip(keys, values):
            cell_text = str(val)
            if key in ("name", "cwd"):
                cell_text = _truncate(cell_text, col_widths[key])
            cells.append(cell_text.ljust(col_widths[key]))
        return " ".join(cells).rstrip()

    lines = [_format_row(list(_HEADERS))]
    for r in display_rows:
        lines.append(
            _format_row(
                [
                    r["name"],
                    r["provider"],
                    r["status"],
                    r["live"],
                    r["last_message"],
                    r["cwd"],
                ]
            )
        )

    out = "\n".join(lines) + "\n"
    if discovered:
        out += _render_discovered_section(discovered, width)
    return out


_DISCOVERED_HEADERS = ("HANDLE", "STATUS", "PROJECT", "HEX", "CWD")


def _render_discovered_section(discovered: list[dict], width: int) -> str:
    """Render the host-local discovered-live-sessions lane (AC1-UI).

    A blank line + a banner separate it from the registry table so the two
    lanes are unmistakable. Columns: HANDLE (friendly alias), STATUS
    (idle/busy/waiting), PROJECT, HEX (the addressable short-id), CWD.
    """
    display = []
    for r in discovered:
        display.append(
            {
                "handle": str(r.get("handle") or "-"),
                "status": str(r.get("status") or "-"),
                "project": str(r.get("project") or "-"),
                "hex": str(r.get("short_id") or "-"),
                "cwd": _collapse_home(str(r.get("cwd") or "")),
            }
        )

    col_widths = {
        "handle": len("HANDLE"),
        "status": len("STATUS"),
        "project": len("PROJECT"),
        "hex": len("HEX"),
        "cwd": len("CWD"),
    }
    for r in display:
        for key in col_widths:
            col_widths[key] = max(col_widths[key], len(r[key]))

    pad_total = sum(col_widths.values()) + 4
    if pad_total > width:
        overflow = pad_total - width
        cwd_shrink = min(overflow, max(0, col_widths["cwd"] - len("CWD")))
        col_widths["cwd"] -= cwd_shrink

    keys = ["handle", "status", "project", "hex", "cwd"]

    def _row(values: list[str]) -> str:
        cells = []
        for key, val in zip(keys, values):
            cell = str(val)
            if key == "cwd":
                cell = _truncate(cell, col_widths[key])
            cells.append(cell.ljust(col_widths[key]))
        return " ".join(cells).rstrip()

    banner = f"\nDISCOVERED LIVE SESSIONS ({len(display)}, host-local)\n"
    section = [banner.rstrip("\n"), _row(list(_DISCOVERED_HEADERS))]
    for r in display:
        section.append(_row([r["handle"], r["status"], r["project"], r["hex"], r["cwd"]]))
    return "\n".join(section) + "\n"
