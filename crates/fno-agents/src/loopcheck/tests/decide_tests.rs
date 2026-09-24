use super::*;

#[test]
fn fire_budget_clamp_stays_under_remaining_and_above_the_floor() {
    let s = std::time::Duration::from_secs(30);
    // Budget-rich: the configured ceiling stands.
    assert_eq!(
        clamp_to_fire_budget(s, s + std::time::Duration::from_secs(1)),
        s
    );
    // Budget-poor: what remains stands.
    assert_eq!(
        clamp_to_fire_budget(s, std::time::Duration::from_millis(500)),
        std::time::Duration::from_millis(500)
    );
    // Budget spent: every read still gets a positive, killable bound.
    assert_eq!(
        clamp_to_fire_budget(s, std::time::Duration::ZERO),
        STOPGATE_BOUND_FLOOR
    );
}

#[test]
fn freshness_same_sha_is_fresh() {
    // No git facts needed at all: the reviewer read this exact commit.
    assert_eq!(
        review_freshness("abc123", "abc123", &FreshnessFacts::default()),
        Freshness::Fresh
    );
}

#[test]
fn freshness_shrunk_diff_carries_as_subset() {
    // The specimen shape: one doc sentence reworded, one test-file hunk
    // gone because main absorbed it. Every raw line still shipping was
    // read, so the shrunk diff carries instead of costing a re-review.
    let f = facts_lines(
        &[
            ":100644 100644 aaa bbb M\tcli/src/a.py",
            ":100644 100644 ccc ddd M\tcli/tests/test_a.py",
        ],
        &[":100644 100644 aaa bbb M\tcli/src/a.py"],
        Some(&["docs/x.md"]),
    );
    let verdict = review_freshness("r1", "h1", &f);
    assert_eq!(verdict, Freshness::CarriedSubset);
    assert!(verdict.counts());
}

#[test]
fn freshness_subset_does_not_swallow_equal_or_added_lines() {
    // Equal sets are not a strict subset (they hash equal and take the
    // tree-paths branch instead), and a HEAD line the reviewer never saw
    // is new unreviewed code: Stale.
    assert_eq!(
        review_freshness(
            "r",
            "h",
            &facts_lines(&["l1", "l2"], &["l1", "l2"], Some(&[]))
        ),
        Freshness::CarriedBaseSync
    );
    assert_eq!(
        review_freshness("r", "h", &facts_lines(&["l1"], &["l1", "l2"], None)),
        Freshness::Stale
    );
}

#[test]
fn freshness_absent_identity_stays_stale() {
    // An identity that failed to compute (None, which is also what an
    // empty code diff yields) can never carry - absence is not evidence.
    assert_eq!(
        review_freshness("r", "h", &facts(None, Some("i"), None)),
        Freshness::Stale
    );
    assert_eq!(
        review_freshness("r", "h", &facts(Some("i"), None, None)),
        Freshness::Stale
    );
}

#[test]
fn a_zero_file_pass_does_not_satisfy_the_reviewers_gate() {
    // The reviewers gate (config.review.reviewers, the self-review floor)
    // reads a separate scan; a review of nothing must not satisfy it
    // there either, only on the coverage axis.
    let zero = serde_json::json!({
        "ts": "2026-01-01T00:00:00Z", "source": "test",
        "type": "review_attestation",
        "data": {"reviewer": "code-review", "head_sha": "h", "verdict": "pass",
                 "attester_session_id": "sess-author",
                 "reviewed_line_count": 0, "reviewed_file_count": 0}
    })
    .to_string();
    let dir = std::env::temp_dir().join(format!(
        "fno-zero-file-scan-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let events = dir.join("events.jsonl");
    std::fs::write(&events, zero).unwrap();
    let reviewers = vec!["code-review".to_string()];
    let (unattested, _malformed) =
        unattested_reviewers_scan(&events, &reviewers, &|_| Freshness::Fresh, "", "h", false);
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        !unattested.is_empty(),
        "a zero-file pass must leave the reviewer unattested"
    );
    assert_eq!(unattested[0].name, "code-review");
}

#[test]
fn optional_staleness_never_disqualifies_local_recovery() {
    // The PR-1151 shape: a REQUIRED bot refused, a fresh local attestation
    // at HEAD, and an OPTIONAL bot whose verdict read an older commit.
    // Recovery must hold - an optional bot going stale is not a property
    // any reviewer owes. The stale optional verdict rides in COVERAGE (the
    // union axis), never in the required-only bot sets.
    let events = attestation_line_on_branch("code-review", "h", "pass", "feature/x");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &["chatgpt-codex-connector".to_string()],
        true,
        Some("sess-author"),
        &|_| Freshness::Fresh,
        "feature/x",
        "h",
    );
    assert_eq!(rep.coverage, Coverage::Covered(1));
    let refused = vec!["some-required-bot".to_string()];
    assert!(local_recovery_from_refusal(&refused, &[], &[], &rep));
}

#[test]
fn required_staleness_still_blocks_local_recovery() {
    // Unchanged, pinned: a stale REQUIRED verdict is one re-read from
    // counting, so recovery must not skip past it.
    let events = attestation_line_on_branch("code-review", "h", "pass", "feature/x");
    let rep = classify_coverage(
        &[],
        &[],
        &events,
        &[],
        true,
        Some("sess-author"),
        &|_| Freshness::Fresh,
        "feature/x",
        "h",
    );
    let refused = vec!["some-required-bot".to_string()];
    let stale = vec![("some-required-bot".to_string(), "oldsha".to_string())];
    assert!(!local_recovery_from_refusal(&refused, &[], &stale, &rep));
    // And no refusal means no recovery at all: absence is a wait.
    assert!(!local_recovery_from_refusal(&[], &[], &[], &rep));
}

#[test]
fn required_reviewer_gate_honors_the_same_carry() {
    // The N-reachable-paths check. `config.review.reviewers` is satisfied
    // by a DIFFERENT scan than the coverage count, so a carry granted to
    // one and refused by the other leaves the gate exactly as tight as
    // before and the softening purely decorative.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("events.jsonl");
    std::fs::write(
        &p,
        attestation_line_on_branch("code-review", "oldhead", "pass", "feature/x") + "\n",
    )
    .unwrap();
    let reviewers = vec!["code-review".to_string()];

    let carried = unattested_reviewers_scan(
        &p,
        &reviewers,
        &|_| Freshness::CarriedBaseSync,
        "feature/x",
        "currenthead",
        false,
    )
    .0;
    assert!(
        carried.is_empty(),
        "a carried attestation must satisfy the gate"
    );

    let stale = unattested_reviewers_scan(
        &p,
        &reviewers,
        &|_| Freshness::Stale,
        "feature/x",
        "currenthead",
        false,
    )
    .0;
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].superseded_head.as_deref(), Some("oldhead"));
}

