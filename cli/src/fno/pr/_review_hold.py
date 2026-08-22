"""Whether a review is RUNNING against this PR - the one predicate, two layers.

Merge readiness models a review as a RECORDED VERDICT. ``review_coverage``
answers "what verdicts exist for this head" and correctly returns ``unknown``
when none do. It has no way to say "a review is executing right now and its
findings are not committed yet", and those two states demand opposite actions:
the first may permit a merge under a config that requires no review, the second
forbids one. Three PRs on 2026-08-22 read ``ready: true`` with empty
``ready_blockers`` while a review of that exact head was mid-flight, one of them
with five counted findings under repair.

That window is the DEFAULT ordering, not a race: a review is dispatched when a
head is pushed, CI runs against the same head in parallel, CI is bounded and a
review is not. Green therefore arrives first, every time.

Two layers answer it, and neither covers the specimens alone:

  1. **The registered hold** - a TTL claim on ``review:branch:<branch>``, taken
     where a review is DISPATCHED rather than by the reviewer itself, so a
     reviewer that crashes still leaves the hold its dispatcher took. This is
     the only layer that sees the window between "review dispatched" and "first
     edit".
  2. **The derived worktree probe** - tracked modifications, or a local HEAD
     that is not what the PR would merge. Needs no cooperation at all, so it
     covers every review footnote never dispatched (a harness verb self-invoked
     through the Skill tool is not footnote code).

The hold is a claim rather than a new state file on purpose: ``fno.claims``
already owns atomic O_EXCL acquisition, TTL bounds, the
LIVE/SUSPECT/STALE/CORRUPTED classification, and a reaper. A second lifecycle
with its own expiry rules is a second lifecycle to get wrong, and a hold that
outlives a crashed reviewer wedges the merge lane, which is worse than the
defect it prevents.

Absence is never the clear answer by itself (the AGENTS.md pitfall): a missing
hold clears only when the worktree enumeration also RAN and answered. A dead
instrument - a corrupted lockfile, a failed ``git worktree list`` - blocks.
"""
from __future__ import annotations

import os
import sys
from typing import Any, Callable, NamedTuple, Optional

from pathlib import Path


# Not one of ``claims.io._GLOBAL_ID_PREFIXES``, so the key routes to the
# canonical repo root: every linked worktree of a project shares one hold, and
# a review running in a worktree is visible to a king in the main checkout.
REVIEW_HOLD_PREFIX = "review:branch:"

# A review is unbounded, so the TTL is a wedge bound rather than an estimate:
# long enough that a real review never ages out under itself, short enough that
# a crashed reviewer does not hold the lane for a working day.
DEFAULT_HOLD_TTL_MINUTES = 90

# Blocker vocabulary. Callers map these onto their own surface (a
# ``ready_blockers`` entry, a merge refusal, an exit code) and never invent a
# second spelling.
REVIEW_IN_FLIGHT = "review_in_flight"
REVIEW_HOLD_UNREADABLE = "review_hold_unreadable"
WORKTREE_DIRTY = "worktree_dirty"
WORKTREE_HEAD_MISMATCH = "worktree_head_mismatch"
WORKTREE_PROBE_FAILED = "worktree_probe_failed"


class ReviewActivity(NamedTuple):
    """What the two layers saw. ``blocker`` is ``""`` exactly when clear.

    ``hold`` and ``worktree`` are reported whether or not they blocked: the
    original complaint was an empty ``ready_blockers``, and a reader has to be
    able to see that the probes ran and what they answered.
    """

    blocked: bool
    blocker: str
    detail: str
    hold: Optional[dict]
    worktree: dict


def review_hold_key(branch: str) -> str:
    return f"{REVIEW_HOLD_PREFIX}{branch}"


def resolve_ttl_ms(minutes_or_none: Optional[float]) -> int:
    """Minutes -> a TTL inside the claim bounds. Coerces, never raises.

    A misconfigured TTL must not make the guard unreadable: an unreadable guard
    is a merge outage, and the recovery from a clamped TTL is a config edit.
    """
    from fno.claims.types import MAX_TTL_MS, MIN_TTL_MS

    if minutes_or_none is None:
        minutes_or_none = DEFAULT_HOLD_TTL_MINUTES
    try:
        ms = int(float(minutes_or_none) * 60_000)
    except (TypeError, ValueError):
        ms = DEFAULT_HOLD_TTL_MINUTES * 60_000
    return max(MIN_TTL_MS, min(MAX_TTL_MS, ms))


def configured_ttl_ms() -> int:
    """``config.review.hold_ttl_minutes``, clamped. Falls back on any failure."""
    try:
        from fno.config import load_settings

        review = getattr(load_settings(), "review", None)
        return resolve_ttl_ms(getattr(review, "hold_ttl_minutes", None))
    except Exception:  # noqa: BLE001 - a config read must not gate a review
        return resolve_ttl_ms(None)


