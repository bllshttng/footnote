"""Pre-PR stale-base guard.

A worktree branched from a stale local HEAD ships a PR full of phantom
deletions (changes you never made appear as reverts), costing a manual
rebase+repush cycle. ``fno worktree ensure`` bases new worktrees off
``origin/main``, but the EnterWorktree and manual-worktree paths bypass it, so
guard at PR-creation time where every path converges.

One implementation (:func:`check_stale_base`), two call sites: the ``/pr create``
router shells ``fno pr base-check``; ``fno worker ship`` imports the function
directly. The bypass and the staleness rule live only here so both sites behave
identically.

Exit-code contract (callers branch on codes, not text):
    0  pass       (fresh base / behind-count 0 / bypass / fail-open)
    3  stale       (merge-base > max_hours of missing main history behind tip)
    4  unrelated    (no merge-base with the base ref)
"""
from __future__ import annotations

import os
import sys
from typing import Mapping, Optional, Tuple

from fno.pr._proc import ToolMissing, run

BASE_DEFAULT = "origin/main"
MAX_HOURS = 24
BYPASS_ENV = "FNO_PR_BASE_OK"
BYPASS_VALUE = "stale-acknowledged"

OK = 0
REFUSED_STALE = 3
UNRELATED = 4


def _git(args, cwd: str):
    return run(["git", *args], cwd=cwd)


def _committer_epoch(ref: str, cwd: str) -> Optional[int]:
    res = _git(["show", "-s", "--format=%ct", ref], cwd)
    out = res.stdout.strip()
    if res.returncode != 0 or not out:
        return None
    try:
        return int(out)
    except ValueError:
        return None


def _split_base(base: str) -> Tuple[str, str]:
    """``origin/main`` -> ``("origin", "main")``; a bare ref fetches all of origin."""
    if "/" in base:
        remote, ref = base.split("/", 1)
        return remote, ref
    return "origin", base


def _stale_message(span_hours: float, base: str) -> str:
    days = span_hours / 24.0
    return (
        f"stale base: your branch's merge-base with {base} is {days:.1f} days "
        f"behind {base}.\n"
        "PRs from stale bases show phantom deletions (changes you never made "
        "appear as reverts).\n"
        "fix:    fno pr rebase --base=origin/main   (handles fetch + conflicts "
        "+ repush guidance)\n"
        "bypass: FNO_PR_BASE_OK=stale-acknowledged  (deliberate old-base PR; "
        "emits gate_escape)"
    )


def _unrelated_message(base: str) -> str:
    return (
        f"unrelated histories: your branch has no common ancestor with {base}. "
        "That is a bigger problem than a stale base - check you branched from the "
        "right place before opening a PR."
    )


def _emit_bypass(cwd: Optional[str], env: Mapping[str, str], events_path=None) -> None:
    """Fire-and-forget gate_escape on a deliberate bypass. Never blocks (AC2-FR)."""
    try:
        from fno.events.gate_escape import default_dedup_key, emit_gate_escape

        emit_gate_escape(
            "stale-base",
            detail="pre-pr base-check bypass",
            dedup_key=default_dedup_key("stale-base", env),
            events_path=events_path,
            cwd=cwd,
        )
    except Exception:
        pass  # ponytail: telemetry must never block a deliberate ship (AC2-FR)


def check_stale_base(
    base: str = BASE_DEFAULT,
    max_hours: int = MAX_HOURS,
    *,
    cwd: Optional[str] = None,
    env: Optional[Mapping[str, str]] = None,
    events_path=None,
) -> Tuple[int, Optional[str]]:
    """Return ``(exit_code, message)`` without printing or exiting.

    Message is a human string for stderr (a warning on fail-open, a refusal on
    stale/unrelated) or ``None`` on a clean pass.
    """
    e = os.environ if env is None else env
    repo = cwd or os.getcwd()

    # Bypass honored here so both call sites are identical. Only the literal
    # string bypasses; any other value falls through and refuses (AC1-FR).
    if e.get(BYPASS_ENV, "") == BYPASS_VALUE:
        _emit_bypass(repo, e, events_path)
        return OK, None

    remote, ref = _split_base(base)
    # Compare against the remote-tracking ref we actually fetch. `git fetch` only
    # advances refs/remotes/<remote>/<ref>, never a local branch, so a bare
    # `--base main` must still read origin/main - comparing local `main` would
    # read a ref the fetch never touched and pass exactly when it should refuse.
    compare = f"{remote}/{ref}"
    try:
        fetch = _git(["fetch", remote, ref, "--quiet"], repo)
    except ToolMissing:
        return OK, "could not run git; stale-base check skipped"
    if fetch.returncode != 0:
        # Fail-open: a network flake must not block a ship, and gh pr create is
        # about to make the real network test anyway.
        return OK, f"could not refresh {compare}; stale-base check skipped"

    mb = _git(["merge-base", "HEAD", compare], repo)
    mb_sha = mb.stdout.strip()
    if mb.returncode != 0 or not mb_sha:
        return UNRELATED, _unrelated_message(compare)

    behind = _git(["rev-list", "--count", f"{mb_sha}..{compare}"], repo)
    if behind.stdout.strip() == "0":
        return OK, None  # up to date; merge-base age is irrelevant

    tip_ts = _committer_epoch(compare, repo)
    mb_ts = _committer_epoch(mb_sha, repo)
    if tip_ts is None or mb_ts is None:
        return OK, "could not read commit dates; stale-base check skipped"

    span_hours = (tip_ts - mb_ts) / 3600.0
    if span_hours > max_hours:
        return REFUSED_STALE, _stale_message(span_hours, compare)
    return OK, None


def run_base_check(base: str = BASE_DEFAULT, *, cwd: Optional[str] = None) -> int:
    """CLI entry: run the check, print any message to stderr, return the code."""
    code, msg = check_stale_base(base=base, cwd=cwd)
    if msg:
        sys.stderr.write(msg.rstrip("\n") + "\n")
    return code
