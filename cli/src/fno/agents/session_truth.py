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
fallback, EXPIRED past ``STALLED_AFTER_S``: a tag describes a TURN, and a turn
stops being news once the transcript has been silent past the bound. An old
``<promise>`` is still ``done`` because a promise is a turn OUTCOME, not news;
an old ``<watching>`` or old question decays to ``stalled`` -- x-c1a3 measured
a dead worker reading ``watching`` at any age, and a wedged node blocked on it):

    <promise ...>                 -> done         (mission declared complete, any age)
    <watching ...>                -> watching     (fresh; past the bound -> stalled)
    ends in '?' OR <help ...>     -> your-move    (fresh; past the bound -> stalled)
    ends in [Y/n] / (y/N) / etc.  -> your-move    (an option prompt, x-1182; fresh)
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

import json
import re
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Optional

# Exact tag openers only: `<promise>` / `<promise ...>`, never `<promised>` or a
# word that merely starts with the tag name. Mirrors the loop runtime's protocol
# so truth and the runtime agree on what a marker is.
_PROMISE_RE = re.compile(r"<promise[>\s]")
_WATCHING_RE = re.compile(r"<watching[>\s]")
_HELP_RE = re.compile(r"<help[>\s]")

# A trailing interactive option prompt (x-1182): [Y/n], [y/N], (y/N), (Y/n),
# or a bracketed numbered menu like [1/2/3]. Matched only at the END of the
# rstripped tail, same position as the trailing "?" check below - a [Y/n]
# mentioned mid-paragraph is prose about a prompt, not a prompt. This is a
# DIFFERENT grammar from the pane answerable-prompt manifest in
# crates/fno-agents/src/manifests/*.toml + scrape.rs: that one classifies a
# rendered TUI SCREEN by glyph and rule priority, this one classifies
# assistant PROSE in a transcript. Shelling to Rust per tail read would cost
# more than this regex is worth.
_OPTION_PROMPT_RE = re.compile(r"[\[(](?:[Yy]/[Nn]|\d+(?:/\d+)+)[\])]\s*$")

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
# It is also the progress axis's positive-evidence window: a written `working`
# state older than this resolves to progress unknown, never advancing. No
# reachability, falsifier, or reap decision keys off this constant.
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

    Expiry (x-c1a3): a tag describes a turn, and a turn stops being news once
    the transcript has been silent past ``stalled_after_s`` -- the content arms
    checked the age AFTER answering, which made the bound unreachable for
    exactly the two states a liveness reader trusts. ``done`` is a turn OUTCOME
    and does not go stale.
    """
    text = last_text or ""
    stale = mtime_age_s is not None and mtime_age_s > stalled_after_s
    if last_role == "assistant":
        if _WATCHING_RE.search(text):
            return "stalled" if stale else "watching"
        if _PROMISE_RE.search(text):
            return "done"
        stripped = text.rstrip()
        if stripped.endswith("?") or _HELP_RE.search(text) or _OPTION_PROMPT_RE.search(stripped):
            return "stalled" if stale else "your-move"
    if stale:
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
) -> tuple[Optional[float], Optional[float], Optional[str]]:
    """``(activity_epoch, age_s, basis)``, all three None if unknowable.

    The epoch is returned alongside the age derived from it so the caller can
    emit an absolute stamp and a relative age from the SAME read; computing them
    from two reads is how a stamp and an age disagree about one transcript.
    ``basis`` names the instrument that answered - ``mtime`` (a file stat,
    which OVERSTATES liveness: trailing untimestamped records keep the file
    young while the conversation is silent, x-54cf) or ``opencode-db`` (the
    store's newest message time).

    Uses the x-a472 transcript resolver (content-aware across all project dirs),
    so a jsonl age reflects the LIVE transcript, not a stale stub. For opencode
    the store is shared, so a per-file mtime is meaningless; the age comes from
    the session's newest message timestamp instead (the store already indexes
    ``(session_id, time_created)``), so an opencode session can go ``stalled``
    like any other. None only when nothing resolves."""
    try:
        if agent in {"claude", "codex"} and transcript_path is not None:
            mtime = transcript_path.stat().st_mtime
            basis = "mtime"
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
                return None, None, None
            if rt.kind == "opencode-db":
                activity_mtime = _opencode_activity_epoch(
                    session_id, Path(rt.transcript_path)
                )
                if activity_mtime is None:
                    return None, None, None
                mtime = activity_mtime
                basis = "opencode-db"
            else:
                mtime = Path(rt.transcript_path).stat().st_mtime
                basis = "mtime"
    except Exception:  # noqa: BLE001 — any read failure -> age unknown (working)
        return None, None, None
    now = now_s if now_s is not None else time.time()
    # An epoch datetime cannot represent (a corrupt opencode time_updated, say)
    # degrades the WHOLE triple, not just the stamp: `max(0.0, now - mtime)` on a
    # far-future epoch would claim a measured age of 0 beside a null stamp,
    # the fresh-vs-absent disagreement this paired return exists to prevent.
    try:
        datetime.fromtimestamp(mtime, tz=timezone.utc)
    except (ValueError, OverflowError, OSError):
        return None, None, None
    return mtime, max(0.0, now - mtime), basis


def _record_stamp_epoch(ts: str) -> Optional[float]:
    """ISO transcript stamp -> epoch seconds, or None.

    Mirrors the positive control's parser (``watchdog._record_epoch``) so the
    two readers cannot disagree about one stamp."""
    try:
        return datetime.fromisoformat(str(ts).replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


#: Tail depth for :func:`newest_entry_epoch`, same bound as the watchdog's
#: tick read: the newest entry is always near the end.
_ENTRY_TAIL_BYTES = 256 * 1024


def newest_entry_epoch(path: Path, tail_bytes: Optional[int] = _ENTRY_TAIL_BYTES) -> Optional[float]:
    """Newest top-level ``timestamp`` in a jsonl transcript, in epoch SECONDS.

    The transcript-age primitive the file stat must not be: trailing records
    (last-prompt, cost state) are appended with NO timestamp field, so mtime
    keeps the file young while the conversation is silent - measured never
    negative, median +20 minutes, maximum +240 hours over 311 claude
    transcripts (x-54cf). Reads the same bounded tail
    ``watchdog.tail_entries`` reads and takes the newest parseable stamp.
    ``tail_bytes=None`` reads the WHOLE file: the adopt stamp passes it so its
    window can never be narrower than the tail truth's own read covers, which
    would split one transcript across two instruments. None when the file is
    unreadable or carries no timestamped entry at all; the caller then falls
    back (and names the fallback via ``last_activity_basis``)."""
    try:
        size = path.stat().st_size
        with path.open("rb") as fh:
            if tail_bytes is not None:
                fh.seek(max(0, size - tail_bytes))
            chunk = fh.read()
        lines = chunk.decode("utf-8").splitlines()
    except (OSError, UnicodeDecodeError):
        return None
    if tail_bytes is not None and size > tail_bytes and lines:
        lines = lines[1:]  # a mid-file seek lands inside a line; drop it
    newest: Optional[float] = None
    for line in lines:
        try:
            ts = json.loads(line).get("timestamp")
        except Exception:  # noqa: BLE001 - a torn/foreign line is not data
            continue
        if not ts:
            continue
        epoch = _record_stamp_epoch(ts)
        if epoch is not None and (newest is None or epoch > newest):
            newest = epoch
    return newest


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


def observed_title(agent: str, transcript_path: Optional[Path]) -> Optional[str]:
    """The title the HARNESS carries for this session, or ``None``.
    claude only: the last ``{"type":"agent-name",...}`` transcript record
    (a Ctrl+R rename) IS the current title. ``None`` renders as absence.
    """
    if agent != "claude" or transcript_path is None:
        return None
    try:
        title: Optional[str] = None
        with open(transcript_path, "r", encoding="utf-8", errors="replace") as fh:
            # agent-name records are rare: pre-filter so json.loads is rare.
            for line in fh:
                if '"agent-name"' not in line:
                    continue
                try:
                    rec = json.loads(line)
                except ValueError:
                    continue
                if (
                    isinstance(rec, dict)
                    and rec.get("type") == "agent-name"
                    and isinstance(rec.get("agentName"), str)
                    and rec["agentName"].strip()
                ):
                    title = rec["agentName"]
        return title
    except OSError:
        return None


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

    Returns ``{handle, state, reason, last_activity_age_s, last_event_at,
    last_activity_basis, last_message, session_id, observed_model,
    harness_title, suggestions}``.
    ``state`` is one of done | watching | your-move | working | stalled |
    unknown; ``reason`` is set only for ``unknown`` (``not-found``/``no-records``);
    ``last_event_at`` is the absolute ISO8601 UTC stamp of the newest transcript
    activity and ``last_message`` the flattened text of the LAST turn (compact
    ``[tool_use: name]`` markers included, whitespace collapsed, capped at 200
    chars) - both None on every unknown path, because an unread transcript must
    render as unread, never as fresh; ``last_activity_basis`` names the
    instrument the age came from (``last-entry`` | ``mtime`` | ``opencode-db``);
    ``observed_model`` is the five-variant
    reading documented on :func:`observed_model` and is present on every path,
    including the ``unknown`` ones (a row that cannot be classified still
    renders)."""
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
            "last_event_at": None,
            "last_activity_basis": None,
            "last_message": None,
            "session_id": session_id,
            "observed_model": observed or {"kind": "no-transcript"},
            "harness_title": None,
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

    # The age comes from the tail ALREADY read above: its newest parseable
    # Record.timestamp. Trailing untimestamped records (last-prompt, cost
    # state) keep touching the file without dating the conversation, so the
    # file stat reports the session as more alive than it is - measured never
    # negative, median +20 minutes, maximum +240 hours (x-54cf). The stat is
    # only the labelled fallback for a tail with no timestamped record at all;
    # whichever instrument answered is served as ``last_activity_basis``.
    epoch: Optional[float] = None
    basis: Optional[str] = None
    if agent in {"claude", "codex"}:
        for rec in reversed(records):
            if not rec.timestamp:
                continue
            stamp_epoch = _record_stamp_epoch(rec.timestamp)
            if stamp_epoch is None:
                continue
            # The same representability guard _transcript_age_s applies: an
            # epoch datetime cannot render degrades the WHOLE pair, so a stamp
            # that cannot produce both an age and a stamp never becomes the
            # epoch - the reading falls to the labelled fallback as one piece.
            try:
                datetime.fromtimestamp(stamp_epoch, tz=timezone.utc)
            except (ValueError, OverflowError, OSError):
                continue
            epoch = stamp_epoch
            basis = "last-entry"
            break
    if epoch is None:
        epoch, age, basis = _transcript_age_s(
            agent,
            sid,
            cwd,
            projects_root,
            codex_sessions_dir,
            now_s,
            transcript_path,
        )
    else:
        now = now_s if now_s is not None else time.time()
        age = max(0.0, now - epoch)

    # Classify the LAST turn, not the last assistant turn: a trailing user turn
    # must clear a stale assistant promise/question (see classify_tail).
    # Peer mail turns (role == "peer") do not clear operator/assistant state.
    last = records[-1]
    last_actor = next((r for r in reversed(records) if r.role != "peer"), last)
    state = classify_tail(last_actor.role, last_actor.text, age, stalled_after_s=stalled_after_s)
    try:
        last_event_at = (
            None
            if epoch is None
            else datetime.fromtimestamp(epoch, tz=timezone.utc).strftime(
                "%Y-%m-%dT%H:%M:%SZ"
            )
        )
    except (ValueError, OverflowError, OSError):
        # An epoch beyond datetime range (e.g. an opencode time_updated stored
        # in microseconds) degrades to an absent stamp; raising here would
        # break the never-raises contract and take the whole list render down
        # with one corrupt reading.
        last_event_at = None
    # The stamp and the age describe the same tail: both derive from the one
    # epoch above, so the pair cannot disagree about when the transcript last
    # moved. ``last_activity_basis`` names the instrument that epoch came from,
    # so a supervisor reading a stale age can see which reader answered.
    return {
        "handle": handle,
        "state": state,
        "reason": None,
        "last_activity_age_s": None if age is None else int(age),
        "last_event_at": last_event_at,
        "last_activity_basis": basis,
        "last_message": " ".join((last.text or "").split())[:200] or None,
        "session_id": sid,
        "observed_model": observed,
        "harness_title": observed_title(agent, transcript_path),
        "suggestions": [],
    }


