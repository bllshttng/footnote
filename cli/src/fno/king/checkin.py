"""``fno agents king checkin`` - one verb runs the reign check-in body.

Gathers the readings the reign skill names, prints them in a fixed order,
diffs against the previous canonical ``reign_checkin`` row, and emits that
row from the same dict it printed. It reads, prints, diffs and journals; it
never decides and never acts: every lever stays the king's judgment, and the
graph is never written.
Contract: docs/architecture/reign.md and skills/reign/SKILL.md.
"""
from __future__ import annotations

import datetime as _dt
import json
import os
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
    candidates: list[Path] = []
    root = os.environ.get("CLAUDE_PLUGIN_ROOT") or os.environ.get("CODEX_PLUGIN_ROOT")
    if root:
        candidates.append(Path(root) / "scripts" / "lib" / "canon-doc-marker.sh")
    # A checkout run (tests, a worktree, `uv run` from a repo) carries the
    # lib at the repo or plugin root: cli/src/fno/king/checkin.py sits four
    # directories below it.
    candidates.append(Path(__file__).resolve().parents[4] / "scripts" / "lib" / "canon-doc-marker.sh")
    flag = Path.home() / ".fno" / "plugin-root"
    if flag.exists():
        candidates.append(
            Path(flag.read_text(encoding="utf-8").strip()) / "scripts" / "lib" / "canon-doc-marker.sh"
        )
    for lib in candidates:
        if lib.exists():
            return lib
    raise ReaderError(f"canon doc marker lib missing (tried {', '.join(str(c) for c in candidates)})")


def _handoff_doc(scope: str) -> Path:
    # Mirrors `fno config paths handoff --scope` (paths_cli.handoff): the
    # crown-keyed doc is the newest existing one, and the check-in body
    # refreshes it before this verb reads it.
    from fno.paths import handoffs_dir

    key = "crown-" + re.sub(r"[^A-Za-z0-9._-]+", "-", scope.strip()).strip("-")
    if key == "crown-":
        raise ReaderError("empty scope names no canon doc")
    directory = handoffs_dir()

    def _mtime(path: Path) -> float:
        try:
            return path.stat().st_mtime
        except OSError:
            return 0.0

    existing = sorted(directory.glob(f"*-{key}.md"), key=_mtime)
    if not existing:
        raise ReaderError(f"no canon handoff doc for scope {scope}")
    return existing[-1]


def _r_user_notes(scope: str) -> tuple[Any, str]:
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


def _fetch_board(scope: str) -> dict:
    import subprocess as _sp

    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        raise ReaderError("the fno-agents binary was not found")
    state = None
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


def _r_board(scope: str, board_fn: Callable, court_fn: Callable) -> tuple[Any, str]:
    board = board_fn(scope)
    court = court_fn(scope)
    stuck = ((court.get("summary") or {}).get("stuck") or {}).get("blocked") or []
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
    return [
        {
            "node": row.get("id"),
            "session": row.get("session"),
            "age_minutes": row.get("age_minutes"),
            "reason": row.get("reason"),
        }
        for row in rows
    ], ""


def _fetch_court(scope: str) -> dict:
    from fno.agents.court import render_court

    try:
        return json.loads(render_court(as_json=True, nodes=True))
    except ValueError as exc:
        raise ReaderError(f"court payload did not parse: {exc}") from exc


def _r_court(scope: str, court_fn: Callable) -> tuple[Any, str]:
    court = court_fn(scope)
    mine = [c for c in court.get("crowns") or [] if c.get("scope") == scope]
    if not mine:
        raise ReaderError(f"the court names no crown for scope {scope}")
    nodes = mine[0].get("scope_nodes") or {}
    counts = nodes.get("counts") or {}
    total = nodes.get("total") or 0
    rows = []
    for n in nodes.get("nodes") or []:
        if n.get("status") in ("done", "superseded"):
            continue
        sessions = n.get("sessions") or []
        first = sessions[0] if sessions else None
        rows.append({
            "id": n.get("id"),
            "status": n.get("status"),
            "worker": n.get("worker"),
            "pr_number": n.get("pr_number"),
            "session": first.get("id") if isinstance(first, dict) else first,
        })
    return {"active_nodes": total - counts.get("done", 0), "total_nodes": total, "rows": rows}, ""


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
        "total": summary.get("total"),
        "splits": summary.get("splits"),
        "disagreements": summary.get("disagreements"),
        "anomalies": anomalies,
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
    runs = _gh_json(["gh", "api", f"repos/{owner_repo}/commits/{sha}/check-runs"])
    status = _gh_json(["gh", "api", f"repos/{owner_repo}/commits/{sha}/status"])
    check_runs = runs.get("check_runs") or []
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
    caches: dict[str, dict] = {"board": {}, "court": {}}

    def cached(kind: str, fetch: Callable) -> Callable:
        def read(scope: str) -> Any:
            if kind not in caches[kind]:
                caches[kind][kind] = fetch(scope)
            return caches[kind][kind]
        return read

    board_fn = cached("board", _fetch_board)
    court_fn = cached("court", _fetch_court)

    def r_board(scope: str) -> tuple[Any, str]:
        return _r_board(scope, board_fn, court_fn)

    def r_blocked_child(scope: str) -> tuple[Any, str]:
        return _r_blocked_child(scope, board_fn)

    def r_court(scope: str) -> tuple[Any, str]:
        return _r_court(scope, court_fn)

    def r_crown(scope: str) -> tuple[Any, str]:
        return _r_crown(scope, court_fn)

    fns: dict[str, Callable] = {
        "user_notes": _r_user_notes,
        "board": r_board,
        "blocked_child": r_blocked_child,
        "court": r_court,
        "capacity": _r_capacity,
        "workers": _r_workers,
        "crown": r_crown,
        "drain": _r_drain,
        "main_ci": _r_main_ci,
    }
    return [(name, fns[name]) for name in READING_NAMES]


