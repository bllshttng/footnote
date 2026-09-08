"""Deliver a `fno backlog note` to the people building the node.

A worker reads its node ONCE, at dispatch. So a note appended after that lands
in a store no consumer re-reads: the write succeeds, the author believes the
finding is delivered, and nothing reports the gap. Hand-relaying by mail failed
one note in seven on 2026-09-08 while the author was actively trying, and it
failed in an order that delivered a retraction of a finding the reader had never
received.

So the verb delivers, and the body is a POINTER, never the note. A full body
spends the 80-word rolling pair budget on the first send.

Nothing here raises into the caller. The note is already written when this runs,
and a delivery fault must degrade to a printed receipt.
"""
from __future__ import annotations

from pathlib import Path
from typing import Any, Callable, Iterable, Optional

# The pointer's ceiling. Well under the 80-word pair budget, because several
# notes in one 10-minute window share that window.
_POINTER_WORDS = 20

# One send's patience. A live inject waits on the recipient's flock (30s default
# plus a grace retry), and a note must not pay that twice over.
# ponytail: fixed bound, make it config if a slow lane needs more.
_SEND_TIMEOUT_SECONDS = 30.0


def _owner_id(entry: dict) -> Optional[str]:
    """The node whose PR carries this work, when it is not this node."""
    for field in ("contained_in", "parent"):
        value = entry.get(field)
        if isinstance(value, str) and value:
            return value
    return None


def _holder(node_id: str, claim_reader: Callable[[str], dict]) -> Optional[str]:
    """The claim holder of ``node_id``, or None.

    ``suspect`` is TTL-unexpired with a dead pid (x-ba4b): still owned, so it
    still gets the mail. A stale or free claim has no reader to reach.
    """
    try:
        status = claim_reader(f"node:{node_id}")
    except Exception:  # noqa: BLE001 - an unreadable claim is one absent recipient
        return None
    if status.get("state") not in ("live", "suspect"):
        return None
    holder = status.get("holder")
    return holder if isinstance(holder, str) and holder else None


def pointer(node_id: str, text: str) -> str:
    """One line naming the node and the note's opening, never the note."""
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
    claim_reader: Callable[[str], dict],
    king_resolver: Callable[[str], Iterable[str]],
    self_session: Optional[str] = None,
) -> list[tuple[str, str]]:
    """Ordered, de-duplicated ``(address, why)`` pairs for one note.

    Three classes, in the order a finding matters: whoever holds this node,
    whoever holds the node whose PR carries it, then whoever is crowned over the
    epic. The caller's own session is dropped - a worker noting on its own node
    must not mail itself.
    """
    node_id = entry.get("id") or ""
    out: list[tuple[str, str]] = []
    seen: set[str] = set()

    def is_self(address: str) -> bool:
        # A claim holder is role-prefixed (`target-session:<id>`) while the
        # identity prover returns the bare session id, so an equality test alone
        # mails the author its own note.
        return bool(self_session) and (
            address == self_session or address.endswith(f":{self_session}")
        )

    def add(address: Optional[str], why: str) -> None:
        if not address or address in seen or is_self(address):
            return
        seen.add(address)
        out.append((address, why))

    add(_holder(node_id, claim_reader), f"holder of {node_id}")

    contained_in = entry.get("contained_in")
    owner_id = _owner_id(entry)
    if owner_id:
        add(_holder(owner_id, claim_reader), f"holder of owner {owner_id}")

    # The crown sits on the EPIC, which is this node's own parent. Only a
    # CONTAINED node looks one level further out, because its parent is the node
    # whose PR carries it. Reading the owner's parent unconditionally would walk
    # past the epic to the grandparent for every ordinary child.
    owner = index.get(contained_in) if isinstance(contained_in, str) else None
    scope = (owner.get("parent") if owner else None) or entry.get("parent")
    if isinstance(scope, str) and scope:
        try:
            kings = list(king_resolver(scope))
        except Exception:  # noqa: BLE001 - a vacant or unreadable crown is not a failure
            kings = []
        for king in kings:
            add(king, f"king of {scope}")

    return out


