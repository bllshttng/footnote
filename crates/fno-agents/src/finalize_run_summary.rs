//! a2a status-breakpoint `run_summary`: count, dedupe, emit, and push the
//! terminal summary event a parked session re-fires on every stop.

use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

/// Payload cap for the run_summary `data` object (the schema's
/// `limits.max_data_bytes`, which the daemon EventEmitter also enforces).
/// run_summary is lean by construction, but honoring the cap keeps the Rust
/// path's behavior identical to the emitter.
const RUN_SUMMARY_DATA_CAP: usize = 500;

/// Count the run's task ticks from committed store rows. Correlates on the
/// envelope-level `run` (the target-run id), so a co-located second run's
/// events never mix in. tasks_failed counts task_done events whose outcome is
/// FAILED - the gap (tasks_started > tasks_done) is what exposes a crashed
/// executor (AC2-FR).
pub(crate) fn count_run_tasks(project_events: &Path, run: &str) -> (u64, u64, u64) {
    let (mut started, mut done, mut failed) = (0u64, 0u64, 0u64);
    if crate::event_store::import_all(project_events).is_err() {
        return (0, 0, 0);
    }
    let rows = match crate::event_store::query_events(
        project_events,
        &crate::event_store::EventQuery {
            types: vec!["task_started".to_string(), "task_done".to_string()],
            ..Default::default()
        },
    ) {
        Ok(rows) => rows,
        Err(_) => return (0, 0, 0),
    };
    for row in rows {
        let Ok(v) = serde_json::from_str::<Value>(&row.line) else {
            continue;
        };
        if v.get("run").and_then(|r| r.as_str()) == Some(run) {
            match v.get("type").and_then(|t| t.as_str()) {
                Some("task_started") => started += 1,
                Some("task_done") => {
                    done += 1;
                    if v.get("outcome").and_then(|o| o.as_str()) == Some("FAILED") {
                        failed += 1;
                    }
                }
                _ => {}
            }
        }
    }
    (started, done, failed)
}

/// True when this run already has a run_summary carrying `reason`. Finalize
/// re-runs on every stop of a session parked at a non-ship terminal, and each
/// fire used to emit + push a fresh copy (one king got 14 mails in 67 minutes
/// for a single DoneAwaitingMerge run). Keys on run PLUS reason so a session
/// that hits Budget and then resumes to DoneAwaitingMerge still reports the
/// new terminal. Committed rows are the record (the cutover stopped journal
/// appends); a missing or unreadable store is a false (emit and push as
/// before).
pub(crate) fn run_summary_already_emitted(project_events: &Path, run: &str, reason: &str) -> bool {
    if crate::event_store::import_all(project_events).is_err() {
        return false;
    }
    let rows = match crate::event_store::query_events(
        project_events,
        &crate::event_store::EventQuery {
            types: vec!["run_summary".to_string()],
            ..Default::default()
        },
    ) {
        Ok(rows) => rows,
        Err(_) => return false,
    };
    rows.iter().any(|row| {
        serde_json::from_str::<Value>(&row.line)
            .ok()
            .is_some_and(|v| {
                v.get("run").and_then(|r| r.as_str()) == Some(run)
                    && v.pointer("/data/termination_reason")
                        .and_then(|r| r.as_str())
                        == Some(reason)
            })
    })
}

/// Append a pre-built extended envelope through the shared Branch-A mutex.
/// Non-fatal: a write failure logs and returns, never wedging finalize.
fn append_envelope(path: &Path, envelope: &Value) {
    if let Err(error) =
        crate::claims::append_event_line(path, envelope, std::time::Duration::from_secs(2))
    {
        eprintln!(
            "finalize: run_summary write to {} failed: {error}",
            path.display()
        );
    }
}

/// Build + emit the run_summary terminal event to both event logs. Best-effort
/// throughout: emission never changes the exit code or holds session_finalized.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_run_summary(
    project_events: &Path,
    global_events: &Path,
    run: &str,
    node: Option<&str>,
    ship: bool,
    reason: &str,
    pr_url: Option<&str>,
) {
    let (started, done, failed) = count_run_tasks(project_events, run);
    // Terminal reason -> return-contract outcome: a ship terminal is SUCCESS
    // (DONE_WITH_CONCERNS if any task failed); a non-ship terminal (Budget /
    // NoProgress / Interrupted) is FAILED.
    let outcome = if !ship {
        "FAILED"
    } else if failed > 0 {
        "DONE_WITH_CONCERNS"
    } else {
        "SUCCESS"
    };
    let mut data = json!({
        "tasks_started": started,
        "tasks_done": done,
        "tasks_failed": failed,
        "termination_reason": reason,
    });
    if let Some(url) = pr_url {
        data["pr_url"] = json!(url);
    }
    // Honor the payload cap (AC2-EDGE, Rust path): oversized data -> the small
    // meta-event, so an auditor sees the drop rather than a silently huge line.
    let payload_len = serde_json::to_string(&data).map(|s| s.len()).unwrap_or(0);
    if payload_len > RUN_SUMMARY_DATA_CAP {
        data = json!({"intended_kind": "run_summary", "size": payload_len});
    }
    let mut env = json!({
        "ts": crate::loopcheck::now_rfc3339_utc(),
        "v": 1,
        "type": "run_summary",
        "source": "target",
        "run": run,
        "outcome": outcome,
        "data": data,
    });
    if let Some(n) = node {
        env["node"] = json!(n);
    }
    append_envelope(project_events, &env);
    if project_events != global_events {
        append_envelope(global_events, &env);
    }
}

