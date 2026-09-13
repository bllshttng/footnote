//! The arm_watch acceptance contract: the pure tick over hand-built
//! `ArmStatus` rows and a tempdir signal store, the send handed in - the
//! same seam `tests/machine_watch.rs` proved. The store ts is real wall
//! clock, so the one test that crosses the rate floor drives `now_unix`
//! from the real clock too; every other test pins the fixed pair
//! `2026-09-04T12:00:00Z` = 1_788_523_200 the tick_ledger tests pin.

use fno_agents::arm_watch::tick_arm_watch;
use fno_agents::tick_ledger::ArmStatus;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const TS: &str = "2026-09-04T12:00:00Z";
const TS_UNIX: u64 = 1_788_523_200;

fn row(arm: &str) -> ArmStatus {
    ArmStatus {
        arm: arm.to_string(),
        scheduler: Some("daemon".to_string()),
        last_ts: None,
        age_s: None,
        acted: None,
        skip_reason: None,
        detail: None,
        interval_s: 600,
        stale: false,
        failing: false,
        failing_for_s: None,
        cause: None,
        line: String::new(),
    }
}

fn failing(mut r: ArmStatus, skip: &str, failing_for_s: u64) -> ArmStatus {
    r.failing = true;
    r.skip_reason = Some(skip.to_string());
    r.failing_for_s = Some(failing_for_s);
    r.age_s = Some(30);
    r
}

fn stale(mut r: ArmStatus, cause: &str) -> ArmStatus {
    r.stale = true;
    r.cause = Some(cause.to_string());
    r.last_ts = Some(TS.to_string());
    r.age_s = Some(2400);
    r
}

fn temp_store(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fno-arm-watch-{}-{name}", std::process::id()))
}

fn real_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// AC1-HP: one failing arm past threshold sends exactly one notice whose
/// title, body and pointer line are the contract's.
#[test]
fn a_failing_arm_past_threshold_sends_one_notice() {
    let store = temp_store("ac1");
    let rows = vec![failing(row("king_wake"), "timeout", 2000)];
    let mut sends: Vec<(String, String)> = Vec::new();
    let out = tick_arm_watch(&rows, 1800, &store, TS_UNIX, |title, body| {
        sends.push((title.to_string(), body.to_string()));
        true
    });
    assert_eq!(out.acted, 1);
    assert_eq!(out.skip_reason, None);
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, "control plane: arm failing");
    assert!(sends[0].1.contains("king_wake"), "{}", sends[0].1);
    assert!(sends[0].1.contains("2000s"), "{}", sends[0].1);
    assert!(sends[0].1.contains("fno agents status"), "{}", sends[0].1);
    std::fs::remove_file(&store).ok();
}

/// AC2-EDGE: the same failing set on a second tick 300s later dedupes - the
/// anchor (now - failing_for_s) is constant while the episode lasts.
#[test]
fn the_same_set_three_hundred_seconds_later_is_deduped() {
    let store = temp_store("ac2");
    let mut sends = 0usize;
    let first = vec![failing(row("king_wake"), "timeout", 2000)];
    tick_arm_watch(&first, 1800, &store, TS_UNIX, |_, _| {
        sends += 1;
        true
    });
    let second = vec![failing(row("king_wake"), "timeout", 2300)];
    let out = tick_arm_watch(&second, 1800, &store, TS_UNIX + 300, |_, _| {
        sends += 1;
        true
    });
    assert_eq!(sends, 1, "no second notice");
    assert_eq!(out.acted, 0);
    assert_eq!(out.detail, "deduped");
    std::fs::remove_file(&store).ok();
}

