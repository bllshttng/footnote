//! The one line a worker actually reads when coverage names a blocker. A
//! module named by its question: `loopcheck.rs` is far over the file budget
//! and shrink-only, so the receipt line, its arm order, and the tests that
//! pin every arm live here.
//!
//! The arm order is the contract: who was OWED and is absent is a wait; who
//! read an older commit needs a re-read; a spent round budget names the
//! terminal act; anything else names the review verb. Absences nobody owed
//! never own the next action - an optional App that never responds sits
//! absent on every PR forever, and letting it drive the line made the stale
//! arm (the rebase case) unreachable.

use super::*;

/// Whether every `github_app` verdict went stale WITHOUT naming a commit.
///
/// One bot with an empty `commit.oid` is a payload quirk. EVERY bot with an
/// empty one, and none reviewed, is the signature of a `gh` too old to return
/// the field - which makes freshness unresolvable for the whole axis, forever,
/// so a required bot never clears and the loop has no reachable exit. Failing
/// closed is right; reporting it as "reviewed an older commit" is not, because
/// the fix is a gh upgrade rather than a re-read.
///
/// Requires at least one stale verdict, so a PR with no bot reviews at all
/// (every verdict `Absent`) never matches: an absence of reviewers is a
/// different fact from an absence of commits on the reviews that exist.
fn blind_to_reviewed_commits(rep: &CoverageReport) -> bool {
    let github: Vec<&ReviewerVerdict> = rep
        .verdicts
        .iter()
        .filter(|v| v.producer == CoverageProducer::GithubApp)
        .collect();
    let staleness: Vec<&&ReviewerVerdict> = github
        .iter()
        .filter(|v| v.verdict == CoverageVerdict::Stale)
        .collect();
    !staleness.is_empty() && staleness.iter().all(|v| v.reviewed_sha.is_empty())
}

/// The terminal act a spent round budget names, replacing the review-verb
/// instruction in the uncovered arm. The verb is what restarted the loop;
/// past the cap the receipt must not teach another round. It names the
/// decline-file-merge act and the one operator lever that reopens review.
/// Contains no slash-verb and never the words "review verb" - the corpus
/// asserts both absences with a positive marker for this very string.
const CAP_SPENT_TERMINAL_ACT: &str = "decline the remainder, file it with the declining identity and the reason, then merge; the operator lever is config.review.max_rounds";

