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

The same tail also answers WHICH MODEL the worker is on (:func:`observed_model`),
for the same reason: a route stamped at spawn records intent, so it reports the
intended model in exactly the case an operator suspects a silent fallback, while
the transcript reports what the vendor answered as.

Read-only; never writes; never raises (every read degrades to ``unknown``).
"""
from __future__ import annotations

import re
import time
from pathlib import Path
from typing import Any, Callable, Optional

# Exact tag openers only: `<promise>` / `<promise ...>`, never `<promised>` or a
# word that merely starts with the tag name. Mirrors the loop runtime's protocol
# so truth and the runtime agree on what a marker is.
_PROMISE_RE = re.compile(r"<promise[>\s]")
_WATCHING_RE = re.compile(r"<watching[>\s]")
_HELP_RE = re.compile(r"<help[>\s]")

# "silent for hours" (the brief's wording): below this the worker is between
# turns; above it, nobody has touched the transcript and it is stalled. Surfaced
# in the rendered age so a mis-tuned window misleads less.
STALLED_AFTER_S = 2 * 3600

# Attention window: above this, a transcript-backed row reads as neglected and
# sorts to the top of every agents surface. Deliberately much tighter than
# STALLED_AFTER_S - that one is the reap-safety window and 7200s is correct for
# it, but its verdict reads reachable for anything dead inside it, and that gap
# is exactly where a stale-live worker hides (a fleet of dead workers once sat
# in it at 20 to 58 minutes idle, every one reporting live). Ordering and
# display only: no verdict, falsifier, or reap decision keys off this constant.
STALE_ATTENTION_S = 600

# Tail depth: enough to find the last assistant turn past trailing tool/user
# rows, bounded so a multi-MB transcript stays cheap (recent_records streams).
_TAIL_N = 40


def classify_tail(
    last_role: Optional[str],
    last_text: Optional[str],
    mtime_age_s: Optional[float],
    *,
    stalled_after_s: float = STALLED_AFTER_S,
) -> str:
    """Pure classifier over the LAST transcript turn (see module docstring).

    Content signals (promise/watching/help/question) apply ONLY when the last
    turn is the ASSISTANT's: a trailing user turn means the operator re-tasked
    or answered, which clears any stale assistant signal (a ``<promise>`` before
    a new user task is no longer ``done``; a question before the user's answer is
    no longer ``your-move``) -- the worker owes the next move, so mtime decides.

    ``watching`` outranks ``done`` because the loop runtime parks on
    ``<watching>`` even when a ``<promise>`` is also present; reporting ``done``
    there would contradict a still-parked worker.

    ``mtime_age_s is None`` means the age is unknowable (an opencode DB has no
    per-session file mtime), so stalled cannot be proven and the fallback is
    ``working`` -- truth never falsely asserts a silent session.
    """
    text = last_text or ""
    if last_role == "assistant":
        if _WATCHING_RE.search(text):
            return "watching"
        if _PROMISE_RE.search(text):
            return "done"
        if text.rstrip().endswith("?") or _HELP_RE.search(text):
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
    transcript_path: Optional[Path] = None,
) -> Optional[float]:
    """Seconds since the session's newest activity, or None if unknowable.

    Uses the x-a472 transcript resolver (content-aware across all project dirs),
    so a jsonl age reflects the LIVE transcript, not a stale stub. For opencode
    the store is shared, so a per-file mtime is meaningless; the age comes from
    the session's newest message timestamp instead (the store already indexes
    ``(session_id, time_created)``), so an opencode session can go ``stalled``
    like any other. None only when nothing resolves."""
    try:
        if agent in {"claude", "codex"} and transcript_path is not None:
            mtime = transcript_path.stat().st_mtime
        else:
            from fno.provenance.resolver import resolve_transcript

            rt = resolve_transcript(
                agent,
                session_id,
                cwd,
                projects_root=projects_root,
                codex_sessions_dir=codex_sessions_dir,
            )
            if not rt.resolved or not rt.transcript_path:
                return None
            if rt.kind == "opencode-db":
                activity_mtime = _opencode_activity_epoch(
                    session_id, Path(rt.transcript_path)
                )
                if activity_mtime is None:
                    return None
                mtime = activity_mtime
            else:
                mtime = Path(rt.transcript_path).stat().st_mtime
    except Exception:  # noqa: BLE001 — any read failure -> age unknown (working)
        return None
    now = now_s if now_s is not None else time.time()
    return max(0.0, now - mtime)


def _opencode_activity_epoch(session_id: str, db_path: Path) -> Optional[float]:
    """Newest message time for an opencode session, in epoch SECONDS, or None.

    Read-only single-column aggregate against the shared store (a stray write is
    destructive: WAL + ON DELETE CASCADE). ``time_updated`` is epoch
    milliseconds, so scale to seconds. None when the session has no message or
    the store is unreadable (the caller then reports age-unknown -> working)."""
    from fno.agents.discover import opencode_query

    try:
        rows = opencode_query(
            db_path,
            "SELECT MAX(time_updated) FROM message WHERE session_id = ?",
            (session_id,),
        )
    except Exception:  # noqa: BLE001 — locked / schema-drifted store -> unknown
        return None
    if not rows or rows[0][0] is None:
        return None
    return float(rows[0][0]) / 1000.0


# Transcript location and the observed-model read live in fno.provenance:
# fno.graph (core) needs them too, and a core module may not import from the
# agents runtime. Re-exported so this module's own callers are unaffected.
from fno.provenance.observed import (  # noqa: E402
    observed_model,
    resolve_transcript_path,
)


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
    observed_model, suggestions}``. ``state`` is one of done | watching |
    your-move | working | stalled | unknown; ``reason`` is set only for
    ``unknown`` (``not-found`` / ``no-records``); ``observed_model`` is the
    five-variant reading documented on :func:`observed_model` and is present on
    every path, including the ``unknown`` ones (a row that cannot be classified
    still renders)."""
    from fno.agents.peek import recent_records

    resolver = resolve if resolve is not None else _default_resolve

    def unknown(
        reason: str, *, session_id=None, suggestions=None, observed=None
    ) -> dict[str, Any]:
        return {
            "handle": handle,
            "state": "unknown",
            "reason": reason,
            "last_activity_age_s": None,
            "session_id": session_id,
            "observed_model": observed or {"kind": "no-transcript"},
            "suggestions": suggestions or [],
        }

    try:
        session, suggestions = resolver(handle)
    except Exception:  # noqa: BLE001 — a broken resolver hands off, never crashes
        # Distinct from not-found: a crashing resolver is a malfunction, while
        # not-found is the routine answer for a reaped or non-claude handle.
        # Callers suppress the routine one (see family1_truth_state_with_command
        # in crates/fno-agents/src/claude_ask.rs), so sharing a reason here would
        # silence the malfunction too.
        return unknown("resolver-error")
    if session is None:
        return unknown("not-found", suggestions=suggestions)

    agent = getattr(session, "agent", "claude") or "claude"
    sid = getattr(session, "session_id", "") or ""
    cwd = getattr(session, "cwd", "") or ""
    raw_transcript_path = getattr(session, "transcript_path", None)
    transcript_path = (
        Path(raw_transcript_path)
        if raw_transcript_path
        else resolve_transcript_path(
            agent, sid, cwd, projects_root, codex_sessions_dir
        )
    )
    observed = observed_model(agent, transcript_path)

    try:
        records = recent_records(
            agent,
            sid,
            cwd,
            tail_n,
            projects_root=projects_root,
            codex_sessions_dir=codex_sessions_dir,
            opencode_storage_dir=opencode_storage_dir,
            transcript_path=transcript_path,
        )
    except Exception:  # noqa: BLE001 — unsupported/unreadable harness -> unknown
        records = []
    if not records:
        return unknown("no-records", session_id=sid, observed=observed)

    # Classify the LAST turn, not the last assistant turn: a trailing user turn
    # must clear a stale assistant promise/question (see classify_tail).
    last = records[-1]
    age = _transcript_age_s(
        agent,
        sid,
        cwd,
        projects_root,
        codex_sessions_dir,
        now_s,
        transcript_path,
    )
    state = classify_tail(last.role, last.text, age, stalled_after_s=stalled_after_s)
    return {
        "handle": handle,
        "state": state,
        "reason": None,
        "last_activity_age_s": None if age is None else int(age),
        "session_id": sid,
        "observed_model": observed,
        "suggestions": [],
    }