#[test]
fn block_reason_pending_ci_is_not_red() {
    // The MUTE_PROBE_N probe runs done() while CI is often still in
    // flight; a Pending conclusion must read as "still running", never
    // as the misleading "CI red ... failed" (observed live on PR #455).
    let pr = PrInfo {
        state: PrState::Open,
        number: 455,
        head_oid: "abc".to_string(),
        ci_conclusion: CiConclusion::Pending,
        mergeable: "UNKNOWN".to_string(),
        ..PrInfo::default()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(
        reason.contains("still running"),
        "pending CI must not read as red; got: {reason}"
    );
    assert!(!reason.contains("failed"), "got: {reason}");
}

#[test]
fn unknown_coverage_names_the_read_remedy_not_a_ci_failure() {
    let mut pr = watch_pr();
    pr.ci_conclusion = CiConclusion::Success;
    pr.ci_has_pending = false;
    pr.coverage = CoverageReport {
        github_approval_satisfies: false,
        coverage: Coverage::Unknown,
        verdicts: vec![],
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(
        reason.contains("coverage read unavailable"),
        "got: {reason}"
    );
    assert!(reason.contains("retry the review verb"), "got: {reason}");
    assert!(!reason.contains("CI red"), "got: {reason}");
    assert!(!reason.contains("failed"), "got: {reason}");
    assert!(!reason.contains("Read the failing log"), "got: {reason}");
    assert!(!reason.contains("fno do pr logs"), "got: {reason}");
}

#[test]
fn unwatched_async_nudge_ci_pending_teaches_arm_and_tag() {
    // AC3-HP: the CI-pending block message must instruct arming a
    // harness-tracked watcher with a timeout and emitting <watching>,
    // replacing the old "wait silently" prose.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        ci_conclusion: CiConclusion::Pending,
        ci_has_pending: true,
        ..watch_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("<watching"), "got: {reason}");
    assert!(reason.contains("timeout"), "got: {reason}");
    // The taught watcher is the sanctioned REST wait verb, never the
    // GraphQL `gh pr checks --watch` and never an inline loop (which a
    // worktree session's Bash isolation refuses as too complex).
    assert!(reason.contains("fno do pr wait"), "got: {reason}");
    assert!(!reason.contains("gh pr checks"), "got: {reason}");
    assert!(!reason.contains("while ["), "got: {reason}");
    assert!(!reason.contains("wait silently"), "got: {reason}");
}

#[test]
fn no_hint_prescribes_the_timeout_binary() {
    // File-wide, so a future hint cannot reintroduce `timeout(1)` at a site
    // this test does not name. The needle is built at runtime so the test
    // does not match its own source.
    let needle = ["timeout", " "].concat();
    for tail in super::production_source().split(&needle).skip(1) {
        assert!(
            !tail.trim_start().starts_with(|c: char| c.is_ascii_digit()),
            "bare timeout invocation: ...{}",
            tail.chars().take(60).collect::<String>()
        );
    }
}

#[test]
fn the_bypass_detector_catches_an_injected_bypass() {
    // The detector itself is under test: a fixture with ONE new direct
    // wait must be named, proving a green production scan is a scan that
    // ran and matched the right symbol - not an empty haystack.
    let fixture = "fn somewhere() {
    let out = Command::new(gh_bin)
        .args([\"pr\", \"view\"])
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;
}
fn run_bounded() {}
bounded_read(); bounded_read();";
    let hits = direct_wait_bypasses(fixture);
    assert_eq!(hits.len(), 1, "exactly the injected bypass: {hits:?}");
    assert!(hits[0].contains("Command::new(gh_bin"), "{hits:?}");
    // The git marker is under test too: a direct git wait is the same
    // hang shape with a different binary, and the detector must name it.
    let git_fixture = "fn g() {
    let out = Command::new(git_bin)
        .args([\"status\"])
        .output()?;
}
fn run_bounded() {}
git_bounded();";
    let git_hits = direct_wait_bypasses(git_fixture);
    assert_eq!(
        git_hits.len(),
        1,
        "exactly the injected git bypass: {git_hits:?}"
    );
    assert!(git_hits[0].contains("Command::new(git_bin"), "{git_hits:?}");
    // And the wait call is what flagged it, not the spawn shape: the same
    // fixture without the wait passes clean.
    let clean = fixture.replace(".output()", ".spawn()");
    assert!(direct_wait_bypasses(&clean).is_empty());
}

#[test]
fn unwatched_async_nudge_missing_review_teaches_arm_and_tag() {
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        ci_conclusion: CiConclusion::Success,
        ci_has_pending: false,
        reviewed: false,
        missing_bots: vec!["chatgpt-codex-connector".into()],
        bot_nudges: vec![],
        ..watch_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("chatgpt-codex-connector"), "got: {reason}");
    assert!(reason.contains("<watching"), "got: {reason}");
}

/// the terminal fires only when the quota bounce is the SOLE unmet
/// conjunct of `reviewed`. Each case drops one other conjunct and must
/// block; reverting `awaiting_review_only` to the bare
/// `!usage_limited.is_empty()` check fails them.
#[test]
fn awaiting_review_only_requires_every_other_conjunct() {
    let bounced = || {
        let mut pr = watch_pr();
        pr.coverage = CoverageReport {
            github_approval_satisfies: false,
            coverage: Coverage::Covered(0),
            verdicts: vec![ReviewerVerdict {
                producer: CoverageProducer::GithubApp,
                name: "chatgpt-codex-connector".to_string(),
                verdict: CoverageVerdict::Refused,
                human_approval: false,
                author_approval: false,
                attestation_origin: AttestationOrigin::Unknown,
                reviewed_sha: String::new(),
                freshness: None,
                scope: None,
                refusal_reason: None,
                reviewer_context: None,
                required: true,
                passed: false,
            }],
        };
        pr
    };
    assert!(awaiting_review_only(&bounced()), "the terminal's own case");
    assert!(!awaiting_review_only(&watch_pr()), "no bounce to report");

    // A bot that has not reviewed YET is owed its nudge window: one bot's
    // quota state must not end the session on the others' behalf.
    let mut still_pending = bounced();
    still_pending.missing_bots = vec!["gemini-code-assist".to_string()];
    assert!(
        !awaiting_review_only(&still_pending),
        "bot still owed a wait"
    );

    // A stale bot owes a re-read: the same window a missing bot
    // gets, never a clean exit on another bot's quota bounce.
    let mut stale_pending = bounced();
    stale_pending.stale_bots = vec![("gemini-code-assist".to_string(), "00001111".to_string())];
    assert!(
        !awaiting_review_only(&stale_pending),
        "stale bot still owed a re-read"
    );

    // A standing blocking finding is work the agent must DO; parking hands
    // a human a PR carrying an unaddressed P1.
    let mut with_finding = bounced();
    with_finding.unaddressed_findings = vec![Finding {
        id: 1,
        author: "gemini-code-assist".to_string(),
        path: "src/lib.rs".to_string(),
        line: 12,
        created_at: "2026-08-06T00:00:00Z".to_string(),
        severity: "P1",
        had_reply: true,
    }];
    assert!(!awaiting_review_only(&with_finding), "unaddressed P1");

    let mut unattested = bounced();
    unattested.unattested_reviewers = vec![UnattestedReviewer {
        name: "sigma".to_string(),
        superseded_head: None,
        failed_at_head: false,
    }];
    assert!(!awaiting_review_only(&unattested), "local review never ran");
}

#[test]
fn watch_idle_classifies_pending_ci() {
    assert_eq!(async_wait_class(&watch_pr(), true, true), Some("ci"));
}

#[test]
fn codex_watch_harness_gate_is_claude_only() {
    // Only Claude self-wakes on a background-task exit, so only Claude idles.
    assert!(harness_can_idle(Some("claude"), false));
    // A loop-run child (FNO_DRIVER_LIB) exits on allow -> never idles.
    assert!(!harness_can_idle(Some("claude"), true));
    // codex/gemini have no self-wake; their daemon-consumer waker ships
    // separately, so until then they keep today's block behavior.
    assert!(!harness_can_idle(Some("codex"), false));
    assert!(!harness_can_idle(Some("gemini"), false));
    // Unknown harness (bare shell / daemon): conservative block.
    assert!(!harness_can_idle(None, false));
}

#[test]
fn watching_refusal_names_the_disqualifying_substrate() {
    assert_eq!(
        watch_lease::watching_harness_refusal(Some("claude"), true),
        "watching ignored: loop-run child cannot idle"
    );
    assert_eq!(
        watch_lease::watching_harness_refusal(Some("codex"), false),
        "watching ignored: harness codex cannot idle"
    );
}

#[test]
fn watch_idle_classifies_awaiting_review() {
    // CI green, no pending checks, and a required GitHub bot has not reviewed.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        ci_conclusion: CiConclusion::Success,
        ci_has_pending: false,
        reviewed: false,
        review_skipped: false,
        missing_bots: vec!["chatgpt-codex-connector".into()],
        bot_nudges: vec![],
        ..watch_pr()
    };
    assert_eq!(async_wait_class(&pr, true, true), Some("review"));
}

#[test]
fn watch_idle_rejects_ci_pending_with_a_failure() {
    // gemini finding: a check has ALREADY concluded red while others run.
    // The agent should debug now, not idle out the remaining pending checks.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        ci_conclusion: CiConclusion::Failure(Some("unit".into())),
        ci_has_pending: true,
        ..watch_pr()
    };
    assert_eq!(async_wait_class(&pr, true, true), None);
}

