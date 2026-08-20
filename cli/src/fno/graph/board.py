"""``fno backlog board`` - three sections, one line each, zero GitHub reads.

Sources are on disk only: the graph (``read_graph``) and the pr-status cache
(``newest_row_offline``). This module must never import ``fno.pr._status``,
``fno.pr._rest``, or ``fno.pr._cache.cached_status`` - any of those reaches a
live GitHub read, and a verb whose job is showing the board must never be the
thing that exhausts the GraphQL quota (test_board.py pins both the raising-
subprocess probe and this import list).

An unreadable source resolves to unknown, never to an empty section: an empty
Just-finished section must mean nothing landed, not that the read failed.
"""
from __future__ import annotations

import re
from datetime import datetime, timedelta, timezone
from typing import Optional

from fno.graph._intake import _project_key
from fno.graph.render import in_progress_epic_ids, make_kanban_classifiers
from fno.graph.statuses import live_claimed_node_ids
from fno.pr._cache import finite_or_zero, newest_row_offline

MAX_ROWS_PER_SECTION = 5
MAX_TITLE_WORDS = 12
RECENT_WINDOW_HOURS = 24
FALLBACK_RECENT_COUNT = 5

_PR_URL_RE = re.compile(r"/([^/]+)/([^/]+)/pull/\d+")


def _slug_key_from_pr_url(pr_url: str) -> Optional[str]:
    """``owner--repo`` from a PR URL, or None. Never touches git or the network."""
    m = _PR_URL_RE.search(pr_url or "")
    if not m:
        return None
    return f"{m.group(1)}--{m.group(2)}"


def _cap_words(text: str, n: int = MAX_TITLE_WORDS) -> str:
    """Cap a title at ``n`` words. The blocking fact is never capped - it is
    the reason the line exists."""
    title = (text or "").replace("\n", " ").strip() or "(untitled)"
    words = title.split()
    if len(words) <= n:
        return title
    return " ".join(words[:n]) + "…"


def _parse_iso(value: object) -> Optional[datetime]:
    if not isinstance(value, str) or not value:
        return None
    try:
        dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)


def _format_age(seconds: float) -> str:
    # Several near-identical seconds/timedelta-to-string age humanizers
    # already exist (fno.mail.cli._humanize_age and others); this is its
    # own copy, not a reuse, because a board line has no use for the
    # sub-minute precision those give - it floors at "1m ago".
    seconds = max(0, int(seconds))
    if seconds < 3600:
        return f"{max(1, seconds // 60)}m ago"
    if seconds < 86400:
        return f"{seconds // 3600}h ago"
    return f"{seconds // 86400}d ago"


def _age_from_iso(value: object, now: datetime) -> str:
    dt = _parse_iso(value)
    if dt is None:
        return "unknown age"
    return _format_age((now - dt).total_seconds())


def _age_from_epoch(ts: object, now: datetime) -> str:
    # The SAME guard the cache applies to the SAME row. This path reads a
    # pr-status row through `newest_row_offline`, and a bare `float()` here
    # crashed on the `Infinity` / `NaN` that `json.loads` accepts by default:
    # `inf` raised OverflowError and `nan` raised ValueError from the `int()`
    # inside `_format_age`, which sits outside the try that used to be here.
    # One guard, every reader of the row - not one guard and a docstring.
    #
    # finite_or_zero closes non-finite, NOT out-of-range, and the difference
    # showed here: a row carrying "ts": 1e18 is perfectly finite, so it passed
    # the guard, went massively negative against now, and `_format_age` floored
    # it at 0 - printing "checks green (as of 1m ago)" off a garbage stamp,
    # which is the exact misreport the guard was added to stop. A future stamp
    # is not an age, so it reads as unknown rather than as fresh.
    if ts is None:
        return "unknown age"
    ts_f = finite_or_zero(ts)
    if not ts_f:
        return "unknown age"
    # The tolerance is not slop. `compute_board` samples `now` once and then
    # reads rows, so a concurrent `fno pr status` writing a row mid-render is
    # legitimately a second or two ahead of it, and flooring that at 0 was the
    # OLD behaviour worth keeping. A garbage stamp misses by decades, never by
    # seconds, so a minute of headroom separates the two cleanly.
    delta = now.timestamp() - ts_f
    if delta < -60:
        return "unknown age"
    return _format_age(max(delta, 0.0))


def _just_finished_fact(entry: dict, now: datetime) -> str:
    age = _age_from_iso(entry.get("completed_at"), now)
    pr = entry.get("pr_number")
    return f"PR {pr} merged {age}" if pr else f"merged {age}"


def _in_progress_fact(entry: dict, now: datetime) -> str:
    pr = entry.get("pr_number")
    if not pr:
        return "no PR yet"
    slug_key = _slug_key_from_pr_url(entry.get("pr_url") or "")
    row = newest_row_offline(slug_key, str(pr)) if slug_key else None
    if not row:
        return f"PR {pr} unknown (no cached row)"
    output = row.get("output") or {}
    verdict = output.get("verdict") or "unknown"
    age = _age_from_epoch(row.get("ts"), now)
    return f"PR {pr} checks {verdict} (as of {age})"


