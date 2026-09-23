use super::*;

#[test]
fn sized_hint_bridge_reads_one_clean_line_and_nothing_else() {
    // The bridge is a dumb pipe for the Python render: one sane line in,
    // Some(line) out; everything else (non-zero exit, chatter, an overlong
    // blob, a missing binary) is None so the refusal falls back to the
    // levelless line rather than embedding garbage as an invocation.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    let stub = |body: &str| -> std::path::PathBuf {
        let p = dir.join(format!("stub-{}", body.len()));
        std::fs::write(&p, body).unwrap();
        #[allow(clippy::permissions_set_readonly_false)]
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        p
    };

    let good = stub("#!/bin/sh\nprintf '/code-review from-stub --comment --fix\\n'\n");
    assert_eq!(
        sized_self_review_hint(good.to_str().unwrap(), dir, Some("claude")).as_deref(),
        Some("/code-review from-stub --comment --fix")
    );

    let failing = stub("#!/bin/sh\nexit 1\n");
    assert_eq!(
        sized_self_review_hint(failing.to_str().unwrap(), dir, None),
        None
    );

    let chatty = stub("#!/bin/sh\nprintf 'line one\\nline two\\n'\n");
    assert_eq!(
        sized_self_review_hint(chatty.to_str().unwrap(), dir, None),
        None
    );

    // An unsized render keeps its `<level>` placeholder; embedding that in
    // a copy-me slot hands the reader a string it cannot run, so the
    // filter reads it as no hint at all.
    let placeholder = stub("#!/bin/sh\nprintf '/code-review <level> --comment\\n'\n");
    assert_eq!(
        sized_self_review_hint(placeholder.to_str().unwrap(), dir, None),
        None
    );

    assert_eq!(sized_self_review_hint("/nonexistent/fno", dir, None), None);
}

/// Run the verb until the quota probe answers decisively (a bool
/// graphql_exhausted): a fork/exec blip on a loaded runner lands as
/// probe_graphql_quota -> None (null), which is correct behavior - the
/// assertions need a decisive read, not the first one.
fn run_exit4_until_decisive(args: &[String]) -> Value {
    for _ in 0..5 {
        let (code, out) = run_review_coverage_capture(args);
        assert_eq!(code, 4);
        let parsed: Value = serde_json::from_str(&out).expect("stdout is one JSON object");
        if parsed
            .get("graphql_exhausted")
            .and_then(|x| x.as_bool())
            .is_some()
        {
            return parsed;
        }
    }
    panic!(
        "exit-4 stdout never carried a decisive graphql_exhausted across 5 \
             runs: either the gh stub kept failing to spawn or the stdout \
             contract changed - both are real failures"
    );
}

#[test]
fn review_coverage_pr_failure_stdout_carries_quota_diagnostic() {
    let _root = crate::paths::DeclaredRoot::declare("review_coverage_pr_failure_s");
    // exit 4 with a known PR persists a schema-gated unknown row,
    // and its stdout must say WHY the read degraded. A bare unknown is
    // indistinguishable from "nobody reviewed this" and sent operators to
    // re-review PRs whose only problem was an exhausted quota window.
    let tmp = tempfile::tempdir().unwrap();
    let gh = write_failing_pr_view_gh(tmp.path(), 0, 14 * 60, 0);
    let events = tmp.path().join("ev.jsonl");
    let args = review_coverage_args(tmp.path(), "930c2e9dad5d2dc5ba2deae320070bd86ecfcfc2", &gh);
    let v = run_exit4_until_decisive(&args);
    assert_eq!(v["coverage"], "unknown");
    assert_eq!(v["graphql_exhausted"], true, "got: {v}");
    let reason = v["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("GraphQL quota exhausted"),
        "reason must name the cause, got: {reason}"
    );
    // The PERSISTED row keeps the bare schema: stdout-only diagnostics, no
    // event-contract fork. Pin the destination, not the tag - the row must
    // lack the diagnostic keys, and carry the unknown verdict as emitted.
    let log = crate::events::committed_journal_text(&events);
    let row: Value =
        serde_json::from_str(log.lines().next().expect("one row")).expect("row is JSON");
    assert_eq!(row["type"], "review_coverage");
    assert_eq!(row["data"]["coverage"], "unknown");
    assert_eq!(row["data"]["verdicts"], serde_json::json!([]));
    assert!(row["data"].get("graphql_exhausted").is_none());
    assert!(row["data"].get("graphql_remaining").is_none());
    assert!(row["data"].get("reason").is_none());
}

#[test]
fn review_coverage_pr_failure_healthy_quota_reports_not_exhausted() {
    let _root = crate::paths::DeclaredRoot::declare("review_coverage_pr_failure_h");
    // The diagnostic must not cry wolf: a gh failure with graphql budget
    // left is an outage, not exhaustion, and stdout saying exhausted=false
    // is what lets a reader stop guessing between the two.
    let tmp = tempfile::tempdir().unwrap();
    let gh = write_failing_pr_view_gh(tmp.path(), 4890, 0, 0);
    let args = review_coverage_args(tmp.path(), "deadbeefdeadbeefdeadbeefdeadbeef00000001", &gh);
    let v = run_exit4_until_decisive(&args);
    assert_eq!(v["graphql_exhausted"], false, "got: {v}");
    assert_eq!(v["graphql_remaining"], 4890);
    assert!(v.get("reason").is_none(), "no reason without exhaustion");
}

