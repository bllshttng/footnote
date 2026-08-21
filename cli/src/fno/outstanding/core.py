"""Fold the two outstanding-work stores into one report.

Why this exists at all: the clearing path was never missing. ``consume_carveouts``
has had automatic callers the whole time (``retro/cli.py`` after a clean land,
``retro/sweep.py`` as the sweep's consume function). What was missing is anyone
ASKING a human to run the sweep. The ledger reached 39 rows over 29 days because
only ``deferred`` carve-outs block anything (condition D in ``graph/_reconcile.py``
refuses a node close on an unharvested one), and 39 of those rows are ``oos-bug``,
which blocks nothing, ever. So the fix is a surface, not a wire.

The report path is read-only; explicit ask, clear, and reindex helpers own writes.
"""

from __future__ import annotations

import json
import os
import sys
import time
from dataclasses import dataclass, field, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Iterable, Iterator, Optional

from fno.king.lane import LaneItem, LaneRead, open_items, parked_items, read_lane

# Rows rendered before the footer takes over. A growing pile should read as a
# number, not as a wall - a block that scrolls the operator's screen gets
# skipped, which is the failure this verb exists to fix.
RENDER_CAP = 3
QUESTION_RENDER_CAP = 10

EVENTS_NAME = "events.jsonl"
# `retro sweep-carveouts` skips this kind; /fno:pr merged owns it.
BACKFILL_KIND = "backfill"
QUESTION_EVENT = "operator_question"
QUESTION_CLOSED_EVENT = "operator_question_closed"
# Both question types share this prefix, so a raw line without it cannot be
# one of ours. See the substring prefilter in read_open_questions.
QUESTION_MARKER = "operator_question"
# SessionStart has a 1.5s hook budget. Reachability may scan multi-gigabyte
# harness stores, so the report gives it one bounded slice and renders an
# explicit unknown for anything that cannot finish in time.
LIVENESS_BUDGET_SECONDS = 0.05


class OutstandingError(Exception):
    """A store exists but could not be read.

    Distinct from "nothing outstanding": a failed read reported as an empty
    one is the absence-as-success trap, and here it would tell an operator the
    queue is clear when it is merely unreadable.
    """


class QuestionIndexWriteError(OutstandingError, RuntimeError):
    """A question event is durable but missing from the recall index."""

    def __init__(self, question_id: str, cause: BaseException) -> None:
        super().__init__(str(cause))
        self.question_id = question_id
        self.cause = cause


@dataclass(frozen=True)
class Question:
    id: str
    ts: str
    question: str
    session_id: Optional[str] = None
    cwd: Optional[str] = None
    node: Optional[str] = None
    asker: Optional[str] = None
    ask: Optional[str] = None
    options: tuple[str, ...] = ()
    blocks: tuple[str, ...] = ()
    live: Optional[bool] = None

    def as_dict(self, *, rank: Optional[int] = None) -> "dict[str, Any]":
        return {
            "id": self.id,
            "ts": self.ts,
            "question": self.question,
            "session_id": self.session_id,
            "cwd": self.cwd,
            "node": self.node,
            "asker": self.asker,
            "ask": self.ask,
            "options": list(self.options),
            "blocks": list(self.blocks),
            "live": self.live,
            "rank": rank,
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
    lane: "list[LaneItem]" = field(default_factory=list)
    lane_parked: int = 0

    @property
    def empty(self) -> bool:
        return (
            self.carveout_total == 0
            and not self.questions
            and not self.captures
            and not self.lane
        )

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
            "questions": [q.as_dict(rank=rank) for rank, q in enumerate(self.questions, 1)],
            "captures": {
                "total": len(self.captures),
                "resolved_files": self.capture_file_total,
                "parsed_open_rows": self.capture_row_total,
                "repeated_ids": self.capture_row_total - len(self.captures),
                "by_project": by_project,
                "items": [c.as_dict() for c in self.captures],
            },
            "lane": {
                "total": len(self.lane),
                "parked": self.lane_parked,
                "items": [{"text": i.text, "line": i.line} for i in self.lane],
            },
        }


