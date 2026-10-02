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
            caller: Some(ClearCaller::default()),
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

    fn ask_from(qid: &str, asker: &str, question: &str) -> Value {
        json!({
            "ts": "2026-09-23T00:00:00Z",
            "type": "operator_question",
            "source": "agent",
            "data": {"question_id": qid, "question": question, "asker": asker},
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

        // The same rerun on stores carrying recovery history: journal_text
        // re-renders committed rows with `_store_seq`/`_history_only`, and
        // hashing that annotated text misses the stored row_hash, so the
        // mirrors must still land the raw stored line. The stores diverge:
        // the decisions store's decision row sits at a different seq, so the
        // journal's _store_seq cannot resolve there and the mirror must
        // still append metadata-free bytes.
        let tmp = tempfile::tempdir().unwrap();
        let req = request(&tmp, "q-recovery", Some("ship it"));
        seed_question(&req, &ask("q-recovery", "which lane?", None, None));
        let stored = decision("q-recovery", "d-recovery1", "ship it");
        let raw_line = stored.to_string();
        crate::backlog::api::decision_record(&crate::backlog::api::Store::new(&req.graph), stored)
            .unwrap();
        crate::event_store::append_envelope(&req.journal_path, &raw_line, None).unwrap();
        crate::event_store::append_envelope(
            &req.decisions_path,
            &json!({
                "ts": "2026-09-23T00:00:30Z",
                "type": "status_control",
                "source": "test",
                "data": {}
            })
            .to_string(),
            None,
        )
        .unwrap();
        crate::event_store::append_envelope(&req.decisions_path, &raw_line, None).unwrap();
        let old_close = json!({
            "ts": "2026-09-23T00:02:00Z",
            "type": "operator_question_closed",
            "source": "target",
            "data": {
                "question_id": "q-recovery",
                "answer": "ship it",
                "closed_by": "test-agent",
            },
        });
        crate::event_store::append_envelope(&req.journal_path, &old_close.to_string(), None)
            .unwrap();
        for path in [&req.journal_path, &req.decisions_path] {
            let store = crate::event_store::store_path(path);
            let conn = Connection::open(&store).unwrap();
            conn.execute_batch(
                "CREATE TABLE recovery_history(event_id TEXT PRIMARY KEY, batch TEXT NOT NULL);
                 INSERT INTO recovery_history SELECT event_id, 'copy-batch' FROM events;",
            )
            .unwrap();
        }

        let result = run_clear(&req);

        assert_eq!(result.exit_code, 0, "{:?}", result.lines);
        assert!(result.lines[0].contains("(decision d-recovery1 resumed)"));
        for path in [&req.journal_path, &req.decisions_path] {
            let store = crate::event_store::store_path(path);
            let conn = Connection::open(&store).unwrap();
            let (count, line): (i64, String) = conn
                .query_row(
                    "SELECT COUNT(*), COALESCE(MAX(line), '') FROM events
                     WHERE type = 'operator_decision'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(count, 1, "one decision row in {}", store.display());
            assert_eq!(
                line, raw_line,
                "the mirror stores the raw envelope, not the annotated render"
            );
        }
        assert_eq!(
            rows(&req.journal_path, &["operator_question_closed"]).len(),
            1
        );
    }

    #[test]
    fn a_different_answer_refuses_unless_it_overrides_a_coordination_row() {
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

        // The user's own answer overrides a coordination row an agent wrote:
        // the board answer mints and the live read returns it.
        let tmp = tempfile::tempdir().unwrap();
        let req = request(&tmp, "q-coord", Some("no"));
        seed_question(&req, &ask("q-coord", "which lane?", None, None));
        let mut coord = decision("q-coord", "d-coord1", "yes");
        coord["data"]["authority_source"] = json!("agent");
        coord["data"]["decided_by"] = json!("agent-9");
        crate::backlog::api::decision_record(&crate::backlog::api::Store::new(&req.graph), coord)
            .unwrap();

        let result = run_clear(&req);

        assert_eq!(result.exit_code, 0, "{:?}", result.lines);
        let connection = crate::backlog::open(&req.graph).unwrap();
        let live = crate::backlog::decisions::live_answer(&connection, "q-coord")
            .unwrap()
            .unwrap();
        let data: Value = serde_json::from_str(&live.2).unwrap();
        assert_eq!(data["decision"], "no");
        assert_eq!(data["authority_source"], "operator");
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
        seed_question(&req, &ask("q-index-fail", "which lane?", None, None));
        let store = crate::event_store::store_path(&req.index_path);
        {
            let connection = rusqlite::Connection::open(&store).unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER fail_question_close
                     BEFORE INSERT ON events
                     WHEN NEW.type = 'operator_question_closed'
                     BEGIN SELECT RAISE(FAIL, 'injected index close failure'); END;",
                )
                .unwrap();
        }

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
        let connection = rusqlite::Connection::open(&store).unwrap();
        connection
            .execute_batch("DROP TRIGGER fail_question_close")
            .unwrap();

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
            "ask": "Use the attention lane again",
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

    #[test]
    fn an_answered_merge_grant_question_becomes_the_operator_grant_at_that_head() {
        let sha = "a29b38c37b18e737eaf850e8765920498287cabf";
        let subject = format!("merge-grant:footnote#2739@{sha}");
        let other = "1111111111111111111111111111111111111111";
        let ask_grant = |req: &ClearRequest, qid: &str| {
            seed_question(
                req,
                &ask(qid, "Merge PR 2739?", Some(&subject), Some("x-0000")),
            );
        };
        // The operator answers yes on the board: the gate reads the grant at
        // that head and nothing at any other head.
        let tmp = tempfile::tempdir().unwrap();
        let req = request(&tmp, "q-grant", Some("Merge it now."));
        ask_grant(&req, "q-grant");
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 0, "{:?}", result.lines);
        let decisions = graph_decisions(&req);
        let at = |rows: &[Value], head: &str| {
            let want = format!("merge-grant:footnote#2739@{head}");
            let rows: Vec<Value> = rows
                .iter()
                .filter(|r| r["subject"] == want.as_str())
                .cloned()
                .collect();
            crate::merge_grant::head_grant_status(
                serde_json::to_vec(&json!({ "decisions": rows }))
                    .ok()
                    .as_deref(),
            )
        };
        assert_eq!(at(&decisions, sha), crate::merge_grant::HeadGrant::Granted);
        assert_eq!(at(&decisions, other), crate::merge_grant::HeadGrant::Absent);

        // A hold answer records operator law at the subject that grants
        // nothing: the gate reads a conflict, never a grant.
        let tmp = tempfile::tempdir().unwrap();
        let req = request(&tmp, "q-hold", Some("Hold it."));
        ask_grant(&req, "q-hold");
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 0, "{:?}", result.lines);
        let rows: Vec<Value> = graph_decisions(&req)
            .into_iter()
            .filter(|r| r["subject"] == subject.as_str())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["authority_source"], "operator");
        assert_eq!(rows[0]["decision"], "Hold it.");
        let payload = serde_json::to_vec(&json!({ "decisions": rows })).unwrap();
        assert_eq!(
            crate::merge_grant::head_grant_status(Some(&payload)),
            crate::merge_grant::HeadGrant::Conflict
        );

        // An agent session's clear is refused outright: the gate refuses the
        // answer before any row lands, so no grant row at the subject and no
        // coordination row at the node either.
        let tmp = tempfile::tempdir().unwrap();
        let mut req = request(&tmp, "q-agent", Some("Merge it now."));
        req.provenance = json!({"decided_by": "agent-1", "authority_source": "agent"});
        req.caller.as_mut().unwrap().ancestry_handle = Some("agent-1".to_string());
        ask_grant(&req, "q-agent");
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 3, "{:?}", result.lines);
        let decisions = graph_decisions(&req);
        assert!(
            decisions.iter().all(|r| r["subject"] != subject.as_str()),
            "no row may land at the merge-grant subject from an agent clear"
        );
        assert!(
            decisions.iter().all(|r| r["subject"] != "x-0000"),
            "no row may land at the node from an agent clear"
        );
        let board_sha = "a29b38c37b18e737eaf850e8765920498287cabf";
        let board_subject = format!("merge-grant:footnote#2911@{board_sha}");
        let ask_board = |req: &ClearRequest, qid: &str| {
            seed_question(
                req,
                &json!({
                    "ts": "2026-10-01T20:00:00Z",
                    "type": "operator_question",
                    "source": "agent",
                    "data": {
                        "question_id": qid,
                        "question": "May PR 2911 merge?",
                        "asker": "test-agent",
                        "node": "x-0000",
                        "subject": board_subject,
                        "options": [
                            {"n": 1, "text": "Yes, merge PR 2911."},
                            {"n": 2, "text": "No, keep it held."},
                        ],
                        "context": {"recommendation": {"option": 1, "why": "every gate is met"}},
                    },
                }),
            );
        };
        // The board words lane delivers the echoed option line with the
        // operator's notes attached: the exact answer shape that once read
        // as a conflict at the gate (q-7845e717).
        let tmp = tempfile::tempdir().unwrap();
        let req = request(
            &tmp,
            "q-echo",
            Some("1. Yes, merge PR 2911. - notes: worried"),
        );
        ask_board(&req, "q-echo");
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 0, "{:?}", result.lines);
        assert!(
            result
                .lines
                .iter()
                .any(|l| l.contains("operator merge grant recorded")),
            "{:?}",
            result.lines
        );
        let rows: Vec<Value> = graph_decisions(&req)
            .into_iter()
            .filter(|r| r["subject"] == board_subject.as_str())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["decision"],
            crate::merge_grant::MERGE_GRANT_DECISION
        );
        let payload = serde_json::to_vec(&json!({ "decisions": rows })).unwrap();
        assert_eq!(
            crate::merge_grant::head_grant_status(Some(&payload)),
            crate::merge_grant::HeadGrant::Granted
        );

        // The non-recommended option echoes too, and grants nothing: the raw
        // words land at the subject and the gate reads a conflict.
        let tmp = tempfile::tempdir().unwrap();
        let req = request(&tmp, "q-hold2", Some("2. No, keep it held."));
        ask_board(&req, "q-hold2");
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 0, "{:?}", result.lines);
        let rows: Vec<Value> = graph_decisions(&req)
            .into_iter()
            .filter(|r| r["subject"] == board_subject.as_str())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["decision"], "2. No, keep it held.");
        let payload = serde_json::to_vec(&json!({ "decisions": rows })).unwrap();
        assert_eq!(
            crate::merge_grant::head_grant_status(Some(&payload)),
            crate::merge_grant::HeadGrant::Conflict
        );
    }

    #[test]
    fn only_the_user_or_the_asking_crown_closes_a_question_asked_of_the_user() {
        // (a) An agent answers the user's question: refused, nothing written.
        let tmp = tempfile::tempdir().unwrap();
        let mut req = request(&tmp, "q-gate", Some("C: set 400 here"));
        req.provenance = json!({"decided_by": "01a0cbdd", "authority_source": "agent"});
        seed_question(&req, &ask_from("q-gate", "49a80492", "which lane?"));
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 3, "{:?}", result.lines);
        assert!(
            result.lines[0].contains("question board"),
            "{:?}",
            result.lines
        );
        assert!(result.lines[0].contains("--answer"), "{:?}", result.lines);
        assert!(rows(
            &req.journal_path,
            &["operator_decision", "operator_question_closed"]
        )
        .is_empty());
        assert!(rows(&req.decisions_path, &["operator_decision"]).is_empty());
        assert!(rows(&req.index_path, &["operator_question_closed"]).is_empty());
        assert!(graph_decisions(&req).is_empty());

        // (b) The same agent withdraws (no --answer): refused, still open.
        let tmp = tempfile::tempdir().unwrap();
        let mut req = request(&tmp, "q-gate-w", None);
        req.provenance = json!({"decided_by": "01a0cbdd", "authority_source": "agent"});
        seed_question(&req, &ask_from("q-gate-w", "49a80492", "which lane?"));
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 3, "{:?}", result.lines);
        assert!(
            result.lines[0].contains("was asked by 49a80492"),
            "{:?}",
            result.lines
        );
        assert!(
            result.lines[0].contains("Only the asker withdraws"),
            "{:?}",
            result.lines
        );
        let (asked, closed) = crate::question_intake::question_rows(&req.index_path);
        assert!(asked.contains_key("q-gate-w"));
        assert!(!closed.contains("q-gate-w"));

        // (c) An agent withdraws its own ask: allowed, closed, no decision.
        let tmp = tempfile::tempdir().unwrap();
        let mut req = request(&tmp, "q-own", None);
        req.caller.as_mut().unwrap().ancestry_handle = Some("01a0cbdd".to_string());
        seed_question(&req, &ask_from("q-own", "01a0cbdd", "which lane?"));
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 0, "{:?}", result.lines);
        assert!(result.lines[0].contains("(no answer, withdrawn)"));
        assert!(rows(&req.journal_path, &["operator_decision"]).is_empty());
        assert_eq!(
            rows(&req.journal_path, &["operator_question_closed"]).len(),
            1
        );

        // (d) A crowned session answers its own ask with --authority crown.
        let tmp = tempfile::tempdir().unwrap();
        let mut req = request(&tmp, "q-crown", Some("C: do as asked"));
        req.provenance = json!({"decided_by": "01a0cbdd", "authority_source": "crown"});
        req.caller.as_mut().unwrap().crowned = true;
        seed_question(&req, &ask_from("q-crown", "01a0cbdd", "which lane?"));
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 0, "{:?}", result.lines);
        let decisions = graph_decisions(&req);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0]["authority_source"], "crown");

        // (e) The same answer without a live crown: refused.
        let tmp = tempfile::tempdir().unwrap();
        let mut req = request(&tmp, "q-uncrowned", Some("C: do as asked"));
        req.provenance = json!({"decided_by": "01a0cbdd", "authority_source": "crown"});
        seed_question(&req, &ask_from("q-uncrowned", "01a0cbdd", "which lane?"));
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 3, "{:?}", result.lines);
        assert!(
            result.lines[0].contains("question board"),
            "{:?}",
            result.lines
        );

        // (f) A crowned crown answering another session's ask: refused.
        let tmp = tempfile::tempdir().unwrap();
        let mut req = request(&tmp, "q-other", Some("C: do as asked"));
        req.provenance = json!({"decided_by": "01a0cbdd", "authority_source": "crown"});
        req.caller.as_mut().unwrap().crowned = true;
        seed_question(&req, &ask_from("q-other", "49a80492", "which lane?"));
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 3, "{:?}", result.lines);
        assert!(
            result.lines[0].contains("asks the user"),
            "{:?}",
            result.lines
        );

        // (g) The forged transport: empty provenance, but the ancestry prover
        // names the agent, so the gate still refuses.
        let tmp = tempfile::tempdir().unwrap();
        let mut req = request(&tmp, "q-forge", Some("C: do it"));
        req.provenance = json!({});
        req.caller.as_mut().unwrap().ancestry_handle = Some("01a0cbdd".to_string());
        seed_question(&req, &ask_from("q-forge", "49a80492", "which lane?"));
        let result = run_clear(&req);
        assert_eq!(result.exit_code, 3, "{:?}", result.lines);
        assert!(
            result.lines[0].contains("asks the user"),
            "{:?}",
            result.lines
        );

        // (h) holds_crown reads the holder session's canonical handle.
        let crown = crate::territory::Crown {
            scope: "scope".to_string(),
            level: 1,
            holder: "king".to_string(),
            holder_session: Some("01a0cbdd-baed-4482-9135-37ba1cb0f0d2".to_string()),
        };
        assert!(holds_crown(&[crown.clone()], "01a0cbdd"));
        assert!(!holds_crown(&[crown], "49a80492"));
    }
}
use crate::backlog::api::{self, Store};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Who is at the clear door, resolved once per request. The gate reads the
/// handle; the crown check is precomputed so a test can drive it without a
/// registry on disk.
#[derive(Clone, Debug, Default)]
pub struct ClearCaller {
    /// The agent handle the Rust ancestry prover resolved in this process.
    pub ancestry_handle: Option<String>,
    /// Whether the caller's handle holds a live crown.
    pub crowned: bool,
}

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
    /// Caller facts resolved in-process. The binary transport never sets it;
    /// `None` means run_clear resolves from the process itself.
    #[serde(skip)]
    pub caller: Option<ClearCaller>,
    pub journal_path: PathBuf,
    pub index_path: PathBuf,
    pub decisions_path: PathBuf,
    pub graph: PathBuf,
    pub repo_root: PathBuf,
}

