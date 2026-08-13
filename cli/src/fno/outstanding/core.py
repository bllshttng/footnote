"""Fold the two outstanding-work stores into one report.

Why this exists at all: the clearing path was never missing. ``consume_carveouts``
has had automatic callers the whole time (``retro/cli.py`` after a clean land,
``retro/sweep.py`` as the sweep's consume function). What was missing is anyone
ASKING a human to run the sweep. The ledger reached 39 rows over 29 days because
only ``deferred`` carve-outs block anything (condition D in ``graph/_reconcile.py``
refuses a node close on an unharvested one), and 39 of those rows are ``oos-bug``,
which blocks nothing, ever. So the fix is a surface, not a wire.

Read-only by construction: this module imports no graph or state writer.
"""
from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

# Rows rendered before the footer takes over. A growing pile should read as a
# number, not as a wall - a block that scrolls the operator's screen gets
# skipped, which is the failure this verb exists to fix.
RENDER_CAP = 3

EVENTS_NAME = "events.jsonl"
# `retro sweep-carveouts` skips this kind; /fno:pr merged owns it.
BACKFILL_KIND = "backfill"
QUESTION_EVENT = "operator_question"
QUESTION_CLOSED_EVENT = "operator_question_closed"
# Both question types share this prefix, so a raw line without it cannot be
# one of ours. See the substring prefilter in read_open_questions.
QUESTION_MARKER = "operator_question"


class OutstandingError(Exception):
    """A store exists but could not be read.

    Distinct from "nothing outstanding": a failed read reported as an empty
    one is the absence-as-success trap, and here it would tell an operator the
    queue is clear when it is merely unreadable.
    """


@dataclass(frozen=True)
class Question:
    id: str
    ts: str
    question: str
    session_id: Optional[str] = None
    cwd: Optional[str] = None
    node: Optional[str] = None

    def as_dict(self) -> "dict[str, Any]":
        return {
            "id": self.id,
            "ts": self.ts,
            "question": self.question,
            "session_id": self.session_id,
            "cwd": self.cwd,
            "node": self.node,
        }


@dataclass(frozen=True)
class Outstanding:
    carveout_total: int
    carveout_by_kind: "dict[str, int]"
    carveout_oldest_ts: Optional[str]
    questions: "list[Question]"

    @property
    def empty(self) -> bool:
        return self.carveout_total == 0 and not self.questions

    def as_dict(self) -> "dict[str, Any]":
        return {
            "carveouts": {
                "total": self.carveout_total,
                "by_kind": dict(self.carveout_by_kind),
                "oldest_ts": self.carveout_oldest_ts,
            },
            "questions": [q.as_dict() for q in self.questions],
        }


def events_path(root: Path) -> Path:
    """The events journal beside the carve-out ledger under the same root.

    Routes through ``project_log``, the single accessor for ``.fno/<name>``,
    rather than hand-building the path: that keeps the ``.resolve()`` (so a
    symlinked root yields one path string, not two) and keeps this reader on
    the same placement rule as every other writer.
    """
    from fno.paths import project_log

    return project_log(EVENTS_NAME, project_root=Path(root))


def read_open_questions(root: Path) -> "list[Question]":
    """Fold ``operator_question`` minus ``operator_question_closed``.

    Newest first. A malformed line is SKIPPED, never raised, inheriting
    ``read_carveouts``' rule: one bad row must not cost the others. A missing
    journal is the common case and reads as no questions.
    """
    path = events_path(root)
    if not path.exists():
        return []
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError) as exc:
        raise OutstandingError(f"cannot read events journal {path}: {exc}") from exc

    asked: "dict[str, Question]" = {}
    closed: "set[str]" = set()
    for line in lines:
        stripped = line.strip()
        if not stripped:
            continue
        # Substring prefilter before json.loads. This journal is shared,
        # append-only and never rotated, so nearly every line belongs to some
        # other event type; parsing all of them cost ~0.9s against the hook's
        # 3s bound, and the bound firing does not surface an error - the block
        # just vanishes and the operator reads "nothing outstanding". That is
        # the absence-as-success failure this whole verb exists to prevent, so
        # the read must not get slower as the journal grows. Full history is
        # preserved: questions never expire, so a tail read is not an option.
        if QUESTION_MARKER not in stripped:
            continue
        try:
            rec = json.loads(stripped)
        except (json.JSONDecodeError, ValueError):
            continue
        if not isinstance(rec, dict):
            continue
        data = rec.get("data")
        if not isinstance(data, dict):
            continue
        if rec.get("type") == QUESTION_EVENT:
            qid = data.get("question_id")
            if not qid:
                continue
            asked[str(qid)] = Question(
                id=str(qid),
                ts=str(rec.get("ts") or ""),
                question=str(data.get("question") or ""),
                session_id=data.get("session_id") or None,
                cwd=data.get("cwd") or None,
                node=data.get("node") or None,
            )
        elif rec.get("type") == QUESTION_CLOSED_EVENT:
            qid = data.get("question_id")
            if qid:
                closed.add(str(qid))

    open_qs = [q for qid, q in asked.items() if qid not in closed]
    # Newest first. No auto-expiry: a question that goes quiet with age is the
    # exact failure being fixed, so age never removes one from this list.
    open_qs.sort(key=lambda q: q.ts, reverse=True)
    return open_qs


