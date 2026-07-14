"""Worktree/branch harness guard (x-193d Wave 5).

The invariant: at most one harness owns a worktree/branch at a time. x-3e70 made
node claims harness-tagged and gave the DISPATCHER a guard, but that only fires
at dispatch time and defers to a foreign harness. The uncovered path is the
MANUAL session - a human opens codex in a worktree a claude worker owns and
nothing objects. This module is the claims-layer core for that gap; the hook +
CLI surfaces consult it.

The collision unit is the worktree PATH (a worktree checks out exactly one
branch, so path and branch are 1:1; the physical toplevel is the cheapest thing
to resolve). The claim is a plain ``worktree:<physical-toplevel>`` key on the
existing generic claim primitive: atomicity and harness tagging come for free
from ``acquire_claim`` (Concurrency: check-and-claim is atomic, never
check-then-act). Repo-local key routing already sends it to the canonical
``.fno/claims`` (see ``claims_root_for``), so every worktree of a repo shares one
claims dir and each worktree gets its own lock.

Liveness mirrors the node claim: TTL plus an optional session-pid arm. We NEVER
run a refresh loop here - re-establishment happens by idempotent re-acquire on
the owner's next write - so the ``fno claim refresh`` "shrinks to 1 minute
without --ttl" footgun cannot apply.
"""
from __future__ import annotations

import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from .core import ClaimHeldByOther, acquire_claim, claim_status

# 2h TTL matches the node claim default (TARGET_CLAIM_TTL). A live session keeps
# it fresh by re-acquiring on each write; the pid arm keeps a claude owner alive
# past TTL. A codex owner (no resolvable session pid) relies on the TTL arm.
DEFAULT_TTL_MS = 2 * 60 * 60 * 1000

VERDICT_NO_WORKTREE = "no-worktree"  # not a git checkout / no harness -> no enforcement
VERDICT_ACQUIRED = "acquired"        # claim was free, now ours
VERDICT_OK = "ok"                    # same harness owns it (re-entry) - never refuse
VERDICT_FOREIGN = "foreign"          # a DIFFERENT harness owns it - refuse
VERDICT_OVERRIDE = "override"        # foreign, but FNO_WORKTREE_OK bypassed the refusal


@dataclass
class WorktreeGuardResult:
    verdict: str
    worktree: Optional[str] = None
    my_harness: Optional[str] = None
    owner_harness: Optional[str] = None
    owner_holder: Optional[str] = None
    owner_pid: Optional[int] = None

    @property
    def blocked(self) -> bool:
        return self.verdict == VERDICT_FOREIGN


def worktree_claim_key(worktree_root: Path) -> str:
    return f"worktree:{worktree_root}"


def resolve_worktree_root(cwd: Optional[Path] = None) -> Optional[Path]:
    """Physical toplevel of the git checkout containing ``cwd``, or None.

    None outside a git repo (bare shell, CI scratch dir) - the guard then
    enforces nothing. ``pwd -P`` semantics: resolve symlinks so two harnesses
    entering the same worktree via different symlinked paths (macOS
    /tmp -> /private/tmp) key on the SAME lock, matching check-impl-location.sh.
    """
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=str(cwd) if cwd else None,
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0:
        return None
    top = out.stdout.strip()
    if not top:
        return None
    try:
        return Path(top).resolve()
    except OSError:
        return Path(top)


