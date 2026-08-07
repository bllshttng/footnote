"""Typed, immutable, versioned resume receipts + live-authority revalidation.

The two invariants this module exists to enforce (x-c3a2):

1. A receipt is EVIDENCE, not AUTHORITY. ``write_receipt`` records a snapshot;
   it never grants the right to act. ``revalidate`` is the gate: it compares a
   receipt against the live claim holder, session liveness, worktree
   registration, branch, and HEAD, and fails CLOSED on any mismatch - without
   mutating predecessor state or claims. A successor writes only after
   ``revalidate`` returns ok AND it acquires the node claim through the
   canonical primitive.

2. No resume database. Event canonicalization reuses the scoreboard reducers
   (``read_jsonl_events_with_coverage`` dedups by signature;
   ``_event_sort_key`` sorts by parsed timestamp with a deterministic
   complete-on-tie). Two journals carrying the same event out of order fold to
   one latest-by-timestamp observation, so stale authority cannot revive.

Identity is (node, session, phase, generation, candidate_sha) where
candidate_sha is the git HEAD at write time. One immutable file per identity;
``write_receipt`` refuses to overwrite. Exactly one next_action per receipt.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Iterable, Optional, Sequence

# Reducer reuse (the canonicalization contract). Importing the underscore
# helpers is deliberate: _event_sort_key IS the "latest by parsed timestamp,
# deterministic complete-on-tie" rule the receipt must not reinvent.
from fno.scoreboard.fold import (
    _event_sort_key,
    _parse_ts,
    read_jsonl_events_with_coverage,
)

RECEIPT_VERSION = 1

# Events that, observed for a node AFTER a receipt was written, mean the
# receipt is stale authority (a later terminal/delegation already happened).
# Subset of fold.CONTEXT_TRACE_EVENT_KINDS that signal "the world moved on."
_SUPERSEDED_KINDS = frozenset(
    {
        "termination",
        "session_satisfied",
        "delegated",
        "loop_check",
    }
)

_NODE_TOKEN_RE = re.compile(r"^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$", re.IGNORECASE)


class MalformedReceiptError(ValueError):
    """A receipt file failed schema validation.

    A malformed DECLARED receipt is rejected explicitly rather than treated as
    absent: silently ignoring a corrupt resume artifact would let a successor
    proceed on no evidence, the exact failure mode this module prevents.
    """


@dataclass(frozen=True)
class NextAction:
    """Exactly one next action a successor should take. verb is required."""

    verb: str
    target: Optional[str] = None

    def __post_init__(self) -> None:
        if not isinstance(self.verb, str) or not self.verb.strip():
            raise MalformedReceiptError("next_action.verb must be a non-empty string")


@dataclass(frozen=True)
class ReceiptIdentity:
    node: str
    session: str
    phase: str
    generation: int
    candidate_sha: str

    def __post_init__(self) -> None:
        if not (self.node and self.session and self.phase):
            raise MalformedReceiptError(
                "identity.node/session/phase must be non-empty"
            )
        if not isinstance(self.generation, int) or self.generation < 1:
            raise MalformedReceiptError(
                f"identity.generation must be a positive int, got {self.generation!r}"
            )
        if not self.candidate_sha:
            raise MalformedReceiptError("identity.candidate_sha must be non-empty")


@dataclass(frozen=True)
class ResumeReceipt:
    version: int
    identity: ReceiptIdentity
    repo: str
    worktree: str
    branch: str
    head: str
    next_action: NextAction
    written_at: str
    completed_tasks: tuple[str, ...] = ()
    remaining_tasks: tuple[str, ...] = ()
    open_findings: tuple[str, ...] = ()
    known_reds: tuple[str, ...] = ()
    claims: tuple[dict[str, Any], ...] = ()
    watchers: tuple[str, ...] = ()
    # External-effect keys (pr_create:<sha>, comment:<sha>, merge:<sha>, ...).
    # A resumed worker checks these before performing the effect so a worker
    # that died after output but before journaling cannot replay a publish,
    # PR create, comment, or merge.
    idempotency_keys: tuple[str, ...] = ()
    content_sha: str = ""

    def to_dict(self) -> dict[str, Any]:
        d = asdict(self)
        d["identity"] = asdict(self.identity)
        d["next_action"] = asdict(self.next_action)
        # tuples -> lists for JSON
        for k in (
            "completed_tasks",
            "remaining_tasks",
            "open_findings",
            "known_reds",
            "claims",
            "watchers",
            "idempotency_keys",
        ):
            d[k] = list(d[k])
        return d


@dataclass(frozen=True)
class RevalidationResult:
    """Outcome of revalidating a receipt against live state.

    ``ok`` is the single decision. ``reason`` is empty on ok and a
    machine-readable fail-closed code otherwise. ``checked`` carries the live
    values compared, for observability; it is never empty so a green verdict
    is distinguishable from a no-op.
    """

    ok: bool
    reason: str
    checked: dict[str, Any] = field(default_factory=dict)


def _short_sha(sha: str) -> str:
    return (sha or "").replace("\n", "").strip()[:12]


def _canonical_payload(receipt_dict: dict[str, Any]) -> str:
    """Stable serialization for the content_sha integrity tag.

    Excludes content_sha itself (circular) and sorts keys so two writers
    producing the same receipt compute the same sha."""
    d = {k: v for k, v in receipt_dict.items() if k != "content_sha"}
    return json.dumps(d, sort_keys=True, separators=(",", ":"))


def build_receipt(
    *,
    node: str,
    session: str,
    phase: str,
    generation: int,
    repo: str,
    worktree: str,
    branch: str,
    head: str,
    next_verb: str,
    next_target: Optional[str],
    written_at: str,
    completed_tasks: Sequence[str] = (),
    remaining_tasks: Sequence[str] = (),
    open_findings: Sequence[str] = (),
    known_reds: Sequence[str] = (),
    claims: Sequence[dict[str, Any]] = (),
    watchers: Sequence[str] = (),
    idempotency_keys: Sequence[str] = (),
) -> ResumeReceipt:
    """Assemble a receipt and compute its content_sha.

    Pure (no I/O). ``head`` becomes the candidate_sha in the identity: a
    receipt is versioned per (node, session, phase, generation, HEAD), so a
    new commit at the same phase/generation is a new version, not an overwrite.
    """
    identity = ReceiptIdentity(
        node=node,
        session=session,
        phase=phase,
        generation=generation,
        candidate_sha=_short_sha(head),
    )
    next_action = NextAction(verb=next_verb, target=next_target)
    receipt = ResumeReceipt(
        version=RECEIPT_VERSION,
        identity=identity,
        repo=repo,
        worktree=worktree,
        branch=branch or "",
        head=head or "",
        next_action=next_action,
        written_at=written_at,
        completed_tasks=tuple(completed_tasks),
        remaining_tasks=tuple(remaining_tasks),
        open_findings=tuple(open_findings),
        known_reds=tuple(known_reds),
        claims=tuple(claims),
        watchers=tuple(watchers),
        idempotency_keys=tuple(idempotency_keys),
    )
    content_sha = hashlib.sha256(
        _canonical_payload(receipt.to_dict()).encode("utf-8")
    ).hexdigest()
    # frozen dataclass: bypass __init__ validation to stamp the computed tag.
    object.__setattr__(receipt, "content_sha", content_sha)
    return receipt


def receipt_filename(identity: ReceiptIdentity) -> str:
    return (
        f"receipt-{identity.node}-{identity.phase}"
        f"-g{identity.generation}-{identity.candidate_sha}.json"
    )


def receipt_path(identity: ReceiptIdentity, artifacts_dir: Path) -> Path:
    return Path(artifacts_dir) / receipt_filename(identity)


def write_receipt(receipt: ResumeReceipt, artifacts_dir: Path) -> Path:
    """Atomically write an immutable receipt. Refuses to overwrite.

    tmp-then-rename is atomic on POSIX; the refuse-overwrite guard makes a
    completed receipt immutable for its identity. A second write for the same
    identity is a programming error (re-deriving a completed boundary), not a
    silent clobber.
    """
    artifacts_dir = Path(artifacts_dir)
    artifacts_dir.mkdir(parents=True, exist_ok=True)
    path = receipt_path(receipt.identity, artifacts_dir)
    if path.exists():
        raise FileExistsError(
            f"resume receipt already exists for {receipt.identity}: {path} "
            "(completed receipts are immutable)"
        )
    tmp = path.with_suffix(path.suffix + f".tmp.{os.getpid()}")
    try:
        tmp.write_text(
            json.dumps(receipt.to_dict(), sort_keys=True, indent=2) + "\n",
            encoding="utf-8",
        )
        os.replace(tmp, path)
    except Exception:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise
    return path


def load_receipt(path: Path) -> ResumeReceipt:
    """Parse + validate a receipt file. Raises MalformedReceiptError on defect.

    A missing file is NOT malformed (callers distinguish via FileNotFoundError);
    a present-but-corrupt file IS malformed and must fail closed, never read as
    empty evidence.
    """
    raw = Path(path).read_text(encoding="utf-8")
    try:
        d = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise MalformedReceiptError(f"{path}: not valid JSON: {exc}") from exc
    return _receipt_from_dict(d, origin=str(path))


def _receipt_from_dict(d: Any, *, origin: str = "<receipt>") -> ResumeReceipt:
    if not isinstance(d, dict):
        raise MalformedReceiptError(f"{origin}: top-level is not an object")
    try:
        ident_raw = d["identity"]
        next_raw = d["next_action"]
        identity = ReceiptIdentity(
            node=_req_str(ident_raw, "node", origin),
            session=_req_str(ident_raw, "session", origin),
            phase=_req_str(ident_raw, "phase", origin),
            generation=_req_int(ident_raw, "generation", origin),
            candidate_sha=_req_str(ident_raw, "candidate_sha", origin),
        )
        next_action = NextAction(
            verb=_req_str(next_raw, "verb", origin),
            target=_opt_str(next_raw, "target"),
        )
        version = _req_int(d, "version", origin)
        if version != RECEIPT_VERSION:
            # A newer-than-known version is additive (a future writer); an older
            # or junk version is drift. Either way fail closed rather than guess.
            raise MalformedReceiptError(
                f"{origin}: receipt version {version} != supported {RECEIPT_VERSION}"
            )
        receipt = ResumeReceipt(
            version=version,
            identity=identity,
            repo=_req_str(d, "repo", origin),
            worktree=_req_str(d, "worktree", origin),
            branch=_opt_str(d, "branch") or "",
            head=_req_str(d, "head", origin),
            next_action=next_action,
            written_at=_req_str(d, "written_at", origin),
            completed_tasks=_str_tuple(d.get("completed_tasks"), "completed_tasks", origin),
            remaining_tasks=_str_tuple(d.get("remaining_tasks"), "remaining_tasks", origin),
            open_findings=_str_tuple(d.get("open_findings"), "open_findings", origin),
            known_reds=_str_tuple(d.get("known_reds"), "known_reds", origin),
            claims=_claims_tuple(d.get("claims"), origin),
            watchers=_str_tuple(d.get("watchers"), "watchers", origin),
            idempotency_keys=_str_tuple(d.get("idempotency_keys"), "idempotency_keys", origin),
            content_sha=_opt_str(d, "content_sha") or "",
        )
    except MalformedReceiptError:
        raise
    except (KeyError, TypeError, ValueError) as exc:
        raise MalformedReceiptError(f"{origin}: {exc}") from exc
    # Integrity: if a content_sha is recorded, the loaded payload must match.
    if receipt.content_sha:
        recomputed = hashlib.sha256(
            _canonical_payload(receipt.to_dict()).encode("utf-8")
        ).hexdigest()
        if recomputed != receipt.content_sha:
            raise MalformedReceiptError(
                f"{origin}: content_sha mismatch (payload tampered or rewritten)"
            )
    return receipt


def _req_str(d: Any, key: str, origin: str) -> str:
    v = d.get(key) if isinstance(d, dict) else None
    if not isinstance(v, str) or not v.strip():
        raise MalformedReceiptError(f"{origin}: missing/empty string field {key!r}")
    return v


def _req_int(d: Any, key: str, origin: str) -> int:
    v = d.get(key) if isinstance(d, dict) else None
    # bool is an int subclass; reject it explicitly so True/False never pose.
    if isinstance(v, bool) or not isinstance(v, int):
        raise MalformedReceiptError(f"{origin}: field {key!r} must be an integer")
    return v


def _opt_str(d: Any, key: str) -> Optional[str]:
    v = d.get(key) if isinstance(d, dict) else None
    return v if isinstance(v, str) else None


def _str_tuple(v: Any, key: str, origin: str) -> tuple[str, ...]:
    if v is None:
        return ()
    if not isinstance(v, list) or not all(isinstance(x, str) for x in v):
        raise MalformedReceiptError(f"{origin}: field {key!r} must be a list of strings")
    return tuple(v)


def _claims_tuple(v: Any, origin: str) -> tuple[dict[str, Any], ...]:
    if v is None:
        return ()
    if not isinstance(v, list) or not all(isinstance(x, dict) for x in v):
        raise MalformedReceiptError(f"{origin}: field 'claims' must be a list of objects")
    return tuple(v)


# ── reducer reuse: canonicalize + latest-by-timestamp ──────────────────────


def canonicalize_node_events(
    events: Iterable[dict[str, Any]],
) -> list[dict[str, Any]]:
    """Dedup + sort the node's observations the way the scoreboard does.

    Reuses ``_event_sort_key`` (parsed-timestamp primary, complete-context
    tiebreak, signature final tiebreak) so the receipt's view of "what
    happened for this node" is identical to the reducer's. Duplicate events
    across the global + delivery-root journals collapse by signature before
    the sort, mirroring ``read_jsonl_events_with_coverage``.
    """
    seen: set[str] = set()
    out: list[dict[str, Any]] = []
    for e in events:
        if not isinstance(e, dict):
            continue
        signature = json.dumps(
            e.get("raw") if isinstance(e.get("raw"), dict) else e,
            sort_keys=True,
            separators=(",", ":"),
        )
        if signature in seen:
            continue
        seen.add(signature)
        out.append(e)
    out.sort(key=_event_sort_key)
    return out


def latest_observation(
    events: Sequence[dict[str, Any]], kinds: Optional[set[str]] = None
) -> Optional[dict[str, Any]]:
    """The latest event (by parsed timestamp) for a node, optionally kind-filtered.

    After canonicalization the last element is the latest by timestamp with a
    deterministic tiebreak - the one rule that makes "select latest" safe
    across duplicated, out-of-order journals.
    """
    canon = canonicalize_node_events(events)
    if kinds is not None:
        canon = [
            e
            for e in canon
            if (e.get("type") or e.get("kind")) in kinds
        ]
    return canon[-1] if canon else None


def detect_duplicate_generation(
    events: Sequence[dict[str, Any]],
    *,
    node: str,
    harness: Optional[str],
    generation: int,
    own_session: str,
) -> bool:
    """True if a `delegated` event already minted this generation off-session.

    Mirrors handoff.sh's (node, harness)-scoped generation count. A delegated
    event with this generation whose from_session is not the receipt's own
    session means a second lineage is claiming the same generation slot - the
    receipt must fail closed as a duplicate rather than re-mint the name.
    """
    for e in canonicalize_node_events(events):
        if (e.get("type") or e.get("kind")) != "delegated":
            continue
        raw_data = e.get("data")
        data: dict = raw_data if isinstance(raw_data, dict) else {}
        if data.get("node_id") != node:
            continue
        if harness is not None and data.get("harness") != harness:
            continue
        raw_gen = data.get("generation")
        if raw_gen is None:
            continue
        try:
            gen = int(raw_gen)
        except (TypeError, ValueError):
            continue
        if gen == generation and data.get("from_session") != own_session:
            return True
    return False


def _event_node(e: dict[str, Any]) -> Optional[str]:
    for key in ("node_id", "graph_node_id"):
        v = e.get(key)
        if isinstance(v, str) and v:
            return v
    data = e.get("data")
    if isinstance(data, dict):
        for key in ("node_id", "graph_node_id"):
            v = data.get(key)
            if isinstance(v, str) and v:
                return v
    return None


def _event_session(e: dict[str, Any]) -> Optional[str]:
    """The session an event belongs to.

    `delegated` carries from_session (the delegating predecessor); other kinds
    carry session_id. Used to exclude a receipt's OWN continuation events
    (its own later delegation/loop_check) from supersession - those are the
    receipt's own succession, not a foreign later authority.
    """
    data = e.get("data")
    if isinstance(data, dict):
        for key in ("from_session", "session_id"):
            v = data.get(key)
            if isinstance(v, str) and v:
                return v
    for key in ("from_session", "session_id"):
        v = e.get(key)
        if isinstance(v, str) and v:
            return v
    return None


def _superseded_by_later_event(
    events: Sequence[dict[str, Any]],
    *,
    node: str,
    receipt_written_at: str,
    own_session: Optional[str] = None,
) -> Optional[str]:
    """A node event strictly after the receipt's write means stale authority.

    Uses parsed-timestamp comparison (the reducer's one timeline). An undated
    late event is conservatively treated as non-superseding so a missing ts
    never false-positives a live receipt into failure.
    """
    written = _parse_ts(receipt_written_at)
    if written is None:
        return None
    for e in canonicalize_node_events(events):
        if (e.get("type") or e.get("kind")) not in _SUPERSEDED_KINDS:
            continue
        if _event_node(e) != node:
            continue
        # Exclude the receipt's own continuation: its own later delegation
        # (handoff.sh emits `delegated` at Step 8, after writing the receipt at
        # Step 2) and its own loop_check are this receipt's succession, not a
        # foreign later authority reviving. Only a FOREIGN later event supersedes.
        if own_session and _event_session(e) == own_session:
            continue
        at = _parse_ts(e.get("ts") or e.get("timestamp"))
        if at is not None and at > written:
            return f"superseded_by_later_event:{e.get('type') or e.get('kind')}"
    return None


def revalidate(
    receipt: ResumeReceipt,
    *,
    live_head: str,
    live_branch: str,
    worktree_exists: bool,
    live_claim_holder: Optional[str],
    own_session: Optional[str],
    node_events: Sequence[dict[str, Any]] = (),
    harness: Optional[str] = None,
) -> RevalidationResult:
    """Compare a receipt against live state. Read-only; never mutates claims.

    Returns ok=True only when every check passes. On any mismatch returns
    ok=False with a machine-readable reason; the caller parks WITHOUT touching
    the predecessor's claim or state (predecessor state preserved by construction).

    Checks (all fail-closed):
      stale_head        - receipt.head != live HEAD (the candidate moved)
      foreign_claim     - node held by a session outside the receipt's lineage
      dead_worktree     - receipt.worktree path no longer exists / registered
      duplicate_generation - a delegated event already minted this generation
                             off the receipt's own session
      superseded        - a later terminal/delegated/loop_check for the node
                             postdates the receipt (stale authority revived)
    """
    ident = receipt.identity
    own = own_session or ident.session
    checked: dict[str, Any] = {
        "node": ident.node,
        "receipt_head": receipt.head,
        "live_head": live_head,
        "receipt_branch": receipt.branch,
        "live_branch": live_branch,
        "worktree_exists": worktree_exists,
        "live_claim_holder": live_claim_holder,
        "own_session": own,
    }

    # stale HEAD: the candidate must still be the live HEAD. A successor starts
    # at the predecessor's HEAD; if it moved (a commit landed, or the branch
    # was rewound), the receipt's next_action is no longer trustworthy.
    if receipt.head and live_head and receipt.head != live_head:
        return RevalidationResult(False, "stale_head", checked)

    # branch drift is the same class of failure (the line of work changed).
    if receipt.branch and live_branch and receipt.branch != live_branch:
        return RevalidationResult(False, "stale_branch", checked)

    # dead worktree: the recorded worktree must still exist. A pruned/renamed
    # worktree means the receipt points at a place that can no longer be written.
    if not worktree_exists:
        return RevalidationResult(False, "dead_worktree", checked)

    # foreign claim: the node may be held by the receipt's own session (ok,
    # idempotent re-acquire), a free claim (ok, the successor acquires), or a
    # FOREIGN session (fail - someone else owns the node). SUSPECT counts as a
    # live foreign holder (a respawned worker's protected slot).
    if live_claim_holder and live_claim_holder != own:
        return RevalidationResult(False, "foreign_claim", checked)

    if detect_duplicate_generation(
        node_events,
        node=ident.node,
        harness=harness,
        generation=ident.generation,
        own_session=own,
    ):
        return RevalidationResult(False, "duplicate_generation", checked)

    # superseded: a later node event means the receipt is stale authority.
    late = _superseded_by_later_event(
        node_events,
        node=ident.node,
        receipt_written_at=receipt.written_at,
        own_session=own,
    )
    if late:
        return RevalidationResult(False, late, checked)

    return RevalidationResult(True, "", checked)


def read_node_events(paths: Sequence[Path]) -> list[dict[str, Any]]:
    """Read + dedup the node's events across one or more journals.

    Thin wrapper over the reducer's coverage-aware reader so the receipt's
    event view is identical to the scoreboard's (same dedup, same paths). The
    caller filters to the node before passing to revalidate.
    """
    kinds = {
        "delegated",
        "handoff_failed",
        "termination",
        "session_satisfied",
        "loop_check",
        "claim_acquired",
        "claim_idempotent_reacquired",
        "claim_stale_reclaimed",
        "claim_released",
    }
    return read_jsonl_events_with_coverage([Path(p) for p in paths], kinds)["events"]
