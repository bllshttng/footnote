"""The merge guard's review-coverage predicate, as one callable.

Lifted out of ``run_merge`` unchanged so a second caller can ask the same
question without a second copy of it. The predicate is MOVED, never restated:
every helper lives in ``fno.pr._merge`` and is reached through the module, so
the merge path is steered by patching ``_merge`` alone. The verb's no-recompute
read calls ``_reviews.review_coverage_for_head_row`` directly, so a test that
must steer BOTH surfaces patches both modules (as this module's own tests do),
and the two readers that already disagree stay two, not three. Patch the name
this module actually calls: ``latest_review_coverage`` is a thin wrapper no
longer on this path, so patching it steers nothing and a test built on it
passes on a read that quietly succeeded.

One predicate, three reachable surfaces:

  - ``_merge.py``        ``fno do pr merge`` (recompute=True)            -> checked
  - ``cli.py``           ``fno do pr coverage-check`` (recompute=False)  -> checked
  - ``hooks/git-protection.py`` for a bare ``gh pr merge``, via the verb
    above, stdlib-only and unable to import this module                -> checked

The hook consults the predicate WITHOUT the recompute: that recompute shells
the Rust producer and is budgeted in minutes, while a PreToolUse hook has a
60s harness budget and a killed hook emits no verdict at all. So the
invariant between hook and guard is deliberately one-directional - the hook
never ALLOWS what the guard refuses, but on a missing or stale row the hook
denies where ``fno do pr merge`` may yet allow after recomputing. The recovery
from a wrong deny is one command; the recovery from a wrong allow is a
revert.

States are ``COVERED`` / ``REFUSED`` / ``UNANSWERED``. ``UNANSWERED`` is a
real third answer and is narrow on purpose: it means the instrument failed,
never that the instrument looked and found nothing. An empty read is an
answer - nothing has attested this head - and it REFUSES. Only a failed head
fetch or a raised events read reaches ``UNANSWERED``, and both carry a note
naming the probe that died.

The one deliberate bypass used to be the ``coverage-override`` label alone: it
answers COVERED with a note carrying ``OVERRIDE_NOTE_PREFIX``, so a caller can
always tell a merge that was reviewed from a merge that was waived. Operator
law joins it through ``fno.decide.current_law`` - one standing subject, one
head-scoped subject minted by the attended ``coverage-waive`` command - with
the same prefix on its receipts and the same fail-closed reading of anything
the decision store could not answer.
"""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any, Optional, Tuple

from fno.pr import _merge

COVERED = 0
REFUSED = 3
UNANSWERED = 4
#: The fourth state (Locked Decision 4): the round budget is spent with
#: blocking findings still non-terminal, so no further review can clear it.
#: Distinct from REFUSED so a caller can tell "needs another round" from
#: "cannot be cleared by reviewing" - and from UNANSWERED, which is an
#: instrument failure, never a verdict.
IMPOSSIBLE = 5

#: The gate's own copy of the harmless-category allowlist (Locked Decision 6:
#: two implementations of one rule, held equal by a shared corpus). The gate
#: never imports the producer-side classifier: it re-derives from the
#: per-finding primitives so a hand-written event claiming
#: ``findings_blocking: 0`` over a CONFIRMED finding is refused.
GATE_NONBLOCKING_CATEGORIES = frozenset(
    {
        "style",
        "formatting",
        "naming",
        "docs",
        "typo",
        "nit",
        "simplification",
        "test-coverage",
    }
)

# The 3am release valve, read in the ONE predicate so it opens on every
# surface the docs point at. `docs/best-practices.md` and
# `docs/troubleshooting.md` both tell the operator to route merges through
# `fno do pr merge` and name the `coverage-override` label as the only way past
# an uncovered head. Read it anywhere but here and that is false: the gate
# refuses at step 2a, the publisher's override branch is never reached, and
# the only path through is the raw `gh` the same docs forbid.
#
# The prefix is the caller's discriminator. An overridden COVERED is a merge
# that landed on the valve, never a merge that was reviewed, and a receipt
# that cannot tell the two apart is a receipt that lies.
OVERRIDE_NOTE_PREFIX = "override: "

# The note on the one COVERED answer that means "this gate does not apply
# here". A reader cannot tell it from a real covered verdict by the empty
# covered_head alone - a covered row that carried no head_sha returns the same
# empty pin - so the discriminator is named rather than inferred.
NO_LANE_NOTE = "no review lane configured"

# The standing operator-law subject for review coverage. One live law verdict
# here (single, on the law lane) waives the coverage conjunct for every PR in
# the fleet, narrowed by the gate: it never clears an unresolved CONFIRMED
# correctness or security finding and never bypasses the checks conjunct. The
# per-head exit beside it is the `coverage-waive` command's head-scoped
# subject, which IS that strong and dies on the next push.
STANDING_WAIVER_SUBJECT = "review-coverage-waiver"

#: The one decision value that counts as an affirmative waiver. The
#: ``coverage-waive`` command mints exactly this string; the gate reads a
#: single law row at a waiver subject as authority ONLY when its decision
#: equals it. Existence alone carries no polarity: a note or a denial
#: recorded at the subject must read as no waiver, never as one.
WAIVER_DECISION = "review coverage waived for this head"


def scoped_waiver_subject(slug: str, pr_number: int, head: str) -> str:
    """The head-pinned waiver subject: one ruling authorizes exactly one head,
    so a push mints a new subject the old ruling cannot answer."""
    return f"{STANDING_WAIVER_SUBJECT}:{slug}#{pr_number}@{head}"


def law_authority(subject: str) -> Tuple[str, str]:
    """Three-state law resolution for one subject: ``(status, probe)``.

    ``status`` is ``single`` / ``none`` / ``unknown``. The deciding list is
    the decision engine's own law-lane live read (the exact filter behind
    ``current_law``) - this wrapper adds no second reader and never scans the
    index file. Damaged rows, a conflicting pair, a failed read, or malformed
    output fold into ``unknown``, because a probe that died is never the same
    answer as a verdict of none. ``probe`` is empty unless status is unknown,
    and then names what died.

    A single row counts as authority only when its decision EQUALS
    ``WAIVER_DECISION`` - the exact value the ``coverage-waive`` command
    mints. Row existence carries no polarity: a note or a denial recorded at
    a waiver subject reads as none, never as a waiver. A single row whose
    decision is missing or unreadable is malformed authority, not a clean no:
    that is ``unknown`` with the field named, the same rule that folds a
    damaged row.
    """
    try:
        from fno.decide import list_decisions

        _label, rows, damaged = list_decisions(subject, lane="law", state="live")
    except Exception as exc:  # noqa: BLE001 - a dead probe is unknown, never none
        return (
            "unknown",
            f"decision probe failed for {subject}: {type(exc).__name__}: {exc}",
        )
    if damaged:
        noun = "row" if damaged == 1 else "rows"
        return "unknown", f"decision probe: {damaged} damaged {noun} for {subject}"
    if not rows:
        return "none", ""
    if len(rows) > 1:
        return "unknown", f"decision probe: conflicting law rows for {subject}"
    decision = rows[0].get("decision")
    if decision is None:
        return (
            "unknown",
            f"decision probe: single law row carries no decision for {subject}",
        )
    if str(decision) == WAIVER_DECISION:
        return "single", ""
    return "none", ""


