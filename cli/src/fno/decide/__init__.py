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

# The deployed writer still labelled agent-authored rulings as ``operator``.
# This safely postdates every row that writer produced during the cutover; the
# reader refuses to fabricate law from those ambiguous append-only records.
AUTHORITY_LANE_CUTOVER = "2026-08-21T00:00:00Z"

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


class IndexWriteError(RuntimeError):
    """The ruling is durable but not yet recoverable.

    Separate from a plain failure because the remedy is the opposite one: the
    project journal already holds the event, so the operator must NOT re-run
    `fno decide` (that mints a second id for one ruling) and must run
    `fno decide reindex` instead.
    """

    def __init__(self, decision_id: str, cause: BaseException) -> None:
        super().__init__(str(cause))
        self.decision_id = decision_id
        self.cause = cause


class RefusedAuthorityError(RuntimeError):
    """An agent session tried to record a ruling as operator law."""

    def __init__(self, agent_handle: str) -> None:
        super().__init__(
            f"agent {agent_handle} cannot record under operator authority"
        )
        self.agent_handle = agent_handle


def mint_decision_id() -> str:
    """A stable handle in the q-/fu- family: d-<hex>."""
    return f"d-{secrets.token_hex(4)}"


def _resolve_decider(
    decided_by: str | None, authority_source: str | None
) -> tuple[str, str]:
    """Resolve omitted provenance from the ambient harness identity."""
    from fno.harness_identity import canonical_handle, resolve_harness_identity

    ident = resolve_harness_identity()
    agent = (
        canonical_handle(ident.session_id)
        if ident.session_id and ident.harness
        else None
    )
    if agent and authority_source == "operator":
        raise RefusedAuthorityError(agent)
    return (
        decided_by or agent or "operator",
        authority_source or ("agent" if agent else "operator"),
    )


