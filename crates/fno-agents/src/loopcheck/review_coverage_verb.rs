//! What does fno-agents review-coverage answer? The review-coverage verb, its usage text, and its quota diagnostics.

use super::*;

/// `fno-agents review-coverage --cwd <dir> [--pr <n>] [--head <sha>] ...`
///. The standalone review_coverage producer. The only writer of the
/// event used to be `read_pr_info` past a streak counter inside `decide()`,
/// so a session with no target manifest could never produce the row the
/// merge gate demands. This verb exposes the SAME computation through the
/// SAME resolver and emitter to every path that can reach the gate.
///
/// Read-only against GitHub, append-only against the two event logs. There
/// is no --force and no key that skips the guard: a caller wanting green
/// must cause a review to exist. Exit contract: 0 = row emitted, covered or
/// not, the number says which; 3 = no PR; 4 = gh read failed, the row is
/// `unknown` unless a pass attested at this exact head makes it known;
/// 2 = bad arguments. stdout is one JSON object.
pub fn run_review_coverage(args: &[String]) -> i32 {
    let (code, json) = decide_review_coverage(args);
    if code != 0 {
        if let Ok(payload) = serde_json::from_str::<Value>(&json) {
            if let Some(error) = payload.get("error").and_then(Value::as_str) {
                eprintln!("review-coverage: {error}");
            }
        }
    }
    println!("{json}");
    code
}

/// Test-friendly variant: (exit_code, json_string) without printing.
pub fn run_review_coverage_capture(args: &[String]) -> (i32, String) {
    decide_review_coverage(args)
}

pub(super) const REVIEW_COVERAGE_USAGE: &str = "\
usage: fno-agents review-coverage --cwd <dir> [--pr <n>] [--head <sha>] [--session-id <id>]
       [--events <p>] [--global-events <p>] [--settings <p>] [--global-settings <p>]
       [--gh-bin <p>] [--git-bin <p>] [--author-harness <h>]

Computes and emits the review_coverage event for a PR using the exact
resolver and emitter the stop hook uses (resolve_review_inputs +
read_pr_info), so any session that can open a PR can also satisfy the
gate that guards it. Read-only against GitHub, append-only against the
event logs.

There is no way to assert coverage without performing the reads: no
--force, no --assume-covered, no skip key. A caller wanting a green gate
must cause a review to exist.

Manifest-less defaults, both strict: external review reads are ON
(no_external=false - the manifest field can only relax them, so its
absence must not), and the author session is --session-id, else the
harness_session_id scanned from <cwd>/.fno/target-state.md, else none
(the payload then omits self_attested_count rather than report an
unmeasured 0).

Exits: 0 emitted a row; 3 no PR for the selector; 4 gh read failed
(emitted row is unknown unless a pass attested at this exact head makes
it known); 2 bad arguments.

On exit 4 stdout additionally carries graphql_remaining /
graphql_exhausted (plus a reason string when exhausted), so a degraded
read is distinguishable from a genuinely unreviewed one. The persisted
row keeps the bare unknown schema.";

/// Decorate an exit-4 payload with the stdout-only quota diagnostic. Both
/// exit-4 arms of `decide_review_coverage` carry the same keys so a reader
/// never needs to know which arm produced the row; the persisted event row
/// is schema-gated and must NOT grow them.
pub(super) fn insert_quota_diagnostic(out: &mut Value, quota: &Option<GraphqlQuota>) {
    // Index assignment, the file's idiom: a non-object payload panics loudly
    // instead of silently dropping the diagnostic.
    out["graphql_remaining"] = serde_json::json!(quota.as_ref().map(|q| q.remaining));
    out["graphql_exhausted"] = serde_json::json!(quota.as_ref().map(|q| q.remaining == 0));
    if let Some(q) = quota.as_ref().filter(|q| q.remaining == 0) {
        out["reason"] = serde_json::json!(graphql_exhausted_reason(q));
    }
}