def operator_waiver_verdict(
    slug: Optional[str], pr_number: int, head: str, hard_findings: list
) -> Tuple[bool, str, str]:
    """The operator-law overlay for a head ordinary coverage refused.

    Returns ``(waived, note, probe_note)``. ``waived`` is True only where
    operator law authorizes this exact shape: a head-scoped waiver decision
    first (explicit, per-head, strong enough to clear even a hard finding, and
    dead the moment the head moves), then a single standing ruling - which
    never clears an unresolved CONFIRMED correctness or security finding.
    ``note`` carries OVERRIDE_NOTE_PREFIX on a waiver so a waived merge and a
    reviewed one stay legible apart. ``probe_note`` names any decision probe
    that answered unknown authority; a caller must never read it as absence.
    """
    scoped_status, scoped_probe = "none", ""
    if slug:
        scoped_status, scoped_probe = law_authority(
            scoped_waiver_subject(slug, pr_number, head)
        )
    if scoped_status == "single":
        return (
            True,
            f"{OVERRIDE_NOTE_PREFIX}head-pinned operator waiver at {head[:8]}",
            "",
        )
    standing_status, standing_probe = law_authority(STANDING_WAIVER_SUBJECT)
    if standing_status == "single" and not hard_findings:
        return True, f"{OVERRIDE_NOTE_PREFIX}standing operator law", ""
    probe_note = "; ".join(p for p in (scoped_probe, standing_probe) if p)
    return False, "", probe_note


def _repo_slug(cwd: str) -> Optional[str]:
    """The canonical ``owner/repo`` for the waiver subject, local git only."""
    from fno.pr._proc import run
    from fno.pr._rest import _repo_slug_reason

    def _bounded(cmd, **kwargs):
        kwargs.setdefault("timeout", 30.0)
        return run(cmd, **kwargs)

    try:
        slug, _reason = _repo_slug_reason(cwd, _bounded)
    except Exception:  # noqa: BLE001 - an unreadable slug skips the scoped exit
        return None
    return slug or None


def unresolved_hard_findings(
    cwd: str, head: str, head_branch: str, cov: Optional[dict]
) -> list:
    """The unresolved CONFIRMED correctness/security finding keys, derived
    exactly as ``_ordinary_verdict`` derives them (the same chain scan, the
    same disposition re-derivation). The standing operator waiver consults
    this list; a second severity table here would be a second place the two
    surfaces could disagree."""
    chain = attestation_chain(cwd, head_branch=head_branch, head=head)
    _text, _note, _named, hard = disposition_refusal(chain, cov, cwd)
    return hard


def run_coverage_waive(pr_number: int, reason: str, cwd: Optional[str] = None) -> int:
    """Record the attended, head-pinned operator waiver for one PR head.

    The one per-head operator exit: resolves the canonical slug and the live
    40-hex head, records an operator decision at the head-scoped subject, and
    prints the positive receipt only after the index write lands. Exit 0
    recorded; 2 no reason; 3 refused provenance (a harness-identified session
    is not the operator); 4 an unreadable slug or head; 1 a failed decision
    write, which records nothing a gate could read.
    """
    cwd = cwd or os.getcwd()
    text = (reason or "").strip()
    if not text:
        sys.stderr.write("coverage-waive refused: --reason is required and empty\n")
        return 2
    slug = _repo_slug(cwd)
    if not slug:
        sys.stderr.write(
            "coverage-waive refused: repository slug unreadable "
            "(git remote get-url origin in this checkout)\n"
        )
        return 4
    head = _merge._pr_head_oid(pr_number, cwd)
    if not head or len(head) != 40 or set(head.lower()) - set("0123456789abcdef"):
        sys.stderr.write(
            "coverage-waive refused: pr head fetch failed or is not a live "
            "40-hex head\n"
        )
        return 4
    subject = scoped_waiver_subject(slug, pr_number, head)
    from fno.decide import (
        IndexWriteError,
        RefusedAuthorityError,
        UnattributedAuthorityError,
        record_decision,
    )

    try:
        record_decision(
            decision=WAIVER_DECISION,
            subject=subject,
            rationale=text,
            authority_source="operator",
        )
    except (RefusedAuthorityError, UnattributedAuthorityError) as exc:
        sys.stderr.write(
            f"coverage-waive refused: {exc}. An operator waiver needs an "
            "attended operator terminal; a session a harness identifies "
            "records nothing.\n"
        )
        return 3
    except IndexWriteError as exc:
        sys.stderr.write(
            f"coverage-waive failed: the decision is durable but not yet "
            f"recoverable ({exc}); run `fno backlog decide-reindex`. Do not "
            "re-run this command - that records the waiver twice. No gate can "
            "read it yet.\n"
        )
        return 1
    except Exception as exc:  # noqa: BLE001 - no receipt without a durable record
        sys.stderr.write(
            f"coverage-waive failed: decision write failed: "
            f"{type(exc).__name__}: {exc}\n"
        )
        return 1
    # Publish the waiver green NOW, so GitHub's ruleset sees the context the
    # law already answers. Best-effort: the recorded law is the authority, a
    # failed POST is the documented publisher lag, and the next publisher run
    # heals it.
    try:
        from fno.pr import _reviews

        posted, why = _reviews.publish_coverage_status(pr_number, head=head, cwd=cwd)
        if not posted:
            sys.stderr.write(
                f"coverage-waive: status publish failed ({why}); the waiver is "
                "recorded and the next publisher run will post it.\n"
            )
    except Exception as exc:  # noqa: BLE001 - the record stands without the marker
        sys.stderr.write(
            f"coverage-waive: status publish raised ({exc}); the waiver is "
            "recorded and the next publisher run will post it.\n"
        )
    sys.stdout.write(f"coverage waiver recorded: {slug}#{pr_number}@{head[:8]}\n")
    return 0


def _pr_author_login(pr_number: int, cwd: str) -> Optional[str]:
    """The PR author's login, or None on any read failure. Only read when an
    override label is held (the one consumer), so a healthy PR never pays
    for it."""
    try:
        from fno.pr._proc import run
        from fno.pr._rest import fetch_pr_info_rest

        # Bounded like _pr_reviews, and for the same reason: this runs inside
        # `fno do pr merge`, and bare `run` passes timeout=None and does not
        # catch TimeoutExpired, so a stalled gh would hang the merge verb
        # rather than fail closed.
        def _bounded(cmd, **kwargs):
            kwargs.setdefault("timeout", 30.0)
            return run(cmd, **kwargs)

        info, _reason = fetch_pr_info_rest(str(pr_number), cwd=cwd, runner=_bounded)
        return str((info or {}).get("author") or "") or None
    except Exception:  # noqa: BLE001 - an unreadable author fails closed below
        return None


def _override_valve(pr_number: int, cwd: str) -> tuple[bool, str, str]:
    """``(valid, note, refusal)`` for the PR's override label.

    The one deliberate bypass, with the same author check the approval path
    beside it has always had: a label applied by the PR AUTHOR does not open
    the valve, because an agent that cannot merge can label its own PR, and
    that is reject-and-attest wearing a label. The actor is unreadable, or
    the author is, and the valve refuses fail-closed - an unreadable actor is
    exactly the state a forger produces, and the approval path already fails
    closed on the same ambiguity. Who applied the label is named in every
    arm (the receipt survives both verdicts): the recovery from a wrong
    refusal is one command; the recovery from a wrong merge is a revert.
    """
    try:
        # Inside the try: an unreadable label state must not open the valve,
        # and an ImportError here is one more way to be unreadable. Hoisting
        # it out would propagate out of coverage_verdict instead.
        from fno.pr import _reviews

        held, actor = _reviews._override_label_actor(pr_number, cwd, _reviews.run)
    except Exception:  # noqa: BLE001 - an unreadable label is not an override
        return False, "", ""
    if not held:
        return False, "", ""
    if not actor:
        return (
            False,
            "",
            "coverage-override label present but the labelling actor is "
            "unreadable; refused (an unreadable actor is not an operator "
            "waiver)",
        )
    author = _pr_author_login(pr_number, cwd)
    if not author:
        return (
            False,
            "",
            f"coverage-override label applied by {actor} but the PR author is "
            "unreadable; refused (cannot prove the labeller is not the author)",
        )
    # Case-insensitive, matching the Rust side's login_equals and _merge.py's
    # node-slug compare: a GitHub login is itself case-insensitive, and an
    # exact-case compare here opens the valve for the author the moment the
    # events feed and /pulls/{n} disagree on casing.
    if actor.lower() == author.lower():
        return (
            False,
            "",
            f"coverage-override label applied by {actor}, the PR author; "
            "refused (an author cannot override its own review gate)",
        )
    return (
        True,
        f"{OVERRIDE_NOTE_PREFIX}{_reviews.COVERAGE_OVERRIDE_LABEL} "
        f"label applied by {actor}",
        "",
    )


