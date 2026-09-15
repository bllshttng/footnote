"""``fno inbox user`` (old spelling ``fno inbox operator`` still works) - the user conversation queue.

A king records from the direction it is pushed: worker mail arrives as a
discrete event with an id and a queue, so it gets recorded, while operator
conversation is a stream with no event boundary and no receipt, so it does
not. This sub-app gives the operator turn the shape mail already has: an id,
a queue, and an ack, with no capture-time write path and no hook.

The transcript is the event log and the system of record, so the queue is
DERIVED from it: the undispositioned turns are the prose user turns minus
the ids already acked, read from the whole transcript. A per-session scan
cursor (``<session>.scan.json``, a deletable cache) remembers how far a
read got, so later reads parse only newly appended bytes and no turn is
lost to distance from EOF. Recording still runs through the existing
capture verbs first; ``ack`` then names what the turn produced - one ack
verb instead of a ``--from-turn`` flag threaded through three surfaces.

Session/transcript/ledger resolution (in order: explicit env pins, then the
ambient identity): ``FNO_OPERATOR_SESSION_ID``, ``FNO_OPERATOR_HARNESS``,
``FNO_OPERATOR_TRANSCRIPT``, ``FNO_OPERATOR_CAPTURE_DIR``. The pins are the
tools/tests/hook seam - a Stop hook runs outside the harness process, so it
pins nothing and lets the ambient identity resolve.

Machine envelopes and markers (task-notification, teammate delivery,
interrupt markers, bash echo, compaction preamble) are refused at
``classify`` and counted; every read names what it skipped, because a filter
that drops a real operator turn silently is worse than the noise it removes.

Known hole, named on purpose: ``fno agents mail send --raw`` strips the
envelope, so raw mail still reads as operator here. For that residual,
over-counting is the safe direction: a false queue entry costs one ack, a
missed operator turn costs the failure this queue exists to close.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
from datetime import datetime, timezone
from pathlib import Path

from fno.config._dispatch_verbs import is_verb_seed
from typing import Optional

import typer

#: Ack outcomes: ``nothing``, or ``<kind>:<ref>`` naming what the turn made.
_ACK_KINDS = ("law", "capture", "node")

_EXCERPT_CHARS = 160

_ARG_TOKEN_RE = re.compile(r"[a-zA-Z0-9._/:@%+=~-]+")
_SENTENCE_TAILS = (".", "?", "!", ";", ",")
_SYSTEM_REMINDER_RE = re.compile(r"<system-reminder>.*?</system-reminder>", re.DOTALL)
#: Skip rules: ``(prefix, skip_reason)`` matched against the reminder-stripped
#: turn text. Every prefix is a harness-injected envelope or marker a person
#: cannot type; the reason names the shape in the visible skip report.
_SKIP_RULES: tuple[tuple[str, str], ...] = (
    ("<command-name>", "command_invocation"),
    ("<command-message>", "command_invocation"),
    ("<local-command", "command_invocation"),
    ("<user_instructions>", "synthetic"),
    ("<environment_context>", "synthetic"),
    ("<task-notification>", "task_notification"),
    ("<bash-input>", "bash_echo"),
    ("<bash-stdout>", "bash_echo"),
    ("[Request interrupted by user", "interrupt_marker"),
    ("This session is being continued from a previous conversation", "compaction_preamble"),
    ("Another Claude session sent a message:", "teammate_message"),
)


class OperatorCaptureError(Exception):
    """A resolution or validation refusal, surfaced as a non-zero exit."""


operator_app = typer.Typer(
    name="user",
    help="Queue of this session's undispositioned user turns, derived "
    "from the transcript and acked to a per-session ledger under "
    "~/.fno/operator-capture/ (the <session>.scan.json scan cursor is a "
    "cache; deleting it only forces a full rescan). Machine envelopes are "
    "refused and counted by name. Residual hole: raw mail (send --raw) "
    "reads as a user turn; over-counting is the safe direction there.",
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
    if "\n" in text or not is_verb_seed(text):
        return False
    return all(
        _ARG_TOKEN_RE.fullmatch(t) and not t.endswith(_SENTENCE_TAILS)
        for t in text.split()[1:]
    )


def classify(text: str) -> tuple[Optional[str], str]:
    """The operator-shaped text of a turn, or ``(None, reason)`` when it is not one.

    In order, failing toward the queue: injected mail never queues; a bare
    command invocation carries no ruling; a turn with no user text outside
    system-reminder/hook content is not a turn; a machine envelope or marker
    is refused with its named reason; everything else queues. The reason is
    ``""`` when the turn is kept, so the caller can count what it dropped.
    """
    from fno.mail.envelope import contains_fno_mail_tag

    if contains_fno_mail_tag(text):
        return None, "fno_mail"
    cleaned = _SYSTEM_REMINDER_RE.sub("", text.strip()).strip()
    if not cleaned:
        return None, "no_user_text"
    for prefix, reason in _SKIP_RULES:
        if cleaned.startswith(prefix):
            return None, reason
    if _is_bare_command(cleaned):
        return None, "bare_command"
    return cleaned, ""


def _scan_path(session_id: str) -> Path:
    return _capture_dir() / f"{session_id}.scan.json"


def _scan(fh, line_no: int) -> tuple[list[dict], dict[str, int], int, int]:
    """Turns and skip counts from fh to its last full row, plus (bytes, lines) consumed.

    A torn trailing row waits for its newline, so a read never parses a
    half-written row and derived turn ids stay stable across reads.
    """
    raw = fh.read()
    cut = raw.rfind(b"\n")
    if cut < 0:
        return [], {}, 0, 0
    kept, turns, skipped = raw[: cut + 1], [], {}
    for i, row in enumerate(kept.split(b"\n")[:-1]):
        try:
            obj = json.loads(row.decode("utf-8", errors="replace"))
        except ValueError:
            continue
        if not isinstance(obj, dict) or not _is_user_turn(obj):
            continue
        text, reason = classify(_turn_text(obj))
        if text is None:
            skipped[reason] = skipped.get(reason, 0) + 1
            continue
        turns.append({"turn_id": _turn_id(obj, text, line_no + i), "ts_epoch": _turn_ts_epoch(obj), "text": text})
    return turns, skipped, len(kept), kept.count(b"\n")


def _load_scan_state(session_id: str, transcript_path: Path) -> dict:
    """The saved cursor, else fresh: missing, invalid, alien, or past EOF all reset."""
    fresh = {"transcript": str(transcript_path), "offset": 0, "line_no": 0, "turns": [], "skipped": {}}
    try:
        saved = json.loads(_scan_path(session_id).read_text(encoding="utf-8"))
        if (
            isinstance(saved, dict)
            and saved.get("transcript") == str(transcript_path)
            and isinstance(saved.get("offset"), int)
            and 0 <= saved["offset"] <= transcript_path.stat().st_size
            and isinstance(saved.get("line_no"), int)
            and isinstance(saved.get("turns"), list)
            and isinstance(saved.get("skipped"), dict)
        ):
            saved["turns"] = [t for t in saved["turns"] if isinstance(t, dict) and isinstance(t.get("turn_id"), str)]
            return saved
    except (OSError, ValueError):
        pass
    return fresh


def _save_scan_state(session_id: str, state: dict) -> None:
    """Atomic best-effort save; failure costs speed on the next read, never correctness."""
    try:
        path = _scan_path(session_id)
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(state), encoding="utf-8")
        os.replace(tmp, path)
    except OSError:
        pass


def pending_turns(session_id: str, transcript_path: Path) -> tuple[list[dict], dict[str, int]]:
    """Unacked turns, oldest first, plus a per-reason skip tally.

    The transcript is the system of record; the scan file is a cache whose
    deletion only forces a full rescan. # ponytail: the first read is one
    full pass (1.07s at 220MB) with no mid-scan checkpoint - checkpoint
    every 64MB in the loop if a transcript ever nears the hook's 10s budget.
    """
    state = _load_scan_state(session_id, transcript_path)
    try:
        with transcript_path.open("rb") as fh:
            fh.seek(state["offset"])
            new_turns, new_skips, consumed, lines = _scan(fh, state["line_no"])
    except OSError as exc:
        raise OperatorCaptureError(f"transcript {transcript_path} unreadable: {exc}") from exc
    if consumed:
        known = {t["turn_id"] for t in state["turns"]}
        state["turns"] += [t for t in new_turns if t["turn_id"] not in known]
        for reason, n in new_skips.items():
            state["skipped"][reason] = state["skipped"].get(reason, 0) + n
        state["offset"] += consumed
        state["line_no"] += lines
    acked = read_acked_turn_ids(session_id)
    state["turns"] = [t for t in state["turns"] if t["turn_id"] not in acked]
    _save_scan_state(session_id, state)
    return state["turns"], dict(state["skipped"])


def format_skip_report(skipped: dict[str, int]) -> str:
    """One stderr line naming every refused shape and count; empty when none."""
    if not skipped:
        return ""
    total = sum(skipped.values())
    detail = ", ".join(f"{reason}={n}" for reason, n in sorted(skipped.items()))
    return f"skipped {total} machine turn(s): {detail}"


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
    pending, skipped = pending_turns(session_id, transcript_path)
    oldest = pending[0] if pending else None
    age = None
    if oldest is not None and oldest["ts_epoch"] is not None:
        age = max(0, int(datetime.now(timezone.utc).timestamp() - oldest["ts_epoch"]))
    return {
        "depth": len(pending),
        "oldest_age_s": age,
        "oldest_excerpt": excerpt(oldest["text"]) if oldest else None,
        "oldest_turn_id": oldest["turn_id"] if oldest else None,
        "skipped": skipped,
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
    pending, skipped = pending_turns(sid, path)
    report = format_skip_report(skipped)
    if report:
        typer.echo(report, err=True)
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
    report = format_skip_report(depth.get("skipped") or {})
    if report:
        typer.echo(report, err=True)
    if depth["depth"] == 0:
        typer.echo("user queue: 0")
        return
    age = depth["oldest_age_s"]
    age_text = f", oldest {age}s old" if age is not None else ""
    typer.echo(f"user queue: {depth['depth']} undispositioned turn(s){age_text}")
    if depth["oldest_excerpt"]:
        typer.echo(f"  oldest: {depth['oldest_excerpt']}")
