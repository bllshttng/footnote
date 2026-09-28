//! `fno-agents needs --items` (AC1-ERR): a question store that exists but
//! cannot be read names itself in `sources` and the exit is non-zero. The
//! output never shows an empty `items` as a clean read.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn workdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("needs-items-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    dir
}

/// `FNO_AGENTS_HOME=<dir>/fno-agents` so the journals resolve under `<dir>`.
fn run(home_parent: &Path, cwd: &Path) -> (Option<i32>, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(["needs", "--items"])
        .current_dir(cwd)
        .env("FNO_AGENTS_HOME", home_parent.join("fno-agents"))
        .envs(fno_agents::test_run::self_owner_env())
        .output()
        .expect("binary spawns");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    (out.status.code(), parsed)
}

#[test]
fn unreadable_question_store_is_a_named_source_and_a_nonzero_exit() {
    let dir = workdir("unreadable");
    // A DIRECTORY named questions.jsonl: it exists and cannot be read.
    std::fs::create_dir_all(dir.join("questions.jsonl")).unwrap();
    let (code, parsed) = run(&dir, &dir);
    assert_eq!(code, Some(1), "exit is non-zero on an unreadable store");
    let sources = parsed
        .get("sources")
        .and_then(Value::as_array)
        .expect("sources array");
    let questions = sources
        .iter()
        .find(|s| s.get("store").and_then(Value::as_str) == Some("questions.jsonl"))
        .expect("questions store named");
    assert_eq!(questions.get("readable"), Some(&Value::Bool(false)));
}

#[test]
fn clean_read_exit_zero_and_items_present() {
    let dir = workdir("clean");
    let row = r#"{"ts":"2026-09-18T12:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-open1","question":"Which reading?","ask":"pick one","session_id":"s1","cwd":"/repo/fno","options":["yes","no"]}}"#;
    std::fs::write(dir.join("questions.jsonl"), format!("{row}\n")).unwrap();
    let (code, parsed) = run(&dir, &dir);
    assert_eq!(code, Some(0));
    let items = parsed
        .get("items")
        .and_then(Value::as_array)
        .expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].get("id").and_then(Value::as_str), Some("q-open1"));
    assert_eq!(
        items[0].get("kind").and_then(Value::as_str),
        Some("question")
    );
}