def covered_conjuncts(
    cov: Optional[dict], head: str, code_review_required: bool
) -> Tuple[bool, str]:
    """Which gate conjuncts a coverage row satisfies: ``(covered, failed)``.

    The single spelling of "does this row clear the coverage guard", consumed
    by ``coverage_verdict`` below AND by ``fno do pr status``'s ``ready``
    conjunct, so status can never report ready on a row merge refuses (a
    partial copy of this predicate is exactly how it did). ``failed`` is
    empty when covered, else one of ``uncovered`` / ``no_local_pass`` /
    ``stale_head`` naming the conjunct that broke; callers map it to their
    own blocker names. ``reviewer_refused`` is distinct from generic
    ``uncovered`` so consumers can prescribe reviewer recovery or a local
    review instead of waiting for evidence that will not arrive. An empty
    ``head`` skips the staleness conjunct: the
    gate always reaches here with a confirmed-live head, and a status read
    missing one degrades on staleness rather than guessing a mismatch.
    """
    if cov is not None and cov.get("review_state") == "reviewer_refused":
        return False, "reviewer_refused"
    if not (
        cov is not None
        and cov.get("coverage") == "covered"
        and cov.get("review_state") == "reviewed"
    ):
        return False, "uncovered"
    if code_review_required and not _merge._coverage_has_local_pass(cov, "code-review"):
        return False, "no_local_pass"
    # Staleness: the event pins a head; if the PR head moved after the row was
    # computed, the coverage no longer describes what would merge.
    ev_head = cov.get("head_sha") if cov else None
    if ev_head and head and head != ev_head:
        return False, "stale_head"
    return True, ""


def _repo_root(cwd: str) -> Path:
    """The repo root ``load_settings_for_repo`` wants. ``_repo_state_dir``
    already does the ``rev-parse --show-toplevel``, so a ``cwd`` that names a
    subdirectory still resolves to the checkout whose config the gate reads."""
    return Path(_merge._repo_state_dir(cwd)).parent


def _github_approval_satisfies(cwd: str) -> bool:
    """The resolved flag, read through ``_reviews``' resolver rather than a
    second copy of it: the reachable-paths gate names a config key carried in
    two Python files as a twin, and this gate's whole subject is one rule with
    one implementation. Passing the rev-parsed root keeps the sibling
    resolvers here (``require_corroboration``, ``nonblocking_categories``)
    reading the same checkout this one does."""
    from fno.pr import _reviews

    return _reviews._resolved_github_approval_flag(str(_repo_root(cwd)))


def rests_on_self_attestation_alone(
    cov: dict, github_approval_satisfies: bool = False
) -> bool:
    """Whether a covered coverage row's whole count is the author's own
    (self_attested) local attestation - the same predicate the Rust gate's
    ``CoverageReport::rests_on_self_attestation_alone`` applies, read from the
    serialized row. Prefers the recorded counts; derives from verdicts on
    pre-field rows. Unmeasured authorship (no self_attested_count, no origins)
    is NOT self-attestation alone: it is not proof of corroboration, but it is
    not proof of its absence either.

    ``github_approval_satisfies`` is the resolved config flag, and it reaches
    the verdicts fallback through ``_reviews._human_approval_counts`` - the
    same helper ``_derive_review_state`` and the Rust ``human_approval_counts``
    apply. It was hardcoded to the flag-off branch here, which made this a
    THIRD implementation of one counting rule: under the flag a non-author
    GitHub approval corroborated on the other two paths and not on this one,
    while the gate's own refusal advertises that approval as a remedy. The
    default is False so a caller with no repo to resolve against (the
    doctor's read-only display) keeps today's answer.
    """
    reviewed = cov.get("reviewed_count")
    self_attested = cov.get("self_attested_count")
    if not isinstance(reviewed, int) or reviewed <= 0:
        # The corroboration policy itself rewrites the rows it holds to
        # covered=uncovered / reviewed_count 0 while PRESERVING the
        # self-attestation counts, so a 0-count row with a recorded
        # self-attestation is exactly the held row - the one population this
        # predicate exists to name. A 0-count row with no self-attestation is
        # merely uncovered, not self-attested-only.
        return isinstance(self_attested, int) and self_attested > 0
    if isinstance(self_attested, int):
        return self_attested == reviewed
    from fno.pr._reviews import _human_approval_counts

    counted = [
        v
        for v in (cov.get("verdicts") or [])
        if isinstance(v, dict)
        and v.get("verdict") == "reviewed"
        and _human_approval_counts(v, github_approval_satisfies)
    ]
    return bool(counted) and all(
        v.get("producer") == "local_attestation"
        and v.get("attestation_origin") == "self_attested"
        for v in counted
    )


def _corroboration_refusal(cov: Optional[dict], cwd: str) -> Optional[str]:
    """The refusal when ``config.review.require_corroboration`` is on and the
    coverage row rests on the author's own attestation alone. None when the
    policy is off, the row carries corroboration, or authorship is unmeasured
    (an unmeasured row is not proof of corroboration, but it is not proof of
    its absence either - fail open, as the Rust-side predicate does).
    """
    if not cov:
        return None
    try:

        from fno.config import load_settings_for_repo

        root = _repo_root(cwd)
        review = load_settings_for_repo(root).review
        if not getattr(review, "require_corroboration", False):
            return None
    except Exception:  # noqa: BLE001 - an unreadable config never tightens a gate
        return None

    if not rests_on_self_attestation_alone(
        cov, _github_approval_satisfies(cwd)
    ):
        return None
    return (
        "coverage rests on the author's own attestation alone "
        "(config.review.require_corroboration = true); corroboration satisfies "
        "it two ways: a second session's head-pinned attestation, or a GitHub "
        "App review"
    )