#[test]
fn watch_idle_rejects_local_attestation_review_gate() {
    // codex P1: reviewed=false with an EMPTY missing_bots is a local
    // attestation (sigma) or unaddressed-finding gate - no GitHub reviewer
    // will ever post to wake the session, so idling would park it forever.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        ci_conclusion: CiConclusion::Success,
        ci_has_pending: false,
        reviewed: false,
        review_skipped: false,
        missing_bots: vec![],
        bot_nudges: vec![],
        ..watch_pr()
    };
    assert_eq!(async_wait_class(&pr, true, true), None);
}

#[test]
fn nudge_needs_nudge_blocks_and_names_the_command() {
    // AC1: not idlable; reason gives the exact gh command; no arm-and-tag hint.
    let pr = bot_review_pr(
        "chatgpt-codex-connector",
        vec![bn(
            "chatgpt-codex-connector",
            NudgeClass::NeedsNudge,
            0,
            0,
            0,
        )],
    );
    assert_eq!(async_wait_class(&pr, true, true), None);
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(
        reason.contains("gh pr comment 618 --body \"@codex review\""),
        "{reason}"
    );
    assert!(
        !reason.contains("harness-tracked watcher"),
        "no arm hint: {reason}"
    );
}

#[test]
fn nudge_awaiting_idles_with_the_arm_hint() {
    // AC2: a genuine async wait - idlable, message says nudged + awaiting,
    // and the arm-and-tag ritual is present.
    let pr = bot_review_pr(
        "chatgpt-codex-connector",
        vec![bn("chatgpt-codex-connector", NudgeClass::Awaiting, 1, 3, 3)],
    );
    assert_eq!(async_wait_class(&pr, true, true), Some("review"));
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("nudged"), "{reason}");
    assert!(reason.contains("awaiting"), "{reason}");
    assert!(
        reason.contains("harness-tracked watcher"),
        "arm hint present: {reason}"
    );
}

#[test]
fn nudge_unresponsive_blocks_and_names_optional_apps() {
    // AC3: not idlable; names the give-up + optional_apps; no arm-and-tag hint.
    let pr = bot_review_pr(
        "chatgpt-codex-connector",
        vec![bn(
            "chatgpt-codex-connector",
            NudgeClass::Unresponsive,
            3,
            20,
            47,
        )],
    );
    assert_eq!(async_wait_class(&pr, true, true), None);
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(
        reason.contains("did not review after 3 nudges over 47m"),
        "{reason}"
    );
    assert!(reason.contains("config.review.optional_apps"), "{reason}");
    assert!(reason.contains("do not arm a watcher"), "{reason}");
    assert!(
        !reason.contains("harness-tracked watcher"),
        "no arm hint: {reason}"
    );
}

#[test]
fn nudge_not_nudgeable_keeps_todays_behavior() {
    // AC5: a non-nudgeable required bot keeps today's string + arm hint and
    // stays idlable, regardless of comment history.
    let pr = bot_review_pr(
        "gemini-code-assist",
        vec![bn("gemini-code-assist", NudgeClass::NotNudgeable, 0, 0, 0)],
    );
    assert_eq!(async_wait_class(&pr, true, true), Some("review"));
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(
        reason.contains("gemini-code-assist has not reviewed"),
        "{reason}"
    );
    assert!(
        reason.contains("harness-tracked watcher"),
        "arm hint present: {reason}"
    );
}

#[test]
fn nudge_empty_classification_is_status_quo() {
    // A non-empty missing_bots with an EMPTY bot_nudges (not classified)
    // behaves exactly as pre-: idlable, today's string.
    let pr = bot_review_pr("chatgpt-codex-connector", vec![]);
    assert_eq!(async_wait_class(&pr, true, true), Some("review"));
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("has not reviewed"), "{reason}");
}

#[test]
fn stale_bot_needs_nudge_keeps_the_trigger_command() {
    // A stale bot classified NeedsNudge needs the concrete remedy the
    // nudge branch carries (the trigger command, the counters), decorated
    // with what it read - not the generic re-read sentence.
    let mut pr = bot_review_pr(
        "chatgpt-codex-connector",
        vec![bn(
            "chatgpt-codex-connector",
            NudgeClass::NeedsNudge,
            0,
            0,
            0,
        )],
    );
    pr.missing_bots = vec![];
    pr.stale_bots = vec![(
        "chatgpt-codex-connector".to_string(),
        "deadbeef1234".to_string(),
    )];
    assert_eq!(async_wait_class(&pr, true, true), None);
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("gh pr comment 618"), "{reason}");
    assert!(
        reason.contains("(read deadbeef, superseded by this head)"),
        "{reason}"
    );
}

