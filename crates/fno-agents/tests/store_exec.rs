//! The one-shot `--store-exec` lane, driven end to end through the real
//! worker binary: the same dispatch the socket keeper serves, no socket
//! bound, no resident process, and a stateless begin/commit across two
//! exec processes (the publish's file flock serializes cross-process).

use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

const WORKER_BIN: &str = env!("CARGO_BIN_EXE_fno-agents-worker");

fn exec_request(graph: &std::path::Path, body: &str) -> (i32, Option<Value>) {
    let mut child = Command::new(WORKER_BIN)
        .args([
            "--store-exec",
            "--graph",
            &graph.to_string_lossy(),
            "--lock-timeout-secs",
            "2",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let reply = if out.stdout.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&out.stdout).unwrap())
    };
    (out.status.code().unwrap_or(-1), reply)
}

#[test]
fn store_exec_serves_read_begin_commit_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    std::fs::write(
        &graph,
        serde_json::to_string(&json!({
            "entries": [{"id": "x-exe", "slug": "exec-node", "title": "e", "status": "ready"}]
        }))
        .unwrap(),
    )
    .unwrap();

    // A read answers and binds no socket beside the graph file.
    let (code, reply) = exec_request(&graph, r#"{"id":1,"method":"read","params":{}}"#);
    assert_eq!(code, 0, "{reply:?}");
    assert!(reply.as_ref().unwrap()["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["id"] == "x-exe"));
    assert!(
        !dir.path().join("graph.json.store.sock").exists(),
        "exec binds no socket"
    );

    // begin on one exec, commit on a SECOND exec: the stateless write path
    // the clients ride.
    let (_code, begin) = exec_request(&graph, r#"{"id":1,"method":"begin","params":{}}"#);
    let begin = begin.unwrap();
    assert_eq!(begin["ok"], json!(true), "{begin}");
    let version = begin["result"]["version"].as_str().unwrap().to_string();
    let mut rows = begin["result"]["entries"].as_array().unwrap().clone();
    for row in rows.iter_mut() {
        if row["id"] == "x-exe" {
            row["title"] = json!("executed");
        }
    }
    let body = format!(
        r#"{{"id":1,"method":"commit","params":{{"version":"{version}","entries":{},"plan_rungs":{{}},"attempt":1}}}}"#,
        serde_json::to_string(&rows).unwrap()
    );
    let (code, commit) = exec_request(&graph, &body);
    assert_eq!(code, 0, "{commit:?}");
    assert_eq!(commit.unwrap()["ok"], json!(true));

    // A third exec sees the landed write.
    let (_code, after) = exec_request(&graph, r#"{"id":1,"method":"read","params":{}}"#);
    let landed = after.unwrap()["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "x-exe")
        .unwrap()
        .clone();
    assert_eq!(landed["title"], json!("executed"));
}

#[test]
fn store_exec_refuses_without_a_graph() {
    let out = Command::new(WORKER_BIN)
        .args(["--store-exec"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing --graph"), "{stderr}");
}
