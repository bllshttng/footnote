"""Core logic for `fno carveout add` - capture left-out work to a session ledger.

A carve-out is a raw signal that the executor consciously left work undone
mid-implementation: a deferred decision (blocked on an open question), an
out-of-scope bug found while building something else, or a data backfill the
just-landed PR enables (blocked on a precondition). It is NOT a backlog node
- the later retro-triage harvest reads these, dedups, classifies, and decides
whether each becomes a node (active or queued) or an inbox line. Keeping the
raw-signal/triaged-node line crisp is why this lives under `fno carveout`, not
`fno backlog` (Locked Decision #10).

The ``backfill`` kind is special-cased downstream: the generic retro harvest
SKIPS it (``retro.harvest.harvest_carveouts``) so it SURVIVES untouched for
``/fno:pr merged``'s backfill slot, which reads it via
:func:`read_carveouts` and removes it via :func:`consume_carveouts` once run or
filed as a node (ab-4a1a4fea, Group 3).

Records append one JSON line to ``.fno/carveouts.jsonl`` using the same
mkdir-mutex + append convention as the events.jsonl writer
(``fno.events.append_event``), so concurrent writers serialize per line.
"""
from __future__ import annotations

import json
import time as _time
import uuid
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional, Tuple

# kind is a closed enum: a deferred decision, an out-of-scope bug, or a data
# backfill the merged PR enables. ``backfill`` is consumed by /pr merged, not
# the generic retro harvest (ab-4a1a4fea).
BACKFILL_KIND = "backfill"
VALID_KINDS: Tuple[str, ...] = ("deferred", "oos-bug", BACKFILL_KIND)
# Severity is orthogonal to kind: how much a left-out item matters. Routed to
# priority p0..p3 by retro.classify.severity_to_priority. None keeps today's p3
# default, so existing records and hand-filed carveouts parse unchanged.
VALID_SEVERITIES: Tuple[str, ...] = ("critical", "high", "medium", "low")

# Max description length retained on disk. Oversized descriptions are TRUNCATED
# (never rejected) so capture is never lost; the marker records the original
# length. A carve-out body larger than this is almost certainly a paste mistake,
# and the triage harvest re-derives the real reasoning from the cited source.
DESCRIPTION_CAP = 8000

CARVEOUTS_NAME = "carveouts.jsonl"


def resolve_carveout_root() -> Path:
    """Resolve the root that owns the carveouts ledger: the CANONICAL (main)
    worktree, never the per-worktree ``--show-toplevel`` root.

    Carve-outs are SHARED project state, like ``ledger.json`` / ``graph.json``
    - not per-session state like ``target-state.md``. A carve-out captured
    inside a linked worktree must survive that worktree's archival so the
    retro-triage harvest (which runs from the main checkout at merge) can still
    find it; ``setup-worktree.sh`` does NOT symlink ``carveouts.jsonl``, so a
    worktree-local ledger is simply lost on teardown (ab-44408b6e). Resolving
    the PATH to canonical (vs symlinking the file) also co-locates the ledger
    with its ``.lock.d`` mutex, so concurrent writers across worktrees actually
    serialize - a per-worktree file + symlink would keep the lock
    worktree-local and not cross-serialize appends.

    Uses ``git worktree list --porcelain`` (the PR #400 pattern, robust across
    ``--separate-git-dir`` layouts), via
    :func:`fno.paths.resolve_canonical_repo_root`. That helper honors the
    ``FNO_REPO_ROOT`` test hook first, so tests stay hermetic.
    """
    from fno.paths import resolve_canonical_repo_root

    return resolve_canonical_repo_root()


class CarveoutError(Exception):
    """Raised when a carve-out cannot be persisted (e.g. unwritable ledger).

    The CLI maps this to a non-zero exit + stderr message so a FAILED capture
    is never a silent success (the advisory nature tolerates a *missed* call,
    not a *failed* one).
    """


