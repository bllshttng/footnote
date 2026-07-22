"""``fno agents truth``: a worker's supervision state from its transcript TAIL.

agent-view's working/idle answers "is the model producing tokens right now" - a
question no supervisor has. Even when honest, a live collaboration reads Idle
~90% of wall-clock, and a turn ending in a prose question is indistinguishable
from "nothing to do". The supervision-grade states (done / watching-external /
your-move / working / stalled) need the transcript TAIL, not process state.

Liveness here is transcript-keyed ONLY. argv, pid, the daemon record, and
state.json's ``state`` field were EACH caught lying about a live session in one
evening (x-a472 forensics: a claimed bg-spare keeps the blank's ``bg-spare``
argv for life, its agent-view row freezes at Idle, and state.json wrote ``done``
mid-conversation). The transcript was the only surface that told the truth at
every point, so it is the only one this module reads.

State precedence (a content signal in the last assistant turn beats the mtime
fallback, so an old ``<promise>`` is still ``done`` and an old question is still
``your-move``):

    <promise ...>                 -> done         (mission declared complete)
    <watching ...>                -> watching     (armed on an external check)
    ends in '?' OR <help ...>     -> your-move    (needs the operator)
    (none) transcript fresh       -> working
    (none) silent for hours       -> stalled
    unresolvable / no records     -> unknown      (hands off, fail-quiet)

Read-only; never writes; never raises (every read degrades to ``unknown``).
"""
from __future__ import annotations

import time
from pathlib import Path
from typing import Any, Callable, Optional

# "silent for hours" (the brief's wording): below this the worker is between
# turns; above it, nobody has touched the transcript and it is stalled. Surfaced
# in the rendered age so a mis-tuned window misleads less.
STALLED_AFTER_S = 2 * 3600

# Tail depth: enough to find the last assistant turn past trailing tool/user
# rows, bounded so a multi-MB transcript stays cheap (recent_records streams).
_TAIL_N = 40


def classify_tail(
    last_assistant_text: Optional[str],
    mtime_age_s: Optional[float],
    *,
    stalled_after_s: float = STALLED_AFTER_S,
) -> str:
    """Pure classifier over the tail signals (see module docstring for the table).

    ``mtime_age_s is None`` means the transcript age is unknowable (e.g. an
    opencode DB), so stalled cannot be proven and the fallback is ``working``.
    """
    text = last_assistant_text or ""
    # Open-ended tag prefixes match both bare (`<promise>`) and attributed
    # (`<watching reason=...>`) forms.
    if "<promise" in text:
        return "done"
    if "<watching" in text:
        return "watching"
    if text.rstrip().endswith("?") or "<help" in text:
        return "your-move"
    if mtime_age_s is not None and mtime_age_s > stalled_after_s:
        return "stalled"
    return "working"


def _transcript_age_s(
    agent: str,
    session_id: str,
    cwd: str,
    projects_root: Optional[Path],
    codex_sessions_dir: Optional[Path],
    now_s: Optional[float],
) -> Optional[float]:
    """Seconds since the session's transcript file was last written, or None.

    Uses the x-a472 transcript resolver (newest across all project dirs), so the
    age reflects the LIVE worktree transcript, not a stale canonical stub. None
    when there is no per-file mtime to read (unresolved, or an opencode store
    whose single DB mtime says nothing about one session)."""
    from fno.provenance.resolver import resolve_transcript

    try:
        rt = resolve_transcript(
            agent,
            session_id,
            cwd,
            projects_root=projects_root,
            codex_sessions_dir=codex_sessions_dir,
        )
        if not rt.resolved or not rt.transcript_path or rt.kind != "jsonl":
            return None
        mtime = Path(rt.transcript_path).stat().st_mtime
    except Exception:  # noqa: BLE001 — any read failure -> age unknown (working)
        return None
    now = now_s if now_s is not None else time.time()
    return max(0.0, now - mtime)


def resolve_session_truth(
    handle: str,
    *,
    resolve: Optional[Callable[[str], tuple]] = None,
    projects_root: Optional[Path] = None,
    codex_sessions_dir: Optional[Path] = None,
    opencode_storage_dir: Optional[Path] = None,
    now_s: Optional[float] = None,
    stalled_after_s: float = STALLED_AFTER_S,
    tail_n: int = _TAIL_N,
) -> dict[str, Any]:
    """Resolve ``handle`` and classify its transcript tail. Never raises.

    Returns ``{handle, state, reason, last_activity_age_s, session_id,
    suggestions}``. ``state`` is one of done | watching | your-move | working |
    stalled | unknown; ``reason`` is set only for ``unknown`` (``not-found`` /
    ``no-records``)."""
    from fno.agents.peek import recent_records

    resolver = resolve if resolve is not None else _default_resolve

    def unknown(reason: str, *, session_id=None, suggestions=None) -> dict[str, Any]:
        return {
            "handle": handle,
            "state": "unknown",
            "reason": reason,
            "last_activity_age_s": None,
            "session_id": session_id,
            "suggestions": suggestions or [],
        }

    try:
        session, suggestions = resolver(handle)
    except Exception:  # noqa: BLE001 — a broken resolver hands off, never crashes
        return unknown("not-found")
    if session is None:
        return unknown("not-found", suggestions=suggestions)

    agent = getattr(session, "agent", "claude") or "claude"
    sid = getattr(session, "session_id", "") or ""
    cwd = getattr(session, "cwd", "") or ""

    try:
        records = recent_records(
            agent,
            sid,
            cwd,
            tail_n,
            projects_root=projects_root,
            codex_sessions_dir=codex_sessions_dir,
            opencode_storage_dir=opencode_storage_dir,
        )
    except Exception:  # noqa: BLE001 — unsupported/unreadable harness -> unknown
        records = []
    if not records:
        return unknown("no-records", session_id=sid)

    last_assistant = next(
        (r.text for r in reversed(records) if r.role == "assistant"), ""
    )
    age = _transcript_age_s(agent, sid, cwd, projects_root, codex_sessions_dir, now_s)
    state = classify_tail(last_assistant, age, stalled_after_s=stalled_after_s)
    return {
        "handle": handle,
        "state": state,
        "reason": None,
        "last_activity_age_s": None if age is None else int(age),
        "session_id": sid,
        "suggestions": [],
    }


def _default_resolve(handle: str):
    from fno.agents.discover import resolve_or_suggest

    return resolve_or_suggest(handle)


_EVIDENCE = {
    "done": "promise emitted",
    "watching": "watching external",
    "your-move": "awaiting your reply",
    "working": "active",
    "stalled": "silent",
}


def _humanize_age(seconds: Optional[int]) -> str:
    if seconds is None:
        return "?"
    if seconds < 60:
        return f"{seconds}s"
    if seconds < 3600:
        return f"{seconds // 60}m"
    return f"{seconds // 3600}h"


def render_truth(result: dict[str, Any]) -> str:
    """One legible human line with the state and its evidence."""
    handle = result.get("handle", "?")
    state = result.get("state")
    if state == "unknown":
        line = f"truth {handle}: unknown ({result.get('reason') or 'unresolved'})"
        suggestions = result.get("suggestions") or []
        if suggestions:
            line += f" -- did you mean: {', '.join(suggestions)}"
        return line
    age = _humanize_age(result.get("last_activity_age_s"))
    return f"truth {handle}: {state} ({_EVIDENCE.get(state, '')}, last activity {age} ago)"
