//! Stop-gate refusal when a decided question left no decision record.
//!
//! The recording obligation is enforced at the stop gate, never self-reported:
//! a session that closed an operator question WITH an answer but emitted no
//! `operator_decision` event is held, and the hold names the question. This is
//! what stops "explicit" from meaning "never".
use std::fs;
use std::path::Path;

use tempfile::TempDir;

#[derive(Debug, serde::Deserialize)]
struct Decision {
    decision: String,
    #[allow(dead_code)]
    termination_reason: Option<String>,
    message: String,
}

fn fire(args: &[&str]) -> (i32, Decision) {
    let mut args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    args_owned.push("--global-settings".to_string());
    args_owned.push("/nonexistent/global-settings.yaml".to_string());
    if !args.iter().any(|a| a.starts_with("--author-harness")) {
        args_owned.push("--author-harness".to_string());
        args_owned.push("none".to_string());
    }
    let (code, json_str) = fno_agents::loopcheck::run_loop_check_capture(&args_owned);
    let d: Decision = serde_json::from_str(&json_str).unwrap_or_else(|e| {
        panic!("run_loop_check returned non-JSON (code={code}): {e}: {json_str}")
    });
    (code, d)
}

fn manifest(session_id: &str) -> String {
    format!(
        "---\nsession_id: {session_id}\ncreated_at: 2026-08-14T00:00:00Z\nattended: true\n---\n"
    )
}

fn transcript_no_intent() -> String {
    let msg = serde_json::json!({
        "message": { "role": "assistant", "content": "still working" }
    });
    serde_json::to_string(&msg).unwrap() + "\n"
}

fn asked(qid: &str, session_id: &str) -> String {
    serde_json::json!({
        "ts": "2026-08-14T21:00:00Z",
        "type": "operator_question",
        "source": "target",
        "data": {
            "question_id": qid,
            "question": "fold or migrate?",
            "session_id": session_id,
            "node": "x-7d94"
        }
    })
    .to_string()
}

fn closed_answered(qid: &str) -> String {
    serde_json::json!({
        "ts": "2026-08-14T21:05:00Z",
        "type": "operator_question_closed",
        "source": "target",
        "data": { "question_id": qid, "answer": "fold" }
    })
    .to_string()
}

fn closed_withdrawn(qid: &str) -> String {
    serde_json::json!({
        "ts": "2026-08-14T21:05:00Z",
        "type": "operator_question_closed",
        "source": "target",
        "data": { "question_id": qid }
    })
    .to_string()
}

fn recorded(qid: &str) -> String {
    serde_json::json!({
        "ts": "2026-08-14T21:06:00Z",
        "type": "operator_decision",
        "source": "target",
        "data": {
            "decision_id": "d-00000001",
            "decision": "fold",
            "question_id": qid,
            "subject": "x-7d94",
            "decided_by": "operator",
            "authority_source": "operator"
        }
    })
    .to_string()
}

fn setup() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().to_path_buf();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    std::env::set_var("FNO_NUDGE_DISABLED", "1");
    std::env::set_var("FNO_LOOPCHECK_MIN_FIRE_GAP_SECS", "0");
    fs::write(
        cwd.join(".fno/config.toml"),
        "[review]\nrequired_bots = []\n",
    )
    .unwrap();
    fs::write(cwd.join("target-state.md"), manifest("sess-d1")).unwrap();
    let transcript = cwd.join("transcript.jsonl");
    fs::write(&transcript, transcript_no_intent()).unwrap();
    (tmp, cwd, transcript)
}

fn base_args(cwd: &Path, transcript: &Path) -> Vec<String> {
    vec![
        "loop-check".to_string(),
        "--state".to_string(),
        cwd.join("target-state.md").to_string_lossy().into_owned(),
        "--transcript".to_string(),
        transcript.to_string_lossy().into_owned(),
        "--cwd".to_string(),
        cwd.to_string_lossy().into_owned(),
        "--gh-bin".to_string(),
        "/nonexistent/gh".to_string(),
        "--git-bin".to_string(),
        "/nonexistent/git".to_string(),
    ]
}

