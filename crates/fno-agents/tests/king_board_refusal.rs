//! The king board's budget flows inward, and every refusal names a source.
//!
//! The caller hands the board its own whole-board budget (`--budget-ms`), the
//! board self-enforces and returns a payload naming what it could not read,
//! and the king's refusal therefore quotes a SOURCE NAME - never an elapsed
//! time. A timeout of the outer transport now means the board did not
//! self-report in time, which is a bug in the board's deadline enforcement
//! rather than a slow source, and the message says exactly that.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn make_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = fs::metadata(&tmp).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&tmp, perms).unwrap();
    fs::rename(&tmp, &path).unwrap();
    path
}

fn king_manifest(dir: &Path, fno_id: &str) -> PathBuf {
    let path = dir.join("king-state.md");
    fs::write(
        &path,
        format!(
            "---\nfno_id: {fno_id}\ncreated_at: 2026-08-18T00:00:00Z\nscope: board drain\n\
             harness: claude\nbudget_max_iterations: 40\n---\n"
        ),
    )
    .unwrap();
    path
}

fn king_fire(state: &Path, cwd: &Path, events: &Path, fno_bin: &Path) -> (i32, serde_json::Value) {
    king_fire_with(state, cwd, events, fno_bin, &[])
}

fn king_fire_with(
    state: &Path,
    cwd: &Path,
    events: &Path,
    fno_bin: &Path,
    extra_args: &[String],
) -> (i32, serde_json::Value) {
    let mut args = vec![
        "loop-check".to_string(),
        "--driver".to_string(),
        "king".to_string(),
        "--state".to_string(),
        state.to_str().unwrap().to_string(),
        "--transcript".to_string(),
        cwd.join("transcript.jsonl").to_str().unwrap().to_string(),
        "--cwd".to_string(),
        cwd.to_str().unwrap().to_string(),
        "--events".to_string(),
        events.to_str().unwrap().to_string(),
        "--global-events".to_string(),
        events.to_str().unwrap().to_string(),
        "--fno-bin".to_string(),
        fno_bin.to_str().unwrap().to_string(),
    ];
    args.extend_from_slice(extra_args);
    let (code, json) = fno_agents::loopcheck::run_loop_check_capture(&args);
    (code, serde_json::from_str(&json).unwrap())
}

#[test]
fn the_board_read_carries_the_callers_budget() {
    // The board derives every per-source slice from ONE total, and that total
    // is the bound this caller enforces. A budget handed in is the only thing
    // that keeps an inner per-read default from being re-invented.
    let tmp = TempDir::new().unwrap();
    let state = king_manifest(tmp.path(), "king-budget");
    let record = tmp.path().join("board-args");
    let script = make_script(
        tmp.path(),
        "fno-board-budget-record",
        &format!(
            "printf '%s' \"$*\" > '{}'\ncat <<'JSON'\n{{\"actionable\": 0, \"unreadable\": 0, \"queues\": []}}\nJSON\n",
            record.display()
        ),
    );

    let events = tmp.path().join("events.jsonl");
    let (code, decision) = king_fire(&state, tmp.path(), &events, &script);
    assert_eq!(code, 0, "{decision}");

    let args = fs::read_to_string(record).unwrap();
    assert!(args.contains("inbox board --json --state"), "{args}");
    let flag = args
        .split_whitespace()
        .find(|a| a.starts_with("--budget-ms"));
    assert_eq!(flag, Some("--budget-ms"), "{args}");
    let idx = args
        .split_whitespace()
        .position(|a| a == "--budget-ms")
        .unwrap();
    let value: u64 = args
        .split_whitespace()
        .nth(idx + 1)
        .expect("budget-ms carries its millisecond value")
        .parse()
        .expect("the budget value is a plain integer of milliseconds");
    assert!(value > 0, "{args}");
}

#[test]
fn an_unreadable_queue_names_its_source_in_the_king_message() {
    // AC2-HP: the board's payload names the source; the king's refusal quotes
    // that name, never an elapsed-seconds reading.
    let tmp = TempDir::new().unwrap();
    let state = king_manifest(tmp.path(), "king-named-source");
    let payload = r#"{
      "actionable": 1, "unreadable": 1,
      "queues": [
        {"name":"undispatched","status":"unreadable","actionable":true,
         "count":null,"rows":[],
         "error":"not-read: board budget exhausted after gh pr list","note":"","source":"s"}
      ]
    }"#;
    let script = make_script(
        tmp.path(),
        "fno-board-blind-payload",
        &format!("cat <<'JSON'\n{payload}\nJSON\n"),
    );

    let events = tmp.path().join("events.jsonl");
    let (code, decision) = king_fire(&state, tmp.path(), &events, &script);
    // The blind-board block is the ordinary block path: the shim keys on the
    // DECISION field, never the exit code.
    assert_eq!(code, 0, "{decision}");
    assert_eq!(decision["decision"], "block", "{decision}");
    let reason = decision["reason"].as_str().unwrap();
    assert!(reason.contains("undispatched"), "{reason}");
    assert!(
        reason.contains("board budget exhausted"),
        "the refusal quotes the board's named source: {reason}"
    );
}

#[test]
fn an_outer_timeout_says_the_board_did_not_self_report() {
    // AC2-EDGE: the outer bound firing is a DIFFERENT failure from a slow
    // source - the board was handed this same budget and answers with a
    // payload naming sources. A transport timeout therefore indicts the
    // board's deadline enforcement, and the message says so instead of
    // reading as an overloaded machine.
    let tmp = TempDir::new().unwrap();
    let state = king_manifest(tmp.path(), "king-self-report");
    let script = make_script(tmp.path(), "fno-board-silent", "sleep 30\n");

    let events = tmp.path().join("events.jsonl");
    // A 500ms read bound: the mock sleeps far past it, so the transport kill
    // wins the race the production 30s-vs-sleep-30 tie once left to chance.
    let (code, decision) = king_fire_with(
        &state,
        tmp.path(),
        &events,
        &script,
        &["--read-timeout-ms".to_string(), "500".to_string()],
    );
    assert_eq!(code, 2, "{decision}");
    let reason = decision["reason"].as_str().unwrap();
    assert!(reason.contains("did not self-report"), "{reason}");
    assert!(reason.contains("budget"), "{reason}");
    assert!(
        !reason.contains("timed out after"),
        "the old clock-only wording must be gone: {reason}"
    );
}