def _default_resolve(handle: str):
    from fno.agents.discover import (
        DiscoveredSession,
        ReachableSession,
        StoreReadError,
        resolve_or_suggest,
        resolve_reachable,
    )

    session: Optional[DiscoveredSession | ReachableSession] = None
    session, suggestions = resolve_or_suggest(handle, require_alive=False)
    if session is not None:
        return session, suggestions
    # The listing discovery serves is liveness-gated, so a handle whose only
    # record is an aged transcript falls through it and truth would read
    # "unknown" for a session a wake can still reach. The durable-store rung
    # the send ladder uses is the reach below discovery; an ambiguous token
    # still resolves to nothing, and an unproven-unique one only when the
    # error carries its lone candidate.
    try:
        session, _ = resolve_reachable(handle)
    except StoreReadError as exc:
        session = exc.resolved
    return session, suggestions


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
        # Capped at 999d (review finding: an uncapped day count breaks the
        # fixed-width-4 invariant once a row is silent for 1000+ days).
        unit, n = "d", min(seconds // 86400, 999)
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
    basis = result.get("last_activity_basis")
    suffix = f" by {basis}" if basis else ""
    return (
        f"truth {handle}: {state}{model} "
        f"({_EVIDENCE.get(state, '')}, last activity {age} ago{suffix})"
    )
