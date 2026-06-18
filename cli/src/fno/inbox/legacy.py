"""Legacy flat-file inbox parser (preserved for migration script only).

The pre-2026-05 inbox format was one file per recipient
(`<inbox-agents-root>/{proj}/inbox.md`) holding all messages as
``## msg-{id} · {ts} · from:{sender} · kind:{kind}`` blocks. Threading
existed via the inline `reply_to:` token in the header.

This module preserves the parser side of that format so the migration
script (`scripts/migrate-inbox-flat-to-threads.py`) can read existing
flat files into the new thread-per-file layout. Nothing in the live
runtime should import from this module.
"""
from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Optional


class LegacyKind(str, Enum):
    QUESTION = "question"
    ANSWER = "answer"
    HEADS_UP = "heads-up"
    NOTIFICATION = "notification"
    LESSON = "lesson"
    COMPLETE = "complete"
    FYI = "fyi"


class LegacyStatus(str, Enum):
    UNREAD = "unread"
    READ = "read"
    ANSWERED = "answered"


@dataclass
class LegacyMessage:
    msg_id: str
    timestamp: datetime
    from_project: str
    kind: str
    reply_to: Optional[str]
    status: str
    triaged_into: Optional[str]
    refs: dict
    body: str


_SEP = " · "
_HEADER_RE = re.compile(
    r"^## (msg-[0-9a-zA-Z]{1,}) · (\d{4}-\d{2}-\d{2} \d{2}:\d{2}) · from:(\S+) · kind:(\S+)(.*)?$"
)
_REPLY_TO_RE = re.compile(r"· reply_to:(msg-[0-9a-zA-Z]{1,})")


def _parse_timestamp(s: str) -> datetime:
    return datetime.strptime(s, "%Y-%m-%d %H:%M").replace(tzinfo=timezone.utc)


def _parse_block(
    header_line: str,
    field_lines: list[str],
    body_lines: list[str],
) -> Optional[LegacyMessage]:
    m = _HEADER_RE.match(header_line)
    if not m:
        return None

    msg_id = m.group(1)
    timestamp_str = m.group(2)
    from_project = m.group(3)
    kind_str = m.group(4)
    trailer = m.group(5) or ""

    reply_to = None
    rt_match = _REPLY_TO_RE.search(trailer)
    if rt_match:
        reply_to = rt_match.group(1)

    try:
        timestamp = _parse_timestamp(timestamp_str)
    except ValueError:
        return None

    fields: dict[str, str] = {}
    for line in field_lines:
        if ": " in line:
            k, _, v = line.partition(": ")
            fields[k.strip()] = v.strip()
        elif ":" in line:
            k, _, v = line.partition(":")
            fields[k.strip()] = v.strip()

    status = fields.get("status", "unread")
    triaged_into_raw = fields.get("triaged_into", "null")
    triaged_into = None if triaged_into_raw in ("null", "None", "") else triaged_into_raw

    ref_keys = (
        "ref_pr",
        "ref_node",
        "ref_gate",
        "mission_id",
        "source_mission",
        "cascade_of",
    )
    refs = {k: fields[k] for k in ref_keys if k in fields}

    body = "\n".join(body_lines).strip()

    return LegacyMessage(
        msg_id=msg_id,
        timestamp=timestamp,
        from_project=from_project,
        kind=kind_str,
        reply_to=reply_to,
        status=status,
        triaged_into=triaged_into,
        refs=refs,
        body=body,
    )


def parse_legacy_inbox(inbox_path: Path) -> list[LegacyMessage]:
    """Parse the pre-2026-05 flat ``inbox.md`` format. Used by migration."""
    if not inbox_path.exists():
        return []

    content = inbox_path.read_text(encoding="utf-8")
    lines = content.splitlines()

    messages: list[LegacyMessage] = []
    i = 0
    n = len(lines)

    while i < n:
        if not lines[i].startswith("## msg-"):
            i += 1
            continue

        header_line = lines[i]
        i += 1

        field_lines: list[str] = []
        while i < n and lines[i].strip() and not lines[i].startswith("## msg-"):
            field_lines.append(lines[i])
            i += 1

        while i < n and not lines[i].strip() and not lines[i].startswith("## msg-"):
            i += 1

        body_lines: list[str] = []
        while i < n and not lines[i].startswith("## msg-"):
            body_lines.append(lines[i])
            i += 1

        msg = _parse_block(header_line, field_lines, body_lines)
        if msg is not None:
            messages.append(msg)

    return messages
