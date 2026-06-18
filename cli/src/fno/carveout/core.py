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

# Max description length retained on disk. Oversized descriptions are TRUNCATED
# (never rejected) so capture is never lost; the marker records the original
# length. A carve-out body larger than this is almost certainly a paste mistake,
# and the triage harvest re-derives the real reasoning from the cited source.
DESCRIPTION_CAP = 8000

CARVEOUTS_RELPATH = ".fno/carveouts.jsonl"


def resolve_carveout_root() -> Path:
    """Resolve the root that owns the carveouts ledger: the CANONICAL (main)
    worktree, never the per-worktree ``--show-toplevel`` root.

    Carve-outs are SHARED project state, like ``ledger.json`` / ``tasks.json``
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


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def resolve_session_id(repo_root: Path) -> Optional[str]:
    """Resolve the active target session id, or None if unresolvable.

    Order: the live ``target-state.md`` frontmatter ``session_id``, then the
    ``$CLAUDECODE_SESSION_ID`` env var. None means the caller records the
    carve-out unscoped (capture is never lost over a missing session).
    """
    import os

    state_path = repo_root / ".fno" / "target-state.md"
    if state_path.exists():
        try:
            from fno.state.io import read_frontmatter

            fm, _ = read_frontmatter(state_path)
            sid = fm.get("session_id")
            if sid is not None and str(sid).strip() and str(sid).strip() != "null":
                return str(sid).strip()
        except Exception:
            # A malformed/locked state file must not break capture.
            pass

    env_sid = os.environ.get("CLAUDECODE_SESSION_ID")
    if env_sid and env_sid.strip():
        return env_sid.strip()
    return None


def truncate_description(text: str, cap: int = DESCRIPTION_CAP) -> Tuple[str, bool]:
    """Return (possibly-truncated text, was_truncated). Never raises."""
    if len(text) <= cap:
        return text, False
    original = len(text)
    marker = f" ... [truncated {original - cap} of {original} chars]"
    return text[:cap] + marker, True


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
    cap: int = DESCRIPTION_CAP,
    storage_root: Optional[Path] = None,
) -> Tuple[Carveout, bool]:
    """Build and persist a carve-out. Returns (carveout, unscoped).

    ``unscoped`` is True when no session id could be resolved (the record is
    still written, with ``session_id: null``). Raises ``CarveoutError`` for an
    invalid kind (closing the programmatic bypass of the CLI's own check) or if
    the ledger cannot be written.

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
    session_id = resolve_session_id(repo_root)
    unscoped = session_id is None
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
    )

    ledger_root = storage_root if storage_root is not None else repo_root
    path = ledger_root / CARVEOUTS_RELPATH
    try:
        _append_jsonl(path, json.dumps(asdict(cv), separators=(",", ":")))
    except OSError as exc:
        raise CarveoutError(str(exc)) from exc

    return cv, unscoped


def read_carveouts(
    root: Path,
    *,
    kind: Optional[str] = None,
    session_ids: Optional["set[str] | list[str]"] = None,
) -> "list[dict[str, Any]]":
    """Read the carve-out ledger under ``root``; return parsed records in order.

    Filtered to ``kind`` when given (e.g. ``backfill`` for /pr merged's slot),
    and to ``session_ids`` when given (the carve-out's ``session_id`` must be in
    the set). Session scoping mirrors ``retro.harvest.harvest_carveouts`` so the
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
    path = root / CARVEOUTS_RELPATH
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
        if want_sessions is not None and str(rec.get("session_id")) not in want_sessions:
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
    want = {str(i) for i in ids}
    if not want:
        return 0
    path = repo_root / CARVEOUTS_RELPATH
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
        path.write_text(
            ("\n".join(kept) + "\n") if kept else "", encoding="utf-8"
        )
        return removed
    except OSError:
        return 0
    finally:
        try:
            lock_dir.rmdir()
        except OSError:
            pass
