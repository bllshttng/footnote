use super::*;

#[test]
fn blocking_severity_codex_p1_both_forms() {
    // The exact markup codex emits (pinned from PR #447).
    assert_eq!(
        blocking_severity("![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) Bug"),
        Some("P1")
    );
    // Alt-text only and URL only each match.
    assert_eq!(blocking_severity("![P1 Badge] something"), Some("P1"));
    assert_eq!(
        blocking_severity("see https://img.shields.io/badge/P1-orange"),
        Some("P1")
    );
}

#[test]
fn blocking_severity_codex_p2_p3_advisory() {
    assert_eq!(
        blocking_severity("![P2 Badge](https://img.shields.io/badge/P2-yellow) nit"),
        None
    );
    assert_eq!(
        blocking_severity("![P3 Badge](https://img.shields.io/badge/P3-green) nit"),
        None
    );
}

#[test]
fn blocking_severity_gemini_critical_high_blocking() {
    assert_eq!(
        blocking_severity(
            "![critical](https://www.gstatic.com/codereviewagent/critical-priority.svg) bad"
        ),
        Some("critical")
    );
    assert_eq!(
        blocking_severity("![high](https://www.gstatic.com/codereviewagent/high-priority.svg) bad"),
        Some("high")
    );
}

#[test]
fn blocking_severity_gemini_medium_low_advisory() {
    assert_eq!(
        blocking_severity(
            "![medium](https://www.gstatic.com/codereviewagent/medium-priority.svg) hmm"
        ),
        None
    );
    assert_eq!(
        blocking_severity("![low](https://www.gstatic.com/codereviewagent/low-priority.svg) hmm"),
        None
    );
}

/// Boundaries: unrecognized / absent severity tokens classify advisory,
/// never blocking (locked decision 4).
#[test]
fn blocking_severity_unparseable_is_advisory() {
    assert_eq!(blocking_severity("just a comment with no badge"), None);
    assert_eq!(blocking_severity(""), None);
    assert_eq!(blocking_severity("P1 mentioned in prose only"), None);
}

#[test]
fn max_ts_none_handling() {
    assert_eq!(
        max_ts("none", "2026-06-05T01:00:00Z"),
        "2026-06-05T01:00:00Z"
    );
    assert_eq!(
        max_ts("2026-06-05T01:00:00Z", "none"),
        "2026-06-05T01:00:00Z"
    );
    assert_eq!(max_ts("none", "none"), "none");
    assert_eq!(max_ts("", ""), "none");
    assert_eq!(
        max_ts("2026-06-05T01:00:00Z", "2026-06-05T02:00:00Z"),
        "2026-06-05T02:00:00Z"
    );
}

/// AC2-ERR core: a P1 with no reply is unaddressed.
#[test]
fn finding_no_reply_is_unaddressed() {
    let comments = vec![finding_comment(
        100,
        "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
        "2026-06-05T01:10:00Z",
    )];
    let (ts, unaddressed) = compute_unaddressed_findings(&comments, &[], &req_vec(), &[]);
    assert_eq!(ts, "2026-06-05T01:10:00Z");
    assert_eq!(unaddressed.len(), 1);
    assert_eq!(unaddressed[0].path, "src/x.rs");
    assert_eq!(unaddressed[0].line, 42);
    assert_eq!(unaddressed[0].severity, "P1");
}

/// AC2-HP commit arm: non-bot reply + commit after the finding -> addressed.
#[test]
fn finding_reply_plus_commit_after_is_addressed() {
    let comments = vec![
        finding_comment(
            100,
            "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
            "2026-06-05T01:10:00Z",
        ),
        reply_comment(
            101,
            100,
            "bllshttng",
            "fixed in abc123",
            "2026-06-05T01:20:00Z",
        ),
    ];
    let commits = vec!["2026-06-05T01:30:00Z".to_string()];
    let (_, unaddressed) = compute_unaddressed_findings(&comments, &commits, &req_vec(), &[]);
    assert!(unaddressed.is_empty(), "commit-after arm must address");
}