def _ordinary_verdict(
    pr_number: int, cwd: str, *, recompute: bool
) -> Tuple[int, str, str, str, Optional[tuple]]:
    """The evidence-only verdict: ``(state, refusal, covered_head, note,
    waiver_inputs)``. Everything but the operator-law overlay lives here; the
    public ``coverage_verdict`` applies that overlay to a refused or
    impossible head. ``waiver_inputs`` is ``(head, hard_findings)`` on every
    path the overlay may consult, None on the covered/unanswered early
    returns that never need it.
    """
    # The second argument is a CWD, and it is threaded into every probe below.
    # Hand it a repo slug and the FIRST probe to notice is the head fetch,
    # which drops its reason and answers None, so the verb reports "pr head
    # fetch failed" - true of the probe, silent about the argument that broke
    # it, and exactly the wrong-subject sentence this gate learned to stop
    # printing (x-51f7). Refuse at the door, before any probe has a chance to
    # misattribute it. UNANSWERED, not REFUSED: a gate that cannot read its
    # own inputs has not judged the PR.
    #
    # _rest._cwd_refusal, not a second copy of the isdir test: the first pass
    # here wrote its own, guarded `if cwd` where the original guarded `is not
    # None`, and an empty string slipped through to every probe as a
    # subprocess cwd - which raises rather than meaning "here". Two copies of
    # a guard are two chances to disagree.
    from fno.pr._rest import _cwd_refusal

    bad_cwd = _cwd_refusal(cwd)
    if bad_cwd:
        return UNANSWERED, "", "", bad_cwd, None

    # The guard's own short circuit: a stock install with no review lane
    # configured opts out of coverage entirely. Checked FIRST so neither the
    # head fetch nor the events read runs for a PR nobody configured review
    # for - same order run_merge has always evaluated, one lane probe cheaper.
    if not _merge._review_lane_configured(cwd, pr_number):
        return COVERED, "", "", NO_LANE_NOTE, None

    # The head fetch is an instrument, and it can fail. A None head is not
    # "no coverage" - it is "the probe that pins coverage to what would
    # actually merge could not run", and every answer built on it would
    # describe an unknown commit. Refuse to answer rather than guess.
    head: Optional[str] = _merge._pr_head_oid(pr_number, cwd)
    if head is None:
        return UNANSWERED, "", "", "pr head fetch failed", None

    # The valve, checked before the events read so an overridden PR skips the
    # recompute entirely, and before the attestation branch so the refusal it
    # would have built never runs. It returns the live head as the pin, so
    # `--match-head-commit` still refuses a push that races the merge: an
    # override waives the review, never the TOCTOU. A label held but REFUSED
    # (author-applied, or an unreadable actor) keeps its refusal for the
    # uncovered answer below, so the operator sees why the valve stayed shut.
    override_valid, override, override_refusal = _override_valve(pr_number, cwd)
    if override_valid:
        return COVERED, "", head, override, None

    code_review_required = _merge._code_review_attestation_required(cwd, pr_number)
    if recompute:
        # run_merge's exact path: the gate read fires the standalone producer
        # once when no usable row describes this head, and every failure inside
        # it degrades to the original row plus a note - a swallow there is the
        # answer of record, not a crash.
        cov, recompute_note = _merge._review_coverage_for_pr(pr_number, cwd, head)
    else:
        try:
            from fno.pr._reviews import review_coverage_for_head_row

            # One scan yields the row and its pin. The pin describes the stored
            # row, so it rides BOTH surfaces or the two refuse with different
            # sentences for one row.
            cov, recompute_note = review_coverage_for_head_row(pr_number, cwd, head)
        except Exception as exc:  # noqa: BLE001 - instrument failure, not absence
            return UNANSWERED, "", "", f"events read raised: {exc}", None

    covered, failed = covered_conjuncts(cov, head, code_review_required)
    corroboration = _corroboration_refusal(cov, cwd)

    # Locked Decision 1: the pass condition is disposition-complete at the
    # head, not clean. The chain read needs the PR's head branch to scope
    # its older rounds; on a covered row a probe miss is an instrument
    # failure, answered UNANSWERED like the head fetch above rather than
    # silently narrowing the chain (an under-collected chain is a fail-open).
    # On an uncovered row the miss keeps today's refusal: the chain there
    # only ever WIDENS the answer to IMPOSSIBLE, and a guessed branch would
    # fire it on the wrong scope.
    refs = _merge._pr_base_head_refs(pr_number, cwd)
    if covered and refs is None:
        return UNANSWERED, "", "", "pr head branch fetch failed", None
    chain = attestation_chain(
        cwd, head_branch=refs[1] if refs else "", head=head
    )
    disposition_text, disposition_note, disposition_named, disposition_hard = (
        disposition_refusal(chain, cov, cwd)
    )
    max_rounds = resolved_max_rounds(cwd)
    # The budget counts BOTH evidence axes: a GitHub-App reviewer's rounds
    # leave no attestation row, so the chain alone reads zero on exactly the
    # lane that spins. The reviews read is paid only where it can change the
    # answer (uncovered, or findings to file/decline); a healthy covered PR
    # with no open findings keeps the events-only count and skips the
    # paginated read. A read failure keeps the events-only answer.
    if disposition_named or disposition_text or not covered:
        reviews_payload, reviews_unread = _pr_reviews(pr_number, cwd)
    else:
        reviews_payload, reviews_unread = None, ""
    rounds = rounds_since_last_pass(chain, reviews=reviews_payload)
    # A failed reviews read still says so. The budget keeps its answer either
    # way (a cap that fired on a broken read would waive a remainder it may not
    # have spent), but a zero an instrument never contributed to must not read
    # as one it measured.
    unread_note = f"reviews read unavailable: {reviews_unread}" if reviews_unread else ""

    # Locked Decision 4's fourth state, before any covered/uncovered branch:
    # the all-fails loop shape never produces a covered row - that is exactly
    # why it spun - so the budget check must not live inside the covered arm.
    # Fires only when a HARD finding (CONFIRMED correctness or security) is
    # non-terminal AND the rounds are spent; either alone keeps its ordinary
    # verdict.
    filed_note = ""
    cap_filed = False
    if disposition_named and rounds >= max_rounds:
        if disposition_hard:
            return (
                IMPOSSIBLE,
                _impossible_refusal(rounds, max_rounds, ", ".join(disposition_hard)),
                "",
                "",
                (head, disposition_hard),
            )
        # The operator's ruling on the cap: the PR merges with its remaining
        # findings FILED as nodes, never dropped. The class gate is what makes
        # this safe - nothing here is a confirmed correctness or security
        # defect. A finding the gate cannot file is one it must not wave
        # through, so a filing failure refuses.
        try:
            filed = file_findings_at_cap(disposition_named, pr_number, cwd)
        except Exception as exc:  # noqa: BLE001 - never drop a finding silently
            return (
                REFUSED,
                f"round cap reached ({rounds}/{max_rounds}) but filing the "
                f"remaining finding(s) failed, so nothing was waived: {exc}",
                "",
                recompute_note,
                (head, disposition_hard),
            )
        filed_note = (
            f"{len(filed)} finding(s) filed at the round cap ({rounds}/{max_rounds}): "
            + ", ".join(f"{k} -> {n}" for k, n in zip(disposition_named, filed))
        )
        disposition_text = ""
        cap_filed = True

    if covered and corroboration:
        return REFUSED, corroboration, "", "; ".join(
            x for x in (recompute_note, unread_note, filed_note) if x
        ), (head, disposition_hard)
    if covered:
        if disposition_text:
            # Rounds remain (the exhausted case returned above), so the
            # refusal teaches the fix-delta remedy AND shows the budget the
            # next round spends - AC7-HP's "how many rounds remain".
            remaining = _rounds_remaining_note(rounds, max_rounds)
            note = "; ".join(x for x in (recompute_note, unread_note, remaining) if x)
            return REFUSED, disposition_text, "", note, (head, disposition_hard)
        notes = [n for n in (recompute_note, unread_note, disposition_note, filed_note) if n]
        return COVERED, "", (cov.get("head_sha") or "") if cov else "", "; ".join(notes), None
    # x-aecc: a fail attestation answers this head, so an uncovered row in
    # that shape is uncovered BECAUSE of the non-terminal findings. Name them
    # (the disposition sentence carries each finding key) instead of falling
    # through to the generic "0 reviewed" text - that text taught the loop to
    # re-review, which is the loop this branch exists to end. Two limits
    # (review finding 3): it never fires after the cap arm FILED the findings
    # (that path must keep its own refusal and note, not return an emptied
    # sentence), and it leaves the specialized conjuncts - no_local_pass,
    # reviewer_refused, stale_head - to their own sized remedies below.
    if (
        failed == "uncovered"
        and not cap_filed
        and disposition_named
        and any(e.get("verdict") != "pass" for e in chain)
    ):
        remaining = _rounds_remaining_note(rounds, max_rounds)
        note = "; ".join(x for x in (recompute_note, unread_note, remaining) if x)
        return REFUSED, disposition_text, "", note, (head, disposition_hard)

    # The spent budget DISCHARGES the review obligation. It does not fail it.
    #
    # This arm used to refuse, and that was the inversion at the heart of the
    # runaway-review problem. `config.review.max_rounds = 2` has to mean "this
    # PR gets two rounds, and then review is DONE" - a budget you spend. It
    # read as "after two rounds you are permanently unmergeable" instead,
    # which is a guard nothing can pass: every remedy that could clear it
    # names a review verb, and running one spends a round that is already
    # spent. Measured across 25 recent merged PRs, seven blew past the cap and
    # four reached double digits (12, 11, 10, 8 rounds) precisely because the
    # cap never ended a review phase - it only refused afterward.
    #
    # The operator's ruling is already written twenty lines above: "the PR
    # merges with its remaining findings FILED as nodes, never dropped". That
    # ruling was only ever reachable through the `covered` branch, so it fired
    # for a PR that had findings and never for a PR that had none. Having
    # findings made a PR MORE mergeable than having none. This is where the
    # ruling actually lands.
    #
    # What still blocks: a CONFIRMED correctness or security finding returns
    # IMPOSSIBLE above and never reaches here, and a filing failure refuses
    # there too. Those are the real safety, not this arm. Requiring an
    # attestation past the cap buys no trust the budget does not: in this
    # fleet the attestation is emitted by the same worker that wrote the code,
    # so both axes are self-certified. External review and human approval are
    # the trust boundary, and neither is weakened here.
    #
    # The waiver is NAMED in the note, never silent, so a merge that happened
    # on a spent budget is legible afterward.
    # `>=`, not `>`. max_rounds is a MAXIMUM: 2 means two rounds, and the
    # second one is the last. The old `>` let a third round run before the
    # budget tripped, so `max_rounds = 2` silently meant three reviews -
    # a reading nobody would arrive at from the key's own name.
    if rounds >= max_rounds:
        # The waiver names what it waived. Past the cap this arm preempts the
        # sized coverage refusals below - the required-reviewer attestation
        # conjunct included - and that is deliberate: a conjunct that still
        # refuses past the cap is unsatisfiable by construction, because the
        # only way to satisfy it is a round the budget will not fund. Losing
        # the FACT would be wrong though, so it rides the receipt as a note
        # instead of a block.
        # `failed` is covered_conjuncts' own name for the conjunct that
        # broke: uncovered / no_local_pass / stale_head / reviewer_refused.
        # A configured code-review requirement is named ALONGSIDE it, never
        # instead: an uncovered row hides that fact behind the generic name,
        # and "we merged without the reviewer config demands" is exactly the
        # fact a later reader needs.
        unsatisfied = "; ".join(
            x
            for x in (
                failed,
                "code-review attestation required by config" if code_review_required else "",
            )
            if x
        )
        waiver = (
            f"review budget discharged ({rounds}/{max_rounds} rounds): the "
            "review phase is complete and the remainder is filed; the "
            "operator lever is config.review.max_rounds"
            + (f" (waived at the cap: {unsatisfied})" if unsatisfied else "")
        )
        return (
            COVERED,
            "",
            head or "",
            "; ".join(n for n in (recompute_note, unread_note, filed_note, waiver) if n),
            None,
        )
    if failed == "uncovered" and corroboration:
        # The policy-rewritten shape (0 counted, self-attestation preserved)
        # fails the count conjunct, but the truer refusal names the policy and
        # both remedies - "re-run your own review" can never satisfy it, and
        # a worker told that will retry into budget. The other failures
        # (stale head, missing local pass, reviewer refusal) keep their own
        # refusals: those name a remedy that can actually work, and the
        # corroboration question may dissolve once the head is re-attested.
        # recompute_note plus the filed node ids when the cap arm fired on the
        # way here (same receipt contract as the returns above and below).
        return REFUSED, corroboration, "", "; ".join(
            x for x in (recompute_note, unread_note, filed_note) if x
        ), (head, disposition_hard)

    # Same branch order run_merge has always used: the attestation refusal is
    # checked first, so a config requiring code-review with no row names the
    # missing attestation, not the missing row.
    #
    # Both refusals carry the sized invocation (rendered once, fail-open to
    # None): a worker told to "run the review verb" with no flag shape runs it
    # bare, the bare call still attests, and the gate cannot tell the
    # difference - the exact delivery gap the render closes.
    hint = None
    try:

        from fno.review_capability import render_self_review_invocation

        root = _repo_root(cwd)
        rendered = render_self_review_invocation(project_root=root)
        # An unsized render (no merge-base against main/master) keeps its
        # `<level>` placeholder - teachable on the orienter surface, but a
        # non-runnable string in a copy-me slot here. No hint, levelless line.
        hint = None if "<level>" in rendered else rendered
    except Exception:  # noqa: BLE001 - advisory text; the refusal still stands
        hint = None
    if code_review_required and not _merge._coverage_has_local_pass(cov, "code-review"):
        sized = f" - `{hint}`" if hint else ""
        refusal = (
            "required code-review has no head-pinned local pass attestation; "
            f"run the harness review verb at HEAD{sized}, "
            "then emit the code-review attestation"
        )
    else:
        refusal = _merge._coverage_refused_reason(
            cov,
            head,
            _merge._coverage_sources(cwd) if cov is None else None,
            self_review_hint=hint,
        )
    # Appended here, where `cov` is in scope, rather than in `coverage_verdict`:
    # one site, and a granted waiver discards this refusal anyway.
    refusal = _with_stale_waiver_guidance(refusal, cov)
    if override_refusal:
        # The label is present but stayed shut; naming that first beats a
        # generic uncovered refusal the operator cannot act on.
        refusal = f"{override_refusal}; {refusal}"
    # The cap arm may already have FILED findings on the way here (the row
    # stayed uncovered on another conjunct): that side effect must ride the
    # receipt, never vanish behind the refusal it did not soften.
    return REFUSED, refusal, "", "; ".join(
        x for x in (recompute_note, unread_note, filed_note) if x
    ), (head, disposition_hard)


