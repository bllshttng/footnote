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
ANY of three independent checks fires:

  (i)  a MERGED PR whose head is the base branch exists - the base already
       landed, so nothing is left to carry these commits onward.
  (ii) the base branch tip is already an ancestor of the default branch - the
       base is fully contained in main.
  (iii) the base branch is confirmed absent from the remote - the state
       ``delete_branch_on_merge`` leaves behind, where (i) and (ii) can both go
       blind (a fresh clone has no ``origin/<base>`` to reason from at all, and
       a stale one can disagree with the merged head under squash).

All three are kept because they go blind in different directions. Under
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
  - ``hooks/git-protection.py`` for an agent-run bare ``gh pr merge`` -> checked
  - the GitHub web/mobile merge button                             -> NOT reachable

The hook is a ``PreToolUse`` chokepoint that already gated ``gh pr merge`` with
its own two-factor check, and it is wired on BOTH harnesses
(``hooks/hooks.json``, ``hooks/codex-hooks.json``), so it covers most of the
merge population here: agents running gh through a tool call. It was very nearly
left unwired on the argument that a guard over one of the ways a human runs a
command invites the belief that the command is guarded - but that reasoning
assumed a single harness and a mostly-human population, and both are wrong.
Leaving lineage out of a gate that already claims to gate this command would
have made THAT gate the incomplete one.

What the hook does not reach: a human typing ``gh pr merge`` in a plain
terminal, and any harness not wired to it. Those, and the web button, are
covered only once an operator marks the workflow's ``stacked-base-guard`` status
context required, which nothing in this repo can do from code.

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

#: Every probe is bounded. `_merge.py` calls this INSIDE the repo-wide merge
#: lock, so a `git fetch` that hangs on a dead remote or a prompting credential
#: helper would hold that lock indefinitely and leave every other lane retrying
#: a `held` forever. A timeout reads as `unknown`, which is already the
#: proceed-with-a-breadcrumb path.
_PROBE_TIMEOUT_S = 30


def _probe(args: list, cwd: str):
    """Run a bounded probe; None when it could not run at all.

    ``ToolMissing`` is caught here rather than at each call site: the in-process
    callers (``_merge.py``, ``_verify.py``) promise that an unevaluated probe
    degrades to a breadcrumb, and an uncaught ``ToolMissing`` from a box with no
    ``git`` on PATH would instead surface as a traceback out of ``fno pr
    verify`` - the opposite of the stated contract.
    """
    try:
        return run(args, cwd=cwd, timeout=_PROBE_TIMEOUT_S)
    except Exception:  # noqa: BLE001 - incl. ToolMissing, subprocess.TimeoutExpired
        return None


def _default_branch(cwd: str) -> Optional[str]:
    res = _probe(
        ["gh", "repo", "view", "--json", "defaultBranchRef", "-q", ".defaultBranchRef.name"],
        cwd,
    )
    if res is None or not res.ok:
        return None
    return res.stdout.strip() or None


def _base_ref(pr_number, cwd: str) -> Optional[str]:
    res = _probe(
        ["gh", "pr", "view", str(pr_number), "--json", "baseRefName", "-q", ".baseRefName"],
        cwd,
    )
    if res is None or not res.ok:
        return None
    return res.stdout.strip() or None


def _merged_pr_for_head(base: str, cwd: str) -> tuple:
    """``(pr_number, head_oid)`` of the newest MERGED PR whose head is ``base``.

    ``(0, "")`` when none exists, ``(_PROBE_FAILED, "")`` on a probe error. The
    three-way return keeps a failed probe from reading as a clean bill of health
    - the whole defect this module guards is a green answer nobody computed.

    ``head_oid`` rides along because the PR's existence alone is a permanent
    historical fact, not a statement about the branch as it stands now. See
    :func:`lineage_verdict` for why the caller compares it to the live tip.
    """
    res = _probe(
        [
            "gh", "pr", "list", "--head", base, "--state", "merged",
            "--limit", "1", "--json", "number,headRefOid",
            "-q", '.[0] | "\\(.number) \\(.headRefOid)"',
        ],
        cwd,
    )
    if res is None or not res.ok:
        return (_PROBE_FAILED, "")
    out = res.stdout.strip()
    if not out or out.startswith("null"):
        return (0, "")
    parts = out.split()
    try:
        number = int(parts[0])
    except (ValueError, IndexError):
        return (_PROBE_FAILED, "")
    head = parts[1] if len(parts) > 1 else ""
    if not head or head == "null":
        # A merged PR with no readable head oid leaves (i) UNEVALUATED, not
        # clean: without the oid the caller cannot compare it to the live tip,
        # and returning the number alone would fall through to `ok` - a green
        # answer nobody computed, the exact defect this module guards.
        return (_PROBE_FAILED, "")
    return (number, head)


