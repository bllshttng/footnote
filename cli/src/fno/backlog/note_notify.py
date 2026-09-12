"""Deliver a `fno backlog note` to the people bound to the node.

Resolution runs BEFORE the append: nobody bound, or a fault, refuses and writes
nothing. Contract: docs/architecture/backlog-graph-verb-contracts.md.
"""
from __future__ import annotations

import threading
from pathlib import Path
from typing import Any, Callable, Iterable, NamedTuple, Optional

import typer

_POINTER_WORDS = 20
_SEND_TIMEOUT_SECONDS = 30.0
_UNDELIVERED = ("notify FAILED", "notify UNCONFIRMED")
_GRAPH_FIELDS = ("locked_by_harness_session", "session_id", "locked_by")
_QUIET_HINT = (
    "Write it anyway with --quiet, or find a reader with fno agents court and mail them by name."
)


class NoteReaders(NamedTuple):
    node_id: str
    recipients: list[tuple[str, str]]
    author_bound: Optional[str]
    readings: list[str]


class Refused(NamedTuple):
    message: str
    exit_code: int


def claim_holder(node_id: str) -> Optional[str]:
    """The session holding ``node:<id>``. ``suspect`` is still owned (x-ba4b)."""
    from fno.claims.core import claim_status

    try:
        status = claim_status(f"node:{node_id}")
    except Exception:  # noqa: BLE001 - an unreadable claim is one absent recipient
        return None
    holder = status.get("holder")
    if status.get("state") not in ("live", "suspect") or not isinstance(holder, str):
        return None
    return holder or None


def crowned_over(scope: str) -> list[str]:
    from fno.agents.crown import resolve_to_king

    return resolve_to_king(scope)


def own_session() -> Optional[str]:
    from fno.claims.self_identity import resolve_self_identity

    try:
        session_id = getattr(resolve_self_identity(), "session_id", None)
    except Exception:  # noqa: BLE001 - an unprovable identity skips the self-drop
        return None
    return session_id if isinstance(session_id, str) and session_id else None


def send_pointer(address: str, body: str) -> str:
    """Mail one pointer; the sender handle is this session's own (provenance by from_name)."""
    from fno.agents.dispatch import dispatch_send
    from fno.harness_identity import canonical_handle

    session = own_session()
    from_name = canonical_handle(session) if session else "fno"
    result = dispatch_send(
        address, body, None, cwd=Path.cwd(), from_name=from_name, lock_timeout=5.0
    )
    return f"{result.delivery} {result.msg_id}"


def pointer(node_id: str, text: str) -> str:
    """One line naming the node and the note's opening, never the note itself."""
    first_line = next((ln.strip() for ln in text.splitlines() if ln.strip()), "")
    words = first_line.split()
    head = " ".join(words[:_POINTER_WORDS])
    if len(words) > _POINTER_WORDS:
        head += "..."
    return f"note on {node_id}: {head} Read: fno backlog get {node_id}"


def note_readers(
    entry: dict,
    *,
    index: dict[str, dict],
    rows: Optional[Iterable[Any]] = None,
    holder_of: Callable[[str], Optional[str]] = claim_holder,
    kings_of: Callable[[str], Iterable[str]] = crowned_over,
    self_session: Optional[str] = None,
) -> NoteReaders:
    """Every bound reader for one note; the author is named, never mailed."""
    from fno.agents.registry import live_row_holding_session_id, load_registry
    from fno.claims.core import holder_agent_name
    from fno.harness_identity import OWNERSHIP_LIVE_STATUSES, session_identity_key

    registry_rows: list[Any] = list(rows) if rows is not None else list(load_registry())
    node_id = str(entry.get("id") or "")
    recipients: list[tuple[str, str]] = []
    readings: list[str] = []
    author_bound: Optional[str] = None
    seen: set[str] = set()
    self_key = session_identity_key(self_session) if self_session else None
    self_sid = self_session or ""

    def add(address: Optional[str], why: str) -> bool:
        """True only when a live RECIPIENT joined; an author hit is never mailed."""
        nonlocal author_bound
        if not address or address in seen:
            return False
        # A role holder is a marker, not an address; None: nobody behind it.
        resolved = holder_agent_name(address, registry_rows)
        if not resolved or resolved in seen:
            return False
        row = next((r for r in registry_rows if r.name == resolved), None)
        if self_key is not None:
            sid = getattr(row, "harness_session_id", None) if row else None
            # A row-backed address decides by identity key; endswith stands
            # only when no row is behind the name.
            if isinstance(sid, str) and sid:
                author = session_identity_key(sid) == self_key
            else:
                author = resolved.endswith(self_sid) or address.endswith(self_sid)
            if author:
                author_bound = author_bound or why
                return False
        seen.add(resolved)
        recipients.append((resolved, why))
        return True

    def bound_row(value: str) -> Optional[Any]:
        """The ownership-live row behind a graph binding, by name or identity."""
        name = holder_agent_name(value, registry_rows)
        return next(
            (r for r in registry_rows
             if getattr(r, "status", None) in OWNERSHIP_LIVE_STATUSES and r.name == name),
            None,
        ) or live_row_holding_session_id(value, rows=registry_rows)

    def worker_readers(subject_id: str, subject: str, source: Optional[dict]) -> None:
        holder = holder_of(subject_id)
        readings.append(f"claim node:{subject_id}: {holder or 'free'}")
        if holder and add(holder, f"holder of {subject}"):
            return
        for field in _GRAPH_FIELDS:
            value = (source or {}).get(field)
            if not isinstance(value, str) or not value:
                readings.append(f"graph {field}: none")
                continue
            row = bound_row(value)
            suffix = f"-> {row.name}" if row else "names no live row"
            readings.append(f"graph {field}: {value} {suffix}")
            if row and add(row.name, f"session bound to {subject} (graph {field})"):
                return
        named = sorted(
            (r for r in registry_rows if getattr(r, "node", None) == subject_id
             and getattr(r, "status", None) in OWNERSHIP_LIVE_STATUSES),
            key=lambda r: r.name,
        )
        readings.append(
            "registry: "
            + (", ".join(r.name for r in named) or f"no live row names {subject_id}")
        )
        for r in named:
            add(r.name, f"worker on {subject} (registry node)")

    contained_in = entry.get("contained_in")
    owner = index.get(contained_in) if isinstance(contained_in, str) else None
    worker_readers(node_id, node_id, entry)
    owner_id = contained_in or entry.get("parent")
    if isinstance(owner_id, str) and owner_id:
        worker_readers(owner_id, f"owner {owner_id}", index.get(owner_id))

    # Crown walk, nearest first; the walk stops at the first scope with a live crown.
    epic = (owner.get("parent") if owner else None) or entry.get("parent")
    scopes: list[tuple[str, str, str]] = []
    if entry.get("type") == "epic":
        scopes.append((node_id, f"king of {node_id}", f"crown {node_id}"))
    if isinstance(epic, str) and epic:
        scopes.append((epic, f"king of {epic}", f"crown {epic}"))
    if isinstance(project := entry.get("project"), str) and project:
        scopes.append((project, f"king of {project} (project)", f"crown {project} (project)"))
    for scope, why, label in scopes:
        try:
            kings = list(kings_of(scope))
        except Exception as exc:  # noqa: BLE001 - one unreadable scope costs it
            readings.append(f"{label}: unreadable ({exc})")
            continue
        if not kings:
            readings.append(f"{label}: vacant")
            continue
        readings.append(label + ": " + ", ".join(kings))
        for king in kings:
            add(king, why)
        break
    return NoteReaders(node_id, recipients, author_bound, readings)


