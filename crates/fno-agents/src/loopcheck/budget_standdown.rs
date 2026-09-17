//! The GraphQL/fleet-budget stand-down tail: what a fire below the GraphQL
//! reserve, or under the fleet GitHub request budget's backoff, does instead
//! of spending. Extracted from loopcheck.rs (shrink-only) so the
//! budget-backoff idle lives beside the stand-down it amends. Nothing here
//! spends a GitHub request: every decision reads data the caller already
//! holds.
//!
//! Three endings, in order. A WATCHING fire with a renewable claim idles on
//! the tag + lease alone (the wait class cannot be verified without the
//! reads being refused). A None-intent fire under a fleet-budget BACKOFF
//! idles for the known window. Everything else blocks, and the message
//! teaches what the next fire re-probes.

use super::watch_lease::{claim_pair, harness_can_idle, permanent_lease_note, watch_window_ms};
use super::{Intent, GRAPHQL_FLOOR};
use crate::completion_output::allow_output;
use crate::gh_budget::Snapshot;

/// Everything the stand-down tail reads. One struct so the call site stays a
/// single expression and the tail's inputs cannot drift apart silently.
pub(super) struct StandDown<'a> {
    pub budget: &'a Snapshot,
    pub budget_cause: Option<&'static str>,
    pub secondary_refusal: bool,
    /// The GraphQL probe's remaining, None when the probe itself failed.
    pub quota_remaining: Option<i64>,
    pub intent: &'a Intent,
    pub intent_source: &'static str,
    pub session_id: &'a str,
    pub author_harness: Option<&'a str>,
    pub manifest_content: &'a str,
    pub emit: &'a dyn Fn(&str, serde_json::Value),
}

/// The idle window for a budget-backoff stand-down: the backoff itself,
/// bounded to a 1s floor (a 0s window would idle on nothing) and a 30m
/// ceiling (the longest sanctioned watcher bound; a longer idle would hold
/// the node claim past any watcher's lifetime).
fn idle_window_ms(backoff_remaining_s: i64) -> i64 {
    (backoff_remaining_s * 1000).clamp(1_000, 30 * 60 * 1_000)
}

