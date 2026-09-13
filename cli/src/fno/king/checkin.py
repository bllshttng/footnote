"""``fno agents king checkin`` - one verb runs the reign check-in body.

Gathers, prints, diffs and journals; the row comes from the same dict the
lines print. It never decides and never acts. Contract: docs/architecture/reign.md.
"""
from __future__ import annotations

import datetime as _dt
import json
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

import typer

#: The nine readings of the check-in body, in print order.
READING_NAMES = [
    "user_notes", "board", "blocked_child", "court", "capacity",
    "workers", "crown", "drain", "main_ci",
]

#: The numeric keys this verb owns and diffs versus the previous beat.
NUMERIC_DIFF_KEYS = [
    "open_prs", "free_claim_no_driver", "blocked", "active_nodes",
    "live_workers", "undelivered",
]

#: Render cap for the per-node rows a court line prints (the count in the
#: payload stays whole, only the rendered rows are cut, as the board does).
MAX_COURT_ROWS = 25

#: Cap on blocked_child rows carried in BOTH the printout and the row, so a
#: fleet-on-fire beat cannot overflow the event data cap and lose the journal
#: row. The total travels beside the list, so nothing is silently dropped.
BLOCKED_CHILD_CAP = 50


class ReaderError(Exception):
    """One reading could not be taken; the beat continues without it."""


@dataclass
class Reading:
    name: str
    ok: bool
    value: Any = None
    detail: str = ""
    error: str = ""


# ---------------------------------------------------------------------------
# readers (each returns (value, detail) or raises ReaderError)


def _marker_lib() -> Path:
    from fno.paths import resolve_plugin_script

    lib = resolve_plugin_script("scripts/lib/canon-doc-marker.sh")
    if not lib.is_file():
        raise ReaderError(f"canon doc marker lib missing at {lib}")
    return lib


def _handoff_doc(scope: str) -> Path:
    # The crown-keyed doc is the newest existing one; the check-in body
    # refreshes it before this verb reads it. Resolution is paths.py's one
    # definition, shared with the writer's save path.
    from fno.paths import crown_handoff_doc

    doc = crown_handoff_doc(scope)
    if not doc.exists():
        raise ReaderError(f"no canon handoff doc for scope {scope}")
    return doc


def _r_user_notes(scope: str) -> tuple[Any, str]:
    # The check-in body refreshes the doc before reading it, so a direct
    # verb call sees this beat's notes. Best-effort: a failed refresh leaves
    # the previous doc readable.
    from fno.paths import resolve_plugin_script

    refresh = resolve_plugin_script("hooks/precompact-canon-doc.sh")
    if refresh.is_file():
        # Devnull both output pipes: a child that inherits them would hold
        # this run open past the script's own exit.
        subprocess.run(
            ["bash", str(refresh)], stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            check=False, timeout=120,
        )
    doc = _handoff_doc(scope)
    lib = _marker_lib()
    proc = subprocess.run(
        ["bash", "-c", 'source "$1" && canon_doc_extract_marker "$2" user', "x", str(lib), str(doc)],
        capture_output=True, text=True, check=False,
    )
    if proc.returncode != 0:
        # rc 1 is the marker lib's "no open block": the doc carries no user
        # section yet. That is an empty block, never a failed reading.
        return None, ""
    text = proc.stdout
    ph = subprocess.run(
        ["bash", "-c", 'source "$1" && canon_doc_is_placeholder "$(cat)"', "x", str(lib)],
        input=text, capture_output=True, text=True, check=False,
    )
    if ph.returncode == 0:
        return None, ""
    return text, ""


def _fetch_board(scope: str, manifest_path: str | None = None) -> dict:
    """The board payload, bound to the manifest given (else the caller's own
    crown, else fleet-wide). The requested scope's own manifest wins, so a
    `--scope B` beat never journals scope A's queues."""
    import subprocess as _sp

    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        raise ReaderError("the fno-agents binary was not found")
    state: Any = manifest_path
    if state is None:
        try:
            from fno.agents.crown import calling_agent_row
            from fno.king.state import resolve_king_manifest_path

            caller = calling_agent_row()
            sid = getattr(caller, "harness_session_id", None) or getattr(caller, "cc_session_id", None) or ""
            if sid:
                state, _ = resolve_king_manifest_path(sid, getattr(caller, "harness", None))
        except Exception:  # noqa: BLE001 - an unresolvable crown reads the fleet board
            state = None
    cmd = [str(binary), "board", "--json"]
    if state is not None:
        cmd += ["--state", str(state)]
    proc = _sp.run(cmd, capture_output=True, text=True, check=False)
    try:
        return json.loads(proc.stdout or "")
    except ValueError as exc:
        detail = (proc.stderr or proc.stdout or "").strip()[:160]
        raise ReaderError(f"board unreadable (exit {proc.returncode}): {detail or exc}") from exc