def collect(root: Path) -> Outstanding:
    """Read both legs. Raises ``OutstandingError`` if either store is unreadable."""
    from fno.carveout.core import CarveoutError, read_carveouts

    try:
        rows = read_carveouts(Path(root))
    except CarveoutError as exc:
        raise OutstandingError(str(exc)) from exc

    by_kind: "dict[str, int]" = {}
    for r in rows:
        kind = str(r.get("kind") or "unknown")
        by_kind[kind] = by_kind.get(kind, 0) + 1
    stamps = sorted(str(r.get("ts") or "") for r in rows if r.get("ts"))

    return Outstanding(
        carveout_total=len(rows),
        carveout_by_kind=by_kind,
        carveout_oldest_ts=stamps[0] if stamps else None,
        questions=read_open_questions(root),
    )


def _age_days(ts: str) -> Optional[int]:
    try:
        when = datetime.fromisoformat(ts.replace("Z", "+00:00"))
    except (ValueError, AttributeError):
        return None
    if when.tzinfo is None:
        when = when.replace(tzinfo=timezone.utc)
    return max(0, (datetime.now(timezone.utc) - when).days)


def _plural(n: int, word: str) -> str:
    return f"{n} {word}" if n == 1 else f"{n} {word}s"


def render(outstanding: Outstanding, *, session_id: Optional[str] = None) -> str:
    """Render the human block. Empty string when nothing is outstanding.

    Silence on zero is the correct steady state and must be reachable, which is
    why the test for it ships with a positive control asserting that non-zero
    input does render.

    There is no attended-vs-worker branch. One existed, rendering a bare count
    to a worker, and it produced two defects at once: it keyed on a presence
    check that answers "is a human here" rather than "is this a worker", so a
    gemini or agy operator got the truncated form, and the branch printed
    "clear <id>" after suppressing every id. The ``RENDER_CAP`` already keeps
    any session's share to three rows plus a count, which is what the short
    render was for, so the branch bought nothing that the cap does not.
    """
    if outstanding.empty:
        return ""

    lines: "list[str]" = ["## Outstanding for you", ""]

    if outstanding.carveout_total:
        split = ", ".join(
            f"{n} {kind}" for kind, n in sorted(outstanding.carveout_by_kind.items())
        )
        head = f"{_plural(outstanding.carveout_total, 'carve-out')} unharvested"
        if split:
            head += f" ({split})"
        if outstanding.carveout_oldest_ts:
            age = _age_days(outstanding.carveout_oldest_ts)
            oldest = outstanding.carveout_oldest_ts[:10]
            head += f", oldest {oldest}"
            if age is not None:
                head += f" ({_plural(age, 'day')} ago)"
        lines.append(head + ".")
        # Naming the clearing verb IS the fix. The harvest is manual by design
        # (a background hook that mints backlog nodes unattended is the wrong
        # shape) and nothing else ever tells a human what to run.
        #
        # Route by kind. `retro sweep-carveouts` SKIPS backfill rows entirely
        # (they belong to /fno:pr merged), so prescribing it against a
        # backfill-only ledger sends the operator to a verb that clears
        # nothing and reports the same count on every later session.
        sweepable = sum(
            n for kind, n in outstanding.carveout_by_kind.items() if kind != BACKFILL_KIND
        )
        if sweepable:
            lines.append(
                "  Clear with: fno retro sweep-carveouts (preview), --apply to file and consume."
            )
        if outstanding.carveout_by_kind.get(BACKFILL_KIND):
            lines.append("  Backfill rows are handled by /fno:pr merged, not by the sweep.")
        lines.append("")

    if outstanding.questions:
        mine = [q for q in outstanding.questions if session_id and q.session_id == session_id]
        theirs = [q for q in outstanding.questions if q not in mine]
        lines.append(f"{_plural(len(outstanding.questions), 'open question')} awaiting you.")

        shown = (mine + theirs)[:RENDER_CAP]
        for q in shown:
            label = "[this session] " if q in mine else ""
            # cwd/node are captured so a cross-project question names its
            # origin. Rendering them is the whole reason they are recorded.
            where = q.node or (Path(q.cwd).name if q.cwd else None)
            suffix = f"  ({where})" if where else ""
            lines.append(f"  {label}{q.id}  {q.question}{suffix}")
        dropped = len(outstanding.questions) - len(shown)
        if dropped:
            lines.append(f"  ... and {dropped} more.")
        lines.append("  Answer with: fno outstanding clear <id> --answer \"...\"")

    return "\n".join(lines).rstrip() + "\n"
