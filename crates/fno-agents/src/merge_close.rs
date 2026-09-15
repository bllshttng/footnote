//! The merge_close arm: one bare reconcile sweep per beat closes a merged
//! PR's node with no worker alive.
//!
//! DonePRGreen means the PR merged; it does not mean the node closed (the
//! specimen: a king stopped the worker four minutes after its merge, and
//! the node read in_review until a peer ran reconcile by hand). The
//! merge verb closes its node in a child bound to the worker's lifetime, so
//! a killed, crashed, or 429-dead worker takes the closer with it. This arm
//! is the session-free floor: it shells the same bare
//! `fno backlog reconcile --json` the SessionStart hook runs, with no
//! session in the loop. The closure decision stays in
//! `graph._reconcile.scan_merge_drift`; nothing here re-implements it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::loops_pause::DispatchPause;
use crate::paths::AgentsHome;

/// The arm's beat, matching `RECONCILE_THROTTLE_SECONDS` in
/// scripts/lib/reconcile-throttle.sh, so this arm and the SessionStart hook
/// share one cadence. Worst-case closure lag after a stranded merge is one
/// interval plus one sweep.
pub const MERGE_CLOSE_INTERVAL_S: u64 = 900;

/// The arm as the daemon holds it: cadence stamp plus one-in-flight gate.
#[derive(Default)]
pub struct Arm {
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
}

/// One tick's outcome: the tick row's `acted`, `skip_reason` and `detail`.
pub struct CloseOutcome {
    pub acted: u64,
    pub skip_reason: Option<String>,
    pub detail: String,
}

fn short(text: &str) -> String {
    text.chars().take(200).collect()
}

fn unreadable() -> CloseOutcome {
    CloseOutcome {
        acted: 0,
        skip_reason: Some("error".to_string()),
        detail: "unreadable reconcile json".to_string(),
    }
}

/// Turn one reconcile run into a tick outcome. Pure, so tests drive it with
/// strings. The verb pretty-prints its JSON across many lines, so the whole
/// trimmed stdout parses as one object - never a search for a `{` line. A
/// run that did not report a readable `closed` array is an error, never a
/// zero: a fabricated zero reads as "in sync" (the stale-summary rule).
pub fn outcome_from_reconcile(run: Result<String, String>) -> CloseOutcome {
    let stdout = match run {
        Ok(stdout) => stdout,
        Err(why) => {
            return CloseOutcome {
                acted: 0,
                skip_reason: Some("error".to_string()),
                detail: short(&format!("reconcile: {why}")),
            };
        }
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return unreadable();
    };
    if parsed.get("held").and_then(serde_json::Value::as_bool) == Some(true) {
        // The single-flight receipt: another reconcile owns the scope, and a
        // held tick is not an error.
        let holder = parsed
            .get("holder")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let held_for_s = parsed
            .get("held_for_s")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        return CloseOutcome {
            acted: 0,
            skip_reason: Some("held".to_string()),
            detail: format!("flight held by {holder} for {held_for_s}s"),
        };
    }
    let Some(closed) = parsed.get("closed").and_then(serde_json::Value::as_array) else {
        return unreadable();
    };
    let count_of = |key: &str| {
        parsed
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map_or(0, |a| a.len()) as u64
    };
    let acted = closed.len() as u64;
    let failures = count_of("failures");
    CloseOutcome {
        acted,
        skip_reason: if acted > 0 {
            None
        } else if failures > 0 {
            Some("failures".to_string())
        } else {
            Some("none_to_close".to_string())
        },
        detail: format!(
            "closed={acted} promise_unmet={} failures={failures}",
            count_of("promise_unmet")
        ),
    }
}

