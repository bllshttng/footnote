"""Graph store: flock helpers, JSON I/O, and the locked read-modify-write cycle.

Public API:
    _acquire_flock, _release_flock  - raw flock operations
    _read_json, _write_json         - raw JSON I/O (callers hold lock for writes)
    _apply_graph_defaults           - lazy migration defaults for ab- entries
    read_graph                      - unlocked read with defaults applied (soft:
                                      swallows corruption to [] for display cmds)
    read_graph_strict               - unlocked read that RAISES on an unreadable
                                      graph, for resolution callers that must tell
                                      "node absent" from "graph unreadable"
    locked_mutate_graph             - locked read-modify-write with status recompute
    GraphCorruptError               - raised on unparseable graph.json (this module)
    GraphUnreadableError            - strict-read failure (bad JSON / bad shape)
    GraphMalformedRootError         - subclass: root object with no 'entries' key

Graph read-failure taxonomy (four types across two modules, distinct call paths,
NOT severities): GraphCorruptError (here, JSON parse on the raw read) and
GraphUnreadableError (here, the strict read) both mean "bytes/shape unusable";
load.py's GraphCorruptionError is a different axis entirely - a SHA256 sidecar
mismatch, checked only by load_graph, never by read_graph/read_graph_strict.

Sidecar / backup protocol (Layer 2 hygiene):
    After every successful atomic write locked_mutate_graph:
      1. Creates a timestamped backup of the PREVIOUS content: graph.json.bak.<ts>
      2. Writes a SHA256 sidecar: graph.json.sha256
    Backups are pruned to GRAPH_BACKUP_KEEP (10) most-recent files.
"""
from __future__ import annotations

import copy
import fcntl
import hashlib
import json
import os
import shutil
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

from fno.graph._constants import (  # noqa: F401  GRAPH_MD re-exported: patched via store.GRAPH_MD
    GRAPH_JSON,
    GRAPH_MD,
)

# Keep at most this many timestamped backups on disk.
GRAPH_BACKUP_KEEP = 10

# Canonical key order for serialized graph entries. Status-forward: a human
# scanning graph.json (or `fno backlog get`) sees the node's id and derived
# status before anything else, then the human-scan fields (title/priority),
# then the parent/children relationship, then the long lifecycle/provenance
# tail. Keys NOT in this list are appended in their original order after the
# canonical block, so forward-compat schema additions and legacy extras (e.g.
# the old `points` field) are reordered-around, never dropped.
CANONICAL_FIELD_ORDER: list[str] = [
    "id",
    "status",
    # Title-derived human handle (ab-f82e8083). Additive: it LEADS display but
    # `id` stays the canonical key. Listed here so canonicalize keeps it and
    # places it right after the status, ahead of the title it derives from.
    "slug",
    "title",
    "priority",
    # Optional curated board rank (nullable float). Orders cards within a
    # (column, project) lane ahead of the (priority, created_at) fallback;
    # null = unranked. Listed here so canonicalize keeps + backfills it
    # (self-healing) rather than dropping it as an unknown extra (ab-95a4a479).
    "rank",
    "type",
    "parent",
    "children",
    "project",
    "cwd",
    "domain",
    "blocked_by",
    # Asserted affinity, symmetric and non-gating. Distinct from the computed
    # relatedness sidecar, which is regenerable and would destroy an assertion.
    "related",
    # Contract-tier dependency classification (G2). Present ONLY on a
    # `dep=contract` dependent (it stubs against a pinned ## Interface Contract);
    # absent on the default `hard` path, so canonicalize keeps the hard-path
    # serialization byte-for-byte unchanged.
    "dep",
    "stub_against",
    "contract_version",
    # Lock owner. locked_by is canonical; session_id is the one-release legacy
    # mirror (kept in sync by _normalize_lock_fields). locked_by_harness* record
    # the holder's harness + harness-session UUID (US6).
    "locked_by",
    "locked_by_harness",
    "locked_by_harness_session",
    "session_id",
    "claimed_at",
    # Structured, recomputed uncertainty on a persisted active owner whose
    # claimed_at age exceeded the graph TTL. It never asserts the worker died.
    "ownership_defect",
    "completed_at",
    "deferred_at",
    "deferred_reason",
    "touched_at",
    "has_brief",
    "roadmap_id",
    "vision_path",
    "details",
    "size",
    "batch",
    "cost_usd",
    "cost_sessions",
    # Delivery-unit containment (x-e957): this node's work ships inside the
    # named node's PR. Deliberately NOT setdefault-ed below, unlike the other
    # nullable scalars - listed here only so it lands in canonical position
    # WHEN present. Defaulting it to null would stamp `"contained_in": null`
    # onto every node in the graph and break the byte-identity guarantee that a
    # containment-free graph serializes exactly as it did pre-change. Same
    # sparse treatment as `dep`/`stub_against` above, for the same reason.
    # Readers must use `.get("contained_in")`, never `[...]`.
    "contained_in",
    "plan_path",
    "pr_number",
    "pr_url",
    "additional_prs",
    "merge_status",
    "artifact_url",
    "completion_note",
    # Append-only timestamped progress notes ({ts, text}), distinct from the
    # single `completion_note` string: the status-fanout backlog-progress adapter
    # stamps one per task_done/run_summary (x-2057). `fno backlog note` appends.
    "progress_notes",
    "created_at",
    "supersedes",
    "superseded_by",
    "supersession",
    "collisions_acknowledged",
    "source",
    "source_kind",
    "source_project",
    "source_session_id",
    # Parent-edge provenance (x-30f6). source_node_id is the backlog->origin-node
    # edge; source_harness/source_plan_path enrich the source_session_id at node
    # birth; spawned_by_* is the ambient parent-session edge stamped at worker
    # spawn. All nullable; ambient-captured, never required of a caller.
    "source_harness",
    "source_cwd",
    "source_node_id",
    "source_plan_path",
    "source_inbox_msg",
    "spawned_by_session",
    "spawned_by_harness",
    "spawned_by_cwd",
    # Append-only lifecycle provenance (x-b6e4): {phase, harness, session_id, at}
    # per phase boundary. Sits in the provenance tail after the birth/spawn edges.
    "sessions",
    "queued_at",
    "queued_reason",
]

# Fields copied into each parent's ``children`` summary. Compact on purpose:
# enough to scan what a child is and where it stands without a second lookup,
# light enough that the flat ``entries`` store is not denormalized into a tree.
CHILD_SUMMARY_FIELDS: tuple[str, ...] = ("id", "title", "project", "status")


def normalize_plan_path(path: str | None) -> str | None:
    """Normalize a ``plan_path`` for comparison across graph / ledger and across
    absolute-vs-relative + trailing-slash conventions.

    A plan owned by two nodes is the delivery-unit violation (x-04b9): a plan is
    one PR is one node. Comparing raw strings lets an abs/rel mismatch smuggle a
    second binding past the refusal, so every comparison site routes through this
    one normalizer rather than each writing its own.
    """
    if not path:
        return None
    return os.path.normpath(path).rstrip(os.sep)


def plan_path_owner_conflict(
    entries: list[dict], node_id: str | None, plan_path: str | None
) -> str | None:
    """Return the id of another node already bound to the same ``plan_path``.

    The one-plan-one-node invariant (x-04b9): a plan file is the delivery unit of
    exactly one node, regardless of which roadmap tracks it. Centralized so every
    plan_path write site routes through one check rather than each reimplementing
    it - the 2026-07-28 mislinking recurred because a guard landed on only one of
    N reachable write sites (cmd_update, intake's claim/create lanes, multi).

    ``node_id`` is the node being bound, excluded from the search so re-binding a
    plan a node already owns is not a self-conflict. Returns ``None`` when no other
    node holds the (normalized) plan. Roadmap-agnostic on purpose: intake's
    same-roadmap match already handles the friendly "already intaked" case, but a
    plan owned under a different roadmap still arms two concurrent dispatches.
    """
    new_norm = normalize_plan_path(plan_path)
    if new_norm is None:
        return None
    for e in entries:
        if not isinstance(e, dict):
            continue
        eid = e.get("id")
        if eid == node_id or not isinstance(eid, str):
            continue
        if normalize_plan_path(e.get("plan_path")) == new_norm:
            return eid
    return None


def _mirror_related(
    by_id: dict, node_id: str, *, added: set, removed: set
) -> None:
    """Write the inverse edge onto each peer ``node_id`` gained or dropped.

    Split out from :func:`set_related` so a test can fault exactly this step and
    prove the half-edge state is unreachable rather than merely unlikely.

    A missing peer in ``added`` raises rather than being skipped. Callers resolve
    peers against this same snapshot under the same lock, so it is a programming
    error, not a race - and failing loudly beats writing an edge that dangles. A
    missing peer in ``removed`` is ignored: the edge is already gone.
    """
    for peer_id in added:
        peer = by_id[peer_id]
        peer["related"] = sorted({*(peer.get("related") or []), node_id})
    for peer_id in removed:
        peer = by_id.get(peer_id)
        if peer is not None:
            peer["related"] = sorted(set(peer.get("related") or []) - {node_id})


