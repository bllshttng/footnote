"""Worktree overlap recording and recurrence reporting.

One hidden write path (`overlap_record`) fed by the SessionStart carrier's
machine-mode observation, and one read-only report (`overlaps`) over the
machine-global event journal. Reuses the canonical event append mutex (bounded
to 250 ms for the hook path) and the scoreboard fold's tolerant-read /
coverage-honesty conventions: a missing journal is a valid empty source, an
unreadable one is ``unknown``, and any malformed JSONL line makes the result
``partial`` rather than a silent zero.
"""
from __future__ import annotations

import json
import sys
from collections import Counter
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

import typer

from fno.events import ValidationError, append_event, validate, worktree_overlap_observed

# Three distinct observations in a rolling 28-day window warrant a Stage 3
# design review. Intentionally low: its only effect is recommending a pass,
# never a lock or an implementation decision.
RECURRENCE_THRESHOLD = 3
RECURRENCE_WINDOW_DAYS = 28
# Hook path bound: far below the 120s liveness window and the event subsystem's
# default 30s lock wait. Contention degrades to an unrecorded advisory, never a
# delayed or refused session.
HOOK_LOCK_BOUND_SECONDS = 0.25


class OverlapReadError(Exception):
    """The global journal exists but cannot be read (coverage: unknown).

    Carries the partial ``coverage`` dict built before the read failed so the
    caller can surface the path and error rather than reconstructing them.
    """

    def __init__(self, coverage: dict):
        self.coverage = coverage
        super().__init__(coverage.get("error") or coverage.get("path") or "unreadable")


def _normalize_key(path_str: Any) -> str:
    """Resolve a Git-directory path to a stable local identity for the digest.

    Symlinks and relative tails from `git rev-parse --git-common-dir` collapse
    to one canonical absolute path so the same physical repo/worktree yields one
    observation id regardless of how the helper reported it. A non-string or
    unresolvable value passes through untouched so validation rejects it later.
    """
    if not isinstance(path_str, str) or not path_str:
        return ""
    try:
        return str(Path(path_str).resolve())
    except (OSError, ValueError):
        return path_str


def _parse_event_ts(raw: Any) -> datetime | None:
    if not isinstance(raw, str) or not raw:
        return None
    try:
        dt = datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _read_overlap_events(journal: Path) -> tuple[list[dict], dict]:
    """Read worktree_overlap_observed lines with honest input coverage.

    Returns ``(events, coverage)``. A missing file is a valid empty source
    (``no_data``); an unreadable file raises :class:`OverlapReadError`
    (``unknown``); any malformed JSONL line makes the result ``partial``.
    """
    coverage: dict[str, Any] = {
        "state": "no_data",
        "malformed_lines": 0,
        "unreadable": False,
        "path": str(journal),
    }
    events: list[dict] = []
    try:
        text = journal.read_text(encoding="utf-8")
    except FileNotFoundError:
        # Only a genuinely missing journal is a valid empty source (no_data).
        # A stat/permission failure must not masquerade as no_data, so do not
        # pre-check .exists(); read directly and let the OSError arm classify
        # any other failure as unknown.
        return events, coverage
    except (OSError, UnicodeError) as exc:
        # UnicodeDecodeError (invalid UTF-8) is a ValueError, not an OSError,
        # so it must be caught here too or it escapes as a crash instead of the
        # promised structured `unknown` result.
        coverage["state"] = "unknown"
        coverage["unreadable"] = True
        coverage["error"] = f"{type(exc).__name__}: {exc}"
        raise OverlapReadError(coverage) from exc
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            coverage["malformed_lines"] += 1
            continue
        # A JSON-valid non-object line (bare scalar/array) is corrupt evidence,
        # not a clean line; count it malformed so coverage reads partial.
        if not isinstance(e, dict):
            coverage["malformed_lines"] += 1
            continue
        if e.get("type") == "worktree_overlap_observed":
            # A JSON-valid-but-schema-invalid overlap line would count toward
            # recurrence while coverage reported complete. Schema-validate it
            # and treat a rejection as a malformed line (partial coverage) so
            # broken evidence never reads as a clean zero or a false trend.
            try:
                validate(e)
            except ValidationError:
                coverage["malformed_lines"] += 1
                continue
            events.append(e)
        # A valid event of any other type is irrelevant here, not malformed.
    coverage["state"] = "partial" if coverage["malformed_lines"] else "complete"
    return events, coverage


