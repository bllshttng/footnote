"""Refuse a merge into a base branch that no longer leads to the default branch.

A stacked PR names another feature branch as its base. When that base lands on
main and nobody retargets the stacked PR, GitHub still merges it - into the dead
base - and reports MERGED while the code reaches nobody.

Specimen: PR #800 merged into ``feature/bg-crown`` at 2026-08-10T19:42:32Z, an
hour after PR #789 had landed that same branch on main at 18:05:58Z. Its merge
commit ``9b665db4`` is the tip of ``origin/feature/bg-crown`` and is not an
ancestor of ``origin/main``; its three commits are still not on main. GitHub
reports the PR MERGED.

WHY the base went stale is unexplained, so this module asserts nothing about
GitHub's retarget behavior. It reads observable git/gh facts and refuses if
EITHER of two independent checks fires:

  (i)  a MERGED PR whose head is the base branch exists - the base already
       landed, so nothing is left to carry these commits onward.
  (ii) the base branch tip is already an ancestor of the default branch - the
       base is fully contained in main.

Both are kept because they go blind in different directions. Under
``config.auto_merge.merge_strategy = "squash"`` a landed base is NOT an ancestor
of main (squash mints a new commit), so (ii) sees nothing and (i) still fires.
A base that landed by a path leaving no merged PR blinds (i) while (ii) fires.

(i) deliberately tests for a MERGED PR rather than requiring an OPEN one:
stacking onto a base nobody has opened a PR for yet is legitimate work, and a
guard that refuses it would be switched off within the week.

One predicate, four call sites, because a guard on one of N reachable merge
paths is decorative. The reachable paths in this repo:

  - ``_merge.py``   ``fno pr merge`` (and its API fallback)        -> checked
  - ``_verify.py``  the bounded ``gh pr merge --auto`` remediation -> checked
  - ``finalize.rs`` the autonomous arm, via ``fno pr base-lineage-check`` -> checked
  - ``.github/workflows/stacked-base-guard.yml``, same verb        -> checked
  - the GitHub web/mobile merge button                             -> NOT reachable
  - a human's bare ``gh pr merge``                                 -> NOT reachable

The last two are covered only once an operator marks the workflow a required
status check; nothing in this repo can do that from code.

Verdicts are ``ok`` / ``stale`` / ``unknown``. ``unknown`` is a real third
answer, not a pass: an in-loop CLI caller proceeds with a stderr breadcrumb
(matching ``_merge._behind_by``, because our own probe failing must not wedge a
merge), while CI treats it as a failure, because a check that could not evaluate
has not verified anything.
"""
from __future__ import annotations

import os
import sys
from typing import Optional, Tuple

from fno.pr._proc import run

OK = 0
REFUSED_STALE = 3
UNKNOWN = 4

BYPASS_ENV = "FNO_PR_BASE_LINEAGE_OK"
BYPASS_VALUE = "stale-acknowledged"

#: Probe failed, as distinct from "probe ran and found nothing".
_PROBE_FAILED = -1


def _default_branch(cwd: str) -> Optional[str]:
    res = run(
        ["gh", "repo", "view", "--json", "defaultBranchRef", "-q", ".defaultBranchRef.name"],
        cwd=cwd,
    )
    if not res.ok:
        return None
    return res.stdout.strip() or None


def _base_ref(pr_number: int, cwd: str) -> Optional[str]:
    res = run(
        ["gh", "pr", "view", str(pr_number), "--json", "baseRefName", "-q", ".baseRefName"],
        cwd=cwd,
    )
    if not res.ok:
        return None
    return res.stdout.strip() or None


def _merged_pr_for_head(base: str, cwd: str) -> int:
    """Number of a MERGED PR whose head is ``base``; 0 none, ``_PROBE_FAILED`` on error.

    The three-way return keeps a failed probe from reading as a clean bill of
    health - the whole defect this module guards is a green answer nobody
    actually computed.
    """
    res = run(
        [
            "gh", "pr", "list", "--head", base, "--state", "merged",
            "--limit", "1", "--json", "number", "-q", ".[0].number",
        ],
        cwd=cwd,
    )
    if not res.ok:
        return _PROBE_FAILED
    out = res.stdout.strip()
    if not out or out == "null":
        return 0
    try:
        return int(out)
    except ValueError:
        return _PROBE_FAILED