/// The agent handle at the door: the provenance resolver's stamp when it
/// names an agent authority, else the process's own ambient resolution. The
/// Python front resolves provenance only when `--answer` is set, so a
/// withdrawal leans on the ancestry prover here.
fn agent_handle(req: &ClearRequest, caller: &ClearCaller) -> Option<String> {
    let authority = req
        .provenance
        .get("authority_source")
        .and_then(Value::as_str)
        .unwrap_or("");
    if matches!(authority, "agent" | "crown" | "beastmode") {
        if let Some(decided_by) = req
            .provenance
            .get("decided_by")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            return Some(decided_by.to_string());
        }
    }
    caller.ancestry_handle.clone()
}

/// The user's own answer outranks a coordination row an agent wrote (agent,
/// crown or beastmode authority): without this, an agent's
/// `decide --question-id` row makes the user's board answer refuse as "a
/// different answer". A live operator or chat_attested row keeps today's
/// refusal, and an agent caller never overrides.
fn override_coordination(
    req: &ClearRequest,
    caller: &ClearCaller,
    row: &Value,
    text: &str,
) -> bool {
    if agent_handle(req, caller).is_some() {
        return false;
    }
    if row.get("decision").and_then(Value::as_str) == Some(text) {
        return false;
    }
    matches!(
        row.get("authority_source").and_then(Value::as_str),
        Some("agent") | Some("crown") | Some("beastmode")
    )
}