/// An answered close with no decision record refuses the stop and names the
/// question.
#[test]
fn answered_close_without_record_blocks() {
    let (_tmp, cwd, transcript) = setup();
    let events = cwd.join(".fno/events.jsonl");
    fs::write(
        &events,
        asked("q-11112222", "sess-d1") + "\n" + &closed_answered("q-11112222") + "\n",
    )
    .unwrap();

    let args = base_args(&cwd, &transcript);
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (_code, d) = fire(&refs);
    assert_eq!(d.decision, "block", "message was: {}", d.message);
    assert!(
        d.message.contains("q-11112222"),
        "the hold must name the question: {}",
        d.message
    );
    assert!(
        d.message.contains("decision"),
        "the hold must say what is missing: {}",
        d.message
    );
}

/// The green path: the same close WITH a matching operator_decision does not
/// trip the gate (the session may still be held by another gate; the assertion
/// is that THIS gate's signature is absent from the message).
#[test]
fn recorded_decision_clears_the_gate() {
    let (_tmp, cwd, transcript) = setup();
    let events = cwd.join(".fno/events.jsonl");
    fs::write(
        &events,
        asked("q-11112222", "sess-d1")
            + "\n"
            + &closed_answered("q-11112222")
            + "\n"
            + &recorded("q-11112222")
            + "\n",
    )
    .unwrap();

    let args = base_args(&cwd, &transcript);
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (_code, d) = fire(&refs);
    assert!(
        !d.message.contains("q-11112222"),
        "the decision gate must not hold a recorded decision: {}",
        d.message
    );
}

/// Attribution: another session's unanswered-record question never holds this
/// session.
#[test]
fn another_sessions_question_does_not_block() {
    let (_tmp, cwd, transcript) = setup();
    let events = cwd.join(".fno/events.jsonl");
    fs::write(
        &events,
        asked("q-33334444", "sess-other") + "\n" + &closed_answered("q-33334444") + "\n",
    )
    .unwrap();

    let args = base_args(&cwd, &transcript);
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (_code, d) = fire(&refs);
    assert!(
        !d.message.contains("q-33334444"),
        "a foreign question must not hold this session: {}",
        d.message
    );
}

/// A withdrawal (close with no answer) decides nothing and must not hold.
#[test]
fn withdrawn_close_does_not_block() {
    let (_tmp, cwd, transcript) = setup();
    let events = cwd.join(".fno/events.jsonl");
    fs::write(
        &events,
        asked("q-55556666", "sess-d1") + "\n" + &closed_withdrawn("q-55556666") + "\n",
    )
    .unwrap();

    let args = base_args(&cwd, &transcript);
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (_code, d) = fire(&refs);
    assert!(
        !d.message.contains("q-55556666"),
        "a withdrawal decides nothing: {}",
        d.message
    );
}

/// The journals are a UNION: a question asked and closed in the project
/// journal is cleared by a decision recorded in the global journal. The
/// operator verbs write to the canonical root's journal, which a worktree
/// stop gate reaches only through the union fold.
#[test]
fn record_in_a_sibling_journal_clears_the_gate() {
    let (_tmp, cwd, transcript) = setup();
    let events = cwd.join(".fno/events.jsonl");
    fs::write(
        &events,
        asked("q-77778888", "sess-d1") + "\n" + &closed_answered("q-77778888") + "\n",
    )
    .unwrap();
    let global = cwd.join("global-events.jsonl");
    fs::write(&global, recorded("q-77778888") + "\n").unwrap();

    let mut args = base_args(&cwd, &transcript);
    args.push("--global-events".to_string());
    args.push(global.to_string_lossy().into_owned());
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (_code, d) = fire(&refs);
    assert!(
        !d.message.contains("q-77778888"),
        "a record in a sibling journal must clear the gate: {}",
        d.message
    );
}