def set_related(entries: list[dict], node_id: str, desired: list[str]) -> None:
    """Declare ``node_id``'s related set and mirror it onto every peer.

    Symmetry is stored on both endpoints, not derived. ``children`` can be
    rebuilt from scratch on every write because it inverts a single ``parent``
    pointer, so the rebuild is lossless. ``related`` has two independently
    declaring sides, so the same idiom would discard whichever side did not
    write last.

    Mutates ``entries`` in place. Both halves land in the caller's
    ``locked_mutate_graph`` call, so ``B in A.related`` iff ``A in B.related``
    cannot be left half-written: an exception aborts the mutation before
    anything is persisted.
    """
    by_id = {e.get("id"): e for e in entries}
    node = by_id[node_id]
    before = set(node.get("related") or [])
    after = set(desired)
    node["related"] = sorted(after)
    _mirror_related(by_id, node_id, added=after - before, removed=before - after)


def _compute_children(entries: list[dict]) -> list[dict]:
    """Populate each entry's ``children`` with summaries of its direct children.

    The inverse of the ``parent`` pointer, rebuilt from scratch on every call so
    it can never drift: any change to a child is itself a mutation, and
    ``locked_mutate_graph`` runs this over the whole list on each write. A
    ``parent`` pointing at a non-existent id is ignored (no phantom summary).
    Each summary's ``status`` DERIVES through ``statuses.readiness_status``
    (the same overlay every live read applies): the stored field never
    encodes ``blocked`` - it is a read-time derivation - so a snapshot of the
    raw field would report a blocked child as ready in the one array a
    surveying king reads. Mutates entries in place and returns the same list.
    """
    from fno.graph.statuses import readiness_status

    id_to_entry = {
        e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    valid_ids = id_to_entry.keys()
    kids: dict[str, list[dict]] = {}
    for e in entries:
        if not isinstance(e, dict):
            # A junk row (scalar/list/null) is skipped, never dropped here:
            # `_apply_graph_defaults` runs this on the read path where the
            # evidence caller needs malformed rows to survive the pass.
            continue
        parent = e.get("parent")
        cid = e.get("id")
        # cid != parent: a self-parented node (corrupt import, manual edit)
        # must not become its own child -- that would accumulate on every write
        # with no self-healing path. The update verb already rejects cycles;
        # this guard keeps the writer correct even if a bad row slips through.
        if (
            isinstance(cid, str)
            and isinstance(parent, str)
            and parent in valid_ids
            and cid != parent
        ):
            summary = {k: e.get(k) for k in CHILD_SUMMARY_FIELDS}
            summary["status"], _ = readiness_status(e, id_to_entry)
            kids.setdefault(parent, []).append(summary)
    for e in entries:
        if not isinstance(e, dict):
            continue
        eid = e.get("id")
        # Most nodes are leaves: skip the empty-list allocation + no-op sort for
        # them, only sorting when a node actually has children.
        summaries = kids.get(eid) if isinstance(eid, str) else None
        if summaries:
            summaries.sort(key=lambda c: c.get("id") or "")
            e["children"] = summaries
        else:
            e["children"] = []
    return entries


def _normalize_lock_fields(entries: list[dict]) -> None:
    """Reconcile the lock-owner field to locked_by, mirroring session_id.

    One-release rename shim. Key-presence disambiguates: a node written by new
    code carries a locked_by key (authoritative, wins over any session_id); a
    pre-rename node carries only session_id, so locked_by adopts it. Both keys
    are then set to the resolved value so the mirror stays in sync for readers
    on either name. Idempotent; mutates entries in place.
    """
    for e in entries:
        if not isinstance(e, dict):
            continue
        # Legacy node (pre-rename): on a LIVE node the session_id key IS the
        # lock owner, so adopt it. On a DONE node session_id is work/cost
        # provenance (done/cli.py:_apply_rollup), NOT a lock - adopting it would
        # make locked_by truthy and mirror it back over a force-overwrite, so
        # leave locked_by unset there.
        if "locked_by" not in e:
            e["locked_by"] = None if e.get("completed_at") else e.get("session_id")
        resolved = e.get("locked_by")
        if resolved:
            # Claimed: session_id mirrors the canonical lock owner for the
            # one-release window (locked_by wins over any divergent session_id).
            e["session_id"] = resolved
        elif not e.get("completed_at"):
            # Released and not done (unclaim / defer / supersede / auto-failure):
            # drop the stale mirror so no consumer reads a dead owner. Keying on
            # locked_by (not session_id) means status is already correct; this
            # just keeps the mirror honest.
            e["session_id"] = None
        # else: done with no active lock - leave session_id, which carries
        # done-time work/cost provenance (done/cli.py:_apply_rollup), a distinct
        # meaning from the lock. A done node never derives 'claimed' (completed_at
        # wins), so this is never read as a live lock.
        if not resolved:
            # A cleared owner must not retain a holder identity: drop the US6
            # harness stamp whenever the lock is unset, so a later re-claim can
            # never route to a stale holder.
            e["locked_by_harness"] = None
            e["locked_by_harness_session"] = None


def canonicalize_entries(entries: list[dict]) -> list[dict]:
    """Reorder each entry's keys status-forward and refresh the children index.

    Returns a new list of new dicts (does not preserve the input dict objects'
    key order). Unknown keys are appended after the canonical block in their
    original relative order so nothing is dropped. Called inside
    ``locked_mutate_graph`` after ``recompute_statuses``; each child summary's
    ``status`` is derived at build time inside ``_compute_children``
    (``recompute_statuses`` never writes ``blocked`` - it is a read-time
    derivation - so the summaries cannot simply copy the cascade field).
    """
    _compute_children(entries)
    # Keep the locked_by/session_id mirror consistent after the mutator +
    # recompute_statuses ran, before the entries are serialized.
    _normalize_lock_fields(entries)
    out: list[dict] = []
    for e in entries:
        ordered: dict = {}
        for k in CANONICAL_FIELD_ORDER:
            if k in e:
                ordered[k] = e[k]
        for k, v in e.items():
            if k not in ordered:
                ordered[k] = v
        out.append(ordered)
    return out


class GraphCorruptError(Exception):
    """Raised when graph.json or graph-archive.json cannot be parsed as JSON.

    Distinguishes genuine corruption (unparseable bytes) from a valid-but-empty
    graph ({"entries": []}), which previous code mis-flagged via a fragile
    file-size heuristic.
    """


class GraphUnreadableError(Exception):
    """Strict-read failure: the graph exists but could not be read as a graph.

    Covers bad JSON bytes, a zero-byte file, and a non-object root (a bare list,
    null, or string). The point is diagnosability: a resolution caller that
    catches this KNOWS the graph could not be read, instead of concluding the
    node is absent (the duplicate-filing class). ``read_graph`` -- the soft
    display path -- never raises this; it swallows to [].
    """


class GraphMalformedRootError(GraphUnreadableError):
    """Root JSON is an object but has no 'entries' key.

    Distinct from a legitimately empty ``{"entries": []}``. A subclass so a
    caller needing only "unreadable vs absent" catches the base, while one
    wanting the finer distinction catches this.
    """


def _acquire_flock(lock_path: Path) -> int:
    """Acquire exclusive flock on the given path. Returns the lock fd."""
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
    fcntl.flock(fd, fcntl.LOCK_EX)
    return fd


def _release_flock(fd: int):
    """Release flock."""
    fcntl.flock(fd, fcntl.LOCK_UN)
    os.close(fd)


def _write_sha256_sidecar(path: Path) -> None:
    """Write SHA256 of path to {path}.sha256 (atomic via temp+rename).

    Called inside the locked critical section after every successful write.
    """
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    sidecar = Path(str(path) + ".sha256")
    tmp_fd, tmp_path = tempfile.mkstemp(dir=path.parent, suffix=".sha256.tmp")
    try:
        with os.fdopen(tmp_fd, "w") as f:
            f.write(digest + "\n")
        os.replace(tmp_path, str(sidecar))
    except Exception:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise


def _create_backup(path: Path) -> Path | None:
    """Copy current graph.json to a timestamped backup, then prune old backups.

    Backups are named graph.json.bak.<ISO-timestamp-no-colons>.
    Keeps GRAPH_BACKUP_KEEP most-recent entries; prunes the rest.
    No-op if graph.json does not yet exist (first write).

    Returns the backup path, or None when none was made -- a caller that tells
    the user their data is recoverable has to be able to check that it is.
    """
    if not path.exists():
        return None

    ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%f")
    backup = path.parent / f"{path.name}.bak.{ts}"
    try:
        shutil.copy2(path, backup)
    except OSError as e:
        print(f"Warning: graph backup failed: {e}", file=sys.stderr)
        return None

    # Prune: keep only the GRAPH_BACKUP_KEEP most-recent .bak.* files
    existing = sorted(path.parent.glob(f"{path.name}.bak.*"))
    to_prune = existing[:-GRAPH_BACKUP_KEEP] if len(existing) > GRAPH_BACKUP_KEEP else []
    for old in to_prune:
        try:
            old.unlink()
        except OSError:
            pass
    return backup


def _apply_readiness_overlay(entries: list[dict]) -> None:
    """Overlay read-time dependency readiness onto `status`/`blocked_reason`.

    Mutates `entries` in place. Shared by `_apply_graph_defaults` (every
    reader) and `locked_mutate_graph`'s auto-render step: `recompute_statuses`
    no longer derives `blocked` at write time, so the entries handed to
    `render_graph_md`/`render_graph_html` right after a mutation need this
    same overlay re-applied, or the freshly written graph.md/graph.html would
    show a mutated node's dependency-blocked status as ready/idea/etc until
    the next explicit read. The per-entry precedence lives in
    `statuses.readiness_status`, shared with `_compute_children`.
    """
    from fno.graph.statuses import pending_supersession_reason, readiness_status

    id_to_entry = {
        e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    for e in entries:
        if not isinstance(e, dict):
            continue
        pending_reason = pending_supersession_reason(e)
        if pending_reason:
            e["status"] = "blocked"
            e["blocked_reason"] = pending_reason
            continue
        e["status"], e["blocked_reason"] = readiness_status(e, id_to_entry)


def _apply_graph_defaults(entries: list[dict], *, keep_malformed: bool = False) -> list[dict]:
    """Apply lazy migration defaults to graph entries (ab- IDs).

    The one migration seam: every reader routes through here, so a row whose
    on-disk vocabulary predates a rename reads the same no matter which reader
    a caller reached for.
    """
    # A junk row (scalar, list, null) is always SKIPPED for migration -- every
    # field access below assumes a dict -- and by default is also DROPPED from
    # the result, because almost every caller then indexes what it gets back
    # (`{e["id"]: e ...}`, `e.get(...)`) and the write path additionally feeds
    # it to ensure_slugs / recompute_statuses / canonicalize_entries, none of
    # which tolerate a scalar. Preserving one there does not merely skip a row,
    # it wedges the graph: the only code that could rewrite the file is the code
    # that crashes on it, so there is no self-healing path back.
    #
    # `keep_malformed=True` is the single exception, for a caller that treats a
    # junk row as EVIDENCE rather than as data: agents/discover.py's
    # _reachable_from_graph counts them to report "graph unreadable" instead of
    # "this token names nothing", and its own comment notes that reporting empty
    # there would drop the mail. Filtering would remove the very signal it reads.
    # In-memory legacy priority backfill so read-only commands (ready,
    # next, status, tree, triage context) sort correctly *before* the
    # first write triggers recompute_statuses' on-disk backfill. The
    # mutate path still rewrites priority on disk; this just keeps the
    # read path honest in the gap before that happens.
    from fno.graph._constants import PRIORITY_MIGRATION
    from fno.graph.statuses import STATUS_MIGRATION
    for e in entries:
        if not isinstance(e, dict):
            continue  # skipped, not dropped -- see the note above
        # Key rename `_status` -> `status`: a pre-rename row still carries the
        # underscore key, so fold it in before anything reads `status`.
        if "_status" in e:
            e.setdefault("status", e["_status"])
            del e["_status"]
        # `in <dict>` hashes the key, so a row carrying an unhashable value (a
        # hand-mangled `"priority": []`) would raise TypeError. Every reader now
        # routes through here, including ones documented as never-fatal, so the
        # type check belongs here rather than in each caller's except clause.
        old_priority = e.get("priority")
        if isinstance(old_priority, str) and old_priority in PRIORITY_MIGRATION:
            e["priority"] = PRIORITY_MIGRATION[old_priority]
        # Same idea for the renamed `claimed` -> `in_progress` status: the read
        # path must speak the current vocabulary even for a row whose on-disk
        # `status` predates the rename and has not been re-mutated yet.
        old_status = e.get("status")
        if isinstance(old_status, str) and old_status in STATUS_MIGRATION:
            e["status"] = STATUS_MIGRATION[old_status]
    for e in entries:
        if not isinstance(e, dict):
            continue
        e.setdefault("parent", None)
        e.setdefault("tags", [])
        e.setdefault("type", "feature")
        e.setdefault("project", None)
        e.setdefault("cwd", None)
        e.setdefault("priority", "p2")
        # Curated board rank: null = unranked (rejoins the priority fallback).
        e.setdefault("rank", None)
        e.setdefault("domain", "code")
        e.setdefault("blocked_by", [])
        e.setdefault("session_id", None)
        # locked_by is the canonical lock owner; harness fields (US6) record the
        # holder's provider + harness-session UUID. session_id stays mirrored.
        e.setdefault("locked_by_harness", None)
        e.setdefault("locked_by_harness_session", None)
        e.setdefault("claimed_at", None)
        e.setdefault("completed_at", None)
        e.setdefault("status", "ready")
        # Title-derived handle (ab-f82e8083). Default null on the read path;
        # the actual value is assigned by ensure_slugs() inside the locked
        # mutate cycle, so a pre-backfill node reads null and display falls
        # back to the hex alone until the next mutation slugs it.
        e.setdefault("slug", None)
        # Derived inverse-of-parent index. Empty until the next mutation runs
        # canonicalize_entries; populated authoritatively on every write.
        e.setdefault("children", [])
        e.setdefault("has_brief", False)
        e.setdefault("roadmap_id", None)
        e.setdefault("vision_path", None)
        e.setdefault("details", None)
        e.setdefault("cost_usd", None)
        e.setdefault("cost_sessions", [])
        e.setdefault("size", None)
        e.setdefault("batch", None)
        e.setdefault("plan_path", None)
        e.setdefault("pr_number", None)
        e.setdefault("pr_url", None)
        e.setdefault("additional_prs", [])
        e.setdefault("merge_status", None)
        e.setdefault("artifact_url", None)
        e.setdefault("completion_note", None)
        e.setdefault("progress_notes", [])
        e.setdefault("collisions_acknowledged", [])
        e.setdefault("related", [])
        e.setdefault("supersedes", [])
        e.setdefault("superseded_by", None)
        e.setdefault("supersession", None)
        e.setdefault("source_kind", "organic")
        e.setdefault("source_project", None)
        e.setdefault("source_session_id", None)
        # Parent-edge provenance (x-30f6): null until ambient-stamped at node
        # birth (graph/cli.py idea/add) or worker spawn (agents dispatch).
        e.setdefault("source_harness", None)
        e.setdefault("source_cwd", None)
        e.setdefault("source_node_id", None)
        e.setdefault("source_plan_path", None)
        e.setdefault("source_inbox_msg", None)
        e.setdefault("spawned_by_session", None)
        e.setdefault("spawned_by_harness", None)
        e.setdefault("spawned_by_cwd", None)
        # Append-only lifecycle provenance (x-b6e4): empty on legacy nodes.
        e.setdefault("sessions", [])
        # Decision records projected from operator_decision events.
        # Append-only; supersession is a marked field on the older row, never
        # a removal, so a reader of an overturned decision can tell.
        e.setdefault("decisions", [])
        # Queued: orthogonal to status. A queued node is still ready (has a
        # plan, unblocked); the queued_at field marks the user's intent to
        # pick it up next/today. Cleared on completion.
        e.setdefault("queued_at", None)
        e.setdefault("queued_reason", None)
    # Populate locked_by (legacy nodes adopt their session_id) so readers and
    # status derivation see the canonical field. Runs last so the key-presence
    # rule still sees a pre-rename node's missing locked_by key.
    _normalize_lock_fields(entries)

    # Dependency readiness, computed fresh on every read rather than trusted
    # from disk: recompute_statuses no longer derives `status` from
    # `blocked_by` at write time, so this is the one seam every reader shares
    # (read_graph, read_graph_strict, load_graph, read_graph_nodes) and the
    # only place a stale on-disk snapshot cannot survive. Precedence matches
    # what recompute_statuses used to enforce inline: done/superseded/
    # deferred/in_review are direct facts about THIS node and already outrank
    # blocked, so they are left alone; everything else (in_progress/idea/
    # design/ready) is a candidate for the blocked overlay.
    _apply_readiness_overlay(entries)

    # Rebuild the parent children summaries AFTER the overlay so every reader
    # derives them live: the persisted snapshot predates this call and carries
    # whatever the last write stamped, so without this a parent's children
    # array can contradict the same read's own top-level statuses (a stored-
    # ready child with an open blocker reads blocked above but ready below).
    # Same function the write path runs; idempotent on already-derived input.
    _compute_children(entries)

    if not keep_malformed:
        entries = [e for e in entries if isinstance(e, dict)]
    return entries


def _read_json(path: Path) -> list[dict]:
    """Raw read of a JSON entries file. Caller must hold lock for write paths.

    Raises GraphCorruptError on JSON parse failure OR when the root value is
    not a JSON object (e.g., `null`, a bare list, or a string). A missing file
    or a valid file with no/empty entries key returns [] -- those are NOT
    corruption.
    """
    if not path.exists():
        return []
    try:
        data = json.loads(path.read_text())
        if not isinstance(data, dict):
            raise json.JSONDecodeError("graph root must be a JSON object", "", 0)
        entries = data.get("entries", [])
        # A present-but-non-list 'entries' (e.g. {"entries": "x"}) would otherwise
        # reach _apply_graph_defaults and raise a bare AttributeError, breaking
        # read_graph's "swallow corruption to [], never crash the terminal"
        # contract. Route it through the same corrupt-handling as bad JSON so
        # read_graph swallows it and locked_mutate exits cleanly -- and so it
        # gets the same .bak that locked_mutate's "restore from backup" message
        # promises (a raise without the .bak would advertise a file that does
        # not exist, leaving the operator only the data-losing delete option).
        if not isinstance(entries, list):
            raise json.JSONDecodeError("graph 'entries' must be a list", "", 0)
    except json.JSONDecodeError:
        backup = path.with_suffix(".json.bak")
        try:
            shutil.copy2(path, backup)
            print(f"Warning: {path} is corrupt, backup saved to {backup}", file=sys.stderr)
        except OSError as e:
            print(f"Warning: {path} is corrupt, backup also failed: {e}", file=sys.stderr)
        raise GraphCorruptError(str(path))
    return entries


def _write_json(entries: list[dict], path: Path) -> None:
    """Raw atomic write. Caller must hold lock."""
    data = {"entries": entries}
    tmp_fd, tmp_path = tempfile.mkstemp(dir=path.parent, suffix=".tmp")
    try:
        with os.fdopen(tmp_fd, "w") as f:
            f.write(json.dumps(data, indent=2) + "\n")
        os.replace(tmp_path, str(path))
    except Exception:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise


def read_graph(path: Path = GRAPH_JSON) -> list[dict]:
    """Read graph.json, applying graph defaults. No lock needed for reads.

    Swallows corruption on the read path -- commands like `status` and `ready`
    should not crash a user's terminal when graph.json is wedged. Write paths
    (locked_mutate_graph) surface the error instead.

    That contract is why a malformed ROW is filtered here rather than in
    ``_apply_graph_defaults``: ordinary consumers index these entries as dicts
    (``cmd_tree`` builds ``{e["id"]: e ...}``, ``resolve_node`` calls
    ``e.get``), so handing one a scalar trades a clean degrade for a
    TypeError. The pass keeps such rows because two callers need them --
    ``locked_mutate_graph`` must not delete what it cannot migrate, and
    ``load_graph``'s discovery caller counts them as evidence the graph is
    unreadable -- but neither of those is an ordinary read.
    """
    try:
        return _apply_graph_defaults(_read_json(path))
    except GraphCorruptError:
        return []


def entries_with_archive(entries: list) -> list:
    """``entries`` plus archived nodes, the working graph winning on id.

    Best-effort and read-only: an absent or unreadable archive degrades to the
    working graph. Shared by historical recall (``cmd_find``) and the
    filing-time dedup net so a duplicate of shipped-and-archived work still
    surfaces (codex P2: the working graph alone misses it).
    """
    from fno.paths import graph_archive_json

    try:
        archive_path = graph_archive_json()
        if not archive_path.exists():
            return entries
        live = {e.get("id") for e in entries if isinstance(e, dict)}
        return [
            *entries,
            *(
                a for a in read_graph(archive_path)
                if isinstance(a, dict) and a.get("id") not in live
            ),
        ]
    except Exception:  # noqa: BLE001 - archive is advisory; any read failure degrades to the working graph
        return entries


def read_graph_with_archive(path: Path | None = None) -> list[dict]:
    """Read the working graph through the canonical seam, then overlay archive."""
    if path is None:
        from fno.paths import graph_json

        path = graph_json()
    return entries_with_archive(read_graph(path))


def read_graph_strict(path: Path = GRAPH_JSON) -> list[dict]:
    """Failure-surfacing counterpart to :func:`read_graph`.

    Returns entries (defaults applied) for a populated OR legitimately empty
    graph, and for an absent file (an absent graph is empty, not unreadable --
    matching ``read_graph``). RAISES instead of returning [] when the graph
    cannot be read cleanly, so a resolution caller can tell "node absent" apart
    from "graph unreadable":

      - :class:`GraphMalformedRootError` -- root object lacks an ``entries`` key
      - :class:`GraphUnreadableError`    -- bad JSON, zero bytes, or a non-object
        root (bare list/null/string), or an ``entries`` value that is not a list

    Never writes a ``.bak``: diagnosis is read-only, and a file that parsed did
    not fail to parse (AC1-EDGE). ``read_graph``'s soft contract is untouched;
    display commands (``status``/``ready``) keep swallowing to [].

    Deliberately does NO sha256-sidecar check: this is the read-path integrity
    layer (bytes/shape), separate from ``load.load_graph``'s hash layer. A graph
    that is byte-corrupt yet still valid JSON is a hash-mismatch, which only
    ``load_graph`` detects; routing every ``get`` through the hash check is out
    of scope here (it would tax every resolution read).
    """
    if not path.exists():
        return []
    # Inside the guard: this function's contract is that anything unreadable
    # surfaces as GraphUnreadableError, and callers branch on that to tell a
    # wedged graph from an absent node. A bare read_text() let a directory, a
    # permission error, or non-UTF-8 bytes escape as OSError/UnicodeDecodeError,
    # past the caller's `except GraphUnreadableError` and out as a generic exit 1
    # -- the code that means "read cleanly, node absent".
    try:
        raw = path.read_text()
    except (OSError, UnicodeDecodeError) as e:
        raise GraphUnreadableError(f"{path} could not be read: {e}") from e
    if raw.strip() == "":
        raise GraphUnreadableError(f"{path} is empty (zero bytes)")
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as e:
        raise GraphUnreadableError(f"{path} is not valid JSON: {e}") from e
    if not isinstance(data, dict):
        raise GraphUnreadableError(
            f"{path} root is not a JSON object (got {type(data).__name__})"
        )
    if "entries" not in data:
        raise GraphMalformedRootError(f"{path} root object has no 'entries' key")
    entries = data["entries"]
    if not isinstance(entries, list):
        raise GraphUnreadableError(
            f"{path} 'entries' is not a list (got {type(entries).__name__})"
        )
    # Same reader-boundary filter as read_graph: a resolution caller indexes
    # these as dicts, and this function's job is to distinguish "graph
    # unreadable" (it raises) from "node absent" -- not to hand back rows no
    # caller can use.
    return _apply_graph_defaults(entries)


def _graph_lock_path(path: Path) -> Path:
    """Sibling lockfile for a graph.json (``<graph>.lock``).

    Resolved so two spellings of the same graph (relative vs absolute, or a
    symlinked .fno) share ONE inode and stay mutually exclusive; mirrors the
    is_canonical resolve below. Falls back to the raw path on a resolve error
    rather than crashing the mutation. A symlink loop raises OSError (ELOOP) or
    RuntimeError depending on the Python version, so catch both.
    """
    # ponytail: resolve() before .lock so aliased paths to one graph share a
    # lock; drop it only if profiling ever flags the stat.
    try:
        base = path.resolve()
    except (OSError, RuntimeError):
        base = path
    return Path(str(base) + ".lock")


# Fields whose change marks a node as human-curated "just now" (x-7dcb). Kept
# to exactly the fields groom's own lever allowlist can move, so this is
# exactly the reversal class an automated sweep must not undo. An include-list
# fails closed: a future janitorial field cannot accidentally freeze the drain.
_CURATION_FIELDS = ("status", "priority", "rank", "parent", "blocked_by", "size")


def _curation_key(entry: dict) -> tuple:
    # blocked_by is a list; freeze it so the tuple stays hashable and
    # order-sensitive (a reordered blocked_by is a real edge change).
    return tuple(
        tuple(entry[f]) if isinstance(entry.get(f), list) else entry.get(f)
        for f in _CURATION_FIELDS
    )


def release_node_claim_at_closure(node_id: str, *, rung: str) -> None:
    """Drop the ``node:<id>`` claim a closure just made moot (x-94f8).

    Holder-agnostic: the closer (reconcile, king, daemon, a human) is usually
    not the worker that holds the claim, and a claim on a closed node protects
    nothing. Both claims roots are checked because node claims moved to the
    global root late (ab-fcf9cec5); a legacy claim may still sit repo-local.
    Best-effort and loud: a release failure is a named stderr line, never a
    failed graph mutation - closure outranks release, and the reaper's
    node-aware settlement is the backstop.
    """
    from fno.claims.core import claim_path, force_release_claim
    from fno.claims.io import claims_root_for, dedup_claims_roots

    key = f"node:{node_id}"
    try:
        for raw_root, _dir in dedup_claims_roots([claims_root_for(key), None]):
            path = claim_path(key, root=raw_root)
            if not path.exists():
                continue
            force_release_claim(key, reason=f"node closed ({rung})", root=raw_root)
    except Exception as exc:  # noqa: BLE001 - closure must not fail on this
        print(
            f"node closure: claim release failed for {key}: {exc}",
            file=sys.stderr,
        )


def locked_mutate_graph(path: Path, mutator) -> list[dict]:
    """Locked read-modify-write for graph entries. Recomputes statuses after mutation."""
    # Import here to avoid circular imports
    from fno.graph.statuses import recompute_statuses
    from fno.graph.render import render_graph_md
    from fno.paths import vault_root

    path.parent.mkdir(parents=True, exist_ok=True)
    # Terminal transitions this mutation collects for the post-flock claim
    # release (see the closure hook near the recompute below).
    closure_releases: list[tuple[str, str]] = []
    fd = _acquire_flock(_graph_lock_path(path))
    try:
        try:
            raw = _read_json(path)
        except GraphCorruptError:
            print(f"Error: {path} appears corrupt (backup at {path.with_suffix('.json.bak')}). "
                  f"Restore from backup or delete before proceeding.", file=sys.stderr)
            sys.exit(1)
        entries = _apply_graph_defaults(raw)
        # Pre-mutator curation snapshot (x-7dcb), keyed by id, for the
        # touched_at stamp below. Taken after defaults so a legacy row's
        # first-touch backfill (e.g. rank -> None) does not itself read as a
        # curation change.
        #
        # `status` is re-derived via `recompute_statuses` on a throwaway
        # COPY rather than read straight off `entries`: `_apply_graph_defaults`
        # ends by overlaying live blocked_by readiness onto `status` (every
        # reader gets "blocked" when a dependency is unmet -
        # `_apply_readiness_overlay`), but `recompute_statuses` below never
        # derives "blocked" and the write path never persists it. Comparing
        # the overlay value against the post-recompute value would misread
        # EVERY mutation on a currently-blocked node as a status change,
        # regardless of what the mutator actually touched. Re-deriving both
        # sides through the same function keeps them comparable.
        _status_normalized = {
            e["id"]: e.get("status")
            for e in recompute_statuses(copy.deepcopy(entries))
            if isinstance(e, dict) and isinstance(e.get("id"), str)
        }
        _pre_curation = {}
        for e in entries:
            if not isinstance(e, dict) or not isinstance(e.get("id"), str):
                continue
            _snapshot_entry = dict(e)
            _snapshot_entry["status"] = _status_normalized.get(e["id"], e.get("status"))
            _pre_curation[e["id"]] = _curation_key(_snapshot_entry)
        # The pass drops rows it cannot migrate, and on THIS path that drop is
        # persisted by the _write_json below. Dropping is still the right call
        # here -- ensure_slugs / recompute_statuses / canonicalize_entries below
        # all assume dicts, so keeping one would wedge every future write with
        # no way back -- but deleting from the user's graph is not something a
        # `backlog update` should do without a word. The prior content survives
        # in the .bak _create_backup takes just before the write.
        _dropped = len(raw) - len(entries)
        entries = mutator(entries)
        from fno.company.contracts import validate_company_work_for_node

        for entry in entries:
            if not isinstance(entry, dict) or entry.get("company_work") is None:
                continue
            entry_id = entry.get("id")
            if not isinstance(entry_id, str):
                raise ValueError("company_work graph entry requires a string id")
            refs = validate_company_work_for_node(
                entry["company_work"], entry_id, owner="graph entry id"
            )
            assert refs is not None
            entry["company_work"] = refs.model_dump(mode="json", exclude_unset=True)
        # Slug assignment (ab-f82e8083). Runs on EVERY persisted mutation so any
        # node-creating path (intake / add / idea / decompose / advance) and any
        # legacy pre-slug node gets a stable, unique, title-derived handle under
        # the lock. Idempotent: a node that already carries a slug is untouched,
        # so this never rewrites a handle and re-runs are no-ops.
        from fno.graph.slug import ensure_slugs
        ensure_slugs(entries)
        entries = recompute_statuses(entries)
        # touched_at stamp (x-7dcb): compare against the post-recompute value,
        # not the raw mutator output, because status is derived from
        # completed_at/deferred_at/superseded_by and the pre-mutator value and
        # the post-recompute value are the only pair that mean the same thing.
        # A node absent from the pre-image is new; created_at already carries
        # that date, so it is left untouched rather than double-stamped.
        _now_iso = datetime.now(timezone.utc).isoformat()
        for entry in entries:
            if not isinstance(entry, dict):
                continue
            entry_id = entry.get("id")
            if not isinstance(entry_id, str) or entry_id not in _pre_curation:
                continue
            if _curation_key(entry) != _pre_curation[entry_id]:
                entry["touched_at"] = _now_iso
        # Node closure releases the node claim (x-94f8). A transition into a
        # terminal rung during THIS mutation is the one moment every closure
        # path shares - `backlog done` (either spelling), reconcile, the epic sweep,
        # and GraphTracker.close all persist through here, and the Rust daemon
        # shells out to `fno backlog done` - so the release lives here rather
        # than on any one caller, where the other N-1 paths would keep leaking.
        # The ids are only COLLECTED here; the release itself runs after the
        # flock drops, because it resolves claims roots (which can shell out
        # to git) and waits on recovery mutexes - none of that belongs inside
        # the untimeouted graph lock every backlog verb shares.
        from fno.graph.statuses import TERMINAL_RUNGS

        try:
            from fno.paths import graph_json as _configured_graph

            _owns_graph = path.resolve() == _configured_graph().resolve()
        except Exception:  # noqa: BLE001 - an unresolvable config owns nothing
            _owns_graph = False
        for entry in entries:
            if not isinstance(entry, dict):
                continue
            entry_id = entry.get("id")
            if not isinstance(entry_id, str):
                continue
            pre_rung = _status_normalized.get(entry_id)
            rung = entry.get("status")
            if rung not in TERMINAL_RUNGS or pre_rung in TERMINAL_RUNGS:
                # Not terminal now, new this mutation (no claim can predate
                # the node), or already terminal before it (no transition).
                continue
            # The lock mirror dies with the claim. session_id is NOT touched:
            # on a done node it is work/cost provenance, not a lock
            # (_normalize_lock_fields keeps it for exactly that reason).
            entry["locked_by"] = None
            entry["claimed_at"] = None
            # Only the process's CONFIGURED graph owns the global node-id
            # space its claims coordinate on. A scratch or explicitly-passed
            # graph (tests, capture flows) closing a node must not release a
            # same-id claim belonging to the configured graph's fleet.
            if _owns_graph:
                closure_releases.append((entry_id, str(rung)))
        # Status-forward key order + fresh children index. Runs after
        # recompute_statuses so status (top-level and inside child summaries)
        # is already current.
        entries = canonicalize_entries(entries)
        # Backup previous content BEFORE overwriting (so --revert has something
        # to fall back to).  No-op on first write when path does not yet exist.
        _backup = _create_backup(path)
        # Announced AFTER the backup, and worded on what it actually returned:
        # _create_backup swallows its own OSError, so promising a .bak before
        # attempting one can tell a user their rows are recoverable on exactly
        # the run where they are not.
        if _dropped > 0:
            _where = (
                f"prior content is preserved in {_backup.name}"
                if _backup is not None
                else "NO backup was written, so this removal is not recoverable"
            )
            print(
                f"Warning: dropping {_dropped} malformed graph "
                f"{'entry' if _dropped == 1 else 'entries'} (not a JSON object) "
                f"from {path}; {_where}",
                file=sys.stderr,
            )
        _write_json(entries, path)
        # Write SHA256 sidecar atomically after every successful mutation.
        _write_sha256_sidecar(path)
        # Resolve the .md/.html render targets. For the canonical graph.json
        # (the one `fno backlog view` and serve_board.py read) render to the
        # canonical GRAPH_MD/GRAPH_HTML so the served/opened board reflects
        # mutations even when config.paths.graph_json points outside
        # state_dir. For any other path -- a tmp graph.json in tests -- render
        # the siblings next to it so test runs never clobber the real
        # ~/.fno/graph.html the board server serves.
        from fno.graph._constants import GRAPH_HTML, GRAPH_JSON, GRAPH_MD
        try:
            is_canonical = path.resolve() == GRAPH_JSON.resolve()
        except OSError:
            is_canonical = False
        md_target = GRAPH_MD if is_canonical else path.with_name("graph.md")
        html_target = GRAPH_HTML if is_canonical else path.with_name("graph.html")
        # Emit Obsidian Kanban scaffolding only when an Obsidian vault is
        # configured; otherwise the frontmatter is inert noise (ab-917f813e).
        # Fail open: graph.json is already written by here, so a malformed
        # settings file (vault_root -> config/validation error, not just
        # OSError) must not crash the mutation. Default to no scaffolding.
        try:
            _obsidian = vault_root() is not None
        except Exception:
            _obsidian = False
        # recompute_statuses (above) does not derive `blocked` - it is a
        # read-time overlay - so re-apply it here before rendering, or a
        # mutation that newly blocks/unblocks a sibling renders stale in
        # graph.md/graph.html until the next explicit read.
        _apply_readiness_overlay(entries)
        try:
            render_graph_md(entries, md_target, obsidian=_obsidian)
        except OSError as e:
            # Only swallow IO errors (disk full, permission denied). Let
            # KeyError/TypeError/etc. surface so render bugs are visible
            # instead of silently producing a stale graph.md.
            print(f"Warning: graph.md render failed: {e}", file=sys.stderr)
        try:
            from fno.graph.render_html import render_graph_html
            render_graph_html(entries_with_archive(entries), html_target)
        except OSError as e:
            print(f"Warning: graph.html render failed: {e}", file=sys.stderr)
        # Wake the active-backlog drain daemon (node x-c070): a mutation may have
        # produced a fresh ready node. Best-effort; the daemon's poll floor is the
        # guarantee, so a failed touch is harmless and never wedges the mutation.
        try:
            from fno.active_backlog import touch_nudge
            touch_nudge()
        except Exception:
            pass
        result = entries
    finally:
        _release_flock(fd)
    # Claim releases run AFTER the flock drops: root resolution and recovery
    # mutexes never hold the graph lock (see the closure hook above).
    for entry_id, rung in closure_releases:
        release_node_claim_at_closure(entry_id, rung=rung)
    return result


def append_progress_note(
    path: Path, node_id: str, note: dict
) -> "tuple[bool, str | None]":
    """Append a ``{ts, text}`` progress note to a node's ``progress_notes``
    (append-only), returning ``(found, plan_path)``. Uses the sanctioned
    ``locked_mutate_graph`` path (NOT a forbidden direct write); shared by
    ``fno backlog note`` and the status-fanout backlog-progress adapter so the
    append logic lives in one place (x-2057)."""
    from fno.graph._intake import _find_node  # function-local: avoid import cycle

    result: dict = {"found": False, "plan_path": None}

    def mutator(entries: list[dict]) -> list[dict]:
        node = _find_node(entries, node_id)
        if node is not None:
            node.setdefault("progress_notes", []).append(note)
            result["found"] = True
            result["plan_path"] = node.get("plan_path")
        return entries

    locked_mutate_graph(path, mutator)
    return result["found"], result["plan_path"]


# Bounded ceiling for harness / session-id strings (x-b6e4). Real ids are UUIDs
# (~36) or short markers; 200 leaves headroom while rejecting a runaway value
# that would bloat the graph. ponytail: fixed cap, widen only if a real id
# legitimately exceeds it.
_SESSION_STR_MAX = 200


def _utc_session_stamp(label: str, value: str) -> str:
    """Normalize a session-row timestamp to the canonical ``...Z`` form.

    The timestamp contract is ISO-8601 *UTC*. ``fromisoformat`` alone would
    accept a date-only value, a naive datetime, or a non-UTC offset -- all of
    which break append-order comparison and a future evidence-based backfill. So
    require a tz-aware instant whose offset is exactly UTC. Shared by the append
    and rollback primitives so a rollback's ``started_at`` compares equal to the
    value the append wrote rather than missing on a formatting difference.
    """
    try:
        parsed = datetime.fromisoformat(value.strip().replace("Z", "+00:00"))
    except ValueError as exc:
        raise ValueError(
            f"{label} must be an ISO-8601 timestamp, got {value!r}"
        ) from exc
    if parsed.tzinfo is None or parsed.utcoffset() != timedelta(0):
        raise ValueError(
            f"{label} must be a UTC timestamp (offset +00:00 / Z), got {value!r}"
        )
    return parsed.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _validate_session_identity(phase: str, harness: str, session_id: str) -> "tuple[str, str]":
    """Validate a session row's identity triple, returning the stripped pair."""
    from fno.graph.types import SESSION_PHASES

    if phase not in SESSION_PHASES:
        raise ValueError(
            f"invalid phase {phase!r}; expected one of {sorted(SESSION_PHASES)}"
        )
    harness = (harness or "").strip()
    session_id = (session_id or "").strip()
    for label, value in (("harness", harness), ("session_id", session_id)):
        if not value:
            raise ValueError(f"{label} must be a non-empty string")
        if len(value) > _SESSION_STR_MAX:
            raise ValueError(f"{label} exceeds {_SESSION_STR_MAX} chars")
    return harness, session_id


# A full session id is a uuid (32 hex, 36 with dashes); anything shorter is the
# 8-hex prefix form the transcript resolver glob-matches. See _observe_model.
_FULL_SESSION_ID_MIN = 32


def _observe_model(harness: str, session_id: str) -> dict:
    """What the row's session ACTUALLY answered as, read from its own transcript.

    Delegates to :func:`fno.provenance.observed.observed_model` - the one truth
    source, never a second one - and returns its variant dict UNCHANGED, so the
    row records ``{"kind": "observed", "model": ..., "samples": N}`` and never a
    bare model string. Flattening would erase the difference between "seen on N
    samples" and "assumed", which is the entire reason the field exists: an
    unknown that looks like an observation is worse than an absent field,
    because a reader spends it as evidence.

    Resolution is keyed on ``session_id`` ALONE: claude's resolver requires a
    non-empty cwd but deliberately does not scope the search by it (x-a472 - a
    transcript is re-keyed into the worktree's project dir on EnterWorktree and
    a stub is left behind in the other, so trusting one slug goes blind exactly
    when a bg worker enters its worktree). ``os.getcwd()`` therefore only has to
    be non-empty, and a row stamped for a session other than the writing process
    still resolves to the right transcript rather than to nothing.

    A PREFIX-shaped id is refused before any read. The resolver accepts an 8-hex
    prefix and glob-matches it; when two sessions in the store share that prefix
    it takes the first-sorted one and the ambiguity is not visible through this
    call. A model read off the wrong session is precisely the partial truth read
    as total that this field exists to prevent, so it is declined by name.

    The import is function-local like ``_find_node`` below: ``fno.graph`` takes
    no module-level dependency on ``fno.agents``. Never raises - a provenance
    field must not fail a claim release - and a resolver that blows up is
    ``unreadable`` rather than a swallowed ``no-transcript``, because a crash
    and an absent file are different facts and keeping those apart is this
    field's whole job.
    """
    try:
        from fno.provenance.observed import observed_model, observed_model_for_session

        # Ask the reader whether this harness is file-backed at all BEFORE the
        # id-shape guard: opencode ids are 30 chars and gemini keeps no
        # transcript, so a length test applied first would report a full id as
        # "prefix-shaped" and send an operator hunting for a broken store that
        # does not exist. Probed rather than hardcoded so the file-backed set
        # stays owned by the reader.
        not_backed = observed_model(harness, None)
        if not_backed.get("kind") == "not-file-backed":
            return not_backed
        if len(session_id) < _FULL_SESSION_ID_MIN:
            return {
                "kind": "unreadable",
                "reason": f"session id {session_id!r} is prefix-shaped; "
                          "a glob match cannot be proven to be this session",
            }
        # NOT resolve_transcript_path + observed_model: that pair collapses a
        # failed resolution to None and reports it as no-transcript, so a
        # permissions error or a drifted store would be recorded as "this
        # session has no transcript yet". The _for_session form keeps the
        # resolver's failure reason and returns unreadable.
        return observed_model_for_session(harness, session_id, os.getcwd())
    except Exception as exc:  # noqa: BLE001 — a reporting field never breaks a stamp
        return {"kind": "unreadable", "reason": f"{type(exc).__name__}: {exc}"}


def _merge_observed_model(prior: "dict | None", fresh: dict) -> "dict | None":
    """The value a re-stamp should write to ``observed_model``, or None to keep.

    The timestamps on this row are owned by the FIRST observation; this field is
    owned by the LATEST one, and the inversion is deliberate. A session can
    change model mid-run - account failover and provider rotation both do it -
    so a row that kept the first reading would record the cheap lane for a phase
    that failed over to the expensive one. That is the error this field exists
    to catch, one level in: a partial truth read as total.

    So a later observation wins, and when it DISAGREES the row stops claiming to
    be a single-model phase::

        {"kind": "observed-multiple", "model": <latest>, "samples": N,
         "prior_models": [<older>, ...]}

    A distinct ``kind`` rather than an extra key on ``observed``: every reader
    that switches on ``kind == "observed"`` would read a sibling key as a clean
    single-model observation and the annotation would never fire, which is the
    same shape as a guard placed on one of several reachable paths. ``model``
    stays the latest so an uninspecting read still gets the least-wrong single
    value.

    An unknown NEVER displaces a recording (a transcript reaped between the two
    stamps must not erase what the first read saw), but a recorded unknown is
    upgraded by a real observation: the ``do`` row opens at claim acquire, early
    enough to honestly read ``no-model-yet``, and closes at release when the
    transcript is thick with evidence. Without the upgrade the most
    cost-relevant row in the graph would permanently record an unknown.

    KNOWN CEILING: one :func:`observed_model` read returns the LAST model in its
    tail window, not a set, so a failover inside a single stamp's window is
    invisible here. ``observed-multiple`` can only ever surface a change that
    straddles two stamps (acquire, then release). The field answers "what did
    the last look see, and did it disagree with the one before", which is
    narrower than "every model that served this phase".
    """
    if prior is None:
        return fresh
    if fresh.get("kind") != "observed":
        return None
    if prior.get("kind") not in {"observed", "observed-multiple"}:
        return fresh
    seen = list(prior.get("prior_models") or [])
    if prior.get("model") == fresh.get("model"):
        # Same model again. A row that has ALREADY recorded a disagreement stays
        # observed-multiple: three stamps are reachable (acquire, release, plus
        # a target-start re-acquire or a retried session add), and a third look
        # landing back on the model of the second must not erase the first. A
        # recorded failover that reverts to a clean single-model claim is the
        # partial truth read as total, one more level in.
        return {**fresh, "kind": prior["kind"], **({"prior_models": seen} if seen else {})}
    if prior.get("model") and prior["model"] not in seen:
        seen.append(prior["model"])
    return {**fresh, "kind": "observed-multiple", "prior_models": seen}


def append_session_record(
    path: Path,
    node_id: str,
    *,
    phase: str,
    harness: str,
    session_id: str,
    ended_at: "str | None" = None,
    started_at: "str | None" = None,
) -> "tuple[bool, bool]":
    """Append a ``{phase, harness, session_id, ended_at, started_at,
    observed_model}`` lifecycle record to a node's append-only ``sessions``
    list, returning ``(found, added)`` (x-b6e4).

    The single graph-owned mutation primitive behind ``fno backlog session add``.
    Idempotent under the graph lock: appends only when ``(phase, harness,
    session_id)`` is absent. A duplicate key collapses to one row whose
    timestamps the first observation owns, with one completion: a later stamp
    may FILL a timestamp the first omitted (``started_at`` / ``ended_at``), so a
    row opened at claim acquire (started_at, no end) closes at release (ended_at
    filled) without losing either value. A value already set is never
    overwritten, and this function never removes a row - the single compensating
    write is :func:`remove_open_session_record`, whose preconditions are narrow
    enough that only a row this function opened moments ago can match.

    ``started_at`` / ``ended_at`` are optional and bound the phase window. Both
    are omitted unless the writer has an honest value: ``ended_at`` is the phase
    END (not the stamp-fire time - defaulting to now would name that instant as
    the end, the receipt-can-lie failure under an honest name), so a row opened
    mid-session with no recorded end omits it. The canonical keys are
    ``started_at`` / ``ended_at``; ``claimed_at`` and ``at`` are the legacy keys
    older rows carry and the reader accepts forever, but no row is rewritten -
    new rows always write the canonical names. Neither is part of the
    idempotency key.

    ``observed_model`` answers the question the other four fields cannot: the
    operator routes phases by model to control cost, and ``harness`` names the
    binary, not the model it answered as. A route stamped at spawn records
    INTENT, so it reports the intended model in exactly the case an operator
    suspects a silent fallback (an ``ANTHROPIC_MODEL`` surviving without its
    ``ANTHROPIC_BASE_URL`` bills the expensive lane while every receipt says the
    cheap one). This reads the session's own transcript instead. The whole
    variant dict from :func:`fno.provenance.observed.observed_model` is stored,
    never a flattened string; see :func:`_observe_model` for why, and
    :func:`_merge_observed_model` for the one field a re-stamp OVERWRITES and
    for the sixth kind (``observed-multiple``) this writer can produce that the
    reader never returns. The read is best-effort and cannot fail a stamp.

    Existing rows are NOT backfilled. A reaped transcript cannot be read, and a
    guessed value is the failure the field exists to prevent - so a row written
    before this field stays without it, and the absent key is itself the honest
    signal that nobody looked.

    Raises ``ValueError`` on an unknown phase, an empty/over-long harness or
    session id, or an unparseable ``ended_at``/``started_at`` -- validation lives
    here so every caller (CLI, tests, future backfill) is bound by the same
    contract.
    ``found=False`` when the node is absent (no mutation).
    """
    from fno.graph._intake import _find_node  # function-local: avoid import cycle

    harness, session_id = _validate_session_identity(phase, harness, session_id)

    # ended_at is the phase END; omitted when the writer has no honest end to
    # record (a row opened mid-session has a start and no end). Defaulting to the
    # stamp-fire time would name that instant as the phase end - the receipt-can-
    # lie failure under an honest name - so it is absent, not faked.
    if ended_at is not None:
        ended_at = _utc_session_stamp("ended_at", ended_at)
    if started_at is not None:
        started_at = _utc_session_stamp("started_at", started_at)

    # Read the transcript BEFORE the graph lock: the mutator runs under
    # _acquire_flock and this is not a cheap read - claude resolution globs every
    # dir under the projects root and the model read seeks a 256KB tail, with a
    # full streaming scan on the rare inconclusive one. It is paid on every call
    # including the duplicate path and a missing node, which puts it on every
    # `fno agents claim acquire` / `release`; that is affordable next to the graph
    # rewrite those already do, but it does not belong inside the lock.
    observed = _observe_model(harness, session_id)

    result = {"found": False, "added": False}

    def mutator(entries: list[dict]) -> list[dict]:
        node = _find_node(entries, node_id)
        if node is None:
            return entries
        result["found"] = True
        rows = node.setdefault("sessions", [])
        key = (phase, harness, session_id)
        prior = next(
            (r for r in rows
             if (r.get("phase"), r.get("harness"), r.get("session_id")) == key),
            None,
        )
        if prior is not None:
            # Duplicate: first observation owns what it set, but a later stamp
            # can COMPLETE the row by filling a timestamp it left open. A do row
            # opened at claim acquire (started_at, no ended_at) is closed at
            # release (ended_at filled); a value already present is never
            # overwritten, so a retried stamp with a different timestamp is a
            # no-op and the first observation still wins.
            if ended_at is not None and "ended_at" not in prior:
                prior["ended_at"] = ended_at
            if started_at is not None and "started_at" not in prior:
                prior["started_at"] = started_at
            # observed_model is the one field the LATEST stamp owns rather than
            # the first, so a mid-run failover is not recorded as the lane the
            # phase started on. See _merge_observed_model.
            merged = _merge_observed_model(prior.get("observed_model"), observed)
            if merged is not None:
                prior["observed_model"] = merged
            return entries
        # Annotated: observed_model is a dict, so the inferred dict[str, str]
        # from the three string fields would reject it.
        row: dict[str, object] = {
            "phase": phase, "harness": harness, "session_id": session_id}
        if ended_at is not None:
            row["ended_at"] = ended_at
        if started_at is not None:
            row["started_at"] = started_at
        # Written unconditionally, including the unknown kinds: an ABSENT key
        # means this row predates the field or its writer never looked, while
        # {"kind": "no-transcript"} means the writer looked and found nothing.
        # Those are different facts, and so are no-transcript (not yet) and
        # not-file-backed (never will be, opencode keeps no per-session file).
        row["observed_model"] = observed
        rows.append(row)
        result["added"] = True
        return entries

    locked_mutate_graph(path, mutator)
    return result["found"], result["added"]


def remove_open_session_record(
    path: Path,
    node_id: str,
    *,
    phase: str,
    harness: str,
    session_id: str,
    started_at: str,
) -> "tuple[bool, bool]":
    """Remove the still-OPEN lifecycle row an acquire opened, returning
    ``(found, removed)`` - the one compensating write against the otherwise
    append-only ``sessions`` list.

    A claim acquired purely as a serialization point is not evidence of work: if
    the post-acquire re-check refuses the worker, the row it opened claims a
    phase that never ran and the node reads as permanently in progress. So the
    refusal path rolls its own row back.

    Four preconditions must ALL hold before a row is dropped, which is what keeps
    this from eating real provenance:

    * the ``(phase, harness, session_id)`` key matches,
    * the row carries no ``ended_at`` -- a closed row recorded a finished window
      and is never touched,
    * its ``started_at`` is present and equals ``started_at`` exactly.

    That last clause is the one that matters. The dangerous case is a session
    that already did real work on this node under the same identity, was killed,
    and then re-acquired and got refused: an idempotent re-acquire refreshes the
    claim's ``acquired_at`` while the row keeps the FIRST observation's
    ``started_at`` (``append_session_record`` never overwrites), so the two
    disagree and the earlier real row survives its successor's rollback.

    ``found=False`` when the node is absent (no mutation). Raises ``ValueError``
    under the same identity/timestamp contract as ``append_session_record``.
    """
    from fno.graph._intake import _find_node  # function-local: avoid import cycle

    harness, session_id = _validate_session_identity(phase, harness, session_id)
    started_at = _utc_session_stamp("started_at", started_at)

    result = {"found": False, "removed": False}

    def mutator(entries: list[dict]) -> list[dict]:
        node = _find_node(entries, node_id)
        if node is None:
            return entries
        result["found"] = True
        rows = node.get("sessions") or []
        keep = [
            r for r in rows
            if not (
                (r.get("phase"), r.get("harness"), r.get("session_id"))
                == (phase, harness, session_id)
                and "ended_at" not in r
                and r.get("started_at") == started_at
            )
        ]
        if len(keep) != len(rows):
            node["sessions"] = keep
            result["removed"] = True
        return entries

    locked_mutate_graph(path, mutator)
    return result["found"], result["removed"]


def reap_open_session_record(
    path: Path,
    node_id: str,
    *,
    phase: str,
    harness: str,
    session_id: str,
    ended_at: "str | None" = None,
) -> dict:
    """Close one exact open observer-owned session row and report settlement.

    Unlike rollback, this operation has positive death evidence from the
    observer and therefore does not require the row's ``started_at`` value.
    Closed provenance is immutable, and only an unfinished row with the exact
    identity can be closed.

    Two close semantics by phase (x-4342). ``do`` REMOVES the row: an open do
    window wedges node status in_progress, so after death the honest state is
    "no do window", which removal restores. Every other phase FILLS
    ``ended_at`` and keeps the row: a spawn-opened review row exists precisely
    to record that a reviewer session ran, and the work it records did happen -
    erasing it would undo the provenance the row was opened for. ``all`` is the
    death-cascade spelling the daemon observer uses: apply BOTH semantics to
    every open row carrying the identity, so a session holding a do window and
    a review window at death settles both in one call. The fill value defaults
    to the reap instant, an UPPER BOUND on the true end (the observer proves
    "dead by now", not "died at"), and a caller with a sharper bound passes it.
    """
    from datetime import datetime, timezone

    from fno.graph._intake import _find_node
    from fno.graph.statuses import is_open_do_row, is_open_phase_row
    from fno.graph.types import SESSION_PHASES

    if phase != "all" and phase not in SESSION_PHASES:
        raise ValueError(
            f"invalid phase {phase!r}; expected 'all' or one of {sorted(SESSION_PHASES)}"
        )
    close_phases = sorted(SESSION_PHASES - {"do"}) if phase == "all" else (
        [] if phase == "do" else [phase]
    )
    remove_do = phase in ("do", "all")
    harness, session_id = _validate_session_identity(
        "do" if phase == "all" else phase, harness, session_id
    )
    if ended_at is None:
        ended_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    else:
        ended_at = _utc_session_stamp("ended_at", ended_at)
    result = {
        "found": False,
        "settled": False,
        "row_removed": False,
        "row_closed": False,
        "status_before": None,
        "status_after": None,
        "remaining_open_do": 0,
    }

    def mutator(entries: list[dict]) -> list[dict]:
        node = _find_node(entries, node_id)
        if node is None:
            return entries
        result["found"] = True
        result["status_before"] = node.get("status")
        rows = node.get("sessions") or []
        if remove_do:
            keep = [
                row for row in rows
                if not (
                    is_open_do_row(row)
                    and (row.get("harness"), row.get("session_id"))
                    == (harness, session_id)
                )
            ]
            result["row_removed"] = len(keep) != len(rows)
            rows = keep
            if result["row_removed"]:
                node["sessions"] = keep
        for close_phase in close_phases:
            for row in rows:
                if (
                    is_open_phase_row(row, close_phase)
                    and (row.get("harness"), row.get("session_id"))
                    == (harness, session_id)
                ):
                    # Fill, never overwrite: a row that somehow already carries
                    # an ended_at is not open and stays untouched.
                    row.setdefault("ended_at", ended_at)
                    result["row_closed"] = True
        return entries

    updated = locked_mutate_graph(path, mutator)
    node = _find_node(updated, node_id)
    if node is None:
        return result
    rows = node.get("sessions") or []
    result["status_after"] = node.get("status")
    result["remaining_open_do"] = sum(is_open_do_row(row) for row in rows)
    result["settled"] = True
    return result


def _node_carries_pr(node: dict, pr_number: int) -> bool:
    """True if the node's primary pr_number OR any additional_prs entry == pr_number."""
    if node.get("pr_number") == pr_number:
        return True
    return any(
        isinstance(extra, dict) and extra.get("number") == pr_number
        for extra in (node.get("additional_prs") or [])
    )


def _node_pr_urls(node: dict) -> "list[str]":
    """The node's PR urls (primary + additional_prs), unparsed."""
    urls = [node.get("pr_url")]
    urls += [
        extra.get("url")
        for extra in (node.get("additional_prs") or [])
        if isinstance(extra, dict)
    ]
    return [u for u in urls if isinstance(u, str)]


def _node_matches_repo_pr(node: dict, pr_number: int, repo: str) -> bool:
    """True if any of the node's PR urls resolves to exactly ``(repo, pr_number)``.

    Repo-scoped narrowing (x-d5f9): ``pr_number`` is not unique across repos, so
    a bare-number match fans out on footnote's cross-project graph; the url is
    the only per-node field carrying the repo slug. ``repo`` is an
    ``<owner>/<repo>`` slug. A node with a ``pr_number`` but no url (legacy,
    pre pr_url stamp) is unattributable and never matches - refusing to guess is
    correct, not a regression.
    """
    want = repo.lower()
    for url in _node_pr_urls(node):
        # Footnote's own stamps are canonical, but a hand-passed url may carry a
        # query, fragment, or trailing slash; the repo slug is case-insensitive.
        clean = url.split("?", 1)[0].split("#", 1)[0].rstrip("/")
        head, sep, tail = clean.rpartition("/pull/")
        if not sep:
            continue
        try:
            if int(tail) == pr_number and head.lower().endswith("/" + want):
                return True
        except ValueError:
            continue
    return False


def find_nodes_for_pr(
    path: Path, pr_number: int, *, repo: "str | None" = None
) -> "list[str]":
    """Node ids carrying ``pr_number``, optionally narrowed to one repo slug.

    Split out of :func:`stamp_session_for_pr` so a caller reporting an ambiguous
    resolution can name the candidates without re-deriving the match rule.
    """
    return [
        e["id"] for e in read_graph(path)
        if isinstance(e.get("id"), str)
        and (
            _node_matches_repo_pr(e, pr_number, repo)
            if repo
            else _node_carries_pr(e, pr_number)
        )
    ]


def stamp_session_for_pr(
    path: Path,
    pr_number: int,
    *,
    phase: str,
    harness: str,
    session_id: str,
    ended_at: "str | None" = None,
    started_at: "str | None" = None,
    repo: "str | None" = None,
) -> "tuple[str | None, str]":
    """Resolve the UNIQUE node carrying ``pr_number`` and append a lifecycle
    record, returning ``(node_id, status)`` (x-b6e4).

    The shared PR->node stamp used by ``fno backlog session add --pr``, the merge
    primitive, and the ``/pr merged`` ritual, so Locked Decision 9 ("resolve
    exactly one same-repo PR-linked node, never fan out") lives in one place.
    ``status`` is ``added`` | ``duplicate`` | ``no-node`` | ``ambiguous``; the
    last two leave the graph untouched (0 or >1 matches never fans out).

    ``repo`` (an ``<owner>/<repo>`` slug, x-d5f9) scopes resolution to one repo:
    ``pr_number`` alone collides across repos in a cross-project graph, so a
    caller that knows its repo passes it to match only nodes whose ``pr_url``
    (primary or an ``additional_prs`` entry) is that exact PR. ``repo=None``
    preserves the bare-``pr_number`` match (single-repo / manual / tests); the
    repo-scoped set is strictly narrower, so it never introduces a false match.
    """
    matches = find_nodes_for_pr(path, pr_number, repo=repo)
    if not matches:
        return None, "no-node"
    if len(matches) > 1:
        return None, "ambiguous"
    node_id = matches[0]
    _found, added = append_session_record(
        path, node_id, phase=phase, harness=harness, session_id=session_id,
        ended_at=ended_at, started_at=started_at,
    )
    return node_id, ("added" if added else "duplicate")
