//! What does the PR look like: its state, its CI conclusion, and the one read that fills them.

use super::*;

/// PR state vocabulary (fu-4faa3d). Parsed once at the read_pr_info boundary.
/// `as_str()` reproduces the exact legacy strings so the fingerprint (which
/// persists across fires in events.jsonl) stays byte-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum PrState {
    Open,
    Merged,
    Closed,
    /// No PR, or an unrecognized gh state string (fail-closed, AC5-EDGE).
    #[default]
    None,
}

impl PrState {
    pub(super) fn from_gh_str(s: &str) -> Self {
        match s {
            "OPEN" => PrState::Open,
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ => PrState::None,
        }
    }

    pub(super) fn as_str(&self) -> &'static str {
        match self {
            PrState::Open => "OPEN",
            PrState::Merged => "MERGED",
            PrState::Closed => "CLOSED",
            PrState::None => "none",
        }
    }

    pub(super) fn is_open_or_merged(&self) -> bool {
        matches!(self, PrState::Open | PrState::Merged)
    }
}

/// CI conclusion vocabulary (fu-4faa3d). `render()` reproduces the exact
/// legacy strings ("FAILURE:{name}" carries the failing check name).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum CiConclusion {
    Success,
    /// Failing check name when one was identified.
    Failure(Option<String>),
    Pending,
    /// CI read skipped via ci.declared_none.
    Skipped,
    /// No checks found (fail-closed unless declared_none).
    #[default]
    None,
}

impl CiConclusion {
    pub(super) fn render(&self) -> String {
        match self {
            CiConclusion::Success => "SUCCESS".to_string(),
            CiConclusion::Failure(Some(name)) => format!("FAILURE:{name}"),
            CiConclusion::Failure(None) => "FAILURE".to_string(),
            CiConclusion::Pending => "PENDING".to_string(),
            CiConclusion::Skipped => "skipped".to_string(),
            CiConclusion::None => "none".to_string(),
        }
    }

    pub(super) fn is_ok(&self) -> bool {
        matches!(self, CiConclusion::Success | CiConclusion::Skipped)
    }
}

