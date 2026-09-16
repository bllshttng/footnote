"""``fno inbox user`` (old spelling ``fno inbox operator`` still works) - the user conversation queue.

A king records from the direction it is pushed: worker mail arrives as a
discrete event with an id and a queue, and it gets recorded, while operator
conversation is a stream with no event boundary and no receipt, so it does
not. This sub-app gives the operator turn the shape mail already has: an id,
a queue, and an ack, with no capture-time write path and no hook.

The queue is derived from the whole transcript by the Rust reader
(``fno-agents compaction operator-turns``): undispositioned turns are the
prose user turns minus the ids already acked. A per-session scan cursor
(``<session>.scan.json``, a deletable cache) is that reader's cache, so
later reads parse only newly appended bytes and no turn is lost to
distance from EOF. Recording still runs through the existing capture verbs
first; ``ack`` names what the turn produced - one ack verb where a
``--from-turn`` flag would have been threaded through three surfaces.

Session/transcript/ledger resolution (in order: explicit env pins, then
the ambient identity): ``FNO_OPERATOR_SESSION_ID``,
``FNO_OPERATOR_HARNESS``, ``FNO_OPERATOR_TRANSCRIPT``,
``FNO_OPERATOR_CAPTURE_DIR``. The pins are the tools/tests/hook seam - a
Stop hook runs outside the harness process, so it pins nothing and lets
the ambient identity resolve.

Machine envelopes and markers (task-notification, teammate delivery,
interrupt markers, bash echo, compaction preamble) are refused and counted
by name inside the Rust reader; every read names what it skipped, because
a filter that drops a real operator turn silently is worse than the noise
it removes.

Known hole, named on purpose: ``fno agents mail send --raw`` strips the
envelope, so raw mail still reads as operator here. For that residual,
over-counting is the safe direction: a false queue entry costs one ack, a
missed operator turn costs the failure this queue exists to close.
"""

from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from pathlib import Path

from typing import Optional

import typer

#: Ack outcomes: ``nothing``, or ``<kind>:<ref>`` naming what the turn made.
_ACK_KINDS = ("law", "capture", "node")


class OperatorCaptureError(Exception):
    """A resolution or validation refusal, surfaced as a non-zero exit."""


operator_app = typer.Typer(
    name="user",
    help="Queue of this session's undispositioned user turns, derived "
    "from the transcript by the Rust reader and acked to a per-session "
    "ledger under ~/.fno/operator-capture/ (the <session>.scan.json scan "
    "cursor is a cache; deleting it only forces a full rescan). Machine "
    "envelopes are refused and counted by name in that reader. Residual "
    "hole: raw mail (send --raw) reads as a user turn; over-counting is "
    "the safe direction there.",
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


def _read_queue(sid: str, path: Path) -> dict:
    """The pending turns and depth; the Rust reader owns the scan, the cursor and classify."""
    from fno.rust_binary import call_binary_json

    err, payload = call_binary_json(
        "compaction",
        [
            "operator-turns",
            "--session", sid,
            "--transcript", str(path),
            "--capture-dir", str(_capture_dir()),
        ],
    )
    if err is not None or not isinstance(payload, dict):
        typer.echo(f"error: operator turn reader failed: {err or 'no JSON object'}", err=True)
        raise typer.Exit(code=1)
    if payload.get("cursor_error"):
        typer.echo(f"warning: scan cursor not saved: {payload['cursor_error']}", err=True)
    return payload


def format_skip_report(skipped: dict[str, int]) -> str:
    """One stderr line naming every refused shape and count; empty when none."""
    if not skipped:
        return ""
    total = sum(skipped.values())
    detail = ", ".join(f"{reason}={n}" for reason, n in sorted(skipped.items()))
    return f"skipped {total} machine turn(s): {detail}"


def _ledger_path(session_id: str) -> Path:
    return _capture_dir() / f"{session_id}.jsonl"


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
    payload = _read_queue(sid, path)
    report = format_skip_report(payload.get("skipped") or {})
    if report:
        typer.echo(report, err=True)
    pending = payload.get("turns") or []
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
        age = f"{int(now - t['ts_epoch'])}s" if t.get("ts_epoch") is not None else "age-unknown"
        marker = "[stand-down] " if t.get("stand_down") else ""
        typer.echo(f"{t['turn_id']}\t{age}\t{marker}{t['excerpt']}")


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
    depth = _read_queue(sid, path)
    depth.pop("turns", None)
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