def _refused(head: str, readings: Iterable[str] = ()) -> Refused:
    trail = "".join(f"\n  {reading}" for reading in readings)
    return Refused(f"{head}so nothing was written.{trail}\n{_QUIET_HINT}", 3)


def readers_before_append(task_id: str, graph_path: Path) -> NoteReaders | Refused:
    """Resolve the readers BEFORE the append; surface any Refused verbatim."""
    from fno.agents.registry import load_registry
    from fno.graph._intake import _find_node
    from fno.graph.store import read_graph

    try:
        rows = read_graph(graph_path)
        entry = _find_node(rows, task_id) or next(  # the write path takes slugs too
            (e for e in rows if str(e.get("slug") or "").lower() == task_id.strip().lower()),
            None,
        )
        if entry is None:
            return Refused(f"Error: no node resolves to '{task_id}'", 1)
        index = {str(e.get("id")): e for e in rows if isinstance(e.get("id"), str)}
        readers = note_readers(
            entry, index=index, rows=load_registry(), holder_of=claim_holder,
            kings_of=crowned_over, self_session=own_session(),
        )
    except Exception as exc:  # noqa: BLE001 - cannot prove a reader, so refuse
        return _refused(f"note refused: could not read who is bound to {task_id} ({exc}), ")
    if not readers.recipients and not readers.author_bound:
        return _refused(f"note refused: nobody bound to {readers.node_id} would be told, ", readers.readings)
    return readers


def send_note(readers: NoteReaders, text: str) -> list[tuple[str, bool]]:
    """Receipt lines for one delivery, each flagged when it is not a delivery."""
    body = pointer(readers.node_id, text)
    lines = [_one_receipt(address, why, body) for address, why in readers.recipients]
    return [(line, line.startswith(_UNDELIVERED)) for line in lines]


def deliver(readers: NoteReaders, text: str, *, json_output: bool) -> int:
    """Send to every bound reader; the exit code reports confirmed delivery."""
    if not readers.recipients:
        typer.echo(
            f"notify: you are the only reader bound to {readers.node_id} "
            f"({readers.author_bound}); nobody else to tell",
            err=json_output,
        )
        return 0
    receipts = send_note(readers, text)
    for line, undelivered in receipts:
        typer.echo(line, err=undelivered or json_output)
    if any(line.startswith("notified ") for line, _ in receipts):
        return 0
    unconfirmed = sum(line.startswith("notify UNCONFIRMED") for line, _ in receipts)
    failed = sum(line.startswith("notify FAILED") for line, _ in receipts)
    typer.echo(
        f"notify: {readers.node_id} is noted, but no reader confirmed delivery "
        f"({unconfirmed} UNCONFIRMED, {failed} FAILED). An UNCONFIRMED send may still "
        "land, so check before you re-send.",
        err=True,
    )
    return 4


def _one_receipt(address: str, why: str, body: str) -> str:
    """One receipt line from a wall-clock-bounded send (a live inject waits on
    the recipient's flock; the daemon thread dies with the process)."""
    out: list[tuple[str, Any]] = []

    def run() -> None:
        try:
            out.append(("ok", send_pointer(address, body)))
        except Exception as exc:  # noqa: BLE001 - one bad address is not the rest
            out.append(("err", exc))

    worker = threading.Thread(target=run, daemon=True)
    worker.start()
    worker.join(_SEND_TIMEOUT_SECONDS)
    state, value = out[0] if out else ("timeout", None)
    if state == "ok":
        return f"notified {address} ({why}): {value}"
    if state == "timeout":
        return f"notify UNCONFIRMED {address} ({why}): no answer in {_SEND_TIMEOUT_SECONDS:.0f}s"
    return f"notify FAILED {address} ({why}): {value}"
