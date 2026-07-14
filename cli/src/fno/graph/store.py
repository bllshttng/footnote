"""Graph store: flock helpers, JSON I/O, and the locked read-modify-write cycle.

Public API:
    _acquire_flock, _release_flock  - raw flock operations
    _read_json, _write_json         - raw JSON I/O (callers hold lock for writes)
    _apply_graph_defaults           - lazy migration defaults for ab- entries
    read_graph                      - unlocked read with defaults applied
    locked_mutate_graph             - locked read-modify-write with status recompute
    GraphCorruptError               - raised on unparseable graph.json

Sidecar / backup protocol (Layer 2 hygiene):
    After every successful atomic write locked_mutate_graph:
      1. Creates a timestamped backup of the PREVIOUS content: graph.json.bak.<ts>
      2. Writes a SHA256 sidecar: graph.json.sha256
    Backups are pruned to GRAPH_BACKUP_KEEP (10) most-recent files.
"""
from __future__ import annotations

import fcntl
import hashlib
import json
import os
import shutil
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

from fno.graph._constants import GRAPH_JSON, GRAPH_LOCK_FILE, GRAPH_MD

# Keep at most this many timestamped backups on disk.
GRAPH_BACKUP_KEEP = 10

# Canonical key order for serialized graph entries. Status-forward: a human
# scanning graph.json (or `fno backlog get`) sees the node's id and derived
# _status before anything else, then the human-scan fields (title/priority),
# then the parent/children relationship, then the long lifecycle/provenance
# tail. Keys NOT in this list are appended in their original order after the
# canonical block, so forward-compat schema additions and legacy extras (e.g.
# the old `points` field) are reordered-around, never dropped.
CANONICAL_FIELD_ORDER: list[str] = [
    "id",
    "_status",
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
    "completed_at",
    "deferred_at",
    "deferred_reason",
    "has_brief",
    "roadmap_id",
    "vision_path",
    "details",
    "size",
    "batch",
    "cost_usd",
    "cost_sessions",
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
CHILD_SUMMARY_FIELDS: tuple[str, ...] = ("id", "title", "project", "_status")


def _compute_children(entries: list[dict]) -> list[dict]:
    """Populate each entry's ``children`` with summaries of its direct children.

    The inverse of the ``parent`` pointer, rebuilt from scratch on every call so
    it can never drift: any change to a child is itself a mutation, and
    ``locked_mutate_graph`` runs this over the whole list on each write. A
    ``parent`` pointing at a non-existent id is ignored (no phantom summary).
    Mutates entries in place and returns the same list.
    """
    valid_ids = {e["id"] for e in entries if isinstance(e.get("id"), str)}
    kids: dict[str, list[dict]] = {}
    for e in entries:
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
            kids.setdefault(parent, []).append(
                {k: e.get(k) for k in CHILD_SUMMARY_FIELDS}
            )
    for e in entries:
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
    ``locked_mutate_graph`` after ``recompute_statuses`` so ``_status`` and the
    child summaries' ``_status`` are already current.
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


def _create_backup(path: Path) -> None:
    """Copy current graph.json to a timestamped backup, then prune old backups.

    Backups are named graph.json.bak.<ISO-timestamp-no-colons>.
    Keeps GRAPH_BACKUP_KEEP most-recent entries; prunes the rest.
    No-op if graph.json does not yet exist (first write).
    """
    if not path.exists():
        return

    ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%f")
    backup = path.parent / f"{path.name}.bak.{ts}"
    try:
        shutil.copy2(path, backup)
    except OSError as e:
        print(f"Warning: graph backup failed: {e}", file=sys.stderr)
        return

    # Prune: keep only the GRAPH_BACKUP_KEEP most-recent .bak.* files
    existing = sorted(path.parent.glob(f"{path.name}.bak.*"))
    to_prune = existing[:-GRAPH_BACKUP_KEEP] if len(existing) > GRAPH_BACKUP_KEEP else []
    for old in to_prune:
        try:
            old.unlink()
        except OSError:
            pass


def _apply_graph_defaults(entries: list[dict]) -> list[dict]:
    """Apply lazy migration defaults to graph entries (ab- IDs)."""
    # In-memory legacy priority backfill so read-only commands (ready,
    # next, status, tree, triage context) sort correctly *before* the
    # first write triggers recompute_statuses' on-disk backfill. The
    # mutate path still rewrites priority on disk; this just keeps the
    # read path honest in the gap before that happens.
    from fno.graph._constants import PRIORITY_MIGRATION
    for e in entries:
        old_priority = e.get("priority")
        if old_priority in PRIORITY_MIGRATION:
            e["priority"] = PRIORITY_MIGRATION[old_priority]
    for e in entries:
        e.setdefault("parent", None)
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
        e.setdefault("_status", "ready")
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
        e.setdefault("supersedes", [])
        e.setdefault("superseded_by", None)
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
        # Queued: orthogonal to _status. A queued node is still ready (has a
        # plan, unblocked); the queued_at field marks the user's intent to
        # pick it up next/today. Cleared on completion.
        e.setdefault("queued_at", None)
        e.setdefault("queued_reason", None)
    # Populate locked_by (legacy nodes adopt their session_id) so readers and
    # status derivation see the canonical field. Runs last so the key-presence
    # rule still sees a pre-rename node's missing locked_by key.
    _normalize_lock_fields(entries)
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
    except json.JSONDecodeError:
        backup = path.with_suffix(".json.bak")
        try:
            shutil.copy2(path, backup)
            print(f"Warning: {path} is corrupt, backup saved to {backup}", file=sys.stderr)
        except OSError as e:
            print(f"Warning: {path} is corrupt, backup also failed: {e}", file=sys.stderr)
        raise GraphCorruptError(str(path))
    return data.get("entries", [])


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
    """
    try:
        return _apply_graph_defaults(_read_json(path))
    except GraphCorruptError:
        return []


def locked_mutate_graph(path: Path, mutator) -> list[dict]:
    """Locked read-modify-write for graph entries. Recomputes statuses after mutation."""
    # Import here to avoid circular imports
    from fno.graph.statuses import recompute_statuses
    from fno.graph.render import render_graph_md
    from fno.paths import vault_root

    path.parent.mkdir(parents=True, exist_ok=True)
    fd = _acquire_flock(GRAPH_LOCK_FILE)
    try:
        try:
            raw = _read_json(path)
        except GraphCorruptError:
            print(f"Error: {path} appears corrupt (backup at {path.with_suffix('.json.bak')}). "
                  f"Restore from backup or delete before proceeding.", file=sys.stderr)
            sys.exit(1)
        entries = _apply_graph_defaults(raw)
        entries = mutator(entries)
        # Slug assignment (ab-f82e8083). Runs on EVERY persisted mutation so any
        # node-creating path (intake / add / idea / decompose / advance) and any
        # legacy pre-slug node gets a stable, unique, title-derived handle under
        # the lock. Idempotent: a node that already carries a slug is untouched,
        # so this never rewrites a handle and re-runs are no-ops.
        from fno.graph.slug import ensure_slugs
        ensure_slugs(entries)
        entries = recompute_statuses(entries)
        # Status-forward key order + fresh children index. Runs after
        # recompute_statuses so _status (top-level and inside child summaries)
        # is already current.
        entries = canonicalize_entries(entries)
        # Backup previous content BEFORE overwriting (so --revert has something
        # to fall back to).  No-op on first write when path does not yet exist.
        _create_backup(path)
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
        try:
            render_graph_md(entries, md_target, obsidian=_obsidian)
        except OSError as e:
            # Only swallow IO errors (disk full, permission denied). Let
            # KeyError/TypeError/etc. surface so render bugs are visible
            # instead of silently producing a stale graph.md.
            print(f"Warning: graph.md render failed: {e}", file=sys.stderr)
        try:
            from fno.graph.render_html import render_graph_html
            render_graph_html(entries, html_target)
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
        return entries
    finally:
        _release_flock(fd)


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


def append_session_record(
    path: Path,
    node_id: str,
    *,
    phase: str,
    harness: str,
    session_id: str,
    at: "str | None" = None,
) -> "tuple[bool, bool]":
    """Append a ``{phase, harness, session_id, at}`` lifecycle record to a node's
    append-only ``sessions`` list, returning ``(found, added)`` (x-b6e4).

    The single graph-owned mutation primitive behind ``fno backlog session add``.
    Idempotent under the graph lock: appends only when ``(phase, harness,
    session_id)`` is absent, so a retried or concurrent duplicate stamp collapses
    to one row and the first observation owns ``at``. Never edits or removes an
    entry.

    Raises ``ValueError`` on an unknown phase, an empty/over-long harness or
    session id, or an unparseable ``at`` -- validation lives here so every caller
    (CLI, tests, future backfill) is bound by the same contract. ``found=False``
    when the node is absent (no mutation).
    """
    from fno.graph._intake import _find_node  # function-local: avoid import cycle
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

    if at is None:
        at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    else:
        # The `at` contract is ISO-8601 *UTC*. fromisoformat alone would accept a
        # date-only value, a naive datetime, or a non-UTC offset -- all of which
        # break append-order comparison and a future evidence-based backfill. So
        # require a tz-aware instant whose offset is exactly UTC, then normalize
        # to the canonical `...Z` form the default path emits.
        raw = at.strip()
        try:
            parsed = datetime.fromisoformat(raw.replace("Z", "+00:00"))
        except ValueError as exc:
            raise ValueError(f"at must be an ISO-8601 timestamp, got {at!r}") from exc
        if parsed.tzinfo is None or parsed.utcoffset() != timedelta(0):
            raise ValueError(f"at must be a UTC timestamp (offset +00:00 / Z), got {at!r}")
        at = parsed.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    result = {"found": False, "added": False}

    def mutator(entries: list[dict]) -> list[dict]:
        node = _find_node(entries, node_id)
        if node is None:
            return entries
        result["found"] = True
        rows = node.setdefault("sessions", [])
        key = (phase, harness, session_id)
        if any((r.get("phase"), r.get("harness"), r.get("session_id")) == key for r in rows):
            return entries  # duplicate: first observation owns `at`
        rows.append({"phase": phase, "harness": harness,
                     "session_id": session_id, "at": at})
        result["added"] = True
        return entries

    locked_mutate_graph(path, mutator)
    return result["found"], result["added"]


def _node_carries_pr(node: dict, pr_number: int) -> bool:
    """True if the node's primary pr_number OR any additional_prs entry == pr_number."""
    if node.get("pr_number") == pr_number:
        return True
    return any(
        isinstance(extra, dict) and extra.get("number") == pr_number
        for extra in (node.get("additional_prs") or [])
    )


def _node_matches_repo_pr(node: dict, pr_number: int, repo: str) -> bool:
    """True if any of the node's PR urls resolves to exactly ``(repo, pr_number)``.

    Repo-scoped narrowing (x-d5f9): ``pr_number`` is not unique across repos, so
    a bare-number match fans out on footnote's cross-project graph; the url is
    the only per-node field carrying the repo slug. ``repo`` is an
    ``<owner>/<repo>`` slug. A node with a ``pr_number`` but no url (legacy,
    pre pr_url stamp) is unattributable and never matches - refusing to guess is
    correct, not a regression.
    """
    urls = [node.get("pr_url")]
    urls += [
        extra.get("url")
        for extra in (node.get("additional_prs") or [])
        if isinstance(extra, dict)
    ]
    for url in urls:
        if not isinstance(url, str):
            continue
        head, sep, tail = url.rpartition("/pull/")
        if sep and tail == str(pr_number) and head.endswith("/" + repo):
            return True
    return False


def stamp_session_for_pr(
    path: Path,
    pr_number: int,
    *,
    phase: str,
    harness: str,
    session_id: str,
    at: "str | None" = None,
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
    matches = [
        e["id"] for e in read_graph(path)
        if isinstance(e.get("id"), str)
        and (
            _node_matches_repo_pr(e, pr_number, repo)
            if repo
            else _node_carries_pr(e, pr_number)
        )
    ]
    if not matches:
        return None, "no-node"
    if len(matches) > 1:
        return None, "ambiguous"
    node_id = matches[0]
    _found, added = append_session_record(
        path, node_id, phase=phase, harness=harness, session_id=session_id, at=at
    )
    return node_id, ("added" if added else "duplicate")