def _with_stale_waiver_guidance(refusal: str, cov) -> str:
    """Name the operator waiver on a refusal a STALE attestation caused.

    Rounds ACCUMULATE: `_reviews._tiling_chain` counts a verdict at an older
    sha when that sha is in the chain, so a fix landing on top of a review does
    not throw the review away. Measured over 387 tiling events: 272 covered,
    and of the 115 uncovered, 95 carried an EMPTY chain - nothing had reviewed
    at any sha. So the common uncovered row is a review that never emitted
    (the empty-diff and lost-attestation defects), not a review the head
    outran, and this text is deliberately not aimed at that majority.

    It answers the remaining minority, where something did review and the row
    still refuses. Operator ruling d-4d05272e waives the gate there, and
    nothing in the refusal said so, so the waiver was reachable only by
    querying the decision store - while the stop hook offered two remedies
    standing rules forbid (a third round past the cap, and a recovery
    attestation for a round that never passed).

    Gated on a stale verdict EXISTING, and that gate carries most of the
    safety: an empty chain is the 83% case, so a version of this text that
    also fired on a never-reviewed PR would hand a merge hint to exactly the
    rows where the gate is doing its job. Advisory only: this returns text,
    never authority, and `coverage-waive` still refuses a harness-identified
    session.
    """
    if not isinstance(cov, dict) or not cov.get("stale_verdicts"):
        return refusal
    return (
        f"{refusal}. A prior attestation exists but its head moved, which is "
        "what operator ruling d-4d05272e waives: with CI green and no "
        "unresolved P1, this may merge without a head-pinned attestation. "
        "P1 does not waive and still blocks. The lever is attended-operator "
        "only: `fno do pr coverage-waive <pr> --reason ...`"
    )


def coverage_verdict(
    pr_number: int, cwd: str, *, recompute: bool
) -> Tuple[int, str, str, str]:
    """Return ``(state, refusal, covered_head, note)``: the ordinary verdict
    with the operator-law overlay applied to a refused or impossible head.

    ``refusal`` is the guard's own sentence (the one ``_coverage_refused_reason``
    builds) and is empty unless ``state`` is REFUSED. ``covered_head`` is the
    head the row pins, for the caller's TOCTOU pin; empty when no lane is
    configured or no row survives. ``note`` names a recompute outcome on
    REFUSED, or the dead probe on UNANSWERED; on a waiver it carries
    OVERRIDE_NOTE_PREFIX, so a merge that landed on operator law and one that
    was reviewed cannot read the same.

    The overlay is consulted ONLY where ordinary coverage refused or reported
    impossible: an independently covered PR never pays the lookup and never
    prints a waiver receipt. A waiver lookup that answers UNKNOWN authority
    (conflict, damaged rows, a dead probe) is UNANSWERED with the probe named
    - never a refusal built on a store that could not answer, and never
    permission either.
    """
    state, refusal, covered_head, note, waiver_inputs = _ordinary_verdict(
        pr_number, cwd, recompute=recompute
    )
    if state not in (REFUSED, IMPOSSIBLE) or waiver_inputs is None:
        return state, refusal, covered_head, note
    head, hard = waiver_inputs
    waived, waiver_note, probe_note = operator_waiver_verdict(
        _repo_slug(cwd), pr_number, head, hard
    )
    if waived:
        # The live head is the pin: a waiver covers exactly the head its
        # subject names, so the merge still refuses a push that races it.
        return COVERED, "", head, waiver_note
    if probe_note:
        return UNANSWERED, "", "", probe_note
    return state, refusal, covered_head, note


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


