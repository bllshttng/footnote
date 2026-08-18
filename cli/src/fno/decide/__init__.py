"""The durable decision record: what a human decided, kept findable.

Three stores, one write. The ``operator_decision`` event lands in the project
journal, which is durable, GC-exempt and never rotated: that is the record.
The same event lands in ``~/.fno/decisions.jsonl``, the machine-wide index:
that is recall, and it is the reader's ONLY source, so a decision about a PR in
one repo answers from another. The projection onto the subject node's graph
entry is the node view, a convenience for anyone reading the node.

The reader takes every subject the writer takes. It used to resolve the subject
as a graph node first and refuse anything else, while the writer accepted free
text and printed an id - so a ruling about ``pr-923`` was written, receipted,
and unreadable by the verb that promised to recover it.
"""
from __future__ import annotations

import json
import secrets
import sys
from pathlib import Path
from typing import Any

DECISION_EVENT = "operator_decision"

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

    An index write that fails is not a success. The exception propagates to
    ``decide/cli.py``, which prints ``decide: failed to record`` and exits 1 - a
    write the operator cannot read back is worse than a refusal.
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
    # Order is the contract: the project journal is durability, the index is
    # recall, the graph projection is the node view.
    append_event(event, events_path=_index_path())
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


def _index_path() -> Path:
    """The machine-wide decision index. Read through ``fno.paths`` every call
    so a redirected state dir (a hermetic test, a ``state_dir`` override) is
    the file both the writer and the reader see."""
    from fno import paths

    return paths.decisions_jsonl()


def _read_index(path: Path) -> "list[dict]":
    """Flatten the index into decision rows, in file order.

    A MISSING index reads as zero decisions - the common case before the first
    write. An index that exists and cannot be read raises: an unreadable store
    answering "no decisions" is the absence-as-success failure this verb exists
    to prevent.
    """
    if not path.exists():
        return []

    rows: "list[dict]" = []
    damaged = 0
    with path.open(encoding="utf-8") as fh:
        for line in fh:
            # Substring prefilter before json.loads, the same shape as
            # outstanding/core.py: this file only ever holds decisions today,
            # but the check costs nothing and keeps a foreign line harmless.
            if DECISION_EVENT not in line:
                continue
            try:
                rec = json.loads(line)
            except (json.JSONDecodeError, ValueError):
                damaged += 1
                continue
            if not isinstance(rec, dict) or rec.get("type") != DECISION_EVENT:
                continue
            data = rec.get("data")
            if not isinstance(data, dict) or not data.get("decision_id"):
                damaged += 1
                continue
            row = dict(data)
            row["ts"] = rec.get("ts")
            rows.append(row)

    if damaged:
        # One bad row must not cost the others, so the row is skipped. But it
        # is never skipped SILENTLY: a truncated append would otherwise make an
        # unreadable record and an empty one look the same, which is the
        # absence-as-success failure this index exists to prevent.
        print(
            f"decide: {damaged} damaged row(s) in {path} were skipped; "
            f"run `fno decide reindex` to recover them from the journals.",
            file=sys.stderr,
        )
    return rows


def _subject_keys(subject: str) -> "set[str]":
    """The strings a query for ``subject`` matches, exactly.

    A subject that resolves to a node also answers to its id and slug, because
    the operator may have recorded under any of the three. Everything else
    matches itself and nothing more: a decision on ``pr-92`` must not answer a
    query for ``pr-921``, so this is set membership, never a fuzzy match.
    """
    keys = {subject}
    try:
        from fno.graph import store as graph_store
        from fno.graph.fuzzy import resolve_node

        entries = graph_store.entries_with_archive(
            graph_store.read_graph(graph_store.GRAPH_JSON)
        )
        match = resolve_node(subject, entries)
    except Exception:  # noqa: BLE001 - the graph is advisory to a string query
        return keys
    if match.kind == "exact" and match.id:
        keys.add(str(match.id))
        candidate = match.candidates[0] if match.candidates else None
        if isinstance(candidate, dict) and candidate.get("slug"):
            keys.add(str(candidate["slug"]))
    return keys


def list_decisions(
    subject: str | None = None, limit: int | None = None
) -> "tuple[str, list[dict]]":
    """Decision history from the index, newest first. Never raises LookupError.

    ``subject=None`` returns every decision, which is the only way to reach a
    record written with no subject at all - what ``fno outstanding clear
    --answer`` writes for a question that names no node.
    """
    rows = _read_index(_index_path())

    # The graph projection stamped superseded_by at write time under the lock.
    # The index cannot (it is append-only), so the reader derives it, across
    # the whole scanned set rather than the filtered one.
    superseded_by: "dict[str, str]" = {}
    for row in rows:
        target = row.get("supersedes")
        if target:
            superseded_by[str(target)] = str(row.get("decision_id"))

    keys = _subject_keys(subject) if subject else None
    out: "list[dict]" = []
    for row in rows:
        if keys is not None and str(row.get("subject") or "") not in keys:
            continue
        row = dict(row)
        row["superseded_by"] = superseded_by.get(str(row.get("decision_id")))
        out.append(row)

    out.sort(key=lambda d: str(d.get("ts") or ""), reverse=True)
    if limit and limit > 0:
        out = out[:limit]
    return subject or "(all)", out