/// AC2-FR wontfix arm: non-bot reply carrying wontfix:, NO commit after.
#[test]
fn finding_wontfix_reply_is_addressed_without_commit() {
    let comments = vec![
        finding_comment(
            100,
            "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
            "2026-06-05T01:10:00Z",
        ),
        reply_comment(
            101,
            100,
            "bllshttng",
            "wontfix: intentional - documented tradeoff",
            "2026-06-05T01:20:00Z",
        ),
    ];
    // Only commit predates the finding -> commit arm unsatisfied.
    let commits = vec!["2026-06-05T01:00:00Z".to_string()];
    let (_, unaddressed) = compute_unaddressed_findings(&comments, &commits, &req_vec(), &[]);
    assert!(unaddressed.is_empty(), "wontfix arm must address alone");
}

/// Anti-gaming: a commit alone (no reply) does NOT address (locked
/// decision 3 - any unrelated commit would silently clear a P1).
#[test]
fn finding_commit_without_reply_is_unaddressed() {
    let comments = vec![finding_comment(
        100,
        "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
        "2026-06-05T01:10:00Z",
    )];
    let commits = vec!["2026-06-05T01:30:00Z".to_string()];
    let (_, unaddressed) = compute_unaddressed_findings(&comments, &commits, &req_vec(), &[]);
    assert_eq!(unaddressed.len(), 1, "commit alone must not address");
}

/// A bot's own reply in the thread is not an ack.
#[test]
fn finding_bot_reply_only_is_unaddressed() {
    let comments = vec![
        finding_comment(
            100,
            "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
            "2026-06-05T01:10:00Z",
        ),
        reply_comment(
            101,
            100,
            "chatgpt-codex-connector[bot]",
            "elaborating on my finding",
            "2026-06-05T01:15:00Z",
        ),
    ];
    let commits = vec!["2026-06-05T01:30:00Z".to_string()];
    let (_, unaddressed) = compute_unaddressed_findings(&comments, &commits, &req_vec(), &[]);
    assert_eq!(unaddressed.len(), 1, "bot self-reply must not count as ack");
}

/// Reply present but neither commit-after nor wontfix -> still unaddressed.
#[test]
fn finding_reply_without_commit_or_wontfix_is_unaddressed() {
    let comments = vec![
        finding_comment(
            100,
            "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
            "2026-06-05T01:10:00Z",
        ),
        reply_comment(
            101,
            100,
            "bllshttng",
            "looking into it",
            "2026-06-05T01:20:00Z",
        ),
    ];
    let commits = vec!["2026-06-05T01:00:00Z".to_string()]; // predates finding
    let (_, unaddressed) = compute_unaddressed_findings(&comments, &commits, &req_vec(), &[]);
    assert_eq!(unaddressed.len(), 1);
}

/// A finding from a NON-required bot does not gate.
#[test]
fn finding_from_non_required_bot_ignored() {
    let comments = vec![serde_json::json!({
        "id": 200,
        "in_reply_to_id": null,
        "user": {"login": "gemini-code-assist[bot]"},
        "body": "![high](https://www.gstatic.com/codereviewagent/high-priority.svg) eh",
        "path": "src/y.rs",
        "line": 7,
        "created_at": "2026-06-05T01:10:00Z"
    })];
    // required = codex only; gemini finding is not gate-relevant
    let (ts, unaddressed) = compute_unaddressed_findings(&comments, &[], &req_vec(), &[]);
    assert!(unaddressed.is_empty());
    // ...but its timestamp still feeds the fingerprint.
    assert_eq!(ts, "2026-06-05T01:10:00Z");
}

/// Boundaries: empty comments array -> no findings, ts "none".
#[test]
fn empty_comments_no_findings() {
    let (ts, unaddressed) = compute_unaddressed_findings(&[], &[], &req_vec(), &[]);
    assert_eq!(ts, "none");
    assert!(unaddressed.is_empty());
}

