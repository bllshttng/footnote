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
    fno_agents::graph_store::seed_rows(path, entries).unwrap();
}

/// The landed rows, read from the store: graph.json is a frozen mirror
/// under graph.db.
fn read_graph(path: &PathBuf) -> serde_json::Value {
    json!({ "entries": fno_agents::graph_store::read_rows(path).unwrap() })
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

/// The note door with all three channels captured: the x-786d contract is
/// asserted on stderr, so it must not inherit the test's stderr.
fn note_captured(args: &[&str], body: &str) -> (i32, String, String) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(std::iter::once("backlog-note").chain(args.iter().copied()))
        .envs(fno_agents::test_run::self_owner_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fno-agents backlog-note");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// x-786d: a corrupt store answers as a read failure (exit 5), never as an
/// absent node.
#[test]
fn corrupt_graph_names_the_read_failure_never_absence() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    std::fs::write(&graph, "{").unwrap();
    let g = graph_arg(&graph);
    let (code, stdout, stderr) = note_captured(&["t-1", "body", "--quiet", &g[0], &g[1]], "");
    assert_eq!(code, 5, "stdout: {stdout} stderr: {stderr}");
    assert!(stderr.contains("graph read failed"), "{stderr}");
    assert!(
        !stdout.contains("no node resolves") && !stderr.contains("no node resolves"),
        "a starved read must not read as an absent node: {stdout} {stderr}"
    );
}

/// The control: a genuinely absent id on a WELL-FORMED store still reads
/// absent, at the unchanged exit code. An absence assertion with no control
/// is the shape x-786d is about.
#[test]
fn absent_node_still_reports_absence_at_the_unchanged_code() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture("t-1", "ready")]);
    let g = graph_arg(&graph);
    let (code, stdout, stderr) = note_captured(&["t-nope", "body", "--quiet", &g[0], &g[1]], "");
    assert_eq!(code, 1, "stdout: {stdout} stderr: {stderr}");
    assert!(stderr.contains("no node resolves to 't-nope'"), "{stderr}");
    assert!(!stderr.contains("graph read failed"), "{stderr}");
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
    // The terminal receipt still names id and text, and replaces nothing.
    let (code, stdout, _) = note_captured(
        &[
            "--stdin", "--json", "--node", "d-1", "--quiet", &g[0], &g[1],
        ],
        "another note",
    );
    assert_eq!(code, 0);
    let receipt: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(receipt["id"], json!("d-1"));
    assert_eq!(receipt["text"], json!("another note"));
    assert!(receipt["replaced"].is_null());
}

#[test]
fn a_note_names_the_state_it_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture("t-1", "ready")]);
    let g = graph_arg(&graph);
    // First note over an empty state: the receipt says so.
    let (code, stdout, stderr) = note_captured(
        &[
            "t-1",
            "first line\nsecond line",
            "--self-session",
            "sess-aaaa1111",
            "--json",
            "--quiet",
            &g[0],
            &g[1],
        ],
        "",
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let first: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(first["replaced"].is_null());
    assert!(
        first["line"]
            .as_str()
            .unwrap()
            .contains("replaced nothing: t-1 had no current state"),
        "{stdout}"
    );
    // Second note: the receipt names revision 1, its size and its author.
    let (code, stdout, stderr) = note_captured(
        &[
            "t-1",
            "probe",
            "--self-session",
            "sess-bbbb2222",
            "--json",
            "--quiet",
            &g[0],
            &g[1],
        ],
        "",
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let second: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(second["replaced"]["revision"], json!(1));
    assert_eq!(second["replaced"]["chars"], json!(22));
    assert_eq!(
        second["replaced"]["source_session_id"],
        json!("sess-aaaa1111")
    );
    assert_eq!(second["replaced"]["head"], json!("first line..."));
    assert_eq!(second["id"], json!("t-1"));
    assert_eq!(second["text"], json!("probe"));
    assert_eq!(second["routed"], json!("state"));
    assert_eq!(second["revision"], json!(2));
    // Third note, text receipt: both sizes and the history pointer print.
    let (code, stdout, stderr) = note_captured(
        &[
            "t-1",
            "probe",
            "--self-session",
            "sess-cccc3333",
            "--quiet",
            &g[0],
            &g[1],
        ],
        "",
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("noted t-1: revision 3, "), "{stdout}");
    assert!(
        stdout.contains("replaced revision 2 (5 chars, written by session sess-bbbb2222"),
        "{stdout}"
    );
    assert!(stdout.contains("fno backlog notes history t-1"), "{stdout}");
    // AC6: the replaced body reads back whole from history.
    let (records, _) = node_state::history_page(&graph, "t-1", 0, 10).unwrap();
    assert!(
        records
            .iter()
            .any(|r| r["original"]["body"] == json!("first line\nsecond line")),
        "the two-line body must read back whole"
    );
}

#[test]
fn a_multibyte_head_is_cut_by_characters() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    write_graph(&graph, &[fixture("t-1", "ready")]);
    let g = graph_arg(&graph);
    let prior = "\u{1f30a}".repeat(100);
    let (code, _, stderr) = note_captured(&["t-1", &prior, "--json", "--quiet", &g[0], &g[1]], "");
    assert_eq!(code, 0, "stderr: {stderr}");
    let (code, stdout, stderr) =
        note_captured(&["t-1", "probe", "--json", "--quiet", &g[0], &g[1]], "");
    assert_eq!(code, 0, "stderr: {stderr}");
    let second: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let expected = format!("{}...", "\u{1f30a}".repeat(80));
    assert_eq!(second["replaced"]["head"], json!(expected));
}