def _fetch_ref(ref: str, cwd: str) -> bool:
    """Fetch one branch with an explicit refspec. Reading a remote-tracking ref
    without fetching answers from whatever the last fetch left behind, and a
    stale ref is exactly how a base that has since landed still looks alive."""
    fetch = _probe(
        ["git", "fetch", "origin", f"+refs/heads/{ref}:refs/remotes/origin/{ref}"],
        cwd,
    )
    return fetch is not None and fetch.ok


def _base_ref_gone(base: str, cwd: str) -> bool:
    """True only when the remote CONFIRMS ``base`` no longer exists.

    Asked only when the base fetch failed, to separate the two reasons it can:
    the branch was deleted (the state this module exists to catch, where the
    pinned local ``origin/<base>`` is still the truth) from a transient error
    (where that same local ref is merely stale). A probe error here answers
    False, so an unanswerable question keeps the git-side checks blind rather
    than letting them trust a ref nobody refreshed.
    """
    res = _probe(["git", "ls-remote", "--heads", "origin", f"refs/heads/{base}"], cwd)
    return res is not None and res.ok and not res.stdout.strip()


def _fetch_refs(base: str, default: str, cwd: str) -> Tuple[bool, bool, bool]:
    """Refresh both refs; ``(default_current, base_ref_trustworthy, base_gone)``.

    One fetch carrying both refspecs fails wholesale when either ref is gone,
    and the base ref being gone is not an edge case: `delete_branch_on_merge`
    deletes it the moment the base lands, which is precisely the state this
    module exists to catch. That failure left `origin/default` unrefreshed too,
    blinded both git-side checks, and produced `unknown` - which every
    in-process caller treats as proceed. Fetched separately, a deleted base
    ref costs nothing: `origin/<base>` still exists locally, pinned at the
    commit that landed, so (i) matches it against the merged PR's head and (ii)
    still resolves ancestry. A base that never existed locally leaves `_rev`
    empty, which is why ``base_gone`` rides along: a fresh clone (every CI
    runner, and any worktree that never fetched the branch) has no
    `origin/<base>` to pin, so both git-side checks go blind in exactly the
    scenario this module was written for, and `unknown` is proceed. The third
    flag lets :func:`lineage_verdict` answer from the deletion itself.

    A FAILED base fetch is not the same as a deleted branch, though, and the
    fetch alone cannot tell them apart. Trusting the local ref either way cuts
    both ways: a transient failure on a branch that has since moved on reads as
    "landed and unmoved" and REFUSES a healthy stacked PR, while a local ref
    that predates the base's last push reads as moved and lets the real stale
    base through. So a failed base fetch is trusted only once ``git ls-remote``
    confirms the branch is actually gone.
    """
    default_ok = _fetch_ref(default, cwd)
    if _fetch_ref(base, cwd):
        return default_ok, True, False
    base_gone = _base_ref_gone(base, cwd)
    return default_ok, base_gone, base_gone


def _rev(ref: str, cwd: str) -> str:
    res = _probe(["git", "rev-parse", ref], cwd)
    return res.stdout.strip() if res is not None and res.ok else ""


def _base_contained_in_default(base: str, default: str, cwd: str) -> Optional[bool]:
    """Whether ``origin/base`` is an ancestor of ``origin/default``; None on error."""
    res = _probe(
        ["git", "merge-base", "--is-ancestor", f"origin/{base}", f"origin/{default}"],
        cwd,
    )
    if res is None:
        return None
    if res.returncode == 0:
        return True
    if res.returncode == 1:
        return False
    return None