def fold_overlap_events(events: list[dict], *, since_days: int, now: datetime) -> dict:
    """Deduplicate observations by id and fold the rolling window. Pure; no I/O.

    Distinct observations (not raw lines) drive every count, so repeated
    deliveries of one observation never inflate recurrence. An observation is
    in-window when its latest delivery timestamp falls inside the window.
    """
    if since_days < 1:
        # A zero/negative window would collapse to "nothing in window" and read
        # as no-data, misrepresenting absent evidence. Reject it loudly.
        raise ValueError("since_days must be at least 1")
    cutoff = now - timedelta(days=since_days)
    by_id: dict[str, dict] = {}
    for e in events:
        raw_data = e.get("data")
        data = raw_data if isinstance(raw_data, dict) else {}
        oid = data.get("observation_id")
        if not isinstance(oid, str) or not oid:
            continue
        ts = _parse_event_ts(e.get("ts"))
        slot = by_id.get(oid)
        if slot is None:
            by_id[oid] = {
                "observation_id": oid,
                "first_ts": ts,
                "latest_ts": ts,
                "worktree_key": data.get("worktree_key"),
                "observer_session_id": data.get("observer_session_id"),
                "peer_count": len(data.get("peer_session_ids") or [])
                if isinstance(data.get("peer_session_ids"), list)
                else 0,
            }
        elif ts is not None:
            if slot["first_ts"] is None or ts < slot["first_ts"]:
                slot["first_ts"] = ts
            if slot["latest_ts"] is None or ts > slot["latest_ts"]:
                slot["latest_ts"] = ts

    def _in(o: dict) -> bool:
        return o["latest_ts"] is not None and cutoff <= o["latest_ts"] <= now

    in_window = [o for o in by_id.values() if _in(o)]
    worktrees = Counter(o["worktree_key"] for o in in_window if o.get("worktree_key"))
    observers = {o["observer_session_id"] for o in in_window if o.get("observer_session_id")}
    distinct = len(in_window)
    first_ts = min((o["first_ts"] for o in in_window if o["first_ts"]), default=None)
    latest_ts = max((o["latest_ts"] for o in in_window if o["latest_ts"]), default=None)
    observations = sorted(in_window, key=lambda o: o["latest_ts"] or datetime.min.replace(tzinfo=timezone.utc))
    return {
        "distinct_observations": distinct,
        "distinct_worktrees": len(worktrees),
        "distinct_observers": len(observers),
        "first_ts": first_ts.isoformat() if first_ts else None,
        "latest_ts": latest_ts.isoformat() if latest_ts else None,
        "per_worktree": dict(worktrees.most_common()),
        "recurrence_threshold": RECURRENCE_THRESHOLD,
        "window_days": since_days,
        "recurrence_threshold_met": distinct >= RECURRENCE_THRESHOLD,
        "observations": [
            {
                "observation_id": o["observation_id"],
                "worktree_key": o["worktree_key"],
                "observer_session_id": o["observer_session_id"],
                "peer_count": o["peer_count"],
                "latest_ts": o["latest_ts"].isoformat() if o["latest_ts"] else None,
            }
            for o in observations
        ],
    }


def overlap_record(
    since_days: int = RECURRENCE_WINDOW_DAYS,
    *,
    journal: Path | None = None,
    stdin: str | None = None,
    now: datetime | None = None,
) -> tuple[dict, int]:
    """Record one observation from stdin; return (result_dict, exit_code).

    Always exits 0: the SessionStart carrier owns advisory rendering and the
    exit-zero contract. Recording and fold status are reported SEPARATELY so the
    carrier can distinguish ``unrecorded`` (recording failed) from
    ``count-unavailable`` (recorded, fold degraded).
    """
    from fno.paths import global_events_json

    journal = journal or global_events_json()
    raw = stdin if stdin is not None else sys.stdin.read()
    result: dict[str, Any] = {"recorded": False, "fold": {"state": "no_data"}}

    def _emit() -> int:
        typer.echo(json.dumps(result, separators=(",", ":"), default=str))
        return 0

    try:
        parsed = json.loads(raw) if raw.strip() else None
    except json.JSONDecodeError:
        parsed = None
    if not isinstance(parsed, dict):
        result["record_reason"] = "invalid-input"
        return result, _emit()
    obj: dict = parsed
    peers = obj.get("peer_session_ids")
    if not isinstance(peers, list) or not peers:
        result["record_reason"] = "invalid-input"
        return result, _emit()

    repository_key = _normalize_key(obj.get("repository_common_dir"))
    worktree_key = _normalize_key(obj.get("worktree_git_dir"))
    try:
        event = worktree_overlap_observed(
            observer_session_id=obj.get("observer_session_id", ""),
            peer_session_ids=peers,
            repository_key=repository_key,
            worktree_key=worktree_key,
            live_window_seconds=int(obj.get("live_window_seconds", 120)),
        )
    except (ValidationError, ValueError, TypeError) as exc:
        result["record_reason"] = f"schema-rejected: {exc}"
        return result, _emit()

    try:
        append_event(event, journal, lock_timeout_seconds=HOOK_LOCK_BOUND_SECONDS)
    except TimeoutError:
        result["record_reason"] = "lock-timeout"
        return result, _emit()
    except OSError as exc:
        result["record_reason"] = f"io-error: {exc}"
        return result, _emit()
    except Exception as exc:  # noqa: BLE001 - never propagate to fail the session
        result["record_reason"] = f"append-error: {exc}"
        return result, _emit()

    result["recorded"] = True
    result["observation_id"] = event["data"]["observation_id"]

    # Recorded: fold the window for recurrence. A degraded fold stays recorded.
    try:
        events, coverage = _read_overlap_events(journal)
    except OverlapReadError as exc:
        result["fold"] = {"state": "unknown", "error": str(exc)}
        return result, _emit()
    now = now or datetime.now(timezone.utc)
    try:
        folded = fold_overlap_events(events, since_days=since_days, now=now)
    except ValueError:
        # since_days < 1 is a degenerate window; degrade the fold rather than
        # crash (the carrier owns exit-zero). The carrier always passes 28.
        result["fold"] = {"state": "unknown", "error": "since_days < 1"}
        return result, _emit()
    result["fold"] = {"state": coverage["state"], **folded}
    return result, _emit()


