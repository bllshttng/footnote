"""Escalate the watchdog's stale bucket to one durable operator question."""
from __future__ import annotations

import hashlib
import re
from dataclasses import dataclass
from pathlib import Path

MARKER = "watchdog-stale"
MAX_LISTED_ROWS = 10


@dataclass(frozen=True)
class StaleRow:
    row_id: str
    name: str
    state: str
    node: str | None
    basis: str


def dedupe_key(row_ids: "list[str]") -> str:
    joined = "\n".join(sorted(set(row_ids)))
    return hashlib.sha256(joined.encode("utf-8")).hexdigest()[:12]


def _unique_rows(rows: "list[StaleRow]") -> "list[StaleRow]":
    return sorted({row.row_id: row for row in rows}.values(), key=lambda row: row.row_id)


def _oldest_hours(rows: "list[StaleRow]") -> "int | None":
    hours = [
        int(match.group(1))
        for row in rows
        if (match := re.search(r"\b(\d+)h\b", row.basis))
    ]
    return max(hours) if hours else None


def question_text(rows: "list[StaleRow]", key: str) -> str:
    unique = _unique_rows(rows)
    oldest = _oldest_hours(unique)
    age = f", oldest {oldest}h" if oldest is not None else ""
    shown = [
        f"{row.name} [{row.node or 'node unknown'}]: "
        f"{row.basis or 'no basis recorded'}"
        for row in unique[:MAX_LISTED_ROWS]
    ]
    if len(unique) > MAX_LISTED_ROWS:
        shown.append(f"and {len(unique) - MAX_LISTED_ROWS} more")
    return (
        f"[{MARKER}:{key}] The fleet watchdog holds {len(unique)} stale row(s) "
        f"no lane will act on{age}. Nothing in the sweep clears these; each needs "
        f"a human to reap it or resume it. Rows: {'; '.join(shown)}"
    )


def already_asked(root: Path, key: str) -> "str | None":
    from fno.outstanding.core import read_open_questions

    needle = f"[{MARKER}:{key}]"
    for question in read_open_questions(root):
        if needle in question.question:
            return question.id
    return None


def escalate_stale(
    rows: "list[StaleRow]",
    *,
    root: Path,
    session_id: "str | None",
    cwd: Path,
) -> "tuple[str, str]":
    if not rows:
        return ("none", "")

    import secrets

    from fno.events import operator_question
    from fno.outstanding.core import append_question_event

    unique = _unique_rows(rows)
    key = dedupe_key([row.row_id for row in unique])
    existing = already_asked(root, key)
    if existing:
        return ("duplicate", existing)

    oldest = _oldest_hours(unique)
    age = f", oldest {oldest}h" if oldest is not None else ""
    qid = f"q-{secrets.token_hex(4)}"
    append_question_event(
        operator_question(
            question_id=qid,
            question=question_text(unique, key),
            session_id=session_id,
            cwd=str(cwd),
            ask=(
                f"triage {len(unique)} stale watchdog row(s){age}: "
                "fno agents watchdog --only stale"
            ),
            source="daemon",
        ),
        root,
    )
    return ("recorded", qid)