# --- The disposition-complete pass condition ---------------------------------
#
# Locked Decision 1: the pass condition is disposition-complete at the head,
# not clean. A head is covered when the attestation chain tiles base..head AND
# every finding in that chain is terminal. The chain and its dispositions are
# read from the events log directly (the coverage row carries the tiling, the
# attestations carry the findings), so this predicate composes with the Rust
# producer rather than duplicating it.

def _gate_finding_blocks(primitive: Any, allow: frozenset[str]) -> bool:
    """The gate-side re-derivation of one finding primitive's blockingness.

    The producer's own ``blocking`` flag is never read. An unreadable
    primitive, a missing-fields primitive, a CONFIRMED verdict, or an
    unrecognized category all block - the same fail-closed order the
    producer-side classifier applies, restated here so the two are held equal
    by tests rather than by trust.
    """
    if not isinstance(primitive, dict):
        return True
    if primitive.get("has_required_fields") is not True:
        return True
    verdict = primitive.get("verdict")
    if isinstance(verdict, str) and verdict.strip().lower() == "confirmed":
        return True
    category = primitive.get("category")
    if (
        isinstance(category, str)
        and category.strip().lower() in allow
    ):
        return False
    return True


def attestation_chain(
    cwd: Optional[str] = None, head_branch: str = "", head: str = ""
) -> list[dict]:
    """The branch-scoped ``review_attestation`` events, oldest first.

    Reads both logs a coverage read consults (project unscoped, global scoped
    by repo identity), with the same scoping rule the Rust pass scan applies:
    an event is in scope when its branch names the PR's head branch, or when
    it pins the PR's exact head sha. A malformed line is skipped (an
    append-only log written by several processes), never fatal.
    """
    try:
        from fno.pr._reviews import _coverage_logs

        project_path, global_path, slug = _coverage_logs(cwd, None)
    except Exception:  # noqa: BLE001 - an unreadable log never tightens a gate
        return []

    def _in_scope(data: dict) -> bool:
        if head and data.get("head_sha") == head:
            return True
        branch = data.get("branch")
        return bool(head_branch) and isinstance(branch, str) and branch == head_branch

    chain: list[dict] = []
    seen: set[str] = set()
    for path, scoped in ((project_path, False), (global_path, True)):
        if path is None:
            continue
        try:
            with open(path, "r", encoding="utf-8", errors="replace") as fh:
                for raw in fh:
                    if '"review_attestation"' not in raw:
                        continue
                    try:
                        event = json.loads(raw)
                    except ValueError:
                        continue
                    data = event.get("data")
                    if not isinstance(data, dict):
                        continue
                    if scoped:
                        if data.get("repo") != slug:
                            continue
                    # Dedup on the producer's invocation_id, falling back to
                    # the whole payload only when the row predates the field.
                    # The payload key is wrong across stores: the global
                    # mirror stamps `repo` onto its copy and the project row
                    # carries none, so one attestation produced two payload
                    # keys and the chain counted every mirrored round twice -
                    # which the no-refund budget turns into "one review reads
                    # 2/2". The invocation_id is minted once by the producer
                    # and lands identically on both rows.
                    invocation_id = data.get("invocation_id")
                    if isinstance(invocation_id, str) and invocation_id:
                        key = f"invocation:{invocation_id}"
                    else:
                        key = json.dumps(data, sort_keys=True)
                    if key in seen:
                        continue
                    if _in_scope(data):
                        seen.add(key)
                        chain.append(
                            {
                                "ts": event.get("ts", ""),
                                "head_sha": data.get("head_sha", ""),
                                "reviewed_base_sha": data.get("reviewed_base_sha", ""),
                                "verdict": data.get("verdict", ""),
                                "review_round": data.get("review_round"),
                                "findings": data.get("findings"),
                                "findings_truncated": data.get("findings_truncated") is True,
                                "dispositions": data.get("dispositions"),
                            }
                        )
        except OSError:
            continue
    chain.sort(key=lambda e: e["ts"])
    return chain


def _resolved_categories(cwd: str) -> frozenset[str]:
    """The configured allowlist, extended per the shipped default."""
    try:
        from fno.config import load_settings_for_repo
        from fno.review.findings import resolve_nonblocking_categories

        root = _repo_root(cwd)
        return resolve_nonblocking_categories(
            getattr(
                load_settings_for_repo(root).review, "nonblocking_categories", None
            )
        )
    except Exception:  # noqa: BLE001 - unreadable config keeps the shipped default
        return GATE_NONBLOCKING_CATEGORIES


#: The two categories the round cap can never file away. The class gate is
#: what makes file-the-remainder safe: noise can be filed, a CONFIRMED
#: correctness or security defect cannot. Mirrors ``HARD_CATEGORIES`` in the
#: Rust gate.
HARD_CATEGORIES = frozenset({"correctness", "security"})


def _hard_finding(primitive: Any) -> bool:
    """A CONFIRMED correctness or security finding: the one shape the round
    cap keeps IMPOSSIBLE for. Read from the same primitive fields the gate
    re-derives blockingness from, never the producer's count."""
    if not isinstance(primitive, dict):
        return True
    verdict = primitive.get("verdict")
    if not (isinstance(verdict, str) and verdict.strip().lower() == "confirmed"):
        return False
    category = primitive.get("category")
    return isinstance(category, str) and category.strip().lower() in HARD_CATEGORIES


