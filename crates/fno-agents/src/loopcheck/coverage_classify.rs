//! Is this head covered? Coverage classification over reviews and local attestations, and the review_coverage event payload.

use super::*;

/// Classify every reviewer response and compute coverage. Pure: takes the
/// already-fetched GitHub review/comment arrays, the events.jsonl text, the
/// current HEAD, and the configured GitHub-App logins; performs no IO, so it is
/// unit-testable in isolation. A failed GitHub read (`github_read_ok = false`)
/// makes coverage `Unknown` UNLESS the local axis carries a head-pinned pass:
/// positive local evidence survives a bot outage, because the local lane is
/// never rate-limited by the bot's quota (the PR #214 failure in a new hat,
/// which this node exists to escape).
///
/// `author_session` is the manifest's `harness_session_id` (the session that ran
/// `fno do target init` in this worktree). Each local attestation's
/// `attester_session_id` is compared against it to label `attestation_origin`;
/// `None` (no manifest / unparseable) leaves every local verdict `Unknown`,
/// failing open on unknown authorship so the coverage verdict is byte-identical
/// to the pre-change behavior. `coverage_count` never reads the origin: every
/// `Reviewed` verdict counts regardless of it, `SelfAttested` included.
pub fn classify_coverage(
    reviews: &[Value],
    comments: &[Value],
    events_text: &str,
    github_app_logins: &[String],
    github_read_ok: bool,
    author_session: Option<&str>,
    freshness: &dyn Fn(&str) -> Freshness,
    head_branch: &str,
    head_sha: &str,
) -> CoverageReport {
    classify_coverage_tiled(
        reviews,
        comments,
        events_text,
        github_app_logins,
        github_read_ok,
        author_session,
        freshness,
        head_branch,
        head_sha,
        None,
        None,
        false,
    )
}

