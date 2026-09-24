use super::*;

#[test]
fn bot_verdict_fresh_clean_pass_supersedes_stale_review_object() {
    // The normal fix cycle: findings review at H1, fix push, clean-pass
    // comment at H2. Coverage used to hold the stale H1 object and never
    // read the comment while the presence gate passed - the two-gate
    // wedge. Both now read Reviewed through one predicate.
    let reviews = vec![stale_findings_review()];
    let comments = vec![clean_pass_comment("abc12345")];
    let fresh_at = fresh_at_head();
    let (verdict, sha, _) = bot_verdict("chatgpt-codex-connector", &reviews, &comments, &fresh_at);
    assert_eq!(verdict, CoverageVerdict::Reviewed);
    assert_eq!(sha, "abc12345");
    let json = serde_json::json!({"reviews": reviews, "comments": comments});
    let info = compute_review_info(&json, &["chatgpt-codex-connector".to_string()], &fresh_at);
    assert!(info.all_required_passed());
}

#[test]
fn bot_verdict_usage_after_clean_pass_refuses_both_axes() {
    // Clean pass pinned at HEAD, then a quota bounce. The pass does not
    // silently outlive the bot's own later refusal, and the presence gate
    // no longer passes a bot coverage reads as Refused.
    let comments = vec![
        clean_pass_comment("abc12345"),
        usage_comment("2026-08-17T03:00:00Z"),
    ];
    let fresh_at = fresh_at_head();
    let (verdict, _, _) = bot_verdict("chatgpt-codex-connector", &[], &comments, &fresh_at);
    assert_eq!(verdict, CoverageVerdict::Refused);
    let json = serde_json::json!({"reviews": [], "comments": comments});
    let info = compute_review_info(&json, &["chatgpt-codex-connector".to_string()], &fresh_at);
    assert!(!info.all_required_passed());
    assert_eq!(
        info.reviewer_refused,
        vec!["chatgpt-codex-connector".to_string()]
    );
}

#[test]
fn bot_verdict_authentication_failure_is_a_refusal() {
    let fresh_at = fresh_at_head();
    let (verdict, _, _) = bot_verdict(
        "chatgpt-codex-connector",
        &[],
        &[authentication_failure_comment()],
        &fresh_at,
    );
    assert_eq!(verdict, CoverageVerdict::Refused);
}

#[test]
fn bot_verdict_empty_or_unrecognized_comment_is_absent() {
    let fresh_at = fresh_at_head();
    for body in [
        "",
        "Review request received",
        "Review finding: authentication failed handling lacks a regression test",
    ] {
        let comment = serde_json::json!({
            "author": {"login": "chatgpt-codex-connector[bot]"},
            "body": body,
            "createdAt": "2026-08-17T03:00:00Z"
        });
        let (verdict, _, _) = bot_verdict("chatgpt-codex-connector", &[], &[comment], &fresh_at);
        assert_eq!(verdict, CoverageVerdict::Absent, "body: {body:?}");
    }
}

#[test]
fn bot_verdict_clean_pass_recovers_after_earlier_quota_refusal() {
    // Quota refusal first, bot recovers, reads HEAD clean: the fresh pass
    // wins on both axes; a historical refusal is not a life sentence.
    let comments = vec![
        usage_comment("2026-08-17T01:00:00Z"),
        clean_pass_comment("abc12345"),
    ];
    let fresh_at = fresh_at_head();
    let (verdict, _, _) = bot_verdict("chatgpt-codex-connector", &[], &comments, &fresh_at);
    assert_eq!(verdict, CoverageVerdict::Reviewed);
    let json = serde_json::json!({"reviews": [], "comments": comments});
    let info = compute_review_info(&json, &["chatgpt-codex-connector".to_string()], &fresh_at);
    assert!(info.all_required_passed());
}

#[test]
fn clean_pass_marker_with_minor_findings_is_not_a_pass() {
    // The bot posts this exact clause while REPORTING findings; a bare
    // substring marker cleared both gates on a PR that has them.
    let comments = vec![serde_json::json!({
        "author": {"login": "chatgpt-codex-connector[bot]"},
        "body": "Codex Review: Didn't find any major issues, but 2 minor ones need attention. Reviewed commit: abc12345",
        "createdAt": "2026-08-17T02:00:00Z"
    })];
    let fresh_at = fresh_at_head();
    assert!(clean_pass_review(&comments, "chatgpt-codex-connector", &fresh_at).is_none());
    let json = serde_json::json!({"reviews": [], "comments": comments});
    let info = compute_review_info(&json, &["chatgpt-codex-connector".to_string()], &fresh_at);
    assert_eq!(
        info.missing_bots,
        vec!["chatgpt-codex-connector".to_string()]
    );
}

