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
from fno.agents.session_truth import STALE_ATTENTION_S

# Bumped when the JSON output shape changes in a breaking way.
# Distinct from registry SCHEMA_VERSION (storage substrate).
# v2 (ab-098967b4): adds the additive ``discovered_sessions`` /
# ``discovered_count`` keys for the P1 live-session lane.
JSON_SCHEMA_VERSION = 2

# Basis values that are falsifiers rather than evidence: a positive
# measurement that the worker is gone, which no other reading outranks.
_FALSIFIER_BASES = {"process-gone", "pane-gone"}


def attention_rank(row: dict) -> int:
    """Evidence tier for one serialized row: 0 needs the operator most.

    Built only from fields that carry their evidence with them (``basis``,
    ``last_activity_age_s``) - never from ``status`` or a bare verdict word,
    both of which read healthy for a worker dead under two hours. Mirrors the
    daemon's ``attention_sort_key`` tier set; the shared fixture in
    ``schemas/agents-attention-order.json`` is what pins the two together.
    """
    basis = row.get("basis")
    if basis in _FALSIFIER_BASES or row.get("reachability") == "unreachable":
        return 5
    age = row.get("last_activity_age_s") or 0
    if basis == "transcript" and age >= STALE_ATTENTION_S:
        return 0
    if basis == "silent":
        return 1
    if basis == "no-evidence":
        return 2
    return 4


def attention_sort_key(row: dict) -> tuple:
    """Attention order: evidence tier, then longest-silent first, then name.

    An absent age counts as 0 (youngest): an absent reading has two
    explanations and a sort cannot tell them apart, so it must never float a
    row to the top.
    """
    age = row.get("last_activity_age_s") or 0
    return (attention_rank(row), -age, str(row.get("name") or ""))


def row_address(
    harness: Optional[str],
    harness_session_id: Optional[str],
    short_id: Optional[str] = None,
) -> Optional[str]:
    """The mailbox address a row can actually receive at.

    One derivation, reached by both lanes (registry rows via
    :func:`serialize_entry`, discovered rows via the table renderer) so the two
    cannot advertise different addresses for the same session. The Rust
    ``agent.list`` projection carries a parity-pinned mirror because it cannot
    import Python; ``schemas/agents-list-row.json`` is what keeps them honest.

    ``short_id`` is a fallback for claude ONLY, where the transport key IS the
    first eight. A codex or opencode ``short_id`` is a daemon worker key, so
    using it here would advertise a mailbox nothing drains - the exact failure
    this column exists to stop.
    """
    from fno.harness_identity import canonical_handle

    if harness_session_id:
        return canonical_handle(harness_session_id)
    if harness == "claude" and short_id:
        return short_id
    return None