/// The production runner: the bare sweep, cwd-free (the graph is one store).
fn run_reconcile() -> Result<String, String> {
    let output = std::process::Command::new(crate::scrape::fno_py())
        .args(["backlog", "reconcile", "--json"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !output.status.success() {
        let last = output
            .stderr
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .next_back()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .unwrap_or_default();
        return Err(format!(
            "exit {}: {last}",
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One pass of the arm body: the pause gate, then the run, then exactly one
/// tick row - on every path, so a silent tick cannot be told from one that
/// never ran. The pause and the runner are parameters so tests drive both
/// without shelling out or touching the account-wide pause sentinel (the
/// `stale_sweep` closure pattern).
fn emit_one(
    home: &AgentsHome,
    pause: &DispatchPause,
    run: impl FnOnce() -> Result<String, String>,
) -> CloseOutcome {
    let outcome = if pause.is_paused() {
        // A close runs advance_dependents; a pause means no dispatch.
        CloseOutcome {
            acted: 0,
            skip_reason: Some(pause.skip_reason().to_string()),
            detail: pause.detail(),
        }
    } else {
        outcome_from_reconcile(run())
    };
    let journal = crate::loop_runtime::Journal::new_raw(
        home.events_jsonl(),
        crate::daemon::global_events_path(home),
    );
    crate::tick_ledger::emit_tick(
        &journal,
        "merge_close",
        crate::tick_ledger::SCHED_DAEMON,
        outcome.acted,
        outcome.skip_reason.as_deref(),
        Some(&outcome.detail),
        MERGE_CLOSE_INTERVAL_S,
    );
    outcome
}

/// The daemon-facing wrapper: due-check plus one-in-flight gate (the
/// `machine_watch::maybe_tick` shape). The sweep spends tens of seconds of
/// CPU, so the body runs off-loop.
pub fn maybe_tick(arm: &Arm, home: AgentsHome) {
    maybe_tick_with(arm, home, run_reconcile);
}

fn maybe_tick_with(
    arm: &Arm,
    home: AgentsHome,
    run: impl FnOnce() -> Result<String, String> + Send + 'static,
) {
    let interval = Duration::from_secs(MERGE_CLOSE_INTERVAL_S);
    {
        let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < interval)
            || arm.in_flight.swap(true, Ordering::SeqCst)
        {
            return;
        }
        *last = Some(Instant::now());
    }
    let flag = Arc::clone(&arm.in_flight);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        emit_one(&home, &crate::loops_pause::dispatch_pause(), run);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> AgentsHome {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("merge-close-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AgentsHome::at(dir.join("home"))
    }

    #[test]
    fn one_closed_row_is_a_real_acted_count() {
        // The verb's ACTUAL pretty-printed shape: many lines, one object.
        let stdout = r#"{
  "dry_run": false,
  "candidates": [
    {"node_id": "ab-179a", "pr_number": 1797, "pr_url": "u", "plan_path": null}
  ],
  "closed": [
    {"node_id": "ab-179a", "pr_number": 1797, "pr_url": "u", "plan_stamped": true, "sentinel": "s"}
  ],
  "promise_unmet": [
    {"node_id": "a", "reason": "r"},
    {"node_id": "b", "reason": "r"}
  ],
  "failures": []
}"#;
        let o = outcome_from_reconcile(Ok(stdout.to_string()));
        assert_eq!(o.acted, 1);
        assert_eq!(o.skip_reason, None);
        assert_eq!(o.detail, "closed=1 promise_unmet=2 failures=0");
    }

    #[test]
    fn a_failed_run_is_an_error_never_a_zero() {
        let o = outcome_from_reconcile(Err("exit 1: boom".to_string()));
        assert_eq!(o.acted, 0);
        assert_eq!(o.skip_reason.as_deref(), Some("error"));
        assert_eq!(o.detail, "reconcile: exit 1: boom");
    }

    #[test]
    fn stdout_without_a_closed_array_is_an_error_never_a_zero() {
        for stdout in ["", "some other output\n", "{\"held\": false}", "[1, 2, 3]"] {
            let o = outcome_from_reconcile(Ok(stdout.to_string()));
            assert_eq!(o.acted, 0, "stdout {stdout:?}");
            assert_eq!(o.skip_reason.as_deref(), Some("error"), "stdout {stdout:?}");
            assert_eq!(o.detail, "unreadable reconcile json", "stdout {stdout:?}");
        }
    }

    #[test]
    fn a_held_receipt_names_the_holder_and_never_errors() {
        let stdout =
            r#"{"held": true, "requests": 3, "holder": "single-flight:42:ab", "held_for_s": 91}"#;
        let o = outcome_from_reconcile(Ok(stdout.to_string()));
        assert_eq!(o.acted, 0);
        assert_eq!(o.skip_reason.as_deref(), Some("held"));
        assert_eq!(o.detail, "flight held by single-flight:42:ab for 91s");
    }

    #[test]
    fn an_empty_close_with_failures_reads_failures_not_none() {
        let stdout = r#"{
  "closed": [],
  "promise_unmet": [],
  "failures": [{"node_id": "x-1", "pr_number": 1, "error": "gh down", "kind": "gh", "remedy": "retry"}]
}"#;
        let o = outcome_from_reconcile(Ok(stdout.to_string()));
        assert_eq!(o.acted, 0);
        assert_eq!(o.skip_reason.as_deref(), Some("failures"));
        assert_eq!(o.detail, "closed=0 promise_unmet=0 failures=1");
    }

    #[test]
    fn an_empty_close_in_sync_reads_none_to_close() {
        let stdout = "{\n  \"closed\": [],\n  \"promise_unmet\": [],\n  \"failures\": []\n}";
        let o = outcome_from_reconcile(Ok(stdout.to_string()));
        assert_eq!(o.acted, 0);
        assert_eq!(o.skip_reason.as_deref(), Some("none_to_close"));
        assert_eq!(o.detail, "closed=0 promise_unmet=0 failures=0");
    }

    #[test]
    fn a_young_cadence_stamp_gates_the_child_and_the_row() {
        let arm = Arm::default();
        let h = home();
        *arm.last_tick.lock().unwrap() = Some(Instant::now());
        maybe_tick_with(&arm, h.clone(), || panic!("arm must be gated"));
        assert!(!h.events_jsonl().exists(), "a gated tick wrote no row");
    }

    #[test]
    fn an_in_flight_run_gates_the_child_and_the_row() {
        let arm = Arm::default();
        let h = home();
        arm.in_flight.store(true, Ordering::SeqCst);
        maybe_tick_with(&arm, h.clone(), || panic!("arm must be gated"));
        assert!(!h.events_jsonl().exists(), "a gated tick wrote no row");
    }

    #[test]
    fn a_dispatch_pause_skips_the_child_and_still_writes_its_row() {
        let h = home();
        let pause = DispatchPause::Manual {
            state: "paused".to_string(),
            detail: "loops paused by test".to_string(),
        };
        let o = emit_one(&h, &pause, || panic!("a paused tick spawns no child"));
        assert_eq!(o.acted, 0);
        assert_eq!(o.skip_reason.as_deref(), Some("loops_paused"));
        assert_eq!(o.detail, "loops paused by test");
        let log = std::fs::read_to_string(h.events_jsonl()).unwrap_or_default();
        assert!(log.contains("\"arm\":\"merge_close\""), "log: {log}");
        assert!(
            log.contains("\"skip_reason\":\"loops_paused\""),
            "log: {log}"
        );
    }

    #[test]
    fn a_real_run_writes_exactly_one_row_on_a_temp_home() {
        let h = home();
        let pause = DispatchPause::Clear;
        let o = emit_one(&h, &pause, || {
            Ok("{\"closed\": [], \"promise_unmet\": [], \"failures\": []}".to_string())
        });
        assert_eq!(o.acted, 0);
        assert_eq!(o.skip_reason.as_deref(), Some("none_to_close"));
        let log = std::fs::read_to_string(h.events_jsonl()).unwrap_or_default();
        assert_eq!(
            log.matches("\"arm\":\"merge_close\"").count(),
            1,
            "log: {log}"
        );
        assert!(log.contains("\"interval_s\":900"), "log: {log}");
    }
}
