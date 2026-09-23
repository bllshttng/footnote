#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::path::Path;
    use tempfile::TempDir;

    fn request(tmp: &TempDir, qid: &str, answer: Option<&str>) -> ClearRequest {
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        ClearRequest {
            ids: vec![qid.to_string()],
            answer: answer.map(str::to_string),
            cap: 2000,
            provenance: json!({"decided_by": "test-agent", "authority_source": "operator"}),
            closed_by: Some("test-agent".to_string()),
            journal_path: tmp.path().join("project/events.jsonl"),
            index_path: tmp.path().join("fno/questions.jsonl"),
            decisions_path: tmp.path().join("fno/decisions.jsonl"),
            graph: tmp.path().join("graph.json"),
            repo_root,
        }
    }

    fn ask(qid: &str, question: &str, subject: Option<&str>, node: Option<&str>) -> Value {
        let mut data = json!({"question_id": qid, "question": question, "asker": "test-agent"});
        if let Some(subject) = subject {
            data["subject"] = json!(subject);
        }
        if let Some(node) = node {
            data["node"] = json!(node);
        }
        json!({
            "ts": "2026-09-23T00:00:00Z",
            "type": "operator_question",
            "source": "agent",
            "data": data,
        })
    }

    fn decision(qid: &str, decision_id: &str, answer: &str) -> Value {
        json!({
            "ts": "2026-09-23T00:01:00Z",
            "type": "operator_decision",
            "source": "target",
            "data": {
                "decision_id": decision_id,
                "decision": answer,
                "subject": format!("question:{qid}"),
                "question_id": qid,
                "question": "which lane?",
                "asked_by": "test-agent",
                "asked_at": "2026-09-23T00:00:00Z",
                "decided_by": "test-agent",
                "authority_source": "operator",
            },
        })
    }

    fn seed_question(req: &ClearRequest, event: &Value) {
        crate::event_store::append_envelope(&req.index_path, &event.to_string(), None).unwrap();
    }

    fn rows(path: &Path, kinds: &[&str]) -> Vec<Value> {
        crate::event_store::journal_text(path, kinds)
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn graph_decisions(req: &ClearRequest) -> Vec<Value> {
        crate::backlog::api::decisions(&crate::backlog::api::Store::new(&req.graph), None, None)
            .unwrap()
    }

    #[test]
    fn answered_clear_records_one_decision_and_receipts_before_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let req = request(&tmp, "q-open", Some("ship it"));
        seed_question(&req, &ask("q-open", "which lane?", None, None));

        let result = run_clear(&req);

        assert_eq!(result.exit_code, 0, "{:?}", result.lines);
        assert!(result.lines[0].starts_with("outstanding: closed q-open (decision d-"));
        assert!(result.lines[0].ends_with(" recorded)"));
        assert!(result
            .lines
            .last()
            .unwrap()
            .starts_with("outstanding: closed 1"));
        assert_eq!(result.closed.len(), 1);
        let id = result.closed[0].decision_id.as_deref().unwrap();
        let project = rows(
            &req.journal_path,
            &["operator_decision", "operator_question_closed"],
        );
        let index = rows(&req.index_path, &["operator_question_closed"]);
        assert_eq!(
            project
                .iter()
                .filter(|r| r["type"] == "operator_decision")
                .count(),
            1
        );
        assert_eq!(
            project
                .iter()
                .filter(|r| r["type"] == "operator_question_closed")
                .count(),
            1
        );
        assert_eq!(index.len(), 1);
        assert_eq!(index[0]["data"]["question_id"], "q-open");
        let decisions = graph_decisions(&req);
        assert_eq!(
            decisions.iter().filter(|r| r["decision_id"] == id).count(),
            1
        );
    }

    #[test]
    fn interrupted_clear_resumes_the_graph_decision_without_a_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let req = request(&tmp, "q-resume", Some("ship it"));
        seed_question(&req, &ask("q-resume", "which lane?", None, None));
        let stored = decision("q-resume", "d-resume1", "ship it");
        let old_line = stored.to_string();
        crate::backlog::api::decision_record(&crate::backlog::api::Store::new(&req.graph), stored)
            .unwrap();
        crate::event_store::append_envelope(&req.journal_path, &old_line, None).unwrap();
        crate::event_store::append_envelope(&req.decisions_path, &old_line, None).unwrap();
        let old_close = json!({
            "ts": "2026-09-23T00:02:00Z",
            "type": "operator_question_closed",
            "source": "target",
            "data": {
                "question_id": "q-resume",
                "answer": "ship it",
                "closed_by": "test-agent",
            },
        });
        crate::event_store::append_envelope(&req.journal_path, &old_close.to_string(), None)
            .unwrap();

        let result = run_clear(&req);

        assert_eq!(result.exit_code, 0, "{:?}", result.lines);
        assert!(result.lines[0].contains("(decision d-resume1 resumed)"));
        assert_eq!(
            graph_decisions(&req)
                .iter()
                .filter(|r| r["decision_id"] == "d-resume1")
                .count(),
            1
        );
        assert_eq!(rows(&req.journal_path, &["operator_decision"]).len(), 1);
        assert_eq!(rows(&req.decisions_path, &["operator_decision"]).len(), 1);
        assert_eq!(
            rows(&req.journal_path, &["operator_question_closed"]).len(),
            1
        );
    }

    #[test]
    fn a_different_answer_refuses_without_writing_or_closing() {
        let tmp = tempfile::tempdir().unwrap();
        let req = request(&tmp, "q-different", Some("no"));
        seed_question(&req, &ask("q-different", "which lane?", None, None));
        crate::backlog::api::decision_record(
            &crate::backlog::api::Store::new(&req.graph),
            decision("q-different", "d-existing1", "yes"),
        )
        .unwrap();

        let result = run_clear(&req);

        assert_eq!(result.exit_code, 3);
        assert!(
            result.lines[0].contains("already has decision d-existing1 with a different answer")
        );
        assert!(result.lines[0].contains("close it with no --answer"));
        assert!(rows(
            &req.journal_path,
            &["operator_decision", "operator_question_closed"]
        )
        .is_empty());
        assert!(rows(&req.index_path, &["operator_question_closed"]).is_empty());
    }

    #[test]
    fn mixed_open_closed_and_unknown_ids_get_labeled_receipts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut req = request(&tmp, "q-open", None);
        req.ids = vec!["q-open".into(), "q-closed".into(), "q-missing".into()];
        seed_question(&req, &ask("q-open", "which lane?", None, None));
        seed_question(&req, &ask("q-closed", "already answered?", None, None));
        let close = json!({
            "ts": "2026-09-23T00:02:00Z",
            "type": "operator_question_closed",
            "source": "agent",
            "data": {"question_id": "q-closed", "closed_by": "test-agent"},
        });
        crate::provider_cap::append_questions_row(&req.index_path, &close).unwrap();

        let result = run_clear(&req);

        assert_eq!(result.exit_code, 4);
        assert!(result.lines[0].contains("q-open (no answer, withdrawn)"));
        assert!(result.lines[1].contains("q-closed was already closed; nothing written"));
        assert!(result.lines[2].contains("q-missing is not a question id this machine knows"));
        assert_eq!(
            result.lines[3],
            "outstanding: closed 1 (resumed 0), already closed 1, unknown 1, refused 0"
        );
    }

    #[test]
    fn a_failed_index_close_resumes_the_same_decision_and_project_close() {
        let tmp = tempfile::tempdir().unwrap();
        let req = request(&tmp, "q-index-fail", Some("ship it"));
        std::fs::create_dir_all(req.index_path.parent().unwrap()).unwrap();
        std::fs::write(
            &req.index_path,
            format!("{}\n", ask("q-index-fail", "which lane?", None, None)),
        )
        .unwrap();
        let blocked_store = crate::event_store::store_path(&req.index_path);
        std::fs::create_dir_all(&blocked_store).unwrap();

        let failed = run_clear(&req);

        assert_eq!(failed.exit_code, 1);
        assert!(failed.lines[0].contains("decision d-"));
        assert!(failed.lines[0].contains("index close did not land"));
        assert_eq!(
            graph_decisions(&req)
                .iter()
                .filter(|r| r["question_id"] == "q-index-fail")
                .count(),
            1
        );
        assert_eq!(
            rows(&req.journal_path, &["operator_question_closed"]).len(),
            1
        );
        std::fs::remove_dir_all(blocked_store).unwrap();

        let resumed = run_clear(&req);

        assert_eq!(resumed.exit_code, 0, "{:?}", resumed.lines);
        assert!(resumed.lines[0].contains("resumed)"));
        assert_eq!(
            graph_decisions(&req)
                .iter()
                .filter(|r| r["question_id"] == "q-index-fail")
                .count(),
            1
        );
        assert_eq!(rows(&req.journal_path, &["operator_decision"]).len(), 1);
        assert_eq!(
            rows(&req.journal_path, &["operator_question_closed"]).len(),
            1
        );
        assert_eq!(
            rows(&req.index_path, &["operator_question_closed"]).len(),
            1
        );
    }

    #[test]
    fn a_closed_question_can_be_asked_again_on_the_same_subject_and_node() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::paths::AgentsHome::at(tmp.path().join("fno/agents"));
        home.ensure_root().unwrap();
        let mut req = request(&tmp, "q-reask", Some("ship it"));
        req.index_path = crate::provider_cap::questions_path(&home);
        seed_question(
            &req,
            &ask(
                "q-reask",
                "which lane?",
                Some("attention lane"),
                Some("x-0000"),
            ),
        );

        let cleared = run_clear(&req);
        assert_eq!(cleared.exit_code, 0, "{:?}", cleared.lines);
        assert!(
            crate::event_store::journal_text(&req.index_path, &["operator_question_closed"])
                .contains("q-reask")
        );

        let intake: crate::question_intake::IntakeRequest = serde_json::from_value(json!({
            "question": "which lane again?",
            "subject": "attention lane",
            "node": "x-0000",
            "storage_root": tmp.path().join("storage"),
            "index_path": req.index_path,
            "journal_path": req.journal_path,
        }))
        .unwrap();
        let asked = crate::question_intake::run_intake(&intake, &home);
        assert_eq!(asked.exit_code, 0, "{:?}", asked.lines);
        assert_ne!(asked.qid.as_deref(), Some("q-reask"));
        let (questions, closed) = crate::question_intake::question_rows(&req.index_path);
        assert!(closed.contains("q-reask"));
        assert!(questions.contains_key("q-reask"));
    }
}
use crate::backlog::api::{self, Store};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
pub struct ClearRequest {
    pub ids: Vec<String>,
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default = "default_cap")]
    pub cap: usize,
    #[serde(default)]
    pub provenance: Value,
    #[serde(default)]
    pub closed_by: Option<String>,
    pub journal_path: PathBuf,
    pub index_path: PathBuf,
    pub decisions_path: PathBuf,
    pub graph: PathBuf,
    pub repo_root: PathBuf,
}