def _base_contained_in_default(base: str, default: str, cwd: str) -> Optional[bool]:
    """Whether ``origin/base`` is an ancestor of ``origin/default``; None on probe error.

    Both refs are fetched with explicit refspecs first. Reading a
    remote-tracking ref without fetching answers from whatever the last fetch
    left behind, and a stale ref is exactly how a base that has since landed
    still looks alive.
    """
    fetch = run(
        [
            "git", "fetch", "origin",
            f"+refs/heads/{base}:refs/remotes/origin/{base}",
            f"+refs/heads/{default}:refs/remotes/origin/{default}",
        ],
        cwd=cwd,
    )
    if not fetch.ok:
        return None
    res = run(
        ["git", "merge-base", "--is-ancestor", f"origin/{base}", f"origin/{default}"],
        cwd=cwd,
    )
    if res.returncode == 0:
        return True
    if res.returncode == 1:
        return False
    return None


def lineage_verdict(pr_number: int, cwd: str) -> Tuple[str, str]:
    """``(verdict, reason)`` for merging PR ``pr_number`` into its declared base.

    ``verdict`` is ``ok`` | ``stale`` | ``unknown``. Both probes run before any
    verdict is chosen, so an ``unknown`` from one cannot mask a ``stale`` from
    the other - the ordering that would otherwise let a gh hiccup on the first
    probe hide the answer the second already had.
    """
    default = _default_branch(cwd)
    if default is None:
        return ("unknown", "could not read the repository default branch (gh repo view failed)")

    base = _base_ref(pr_number, cwd)
    if base is None:
        return ("unknown", f"could not read the base ref of PR #{pr_number} (gh pr view failed)")

    if base == default:
        return ("ok", f"base is the default branch ({default})")

    merged = _merged_pr_for_head(base, cwd)
    contained = _base_contained_in_default(base, default, cwd)
    retarget = f"retarget it first: gh pr edit {pr_number} --base {default}"

    if merged > 0:
        return (
            "stale",
            f"base branch '{base}' already landed via merged PR #{merged}, so merging "
            f"PR #{pr_number} into it would report MERGED while the commits never reach "
            f"'{default}'; {retarget}",
        )
    if contained is True:
        return (
            "stale",
            f"base branch '{base}' is already fully contained in '{default}', so merging "
            f"PR #{pr_number} into it would report MERGED while the commits never reach "
            f"'{default}'; {retarget}",
        )

    if merged == _PROBE_FAILED and contained is None:
        return ("unknown", f"both lineage probes failed for base '{base}' (gh and git)")
    if merged == _PROBE_FAILED:
        return ("unknown", f"merged-PR probe failed for base '{base}' (gh pr list)")
    if contained is None:
        return ("unknown", f"ancestry probe failed for base '{base}' (git fetch or merge-base)")

    return ("ok", f"base '{base}' still leads to '{default}'")


def bypassed() -> bool:
    """Whether the operator acknowledged a stale base for this invocation."""
    return os.environ.get(BYPASS_ENV, "") == BYPASS_VALUE


def emit_bypass_escape(pr_number: int, cwd: str, reason: str) -> None:
    """Record a bypassed refusal as autonomy debt. Telemetry never blocks."""
    try:
        from fno.events.gate_escape import emit_gate_escape

        emit_gate_escape(
            "other",
            pr=pr_number,
            detail=f"stacked-base guard bypassed via {BYPASS_ENV}: {reason}",
            cwd=cwd,
        )
    except Exception:  # noqa: BLE001 - telemetry never blocks a merge
        pass


def run_base_lineage_check(pr_number: int, cwd: Optional[str] = None) -> int:
    """CLI entry: 0 ok/bypassed, 3 stale, 4 unknown.

    ``unknown`` is exit 4 rather than 0 so a caller that cannot tolerate an
    unevaluated gate (CI) refuses on it, while the in-process callers that must
    not wedge on a gh outage keep their own fail-open policy in their own code.
    """
    repo = cwd or os.getcwd()
    verdict, reason = lineage_verdict(pr_number, repo)
    if verdict == "ok":
        sys.stdout.write(f"base-lineage: ok - {reason}\n")
        return OK
    if verdict == "stale":
        if bypassed():
            emit_bypass_escape(pr_number, repo, reason)
            sys.stdout.write(f"base-lineage: bypassed ({BYPASS_ENV}) - {reason}\n")
            return OK
        sys.stderr.write(
            f"base-lineage: REFUSED - {reason}\n"
            f"  bypass (records a gate_escape): {BYPASS_ENV}={BYPASS_VALUE}\n"
        )
        return REFUSED_STALE
    sys.stderr.write(f"base-lineage: unknown - {reason}\n")
    return UNKNOWN