#[test]
fn stale_bot_block_reason_names_the_read_sha_and_a_reread() {
    // AC1: the bot DID respond, so the message must not read as a
    // first read - it names the commit it read and asks for a re-read.
    // Still idles like any nudgeable outstanding bot.
    let mut pr = bot_review_pr(
        "chatgpt-codex-connector",
        vec![bn(
            "chatgpt-codex-connector",
            NudgeClass::NotNudgeable,
            0,
            0,
            0,
        )],
    );
    pr.missing_bots = vec![];
    pr.stale_bots = vec![(
        "chatgpt-codex-connector".to_string(),
        "0daa4d6cea7c".to_string(),
    )];
    assert_eq!(async_wait_class(&pr, true, true), Some("review"));
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(
        reason.contains("chatgpt-codex-connector (read 0daa4d6c, superseded by this head)"),
        "{reason}"
    );
    assert!(reason.contains("ask for a re-read"), "{reason}");
    assert!(!reason.contains("has not reviewed"), "{reason}");
}

#[test]
fn nudge_message_for_a_stale_bot_appends_what_it_read() {
    // Mixed set: gemini never responded, codex read an older commit. The
    // Awaiting message names codex, so it must carry the read-note suffix
    // rather than read as "never responded".
    let mut pr = bot_review_pr(
        "gemini-code-assist",
        vec![
            bn("gemini-code-assist", NudgeClass::NotNudgeable, 0, 0, 0),
            bn("chatgpt-codex-connector", NudgeClass::Awaiting, 1, 3, 3),
        ],
    );
    pr.stale_bots = vec![(
        "chatgpt-codex-connector".to_string(),
        "111122223333".to_string(),
    )];
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(
        reason.contains("chatgpt-codex-connector (read 11112222, superseded by this head)"),
        "{reason}"
    );
}

#[test]
fn finding_block_reason_names_the_reply_handle() {
    // AC14: an unaddressed finding by a known bot names the handle a reply
    // must address, not just "reply in-thread".
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        ci_conclusion: CiConclusion::Success,
        ci_has_pending: false,
        reviewed: false,
        unaddressed_findings: vec![Finding {
            id: 1,
            author: "chatgpt-codex-connector".into(),
            path: "a.rs".into(),
            line: 10,
            created_at: "2026-07-06T01:00:00Z".into(),
            severity: "P1",
            had_reply: true,
        }],
        ..watch_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("@chatgpt-codex-connector"), "{reason}");
}

#[test]
fn block_reason_names_the_reviewers_gate_not_a_bot() {
    // AC2: the old string claimed a bot had not reviewed while
    // required_bots was empty and the real blocker was local. sigma is
    // retired, so the unmet reason names the lane, never a panel run.
    let reason = build_block_reason(&reviewers_gate_pr(), "abc", true, true);
    assert!(reason.contains("reviewers gate unmet"), "got: {reason}");
    assert!(reason.contains("sigma"), "got: {reason}");
    assert!(reason.contains("/fno:review"), "got: {reason}");
    assert!(!reason.contains("/fno:review sigma"), "got: {reason}");
    assert!(!reason.contains("bot reviewer"), "got: {reason}");
}

#[test]
fn block_reason_names_the_local_peer_invocation() {
    let mut pr = reviewers_gate_pr();
    pr.unattested_reviewers[0].name = LOCAL_PEER_REVIEWER.to_string();
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(
        reason.contains("/fno:review peer --attest"),
        "got: {reason}"
    );
    assert!(
        !reason.contains("wait on a GitHub reviewer"),
        "got: {reason}"
    );
}

#[test]
fn block_reason_explains_same_model_local_peer_refusal() {
    let mut pr = reviewers_gate_pr();
    pr.unattested_reviewers[0].name = SAME_MODEL_LOCAL_PEER_SENTINEL.to_string();
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(
        reason.contains("configure a cross-model peer"),
        "got: {reason}"
    );
    assert!(
        !reason.contains(SAME_MODEL_LOCAL_PEER_SENTINEL),
        "got: {reason}"
    );
}

#[test]
fn block_reason_reviewers_gate_emits_no_idle_ritual() {
    // AC3: async_wait_class already excluded this blocker from idling
    // (watch_idle_rejects_local_attestation_review_gate), so prescribing
    // the arm-and-tag ritual here is the code contradicting itself.
    let pr = reviewers_gate_pr();
    assert_eq!(async_wait_class(&pr, true, true), None);
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(!reason.contains("<watching"), "got: {reason}");
    assert!(
        !reason.contains("Arm a harness-tracked watcher"),
        "got: {reason}"
    );
    assert!(!reason.contains("gh pr checks"), "got: {reason}");
}

#[test]
fn block_reason_names_a_superseded_attestation_head() {
    // A session that ran sigma and then pushed must not read "you never
    // ran sigma"; name the head the pass is pinned to.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        unattested_reviewers: vec![UnattestedReviewer {
            name: "sigma".to_string(),
            superseded_head: Some("0123456789abcdef".to_string()),
            failed_at_head: false,
        }],
        ..reviewers_gate_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("01234567"), "got: {reason}");
    assert!(reason.contains("superseded"), "got: {reason}");
}

#[test]
fn block_reason_generic_review_fallback_has_no_idle_ritual() {
    // The fallback is only reachable with an EMPTY missing_bots, which
    // async_wait_class refuses to idle. It must not teach the ritual either.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        unattested_reviewers: vec![],
        ..reviewers_gate_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(!reason.contains("<watching"), "got: {reason}");
    assert!(!reason.contains("bot reviewer"), "got: {reason}");
}

#[test]
fn block_reason_missing_bot_still_teaches_the_ritual() {
    // AC7-adjacent regression: a REAL outstanding GitHub bot, and nothing
    // local outstanding, is a valid async wait and keeps today's message.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        missing_bots: vec!["chatgpt-codex-connector".into()],
        bot_nudges: vec![],
        unattested_reviewers: vec![],
        ..reviewers_gate_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("chatgpt-codex-connector"), "got: {reason}");
    assert!(reason.contains("<watching"), "got: {reason}");
}

#[test]
fn block_reason_local_work_outranks_a_bot_wait() {
    // Codex review of this PR: with a bot AND a local reviewer both
    // outstanding, naming only the bot hides the half the session can act
    // on now. Worse, if the bot never posts, the local work never happens
    // and the run dies on budget with the gate still unmet - the #618 shape
    // this node exists to delete.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        missing_bots: vec!["chatgpt-codex-connector".into()],
        bot_nudges: vec![],
        ..reviewers_gate_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("reviewers gate unmet"), "got: {reason}");
    assert!(!reason.contains("<watching"), "got: {reason}");
    // Once the local half is attested, the bot wait is the sole blocker and
    // the arm-and-tag message returns.
    let after = PrInfo {
        range_tiling: RangeTiling::default(),
        unattested_reviewers: vec![],
        ..pr
    };
    assert!(build_block_reason(&after, "abc", true, true).contains("<watching"));
}

