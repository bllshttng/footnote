"""``fno inbox user`` (old spelling ``fno inbox operator`` still works) - the user conversation queue.

A king records from the direction it is pushed: worker mail arrives as a
discrete event with an id and a queue, so it gets recorded, while operator
conversation is a stream with no event boundary and no receipt, so it does
not. This sub-app gives the operator turn the shape mail already has: an id,
a queue, and an ack, with no capture-time write path and no hook.

The transcript is already the event log, so the queue is DERIVED: the
undispositioned turns are the prose user turns minus the ids already acked,
which works retroactively on something said an hour ago. Recording still
runs through the existing capture verbs first; ``ack`` then names what the
turn produced - one ack verb instead of a ``--from-turn`` flag threaded
through three surfaces.

Session/transcript/ledger resolution (in order: explicit env pins, then the
ambient identity): ``FNO_OPERATOR_SESSION_ID``, ``FNO_OPERATOR_HARNESS``,
``FNO_OPERATOR_TRANSCRIPT``, ``FNO_OPERATOR_CAPTURE_DIR``. The pins are the
tools/tests/hook seam - a Stop hook runs outside the harness process, so it
pins nothing and lets the ambient identity resolve.

Known hole, named on purpose: ``fno agents mail send --raw`` strips the
envelope, so raw mail reads as operator here. Over-counting is the safe
direction: a false queue entry costs one ack, a missed operator turn costs
the failure this queue exists to close.
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

#: Ack outcomes: ``nothing``, or ``<kind>:<ref>`` naming what the turn made.
_ACK_KINDS = ("law", "capture", "node")

_EXCERPT_CHARS = 160

_ARG_TOKEN_RE = re.compile(r"[a-zA-Z0-9._/:@%+=~-]+")
_SENTENCE_TAILS = (".", "?", "!", ";", ",")
_SYSTEM_REMINDER_RE = re.compile(r"<system-reminder>.*?</system-reminder>", re.DOTALL)
_SYNTHETIC_PREFIXES = (
    "<command-name>",
    "<local-command",
    "<user_instructions>",
    "<environment_context>",
)


class OperatorCaptureError(Exception):
    """A resolution or validation refusal, surfaced as a non-zero exit."""


operator_app = typer.Typer(
    name="user",
    help="Queue of this session's undispositioned user turns, derived "
    "from the transcript and acked to a per-session ledger under "
    "~/.fno/operator-capture/. Hole: raw mail (send --raw) reads as "
    "a user turn; over-counting is the safe direction.",
    no_args_is_help=True,
)


def _capture_dir() -> Path:
    override = os.environ.get("FNO_OPERATOR_CAPTURE_DIR")
    if override:
        return Path(override)
    from fno.paths import state_dir

    return state_dir() / "operator-capture"


def _resolve_session(require_transcript: bool = True) -> tuple[str, Optional[Path]]:
    """``(session_id, transcript_path)``, named on failure, never an empty queue.

    The ambient identity is not crown-gated: the queue depth is state the
    code derives for itself. ``require_transcript=False`` (the ack ledger)
    needs only the session id - the ledger outlives transcripts.
    """
    sid = os.environ.get("FNO_OPERATOR_SESSION_ID", "").strip()
    harness = os.environ.get("FNO_OPERATOR_HARNESS") or "claude"
    transcript: Optional[Path] = (
        Path(os.environ["FNO_OPERATOR_TRANSCRIPT"])
        if os.environ.get("FNO_OPERATOR_TRANSCRIPT")
        else None
    )
    if not sid:
        from fno.claims.self_identity import resolve_self_identity

        ident = resolve_self_identity()
        sid = (ident.session_id or "").strip()
        if not sid:
            raise OperatorCaptureError(
                "no resolvable session identity (FNO_OPERATOR_SESSION_ID or "
                "a harness session)"
            )
        harness = ident.harness or harness
    if not require_transcript:
        return sid, None
    if transcript is None:
        from fno.provenance.observed import resolve_transcript_path

        transcript = resolve_transcript_path(harness, sid, os.getcwd())
    if transcript is None or not transcript.is_file():
        raise OperatorCaptureError(
            f"no readable transcript for session {sid} ({harness}); "
            f"resolved to {transcript or 'nothing'} - set FNO_OPERATOR_TRANSCRIPT"
        )
    return sid, transcript


def _is_user_turn(obj: dict) -> bool:
    """True for a row the transcript writes when a user (or mail) speaks."""
    if obj.get("type") == "user":
        return not obj.get("isMeta")
    payload = obj.get("payload")
    return isinstance(payload, dict) and payload.get("type") == "message" and payload.get("role") == "user"


def _turn_text(obj: dict) -> str:
    """The user-visible text of a row, ``""`` when it has none.

    Both content shapes (string, block list) across the claude and codex row
    formats; tool-result and hook blocks carry no text, so a turn made only
    of those reads empty.
    """
    msg = obj.get("message")
    content = msg.get("content") if isinstance(msg, dict) else obj.get("content")
    payload = obj.get("payload")
    if isinstance(payload, dict):
        content = payload.get("content")
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    return " ".join(
        b["text"] for b in content if isinstance(b, dict) and isinstance(b.get("text"), str)
    )


def _turn_id(obj: dict, text: str, line_no: int) -> str:
    """A stable ledger id: the transcript's own uuid/id, else a digest.

    The digest folds the row's line number in, so byte-identical duplicate
    rows still get distinct ids and one ack can never dispose two turns.
    """
    for key in ("uuid", "id"):
        val = obj.get(key)
        if isinstance(val, str) and val.strip():
            return val.strip()
    seed = f"{line_no}:{obj.get('timestamp')}:{text}"
    return f"derived-{hashlib.sha1(seed.encode()).hexdigest()[:12]}"


def _turn_ts_epoch(obj: dict) -> Optional[float]:
    ts = obj.get("timestamp") or obj.get("ts")
    if not isinstance(ts, str) or not ts.strip():
        return None
    try:
        parsed = datetime.fromisoformat(ts.strip().replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        # Transcripts are UTC by convention; a naive stamp must not read as
        # local time or the age skews by the machine's offset.
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.timestamp()


def _is_bare_command(text: str) -> bool:
    """A single-line slash command or ``$fno:`` verb with flag-shaped args only.

    A token ending in sentence punctuation means the turn carries prose, and
    prose may carry a ruling. A filename dot is fine (the safe direction is
    over-counting); ``x-1.`` is not.
    """
    if "\n" in text or not (text.startswith("/") or text.startswith("$fno:")):
        return False
    return all(
        _ARG_TOKEN_RE.fullmatch(t) and not t.endswith(_SENTENCE_TAILS)
        for t in text.split()[1:]
    )


def classify(text: str) -> Optional[str]:
    """The operator-shaped text of a turn, or ``None`` when it is not one.

    In order, failing toward the queue: injected mail never queues; a bare
    command invocation carries no ruling; a turn with no user text outside
    system-reminder/hook content is not a turn; everything else queues.
    """
    from fno.mail.envelope import contains_fno_mail_tag

    if contains_fno_mail_tag(text):
        return None
    cleaned = _SYSTEM_REMINDER_RE.sub("", text.strip()).strip()
    if not cleaned or cleaned.startswith(_SYNTHETIC_PREFIXES):
        return None
    if _is_bare_command(cleaned):
        return None
    return cleaned


#: The retroactivity window. The read is tail-bounded so a multi-MB
#: transcript costs fixed bytes per hook fire; past the window a turn can
#: only be surfaced by acking nothing and re-reading, which no caller does.
_TAIL_BYTES = 2_000_000


def read_operator_turns(transcript_path: Path) -> list[dict]:
    """Operator turns, oldest first, as ``{turn_id, ts_epoch, text}``."""
    try:
        with transcript_path.open("rb") as fh:
            fh.seek(0, 2)
            size = fh.tell()
            fh.seek(max(0, size - _TAIL_BYTES))
            raw = fh.read().decode("utf-8", errors="replace")
    except OSError as exc:
        raise OperatorCaptureError(f"transcript {transcript_path} unreadable: {exc}") from exc
    if size > _TAIL_BYTES:
        # Drop the torn line the seek landed inside.
        newline = raw.find("\n")
        raw = raw[newline + 1 :] if newline >= 0 else ""
    turns: list[dict] = []
    for line_no, line in enumerate(raw.splitlines()):
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if not isinstance(obj, dict) or not _is_user_turn(obj):
            continue
        text = classify(_turn_text(obj))
        if text is not None:
            turns.append(
                {
                    "turn_id": _turn_id(obj, text, line_no),
                    "ts_epoch": _turn_ts_epoch(obj),
                    "text": text,
                }
            )
    return turns


def _ledger_path(session_id: str) -> Path:
    return _capture_dir() / f"{session_id}.jsonl"


def read_acked_turn_ids(session_id: str) -> set[str]:
    try:
        raw = _ledger_path(session_id).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return set()
    acked: set[str] = set()
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
    kind, _, ref = (outcome or "").strip().partition(":")
    kind, ref = kind.strip(), ref.strip()
    if kind == "nothing" and not ref:
        outcome = "nothing"
    elif not (kind in _ACK_KINDS and ref):
        legal = ", ".join(f"{k}:<ref>" for k in _ACK_KINDS)
        raise OperatorCaptureError(f"invalid --outcome {outcome!r}. Must be nothing or {legal}")
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


def excerpt(text: str, limit: int = _EXCERPT_CHARS) -> str:
    """One-line excerpt; newlines collapse so a row stays one row."""
    flat = " ".join(text.split())
    return flat if len(flat) <= limit else flat[: limit - 1] + "\N{HORIZONTAL ELLIPSIS}"


def queue_depth(session_id: str, transcript_path: Path) -> dict:
    acked = read_acked_turn_ids(session_id)
    pending = [t for t in read_operator_turns(transcript_path) if t["turn_id"] not in acked]
    oldest = pending[0] if pending else None
    age = None
    if oldest is not None and oldest["ts_epoch"] is not None:
        age = max(0, int(datetime.now(timezone.utc).timestamp() - oldest["ts_epoch"]))
    return {
        "depth": len(pending),
        "oldest_age_s": age,
        "oldest_excerpt": excerpt(oldest["text"]) if oldest else None,
        "oldest_turn_id": oldest["turn_id"] if oldest else None,
    }


def _resolve_or_fail(require_transcript: bool = True) -> tuple[str, Optional[Path]]:
    try:
        return _resolve_session(require_transcript)
    except OperatorCaptureError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)


def _resolve_with_transcript() -> tuple[str, Path]:
    sid, path = _resolve_or_fail(require_transcript=True)
    assert path is not None  # require_transcript=True guarantees it
    return sid, path


@operator_app.command("list")
def cmd_list(
    limit: int = typer.Option(None, "--limit", "-L", min=1, help="Max turns to show."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit a JSON array."),
) -> None:
    """Undispositioned user turns, oldest first."""
    sid, path = _resolve_with_transcript()
    acked = read_acked_turn_ids(sid)
    pending = [t for t in read_operator_turns(path) if t["turn_id"] not in acked]
    if limit is not None:
        pending = pending[:limit]
    if json_output:
        typer.echo(json.dumps(pending, indent=2))
        return
    if not pending:
        typer.echo("no undispositioned user turns")
        return
    now = datetime.now(timezone.utc).timestamp()
    for t in pending:
        age = f"{int(now - t['ts_epoch'])}s" if t["ts_epoch"] is not None else "age-unknown"
        typer.echo(f"{t['turn_id']}\t{age}\t{excerpt(t['text'])}")


@operator_app.command("ack")
def cmd_ack(
    turn_id: str = typer.Argument(..., help="The user turn id to dispose."),
    outcome: str = typer.Option(
        ...,
        "--outcome",
        help="nothing | law:<decision-id> | capture:<fu-id> | node:<node-id>",
    ),
    why: str = typer.Option(None, "--why", help="One-line reason, kept in the ledger."),
) -> None:
    """Dispose one user turn, naming what it produced."""
    sid, _ = _resolve_or_fail(require_transcript=False)
    try:
        row = ack_turn(sid, turn_id, outcome, why or "")
    except OperatorCaptureError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)
    typer.echo(json.dumps(row))


@operator_app.command("status")
def cmd_status(
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit the depth payload."),
) -> None:
    """Queue depth for this session - the number the capture hook reads."""
    sid, path = _resolve_with_transcript()
    depth = queue_depth(sid, path)
    if json_output:
        typer.echo(json.dumps(depth, indent=2))
        return
    if depth["depth"] == 0:
        typer.echo("user queue: 0")
        return
    age = depth["oldest_age_s"]
    age_text = f", oldest {age}s old" if age is not None else ""
    typer.echo(f"user queue: {depth['depth']} undispositioned turn(s){age_text}")
    if depth["oldest_excerpt"]:
        typer.echo(f"  oldest: {depth['oldest_excerpt']}")
