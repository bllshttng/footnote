//! What the fleet GitHub request budget does to a loop-check fire.
//!
//! The budget is one ledger per machine, so the gate reads it BEFORE the
//! quota probe and every PR read: a stand-down under the ledger spends zero
//! GitHub requests (the measured 2026-09-13 refusal window kept 160
//! stand-downs alive by probing through them). Promise and abort stay
//! exempt - the floor belongs to the merge guard.

use super::*;

/// A live fleet-ledger backoff stands a no-promise fire down BEFORE it spends
/// anything: the measured 2026-09-13 refusal window kept 160 stand-downs
/// alive by probing through them, so the budget read must precede the quota
/// probe and every PR read.
#[test]
fn floor_stands_down_on_a_live_ledger_backoff_without_spending() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-ledger", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    // A GitHub refusal some OTHER caller recorded opened a fleet-wide backoff
    // that runs 10 minutes past this fire's --now. One machine, one ledger:
    // the fire must read it before spending.
    let t0 = "2026-06-05T00:30:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap();
    let ledger = cwd.join(".fno/gh-budget.json");
    fs::write(
        &ledger,
        serde_json::json!({
            "stamps": [],
            "backoff_until_ms": t0.timestamp_millis() + 600_000,
            "backoff_step": 0,
            "last_refusal_ms": t0.timestamp_millis() - 60_000,
            "last_half_open_ms": 0
        })
        .to_string(),
    )
    .unwrap();

    // 4922 remaining, well above GRAPHQL_FLOOR (200): only the ledger can
    // stand this fire down.
    let gh = quota_gh(cwd, 4922, false);
    let git = MockBins::green().git;
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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
        &format!("--gh-budget-ledger={}", ledger.display()),
    ]);
    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(d.message.contains("standing down"), "got: {}", d.message);
    assert!(
        d.message.contains("gh-budget"),
        "the cause must name the budget ledger: {}",
        d.message
    );
    let calls = fs::read_to_string(cwd.join("calls.log")).unwrap();
    assert!(
        !calls.contains("api rate_limit"),
        "a ledger stand-down spends no probe: {calls}"
    );
    assert!(
        !calls.contains("pr view"),
        "no GraphQL spend under a ledger backoff: {calls}"
    );
    // AC3-HP: the stand-down row carries the budget cause.
    let events = fs::read_to_string(project_events(&cwd)).unwrap_or_default();
    assert!(
        events.contains("\"budget_cause\":\"backoff\"")
            || events.contains("\"budget_cause\": \"backoff\""),
        "standdown must carry budget_cause: {events}"
    );
}

/// The `budget` half of the same guard: a 60s window already at its point cap
/// stands a fire down too. The stamps are OTHER sessions' admits (one
/// machine, one ledger), so this is also the fleet-wide property the old
/// journal scan carried: this session did nothing and is held anyway.
#[test]
fn floor_stands_down_when_the_point_window_is_at_cap() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-capped", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();

    // Other callers' stamps sum to the default cap (450) shortly before the
    // fire, still inside the 60s window.
    let t0 = "2026-06-05T00:30:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap();
    let ledger = cwd.join(".fno/gh-budget.json");
    let stamps: Vec<(i64, u32)> = (0..450)
        .map(|i| (t0.timestamp_millis() - 5_000 + i as i64, 1))
        .collect();
    fs::write(
        &ledger,
        serde_json::json!({
            "stamps": stamps,
            "backoff_until_ms": 0,
            "backoff_step": 0,
            "last_refusal_ms": 0,
            "last_half_open_ms": 0
        })
        .to_string(),
    )
    .unwrap();

    let gh = quota_gh(cwd, 4922, false);
    let git = MockBins::green().git;
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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
        &format!("--gh-budget-ledger={}", ledger.display()),
    ]);
    assert_eq!(code, 0);
    assert_eq!(d.decision, "block");
    assert!(d.message.contains("standing down"), "got: {}", d.message);
    assert!(
        d.message.contains("budget"),
        "the cause must name the exhausted window: {}",
        d.message
    );
    let calls = fs::read_to_string(cwd.join("calls.log")).unwrap();
    assert!(
        !calls.contains("api rate_limit"),
        "a capped window spends no probe: {calls}"
    );
}

/// A promise-intent fire proceeds under a live ledger backoff: the floor
/// belongs to the merge guard, and the promise must still be evaluated even
/// when every other call on the machine is being held (AC3-ERR).
#[test]
fn floor_never_blocks_a_promise_under_a_ledger_backoff() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);
    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(
        &manifest_path,
        new_manifest("sess-ledger-promise", "2026-06-05T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_with_promise()).unwrap();

    let t0 = "2026-06-05T00:30:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap();
    let ledger = cwd.join(".fno/gh-budget.json");
    fs::write(
        &ledger,
        serde_json::json!({
            "stamps": [],
            "backoff_until_ms": t0.timestamp_millis() + 600_000,
            "backoff_step": 0,
            "last_refusal_ms": t0.timestamp_millis() - 60_000,
            "last_half_open_ms": 0
        })
        .to_string(),
    )
    .unwrap();

    // Exhausted (0 remaining) AND green: the promise must still be evaluated.
    let gh = quota_gh(cwd, 0, true);
    let git = MockBins::green().git;
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
        &format!("--gh-bin={}", gh.display()),
        &format!("--git-bin={}", git.display()),
        &format!("--gh-budget-ledger={}", ledger.display()),
    ]);
    assert!(!d.message.contains("standing down"), "got: {}", d.message);
    let calls = fs::read_to_string(cwd.join("calls.log")).unwrap();
    assert!(calls.contains("pr view"), "promise reads proceed: {calls}");
    assert_eq!(code, 0);
}
