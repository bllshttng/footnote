"""The merge guard's review-coverage predicate, as one callable.

Lifted out of ``run_merge`` unchanged so a second caller can ask the same
question without a second copy of it. The predicate is MOVED, never restated:
every helper lives in ``fno.pr._merge`` and is reached through the module, so
the merge path is steered by patching ``_merge`` alone. The verb's no-recompute
read calls ``_reviews.latest_review_coverage`` directly, so a test that must
steer BOTH surfaces patches both modules (as this module's own tests do), and
the two readers that already disagree stay two, not three.

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

The one deliberate bypass is the ``coverage-override`` label: it answers
COVERED with a note carrying ``OVERRIDE_NOTE_PREFIX``, so a caller can always
tell a merge that was reviewed from a merge that was waived.
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


def _override_note(pr_number: int, repo: str) -> str:
    """The override note when the PR carries the label, else ``""``.

    ``_reviews`` owns the label reader (one reader, name-matched, never a grep
    over the raw label JSON). Every failure inside it degrades to "no label":
    an unreadable label state must not open the valve, because the recovery
    from a wrong refusal is one command and the recovery from a wrong merge is
    a revert.
    """
    try:
        from fno.pr import _reviews

        held, actor = _reviews._override_label_actor(pr_number, repo, _reviews.run)
        if not held:
            return ""
        return (
            f"{OVERRIDE_NOTE_PREFIX}{_reviews.COVERAGE_OVERRIDE_LABEL} "
            f"label applied by {actor or 'unknown actor'}"
        )
    except Exception:  # noqa: BLE001 - an unreadable label is not an override
        return ""


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


def rests_on_self_attestation_alone(cov: dict) -> bool:
    """Whether a covered coverage row's whole count is the author's own
    (self_attested) local attestation - the same predicate the Rust gate's
    ``CoverageReport::rests_on_self_attestation_alone`` applies, read from the
    serialized row. Prefers the recorded counts; derives from verdicts on
    pre-field rows. Unmeasured authorship (no self_attested_count, no origins)
    is NOT self-attestation alone: it is not proof of corroboration, but it is
    not proof of its absence either.
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
    counted = [
        v
        for v in (cov.get("verdicts") or [])
        if isinstance(v, dict)
        and v.get("verdict") == "reviewed"
        and not v.get("human_approval")
    ]
    return bool(counted) and all(
        v.get("producer") == "local_attestation"
        and v.get("attestation_origin") == "self_attested"
        for v in counted
    )


def _corroboration_refusal(cov: Optional[dict], repo: str) -> Optional[str]:
    """The refusal when ``config.review.require_corroboration`` is on and the
    coverage row rests on the author's own attestation alone. None when the
    policy is off, the row carries corroboration, or authorship is unmeasured
    (an unmeasured row is not proof of corroboration, but it is not proof of
    its absence either - fail open, as the Rust-side predicate does).
    """
    if not cov:
        return None
    try:
        from pathlib import Path

        from fno.config import load_settings_for_repo

        root = Path(_merge._repo_state_dir(repo)).parent
        review = load_settings_for_repo(root).review
        if not getattr(review, "require_corroboration", False):
            return None
    except Exception:  # noqa: BLE001 - an unreadable config never tightens a gate
        return None

    if not rests_on_self_attestation_alone(cov):
        return None
    return (
        "coverage rests on the author's own attestation alone "
        "(config.review.require_corroboration = true); corroboration satisfies "
        "it two ways: a second session's head-pinned attestation, or a GitHub "
        "App review"
    )