fn default_cap() -> usize {
    crate::question_intake::QUESTION_CAP
}

#[derive(Serialize)]
pub struct ClearAnswer {
    pub exit_code: i32,
    pub lines: Vec<String>,
    pub closed: Vec<ClosedQuestion>,
    pub deliveries: Vec<Delivery>,
}

#[derive(Serialize)]
pub struct ClosedQuestion {
    pub qid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_event: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Serialize)]
pub struct Delivery {
    pub qid: String,
    pub question: String,
    pub asker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub decision_id: String,
}

impl ClearAnswer {
    fn new() -> Self {
        Self {
            exit_code: 0,
            lines: Vec::new(),
            closed: Vec::new(),
            deliveries: Vec::new(),
        }
    }
}

/// The binary transport: request on stdin, answer on stdout. The verdict
/// lives in `exit_code`, so a computed refusal still exits successfully.
pub fn run_question_clear() -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("fno-agents question-clear: could not read stdin");
        return 2;
    }
    let req: ClearRequest = match serde_json::from_str(&input) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("fno-agents question-clear: bad request: {error}");
            return 2;
        }
    };
    let answer = run_clear(&req);
    match serde_json::to_string(&answer) {
        Ok(line) => println!("{line}"),
        Err(error) => {
            eprintln!("fno-agents question-clear: could not serialize answer: {error}");
            return 1;
        }
    }
    0
}

