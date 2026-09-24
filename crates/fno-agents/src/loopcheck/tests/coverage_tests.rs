use super::*;

#[test]
fn a_required_bot_that_went_stale_lands_in_stale_bots() {
    // The other reachable path: `stale_bots` carries the same gate weight
    // `missing_bots` always had. A required bot whose only verdict sits on
    // a commit it read two commits ago has not reviewed THIS code; the
    // entry keeps the sha it read so the block message can ask for a
    // re-read instead of a first read.
    let json = serde_json::json!({"reviews": pr826_reviews(), "comments": []});
    let required = vec!["chatgpt-codex-connector".to_string()];
    let stale = compute_review_info(&json, &required, &|_| Freshness::Stale);
    assert!(stale.missing_bots.is_empty());
    assert_eq!(stale.stale_bots.len(), 1);
    assert_eq!(stale.stale_bots[0].0, "chatgpt-codex-connector");
    assert!(!stale.all_required_passed());
    let carried = compute_review_info(&json, &required, &|_| Freshness::CarriedDocsOnly);
    assert!(carried.missing_bots.is_empty());
    assert!(carried.stale_bots.is_empty());
    // Activity timestamp is not a freshness question: a stale review is
    // still activity, and the no-progress probe must keep seeing it.
    assert_eq!(stale.latest_ts, "2026-08-12T17:51:48Z");
    assert_eq!(review_activity_ts(&json), "2026-08-12T17:51:48Z");
}

#[test]
fn compute_review_info_per_bot_verdict() {
    let required = vec![
        "chatgpt-codex-connector".to_string(),
        "gemini-code-assist".to_string(),
    ];
    // Only codex posted a completed pass (COMMENTED counts).
    let json = serde_json::json!({
        "reviews": [
            {"author": {"login": "chatgpt-codex-connector"}, "state": "COMMENTED",
             "submittedAt": "2026-06-05T01:00:00Z"}
        ],
        "comments": []
    });
    let info = compute_review_info(&json, &required, &|_| Freshness::Fresh);
    assert!(!info.all_required_passed());
    assert_eq!(info.missing_bots, vec!["gemini-code-assist".to_string()]);
    assert_eq!(info.latest_ts, "2026-06-05T01:00:00Z");
}