/// One exempt quota probe plus its verdict for an exit-4 arm: the probe
/// answers during a refusal and counts against no bucket, so it is the
/// classifier, not a doomed extra call against the limiter.
pub(super) fn exit4_quota(
    gh_bin: &str,
    cwd: &Path,
    read: &str,
    tail: &str,
) -> (bool, Option<GraphqlQuota>) {
    let probed = if stderr_smells_rate_limit(tail) || is_graphql_read(read) {
        probe_graphql_quota(gh_bin, cwd)
    } else {
        None
    };
    let secondary = refusal_is_secondary(tail, probed.as_ref(), is_graphql_read(read));
    let quota = if secondary || !is_graphql_read(read) {
        None
    } else {
        probed
    };
    (secondary, quota)
}

/// The exit-4 reason for a secondary (burst) limit refusal. Shared by every
/// exit-4 arm so the stdout contract does not fork on whether `--pr` was
/// passed: the verdict came from the ONE exempt `gh api rate_limit` probe
/// (`refusal_is_secondary` - the endpoint answers during a refusal and counts
/// against no bucket), and `graphql_*` read null because the secondary
/// verdict keeps the quota diagnostics out rather than blame a bucket that
/// still reads healthy.
pub(super) fn secondary_limit_reason() -> &'static str {
    "GitHub secondary rate limit refused this gh read (a burst limit, distinct \
     from the hourly quota; advertised remaining stays healthy). Stop retrying \
     for a few minutes."
}

