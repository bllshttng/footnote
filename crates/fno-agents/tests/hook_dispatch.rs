use serde_json::{json, Value};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[test]
fn bash_pretooluse_dispatch_preserves_guard_refusal_and_events() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("repo root")
        .to_path_buf();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let events = temp.path().join("events.jsonl");
    let payload = json!({
        "tool_name": "Bash",
        "cwd": repo,
        "tool_input": {"command": "rg --files | head -4"}
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(["hook", "pretooluse-bash"])
        .current_dir(&repo)
        .env("FNO_REPO_ROOT", &repo)
        .env("FNO_EVENTS_PATH", &events)
        .env("HOME", &home)
        .env("CARGO_HOME", home.join(".cargo"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fno-agents hook process");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(serde_json::to_string(&payload).unwrap().as_bytes())
        .expect("write payload");
    let output = child.wait_with_output().expect("hook output");

    assert!(
        output.status.success(),
        "hook dispatch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).expect("hook JSON");
    assert_eq!(response["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(response["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap_or_default()
        .contains("[fno pipe guard]"));

    let rows = std::fs::read_to_string(events).expect("guard events");
    let guards: Vec<String> = rows
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|row| row["data"]["guard"].as_str().map(str::to_owned))
        .collect();
    let expected = [
        "bg-process-guard",
        "bin-install-guard",
        "git-protection",
        "pipe-guard",
        "recursive-grep-guard",
        "test-run-guard",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(
        guards, expected,
        "every guard must run once, in its former registration order"
    );
}
