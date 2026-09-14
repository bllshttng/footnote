//! The async-wait classifier and the arm-and-tag hint it renders: whether a
//! `<watching>` tag may idle on this PR, and the one ritual it should run.
//! Moved out of `loopcheck.rs` (shrink-only) so the conflicting-head refusal
//! lives beside the classifier it gates.

use super::watch_lease::ARM_HINT_LEAD;
use super::{nudge_class_idlable, short_sha, CiConclusion, PrInfo, PrState};

/// An open PR whose merge commit cannot be created. GitHub starts no
/// `pull_request` workflow on it, so a zero check count is a positive fact,
/// not a delay to sleep through. UNKNOWN (GitHub still computing) is NOT
/// this: it clears by itself.
fn is_conflicting(pr: &PrInfo) -> bool {
    pr.state == PrState::Open && pr.mergeable == "CONFLICTING"
}

/// The rebase receipt for a conflicting head - the same text the Python wait
/// (`cli/src/fno/pr/_wait.py`, exit 5) prints, so both surfaces teach one
/// remedy. None when the PR is not an open conflicting head.
pub(super) fn conflicting_reason(pr: &PrInfo) -> Option<String> {
    if !is_conflicting(pr) {
        return None;
    }
    Some(format!(
        "PR #{} is CONFLICTING at {}. GitHub starts no checks on a conflicting PR, \
         so none will arrive. Rebase onto the base (`fno do pr rebase`), push, \
         then re-arm the wait.",
        pr.number,
        short_sha(&pr.head_oid)
    ))
}

/// Whether the PR is in the async-wait class a `<watching>` tag may idle on
/// (x-e2c8): PR open, local HEAD pushed, no unaddressed findings (inline OR
/// operator), and the sole remaining blocker is CI still pending or an
/// outstanding bot review. Returns the blocker label, or None if anything else
/// blocks. External truth only - the tag is a request, this is the authority.
/// `head_shipped` is passed in rather than recomputed. It used to be
/// `pr.head_oid == local_head` here, a third copy of a predicate that must
/// agree with `done()`, and the copies drifted: this one returning false is
/// why a `<watching>` tag could not rescue a session whose branch had moved
/// past its own merge. One caller computes it once via `head_is_shipped`.
pub(super) fn async_wait_class(
    pr: &PrInfo,
    open_findings_empty: bool,
    head_shipped: bool,
) -> Option<&'static str> {
    // A conflicting PR has work to do NOW (rebase): no check can ever arrive
    // on this head, and a review of it is superseded by the rebase - so the
    // review idle below is refused with the CI one.
    if is_conflicting(pr) {
        return None;
    }
    if pr.state != PrState::Open
        || !head_shipped
        || !pr.unaddressed_findings.is_empty()
        || !open_findings_empty
    {
        return None;
    }
    // CI still pending AND nothing has concluded red yet: idle on CI. If a
    // check has ALREADY failed while others run, do NOT idle - the agent should
    // start debugging the failure now rather than wait out the rest (gemini).
    if pr.ci_has_pending && !matches!(pr.ci_conclusion, CiConclusion::Failure(_)) {
        return Some("ci");
    }
    // Awaiting an EXTERNAL bot review: a real GitHub login WILL post it, so
    // idling until it does is correct. `reviewed == false` with an EMPTY
    // missing_bots is instead a LOCAL-attestation gate (config.review.reviewers,
    // e.g. sigma) or an unaddressed finding - work the agent must DO, and no
    // GitHub reviewer will ever appear to wake it, so idling would park the
    // session forever. Require an outstanding bot (codex P1).
    //
    // An outstanding LOCAL reviewer disqualifies the wait even when a bot is
    // also outstanding (codex review of x-cdc7): the session has work it can do
    // right now, and if the bot never posts, idling means that work never
    // happens and the run dies on budget with the gate still unmet.
    //
    // x-b167: idle ONLY when every missing bot is in an idlable nudge state
    // (Awaiting, a genuine async wait; or NotNudgeable, today's status quo). A
    // NeedsNudge bot is work to DO (post its trigger) and an Unresponsive bot is
    // a wait nobody ends - idling on either parks the session. This is the same
    // rule x-cdc7 gave unattested_reviewers. An empty bot_nudges (not classified)
    // means every-bot-idlable vacuously, preserving pre-x-b167 behavior.
    if pr.ci_conclusion.is_ok()
        && !pr.reviewed
        && !pr.review_skipped
        && (!pr.missing_bots.is_empty() || !pr.stale_bots.is_empty())
        && pr.unattested_reviewers.is_empty()
        && pr.bot_nudges.iter().all(|n| nudge_class_idlable(&n.class))
    {
        return Some("review");
    }
    None
}