def _on_deck_fact(entry: dict, id_to_entry: dict) -> str:
    for bid in entry.get("blocked_by") or []:
        if not isinstance(bid, str):
            continue
        blocker = id_to_entry.get(bid)
        if blocker and not blocker.get("completed_at"):
            return f"blocked by {bid}"
    return "ready"


def _row(entry: dict, fact: str) -> dict:
    return {
        "id": entry.get("id", "?"),
        "title": _cap_words(entry.get("title", "")),
        "fact": fact,
    }


def _section(rows: list[dict], *, more: int = 0, note: Optional[str] = None,
             unknown: Optional[str] = None) -> dict:
    return {"rows": rows, "more": more, "note": note, "unknown": unknown}


def compute_board(
    entries: list[dict],
    *,
    project: Optional[str] = None,
    now: Optional[datetime] = None,
) -> dict:
    """Just finished / In progress / On deck, off the graph and the pr-status
    cache only. Never calls subprocess - not for GitHub, not for local git."""
    now = now or datetime.now(timezone.utc)
    all_entries = [e for e in entries if isinstance(e, dict)]
    id_to_entry = {
        e["id"]: e for e in all_entries if isinstance(e.get("id"), str)
    }
    scoped = (
        [e for e in all_entries if _project_key(e) == project]
        if project
        else all_entries
    )

    try:
        # strict=True is the only correct mode here: the non-strict default
        # swallows a claims-subsystem fault into an empty set, which would
        # render "nobody is working on anything" instead of the required
        # unknown - the exact lie this module exists to refuse.
        live_claimed = frozenset(live_claimed_node_ids(strict=True))
        claims_reason: Optional[str] = None
    except Exception as exc:  # noqa: BLE001 - render unknown, never a fabricated empty set
        live_claimed = frozenset()
        claims_reason = f"live claim state unavailable ({exc})"

    board_order, column_for = make_kanban_classifiers(scoped, live_claimed=live_claimed)
    # Epics never carry their own claim or status=="in_progress" - sessions
    # claim leaf children, not containers - so an in-progress epic (a done or
    # claimed child) needs this separate promotion set to land in In progress
    # instead of falling through to On deck (mirrors _kanban_column's own
    # epic-promotion overlay in render.py).
    in_progress_epics = in_progress_epic_ids(scoped, live_claimed)

    # -- Just finished --
    def _completed_dt(e: dict) -> datetime:
        return _parse_iso(e.get("completed_at")) or datetime.min.replace(tzinfo=timezone.utc)

    completed = sorted(
        (e for e in scoped if e.get("completed_at")), key=_completed_dt, reverse=True
    )
    recent = [e for e in completed if now - _completed_dt(e) <= timedelta(hours=RECENT_WINDOW_HOURS)]
    used_fallback = not recent and bool(completed)
    finished = recent if recent else completed[:FALLBACK_RECENT_COUNT]
    just_finished = _section(
        [_row(e, _just_finished_fact(e, now)) for e in finished[:MAX_ROWS_PER_SECTION]],
        more=max(0, len(finished) - MAX_ROWS_PER_SECTION),
        note="no completions in the last 24h - showing the most recent" if used_fallback else None,
    )

    # -- In progress --
    if claims_reason is not None:
        in_progress = _section([], unknown=claims_reason)
        in_progress_ids: frozenset = frozenset()
    else:
        ip_entries = sorted(
            (
                e for e in scoped
                if isinstance(e.get("id"), str)
                and not e.get("completed_at")
                and (
                    e["id"] in live_claimed
                    or e.get("status") == "in_progress"
                    or e["id"] in in_progress_epics
                )
            ),
            key=board_order,
        )
        in_progress_ids = frozenset(e["id"] for e in ip_entries)
        in_progress = _section(
            [_row(e, _in_progress_fact(e, now)) for e in ip_entries[:MAX_ROWS_PER_SECTION]],
            more=max(0, len(ip_entries) - MAX_ROWS_PER_SECTION),
        )

    # -- On deck: board order, excluding done/deferred/superseded/roadmap and
    # anything already counted as In progress. status=="in_progress" and
    # in_progress_epics are checked directly, independent of in_progress_ids
    # (which the claims-fault branch leaves empty) - a node the graph itself
    # marks in_progress, or an epic with a claimed/done child, must never
    # fall through to On deck just because the live-claims read failed.
    deck_entries = [
        e for e in sorted(scoped, key=board_order)
        if column_for(e) not in (None, "Done")
        and isinstance(e.get("id"), str)
        and e["id"] not in in_progress_ids
        and e.get("status") != "in_progress"
        and e["id"] not in in_progress_epics
    ]
    on_deck = _section(
        [_row(e, _on_deck_fact(e, id_to_entry)) for e in deck_entries[:MAX_ROWS_PER_SECTION]],
        more=max(0, len(deck_entries) - MAX_ROWS_PER_SECTION),
    )

    return {"just_finished": just_finished, "in_progress": in_progress, "on_deck": on_deck}