def events_path(root: Path) -> Path:
    """The events journal beside the carve-out ledger under the same root.

    Routes through ``project_log``, the single accessor for ``.fno/<name>``,
    rather than hand-building the path: that keeps the ``.resolve()`` (so a
    symlinked root yields one path string, not two) and keeps this reader on
    the same placement rule as every other writer.

    Deliberately NOT ``paths.project_events_json``, which honors the
    ``FNO_EVENTS_PATH`` pin: that pin stands in for a root nobody resolved, and
    this caller resolves one. Letting it win here would collapse every root the
    reader folds onto one file, and under test it would replace each test's own
    journal with the one sandbox journal the whole pytest process shares.
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


def questions_path() -> Path:
    """Resolve the machine-wide question index through the configured state dir."""
    from fno import paths

    return paths.questions_jsonl()


def append_question_event(event: dict[str, Any], root: Path) -> None:
    """Write one event to project durability first, then machine-wide recall."""
    from fno.events import append_event

    data = event.get("data")
    question_id = data.get("question_id") if isinstance(data, dict) else None
    if not question_id:
        raise ValueError("question event has no question_id")
    try:
        append_event(event, events_path=events_path(root))
    except Exception as exc:
        raise OutstandingError(f"failed to append question to project journal: {exc}") from exc
    try:
        append_event(event, events_path=questions_path())
    except Exception as exc:  # noqa: BLE001 - caller needs the durable event id
        raise QuestionIndexWriteError(str(question_id), exc) from exc


def _read_question_events(path: Path, *, missing_hint: bool) -> "list[dict[str, Any]]":
    """Read valid question envelopes, distinguishing absent from unreadable."""
    try:
        path.stat()
    except FileNotFoundError:
        try:
            path.lstat()
        except FileNotFoundError:
            if missing_hint:
                print(
                    f"outstanding: question index {path} is missing; run "
                    "`fno inbox outstanding reindex` to recover project questions.",
                    file=sys.stderr,
                )
            return []
        except OSError as exc:
            raise OutstandingError(f"cannot read question index {path}: {exc}") from exc
        raise OutstandingError(f"cannot read question index {path}: dangling symlink")
    except OSError as exc:
        raise OutstandingError(f"cannot read question index {path}: {exc}") from exc

    events: "list[dict[str, Any]]" = []
    try:
        with path.open(encoding="utf-8") as fh:
            for line in _iter_question_lines(fh):
                try:
                    rec = json.loads(line)
                except (json.JSONDecodeError, ValueError):
                    continue
                if not isinstance(rec, dict) or rec.get("type") not in {
                    QUESTION_EVENT,
                    QUESTION_CLOSED_EVENT,
                }:
                    continue
                data = rec.get("data")
                if isinstance(data, dict) and data.get("question_id"):
                    events.append(rec)
    except (OSError, UnicodeDecodeError) as exc:
        raise OutstandingError(f"cannot read question index {path}: {exc}") from exc
    return events


def _resolve_question_liveness(
    askers: "list[str]",
    *,
    budget_seconds: float,
    clock: "Callable[[], float]",
    resolver: "Callable[[str], tuple[Any, list[str]]] | None",
) -> "dict[str, bool]":
    """Resolve unique askers within one wall-clock slice.

    The resolver runs in a child process that is terminated and reaped at the
    deadline, so a store scan cannot survive until interpreter shutdown. Only
    answers completed by the deadline land in the result; every absent key is
    explicitly unknown, never guessed stale.
    """
    import multiprocessing

    if not askers or budget_seconds <= 0:
        return {}
    if resolver is None:
        from fno.agents.discover import resolve_reachable

        resolver = resolve_reachable
    context = multiprocessing.get_context("fork")
    receive, send = context.Pipe(duplex=False)
    _ = clock

    def resolve_all() -> None:
        answers: "dict[str, bool]" = {}
        try:
            for asker in dict.fromkeys(askers):
                try:
                    reachable, ambiguous = resolver(asker)
                    answer = reachable is not None and not ambiguous
                except Exception:  # noqa: BLE001 - a completed refusal is stale
                    answer = False
                answers[asker] = answer
        finally:
            try:
                send.send(answers)
            except (BrokenPipeError, OSError):
                pass
            send.close()

    process = context.Process(
        target=resolve_all, name="fno-question-liveness", daemon=True
    )
    process.start()
    send.close()
    answers: "dict[str, bool]" = {}
    try:
        if receive.poll(budget_seconds):
            payload = receive.recv()
            if isinstance(payload, dict):
                answers = payload
    except (EOFError, OSError):
        pass
    finally:
        receive.close()
        if process.is_alive():
            process.terminate()
        process.join(timeout=budget_seconds)
        if process.is_alive():
            process.kill()
            process.join()
    return answers


def read_open_questions(
    root: Path,
    *,
    liveness_budget_seconds: float = LIVENESS_BUDGET_SECONDS,
    clock: "Callable[[], float]" = time.monotonic,
    resolver: "Callable[[str], tuple[Any, list[str]]] | None" = None,
) -> "list[Question]":
    """Fold ``operator_question`` minus ``operator_question_closed``.

    Ranked by liveness, blocked nodes, age, then id. A malformed line is SKIPPED, never raised, inheriting
    ``read_carveouts``' rule: one bad row must not cost the others. A missing
    index reads as no questions with a recovery hint; an unreadable one fails.
    """
    _ = root  # The index is machine-wide; retain the argument for caller parity.
    asked: "dict[str, Question]" = {}
    closed: "set[str]" = set()
    for rec in _read_question_events(questions_path(), missing_hint=True):
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
                asker=data.get("asker") or None,
                ask=data.get("ask") or None,
                options=tuple(data.get("options") or ()),
                blocks=tuple(data.get("blocks") or ()),
            )
        elif rec.get("type") == QUESTION_CLOSED_EVENT:
            qid = data.get("question_id")
            if qid:
                closed.add(str(qid))

    open_qs = [q for qid, q in asked.items() if qid not in closed]
    resolved = _resolve_question_liveness(
        [q.asker for q in open_qs if q.asker],
        budget_seconds=liveness_budget_seconds,
        clock=clock,
        resolver=resolver,
    )
    open_qs = [
        replace(q, live=False if not q.asker else resolved.get(q.asker))
        for q in open_qs
    ]
    # A stale asker never outranks a reachable one, even when it blocks more.
    # Within each liveness lane, unblock the most nodes first, then honor the
    # questions that have waited longest. No auto-expiry: age only ranks.
    open_qs.sort(
        key=lambda q: (
            q.live is not True,
            -len(q.blocks),
            not bool(q.ts),
            q.ts,
            q.id,
        )
    )
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
        backend = os.environ.get("FNO_TRACKER_BACKEND") or "graph"
        if backend == "graph":
            from fno import paths

            payload = json.loads(paths.graph_json().read_text(encoding="utf-8"))
            entries = payload.get("entries", []) if isinstance(payload, dict) else payload
            locations = (
                (entry.get("cwd"), entry.get("source_cwd"))
                for entry in entries
                if isinstance(entry, dict)
            )
        else:
            # External trackers keep footnote-owned checkout fields in their
            # sidecars; graph mode reads those same fields from their owner.
            from fno.tracker import sidecar as sidecar_store

            locations = ((sc.cwd, sc.source_cwd) for sc in sidecar_store.load_all().values())
        for pair in locations:
            for raw in pair:
                if isinstance(raw, str) and raw:
                    p = Path(raw)
                    if p.is_dir():
                        # Graph checkout paths are already absolute in normal
                        # records. Keep relative legacy rows absolute without
                        # resolving every symlink here: the inbox and journal
                        # folds below resolve their physical stores once for
                        # deduplication, which is the authority that matters.
                        roots.add(Path(os.path.abspath(p)))
    except Exception:  # noqa: BLE001 - the graph is advisory here, never fatal
        pass
    return sorted(roots)


def _canonical_checkout_for(root: Path) -> Path:
    """Collapse a linked worktree onto its canonical checkout without git.

    Standard linked-worktree ``.git`` files point into
    ``<canonical>/.git/worktrees/<name>``. Other shapes keep the existing
    root so their established resolver can handle them.
    """
    root = Path(root)
    marker = root / ".git"
    if marker.is_dir():
        return root
    try:
        line = marker.read_text(encoding="utf-8").splitlines()[0]
    except (OSError, UnicodeDecodeError, IndexError):
        return root
    if not line.startswith("gitdir:"):
        return root
    git_dir = Path(line.partition(":")[2].strip())
    if not git_dir.is_absolute():
        git_dir = root / git_dir
    if git_dir.parent.name != "worktrees" or git_dir.parent.parent.name != ".git":
        return root
    canonical = git_dir.parent.parent.parent
    return canonical if (canonical / ".git").is_dir() else root


def _question_journals(root: Path) -> "list[Path]":
    """Every graph-named project journal, deduped by physical file."""
    seen: "set[tuple[int, int]]" = set()
    journals: "list[Path]" = []
    for project_root in _capture_project_roots(root):
        path = events_path(project_root)
        try:
            stat = path.stat()
        except OSError:
            continue
        key = (stat.st_dev, stat.st_ino)
        if key in seen:
            continue
        seen.add(key)
        journals.append(path)
    return journals


def _question_event_identity(event: dict[str, Any]) -> "tuple[str, str] | None":
    event_type = event.get("type")
    data = event.get("data")
    question_id = data.get("question_id") if isinstance(data, dict) else None
    if event_type not in {QUESTION_EVENT, QUESTION_CLOSED_EVENT} or not question_id:
        return None
    return str(event_type), str(question_id)


def reindex_questions(root: Path) -> "dict[str, int]":
    """Append every missing ask/close identity from graph-named projects."""
    from fno.events import append_event

    index = questions_path()
    existing = _read_question_events(index, missing_hint=False)
    known = {
        identity for event in existing if (identity := _question_event_identity(event)) is not None
    }
    preexisting = set(known)
    added = 0
    already: "set[tuple[str, str]]" = set()
    for journal in _question_journals(root):
        try:
            events = _read_question_events(journal, missing_hint=False)
        except OutstandingError:
            continue
        for event in events:
            identity = _question_event_identity(event)
            if identity is None:
                continue
            if identity in known:
                if identity in preexisting:
                    already.add(identity)
                continue
            append_event(event, events_path=index)
            known.add(identity)
            added += 1
    return {"added": added, "already": len(already), "total": len(known)}


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

    by_repo: "dict[Path, list[Path]]" = {}
    for project_root in _capture_project_roots(root):
        by_repo.setdefault(_canonical_checkout_for(project_root), []).append(project_root)

    by_path: "dict[Path, list[Path]]" = {}
    for config_root, project_roots in by_repo.items():
        try:
            path = inbox_path(project_root=config_root).resolve()
            if not path.exists():
                continue
        except Exception:  # noqa: BLE001 - a bad path is one project less
            continue
        by_path.setdefault(path, []).extend(project_roots)

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


def collect(root: Path, *, lane: "LaneRead | None" = None) -> Outstanding:
    """Read every leg. Raises ``OutstandingError`` if a store is unreadable.

    ``lane`` is a keyword-only, already-fetched read so core stays pure and
    testable with a hand-built one; the caller resolves the real file.
    """
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
    lane_read = lane if lane is not None else read_lane()
    if lane_read.error:
        raise OutstandingError(lane_read.error)
    return Outstanding(
        carveout_total=len(rows),
        carveout_by_kind=by_kind,
        carveout_oldest_ts=stamps[0] if stamps else None,
        questions=read_open_questions(root),
        captures=captures,
        capture_file_total=capture_file_total,
        capture_row_total=capture_row_total,
        lane=open_items(lane_read),
        lane_parked=len(parked_items(lane_read)),
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


def render(
    outstanding: Outstanding, *, session_id: Optional[str] = None, crowned: bool = False
) -> str:
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

    The crown splits the RENDER, never the READ. Every session prints the lane
    count and top item; only a crowned session also gets the action line.
    Gating the read itself would reproduce the defect this block exists to
    fix - most sessions are not crowned, so the lane would stay invisible in
    the common case and nobody would read it again.
    """
    if outstanding.empty:
        return ""

    lines: "list[str]" = ["## Outstanding for you", ""]

    if outstanding.lane:
        head = f"{_plural(len(outstanding.lane), 'item')} on your lane, top first."
        if outstanding.lane_parked:
            head += f" {outstanding.lane_parked} parked."
        lines.append(head)
        lines.append(f"  {outstanding.lane[0].text}")
        if crowned:
            lines.append(
                '  File one with: fno backlog idea "<text>", then stamp `-> <id>` '
                "onto its line, or park it with `-> parked: <reason>`."
            )
        lines.append("")

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
        lines.append(f"{_plural(len(outstanding.questions), 'open question')} awaiting you.")

        shown = outstanding.questions[:QUESTION_RENDER_CAP]

        def append_question(q: Question, *, stale_row: bool = False) -> None:
            label = "[this session] " if q in mine else ""
            where = q.node or (Path(q.cwd).name if q.cwd else None)
            details = [where] if where else []
            if stale_row:
                age = _age_days(q.ts)
                details.append(f"{_plural(age, 'day')} old" if age is not None else "age unknown")
            suffix = f"  ({'; '.join(details)})" if details else ""
            lines.append(f"  {label}{q.id}  {q.question}{suffix}")
            if q.ask:
                lines.append(f"    Action: {q.ask}")

        group_order: "list[str | bool | None]" = []
        grouped: "dict[str | bool | None, list[Question]]" = {}
        for q in shown:
            group: "str | bool | None" = "unaddressed" if q.asker is None else q.live
            if group not in grouped:
                group_order.append(group)
                grouped[group] = []
            grouped[group].append(q)

        has_stale = False
        has_unknown = False
        for group in group_order:
            if group == "unaddressed":
                lines.append("  Questions for you (no agent is waiting):")
            elif group is True:
                lines.append("  Live open questions:")
            elif group is False:
                lines.append("  Stale questions:")
            else:
                lines.append("  Questions with unknown liveness:")
            for q in grouped[group]:
                append_question(q, stale_row=group is False)
            has_stale = has_stale or group is False
            has_unknown = has_unknown or group is None
        if has_stale:
            lines.append("  Answering a stale question records the decision but reaches nobody.")
        if has_unknown:
            lines.append(
                "  Unknown means reachability did not finish inside the report budget; "
                "answering still records the decision."
            )
        if len(outstanding.questions) > QUESTION_RENDER_CAP:
            lines.append(
                f"  Showing {len(shown)} of {len(outstanding.questions)} open questions."
            )
        lines.append("  Answer with: fno inbox outstanding clear <id> --answer \"...\"")
        lines.append("")

    if outstanding.captures:
        total = len(outstanding.captures)
        lines.append(f"{_plural(total, 'capture')} awaiting triage (fu- items).")
        shown_captures = outstanding.captures[:RENDER_CAP]
        for c in shown_captures:
            where = f"  ({c.project})" if c.project != Path.cwd().name else ""
            lines.append(f"  {c.fu_id}  {c.title}{where}")
        # Ruling 2: the cap without the count is a lie by omission in a nicer
        # font - a 200-row pile gets abandoned in a week, but so does a list
        # showing 3 rows while 195 wait, unnamed.
        ages = [c.age_days for c in outstanding.captures if c.age_days is not None]
        summary = f"  Showing {len(shown_captures)} of {total}"
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
