use super::*;

#[test]
fn refused_invocation_row_reads_reviewer_refused_not_unreviewed() {
    // The measured 2026-08-30 empty-diff shape: a review ran at a
    // recipient sitting on the base branch, read nothing, and the producer
    // refused to attest. Before the refused terminal row, that attempt was
    // byte-identical to "never attempted" at every coverage surface, so a
    // king re-fired the review and reproduced the failure exactly.
    let events = format!(
        "{}\n{}\n",
        serde_json::json!({
            "type": "review_invocation",
            "data": {"invocation_id": "ri-1", "stage": "refused",
                     "verb": "/review", "reason": "empty_diff",
                     "head_sha": "abc123", "branch": "feature/x"}
        })
        .to_string(),
        serde_json::json!({
            "type": "review_invocation",
            "data": {"invocation_id": "ri-1", "stage": "sent",
                     "verb": "/review"}
        })
        .to_string(),
    );
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Stale,
        "feature/x",
        "abc123",
    );
    assert_eq!(rep.coverage, Coverage::Covered(0), "a refusal never covers");
    assert_eq!(rep.review_state(), Some(ReviewState::ReviewerRefused));
    assert_eq!(rep.refused_reviewers(), vec!["review"]);
    assert_eq!(
        rep.verdicts[0].refusal_reason.as_deref(),
        Some("empty_diff"),
        "the refusal class rides the verdict so a reader names the cause"
    );
    // A real review outranks the refusal: once something Reviewed exists,
    // the state must not stay parked at ReviewerRefused.
    let with_pass = format!(
        "{}{}\n",
        events,
        attestation_line("code-review", "abc123", "pass")
    );
    let rep = classify_coverage(
        &[],
        &[],
        &with_pass,
        &[],
        true,
        None,
        &|_| Freshness::Fresh,
        "feature/x",
        "abc123",
    );
    assert_eq!(rep.review_state(), Some(ReviewState::Reviewed));
}

#[test]
fn refused_invocation_row_out_of_scope_is_ignored() {
    // A refusal on ANOTHER branch's attempt is not this PR's state; only
    // exact-head rows mint a verdict. The row below carries a foreign
    // branch AND a foreign head.
    let events = serde_json::json!({
        "type": "review_invocation",
        "data": {"invocation_id": "ri-1", "stage": "refused",
                 "verb": "/review", "reason": "empty_diff",
                 "head_sha": "fff999", "branch": "feature/OTHER"}
    })
    .to_string();
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Stale,
        "feature/x",
        "abc123",
    );
    assert_eq!(rep.review_state(), Some(ReviewState::Unreviewed));
    assert!(rep.refused_reviewers().is_empty());
}

#[test]
fn refused_invocation_row_retires_when_the_head_moves() {
    // The refusal is a terminal of ONE attempt at ONE measured head, not a
    // claim about the branch: once the author pushes a new head, the old
    // refusal must not park ReviewerRefused at the new head (round-2
    // review finding 1 - the state read as "the reviewer declined" at a
    // head nobody attempted, and only a real review could clear it).
    let events = serde_json::json!({
        "type": "review_invocation",
        "data": {"invocation_id": "ri-1", "stage": "refused",
                 "verb": "/review", "reason": "empty_diff",
                 "head_sha": "abc123", "branch": "feature/x"}
    })
    .to_string();
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Stale,
        "feature/x",
        "def456",
    );
    assert_eq!(
        rep.review_state(),
        Some(ReviewState::Unreviewed),
        "a moved head retires the old refusal; nothing reviewed the new head"
    );
    assert!(rep.refused_reviewers().is_empty());
}

#[test]
fn github_app_verdict_at_an_older_commit_is_stale_and_uncovers_the_pr() {
    // THE specimen. Before this, the github_app axis read `state !=
    // ""` and never asked which commit the review was submitted against,
    // so this exact payload produced `coverage: covered, reviewed_count:
    // 1` for a commit codex never saw. The state is non-empty here on
    // purpose: that is the whole of what the old rule looked at.
    let rep = classify_coverage(
        &pr826_reviews(),
        &[],
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &|_| Freshness::Stale,
        "",
        "",
    );
    let v = &rep.verdicts[0];
    assert_eq!(v.verdict, CoverageVerdict::Stale);
    assert_eq!(v.freshness, Some(Freshness::Stale));
    assert_eq!(v.reviewed_sha, "8e557ccdecec07abc7e409ad8d888318016612c1");
    assert_eq!(rep.coverage, Coverage::Covered(0));
    // And the word a human reads now agrees with the number beside it.
    let data = coverage_event_data(826, &rep, "89bc0b91", "", None);
    assert_eq!(data["coverage"], serde_json::json!("uncovered"));
    assert_eq!(data["reviewed_count"], serde_json::json!(0));
}

