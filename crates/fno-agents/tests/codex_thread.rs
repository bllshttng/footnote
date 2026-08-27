use fno_agents::codex_thread::{
    parse_thread_start_response, thread_start_request_json, CodexThread, ThreadStartError,
};

#[test]
fn thread_start_reads_nested_thread_id_and_rejects_top_level_alias() {
    let request: serde_json::Value =
        serde_json::from_str(&thread_start_request_json("/tmp/worktree", "never"))
            .expect("valid request JSON");
    assert_eq!(request["method"], "thread/start");
    assert_eq!(request["params"]["cwd"], "/tmp/worktree");
    assert_eq!(request["params"]["approvalPolicy"], "never");

    assert_eq!(
        parse_thread_start_response(
            r#"{"id":1,"result":{"thread":{"id":"thread-1","path":"/tmp/rollout"}}}"#
        ),
        Ok(("thread-1".to_string(), "/tmp/rollout".to_string()))
    );
    assert_eq!(
        parse_thread_start_response(r#"{"id":1,"result":{"threadId":"thread-1"}}"#),
        Err(ThreadStartError::NotConfirmed)
    );
}

/// A stub app-server on PATH: enough protocol to start a thread, then a turn
/// that streams MORE frames than the old frame ceiling and goes quiet LONGER
/// than the old per-frame timeout before completing. The journey test's live
/// seed answers in a handful of frames and well under 15s, so it cleared both
/// old bounds by being small; this fixture is the scale the real lane runs at.
const FAKE_APP_SERVER: &str = r#"#!/usr/bin/env python3
import json, sys, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    method = msg.get("method")
    if method == "initialize":
        send({"id": msg.get("id"), "result": {}})
    elif method == "thread/start":
        send({"id": msg.get("id"), "result": {"thread": {"id": "thread-bounds", "path": "/tmp/fake-rollout.jsonl"}}})
    elif method == "turn/start":
        send({"id": msg.get("id"), "result": {"turn": {"id": "turn-bounds"}}})
        for i in range(300):
            send({"method": "turn/event", "params": {"seq": i}})
        time.sleep(16)
        send({"method": "turn/completed", "params": {"turn": {"id": "turn-bounds", "status": "completed", "items": [{"type": "agentMessage", "text": "BOUNDS_EXCEEDED_TOKEN"}]}}})
"#;

#[tokio::test]
async fn turn_survives_more_frames_and_a_longer_gap_than_the_old_bounds() {
    let bin_dir = tempfile::tempdir().unwrap();
    let fake = bin_dir.path().join("codex");
    std::fs::write(&fake, FAKE_APP_SERVER).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let saved_path = std::env::var_os("PATH");
    let mut prefixed = std::ffi::OsString::from(bin_dir.path().as_os_str());
    prefixed.push(":");
    if let Some(rest) = saved_path.as_ref() {
        prefixed.push(rest);
    }
    std::env::set_var("PATH", &prefixed);

    let worktree = tempfile::tempdir().unwrap();
    let mut thread = CodexThread::start(worktree.path(), None, false, None)
        .await
        .expect("thread starts against the stub app-server");
    let result = thread.drive_turn("run the wide turn").await;

    if let Some(path) = saved_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }

    let turn = result.expect("turn completes under the whole-turn budget");
    assert_eq!(turn.turn_id, "turn-bounds");
    assert_eq!(turn.text, "BOUNDS_EXCEEDED_TOKEN");
}