def _default_resolve(handle: str):
    from fno.agents.discover import resolve_or_suggest

    return resolve_or_suggest(handle, require_alive=False)


_EVIDENCE = {
    "done": "promise emitted",
    "watching": "watching external",
    "your-move": "awaiting your reply",
    "working": "active",
    "stalled": "silent",
}


def _humanize_age(seconds: Optional[int]) -> str:
    """Fixed-width (4) so a column of these never reflows when a value rolls
    from ``59m`` to ``1h`` or past a day. ``None`` renders ``?`` padded to the
    same width - never ``0s``, because an unread probe and a zero-second age
    are different facts.
    """
    if seconds is None:
        return "   ?"
    if seconds < 60:
        unit, n = "s", seconds
    elif seconds < 3600:
        unit, n = "m", seconds // 60
    elif seconds < 86400:
        unit, n = "h", seconds // 3600
    else:
        unit, n = "d", seconds // 86400
    return f"{n}{unit}".rjust(4)


def _model_clause(observed: Any) -> str:
    """The model half of the truth line, or '' when there is nothing to say.

    ``no-transcript`` renders NOTHING rather than "unknown": a worker spawned
    two seconds ago has no transcript yet and must not look broken.
    ``not-file-backed`` renders nothing for the same reason one level over --
    an opencode worker will never have one, and saying so on every line would be
    noise, not news. The remaining two each get their own words, because a
    session that came up and never answered is the state this reading exists to
    make visible and must not read as healthy.
    """
    if not isinstance(observed, dict):
        return ""
    kind = observed.get("kind")
    if kind == "observed":
        return f" on {observed.get('model')}"
    if kind == "no-model-yet":
        return ", no model yet"
    if kind == "unreadable":
        return ", model unreadable"
    return ""


def render_truth(result: dict[str, Any]) -> str:
    """One legible human line with the state, the model, and the evidence."""
    handle = result.get("handle", "?")
    state = str(result.get("state") or "")
    if state == "unknown":
        line = f"truth {handle}: unknown ({result.get('reason') or 'unresolved'})"
        suggestions = result.get("suggestions") or []
        if suggestions:
            line += f" -- did you mean: {', '.join(suggestions)}"
        return line
    age = _humanize_age(result.get("last_activity_age_s"))
    model = _model_clause(result.get("observed_model"))
    return (
        f"truth {handle}: {state}{model} "
        f"({_EVIDENCE.get(state, '')}, last activity {age} ago)"
    )