#[test]
fn human_login_substring_of_bot_cannot_satisfy_the_gate() {
    // A drive-by human "codex" posting a review object used to clear the
    // required bot "chatgpt-codex-connector" through the symmetric
    // correspond test; the gate has always been one-way.
    let reviews = vec![serde_json::json!({
        "author": {"login": "codex"},
        "state": "COMMENTED",
        "commit": {"oid": "abc12345"}
    })];
    let fresh_at = fresh_at_head();
    let (verdict, _, _) = bot_verdict("chatgpt-codex-connector", &reviews, &[], &fresh_at);
    assert_eq!(verdict, CoverageVerdict::Absent);
    let json = serde_json::json!({"reviews": reviews, "comments": []});
    let info = compute_review_info(&json, &["chatgpt-codex-connector".to_string()], &fresh_at);
    assert_eq!(
        info.missing_bots,
        vec!["chatgpt-codex-connector".to_string()]
    );
}

#[test]
fn period_reworded_findings_note_is_not_a_clean_pass() {
    // "...issues. 2 minor ones need attention." shares the clause with a
    // genuine pass; only the FOLLOW-UP sentence separates them.
    let comments = vec![serde_json::json!({
        "author": {"login": "chatgpt-codex-connector[bot]"},
        "body": "Codex Review: Didn't find any major issues. 2 minor ones need attention. Reviewed commit: abc12345",
        "createdAt": "2026-08-17T02:00:00Z"
    })];
    let fresh_at = fresh_at_head();
    assert!(clean_pass_review(&comments, "chatgpt-codex-connector", &fresh_at).is_none());
}

#[test]
fn line_broken_clean_pass_with_pin_is_evidence() {
    // The pin is often its own line; a newline before it is a boundary a
    // genuine pass must not be dropped over.
    let comments = vec![serde_json::json!({
        "author": {"login": "chatgpt-codex-connector[bot]"},
        "body": "Codex Review: Didn't find any major issues\nReviewed commit: abc12345",
        "createdAt": "2026-08-17T02:00:00Z"
    })];
    let fresh_at = fresh_at_head();
    let got = clean_pass_review(&comments, "chatgpt-codex-connector", &fresh_at);
    assert_eq!(got.map(|(s, _, _)| s).as_deref(), Some("abc12345"));
}

#[test]
fn equal_rank_later_pass_replaces_the_earlier_one() {
    // Pass A (01:00), quota bounce (02:00), pass B (03:00), both passes
    // equal rank: the NEWEST pass is the evidence, and the bounce between
    // them must not outdate it. Keeping first-seen manufactured Refused.
    let pass = |at: &str, sha: &str| {
        serde_json::json!({
            "author": {"login": "chatgpt-codex-connector[bot]"},
            "body": format!("Codex Review: Didn't find any major issues. Bravo. Reviewed commit: {sha}"),
            "createdAt": at
        })
    };
    let comments = vec![
        pass("2026-08-17T01:00:00Z", "abc12345"),
        usage_comment("2026-08-17T02:00:00Z"),
        pass("2026-08-17T03:00:00Z", "abc12345"),
    ];
    let fresh_at = fresh_at_head();
    let (verdict, _, _) = bot_verdict("chatgpt-codex-connector", &[], &comments, &fresh_at);
    assert_eq!(verdict, CoverageVerdict::Reviewed);
}

#[test]
fn equal_rank_tie_breaks_on_the_stamp_not_the_array_order() {
    // The same three comments as the test above, delivered NEWEST FIRST.
    // Array order is the payload's promise, not ours: resolving the
    // equal-rank tie with `>=` over iteration order picked the 01:00 pass
    // here, the 02:00 bounce then postdated it, and a required bot fell
    // through Refused -> usage_limited -> all_required_passed false. The
    // parsed `createdAt` orders it the same way whatever order it arrives
    // in, which is the rule `bot_verdict` already applies to `submittedAt`.
    let pass = |at: &str| {
        serde_json::json!({
            "author": {"login": "chatgpt-codex-connector[bot]"},
            "body": "Codex Review: Didn't find any major issues. Bravo. Reviewed commit: abc12345",
            "createdAt": at
        })
    };
    let comments = vec![
        pass("2026-08-17T03:00:00Z"),
        pass("2026-08-17T01:00:00Z"),
        usage_comment("2026-08-17T02:00:00Z"),
    ];
    let fresh_at = fresh_at_head();
    let got = clean_pass_review(&comments, "chatgpt-codex-connector", &fresh_at);
    assert_eq!(
        got.and_then(|(_, _, ts)| ts).as_deref(),
        Some("2026-08-17T03:00:00Z"),
        "the newest equal-rank pass must win regardless of array order"
    );
    let (verdict, _, _) = bot_verdict("chatgpt-codex-connector", &[], &comments, &fresh_at);
    assert_eq!(verdict, CoverageVerdict::Reviewed);
}

#[test]
fn usage_marker_matches_mixed_case() {
    // The helper's contract is case-insensitive; the pass-vs-bounce
    // ordering must see a mixed-case bounce as a bounce.
    let comments = vec![
        clean_pass_comment("abc12345"),
        serde_json::json!({
            "author": {"login": "chatgpt-codex-connector[bot]"},
            "body": "Codex Usage Limits for code reviews reached",
            "createdAt": "2026-08-17T03:00:00Z"
        }),
    ];
    let fresh_at = fresh_at_head();
    let (verdict, _, _) = bot_verdict("chatgpt-codex-connector", &[], &comments, &fresh_at);
    assert_eq!(verdict, CoverageVerdict::Refused);
}