/// [`classify_coverage`] with the range-tiling answer supplied. The two
/// production call sites compute the tiling once and pass it; the bare
/// spelling keeps the pre-tiling semantics for the unit-test corpus.
/// `pr_author` is the PR author's login (None = unreadable, which excludes
/// human approvals from the count fail-closed); `github_approval_satisfies`
/// is the resolved config flag (false on the bare spelling = today's
/// semantics for the unit-test corpus).
#[allow(clippy::too_many_arguments)]
pub fn classify_coverage_tiled(
    reviews: &[Value],
    comments: &[Value],
    events_text: &str,
    github_app_logins: &[String],
    github_read_ok: bool,
    author_session: Option<&str>,
    freshness: &dyn Fn(&str) -> Freshness,
    head_branch: &str,
    head_sha: &str,
    tiling: Option<&RangeTiling>,
    pr_author: Option<&str>,
    github_approval_satisfies: bool,
) -> CoverageReport {
    let (local_passes, pairs_raising_findings) =
        local_latest_attestations(events_text, head_branch, head_sha);
    let mut verdicts: Vec<ReviewerVerdict> = Vec::new();
    verdicts.extend(local_refused_verdicts(events_text, head_sha));

    if github_read_ok {
        // (1) Collect distinct KNOWN review-App authors that posted a review
        // object. A present review counts whether or not the App is in the
        // configured required/optional list - "did anyone review" must not
        // hinge on the operator having pre-listed the bot that happened to
        // review (chatgpt-codex-connector reviewing on a default
        // no-required-bots config still counts). A known App is a BOT_PROFILES
        // login or a configured github_app; a bare `[bot]` suffix is not.
        //
        // Each author keeps its FRESHEST review, not its latest: `.commit.oid`
        // says which commit that review read, and an author that reviewed
        // several commits is covered by whichever of them still describes HEAD.
        let mut reviewed_authors: Vec<(String, String, Freshness)> = Vec::new();
        for r in reviews {
            let author = r
                .pointer("/author/login")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if author.is_empty() || !author_is_known_bot(author, github_app_logins) {
                continue;
            }
            let state = r.get("state").and_then(|v| v.as_str()).unwrap_or("");
            if state.is_empty() {
                continue;
            }
            // The commit the reviewer actually read. Already in this payload
            // (`gh pr view --json reviews`) and discarded until now, so pinning
            // the github_app axis costs no new API call. Absent -> "" ->
            // Stale, which is the fail-closed direction.
            let oid = r
                .pointer("/commit/oid")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let fresh = freshness(oid);
            match reviewed_authors
                .iter_mut()
                .find(|(a, _, _)| logins_correspond(a, author))
            {
                Some(entry) => {
                    if freshness_rank(fresh) > freshness_rank(entry.2) {
                        entry.1 = oid.to_string();
                        entry.2 = fresh;
                    }
                }
                None => reviewed_authors.push((author.to_string(), oid.to_string(), fresh)),
            }
        }
        // (1b) A known App whose clean pass is a COMMENT posts no review
        // object, so the scan above never sees it. Without this, (3) counts a
        // findings review from an unconfigured App and drops its clean one -
        // the gate strictly easier to satisfy with a flawed PR than a clean
        // one, the same defect `bot_verdict` closes for CONFIGURED logins.
        for c in comments {
            let author = c
                .pointer("/author/login")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if author.is_empty()
                || !author_is_known_bot(author, github_app_logins)
                || reviewed_authors
                    .iter()
                    .any(|(a, _, _)| logins_correspond(a, author))
            {
                continue;
            }
            if let Some((sha, fresh, _)) = clean_pass_review(comments, author, freshness) {
                reviewed_authors.push((author.to_string(), sha, fresh));
            }
        }
        // (2) One verdict per unique configured login: reviewed if a
        // corresponding author posted, else refused on a usage-limit comment,
        // else absent. Dedup so a login in two lists is one verdict, not two
        // units of coverage (Failure Modes).
        let mut seen: Vec<String> = Vec::new();
        for login in github_app_logins {
            let login = login.trim();
            if login.is_empty() || seen.iter().any(|s| logins_correspond(s, login)) {
                continue;
            }
            seen.push(login.to_string());
            // One verdict from the ONE per-bot predicate the presence gate
            // also reads (`bot_verdict`): every arm - review object, pinned
            // clean-pass comment, usage refusal, the stale fallbacks - is
            // shared, so this axis can never answer differently from
            // `compute_review_info` about the same evidence. The stale
            // fallbacks keep a responded-but-outdated verdict RECORDED rather
            // than dropped, which is what makes the tightening auditable.
            let (verdict, reviewed_sha, fresh) = bot_verdict(login, reviews, comments, freshness);
            // A bot `reviewed` verdict means the predicate read an approval or
            // clean-pass marker, so the pass subset counts it.
            let passed = verdict == CoverageVerdict::Reviewed;
            verdicts.push(ReviewerVerdict {
                producer: CoverageProducer::GithubApp,
                name: login.to_string(),
                verdict,
                human_approval: false,
                author_approval: false,
                attestation_origin: AttestationOrigin::Unknown,
                reviewed_sha,
                freshness: fresh,
                scope: None,
                refusal_reason: None,
                reviewer_context: None,
                // A configured login's verdict was OWED until proven otherwise:
                // the resolved required set (mark_owed_verdicts) is what may
                // downgrade an optional login, never this scan.
                required: true,
                passed,
            });
        }
        // (3) Known-App reviewers NOT in the configured list still count
        // (reviewed), so coverage reflects the review that actually happened.
        for (author, sha, fresh) in &reviewed_authors {
            if !seen.iter().any(|s| logins_correspond(author, s)) {
                verdicts.push(ReviewerVerdict {
                    producer: CoverageProducer::GithubApp,
                    name: author.clone(),
                    verdict: if fresh.counts() {
                        CoverageVerdict::Reviewed
                    } else {
                        CoverageVerdict::Stale
                    },
                    human_approval: false,
                    author_approval: false,
                    attestation_origin: AttestationOrigin::Unknown,
                    reviewed_sha: sha.clone(),
                    freshness: Some(*fresh),
                    scope: None,
                    refusal_reason: None,
                    reviewer_context: None,
                    // Not in any configured list, so a fortiori not owed.
                    required: false,
                    // A recorded GitHub review read with an approving state.
                    passed: fresh.counts(),
                });
            }
        }
        // (4) Human GitHub approvals: non-bot authors with state APPROVED.
        // Recorded as `reviewed` with `human_approval: true` so they are
        // visible but excluded from the count until the operator decides (lean:
        // exclude). Computed here so the answer is a one-line flip, not a
        // redesign.
        let mut seen_human: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in reviews {
            let author = r
                .pointer("/author/login")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if author.is_empty()
                || author_is_bot(author, github_app_logins)
                || seen_human.contains(author)
            {
                continue;
            }
            if r.get("state").and_then(|v| v.as_str()) == Some("APPROVED") {
                seen_human.insert(author.to_string());
                // Freshness applies here too, though it changes no count: a
                // human approval is excluded either way. It changes what a
                // human READS in `fno do pr status`, and an approval rendered
                // identically whether or not its author saw this code is the
                // same lie one axis down.
                let oid = r
                    .pointer("/commit/oid")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let fresh = freshness(oid);
                // The belt over GitHub's braces: the server refuses an
                // author's own approval, and this asserts the property the
                // counting rule depends on instead of inferring it. An
                // unreadable PR author (None) asserts author_approval - the
                // fail-closed direction is "do not count", never "count".
                let author_approval = match pr_author {
                    Some(pa) => login_equals(pa, author),
                    None => true,
                };
                verdicts.push(ReviewerVerdict {
                    producer: CoverageProducer::GithubApp,
                    name: author.to_string(),
                    verdict: if fresh.counts() {
                        CoverageVerdict::Reviewed
                    } else {
                        CoverageVerdict::Stale
                    },
                    human_approval: true,
                    author_approval,
                    attestation_origin: AttestationOrigin::Unknown,
                    reviewed_sha: oid.to_string(),
                    freshness: Some(fresh),
                    scope: None,
                    refusal_reason: None,
                    reviewer_context: None,
                    // Not in any configured list, so a fortiori not owed.
                    required: false,
                    // An APPROVED review object is a pass by definition.
                    passed: fresh.counts(),
                });
            }
        }
    }

    // local_attestation axis: one verdict per distinct latest attestation -
    // every `pass`, plus an ANSWERED `fail` - labeled
    // with whether the authoring session emitted it, and pinned to the head the
    // attestation itself recorded rather than to the head at eval time.
    // A verdict also counts when the branch's attestation CHAIN tiles
    // base..head and this attestation is one of its links: every commit that fixes a
    // finding moves the head, so under a single-attestation freshness rule a
    // disciplined fix-and-re-review loop still cannot terminate. Tiling is
    // what makes the later rounds of that loop count.
    //
    //: a latest-`fail` pair whose chain findings are all terminally
    // dispositioned ANSWERS this head and counts exactly like a pass here.
    // The pass condition is answered-at-this-head, never clean-at-this-head,
    // so declining everything terminates the loop instead of demanding the
    // "reviewer finds nothing" outcome that made it unkillable. Two guards
    // keep the promotion honest (review findings 1-2): the pair's OWN chain
    // must have raised keyed findings (a bystander's findings-free fail never
    // rides another reviewer's dispositions), and a RETRACTION entry never
    // promotes (it revokes, it never covers). Terminality reads the chain
    // alone: origin never gates, so the author's own decline is terminal
    // exactly as a second session's. Below the cap the disposition scan still
    // withholds on any open finding; at the cap the budget discharges it.
    let rounds_exhausted = tiling.map(|t| t.rounds_exhausted).unwrap_or(false);
    let answered_fail = |lp: &LocalPass| {
        !lp.is_pass
            && !lp.is_retraction
            && pairs_raising_findings.contains(&(lp.reviewer.clone(), lp.attester.clone()))
    };
    for lp in local_passes.iter().filter(|lp| lp.is_pass) {
        verdicts.push(local_attestation_verdict(
            lp,
            freshness,
            tiling,
            author_session,
        ));
    }
    if local_passes.iter().any(|lp| answered_fail(lp)) {
        let chain = in_scope_chain(events_text, head_branch, head_sha);
        let blockers = disposition_blockers_on_chain(&chain);
        if !blockers_withhold(&blockers, rounds_exhausted) {
            for lp in local_passes.iter().filter(|lp| answered_fail(lp)) {
                verdicts.push(local_attestation_verdict(
                    lp,
                    freshness,
                    tiling,
                    author_session,
                ));
            }
        }
    }

    // The spent-budget arm of the local axis (the operator's cap ruling): a
    // chain of rounds that found things READ the diff every round, but a
    // declined round emits verdict==fail, and the pass scan above admits
    // passes only - so at the cap, with findings filed or declined and
    // nothing hard left, the chain still leaves NO pass anywhere and the
    // terminal act the receipt names (decline, file, merge) is structurally
    // unreachable: a clean round would be required, the one round the cap
    // exists to refuse. Past the budget, when the chain tiles, the newest
    // in-scope FAIL attestation per reviewer therefore counts as Reviewed
    // at its chain-member head, exactly as a pass link does. Under the
    // budget nothing changes: the disposition gate (not coverage) is what
    // findings must satisfy, and it still withholds there - Locked
    // Decision 1's terminal half is enforced by blockers_withhold, which
    // past the cap keeps withholding every HARD finding and, before the
    // cap, every finding at all.
    if let Some(t) = tiling {
        if t.rounds_exhausted && t.tiled {
            // Newest in-scope fail attestation per reviewer whose head is a
            // chain link. Append order is recency (same assumption the pass
            // scan makes), so a later row for a reviewer replaces the earlier.
            let mut fails: Vec<LocalPass> = Vec::new();
            for line in events_text.lines() {
                let Ok(val) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
                    continue;
                }
                let Some(verdict) = val.pointer("/data/verdict").and_then(|v| v.as_str()) else {
                    continue;
                };
                if verdict == "pass" {
                    continue;
                }
                // A RETRACTION is a fail row carrying retracts_attester, and
                // local_latest_passes drops the pass it names. Admitting it
                // here would re-mint at the revoked head the very coverage
                // the revocation exists to destroy, so a revoke would ADD
                // coverage. It is not a review round; it is the undoing of
                // one.
                if val
                    .pointer("/data/retracts_attester")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
                {
                    continue;
                }
                // The same zero-evidence guard the pass scan applies. A row
                // that measured no lines and no files read nothing, whatever
                // its verdict says, and counting it past the cap would let a
                // hand-crafted or pre-guard fail row mint Covered(1).
                if zero_evidence_attestation(&val) {
                    continue;
                }
                let Some(reviewer) = val.pointer("/data/reviewer").and_then(|v| v.as_str()) else {
                    continue;
                };
                // Normalized exactly as the pass scan normalizes it (6086):
                // a `/code-review` fail row beside a `code-review` pass is
                // ONE reviewer, and an unnormalized compare here would slip
                // past the dedup below and count it twice.
                let reviewer = reviewer.trim_start_matches('/');
                let Some(line_head) = val.pointer("/data/head_sha").and_then(|v| v.as_str()) else {
                    continue;
                };
                if line_head.is_empty() {
                    continue;
                }
                let line_branch = val
                    .pointer("/data/branch")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !attestation_in_scope(line_branch, line_head, head_branch, head_sha) {
                    continue;
                }
                if !t.chain_heads.iter().any(|h| h == line_head) {
                    continue;
                }
                let attester = val
                    .pointer("/data/attester_session_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .filter(|s| !s.is_empty());
                if let Some(existing) = fails.iter_mut().find(|f| f.reviewer == reviewer) {
                    existing.head = line_head.to_string();
                    existing.attester = attester;
                    existing.branch = line_branch.to_string();
                } else {
                    fails.push(LocalPass {
                        reviewer: reviewer.to_string(),
                        attester,
                        head: line_head.to_string(),
                        branch: line_branch.to_string(),
                        reviewed_base: String::new(),
                        // Both false by construction: this scan admits only
                        // non-pass lines, and it skips retractions outright
                        // above rather than recording them as covering.
                        is_pass: false,
                        is_retraction: false,
                        reviewer_context: None,
                    });
                }
            }
            for lp in &fails {
                // A reviewer whose chain already yielded a PASS is already in
                // verdicts from the loop above. Pushing its fail link too
                // would count one reviewer twice, so Covered(2) and the row's
                // reviewed_count would both read 2 for a single reviewer.
                if verdicts.iter().any(|v| {
                    v.producer == CoverageProducer::LocalAttestation
                        && login_equals(&v.name, &lp.reviewer)
                }) {
                    continue;
                }
                let fresh = freshness(&lp.head);
                verdicts.push(ReviewerVerdict {
                    producer: CoverageProducer::LocalAttestation,
                    name: lp.reviewer.clone(),
                    // A chain-member head is Reviewed whatever the single-sha
                    // freshness rule says, because the chain as a whole read
                    // base..head - the same rescue the pass loop applies.
                    verdict: CoverageVerdict::Reviewed,
                    human_approval: false,
                    author_approval: false,
                    attestation_origin: classify_attestation_origin(
                        lp.attester.as_deref(),
                        author_session,
                    ),
                    reviewed_sha: lp.head.clone(),
                    freshness: Some(fresh),
                    scope: Some(if lp.branch.is_empty() {
                        AttestationScope::LegacyHeadMatch
                    } else {
                        AttestationScope::AttestedBranch
                    }),
                    refusal_reason: None,
                    reviewer_context: lp.reviewer_context.clone(),
                    // Local-attestation lane: always owed.
                    required: true,
                    // A cap-arm rescue: the attestation said fail, and only the
                    // spent budget discharged it.
                    passed: false,
                });
            }
        }
    }

    // Unknown only when the GitHub read failed AND no local review still
    // describes HEAD. A COUNTING local pass is positive evidence that trumps a
    // bot outage, so coverage is Known(local) in that case, not Unknown. A
    // stale local pass is not evidence of anything current, so it must not
    // rescue the read the way a fresh one does.
    let local_counts = verdicts.iter().any(|v| {
        v.producer == CoverageProducer::LocalAttestation && v.verdict == CoverageVerdict::Reviewed
    });
    let counted = verdicts
        .iter()
        .filter(|v| {
            v.verdict == CoverageVerdict::Reviewed
                && human_approval_counts(v, github_approval_satisfies)
        })
        .count();
    // A SPENT round budget DISCHARGES the review obligation, and it does so
    // here, before the evidence arms below, so that no shape of PR can escape
    // it. `config.review.max_rounds = 2` means "this PR gets two rounds, then
    // review is DONE" - a budget you spend, not a bar you clear.
    //
    // This is the mirror of the same discharge in `_coverage_gate`, and it has
    // to live at the coverage decision rather than at any one consumer: this
    // value feeds the published fno/review-coverage status, the stop-hook
    // receipt, and the emitted row alike. Fixing only the merge verb left the
    // status red and the hook still asking for another round, which is what
    // kept spending rounds past the cap.
    //
    // Why it survives a rebase or a force push, which is the case that
    // mattered: `rounds_used` counts distinct reviewed commits from GitHub
    // review objects, and those objects outlive the shas a rebase orphans.
    // Coverage evidence is head-pinned and does NOT survive, so before this
    // change every force push reset coverage to zero while the spent budget
    // stayed spent - a ratchet that made each rebase strictly harder to merge.
    // Reading the discharge from the budget turns that same persistence into
    // the thing that keeps a reviewed PR reviewed.
    //
    // Unknown is deliberately NOT preferred over a discharge: an unreadable
    // GitHub is a reason to stop asking for more review at a spent budget,
    // never a reason to demand a round the budget cannot fund. CONFIRMED
    // correctness and security findings are unaffected - they block through
    // the disposition gate, which never consults coverage.
    let budget_spent = tiling.map(|t| t.rounds_exhausted).unwrap_or(false);
    let coverage = if budget_spent {
        Coverage::Covered(counted.max(1))
    } else if !github_read_ok && !local_counts {
        Coverage::Unknown
    } else {
        Coverage::Covered(counted)
    };

    CoverageReport {
        coverage,
        verdicts,
        github_approval_satisfies,
    }
}