/// sigma-review: a blocking finding row with a missing id is SKIPPED
/// (under-block per locked decision 4), never pooled on a default id
/// where one stray reply could clear multiple findings.
#[test]
fn finding_missing_id_skipped_not_pooled() {
    let no_id = serde_json::json!({
        "in_reply_to_id": null,
        "user": {"login": "chatgpt-codex-connector[bot]"},
        "body": "![P1 Badge](https://img.shields.io/badge/P1-orange) idless",
        "path": "src/z.rs", "line": 3,
        "created_at": "2026-06-05T01:05:00Z"
    });
    let real = finding_comment(
        100,
        "![P1 Badge](https://img.shields.io/badge/P1-orange) real",
        "2026-06-05T01:10:00Z",
    );
    // A stray reply keyed to id 0 must not ack anything.
    let stray = reply_comment(
        101,
        0,
        "bllshttng",
        "wontfix: stray",
        "2026-06-05T01:20:00Z",
    );
    let comments = vec![no_id, real, stray];
    let (_, unaddressed) = compute_unaddressed_findings(&comments, &[], &req_vec(), &[]);
    assert_eq!(unaddressed.len(), 1, "only the real finding remains");
    assert_eq!(unaddressed[0].id, 100);
}

/// sigma-review: commit-after comparison parses timestamps instead of
/// string-comparing - an offset-suffixed commit date that lexicographically
/// sorts above a Zulu finding date but is EARLIER in UTC must not clear
/// the finding.
#[test]
fn ts_after_parses_offsets_correctly() {
    // 23:30+13:00 == 10:30Z, which is BEFORE 11:00Z - but the raw string
    // "2026-06-05T23:30:00+13:00" > "2026-06-05T11:00:00Z".
    assert!(!ts_after(
        "2026-06-05T23:30:00+13:00",
        "2026-06-05T11:00:00Z"
    ));
    // POSITIVE direction proves chrono's FromStr for DateTime<Utc>
    // parses offset-suffixed RFC3339 and converts to UTC (gemini's
    // #448 critical claimed it errors; empirically it returns
    // Ok(2026-06-05T13:30:00Z) here). Without this assertion the
    // offset case above could pass vacuously via the Err arm.
    assert!(ts_after(
        "2026-06-05T23:30:00+10:00", // == 13:30Z
        "2026-06-05T11:00:00Z"
    ));
    assert!(ts_after("2026-06-05T11:00:01Z", "2026-06-05T11:00:00Z"));
    assert!(!ts_after("2026-06-05T11:00:00Z", "2026-06-05T11:00:00Z"));
    // Unparseable on either side never clears a finding.
    assert!(!ts_after("garbage", "2026-06-05T11:00:00Z"));
    assert!(!ts_after("2026-06-05T11:00:00Z", "garbage"));
    assert!(!ts_after("2026-06-05T11:00:00Z", ""));
}

/// gemini high on #448: max_ts compares chronologically when both sides
/// parse, returning the original string either way (byte-stable
/// fingerprint).
#[test]
fn max_ts_chronological_with_offsets() {
    // +13:00 form is EARLIER in UTC despite sorting higher as a string.
    assert_eq!(
        max_ts("2026-06-05T23:30:00+13:00", "2026-06-05T11:00:00Z"),
        "2026-06-05T11:00:00Z"
    );
    // The winner is returned verbatim.
    assert_eq!(
        max_ts("2026-06-05T23:30:00+10:00", "2026-06-05T11:00:00Z"),
        "2026-06-05T23:30:00+10:00"
    );
}

/// Concurrency (Failure Modes): a reply arriving BEFORE its parent
/// finding in the comments array (REST ordering is not guaranteed across
/// pagination) still acks the finding - no order dependence.
#[test]
fn finding_reply_listed_before_finding_still_addressed() {
    let comments = vec![
        reply_comment(
            101,
            100,
            "bllshttng",
            "wontfix: ordering test",
            "2026-06-05T01:20:00Z",
        ),
        finding_comment(
            100,
            "![P1 Badge](https://img.shields.io/badge/P1-orange) bug",
            "2026-06-05T01:10:00Z",
        ),
    ];
    let (_, unaddressed) = compute_unaddressed_findings(&comments, &[], &req_vec(), &[]);
    assert!(
        unaddressed.is_empty(),
        "reply-before-finding ordering must still ack"
    );
}