#[test]
fn github_app_verdict_carried_across_a_rebase_still_counts() {
    // The same payload where the head moved by a rebase rather than a code
    // change: the reviewer's read still describes the code, so it counts.
    let rep = classify_coverage(
        &pr826_reviews(),
        &[],
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        &|_| Freshness::CarriedBaseSync,
        "",
        "",
    );
    assert_eq!(rep.verdicts[0].verdict, CoverageVerdict::Reviewed);
    assert_eq!(rep.coverage, Coverage::Covered(1));
}

#[test]
fn github_app_review_without_a_commit_oid_fails_closed() {
    // An older review object, or a payload shape change, leaves no commit
    // to pin. That is an absence, and an absence is never freshness.
    let reviews = vec![serde_json::json!({
        "author": {"login": "chatgpt-codex-connector"},
        "state": "APPROVED"
    })];
    let rep = classify_coverage(
        &reviews,
        &[],
        "",
        &["chatgpt-codex-connector".to_string()],
        true,
        None,
        // The real predicate, not a stub: an empty sha must reach Stale on
        // its own rather than because a fake said so.
        &|sha| review_freshness(sha, "89bc0b91", &FreshnessFacts::default()),
        "",
        "",
    );
    assert_eq!(rep.verdicts[0].verdict, CoverageVerdict::Stale);
    assert_eq!(rep.coverage, Coverage::Covered(0));
}

#[test]
fn local_attestation_survives_a_carrying_head_move() {
    // THE relief, and the only relief the measurement supports: an
    // attestation at an older commit whose code identity still matches
    // keeps counting. Before this, the scan dropped every line whose head
    // was not byte-equal to the current one, so the mandatory pre-merge
    // rebase destroyed a review that was still entirely valid.
    let events = attestation_line_on_branch("code-review", "oldhead", "pass", "feature/x");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some("sess-author"),
        &|_| Freshness::CarriedBaseSync,
        "feature/x",
        "currenthead",
    );
    assert_eq!(rep.verdicts[0].verdict, CoverageVerdict::Reviewed);
    assert_eq!(rep.verdicts[0].reviewed_sha, "oldhead");
    assert_eq!(rep.coverage, Coverage::Covered(1));
}

#[test]
fn local_attestation_dies_on_a_real_code_change() {
    // The other 91%. No rule that refuses to guess can absorb these.
    let events = attestation_line_on_branch("code-review", "oldhead", "pass", "feature/x");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Stale,
        "feature/x",
        "currenthead",
    );
    assert_eq!(rep.verdicts[0].verdict, CoverageVerdict::Stale);
    assert_eq!(rep.coverage, Coverage::Covered(0));
}

#[test]
fn a_later_fail_still_revokes_an_earlier_pass_across_heads() {
    // Retraction ordering must survive the scan no longer filtering by
    // head: a `fail` posted after a `pass` revokes it, even when the two
    // sit on different commits that both carry.
    let events = format!(
        "{}\n{}",
        attestation_line_on_branch("code-review", "headA", "pass", "feature/x"),
        attestation_line_on_branch("code-review", "headB", "fail", "feature/x")
    );
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Fresh,
        "feature/x",
        "headA",
    );
    assert!(rep.verdicts.is_empty());
    assert_eq!(rep.coverage, Coverage::Covered(0));
}

#[test]
fn a_stale_local_pass_does_not_rescue_a_failed_github_read() {
    // Positive local evidence trumps a bot outage. A STALE local
    // pass is not positive evidence of anything current, so it must not
    // buy `covered` the way a fresh one does.
    let events = attestation_line_on_branch("code-review", "oldhead", "pass", "feature/x");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        false,
        None,
        &|_| Freshness::Stale,
        "feature/x",
        "currenthead",
    );
    assert_eq!(rep.coverage, Coverage::Unknown);

    let fresh = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        false,
        None,
        &|_| Freshness::Fresh,
        "feature/x",
        "currenthead",
    );
    assert_eq!(fresh.coverage, Coverage::Covered(1));
}