/// The whole tail. Always answers: a non-terminal allow (watching lease or
/// budget-backoff idle) or the stand-down block. The caller has already
/// excluded Promise and Aborted intents.
pub(super) fn fire(sd: StandDown<'_>) -> (i32, String) {
    let StandDown {
        budget,
        budget_cause,
        secondary_refusal,
        quota_remaining,
        intent,
        intent_source,
        session_id,
        author_harness,
        manifest_content,
        emit,
    } = sd;
    let remaining_display = quota_remaining
        .map(|r| r.to_string())
        .unwrap_or_else(|| "unknown (probe failed)".to_string());
    let cause: String = if secondary_refusal {
        format!(
            "the fleet GitHub request budget is holding calls ({}: {}/{} points in 60s, \
             backoff {}s left) - `fno-agents fleet-incident gh-budget status` reads the \
             ledger",
            budget_cause.unwrap_or("budget"),
            budget.points_60s,
            budget.cap,
            budget.backoff_remaining_s
        )
    } else {
        "GraphQL primary quota".to_string()
    };
    // Lease-only exemption for a WATCHING fire (review finding on the
    // floor): the watch-idle branch below is unreachable from here, so
    // without this a quota window converts every watching fire into a
    // "continue working" block - killing watch-idle exactly when quota
    // is low - and the claim lease never renews. The wait class itself
    // CANNOT be verified (that needs the reads we are refusing to
    // spend), so this idles on the tag + lease alone, only on a
    // harness that self-wakes, and the message says the state was not
    // verified. The watcher's exit re-evaluates with fresh quota.
    let mut lease_note = String::new();
    if let Intent::Watching {
        ref reason,
        ref timeout,
        ref pr,
    } = intent
    {
        if harness_can_idle(author_harness, std::env::var("FNO_DRIVER_LIB").is_ok()) {
            let window_ms = watch_window_ms(timeout.as_deref());
            let claim = claim_pair(manifest_content);
            let renew_outcome = claim
                .as_ref()
                .map(|(key, holder)| crate::claims::renew(key, holder, window_ms, None));
            let renewed = matches!(renew_outcome.as_ref(), Some(Ok(true)));
            lease_note = permanent_lease_note(claim.as_ref(), renew_outcome.as_ref());
            if renewed {
                // The tag's own `pr=`/`reason=` attributes are the only
                // source here (the stand-down verifies nothing), so
                // `blocker` is only as trustworthy as the agent's tag:
                // pass the declared reason through when it is one of
                // the three real classes, else the honest "unknown"
                // (schema-valid) rather than guessing.
                let blocker = match reason.as_str() {
                    "ci" => "ci",
                    "review" => "review",
                    "merge_slot" => "merge_slot",
                    _ => "unknown",
                };
                emit(
                    "loop_check_watch_idle",
                    serde_json::json!({
                        "session_id": session_id,
                        "pr": pr.as_deref().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
                        "blocker": blocker,
                        "declared_timeout": timeout.clone().unwrap_or_default(),
                        "reason": reason,
                        "lease_ms": window_ms,
                        "stand_down": true,
                        "graphql_remaining": quota_remaining,
                        "secondary_refusal": secondary_refusal,
                        "budget_cause": budget_cause
                    }),
                );
                return (
                    0,
                    allow_output(
                        "allow",
                        None,
                        &format!(
                            "watching under GraphQL stand-down ({cause}, remaining \
                             {remaining_display}): idling until the watcher fires. \
                             This fire verified NO PR state - the lease is renewed \
                             for the window and the watcher's exit re-evaluates."
                        ),
                        0,
                        None,
                    ),
                );
            }
            // renewal failed -> never idle without a lease: fall
            // through to the stand-down block below.
        }
    }
    // A dedicated type, not "loop_check", for BOTH events below: a
    // stand-down fire verified nothing (that is the point of standing
    // down), so it has no real fingerprint. Emitting it AS a loop_check gave
    // read_prior_fires an empty-string fingerprint that never matches
    // current_fp, breaking the reverse-scan the instant it hit this row -
    // one stand-down fire silently truncated the whole consecutive-unchanged
    // streak for the session. loop_check_gh_error/_config are already
    // siblings of loop_check for exactly this reason: an event whose
    // fields do not fit the fingerprint contract does not overload it.
    //
    // x-9df9: a fleet-budget BACKOFF carries a known window (N seconds). A
    // None-intent fire under it can only re-print the stand-down, so it
    // idles for the window instead of burning a model invocation per tick -
    // the same non-terminal allow shape the watching exemption returns, with
    // the claim renewed over the window when the manifest records a pair.
    // Watching fires keep the lease-only arm above: idling without a claim
    // re-opens the dispatcher gap AC3-ERR closes, so a failed renewal still
    // blocks. codex/gemini and loop-run children have no self-wake and keep
    // the block, like every idle path.
    if budget_cause == Some("backoff")
        && matches!(intent, Intent::None)
        && harness_can_idle(author_harness, std::env::var("FNO_DRIVER_LIB").is_ok())
    {
        let window_ms = idle_window_ms(budget.backoff_remaining_s);
        let claim = claim_pair(manifest_content);
        let renew_outcome = claim
            .as_ref()
            .map(|(key, holder)| crate::claims::renew(key, holder, window_ms, None));
        emit(
            "loop_check_budget_standdown_idle",
            serde_json::json!({
                "session_id": session_id,
                "decision": "allow",
                "intent": "none",
                "backoff_remaining_s": budget.backoff_remaining_s,
                "lease_ms": window_ms,
                "claim_present": claim.is_some(),
                "renewed": matches!(renew_outcome.as_ref(), Some(Ok(true))),
                "graphql_remaining": quota_remaining,
                "secondary_refusal": true,
                "budget_cause": budget_cause
            }),
        );
        return (
            0,
            allow_output(
                "allow",
                None,
                &format!(
                    "standing down: {cause} - idling for the {n}s backoff window. \
                     An armed watcher's exit, mail, or the operator wakes this \
                     session; nothing to do now.",
                    n = budget.backoff_remaining_s
                ),
                0,
                None,
            ),
        );
    }
    emit(
        "loop_check_graphql_standdown",
        serde_json::json!({
            "session_id": session_id,
            "decision": "block",
            "intent": match intent {
                Intent::Promise => "promise",
                Intent::Aborted { .. } => "aborted",
                Intent::Watching { .. } => "watching",
                Intent::None => "none",
            },
            "intent_source": intent_source,
            "standing_down": true,
            "graphql_remaining": quota_remaining,
            "graphql_floor": GRAPHQL_FLOOR,
            "secondary_refusal": secondary_refusal,
            "budget_cause": budget_cause
        }),
    );
    (
        0,
        allow_output(
            "block",
            None,
            &format!(
                "standing down: {cause} (remaining {remaining_display}, floor \
                 {GRAPHQL_FLOOR}), so this fire spends no GraphQL - `gh pr view` / \
                 `gh pr checks` are SKIPPED, not retried. `fno do pr status <n>` still \
                 answers its CI verdict on the REST budget (a cache hit skips the \
                 review-thread read too, and REST shares the same secondary limit \
                 as GraphQL, so porting a read to REST alone does not escape a \
                 burst refusal). The next fire re-probes; a promise intent always \
                 proceeds.{lease_note}"
            ),
            0,
            None,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::idle_window_ms;

    #[test]
    fn idle_window_is_the_backoff_bounded() {
        assert_eq!(idle_window_ms(19), 19_000);
        assert_eq!(idle_window_ms(92), 92_000);
        // A 0s backoff would idle on nothing; the floor keeps the window real.
        assert_eq!(idle_window_ms(0), 1_000);
        // The ceiling is the longest sanctioned watcher bound: an idle past it
        // would hold the node claim with nothing watching.
        assert_eq!(idle_window_ms(4_000), 30 * 60 * 1_000);
    }
}