def acquire_review_hold(
    branch: str,
    *,
    head: str,
    holder: str,
    verb: Optional[str] = None,
    ttl_ms: Optional[int] = None,
    root: Optional[Path] = None,
):
    """Register that a review of ``branch`` at ``head`` has started.

    Returns the :class:`Claim`, or ``None`` when the hold could not be taken.
    Never raises: registration must not block a review from starting. A review
    that runs unheld is covered by the worktree probe; a review that refuses to
    start because a lockfile write failed is a strictly worse outcome.
    """
    from fno.claims.core import acquire_claim

    metadata: dict[str, Any] = {"head_sha": head}
    if verb:
        metadata["verb"] = verb
    try:
        return acquire_claim(
            review_hold_key(branch),
            holder,
            reason=f"review in flight at {head[:12] or 'unknown head'}",
            ttl_ms=ttl_ms if ttl_ms is not None else configured_ttl_ms(),
            metadata=metadata,
            root=root,
        )
    except Exception as exc:  # noqa: BLE001 - see docstring
        sys.stderr.write(f"review-hold: could not register hold on {branch}: {exc}\n")
        return None


def release_review_hold(
    branch: str, *, holder: str, root: Optional[Path] = None
) -> bool:
    """Release this holder's hold. Idempotent; never raises."""
    from fno.claims.core import release_claim

    try:
        release_claim(review_hold_key(branch), holder, root=root)
        return True
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(f"review-hold: could not release hold on {branch}: {exc}\n")
        return False


def _emit_expired(*, key: str, holder: str, head: str, expires_at: Any) -> None:
    """Best-effort audit line for a hold the gate aged out.

    Separate function so the receipt is one monkeypatch away in tests, and so
    an events failure can never turn a clear verdict into a refusal.
    """
    try:
        import datetime as _dt

        from fno.events import append_event

        append_event(
            {
                "ts": _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                "type": "review_hold_expired",
                "source": "fno-loop",
                "data": {
                    "key": key,
                    "holder": holder,
                    "head_sha": head,
                    "expires_at": expires_at,
                },
            }
        )
    except Exception:  # noqa: BLE001 - the stderr receipt is the guarantee
        pass


def _parse_worktree_list(stdout: str) -> list[dict]:
    """``git worktree list --porcelain`` -> ``[{path, head, branch}]``.

    The porcelain form already carries each worktree's HEAD, so the probe needs
    no second ``rev-parse`` per entry.
    """
    entries: list[dict] = []
    current: dict = {}
    for raw in stdout.splitlines():
        line = raw.strip()
        if not line:
            if current.get("path"):
                entries.append(current)
            current = {}
            continue
        if line.startswith("worktree "):
            if current.get("path"):
                entries.append(current)
            current = {"path": line[len("worktree ") :], "head": "", "branch": None}
        elif line.startswith("HEAD "):
            current["head"] = line[len("HEAD ") :]
        elif line.startswith("branch refs/heads/"):
            current["branch"] = line[len("branch refs/heads/") :]
    if current.get("path"):
        entries.append(current)
    return entries


def _worktree_probe(
    branch: str, pr_head: str, repo: str, runner: Callable
) -> tuple[str, str, dict]:
    """The derived layer: ``(blocker, detail, reading)``.

    A FAILED enumeration is not an answer and blocks. A SUCCESSFUL enumeration
    that matches no worktree IS an answer and clears - the instrument ran, and
    a review with no local worktree leaves nothing here to see.
    """
    from fno.pr._proc import ToolMissing

    reading: dict = {"probed": False, "path": None, "dirty": None, "head": None, "note": ""}
    try:
        listed = runner(["git", "worktree", "list", "--porcelain"], cwd=repo)
    except ToolMissing as exc:
        reading["note"] = str(exc)
        return WORKTREE_PROBE_FAILED, f"worktree probe could not run: {exc}", reading
    if not listed.ok:
        note = (listed.stderr or listed.stdout or "").strip().splitlines()
        reading["note"] = note[0] if note else "git worktree list failed"
        return (
            WORKTREE_PROBE_FAILED,
            f"worktree probe could not run: {reading['note']}; refusing to assume nothing is in flight",
            reading,
        )

    reading["probed"] = True
    match = next(
        (e for e in _parse_worktree_list(listed.stdout) if e.get("branch") == branch), None
    )
    if match is None:
        reading["note"] = "no local worktree on this branch"
        return "", "", reading

    reading["path"] = match["path"]
    reading["head"] = match.get("head") or None
    try:
        status = runner(
            ["git", "-C", match["path"], "status", "--porcelain", "--untracked-files=no"],
            cwd=repo,
        )
    except ToolMissing as exc:
        reading["probed"] = False
        reading["note"] = str(exc)
        return WORKTREE_PROBE_FAILED, f"worktree probe could not run: {exc}", reading
    if not status.ok:
        reading["probed"] = False
        note = (status.stderr or status.stdout or "").strip().splitlines()
        reading["note"] = note[0] if note else "git status failed"
        return (
            WORKTREE_PROBE_FAILED,
            f"worktree probe could not read {match['path']}: {reading['note']}",
            reading,
        )

    reading["dirty"] = bool(status.stdout.strip())
    if reading["dirty"]:
        return (
            WORKTREE_DIRTY,
            f"{match['path']} carries uncommitted changes to tracked files; "
            "merging would ship the code without them",
            reading,
        )
    # Empty pr_head means the caller could not pin one, so the conjunct is
    # skipped rather than guessed at - the same degrade `covered_conjuncts`
    # takes on a missing head.
    local_head = match.get("head") or ""
    if pr_head and local_head and local_head != pr_head:
        return (
            WORKTREE_HEAD_MISMATCH,
            f"{match['path']} is at {local_head} but the PR would merge {pr_head}; "
            "the local branch is not what this merge lands",
            reading,
        )
    return "", "", reading


