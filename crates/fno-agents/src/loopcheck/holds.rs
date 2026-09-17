//! The question-family folds for the stop gate: which open questions hold
//! this node or this session, and which decided questions lack a decision
//! record.
//!
//! A node HELD on an open question must stop once, name the question, and
//! never re-ask on a later fire: the first fire emits a `loop_check` block
//! row with `gate: held_on_question`, and the second fire (the same question
//! still open) terminates `HeldOnQuestion` with a digest naming the decide
//! verb. The held state is the journal itself, never the fingerprint.

use serde_json::Value;
use std::path::PathBuf;

/// A question THIS session asked, that was closed WITH an answer, and for
/// which no `operator_decision` event exists on any reachable journal. The
/// stop gate holds the session until the decision is recorded, because a
/// ruling that dies with the transcript is the failure the decision record
/// exists to prevent.
pub(crate) struct UnrecordedDecision {
    pub(crate) question_id: String,
    pub(crate) question: String,
}

/// Fold the question/decision family across a UNION of journals.
///
/// A question can be asked and closed in one journal while the decision lands
/// in another (the operator verbs write to the canonical root's journal; a
/// worktree stop gate reads its own cwd's), so membership is only decidable
/// after every journal is folded - checking per-file would hold a session
/// whose record sits one path away.
///
/// An unreadable or absent journal contributes nothing (fail open): this gate
/// scans for an OBLIGATION contracted elsewhere, and a missing journal means
/// no obligation is visible, not that one was breached. The substring
/// prefilter mirrors the Python reader: the journals are shared, append-only,
/// and never rotated, so parsing every line costs more than the scan.
pub(crate) fn scan_unrecorded_decisions(
    journals: &[PathBuf],
    session_id: &str,
) -> Vec<UnrecordedDecision> {
    let mut asked: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut closed_with_answer: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut recorded: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in journals {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines() {
            if !(line.contains("operator_question") || line.contains("operator_decision")) {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let kind = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let data = val
                .get("data")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            match kind {
                "operator_question" => {
                    if data.get("session_id").and_then(|v| v.as_str()) == Some(session_id) {
                        if let Some(qid) = data.get("question_id").and_then(|v| v.as_str()) {
                            asked.insert(
                                qid.to_string(),
                                data.get("question")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .chars()
                                    .take(80)
                                    .collect(),
                            );
                        }
                    }
                }
                "operator_question_closed" => {
                    let answered = data
                        .get("answer")
                        .and_then(|v| v.as_str())
                        .map(|a| !a.trim().is_empty())
                        .unwrap_or(false);
                    if answered {
                        if let Some(qid) = data.get("question_id").and_then(|v| v.as_str()) {
                            closed_with_answer.insert(qid.to_string());
                        }
                    }
                }
                "operator_decision" => {
                    if let Some(qid) = data.get("question_id").and_then(|v| v.as_str()) {
                        recorded.insert(qid.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    let mut out: Vec<UnrecordedDecision> = asked
        .into_iter()
        .filter(|(qid, _)| closed_with_answer.contains(qid) && !recorded.contains(qid))
        .map(|(question_id, question)| UnrecordedDecision {
            question_id,
            question,
        })
        .collect();
    out.sort_by(|a, b| a.question_id.cmp(&b.question_id));
    out
}

/// One open question holding this node or this session.
pub(crate) struct OpenHold {
    pub(crate) question_id: String,
    /// First line of the question, capped at 80 chars.
    pub(crate) question: String,
    pub(crate) options: Vec<String>,
    /// A prior fire of THIS session already emitted its one held_on_question
    /// block for this question: the next fire terminates instead of blocking.
    pub(crate) already_blocked: bool,
}

/// The open questions that hold this run: a question holds when it names
/// this node in `blocks`, or when this session asked it with a non-empty
/// `blocks`. Everything else a question row may carry is out of scope here.
pub(crate) fn scan_open_holds(
    journals: &[PathBuf],
    session_id: &str,
    node_id: &str,
) -> Vec<OpenHold> {
    let mut asked: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let mut closed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut blocked_once: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in journals {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines() {
            if !(line.contains("operator_question") || line.contains("held_on_question")) {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let kind = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let data = val
                .get("data")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            match kind {
                "operator_question" => {
                    if let Some(qid) = data.get("question_id").and_then(|v| v.as_str()) {
                        asked.insert(qid.to_string(), data);
                    }
                }
                "operator_question_closed" => {
                    if let Some(qid) = data.get("question_id").and_then(|v| v.as_str()) {
                        closed.insert(qid.to_string());
                    }
                }
                // The first fire's own block row: gate held_on_question,
                // session-scoped, naming the questions it blocked on.
                "loop_check" => {
                    if data.get("gate").and_then(|v| v.as_str()) == Some("held_on_question")
                        && data.get("session_id").and_then(|v| v.as_str()) == Some(session_id)
                    {
                        if let Some(held) = data.get("held").and_then(|v| v.as_array()) {
                            for qid in held {
                                if let Some(qid) = qid.as_str() {
                                    blocked_once.insert(qid.to_string());
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut holds: Vec<OpenHold> = asked
        .into_iter()
        .filter(|(qid, data)| {
            if closed.contains(qid) {
                return false;
            }
            let blocks: Vec<String> = data
                .get("blocks")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let asked_by_session =
                data.get("session_id").and_then(|v| v.as_str()) == Some(session_id);
            node_in(node_id, &blocks) || (asked_by_session && !blocks.is_empty())
        })
        .map(|(qid, data)| {
            let question = data
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            let options = data
                .get("options")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            OpenHold {
                question_id: qid.clone(),
                question,
                options,
                already_blocked: blocked_once.contains(&qid),
            }
        })
        .collect();
    holds.sort_by(|a, b| a.question_id.cmp(&b.question_id));
    holds
}

fn node_in(node_id: &str, blocks: &[String]) -> bool {
    blocks.iter().any(|b| b == node_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qrow(qid: &str, session: &str, blocks: &[&str]) -> String {
        let blocks = blocks
            .iter()
            .map(|b| format!("\"{b}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"ts":"2026-09-17T10:00:00Z","type":"operator_question","source":"target","data":{{"question_id":"{qid}","question":"merge now or hold?","session_id":"{session}","blocks":[{blocks}]}}}}"#
        )
    }

    fn crow(qid: &str) -> String {
        format!(
            r#"{{"ts":"2026-09-17T11:00:00Z","type":"operator_question_closed","source":"target","data":{{"question_id":"{qid}","answer":"decided"}}}}"#
        )
    }

    fn brow(qid: &str, session: &str) -> String {
        format!(
            r#"{{"ts":"2026-09-17T11:30:00Z","type":"loop_check","source":"target","data":{{"session_id":"{session}","decision":"block","gate":"held_on_question","held":["{qid}"]}}}}"#
        )
    }

    #[test]
    fn open_blocking_question_holds_the_node_once() {
        let raw = [qrow("q-1", "sess-a", &["x-n"]), brow("q-1", "sess-a")].join("\n");
        let path = std::env::temp_dir().join("fno-holds-test-1.jsonl");
        std::fs::write(&path, &raw).unwrap();
        let journals = vec![path.clone()];
        let holds = scan_open_holds(&journals, "sess-a", "x-n");
        assert_eq!(holds.len(), 1);
        assert!(holds[0].already_blocked, "the prior block row is seen");
        assert_eq!(holds[0].question_id, "q-1");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn second_fire_without_a_prior_block_is_a_first_fire() {
        let raw = qrow("q-2", "sess-b", &["x-n"]);
        let path = std::env::temp_dir().join("fno-holds-test-2.jsonl");
        std::fs::write(&path, &raw).unwrap();
        let journals = vec![path.clone()];
        let holds = scan_open_holds(&journals, "sess-b", "x-n");
        assert_eq!(holds.len(), 1);
        assert!(!holds[0].already_blocked);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn closed_question_and_foreign_blocks_hold_nothing() {
        let raw = [
            qrow("q-3", "sess-c", &["x-n"]),
            crow("q-3"),
            qrow("q-4", "sess-c", &["x-n"]),
        ]
        .join("\n");
        let path = std::env::temp_dir().join("fno-holds-test-3.jsonl");
        std::fs::write(&path, &raw).unwrap();
        let journals = vec![path.clone()];
        let mine = scan_open_holds(&journals, "sess-c", "x-n");
        assert_eq!(mine.len(), 1, "only the open one holds");
        assert_eq!(mine[0].question_id, "q-4");
        // A node the blocks never names, and a session that did not ask:
        let other = scan_open_holds(&journals, "sess-z", "x-other");
        assert!(other.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