/// Build the `review_coverage` event payload. The per-reviewer verdicts
/// serialize via their serde derives (producer/verdict snake_cased);
/// `reviewed_count` and `passed_count` ride EVERY row: a review that happened
/// with findings open must not read as zero reviews, whatever the coverage
/// word says (the fail rounds count as reviews - the operator's round-cap
/// ruling).
///
/// `repo` is the git-remote slug, and it is what makes this event safe to write
/// into the CROSS-PROJECT `~/.fno/events.jsonl`: `pr` alone is a bare integer,
/// so a reader scanning the global log for PR 781 would otherwise accept
/// another repo's PR 781 as coverage for this one. Omitted (not null) when the
/// slug is unresolvable, and a reader must then decline to match it globally.
pub fn coverage_event_data(
    pr: i64,
    rep: &CoverageReport,
    head_sha: &str,
    repo: &str,
    author_session: Option<&str>,
) -> serde_json::Value {
    coverage_event_data_tiled(pr, rep, head_sha, repo, author_session, None)
}

pub fn coverage_event_data_tiled(
    pr: i64,
    rep: &CoverageReport,
    head_sha: &str,
    repo: &str,
    author_session: Option<&str>,
    tiling: Option<&RangeTiling>,
) -> serde_json::Value {
    coverage_event_data_full(pr, rep, head_sha, repo, author_session, tiling, None)
}