/// Push leg for run_summary: notify the parent handle. run_summary
/// emits natively above, so the push shells the Python resolver (`fno doctor event
/// push-parent`) rather than reimplementing registry lookup + mail in Rust.
/// Best-effort: a missing `fno` / no spawn lineage is a silent skip; the
/// events.jsonl line already landed independently (AC1-FR). `fno` (not a bare
/// interpreter) is safe to shell - a PATH miss just skips.
pub(crate) fn push_run_summary_to_parent(run: &str, node: Option<&str>, reason: &str) {
    let mut cmd = Command::new("fno");
    cmd.args([
        "doctor",
        "event",
        "push-parent",
        "--type",
        "run_summary",
        "--run",
        run,
        "--reason",
        reason,
    ]);
    if let Some(n) = node {
        cmd.args(["--node", n]);
    }
    if let Err(e) = cmd.output() {
        eprintln!("finalize: run_summary parent push skipped (non-fatal): {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;

    #[test]
    fn count_run_tasks_correlates_on_run_and_flags_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let events = tmp.path().join("events.jsonl");
        fs::write(
            &events,
            "{\"ts\":\"2026-01-01T00:00:00Z\",\"type\":\"task_started\",\"source\":\"target\",\"run\":\"R1\",\"data\":{}}\n\
             {\"ts\":\"2026-01-01T00:00:01Z\",\"type\":\"task_started\",\"source\":\"target\",\"run\":\"R1\",\"data\":{}}\n\
             {\"ts\":\"2026-01-01T00:00:02Z\",\"type\":\"task_done\",\"source\":\"target\",\"run\":\"R1\",\"outcome\":\"SUCCESS\",\"data\":{}}\n\
             {\"ts\":\"2026-01-01T00:00:03Z\",\"type\":\"task_done\",\"source\":\"target\",\"run\":\"R1\",\"outcome\":\"FAILED\",\"data\":{}}\n\
             {\"ts\":\"2026-01-01T00:00:04Z\",\"type\":\"task_started\",\"source\":\"target\",\"run\":\"OTHER\",\"data\":{}}\n\
             not json\n",
        )
        .unwrap();
        // R1: 2 started, 2 done, 1 failed; the OTHER-run row and the rejected
        // junk row are ignored.
        assert_eq!(count_run_tasks(&events, "R1"), (2, 2, 1));
    }

    #[test]
    fn emit_run_summary_writes_extended_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let events = tmp.path().join("events.jsonl");
        // pre-seed one started with no matching done -> exposes the gap (AC2-FR).
        fs::write(
            &events,
            "{\"ts\":\"2026-01-01T00:00:00Z\",\"type\":\"task_started\",\"source\":\"target\",\"run\":\"R9\",\"data\":{}}\n",
        )
        .unwrap();
        emit_run_summary(
            &events,
            &events,
            "R9",
            Some("prj-0001"),
            true,
            "DonePRGreen",
            None,
        );
        let rows = crate::event_store::query_events(
            &events,
            &crate::event_store::EventQuery {
                types: vec!["run_summary".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 1, "one committed run_summary");
        let last: Value = serde_json::from_str(&rows[0].line).unwrap();
        assert_eq!(last["type"], "run_summary");
        assert_eq!(last["v"], 1);
        assert_eq!(last["run"], "R9");
        assert_eq!(last["node"], "prj-0001");
        assert_eq!(last["outcome"], "SUCCESS");
        assert_eq!(last["data"]["tasks_started"], 1);
        assert_eq!(last["data"]["tasks_done"], 0);
        assert_eq!(last["data"]["termination_reason"], "DonePRGreen");
    }

    #[test]
    fn emit_run_summary_non_ship_is_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let events = tmp.path().join("events.jsonl");
        emit_run_summary(&events, &events, "R2", None, false, "NoProgress", None);
        let rows = crate::event_store::query_events(
            &events,
            &crate::event_store::EventQuery {
                types: vec!["run_summary".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        let ev: Value = serde_json::from_str(&rows[0].line).unwrap();
        assert_eq!(ev["outcome"], "FAILED");
        assert!(ev.get("node").is_none(), "no node -> omitted, not null");
    }

    #[test]
    fn run_summary_already_emitted_matches_run_and_reason() {
        // Only a run_summary for THIS run AND this reason satisfies the gate:
        // another reason (Budget then DoneAwaitingMerge) or another run still
        // reports. A malformed line must not abort the scan.
        let dir = std::env::temp_dir().join(format!("fin-rse-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("events.jsonl");
        fs::write(
            &log,
            concat!(
                r#"{"ts":"2026-01-01T00:00:00Z","type":"session_finalized","source":"target","run":"R1","data":{"session_id":"S1","ship":false}}"#,
                "\n",
                r#"{"ts":"2026-01-01T00:00:01Z","type":"run_summary","source":"target","run":"R1","data":{"termination_reason":"Budget"}}"#,
                "\n",
                "not json\n",
                r#"{"ts":"2026-01-01T00:00:02Z","type":"run_summary","source":"target","run":"R2","data":{"termination_reason":"DoneAwaitingMerge"}}"#,
                "\n",
                r#"{"ts":"2026-01-01T00:00:02Z","type":"run_summary","source":"target","run":"R1","data":{"termination_reason":"DoneAwaitingMerge"}}"#,
                "\n",
            ),
        )
        .unwrap();
        assert!(run_summary_already_emitted(&log, "R1", "DoneAwaitingMerge"));
        assert!(!run_summary_already_emitted(&log, "R1", "DonePRGreen"));
        assert!(!run_summary_already_emitted(&log, "R2", "Budget"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_summary_already_emitted_missing_file_is_false() {
        // A missing log is a false, so the first fire emits and pushes as
        // before (AC3-ERR).
        assert!(!run_summary_already_emitted(
            Path::new("/nonexistent/fin-rse-missing/events.jsonl"),
            "R1",
            "DoneAwaitingMerge"
        ));
    }
}