def serialize_entry(
    entry: AgentEntry,
    live_status: Optional[str],
    observed_model: Optional[dict] = None,
    reachability: Optional[str] = None,
    basis: Optional[str] = None,
    progress: Optional[str] = None,
    progress_basis: Optional[str] = None,
    last_activity_age_s: Optional[int] = None,
    last_event_at: Optional[str] = None,
    last_message: Optional[str] = None,
) -> dict:
    """Produce the canonical dict shape for one agent.

    Returns the same key set for every provider so JSON consumers can
    iterate a list of agents without per-provider branching (AC3-HP).
    The key set is pinned by ``schemas/agents-list-row.json``, which the
    Rust daemon's ``agent.list`` projection is asserted against too — this
    function is NOT what serves ``fno agents list``, so the two have drifted
    before and only the shared contract file keeps them honest.
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

    ``observed_model`` is the five-variant reading from
    :func:`fno.provenance.observed.observed_model` -- what the worker is
    ACTUALLY answering as, derived from its own transcript rather than from
    anything the spawn recorded. Defaulted rather than required so a caller
    that has no truth reading still produces the full key set; the default is
    the same ``no-transcript`` the resolver reports when it finds no file.
    """
    return {
        "name": entry.name,
        # `harness` is the sole identity axis. The `provider` alias that used to
        # sit beside it carried the HARNESS value ("claude") for a worker routed
        # to another vendor, and an operator read that as proof the route had
        # fallen back to Anthropic. A key whose name says vendor and whose value
        # is a harness is worse than no key, so it is gone; `observed_model`
        # below is the honest answer to the question it looked like it answered.
        "harness": entry.harness,
        # The worker's own session id in its harness's store. Distinct from
        # `session_id` (the resume-target id, which is the 8-hex jobId for
        # claude) and from `short_id` (the transport key).
        "harness_session_id": entry.harness_session_id,
        "short_id": entry.short_id or None,
        "session_id": entry.session_id,
        # The one identifier in this row that mail can be sent to. Every other
        # one names something else: `name` is a spawn label, `short_id` is a
        # transport key and is null for most rows, `session_id` is a resume
        # target. A reader with no address column copies `name`, and a name-lane
        # durable write queues under a key no drain reads.
        "address": row_address(
            entry.harness, entry.harness_session_id, entry.short_id or None
        ),
        "cwd": entry.cwd,
        "created_at": entry.created_at,
        "last_message_at": entry.last_message_at,
        "status": entry.status,
        "live_status": live_status,
        # The model the worker is answering as, read from its transcript. A
        # spawn-recorded route would report the INTENDED model in exactly the
        # case an operator suspects a silent fallback; this cannot.
        "observed_model": observed_model or {"kind": "no-transcript"},
        "log_path": entry.log_path,
        # Crown (US9): a compact "L1 epic-x" descriptor + the raw fields, so a
        # minion can resolve who to escalate to and a second live crown over the
        # same scope is detectable. null for an uncrowned row.
        "crown": entry.crown_label,
        "crown_level": entry.crown_level,
        "crown_scope": entry.crown_scope,
        "crown_grantor": entry.crown_grantor,
        # The mux hosting ref ({session, pane_id}) for a pane-hosted row, else
        # null. Exposed so a caller can address the pane - e.g. close a handed-off
        # teammate with `fno mux pane kill <session>:<pane_id>` (a mux row's
        # short_id is empty, so `fno agents stop` refuses it).
        "mux": entry.mux,
        # The shared reachability verdict and the evidence it was reached from
        # (fno.agents.reachability). Emitted here rather than bolted onto the row
        # by the caller so the key set stays pinned by the shared contract file —
        # a key that exists only on the path that happened to answer is the drift
        # this contract was written to stop.
        "reachability": reachability,
        "basis": basis,
        # The orthogonal axis: reachability answers "can I reach this
        # process"; progress answers "is it advancing, awaiting the operator,
        # parked, or refused" -- a question a refused-but-reachable worker
        # needs answered separately (fno.agents.reachability.classify_progress).
        "progress": progress,
        "progress_basis": progress_basis,
        "last_activity_age_s": last_activity_age_s,
        # The absolute stamp of the newest transcript activity and the flattened
        # text of the LAST turn. Both come from the same truth probe as
        # the age above, so a reader can see WHAT the worker last did and WHEN -
        # a `working` row whose stamp is hours old is the wedged-worker signal
        # no store could express. Null when the probe never answered: an absent
        # reading is never a fresh one.
        "last_event_at": last_event_at,
        "last_message": last_message,
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
    Rows render in attention order - the same order the daemon projection
    and the mux table apply - so a reader scanning the list top-down meets
    the row that needs them first.
    """
    discovered = discovered or []
    rows = sorted(rows, key=attention_sort_key)
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
# Column layout (in order): NAME, ADDRESS, HARNESS, STATUS, LIVE, EVENT AGE,
# LAST MESSAGE, CWD. Width auto-sizes to terminal columns. STATUS and LIVE never
# truncate — they are short-text columns. NAME and CWD truncate with
# right-aligned ellipsis if needed; LAST MESSAGE is the flattened text of the
# worker's last transcript turn (capped at 40 chars), and EVENT AGE is the
# relative age of that transcript's newest activity - next to STATUS on purpose,
# so a `working` row hours stale is visible as the disagreement it is.
#
# ADDRESS never truncates either, and not for cosmetic reasons: a truncated
# address is a WRONG address, and mail sent to it queues under a key no drain
# reads. Overflow comes out of CWD then NAME, which lose meaning gracefully.

# One ordering of the table columns as (key, header) pairs, the single place
# a column insert touches: headers, widths, and row rendering all derive from
# it, so the order cannot disagree with itself across hand-kept parallel lists.
_COLUMNS = (
    ("name", "NAME"),
    ("address", "ADDRESS"),
    ("harness", "HARNESS"),
    ("status", "STATUS"),
    ("live", "LIVE"),
    ("event_age", "EVENT AGE"),
    ("last_message", "LAST MESSAGE"),
    ("cwd", "CWD"),
)
_COL_KEYS = tuple(key for key, _ in _COLUMNS)
# HARNESS, not PROVIDER: the column has always shown the harness, and the old
# heading made a claude-hosted worker on a zai route read as running on claude.
# LAST MESSAGE cap: long transcript lines must not blow out the table; CWD and
# NAME absorb the rest. 40 keeps a one-line gist readable at 120 cols.
_LAST_MESSAGE_WIDTH = 40
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

    Rows render in attention order, same as ``render_json`` - an operator
    reading the plain TTY table top-down meets the row that needs them first,
    not whatever order the registry happened to return.
    """
    width = terminal_width or _terminal_width()
    rows = sorted(rows, key=attention_sort_key)

    # Compute display fields once, then size columns from the actual data.
    display_rows = []
    for row in rows:
        live = row.get("live_status") or "-"
        event_age = _relative_time(row.get("last_event_at"))
        # The transcript's LAST turn, not a registry timestamp: the column spent
        # its life wired to `last_message_at`, which is null on many rows while
        # the worker was mid-sentence - a "last message" column that never
        # showed a message. Cap it so one long line cannot own the table.
        last_msg = _truncate(
            row.get("last_message") or "-", _LAST_MESSAGE_WIDTH
        )
        cwd = _collapse_home(row.get("cwd") or "")
        # Mark a crowned row (US9) with a compact ASCII marker in the name cell -
        # ASCII, not an emoji, so the column-width math (len-based) stays aligned.
        name = row.get("name") or "-"
        if row.get("crown"):
            name = f"{name} [{row['crown']}]"
        display_rows.append(
            {
                "name": name,
                "address": row.get("address") or "-",
                "harness": row.get("harness") or "-",
                "status": row.get("status") or "-",
                "live": live,
                "event_age": event_age,
                "last_message": last_msg,
                "cwd": cwd,
            }
        )

    # Column widths: max(header, longest value), bounded by terminal width.
    # NAME and CWD are the truncation candidates if the row overflows.
    min_widths = {key: len(header) for key, header in _COLUMNS}
    col_widths = dict(min_widths)
    for r in display_rows:
        for key in col_widths:
            col_widths[key] = max(col_widths[key], len(str(r[key])))

    # Pad widths produce total row width; check overflow and truncate
    # NAME / CWD if necessary. The single-space separators contribute
    # one per column boundary.
    pad_total = sum(col_widths.values()) + len(_COL_KEYS) - 1
    if pad_total > width:
        overflow = pad_total - width
        # Take from CWD first, then NAME.
        cwd_shrink = min(overflow, col_widths["cwd"] - min_widths["cwd"])
        col_widths["cwd"] -= cwd_shrink
        overflow -= cwd_shrink
        if overflow > 0:
            name_shrink = min(overflow, col_widths["name"] - min_widths["name"])
            col_widths["name"] -= name_shrink

    def _format_row(cells_by_col: dict) -> str:
        cells = []
        for key in _COL_KEYS:
            cell_text = str(cells_by_col[key])
            if key in ("name", "cwd"):
                cell_text = _truncate(cell_text, col_widths[key])
            cells.append(cell_text.ljust(col_widths[key]))
        return " ".join(cells).rstrip()

    lines = [_format_row(dict(_COLUMNS))]
    for r in display_rows:
        lines.append(_format_row(r))

    out = "\n".join(lines) + "\n"
    if discovered:
        out += _render_discovered_section(discovered, width)
    return out


_DISCOVERED_HEADERS = ("ADDRESS", "LABEL", "STATUS", "PROJECT", "CWD")


def _render_discovered_section(discovered: list[dict], width: int) -> str:
    """Render the host-local discovered-live-sessions lane (AC1-UI).

    A blank line + a banner separate it from the registry table so the two
    lanes are unmistakable. Columns: ADDRESS (the mailbox address), LABEL (the
    friendly alias), STATUS (idle/busy/waiting), PROJECT, CWD.

    ADDRESS leads and the alias is demoted to LABEL because the alias led this
    table for its whole life, which made it the leftmost thing a reader copied
    - and ``<project>-<short8>`` is not an address. The old HEX column already
    held the right value in position four, where nobody read it.
    """
    display = []
    for r in discovered:
        display.append(
            {
                # Read off the row, not re-derived: `to_row` already resolved it
                # from the session's own harness, and the Rust renderer reads the
                # same key. A second derivation here would be a second answer.
                "address": str(r.get("address") or "-"),
                "label": str(r.get("handle") or "-"),
                "status": str(r.get("status") or "-"),
                "project": str(r.get("project") or "-"),
                "cwd": _collapse_home(str(r.get("cwd") or "")),
            }
        )

    col_widths = {
        "address": len("ADDRESS"),
        "label": len("LABEL"),
        "status": len("STATUS"),
        "project": len("PROJECT"),
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

    keys = ["address", "label", "status", "project", "cwd"]

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
        section.append(
            _row([r["address"], r["label"], r["status"], r["project"], r["cwd"]])
        )
    return "\n".join(section) + "\n"