pub(super) fn decide_review_coverage(args: &[String]) -> (i32, String) {
    let args = if args.first().map(|s| s.as_str()) == Some("review-coverage") {
        &args[1..]
    } else {
        args
    };
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return (
            0,
            serde_json::json!({"usage": REVIEW_COVERAGE_USAGE}).to_string(),
        );
    }
    let mut cwd: Option<PathBuf> = None;
    let mut pr: Option<String> = None;
    let mut head: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut events_path: Option<PathBuf> = None;
    let mut global_events_path: Option<PathBuf> = None;
    let mut settings_path: Option<PathBuf> = None;
    let mut global_settings_path: Option<PathBuf> = None;
    let mut gh_bin =
        std::env::var("FNO_LOOPCHECK_GH_BIN").unwrap_or_else(|_| "fno-gh-coverage".to_string());
    let mut git_bin = std::env::var("FNO_LOOPCHECK_GIT_BIN").unwrap_or_else(|_| "git".to_string());
    let fno_bin = loopcheck_fno_bin();
    let mut author_harness_override: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        if let Some(val) = try_flag_value(&args[i], "--cwd", args, &mut i) {
            cwd = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--pr", args, &mut i) {
            pr = Some(val);
        } else if let Some(val) = try_flag_value(&args[i], "--head", args, &mut i) {
            head = Some(val);
        } else if let Some(val) = try_flag_value(&args[i], "--session-id", args, &mut i) {
            session_id = Some(val);
        } else if let Some(val) = try_flag_value(&args[i], "--events", args, &mut i) {
            events_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--global-events", args, &mut i) {
            global_events_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--settings", args, &mut i) {
            settings_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--global-settings", args, &mut i) {
            global_settings_path = Some(PathBuf::from(val));
        } else if let Some(val) = try_flag_value(&args[i], "--gh-bin", args, &mut i) {
            gh_bin = val;
        } else if let Some(val) = try_flag_value(&args[i], "--git-bin", args, &mut i) {
            git_bin = val;
        } else if let Some(val) = try_flag_value(&args[i], "--author-harness", args, &mut i) {
            author_harness_override = Some(val);
        } else if args[i].starts_with('-') {
            // Unknown flag (or one missing its value): silently ignoring it is
            // not leniency - a typo'd `--events` would leave events_path=None
            // and append to the REAL logs while the caller believed their
            // scratch path was used. Exit 2, as the usage text promises.
            return (
                2,
                serde_json::json!({"error": format!("unknown or valueless argument: {}", args[i])})
                    .to_string(),
            );
        }
        i += 1;
    }
    let cwd = match cwd {
        Some(c) => c,
        None => {
            return (
                2,
                serde_json::json!({"error": "--cwd is required"}).to_string(),
            );
        }
    };
    if let Some(explicit) = head.as_deref() {
        if !crate::verify_evidence::full_sha(explicit) {
            return (
                2,
                serde_json::json!({
                    "error": format!(
                        "--head value '{explicit}' must be a full 40-hex git sha"
                    )
                })
                .to_string(),
            );
        }
    }

    let inputs = resolve_review_inputs(
        &cwd,
        events_path.as_deref(),
        global_events_path.as_deref(),
        settings_path.as_deref(),
        global_settings_path.as_deref(),
        author_harness_override.as_deref(),
    );

    // Authorship: --session-id, else the manifest's harness_session_id
    // (authorship::resolve_manifest_author). The historical carry-forward
    // happens inside read_pr_info, where the PR number is known: the events
    // file is project-wide, so a carry computed before the number resolves
    // cannot be filtered and would hand this PR a foreign PR's author.
    let author_session = session_id.or_else(|| authorship::resolve_manifest_author(&cwd));

    // Head precedence is explicit --head, then the named PR's head, then the
    // local checkout for the branch-inference path. A failed named-PR read
    // must never fall through to local HEAD: that is how a canonical checkout
    // used to publish a row describing main against a different PR.
    // `branch` is read lazily, inside the closure, since it is only ever
    // needed on the no-PR error path - an explicit --head never calls it.
    let no_pr_payload = || {
        serde_json::json!({
            "coverage": "none",
            "emitted": false,
            "reason": "no PR for the selector",
            "selector": pr,
            "branch": git_head_branch(&git_bin, &cwd),
        })
        .to_string()
    };
    // Set when the (None, Some(selector)) branch below resolves the head via
    // a `gh pr view` read, so the read_pr_info call further down can reuse
    // that same response instead of issuing an identical second request.
    let mut prefetched_pr_json: Option<Value> = None;
    let (head_sha, head_explicit) = match (head, pr.as_deref()) {
        (Some(explicit), _) => (explicit, true),
        (None, Some(selector)) => match read_pr_head_oid(&gh_bin, &cwd, selector) {
            Ok(Some((resolved, pr_json))) => {
                prefetched_pr_json = Some(pr_json);
                (resolved, true)
            }
            Ok(None) => return (3, no_pr_payload()),
            Err(read_err) => {
                // A killed timeout reports its own shape before the unknown-row
                // machinery: no quota diagnosis for a child that never answered.
                if read_err.kind == ReadErrorKind::TimedOut {
                    let mut out = serde_json::json!({
                        "error": read_err.render(),
                        "outcome": read_err.outcome(),
                        "emitted": false,
                    });
                    insert_quota_diagnostic(&mut out, &None);
                    return (4, out.to_string());
                }
                let read = read_err.read.clone();
                let tail = read_err.stderr_tail.clone();
                let pr_num: i64 = pr.as_deref().and_then(|p| p.parse().ok()).unwrap_or(0);
                let data = coverage_event_data(
                    pr_num,
                    &CoverageReport {
                        github_approval_satisfies: false,
                        coverage: Coverage::Unknown,
                        verdicts: Vec::new(),
                    },
                    "",
                    &inputs.repo_slug,
                    author_session.as_deref(),
                );
                emit_to_both(
                    &inputs.project_events,
                    &inputs.global_events,
                    "review_coverage",
                    data.clone(),
                );
                let (secondary, quota) = exit4_quota(&gh_bin, &cwd, &read, &tail);
                let mut out = data;
                insert_quota_diagnostic(&mut out, &quota);
                if secondary {
                    out["reason"] = serde_json::json!(secondary_limit_reason());
                }
                return (4, out.to_string());
            }
        },
        (None, None) => (git_head_sha(&git_bin, &cwd), false),
    };

    // The self-review floor, exactly as decide() applies it for the stop
    // hook: a code payload on a lane-less stock install owes a local
    // code-review pass. Without the floor here the verb's publish computed
    // "no lane" and posted nothing on the install shape whose only publisher
    // is this verb (a session with no stop hook), and the emitted row
    // disagreed with the stop hook's floored row for the same PR.
    let mut required_reviewers = inputs.required_reviewers;
    let lane_configured = !(inputs.required_bots.is_empty()
        && !inputs.optional_lane_configured
        && required_reviewers.is_empty());
    if !lane_configured
        && inputs.settings.self_review_required.unwrap_or(true)
        && self_review_floor_applies(
            inputs.author_harness.as_deref(),
            inputs.author_harness_pinned_none,
        )
    {
        let payload = classify_payload_for_floor(&gh_bin, &git_bin, &cwd, pr.as_deref());
        if let Some(floored) = floor_self_review(&required_reviewers, false, payload.0, true) {
            required_reviewers.push(floored);
        }
    }

    match read_pr_info(
        &gh_bin,
        &git_bin,
        &cwd,
        inputs.settings.ci_declared_none,
        // no_external=false: the manifest field can only RELAX external review,
        // so its absence here must not.
        false,
        &inputs.required_bots,
        &inputs.optional_bots,
        inputs.optional_lane_configured,
        &inputs.settings.external_reviewers,
        &required_reviewers,
        &inputs.nudge_configs,
        &head_sha,
        &inputs.project_events,
        &inputs.global_events,
        &inputs.repo_slug,
        author_session.as_deref(),
        pr.as_deref(),
        prefetched_pr_json,
        inputs.settings.github_approval_satisfies.unwrap_or(true),
        inputs.settings.max_rounds.unwrap_or(2).max(1),
        carry_interdiff_lines_resolved(&inputs.settings),
        Some(&resolve_posture_config(&inputs.settings)),
        &resolved_local_peer_reviewers_for_author(
            &inputs.settings,
            inputs.author_harness.as_deref(),
        ),
    ) {
        Ok(pr_info) => {
            if pr_info.number == 0 {
                // PrState::None: no PR for the selector (or the branch). There
                // is nothing to cover and nothing was emitted.
                return (3, no_pr_payload());
            }
            // read_pr_info already emitted this exact payload to both logs;
            // print the same object so stdout and the logs agree. And publish
            // the same verdict as the commit status, so the standalone
            // verb satisfies the server-side gate for every session shape that
            // has no stop hook at all. Open PRs only: a MERGED PR carries the
            // Covered(0) sentinel, not evidence.
            if matches!(pr_info.state, PrState::Open) {
                publish_coverage_status(
                    &gh_bin,
                    &fno_bin,
                    &cwd,
                    &inputs.repo_slug,
                    pr_info.number,
                    &pr_info.head_oid,
                    &head_sha,
                    &pr_info.coverage,
                    &inputs.required_bots,
                    &inputs.optional_bots,
                    inputs.optional_lane_configured,
                    &required_reviewers,
                    pr_info.range_tiling.rounds_exhausted,
                    pr_info.range_tiling.hard_blocker,
                );
            }
            (
                0,
                coverage_event_data_full(
                    pr_info.number,
                    &pr_info.coverage,
                    &head_sha,
                    &inputs.repo_slug,
                    // The same author read_pr_info classified with: the
                    // measured session, else the PR-filtered carry-forward.
                    // Recomputed rather than threaded back so PrInfo keeps its
                    // shape; deterministic, and idempotent across the row
                    // read_pr_info just wrote (its own author_session_id now
                    // answers the scan).
                    author_session
                        .as_deref()
                        .map(str::to_string)
                        .or_else(|| {
                            let carried = crate::event_store::journal_text(
                                &inputs.project_events,
                                &["review_coverage"],
                            );
                            carry_author_session_forward(&carried, pr_info.number)
                        })
                        .as_deref(),
                    Some(&pr_info.range_tiling),
                    pr_info.posture.as_ref(),
                )
                .to_string(),
            )
        }
        Err(read_err) => {
            // A killed timeout gets its own stdout shape: the caller must see
            // what was killed and after how long, never a quota diagnosis for
            // a child that never answered.
            if read_err.kind == ReadErrorKind::TimedOut {
                let mut out = serde_json::json!({
                    "error": read_err.render(),
                    "outcome": read_err.outcome(),
                    "emitted": false,
                });
                insert_quota_diagnostic(&mut out, &None);
                return (4, out.to_string());
            }
            let read = read_err.read.clone();
            let tail = read_err.stderr_tail.clone();
            // The gh read failed. Emit an unknown row when the PR number is
            // known (--pr was passed - always true for the merge recompute) so
            // downstream readers see the failed read rather than nothing; with
            // no number the row cannot be attributed, so emit nothing.
            let pr_num: i64 = pr.as_deref().and_then(|p| p.parse().ok()).unwrap_or(0);
            if pr_num > 0 {
                // A failed gh read must not erase a pass the journal already
                // holds at this exact head: classify the local axis. No base
                // ref is known here, so freshness is exact-head equality (fails closed).
                let journal = review_journal_text(
                    &inputs.project_events,
                    &inputs.global_events,
                    &inputs.repo_slug,
                );
                let exact = |sha: &str| {
                    if !sha.is_empty() && sha == head_sha {
                        Freshness::Fresh
                    } else {
                        Freshness::Stale
                    }
                };
                let rescued = classify_coverage(
                    &[],
                    &[],
                    &journal,
                    &[],
                    false,
                    author_session.as_deref(),
                    &exact,
                    "",
                    &head_sha,
                );
                let data = coverage_event_data(
                    pr_num,
                    &rescued,
                    &head_sha,
                    &inputs.repo_slug,
                    author_session.as_deref(),
                );
                emit_to_both(
                    &inputs.project_events,
                    &inputs.global_events,
                    "review_coverage",
                    data.clone(),
                );
                // The failed read must stay visible: publish the rescued row
                // (unknown, or covered when the journal held the pass) so the
                // ruleset refuses on unknown rather than silently waiting. Only when the
                // caller PASSED --head (the merge recompute always does): an
                // explicit head is the PR head a caller that knows; a derived
                // local HEAD can be the canonical checkout's default-branch
                // tip, and a red marker there is a refusal aimed at nothing.
                if head_explicit {
                    publish_coverage_status(
                        &gh_bin,
                        &fno_bin,
                        &cwd,
                        &inputs.repo_slug,
                        pr_num,
                        &head_sha,
                        &head_sha,
                        &rescued,
                        &inputs.required_bots,
                        &inputs.optional_bots,
                        inputs.optional_lane_configured,
                        &required_reviewers,
                        // No tiling on the failed-read path, so no budget
                        // claim. The Unknown arm never reaches the veto.
                        false,
                        // No chain was read, so no hard-finding claim either;
                        // the Unknown arm never consults the waiver.
                        false,
                    );
                }
                // The persisted row above is schema-gated (the pr_num == 0
                // comment below applies here too); the quota diagnosis rides
                // stdout only. Without it, exit 4's unknown row is
                // indistinguishable from a genuine "nobody reviewed this" -
                // the reader was told to re-review a PR whose only problem
                // was an exhausted quota window.
                //
                let (secondary, quota) = exit4_quota(&gh_bin, &cwd, &read, &tail);
                let mut out = data;
                insert_quota_diagnostic(&mut out, &quota);
                if secondary {
                    out["reason"] = serde_json::json!(secondary_limit_reason());
                }
                return (4, out.to_string());
            }
            // The emitted unknown row above is schema-gated, so the exhaustion
            // diagnosis rides this stdout-only branch (and the stop hook's own
            // block reason); it must not fork the event contract. The same
            // exempt-probe classification as the --pr arm: a refusal with no
            // PR number gets the same verdict, not a divergent wording gate.
            let (secondary, quota) = exit4_quota(&gh_bin, &cwd, &read, &tail);
            let mut out = serde_json::json!({
                "error": format!("gh read failed: {read}"),
                "detail": tail,
                "emitted": false,
            });
            insert_quota_diagnostic(&mut out, &quota);
            if secondary {
                out["reason"] = serde_json::json!(secondary_limit_reason());
            }
            (4, out.to_string())
        }
    }
}