def disposition_refusal(
    chain: list[dict], cov: Optional[dict], cwd: str = "."
) -> Tuple[str, str, list, list]:
    """The refusal when a blocking finding in the chain is non-terminal.

    Returns ``(refusal, note, named, hard)``: the refusal sentence (empty
    when everything terminal), the by-class note for a covered answer, the
    sorted finding keys that are non-terminal or uncorroborated, and the
    subset of those that are HARD (a CONFIRMED correctness or security
    finding, or the truncated remainder). At the round cap the hard subset
    is what keeps IMPOSSIBLE; the rest are filed as nodes and the PR merges.
    Neither list carries the fix-delta remedy the REFUSED sentence teaches -
    that remedy is exactly what an exhausted loop must stop being told.

    A finding is terminal when it is fixed (and the chain moved past the round
    that raised it), non-blocking by the gate's own re-derivation, declined
    WITH corroboration the author cannot mint alone, or waived by the
    override label (which answers COVERED before this runs). A declined
    blocking finding on the author's own signature alone is NOT terminal:
    that is the whole difference between this gate and the exploit.
    """
    if not chain:
        return "", "", [], []
    allow = _resolved_categories(cwd)
    # Latest disposition per finding_key across the chain, plus the round
    # each blocking finding was raised in (a fixed finding is terminal only
    # when a LATER round reviewed the fix delta).
    dispositions: dict[str, dict] = {}
    raised_in: dict[str, int] = {}
    findings_by_key: dict[str, dict] = {}
    truncated = False
    last_round = len(chain) - 1
    for index, event in enumerate(chain):
        if event["findings_truncated"]:
            truncated = True
        for primitive in event["findings"] or []:
            if isinstance(primitive, dict) and primitive.get("finding_key"):
                key = primitive["finding_key"]
                findings_by_key[key] = primitive
                raised_in[key] = index
        for entry in event["dispositions"] or []:
            if isinstance(entry, dict) and entry.get("finding_key"):
                dispositions[entry["finding_key"]] = entry

    if truncated:
        return (
            "attestation chain findings were truncated (findings_truncated); the "
            "truncated remainder is non-terminal, so the gate refuses rather "
            "than trust a count it cannot re-derive",
            "",
            ["(truncated remainder)"],
            ["(truncated remainder)"],
        )

    # Corroboration for declines: the coverage row's existing predicate, read
    # independent of config.review.require_corroboration (Locked Decision 2 -
    # a disposition pass can be gamed by declining, a clean review cannot).
    corroborated = not (
        cov is not None
        and rests_on_self_attestation_alone(
            cov, _github_approval_satisfies(cwd)
        )
    )

    nonterminal: list[str] = []
    uncorroborated: list[str] = []
    for key, primitive in findings_by_key.items():
        if not _gate_finding_blocks(primitive, allow):
            continue  # non-blocking by class: no action needed to clear the gate
        disposition = dispositions.get(key)
        if disposition is None:
            nonterminal.append(key)
        elif disposition.get("disposition") == "fixed":
            # Terminal when the chain moved past the round that raised it:
            # a later attestation reviewed the fix delta. Exact range
            # coverage is the tiling conjunct the covered row carries.
            if raised_in.get(key, last_round) >= last_round:
                nonterminal.append(key)
        elif disposition.get("disposition") == "declined":
            reason = disposition.get("reason")
            if not isinstance(reason, str) or not reason.strip():
                nonterminal.append(key)
            elif not corroborated:
                uncorroborated.append(key)
        # disposition nonblocking on a gate-blocking finding: the producer
        # claimed harmless where the gate re-derives blocking. The gate wins
        # (Locked Decision 6); it stays non-terminal.
        else:
            nonterminal.append(key)

    hard = sorted(
        key
        for key in (*nonterminal, *uncorroborated)
        if _hard_finding(findings_by_key.get(key))
    )
    if nonterminal:
        keys = sorted(nonterminal)
        return (
            f"blocking finding(s) not terminal: {', '.join(keys)}; a blocking "
            "finding is cleared by fixing it and letting the next review cover "
            "the fix delta, nothing else clears it on your own signature",
            "",
            keys,
            hard,
        )
    if uncorroborated:
        keys = sorted(uncorroborated)
        return (
            f"declined blocking finding(s) {', '.join(keys)} rest on the author's "
            "own signature alone; corroboration satisfies it two ways: a second "
            "session's head-pinned attestation, or a non-author GitHub approval",
            "",
            keys,
            hard,
        )
    by_class = [
        key
        for key, primitive in findings_by_key.items()
        if not _gate_finding_blocks(primitive, allow)
    ]
    note = (
        f"{len(by_class)} non-blocking finding(s) treated by class"
        if by_class
        else ""
    )
    return "", note, [], []


# --- The round budget and the fourth verdict --------------------------------
#
# Locked Decision 4's termination clause: a review loop that declines its
# blocking findings must terminate in a state whose remedy is NOT another
# round. The budget is config.review.max_rounds (default 2, at least 1); the
# round count is re-derived here from the same chain the disposition gate
# reads, never trusted off the producer's row (Locked Decision 6).

#: The shipped default when config is unreadable or the key absent. Matches
#: the Rust parse's own ``unwrap_or(2).max(1)`` so the two gates cannot
#: disagree on the same unreadable config.
DEFAULT_MAX_ROUNDS = 2

#: The AC7-MARKER literals: the refusal must carry the word ``impossible``
#: and name every truthful exit. Never "run the review verb at HEAD" - that is
#: the instruction that caused the loop this state exists to end. Three exits,
#: because the first two need a second GitHub account and the third is the one
#: a single-account operator can actually run.
IMPOSSIBLE_REMEDIES = (
    "a non-author GitHub approval on the PR, a non-author coverage-override "
    'label, or the attended `fno do pr coverage-waive <pr> --reason "..."` '
    "command (the operator exit)"
)


def rounds_since_last_pass(
    chain: list[dict],
    reviews: Optional[list[dict]] = None,
) -> int:
    """The PR's review-round total, oldest-first.

    The operator's ruling (x-2219, 2026-08-27) made this a PER-PR TOTAL:
    ``max_rounds`` counts rounds across the whole life of the PR, and a
    ``verdict: pass`` refunds nothing - it is one round like any verdict,
    and its coverage role lives in the coverage classify, never here. The
    name survives from the reset semantics it used to implement; this
    docstring, not the name, is the contract. The chain axis counts
    attestation verdicts (the declared ``review_round`` wins when present,
    as the running max; events from before the field existed fall back to
    counting verdicts). The reviews axis, when a payload is supplied,
    counts DISTINCT reviewed commits - a GitHub-App reviewer's rounds leave
    no attestation row anywhere, so they exist only as review objects, and
    every fix moves the head, making one reviewed commit one round. No
    timestamp filter on this axis either: a pass that truncated the reviews
    older than itself would refund rounds only GitHub saw. No author
    filter: the codex cloud connector posts its review objects under the PR
    author's own login (measured live - 116 of 117 objects on the spinning
    specimen), so an author exclusion deletes the round trace on exactly
    that lane. Known bound, accepted: reply volume at ONE commit is
    neutral, but replies landed on distinct never-reviewed heads each count
    as a round. No discriminator exists in the review-object data, and
    over-counting fires the cap on a worker already push-replying without
    re-review, where the old under-count spun forever. The answer is the
    MAX of the two axes, never the sum: a healthy lane leaves both traces
    per round. The Rust-side mirror is ``loopcheck::rounds_since_last_pass``;
    the two are held equal by the shared corpus.
    """
    rounds = 0
    counted_heads: set[str] = set()
    for event in chain:
        declared = event.get("review_round")
        if isinstance(declared, int) and not isinstance(declared, bool) and declared >= 0:
            # A declared round number is already the round's identity, so the
            # running max cannot double-count and needs no head collapse.
            rounds = max(rounds, declared)
            continue
        # One reviewed head is ONE round, the same unit the reviews axis uses
        # (it counts DISTINCT commit.oid). Without this the two axes measure
        # different things and max() over them is not a budget: a producer
        # that emits a corrective second verdict at an unchanged head spends
        # two rounds for zero code change. A row carrying no head is counted,
        # fail-closed: an unreadable head never buys a free round.
        event_head = event.get("head_sha")
        if isinstance(event_head, str) and event_head:
            if event_head in counted_heads:
                continue
            counted_heads.add(event_head)
        rounds += 1
    events_rounds = rounds
    if reviews is None:
        return events_rounds
    counted: set[str] = set()
    for review in reviews:
        state = review.get("state")
        if not isinstance(state, str) or not state:
            continue
        commit = review.get("commit")
        oid = commit.get("oid") if isinstance(commit, dict) else None
        if not isinstance(oid, str) or not oid:
            continue
        counted.add(oid)
    return max(events_rounds, len(counted))