def guard_worktree(
    worktree_root: Optional[Path],
    *,
    my_harness: Optional[str],
    my_holder: str,
    session_pid: Optional[int] = None,
    ttl_ms: Optional[int] = DEFAULT_TTL_MS,
    override: bool = False,
    root: Optional[Path] = None,
) -> WorktreeGuardResult:
    """Consult (and, when free, acquire) the worktree claim.

    Returns a verdict; the CALLER decides how to act (the PreToolUse hook blocks
    on ``foreign``, ``fno target init`` only warns). Fail-open by design: no git
    checkout or no ambient harness => ``no-worktree`` (enforce nothing), because
    a non-harness context (a plain script, CI) must never be able to brick work.

    Ownership keys on HARNESS, not holder: two claude sessions in one worktree
    are the SAME harness and never refuse each other (AC2-EDGE); a codex session
    entering a claude-owned worktree is ``foreign``. ``override`` (the
    ``FNO_WORKTREE_OK`` escape hatch) downgrades a foreign verdict to
    ``override`` so the caller can proceed - never silent.
    """
    if worktree_root is None or not my_harness:
        return WorktreeGuardResult(verdict=VERDICT_NO_WORKTREE, my_harness=my_harness)

    key = worktree_claim_key(worktree_root)
    wt = str(worktree_root)
    status = claim_status(key, root=root)
    state = status.get("state")

    if state in ("live", "suspect"):
        owner_holder = status.get("holder")
        owner_harness = status.get("harness")
        # My own claim, or a sibling session of the same harness: never refuse.
        if owner_holder == my_holder or owner_harness == my_harness:
            # Keep my own claim fresh (TTL + acquired_at) on my next write. A
            # sibling same-harness holder is left untouched - not ours to refresh.
            if owner_holder == my_holder:
                _try_acquire(key, my_holder, my_harness, session_pid, ttl_ms, root)
            return WorktreeGuardResult(
                verdict=VERDICT_OK,
                worktree=wt,
                my_harness=my_harness,
                owner_harness=owner_harness,
                owner_holder=owner_holder,
                owner_pid=status.get("pid"),
            )
        # A different (or untaggable) harness owns a live claim -> refuse.
        verdict = VERDICT_OVERRIDE if override else VERDICT_FOREIGN
        return WorktreeGuardResult(
            verdict=verdict,
            worktree=wt,
            my_harness=my_harness,
            owner_harness=owner_harness,
            owner_holder=owner_holder,
            owner_pid=status.get("pid"),
        )

    # free / stale / corrupted -> establish ownership. acquire_claim handles
    # stale recovery and the concurrent-double-entry race atomically (exactly
    # one winner); a lost race surfaces as the loser reading a foreign owner.
    claim = _try_acquire(key, my_holder, my_harness, session_pid, ttl_ms, root)
    if claim is None:
        # Raced and lost: re-read to report the winner.
        st = claim_status(key, root=root)
        owner_harness = st.get("harness")
        if owner_harness == my_harness or st.get("holder") == my_holder:
            return WorktreeGuardResult(
                verdict=VERDICT_OK, worktree=wt, my_harness=my_harness,
                owner_harness=owner_harness, owner_holder=st.get("holder"),
                owner_pid=st.get("pid"),
            )
        return WorktreeGuardResult(
            verdict=VERDICT_OVERRIDE if override else VERDICT_FOREIGN,
            worktree=wt, my_harness=my_harness, owner_harness=owner_harness,
            owner_holder=st.get("holder"), owner_pid=st.get("pid"),
        )
    return WorktreeGuardResult(
        verdict=VERDICT_ACQUIRED,
        worktree=wt,
        my_harness=my_harness,
        owner_harness=my_harness,
        owner_holder=my_holder,
        owner_pid=claim.pid,
    )


def _try_acquire(key, holder, harness, session_pid, ttl_ms, root):
    """acquire_claim, but a lost race (ClaimHeldByOther) returns None instead of
    raising - the guard re-reads to report the winner rather than crashing a
    hook on a benign concurrent entry."""
    try:
        return acquire_claim(
            key,
            holder,
            ttl_ms=ttl_ms,
            pid=session_pid,
            harness=harness,
            reason="worktree harness guard",
            root=root,
        )
    except ClaimHeldByOther:
        return None


__all__ = [
    "DEFAULT_TTL_MS",
    "VERDICT_ACQUIRED",
    "VERDICT_FOREIGN",
    "VERDICT_NO_WORKTREE",
    "VERDICT_OK",
    "VERDICT_OVERRIDE",
    "WorktreeGuardResult",
    "guard_worktree",
    "resolve_worktree_root",
    "worktree_claim_key",
]