#[test]
fn reviewers_gate_stays_fail_closed() {
    // AC7: promoting the predicate's return value to a list must not move
    // the gate. Missing file, missing event, stale head, and a `fail`
    // verdict all still leave the reviewer unsatisfied.
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("absent.jsonl");
    let sigma = vec!["sigma".to_string()];
    assert!(!unattested_reviewers(&missing, &sigma, "h", "").is_empty());

    let stale = tmp.path().join("stale.jsonl");
    std::fs::write(&stale, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass","branch":"feature/x"}}"#)).unwrap();
    let out = unattested_reviewers(&stale, &sigma, "NEW", "feature/x");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].superseded_head.as_deref(), Some("OLD"));
    assert!(!out[0].failed_at_head);

    let failed = tmp.path().join("fail.jsonl");
    std::fs::write(&failed, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#)).unwrap();
    let out = unattested_reviewers(&failed, &sigma, "h", "");
    assert_eq!(out.len(), 1);
    // A head-pinned fail is not a superseded pass; do not offer a stale head.
    assert_eq!(out[0].superseded_head, None);
    // ...but it IS an attestation at this head, and the message says so
    // rather than claiming none exists. Pinned at the PARSER: the message
    // test hand-builds the struct and never exercises this derivation, so
    // `failed_at_head: false` survived the whole suite before this line.
    assert!(
        out[0].failed_at_head,
        "a fail at HEAD must be reported as such"
    );
}

#[test]
fn unpinned_attestation_never_counts_as_evidence() {
    // codex P1 on this PR: defaulting a missing head_sha to "" made an
    // unpinned event MATCH a caller whose own head_sha is "", turning
    // no-evidence into a pass.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("e.jsonl");
    std::fs::write(
            &p,
            format!(
                "{}\n",
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","verdict":"pass"}}"#
            ),
        )
        .unwrap();
    let out = unattested_reviewers(&p, &["sigma".to_string()], "", "");
    assert_eq!(out.len(), 1, "unpinned evidence must not satisfy the gate");
    assert_eq!(out[0].superseded_head, None);
}

#[test]
fn a_failed_old_head_is_not_reported_as_superseded() {
    // codex P2: "attested at X, superseded" implies a prior PASS. An
    // old-head fail rendered that way invents a review that never passed.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("e.jsonl");
    std::fs::write(&p, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"fail","branch":"feature/x"}}"#)).unwrap();
    let out = unattested_reviewers(&p, &["sigma".to_string()], "NEW", "feature/x");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].superseded_head, None);
}

#[test]
fn a_corrupt_attestation_line_is_counted_and_named() {
    // A torn write leaves an unparseable review_attestation in the file.
    // The gate must still fail closed, but reporting "no head-pinned
    // review_attestation" over a corrupt one is the same class of lie this
    // node deletes. The sibling review_finding scanner already counts its
    // malformed lines; this one did not.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("e1.jsonl");
    std::fs::write(
            &p,
            concat!(
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","hea"#,
                "\n",
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"loop_check","data":{}}"#,
            ),
        )
        .unwrap();
    let (out, malformed) = unattested_reviewers_scan(
        &p,
        &["sigma".to_string()],
        &sha_equality_freshness("h"),
        "",
        "h",
        false,
    );
    assert_eq!(out.len(), 1, "a corrupt line never satisfies the gate");
    assert_eq!(malformed, 1, "and it is counted, not silently dropped");

    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        malformed_attestations: malformed,
        ..reviewers_gate_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(
        reason.contains("unparseable attestation line"),
        "got: {reason}"
    );

    // A clean file adds nothing to the message.
    let p = tmp.path().join("e2.jsonl");
    std::fs::write(
        &p,
        r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"loop_check","data":{}}"#,
    )
    .unwrap();
    assert_eq!(
        unattested_reviewers_scan(
            &p,
            &["sigma".to_string()],
            &sha_equality_freshness("h"),
            "",
            "h",
            false
        )
        .1,
        0
    );
    assert!(!build_block_reason(&reviewers_gate_pr(), "abc", true, true)
        .contains("unparseable attestation line"));
}

#[test]
fn a_revoked_pass_falls_back_to_an_older_passing_head() {
    // codex P2: `pass A, pass B, fail B` with HEAD C. A single "most recent
    // pass" entry overwrites A with B and then drops B, so the message
    // claims no prior pass while A is still a real one - the misleading
    // guidance this whole node exists to delete, reappearing in exactly the
    // multi-round review/fix cycle that produces this sequence.
    let tmp = tempfile::tempdir().unwrap();
    // One journal per scenario: a rewrite would leak rows across them.
    let p = tmp.path().join("e1.jsonl");
    let line = |head: &str, verdict: &str| {
        format!(
            r#"{{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{{"reviewer":"sigma","head_sha":"{head}","verdict":"{verdict}","branch":"feature/x"}}}}"#
        )
    };
    std::fs::write(
        &p,
        [
            line("AAA", "pass"),
            line("BBB", "pass"),
            line("BBB", "fail"),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let out = unattested_reviewers(&p, &["sigma".to_string()], "CCC", "feature/x");
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].superseded_head.as_deref(),
        Some("AAA"),
        "a still-valid older pass must survive a newer head's retraction"
    );

    // The newest STILL-PASSING head wins when several are valid.
    let p = tmp.path().join("e2.jsonl");
    std::fs::write(
        &p,
        [line("AAA", "pass"), line("BBB", "pass")].join("\n") + "\n",
    )
    .unwrap();
    let out = unattested_reviewers(&p, &["sigma".to_string()], "CCC", "feature/x");
    assert_eq!(out[0].superseded_head.as_deref(), Some("BBB"));

    // Every old head retracted -> nothing to name.
    let p = tmp.path().join("e3.jsonl");
    std::fs::write(
        &p,
        [
            line("AAA", "pass"),
            line("BBB", "pass"),
            line("BBB", "fail"),
            line("AAA", "fail"),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let out = unattested_reviewers(&p, &["sigma".to_string()], "CCC", "feature/x");
    assert_eq!(out[0].superseded_head, None);
}

#[test]
fn a_later_fail_revokes_the_superseded_pass_for_that_head() {
    // Append-ordered pass-then-fail on the SAME old head. The pass was
    // recorded as superseded and the fail merely skipped, so the message
    // kept claiming that head was successfully attested after its latest
    // verdict retracted exactly that (codex P2 on this PR).
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("e1.jsonl");
    std::fs::write(
            &p,
            concat!(
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass","branch":"feature/x"}}"#,
                "\n",
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"fail","branch":"feature/x"}}"#,
                "\n",
            ),
        )
        .unwrap();
    let out = unattested_reviewers(&p, &["sigma".to_string()], "NEW", "feature/x");
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].superseded_head, None,
        "a retracted pass is not evidence"
    );

    // A re-run pass after the fail restores it: revocation is latest-wins,
    // not a one-way latch.
    std::fs::write(
            &tmp.path().join("e2.jsonl"),
            concat!(
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass","branch":"feature/x"}}"#,
                "\n",
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"fail","branch":"feature/x"}}"#,
                "\n",
                // Distinct ts: byte-dedupe would drop an identical row.
                r#"{"ts":"2026-01-01T00:00:01Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass","branch":"feature/x"}}"#,
                "\n",
            ),
        )
        .unwrap();
    let out = unattested_reviewers(
        &tmp.path().join("e2.jsonl"),
        &["sigma".to_string()],
        "NEW",
        "feature/x",
    );
    assert_eq!(out[0].superseded_head.as_deref(), Some("OLD"));
}

