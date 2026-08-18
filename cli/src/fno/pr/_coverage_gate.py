"""The merge guard's review-coverage predicate, as one callable.

Lifted out of ``run_merge`` unchanged so a second caller can ask the same
question without a second copy of it. The predicate is MOVED, never restated:
every helper lives in ``fno.pr._merge`` and is reached through the module, so
the merge path is steered by patching ``_merge`` alone. The verb's no-recompute
read calls ``_reviews.latest_review_coverage`` directly, so a test that must
steer BOTH surfaces patches both modules (as this module's own tests do), and
the two readers that already disagree stay two, not three.

One predicate, three reachable surfaces:

  - ``_merge.py``        ``fno pr merge`` (recompute=True)            -> checked
  - ``cli.py``           ``fno pr coverage-check`` (recompute=False)  -> checked
  - ``hooks/git-protection.py`` for a bare ``gh pr merge``, via the verb
    above, stdlib-only and unable to import this module                -> checked

The hook consults the predicate WITHOUT the recompute: that recompute shells
the Rust producer and is budgeted in minutes, while a PreToolUse hook has a
60s harness budget and a killed hook emits no verdict at all. So the
invariant between hook and guard is deliberately one-directional - the hook
never ALLOWS what the guard refuses, but on a missing or stale row the hook
denies where ``fno pr merge`` may yet allow after recomputing. The recovery
from a wrong deny is one command; the recovery from a wrong allow is a
revert.

States are ``COVERED`` / ``REFUSED`` / ``UNANSWERED``. ``UNANSWERED`` is a
real third answer and is narrow on purpose: it means the instrument failed,
never that the instrument looked and found nothing. An empty read is an
answer - nothing has attested this head - and it REFUSES. Only a failed head
fetch or a raised events read reaches ``UNANSWERED``, and both carry a note
naming the probe that died.
"""
from __future__ import annotations

import os
import sys
from typing import Optional, Tuple

from fno.pr import _merge

COVERED = 0
REFUSED = 3
UNANSWERED = 4


def coverage_verdict(
    pr_number: int, repo: str, *, recompute: bool, head: Optional[str] = None
) -> Tuple[int, str, str, str]:
    """Return ``(state, refusal, covered_head, note)``.

    ``refusal`` is the guard's own sentence (the one ``_coverage_refused_reason``
    builds) and is empty unless ``state`` is REFUSED. ``covered_head`` is the
    head the row pins, for the caller's TOCTOU pin; empty when no lane is
    configured or no row survives. ``note`` names a recompute outcome on
    REFUSED, or the dead probe on UNANSWERED.

    ``head`` lets a caller that already fetched the PR's head (``fno pr
    status`` reads it off the same ``gh`` response it built its own verdict
    from) hand it in rather than pay a second ``gh pr view``. Omit it to keep
    the original self-fetching behavior.
    """
    # The guard's own short circuit: a stock install with no review lane
    # configured opts out of coverage entirely. Checked FIRST so neither the
    # head fetch nor the events read runs for a PR nobody configured review
    # for - same order run_merge has always evaluated, one lane probe cheaper.
    if not _merge._review_lane_configured(repo, pr_number):
        return COVERED, "", "", ""

    # The head fetch is an instrument, and it can fail. A None head is not
    # "no coverage" - it is "the probe that pins coverage to what would
    # actually merge could not run", and every answer built on it would
    # describe an unknown commit. Refuse to answer rather than guess.
    if head is None:
        head = _merge._pr_head_oid(pr_number, repo)
        if head is None:
            return UNANSWERED, "", "", "pr head fetch failed"

    code_review_required = _merge._code_review_attestation_required(repo, pr_number)
    if recompute:
        # run_merge's exact path: the gate read fires the standalone producer
        # once when no usable row describes this head, and every failure inside
        # it degrades to the original row plus a note - a swallow there is the
        # answer of record, not a crash.
        cov, recompute_note = _merge._review_coverage_for_pr(pr_number, repo, head)
    else:
        try:
            from fno.pr._reviews import latest_review_coverage

            cov = latest_review_coverage(pr_number, repo)
        except Exception as exc:  # noqa: BLE001 - instrument failure, not absence
            return UNANSWERED, "", "", f"events read raised: {exc}"
        recompute_note = ""

    covered = (
        cov is not None
        and cov.get("coverage") == "covered"
        and _merge._safe_int(cov.get("reviewed_count"), 0) > 0
        and (
            not code_review_required
            or _merge._coverage_has_local_pass(cov, "code-review")
        )
    )
    if covered:
        # Staleness: the event pins a head; if the PR head moved after the gate
        # eval, the coverage no longer describes what would merge. The caller's
        # head is confirmed live here (UNANSWERED above), so a mismatch is
        # always a real mismatch.
        ev_head = cov.get("head_sha") if cov else None
        if ev_head and head != ev_head:
            covered = False
    if covered:
        return COVERED, "", (cov.get("head_sha") or "") if cov else "", recompute_note

    # Same branch order run_merge has always used: the attestation refusal is
    # checked first, so a config requiring code-review with no row names the
    # missing attestation, not the missing row.
    if code_review_required and not _merge._coverage_has_local_pass(cov, "code-review"):
        refusal = (
            "required code-review has no head-pinned local pass attestation; "
            "run the harness review verb at HEAD, then emit the code-review attestation"
        )
    else:
        refusal = _merge._coverage_refused_reason(
            cov, head, _merge._coverage_sources(repo) if cov is None else None
        )
    return REFUSED, refusal, "", recompute_note


def refusal_line(refusal: str, note: str) -> str:
    """The one refusal sentence: the reason with the note bracket-appended.

    Bracket append, never paren-splice surgery on a builder's output: a reason
    whose trailing paren closes an inner clause (a searched list, a truncated
    sha) would swallow the note into the wrong parenthetical. One copy shared
    by ``run_merge`` and ``run_coverage_check`` so the two surfaces cannot grow
    different formatting rules for the same verdict.
    """
    if refusal and note:
        return f"{refusal} [{note}]"
    return refusal or note


def run_coverage_check(
    pr_number: int, recompute: bool = False, cwd: Optional[str] = None
) -> int:
    """The verb body: print the refusal, return the state as an exit code.

    Exit 0 covered, 3 refused (the guard's sentence on stderr), 4 unanswered
    (the note naming the dead probe). Callers that cannot import ``fno`` - a
    stdlib-only hook - read the first stderr line and the exit code.
    """
    repo = cwd or os.getcwd()
    state, refusal, _covered_head, note = coverage_verdict(
        pr_number, repo, recompute=recompute
    )
    if state == REFUSED:
        sys.stderr.write(f"{refusal_line(refusal, note)}\n")
    elif state == UNANSWERED:
        sys.stderr.write(f"{note}\n")
    return state
