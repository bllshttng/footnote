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
    if let Ok(rows) = crate::backlog::api::rows(store) {
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
        let content = crate::event_store::journal_text(
            &path,
            &[
                "operator_question",
                "operator_question_closed",
                "question_sweep",
            ],
        );
        raw.push_str(&content);
        raw.push('\n');
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
    let task_ids = crate::fleet_task::node_closed_task_ids(&raw, &statuses);
    let legacy_ids = crate::fleet_task::legacy_question_ids(&raw);
    if ids.is_empty() && task_ids.is_empty() && legacy_ids.is_empty() {
        let _ = emitter.emit(
            "question_sweep",
            &json!({"closed": 0, "tasks_closed": 0, "legacy_moved": 0, "outcome": "none"}),
        );
    } else {
        let store = crate::provider_cap::questions_path(home);
        for qid in &ids {
            crate::provider_cap::append_questions_row(
                &store,
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
        for tid in &task_ids {
            crate::provider_cap::append_questions_row(
                &store,
                &json!({
                    "ts": crate::provider_cap::epoch_to_rfc3339(now),
                    "type": "fleet_task_closed",
                    "source": "daemon",
                    "data": {
                        "task_id": tid,
                        "reason": "node-closed",
                        "closed_by": "question-sweep",
                    },
                }),
            );
        }
        for qid in &legacy_ids {
            crate::provider_cap::append_questions_row(
                &store,
                &json!({
                    "ts": crate::provider_cap::epoch_to_rfc3339(now),
                    "type": "operator_question_closed",
                    "source": "daemon",
                    "data": {
                        "question_id": qid,
                        // Empty answer for the same reason as node-closed
                        // above: nobody made this decision.
                        "answer": "",
                        "reason": "moved-to-fleet-task",
                        "closed_by": "question-sweep",
                    },
                }),
            );
        }
        let _ = emitter.emit(
            "question_sweep",
            &json!({
                "closed": ids.len(),
                "tasks_closed": task_ids.len(),
                "legacy_moved": legacy_ids.len(),
                "outcome": "closed",
                "ids": ids,
                "task_ids": task_ids,
                "legacy_ids": legacy_ids,
            }),
        );
    }
    let _ = std::fs::write(&stamp, now.to_string());
    if ids.is_empty() && task_ids.is_empty() && legacy_ids.is_empty() {
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
        // The question store resolves one level ABOVE the home root, so the
        // home gets an "agents" child: the store then lands beside it, in
        // this test's own unique directory instead of the shared temp root.
        let home = AgentsHome::at(p.join("agents"));
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

    #[test]
    fn journals_raw_reads_a_store_committed_question() {
        // AC11-SWEEP: the sweep's raw read reaches the store now.
        let home = tmp_home("store");
        let cwd = home.root().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let space = crate::paths::space_dir(&cwd).join("events.jsonl");
        let row = serde_json::json!({
            "ts": "2026-09-17T10:00:00Z", "type": "operator_question", "source": "target",
            "data": {"question_id": "q-store", "question": "which way?", "blocks": ["x-s"]}
        });
        crate::event_store::append_envelope(&space, &row.to_string(), None).unwrap();
        let raw = journals_raw(&fno_dir_of(&home), &cwd);
        assert!(raw.contains("q-store"), "{raw}");
    }

    #[test]
    fn closes_a_node_closed_task_and_carries_the_count() {
        // AC11-HP: a fleet task whose node reads done closes with reason
        // node-closed, and the event carries tasks_closed.
        let home = tmp_home("task-close");
        let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
        let raw = format!(
            "{}\n{}\n",
            open_row("q-live", "x-live"),
            r#"{"ts":"2026-09-22T10:00:00Z","type":"fleet_task","source":"daemon","data":{"task_id":"ft-done","lane":"heal","key":"PR 7 rebase conflict","cwd":"/r","node":"x-done"}}"#
        );
        let read = fixture_read(
            vec![
                ("x-live".to_string(), "in_progress".to_string()),
                ("x-done".to_string(), "done".to_string()),
            ],
            raw,
        );
        assert_eq!(question_sweep_in(&home, &emitter, 1_000_000, &read), 1);
        let questions =
            std::fs::read_to_string(crate::provider_cap::questions_path(&home)).unwrap_or_default();
        assert!(
            questions.contains(r#""type":"fleet_task_closed""#)
                && questions.contains("ft-done")
                && questions.contains("node-closed"),
            "{questions}"
        );
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(events.contains(r#""tasks_closed":1"#), "{events}");
    }

    #[test]
    fn retires_legacy_machine_questions_and_leaves_agent_asks_open() {
        // AC12-HP + AC13-EDGE + AC14-EDGE: each legacy-marker row closes
        // with an empty answer, reason moved-to-fleet-task; an agent ask
        // quoting a marker mid-text stays open, as do the markers this plan
        // did not move.
        let home = tmp_home("legacy-retire");
        let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
        let raw = [
            r#"{"ts":"t","type":"operator_question","source":"daemon","data":{"question_id":"q-heal","question":"heal: PR 9 rebase conflict. Resolve with x"}}"#,
            r#"{"ts":"t","type":"operator_question","source":"daemon","data":{"question_id":"q-hold","question":"[reap-hold: x] held"}}"#,
            r#"{"ts":"t","type":"operator_question","source":"daemon","data":{"question_id":"q-agent","question":"PR 9 failed and the log quotes heal: PR 9 mid-sentence"}}"#,
            r#"{"ts":"t","type":"operator_question","source":"daemon","data":{"question_id":"q-branch","question":"[session-transition-branch: keep or clean?]"}}"#,
        ]
        .join("\n");
        let read = fixture_read(vec![], raw);
        assert_eq!(question_sweep_in(&home, &emitter, 1_000_000, &read), 1);
        let questions =
            std::fs::read_to_string(crate::provider_cap::questions_path(&home)).unwrap_or_default();
        // The store beside the temp home is shared by sibling tests, so the
        // count is scoped per seeded id, never global.
        let moved = |qid: &str| {
            questions
                .lines()
                .filter(|l| l.contains(qid) && l.contains("moved-to-fleet-task"))
                .count()
        };
        assert_eq!(moved("q-heal"), 1, "q-heal retired: {questions}");
        assert_eq!(moved("q-hold"), 1, "q-hold retired: {questions}");
        assert_eq!(
            moved("q-agent"),
            0,
            "the agent ask quoting a marker mid-text stays open: {questions}"
        );
        assert_eq!(
            moved("q-branch"),
            0,
            "the markers this plan did not move stay open: {questions}"
        );
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(events.contains(r#""legacy_moved":2"#), "{events}");
    }
}
