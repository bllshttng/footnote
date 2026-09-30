//! Daemon-owned expiry for sessions parked on a local `<watching>` intent.
//!
//! The hook records the declaration in the checked global event stream. This
//! arm folds only a bounded recent window, confirms the same session still
//! owns exactly one live node claim, and uses the burn-watch mail/resume lane.
//! It never signals or kills the watched process.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::paths::AgentsHome;

const INTERVAL: Duration = Duration::from_secs(60);
const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_EVENTS: u32 = 10_000;
const MAX_LIVE_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;
const WATCH_IDLE: &str = "loop_check_watch_idle";
const WAKE_EVENT: &str = "loop_check_watch_expiry_wake";
const ACTIVITY_AND_TERMINAL: &[&str] = &[
    "loop_check",
    "termination",
    "mission_complete",
    "agent_stopped",
    "agent_exited",
    "inside_leg_completed",
];

#[derive(Default)]
pub struct Arm {
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
pub(crate) struct Watch {
    pub event_id: String,
    pub seq: i64,
    pub session_id: String,
    pub node: String,
    pub blocker: String,
    pub task_id: Option<String>,
    pub reason: Option<String>,
    pub expires_at_ms: i64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct OverdueWatch {
    pub session_id: String,
    pub overdue_ms: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct Evidence {
    pub event_id: String,
    pub seq: i64,
    pub ts_ms: i64,
    pub kind: String,
    pub session_id: Option<String>,
    pub data: Value,
}

pub(crate) fn should_wake(watch: &Watch, now_ms: i64, evidence: &[Evidence]) -> bool {
    if now_ms < watch.expires_at_ms {
        return false;
    }
    is_current_watch(watch, evidence)
        && !evidence.iter().any(|row| {
            row.session_id.as_deref() == Some(watch.session_id.as_str())
                && row.kind == WAKE_EVENT
                && row.data.get("watch_event_id").and_then(Value::as_str)
                    == Some(watch.event_id.as_str())
        })
}

pub(crate) fn is_current_watch(watch: &Watch, evidence: &[Evidence]) -> bool {
    !evidence.iter().any(|row| {
        if row.session_id.as_deref() != Some(watch.session_id.as_str()) {
            return false;
        }
        let is_later = (row.ts_ms, row.seq) > (watch.ts_ms, watch.seq);
        match row.kind.as_str() {
            WATCH_IDLE => is_later,
            "loop_check" => {
                is_later && row.data.get("intent").and_then(Value::as_str) != Some("watching")
            }
            "termination"
            | "mission_complete"
            | "agent_stopped"
            | "agent_exited"
            | "inside_leg_completed" => is_later,
            _ => false,
        }
    })
}

fn millis_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn event_id(line: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(line.as_bytes()))
}

fn read_evidence(home: &AgentsHome, now_ms: i64) -> Result<Vec<Evidence>, String> {
    let mut types = vec![WATCH_IDLE, WAKE_EVENT];
    types.extend_from_slice(ACTIVITY_AND_TERMINAL);
    let query = crate::event_store::EventQuery {
        since_ms: Some(now_ms.saturating_sub(WINDOW_MS)),
        limit: Some(MAX_EVENTS + 1),
        ..crate::event_store::EventQuery::of_types(&types)
    };
    let global_events = crate::daemon::global_events_path(home);
    if !crate::event_store::store_path(&global_events).is_file()
        && std::fs::metadata(&global_events)
            .map(|metadata| metadata.len() > MAX_LIVE_JOURNAL_BYTES)
            .unwrap_or(false)
    {
        return Err(format!(
            "watch expiry live journal exceeds {MAX_LIVE_JOURNAL_BYTES} bytes"
        ));
    }
    let text = crate::event_store::journal_text_checked(&global_events, &query)?;
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() > MAX_EVENTS as usize {
        return Err(format!(
            "watch expiry event window exceeds {MAX_EVENTS} rows"
        ));
    }
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let value: Value = serde_json::from_str(line)
                .map_err(|error| format!("watch expiry event row is invalid: {error}"))?;
            let data = value.get("data").cloned().unwrap_or(Value::Null);
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let session_id = data
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let ts_ms = timestamp_ms(&value)
                .ok_or_else(|| "watch expiry event has no valid timestamp".to_string())?;
            if ts_ms > now_ms {
                return Err("watch expiry event has a future timestamp".to_string());
            }
            Ok(Evidence {
                event_id: event_id(line),
                seq: index as i64,
                ts_ms,
                kind,
                session_id,
                data,
            })
        })
        .filter_map(|row| match row {
            Ok(row) if row.ts_ms >= now_ms.saturating_sub(WINDOW_MS) => Some(Ok(row)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn timestamp_ms(value: &Value) -> Option<i64> {
    value.get("ts_ms").and_then(Value::as_i64).or_else(|| {
        value
            .get("ts")
            .and_then(Value::as_str)
            .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
            .map(|stamp| stamp.timestamp_millis())
    })
}

fn watches(evidence: &[Evidence]) -> Vec<Watch> {
    let mut latest = std::collections::BTreeMap::<String, Watch>::new();
    for row in evidence.iter().filter(|row| row.kind == WATCH_IDLE) {
        let Some(session_id) = row.session_id.clone() else {
            continue;
        };
        let data = &row.data;
        let Some(expires_at_ms) = data.get("expires_at_ms").and_then(Value::as_i64) else {
            continue;
        };
        let watch = Watch {
            event_id: row.event_id.clone(),
            seq: row.seq,
            session_id: session_id.clone(),
            node: data
                .get("node")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            blocker: data
                .get("blocker")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            task_id: data
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            reason: data
                .get("reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.trim().is_empty())
                .map(str::to_string),
            expires_at_ms,
            ts_ms: row.ts_ms,
        };
        if latest
            .get(&session_id)
            .map_or(true, |old| (watch.ts_ms, watch.seq) > (old.ts_ms, old.seq))
        {
            latest.insert(session_id, watch);
        }
    }
    latest.into_values().collect()
}

pub(crate) fn overdue(home: &AgentsHome) -> Result<Vec<OverdueWatch>, String> {
    let now_ms = millis_now();
    let evidence = read_evidence(home, now_ms)?;
    let mut overdue = Vec::new();
    for watch in watches(&evidence)
        .into_iter()
        .filter(|watch| now_ms >= watch.expires_at_ms && is_current_watch(watch, &evidence))
    {
        let Some(node) = current_node_claim(home, &watch.session_id)? else {
            continue;
        };
        if !watch.node.is_empty() && node != watch.node {
            continue;
        }
        overdue.push(OverdueWatch {
            session_id: watch.session_id,
            overdue_ms: now_ms.saturating_sub(watch.expires_at_ms),
        });
    }
    Ok(overdue)
}

pub(crate) fn current_node_claim(
    home: &AgentsHome,
    session_id: &str,
) -> Result<Option<String>, String> {
    let registry = crate::state::load_registry(&home.registry_json())
        .map_err(|error| format!("registry read failed: {error}"))?;
    let row = registry
        .entries
        .iter()
        .find(|row| registry_session_id(row) == Some(session_id));
    let Some(row) = row else {
        return Ok(None);
    };
    if matches!(
        row.status,
        crate::AgentStatus::Exited
            | crate::AgentStatus::PermanentDead
            | crate::AgentStatus::Failed
            | crate::AgentStatus::Orphaned
    ) {
        return Ok(None);
    }
    let records = crate::claims::list_strict(Some("node:"), None, false)
        .map_err(|error| format!("node claim read failed: {error}"))?;
    let mut owned = Vec::new();
    for record in records
        .iter()
        .filter(|record| record.session_id.as_deref() == Some(session_id))
    {
        match crate::claims::status(&record.key, None).0 {
            crate::claims::ClaimState::Live => {
                if let Some(node) = record.key.strip_prefix("node:") {
                    owned.push(node.to_string());
                }
            }
            crate::claims::ClaimState::Corrupted => {
                return Err(format!("node claim {} is unreadable", record.key));
            }
            crate::claims::ClaimState::Free
            | crate::claims::ClaimState::Suspect
            | crate::claims::ClaimState::Stale => {}
        }
    }
    match owned.as_slice() {
        [node] => Ok(Some(node.clone())),
        [] => Ok(None),
        _ => Err(format!(
            "session {session_id} owns multiple live node claims: {}",
            owned.join(", ")
        )),
    }
}

fn registry_session_id(entry: &crate::state::RegistryEntry) -> Option<&str> {
    entry
        .harness_session_id
        .as_deref()
        .or(entry.session_id.as_deref())
}

pub(crate) fn message(watch: &Watch) -> String {
    let task = watch.task_id.as_deref().unwrap_or("missing; do not guess");
    let blocker = match watch.reason.as_deref() {
        Some(blocker @ ("ci" | "review" | "merge_slot" | "local")) => blocker,
        _ => watch.blocker.as_str(),
    };
    format!(
        "Automatic watch-expiry notice from the fno daemon. Your {blocker} watch expired for node {} (reason: {}). Harness task id: {task}. Check the watcher output or status. If a local task is still running, kill it; then continue or re-arm a bounded watch.",
        watch.node,
        watch.reason.as_deref().unwrap_or("watch deadline expired")
    )
}

pub(crate) fn run_pass(home: &AgentsHome) -> Result<(), String> {
    let now_ms = millis_now();
    let evidence = read_evidence(home, now_ms)?;
    let emitter =
        crate::events::EventEmitter::new(crate::daemon::global_events_path(home), "daemon");
    let mut runner: crate::burn_watch::Runner = &mut crate::burn_watch::run_command;
    let mut delivery_failures = 0usize;
    let mut eligibility_failures = 0usize;
    let mut receipt_failures = 0usize;
    let mut first_error = None;
    for watch in watches(&evidence) {
        if !should_wake(&watch, now_ms, &evidence) {
            continue;
        }
        let node = match current_node_claim(home, &watch.session_id) {
            Ok(Some(node)) => node,
            Ok(None) => continue,
            Err(error) => {
                eligibility_failures += 1;
                first_error.get_or_insert(error);
                continue;
            }
        };
        if !watch.node.is_empty() && node != watch.node {
            continue;
        }
        let mut claimed_watch = watch.clone();
        claimed_watch.node = node;
        let text = message(&claimed_watch);
        let (delivered, via) = crate::burn_watch::wake_with_text(
            &watch.session_id,
            false,
            &text,
            "burn-watch",
            &mut runner,
        );
        if !delivered && via != "durable" {
            delivery_failures += 1;
            first_error.get_or_insert_with(|| {
                format!(
                    "wake delivery failed for watch {} via {via}",
                    watch.event_id
                )
            });
            continue;
        }
        if let Err(error) = emitter.emit(
            WAKE_EVENT,
            &serde_json::json!({
                "session_id": watch.session_id,
                "node": claimed_watch.node,
                "watch_event_id": watch.event_id,
                "blocker": claimed_watch.blocker,
                "task_id": watch.task_id,
                "expires_at_ms": watch.expires_at_ms,
                "delivered": delivered,
                "via": via,
                "reason": watch.reason.as_deref().unwrap_or("watch deadline expired")
            }),
        ) {
            receipt_failures += 1;
            first_error.get_or_insert_with(|| {
                format!(
                    "wake {} was accepted via {via}, but its receipt could not be stored: {error}",
                    watch.event_id
                )
            });
        }
    }
    if eligibility_failures > 0 || delivery_failures > 0 || receipt_failures > 0 {
        return Err(format!(
            "eligibility failures={eligibility_failures}, delivery failures={delivery_failures}, receipt failures={receipt_failures}; accepted wakes with missing receipts retry at least once and may redeliver; first error: {}",
            first_error.unwrap_or_else(|| "unknown watch-expiry failure".into())
        ));
    }
    Ok(())
}

pub fn maybe_tick(arm: &Arm, home: AgentsHome) {
    {
        let mut last = arm
            .last_tick
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if last.is_some_and(|tick| tick.elapsed() < INTERVAL)
            || arm.in_flight.swap(true, Ordering::SeqCst)
        {
            return;
        }
        *last = Some(Instant::now());
    }
    let flag = Arc::clone(&arm.in_flight);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        if let Err(error) = run_pass(&home) {
            eprintln!("watch-expiry: {error}");
        }
    });
}
