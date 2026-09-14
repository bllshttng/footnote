//! x-920a wave 2: the native note action (backlog-note), real-store tests.

use fno_agents::backlog::node_state;
use serde_json::json;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn fixture(id: &str, status: &str) -> serde_json::Value {
    json!({
        "id": id,
        "slug": format!("slug-{id}"),
        "title": format!("Node {id}"),
        "type": "feature",
        "status": status,
        "priority": "p1",
        "details": "premise",
    })
}

fn write_graph(path: &PathBuf, entries: &[serde_json::Value]) {
    std::fs::write(
        path,
        serde_json::to_string(&json!({ "entries": entries })).unwrap(),
    )
    .unwrap();
}

fn read_graph(path: &PathBuf) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Run the binary entry point with the body on stdin.
fn note(_graph: &std::path::Path, args: &[&str], body: &str) -> i32 {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(std::iter::once("backlog-note").chain(args.iter().copied()))
        // The client lazy-starts a daemon that inherits this env, so the
        // daemon dies with this test run instead of idling an hour (x-5533).
        .envs(fno_agents::test_run::self_owner_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn fno-agents backlog-note");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    std::io::stdout().write_all(&out.stdout).unwrap();
    out.status.code().unwrap_or(-1)
}

fn graph_arg(graph: &std::path::Path) -> [String; 2] {
    ["--graph".to_string(), graph.display().to_string()]
}

#[test]
fn ac5_positional_stdin_and_file_bodies_all_replace_state() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture("t-1", "ready")]);
    let g = graph_arg(&graph);
    // stdin
    let code = note(
        &graph,
        &[
            "--stdin", "--json", "--node", "t-1", "--quiet", &g[0], &g[1],
        ],
        "body one",
    );
    assert_eq!(code, 0);
    assert_eq!(node_state::current_revision(&graph, "t-1").unwrap(), 1);
    // body file
    let body_file = dir.path().join("body.txt");
    std::fs::write(&body_file, "body two").unwrap();
    let bf = body_file.display().to_string();
    let code = note(
        &graph,
        &[
            "--body-file",
            &bf,
            "--json",
            "--node",
            "t-1",
            "--quiet",
            &g[0],
            &g[1],
        ],
        "",
    );
    assert_eq!(code, 0);
    assert_eq!(node_state::current_revision(&graph, "t-1").unwrap(), 2);
    // positional
    let code = note(
        &graph,
        &["t-1", "body three", "--json", "--quiet", &g[0], &g[1]],
        "",
    );
    assert_eq!(code, 0);
    let entries = read_graph(&graph)["entries"].as_array().unwrap().clone();
    let row = entries.iter().find(|r| r["id"] == json!("t-1")).unwrap();
    assert_eq!(row[node_state::STATE_KEY]["body"], json!("body three"));
    // A stale explicit revision refuses without overwrite.
    let code = note(
        &graph,
        &[
            "t-1",
            "stale",
            "--json",
            "--quiet",
            "--if-revision",
            "1",
            &g[0],
            &g[1],
        ],
        "",
    );
    assert_eq!(code, 3);
    let row = read_graph(&graph)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!("t-1"))
        .unwrap()
        .clone();
    assert_eq!(row[node_state::STATE_KEY]["body"], json!("body three"));
}

#[test]
fn ac7_machine_and_wave_records_go_to_history_only() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture("m-1", "in_progress")]);
    let g = graph_arg(&graph);
    // A human state first so there is something a machine must not overwrite.
    note(
        &graph,
        &[
            "--stdin", "--json", "--node", "m-1", "--quiet", &g[0], &g[1],
        ],
        "human state",
    );
    let code = note(
        &graph,
        &[
            "--machine",
            "task_done",
            "--stdin",
            "--json",
            "--node",
            "m-1",
            "--quiet",
            &g[0],
            &g[1],
        ],
        "machine progress row",
    );
    assert_eq!(code, 0);
    let row = read_graph(&graph)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!("m-1"))
        .unwrap()
        .clone();
    assert_eq!(row[node_state::STATE_KEY]["body"], json!("human state"));
    let (_, total) = fno_agents::backlog::note_history::read(&graph, Some("m-1"), 0, 50).unwrap();
    assert_eq!(total, 1, "machine record landed in history");
    // Wave addition: history + the refresh marker.
    let code = note(
        &graph,
        &[
            "--wave", "--stdin", "--json", "--node", "m-1", "--quiet", &g[0], &g[1],
        ],
        "wave 2 done",
    );
    assert_eq!(code, 0);
    let row = read_graph(&graph)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!("m-1"))
        .unwrap()
        .clone();
    assert_eq!(row["state_needs_refresh"], json!(true));
}

#[test]
fn ac7_terminal_node_note_is_history_only() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture("d-1", "done")]);
    let g = graph_arg(&graph);
    let code = note(
        &graph,
        &[
            "--stdin", "--json", "--node", "d-1", "--quiet", &g[0], &g[1],
        ],
        "",
    );
    assert_eq!(code, 0);
    let row = read_graph(&graph)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!("d-1"))
        .unwrap()
        .clone();
    assert!(
        row.get(node_state::STATE_KEY).is_none(),
        "no hot state on a done node"
    );
    let (_, total) = fno_agents::backlog::note_history::read(&graph, Some("d-1"), 0, 50).unwrap();
    assert_eq!(total, 1);
}
