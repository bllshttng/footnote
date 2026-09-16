//! `evals-arm` tests: one per surviving deleted phase test plus the two
//! claim paths. No test runs the real bank: `--fno-bin` is a fixture
//! shell script, and tick mode runs run mode through an injected spawner that
//! re-parses the child argv in-process.
use super::*;

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn base_args(tmp: &TempDir, extra: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = [
        "--history",
        &tmp.path().join("history.jsonl").to_string_lossy(),
        "--events",
        &tmp.path().join("events.jsonl").to_string_lossy(),
        "--fno-bin",
        &tmp.path().join("fno-bin.sh").to_string_lossy(),
        "--schedule-days",
        "7",
        "--stale-days",
        "7",
        "--claims-root",
        &tmp.path().join("claims-root").to_string_lossy(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

fn reg_row(task_id: &str, ts_rfc3339: &str) -> String {
    serde_json::json!({
        "ts": ts_rfc3339, "task_id": task_id, "tier": "regression",
        "pass": true, "reason": "", "duration_s": 1.0, "repeat_index": 0,
        "bank_rev": null, "worker_provider": null,
    })
    .to_string()
}

/// Fixture `fno-bin`: `doctor evals run` appends ROWS (stamped one second
/// after the run window opens, so `ts >= started` holds at second
/// granularity); `inbox notify` records the call.
fn write_fixture(tmp: &TempDir, rows: &[String], history: &Path, notify_log: &Path) {
    let mut script = String::from("#!/bin/sh\ncase \"$1/$2\" in\n  doctor/evals)\n    sleep 1\n");
    // Rows baked as literal JSON; the ts is stamped at run time (second
    // granularity), which `sleep 1` places strictly after `--started`. The
    // JSON rides a double-quoted echo so `$(date)` substitutes; its own
    // double quotes are escaped for the shell.
    for row in rows {
        let stamped = row
            .replace("\"ts\":\"\"", "\"ts\":\"__TS__\"")
            .replace('"', "\\\"")
            .replace("__TS__", "$(date -u +%Y-%m-%dT%H:%M:%SZ)");
        script += &format!(
            "    echo \"{}\" >> '{}'\n",
            stamped,
            history.to_string_lossy()
        );
    }
    script += "    ;;\n  inbox/notify)\n    echo \"$*\" >> '";
    script += &notify_log.to_string_lossy();
    script += "'\n    ;;\nesac\n";
    let p = tmp.path().join("fno-bin.sh");
    fs::write(&p, script).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The injected spawner: re-parses the child argv (the same contract the real
/// binary exec satisfies) and runs run mode on a detached thread.
fn thread_spawner() -> impl Fn(&[String]) -> Result<u32, String> {
    |argv: &[String]| {
        assert_eq!(argv[1], "evals-arm");
        assert_eq!(argv[2], "--run");
        let child = parse_args(&argv[2..])?;
        std::thread::spawn(move || {
            run_run_mode(&child);
        });
        Ok(std::process::id())
    }
}

fn wait_for_row(events: &Path, kind: &str, timeout: Duration) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(events) {
            for line in text.lines().rev() {
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    if v.get("type").and_then(Value::as_str) == Some(kind) {
                        return Some(v);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn count_kind(events: &Path, kind: &str) -> usize {
    fs::read_to_string(events)
        .map(|text| {
            text.lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|v| v.get("type").and_then(Value::as_str) == Some(kind))
                .count()
        })
        .unwrap_or(0)
}

fn claim_state(root: &Path) -> claims::ClaimState {
    claims::status(CLAIM_KEY, Some(root)).0
}

fn ok_gate() -> GateReading {
    GateReading {
        slots: 0,
        max_live: 5,
        ram_gb: Some(16.0),
        min_free_gb: 1.0,
    }
}

fn full_gate() -> GateReading {
    GateReading {
        slots: 5,
        max_live: 5,
        ram_gb: Some(16.0),
        min_free_gb: 1.0,
    }
}

// AC2-HP: summary 9 days old, schedule 7, gate headroom, fixture appends 3
// rows -> one evals_scheduled_run with task_count 3, claim free at the end.
#[test]
fn fires_past_window_journals_run_and_frees_claim() {
    let tmp = TempDir::new().unwrap();
    let history = tmp.path().join("history.jsonl");
    let notify_log = tmp.path().join("notify.log");
    let events = tmp.path().join("events.jsonl");
    let rows: Vec<String> = (1..=3)
        .map(|i| reg_row(&format!("reg-task-{i}"), ""))
        .collect();
    write_fixture(&tmp, &rows, &history, &notify_log);
    let summary = r#"{"age_days": 9.0, "never_ran": false}"#;
    let args = base_args(&tmp, &["--summary-json", summary]);
    let o = parse_args(&args).unwrap();

    run_tick_mode(&o, &ok_gate, &thread_spawner());

    let row = wait_for_row(&events, "evals_scheduled_run", Duration::from_secs(20))
        .expect("scheduled-run row within 20s");
    let data = row.get("data").unwrap();
    assert_eq!(data.get("task_count"), Some(&Value::from(3)));
    assert_eq!(data.get("passes"), Some(&Value::from(3)));
    assert_eq!(data.get("window_days"), Some(&Value::from(7)));
    let claims_root = tmp.path().join("claims-root");
    assert_eq!(claim_state(&claims_root), claims::ClaimState::Free);
}

#[test]
fn skips_when_fresh() {
    let tmp = TempDir::new().unwrap();
    let args = base_args(&tmp, &["--summary-json", r#"{"age_days": 2.0}"#]);
    let o = parse_args(&args).unwrap();
    let code = run_tick_mode(&o, &ok_gate, &|_argv| panic!("spawner must not run"));
    assert_eq!(code, 0);
    assert_eq!(
        count_kind(&tmp.path().join("events.jsonl"), "evals_stale"),
        0
    );
}

// AC2-ERR: refusing gate twice inside one window -> no child, two evals_stale
// rows with reason gate, exactly one notify.
#[test]
fn refusing_gate_journals_stale_and_notice_deduped() {
    let tmp = TempDir::new().unwrap();
    let notify_log = tmp.path().join("notify.log");
    let events = tmp.path().join("events.jsonl");
    write_fixture(&tmp, &[], &tmp.path().join("history.jsonl"), &notify_log);
    let summary = r#"{"age_days": 15.0, "never_ran": false}"#;
    let args = base_args(&tmp, &["--summary-json", summary]);
    let o = parse_args(&args).unwrap();
    let never = |_argv: &[String]| -> Result<u32, String> {
        panic!("spawner must not run under a refusing gate")
    };

    run_tick_mode(&o, &full_gate, &never);
    run_tick_mode(&o, &full_gate, &never);

    assert_eq!(count_kind(&events, "evals_stale"), 2);
    let notices = fs::read_to_string(&notify_log).unwrap_or_default();
    assert_eq!(notices.lines().count(), 1, "one notice per schedule window");
}

#[test]
fn no_notice_when_emit_fails() {
    let tmp = TempDir::new().unwrap();
    let notify_log = tmp.path().join("notify.log");
    write_fixture(&tmp, &[], &tmp.path().join("history.jsonl"), &notify_log);
    // The events path IS a directory: the emitter's append fails, so the
    // journal cannot carry the receipt and no notice may ride.
    let mut args = base_args(&tmp, &["--summary-json", r#"{"age_days": 15.0}"#]);
    let idx = args.iter().position(|a| a == "--events").unwrap();
    let events_dir = tmp.path().join("events-dir");
    std::fs::create_dir(&events_dir).unwrap();
    args[idx + 1] = events_dir.to_string_lossy().into_owned();
    let o = parse_args(&args).unwrap();

    run_tick_mode(&o, &full_gate, &|_a| panic!("must not spawn"));

    assert!(!notify_log.exists(), "no notice without its journal row");
}

// Run mode, exit 0, no appended rows: a could-not-fire, never a success.
#[test]
fn exit0_without_rows_journals_no_rows() {
    let tmp = TempDir::new().unwrap();
    let notify_log = tmp.path().join("notify.log");
    let events = tmp.path().join("events.jsonl");
    write_fixture(&tmp, &[], &tmp.path().join("history.jsonl"), &notify_log);
    let args = base_args(&tmp, &["--run", "--started", &Utc::now().to_rfc3339()]);
    let o = parse_args(&args).unwrap();

    run_run_mode(&o);

    let row = wait_for_row(&events, "evals_stale", Duration::from_secs(5)).unwrap();
    assert_eq!(row["data"]["reason"], "no_rows");
    let claims_root = tmp.path().join("claims-root");
    assert_eq!(claim_state(&claims_root), claims::ClaimState::Free);
}

#[test]
fn rows_attributed_by_timestamp_not_count() {
    let tmp = TempDir::new().unwrap();
    let history = tmp.path().join("history.jsonl");
    let notify_log = tmp.path().join("notify.log");
    let events = tmp.path().join("events.jsonl");
    // Pre-existing rows 40 days old: inside the file, outside this receipt.
    let old = (Utc::now() - chrono::Duration::days(40)).to_rfc3339();
    fs::write(
        &history,
        format!("{}\n{}\n", reg_row("old-a", &old), reg_row("old-b", &old)),
    )
    .unwrap();
    let rows: Vec<String> = (1..=3)
        .map(|i| reg_row(&format!("reg-task-{i}"), ""))
        .collect();
    write_fixture(&tmp, &rows, &history, &notify_log);
    let args = base_args(&tmp, &["--summary-json", r#"{"age_days": 9.0}"#]);
    let o = parse_args(&args).unwrap();

    run_tick_mode(&o, &ok_gate, &thread_spawner());

    let row = wait_for_row(&events, "evals_scheduled_run", Duration::from_secs(20)).unwrap();
    assert_eq!(row["data"]["task_count"], Value::from(3));
}

fn dead_pid() -> u32 {
    let mut child = Command::new("/bin/sleep").arg("0").spawn().unwrap();
    let pid = child.id();
    let _ = child.wait();
    pid
}

fn acquire_with(tmp: &TempDir, pid: u32) {
    let out = claims::acquire(
        CLAIM_KEY,
        &pid.to_string(),
        claims::AcquireOpts {
            pid: Some(pid),
            pid_unavailable: false,
            ttl_ms: Some(3_600_000),
            reason: None,
            metadata: None,
            pid_provenance: None,
            root: Some(tmp.path().join("claims-root")),
            events_dir: Some(tmp.path().to_path_buf()),
        },
    );
    assert!(
        matches!(out, claims::AcquireOutcome::Acquired(_)),
        "{out:?}"
    );
}

// AC2-EDGE, live holder: in_flight, nothing journaled, no child.
#[test]
fn live_holder_skips_in_flight() {
    let tmp = TempDir::new().unwrap();
    acquire_with(&tmp, std::process::id());
    let args = base_args(&tmp, &["--summary-json", r#"{"age_days": 9.0}"#]);
    let o = parse_args(&args).unwrap();

    run_tick_mode(&o, &ok_gate, &|_a| panic!("must not spawn"));

    assert_eq!(
        count_kind(&tmp.path().join("events.jsonl"), "evals_stale"),
        0
    );
    assert_eq!(
        claim_state(&tmp.path().join("claims-root")),
        claims::ClaimState::Live
    );
}

// AC2-EDGE, dead holder: journal stale reason error, release, continue.
#[test]
fn dead_holder_journals_stale_releases_and_continues() {
    let tmp = TempDir::new().unwrap();
    let claims_root = tmp.path().join("claims-root");
    acquire_with(&tmp, dead_pid());
    let notify_log = tmp.path().join("notify.log");
    write_fixture(&tmp, &[], &tmp.path().join("history.jsonl"), &notify_log);
    let args = base_args(&tmp, &["--summary-json", r#"{"age_days": 9.0}"#]);
    let o = parse_args(&args).unwrap();

    run_tick_mode(&o, &ok_gate, &|argv: &[String]| {
        // Record that the tick reached the launch step, then decline so the
        // claim's post-release state stays observable.
        fs::write(tmp.path().join("launched"), "1").unwrap();
        Err(argv.first().cloned().unwrap_or_default())
    });

    assert_eq!(
        count_kind(&tmp.path().join("events.jsonl"), "evals_stale"),
        1
    );
    assert!(
        tmp.path().join("launched").exists(),
        "tick continued past the dead holder"
    );
    // The dead holder's claim was released; the spawner declined, so the
    // claim is gone (the tick's own acquire never ran).
    assert_eq!(claim_state(&claims_root), claims::ClaimState::Free);
}

// The process-group kill: a fixture that outlives the run budget is killed
// and journaled as a timeout, and the claim is released.
#[test]
fn run_timeout_kills_group_journals_timeout() {
    let tmp = TempDir::new().unwrap();
    let events = tmp.path().join("events.jsonl");
    let notify_log = tmp.path().join("notify.log");
    // A fixture whose evals run sleeps far past the budget.
    let mut script = String::from("#!/bin/sh\ncase \"$1/$2\" in\n  doctor/evals)\n    sleep 30\n    ;;\n  inbox/notify)\n    echo \"$*\" >> '");
    script += &notify_log.to_string_lossy();
    script += "'\n    ;;\nesac\n";
    let bin = tmp.path().join("fno-bin.sh");
    fs::write(&bin, script).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let args = base_args(
        &tmp,
        &[
            "--run",
            "--started",
            &Utc::now().to_rfc3339(),
            "--run-timeout-s",
            "1",
        ],
    );
    let o = parse_args(&args).unwrap();

    run_run_mode(&o);

    let row = wait_for_row(&events, "evals_stale", Duration::from_secs(5)).unwrap();
    assert_eq!(row["data"]["reason"], "timeout");
    assert_eq!(
        claim_state(&tmp.path().join("claims-root")),
        claims::ClaimState::Free
    );
}

// Usage: a missing required flag exits 2.
#[test]
fn usage_error_exits_2() {
    assert_eq!(run_evals_arm(&[]), 2);
    let missing = ["--history".to_string(), "/tmp/h.jsonl".to_string()];
    assert_eq!(run_evals_arm(&missing), 2);
}