def coverage_verdict(
    pr_number: int, repo: str, *, recompute: bool
) -> Tuple[int, str, str, str]:
    """Return ``(state, refusal, covered_head, note)``.

    ``refusal`` is the guard's own sentence (the one ``_coverage_refused_reason``
    builds) and is empty unless ``state`` is REFUSED. ``covered_head`` is the
    head the row pins, for the caller's TOCTOU pin; empty when no lane is
    configured or no row survives. ``note`` names a recompute outcome on
    REFUSED, or the dead probe on UNANSWERED.
    """
    # The guard's own short circuit: a stock install with no review lane
    # configured opts out of coverage entirely. Checked FIRST so neither the
    # head fetch nor the events read runs for a PR nobody configured review
    # for - same order run_merge has always evaluated, one lane probe cheaper.
    if not _merge._review_lane_configured(repo, pr_number):
        return COVERED, "", "", NO_LANE_NOTE

    # The head fetch is an instrument, and it can fail. A None head is not
    # "no coverage" - it is "the probe that pins coverage to what would
    # actually merge could not run", and every answer built on it would
    # describe an unknown commit. Refuse to answer rather than guess.
    head: Optional[str] = _merge._pr_head_oid(pr_number, repo)
    if head is None:
        return UNANSWERED, "", "", "pr head fetch failed"

    # The valve, checked before the events read so an overridden PR skips the
    # recompute entirely, and before the attestation branch so the refusal it
    # would have built never runs. It returns the live head as the pin, so
    # `--match-head-commit` still refuses a push that races the merge: an
    # override waives the review, never the TOCTOU.
    override = _override_note(pr_number, repo)
    if override:
        return COVERED, "", head, override

    code_review_required = _merge._code_review_attestation_required(repo, pr_number)
    if recompute:
        # run_merge's exact path: the gate read fires the standalone producer
        # once when no usable row describes this head, and every failure inside
        # it degrades to the original row plus a note - a swallow there is the
        # answer of record, not a crash.
        cov, recompute_note = _merge._review_coverage_for_pr(pr_number, repo, head)
    else:
        try:
            from fno.pr._reviews import review_coverage_for_head

            cov = review_coverage_for_head(pr_number, repo, head)
        except Exception as exc:  # noqa: BLE001 - instrument failure, not absence
            return UNANSWERED, "", "", f"events read raised: {exc}"
        recompute_note = ""

    covered, failed = covered_conjuncts(cov, head, code_review_required)
    corroboration = _corroboration_refusal(cov, repo)

    # Locked Decision 1: the pass condition is disposition-complete at the
    # head, not clean. The chain read needs the PR's head branch to scope
    # its older rounds; on a covered row a probe miss is an instrument
    # failure, answered UNANSWERED like the head fetch above rather than
    # silently narrowing the chain (an under-collected chain is a fail-open).
    # On an uncovered row the miss keeps today's refusal: the chain there
    # only ever WIDENS the answer to IMPOSSIBLE, and a guessed branch would
    # fire it on the wrong scope.
    refs = _merge._pr_base_head_refs(pr_number, repo)
    if covered and refs is None:
        return UNANSWERED, "", "", "pr head branch fetch failed"
    chain = attestation_chain(
        repo, head_branch=refs[1] if refs else "", head=head
    )
    disposition_text, disposition_note, disposition_named, disposition_hard = (
        disposition_refusal(chain, cov, repo)
    )
    max_rounds = resolved_max_rounds(repo)
    rounds = rounds_since_last_pass(chain)

    # Locked Decision 4's fourth state, before any covered/uncovered branch:
    # the all-fails loop shape never produces a covered row - that is exactly
    # why it spun - so the budget check must not live inside the covered arm.
    # Fires only when a HARD finding (CONFIRMED correctness or security) is
    # non-terminal AND the rounds are spent; either alone keeps its ordinary
    # verdict.
    filed_note = ""
    if disposition_named and rounds > max_rounds:
        if disposition_hard:
            return (
                IMPOSSIBLE,
                _impossible_refusal(rounds, max_rounds, ", ".join(disposition_hard)),
                "",
                "",
            )
        # The operator's ruling on the cap: the PR merges with its remaining
        # findings FILED as nodes, never dropped. The class gate is what makes
        # this safe - nothing here is a confirmed correctness or security
        # defect. A finding the gate cannot file is one it must not wave
        # through, so a filing failure refuses.
        try:
            filed = file_findings_at_cap(disposition_named, pr_number, repo)
        except Exception as exc:  # noqa: BLE001 - never drop a finding silently
            return (
                REFUSED,
                f"round cap reached ({rounds}/{max_rounds}) but filing the "
                f"remaining finding(s) failed, so nothing was waived: {exc}",
                "",
                recompute_note,
            )
        filed_note = (
            f"{len(filed)} finding(s) filed at the round cap ({rounds}/{max_rounds}): "
            + ", ".join(f"{k} -> {n}" for k, n in zip(disposition_named, filed))
        )
        disposition_text = ""

    if covered and corroboration:
        return REFUSED, corroboration, "", recompute_note
    if covered:
        if disposition_text:
            # Rounds remain (the exhausted case returned above), so the
            # refusal teaches the fix-delta remedy AND shows the budget the
            # next round spends - AC7-HP's "how many rounds remain".
            remaining = _rounds_remaining_note(rounds, max_rounds)
            note = "; ".join(x for x in (recompute_note, remaining) if x)
            return REFUSED, disposition_text, "", note
        notes = [n for n in (recompute_note, disposition_note, filed_note) if n]
        return COVERED, "", (cov.get("head_sha") or "") if cov else "", "; ".join(notes)
    if failed == "uncovered" and corroboration:
        # The policy-rewritten shape (0 counted, self-attestation preserved)
        # fails the count conjunct, but the truer refusal names the policy and
        # both remedies - "re-run your own review" can never satisfy it, and
        # a worker told that will retry into budget. The other failures
        # (stale head, missing local pass, reviewer refusal) keep their own
        # refusals: those name a remedy that can actually work, and the
        # corroboration question may dissolve once the head is re-attested.
        return REFUSED, corroboration, "", recompute_note

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
        from pathlib import Path

        from fno.review_capability import render_self_review_invocation

        root = Path(_merge._repo_state_dir(repo)).parent
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
            _merge._coverage_sources(repo) if cov is None else None,
            self_review_hint=hint,
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