/// The full serializer: `posture` rides only on rows that resolved one (the
/// unknown/failure rows constructed ad hoc carry None and omit the object, so
/// absence reads "no posture resolved here", never "posture unsatisfied").
pub(super) fn coverage_event_data_full(
    pr: i64,
    rep: &CoverageReport,
    head_sha: &str,
    repo: &str,
    author_session: Option<&str>,
    tiling: Option<&RangeTiling>,
    posture: Option<&PostureVerdict>,
) -> serde_json::Value {
    // Three states, not two. `Covered(0)` is a real known zero and
    // `Coverage::is_covered()` has always returned false for it, but the
    // serializer rendered every `Covered(n)` as the string "covered" - so
    // `coverage: "covered"` and `reviewed_count: 0` co-occurred on three PRs
    // in flight, and the reassuring WORD sat beside the honest NUMBER. A
    // reader trusts the word. Emitting "uncovered" for a zero makes the two
    // agree, and it is additive for every current consumer: they all already
    // test `coverage == "covered" AND count > 0`, so a historical "covered"
    // event with a zero count keeps reading as not-covered.
    let coverage_str = match &rep.coverage {
        Coverage::Unknown => "unknown",
        Coverage::Covered(0) => "uncovered",
        Coverage::Covered(_) => "covered",
    };
    let mut data = serde_json::json!({
        "pr": pr,
        "coverage": coverage_str,
        "verdicts": &rep.verdicts,
        "head_sha": head_sha,
    });
    if let Some(review_state) = rep.review_state_at(tiling.is_some_and(|t| t.rounds_exhausted)) {
        data["review_state"] = serde_json::json!(review_state);
    }
    // One counting rule for every row: a round is a review whatever it
    // concluded, so the count never reads below the rounds the chain already
    // spent (review_count). The coverage word and the count answer different
    // questions; a fail round moves the count while the word stays red.
    data["reviewed_count"] = serde_json::json!(review_count::reviewed_count(rep, tiling));
    if let Coverage::Covered(_) = &rep.coverage {
        // How much of that count is the author reviewing its own diff. Nothing
        // gates on it: self-review is the DEFAULT path (`self_review_required`
        // floors `/code-review` onto the author's own head), so refusing a
        // self-attested pass would wedge every single-session PR, and whether
        // it SHOULD is a merge-authority decision rather than a freshness one.
        // What was missing is that the answer lived only in prose. It is a
        // number on the verdict now, so a reader can see it and a future gate
        // is one predicate rather than a redesign. Deliberately not called
        // `independent_count`: the schema is explicit that `other_session` is
        // not independence, and this must not launder that.
        //
        // Emitted only when an author session was established. With none,
        // classify_attestation_origin labels a present-attester verdict
        // Unmeasured (or Unknown when the attester is absent too), so
        // `self_attested_count()` would read 0 while the truth is unmeasured
        // - a measured-zero shape (: an aggregate reporting a state
        // its inputs do not support). The field is omitted instead, never 0,
        // so the day a gate enforces it, absence reads unmeasured rather
        // than "no self-attest" and cannot serve as the bypass. After a
        // carry-forward the session may be the HISTORICAL author's, so the
        // count is telemetry about the carried identity, not a proof this
        // process measured anything.
        if author_session.is_some() {
            data["self_attested_count"] = serde_json::json!(rep.self_attested_count());
        }
    }
    data["passed_count"] = serde_json::json!(rep.passed_count());
    if let Some(author_session_id) = author_session {
        data["author_session_id"] = serde_json::json!(author_session_id);
    }
    if !repo.is_empty() {
        data["repo"] = serde_json::json!(repo);
    }
    // The tiling answer, so the Python readers (which cannot re-run the git
    // walk the Rust side ran) see the same chain: which ranges covered, which
    // dropped, and the uncovered stretches named by sha. A refusal that says
    // only "0 reviewed" cannot tell a never-reviewed PR from a one-gap chain.
    if let Some(t) = tiling {
        let gaps: Vec<String> = t.gaps.iter().map(|(a, b)| format!("{a}..{b}")).collect();
        data["range_tiling"] = serde_json::json!({
            "tiled": t.tiled,
            "gaps": gaps,
            "dropped": t.dropped,
            "chain_heads": t.chain_heads,
            "carried": t.carried.iter().map(|(head, freshness)| {
                serde_json::json!({ "head": head, "freshness": freshness })
            }).collect::<Vec<_>>(),
        });
        // The round budget, same chain, same scoping. Emitted beside the
        // tiling (not inside it) because it is a property of the review
        // loop, not of the ranges: a chain can tile perfectly and still have
        // burned its rounds. Advisory: every gate surface re-derives before
        // refusing or blocking (Locked Decision 6: never trust a producer
        // count), so a stored flag here is an audit trail, never an input.
        data["rounds_used"] = serde_json::json!(t.rounds_used);
        // The budget beside the count, so the row is the one producer of the
        // pair: `fno do pr status` reads both from here instead of re-reading
        // config.
        data["rounds_max"] = serde_json::json!(t.rounds_max);
        data["rounds_exhausted"] = serde_json::json!(t.rounds_exhausted);
    }
    if let Some(p) = posture {
        data["review_posture"] = serde_json::json!({
            "posture": p.posture,
            "rank": p.rank,
            "source": p.source,
            "cost": p.cost,
            "freshness": p.freshness,
            "diversity": p.diversity,
            "posture_satisfied": p.posture_satisfied,
            "posture_gaps": p.posture_gaps,
        });
    }
    data
}
