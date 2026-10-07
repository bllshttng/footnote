"""The ``fno agents mail backfill`` verb: outage-era native-transport mail
back into the store, with provenance.

While ``fno mail send`` was down, agent-to-agent messages went over the
harness's native cross-session transport (the ``SendMessage`` tool call in
the sender's transcript, the ``<cross-session-message>`` block in the
receiver's). Both sides are in the harness transcripts, so the traffic can
be joined and written back as durable audit-only rows
(``delivery=cross-session``, provenance in ``meta``) that never re-deliver.

Extracted-file shape follows ``hold_cli.py`` (file budget: ``mail/cli.py``
is shrink-only); the pure bodies it calls live behind the hidden
``fno-agents mail-backfill`` verb (the mail-receipt pattern: Rust owns the
archive id, the block parser, and the provenance gate; Python keeps the
transcript scan and the bus write).
"""

from __future__ import annotations

import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterator, Optional

import typer

# The mail-send outage window: the default backfill scope. Every message the
# scan joins between these bounds is traffic the store never saw. Overrides:
# --since / --until.
OUTAGE_SINCE = "2026-10-07T08:42:47Z"
OUTAGE_UNTIL = "2026-10-07T12:52:39Z"

_MAX_LINES_PER_FILE = 200_000


def _parse_ts(s: str) -> datetime:
    return datetime.fromisoformat(s.strip().replace("Z", "+00:00"))