#[test]
fn coverage_event_reports_how_much_of_the_count_is_self_attested() {
    // The self-review question answered as a number on the verdict rather
    // than in prose. It gates nothing; it is now READABLE.
    let events = format!(
        "{}\n{}",
        attestation_line("code-review", "h", "pass"),
        serde_json::json!({
            "ts": "2026-01-01T00:00:00Z",
            "source": "test",
            "type": "review_attestation",
            "data": {"reviewer": "sigma", "head_sha": "h", "verdict": "pass",
                     "attester_session_id": "sess-peer"}
        })
    );
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some("sess-author"),
        &|_| Freshness::Fresh,
        "",
        "h",
    );
    assert_eq!(rep.coverage, Coverage::Covered(2));
    assert_eq!(rep.self_attested_count(), 1);
    let data = coverage_event_data(826, &rep, "h", "", Some("sess-author"));
    assert_eq!(data["coverage"], serde_json::json!("covered"));
    assert_eq!(data["reviewed_count"], serde_json::json!(2));
    // The pass subset: both verdicts came from pass attestations.
    assert_eq!(data["passed_count"], serde_json::json!(2));
    assert_eq!(data["self_attested_count"], serde_json::json!(1));
    assert_eq!(data["author_session_id"], serde_json::json!("sess-author"));
}

#[test]
fn coverage_event_omits_self_attested_count_when_authorship_unmeasured() {
    // The manifest-less recompute shape: no author session, so an
    // attested line classifies Unmeasured (a concrete attester, no
    // comparison possible) and self_attested_count() reads 0 while the
    // truth is UNMEASURED. A measured zero and an unmeasured one must
    // not serialize identically - the field is omitted, never 0, so a
    // future gate on it cannot read absence-of-measurement as
    // absence-of-self-attestation (the aggregate shape).
    let events = attestation_line("code-review", "h", "pass");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        None,
        &|_| Freshness::Fresh,
        "",
        "h",
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    // No verdict carries a MEASURED classification - the direct statement
    // of "unmeasured" authorship.
    assert!(rep
        .verdicts
        .iter()
        .all(|v| v.attestation_origin == AttestationOrigin::Unmeasured));
    let data = coverage_event_data(826, &rep, "h", "", None);
    assert_eq!(data["reviewed_count"], serde_json::json!(1));
    assert!(
        data.get("self_attested_count").is_none(),
        "an unmeasured authorship must omit the field, not report 0: {data}"
    );
    // The control: the same events with a measured author emit the field
    // (attestation_line stamps attester "sess-author"), so the omission
    // above is the unmeasured marker, not a dropped key. Classification
    // happens at classify_coverage time, so the measured report is built
    // with the author - exactly how read_pr_info threads one
    // author_session into both.
    let measured_rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some("sess-author"),
        &|_| Freshness::Fresh,
        "",
        "h",
    );
    let measured = coverage_event_data(826, &measured_rep, "h", "", Some("sess-author"));
    assert_eq!(measured["self_attested_count"], serde_json::json!(1));
}

