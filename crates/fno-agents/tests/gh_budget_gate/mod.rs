//! What the fleet GitHub request budget does to a loop-check fire.
//!
//! The budget is one ledger per machine, read BEFORE the quota probe and
//! every PR read. Promise and abort stay exempt - a promise fire is still
//! evaluated under a live backoff (AC3-ERR).

use super::*;

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
