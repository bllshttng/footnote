//! Is CI green, red or pending? Check-run classification, failing names, and pending detection.

use super::*;

pub(super) fn without_coverage_statuses(checks: &Value) -> Value {
    let Some(arr) = checks.as_array() else {
        return checks.clone();
    };
    Value::Array(
        arr.iter()
            .filter(|check| {
                let name = check.get("name").and_then(|v| v.as_str());
                let context = check.get("context").and_then(|v| v.as_str());
                let is_coverage = |value: Option<&str>| {
                    value == Some(COVERAGE_STATUS_CONTEXT)
                        || value == Some(COVERAGE_UNAVAILABLE_STATUS_CONTEXT)
                };
                !is_coverage(name) && !is_coverage(context)
            })
            .cloned()
            .collect(),
    )
}

/// One truth table for a `gh pr checks --json` payload: dedup to the latest run per name, drop the
/// coverage projections, then derive the conclusion, the failing names, and the pending flag. A
/// rollup the filter EMPTIED (only the two coverage contexts existed) reads Pending - "CI has not
/// reported yet", never the declared-none None, matching the Python twin's unknown - and its
/// pending flag is set too, so the wait stays watchable instead of a non-idlable re-invoke loop.
pub(crate) fn classify_checks_payload(
    checks: &Value,
) -> Result<(CiConclusion, Vec<String>, bool), String> {
    let deduped = latest_per_name(checks);
    let had_rows = deduped.as_array().map(|a| !a.is_empty()).unwrap_or(false);
    let filtered = without_coverage_statuses(&deduped);
    let mut conclusion = compute_ci_conclusion(&filtered)?;
    let emptied = had_rows && matches!(conclusion, CiConclusion::None);
    if emptied {
        conclusion = CiConclusion::Pending;
    }
    let pending = emptied || ci_has_pending_checks(&filtered);
    Ok((conclusion, failing_check_names(&filtered), pending))
}

pub(super) fn compute_ci_conclusion(checks: &Value) -> Result<CiConclusion, String> {
    let arr = match checks.as_array() {
        Some(a) => a,
        None => return Err("pr_checks_parse".to_string()),
    };

    if arr.is_empty() {
        // No checks configured and no declared_none -> fail closed
        return Ok(CiConclusion::None);
    }

    // `gh pr checks --json` classifies each check into a rollup `bucket`: pass | fail | pending |
    // skipping | cancel. (`conclusion` is NOT an available field on this subcommand; requesting it
    // errored the read on every fire - follow-on, previously masked by the budget bug terminating
    // sessions before this read ran.) Unknown or missing buckets fail closed as Pending - never
    // green.
    let bucket_of = |check: &Value| -> String {
        check
            .get("bucket")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase()
    };

    if let Some(failing) = arr
        .iter()
        .find(|c| matches!(bucket_of(c).as_str(), "fail" | "cancel"))
    {
        let name = failing
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Ok(CiConclusion::Failure(Some(name.to_string())));
    }
    if arr
        .iter()
        .any(|c| !matches!(bucket_of(c).as_str(), "pass" | "skipping"))
    {
        return Ok(CiConclusion::Pending);
    }
    Ok(CiConclusion::Success)
}

/// The local-review recovery from refusal: a REQUIRED bot explicitly refused
/// (not merely absent - that is a wait), nothing else required is missing or
/// stale, and a fresh local attestation at HEAD exists.
///
/// Optional staleness deliberately plays no part: an optional bot reading an
/// older commit is not a property any reviewer owes, and letting it disqualify
/// recovery wedges exactly the lanes the recovery exists to unwedge (PR 1151:
/// attestation minted, reviewed_count 1, gate still uncovered until the bot
/// was re-triggered and waited out). The three bot inputs are REQUIRED-only by
/// construction (`compute_review_info` walks `required_bots` alone), and that
/// is the invariant this split exists to state. Required staleness still
/// disqualifies: a stale REQUIRED verdict is one re-read from counting, and
/// recovery must not skip past it.
pub(super) fn local_recovery_from_refusal(
    reviewer_refused: &[String],
    missing_bots: &[String],
    stale_bots: &[(String, String)],
    coverage: &CoverageReport,
) -> bool {
    !reviewer_refused.is_empty()
        && missing_bots.is_empty()
        && stale_bots.is_empty()
        && coverage.verdicts.iter().any(|verdict| {
            verdict.producer == CoverageProducer::LocalAttestation
                && verdict.verdict == CoverageVerdict::Reviewed
        })
}

/// True when explicit reviewer refusal is the only remaining review obstacle.
pub(super) fn awaiting_review_only(pr: &PrInfo) -> bool {
    pr.coverage
        .review_state_at(pr.range_tiling.rounds_exhausted)
        == Some(ReviewState::ReviewerRefused)
        && pr.missing_bots.is_empty()
        && pr.stale_bots.is_empty()
        && pr.unaddressed_findings.is_empty()
        && pr.unattested_reviewers.is_empty()
}

/// Failing check/job names on a `gh pr checks --json name,bucket` payload
/// (bucket fail|cancel), the same granularity a main-HEAD job carries. Non-fail
/// buckets (pass|pending|skipping) are ignored. Malformed entries are skipped.
pub(super) fn failing_check_names(checks: &Value) -> Vec<String> {
    let Some(arr) = checks.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter(|c| {
            let bucket = c
                .get("bucket")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            matches!(bucket.as_str(), "fail" | "cancel")
        })
        .filter_map(|c| c.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .collect()
}

/// True iff any check is still in a non-terminal bucket (`pending`, `cancel`,
/// or an unrecognized bucket that is not one of pass|fail|skipping). The
/// DoneAwaitingMerge terminal must not fire while any check is unresolved: a
/// still-running check (e.g. the session's own new job) could turn red, so a
/// partial `Failure` is not yet proof that the ONLY problem is pre-existing
/// main-red. `cancel` is deliberately non-terminal here even though it stays
/// red in `failing_check_names` and `compute_ci_conclusion`: a cancelled run
/// produced NO result, so declaring the session done off it would assert a
/// verdict that never ran. The held terminal waits for a newer run (or the
/// iteration/budget kill criteria), which is the correct trade.
pub(super) fn ci_has_pending_checks(checks: &Value) -> bool {
    let Some(arr) = checks.as_array() else {
        return false;
    };
    arr.iter().any(|c| {
        let bucket = c
            .get("bucket")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        !matches!(bucket.as_str(), "pass" | "fail" | "skipping")
    })
}
