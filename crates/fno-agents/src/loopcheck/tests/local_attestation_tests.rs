use super::*;

#[test]
fn rounds_bot_review_and_local_attestation_at_one_sha_are_one_round() {
    // The law's sentence: a round is keyed by HEAD, not by who recorded
    // it. A GitHub App review object and a local attestation at the same
    // commit are ONE round; the same two traces at different heads are
    // two - which the per-axis counters the shared set replaces would
    // have under-counted as max(1, 1).
    let events = format!("{}\n", attest_line("aaaaaaaaaa", "feature/x"));
    let reviews = vec![serde_json::json!({
        "state": "CHANGES_REQUESTED",
        "commit": {"oid": "aaaaaaaaaa"},
    })];
    assert_eq!(
        rounds_since_last_pass(&events, "feature/x", "bbbbbbbbbb", Some(&reviews)),
        1
    );
    let reviews_b = vec![serde_json::json!({
        "state": "APPROVED",
        "commit": {"oid": "cccccccccc"},
    })];
    assert_eq!(
        rounds_since_last_pass(&events, "feature/x", "bbbbbbbbbb", Some(&reviews_b)),
        2
    );
}

#[test]
fn a_zero_file_retraction_for_an_old_head_spares_the_newer_pass() {
    // The verb emits the retraction after any newer pass, so the events
    // order is pass@head2 then retract@head1: the named (old) pass is
    // already superseded, and revoking it must not also revoke the live
    // head-2 pass nobody asked about.
    let mut text = String::new();
    text.push_str(
        &serde_json::json!({
            "ts": "2026-01-01T00:00:00Z",
            "source": "test",
            "type": "review_attestation",
            "data": {"reviewer": "code-review", "head_sha": "head2", "verdict": "pass",
                     "attester_session_id": "sess-a", "branch": "b",
                     "reviewed_line_count": 5, "reviewed_file_count": 1}
        })
        .to_string(),
    );
    text.push('\n');
    text.push_str(
        &serde_json::json!({
            "ts": "2026-01-01T00:00:00Z",
            "source": "test",
            "type": "review_attestation",
            "data": {"reviewer": "code-review", "head_sha": "head1", "verdict": "fail",
                     "attester_session_id": "sess-a", "branch": "b",
                     "retracts_attester": "sess-a",
                     "reviewed_line_count": 3, "reviewed_file_count": 1}
        })
        .to_string(),
    );
    // The pair map keeps every latest verdict, so "the head-2 pass
    // survives" is "a PASS at head2 exists" - an answered fail alone
    // must not satisfy this assertion.
    let (passes, _raised) = local_latest_attestations(&text, "b", "head2");
    assert!(
        passes.iter().any(|p| p.head == "head2" && p.is_pass),
        "the newer head-2 pass must survive a retraction naming head-1: {passes:?}"
    );
}

#[test]
fn retraction_for_the_current_head_still_revokes() {
    // Same guard, the case it must not break: the retraction names the
    // head the pair entry already holds, so it lands and revokes.
    let mut text = String::new();
    text.push_str(
        &serde_json::json!({
            "ts": "2026-01-01T00:00:00Z",
            "source": "test",
            "type": "review_attestation",
            "data": {"reviewer": "code-review", "head_sha": "h", "verdict": "pass",
                     "attester_session_id": "sess-a", "branch": "b",
                     "reviewed_line_count": 5, "reviewed_file_count": 1}
        })
        .to_string(),
    );
    text.push('\n');
    text.push_str(
        &serde_json::json!({
            "ts": "2026-01-01T00:00:00Z",
            "source": "test",
            "type": "review_attestation",
            "data": {"reviewer": "code-review", "head_sha": "h", "verdict": "fail",
                     "attester_session_id": "sess-a", "branch": "b",
                     "retracts_attester": "sess-a",
                     "reviewed_line_count": 5, "reviewed_file_count": 1}
        })
        .to_string(),
    );
    let (passes, _raised) = local_latest_attestations(&text, "b", "h");
    assert!(
        !passes.iter().any(|p| p.head == "h" && p.is_pass),
        "a same-head retraction must revoke the pass"
    );
}
