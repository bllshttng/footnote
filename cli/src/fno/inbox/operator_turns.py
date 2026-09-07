"""``fno inbox operator`` - the operator conversation queue.

A king records from the direction it is pushed: worker mail arrives as a
discrete event with an id and a queue, so it gets recorded, while operator
conversation is a stream with no event boundary and no receipt, so it does
not. This sub-app gives the operator turn the same shape mail already has -
an id, a queue, and an ack - without a capture-time write path or a hook.

The session transcript is already the event log. Every operator message is a
user turn with an id and a timestamp, so the queue is DERIVED rather than
stored: the undispositioned turns are the user turns in this session's
transcript minus the turn ids already acked. It works retroactively on
something said an hour ago.

Recording still goes through the existing capture verbs first (``fno inbox
law set``, ``fno backlog capture add``, ``fno backlog idea --source-kind
operator_request``); ``ack`` then names what the turn produced. One ack verb
is fewer moving parts than a ``--from-turn`` flag threaded through three
surfaces.

Known hole, named in ``--help`` on purpose: ``fno agents mail send --raw``
strips the mail envelope, so raw mail reads as operator here. Over-counting
is the safe direction - a false queue entry costs one ack, a missed operator
turn costs the failure this queue exists to close.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import typer

#: Ack outcomes. ``nothing`` disposes a turn that needed no artifact; every
#: other outcome is ``<kind>:<ref>`` naming what the turn produced.
_ACK_KINDS = ("law", "capture", "node")

#: Rendered excerpt length for human output and ``status``.
_EXCERPT_CHARS = 160

operator_app = typer.Typer(
    name="operator",
    help="Queue of this session's undispositioned operator turns. "
    "Derived from the transcript, acked to a per-session ledger. Hole: "
    "`fno agents mail send --raw` strips the envelope, so raw mail reads "
    "as operator here (over-counting is the safe direction).",
    no_args_is_help=True,
)


# ---------------------------------------------------------------------------
# Resolution: which session, which transcript, which ledger
# ---------------------------------------------------------------------------


def _capture_dir() -> Path:
    """The operator-capture ledger root.

    ``FNO_OPERATOR_CAPTURE_DIR`` wins (tests, tools); else ``$FNO_HOME`` /
    ``~/.fno``, matching the session-start hook's resolution.
    """
    override = os.environ.get("FNO_OPERATOR_CAPTURE_DIR")
    if override:
        return Path(override)
    home = os.environ.get("FNO_HOME")
    base = Path(home).expanduser() if home else Path.home() / ".fno"
    return base / "operator-capture"


def _resolve_session(
    session_id: Optional[str],
    harness: str,
    transcript: Optional[Path],
    *,
    require_transcript: bool = True,
) -> tuple[str, str, Optional[Path]]:
    """``(session_id, harness, transcript_path)`` for this run.

    Explicit overrides win; otherwise the session comes from the ambient
    identity (no crown gate - the derived queue depth is state the code reads
    for itself) and the transcript from the harness resolver. Both failures
    are named, never read as an empty queue. ``require_transcript=False``
    (the ack ledger) needs only the session id.
    """
    sid = (session_id or "").strip()
    if not sid:
        from fno.claims.self_identity import resolve_self_identity

        ident = resolve_self_identity()
        sid = (ident.session_id or "").strip()
        if not sid:
            raise OperatorCaptureError(
                "no resolvable session identity: pass --session-id, or run "
                "inside a harness session"
            )
        harness = ident.harness or harness
    if not require_transcript:
        return sid, harness, None
    if transcript is not None:
        if not transcript.is_file():
            raise OperatorCaptureError(
                f"no readable transcript for session {sid} ({harness}); "
                f"resolved to {transcript} - pass --transcript to name it"
            )
        return sid, harness, transcript
    from fno.provenance.observed import resolve_transcript_path

    path = resolve_transcript_path(harness, sid, os.getcwd())
    if path is None or not path.is_file():
        raise OperatorCaptureError(
            f"no readable transcript for session {sid} ({harness}); "
            f"resolved to {path or 'nothing'} - pass --transcript to name it"
        )
    return sid, harness, path


class OperatorCaptureError(Exception):
    """A resolution or validation refusal, surfaced as a non-zero exit."""


# ---------------------------------------------------------------------------
# The transcript reader and the classifier
# ---------------------------------------------------------------------------

_SYSTEM_REMINDER_RE = re.compile(r"<system-reminder>.*?</system-reminder>", re.DOTALL)
_SYNTHETIC_PREFIXES = (
    "<command-name>",
    "<local-command",
    "<user_instructions>",
    "<environment_context>",
)


def _turn_text(obj: dict) -> str:
    """The user-visible text of a transcript row, ``""`` when it has none.

    Handles both content shapes (plain string, block list) across the claude
    and codex row formats. Tool-result and hook blocks carry no text, so a
    turn made only of those reads empty - which the classifier then refuses.
    """
    msg = obj.get("message")
    content = msg.get("content") if isinstance(msg, dict) else obj.get("content")
    payload = obj.get("payload") if isinstance(obj.get("payload"), dict) else None
    if payload is not None:
        content = payload.get("content")
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts = []
    for block in content:
        if isinstance(block, dict) and isinstance(block.get("text"), str):
            parts.append(block["text"])
    return " ".join(parts)


def _is_user_turn(obj: dict) -> bool:
    """True for a row the transcript writes when a user (or mail) speaks."""
    if obj.get("type") == "user":
        return not obj.get("isMeta")
    payload = obj.get("payload")
    if isinstance(payload, dict):
        # Codex rollout rows carry the message one level down.
        return payload.get("type") == "message" and payload.get("role") == "user"
    return False


def _turn_id(obj: dict, text: str) -> str:
    """A stable id for the ack ledger: the transcript's own when it has one."""
    for key in ("uuid", "id"):
        val = obj.get(key)
        if isinstance(val, str) and val.strip():
            return val.strip()
    digest = hashlib.sha1(f"{obj.get('timestamp')}:{text}".encode()).hexdigest()[:12]
    return f"derived-{digest}"