def _iso(dt: datetime) -> str:
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _verb(argv: list[str], stdin_text: Optional[str] = None) -> str:
    """One mail-backfill read through the native door (the mail-receipt
    pattern: Python keeps transports, the body lives in Rust). A missing
    binary refuses rather than falling back to a second implementation."""
    from fno.rust_binary import VerbUnavailable, resolve_binary

    binary = resolve_binary()
    if binary is None:
        raise VerbUnavailable(
            "fno-agents binary not found; run `fno doctor update`"
        )
    proc = subprocess.run(
        [str(binary), "mail-backfill", *argv],
        input=stdin_text,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if proc.returncode != 0:
        raise VerbUnavailable(
            (proc.stderr or "fno-agents mail-backfill failed").strip()[:200]
        )
    return proc.stdout.rstrip("\n")


def _iter_rows(
    roots: list[Path], since: datetime, until: datetime
) -> Iterator[tuple[Path, dict]]:
    """Yield ``(path, row)`` for transcript rows inside the window.

    The mtime prefilter bounds the scan: a file whose last write predates
    the window start cannot hold a row from it.
    """
    since_s = since.timestamp()
    seen: set[Path] = set()
    for root in roots:
        if not root.exists():
            continue
        for path in sorted(root.glob("**/*.jsonl")):
            if path in seen:
                continue
            seen.add(path)
            try:
                if path.stat().st_mtime < since_s:
                    continue
                raw_lines = path.read_text(errors="replace").splitlines()
            except OSError:
                continue
            for raw in raw_lines[:_MAX_LINES_PER_FILE]:
                if not raw.strip():
                    continue
                try:
                    row = json.loads(raw)
                except json.JSONDecodeError:
                    continue
                if not isinstance(row, dict):
                    continue
                yield path, row


def _sender_sends(path: Path, row: dict) -> list[dict]:
    """Extract one transcript row's native ``SendMessage`` tool calls."""
    if row.get("type") != "assistant":
        return []
    message = row.get("message") or {}
    content = message.get("content")
    if not isinstance(content, list):
        return []
    out: list[dict] = []
    for block in content:
        if not isinstance(block, dict):
            continue
        if block.get("type") != "tool_use" or block.get("name") != "SendMessage":
            continue
        tool_input = block.get("input") or {}
        to = tool_input.get("to") or tool_input.get("recipient") or ""
        body = tool_input.get("message")
        if body is None:
            body = tool_input.get("content")
        if not isinstance(to, str) or not to or not isinstance(body, str) or not body:
            continue
        out.append(
            {
                "row_uuid": str(row.get("uuid") or ""),
                "ts": str(row.get("timestamp") or ""),
                "sender_session": str(row.get("sessionId") or ""),
                "to": to,
                "summary": tool_input.get("summary"),
                "body": body,
                "sender_transcript": str(path),
            }
        )
    return out


def _norm_head(text: str, n: int = 120) -> str:
    return " ".join(text.split())[:n]


def _join(
    sends: list[dict], blocks: list[dict]
) -> Iterator[tuple[dict, dict]]:
    """Pair each send with its receiver-side block: the addressed socket
    must match the block's ``from`` and the body head must match. The join
    is deterministic, so a re-scan pairs the same halves."""
    by_from: dict[str, list[dict]] = {}
    for block in blocks:
        key = block.get("from")
        if isinstance(key, str) and key:
            by_from.setdefault(key, []).append(block)
    for send in sends:
        candidates = by_from.get(send["to"], [])
        head = _norm_head(send["body"])
        match = next(
            (b for b in candidates if _norm_head(b.get("body") or "") == head),
            None,
        )
        if match is not None:
            yield send, match


def cmd_mail_backfill(
    since: Optional[str] = typer.Option(
        None, "--since", help="Window start (ISO). Default: the outage start."
    ),
    until: Optional[str] = typer.Option(
        None, "--until", help="Window end (ISO). Default: the outage end."
    ),
    root: Optional[list[Path]] = typer.Option(
        None, "--root", help="Transcript store root(s). Default: this host's."
    ),
    apply: bool = typer.Option(
        False, "--apply", help="Write the joined rows. Default: dry run."
    ),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit JSON."),
) -> None:
    """Backfill outage-era cross-session traffic into the mail store.

    Scans the harness transcripts for native ``SendMessage`` calls, joins
    each with its receiver-side block, and writes the joined traffic as
    audit-only rows (``delivery=cross-session``) that never re-deliver.
    Idempotent by msg_id: a re-run skips what already landed.
    """
    from fno.agents.discover import default_projects_dir
    from fno.bus.log import iter_messages, record_backfill_delivery

    since_dt = _parse_ts(since) if since else _parse_ts(OUTAGE_SINCE)
    until_dt = _parse_ts(until) if until else _parse_ts(OUTAGE_UNTIL)
    roots = [Path(r) for r in root] if root else [default_projects_dir()]

    sends: list[dict] = []
    blocks: list[dict] = []
    for path, row in _iter_rows(roots, since_dt, until_dt):
        row_ts = row.get("timestamp")
        if isinstance(row_ts, str) and row_ts:
            try:
                in_window = since_dt <= _parse_ts(row_ts) <= until_dt
            except ValueError:
                in_window = True
            if not in_window:
                continue
        sends.extend(_sender_sends(path, row))
        if row.get("type") != "user":
            continue
        text = _row_text(row)
        if "<cross-session-message" not in text:
            continue
        try:
            parsed = json.loads(_verb(["block"], stdin_text=text))
        except Exception as exc:  # noqa: BLE001 - one bad row never kills the scan
            print(f"warning: block parse failed: {exc}", file=sys.stderr)
            continue
        for block in parsed if isinstance(parsed, list) else []:
            if isinstance(block, dict):
                block["receiver_session"] = str(row.get("sessionId") or "")
                block["receiver_transcript"] = str(path)
                blocks.append(block)

    joined = list(_join(sends, blocks))
    written = 0
    if apply:
        existing_ids = {m.id for m in iter_messages()}
        rows: list[dict] = []
        for send, block in joined:
            if not send["row_uuid"]:
                print(
                    "warning: send without row uuid cannot get a stable id; skipped",
                    file=sys.stderr,
                )
                continue
            msg_id = _verb(
                ["msgid", "--sender-session", send["sender_session"], "--row-uuid", send["row_uuid"]]
            )
            if msg_id in existing_ids:
                continue
            argv = [
                "row",
                "--msg-id", msg_id,
                "--from", block.get("from_name") or send["sender_session"],
                "--to", block["receiver_session"],
                "--ts", send["ts"],
                "--from-session", send["sender_session"],
                "--to-session", block["receiver_session"],
                "--sender-transcript", send["sender_transcript"],
                "--receiver-transcript", block["receiver_transcript"],
                "--sender-row", send["row_uuid"],
                "--backfilled-at", _iso(datetime.now(tz=timezone.utc)),
            ]
            if send.get("summary"):
                argv += ["--subject", str(send["summary"])]
            try:
                raw = _verb(argv, stdin_text=send["body"])
            except Exception as exc:  # noqa: BLE001
                print(f"warning: row refused for {send['row_uuid']}: {exc}", file=sys.stderr)
                continue
            payload = json.loads(raw)
            if payload["id"] in existing_ids:
                continue
            record_backfill_delivery(
                msg_id=payload["id"],
                sender=payload["from_"],
                recipient=payload["to"],
                body=payload["body"],
                ts=payload["ts"],
                from_session=payload["from_session"],
                to_session=payload["to_session"],
                sender_transcript=payload["meta"]["sender_transcript"],
                receiver_transcript=payload["meta"]["receiver_transcript"],
                subject=payload.get("subject"),
                sender_row=payload["meta"].get("sender_row"),
                backfilled_at=payload["meta"].get("backfilled_at"),
                word_count=payload.get("word_count"),
            )
            written += 1
            rows.append({"id": payload["id"], "to": payload["to"]})
        if json_out:
            print(json.dumps({"written": written, "joined": len(joined)}, ensure_ascii=False))
            return
        print(f"backfill: wrote {written}, joined {len(joined)}")
        return

    if json_out:
        print(json.dumps([_dry_row(s, b) for s, b in joined], ensure_ascii=False, indent=None))
        return
    for send, block in joined:
        print(f"{send['ts']}  {send['sender_session'][:8]} -> {block['receiver_session'][:8]}  {_norm_head(send['body'], 60)}")
    print(f"backfill dry run: {len(joined)} row(s); re-run with --apply to write")


def _dry_row(send: dict, block: dict) -> dict:
    return {
        "ts": send["ts"],
        "from_session": send["sender_session"],
        "to_session": block.get("receiver_session"),
        "body_head": _norm_head(send["body"], 60),
    }


def _row_text(row: dict) -> str:
    message = row.get("message") or {}
    content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(
            b.get("text", "") for b in content if isinstance(b, dict)
        )
    return ""