def review_activity(
    branch: str,
    *,
    pr_head: str = "",
    repo: Optional[str] = None,
    root: Optional[Path] = None,
    runner: Optional[Callable] = None,
) -> ReviewActivity:
    """Is a review of ``branch`` in flight? Layer 1, then layer 2."""
    from fno.claims.core import claim_status
    from fno.claims.types import ClaimState

    key = review_hold_key(branch)
    try:
        status = claim_status(key, root=root)
    except Exception as exc:  # noqa: BLE001 - claim_status is documented never
        # to raise, but its claims-root resolution shells out and can. An
        # unreadable hold is a dead instrument, and a dead instrument blocks.
        return ReviewActivity(
            True,
            REVIEW_HOLD_UNREADABLE,
            f"review hold on {branch} is unreadable ({exc}); refusing to assume unheld",
            None,
            {"probed": False, "path": None, "dirty": None, "head": None, "note": "not reached"},
        )

    state = status.get("state")
    not_reached = {"probed": False, "path": None, "dirty": None, "head": None, "note": "not reached"}
    if state == ClaimState.CORRUPTED.value:
        return ReviewActivity(
            True,
            REVIEW_HOLD_UNREADABLE,
            f"review hold on {branch} could not be parsed "
            f"({status.get('error', 'unknown error')}); refusing to assume unheld",
            status,
            not_reached,
        )
    if state in (ClaimState.LIVE.value, ClaimState.SUSPECT.value):
        held_head = (status.get("metadata") or {}).get("head_sha") or "an unrecorded head"
        return ReviewActivity(
            True,
            REVIEW_IN_FLIGHT,
            f"a review is in flight on {branch}: held by {status.get('holder')} "
            f"at {held_head}. Merging now ships the code the review is still fixing. "
            f"Clear it with `fno do pr review-hold release --branch {branch}` once the "
            "review has landed its findings.",
            status,
            not_reached,
        )
    if state == ClaimState.STALE.value:
        # Ages out, but never silently: a lane that clears with no receipt is
        # indistinguishable from one that was never held.
        holder = status.get("holder") or "unknown"
        sys.stderr.write(
            f"note: review hold on {branch} expired (holder {holder}, "
            f"expires_at {status.get('expires_at')}); it no longer blocks. "
            "A reviewer that died mid-run leaves no findings behind it.\n"
        )
        _emit_expired(
            key=key,
            holder=holder,
            head=(status.get("metadata") or {}).get("head_sha") or "",
            expires_at=status.get("expires_at"),
        )

    if runner is None:
        from fno.pr._proc import run as runner  # type: ignore[assignment]

    blocker, detail, reading = _worktree_probe(
        branch, pr_head, repo or os.getcwd(), runner
    )
    hold = status if state == ClaimState.STALE.value else None
    return ReviewActivity(bool(blocker), blocker, detail, hold, reading)


def review_hold_refusal(
    branch: str,
    *,
    pr_head: str = "",
    repo: Optional[str] = None,
    root: Optional[Path] = None,
    runner: Optional[Callable] = None,
) -> Optional[str]:
    """The one refusal sentence shared by every merge surface, or ``None``."""
    activity = review_activity(
        branch, pr_head=pr_head, repo=repo, root=root, runner=runner
    )
    if not activity.blocked:
        return None
    return f"{activity.blocker}: {activity.detail}"
