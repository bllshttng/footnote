"""Fold labelled event-journal rows into a failure-pattern leaderboard."""
from __future__ import annotations

from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from fno.events import _utc_timestamp
from fno.events.log import normalize_event

HEALTHY = frozenset({
    "pass", "found", "stamped", "graduated", "idempotent_noop", "allow",
    "DonePRGreen", "DoneAdvisory", "DoneDelivery", "DoneUnreviewed",
    "DoneAwaitingMerge", "DoneAwaitingReview",
})
NOISE_TYPES = frozenset({"guard_decision", "control_plane_tick", "gh_probe"})
_LABEL_KEYS = ("reason", "outcome", "verdict", "termination_reason")


def load_events(paths: list[Path], since: datetime | None = None) -> tuple[list[dict], dict]:
    from fno.scoreboard.fold import read_jsonl_events_with_coverage

    result = read_jsonl_events_with_coverage(paths, kinds=None)
    if since is None:
        return result["events"], result["coverage"]
    cutoff = since if since.tzinfo is not None else since.replace(tzinfo=timezone.utc)
    rows = []
    for row in result["events"]:
        timestamp = _timestamp(row)
        if timestamp is None or timestamp >= cutoff:
            rows.append(row)
    return rows, result["coverage"]


def label_of(row: dict) -> str | None:
    data = _view(row)["data"]
    for key in _LABEL_KEYS:
        value = data.get(key)
        if value is not None and str(value):
            return str(value)[:60]
    return None


def _timestamp(row: dict) -> datetime | None:
    return _utc_timestamp(row.get("ts") or row.get("timestamp"))


def _view(row: dict) -> dict[str, Any]:
    view = normalize_event(row)
    data = view["data"]
    if not view.get("session_id") and isinstance(data, dict):
        view["session_id"] = data.get("attester_session_id")
    return view


def _event_type(row: dict) -> str | None:
    value = _view(row).get("type")
    return str(value) if value is not None else None


def _session_id(row: dict) -> str | None:
    value = _view(row).get("session_id")
    return str(value) if value else None


def _node_id(row: dict) -> str | None:
    value = _view(row).get("node_id")
    return str(value) if value else None


def _pattern(row: dict, *, include_all: bool = False) -> str | None:
    event_type = _event_type(row)
    label = label_of(row)
    if not event_type or label is None:
        return None
    if not include_all and (label in HEALTHY or event_type in NOISE_TYPES):
        return None
    return f"{event_type}:{label}"


def _ordered_rows(rows: list[dict]) -> list[dict]:
    return sorted(enumerate(rows), key=lambda item: (_timestamp(item[1]) is None,
                                                      _timestamp(item[1]) or datetime.max.replace(tzinfo=timezone.utc),
                                                      item[0]))


def _suspects_for_pattern(rows: list[dict], target: str, *, window: int,
                          include_all: bool) -> list[dict]:
    sessions: dict[str, list[dict]] = defaultdict(list)
    for row in rows:
        session = _session_id(row)
        if session is not None:
            sessions[session].append(row)
    occurrences = [
        (session, index)
        for session, session_rows in sessions.items()
        for index, row in enumerate(session_rows)
        if _pattern(row, include_all=include_all) == target
    ]
    target_sessions = {session for session, _ in occurrences}
    total_pattern_rows = sum(
        1 for row in rows if _pattern(row, include_all=include_all) is not None
    )
    candidate_sessions: dict[str, set[str]] = defaultdict(set)
    candidate_counts: dict[str, int] = defaultdict(int)
    for session, index in occurrences:
        for row in sessions[session][max(0, index - window):index]:
            pattern = _pattern(row, include_all=include_all)
            if pattern is None or pattern == target:
                continue
            candidate_sessions[pattern].add(session)
            candidate_counts[pattern] += 1
    suspects = []
    for pattern, support_sessions in candidate_sessions.items():
        support = len(support_sessions)
        if support < 2:
            continue
        conditional = support / len(target_sessions) if target_sessions else 0.0
        global_prevalence = candidate_counts[pattern] / total_pattern_rows if total_pattern_rows else 0.0
        lift = conditional / global_prevalence if global_prevalence else 0.0
        suspects.append({
            "pattern": pattern,
            "sessions": support,
            "count": candidate_counts[pattern],
            "lift": round(lift, 4),
        })
    return sorted(suspects, key=lambda item: (-item["lift"], -item["sessions"], item["pattern"]))[:3]


def build_leaderboard(rows: list[dict], *, window: int = 20,
                      include_all: bool = False) -> dict[str, Any]:
    ordered = [row for _, row in _ordered_rows(rows)]
    entries: dict[str, dict[str, Any]] = {}
    for row in ordered:
        pattern = _pattern(row, include_all=include_all)
        if pattern is None:
            continue
        entry = entries.setdefault(pattern, {
            "pattern": pattern,
            "count": 0,
            "sessions": 0,
            "nodes": 0,
            "unassigned": 0,
            "first_seen": None,
            "last_seen": None,
            "suspects": [],
            "_session_ids": set(),
            "_node_ids": set(),
        })
        entry["count"] += 1
        session = _session_id(row)
        node = _node_id(row)
        if session is None:
            entry["unassigned"] += 1
        else:
            entry["_session_ids"].add(session)
        if node is not None:
            entry["_node_ids"].add(node)
        timestamp = row.get("ts") or row.get("timestamp")
        if entry["first_seen"] is None:
            entry["first_seen"] = timestamp
        entry["last_seen"] = timestamp
    for entry in entries.values():
        entry["sessions"] = len(entry.pop("_session_ids"))
        entry["nodes"] = len(entry.pop("_node_ids"))
        entry["suspects"] = _suspects_for_pattern(
            ordered, entry["pattern"], window=window, include_all=include_all
        )
    leaderboard = sorted(
        entries.values(),
        key=lambda item: (
            -item["sessions"],
            -item["count"],
            0 if item["pattern"].startswith("termination:") else 1,
            item["pattern"],
        ),
    )
    return {"leaderboard": leaderboard}


def drilldown(rows: list[dict], pattern: str, *, window: int = 20,
              limit: int = 5, include_all: bool = False) -> dict[str, Any]:
    ordered = [row for _, row in _ordered_rows(rows)]
    sessions: dict[str, list[dict]] = defaultdict(list)
    for row in ordered:
        session = _session_id(row)
        if session is not None:
            sessions[session].append(row)
    fires = [
        (session, index, row)
        for session, session_rows in sessions.items()
        for index, row in enumerate(session_rows)
        if _pattern(row, include_all=include_all) == pattern
    ]
    fires.sort(key=lambda item: (_timestamp(item[2]) or datetime.min.replace(tzinfo=timezone.utc)), reverse=True)
    details = []
    for session, index, row in fires[:limit]:
        chain = []
        for event in sessions[session][max(0, index - window):index + 1]:
            chain.append({
                "ts": event.get("ts") or event.get("timestamp"),
                "type": _event_type(event),
                "label": label_of(event),
            })
        details.append({
            "session_id": session,
            "node_id": _node_id(row),
            "ts": row.get("ts") or row.get("timestamp"),
            "chain": chain,
        })
    summary = build_leaderboard(rows, window=window, include_all=include_all)["leaderboard"]
    match = next((entry for entry in summary if entry["pattern"] == pattern), None)
    return {
        "pattern": pattern,
        "count": match["count"] if match else 0,
        "unassigned": match["unassigned"] if match else 0,
        "sessions": details,
        "suspects": match["suspects"] if match else [],
    }