def record_decision(
    *,
    decision: str,
    subject: str | None = None,
    decided_by: str | None = None,
    authority_source: str | None = None,
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

    An index write that fails is not a success, so it raises
    :class:`IndexWriteError`. That error carries the decision_id, because by
    then the durable event HAS landed: re-running the command would mint a
    second id for one ruling, and `fno decide reindex` is the recovery.
    """
    from fno.events import append_event, operator_decision
    from fno.outstanding.core import events_path

    decided_by, authority_source = _resolve_decider(decided_by, authority_source)

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
    try:
        append_event(event, events_path=_index_path())
    except Exception as exc:  # noqa: BLE001 - re-raised with what the caller must know
        raise IndexWriteError(decision_id, exc) from exc
    try:
        node_id = _project(event)
    except (Exception, SystemExit) as exc:  # noqa: BLE001
        # The projection is the node VIEW, the third of three writes. Both
        # durable stores already hold the decision, so failing the command here
        # would report a lost capture and invite the retry that mints a second
        # id. SystemExit is caught on purpose: locked_mutate_graph exits the
        # process on a corrupt graph, and SystemExit is not an Exception.
        print(
            f"decide: recorded {decision_id}, but the graph projection failed: "
            f"{exc}. The decision is durable and recoverable with "
            f"`fno decide list`; the subject node just does not show it.",
            file=sys.stderr,
        )
        node_id = None
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
    # decisions list is footnote-minted metadata, read through the guarded
    # metadata reader; an external selection raises and the decision stays
    # events-only (the same outcome as an unresolvable subject - the append
    # above already landed, the node projection is what is unavailable).
    from fno.tracker.metadata import ExternalMetadataUnavailable, read_entries

    try:
        precheck_entries = read_entries("decide")
    except ExternalMetadataUnavailable:
        return None
    if resolve_node(subject, precheck_entries).kind != "exact":
        # read_entries swallows a corrupt default graph to [], which resolves
        # the same as a genuinely unmatched subject. On the default backend,
        # diagnose the corrupt case with _graph_entries()'s own strict-read
        # warning so "no such node" is never printed for "unreadable graph" -
        # the distinction this precheck made before the guarded seam existed.
        if not precheck_entries:
            from fno.tracker import active_backend_name

            if active_backend_name() == "graph":
                _graph_entries()
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


def _read_index(path: Path, *, warn: bool = True) -> "tuple[list[dict], int]":
    """Flatten the index into decision rows plus a damaged-row count.

    A MISSING index reads as zero decisions - the common case before the first
    write. An index that exists and cannot be read raises: an unreadable store
    answering "no decisions" is the absence-as-success failure this verb exists
    to prevent.

    ``warn=False`` silences the damaged-row notice for the reader that is about
    to REPAIR them, so a backfill does not tell the operator to run the command
    they are already running.
    """
    # os.stat, not path.exists(): exists() answers False for a dangling symlink
    # and for a PermissionError on an ancestor, so it turns an UNREACHABLE index
    # into "no decisions" - the absence-as-success failure named above.
    try:
        path.stat()
    except FileNotFoundError:
        try:
            path.lstat()
        except OSError:
            return [], 0  # nothing there: the case before the first write
        raise  # a symlink whose target is gone is UNREACHABLE, not absent

    rows: "list[dict]" = []
    damaged = 0
    # errors="replace", not strict: a crash can split a multi-byte character
    # mid-append, and a strict read raises on the whole file. That takes every
    # good row with it AND breaks reindex, the recovery this warning names.
    with path.open(encoding="utf-8", errors="replace") as fh:
        for line in fh:
            if not line.strip():
                continue
            # This file holds decisions and nothing else, so a line that is not
            # one is damage. No substring prefilter here, deliberately: a torn
            # append can end before the type string ever appears, and a
            # prefilter would drop exactly that line without counting it.
            try:
                rec = json.loads(line)
            except (json.JSONDecodeError, ValueError):
                damaged += 1
                continue
            data = rec.get("data") if isinstance(rec, dict) else None
            if (
                not isinstance(rec, dict)
                or rec.get("type") != DECISION_EVENT
                or not isinstance(data, dict)
                or not data.get("decision_id")
            ):
                damaged += 1
                continue
            row = dict(data)
            row["ts"] = rec.get("ts")
            rows.append(row)

    if damaged and warn:
        # One bad row must not cost the others, so the row is skipped. But it
        # is never skipped SILENTLY: a truncated append would otherwise make an
        # unreadable record and an empty one look the same, which is the
        # absence-as-success failure this index exists to prevent.
        #
        # The text names the sidecar rather than promising recovery. reindex
        # re-folds the journals, so a row whose source journal is gone (a
        # deleted repo, another machine) is NOT recovered; it is moved aside
        # where a human can still read it.
        print(
            f"decide: {damaged} damaged row(s) in {path} were skipped. "
            f"`fno decide reindex` re-folds the journals and moves the rest to "
            f"{path.name}.corrupt.",
            file=sys.stderr,
        )
    return rows, damaged


def _graph_entries(*, required: bool = False) -> "list[dict]":
    """The machine-wide graph, archive included, or [] if it cannot be read.

    ``required=True`` re-raises instead of degrading. A QUERY can answer
    usefully without the graph, so it warns and falls back to literal matching.
    A BACKFILL cannot: folding zero projection rows and reporting "+0
    decisions" on exit 0 is the partial-backfill-reads-as-done outcome.

    The path is passed EXPLICITLY: read_graph's default argument froze at
    import, so a redirected graph is the one read here.
    """
    try:
        from fno.graph import store as graph_store

        # read_graph_STRICT, not read_graph. The soft reader swallows
        # corruption and answers [], which here would silently collapse
        # cross-spelling recall to a literal match and report "no decisions" on
        # exit 0. The strict reader raises, so the degrade below is reachable
        # on the path a half-written graph actually takes.
        entries = graph_store.read_graph_strict(graph_store.GRAPH_JSON)
        if not required:
            return graph_store.entries_with_archive(entries)
        # entries_with_archive reads the ARCHIVE softly and degrades on any
        # failure, so a torn graph-archive.json would drop every archived
        # node's decisions from a backfill that still printed "+0" and exited
        # 0. Both graph files, or neither: a guard on one is decorative.
        from fno.paths import graph_archive_json

        archive_path = graph_archive_json()
        if not archive_path.exists():
            return entries
        live = {e.get("id") for e in entries if isinstance(e, dict)}
        return [
            *entries,
            *(
                a
                for a in graph_store.read_graph_strict(archive_path)
                if isinstance(a, dict) and a.get("id") not in live
            ),
        ]
    except Exception as exc:  # noqa: BLE001 - the graph is advisory to a string query
        if required:
            raise
        # Advisory, but NOT silent. Without the graph the reader falls back to
        # literal matching, so a ruling recorded under a slug stops answering
        # the canonical id the receipt printed. A degraded read that says
        # nothing is indistinguishable from no such decision.
        print(
            f"decide: the graph could not be read ({exc}), so a subject only "
            f"matches the exact string it was recorded under.",
            file=sys.stderr,
        )
        return []


def _resolved_node(subject: str, entries: "list[dict]") -> str | None:
    """The node id ``subject`` names EXACTLY, or None.

    Exact only. A decision on ``pr-92`` must never answer a query for
    ``pr-921``, so a near match is no match.
    """
    if not subject or not entries:
        return None
    try:
        from fno.graph.fuzzy import resolve_node

        match = resolve_node(subject, entries)
    except Exception:  # noqa: BLE001
        return None
    return str(match.id) if match.kind == "exact" and match.id else None


def _subject_matcher(subject: str):
    """A predicate over a recorded subject string, matching the same node.

    BOTH sides expand, not just the query. The operator records under whatever
    spelling was in front of them - the id, the slug, the bare hex, a different
    case - and a reader that expands only the query answers nothing for every
    other spelling. That is the PR's own defect wearing a different word, and
    the receipt makes it worse by printing the canonical id as the way back.

    A subject that names no node matches itself and nothing more.
    """
    entries = _graph_entries()
    node_id = _resolved_node(subject, entries) or _resolved_node(
        subject.strip().casefold(), entries
    )
    if node_id is None:
        # casefold, not ==. A node subject gets case-folding free through the
        # resolver's slug tier, so without this `--subject PR-923` answers
        # nothing for a ruling recorded as `pr-923` while the node classes work
        # - two subject shapes behaving differently on the case this fixes.
        # Still exact: folding case never turns `pr-92` into `pr-921`.
        want = subject.strip().casefold()
        return lambda recorded: recorded.strip().casefold() == want

    # Resolved per DISTINCT recorded subject by the caller's cache, never per
    # row: the graph read is the expensive part and it already happened.
    seen: "dict[str, str | None]" = {}
    want = subject.strip().casefold()

    def matches(recorded: str) -> bool:
        # Casefold on this branch too. The resolver's id tier is
        # case-sensitive, so without it a ruling recorded as `X-7D94` answers
        # nothing for `x-7d94` while the unresolved branch folds case happily -
        # and the doc promises every spelling, "any case", for a node subject.
        if recorded.strip().casefold() == want:
            return True
        if recorded not in seen:
            seen[recorded] = _resolved_node(recorded, entries) or _resolved_node(
                recorded.strip().casefold(), entries
            )
        return seen[recorded] == node_id

    return matches


def _decision_lane(row: dict) -> str:
    """Map stored provenance to the authority lane a reader can trust."""
    authority = str(row.get("authority_source") or "")
    if authority == "agent":
        return "coord"
    if authority == "beastmode":
        return "grant"
    if authority == "operator":
        if str(row.get("ts") or "") >= AUTHORITY_LANE_CUTOVER:
            return "law"
        return "unattributed"
    return "unattributed"


def list_decisions(
    subject: str | None = None,
    limit: int | None = None,
    lane: str | None = None,
) -> "tuple[str, list[dict], int]":
    """Decision history from the index, newest first. Never raises LookupError.

    Returns the query label, the rows, and the number of DAMAGED index rows the
    read skipped. That third value travels so the machine-readable surface can
    say the answer is short, rather than under-reporting a total that looks
    complete.

    ``subject=None`` returns every decision, which is the only way to reach a
    record written with no subject at all - what ``fno outstanding clear
    --answer`` writes for a question that names no node.
    """
    rows, damaged = _read_index(_index_path())

    # The graph projection stamped superseded_by at write time under the lock.
    # The index cannot (it is append-only), so the reader derives it, across
    # the whole scanned set rather than the filtered one. Newest superseder
    # wins: an operator can overturn one ruling twice, and file order is not
    # recency once a backfill has interleaved journals and projections.
    superseded_by: "dict[str, tuple[str, str]]" = {}
    for row in rows:
        target = row.get("supersedes")
        if not target:
            continue
        rank = (str(row.get("ts") or ""), str(row.get("decision_id") or ""))
        if rank > superseded_by.get(str(target), ("", ""))[0:2]:
            superseded_by[str(target)] = rank

    matches = _subject_matcher(subject) if subject else None
    out: "list[dict]" = []
    emitted: "set[str]" = set()
    for row in rows:
        if matches is not None and not matches(str(row.get("subject") or "")):
            continue
        # One id, one row. reindex is read-then-write with no lock across the
        # fold, so a `fno decide` landing mid-backfill can be appended twice;
        # the index is append-only, so the reader is where that stops being
        # visible as two rulings.
        did = str(row.get("decision_id") or "")
        if did in emitted:
            continue
        emitted.add(did)
        row = dict(row)
        winner = superseded_by.get(str(row.get("decision_id")))
        row["superseded_by"] = winner[1] if winner else None
        row["lane"] = _decision_lane(row)
        if lane is not None and row["lane"] != lane:
            continue
        out.append(row)

    # decision_id breaks the tie. A stable sort keeps file order for equal
    # timestamps, which silently inverts "newest first" for the backfill rows
    # that all share the no-ts fallback.
    out.sort(
        key=lambda d: (str(d.get("ts") or ""), str(d.get("decision_id") or "")),
        reverse=True,
    )
    if limit and limit > 0:
        out = out[:limit]
    return subject or "(all)", out, damaged


def _projection_events() -> "list[dict]":
    """Every decision projected onto a graph node, as event envelopes.

    The graph is machine-wide, so this reaches decisions recorded from any
    project - the half of the backfill a per-journal fold cannot see.
    """
    from fno.events import operator_decision

    events: "list[dict]" = []
    # The SAME strict-and-noisy read the query path uses. The soft reader
    # answers [] on a corrupt graph, which would fold zero projection rows and
    # still print "+0 decisions" on exit 0 - a backfill reading as done.
    entries = _graph_entries(required=True)
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
            try:
                event = operator_decision(**kwargs)
            except Exception:  # noqa: BLE001 - one unusable row must not cost the rest
                # The builder slices strings and validates. A row carrying a
                # non-string rationale raises here, and an eager list build
                # would abort the WHOLE backfill - the journal half included.
                continue
            # A row with no ts is older than anything that records one. The
            # builder would stamp NOW, which floats a legacy ruling to the top
            # of a list whose whole promise is newest-first.
            event["ts"] = row.get("ts") or "1970-01-01T00:00:00.000000Z"
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
            # errors="replace" for the same reason the index reader uses it,
            # and it matters MORE here: this folds every journal the graph
            # names, so one torn multi-byte append in any of them would make
            # reindex impossible - the very recovery the damaged-row warning
            # sends the operator to.
            fh = path.open(encoding="utf-8", errors="replace")
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
    from fno.events import append_event, validate

    index = _index_path()
    repaired = _compact_index(index)
    known = {str(row["decision_id"]) for row in _read_index(index, warn=False)[0]}
    preexisting = set(known)
    counted: "set[str]" = set()
    already = 0
    invalid = 0
    unusable = 0
    added = 0

    paths = list(sources) if sources is not None else _default_journals()
    if len(paths) > 1:
        # 83 project roots is a slow enough fold that a silent terminal reads
        # as a wedged one.
        print(f"reindex: folding {len(paths)} journal(s)...", file=sys.stderr)
    for event in _journal_events(paths) + _projection_events():
        did = str(event["data"].get("decision_id") or "")
        if not did:
            continue
        if did in known:
            # Counted once per DECISION, not once per sighting, and only
            # against what the index already held. A journal row and its own
            # projection are one decision seen twice in one run, not a record
            # that was "already indexed", and not two of them either.
            if did in preexisting and did not in counted:
                counted.add(did)
                already += 1
            continue
        try:
            # Validate FIRST, so a row the schema will never accept is told
            # apart from a store that will not take a write. Retrying the
            # former forever wedges the recovery verb; retrying the latter is
            # exactly what the operator should do.
            validate(event)
        except Exception:  # noqa: BLE001 - permanent: this row can never land
            unusable += 1
            continue
        try:
            append_event(event, events_path=index)
        except Exception:  # noqa: BLE001 - transient: the store refused a write
            invalid += 1
            continue
        known.add(did)
        added += 1

    return {
        "added": added,
        "already": already,
        "invalid": invalid,
        "unusable": unusable,
        "repaired": repaired,
        "total": len(known),
    }


def _compact_index(path: Path) -> int:
    """Rewrite the index without its damaged rows. Returns how many were dropped.

    Without this the recovery the warning names cannot succeed: the index is
    never rotated, so a torn line stays forever and every read reprints the
    same notice. Runs BEFORE the fold, so a decision lost with the damaged row
    is re-appended from the journals in the same command.

    Nothing is destroyed. The dropped lines are appended to a
    ``decisions.jsonl.corrupt`` sidecar first, because a decision whose source
    journal is gone (a deleted repo, another machine) has no other copy left.

    Under the same mkdir mutex ``append_event`` uses, so a concurrent write
    cannot land between the read and the replace.
    """
    raw = _read_lines(path)
    if all(_is_index_line(line) for line in raw):
        return 0

    from fno.mutex import acquire_dir_mutex, release_dir_mutex

    resolved = path.resolve()
    lock_dir = resolved.parent / (resolved.name + ".lock.d")
    token = acquire_dir_mutex(lock_dir, 30)
    if token is None:
        raise TimeoutError(f"decisions.jsonl lock timeout: {lock_dir}")
    try:
        # Re-read under the lock: a writer may have appended since the check.
        raw = _read_lines(resolved)
        good = [line for line in raw if _is_index_line(line)]
        dropped = [line for line in raw if not _is_index_line(line)]
        if dropped:
            with resolved.with_suffix(resolved.suffix + ".corrupt").open(
                "a", encoding="utf-8"
            ) as fh:
                fh.write("".join(line + "\n" for line in dropped))
        tmp = resolved.with_suffix(resolved.suffix + ".compact")
        tmp.write_text("".join(line + "\n" for line in good), encoding="utf-8")
        tmp.replace(resolved)
    finally:
        release_dir_mutex(lock_dir, token)
    return len(raw) - len(good)


def _read_lines(path: Path) -> "list[str]":
    """Non-blank lines, tolerant of a torn multi-byte character.

    ``errors="replace"``: a strict read raises on the whole file, which would
    make the damage unrecoverable by the very command sent to repair it.
    """
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except FileNotFoundError:
        return []
    return [line for line in text.splitlines() if line.strip()]


def _is_index_line(line: str) -> bool:
    try:
        rec = json.loads(line)
    except (json.JSONDecodeError, ValueError):
        return False
    data = rec.get("data") if isinstance(rec, dict) else None
    return (
        isinstance(rec, dict)
        and rec.get("type") == DECISION_EVENT
        and isinstance(data, dict)
        and bool(data.get("decision_id"))
    )