#[test]
fn short_sha_never_panics_on_multibyte() {
    // codex P2: `&s[..8]` panics when byte 8 lands inside a character, and
    // superseded_head comes from a user-writable events.jsonl.
    assert_eq!(short_sha("0123456789ab"), "01234567");
    assert_eq!(short_sha("abc"), "abc");
    assert_eq!(short_sha(""), "");
    assert_eq!(short_sha("1234567\u{e9}xyz"), "1234567\u{e9}");
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        unattested_reviewers: vec![UnattestedReviewer {
            name: "sigma".to_string(),
            superseded_head: Some("1234567\u{e9}abc".to_string()),
            failed_at_head: false,
        }],
        ..reviewers_gate_pr()
    };
    build_block_reason(&pr, "1234567\u{e9}abc", true, true);
}

#[test]
fn watcher_hint_never_contradicts_the_idle_classifier() {
    // codex P1: the missing-bot branch emitted the ritual unconditionally,
    // including for states async_wait_class refuses to idle (an unaddressed
    // finding, or an open operator finding). The hint is now derived from
    // that same classifier, so the two agree by construction.
    let bot_only = PrInfo {
        range_tiling: RangeTiling::default(),
        missing_bots: vec!["chatgpt-codex-connector".into()],
        bot_nudges: vec![],
        unattested_reviewers: vec![],
        ..reviewers_gate_pr()
    };
    for (label, pr, open_empty) in [
        // Reaches the FINDINGS branch, not the bot branch: unaddressed
        // findings render first. Kept because the invariant under test is
        // "no hint for a non-idlable state", which holds branch-wide - but
        // it is the case below that reaches `missing_bots`, so that one is
        // what would catch a reintroduced unconditional `arm_watch_hint`
        // there. A sigma round caught this test silently losing its teeth
        // when the branch order moved out from under it.
        (
            "bot + unaddressed finding (renders as the finding)",
            PrInfo {
                range_tiling: RangeTiling::default(),
                missing_bots: vec!["chatgpt-codex-connector".into()],
                bot_nudges: vec![],
                unattested_reviewers: vec![],
                unaddressed_findings: vec![Finding {
                    id: 1,
                    author: "codex".into(),
                    path: "a.rs".into(),
                    line: 1,
                    created_at: "2026-07-27T00:00:00Z".into(),
                    severity: "P1",
                    had_reply: true,
                }],
                ..reviewers_gate_pr()
            },
            true,
        ),
        (
            // The one that DOES reach `missing_bots` while non-idlable.
            "bot + open operator finding",
            PrInfo {
                range_tiling: RangeTiling::default(),
                missing_bots: vec!["chatgpt-codex-connector".into()],
                bot_nudges: vec![],
                unattested_reviewers: vec![],
                ..reviewers_gate_pr()
            },
            false,
        ),
    ] {
        let reason = build_block_reason(&pr, "abc", open_empty, true);
        assert_eq!(async_wait_class(&pr, open_empty, true), None, "{label}");
        assert!(!reason.contains("<watching"), "{label}: {reason}");
    }
    // The genuinely idlable state keeps the ritual.
    assert_eq!(async_wait_class(&bot_only, true, true), Some("review"));
    assert!(build_block_reason(&bot_only, "abc", true, true).contains("<watching"));
}

#[test]
fn an_unaddressed_finding_is_named_before_the_reviewers_gate() {
    // Sigma review of this PR: addressing an inline finding MOVES HEAD,
    // which supersedes any attestation produced first. Naming the reviewer
    // first would make the session run the panel twice.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        unaddressed_findings: vec![Finding {
            id: 1,
            author: "codex".into(),
            path: "a.rs".into(),
            line: 7,
            created_at: "2026-07-27T00:00:00Z".into(),
            severity: "P1",
            had_reply: true,
        }],
        ..reviewers_gate_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("unaddressed"), "got: {reason}");
    assert!(!reason.contains("reviewers gate unmet"), "got: {reason}");
    // With the finding cleared, the reviewers gate is what is named.
    let after = PrInfo {
        range_tiling: RangeTiling::default(),
        unaddressed_findings: vec![],
        ..pr
    };
    assert!(build_block_reason(&after, "abc", true, true).contains("reviewers gate unmet"));
}

#[test]
fn unaddressed_finding_with_no_reply_names_the_top_level_blind_spot() {
    // A finding answered with a top-level PR comment reads as unaddressed
    // because the gate walks in_reply_to_id chains only. The block reason
    // must name the mechanism and the exact gh command, not the ambiguous
    // "reply in-thread" a worker who posted a top-level comment reads as
    // "I did reply" (PR #447, #787 both stalled green PRs this way).
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        unaddressed_findings: vec![Finding {
            id: 1,
            author: "codex".into(),
            path: "a.rs".into(),
            line: 7,
            created_at: "2026-07-27T00:00:00Z".into(),
            severity: "P1",
            had_reply: false,
        }],
        ..reviewers_gate_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("no in-thread reply"), "got: {reason}");
    assert!(reason.contains("in_reply_to_id"), "got: {reason}");
    assert!(reason.contains("top-level PR comment"), "got: {reason}");
    assert!(reason.contains("in_reply_to=<id>"), "got: {reason}");
}

#[test]
fn a_failed_attestation_at_this_head_is_not_reported_as_absent() {
    // "no head-pinned review_attestation" reads as "you never ran it" to a
    // session that ran the reviewer and was told no.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        unattested_reviewers: vec![UnattestedReviewer {
            name: "sigma".to_string(),
            superseded_head: None,
            failed_at_head: true,
        }],
        ..reviewers_gate_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("verdict NOT pass"), "got: {reason}");
}

#[test]
fn the_stop_gate_marks_declare_as_a_self_cert() {
    // AC5: every surface that prints `declare` says it asserts nothing.
    // The Rust block message is such a surface.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        unattested_reviewers: vec![UnattestedReviewer {
            name: "declare".to_string(),
            superseded_head: None,
            failed_at_head: false,
        }],
        ..reviewers_gate_pr()
    };
    let reason = build_block_reason(&pr, "abc", true, true);
    assert!(reason.contains("self-cert"), "got: {reason}");
    assert!(
        reason.contains("asserts no review evidence"),
        "got: {reason}"
    );
    // A real reviewer carries no such mark.
    assert!(!build_block_reason(&reviewers_gate_pr(), "abc", true, true).contains("self-cert"));
}