#[derive(Debug, Default)]
pub(super) struct PrInfo {
    pub(super) state: PrState,
    pub(super) number: i64,
    /// PR head commit OID; must match local HEAD for DonePRGreen (codex P1
    /// on #447: a green PR must not complete a session with unpushed work).
    pub(super) head_oid: String,
    pub(super) ci_conclusion: CiConclusion,
    /// Every failing check/job name on the PR head (bucket fail|cancel), at the
    /// same granularity as `gh pr checks .name`. Feeds the DoneAwaitingMerge
    /// subset rule against main's failing set. Empty when CI is green/pending.
    pub(super) failing_checks: Vec<String>,
    /// True iff any check on the PR head is still pending (a non-terminal
    /// bucket). `ci_conclusion` reports `Failure` as soon as ONE check fails even
    /// while others run, so the DoneAwaitingMerge terminal must consult this to
    /// avoid firing while the session's own in-flight job could still turn red.
    pub(super) ci_has_pending: bool,
    /// GitHub mergeable state ("MERGEABLE" | "CONFLICTING" | "UNKNOWN"). The
    /// DoneAwaitingMerge terminal must not fire on a "CONFLICTING" PR: the human
    /// cannot merge past main-red until the branch is rebased, and the terminal
    /// would drop the node from retry circulation while it is un-mergeable.
    pub(super) mergeable: String,
    /// Live merge-slot holder for this PR's base ref when ANOTHER PR holds it.
    /// A fail-open read of the local claims store - no GitHub spend.
    /// None on no hold, a self-held slot, or an unreadable store.
    pub(super) merge_slot_holder: Option<u64>,
    /// GitHub `mergeStateStatus` == BEHIND (REST `mergeable_state` == behind):
    /// the base moved past this PR's head, so a rebase is work to do now and a
    /// merge-slot hold must not idle: the refusal stays for a hold the
    /// session can act on. Absent on either payload reads as false.
    pub(super) base_behind: bool,
    /// Newest review/comment/inline-comment activity (ISO8601 or "none");
    /// folded into the fingerprint's 4th component on done() fires.
    pub(super) latest_review_ts: String,
    pub(super) reviewed: bool, // every required bot passed AND no unaddressed blocking finding
    /// Required bots with no completed review pass (names the gap in the
    /// block message, AC1-UI).
    pub(super) missing_bots: Vec<String>,
    /// Per-missing-bot nudge classification for this fire, same order
    /// as `missing_bots`. Empty when the review reads were skipped or there is no
    /// PR. `missing_bots` stays the gate; this only changes idling and messaging.
    /// An EMPTY list with a non-empty `missing_bots` means "not classified" and
    /// is treated exactly like today (every missing bot idlable, today's string).
    pub(super) bot_nudges: Vec<BotNudge>,
    /// Required bots whose best evidence READ AN OLDER COMMIT
    /// (`CoverageVerdict::Stale`): (login, reviewed sha). Same gate weight as
    /// `missing_bots` - a stale bot still fails `all_required_passed` - but a
    /// different remedy, so the block message names the sha it read and asks
    /// for a re-read instead of a first read. Nudge-classified
    /// alongside the missing bots so the re-read ask idles like one.
    pub(super) stale_bots: Vec<(String, String)>,
    /// Blocking inline findings (codex P1 / gemini critical|high) whose
    /// thread has no qualifying ack (AC2).
    pub(super) unaddressed_findings: Vec<Finding>,
    /// Reads 3+4 were skipped (per-session no_external OR the repo declared
    /// `required_bots: []`). Recorded in loop_check events so the skip is
    /// observable, not silently absent (AC3-UI).
    pub(super) review_skipped: bool,
    /// Configured `config.review.reviewers` with no head-pinned attestation.
    /// The sole failing term whenever the login gate is vacuous, and the reason
    /// the block message can name real local work instead of an absent bot.
    pub(super) unattested_reviewers: Vec<UnattestedReviewer>,
    /// Unparseable events.jsonl lines that carry the literal
    /// `review_attestation`. Named in the reason so a corrupt attestation is
    /// not silently dropped. Not exhaustive by construction: a write torn
    /// before that token cannot be recognized at all.
    pub(super) malformed_attestations: usize,
    /// Review coverage: did anyone actually review, distinct from `reviewed`
    /// (did anyone object). Computed at read time from observed evidence
    /// across two producer axes (github_app review objects; local_attestation
    /// head-pinned passes). Terminal selection consumes this: a run that would
    /// report `DonePRGreen` at coverage 0/Unknown reports `DoneUnreviewed`
    /// instead. Never cached, never inferred from `reviewed`.
    pub(super) coverage: CoverageReport,
    /// The resolved review-posture verdict , computed alongside
    /// coverage when the caller supplied a resolved `review.posture`. None on
    /// the no-PR early return and on callers without settings context.
    pub(super) posture: Option<PostureVerdict>,
    /// The attestation-chain range tiling computed for this read, carried so
    /// the standalone verb's stdout payload equals the row read_pr_info
    /// emitted, field for field (payload parity). Default on every early
    /// return and test fixture: no chain, no rescue.
    pub(super) range_tiling: RangeTiling,
}

