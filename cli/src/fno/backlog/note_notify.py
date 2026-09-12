"""Deliver a `fno backlog note` to the people bound to the node.

A worker reads its node once, at dispatch, so a note appended after that reaches
nobody on its own. Resolution runs BEFORE the append: nobody bound, or a fault,
refuses and writes nothing - a note no reader would hear is a silent drop wearing
a receipt. Contract in docs/architecture/backlog-graph-verb-contracts.md. It
lives beside ``advance`` because it reads the graph AND the agent runtime (claim,
crown, send); ``fno.backlog`` holds that pair, the core layer may not import it.
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
    """Mail one pointer (short lock timeout: a contended recipient takes the
    durable envelope now). The sender is this session's own handle, not a
    literal: provenance is looked up by from_name."""
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


def _is_author(row: Any, resolved: str, address: str, self_session: str) -> bool:
    """A row-backed address decides by identity key; endswith stands only when
    no row is behind the name."""
    from fno.harness_identity import session_identity_key

    if row is None:
        return resolved.endswith(self_session) or address.endswith(self_session)
    row_sid = getattr(row, "harness_session_id", None)
    if not isinstance(row_sid, str) or not row_sid:
        return False
    return session_identity_key(row_sid) == session_identity_key(self_session)


def note_readers(
    entry: dict,
    *,
    index: dict[str, dict],
    rows: Optional[Iterable[Any]] = None,
    holder_of: Callable[[str], Optional[str]] = claim_holder,
    kings_of: Callable[[str], Iterable[str]] = crowned_over,
    self_session: Optional[str] = None,
) -> NoteReaders:
    """Every bound reader for one note; the author is named, never mailed.

    The worker chain runs for the node and again for its owner, first winning
    arm per run; the crown walk goes outward, first live crown wins. ``rows``
    is the caller's registry read; ``None`` reads the machine's once (tests
    always pass ``rows``, so no test reads this machine's registry).
    """
    from fno.agents.registry import live_row_holding_session_id, load_registry
    from fno.claims.core import holder_agent_name
    from fno.harness_identity import OWNERSHIP_LIVE_STATUSES

    registry_rows: list[Any] = list(rows) if rows is not None else list(load_registry())
    node_id = str(entry.get("id") or "")
    recipients: list[tuple[str, str]] = []
    readings: list[str] = []
    author_bound: Optional[str] = None
    seen: set[str] = set()

    def add(address: Optional[str], why: str) -> bool:
        """Bind one address; True only when a live RECIPIENT joined - an author
        hit names itself in author_bound and never stops the chain."""
        nonlocal author_bound
        if not address or address in seen:
            return False
        # A role holder is a marker, not an address; None: nobody behind it.
        resolved = holder_agent_name(address, registry_rows)
        if not resolved or resolved in seen:
            return False
        row = next((r for r in registry_rows if r.name == resolved), None)
        if self_session and _is_author(row, resolved, address, self_session):
            author_bound = author_bound or why
            return False
        seen.add(resolved)
        recipients.append((resolved, why))
        return True

    def bound_row(value: str) -> Optional[Any]:
        """The ownership-live row behind a graph binding: the row named by the
        resolved holder, else the row owning the value's session identity."""
        name = holder_agent_name(value, registry_rows)
        return next(
            (
                r
                for r in registry_rows
                if getattr(r, "status", None) in OWNERSHIP_LIVE_STATUSES and r.name == name
            ),
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
            if row is None:
                readings.append(f"graph {field}: {value} names no live row")
                continue
            readings.append(f"graph {field}: {value} -> {row.name}")
            if add(row.name, f"session bound to {subject} (graph {field})"):
                return
        named = sorted(
            (
                r
                for r in registry_rows
                if getattr(r, "node", None) == subject_id
                and getattr(r, "status", None) in OWNERSHIP_LIVE_STATUSES
            ),
            key=lambda r: r.name,
        )
        if named:
            readings.append("registry: " + ", ".join(r.name for r in named))
            for r in named:
                if add(r.name, f"worker on {subject} (registry node)"):
                    return
        else:
            readings.append(f"registry: no live row names {subject_id}")

    contained_in = entry.get("contained_in")
    owner = index.get(contained_in) if isinstance(contained_in, str) else None
    worker_readers(node_id, node_id, entry)
    owner_id = contained_in or entry.get("parent")
    if isinstance(owner_id, str) and owner_id:
        worker_readers(owner_id, f"owner {owner_id}", index.get(owner_id))

    # Crown walk, nearest first: this epic when type==epic, then the epic (the
    # owner's parent for a contained node), then the project.
    epic = (owner.get("parent") if owner else None) or entry.get("parent")
    project = entry.get("project")
    scopes: list[tuple[str, str, str]] = []
    if entry.get("type") == "epic":
        scopes.append((node_id, f"king of {node_id}", f"crown {node_id}"))
    if isinstance(epic, str) and epic:
        scopes.append((epic, f"king of {epic}", f"crown {epic}"))
    if isinstance(project, str) and project:
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


def readers_before_append(task_id: str, graph_path: Path) -> NoteReaders | Refused:
    """Resolve the readers BEFORE the append; a Refused must be surfaced. A
    fault refuses for the reason a vacancy does: neither proves anyone would
    be told."""
    from fno.agents.registry import load_registry
    from fno.graph._intake import _find_node
    from fno.graph.store import read_graph

    try:
        rows = read_graph(graph_path)
        entry = _find_node(rows, task_id)
        if entry is None:
            return Refused(f"Error: no node resolves to '{task_id}'", 1)
        index = {str(e.get("id")): e for e in rows if isinstance(e.get("id"), str)}
        readers = note_readers(
            entry,
            index=index,
            rows=load_registry(),
            holder_of=claim_holder,
            kings_of=crowned_over,
            self_session=own_session(),
        )
    except Exception as exc:  # noqa: BLE001 - cannot prove a reader, so refuse
        return Refused(
            f"note refused: could not read who is bound to {task_id} ({exc}), "
            f"so nothing was written.\n{_QUIET_HINT}",
            3,
        )
    if not readers.recipients and not readers.author_bound:
        trail = "".join(f"\n  {reading}" for reading in readers.readings)
        return Refused(
            f"note refused: nobody bound to {readers.node_id} would be told, "
            f"so nothing was written.{trail}\n{_QUIET_HINT}",
            3,
        )
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
        f"({unconfirmed} UNCONFIRMED, {failed} FAILED). An UNCONFIRMED send may "
        "still land, so check before you re-send.",
        err=True,
    )
    return 4


def _one_receipt(address: str, why: str, body: str) -> str:
    state, value = _bounded_send(address, body)
    if state == "ok":
        return f"notified {address} ({why}): {value}"
    if state == "timeout":
        return (
            f"notify UNCONFIRMED {address} ({why}): no answer in "
            f"{_SEND_TIMEOUT_SECONDS:.0f}s"
        )
    return f"notify FAILED {address} ({why}): {value}"


def _bounded_send(address: str, body: str) -> tuple[str, Any]:
    """One send, bounded by a wall clock: ``(ok|err|timeout, value)``. A live
    inject waits on the recipient's flock; one run wedged past 150s, so the
    daemon thread dies with the process and the OS drops that lock."""
    out: list[tuple[str, Any]] = []

    def run() -> None:
        try:
            out.append(("ok", send_pointer(address, body)))
        except Exception as exc:  # noqa: BLE001 - one bad address is not the rest
            out.append(("err", exc))

    worker = threading.Thread(target=run, daemon=True)
    worker.start()
    worker.join(_SEND_TIMEOUT_SECONDS)
    return out[0] if out else ("timeout", None)