def _projection_events() -> "list[dict]":
    """Every decision projected onto a graph node, as event envelopes.

    The graph is machine-wide, so this reaches decisions recorded from any
    project - the half of the backfill a per-journal fold cannot see.
    """
    from fno.events import operator_decision
    from fno.graph import store as graph_store

    events: "list[dict]" = []
    # Path passed EXPLICITLY: read_graph's default argument froze at import.
    entries = graph_store.entries_with_archive(
        graph_store.read_graph(graph_store.GRAPH_JSON)
    )
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        for row in entry.get("decisions") or []:
            if not isinstance(row, dict):
                continue
            if not row.get("decision_id") or not row.get("decision"):
                continue
            kwargs = {k: row[k] for k in PROJECTION_FIELDS if row.get(k) is not None}
            # An early projection row stored no subject (it predates the field).
            # The row lives ON the node, so the node IS the subject; without
            # this the recovered decision answers no query at all.
            kwargs.setdefault("subject", entry.get("id"))
            event = operator_decision(**kwargs)
            if row.get("ts"):
                event["ts"] = row["ts"]
            events.append(event)
    return events


def _default_journals() -> "list[Path]":
    """Every journal on this machine that can hold a decision, deduped by inode.

    EVERY project root, not just this one. A free-text decision recorded from
    another repo has no graph projection to recover it, so a backfill that
    folds only the invoking repo leaves exactly the records this verb exists to
    find. The per-read fold of all 83 roots is what LD1 rejected as too slow;
    paying it once, in an explicit backfill, is a different bargain.

    Deduping matters more here than the enumeration. A linked checkout symlinks
    ``.fno/events.jsonl`` at the canonical file, so one 54 MB journal is
    reachable under several names, and reading it once per name is the
    difference between a slow command and a hung one.
    """
    from fno.carveout.core import resolve_carveout_root
    from fno.outstanding.core import _capture_project_roots, events_path
    from fno.paths import global_events_json, resolve_repo_root

    def _machine_wide() -> "list[Path]":
        # Reused rather than re-derived: this fact (the roots the machine-wide
        # graph names) has one owner, and a second copy is a second thing to
        # keep in parity.
        return [events_path(r) for r in _capture_project_roots(resolve_repo_root())]

    candidates: "list[Path]" = []
    for produce in (
        _machine_wide,
        lambda: [events_path(resolve_carveout_root())],
        lambda: [global_events_json()],
    ):
        try:
            candidates.extend(Path(p) for p in produce())
        except Exception:  # noqa: BLE001 - a root that will not resolve has no journal
            continue

    seen: "set[tuple[int, int]]" = set()
    out: "list[Path]" = []
    for path in candidates:
        try:
            stat = path.stat()
        except OSError:
            continue
        key = (stat.st_dev, stat.st_ino)
        if key in seen:
            continue
        seen.add(key)
        out.append(path)
    return out


def _journal_events(paths: "list[Path]") -> "list[dict]":
    events: "list[dict]" = []
    for path in paths:
        try:
            fh = path.open(encoding="utf-8")
        except OSError:
            continue
        with fh:
            for line in fh:
                if DECISION_EVENT not in line:
                    continue
                try:
                    rec = json.loads(line)
                except (json.JSONDecodeError, ValueError):
                    continue
                if not isinstance(rec, dict) or rec.get("type") != DECISION_EVENT:
                    continue
                data = rec.get("data")
                if isinstance(data, dict) and data.get("decision_id"):
                    events.append(rec)
    return events


def reindex(sources: "list[Path] | None" = None) -> "dict[str, int]":
    """Make the index a superset of every decision already on this machine.

    Without this the fix helps no record that already exists. Idempotent by
    ``decision_id``, so a second run adds nothing.

    Journals are folded BEFORE projections and win a tie: the journal holds the
    event as written, while a projection row is derived and can be lossier -
    the oldest one on this machine dropped ``subject``, which is the one field
    a recall query reads. Projections still run, because they are machine-wide
    and reach decisions no journal here can see.
    """
    from fno.events import append_event

    index = _index_path()
    known = {str(row["decision_id"]) for row in _read_index(index)}
    already = 0
    invalid = 0
    added = 0

    journals = _journal_events(
        list(sources) if sources is not None else _default_journals()
    )
    for event in journals + _projection_events():
        did = str(event["data"].get("decision_id") or "")
        if not did:
            continue
        if did in known:
            already += 1
            continue
        try:
            append_event(event, events_path=index)
        except Exception:  # noqa: BLE001 - one unusable row must not cost the rest
            invalid += 1
            continue
        known.add(did)
        added += 1

    return {"added": added, "already": already, "invalid": invalid, "total": len(known)}
