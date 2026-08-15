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
from typing import Any, Iterable, Iterator, Optional

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
class Capture:
    """One open ``fu-`` item from some project's capture-tier inbox."""

    fu_id: str
    title: str
    project: str
    added_at: Optional[str] = None

    @property
    def age_days(self) -> Optional[int]:
        return _age_days(self.added_at) if self.added_at else None

    def as_dict(self) -> "dict[str, Any]":
        return {
            "id": self.fu_id,
            "title": self.title,
            "project": self.project,
            "added_at": self.added_at,
        }


@dataclass(frozen=True)
class Outstanding:
    carveout_total: int
    carveout_by_kind: "dict[str, int]"
    carveout_oldest_ts: Optional[str]
    questions: "list[Question]"
    captures: "list[Capture]"
    capture_file_total: int = 0
    capture_row_total: int = 0

    @property
    def empty(self) -> bool:
        return self.carveout_total == 0 and not self.questions and not self.captures

    def as_dict(self) -> "dict[str, Any]":
        by_project: "dict[str, int]" = {}
        for c in self.captures:
            by_project[c.project] = by_project.get(c.project, 0) + 1
        return {
            "carveouts": {
                "total": self.carveout_total,
                "by_kind": dict(self.carveout_by_kind),
                "oldest_ts": self.carveout_oldest_ts,
            },
            "questions": [q.as_dict() for q in self.questions],
            "captures": {
                "total": len(self.captures),
                "resolved_files": self.capture_file_total,
                "parsed_open_rows": self.capture_row_total,
                "repeated_ids": self.capture_row_total - len(self.captures),
                "by_project": by_project,
                "items": [c.as_dict() for c in self.captures],
            },
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


def _iter_question_lines(fh: "Iterable[str]") -> "Iterator[str]":
    """Yield only the lines that can possibly be a question event.

    The substring test runs per line as it is read, so neither the whole file
    nor the whole filtered set is ever held beyond what the fold needs. Both
    question types share ``QUESTION_MARKER``, so a line without it cannot be
    one of ours and skipping it changes no outcome.
    """
    for line in fh:
        if QUESTION_MARKER in line:
            yield line


def read_open_questions(root: Path) -> "list[Question]":
    """Fold ``operator_question`` minus ``operator_question_closed``.

    Newest first. A malformed line is SKIPPED, never raised, inheriting
    ``read_carveouts``' rule: one bad row must not cost the others. A missing
    journal is the common case and reads as no questions.
    """
    path = events_path(root)
    if not path.exists():
        return []

    asked: "dict[str, Question]" = {}
    closed: "set[str]" = set()
    try:
        # STREAM, never read_text().splitlines(). This journal is shared and
        # never rotated, so materializing it holds the whole file in memory
        # before the prefilter below skips anything - the read itself becomes
        # the cost the prefilter was added to remove.
        with path.open(encoding="utf-8") as fh:
            lines = list(_iter_question_lines(fh))
    except (OSError, UnicodeDecodeError) as exc:
        raise OutstandingError(f"cannot read events journal {path}: {exc}") from exc

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


def _capture_project_roots(root: Path) -> "list[Path]":
    """This project plus every project root the machine-wide graph names.

    The graph is the one machine-wide store (``~/.fno/graph.json``), and its
    entries carry the ``cwd``/``source_cwd`` each node was worked from, so it
    is the measured-fact enumeration of sibling projects - no second registry.
    Dead or missing roots degrade to zero captures in the fold below, and a
    corrupt or absent graph reads as this project alone.
    """
    roots = {Path(root).resolve()}
    try:
        # The path is passed EXPLICITLY (module attr, not the def-time default
        # arg) so a redirected graph - hermetic tests, a configured override -
        # is the one enumerated here.
        from fno.graph import store as graph_store
        from fno.graph.store import read_graph

        for e in read_graph(graph_store.GRAPH_JSON):
            if not isinstance(e, dict):
                continue
            for key in ("cwd", "source_cwd"):
                raw = e.get(key)
                if isinstance(raw, str) and raw:
                    p = Path(raw)
                    if p.is_dir():
                        roots.add(p.resolve())
    except Exception:  # noqa: BLE001 - the graph is advisory here, never fatal
        pass
    return sorted(roots)


def _capture_added_at(root: Path) -> "dict[str, str]":
    """fu_id -> capture_add ts from this root's events journal.

    The inbox markdown carries no dates, so age comes from the event that
    recorded the capture. Substring prefilter before json.loads, mirroring
    ``read_open_questions``: the journal is shared and never rotated.
    """
    path = events_path(root)
    if not path.exists():
        return {}
    added: "dict[str, str]" = {}
    try:
        with path.open(encoding="utf-8") as fh:
            for line in fh:
                if "capture_add" not in line:
                    continue
                try:
                    rec = json.loads(line)
                except (json.JSONDecodeError, ValueError):
                    continue
                if not isinstance(rec, dict) or rec.get("type") != "capture_add":
                    continue
                data = rec.get("data")
                if not isinstance(data, dict):
                    continue
                fu_id = data.get("fu_id")
                ts = rec.get("ts")
                if isinstance(fu_id, str) and isinstance(ts, str) and ts:
                    # First write wins: the ORIGINAL capture date is the age
                    # signal, not a re-capture of the same fu- id.
                    added.setdefault(fu_id, ts)
    except (OSError, UnicodeDecodeError):
        return {}
    return added


def _read_open_captures_with_counts(root: Path) -> "tuple[list[Capture], int, int]":
    """Fold every project's capture-tier inbox. Never raises.

    A missing or unreadable inbox (or a whole missing project root)
    contributes zero captures while the other projects still report -
    mirroring ``read_open_questions``' degrade-to-empty contract, because a
    pile the operator cannot see and a pile that failed to read must not
    render as the same "nothing".

    Deduped twice, because the roots enumerate WORKTREES too: sibling
    worktrees of one repo resolve (through the vault symlink) to the SAME
    inbox file, so counting per root multiplied this repo's 293 open items
    into 18,459 on the first live run. First by RESOLVED inbox path (one
    file is read once, and a canonical checkout - a real ``.git`` dir -
    supplies the label, since a worktree directory names a branch rather than
    a project), then by ``fu_id`` across the distinct files that
    remain.
    """
    from fno.backlog.capture import parse_items
    from fno.paths import inbox_path

    def _store_label(path: Path, roots: "list[Path]") -> str:
        """Name the pile by the STORE it is, not by which repo reached it.

        On a vault-linked machine many repos' ``internal`` symlinks land in
        ONE shared inbox file; naming that group after whichever repo sorted
        first (or last) reads as one project's pile when it is everyone's.
        Under $HOME the first path segment names the vault/store; outside it
        (hermetic tests, foreign roots) the first contributing root's name.
        """
        try:
            home = Path.home().resolve()
            rel = path.relative_to(home)
            first = next(iter(rel.parts), None)
            if first:
                return first
        except (ValueError, OSError):
            pass
        return roots[0].name if roots else "unknown"

    by_path: "dict[Path, list[Path]]" = {}
    for project_root in _capture_project_roots(root):
        try:
            path = inbox_path(project_root=project_root).resolve()
            if not path.exists():
                continue
        except Exception:  # noqa: BLE001 - a bad path is one project less
            continue
        by_path.setdefault(path, []).append(project_root)

    captures: "list[Capture]" = []
    seen_ids: "set[str]" = set()
    file_total = 0
    row_total = 0
    for path, roots in sorted(by_path.items()):
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        file_total += 1
        items = parse_items(text)
        row_total += len(items)
        project = _store_label(path, roots)
        # Canonical checkouts (a real .git dir) lead the events lookup. Fold
        # every distinct journal because projects can share one configured
        # capture file while writing capture events to separate journals.
        roots = sorted(roots, key=lambda r: not (r / ".git").is_dir())
        added: "dict[str, str]" = {}
        seen_journals: "set[Path]" = set()
        for r in roots:
            try:
                journal = events_path(r).resolve()
            except OSError:
                continue
            if journal in seen_journals:
                continue
            seen_journals.add(journal)
            for fu_id, ts in _capture_added_at(r).items():
                previous = added.get(fu_id)
                if previous is None or ts < previous:
                    added[fu_id] = ts
        for item in items:
            if item["id"] in seen_ids:
                continue
            seen_ids.add(item["id"])
            captures.append(
                Capture(
                    fu_id=item["id"],
                    title=item["title"],
                    project=project,
                    added_at=added.get(item["id"]),
                )
            )
    # Oldest first where the age is known, so the render's cap keeps the
    # items that have waited longest, not an arbitrary three.
    captures.sort(key=lambda c: (c.added_at is None, c.added_at or "", c.fu_id))
    return captures, file_total, row_total


def read_open_captures(root: Path) -> "list[Capture]":
    """Return the open capture rows from the machine-wide project fold."""
    return _read_open_captures_with_counts(root)[0]


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

    captures, capture_file_total, capture_row_total = _read_open_captures_with_counts(root)
    return Outstanding(
        carveout_total=len(rows),
        carveout_by_kind=by_kind,
        carveout_oldest_ts=stamps[0] if stamps else None,
        questions=read_open_questions(root),
        captures=captures,
        capture_file_total=capture_file_total,
        capture_row_total=capture_row_total,
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
        lines.append("")

    if outstanding.captures:
        total = len(outstanding.captures)
        lines.append(f"{_plural(total, 'capture')} awaiting triage (fu- items).")
        shown = outstanding.captures[:RENDER_CAP]
        for c in shown:
            where = f"  ({c.project})" if c.project != Path.cwd().name else ""
            lines.append(f"  {c.fu_id}  {c.title}{where}")
        # Ruling 2: the cap without the count is a lie by omission in a nicer
        # font - a 200-row pile gets abandoned in a week, but so does a list
        # showing 3 rows while 195 wait, unnamed.
        ages = [c.age_days for c in outstanding.captures if c.age_days is not None]
        summary = f"  Showing {len(shown)} of {total}"
        if ages:
            summary += f", oldest {_plural(max(ages), 'day')}"
        summary += (
            f". Count rule: {total} unique open fu-* IDs across "
            f"{_plural(outstanding.capture_file_total, 'resolved capture file')} "
            f"({_plural(outstanding.capture_row_total, 'parsed open row')} minus "
            f"{_plural(outstanding.capture_row_total - total, 'repeated ID')}); "
            "post_merge.parking_lot_path is used when configured."
        )
        lines.append(summary)
        lines.append("  Triage with: fno backlog triage, or fno backlog capture promote <fu-id>.")

    return "\n".join(lines).rstrip() + "\n"