@dataclass
class Carveout:
    """One left-out-work record. Serialized as a single JSONL line."""

    id: str
    ts: str
    session_id: Optional[str]
    kind: str
    priority: Optional[str]
    need: Optional[str]
    description: str
    truncated: bool
    # Optional crown scope this carve-out discharges (e.g. an epic id). The king
    # orphan check (hooks/context-nudge.sh) reads this FIELD rather than
    # grepping `description` free text, so rewording a sentence cannot silently
    # silence it. Optional + last so existing records and retro-triage parse
    # unchanged (a missing key reads as None).
    scope: Optional[str] = None
    # Severity stamped by provenance, not chosen by the filer: a carveout filed to
    # satisfy the fidelity gate is by construction "a planned deliverable is
    # unbuilt" and lands high/p1; hand-filed carveouts default to None (p3).
    # Optional + defaulted so existing JSONL records parse unchanged.
    severity: Optional[str] = None
    # The node whose worker PROVABLY owned the claim when the row was filed
    # (live node:<id> claim held by this harness session, manifest fallback).
    # The close gate blocks only the node stamped here; None (ambient shell,
    # legacy row) blocks nothing at close time. A FIELD for the same reason
    # `scope` is one: attribution read from free text can be spoofed by a
    # mention. Optional + defaulted so existing records parse unchanged.
    node: Optional[str] = None


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def resolve_session_id(repo_root: Path) -> Optional[str]:
    """Resolve the active target session id, or None if unresolvable.

    Order: the live ``target-state.md`` frontmatter ``fno_id`` (legacy
    ``session_id`` fallback), then the ``$CLAUDECODE_SESSION_ID`` env var.
    None means the caller records the
    carve-out unscoped (capture is never lost over a missing session).
    """
    import os

    state_path = repo_root / ".fno" / "target-state.md"
    if state_path.exists():
        try:
            from fno.state.io import read_frontmatter

            fm, _ = read_frontmatter(state_path)
            sid = fm.get("fno_id") or fm.get("session_id")
            if sid is not None and str(sid).strip() and str(sid).strip() != "null":
                return str(sid).strip()
        except Exception:
            # A malformed/locked state file must not break capture.
            pass

    env_sid = os.environ.get("CLAUDECODE_SESSION_ID")
    if env_sid and env_sid.strip():
        return env_sid.strip()
    return None


def _owning_node_id(repo_root: Path) -> Optional[str]:
    """The node THIS worker provably owns, as a bare id, or None.

    The claim lockfile is the liveness authority (x-4af4), and it is
    harness-agnostic: a live ``node:<id>`` claim held by
    ``target-session:<this harness session>`` names the owner for a codex or
    gemini executor too, not just claude. init acquires the claim before the
    manifest exists, and a handoff re-points the claim while a stale manifest
    would still name the former node - so the claim is consulted FIRST and a
    mismatch stamps nothing. The manifest match (find_held_node) stays as the
    fallback for a session whose claim died mid-run while it still holds the
    worktree manifest. Never guesses: zero or 2+ matching claims fall through.
    """
    import os

    candidates = set()
    # The same ids init-target-state.sh / _successor_claim_holder anchor the
    # node:<id> claim holder on. CODEX_THREAD_ID (not CODEX_SESSION_ID) is the
    # durable codex identity the claim names; opencode has its own marker.
    for var in (
        "TARGET_SESSION_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
        "OPENCODE_SESSION_ID",
    ):
        val = os.environ.get(var)
        if val and val.strip():
            candidates.add(val.strip())
    if candidates:
        wanted = {f"target-session:{c}" for c in candidates}
        try:
            from fno.claims.core import list_claims
            from fno.claims.io import claims_dir, global_claims_root

            matches = [
                claim.get("key", "")
                for claim in list_claims(
                    prefix="node:", root=claims_dir(global_claims_root())
                )
                if (claim.get("holder") or "") in wanted
            ]
            if len(matches) == 1 and matches[0].startswith("node:"):
                return matches[0][len("node:"):]
        except Exception:
            pass  # an unreadable claims dir falls to the manifest below

    from fno.agents.whoami import find_held_node

    try:
        held = find_held_node(str(repo_root), os.environ.get("CLAUDE_CODE_SESSION_ID"))
    except Exception:
        # Same invariant as resolve_session_id: a malformed state file must
        # not break capture. An unattributed row is always fileable.
        return None
    return held.split(":", 1)[1] if held else None


def truncate_description(text: str, cap: int = DESCRIPTION_CAP) -> Tuple[str, bool]:
    """Return (possibly-truncated text, was_truncated). Never raises."""
    if len(text) <= cap:
        return text, False
    original = len(text)
    marker = f" ... [truncated {original - cap} of {original} chars]"
    return text[:cap] + marker, True