def _turn_ts_epoch(obj: dict) -> Optional[float]:
    ts = obj.get("timestamp") or obj.get("ts")
    if not isinstance(ts, str) or not ts.strip():
        return None
    try:
        return datetime.fromisoformat(ts.strip().replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


_ARG_TOKEN_RE = re.compile(r"[a-zA-Z0-9._/:@%+=~-]+")
_SENTENCE_TAILS = (".", "?", "!", ";", ",")


def _is_bare_command(text: str) -> bool:
    """True for a single-line slash command or ``$fno:`` verb invocation.

    The tail after the command token must be only flag/argument tokens: as
    soon as a token ends in sentence punctuation, the turn carries prose, and
    prose may carry a ruling. A filename dot is fine (over-counting toward
    the queue is the safe direction); ``x-1.`` is not.
    """
    if "\n" in text or not (text.startswith("/") or text.startswith("$fno:")):
        return False
    tokens = text.split()
    return all(
        _ARG_TOKEN_RE.fullmatch(t) and not t.endswith(_SENTENCE_TAILS)
        for t in tokens[1:]
    )


def classify(text: str) -> Optional[str]:
    """The operator-shaped text of a turn, or ``None`` when it is not one.

    In order, failing toward the queue: injected mail never queues; a bare
    slash command or ``$fno:`` verb with no following prose carries no
    ruling; a turn with no user text outside hook/system-reminder content is
    not a turn; everything else enters the queue.
    """
    from fno.mail.envelope import contains_fno_mail_tag

    if contains_fno_mail_tag(text):
        return None
    stripped = text.strip()
    if not stripped:
        return None
    if stripped.startswith(("<command-name>", "<local-command")):
        return None
    cleaned = _SYSTEM_REMINDER_RE.sub("", stripped).strip()
    if not cleaned:
        return None
    if cleaned.startswith(_SYNTHETIC_PREFIXES):
        return None
    if _is_bare_command(cleaned):
        return None
    return cleaned


def read_operator_turns(transcript_path: Path) -> list[dict]:
    """Undispositioned-candidate operator turns, oldest first.

    Each row is ``{"turn_id", "ts_epoch", "text"}``; ``ts_epoch`` is ``None``
    when the row carries no parseable timestamp, and the caller treats an
    unknown age as unknown rather than inventing one.
    """
    turns: list[dict] = []
    try:
        raw = transcript_path.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        raise OperatorCaptureError(
            f"transcript {transcript_path} could not be read: {exc}"
        ) from exc
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if not isinstance(obj, dict) or not _is_user_turn(obj):
            continue
        text = classify(_turn_text(obj))
        if text is None:
            continue
        turns.append(
            {
                "turn_id": _turn_id(obj, text),
                "ts_epoch": _turn_ts_epoch(obj),
                "text": text,
            }
        )
    return turns


# ---------------------------------------------------------------------------
# The ack ledger
# ---------------------------------------------------------------------------


def _ledger_path(session_id: str) -> Path:
    return _capture_dir() / f"{session_id}.jsonl"


def read_acked_turn_ids(session_id: str) -> set[str]:
    """Turn ids this session already disposed, from the ledger file."""
    path = _ledger_path(session_id)
    if not path.is_file():
        return set()
    acked: set[str] = set()
    try:
        raw = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return acked
    for line in raw.splitlines():
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if isinstance(row, dict) and isinstance(row.get("turn_id"), str):
            acked.add(row["turn_id"])
    return acked


def ack_turn(session_id: str, turn_id: str, outcome: str, why: str) -> dict:
    """Append one ack row; the file is the receipt and the watermark at once."""
    outcome = (outcome or "").strip()
    kind, _, ref = outcome.partition(":")
    kind = kind.strip()
    ref = ref.strip()
    if kind == "nothing" and not ref:
        outcome = "nothing"
    elif kind in _ACK_KINDS and ref:
        outcome = f"{kind}:{ref}"
    else:
        legal = ", ".join(f"{k}:<ref>" for k in _ACK_KINDS)
        raise OperatorCaptureError(
            f"invalid --outcome {outcome!r}. Must be nothing or {legal}"
        )
    row = {
        "turn_id": turn_id,
        "ts": datetime.now(timezone.utc).isoformat(),
        "outcome": outcome,
        "ref": ref or None,
        "why": (why or "").strip() or None,
    }
    path = _ledger_path(session_id)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(row) + "\n")
    return row


