"""The durable decision record: what a human decided, kept findable.

Two halves that must both exist. The ``operator_decision`` event is append-only
and machine-wide, so it survives the compaction that eats the transcript the
decision was stated in. The projection onto the subject node's graph entry is
what makes the record findable by subject rather than merely greppable, and it
inherits the archive read-through (``entries_with_archive``) so a decision made
about a node in June still answers in September after the node archives.
"""
from __future__ import annotations

import secrets
from typing import Any

PROJECTION_FIELDS = (
    "decision_id",
    "decision",
    "subject",
    "question",
    "asked_by",
    "asked_at",
    "options",
    "decided_by",
    "authority_source",
    "rationale",
    "supersedes",
    "question_id",
)


def mint_decision_id() -> str:
    """A stable handle in the q-/fu- family: d-<hex>."""
    return f"d-{secrets.token_hex(4)}"


def record_decision(
    *,
    decision: str,
    subject: str | None = None,
    decided_by: str = "operator",
    authority_source: str = "operator",
    rationale: str | None = None,
    options: "list[str] | None" = None,
    supersedes: str | None = None,
    question_id: str | None = None,
    question: str | None = None,
    asked_by: str | None = None,
    asked_at: str | None = None,
    events_root: Any = None,
) -> dict[str, Any]:
    """Append the event, then project it onto the subject node.

    Returns ``{"decision_id", "event", "node_id"}`` where ``node_id`` is None
    when the subject names no graph node (a file or an area): the durable event
    still lands, because a record that only exists when the subject resolves is
    a record the operator cannot rely on.
    """
    from fno.events import append_event, operator_decision
    from fno.outstanding.core import events_path

    if events_root is None:
        from fno.carveout.core import resolve_carveout_root

        events_root = resolve_carveout_root()

    decision_id = mint_decision_id()
    event = operator_decision(
        decision_id=decision_id,
        decision=decision,
        subject=subject,
        question_id=question_id,
        question=question,
        asked_by=asked_by,
        asked_at=asked_at,
        options=options,
        decided_by=decided_by,
        authority_source=authority_source,
        rationale=rationale,
        supersedes=supersedes,
    )
    append_event(event, events_path=events_path(events_root))
    node_id = _project(event)
    return {"decision_id": decision_id, "event": event, "node_id": node_id}


def _project(event: dict[str, Any]) -> str | None:
    """Write the decision onto the subject node's ``decisions`` list.

    Runs inside the locked mutate cycle, with the subject resolved under the
    lock, so two concurrent decides on one node serialize. Supersession marks
    the older row (``superseded_by``) rather than removing it: a reader of an
    overturned decision must be able to tell it is not current.
    """
    from fno.graph import store as graph_store
    from fno.graph.fuzzy import resolve_node

    data = event["data"]
    subject = data.get("subject")
    if not subject:
        return None

    # Pre-check on the unlocked read so an unresolvable subject (a file, an
    # area) does not pay for a full graph rewrite that changes nothing. The
    # path is passed EXPLICITLY: read_graph's default arg froze at import, so
    # a bare read_graph() can read a different graph than the one the locked
    # mutate below writes (the hermetic-test HOME redirect trips exactly this).
    if resolve_node(subject, graph_store.read_graph(graph_store.GRAPH_JSON)).kind != "exact":
        return None

    matched: list[str] = []

    def mutator(entries: list[dict]) -> list[dict]:
        match = resolve_node(subject, entries)
        if match.kind != "exact" or not match.id:
            return entries
        for e in entries:
            if not isinstance(e, dict) or e.get("id") != match.id:
                continue
            record = {k: data[k] for k in PROJECTION_FIELDS if data.get(k) is not None}
            record["ts"] = event.get("ts")
            record["superseded_by"] = None
            sup = data.get("supersedes")
            if sup:
                for d in e.setdefault("decisions", []):
                    if isinstance(d, dict) and d.get("decision_id") == sup:
                        d["superseded_by"] = data["decision_id"]
            e.setdefault("decisions", []).append(record)
            matched.append(match.id)
            break
        return entries

    graph_store.locked_mutate_graph(graph_store.GRAPH_JSON, mutator)
    return matched[0] if matched else None


def list_decisions(subject: str) -> "tuple[str, list[dict]]":
    """The subject node's decision history, newest first.

    Reads through ``entries_with_archive`` so an archived subject still
    resolves; raises ``LookupError`` when the subject names nothing.
    """
    from fno.graph.fuzzy import resolve_node
    from fno.graph.store import entries_with_archive, read_graph

    entries = entries_with_archive(read_graph(_graph_json_path()))
    match = resolve_node(subject, entries)
    if match.kind != "exact" or not match.id:
        raise LookupError(f"no node matches '{subject}'")
    entry = match.candidates[0]
    decisions = [d for d in (entry.get("decisions") or []) if isinstance(d, dict)]
    decisions.sort(key=lambda d: str(d.get("ts") or ""), reverse=True)
    return match.id, decisions


def _graph_json_path():
    from fno.graph import store as graph_store

    return graph_store.GRAPH_JSON