#[test]
fn quoted_marker_does_not_shadow_the_real_pin() {
    // A reply quoting an unparsable earlier marker keeps the real pin in
    // the same body visible to the scan.
    let comments = vec![serde_json::json!({
        "author": {"login": "chatgpt-codex-connector[bot]"},
        "body": "Codex Review: Didn't find any major issues. Bravo.\n> Reviewed commit: (see previous)\nReviewed commit: abc12345",
        "createdAt": "2026-08-17T02:00:00Z"
    })];
    let fresh_at = fresh_at_head();
    let got = clean_pass_review(&comments, "chatgpt-codex-connector", &fresh_at);
    assert_eq!(got.map(|(s, _, _)| s).as_deref(), Some("abc12345"));
}

#[test]
fn bravo_wearing_findings_is_not_a_clean_pass() {
    // `Bravo` must END its sentence: `Bravo, but...` and `Bravo. 2 minor
    // ones...` are findings notes wearing the flourish, and a bare
    // starts_with("bravo") counted both as passes.
    for body in [
            "Codex Review: Didn't find any major issues. Bravo, but two minor ones need attention. Reviewed commit: abc12345",
            "Codex Review: Didn't find any major issues. Bravo. 2 minor ones need attention. Reviewed commit: abc12345",
        ] {
            let comments = vec![serde_json::json!({
                "author": {"login": "chatgpt-codex-connector[bot]"},
                "body": body,
                "createdAt": "2026-08-17T02:00:00Z"
            })];
            let fresh_at = fresh_at_head();
            assert!(
                clean_pass_review(&comments, "chatgpt-codex-connector", &fresh_at).is_none(),
                "counted as a pass: {body}"
            );
            let (verdict, _, _) = bot_verdict("chatgpt-codex-connector", &[], &comments, &fresh_at);
            assert_eq!(verdict, CoverageVerdict::Absent, "verdict for: {body}");
        }
}

#[test]
fn bravo_terminated_by_the_pin_line_still_passes() {
    // The tightening must not eat the measured shape: flourish, sentence
    // end, pin on its own line.
    let comments = vec![serde_json::json!({
        "author": {"login": "chatgpt-codex-connector[bot]"},
        "body": "Codex Review: Didn't find any major issues. Bravo\nReviewed commit: abc12345",
        "createdAt": "2026-08-17T02:00:00Z"
    })];
    let fresh_at = fresh_at_head();
    let got = clean_pass_review(&comments, "chatgpt-codex-connector", &fresh_at);
    assert_eq!(got.map(|(s, _, _)| s).as_deref(), Some("abc12345"));
}

#[test]
fn quota_bounce_predating_the_bots_latest_evidence_reads_stale() {
    // A stale review object (01:00) plus an EARLIER bounce (00:00): the
    // bot recovered and reviewed; parking at Refused has no path back,
    // while Stale asks a re-read. The inverse ordering still refuses.
    let fresh_at = fresh_at_head();
    let (verdict, _, _) = bot_verdict(
        "chatgpt-codex-connector",
        &[stale_findings_review()],
        &[usage_comment("2026-08-17T00:00:00Z")],
        &fresh_at,
    );
    assert_eq!(verdict, CoverageVerdict::Stale);
    let (verdict, _, _) = bot_verdict(
        "chatgpt-codex-connector",
        &[stale_findings_review()],
        &[usage_comment("2026-08-17T02:00:00Z")],
        &fresh_at,
    );
    assert_eq!(verdict, CoverageVerdict::Refused);
}

#[test]
fn short_name_config_cannot_draft_a_fan_into_the_marker_lane() {
    // Config may name the bot by a short login ("codex"); containment then
    // matches any human whose login merely contains it. The comment lanes
    // require the AUTHOR to be a characterized bot profile, so the fan's
    // clean-pass marker and quota text are both inert. The review-object
    // lane is unaffected: a review object carries no marker text to
    // counterfeit, and the real bot still passes it.
    let fan = |body: &str| {
        serde_json::json!({
            "author": {"login": "codex-fan"},
            "body": body,
            "createdAt": "2026-08-17T02:00:00Z"
        })
    };
    let fresh_at = fresh_at_head();
    let (verdict, _, _) = bot_verdict(
        "codex",
        &[],
        &[
            fan("Codex Review: Didn't find any major issues. Bravo. Reviewed commit: abc12345"),
            fan("You have reached your Codex usage limits for code reviews"),
        ],
        &fresh_at,
    );
    assert_eq!(verdict, CoverageVerdict::Absent);
    // The real bot under the same short config still reads Reviewed.
    let (verdict, _, _) = bot_verdict("codex", &[], &[clean_pass_comment("abc12345")], &fresh_at);
    assert_eq!(verdict, CoverageVerdict::Reviewed);
}