def _queue(board: dict, name: str) -> dict:
    for q in board.get("queues") or []:
        if q.get("name") == name:
            status = q.get("status")
            if status not in ("ok", None):
                raise ReaderError(f"{name} queue {status}: {q.get('error', '')}")
            return q
    raise ReaderError(f"board payload names no {name} queue")


def _open_pr_count() -> int:
    # The board renders only actionable PR queues, so the open count comes
    # from the same listing the board itself runs.
    proc = subprocess.run(
        ["gh", "pr", "list", "--state", "open", "--limit", "200", "--json", "number"],
        capture_output=True, text=True, check=False,
    )
    if proc.returncode != 0:
        raise ReaderError(f"open PR listing failed: {(proc.stderr or '').strip()[:120]}")
    try:
        return len(json.loads(proc.stdout or "[]"))
    except ValueError as exc:
        raise ReaderError(f"open PR listing did not parse: {exc}") from exc


def _scope_court_row(scope: str, court: dict) -> dict | None:
    return next(
        (c for c in court.get("crowns") or [] if c.get("scope") == scope),
        None,
    )


def _r_board(scope: str, board_fn: Callable, court_fn: Callable) -> tuple[Any, str]:
    court = court_fn(scope)
    mine = _scope_court_row(scope, court) or {}
    board = board_fn(scope, mine.get("manifest_path"))
    # Blocked rows come from the requested crown's own fold, never the
    # court-wide stuck summary: another crown's blocked node is not this
    # beat's evidence.
    stuck = [
        n for n in (mine.get("scope_nodes") or {}).get("nodes") or []
        if n.get("blocked_by")
    ]
    blocked_on = [
        f"{row.get('id')} on {','.join(row.get('blocked_by') or [])}"
        for row in stuck
    ]
    value = {
        "open_prs": _open_pr_count(),
        "free_claim_no_driver": _queue(board, "undriven_pr").get("count", 0),
        "blocked": len(stuck),
        "blocked_on": blocked_on,
    }
    return value, ""


def _r_blocked_child(scope: str, board_fn: Callable) -> tuple[Any, str]:
    rows = _queue(board_fn(scope), "blocked_child").get("rows") or []
    bounded = [
        {
            "node": row.get("id"),
            "session": row.get("session"),
            "age_minutes": row.get("age_minutes"),
            "reason": row.get("reason"),
        }
        for row in rows[:BLOCKED_CHILD_CAP]
    ]
    return {"rows": bounded, "total": len(rows)}, ""


def _fetch_court(scope: str) -> dict:
    from fno.agents.court import render_court

    try:
        return json.loads(render_court(as_json=True, nodes=True))
    except ValueError as exc:
        raise ReaderError(f"court payload did not parse: {exc}") from exc


def _r_court(scope: str, court_fn: Callable) -> tuple[Any, str]:
    court = court_fn(scope)
    mine = _scope_court_row(scope, court)
    if mine is None:
        raise ReaderError(f"the court names no crown for scope {scope}")
    fold = mine.get("scope_nodes") or {}
    if fold.get("status") not in (None, "ok"):
        # An unresolved fold carries no rows: reporting zero active nodes
        # would read the failed instrument as an empty territory.
        raise ReaderError(f"scope fold {fold.get('status')}: {fold.get('reason', '')}")
    rows = []
    for n in fold.get("nodes") or []:
        # The fold lists ACTIVE rows only (its own status vocabulary); the
        # count is the fold's, never total minus done.
        sessions = n.get("sessions") or []
        first = sessions[0] if sessions else None
        rows.append({
            "id": n.get("id"),
            "status": n.get("status"),
            "worker": n.get("worker"),
            "pr_number": n.get("pr_number"),
            "session": first.get("id") if isinstance(first, dict) else first,
        })
    return {"active_nodes": len(rows), "total_nodes": fold.get("total") or 0, "rows": rows}, ""