#[test]
fn review_coverage_pr_failure_verbatim_403_classifies_secondary_via_the_exempt_probe() {
    let _root = crate::paths::DeclaredRoot::declare("review_coverage_pr_failure_v");
    // The p0 shape on this verb: the failed read's stderr is the
    // MEASURED 2026-08-24 body (no "secondary" anywhere) while the exempt
    // rate_limit endpoint still answers healthy. The classifier is that
    // one probe - it fires exactly once (the marker), never counts
    // against a bucket, and the verdict keeps the graphql diagnostics
    // null so the reason names the burst limit, not a healthy bucket.
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("probe-fired");
    let gh = write_exec(
            tmp.path(),
            "gh",
            &format!(
                "#!/bin/sh\n\
                 [ \"$1\" = pr ] && [ \"$2\" = view ] && \
                 echo 'API rate limit exceeded for user ID 4994564. If you reach out to GitHub Support for help, please include the request ID FAEB:283161:6EF36:99B72:6A8B97DD ... (HTTP 403)' >&2 && exit 1\n\
                 [ \"$1\" = api ] && [ \"$2\" = rate_limit ] && \
                 echo '{{\"resources\":{{\"graphql\":{{\"remaining\":4446,\"reset\":1750000000}},\
                 \"core\":{{\"remaining\":4980,\"limit\":5000,\"reset\":1750000000}}}}}}' && \
                 echo probe > {marker} && exit 0\n\
                 exit 1\n",
                marker = marker.display()
            ),
        );
    let events = tmp.path().join("ev.jsonl");
    // Retry until the pr-view spawn lands (its fork is the one flake this
    // suite retries for): a spawn failure degrades tail to an exec error,
    // the secondary match misses, and the probe fires.
    let mut v: Option<Value> = None;
    for _ in 0..5 {
        let (code, out) = run_review_coverage_capture(&[
            "review-coverage".to_string(),
            "--cwd".to_string(),
            tmp.path().display().to_string(),
            "--pr".to_string(),
            "865".to_string(),
            "--head".to_string(),
            "deadbeefdeadbeefdeadbeefdeadbeef00000001".to_string(),
            "--events".to_string(),
            events.display().to_string(),
            "--global-events".to_string(),
            tmp.path().join("gev.jsonl").display().to_string(),
            "--settings".to_string(),
            tmp.path().join("absent.toml").display().to_string(),
            "--gh-bin".to_string(),
            gh.display().to_string(),
        ]);
        assert_eq!(code, 4);
        if let Ok(parsed) = serde_json::from_str::<Value>(&out) {
            if parsed.get("reason").is_some() {
                v = Some(parsed);
                break;
            }
        }
    }
    let v = v.expect("5 runs never produced a reasoned stdout");
    let reason = v["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("secondary rate limit"),
        "reason must name the secondary limit, got: {reason}"
    );
    assert!(
        v["graphql_exhausted"].is_null(),
        "secondary keeps the quota diagnostics null: exhausted reads null, got: {v}"
    );
    // The exempt probe FIRED exactly as the classifier; the marker is the
    // positive control that classification came from a live reading.
    assert!(
        marker.exists(),
        "the exempt probe is the classifier - it must fire"
    );
    assert_eq!(v["coverage"], "unknown");
}

#[test]
fn review_coverage_no_pr_secondary_limit_classifies_the_same_way() {
    let _root = crate::paths::DeclaredRoot::declare("review_coverage_no_pr_second");
    // The pr_num == 0 arm (no --pr passed) shares the classification: a
    // refusal with no PR number gets the same live-bucket verdict (here
    // with the OLD phrase in the stderr - the bucket, not the wording,
    // must stay the discriminator) and the same exempt-probe cost.
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("probe-fired");
    let gh = write_exec(
            tmp.path(),
            "gh",
            &format!(
                "#!/bin/sh\n\
                 [ \"$1\" = pr ] && [ \"$2\" = view ] && \
                 echo 'You have exceeded a secondary rate limit and have been temporarily blocked.' >&2 && exit 1\n\
                 [ \"$1\" = api ] && [ \"$2\" = rate_limit ] && \
                 echo '{{\"resources\":{{\"graphql\":{{\"remaining\":4890,\"reset\":1750000000}},\
                 \"core\":{{\"remaining\":4922,\"limit\":5000,\"reset\":1750000000}}}}}}' && \
                 echo probe > {marker} && exit 0\n\
                 exit 1\n",
                marker = marker.display()
            ),
        );
    let events = tmp.path().join("ev.jsonl");
    let mut reasoned: Option<Value> = None;
    for _ in 0..5 {
        let (code, out) = run_review_coverage_capture(&[
            "review-coverage".to_string(),
            "--cwd".to_string(),
            tmp.path().display().to_string(),
            "--head".to_string(),
            "deadbeefdeadbeefdeadbeefdeadbeef00000001".to_string(),
            "--events".to_string(),
            events.display().to_string(),
            "--global-events".to_string(),
            tmp.path().join("gev.jsonl").display().to_string(),
            "--settings".to_string(),
            tmp.path().join("absent.toml").display().to_string(),
            "--gh-bin".to_string(),
            gh.display().to_string(),
        ]);
        assert_eq!(code, 4);
        if let Ok(parsed) = serde_json::from_str::<Value>(&out) {
            if parsed.get("reason").is_some() {
                reasoned = Some(parsed);
                break;
            }
        }
    }
    let v = reasoned.expect("5 runs never produced a reasoned stdout");
    assert!(
        v["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("secondary rate limit"),
        "reason must name the secondary limit, got: {v}"
    );
    assert!(
        marker.exists(),
        "the no-PR arm must classify through the same exempt probe"
    );
}