def lineage_verdict(pr_number, cwd: str) -> Tuple[str, str]:
    """``(verdict, reason)`` for merging PR ``pr_number`` into its declared base.

    ``pr_number`` is whatever identifier ``gh pr view`` accepts - a number, a
    URL, a branch name - and is never parsed here. ``fno pr verify`` takes its
    ``--pr-number`` as a free-form string for exactly that reason, so an
    ``int()`` at the call site would turn a URL into a ValueError traceback out
    of the remediation arm, after every precondition had already passed.

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

    merged, merged_head = _merged_pr_for_head(base, cwd)
    fetched, base_current, base_gone = _fetch_refs(base, default, cwd)
    git_ok = fetched and base_current
    base_tip = _rev(f"origin/{base}", cwd) if git_ok else ""
    contained = _base_contained_in_default(base, default, cwd) if git_ok else None
    retarget = f"retarget it first: gh pr edit {pr_number} --base {default}"

    # (i) fires only when the branch has not moved since that PR merged it.
    # A merged PR is a permanent historical fact about a NAME, not a statement
    # about the branch as it stands now: delete `feature/x` after merging it,
    # recreate it later for new work, and the old PR still answers this query
    # forever. Comparing to the live tip keeps the specimen caught (#789 merged
    # head 85b90e485, which was still feature/bg-crown's tip when #800 merged)
    # while a recreated or continued branch reads as alive - the false refusal
    # that would otherwise get this guard switched off.
    if merged > 0 and merged_head and base_tip and merged_head == base_tip:
        return (
            "stale",
            f"base branch '{base}' already landed via merged PR #{merged} and has not "
            f"moved since ({base_tip[:8]}), so merging PR #{pr_number} into it would "
            f"report MERGED while the commits never reach '{default}'; {retarget}",
        )
    # (iii) The branch is confirmed absent from the remote, so it carries
    # nothing onward whatever killed it - `delete_branch_on_merge` removes the
    # base the moment it lands. Both other checks can miss this: a fresh clone
    # (every CI runner, and any worktree that never fetched the branch) has no
    # `origin/<base>` to compare a tip against or resolve ancestry from, and a
    # STALE local ref is just as bad - under squash the landed base is no
    # ancestor of the default branch, and a local ref left behind the merged
    # head fails (i)'s equality test too, so both answer "healthy" about a
    # branch that no longer exists. Gating this on `not base_tip` therefore let
    # a confirmed deletion return `ok`, and `unknown`/`ok` both fail OPEN in
    # every in-process caller - the whole defect. `base_gone` is only ever True
    # after `git ls-remote` answered, so a network failure still lands in
    # `unknown` below.
    if base_gone:
        landed = f" (it landed via merged PR #{merged})" if merged > 0 else ""
        return (
            "stale",
            f"base branch '{base}' no longer exists on the remote{landed}, so merging "
            f"PR #{pr_number} into it would report MERGED while the commits never reach "
            f"'{default}'; {retarget}",
        )
    if contained is True:
        # Known, accepted false positive: a base branch created off the default
        # branch whose own work is not pushed yet is ALSO fully contained, and
        # is locally indistinguishable from one that landed by a direct push
        # (both are just an ancestor of the default branch, and neither leaves a
        # merged PR for (i) to see). The message names that case rather than
        # pretending the verdict is certain, because the reader is the only one
        # who can tell the two apart.
        return (
            "stale",
            f"base branch '{base}' is already fully contained in '{default}' (nothing on "
            f"it that '{default}' lacks), so merging PR #{pr_number} into it would report "
            f"MERGED while the commits never reach '{default}'; {retarget}. If '{base}' is "
            f"instead a new branch whose own commits are not pushed yet, push them first, "
            f"or set {BYPASS_ENV}={BYPASS_VALUE}",
        )

    # Only reached when neither check fired. A probe that could not run leaves
    # its half of the question unanswered, so it is `unknown` rather than a
    # pass, and the reason names WHICH probe - a caller reading "unknown" alone
    # cannot tell a broken gh from a broken git.
    git_blind = not git_ok or contained is None or not base_tip
    if merged == _PROBE_FAILED and git_blind:
        return ("unknown", f"both lineage probes failed for base '{base}' (gh and git)")
    if merged == _PROBE_FAILED:
        return ("unknown", f"merged-PR probe failed for base '{base}' (gh pr list)")
    if git_blind:
        return ("unknown", f"ancestry probe failed for base '{base}' (git fetch or merge-base)")

    return ("ok", f"base '{base}' still leads to '{default}'")


def bypassed() -> bool:
    """Whether the operator acknowledged a stale base for this invocation."""
    return os.environ.get(BYPASS_ENV, "") == BYPASS_VALUE


def emit_bypass_escape(pr_number, cwd: str, reason: str) -> None:
    """Record a bypassed refusal as autonomy debt. Telemetry never blocks.

    ``pr`` is an int downstream (``emit_gate_escape`` does ``pr <= 0`` and
    dedups on ``(reason, pr)``), while this module's identifier is free-form -
    ``fno pr verify`` carries its PR as a str. Passing the str through raised
    TypeError straight into the swallow below, so the bypass on that path
    recorded NO escape at all: a fail-open telemetry path that reported an
    unbypassed run. The identifier is kept in ``detail`` for the URL/branch
    forms that cannot become an int.
    """
    try:
        from fno.events.gate_escape import emit_gate_escape

        try:
            pr = int(str(pr_number).strip())
        except ValueError:
            pr = None
        emit_gate_escape(
            "other",
            pr=pr,
            detail=f"stacked-base guard bypassed via {BYPASS_ENV} (PR {pr_number}): {reason}",
            cwd=cwd,
        )
    except Exception:  # noqa: BLE001 - telemetry never blocks a merge
        pass


def run_base_lineage_check(pr_number, cwd: Optional[str] = None) -> int:
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