def _r_capacity(scope: str) -> tuple[Any, str]:
    from fno.agents.spawn_gate import _cpu_axis
    from fno.doctor_footprint import _payload, cause_reading

    reading, err = cause_reading()
    if err is not None or reading is None:
        raise ReaderError(f"footprint unavailable: {err}")
    payload = _payload(reading, process_threshold=None, exit_code=0)
    admission = _cpu_axis()
    fp = payload.get("capacity_verdict")
    return {
        "footprint": fp,
        "gate": admission.verdict,
        "disagree": fp != admission.verdict,
        "unparsed_lines": payload.get("unparsed_lines", 0),
    }, ""


def _r_workers(scope: str) -> tuple[Any, str]:
    from fno.agents.top import render_top

    try:
        payload = json.loads(render_top(as_json=True))
    except ValueError as exc:
        raise ReaderError(f"top payload did not parse: {exc}") from exc
    predicate = (payload.get("predicate") or "").strip()
    workers = payload.get("workers")
    if not predicate or not isinstance(workers, list):
        raise ReaderError("the top payload carries no positive predicate")
    ages = [
        (w.get("status_age_s"), w.get("handle") or w.get("name"))
        for w in workers
        if isinstance(w.get("status_age_s"), (int, float))
    ]
    if not ages:
        raise ReaderError("every status_age_s is null")
    oldest_age, oldest_handle = max(ages)
    return {
        "live_workers": len(workers),
        "oldest_worker_seen": f"{int(oldest_age)}s {oldest_handle}",
    }, ""


def _r_crown(scope: str, court_fn: Callable) -> tuple[Any, str]:
    court = court_fn(scope)
    summary = court.get("summary") or {}
    anomalies = [
        f"{c.get('holder')} scope {c.get('scope')} status {c.get('status')} agree {c.get('agree')}"
        + (f" ({c.get('reason')})" if c.get("reason") else "")
        for c in court.get("crowns") or []
        if c.get("status") != "live" or c.get("agree") is not True
    ]
    return {
        "crown_total": summary.get("total"),
        "crown_splits": summary.get("splits"),
        "crown_disagreements": summary.get("disagreements"),
        "crown_anomalies": anomalies,
    }, ""


def _r_drain(scope: str) -> tuple[Any, str]:
    from fno.graph.store import GraphUnreadableError, StoreUnavailable
    from fno.king.scope import scope_undelivered
    from fno.tracker.metadata import ExternalMetadataUnavailable, read_entries

    try:
        entries = read_entries("king drain", strict=True)
        return scope_undelivered(scope, entries), ""
    except (
        ExternalMetadataUnavailable,
        GraphUnreadableError,
        StoreUnavailable,
        ValueError,
    ) as exc:
        raise ReaderError(f"drain unreadable: {exc}") from exc


_RED_CONCLUSIONS = {"failure", "timed_out", "cancelled", "action_required", "startup_failure"}


def _owner_repo(url: str) -> str:
    match = re.search(r"[:/]([^:/]+)/([^/]+?)(?:\.git)?$", url.strip())
    if not match:
        raise ReaderError(f"cannot read owner/repo from origin url {url!r}")
    return f"{match.group(1)}/{match.group(2)}"