def overlaps_report(
    since_days: int = RECURRENCE_WINDOW_DAYS,
    *,
    json_out: bool = False,
    journal: Path | None = None,
    now: datetime | None = None,
) -> tuple[dict, int]:
    """Build the deduplicated overlap report; return (report_dict, exit_code).

    Exit 0 for ``no_data`` (valid empty) and ``complete``; exit 1 for
    ``unknown`` (unreadable) and ``partial`` (malformed lines) so automation
    cannot mistake incomplete evidence for zero.
    """
    from fno.paths import global_events_json

    journal = journal or global_events_json()
    if since_days < 1:
        typer.echo(
            f"worktree overlaps: --since must be at least 1 (got {since_days})",
            err=True,
        )
        return {"state": "unknown", "since_days": since_days, "error": "since_days < 1"}, 1
    try:
        events, coverage = _read_overlap_events(journal)
    except OverlapReadError as exc:
        report: dict[str, Any] = {"state": "unknown", "coverage": exc.coverage, "distinct_observations": None}
        return report, 1
    now = now or datetime.now(timezone.utc)
    folded = fold_overlap_events(events, since_days=since_days, now=now)
    report = {
        "state": coverage["state"],
        "since_days": since_days,
        "coverage": {
            "path": coverage["path"],
            "malformed_lines": coverage["malformed_lines"],
            "journal_lines": len(events),
        },
        **folded,
    }
    exit_code = 0 if coverage["state"] in ("no_data", "complete") else 1
    return report, exit_code


def render_overlaps_text(report: dict) -> str:
    """Human-readable one-screen rendering of the overlap report."""
    state = report.get("state")
    cov = report.get("coverage") or {}
    where = cov.get("path", "the overlap journal")
    if state == "no_data":
        return (
            f"no worktree overlap observations in the last "
            f"{report.get('since_days', RECURRENCE_WINDOW_DAYS)} days "
            f"(journal absent at {where})"
        )
    if state == "unknown":
        reason = report.get("error") or "overlap journal unreadable"
        return f"unknown: {reason} at {where} ({cov.get('malformed_lines', 0)} malformed lines)"
    lines: list[str] = []
    n = report.get("distinct_observations", 0)
    lines.append(
        f"{n} distinct worktree overlap observation(s) in the last "
        f"{report.get('window_days', RECURRENCE_WINDOW_DAYS)} days"
    )
    lines.append(
        f"  worktrees: {report.get('distinct_worktrees', 0)}  "
        f"observers: {report.get('distinct_observers', 0)}"
    )
    if report.get("first_ts") or report.get("latest_ts"):
        lines.append(
            f"  first: {report.get('first_ts')}  latest: {report.get('latest_ts')}"
        )
    for wt, count in (report.get("per_worktree") or {}).items():
        lines.append(f"  {count}x  {wt}")
    coverage = report.get("coverage") or {}
    if state == "partial" or coverage.get("malformed_lines"):
        lines.append(
            f"  coverage: PARTIAL - {coverage.get('malformed_lines', 0)} malformed "
            f"journal line(s) skipped (counts may understate)"
        )
    if report.get("recurrence_threshold_met"):
        lines.append(
            f"  recurrence reached {n}/{report.get('recurrence_threshold', RECURRENCE_THRESHOLD)}: "
            "a Stage 3 worktree-write-lock design node is now warranted."
        )
    lines.append("  report: fno worktree overlaps --since 28 --json")
    return "\n".join(lines)