/// Run done() reads. Returns Ok(PrInfo) or Err((read_name, stderr_tail)) on gh failure.
#[allow(clippy::too_many_arguments)]
pub(super) fn read_pr_info(
    gh_bin: &str,
    git_bin: &str,
    cwd: &Path,
    ci_declared_none: bool,
    no_external: bool,
    required_bots: &[String],
    optional_bots: &[String],
    optional_lane_configured: bool,
    external_reviewers: &[String],
    reviewers: &[String],
    nudge_configs: &[NudgeConfig],
    head_sha: &str,
    events_path: &Path,
    global_events_path: &Path,
    repo_slug: &str,
    author_session: Option<&str>,
    pr_selector: Option<&str>,
    prefetched_pr_json: Option<Value>,
    github_approval_satisfies: bool,
    max_rounds: i64,
    // The resolved `review.carry_interdiff_lines` (law d-608344c1): how many
    // interdiff lines a rebase or fix may add and still carry an attestation.
    // `0` disables the arm. Resolved by the caller from settings, default 100.
    carry_interdiff_lines: usize,
    // The resolved `review.posture` , computed by the caller from the
    // parsed settings. None on callers that have no settings context.
    posture: Option<&PostureConfig>,
    // The local reviewer names allowed to satisfy the peer posture component
    // (the cross-model-resolved set; the same-model sentinel never matches).
    peer_reviewers: &[String],
) -> Result<PrInfo, GhReadError> {
    let rest_adapter = internal_gh_adapter(gh_bin);
    let checks_read = if rest_adapter {
        "pr_status_rest"
    } else {
        "pr_checks"
    };
    let checks_parse = if rest_adapter {
        "pr_status_rest_parse"
    } else {
        "pr_checks_parse"
    };
    // An explicit PR selector for the branch-resolved gh calls:
    // Some(n) inserts the number (`gh pr view <n>`, `gh pr checks <n>`) so the
    // standalone review-coverage verb can evaluate a PR from a checkout that is
    // NOT on its branch (`fno do pr merge <n>` from canonical); None keeps the
    // argv byte-identical to the stop hook's branch-resolved form. The one
    // number-based call (`gh api .../pulls/<n>/comments`) already carries the
    // number the first read returned.
    let sel: Vec<&str> = pr_selector.into_iter().collect();
    // Read 1: PR state + number + head OID + mergeability. Reuse the caller's
    // read when it already resolved this exact selector (e.g. review-coverage
    // pinning --pr N via read_pr_head_oid) instead of asking gh again.
    let Some(pr_json) = (match prefetched_pr_json {
        Some(json) => Some(json),
        None => read_pr_view(gh_bin, cwd, pr_selector)?,
    }) else {
        // No PR yet: world-state, not an error. done() is simply false, and
        // the backstop can resolve a stuck no-PR session as NoProgress. Every
        // omitted field is PrInfo's no-information default.
        return Ok(PrInfo {
            mergeable: "UNKNOWN".to_string(),
            ..PrInfo::default()
        });
    };

    let state = PrState::from_gh_str(
        pr_json
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("none"),
    );
    let number = pr_json.get("number").and_then(|v| v.as_i64()).unwrap_or(0);
    let head_oid = pr_head_oid(&pr_json).unwrap_or_default();
    // The PR's head branch, same `gh pr view` round trip as headRefOid. The
    // scope predicate needs it: attestations are keyed to the branch they
    // reviewed, and an empty read must pass "" so the predicate fails closed
    // onto exact head equality.
    let head_branch = pr_json
        .get("headRefName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // The PR author's login, for the human-approval counting rule: an
    // approval counts only when its login provably is not this one. An
    // absent/unreadable author is None, which the rule reads fail-closed
    // (exclude the approval from the count, never include it on a guess).
    let pr_author = pr_json
        .pointer("/author/login")
        .and_then(|v| v.as_str())
        .filter(|a| !a.is_empty())
        .map(str::to_string);
    // GitHub's mergeable state: "MERGEABLE" | "CONFLICTING" | "UNKNOWN" (still
    // computing). Only "CONFLICTING" is a definitive no; UNKNOWN must not hold
    // the terminal (it clears on its own). Missing field -> "UNKNOWN".
    let mergeable = pr_json
        .get("mergeable")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    // BEHIND on either payload spelling (GraphQL `mergeStateStatus`, REST
    // `mergeable_state`): the base moved past this head, so a rebase is work
    // to do now. Absent on both reads as false (fail-open to idlable).
    let base_behind = ["mergeStateStatus", "mergeable_state"].iter().any(|k| {
        pr_json
            .get(*k)
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("behind"))
            .unwrap_or(false)
    });

    // One freshness resolver for every reviewer on this PR.
    // Both producers and both presence scans read it, so there is one rule
    // rather than the two divergent ones this replaces. Memoized per reviewed
    // sha, and the HEAD identity is computed lazily, so a PR whose reviewers
    // are all at HEAD (the common case) pays no git at all.
    let base_ref = pr_json
        .get("baseRefName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let resolver = FreshnessResolver::new(git_bin, cwd, base_ref, head_sha, carry_interdiff_lines);
    // Fetch a head the store cannot read before anything judges it: a
    // server-side rebase publishes the new head on GitHub before the next
    // local fetch, and an absent head reads `None`, then stale, then a stored
    // uncovered row that never recomputes. Memoized, so at most one fetch.
    resolver.ensure_local(head_sha);
    let freshness = |sha: &str| resolver.freshness(sha);

    // The range-tiling answer for this PR's attestation chain, computed ONCE
    // and shared by every consumer below (the classify_coverage local axis,
    // the emitted review_coverage row). The local attestation axis reads the
    // project rotations plus the global journal's slug-scoped mirrors; any
    // git failure answers not-tiled and today's rule stands alone.
    let events_text = review_journal_text(events_path, global_events_path, repo_slug);
    let mut tiling = compute_range_tiling(
        git_bin,
        cwd,
        base_ref,
        &events_text,
        &head_branch,
        head_sha,
        max_rounds,
        // The same resolver the per-verdict axis built above, same base_ref
        // and head_sha: the carry costs no git call the per-verdict axis was
        // not already making.
        Some(&resolver),
    );

    // (E): a MERGED PR is terminal. A PR merged out-of-band (GitHub
    // web/mobile, or `gh pr merge`) is done regardless of whether the required
    // bot ever reviewed it or whether CI is still green post-merge - the merge
    // IS the authority. Short-circuit the now-irrelevant CI + review polls
    // (which also avoids a transient gh blip on those reads re-blocking a
    // finished session). The single merge signal is `state` from the same
    // `gh pr view` call that `reconcile`/`fno do pr verify` read - one signal, not
    // two independently-polled sources. done()'s `head_shipped` guard still
    // applies downstream: an unpushed commit on top of a merged PR stays
    // unshipped work.
    if state == PrState::Merged {
        return Ok(PrInfo {
            state,
            number,
            head_oid,
            ci_conclusion: CiConclusion::Skipped,
            mergeable,
            reviewed: true,
            review_skipped: true,
            ..PrInfo::default()
        });
    }

    // Read 2: CI checks. Compute the conclusion, the full failing-check-name set,
    // AND whether any check is still pending from the same payload (the set feeds
    // the DoneAwaitingMerge subset rule; the pending flag gates that terminal so
    // it never fires on partial CI).
    let no_hosted_ci =
        crate::verify_evidence::hosted_ci_not_configured(ci_declared_none, cwd, head_sha);
    let (ci_conclusion, failing_checks, ci_has_pending) = if no_hosted_ci {
        (CiConclusion::Skipped, Vec::new(), false)
    } else {
        let mut checks_args = vec!["pr", "checks"];
        checks_args.extend(sel.iter().copied());
        checks_args.extend(["--json", "name,state,bucket,startedAt,workflow"]);
        let checks_out = bounded_read(
            gh_bin.as_ref(),
            &checks_args,
            cwd,
            checks_read,
            stopgate_read_timeout(),
        )?;

        if !checks_out.status.success() {
            return Err(GhReadError::failed(
                checks_read,
                stderr_tail(&checks_out.stderr_tail),
            ));
        }

        let checks: Value = serde_json::from_slice(&checks_out.stdout)
            .map_err(|_| GhReadError::parse_failed(checks_parse))?;
        // One truth table for the payload (classify_checks_payload): the
        // conclusion, the failing-name set, and the pending flag can never
        // answer off different rollups.
        classify_checks_payload(&checks).map_err(|e| GhReadError::parse_failed(&e))?
    };

    // Reads 3+4: reviews + inline findings. Skipped when the session declares
    // no_external OR the repo declares `required_bots: []` (the no-review-gate
    // path, US3 - mirrors ci.declared_none; PR + CI carry the gate). The two
    // skips are orthogonal: one is per-session, the other repo config.
    // Skip the review reads only when there is NOTHING to honor: no required
    // login AND no optional login. An optional-only gate still reads (to catch
    // an optional blocking finding), but its presence is never required.
    //: the gate is a strict conjunction over the union of GitHub-login
    // evidence (github_apps/peers via optional_bots+required_bots) AND the
    // local-attestation `reviewers`. Each satisfied by its own evidence source,
    // so the two skips are INDEPENDENT: `no_external` (and an empty login set)
    // skips only the EXTERNAL GitHub-login reads - it is scoped to external
    // review (control-plane-loop.md step 2), NOT the local attestation gate. A
    // repo that pins `reviewers: [sigma]` still requires that local pass even
    // when a session runs `--no-external` to skip usage-wedged App bots
    // (fixes a fail-open the sigma review caught). `reviewers` is empty for
    // every pre- config, so `reviewers_all_attested` is vacuously true
    // there and this changes nothing for them.
    let login_gate_active = !required_bots.is_empty() || optional_lane_configured;
    let login_skipped = no_external || !login_gate_active;
    // One scan feeds both the gate and its explanation, so the two cannot
    // disagree the way the decision and the message did on PR #618.
    let (unattested, malformed_attestations) = unattested_reviewers_scan_text(
        &events_text,
        reviewers,
        &freshness,
        &head_branch,
        head_sha,
        tiling.rounds_exhausted,
    );
    let reviewers_ok = unattested.is_empty();
    // Coverage reads the same merged journal text as the attestation scan
    // (its local axis) plus the GitHub review arrays (its github_app axis).
    // Authorship carry-forward: when this process resolved no manifest
    // session, the previous coverage row's recorded author FOR THIS PR (the
    // scan filters on the number; the events file is project-wide) stands
    // in, so a re-read from a manifest-less cwd classifies against the
    // historical author instead of landing every local verdict Unmeasured.
    let carried_author = author_session
        .map(str::to_string)
        .or_else(|| carry_author_session_forward(&events_text, number));
    let author_session = carried_author.as_deref();
    let (
        latest_review_ts,
        reviewed,
        missing_bots,
        bot_nudges,
        stale_bots,
        unaddressed_findings,
        coverage,
        hard_finding_present,
    ) = if login_skipped {
        // No GitHub logins to poll (nothing configured, or no_external): skip
        // the gh review reads entirely (fewer calls + no spurious gh-error
        // block). The local attestation gate still applies - reviewers_ok is
        // true when unconfigured, so a login-only or no-gate config is
        // unaffected. Coverage's github axis is empty here (no logins read),
        // so coverage is the local axis alone - which is exactly how a
        // worker-run /code-review counts even on a no-required-bots config.
        // The GitHub axis was intentionally not queried. That is a known
        // zero ONLY when nothing was configured to read: the skip is the
        // inactive gate, with or without no_external. A `no_external` session
        // on a repo with an ACTIVE login gate suppressed reads the config
        // demanded, so the honest answer for that axis is Unknown with its
        // retry remedy - reporting a healthy read of zero bots fabricated
        // "uncovered" and an instrument-health receipt for reviews that were
        // never queried. A fresh local pass still rescues it inside
        // classify_coverage (positive evidence).
        // One classification pass over a tiling, as (coverage, blockers): the
        // round-budget refresh below re-runs it so every conjunct downstream
        // reads the SAME budget, never a mix.
        let classify_with = |tiling: &RangeTiling| {
            let mut coverage = classify_coverage_tiled(
                &[],
                &[],
                &events_text,
                &[],
                !(no_external && login_gate_active),
                author_session,
                &freshness,
                &head_branch,
                head_sha,
                Some(tiling),
                pr_author.as_deref(),
                github_approval_satisfies,
            );
            demote_unmeasured_coverage(&mut coverage.coverage, &resolver);
            // Locked Decision 1: the pass condition is disposition-complete.
            // Non-terminal blocking findings withhold `reviewed` here exactly as
            // the Python merge gate refuses on them - below the cap only.
            let blockers = disposition_blockers(&events_text, &head_branch, head_sha);
            (coverage, blockers)
        };
        let (mut coverage, mut blockers) = classify_with(&tiling);
        // The reviewer scan above read the pre-refresh budget too; a local
        // mut so the arm's tuple below answers from whatever budget this arm
        // ends on.
        let mut reviewers_ok = reviewers_ok;
        //: the round budget counts the GitHub reviews axis on THIS arm
        // too. A stock install (no required bots, no optional lane) never
        // reached the external arm's refresh, so rounds the connector posted
        // read 0 here and the cap could not fire on exactly the lane that
        // spun (PR #1225: five real rounds, counter 0/2). The read mirrors
        // the Python merge gate's own gating (_coverage_gate._pr_reviews):
        // pay it only where it can change the answer - the row is uncovered,
        // or findings are in play - never on a healthy covered PR. A
        // no_external session reads nothing, whatever the gate says. A
        // failed read keeps the events-only answer rather than guessing: a
        // cap that fires on a broken read spends a budget that may be
        // unspent.
        if !no_external && (!coverage.coverage.is_covered() || !blockers.is_empty()) {
            let mut reviews_args = vec!["pr", "view"];
            reviews_args.extend(sel.iter().copied());
            reviews_args.extend(["--json", "reviews"]);
            match bounded_read(
                gh_bin.as_ref(),
                &reviews_args,
                cwd,
                "pr_reviews",
                stopgate_read_timeout(),
            ) {
                Ok(out) if out.status.success() => {
                    let parsed = serde_json::from_slice::<Value>(&out.stdout)
                        .map_err(|_| GhReadError::parse_failed("pr_reviews_parse"));
                    match parsed {
                        Ok(reviews_json) => {
                            let reviews_arr =
                                reviews_json.get("reviews").and_then(|v| v.as_array());
                            if let Some(reviews_arr) = reviews_arr {
                                tiling.rounds_used = rounds_since_last_pass(
                                    &events_text,
                                    &head_branch,
                                    head_sha,
                                    Some(reviews_arr),
                                );
                                tiling.rounds_exhausted = tiling.rounds_used >= max_rounds.max(1);
                                tiling.rounds_max = max_rounds;
                                // The classification and the reviewer scan
                                // above judged the pre-refresh budget; re-run
                                // both so the in-classify spent-budget
                                // discharge, the withhold conjunct, and the
                                // unattested reviewer's cap-yield all read the
                                // refreshed one.
                                let (re_coverage, re_blockers) = classify_with(&tiling);
                                coverage = re_coverage;
                                blockers = re_blockers;
                                let (re_unattested, _re_malformed) = unattested_reviewers_scan_text(
                                    &events_text,
                                    reviewers,
                                    &freshness,
                                    &head_branch,
                                    head_sha,
                                    tiling.rounds_exhausted,
                                );
                                reviewers_ok = re_unattested.is_empty();
                            }
                        }
                        Err(e) => log_bounded_read_error("round-budget reviews read", &e),
                    }
                }
                Ok(out) => log_bounded_read_error(
                    "round-budget reviews read",
                    &GhReadError::failed("pr_reviews", stderr_tail(&out.stderr_tail)),
                ),
                Err(e) => log_bounded_read_error("round-budget reviews read", &e),
            }
        }
        (
            "none".to_string(),
            reviewers_ok && !blockers_withhold(&blockers, tiling.rounds_exhausted),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            coverage,
            blockers.iter().any(|b| b.hard),
        )
    } else {
        // Read 3: top-level reviews + issue comments
        let mut reviews_args = vec!["pr", "view"];
        reviews_args.extend(sel.iter().copied());
        reviews_args.extend(["--json", "reviews,comments"]);
        let reviews_out = bounded_read(
            gh_bin.as_ref(),
            &reviews_args,
            cwd,
            "pr_reviews",
            stopgate_read_timeout(),
        )?;

        if !reviews_out.status.success() {
            return Err(GhReadError::failed(
                "pr_reviews",
                stderr_tail(&reviews_out.stderr_tail),
            ));
        }

        let reviews_json: Value = serde_json::from_slice(&reviews_out.stdout)
            .map_err(|_| GhReadError::parse_failed("pr_reviews_parse"))?;

        // PRESENCE is required-only: an optional login's absence must never
        // create a missing_bot (never wait for it), and its STALENESS must
        // never reach stale_bots either - an optional bot reading an older
        // commit is not a property any reviewer owes, so it may neither block
        // nor disqualify local-review recovery. FINDINGS honor the union: an
        // optional login's blocking P1 still holds the gate ("honor if
        // present"). A dedup keeps a login that is in both lists counted once.
        let info = compute_review_info(&reviews_json, required_bots, &freshness);
        // Per-outstanding-bot nudge classification, computed AFTER the
        // usage-limit retain (which happened inside compute_review_info) so
        // the two give-up paths never compose (AC6): a usage_limited bot is
        // already out of missing_bots and is never classified here. A STALE
        // bot is classified alongside a missing one: its remedy is
        // the same trigger comment, and without classification the session
        // could neither tell whether it had already asked nor idle while
        // waiting for the re-read. Derived from the same issue-comment list,
        // fresh every fire.
        let now = Utc::now();
        let review_comments = reviews_json
            .get("comments")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let classify = |bot: &String| {
            classify_bot_nudge(
                bot,
                review_comments,
                nudge_config_for(nudge_configs, bot),
                now,
            )
        };
        let bot_nudges: Vec<BotNudge> = info
            .missing_bots
            .iter()
            .chain(info.stale_bots.iter().map(|(bot, _)| bot))
            .map(classify)
            .collect();
        // The "empty bot_nudges = not classified = status quo" contract that
        // async_wait_class and build_block_reason rely on holds only because
        // this is an all-or-nothing map: bot_nudges is either empty or 1:1
        // with missing_bots + stale_bots. A future partial classification
        // would silently mis-idle, so pin the invariant here rather than let
        // it drift.
        debug_assert_eq!(
            bot_nudges.len(),
            info.missing_bots.len() + info.stale_bots.len()
        );
        let mut findings_bots: Vec<String> = required_bots.to_vec();
        for b in optional_bots {
            if !findings_bots.iter().any(|x| x == b) {
                findings_bots.push(b.clone());
            }
        }

        // Read 4: inline review comments (NEW in step 2). Codex's P1s land on
        // the /pulls/N/comments REST endpoint, which `gh pr view --json
        // comments` does NOT return (verified on PR #447). --paginate may
        // emit CONCATENATED JSON arrays (one per page), so parse as a stream.
        let pulls_target = format!("repos/{{owner}}/{{repo}}/pulls/{number}/comments");
        let comments_out = bounded_read(
            gh_bin.as_ref(),
            &["api", &pulls_target, "--paginate"],
            cwd,
            "pulls_comments",
            stopgate_read_timeout(),
        )?;

        if !comments_out.status.success() {
            return Err(GhReadError::failed(
                "pulls_comments",
                stderr_tail(&comments_out.stderr_tail),
            ));
        }

        let mut inline_comments: Vec<Value> = Vec::new();
        for page in serde_json::Deserializer::from_slice(&comments_out.stdout).into_iter::<Value>()
        {
            let page = page.map_err(|_| GhReadError::parse_failed("pulls_comments_parse"))?;
            match page.as_array() {
                Some(arr) => inline_comments.extend(arr.iter().cloned()),
                None => return Err(GhReadError::parse_failed("pulls_comments_parse")),
            }
        }

        // Commit timestamps feed the commit-after arm of "addressed". Only
        // fetched when a blocking candidate could exist (cheap pre-scan).
        let has_blocking_candidate = inline_comments.iter().any(|c| {
            c.get("in_reply_to_id").and_then(|v| v.as_i64()).is_none()
                && blocking_severity(c.get("body").and_then(|v| v.as_str()).unwrap_or("")).is_some()
        });
        let commit_dates: Vec<String> = if has_blocking_candidate {
            let mut commits_args = vec!["pr", "view"];
            commits_args.extend(sel.iter().copied());
            commits_args.extend(["--json", "commits"]);
            let commits_out = bounded_read(
                gh_bin.as_ref(),
                &commits_args,
                cwd,
                "pr_commits",
                stopgate_read_timeout(),
            )?;
            if !commits_out.status.success() {
                return Err(GhReadError::failed(
                    "pr_commits",
                    stderr_tail(&commits_out.stderr_tail),
                ));
            }
            let commits_json: Value = serde_json::from_slice(&commits_out.stdout)
                .map_err(|_| GhReadError::parse_failed("pr_commits_parse"))?;
            commits_json
                .get("commits")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| {
                            c.get("committedDate")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let (inline_ts, unaddressed) = compute_unaddressed_findings(
            &inline_comments,
            &commit_dates,
            &findings_bots,
            external_reviewers,
        );

        // Read 4's newest comment timestamp joins the activity timestamp so
        // inline-only review traffic advances the fingerprint (closes the
        // false-NoProgress hole).
        let activity_ts = max_ts(&info.latest_ts, &inline_ts);
        //: the login gate AND the local-attestation reviewers gate must
        // both clear. reviewers is usually empty (vacuously true) so this is a
        // no-op for login-only configs.
        // (a) Record the rate-limit drop so a post-hoc audit sees why the gate
        // proceeded without a required bot (AC1-UI). append_loop_event, not
        // Branch-B emit: these are target-stream events (see the doc comment on
        // append_loop_event), deliberately unregistered in KNOWN_EVENT_KINDS.
        let usage_limited: Vec<String> = info
            .reviewer_refused
            .iter()
            .filter(|bot| {
                review_comments
                    .iter()
                    .any(|comment| usage_limit_comment_by(bot, comment))
            })
            .cloned()
            .collect();
        if !usage_limited.is_empty() {
            append_loop_event(
                events_path,
                "review_gate_bot_usage_limited",
                serde_json::json!({"pr": number, "bots": usage_limited}),
            );
        }
        // Coverage's github_app axis: configured required + optional logins
        // (external_reviewers are local-attestation peers, not github
        // posters). Dedup so a login in both lists is one verdict.
        let reviews_arr: &[Value] = reviews_json
            .get("reviews")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        // The reviews are in hand, so the round budget now counts BOTH
        // evidence axes: the attestations above and the review objects
        // here. A GitHub-App reviewer's rounds leave no attestation row
        // anywhere, so without this refresh a connector-driven loop reads
        // zero rounds and the cap cannot fire. The refreshed values feed
        // every consumer below in this arm (the coverage classify, the
        // withhold/impossible conjuncts, the emitted row). The no-external
        // arm above pays for the same axis only where it can change the
        // answer (uncovered row, findings in play) - never on a healthy
        // covered PR.
        tiling.rounds_used =
            rounds_since_last_pass(&events_text, &head_branch, head_sha, Some(reviews_arr));
        tiling.rounds_exhausted = tiling.rounds_used >= max_rounds.max(1);
        tiling.rounds_max = max_rounds;
        let comments_arr: &[Value] = reviews_json
            .get("comments")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let mut gh_logins: Vec<String> = required_bots.to_vec();
        for b in optional_bots {
            if !gh_logins.iter().any(|x| x == b) {
                gh_logins.push(b.clone());
            }
        }
        // github_read_ok is true here: a failed gh read returned Err above.
        let mut coverage = classify_coverage_tiled(
            reviews_arr,
            comments_arr,
            &events_text,
            &gh_logins,
            true,
            author_session,
            &freshness,
            &head_branch,
            head_sha,
            Some(&tiling),
            pr_author.as_deref(),
            github_approval_satisfies,
        );
        demote_unmeasured_coverage(&mut coverage.coverage, &resolver);
        mark_owed_verdicts(&mut coverage, required_bots);
        let local_recovery = local_recovery_from_refusal(
            &info.reviewer_refused,
            &info.missing_bots,
            &info.stale_bots,
            &coverage,
        );
        // Locked Decision 1, same conjunct as the solo lane arm: the pass
        // condition is disposition-complete, withheld below the cap only.
        let blockers = disposition_blockers(&events_text, &head_branch, head_sha);
        let reviewed = (info.all_required_passed() || local_recovery || tiling.rounds_exhausted)
            && unaddressed.is_empty()
            && reviewers_ok
            && !blockers_withhold(&blockers, tiling.rounds_exhausted);
        (
            activity_ts,
            reviewed,
            info.missing_bots,
            bot_nudges,
            info.stale_bots,
            unaddressed,
            coverage,
            blockers.iter().any(|b| b.hard),
        )
    };
    // The hard axis rides the row: the standing operator-law waiver's
    // condition is hard findings alone, independent of the budget, and it
    // only ever consults this below the cap - at the cap the configured
    // rounds discharge every open finding.
    let mut tiling = tiling;
    tiling.hard_blocker = hard_finding_present;

    // Emit coverage every gate eval so the Python readers (the merge primitive
    // and the polling command) and audit see one coherent, fresh number rather
    // than recomputing it (the Ownership rule: loopcheck computes, Python
    // reads). Skipped for the no-PR early returns above.
    //
    // BOTH logs, like every other loop-check event. This one used to write only
    // the project log, and since the stop hook runs wherever the session runs,
    // that put the attestation in `<worktree>/.fno/events.jsonl` while a merge
    // run from canonical read `<canonical>/.fno/events.jsonl` - a satisfied
    // gate reading as an unsatisfiable one, silently, with a refusal that
    // named a count and not a location. The global log is the one file both
    // stand in; `repo` in the payload keeps it scoped.
    // The posture verdict rides the same emit: coverage computed the verdicts,
    // so satisfaction against the resolved rung is one predicate here rather
    // than a reclassification on the Python side (AC6-HP).
    let posture_v = posture.map(|pc| posture_verdict(pc, &coverage, peer_reviewers));
    if number > 0 {
        emit_to_both(
            events_path,
            global_events_path,
            "review_coverage",
            coverage_event_data_full(
                number,
                &coverage,
                head_sha,
                repo_slug,
                author_session,
                Some(&tiling),
                posture_v.as_ref(),
            ),
        );
    }

    // The merge-slot hold: a local claims read keyed to this PR's
    // base ref, so idling on a slot held by another PR costs no GitHub spend.
    // A slot this PR holds itself is not a hold on THIS session. The read is
    // gated on the classifier's cheap preconditions (green CI, review gate
    // satisfied): every consumer of this field sits behind both, so a red or
    // unreviewed PR pays no `git worktree list` subprocess for a value it
    // never reads.
    let merge_slot_holder = if ci_conclusion.is_ok() && reviewed {
        crate::authorized_merge::merge_slot_holder(cwd, base_ref)
            .filter(|m| *m != number.unsigned_abs())
    } else {
        None
    };
    Ok(PrInfo {
        range_tiling: tiling,
        state,
        number,
        head_oid,
        ci_conclusion,
        failing_checks,
        ci_has_pending,
        mergeable,
        merge_slot_holder,
        base_behind,
        latest_review_ts,
        reviewed,
        missing_bots,
        bot_nudges,
        stale_bots,
        unaddressed_findings,
        // Telemetry only (no decision reads this): "no review gate of any kind
        // applied" = the login reads were skipped AND no local reviewers gate.
        // A reviewers-only config did gate, so it is NOT review_skipped.
        review_skipped: login_skipped && reviewers.is_empty(),
        unattested_reviewers: unattested,
        malformed_attestations,
        posture: posture_v,
        coverage,
    })
}