def _rewrite_jsonl(path: Path, lines: "list[str]") -> None:
    """Replace the ledger's contents atomically. Raises OSError on failure.

    A plain ``write_text`` truncates in place, so a kill or an ENOSPC between
    truncate and flush leaves the ledger empty or half-written - every carve-out
    lost, not just the row being changed. Both rewriting callers hold the mkdir
    mutex, which serializes writers but does nothing about a torn write.

    Same mkstemp + ``os.replace`` shape ``graph.store._write_json`` uses, and
    the temp file is created in the ledger's own directory so the replace is a
    same-filesystem rename rather than a copy.
    """
    import os
    import tempfile

    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), prefix=path.name + ".", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(("\n".join(lines) + "\n") if lines else "")
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def _append_jsonl(path: Path, line: str, lock_timeout_seconds: int = 30) -> None:
    """Append one line under a mkdir mutex (mirrors events.append_event).

    Raises OSError on an unwritable target so the caller can surface a failed
    capture rather than swallowing it.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    lock_dir = path.parent / (path.name + ".lock.d")
    deadline = _time.monotonic() + lock_timeout_seconds
    while True:
        try:
            lock_dir.mkdir()
            break
        except FileExistsError:
            if _time.monotonic() >= deadline:
                raise TimeoutError(f"carveouts.jsonl lock timeout: {lock_dir}")
            _time.sleep(0.05)
    try:
        with path.open("a", encoding="utf-8") as fh:
            fh.write(line + "\n")
    finally:
        try:
            lock_dir.rmdir()
        except OSError:
            pass


def add_carveout(
    repo_root: Path,
    *,
    kind: str,
    description: str,
    need: Optional[str] = None,
    priority: Optional[str] = None,
    scope: Optional[str] = None,
    severity: Optional[str] = None,
    cap: int = DESCRIPTION_CAP,
    storage_root: Optional[Path] = None,
) -> Tuple[Carveout, bool]:
    """Build and persist a carve-out. Returns (carveout, unscoped).

    ``unscoped`` is True when no session id could be resolved (the record is
    still written, with ``session_id: null``). Raises ``CarveoutError`` for an
    invalid kind (closing the programmatic bypass of the CLI's own check) or if
    the ledger cannot be written.

    ``severity`` is stamped by provenance, not chosen freely: a carveout filed to
    satisfy the fidelity gate is by construction "a planned deliverable is
    unbuilt" and the gate stamps it high (p1). Hand-filed carveouts pass
    ``--severity`` or default to None (p3). Either way it routes through
    ``retro.classify.severity_to_priority`` at harvest.

    The session id is always resolved from ``repo_root`` (the live worktree's
    ``target-state.md``). The ledger is written under ``storage_root`` when
    given, else ``repo_root``. The CLI passes ``storage_root`` =
    :func:`resolve_carveout_root` (the canonical/main worktree) so a carve-out
    written inside a linked worktree survives that worktree's archival; the
    default keeps both roots equal for callers that don't split them
    (ab-44408b6e).
    """
    if kind not in VALID_KINDS:
        raise CarveoutError(
            f"invalid kind {kind!r}; must be one of {VALID_KINDS}"
        )
    if severity is not None and severity not in VALID_SEVERITIES:
        raise CarveoutError(
            f"invalid severity {severity!r}; must be one of {VALID_SEVERITIES}"
        )
    session_id = resolve_session_id(repo_root)
    unscoped = session_id is None
    owning_node = _owning_node_id(repo_root)
    desc, truncated = truncate_description(description, cap)

    cv = Carveout(
        id="cv-" + uuid.uuid4().hex[:8],
        ts=_utc_now_iso(),
        session_id=session_id,
        kind=kind,
        priority=priority,
        need=need,
        description=desc,
        truncated=truncated,
        scope=scope,
        severity=severity,
        node=owning_node,
    )

    from fno.paths import project_log

    ledger_root = storage_root if storage_root is not None else repo_root
    path = project_log(CARVEOUTS_NAME, project_root=ledger_root)
    try:
        _append_jsonl(path, json.dumps(asdict(cv), separators=(",", ":")))
    except OSError as exc:
        raise CarveoutError(str(exc)) from exc

    return cv, unscoped


class CarveoutNotFound(CarveoutError):
    """Raised when the id to update is not on the ledger.

    Its own type because the CLI must NOT fall back to creating the row. A
    create-on-miss would resurrect a carve-out ``/pr merged`` already consumed,
    under a later PR's number - the exact re-filing hazard ``consume_carveouts``
    exists to prevent.
    """


def update_carveout(
    root: Path,
    cv_id: str,
    *,
    description: Optional[str] = None,
    kind: Optional[str] = None,
    need: Optional[str] = None,
    priority: Optional[str] = None,
    scope: Optional[str] = None,
    cap: int = DESCRIPTION_CAP,
) -> "dict[str, Any]":
    """Edit one carve-out IN PLACE, preserving its identity. Returns the new record.

    ``id``, ``ts`` and ``session_id`` are never touched. That is the whole point
    of the verb: correcting a carve-out used to mean ``resolve`` then ``add``,
    which minted a new id, so every id already quoted in a PR body or a mail
    became a dead pointer. It was also LOSSY - the two steps are two writes, and
    a failure between them left the ledger with neither row. This does one
    locked rewrite, so the row either changes or does not.

    Only the fields passed are replaced; ``None`` means "leave alone", which is
    why an empty description has to be rejected at the CLI boundary rather than
    here (it would be indistinguishable from an omitted one). A new description
    is re-truncated and ``truncated`` recomputed, matching :func:`add_carveout`.

    Unlike :func:`consume_carveouts`, this is NOT best-effort. That function can
    return 0 for "already gone" and for "the ledger is unwritable" because both
    leave the caller's invariant intact. Here they differ: an absent id means
    the edit cannot apply, an unwritable ledger means the edit did not apply,
    and reporting a failed write as a clean no-op would tell the operator their
    correction landed when the old wording is still on disk. So it raises
    :class:`CarveoutNotFound` for the first and :class:`CarveoutError` for the
    second.

    Preserves a malformed neighbouring line verbatim, the same way
    ``consume_carveouts`` does: one bad row must not cost the others.
    """
    if kind is not None and kind not in VALID_KINDS:
        raise CarveoutError(f"invalid kind {kind!r}; must be one of {VALID_KINDS}")

    from fno.paths import project_log

    path = project_log(CARVEOUTS_NAME, project_root=root)
    if not path.exists():
        raise CarveoutNotFound(f"no carve-out ledger at {path}")

    lock_dir = path.parent / (path.name + ".lock.d")
    deadline = _time.monotonic() + 30
    while True:
        try:
            lock_dir.mkdir()
            break
        except FileExistsError:
            if _time.monotonic() >= deadline:
                raise CarveoutError(f"carveouts.jsonl lock timeout: {lock_dir}")
            _time.sleep(0.05)
        except OSError as exc:
            raise CarveoutError(str(exc)) from exc
    try:
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeDecodeError) as exc:
            raise CarveoutError(f"cannot read carve-out ledger {path}: {exc}") from exc

        updated: "Optional[dict[str, Any]]" = None
        kept: list[str] = []
        for line in lines:
            stripped = line.strip()
            if not stripped:
                continue
            try:
                rec = json.loads(stripped)
            except (json.JSONDecodeError, ValueError):
                kept.append(stripped)  # keep malformed; don't lose data
                continue
            if not isinstance(rec, dict) or str(rec.get("id", "")) != cv_id:
                kept.append(stripped)
                continue
            if description is not None:
                rec["description"], rec["truncated"] = truncate_description(
                    description, cap
                )
            for field, value in (
                ("kind", kind),
                ("need", need),
                ("priority", priority),
                ("scope", scope),
            ):
                if value is not None:
                    rec[field] = value
            updated = rec
            kept.append(json.dumps(rec, separators=(",", ":")))

        if updated is None:
            raise CarveoutNotFound(
                f"{cv_id} is not on the ledger (already resolved, or recorded "
                f"under a different project root)"
            )

        try:
            _rewrite_jsonl(path, kept)
        except OSError as exc:
            raise CarveoutError(f"cannot write carve-out ledger {path}: {exc}") from exc
        return updated
    finally:
        try:
            lock_dir.rmdir()
        except OSError:
            pass


def read_carveouts(
    root: Path,
    *,
    kind: Optional[str] = None,
    session_ids: Optional["set[str] | list[str]"] = None,
    include_unscoped: bool = False,
) -> "list[dict[str, Any]]":
    """Read the carve-out ledger under ``root``; return parsed records in order.

    Filtered to ``kind`` when given (e.g. ``backfill`` for /pr merged's slot),
    and to ``session_ids`` when given (the carve-out's ``session_id`` must be in
    the set). ``include_unscoped`` additionally keeps rows whose ``session_id``
    is null: they belong to no session, so an equality filter would hide them
    from every reader. The `list` default scope sets it; the explicit
    ``--session-id`` / ``--pr-number`` filters deliberately do not, since those
    ask a precise ownership question. Session scoping mirrors ``retro.harvest.harvest_carveouts`` so the
    backfill slot only handles backfills belonging to the merged PR's session(s),
    not another concurrent session's (codex P1 on PR #465).

    A malformed or non-object LINE is skipped, never raised, so one bad row
    cannot hide the rest - capture is never lost. A MISSING ledger returns
    ``[]`` (the common case, not an error). But a ledger that EXISTS yet cannot
    be read/decoded raises ``CarveoutError`` rather than masquerading as empty:
    a failed read must not be a silent success (the /pr merged backfill slot
    would otherwise drop a real backfill with no signal). Read-only: never
    mutates the ledger.
    """
    from fno.paths import project_log

    path = project_log(CARVEOUTS_NAME, project_root=root)
    if not path.exists():
        return []
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError) as exc:
        raise CarveoutError(f"cannot read carve-out ledger {path}: {exc}") from exc
    want_sessions = {str(s) for s in session_ids} if session_ids is not None else None
    out: "list[dict[str, Any]]" = []
    for line in lines:
        stripped = line.strip()
        if not stripped:
            continue
        try:
            rec = json.loads(stripped)
        except (json.JSONDecodeError, ValueError):
            continue
        if not isinstance(rec, dict):
            continue
        if kind is not None and rec.get("kind") != kind:
            continue
        if want_sessions is not None:
            rec_session = rec.get("session_id")
            # An ownerless row (session_id null) rides along with every scoped
            # read. `add` mints these deliberately when no session resolves, so
            # filtering on equality hides them from EVERY session forever, and
            # add-then-list stops round-tripping for the caller that filed one.
            # Explicit --session-id / --pr-number filters are unaffected: they
            # pass include_unscoped=False.
            if rec_session is None:
                if not include_unscoped:
                    continue
            elif str(rec_session) not in want_sessions:
                continue
        out.append(rec)
    return out


def consume_carveouts(repo_root: Path, ids: "set[str] | list[str]") -> int:
    """Remove the given carve-out ids from carveouts.jsonl. Returns count removed.

    Called by retro-triage after a clean land so a processed carve-out is never
    re-harvested - this both bounds the ledger and prevents an old carve-out
    from being re-filed under a later PR's number (the unscoped-read hazard).
    Rewrites the file under the same mkdir mutex as the writer. A malformed line
    is preserved (never silently dropped). Best-effort: returns 0 on any error.
    """
    from fno.paths import project_log

    want = {str(i) for i in ids}
    if not want:
        return 0
    path = project_log(CARVEOUTS_NAME, project_root=repo_root)
    if not path.exists():
        return 0

    lock_dir = path.parent / (path.name + ".lock.d")
    deadline = _time.monotonic() + 30
    while True:
        try:
            lock_dir.mkdir()
            break
        except FileExistsError:
            if _time.monotonic() >= deadline:
                return 0
            _time.sleep(0.05)
        except OSError:
            return 0
    try:
        kept: list[str] = []
        removed = 0
        for line in path.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if not stripped:
                continue
            try:
                rec = json.loads(stripped)
            except (json.JSONDecodeError, ValueError):
                kept.append(stripped)  # keep malformed; don't lose data
                continue
            if str(rec.get("id", "")) in want:
                removed += 1
                continue
            kept.append(stripped)
        # Atomic for the same reason the update path is: an in-place truncate
        # that dies mid-write loses every carve-out, not just the consumed ones.
        _rewrite_jsonl(path, kept)
        return removed
    except OSError:
        return 0
    finally:
        try:
            lock_dir.rmdir()
        except OSError:
            pass