/// One-line coverage summary for the terminal message and receipts (x-0eaf
/// task 3.1). Printed from the coverage value at print time, never from a
/// remembered gate verdict (receipts have lied before).
///
/// `self_review_hint` is the sized invocation from `sized_self_review_hint`
/// (the Python single source). None keeps the levelless line - the hint is
/// advisory, and its absence must read identically to a build without the
/// render, never as a different verdict.
///
/// `round_cap` is `Some((rounds_used, max_rounds))` ONLY when the round
/// budget is already spent. The uncovered arm then names the terminal act
/// instead of the review verb - the instruction that restarts the loop this
/// cap exists to bound. None (the default at every under-cap call site)
/// renders exactly the pre-cap line.
pub fn coverage_receipt_line(
    rep: &CoverageReport,
    self_review_hint: Option<&str>,
    round_cap: Option<(i64, i64)>,
) -> String {
    match &rep.coverage {
        Coverage::Unknown => "review coverage: unknown (review read failed)".to_string(),
        Coverage::Covered(n) => {
            let reviewed_names: Vec<&str> = rep
                .verdicts
                .iter()
                .filter(|v| {
                    v.verdict == CoverageVerdict::Reviewed
                        && human_approval_counts(v, rep.github_approval_satisfies)
                })
                .map(|v| v.name.as_str())
                .collect();
            if *n > 0 {
                // Origin breakdown over EVERY reviewed (non-human) verdict, folded
                // by its attestation_origin, so the four buckets sum to `n`. The
                // self-attestation hazard lives on the local lane; a GitHub App
                // review has no session to compare and reads `unknown` here (it is
                // named above, so a reader sees it reviewed - "unknown" is its
                // origin, not its verdict). All four buckets are always shown so
                // a reader learns the vocabulary even when three are zero; `other`
                // is a different session, NOT "independent", and `unmeasured` is a
                // comparison that failed, not a third party.
                //
                // "all origins counted" is load-bearing: readers took the bare
                // tally for a subtraction and refused to merge green PRs over
                // it. A positive claim, not a disclaimer - a denial ("not a
                // gate") answers the question by raising it. Scoped to ORIGINS
                // because `n` does drop human approvals, so a bare "all
                // counted" would be false on a human-approved PR.
                let (self_n, other_n, unmeasured_n, unknown_n) = rep
                    .verdicts
                    .iter()
                    .filter(|v| {
                        v.verdict == CoverageVerdict::Reviewed
                            && human_approval_counts(v, rep.github_approval_satisfies)
                    })
                    .fold((0, 0, 0, 0), |(s, o, m, u), v| match v.attestation_origin {
                        AttestationOrigin::SelfAttested => (s + 1, o, m, u),
                        AttestationOrigin::OtherSession => (s, o + 1, m, u),
                        AttestationOrigin::Unmeasured => (s, o, m + 1, u),
                        AttestationOrigin::Unknown => (s, o, m, u + 1),
                    });
                return format!(
                    "review coverage: {} reviewed ({}) - all origins counted; self {}, other {}, unmeasured {}, unknown {}",
                    n,
                    reviewed_names.join(", "),
                    self_n,
                    other_n,
                    unmeasured_n,
                    unknown_n
                );
            }
            let refused: Vec<&str> = rep
                .verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Refused)
                .map(|v| v.name.as_str())
                .collect();
            let errored = rep
                .verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Errored)
                .count();
            // Absent reviewers are NAMED: "the reviewers above" pointed at the
            // refused ones, the only names the line had.
            let absent: Vec<&str> = rep
                .verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Absent)
                .map(|v| v.name.as_str())
                .collect();
            // The waiting-on arm fires only for absences anyone was OWED. An
            // optional App that never responds used to own this arm, and since
            // such an App is absent on every PR forever, the stale arm below -
            // "your local review read an older commit", the rebase case - was
            // unreachable: the worker was told to check config.review for a
            // reviewer that was not the problem. Unowed absences still ride
            // the line as a trailing clause, so the fact is named without
            // being the blocker.
            let absent_owed: Vec<&str> = rep
                .verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Absent && v.required)
                .map(|v| v.name.as_str())
                .collect();
            let absent_unowed: Vec<&str> = rep
                .verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Absent && !v.required)
                .map(|v| v.name.as_str())
                .collect();
            // Stale reviewers are NAMED too. Without this the receipt for the
            // x-5b99 specimen reads "0 reviewed, 0 refused, 0 errored, 0
            // absent" - four zeros describing a PR a bot really did review, at
            // an older commit. That is the absence-shaped lie the Stale variant
            // exists to delete, and dropping it from the one line a human reads
            // puts it straight back.
            let stale: Vec<&str> = rep
                .verdicts
                .iter()
                .filter(|v| v.verdict == CoverageVerdict::Stale)
                .map(|v| v.name.as_str())
                .collect();
            // Never prescribe the local verb while an OWED reviewer is absent,
            // and never suppress the next action entirely either. Both were
            // tried here and both were wrong: the offer walks a worker into
            // self-attesting past a reviewer the merge gate actually waits on,
            // and bare suppression strands an optional App that is never
            // installed with no reachable exit. The verdict now carries
            // required-ness, so the escape is exact: an owed absence names
            // itself as the wait, and an unowed one is named without owning
            // the next action.
            let next = if !absent_owed.is_empty() {
                format!(
                    "waiting on {} - if a reviewer there is uninstalled or no longer configured, check config.review",
                    absent_owed.join(", ")
                )
            } else if !stale.is_empty() && blind_to_reviewed_commits(rep) {
                // EVERY github_app verdict is stale AND none carries a commit at
                // all. That is not "the bots read an older commit", it is "we
                // cannot see which commit any bot read", and the two need
                // opposite responses. `gh pr view --json reviews` supplies
                // `commit.oid`; a gh too old to return it makes every bot review
                // stale forever, so a required bot never clears and the loop
                // blocks with no reachable exit. Failing closed is correct, but
                // a closed gate that reports the wrong cause is the same
                // absence-shaped lie this whole change deletes - so say which
                // absence it is.
                format!(
                    "no review carries a reviewed commit ({}) - `gh pr view --json reviews` must return `commit.oid`; upgrade gh, then ask for a re-read",
                    stale.join(", ")
                )
            } else if !stale.is_empty() {
                // A re-read by the reviewer that already responded, not a local
                // self-attest: "run the review verb" would walk a worker past a
                // reviewer that may be REQUIRED and has simply gone stale.
                format!(
                    "{} reviewed an older commit whose code no longer matches HEAD - ask for a re-read",
                    stale.join(", ")
                )
            } else if let Some((used, max)) = round_cap {
                // The round budget is spent. Naming the review verb here is
                // the instruction that restarts the loop this cap exists to
                // bound: every fix moves HEAD, voids the attestation, and
                // returns the worker to this exact line. So this arm names
                // no verb at all - it names the terminal act (decline, file,
                // merge) and the operator lever. The absent/stale arms above
                // are untouched: they answer reviewer configuration, which
                // the round budget neither causes nor cures.
                format!(
                    "the review round budget is spent ({used}/{max}) - {CAP_SPENT_TERMINAL_ACT}"
                )
            } else {
                // Both arms carry the ordering, because the verb alone does not
                // teach it: close findings, commit, push, review at the final
                // head, attest last. The None arm is the one a CI runner prints
                // (no `fno` on PATH there), so it must still name a producer -
                // a bare "run the review verb" names none.
                match self_review_hint {
                    Some(hint) => {
                        format!("run the review verb at HEAD - `{hint}` - {REVIEW_ORDER}")
                    }
                    None => format!("run the review verb at HEAD - {REVIEW_ORDER}"),
                }
            };
            // An unowed absence is named on every arm, not only the one it no
            // longer drives: the fact that a configured App is silent belongs
            // on the line, but it is not the blocker and must not read as one.
            let next = if absent_unowed.is_empty() {
                next
            } else {
                format!(
                    "{next} (not owed, still not responding: {})",
                    absent_unowed.join(", ")
                )
            };
            // `stale` counts in the tally and is NAMED in the next action, like
            // `absent`. `refused` keeps its inline names, because a refusal is
            // terminal and never drives the next action, so the tally is the
            // only place a reader can learn who declined.
            //
            // Either way the parenthetical is dropped when the list is empty.
            // A trailing `()` is a shape a previous fix deleted from this exact
            // line, and the refused bucket had quietly kept printing it in
            // every case where nothing refused - which is most of them.
            let refused_names = if refused.is_empty() {
                String::new()
            } else {
                format!(" ({})", refused.join(", "))
            };
            format!(
                "review coverage: 0 reviewed, {} refused{}, {} errored, {} stale, {} absent. No head-pinned pass attestation for this head - {}.",
                refused.len(),
                refused_names,
                errored,
                stale.len(),
                absent.len(),
                next
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::pr826_reviews;
    use super::*;
    use crate::loopcheck::{
        classify_coverage, classify_coverage_tiled, mark_owed_verdicts, review_freshness,
    };
    use crate::loopcheck::{Freshness, FreshnessFacts};

    /// Copy of the loopcheck test fixture: this module answers its own
    /// question with its own fixtures, and the helper is three fields of JSON.
    fn attestation_line_on_branch(
        reviewer: &str,
        head: &str,
        verdict: &str,
        branch: &str,
    ) -> String {
        serde_json::json!({
            "type": "review_attestation",
            "data": {"reviewer": reviewer, "head_sha": head, "verdict": verdict,
                     "attester_session_id": "sess-author", "branch": branch}
        })
        .to_string()
    }

    #[test]
    fn the_origin_key_is_never_absent_from_a_serialized_local_verdict() {
        // A read whose process resolved no authoring session (a
        // carried_base_sync row re-read from a cwd whose target manifest is
        // elsewhere) classifies the local verdict Unmeasured: a concrete
        // attester id is on the row, only the comparison failed - and that is
        // the shape of the author's own re-read, so the twin refuses it under
        // require_corroboration. The key is always present either way;
        // Unknown used to be SKIPPED on serialize, and a consumer reading
        // absent as "not self_attested" cleared a PR whose only review was
        // the author's own.
        let events = attestation_line_on_branch("code-review", "h", "pass", "feature/x");
        let unmeasured = classify_coverage(
            &[],
            &[],
            &events,
            &[],
            true,
            None,
            &|_| Freshness::Fresh,
            "feature/x",
            "h",
        );
        let row = serde_json::to_value(&unmeasured.verdicts).unwrap();
        let v = row
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["producer"] == "local_attestation")
            .unwrap();
        assert_eq!(v["attestation_origin"], serde_json::json!("unmeasured"));

        let measured = classify_coverage(
            &[],
            &[],
            &events,
            &[],
            true,
            Some("sess-author"),
            &|_| Freshness::Fresh,
            "feature/x",
            "h",
        );
        let row = serde_json::to_value(&measured.verdicts).unwrap();
        let v = row
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["producer"] == "local_attestation")
            .unwrap();
        assert_eq!(v["attestation_origin"], serde_json::json!("self_attested"));

        let mut held = unmeasured;
        assert!(held.rests_on_self_attestation_alone());
        held.apply_corroboration_policy(true);
        assert_eq!(held.coverage, Coverage::Covered(0));
    }

    #[test]
    fn an_attestation_origin_of_unknown_or_absent_counts_as_the_author_s_own() {
        // The pin the operator asked for, on the Rust twin: an env_only lane
        // leaves the attester id absent, classify lands Unknown, and Unknown
        // REFUSES - it is no evidence of who attested, and an author running
        // the review from a shell with no harness marker (harness_identity
        // returns an empty id) lands in exactly this bucket, so treating it
        // as a peer would clear a self-review. The whole count is then the
        // author's own and rests_on_self_attestation_alone is TRUE; the
        // corroboration policy demotes the row to uncovered.
        let events = serde_json::json!({
            "type": "review_attestation",
            "data": {"reviewer": "code-review", "head_sha": "h", "verdict": "pass",
                     "branch": "feature/x"}
        })
        .to_string();
        let rep = classify_coverage(
            &[],
            &[],
            &events,
            &[],
            true,
            Some("sess-author"),
            &|_| Freshness::Fresh,
            "feature/x",
            "h",
        );
        assert_eq!(rep.coverage, Coverage::Covered(1));
        assert!(rep.rests_on_self_attestation_alone());
        let mut held = rep;
        held.apply_corroboration_policy(true);
        assert_eq!(held.coverage, Coverage::Covered(0));

        // The row-level twin: a verdict whose attestation_origin key is
        // ABSENT (a pre-field producer, or any writer that dropped the field)
        // deserializes to the refusing default, so the predicate reads it as
        // the author's own too. A positive marker on the default path, not a
        // matching-word probe.
        let events = attestation_line_on_branch("code-review", "h", "pass", "feature/x");
        let measured = classify_coverage(
            &[],
            &[],
            &events,
            &[],
            true,
            Some("sess-author"),
            &|_| Freshness::Fresh,
            "feature/x",
            "h",
        );
        let mut row = serde_json::to_value(&measured.verdicts[0]).unwrap();
        row.as_object_mut().unwrap().remove("attestation_origin");
        let v: ReviewerVerdict = serde_json::from_value(row).unwrap();
        assert_eq!(v.attestation_origin, AttestationOrigin::Unknown);
        assert!(counts_as_self_attestation_basis(&v, false));
    }

    #[test]
    fn an_unmeasured_peer_refuses_and_a_measured_one_corroborates() {
        // The mixed row the peer review named, answered by the split: a
        // measured self plus a peer whose comparison failed (author session
        // unavailable) refuses - the whole count then reads as the author's
        // own - while the same peer with both sessions measured is a real
        // OtherSession and corroborates. Both classifications come from the
        // same events line with the author session supplied or not.
        let events = format!(
            "{}\n{}",
            attestation_line_on_branch("code-review", "h", "pass", "feature/x"),
            serde_json::json!({
                "type": "review_attestation",
                "data": {"reviewer": "peer", "head_sha": "h", "verdict": "pass",
                         "attester_session_id": "sess-peer", "branch": "feature/x"}
            })
        );
        let split = classify_coverage(
            &[],
            &[],
            &events,
            &[],
            true,
            None,
            &|_| Freshness::Fresh,
            "feature/x",
            "h",
        );
        assert!(split.rests_on_self_attestation_alone());

        let corroborated = classify_coverage(
            &[],
            &[],
            &events,
            &[],
            true,
            Some("sess-author"),
            &|_| Freshness::Fresh,
            "feature/x",
            "h",
        );
        assert!(!corroborated.rests_on_self_attestation_alone());
    }

    #[test]
    fn coverage_receipt_names_a_stale_reviewer_instead_of_four_zeros() {
        // The receipt for the x-5b99 specimen used to read "0 reviewed, 0
        // refused, 0 errored, 0 absent" - four zeros over a PR codex really did
        // review, at an older commit - and then prescribed the local verb,
        // which is the one move that does NOT get the bot to re-read.
        let rep = classify_coverage(
            &pr826_reviews(),
            &[],
            "",
            &["chatgpt-codex-connector".to_string()],
            true,
            None,
            &|_| Freshness::Stale,
            "",
            "",
        );
        let line = coverage_receipt_line(&rep, None, None);
        // Counted in the tally, NAMED in the next action - the same split the
        // absent bucket uses, and the reason the line carries no empty `()`.
        assert!(line.contains("1 stale,"), "{line}");
        assert!(line.contains("chatgpt-codex-connector"), "{line}");
        assert!(!line.contains("()"), "{line}");
        assert!(line.contains("ask for a re-read"), "{line}");
        assert!(!line.contains("run the review verb"), "{line}");
    }

    #[test]
    fn coverage_receipt_separates_an_old_commit_from_no_commit_at_all() {
        // Both shapes are "stale", and they need OPPOSITE responses. A bot that
        // read an older commit needs a re-read. A whole axis with no commit on
        // any review needs a gh upgrade, because `commit.oid` is where
        // freshness comes from and without it every bot review is stale
        // forever - a required bot never clears and the loop has no exit.
        let no_commit = vec![serde_json::json!({
            "author": {"login": "chatgpt-codex-connector"}, "state": "COMMENTED"
        })];
        let rep = classify_coverage(
            &no_commit,
            &[],
            "",
            &["chatgpt-codex-connector".to_string()],
            true,
            None,
            &|sha| review_freshness(sha, "89bc0b91", &FreshnessFacts::default()),
            "",
            "",
        );
        let line = coverage_receipt_line(&rep, None, None);
        assert!(
            line.contains("no review carries a reviewed commit"),
            "{line}"
        );
        assert!(line.contains("upgrade gh"), "{line}");

        // The ordinary stale case keeps the re-read instruction and must NOT
        // mention gh: the payload named a commit, it is simply an older one.
        let old_commit = classify_coverage(
            &pr826_reviews(),
            &[],
            "",
            &["chatgpt-codex-connector".to_string()],
            true,
            None,
            &|_| Freshness::Stale,
            "",
            "",
        );
        let line = coverage_receipt_line(&old_commit, None, None);
        assert!(line.contains("ask for a re-read"), "{line}");
        assert!(!line.contains("upgrade gh"), "{line}");
    }

    #[test]
    fn coverage_receipt_embeds_the_sized_self_review_hint() {
        // The refusal's whole job is to hand the worker the exact invocation
        // (sized by the Python single source); the receipt line embeds whatever
        // the bridge produced, verbatim and backticked, after the existing
        // instruction so the phrase's other assertions keep holding.
        let comments = vec![serde_json::json!({
            "author": {"login": "chatgpt-codex-connector[bot]"},
            "body": "You have reached your Codex usage limits for code reviews."
        })];
        let rep = classify_coverage(
            &[],
            &comments,
            "",
            &["chatgpt-codex-connector".to_string()],
            true,
            None,
            &|_| Freshness::Fresh,
            "",
            "89bc0b91",
        );
        let hint = "/verb-from-the-builder --flags";
        let line = coverage_receipt_line(&rep, Some(hint), None);
        assert!(line.contains("run the review verb at HEAD"), "{line}");
        assert!(line.contains(&format!("`{hint}`")), "{line}");
        // None must read identically to a build without the render.
        let bare = coverage_receipt_line(&rep, None, None);
        assert!(!bare.contains("verb-from-the-builder"), "{bare}");
    }

    #[test]
    fn coverage_receipt_past_the_cap_names_the_terminal_act_and_no_verb() {
        // The spent-budget arm. The uncovered receipt used to answer a worker
        // past the round cap with "run the review verb at HEAD" - the exact
        // instruction that restarts the loop the cap exists to bound. Past the
        // cap the line must name the terminal act instead. Absences alone
        // pass on a line that never rendered, so the render itself is
        // asserted first, then the positive marker, then the four needles.
        let rep = CoverageReport {
            github_approval_satisfies: false,
            coverage: Coverage::Covered(0),
            verdicts: Vec::new(),
        };
        // The hint is a placeholder, not a real invocation: concrete review
        // levels live in the sized-invocation builder alone (single-source
        // guard), and this test only needs SOME hint string to prove the
        // past-cap arm ignores it.
        let hint = "/verb-from-the-builder --flags";
        let line = coverage_receipt_line(&rep, Some(hint), Some((3, 2)));
        assert!(line.starts_with("review coverage:"), "{line}");
        assert!(
            line.contains("the review round budget is spent (3/2)"),
            "{line}"
        );
        assert!(
            line.contains(
                "decline the remainder, file it with the declining identity and the reason, then merge"
            ),
            "{line}"
        );
        for needle in ["/code-review", "/review", "/fno:review", "review verb"] {
            assert!(
                !line.contains(needle),
                "past-cap line names {needle}: {line}"
            );
        }
    }

    #[test]
    fn coverage_receipt_under_the_cap_keeps_the_review_verb() {
        // The same uncovered report with the budget unspent: the verb arm is
        // untouched, so the arm swap above did not eat the normal path.
        let rep = CoverageReport {
            github_approval_satisfies: false,
            coverage: Coverage::Covered(0),
            verdicts: Vec::new(),
        };
        let hint = "/verb-from-the-builder --flags";
        let line = coverage_receipt_line(&rep, Some(hint), None);
        assert!(line.contains("run the review verb at HEAD"), "{line}");
        assert!(line.contains(&format!("`{hint}`")), "{line}");
    }

    #[test]
    fn an_unowed_absent_reviewer_does_not_outrank_a_stale_one() {
        // The rebase shape: a head move staled the local review, and the
        // absent-forever optional App owned the receipt's next action, so the
        // worker was told to check config.review for a reviewer that was not
        // the problem. Nobody owes the absence, so the stale arm - ask for a
        // re-read - finally fires.
        let events = attestation_line_on_branch("code-review", "oldhead", "pass", "feature/x");
        let mut rep = classify_coverage_tiled(
            &[],
            &[],
            &events,
            &["gemini-code-assist".to_string()],
            true,
            Some("sess-author"),
            &|_| Freshness::Stale,
            "feature/x",
            "currenthead",
            None,
            None,
            false,
        );
        mark_owed_verdicts(&mut rep, &[]);
        let line = coverage_receipt_line(&rep, None, None);
        assert!(line.contains("reviewed an older commit"), "{line}");
        assert!(line.contains("code-review"), "{line}");
        // The unowed absence is still named - as a clause, never the blocker.
        assert!(line.contains("gemini-code-assist"), "{line}");
    }

    #[test]
    fn an_owed_absent_reviewer_still_wins() {
        let events = attestation_line_on_branch("code-review", "oldhead", "pass", "feature/x");
        let mut rep = classify_coverage_tiled(
            &[],
            &[],
            &events,
            &["gemini-code-assist".to_string()],
            true,
            Some("sess-author"),
            &|_| Freshness::Stale,
            "feature/x",
            "currenthead",
            None,
            None,
            false,
        );
        mark_owed_verdicts(&mut rep, &["gemini-code-assist".to_string()]);
        let line = coverage_receipt_line(&rep, None, None);
        assert!(line.contains("waiting on gemini-code-assist"), "{line}");
        assert!(!line.contains("reviewed an older commit"), "{line}");
    }

    #[test]
    fn an_unowed_absent_reviewer_is_still_named() {
        // Dropping the absence from the line would lose the fact that a
        // configured App is silent. It rides every arm as the trailing
        // clause - named, but never the blocker.
        let mut rep = classify_coverage_tiled(
            &[],
            &[],
            "",
            &["gemini-code-assist".to_string()],
            true,
            None,
            &|_| Freshness::Fresh,
            "",
            "",
            None,
            None,
            false,
        );
        mark_owed_verdicts(&mut rep, &[]);
        let line = coverage_receipt_line(&rep, None, None);
        assert!(line.contains("gemini-code-assist"), "{line}");
        assert!(line.contains("not owed, still not responding"), "{line}");
        assert_eq!(rep.review_state(), Some(ReviewState::Unreviewed));
    }
}