/// The arm-and-tag ritual (x-e2c8, US3) that converts an unwatched async wait
/// into a single idle turn. Supersedes the old "wait silently" prose: waiting
/// silently still costs a full model invocation every ~90s tick, whereas arming
/// a harness-tracked watcher and emitting `<watching>` idles the session to ZERO
/// invocations until the watcher fires. The `gh pr checks` shape is a template
/// (gh's `--watch` exit varies by version); the design depends only on the task
/// EXITING, never on its exit code.
///
/// The bound uses shell builtins, never `timeout(1)`: the plugin must work on
/// hosts where that binary (and `gtimeout`) is absent, so naming it makes the
/// watcher no-op and the session idle forever on a wait that never started.
/// The watchdog is reaped once the wait returns - left alive, it wakes 30m
/// later and kills whatever now holds that recycled pid (codex P1).
pub(super) fn arm_watch_hint(pr_number: i64, blocker: &str) -> String {
    // The watcher must WAIT on the actual blocker (codex P2): a review wait
    // has CI already green, so a checks watcher returns instantly and the
    // session just re-blocks. Both waits are the sanctioned `fno do pr wait`
    // verb, one plain command per wait: it polls REST at a 60s interval
    // (`gh pr checks --watch` / `gh pr view` are GraphQL, and a fleet of 60s
    // GraphQL watchers is exactly what exhausts the per-USER quota the merge
    // guard needs), it greps for the POSITIVE settled marker so a
    // rate-limited read keeps the watcher waiting instead of reading as
    // "nothing pending", and a plain command is the one shape a
    // worktree-isolated session's Bash guard always admits - an inline
    // `while`/`$(...)` loop is refused as "too complex to verify", which
    // wedged the very turn this hint was trying to unblock.
    let watcher = if blocker == "review" {
        format!(
            "background Bash `fno do pr wait {pr_number} --until review --timeout=30m` (wakes when a new review posts, or after ~30m)"
        )
    } else {
        format!(
            "background Bash `fno do pr wait {pr_number} --until settled --timeout=30m` (wakes when CI settles - green or red - or after ~30m)"
        )
    };
    format!(
        "{ARM_HINT_LEAD} with a hard timeout (e.g. {watcher}), then end your turn with `<watching reason=\"{blocker}\" pr=\"{pr_number}\" timeout=\"30m\">` and nothing else - the session then idles until the watcher exits."
    )
}

#[cfg(test)]
mod tests {
    use super::super::{build_block_reason, Coverage, CoverageReport, RangeTiling};
    use super::*;

    /// An open PR whose head matches local HEAD, CI still pending, no
    /// findings - the same shape `loopcheck.rs` tests build, with the
    /// mergeable field reading CONFLICTING.
    fn conflicting_pr() -> PrInfo {
        PrInfo {
            range_tiling: RangeTiling::default(),
            state: PrState::Open,
            number: 404,
            head_oid: "abc".to_string(),
            ci_conclusion: CiConclusion::Pending,
            failing_checks: vec![],
            ci_has_pending: true,
            mergeable: "CONFLICTING".to_string(),
            latest_review_ts: "none".to_string(),
            reviewed: false,
            missing_bots: vec![],
            bot_nudges: vec![],
            stale_bots: vec![],
            unaddressed_findings: vec![],
            review_skipped: false,
            unattested_reviewers: vec![],
            malformed_attestations: 0,
            posture: None,
            coverage: CoverageReport {
                github_approval_satisfies: false,
                coverage: Coverage::Covered(0),
                verdicts: vec![],
            },
        }
    }

    #[test]
    fn conflicting_pending_ci_is_not_an_async_wait() {
        assert_eq!(async_wait_class(&conflicting_pr(), true, true), None);
    }

    #[test]
    fn block_reason_names_the_rebase_not_a_wait() {
        let reason = build_block_reason(&conflicting_pr(), "abc", true, true);
        assert!(reason.contains("CONFLICTING"), "got: {reason}");
        assert!(reason.contains("fno do pr rebase"), "got: {reason}");
        assert!(!reason.contains("CI still running"), "got: {reason}");
        assert!(
            !reason.contains("declare ci.declared_none"),
            "got: {reason}"
        );
    }

    #[test]
    fn mergeable_or_unknown_still_idles_on_ci() {
        for mergeable in ["MERGEABLE", "UNKNOWN"] {
            let mut pr = conflicting_pr();
            pr.mergeable = mergeable.to_string();
            assert_eq!(async_wait_class(&pr, true, true), Some("ci"));
            let reason = build_block_reason(&pr, "abc", true, true);
            assert!(reason.contains("CI still running"), "got: {reason}");
            assert!(reason.contains("fno do pr wait"), "got: {reason}");
        }
    }

    #[test]
    fn unshipped_conflicting_head_still_teaches_the_push() {
        // The worker may have rebased already and just not pushed: the push
        // arm precedes the conflict arm, so the rebase receipt cannot shadow
        // it.
        let reason = build_block_reason(&conflicting_pr(), "bcd", true, false);
        assert!(reason.contains("push the latest commits"), "got: {reason}");
    }

    #[test]
    fn conflicting_review_idle_is_refused_too() {
        // A review of a conflicting head is superseded by the rebase that
        // moves it, so the bot-review idle must refuse with the CI one.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            missing_bots: vec!["chatgpt-codex-connector".into()],
            mergeable: "CONFLICTING".to_string(),
            ..conflicting_pr()
        };
        assert_eq!(async_wait_class(&pr, true, true), None);
    }
}