def _resolved_categories(repo: str) -> frozenset[str]:
    """The configured allowlist, extended per the shipped default."""
    try:
        from fno.config import load_settings_for_repo
        from fno.review.findings import resolve_nonblocking_categories

        root = Path(_merge._repo_state_dir(repo)).parent
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
    chain: list[dict], cov: Optional[dict], repo: str = "."
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
    allow = _resolved_categories(repo)
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
    corroborated = not (cov is not None and rests_on_self_attestation_alone(cov))

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
#: and name BOTH remedies. Never "run the review verb at HEAD" - that is the
#: instruction that caused the loop this state exists to end.
IMPOSSIBLE_REMEDIES = (
    "a non-author GitHub approval on the PR, or the coverage-override label"
)


def rounds_since_last_pass(chain: list[dict]) -> int:
    """Review rounds since the last pass on the chain, oldest-first.

    A round is a review VERDICT since the last pass; a pass resets the
    counter. The declared ``review_round`` wins when present (the running max
    since the reset); every event from before the field existed falls back to
    counting verdicts. The Rust-side mirror is
    ``loopcheck::rounds_since_last_pass``; the two are held equal by the
    shared corpus.
    """
    rounds = 0
    for event in chain:
        if event.get("verdict") == "pass":
            rounds = 0
            continue
        declared = event.get("review_round")
        if isinstance(declared, int) and not isinstance(declared, bool) and declared >= 0:
            rounds = max(rounds, declared)
        else:
            rounds += 1
    return rounds


def resolved_max_rounds(repo: str) -> int:
    """The clamped ``config.review.max_rounds`` for the repo (>= 1)."""
    try:
        from fno.config import load_settings_for_repo

        root = Path(_merge._repo_state_dir(repo)).parent
        value = getattr(load_settings_for_repo(root).review, "max_rounds", None)
        if isinstance(value, int) and not isinstance(value, bool) and value >= 1:
            return value
        return DEFAULT_MAX_ROUNDS
    except Exception:  # noqa: BLE001 - unreadable config keeps the shipped default
        return DEFAULT_MAX_ROUNDS


def file_findings_at_cap(keys: list[str], pr_number: int, repo: str) -> list[str]:
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
        found = run(["fno", "backlog", "find", title], cwd=repo)
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
                "--details",
                (
                    f"Filed by the review-coverage gate at the round cap on PR "
                    f"{pr_number}. The finding was still non-terminal when the "
                    "review budget was spent; it was not CONFIRMED correctness or "
                    "security, so the PR merged and the finding lands here rather "
                    "than being dropped."
                ),
            ],
            cwd=repo,
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
        f"re-reviewing - the two acts that clear it are {IMPOSSIBLE_REMEDIES}"
    )


def _rounds_remaining_note(rounds: int, max_rounds: int) -> str:
    """The REFUSED-side note AC7-HP demands: the budget a worker can see
    before the next round reports impossible. Zero remaining is still worth
    saying - the next round is the one that trips, and a worker who cannot
    see the budget cannot choose to stop."""
    remaining = max_rounds - rounds
    if remaining <= 0:
        return (
            f"{rounds}/{max_rounds} review rounds used; the next round "
            "reports impossible"
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
    repo = cwd or os.getcwd()
    state, refusal, _covered_head, note = coverage_verdict(
        pr_number, repo, recompute=recompute
    )
    if state in (REFUSED, IMPOSSIBLE):
        sys.stderr.write(f"{refusal_line(refusal, note)}\n")
    elif state == UNANSWERED:
        sys.stderr.write(f"{note}\n")
    return state