/// AC3-HP: still failing plus a newly failing arm, past the rate floor, is
/// one send naming both arms.
#[test]
fn a_new_arm_joining_the_set_sends_after_the_rate_floor() {
    let store = temp_store("ac3");
    let now = real_now();
    let first = vec![failing(row("king_wake"), "timeout", 2000)];
    let out1 = tick_arm_watch(&first, 1800, &store, now, |_, _| true);
    assert_eq!(out1.acted, 1);
    let second = vec![
        failing(row("king_wake"), "timeout", 2000 + 1900),
        failing(row("notify_watch"), "timeout", 1900),
    ];
    let mut bodies: Vec<String> = Vec::new();
    let out2 = tick_arm_watch(&second, 1800, &store, now + 1900, |_, body| {
        bodies.push(body.to_string());
        true
    });
    assert_eq!(out2.acted, 1);
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("king_wake"), "{}", bodies[0]);
    assert!(bodies[0].contains("notify_watch"), "{}", bodies[0]);
    std::fs::remove_file(&store).ok();
}

/// AC4-EDGE: below threshold no send goes out and the stored entry is
/// removed, so a later episode sends again.
#[test]
fn below_threshold_sends_nothing_and_forgets_the_stored_token() {
    let store = temp_store("ac4");
    std::fs::write(
        &store,
        r#"{"arm_failing": {"token": "king_wake@1", "ts": "2026-09-04T12:00:00Z"}}"#,
    )
    .unwrap();
    let rows = vec![failing(row("king_wake"), "timeout", 600)];
    let out = tick_arm_watch(&rows, 1800, &store, TS_UNIX, |_, _| {
        panic!("no send below threshold")
    });
    assert_eq!(out.acted, 0);
    assert_eq!(out.skip_reason.as_deref(), Some("clear"));
    let text = std::fs::read_to_string(&store).unwrap();
    assert!(!text.contains("arm_failing"), "{text}");
    std::fs::remove_file(&store).ok();
}

/// AC5-HP: a dead scheduler names all four arms and the cause in one send.
#[test]
fn a_dead_scheduler_names_all_its_arms_and_the_cause() {
    let store = temp_store("ac5");
    let now = TS_UNIX + 2400;
    let rows = vec![
        stale(row("pr_watch_merge"), "scheduler_down"),
        stale(row("king_wake"), "scheduler_down"),
        stale(row("notify_watch"), "scheduler_down"),
        stale(row("watchdog"), "scheduler_down"),
    ];
    let mut bodies: Vec<String> = Vec::new();
    let out = tick_arm_watch(&rows, 1800, &store, now, |_, body| {
        bodies.push(body.to_string());
        true
    });
    assert_eq!(out.acted, 1);
    assert_eq!(bodies.len(), 1);
    for arm in ["pr_watch_merge", "king_wake", "notify_watch", "watchdog"] {
        assert!(bodies[0].contains(arm), "{arm} missing: {}", bodies[0]);
    }
    assert!(bodies[0].contains("scheduler_down"), "{}", bodies[0]);
    std::fs::remove_file(&store).ok();
}

/// AC6-EDGE: a stale row with an unexplained cause is not in the set.
#[test]
fn an_unexplained_stale_row_is_not_in_the_set() {
    let store = temp_store("ac6");
    let rows = vec![stale(row("notify_watch"), "unexplained")];
    let out = tick_arm_watch(&rows, 1800, &store, TS_UNIX + 2400, |_, _| {
        panic!("unexplained never pages")
    });
    assert_eq!(out.acted, 0);
    assert_eq!(out.skip_reason.as_deref(), Some("clear"));
    std::fs::remove_file(&store).ok();
}

/// AC7-EDGE: a failed send stores no token, so the next tick retries.
#[test]
fn a_failed_send_stores_no_token_so_the_next_tick_retries() {
    let store = temp_store("ac7");
    let rows = vec![failing(row("king_wake"), "timeout", 2000)];
    let out = tick_arm_watch(&rows, 1800, &store, TS_UNIX, |_, _| false);
    assert_eq!(out.acted, 0);
    assert_eq!(out.skip_reason.as_deref(), Some("notify_failed"));
    let text = std::fs::read_to_string(&store).unwrap_or_default();
    assert!(!text.contains("arm_failing"), "{text}");
    let mut sends = 0usize;
    let out2 = tick_arm_watch(&rows, 1800, &store, TS_UNIX, |_, _| {
        sends += 1;
        true
    });
    assert_eq!(out2.acted, 1);
    assert_eq!(sends, 1);
    std::fs::remove_file(&store).ok();
}