#[test]
fn an_empty_head_sha_never_becomes_a_superseded_head() {
    // Option<String> cannot say "non-empty", so normalize at construction
    // rather than leaving is_empty() as a convention every reader re-derives.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("e.jsonl");
    std::fs::write(&p, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"","verdict":"pass"}}"#)).unwrap();
    let out = unattested_reviewers(&p, &["sigma".to_string()], "NEW", "");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].superseded_head, None);
}

#[test]
fn an_outstanding_local_reviewer_is_never_an_idlable_wait() {
    // The classifier half of the same fix: with a bot AND a local reviewer
    // outstanding, a stray <watching> tag must not park the session on work
    // it could do now.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        missing_bots: vec!["chatgpt-codex-connector".into()],
        bot_nudges: vec![],
        ..reviewers_gate_pr()
    };
    assert_eq!(async_wait_class(&pr, true, true), None);
}

/// A degraded `gh pr view` can return an open PR with no `headRefOid`.
/// `head_is_shipped` answers false for that, so a bare `!head_shipped`
/// would render the push message with a blank sha and hide the real
/// blocker. The `is_empty` guard in front of it is what prevents that.
#[test]
fn block_reason_never_demands_a_push_for_a_pr_with_no_recorded_head() {
    let pr = shipped_pr("", PrState::Open);
    let reason = build_block_reason(&pr, "fe407c3b", true, false);
    assert!(
        !reason.contains("push the latest commits"),
        "an empty recorded head must fall through to the real blocker: {reason}"
    );
    assert!(!reason.is_empty(), "a reason is still rendered: {reason}");
}

#[test]
fn block_reason_stops_demanding_a_push_for_a_shipped_head() {
    // The wedge, end to end. Positive marker AND absence: a panicking
    // renderer would also fail to print the push message.
    let pr = shipped_pr("23480a0e", PrState::Merged);
    let reason = build_block_reason(&pr, "fe407c3b", true, true);
    assert!(
        !reason.contains("push the latest commits"),
        "shipped head must not be told to push: {reason}"
    );
    assert!(!reason.is_empty(), "a reason is still rendered: {reason}");
}

#[test]
fn block_reason_still_demands_a_push_for_unshipped_work() {
    // The half that proves the fix did not just delete the guard.
    let pr = shipped_pr("23480a0e", PrState::Open);
    let reason = build_block_reason(&pr, "deadbeef", true, false);
    assert!(
        reason.contains("push the latest commits before completing"),
        "unpushed work must still block: {reason}"
    );
}

#[test]
fn self_review_gate_pass_attestation_clears_code_review() {
    // AC2-HP: once a head-pinned code-review pass lands, the scan no longer
    // holds it - the floor is satisfiable by the self-serve route, not a wait.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("e.jsonl");
    std::fs::write(&p, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"code-review","head_sha":"h","verdict":"pass"}}"#)).unwrap();
    let out = unattested_reviewers(&p, &["code-review".to_string()], "h", "");
    assert!(
        out.is_empty(),
        "code-review should clear on a pass: {out:?}"
    );
}

#[test]
fn unwatched_async_nudge_review_uses_review_aware_watcher() {
    // codex P2: the review-wait watcher must poll REVIEW state, not
    // checks. It must also poll on REST - the GraphQL
    // reviews read is part of what exhausts the shared quota. The recipes
    // are the sanctioned `fno do pr wait` verb, one plain command: an
    // inline `while`/`$(...)` loop is refused by Claude Code's worktree
    // Bash isolation, so a worktree session cannot arm the watcher at all.
    let hint = arm_watch_hint(404, "review");
    assert!(
        hint.contains("fno do pr wait 404 --until review"),
        "got: {hint}"
    );
    assert!(!hint.contains("gh pr view"), "got: {hint}");
    assert!(!hint.contains("while ["), "got: {hint}");
    assert!(!hint.contains("$("), "got: {hint}");
    // The CI-wait watcher polls the REST status chokepoint for the
    // POSITIVE settled marker, never `gh pr checks --watch` (GraphQL).
    let ci_hint = arm_watch_hint(404, "ci");
    assert!(
        ci_hint.contains("fno do pr wait 404 --until settled"),
        "got: {ci_hint}"
    );
    assert!(!ci_hint.contains("gh pr checks"), "got: {ci_hint}");
    assert!(!ci_hint.contains("while ["), "got: {ci_hint}");
    assert!(!ci_hint.contains("$("), "got: {ci_hint}");
}

#[test]
fn watch_idle_rejects_unshipped_work() {
    // AC2-ERR: unpushed work is never async-wait. The predicate used to be
    // `PR head != local HEAD` and is now "not shipped", which is the same
    // verdict for a genuine unpushed commit and a different one for a head
    // that merely moved past its own merge.
    assert_eq!(async_wait_class(&watch_pr(), true, false), None);
}

#[test]
fn watch_idle_rejects_ci_red() {
    // AC1-ERR: settled-red CI (no pending) blocks, never idles.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        ci_conclusion: CiConclusion::Failure(Some("unit".into())),
        ci_has_pending: false,
        ..watch_pr()
    };
    assert_eq!(async_wait_class(&pr, true, true), None);
}

#[test]
fn watch_idle_rejects_unaddressed_finding() {
    // AC2-ERR: an unaddressed blocking inline finding is not async-wait.
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        unaddressed_findings: vec![Finding {
            id: 1,
            author: "codex".into(),
            path: "a.rs".into(),
            line: 1,
            created_at: "none".into(),
            severity: "P1",
            had_reply: true,
        }],
        ..watch_pr()
    };
    assert_eq!(async_wait_class(&pr, true, true), None);
}

#[test]
fn watch_idle_rejects_open_operator_finding() {
    // An open operator review_finding for the node also blocks idling.
    assert_eq!(async_wait_class(&watch_pr(), false, true), None);
}

#[test]
fn watch_idle_rejects_non_open_pr() {
    // A merged/closed PR is not an async wait (green+merged is DonePRGreen).
    let pr = PrInfo {
        range_tiling: RangeTiling::default(),
        state: PrState::Merged,
        ..watch_pr()
    };
    assert_eq!(async_wait_class(&pr, true, true), None);
}

#[test]
fn watch_idle_window_defaults_clamps_and_slacks() {
    // Default (no tag timeout): 30m + 12m slack.
    assert_eq!(
        watch_window_ms(None),
        30 * 60_000 + watch_lease::WATCH_SLACK_MS
    );
    // Honored within range.
    assert_eq!(
        watch_window_ms(Some("30m")),
        30 * 60_000 + watch_lease::WATCH_SLACK_MS
    );
    // Below the 5m floor clamps up.
    assert_eq!(
        watch_window_ms(Some("1m")),
        5 * 60_000 + watch_lease::WATCH_SLACK_MS
    );
    // Above the 2h ceiling clamps down.
    assert_eq!(
        watch_window_ms(Some("5h")),
        2 * 3_600_000 + watch_lease::WATCH_SLACK_MS
    );
    // Garbage falls back to the default.
    assert_eq!(
        watch_window_ms(Some("soon")),
        30 * 60_000 + watch_lease::WATCH_SLACK_MS
    );
}

