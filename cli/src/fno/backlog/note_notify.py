"""Deliver a `fno backlog note` to the people building the node.

A worker reads its node once, at dispatch, so a note appended after that reaches
nobody on its own. Nothing here raises: the note is already written, so a fault
becomes a printed receipt. Contract, and every "why", in
docs/architecture/backlog-graph-verb-contracts.md.

It lives beside ``advance`` rather than under ``fno.graph`` because it reads the
graph AND reaches the agent runtime for the claim, the crown and the send. That
pair is what ``fno.backlog`` already holds; the core layer may not import it.
"""
from __future__ import annotations

import threading
from pathlib import Path
from typing import Any, Callable, Iterable, Optional

_POINTER_WORDS = 20
_SEND_TIMEOUT_SECONDS = 30.0
_UNDELIVERED = ("notify FAILED", "notify UNCONFIRMED")


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
    """Mail one pointer. Short lock timeout: a contended recipient takes the
    durable envelope now rather than blocking the writer.

    The sender is this session's own handle, not a literal: provenance is
    looked up by from_name, so an unregistered literal ships harness=unknown
    with no from_session. No ambient identity keeps the default; the
    dispatch-side miss is loud."""
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


def note_recipients(
    entry: dict,
    *,
    index: dict[str, dict],
    holder_of: Callable[[str], Optional[str]] = claim_holder,
    kings_of: Callable[[str], Iterable[str]] = crowned_over,
    self_session: Optional[str] = None,
) -> list[tuple[str, str]]:
    """Ordered, de-duplicated ``(address, why)`` pairs for one note."""
    from fno.claims.core import holder_agent_name

    node_id = str(entry.get("id") or "")
    out: list[tuple[str, str]] = []
    seen: set[str] = set()

    def add(address: Optional[str], why: str) -> None:
        if not address or address in seen:
            return
        # A role holder is a marker, not an address; resolve it where the
        # address is born. None means nobody is behind it: skip, not fail.
        address, raw = holder_agent_name(address), address
        if not address or address in seen:
            return
        if self_session and (address.endswith(self_session) or raw.endswith(self_session)):
            return
        seen.add(address)
        out.append((address, why))

    add(holder_of(node_id), f"holder of {node_id}")

    contained_in = entry.get("contained_in")
    owner_id = contained_in or entry.get("parent")
    if isinstance(owner_id, str) and owner_id:
        add(holder_of(owner_id), f"holder of owner {owner_id}")

    # The crown sits on the epic: this node's own parent, or the owner's parent
    # when another node's PR carries this one.
    owner = index.get(contained_in) if isinstance(contained_in, str) else None
    scope = (owner.get("parent") if owner else None) or entry.get("parent")
    if isinstance(scope, str) and scope:
        try:
            kings = list(kings_of(scope))
        except Exception:  # noqa: BLE001 - a vacant or unreadable crown is not a failure
            kings = []
        for king in kings:
            add(king, f"king of {scope}")
    return out


def deliver_note(
    node_id: str,
    text: str,
    graph_path: Path,
    entries: Optional[list[dict]] = None,
) -> list[tuple[str, bool]]:
    """Receipt lines for one delivery, each flagged when it is not a delivery.

    Every outcome returns a line, "nobody to reach" included: silence reads the
    same as delivery, which is the defect this closes.
    """
    from fno.graph._intake import _find_node
    from fno.graph.store import read_graph

    try:
        # The write already read the graph; a second read measured 19.2s here.
        rows = read_graph(graph_path) if entries is None else entries
        entry = _find_node(rows, node_id)
        if entry is None:
            return [(f"notify FAILED {node_id}: no node resolves to it", True)]
        recipients = note_recipients(
            entry,
            index={str(e.get("id")): e for e in rows if isinstance(e.get("id"), str)},
            holder_of=claim_holder,
            kings_of=crowned_over,
            self_session=own_session(),
        )
        if not recipients:
            return [(f"notify: no holder, owner or king to reach for {node_id}", False)]
        body = pointer(str(entry.get("id") or node_id), text)
        lines = [_one_receipt(address, why, body) for address, why in recipients]
    except Exception as exc:  # noqa: BLE001 - the note is already written
        return [(f"notify FAILED {node_id}: {exc}", True)]
    return [(line, line.startswith(_UNDELIVERED)) for line in lines]


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
    """One send, bounded by a wall clock: ``(ok|err|timeout, value)``.

    A live inject waits on the recipient's flock; one run wedged past 150s. The
    thread is a daemon, so the process exits without it and the OS drops that lock.
    """
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