def notify_note(
    node_id: str,
    text: str,
    *,
    graph_path: Path,
    entries: Optional[list[dict]] = None,
    sender: Optional[Callable[[str, str], str]] = None,
    claim_reader: Optional[Callable[[str], dict]] = None,
    king_resolver: Optional[Callable[[str], Iterable[str]]] = None,
    self_session: Optional[str] = None,
) -> list[str]:
    """Deliver a pointer to every reader of ``node_id``. Returns receipt lines.

    A receipt opening with ``notify FAILED`` is a delivery that did not happen,
    including a budget refusal: reporting it is the whole point, since an
    unreported failure is the defect this module closes.
    """
    from fno.graph._intake import _find_node
    from fno.graph.store import read_graph

    try:
        # The note write already read the graph; reuse that snapshot rather than
        # paying a second full read, which measured 19.2s on a contended keeper.
        if entries is None:
            entries = read_graph(graph_path)
        entry = _find_node(entries, node_id)
    except Exception as exc:  # noqa: BLE001 - the note is already written
        return [f"notify FAILED {node_id} (graph unreadable): {exc}"]
    if entry is None:
        return [f"notify FAILED {node_id} (no such node): nothing to resolve"]

    index = {e.get("id"): e for e in entries if isinstance(e.get("id"), str)}
    recipients = note_recipients(
        entry,
        index=index,
        claim_reader=claim_reader or _default_claim_reader,
        king_resolver=king_resolver or _default_king_resolver,
        self_session=self_session or _self_session(),
    )
    if not recipients:
        return []

    send = sender or _default_sender
    body = pointer(entry.get("id") or node_id, text)
    receipts: list[str] = []
    for address, why in recipients:
        state, value = _bounded_send(send, address, body)
        if state == "ok":
            receipts.append(f"notified {address} ({why}): {value}")
        elif state == "timeout":
            receipts.append(
                f"notify UNCONFIRMED {address} ({why}): no answer in "
                f"{_SEND_TIMEOUT_SECONDS:.0f}s"
            )
        else:
            receipts.append(f"notify FAILED {address} ({why}): {value}")
    return receipts


def _bounded_send(
    send: Callable[[str, str], str], address: str, body: str
) -> tuple[str, Any]:
    """One send, bounded by a wall clock. Returns ``(ok|err|timeout, value)``.

    A live inject waits on the recipient's per-agent flock and can outlast any
    patience a writer has: one measured run wedged past 150s. A note is a write
    verb, so an unanswered recipient must cost seconds and a printed receipt,
    never the turn. The thread is a daemon: the process exits without it, the OS
    drops the flock, and the recipient sees no half-written envelope because the
    mail store writes atomically.
    """
    import threading

    out: list[tuple[str, Any]] = []

    def run() -> None:
        try:
            out.append(("ok", send(address, body)))
        except Exception as exc:  # noqa: BLE001 - one bad address is not the rest
            out.append(("err", exc))

    worker = threading.Thread(target=run, daemon=True)
    worker.start()
    worker.join(_SEND_TIMEOUT_SECONDS)
    return out[0] if out else ("timeout", None)


def _default_claim_reader(key: str) -> dict[str, Any]:
    from fno.claims.core import claim_status

    return claim_status(key)


def _default_king_resolver(scope: str) -> list[str]:
    from fno.agents.crown import resolve_to_king

    return resolve_to_king(scope)


def _self_session() -> Optional[str]:
    try:
        from fno.claims.self_identity import resolve_self_identity

        identity = resolve_self_identity()
    except Exception:  # noqa: BLE001 - an unprovable identity just skips the self-drop
        return None
    session_id = getattr(identity, "session_id", None)
    return session_id if isinstance(session_id, str) and session_id else None


def _default_sender(address: str, body: str) -> str:
    from fno.agents.dispatch import dispatch_send

    # A short lock timeout on purpose: a contended recipient should get the
    # durable envelope now rather than block the writer for 30 seconds.
    result = dispatch_send(
        address, body, None, cwd=Path.cwd(), from_name="fno", lock_timeout=5.0
    )
    return f"{result.delivery} {result.msg_id}"