#[test]
fn watch_idle_event_is_non_terminal_allow() {
    // AC1-HP invariant: the idle branch emits allow + null termination, so
    // the stop-hook shim (which runs finalize only on a NON-null
    // termination_reason) never invokes finalize / stamps the ledger /
    // graduates a plan on an idle fire. This is the exact output shape the
    // idle branch returns.
    let json = allow_output(
        "allow",
        None,
        "watching: idling until watcher fires (PR #404, ci pending)",
        3,
        Some("sha|OPEN|PENDING|none".to_string()),
    );
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["decision"], "allow");
    assert!(
        v["termination_reason"].is_null(),
        "idle-allow MUST be non-terminal or finalize would run"
    );
    assert!(v["message"].as_str().unwrap().contains("watching"));
}

#[test]
fn termination_reason_variant_names_byte_identical() {
    // Fix 6: all TerminationReason variants must serialize to the exact strings
    // the spec names - no rename attributes applied.
    let cases = [
        (TerminationReason::DonePRGreen, "DonePRGreen"),
        (TerminationReason::DoneAdvisory, "DoneAdvisory"),
        (TerminationReason::DoneAwaitingReview, "DoneAwaitingReview"),
        (TerminationReason::NoWork, "NoWork"),
        (TerminationReason::Budget, "Budget"),
        (TerminationReason::NoProgress, "NoProgress"),
        (TerminationReason::Interrupted, "Interrupted"),
        (TerminationReason::Aborted, "Aborted"),
    ];
    for (variant, expected) in cases {
        let json = serde_json::to_string(&variant).unwrap();
        // serde serializes enum unit variants as "\"VariantName\""
        assert_eq!(
            json,
            format!("\"{expected}\""),
            "variant {expected} serialized incorrectly"
        );
    }
}

#[test]
fn reviewers_all_attested_empty_is_vacuously_true() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("nonexistent.jsonl");
    assert!(reviewers_all_attested(&p, &[], "abc"));
}

#[test]
fn reviewers_all_attested_head_pinned_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let p = write_events(
        tmp.path(),
        &[
            r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"abc123","verdict":"pass"}}"#,
        ],
    );
    assert!(reviewers_all_attested(&p, &["sigma".to_string()], "abc123"));
}

#[test]
fn reviewers_all_attested_stale_head_is_unsatisfied() {
    // Head-pin: a pass for a PRIOR commit must not satisfy the current HEAD
    // (AC1-EDGE / AC8-HP). A new commit invalidates the old attestation.
    let tmp = tempfile::tempdir().unwrap();
    let p = write_events(
        tmp.path(),
        &[
            r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass"}}"#,
        ],
    );
    assert!(!reviewers_all_attested(&p, &["sigma".to_string()], "NEW"));
}

#[test]
fn reviewers_all_attested_fail_and_missing_are_unsatisfied() {
    let tmp = tempfile::tempdir().unwrap();
    // fail verdict -> unsatisfied
    let fail = write_events(
        tmp.path(),
        &[
            r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#,
        ],
    );
    assert!(!reviewers_all_attested(&fail, &["sigma".to_string()], "h"));
    // missing file -> fail closed
    let gone = tmp.path().join("gone.jsonl");
    assert!(!reviewers_all_attested(&gone, &["sigma".to_string()], "h"));
}

#[test]
fn reviewers_all_attested_conjunction_and_slash_normalized() {
    // Every reviewer must be attested (strict conjunction); a '/'-prefixed
    // config entry matches an event that emits the bare name and vice-versa.
    let tmp = tempfile::tempdir().unwrap();
    let p = write_events(
        tmp.path(),
        &[
            r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"pass"}}"#,
            r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"code-review","head_sha":"h","verdict":"pass"}}"#,
        ],
    );
    // Both present -> satisfied ('/code-review' config vs 'code-review' event).
    assert!(reviewers_all_attested(
        &p,
        &["sigma".to_string(), "/code-review".to_string()],
        "h"
    ));
    // One missing -> unsatisfied.
    assert!(!reviewers_all_attested(
        &p,
        &["sigma".to_string(), "declare".to_string()],
        "h"
    ));
}

#[test]
fn reviewers_all_attested_latest_verdict_wins() {
    // events.jsonl is append-ordered: a later attestation supersedes an
    // earlier one for the same reviewer at the same head (codex peer P1).
    let tmp = tempfile::tempdir().unwrap();
    // pass THEN fail -> latest is fail -> unsatisfied.
    let pf = write_events(
        tmp.path(),
        &[
            r#"{"ts":"t1","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"pass"}}"#,
            r#"{"ts":"t2","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#,
        ],
    );
    assert!(
        !reviewers_all_attested(&pf, &["sigma".to_string()], "h"),
        "a fail posted after a pass must revoke it"
    );
    // fail THEN pass -> latest is pass -> satisfied (re-review cleared it).
    let fp = write_events(
        tmp.path(),
        &[
            r#"{"ts":"t1","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#,
            r#"{"ts":"t2","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"pass"}}"#,
        ],
    );
    assert!(
        reviewers_all_attested(&fp, &["sigma".to_string()], "h"),
        "a pass posted after a fail must restore satisfaction"
    );
}

#[test]
fn local_peer_attestation_is_head_pinned() {
    let td = tempfile::tempdir().unwrap();
    let events = td.path().join("events.jsonl");
    std::fs::write(&events, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"peer","head_sha":"OLD","verdict":"pass"}}"#)).unwrap();
    let peer = vec![LOCAL_PEER_REVIEWER.to_string()];
    assert!(!reviewers_all_attested(&events, &peer, "NEW"));
    std::fs::write(&events, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"peer","head_sha":"NEW","verdict":"pass"}}"#)).unwrap();
    assert!(reviewers_all_attested(&events, &peer, "NEW"));
}

#[test]
fn optional_only_refusal_is_not_awaiting_review() {
    // The stop gate must not end the session DoneAwaitingReview over a
    // refusal nobody was owed: on a repo with no required bots that
    // terminal was trivially reachable, and the message told the worker
    // to wait for a reviewer that will never come. The gate blocks, so
    // the unattested local reviewer reads as the real work it is.
    let owed = |required| ReviewerVerdict {
        producer: CoverageProducer::GithubApp,
        name: "chatgpt-codex-connector".to_string(),
        verdict: CoverageVerdict::Refused,
        human_approval: false,
        author_approval: false,
        attestation_origin: AttestationOrigin::Unknown,
        reviewed_sha: String::new(),
        freshness: None,
        scope: None,
        refusal_reason: None,
        reviewer_context: None,
        required,
        passed: false,
    };
    let mut pr = watch_pr();
    pr.coverage = CoverageReport {
        github_approval_satisfies: false,
        coverage: Coverage::Covered(0),
        verdicts: vec![owed(false)],
    };
    assert!(!awaiting_review_only(&pr));
    // The same refusal OWED is still the terminal's case - the pre-change
    // semantics, kept: a required bot declined and nothing else is unmet.
    pr.coverage.verdicts[0].required = true;
    assert!(awaiting_review_only(&pr));
}