def _gh_json(args: list[str]) -> dict:
    proc = subprocess.run(args, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        raise ReaderError(f"gh api failed: {(proc.stderr or proc.stdout).strip()[:120]}")
    try:
        return json.loads(proc.stdout or "{}")
    except ValueError as exc:
        raise ReaderError(f"gh api payload did not parse: {exc}") from exc


def _r_main_ci(scope: str) -> tuple[Any, str]:
    def git(args: list[str]) -> str:
        proc = subprocess.run(args, capture_output=True, text=True, check=False)
        if proc.returncode != 0:
            raise ReaderError(f"git {args[1]} failed: {(proc.stderr or '').strip()[:120]}")
        return proc.stdout.strip()

    sha = git(["git", "rev-parse", "origin/main"])
    owner_repo = _owner_repo(git(["git", "remote", "get-url", "origin"]))
    # Page through every check run before reducing: a failing or pending run
    # on page two must not read green from a completed page one.
    check_runs: list[dict[str, Any]] = []
    for page in range(1, 6):
        payload = _gh_json([
            "gh", "api",
            f"repos/{owner_repo}/commits/{sha}/check-runs?per_page=100&page={page}",
        ])
        batch = payload.get("check_runs") or []
        check_runs.extend(batch)
        total = payload.get("total_count") or 0
        if not batch or len(check_runs) >= total:
            break
    status = _gh_json(["gh", "api", f"repos/{owner_repo}/commits/{sha}/status"])
    combined = (status.get("state") or "").lower()
    red = any(r.get("conclusion") in _RED_CONCLUSIONS for r in check_runs) or combined in ("failure", "error")
    if red:
        return "red", ""
    all_completed = all(r.get("status") == "completed" for r in check_runs)
    if check_runs and all_completed and combined == "success":
        return "green", ""
    return "pending", ""


# ---------------------------------------------------------------------------
# gather


def _readers() -> list[tuple[str, Callable]]:
    """The nine readers as (name, fn) in print order.

    The board and the court are each read once per beat and shared by the
    readings that consume them.
    """
    shared: dict[str, Any] = {}

    def court_fn(scope: str) -> dict:
        if "court" not in shared:
            shared["court"] = _fetch_court(scope)
        return shared["court"]

    def board_fn(scope: str, manifest_path: str | None = None) -> dict:
        if "board" not in shared:
            shared["board"] = _fetch_board(scope, manifest_path)
        return shared["board"]

    return [
        ("user_notes", _r_user_notes),
        ("board", lambda s: _r_board(s, board_fn, court_fn)),
        ("blocked_child", lambda s: _r_blocked_child(s, board_fn)),
        ("court", lambda s: _r_court(s, court_fn)),
        ("capacity", _r_capacity),
        ("workers", _r_workers),
        ("crown", lambda s: _r_crown(s, court_fn)),
        ("drain", _r_drain),
        ("main_ci", _r_main_ci),
    ]


def collect_readings(scope: str, readers: list[tuple[str, Callable]] | None = None) -> list[Reading]:
    readings = []
    for name, fn in readers if readers is not None else _readers():
        try:
            value, detail = fn(scope)
            readings.append(Reading(name=name, ok=True, value=value, detail=detail))
        except ReaderError as exc:
            readings.append(Reading(name=name, ok=False, error=str(exc)))
        except Exception as exc:  # noqa: BLE001 - one reader's crash is a failed reading, never a dead beat
            readings.append(Reading(name=name, ok=False, error=f"{type(exc).__name__}: {exc}"))
    return readings


# ---------------------------------------------------------------------------
# data, diff, change


#: reading name -> {emit key: subkey inside the reading's value}; a None
#: subkey stores the value whole. Every printed reading lands here, so the
#: stored row carries the same evidence the lines show.
_DATA_SPEC: dict[str, dict[str, str | None]] = {
    "user_notes": {"user_notes": None},
    "board": {
        "open_prs": "open_prs", "free_claim_no_driver": "free_claim_no_driver",
        "blocked": "blocked", "blocked_on": "blocked_on",
    },
    "blocked_child": {"blocked_children": "rows", "blocked_children_total": "total"},
    "court": {"active_nodes": "active_nodes", "total_nodes": "total_nodes", "active_rows": "rows"},
    "workers": {"live_workers": "live_workers", "oldest_worker_seen": "oldest_worker_seen"},
    "capacity": {
        "capacity_footprint": "footprint", "capacity_gate": "gate",
        "capacity_disagree": "disagree", "capacity_unparsed_lines": "unparsed_lines",
    },
    "crown": {
        "crown_total": "crown_total", "crown_splits": "crown_splits",
        "crown_disagreements": "crown_disagreements", "crown_anomalies": "crown_anomalies",
    },
    "drain": {"undelivered": None},
    "main_ci": {"main_ci": None},
}


def build_data(readings: list[Reading], scope: str) -> dict[str, Any]:
    """The one dict the lines print and the journal row stores."""
    data: dict[str, Any] = {"scope": scope}
    for reading in readings:
        spec = _DATA_SPEC.get(reading.name)
        if reading.ok and spec:
            for key, sub in spec.items():
                data[key] = reading.value.get(sub) if sub else reading.value
    # The stored active rows are exactly the rows the lines print; the count
    # above stays whole.
    if isinstance(data.get("active_rows"), list):
        data["active_rows"] = data["active_rows"][:MAX_COURT_ROWS]
    failed = [r.name for r in readings if not r.ok]
    data["coverage"] = len(readings) - len(failed)
    data["readers_failed"] = failed
    return data


def _history_rows(scope: str) -> list[dict[str, Any]]:
    """Every recorded event for this scope through the history reader's own corpus."""
    from fno.king.history import run_native
    from fno.paths import event_journals

    code, out, err = run_native(event_journals(), scope, True)
    if code != 0:
        raise ReaderError(f"history exited {code}: {(err or out).strip()[:120]}")
    try:
        payload = json.loads(out or "{}")
    except ValueError as exc:
        raise ReaderError(f"history payload did not parse: {exc}") from exc
    return payload.get("events") or []


def _previous_row(scope: str) -> tuple[dict[str, Any] | None, str]:
    """The newest canonical reign_checkin for this scope: (row, error)."""
    try:
        events = _history_rows(scope)
    except ReaderError as exc:
        return None, str(exc)
    for event in events:
        data = event.get("data") or {}
        if event.get("type") == "reign_checkin" and data.get("scope") == scope:
            return event, ""
    return None, ""


def derive_change(
    previous_data: dict[str, Any] | None,
    data: dict[str, Any],
    previous_error: str = "",
) -> str:
    """`no change` only when every diffed number is equal AND coverage is full."""
    if previous_error:
        return f"previous beat unreadable: {previous_error}"
    moved = []
    shared = 0
    if previous_data:
        for key in NUMERIC_DIFF_KEYS:
            if key in previous_data and key in data:
                shared += 1
                if previous_data[key] != data[key]:
                    moved.append(f"{key} {previous_data[key]} -> {data[key]}")
    if moved:
        return "moved: " + ", ".join(moved)
    if previous_data is not None and shared == 0:
        # Zero shared numbers is a vacuous match, never evidence of no change.
        return "previous beat carries no comparable numbers"
    failed = data.get("readers_failed") or []
    if failed:
        return "no numeric movement; readings failed: " + ", ".join(failed)
    if previous_data is None:
        return "first canonical beat for this scope"
    return "no change"


# ---------------------------------------------------------------------------
# render


def _dash(value: Any) -> str:
    return "-" if value is None else str(value)


def render_lines(
    scope: str,
    readings: list[Reading],
    data: dict[str, Any],
    previous: dict[str, Any] | None,
    previous_error: str,
    change: str,
) -> list[str]:
    failed = {r.name: r.error for r in readings if not r.ok}
    lines: list[str] = []

    def emit(name: str, text: str) -> None:
        if name in failed:
            lines.append(f"READER FAILED {name}: {failed[name]}")
        else:
            lines.append(text)

    notes = data.get("user_notes")
    if notes:
        lines.append("User notes:")
        lines.extend(str(notes).rstrip("\n").splitlines())
    elif "user_notes" in failed:
        lines.append(f"READER FAILED user_notes: {failed['user_notes']}")

    on = data.get("blocked_on") or []
    emit("board",
         f"board: open_prs {data.get('open_prs')}, free_claim_no_driver {data.get('free_claim_no_driver')}, "
         f"blocked {data.get('blocked')}" + (f" (on: {'; '.join(on)})" if on else ""))

    if "blocked_child" in failed:
        lines.append(f"READER FAILED blocked_child: {failed['blocked_child']}")
    else:
        rows = data.get("blocked_children") or []
        total = data.get("blocked_children_total") or len(rows)
        lines.append(f"blocked_child: {total}")
        for row in rows:
            lines.append(f"  {row.get('node')}, session {row.get('session')}, age {row.get('age_minutes')}m"
                         + (f" ({row.get('reason')})" if row.get("reason") else ""))
        if total > len(rows):
            lines.append(f"  ... and {total - len(rows)} more not shown")

    if "court" in failed:
        lines.append(f"READER FAILED court: {failed['court']}")
    else:
        emit("court", f"scope {scope}: {data.get('active_nodes')} active of {data.get('total_nodes')} nodes")
        shown = data.get("active_rows") or []
        for row in shown:
            lines.append(f"  {row['id']} {row['status']} worker {_dash(row['worker'])} pr {_dash(row['pr_number'])} session {_dash(row['session'])}")
        hidden = (data.get("active_nodes") or 0) - len(shown)
        if hidden > 0:
            lines.append(f"  ... {hidden} more not shown")

    if "capacity" in failed:
        lines.append(f"READER FAILED capacity: {failed['capacity']}")
    else:
        text = f"capacity: footprint {data.get('capacity_footprint')} / gate {data.get('capacity_gate')}"
        if data.get("capacity_disagree"):
            text += " DISAGREE"
        unparsed = data.get("capacity_unparsed_lines") or 0
        if unparsed:
            text += f" (unparsed_lines {unparsed})"
        lines.append(text)

    if "workers" in failed:
        lines.append(f"READER FAILED workers: {failed['workers']}")
        lines.append(f"worker activity unmeasured: {failed['workers']}")
    else:
        lines.append(f"workers: live {data.get('live_workers')}, oldest activity {data.get('oldest_worker_seen')}")

    if "crown" in failed:
        lines.append(f"READER FAILED crown: {failed['crown']}")
    else:
        lines.append(f"crown: {data.get('crown_total')} crowns, splits {data.get('crown_splits')}, "
                     f"disagreements {data.get('crown_disagreements')}")
        for anomaly in data.get("crown_anomalies") or []:
            lines.append(f"  {anomaly}")

    emit("drain", f"drain: undelivered {data.get('undelivered')}")
    emit("main_ci", f"main ci: {data.get('main_ci')}")

    lines.append(f"coverage: {data['coverage']} of {len(READING_NAMES)} readings ok")
    if data["readers_failed"]:
        named = ", ".join(f"{name} ({failed[name]})" for name in data["readers_failed"])
        lines.append(f"failed readers: {named}")

    if previous_error:
        lines.append(f"vs last beat: unmeasured ({previous_error})")
    elif previous is None:
        lines.append("vs last beat: none, this is the first canonical beat for this scope")
    else:
        parts = [
            f"{key} {previous['data'][key]} -> {data[key]}"
            for key in NUMERIC_DIFF_KEYS
            if key in previous["data"] and key in data
        ]
        body = ", ".join(parts) if parts else "no shared numeric keys to diff"
        lines.append(f"vs last beat ({previous.get('ts')}): {body}")
    lines.append(f"change: {change}")
    return lines


def _faq_prompt_needed(scope: str, readers_failed: list[str]) -> bool:
    if readers_failed:
        return True
    try:
        from fno.king.king_faq import entries_for_scope

        return not entries_for_scope(scope)
    except Exception:  # noqa: BLE001 - an unreadable store is unmeasured, never clean
        return True


# ---------------------------------------------------------------------------
# entry


def run_checkin(
    scope: str,
    *,
    emit: bool = True,
    as_json: bool = False,
    readers: list[tuple[str, Callable]] | None = None,
    events_path: Path | None = None,
) -> dict[str, Any]:
    ts = _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    readings = collect_readings(scope, readers)
    data = build_data(readings, scope)
    previous, previous_error = _previous_row(scope)
    previous_data = previous.get("data") if previous else None
    change = derive_change(previous_data, data, previous_error)
    data["change"] = change
    lines = render_lines(scope, readings, data, previous, previous_error, change)
    if _faq_prompt_needed(scope, data["readers_failed"]):
        lines.append(
            'fno agents king faq add --question "..." --answer "..." '
            '--specimen "<node or PR>, <date>" --exit "<the change that retires this>"'
        )

    emitted = False
    if emit:
        try:
            from fno.events import _build, append_event

            append_event(_build("reign_checkin", "loop", data), events_path=events_path)
            emitted = True
        except Exception as exc:  # noqa: BLE001 - the beat was still printed
            typer.echo(f"king: WARNING: reign_checkin row not emitted: {exc}", err=True)

    payload = {
        "scope": scope,
        "ts": ts,
        "coverage": data["coverage"],
        "readers_failed": data["readers_failed"],
        "change": change,
        "previous_ts": (previous or {}).get("ts"),
        "previous_error": previous_error,
        "emitted": emitted,
        "data": data,
        "readings": [
            {"name": r.name, "ok": r.ok, **({} if r.ok else {"error": r.error}),
             **({"value": r.value} if r.ok else {})}
            for r in readings
        ],
        "lines": lines,
    }
    if as_json:
        typer.echo(json.dumps(payload, indent=2))
    else:
        for line in lines:
            typer.echo(line)
    return payload