pub fn run_clear(req: &ClearRequest) -> ClearAnswer {
    let (asked, mut closed_ids) = crate::question_intake::question_rows(&req.index_path);
    let mut answer = ClearAnswer::new();
    let mut newly_closed = 0usize;
    let mut resumed = 0usize;
    let mut already_closed = 0usize;
    let mut unknown = 0usize;
    let mut refused = 0usize;

    for qid in &req.ids {
        if closed_ids.contains(qid) {
            already_closed += 1;
            answer.lines.push(format!(
                "outstanding: {qid} was already closed; nothing written"
            ));
            continue;
        }
        let Some(question_event) = asked.get(qid) else {
            unknown += 1;
            answer.lines.push(format!(
                "outstanding: {qid} is not a question id this machine knows"
            ));
            continue;
        };
        if let Some(text) = req.answer.as_deref() {
            let text = cut(text, req.cap);
            let current = match live_decision(req, qid) {
                Ok(current) => current,
                Err(error) => {
                    answer.exit_code = 1;
                    answer.lines.push(format!(
                        "outstanding: nothing was written for {qid} (could not read decisions: {error})"
                    ));
                    break;
                }
            };
            let (event, line, decision_id, was_resumed) = match current {
                Some((row, event, line)) => {
                    let id = row
                        .get("decision_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if row.get("decision").and_then(Value::as_str) != Some(text.as_str()) {
                        refused += 1;
                        answer.lines.push(format!(
                            "outstanding: {qid} already has decision {id} with a different answer; close it with no --answer, or retract {id} first"
                        ));
                        continue;
                    }
                    (event, line, id, true)
                }
                None => {
                    let authority = req
                        .provenance
                        .get("authority_source")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if authority != "operator" {
                        let mut runner =
                            |cmd: &str, root: &Path| crate::evidence::shell_run(cmd, root, 20);
                        if let Err(error) = crate::evidence::check_ruling_evidence(
                            &text,
                            &[],
                            &req.repo_root,
                            &mut runner,
                        ) {
                            refused += 1;
                            answer.lines.push(format!(
                                "outstanding: refused: {}. Nothing was closed; this question stays open.",
                                error.message
                            ));
                            continue;
                        }
                    }
                    let event = match make_decision(req, qid, question_event, &text) {
                        Ok(event) => event,
                        Err(error) => {
                            answer.exit_code = 1;
                            answer.lines.push(format!(
                                "outstanding: nothing was written for {qid} ({error})"
                            ));
                            break;
                        }
                    };
                    let id = event["data"]["decision_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    if let Err(error) = api::decision_record(&Store::new(&req.graph), event.clone())
                    {
                        answer.exit_code = 1;
                        answer.lines.push(format!(
                            "outstanding: nothing was written for {qid} (decision record failed: {error})"
                        ));
                        break;
                    }
                    let line = match serde_json::to_string(&event) {
                        Ok(line) => line,
                        Err(error) => {
                            answer.exit_code = 1;
                            answer.lines.push(format!(
                                "outstanding: decision {id} is recorded; its journal mirror failed ({error}); rerun the same clear to finish"
                            ));
                            break;
                        }
                    };
                    (event, line, id, false)
                }
            };
            if let Err(error) = append_decision_mirror(&req.journal_path, &line, &decision_id) {
                answer.exit_code = 1;
                answer.lines.push(format!(
                    "outstanding: decision {decision_id} is recorded; its project journal mirror failed ({error}); rerun the same clear to finish"
                ));
                break;
            }
            if let Err(error) = append_decision_mirror(&req.decisions_path, &line, &decision_id) {
                answer.exit_code = 1;
                answer.lines.push(format!(
                    "outstanding: decision {decision_id} is recorded; its decision index mirror failed ({error}); rerun the same clear to finish"
                ));
                break;
            }
            let node = question_event
                .get("data")
                .and_then(|data| data.get("node"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let (close, close_line) = close_event(req, qid, Some(&text));
            if let Err(error) = append_close(&req.journal_path, &close_line, qid) {
                answer.exit_code = 1;
                answer.lines.push(format!(
                    "outstanding: decision {decision_id} is recorded; the close did not land ({error}); rerun the same clear to finish"
                ));
                break;
            }
            if let Err(error) = crate::provider_cap::append_questions_row(&req.index_path, &close) {
                answer.exit_code = 1;
                answer.lines.push(format!(
                    "outstanding: decision {decision_id} and project close are recorded; the index close did not land ({error}); rerun the same clear to finish"
                ));
                break;
            }
            let label = if was_resumed { "resumed" } else { "recorded" };
            answer.lines.push(format!(
                "outstanding: closed {qid} (decision {decision_id} {label})"
            ));
            answer.closed.push(ClosedQuestion {
                qid: qid.clone(),
                decision_id: Some(decision_id.clone()),
                decision_event: Some(event),
                node,
            });
            if was_resumed {
                resumed += 1;
            }
            newly_closed += 1;
            closed_ids.insert(qid.clone());
            let asker = question_event
                .get("data")
                .and_then(|data| data.get("asker"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            answer.deliveries.push(Delivery {
                qid: qid.clone(),
                question: question_text(question_event),
                asker: asker.to_string(),
                session_id: question_event
                    .get("data")
                    .and_then(|data| data.get("session_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                decision_id,
            });
        } else {
            let (close, close_line) = close_event(req, qid, None);
            if let Err(error) = append_close(&req.journal_path, &close_line, qid) {
                answer.exit_code = 1;
                answer.lines.push(format!(
                    "outstanding: nothing was written for {qid} (close failed: {error})"
                ));
                break;
            }
            if let Err(error) = crate::provider_cap::append_questions_row(&req.index_path, &close) {
                answer.exit_code = 1;
                answer.lines.push(format!(
                    "outstanding: the project close for {qid} is recorded; the index close did not land ({error}); rerun the same clear to finish"
                ));
                break;
            }
            answer
                .lines
                .push(format!("outstanding: closed {qid} (no answer, withdrawn)"));
            answer.closed.push(ClosedQuestion {
                qid: qid.clone(),
                decision_id: None,
                decision_event: None,
                node: question_event
                    .get("data")
                    .and_then(|data| data.get("node"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
            newly_closed += 1;
            closed_ids.insert(qid.clone());
        }
    }

    answer.lines.push(format!(
        "outstanding: closed {newly_closed} (resumed {resumed}), already closed {already_closed}, unknown {unknown}, refused {refused}"
    ));
    if answer.exit_code == 0 {
        answer.exit_code = if refused > 0 {
            3
        } else if unknown > 0 {
            4
        } else {
            0
        };
    }
    answer
}

fn live_decision(req: &ClearRequest, qid: &str) -> Result<Option<(Value, Value, String)>, String> {
    use rusqlite::OptionalExtension;

    let connection = crate::backlog::open(&req.graph)?;
    let stored: Option<(String, Option<String>, String)> = connection
        .query_row(
            "SELECT d.ts, d.source, d.data
             FROM decisions d
             WHERE d.event_type = 'operator_decision'
               AND json_extract(d.data, '$.question_id') = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM decisions r
                   WHERE r.event_type = 'decision_retracted'
                     AND json_extract(r.data, '$.target_decision_id') =
                         json_extract(d.data, '$.decision_id')
               )
             ORDER BY d.seq DESC LIMIT 1",
            [qid],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some((ts, source, data)) = stored else {
        return Ok(None);
    };
    let data: Value = serde_json::from_str(&data).map_err(|error| error.to_string())?;
    let row = data.clone();
    let id = row
        .get("decision_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some((event, line)) =
        find_decision(&req.journal_path, id).or_else(|| find_decision(&req.decisions_path, id))
    {
        return Ok(Some((row, event, line)));
    }
    let event = json!({
        "ts": ts,
        "type": "operator_decision",
        "source": source.unwrap_or_else(|| "target".to_string()),
        "data": data,
    });
    let line = serde_json::to_string(&event).map_err(|error| error.to_string())?;
    Ok(Some((row, event, line)))
}

fn find_decision(path: &Path, decision_id: &str) -> Option<(Value, String)> {
    crate::event_store::journal_text(path, &["operator_decision"])
        .lines()
        .find_map(|line| {
            let event: Value = serde_json::from_str(line).ok()?;
            (event.get("data")?.get("decision_id")?.as_str()? == decision_id)
                .then(|| (event, line.to_string()))
        })
}

fn append_decision_mirror(path: &Path, line: &str, decision_id: &str) -> Result<(), String> {
    let event_id = stored_event_id(path, line)?.unwrap_or_else(|| decision_id.to_string());
    crate::event_store::append_envelope(path, line, Some(&event_id)).map(|_| ())
}

fn stored_event_id(path: &Path, line: &str) -> Result<Option<String>, String> {
    let store = crate::event_store::store_path(path);
    if !store.exists() {
        return Ok(None);
    }
    let mut connection =
        Connection::open(&store).map_err(|error| format!("{}: {error}", store.display()))?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| format!("{}: {error}", store.display()))?;
    crate::event_store::ensure_schema(&mut connection, &store)?;
    let hash = Sha256::digest(line.trim().as_bytes()).to_vec();
    let stored: Option<(String, String)> = connection
        .query_row(
            "SELECT event_id, line FROM events WHERE row_hash = ?1",
            params![hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| format!("{}: {error}", store.display()))?;
    match stored {
        Some((event_id, existing)) if existing == line.trim() => Ok(Some(event_id)),
        Some(_) => Err(format!(
            "{}: row hash matched a different decision envelope",
            store.display()
        )),
        None => Ok(None),
    }
}

fn make_decision(
    req: &ClearRequest,
    qid: &str,
    question_event: &Value,
    answer: &str,
) -> Result<Value, String> {
    let data = question_event.get("data").unwrap_or(question_event);
    let node = data
        .get("node")
        .and_then(Value::as_str)
        .filter(|node| !node.trim().is_empty());
    let subject = node
        .map(str::to_string)
        .unwrap_or_else(|| format!("question:{qid}"));
    let mut decision = Map::new();
    let mut id_bytes = [0u8; 4];
    getrandom::fill(&mut id_bytes).expect("OS CSPRNG unavailable");
    decision.insert("decision_id".into(), json!(format!("d-{}", hex(&id_bytes))));
    decision.insert("decision".into(), json!(answer));
    decision.insert("subject".into(), json!(subject));
    decision.insert("question_id".into(), json!(qid));
    decision.insert(
        "question".into(),
        json!(cut(&question_text(question_event), req.cap)),
    );
    if let Some(asked_by) = data
        .get("asker")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| data.get("session_id").and_then(Value::as_str))
        .filter(|value| !value.is_empty())
    {
        decision.insert("asked_by".into(), json!(asked_by));
    }
    if let Some(asked_at) = question_event
        .get("ts")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        decision.insert("asked_at".into(), json!(asked_at));
    }
    for key in [
        "decided_by",
        "attested_by",
        "relayed_by",
        "origin",
        "authority_source",
    ] {
        if let Some(value) = req.provenance.get(key).filter(|value| !value.is_null()) {
            decision.insert(key.into(), value.clone());
        }
    }
    let authority = req
        .provenance
        .get("authority_source")
        .and_then(Value::as_str)
        .unwrap_or("");
    if matches!(authority, "crown" | "agent" | "beastmode") {
        if let Some(node) = node {
            let connection = crate::backlog::open(&req.graph)?;
            let exists: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM nodes WHERE id = ?1)",
                    [node],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if exists {
                decision.insert(
                    "expiry_ref".into(),
                    json!({"kind": "node", "node_id": node}),
                );
            }
        }
    }
    Ok(json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "type": "operator_decision",
        "source": "target",
        "data": decision,
    }))
}

fn close_event(req: &ClearRequest, qid: &str, answer: Option<&str>) -> (Value, String) {
    if let Some((event, line)) =
        crate::event_store::journal_text(&req.journal_path, &["operator_question_closed"])
            .lines()
            .find_map(|line| {
                let event: Value = serde_json::from_str(line).ok()?;
                (event["data"]["question_id"].as_str() == Some(qid))
                    .then(|| (event, line.to_string()))
            })
    {
        return (event, line);
    }
    let mut data = json!({"question_id": qid});
    if let Some(answer) = answer {
        data["answer"] = json!(cut(answer, req.cap));
    }
    if let Some(closed_by) = req.closed_by.as_deref() {
        data["closed_by"] = json!(closed_by);
    }
    let event = json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "type": "operator_question_closed",
        "source": "target",
        "data": data,
    });
    let line = serde_json::to_string(&event).expect("close event serializes");
    (event, line)
}

fn append_close(journal: &Path, line: &str, qid: &str) -> Result<(), String> {
    let event_id = stored_event_id(journal, line)?.unwrap_or_else(|| format!("close:{qid}"));
    crate::event_store::append_envelope(journal, line, Some(&event_id)).map(|_| ())
}

fn question_text(question: &Value) -> String {
    question
        .get("data")
        .and_then(|data| data.get("question"))
        .or_else(|| question.get("question"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn cut(text: &str, cap: usize) -> String {
    text.chars().take(cap).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