# ---------------------------------------------------------------------------
# The queue projection
# ---------------------------------------------------------------------------


def queue_depth(session_id: str, transcript_path: Path) -> dict:
    """``{"depth", "oldest_age_s", "oldest_excerpt", "oldest_turn_id"}``."""
    acked = read_acked_turn_ids(session_id)
    pending = [t for t in read_operator_turns(transcript_path) if t["turn_id"] not in acked]
    now = datetime.now(timezone.utc).timestamp()
    oldest = pending[0] if pending else None
    age = None
    if oldest is not None and oldest["ts_epoch"] is not None:
        age = max(0, int(now - oldest["ts_epoch"]))
    return {
        "depth": len(pending),
        "oldest_age_s": age,
        "oldest_excerpt": excerpt(oldest["text"]) if oldest else None,
        "oldest_turn_id": oldest["turn_id"] if oldest else None,
    }


def excerpt(text: str, limit: int = _EXCERPT_CHARS) -> str:
    """One-line excerpt; newlines collapse so a row stays one row."""
    flat = " ".join(text.split())
    return flat if len(flat) <= limit else flat[: limit - 1] + "\N{HORIZONTAL ELLIPSIS}"


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

_SESSION_OVERRIDE_HELP = "Session override (diagnostics/tests; default: ambient identity)"
_HARNESS_OVERRIDE_HELP = "Harness for the transcript lookup (default: the resolved one)"


def _fail(message: str) -> None:
    typer.echo(f"error: {message}", err=True)
    raise typer.Exit(code=1)


