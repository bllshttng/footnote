//! What a discarded `<watching>` tag must SAY.
//!
//! The tag is a request and the stop gate is the authority, so every refusal
//! here is read by an agent deciding what to do on its next wake. A refusal
//! that names a transient cause for a permanent one sends it back to arm
//! another watcher, which is the loop these cases exist to prevent.

use super::*;

#[test]
fn watching_ignored_unsupported_harness_is_audible() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-watching-unsupported", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_watching()).unwrap();

    let mock = MockBins::ci_pending();
    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
        "--author-harness",
        "grok",
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(
        d.message
            .contains("watching ignored: harness grok cannot idle"),
        "discarded watching tag must be named: {}",
        d.message
    );
    assert!(
        d.message.contains("CI still running on PR #17"),
        "the actionable PR blocker must remain: {}",
        d.message
    );
    // A harness that cannot self-wake can never honor the arm-and-tag hint,
    // so the composed message must not prescribe it (the re-arm loop
    // the module doc names).
    assert!(
        !d.message.contains("Arm a harness-tracked watcher"),
        "the refused ritual must not be prescribed in the same message: {}",
        d.message
    );
}

// A session whose init found the node already claimed records no claim key,
// and the manifest is write-once, so the lease can never renew. The generic
// renewal refusal reads as transient, and a reader who believes it re-arms a
// watcher on every stop and never idles once. Name the permanent cause.
#[test]
fn watching_ignored_names_a_missing_claim_as_permanent() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    // new_manifest writes no target_claim_key / target_claim_holder, which is
    // exactly the shape init leaves behind on claim_held_by_other.
    fs::write(
        &manifest_path,
        new_manifest("sess-watching-noclaim", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_watching()).unwrap();

    let mock = MockBins::ci_pending();
    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
        "--author-harness",
        "claude",
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(
        d.message.contains("recorded no node claim at init"),
        "the permanent cause must be named: {}",
        d.message
    );
    assert!(
        d.message
            .contains("arming another watcher will not change that"),
        "the refusal must stop the re-arm loop it caused: {}",
        d.message
    );
    assert!(
        d.message.contains("fno do target start <node>"),
        "the refusal must name the way back, not only the dead end: {}",
        d.message
    );
    assert!(!d.message.contains("Arm a harness-tracked watcher"));
    assert!(d.message.contains("CI still running on PR #17"));
}

#[test]
fn watching_requires_durable_registration() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let key = "node:registration-fixture";
    let holder = "registration-fixture";
    assert!(matches!(
        fno_agents::claims::acquire(
            key,
            holder,
            fno_agents::claims::AcquireOpts {
                root: Some(cwd.to_path_buf()),
                events_dir: Some(cwd.to_path_buf()),
                pid: Some(std::process::id()),
                ttl_ms: Some(3_600_000),
                ..Default::default()
            }
        ),
        fno_agents::claims::AcquireOutcome::Acquired(_)
    ));
    fs::write(&manifest_path, format!(
        "---\nsession_id: sess-registration\ncreated_at: 2026-06-05T00:00:00Z\nattended: true\ntarget_claim_key: {key}\ntarget_claim_holder: {holder}\n---\n"
    )).unwrap();
    fs::write(&transcript_path, transcript_with_watching()).unwrap();

    let mock = MockBins::ci_pending();
    let blocked_parent = cwd.join("blocked-parent");
    fs::write(&blocked_parent, "not a directory").unwrap();
    let blocked_journal = blocked_parent.join("events.jsonl");
    let harness = "claude";
    let durable_journal = cwd.join(format!("{harness}-events.jsonl"));
    for (journal, expected) in [(&blocked_journal, "block"), (&durable_journal, "allow")] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_fno-agents"))
            .env("FNO_CLAIMS_ROOT", cwd)
            .env("FNO_NUDGE_DISABLED", "1")
            .env_remove("FNO_DRIVER_LIB")
            .args([
                "loop-check",
                "--state",
                manifest_path.to_str().unwrap(),
                "--transcript",
                transcript_path.to_str().unwrap(),
                "--cwd",
                cwd.to_str().unwrap(),
                "--now",
                "2026-06-05T00:30:00Z",
                &format!("--gh-bin={}", mock.gh.display()),
                &format!("--git-bin={}", mock.git.display()),
                "--author-harness",
                harness,
                "--global-events",
                journal.to_str().unwrap(),
                "--global-settings",
                "/nonexistent/global-settings.yaml",
            ])
            .output()
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
        assert_eq!(value["decision"], expected, "{harness}: {value}");
        if expected == "block" {
            assert!(
                value["message"]
                    .as_str()
                    .unwrap()
                    .contains("watch registration"),
                "{harness}: {value}"
            );
        } else {
            assert!(
                event_text(journal).lines().any(|line| {
                    let event: serde_json::Value = serde_json::from_str(line).unwrap();
                    event["type"] == "loop_check_watch_idle"
                        && event["data"]["session_id"] == "sess-registration"
                        && event["data"]["harness"] == harness
                }),
                "{harness}: durable watch registration missing"
            );
        }
    }
}

#[test]
fn watching_ignored_unaddressed_findings_are_audible() {
    let tmp = findings_cwd("sess-watching-findings");
    let cwd = tmp.path();
    fs::write(cwd.join("transcript.jsonl"), transcript_with_watching()).unwrap();

    let comments = r#"[
  {"id":100,"in_reply_to_id":null,"user":{"login":"chatgpt-codex-connector[bot]"},"body":"![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) First","path":"src/one.rs","line":11,"created_at":"2026-06-05T01:10:00Z"},
  {"id":101,"in_reply_to_id":null,"user":{"login":"chatgpt-codex-connector[bot]"},"body":"![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) Second","path":"src/two.rs","line":22,"created_at":"2026-06-05T01:11:00Z"},
  {"id":102,"in_reply_to_id":null,"user":{"login":"chatgpt-codex-connector[bot]"},"body":"![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) Third","path":"src/three.rs","line":33,"created_at":"2026-06-05T01:12:00Z"},
  {"id":103,"in_reply_to_id":null,"user":{"login":"chatgpt-codex-connector[bot]"},"body":"![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat) Fourth","path":"src/four.rs","line":44,"created_at":"2026-06-05T01:13:00Z"}
]"#;
    let mock = findings_mock(comments, r#"{"commits":[]}"#);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T02:00:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
        "--author-harness",
        "claude",
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(
        d.message
            .contains("watching ignored: 4 unaddressed findings, this is not an async wait"),
        "discarded watching tag must name the finding reason: {}",
        d.message
    );
    assert!(
        d.message.contains("src/one.rs:11"),
        "the actionable first finding must remain: {}",
        d.message
    );
}