def collect_readings(scope: str, readers: list[tuple[str, Callable]] | None = None) -> list[Reading]:
    readings = []
    for name, fn in readers if readers is not None else _readers():
        try:
            value, detail = fn(scope)
            readings.append(Reading(name=name, ok=True, value=value, detail=detail))
        except ReaderError as exc:
            readings.append(Reading(name=name, ok=False, error=str(exc)))
    return readings


# ---------------------------------------------------------------------------
# data, diff, change


def build_data(readings: list[Reading], scope: str) -> dict[str, Any]:
    """The one dict the lines print and the journal row stores."""
    data: dict[str, Any] = {"scope": scope}
    values = {r.name: r.value for r in readings if r.ok}
    board = values.get("board") or {}
    if "board" in values:
        data["open_prs"] = board["open_prs"]
        data["free_claim_no_driver"] = board["free_claim_no_driver"]
        data["blocked"] = board["blocked"]
    if "blocked_child" in values:
        data["blocked_children"] = values["blocked_child"]
    if "court" in values:
        data["active_nodes"] = values["court"]["active_nodes"]
    if "workers" in values:
        data["live_workers"] = values["workers"]["live_workers"]
        data["oldest_worker_seen"] = values["workers"]["oldest_worker_seen"]
    if "capacity" in values:
        data["capacity_footprint"] = values["capacity"]["footprint"]
        data["capacity_gate"] = values["capacity"]["gate"]
        data["capacity_disagree"] = values["capacity"]["disagree"]
    if "drain" in values:
        data["undelivered"] = values["drain"]
    if "main_ci" in values:
        data["main_ci"] = values["main_ci"]
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
    by_name = {r.name: r for r in readings}
    failed = {r.name: r.error for r in readings if not r.ok}
    lines: list[str] = []

    def emit(name: str, text: str) -> None:
        if name in failed:
            lines.append(f"READER FAILED {name}: {failed[name]}")
        else:
            lines.append(text)

    user_notes = by_name.get("user_notes")
    if user_notes is not None and user_notes.ok and user_notes.value:
        lines.append("User notes:")
        lines.extend(str(user_notes.value).rstrip("\n").splitlines())
    elif "user_notes" in failed:
        lines.append(f"READER FAILED user_notes: {failed['user_notes']}")

    board = by_name["board"]
    if board.ok:
        on = board.value.get("blocked_on") or []
        emit("board",
             f"board: open_prs {data.get('open_prs')}, free_claim_no_driver {data.get('free_claim_no_driver')}, "
             f"blocked {data.get('blocked')}" + (f" (on: {'; '.join(on)})" if on else ""))
    else:
        emit("board", "")

    if "blocked_child" in failed:
        lines.append(f"READER FAILED blocked_child: {failed['blocked_child']}")
    else:
        rows = data.get("blocked_children") or []
        lines.append(f"blocked_child: {len(rows)}")
        for row in rows:
            lines.append(f"  {row.get('node')}, session {row.get('session')}, age {row.get('age_minutes')}m"
                         + (f" ({row.get('reason')})" if row.get("reason") else ""))

    court = by_name["court"]
    if court.ok:
        emit("court", f"scope {scope}: {data.get('active_nodes')} active of {court.value['total_nodes']} nodes")
        for row in court.value["rows"][:MAX_COURT_ROWS]:
            lines.append(f"  {row['id']} {row['status']} worker {_dash(row['worker'])} pr {_dash(row['pr_number'])} session {_dash(row['session'])}")
        hidden = len(court.value["rows"]) - MAX_COURT_ROWS
        if hidden > 0:
            lines.append(f"  ... {hidden} more not shown")
    else:
        emit("court", "")

    if "capacity" in failed:
        lines.append(f"READER FAILED capacity: {failed['capacity']}")
    else:
        text = f"capacity: footprint {data.get('capacity_footprint')} / gate {data.get('capacity_gate')}"
        if data.get("capacity_disagree"):
            text += " DISAGREE"
        unparsed = by_name["capacity"].value.get("unparsed_lines") or 0
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
        crown = by_name["crown"].value
        lines.append(f"crown: {crown['total']} crowns, splits {crown['splits']}, disagreements {crown['disagreements']}")
        for anomaly in crown["anomalies"]:
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