def _pr_reviews(pr_number: int, cwd: str) -> Tuple[Optional[list[dict]], str]:
    """``(reviews, unread_reason)`` for the round budget.

    ``unread_reason`` is empty when the read succeeded and names the cause
    when it did not. It used to be swallowed: every failure answered a bare
    None, the budget silently kept its events-only count, and on a
    GitHub-App lane - which leaves no attestation row at all - that count is
    zero. So a failed read and a genuinely unreviewed PR were byte-identical,
    the cap never fired, and one PR ran twelve rounds against a budget of two.

    The paginated REST read rides ``_internal_gh._rest_pages`` (the same
    reader every other gate REST read uses, with its rate-limit-aware
    failure classification) and the field mapping is the subset of
    ``_internal_gh._coverage_reviews`` the counter reads. Each page call is
    bounded at 30s like the Rust gate's stopgate timeout, so a STALLED gh
    costs one timeout and then answers None rather than hanging the merge
    verb. That is a per-call bound, not a whole-read one: ``_rest_pages``
    walks up to 100 pages, so a read that keeps succeeding slowly is bounded
    by the page cap, not by 30s. A read failure answers None: the round budget
    then keeps its events-only answer rather than guessing - a cap that
    fires on a broken read would decline review remainder the budget may
    not have spent.
    """
    from fno.pr._internal_gh import _rest_pages
    from fno.pr._proc import run
    from fno.pr._rest import _repo_slug_reason

    def _bounded(cmd, **kwargs):
        kwargs.setdefault("timeout", 30.0)
        return run(cmd, **kwargs)

    try:
        slug, slug_reason = _repo_slug_reason(cwd, _bounded)
        if not slug:
            # The reason names its own subject. This used to be the fixed
            # sentence "repo slug unreadable", which is wrong whenever the slug
            # is the readable thing and the CWD is what does not exist - the
            # exact case a caller hits by passing `owner/repo` here (x-51f7).
            return None, slug_reason or "repo slug unreadable"
        # The plain bounded runner, NOT _rest_runner. _rest_runner stamps
        # _quota.delegate_environment(), which strips the quota proxy from
        # PATH so the proxy's own delegate call does not recurse. That is
        # right inside _internal_gh and wrong from here: this gate is a proxy
        # CLIENT, and every sibling read in this module (_repo_slug just
        # above, _pr_head_oid, _pr_author_login) is brokered. Stamping it
        # here would spend the shared quota unmetered.
        rows, reason = _rest_pages(
            f"repos/{slug}/pulls/{pr_number}/reviews",
            "pull reviews",
            cwd=cwd,
            runner=_bounded,
        )
        if rows is None:
            return None, (reason or "pull reviews read failed")
        return [
            {
                "state": row.get("state") if isinstance(row.get("state"), str) else "",
                "submittedAt": (
                    row.get("submitted_at")
                    if isinstance(row.get("submitted_at"), str)
                    else ""
                ),
                "commit": {
                    "oid": row.get("commit_id")
                    if isinstance(row.get("commit_id"), str)
                    else ""
                },
            }
            for row in rows
        ], ""
    except Exception as exc:  # noqa: BLE001 - an instrument failure never fires the cap
        return None, f"{type(exc).__name__}: {exc}"


def resolved_max_rounds(cwd: str) -> int:
    """The clamped ``config.review.max_rounds`` for the repo (>= 1)."""
    try:
        from fno.config import load_settings_for_repo

        root = _repo_root(cwd)
        value = getattr(load_settings_for_repo(root).review, "max_rounds", None)
        if isinstance(value, int) and not isinstance(value, bool) and value >= 1:
            return value
        return DEFAULT_MAX_ROUNDS
    except Exception:  # noqa: BLE001 - unreadable config keeps the shipped default
        return DEFAULT_MAX_ROUNDS


def file_findings_at_cap(keys: list[str], pr_number: int, cwd: str) -> list[str]:
    """File each remaining finding as a backlog node at the round cap.

    The operator's ruling on the cap: the PR merges with its remaining
    findings FILED, never dropped. Idempotent on the finding key (a re-run of
    the gate, or the merge verb after a status read, must not mint twice):
    an existing node whose title carries the key is reused. Returns the node
    ids in key order. Raises on any failure - a finding the gate cannot file
    is a finding it must not wave through, so the caller refuses.
    """
    import re

    from fno.pr._proc import run

    ids: list[str] = []
    for key in keys:
        title = f"review finding filed at round cap: {key}"
        found = run(["fno", "backlog", "find", title], cwd=cwd)
        existing = None
        if found.ok:
            for line in found.stdout.splitlines():
                if key in line:
                    match = re.search(r"\b[a-z]+-[0-9a-f]{4,}\b", line)
                    if match:
                        existing = match.group(0)
                        break
        if existing:
            ids.append(existing)
            continue
        made = run(
            [
                "fno",
                "backlog",
                "idea",
                title,
                "--type",
                "bug",
                # `fno backlog idea` refuses a non-interactive filing with no
                # --difficulty, and this call has no tty. Without it every
                # filing raised, so the cap's merge exit - file the remainder,
                # then merge - was unreachable on every PR that reached the
                # cap. `low` is correct by construction: a CONFIRMED
                # correctness or security finding returns IMPOSSIBLE in
                # `coverage_verdict` before the filing runs, so nothing hard
                # ever reaches this line.
                "--difficulty",
                "low",
                # --separate is load-bearing BECAUSE of --difficulty. The
                # pre-mint fold gate keys on difficulty being set, and with a
                # fold candidate present and no tty it prints the offer and
                # exits 0 having minted NOTHING. Every finding filed at the cap
                # shares a title prefix, so the second one on a PR is always a
                # fold candidate for the first. Without this the caller reads
                # ok, scrapes an unrelated id out of the printed wave command,
                # reports the finding as filed, and merges over a finding that
                # was silently dropped.
                "--separate",
                "--details",
                (
                    f"Filed by the review-coverage gate at the round cap on PR "
                    f"{pr_number}. The finding was still non-terminal when the "
                    "review budget was spent; it was not CONFIRMED correctness or "
                    "security, so the PR merged and the finding lands here rather "
                    "than being dropped."
                ),
            ],
            cwd=cwd,
        )
        if not made.ok:
            raise RuntimeError(
                f"filing {key} failed: {(made.stderr or made.stdout).strip()}"
            )
        match = re.search(r"\b[a-z]+-[0-9a-f]{4,}\b", made.stdout)
        if not match:
            raise RuntimeError(f"filing {key} returned no node id: {made.stdout.strip()}")
        ids.append(match.group(0))
    return ids


def _impossible_refusal(
    rounds: int, max_rounds: int, disposition_refusal_text: str
) -> str:
    """The IMPOSSIBLE sentence: rounds spent, findings non-terminal, both
    remedies, and no instruction that asks for another review."""
    return (
        f"review coverage is impossible to satisfy by further review: {rounds} "
        f"review rounds used (max {max_rounds}) with blocking finding(s) still "
        f"non-terminal ({disposition_refusal_text}); this cannot be cleared by "
        f"re-reviewing - the acts that clear it are {IMPOSSIBLE_REMEDIES}"
    )


def _rounds_remaining_note(rounds: int, max_rounds: int) -> str:
    """The REFUSED-side note AC7-HP demands: the budget a worker can see
    before the next round reports impossible. Zero remaining is still worth
    saying - the next round is the one that trips, and a worker who cannot
    see the budget cannot choose to stop."""
    # One less than the raw difference: at rounds = max_rounds - 1 the NEXT
    # round is the last the budget funds, so zero remain after it.
    remaining = max_rounds - rounds - 1
    if remaining <= 0:
        return (
            f"{rounds}/{max_rounds} review rounds used; the next round "
            "is the last the budget funds"
        )
    plural = "" if remaining == 1 else "s"
    return (
        f"{rounds}/{max_rounds} review rounds used; {remaining} round{plural} "
        "remain before the gate reports impossible"
    )


def run_coverage_check(
    pr_number: int, recompute: bool = False, cwd: Optional[str] = None
) -> int:
    """The verb body: print the refusal, return the state as an exit code.

    Exit 0 covered, 3 refused (the guard's sentence on stderr), 4 unanswered
    (the note naming the dead probe), 5 impossible (rounds spent with
    blocking findings non-terminal; the sentence names the two remedies, on
    stderr like every refusal). Callers that cannot import ``fno`` - a
    stdlib-only hook - read the first stderr line and the exit code.
    """
    cwd = cwd or os.getcwd()
    state, refusal, _covered_head, note = coverage_verdict(
        pr_number, cwd, recompute=recompute
    )
    if state in (REFUSED, IMPOSSIBLE):
        sys.stderr.write(f"{refusal_line(refusal, note)}\n")
    elif state == UNANSWERED:
        sys.stderr.write(f"{note}\n")
    return state
