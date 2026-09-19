//! The daemon's question sweep: an open question whose node closed is closed
//! with it.
//!
//! A question that outlives its long-lived work gets closed on cadence; every
//! question whose node or blocks entry reads `done` or `superseded` gets one
//! `operator_question_closed` row with `reason: node-closed`, and one
//! `question_sweep` event logs the count. Graph unreadable closes nothing.
use crate::paths::AgentsHome;
use serde_json::json;
use std::path::{Path, PathBuf};

/// Sweep cadence, matching the stale sweep: the interval bounds discovery
/// lag, not freshness.
const QUESTION_SWEEP_INTERVAL_SECS: i64 = 21_600;

static QUESTION_SWEEP_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

struct Gate<'a>(&'a std::sync::atomic::AtomicBool);
impl Drop for Gate<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The daemon's tick arm: one sweep in flight, off the select loop.
pub fn daemon_tick(home: &AgentsHome, now: i64) {
    use std::sync::atomic::Ordering;
    if !QUESTION_SWEEP_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        let home = home.clone();
        let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
        tokio::task::spawn_blocking(move || {
            let _gate = Gate(&QUESTION_SWEEP_IN_FLIGHT);
            question_sweep(&home, &emitter, now);
        });
    }
}

/// Sweep the question journals against the graph on a fixed cadence.
pub fn question_sweep(home: &AgentsHome, emitter: &crate::events::EventEmitter, now: i64) -> usize {
    question_sweep_in(home, emitter, now, &read_closed_rung_facts)
}

/// The real reading: graph statuses plus the question journal family.
fn read_closed_rung_facts(cwd: &Path, home: &AgentsHome) -> (Vec<(String, String)>, String) {
    let graph = crate::king_board::graph_json_path(cwd);
    let store = crate::backlog::api::Store::new(&graph);
    (statuses_of(&store), journals_raw(&fno_dir_of(home), cwd))
}

fn statuses_of(store: &crate::backlog::api::Store) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(rows) = crate::backlog::api::rows(store, true) {
        for row in rows {
            if let (Some(id), Some(status)) = (
                row.get("id").and_then(serde_json::Value::as_str),
                row.get("status").and_then(serde_json::Value::as_str),
            ) {
                out.push((id.to_string(), status.to_string()));
            }
        }
    }
    out
}

fn fno_dir_of(home: &AgentsHome) -> PathBuf {
    home.root()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".fno"))
}

fn journals_raw(fno_dir: &Path, cwd: &Path) -> String {
    let mut raw = String::new();
    for path in crate::needs::question_journals(fno_dir, cwd) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            raw.push_str(&content);
            raw.push('\n');
        }
    }
    raw
}
/// [`question_sweep`] with the reading injected, so tests drive the stamp,
/// the fold, and the writes against fixtures instead of the live graph.
pub fn question_sweep_in(
    home: &AgentsHome,
    emitter: &crate::events::EventEmitter,
    now: i64,
    read: &dyn Fn(&Path, &AgentsHome) -> (Vec<(String, String)>, String),
) -> usize {
    let stamp = home.root().join("question-sweep.stamp");
    let last = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now.saturating_sub(last) < QUESTION_SWEEP_INTERVAL_SECS {
        return 0;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (statuses, raw) = read(&cwd, home);
    let statuses: std::collections::BTreeMap<String, String> = statuses.into_iter().collect();
    let ids = crate::needs::node_closed_question_ids(&raw, &statuses);
    if ids.is_empty() {
        let _ = emitter.emit("question_sweep", &json!({"closed": 0, "outcome": "none"}));
    } else {
        for qid in &ids {
            crate::provider_cap::append_questions_row(
                &crate::provider_cap::questions_path(home),
                &json!({
                    "ts": crate::provider_cap::epoch_to_rfc3339(now),
                    "type": "operator_question_closed",
                    "source": "daemon",
                    "data": {
                        "question_id": qid,
                        // Empty answer, deliberately: a non-empty answer arms
                        // the unrecorded-decision gate against the asking
                        // session for a decision nobody made.
                        "answer": "",
                        "reason": "node-closed",
                        "closed_by": "question-sweep",
                    },
                }),
            );
        }
        let _ = emitter.emit(
            "question_sweep",
            &json!({"closed": ids.len(), "outcome": "closed", "ids": ids}),
        );
    }
    let _ = std::fs::write(&stamp, now.to_string());
    if ids.is_empty() {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tmp_home(tag: &str) -> AgentsHome {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-agents-qswp-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(&p);
        home.ensure_root().unwrap();
        home
    }

    fn fixture_read(
        statuses: Vec<(String, String)>,
        raw: String,
    ) -> impl Fn(&Path, &AgentsHome) -> (Vec<(String, String)>, String) {
        move |_, _| (statuses.clone(), raw.clone())
    }

    fn open_row(qid: &str, node: &str) -> String {
        format!(
            r#"{{"ts":"2026-09-17T10:00:00Z","type":"operator_question","source":"target","data":{{"question_id":"{qid}","question":"which way?","node":"{node}","blocks":["{node}"]}}}}"#
        )
    }

    #[test]
    fn closes_a_done_node_s_open_question() {
        let home = tmp_home("closes");
        let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
        let read = fixture_read(
            vec![("x-done".to_string(), "done".to_string())],
            open_row("q-done", "x-done"),
        );
        let now = 1_000_000;
        assert_eq!(question_sweep_in(&home, &emitter, now, &read), 1);
        let questions =
            std::fs::read_to_string(crate::provider_cap::questions_path(&home)).unwrap_or_default();
        assert!(questions.contains("q-done"));
        assert!(questions.contains("node-closed"));
        assert_eq!(question_sweep_in(&home, &emitter, now + 60, &read), 0);
    }

    #[test]
    fn leaves_an_in_progress_node_s_question_open() {
        let home = tmp_home("inprogress");
        let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
        let read = fixture_read(
            vec![("x-live".to_string(), "in_progress".to_string())],
            open_row("q-live", "x-live"),
        );
        assert_eq!(question_sweep_in(&home, &emitter, 1_000_000, &read), 0);
        let questions =
            std::fs::read_to_string(crate::provider_cap::questions_path(&home)).unwrap_or_default();
        assert!(!questions.contains("q-live"));
    }
}