/// Production caller resolution: the ambient prover for the handle, the
/// registry for the crown. A registry read error reads as not crowned
/// (fail closed).
fn resolve_clear_caller(req: &ClearRequest) -> ClearCaller {
    let ancestry_handle = crate::identity::ambient_agent_handle();
    let handle = agent_handle(
        req,
        &ClearCaller {
            ancestry_handle: ancestry_handle.clone(),
            crowned: false,
        },
    );
    let crowned = handle
        .as_deref()
        .map(|handle| {
            crate::territory::live_crowns(&crate::paths::AgentsHome::from_env().registry_json())
                .map(|crowns| holds_crown(&crowns, handle))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    ClearCaller {
        ancestry_handle,
        crowned,
    }
}

/// Whether `handle` holds a live crown: the crown-name store binds by session
/// id (law d-e952ed19), so the holder's canonical handle is what compares.
/// Pure, so tests drive it without a registry.
fn holds_crown(crowns: &[crate::territory::Crown], handle: &str) -> bool {
    crowns.iter().any(|crown| {
        crown
            .holder_session
            .as_deref()
            .is_some_and(|session| crate::identity::canonical_handle(session) == handle)
    })
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
    let caller = req
        .caller
        .clone()
        .unwrap_or_else(|| resolve_clear_caller(req));
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
        // The clear door is the user's answer lane: an agent session never
        // answers or withdraws a question asked of the user, and a crown
        // answers (with --authority crown) only a question it asked itself.
        // This gate runs before make_decision, so the only agent session that
        // reaches operator_can_grant below is a crown on its own ask, whose
        // authority crown already fails there.
        let asker = question_event
            .get("data")
            .and_then(|data| data.get("asker"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(agent) = agent_handle(req, &caller) {
            let own_ask = asker == agent;
            let allowed = if req.answer.is_some() {
                own_ask
                    && req
                        .provenance
                        .get("authority_source")
                        .and_then(Value::as_str)
                        == Some("crown")
                    && caller.crowned
            } else {
                own_ask
            };
            if !allowed {
                refused += 1;
                answer.lines.push(if req.answer.is_some() {
                    format!(
                        "outstanding: refused: {qid} asks the user, and this session is agent {agent}. Nothing was closed; the question stays open. The user answers it on the question board (fno-agents state path questions) or at their own terminal: fno inbox outstanding clear {qid} --answer \"<answer>\". A crown answers only a question it asked, with --authority crown."
                    )
                } else {
                    let shown = if asker.is_empty() { "the user" } else { asker };
                    format!(
                        "outstanding: refused: {qid} was asked by {shown}, and this session is agent {agent}. Only the asker withdraws a question. Nothing was closed; the user answers it on the question board (fno-agents state path questions)."
                    )
                });
                continue;
            }
        }
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
                Some((row, event, line)) if !override_coordination(req, &caller, &row, &text) => {
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
                _ => {
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
                            "outstanding: nothing was written for {qid} (decision record failed: {})",
                            error.0
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
            if let Some(subject) = merge_grant_subject(question_event) {
                if operator_can_grant(req) {
                    answer.lines.push(if merge_answer_grants(question_event, &text) {
                        format!(
                            "outstanding: operator merge grant recorded at {subject} (binds this head only)"
                        )
                    } else {
                        format!(
                            "outstanding: law recorded at {subject} grants nothing; the gate grants only \"{}\"",
                            crate::merge_grant::MERGE_GRANT_DECISION
                        )
                    });
                }
            }
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
    let connection = crate::backlog::open(&req.graph)?;
    let Some((ts, source, data)) = crate::backlog::decisions::live_answer(&connection, qid)? else {
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
    // A recovery-annotated read re-mirrors only its raw stored envelope: the
    // mirror row must stay byte-identical to the first append, so identity
    // hashes the stored bytes, never the annotated render.
    let line = crate::event_store::raw_stored_line(path, line)?;
    let event_id = stored_event_id(path, &line)?.unwrap_or_else(|| decision_id.to_string());
    crate::event_store::append_envelope(path, &line, Some(&event_id)).map(|_| ())
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
    // A merge-grant question's answer IS the operator's ruling on that exact
    // head, so the row lands at the head-scoped subject the merge gate reads
    // (`merge-grant:<repo>#<pr>@<sha>`), never at the node: a row at the node
    // subject is invisible to `head_grant_status`, so a board-approved head
    // used to read absent at the gate. Operator law only: a session that
    // resolves an agent authority can no more mint the grant through the
    // clear door than through decide.
    let grant_subject = merge_grant_subject(question_event).filter(|_| operator_can_grant(req));
    let affirmative = grant_subject.is_some() && merge_answer_grants(question_event, answer);
    let decision_text = if affirmative {
        crate::merge_grant::MERGE_GRANT_DECISION.to_string()
    } else {
        answer.to_string()
    };
    let subject = match &grant_subject {
        Some(subject) => subject.clone(),
        None => node
            .map(str::to_string)
            .unwrap_or_else(|| format!("question:{qid}")),
    };
    let mut decision = Map::new();
    let mut id_bytes = [0u8; 4];
    getrandom::fill(&mut id_bytes).expect("OS CSPRNG unavailable");
    decision.insert("decision_id".into(), json!(format!("d-{}", hex(&id_bytes))));
    decision.insert("decision".into(), json!(decision_text));
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
    // The board answer carries the operator's authority onto the row: the
    // gate honors only `operator` rows, and the answer came from the
    // operator's own surface. `decided_by` is set outright: the no-identity
    // path resolves "unattributed-caller", which must not sit on an operator
    // law row, and every attended path already resolves "operator".
    if grant_subject.is_some() {
        decision
            .entry("authority_source")
            .or_insert(json!("operator"));
        decision.insert("decided_by".into(), json!("operator"));
        decision
            .entry("attested_by")
            .or_insert(json!("question-board"));
        if affirmative {
            // The row carries the canonical decision the gate reads; the
            // user's exact words stay on it as rationale.
            decision.insert("rationale".into(), json!(answer));
        }
    }
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

/// Operator law only. States 2/3 of the provenance resolver (no harness
/// identity: the question board, the attention arm, an operator terminal)
/// and an explicit `--authority operator` are the two shapes allowed to mint
/// the grant row; a resolved agent authority never is.
fn operator_can_grant(req: &ClearRequest) -> bool {
    matches!(
        req.provenance
            .get("authority_source")
            .and_then(Value::as_str),
        None | Some("") | Some("operator")
    )
}

/// The closed set of board answers that read as the affirmative grant.
/// Exact-match polarity like the gate's own constant check: free prose never
/// parses, so an answer outside the set still records law at the subject
/// (the gate reads it as a conflict and refuses) but grants nothing.
fn is_affirmative_merge_answer(answer: &str) -> bool {
    let normalized = answer
        .trim()
        .trim_end_matches(['.', '!', '?', ';', ',', ':'])
        .to_lowercase();
    matches!(
        normalized.as_str(),
        "merge it now"
            | "merge now"
            | "merge it"
            | "merge"
            | "merge authorized for this head"
            | "yes"
            | "y"
            | "approve"
            | "approved"
            | "lgtm"
            | "ship it"
            | "go ahead"
            | "do it"
    )
}

/// The question's merge-grant subject, when it is one.
fn merge_grant_subject(question_event: &Value) -> Option<String> {
    let subject = question_event
        .get("data")
        .and_then(|data| data.get("subject"))
        .and_then(Value::as_str)?;
    crate::merge_grant::parse_head_grant_subject(subject).map(|_| subject.to_string())
}

/// Does this answer on a merge-grant question record the grant? Two shapes:
/// the closed terminal set above, or picking the question's recommended
/// option when that option reads affirmative.
fn merge_answer_grants(question_event: &Value, answer: &str) -> bool {
    is_affirmative_merge_answer(answer) || picks_recommended_option(question_event, answer)
}

/// The affirmative LEADS a board option's text may open with. Exact match is
/// unavailable here (the option text is its own sentence), so the lead set is
/// closed and conservative: an option that opens any other way never reads as
/// the grant, whatever the recommendation says.
fn is_affirmative_option_text(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    [
        "yes", "y", "merge", "approve", "approved", "lgtm", "ship it", "go ahead", "do it",
    ]
    .iter()
    .any(|lead| {
        normalized == *lead
            || normalized.starts_with(&format!("{lead} "))
            || normalized.starts_with(&format!("{lead},"))
    })
}

/// True when the answer picks the question's recommended option and that
/// option reads affirmative. The board words lane delivers the echoed option
/// line ("1. Yes, merge PR 2911 at 5bf16e0bed. - notes: ..."), which the
/// closed exact-match set can never admit; the recommendation intake
/// validated is the asker's own affirmative, so picking it IS the grant.
/// A recommendation whose text is not affirmative (a hold) never grants.
fn picks_recommended_option(question_event: &Value, answer: &str) -> bool {
    let data = question_event.get("data").unwrap_or(question_event);
    let Some(recommended) = data
        .get("context")
        .and_then(|context| context.get("recommendation"))
        .and_then(|rec| rec.get("option"))
        .and_then(Value::as_u64)
        .filter(|n| *n >= 1)
    else {
        return false;
    };
    let Some(options) = data.get("options").and_then(Value::as_array) else {
        return false;
    };
    // File options carry an explicit `n` (1-based); flag options are bare
    // strings whose order is the option number.
    let text = options
        .iter()
        .enumerate()
        .find(|(i, o)| {
            o.get("n")
                .and_then(Value::as_u64)
                .is_some_and(|n| n == recommended)
                || o.get("n").is_none() && (*i as u64) + 1 == recommended
        })
        .and_then(|(_, o)| {
            o.get("text")
                .and_then(Value::as_str)
                .or_else(|| o.as_str())
                .map(str::to_string)
        })
        .filter(|text| !text.is_empty());
    let Some(text) = text else {
        return false;
    };
    if !is_affirmative_option_text(&text) {
        return false;
    }
    let norm = |s: &str| {
        s.trim()
            .trim_end_matches(['.', '!', '?', ';', ',', ':'])
            .to_lowercase()
    };
    let answer_norm = norm(answer);
    if answer_norm.is_empty() {
        return false;
    }
    let text_norm = norm(&text);
    if answer_norm == text_norm || answer_norm.starts_with(&text_norm) {
        return true;
    }
    // The echoed number: "1", "1. Yes, ...", "1) Yes", "1: yes". The
    // boundary check keeps "10." from reading as option 1.
    let num = recommended.to_string();
    match answer_norm.strip_prefix(&num) {
        Some(rest) => rest.is_empty() || rest.starts_with(['.', ')', ':', ' ']),
        None => false,
    }
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
    // Same recovery rule as the decision mirror: a close read back through
    // journal_text re-appends only as its raw stored envelope.
    let line = crate::event_store::raw_stored_line(journal, line)?;
    let event_id = stored_event_id(journal, &line)?.unwrap_or_else(|| format!("close:{qid}"));
    crate::event_store::append_envelope(journal, &line, Some(&event_id)).map(|_| ())
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