#[test]
fn a_zero_line_attestation_is_not_review_evidence() {
    // A pass over a zero-line diff is a review of nothing: a session
    // resolving its target from a checkout sitting on the base branch
    // reads an empty diff, reports clean, and before this guard that
    // pass counted exactly like a real one. The producer refuses to emit
    // at 0, so a line carrying the field as 0 is either pre-guard or
    // hand-crafted; skipped either way. The control pins that the field
    // ABSENT (the pre-landed backlog) still counts - absence must never
    // be read as zero.
    let zero = serde_json::json!({
        "ts": "2026-01-01T00:00:00Z", "source": "test",
        "type": "review_attestation",
        "data": {"reviewer": "code-review", "head_sha": "h", "verdict": "pass",
                 "attester_session_id": "sess-author", "reviewed_line_count": 0}
    })
    .to_string();
    let rep = classify_coverage(
        &[],
        &[],
        &zero,
        &[],
        false,
        None,
        &|_| Freshness::Fresh,
        "",
        "h",
    );
    assert!(
        rep.verdicts.is_empty(),
        "a zero-line pass must yield no verdict"
    );
    assert_eq!(rep.coverage, Coverage::Unknown);

    let control = attestation_line("code-review", "h", "pass");
    let rep = classify_coverage(
        &[],
        &[],
        &control,
        &[],
        false,
        None,
        &|_| Freshness::Fresh,
        "",
        "h",
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
}

#[test]
fn a_zero_file_attestation_is_not_review_evidence_either() {
    // The forged row of the future spells the file count explicitly: 0
    // lines AND 0 files is the empty-diff shape the producer refuses, so
    // the gate must refuse it too, whatever else the line claims.
    let zero = serde_json::json!({
        "ts": "2026-01-01T00:00:00Z", "source": "test",
        "type": "review_attestation",
        "data": {"reviewer": "code-review", "head_sha": "h", "verdict": "pass",
                 "attester_session_id": "sess-author",
                 "reviewed_line_count": 0, "reviewed_file_count": 0}
    })
    .to_string();
    let rep = classify_coverage(
        &[],
        &[],
        &zero,
        &[],
        false,
        None,
        &|_| Freshness::Fresh,
        "",
        "h",
    );
    assert!(
        rep.verdicts.is_empty(),
        "a zero-file pass must yield no verdict"
    );
}

#[test]
fn a_zero_line_pass_with_files_is_a_real_review() {
    // Binary, pure-rename and empty-file diffs change files without
    // changing text lines: reviewed_line_count 0 WITH reviewed_file_count
    // above zero is an honest measurement of a real review, and skipping
    // it would strand exactly those PRs (images, fonts, renames) with no
    // satisfiable producer path.
    let binary = serde_json::json!({
        "ts": "2026-01-01T00:00:00Z", "source": "test",
        "type": "review_attestation",
        "data": {"reviewer": "code-review", "head_sha": "h", "verdict": "pass",
                 "attester_session_id": "sess-author",
                 "reviewed_line_count": 0, "reviewed_file_count": 1}
    })
    .to_string();
    let rep = classify_coverage(
        &[],
        &[],
        &binary,
        &[],
        false,
        None,
        &|_| Freshness::Fresh,
        "",
        "h",
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
}

#[test]
fn a_retraction_revokes_the_named_pair_not_the_retractor() {
    // The retraction verb addresses the EVENT, not the identity: an
    // operator session emits a fail carrying retracts_attester naming the
    // pair that passed. The pair-keyed scan must revoke THAT pair while
    // the retracting session's own entry stays untouched - undoing an
    // impersonation must not require performing it a second time.
    let pass = serde_json::json!({
        "ts": "2026-01-01T00:00:00Z", "source": "test",
        "type": "review_attestation",
        "data": {"reviewer": "code-review", "head_sha": "h", "verdict": "pass",
                 "attester_session_id": "sess-A", "branch": "feature/x"}
    })
    .to_string();
    let control = classify_coverage(
        &[],
        &[],
        &pass,
        &[],
        false,
        None,
        &|_| Freshness::Fresh,
        "feature/x",
        "h",
    );
    assert_eq!(control.coverage, Coverage::Covered(1));

    let retraction = serde_json::json!({
        "ts": "2026-01-01T00:00:00Z", "source": "test",
        "type": "review_attestation",
        "data": {"reviewer": "code-review", "head_sha": "h", "verdict": "fail",
                 "attester_session_id": "sess-operator",
                 "retracts_attester": "sess-A", "branch": "feature/x"}
    })
    .to_string();
    let revoked = classify_coverage(
        &[],
        &[],
        &format!("{}\n{}", pass, retraction),
        &[],
        false,
        None,
        &|_| Freshness::Fresh,
        "feature/x",
        "h",
    );
    assert!(
        revoked.verdicts.is_empty(),
        "the named pair must yield no verdict"
    );
    assert_eq!(revoked.coverage, Coverage::Unknown);
}

#[test]
fn one_attesters_emit_never_moves_another_pairs_head() {
    // The staleness defence of the pair key, pinned: session B emitting at
    // a new head inserts B's own entry and leaves A's pass pinned to the
    // head A actually reviewed. A same-key overwrite would have refreshed
    // A's stale verdict onto code A never saw.
    let events = format!(
        "{}\n{}",
        serde_json::json!({
            "ts": "2026-01-01T00:00:00Z",
            "source": "test",
            "type": "review_attestation",
            "data": {"reviewer": "code-review", "head_sha": "head-1", "verdict": "pass",
                     "attester_session_id": "sess-A", "branch": "feature/x"}
        }),
        serde_json::json!({
            "ts": "2026-01-01T00:00:00Z",
            "source": "test",
            "type": "review_attestation",
            "data": {"reviewer": "code-review", "head_sha": "head-2", "verdict": "pass",
                     "attester_session_id": "sess-B", "branch": "feature/x"}
        })
    );
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        false,
        None,
        &|_| Freshness::Fresh,
        "feature/x",
        "head-2",
    );
    assert_eq!(rep.coverage, Coverage::Covered(2));
    let shas: Vec<&str> = rep
        .verdicts
        .iter()
        .map(|v| v.reviewed_sha.as_str())
        .collect();
    assert!(
        shas.contains(&"head-1") && shas.contains(&"head-2"),
        "{:?}",
        shas
    );
}

#[test]
fn coverage_event_carries_the_repo_slug() {
    let rep = CoverageReport {
        github_approval_satisfies: false,
        coverage: Coverage::Covered(1),
        verdicts: vec![],
    };
    let data = coverage_event_data(
        781,
        &rep,
        "a3f4b413b",
        "github.com/bllshttng/footnote",
        None,
    );
    assert_eq!(
        data["repo"],
        serde_json::json!("github.com/bllshttng/footnote")
    );
    assert_eq!(data["pr"], serde_json::json!(781));
    assert_eq!(data["reviewed_count"], serde_json::json!(1));
}

#[test]
fn coverage_event_omits_repo_when_unresolvable() {
    // Omitted, not null: a reader scanning the shared cross-project log
    // must be able to tell "not attributed" from "attributed to nothing",
    // and decline to match it either way.
    let rep = CoverageReport {
        github_approval_satisfies: false,
        coverage: Coverage::Unknown,
        verdicts: vec![],
    };
    let data = coverage_event_data(781, &rep, "a3f4b413b", "", None);
    assert!(data.get("repo").is_none());
}

#[test]
fn coverage_emit_reaches_the_global_log() {
    // The specimen: the stop hook writes the events file of whatever
    // directory the session ran in, so an attestation made inside a
    // worktree never reached the log a merge from canonical reads. Every
    // other loop-check event already went to both.
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("worktree-events.jsonl");
    let global = dir.path().join("global-events.jsonl");
    let rep = CoverageReport {
        github_approval_satisfies: false,
        coverage: Coverage::Covered(1),
        verdicts: vec![],
    };
    emit_to_both(
        &project,
        &global,
        "review_coverage",
        coverage_event_data(
            781,
            &rep,
            "a3f4b413b",
            "github.com/bllshttng/footnote",
            None,
        ),
    );
    for path in [&project, &global] {
        let text = crate::events::committed_journal_text(path);
        assert!(text.contains("review_coverage"), "missing in {path:?}");
        assert!(
            text.contains("\"repo\":\"github.com/bllshttng/footnote\""),
            "unscoped in {path:?}"
        );
    }
}

#[test]
fn coverage_event_review_state_preserves_refusal_and_unknown_boundaries() {
    let login = ["chatgpt-codex-connector".to_string()];
    let fresh_at = fresh_at_head();

    let unreviewed = classify_coverage(&[], &[], "", &login, true, None, &fresh_at, "", "abc12345");
    let unreviewed_event = coverage_event_data(1, &unreviewed, "abc12345", "", None);
    assert_eq!(unreviewed_event["review_state"], "unreviewed");

    let refused = classify_coverage(
        &[],
        &[usage_comment("2026-08-17T03:00:00Z")],
        "",
        &login,
        true,
        None,
        &fresh_at,
        "",
        "abc12345",
    );
    let refused_event = coverage_event_data(1, &refused, "abc12345", "", None);
    assert_eq!(refused_event["review_state"], "reviewer_refused");

    let recovered = classify_coverage(
        &[],
        &[usage_comment("2026-08-17T03:00:00Z")],
        &attestation_line("code-review", "abc12345", "pass"),
        &login,
        true,
        Some("sess-author"),
        &fresh_at,
        "",
        "abc12345",
    );
    let recovered_event = coverage_event_data(1, &recovered, "abc12345", "", None);
    assert_eq!(recovered_event["review_state"], "reviewed");
    assert!(recovered_event["verdicts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["verdict"] == "refused"));

    let unknown = classify_coverage(&[], &[], "", &login, false, None, &fresh_at, "", "abc12345");
    let unknown_event = coverage_event_data(1, &unknown, "abc12345", "", None);
    assert_eq!(unknown_event["coverage"], "unknown");
    assert!(unknown_event.get("review_state").is_none());
}