def _context(
    session_id: Optional[str],
    harness: str,
    transcript: Optional[Path],
    *,
    require_transcript: bool = True,
) -> tuple[str, Optional[Path]]:
    try:
        sid, _, path = _resolve_session(
            session_id, harness, transcript, require_transcript=require_transcript
        )
    except OperatorCaptureError as exc:
        _fail(str(exc))
    return sid, path


@operator_app.command("list")
def cmd_list(
    limit: int = typer.Option(None, "--limit", "-L", min=1, help="Max turns to show."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit a JSON array."),
    session_id: Optional[str] = typer.Option(None, "--session-id", help=_SESSION_OVERRIDE_HELP),
    harness: str = typer.Option("claude", "--harness", help=_HARNESS_OVERRIDE_HELP),
    transcript: Optional[Path] = typer.Option(None, "--transcript", help="Transcript file override"),
) -> None:
    """Undispositioned operator turns, oldest first."""
    sid, path = _context(session_id, harness, transcript)
    acked = read_acked_turn_ids(sid)
    pending = [t for t in read_operator_turns(path) if t["turn_id"] not in acked]
    if limit is not None:
        pending = pending[:limit]
    if json_output:
        typer.echo(
            json.dumps(
                [
                    {"turn_id": t["turn_id"], "ts_epoch": t["ts_epoch"], "text": t["text"]}
                    for t in pending
                ],
                indent=2,
            )
        )
        return
    if not pending:
        typer.echo("no undispositioned operator turns")
        return
    now = datetime.now(timezone.utc).timestamp()
    for t in pending:
        age = f"{int(now - t['ts_epoch'])}s" if t["ts_epoch"] is not None else "age-unknown"
        typer.echo(f"{t['turn_id']}\t{age}\t{excerpt(t['text'])}")


@operator_app.command("ack")
def cmd_ack(
    turn_id: str = typer.Argument(..., help="The operator turn id to dispose."),
    outcome: str = typer.Option(
        ...,
        "--outcome",
        help="nothing | law:<decision-id> | capture:<fu-id> | node:<node-id>",
    ),
    why: str = typer.Option(None, "--why", help="One-line reason, kept in the ledger."),
    session_id: Optional[str] = typer.Option(None, "--session-id", help=_SESSION_OVERRIDE_HELP),
    harness: str = typer.Option("claude", "--harness", help=_HARNESS_OVERRIDE_HELP),
    transcript: Optional[Path] = typer.Option(None, "--transcript", help="Transcript file override"),
) -> None:
    """Dispose one operator turn, naming what it produced."""
    # An ack needs the session only: the ledger outlives transcripts, so a
    # rotated or compacted transcript must not block disposing a turn.
    sid, _ = _context(session_id, harness, transcript, require_transcript=False)
    try:
        row = ack_turn(sid, turn_id, outcome, why or "")
    except OperatorCaptureError as exc:
        _fail(str(exc))
    typer.echo(json.dumps(row))


@operator_app.command("status")
def cmd_status(
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit the depth payload."),
    session_id: Optional[str] = typer.Option(None, "--session-id", help=_SESSION_OVERRIDE_HELP),
    harness: str = typer.Option("claude", "--harness", help=_HARNESS_OVERRIDE_HELP),
    transcript: Optional[Path] = typer.Option(None, "--transcript", help="Transcript file override"),
) -> None:
    """Queue depth for this session - the number the capture hook reads."""
    sid, path = _context(session_id, harness, transcript)
    depth = queue_depth(sid, path)
    if json_output:
        typer.echo(json.dumps(depth, indent=2))
        return
    if depth["depth"] == 0:
        typer.echo("operator queue: 0")
        return
    age = depth["oldest_age_s"]
    age_text = f", oldest {age}s old" if age is not None else ""
    typer.echo(
        f"operator queue: {depth['depth']} undispositioned turn(s){age_text}"
    )
    if depth["oldest_excerpt"]:
        typer.echo(f"  oldest: {depth['oldest_excerpt']}")