#[test]
fn compute_review_info_clean_pass_comment_satisfies_the_gate() {
    let required = vec!["chatgpt-codex-connector".to_string()];
    let json = serde_json::json!({
        "reviews": [],
        "comments": [clean_pass_comment("abc12345")]
    });
    let fresh_at = |sha: &str| {
        if sha == "abc12345" {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    };
    let info = compute_review_info(&json, &required, &fresh_at);
    assert!(info.all_required_passed());
    assert!(info.missing_bots.is_empty());
    assert!(info.reviewer_refused.is_empty());
}

#[test]
fn compute_review_info_stale_clean_pass_lands_in_stale_bots() {
    // The comment names an older commit: the bot responded, but not to
    // this code - same staleness rule the review-object path applies.
    // The entry keeps the sha it read and FAILS the gate exactly
    // like a missing bot; only the message differs.
    let required = vec!["chatgpt-codex-connector".to_string()];
    let json = serde_json::json!({
        "reviews": [],
        "comments": [clean_pass_comment("0000000000")]
    });
    let fresh_at = |sha: &str| {
        if sha == "abc12345" {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    };
    let info = compute_review_info(&json, &required, &fresh_at);
    assert!(info.missing_bots.is_empty());
    assert_eq!(
        info.stale_bots,
        vec![(
            "chatgpt-codex-connector".to_string(),
            "0000000000".to_string()
        )]
    );
    assert!(!info.all_required_passed());
}

#[test]
fn compute_review_info_unpinned_clean_pass_stays_missing() {
    // A marker with no Reviewed commit line is not evidence on the
    // coverage axis; it must not be evidence on this one either.
    let required = vec!["chatgpt-codex-connector".to_string()];
    let json = serde_json::json!({
        "reviews": [],
        "comments": [{
            "author": {"login": "chatgpt-codex-connector[bot]"},
            "body": "Codex Review: Didn't find any major issues. Bravo.",
            "createdAt": "2026-08-17T02:00:00Z"
        }]
    });
    let info = compute_review_info(&json, &required, &|_| Freshness::Fresh);
    assert_eq!(
        info.missing_bots,
        vec!["chatgpt-codex-connector".to_string()]
    );
}

#[test]
fn compute_review_info_empty_state_not_a_pass() {
    // A review row with an empty state is not a completed pass.
    let required = vec!["chatgpt-codex-connector".to_string()];
    let json = serde_json::json!({
        "reviews": [
            {"author": {"login": "chatgpt-codex-connector"}, "state": "",
             "submittedAt": "2026-06-05T01:00:00Z"}
        ],
        "comments": []
    });
    let info = compute_review_info(&json, &required, &|_| Freshness::Fresh);
    assert!(!info.all_required_passed());
}

#[test]
fn compute_review_info_usage_limited_bot_blocks_gate() {
    // a required bot that posted only a usage-limit (quota) comment,
    // never a review, is detected as rate-limited (moved to usage_limited,
    // out of missing_bots) AND must FAIL the gate closed: a quota bounce is
    // not a review, so all_required_passed is false and the PR does not
    // merge. Reverting all_required_passed to `missing_bots.is_empty()`
    // alone makes this assertion fail - that is the regression guard.
    let required = vec!["chatgpt-codex-connector".to_string()];
    let json = serde_json::json!({
        "reviews": [],
        "comments": [
            {"author": {"login": "chatgpt-codex-connector"},
             "body": "You have reached your Codex usage limits for code reviews.",
             "createdAt": "2026-07-06T01:00:00Z"}
        ]
    });
    let info = compute_review_info(&json, &required, &|_| Freshness::Fresh);
    // Detection still holds: the bot is classified rate-limited, not missing.
    assert!(info.missing_bots.is_empty());
    assert_eq!(
        info.reviewer_refused,
        vec!["chatgpt-codex-connector".to_string()]
    );
    // The gate decision: a usage-limit body does NOT satisfy the gate.
    assert!(
        !info.all_required_passed(),
        "a usage-limit comment must not satisfy the review gate"
    );
}

#[test]
fn compute_review_info_usage_limit_only_own_comment_counts() {
    // AC1-ERR: a usage-limit marker in a HUMAN's comment must not drop the
    // bot - detection is scoped to the bot's own author.login.
    let required = vec!["chatgpt-codex-connector".to_string()];
    let json = serde_json::json!({
        "reviews": [],
        "comments": [
            {"author": {"login": "some-human"},
             "body": "The bot hit its usage limits for code reviews, ugh.",
             "createdAt": "2026-07-06T01:00:00Z"}
        ]
    });
    let info = compute_review_info(&json, &required, &|_| Freshness::Fresh);
    assert_eq!(
        info.missing_bots,
        vec!["chatgpt-codex-connector".to_string()]
    );
    assert!(info.reviewer_refused.is_empty());
    assert!(!info.all_required_passed());
}

#[test]
fn compute_review_info_real_review_beats_ratelimit_comment() {
    // AC1-EDGE: a bot that posted a usage-limit comment earlier AND a real
    // COMMENTED review is counted as passed, never usage-limited (it is
    // never in missing_bots to be scanned).
    let required = vec!["chatgpt-codex-connector".to_string()];
    let json = serde_json::json!({
        "reviews": [
            {"author": {"login": "chatgpt-codex-connector"}, "state": "COMMENTED",
             "submittedAt": "2026-07-06T02:00:00Z"}
        ],
        "comments": [
            {"author": {"login": "chatgpt-codex-connector"},
             "body": "codex usage limits reached",
             "createdAt": "2026-07-06T01:00:00Z"}
        ]
    });
    let info = compute_review_info(&json, &required, &|_| Freshness::Fresh);
    assert!(info.missing_bots.is_empty());
    assert!(info.reviewer_refused.is_empty());
    assert!(info.all_required_passed());
}
