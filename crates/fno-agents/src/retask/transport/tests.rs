//! The live-seam tests, ported from the Python run-level suite. The one
//! subprocess seam (`run`) is injected the way the Python tests monkeypatched
//! `subprocess.run`; the registry and graph are real files in a temp dir.

use super::*;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Output;

fn out(code: i32, stdout: &str, stderr: &str) -> Output {
    Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

fn registry_row(name: &str, harness: &str, session: &str, predecessors: Value) -> Value {
    json!({
        "name": name,
        "cwd": "/repo",
        "status": "live",
        "created_at": "2026-09-25T00:00:00Z",
        "harness": harness,
        "harness_session_id": session,
        "predecessor_session_ids": predecessors,
    })
}

fn write_registry(path: &std::path::Path, agents: Value) {
    std::fs::write(
        path,
        json!({"schema_version": crate::state::REGISTRY_SCHEMA_VERSION, "agents": agents})
            .to_string(),
    )
    .expect("registry fixture writes");
}

fn graph_file(dir: &std::path::Path, entries: Value) -> PathBuf {
    // A distinct file from seams_with's default graph, so an override here is
    // the one the preflight reads.
    let path = dir.join("graph-fixture.json");
    crate::graph_store::seed_rows(&path, entries.as_array().expect("rows array"))
        .expect("graph fixture writes");
    path
}

fn codex_row() -> RetaskRow {
    RetaskRow {
        name: "bp-xbdb9-retask".to_string(),
        harness: "codex".to_string(),
        provider: None,
        model: Some("gpt-5.6-sol".to_string()),
        effort: Some("high".to_string()),
        substrate: Some("pane".to_string()),
        status_live: true,
        harness_session_id: Some("old-session".to_string()),
        launch_account: None,
        mux: Some(("main".to_string(), 12)),
        thread_id: None,
    }
}

/// A seam set whose `run` answers from the given closure, over a temp state
/// tree. A source node whose session matches the row is pre-written so the
/// preflight joins ready; a test that needs a different graph overwrites
/// `graph_path` after.
fn seams_with(
    dir: &tempfile::TempDir,
    row: RetaskRow,
    run: impl FnMut(&[String], u64, Option<&str>) -> Result<Output, TransportFailure> + 'static,
) -> LiveSeams {
    let graph_path = dir.path().join("graph.json");
    crate::graph_store::seed_rows(
        &graph_path,
        &[json!({
            "id": "x-source",
            "pr_number": Value::Null,
            "sessions": [{"harness": row.harness, "session_id": row.harness_session_id}],
        })],
    )
    .expect("default graph fixture writes");
    LiveSeams {
        session: "main".to_string(),
        pane: "12".to_string(),
        node: "x-bbbb".to_string(),
        registry_path: dir.path().join("registry.json"),
        graph_path,
        clear_sent: false,
        restamped: row.harness_session_id.clone(),
        renamed: None,
        worker: row,
        run: Box::new(run),
    }
}

fn tempdir(tag: &str) -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir()).unwrap_or_else(|_| panic!("tempdir {tag}"))
}

const CODEX_PROMPT: &str = "› Ask Codex to do anything\n";

#[test]
fn test_source_preflight_joins_exact_session_and_refuses_open_non_green() {
    let dir = tempdir("preflight-open");
    let graph = graph_file(
        dir.path(),
        json!([{
            "id": "x-source",
            "cwd": "/repo",
            "pr_number": 1168,
            "sessions": [{"harness": "codex", "session_id": "old-session"}],
        }]),
    );
    let row = codex_row();
    let mut seams = seams_with(&dir, row.clone(), |_argv, _secs, _cwd| {
        Ok(out(
            1,
            &json!({
                "pr_state": "OPEN",
                "green": false,
                "head_sha": "source-head",
                "verdict": "red",
                "checks": {"failing": 4},
            })
            .to_string(),
            "",
        ))
    });
    seams.graph_path = graph;
    let receipt = seams.source_preflight().expect("preflight answers");

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("source_pr_not_green"));
    assert_eq!(receipt["source_node_id"], json!("x-source"));
    assert_eq!(receipt["pr"], json!(1168));
}

#[test]
fn test_source_preflight_trusts_a_graph_done_merged_source_node() {
    let dir = tempdir("preflight-done");
    let graph = graph_file(
        dir.path(),
        json!([{
            "id": "x-source",
            "cwd": "/repo",
            "pr_number": 2042,
            "status": "done",
            "merge_status": "merged",
            "sessions": [{"harness": "codex", "session_id": "old-session"}],
        }]),
    );
    let row = codex_row();
    let mut seams = seams_with(&dir, row.clone(), |_argv, _secs, _cwd| {
        panic!("no pr status read")
    });
    seams.graph_path = graph;
    let receipt = seams.source_preflight().expect("preflight answers");

    assert_eq!(receipt["status"], json!("ready"));
    assert_eq!(receipt["source_node_id"], json!("x-source"));
}

#[test]
fn test_source_preflight_trusts_a_graph_superseded_source_node() {
    let dir = tempdir("preflight-superseded");
    let graph = graph_file(
        dir.path(),
        json!([{
            "id": "x-source",
            "cwd": "/repo",
            "pr_number": 2042,
            "status": "superseded",
            "sessions": [{"harness": "codex", "session_id": "old-session"}],
        }]),
    );
    let row = codex_row();
    let mut seams = seams_with(&dir, row.clone(), |_argv, _secs, _cwd| {
        panic!("no pr status read")
    });
    seams.graph_path = graph;
    let receipt = seams.source_preflight().expect("preflight answers");

    assert_eq!(receipt["status"], json!("ready"));
}

#[test]
fn test_source_preflight_reads_an_open_source_pr_once_without_refresh() {
    let dir = tempdir("preflight-in-review");
    let graph = graph_file(
        dir.path(),
        json!([{
            "id": "x-source",
            "cwd": "/repo",
            "pr_number": 1168,
            "status": "in_review",
            "sessions": [{"harness": "codex", "session_id": "old-session"}],
        }]),
    );
    let row = codex_row();
    let mut seams = seams_with(&dir, row.clone(), |_argv, _secs, _cwd| {
        Ok(out(
            1,
            &json!({
                "pr_state": "OPEN",
                "green": true,
                "head_sha": "source-head",
                "verdict": "green",
            })
            .to_string(),
            "",
        ))
    });
    seams.graph_path = graph;
    let receipt = seams.source_preflight().expect("preflight answers");

    assert_eq!(receipt["status"], json!("ready"));
    assert_eq!(receipt["source_pr"], json!(1168));
}

#[test]
fn test_source_preflight_folds_a_pr_status_failure_into_the_unknown_refusal() {
    let dir = tempdir("preflight-timeout");
    let graph = graph_file(
        dir.path(),
        json!([{
            "id": "x-source",
            "cwd": "/repo",
            "pr_number": 1168,
            "status": "in_review",
            "sessions": [{"harness": "codex", "session_id": "old-session"}],
        }]),
    );
    let row = codex_row();
    let mut seams = seams_with(&dir, row.clone(), |_argv, _secs, _cwd| {
        // The bounded runner's deadline death: killed, so no exit code.
        Err(TransportFailure {
            reason: "pane_read_timeout".to_string(),
            detail: None,
        })
    });
    seams.graph_path = graph;
    let receipt = seams.source_preflight().expect("preflight answers");

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("source_pr_status_unknown"));
    assert!(receipt.get("error").is_some());
}

#[test]
fn test_source_preflight_multi_phase_entries_on_one_node_are_not_ambiguous() {
    let dir = tempdir("preflight-phases");
    let graph = graph_file(
        dir.path(),
        json!([{
            "id": "x-source",
            "cwd": "/repo",
            "pr_number": Value::Null,
            "sessions": [
                {"harness": "codex", "session_id": "old-session", "phase": "think"},
                {"harness": "codex", "session_id": "old-session", "phase": "blueprint"},
            ],
        }]),
    );
    let row = codex_row();
    let mut seams = seams_with(&dir, row.clone(), |_argv, _secs, _cwd| unreachable!());
    seams.graph_path = graph;
    let receipt = seams.source_preflight().expect("preflight answers");

    assert_eq!(receipt["status"], json!("ready"));
    assert_eq!(receipt["source_node_id"], json!("x-source"));
}

#[test]
fn test_source_preflight_two_distinct_nodes_stay_ambiguous() {
    let dir = tempdir("preflight-ambiguous");
    let graph = graph_file(
        dir.path(),
        json!([
            {
                "id": "x-one",
                "pr_number": Value::Null,
                "sessions": [{"harness": "codex", "session_id": "old-session"}],
            },
            {
                "id": "x-two",
                "pr_number": Value::Null,
                "sessions": [{"harness": "codex", "session_id": "old-session"}],
            },
        ]),
    );
    let row = codex_row();
    let mut seams = seams_with(&dir, row.clone(), |_argv, _secs, _cwd| unreachable!());
    seams.graph_path = graph;
    let receipt = seams.source_preflight().expect("preflight answers");

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("source_node_ambiguous"));
}

#[test]
fn test_live_permission_mode_reads_the_last_transcript_record() {
    let dir = tempdir("permission-mode");
    let projects = dir.path().join("projects");
    let session_dir = projects.join("proj");
    std::fs::create_dir_all(&session_dir).unwrap();
    let transcript = session_dir.join("11111111-2222-3333-4444-555555555555.jsonl");
    std::fs::write(
        &transcript,
        json!({"type": "user", "message": "hi"}).to_string()
            + "\n"
            + &json!({"type": "permission-mode", "permissionMode": "default"}).to_string()
            + "\nnot json\n"
            + &json!({"type": "permission-mode", "permissionMode": "bypassPermissions"})
                .to_string()
            + "\n"
            + &json!({"type": "user", "message": "go"}).to_string()
            + "\n",
    )
    .unwrap();
    std::env::set_var(
        super::super::super::claude_drive::PROJECTS_DIR_ENV,
        &projects,
    );
    let claude_row = RegistryEntry {
        harness: Some("claude".to_string()),
        harness_session_id: Some("11111111-2222-3333-4444-555555555555".to_string()),
        ..RegistryEntry::default()
    };

    assert_eq!(
        live_permission_mode(&claude_row),
        Some("bypassPermissions".to_string())
    );

    // A record torn by a concurrent append does not decide; the last complete
    // record does.
    use std::io::Write;
    let mut handle = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    handle
        .write_all(b"{\"type\":\"permission-mode\",\"permissionMode\":\"yolo\"")
        .unwrap();
    drop(handle);
    assert_eq!(
        live_permission_mode(&claude_row),
        Some("bypassPermissions".to_string())
    );

    // Another harness never reads a transcript at all.
    std::env::remove_var(super::super::super::claude_drive::PROJECTS_DIR_ENV);
    assert_eq!(live_permission_mode(&codex_registry_entry()), None);
}

fn codex_registry_entry() -> RegistryEntry {
    RegistryEntry {
        name: "bp-xbdb9-retask".to_string(),
        harness: Some("codex".to_string()),
        harness_session_id: Some("old-session".to_string()),
        cwd: "/repo".to_string(),
        ..RegistryEntry::default()
    }
}

/// The shared full-transit harness: a codex pane whose /clear send restamps
/// the registry row (modeling the daemon-side succession), then the real
/// rename-with-node and tier projection run against the temp registry.
struct Transit {
    dir: tempfile::TempDir,
    seams: LiveSeams,
}

fn full_transit(row: RetaskRow, target_command: &'static str) -> Transit {
    let dir = tempdir("full-transit");
    write_registry(
        &dir.path().join("registry.json"),
        json!([registry_row(
            &row.name,
            &row.harness,
            "old-session",
            json!([])
        )]),
    );
    let registry_path = dir.path().join("registry.json");
    let frames = RefCell::new(VecDeque::from(vec![
        CODEX_PROMPT.to_string(),
        "To continue this session, run codex resume old-session\n".to_string(),
        "Model: gpt-5.6-sol (reasoning high, summaries auto)".to_string(),
    ]));
    let sends: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let run = move |argv: &[String],
                    _secs: u64,
                    _cwd: Option<&str>|
          -> Result<Output, TransportFailure> {
        if argv.contains(&"send".to_string()) {
            let text = argv
                .iter()
                .position(|part| part == "--text")
                .map(|at| argv[at + 1].clone())
                .unwrap_or_default();
            sends.borrow_mut().push(text.clone());
            if text == "/clear" {
                write_registry(
                    &registry_path,
                    json!([registry_row(
                        "bp-xbdb9-retask",
                        "codex",
                        "new-session",
                        json!(["old-session"])
                    )]),
                );
            }
            if target_command != "" && text == target_command {
                // the submitted command is observed through `sends`
            }
            return Ok(out(0, "", ""));
        }
        if argv.contains(&"read".to_string()) {
            let frame = frames.borrow_mut().pop_front().unwrap_or_default();
            return Ok(out(0, &frame, ""));
        }
        Ok(out(0, "", ""))
    };
    let seams = seams_with(&dir, row, run);
    Transit { dir, seams }
}

fn run_transaction(row: &RetaskRow, target: &RetaskTarget, seams: &mut LiveSeams) -> Value {
    execute_retask(
        row,
        target,
        "x-bbbb",
        "$fno:target --no-merge x-bbbb",
        seams,
        None,
    )
    .unwrap_or_else(|failure| refused_receipt_from_transport(row, seams, failure))
}

fn codex_target() -> RetaskTarget {
    RetaskTarget {
        harness: "codex".to_string(),
        provider: None,
        model: Some("gpt-5.6-sol".to_string()),
        effort: Some("high".to_string()),
        substrate: None,
        permission_mode: None,
        route: None,
        account: None,
        verb: "target".to_string(),
    }
}

#[test]
fn test_run_retask_parses_codex_clear_receipt_before_accepting_successor() {
    let row = codex_row();
    let mut transit = full_transit(row.clone(), "$fno:target --no-merge x-bbbb");
    let receipt = run_transaction(&row, &codex_target(), &mut transit.seams);

    assert_eq!(receipt["status"], json!("retasked"));
    assert_eq!(receipt["source_session_id"], json!("old-session"));
    assert_eq!(receipt["current_session_id"], json!("new-session"));
    assert_eq!(receipt["transition"], json!("succession"));
    assert_eq!(receipt["registry_rows"], json!(1));
    // The rename carries the node in the same transaction (AC8).
    let registry = crate::state::load_registry(&transit.dir.path().join("registry.json")).unwrap();
    let renamed = registry
        .entries
        .iter()
        .find(|entry| entry.harness_session_id.as_deref() == Some("new-session"))
        .expect("restamped row survives");
    assert_eq!(renamed.node.as_deref(), Some("x-bbbb"));
    assert!(renamed
        .aliases
        .iter()
        .any(|alias| alias == "bp-xbdb9-retask"));
}

#[test]
fn test_run_retask_refuses_a_branch_row_before_rename() {
    let dir = tempdir("branch-row");
    let mut fork = registry_row("fork-xbdb9", "codex", "new-session", json!([]));
    fork["forked_from_session_id"] = json!("old-session");
    write_registry(
        &dir.path().join("registry.json"),
        json!([
            registry_row("bp-xbdb9-retask", "codex", "old-session", json!([])),
            fork,
        ]),
    );
    let row = codex_row();
    let mut seams = seams_with(&dir, row.clone(), |_argv, _secs, _cwd| {
        if _argv.contains(&"read".to_string()) {
            return Ok(out(0, CODEX_PROMPT, ""));
        }
        Ok(out(0, "", ""))
    });
    let receipt = run_transaction(&row, &codex_target(), &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(
        receipt["reason"],
        json!("session_transition_not_succession")
    );
    assert_eq!(receipt["cleared"], json!(true));
    assert!(
        seams.renamed_name().is_none(),
        "rename must wait for succession proof"
    );
}

#[test]
fn test_run_retask_converts_mux_timeout_to_structured_refusal() {
    let dir = tempdir("mux-timeout");
    let row = codex_row();
    let mut seams = seams_with(&dir, row.clone(), |_argv, _secs, _cwd| {
        Err(TransportFailure {
            reason: "pane_read_timeout".to_string(),
            detail: None,
        })
    });
    let receipt = run_transaction(&row, &codex_target(), &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("pane_read_timeout"));
    assert_eq!(receipt["target_submit_confirmed"], json!(false));
}

#[test]
fn test_run_retask_exit_23_on_clear_reports_view_left_worker() {
    let dir = tempdir("exit23-clear");
    let stderr_line: String = "fno mux pane send: pane 12 is the portal for bp-xbdb9-retask (attach deadbee1) but its child runs claude agents; the viewer left that session".to_string();
    let row = codex_row();
    let detail = stderr_line.clone();
    let mut seams = seams_with(&dir, row.clone(), move |_argv, _secs, _cwd| {
        if _argv.contains(&"send".to_string()) {
            return Ok(out(23, "", &format!("{}\n", detail)));
        }
        Ok(out(0, CODEX_PROMPT, ""))
    });
    let receipt = run_transaction(&row, &codex_target(), &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("view_left_worker"));
    assert_eq!(receipt["detail"], json!(stderr_line));
    assert_eq!(receipt["cleared"], json!(false));
    assert_eq!(receipt["session_restamped"], json!(false));
}

#[test]
fn test_run_retask_exit_23_without_the_portal_marker_names_the_family() {
    let dir = tempdir("exit23-family");
    let stderr_line: String = "fno mux pane send: pane 12 carries label bp-xbdb9-retask but no session id resolves for it; re-address by session id through fno mux where".to_string();
    let row = codex_row();
    let detail = stderr_line.clone();
    let mut seams = seams_with(&dir, row.clone(), move |_argv, _secs, _cwd| {
        if _argv.contains(&"send".to_string()) {
            return Ok(out(23, "", &format!("{}\n", detail)));
        }
        Ok(out(0, CODEX_PROMPT, ""))
    });
    let receipt = run_transaction(&row, &codex_target(), &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("identity_refused"));
    assert_eq!(receipt["detail"], json!(stderr_line));
}

#[test]
fn test_run_retask_exit_23_after_clear_keeps_the_partial_state_truthful() {
    let dir = tempdir("exit23-after-clear");
    write_registry(
        &dir.path().join("registry.json"),
        json!([registry_row(
            "bp-xbdb9-retask",
            "codex",
            "old-session",
            json!([])
        )]),
    );
    let registry_path = dir.path().join("registry.json");
    let reads = RefCell::new(VecDeque::from(vec![
        CODEX_PROMPT.to_string(),
        "To continue this session, run codex resume old-session".to_string(),
    ]));
    let row = codex_row();
    let mut seams = seams_with(&dir, row.clone(), move |_argv, _secs, _cwd| {
        if _argv.contains(&"send".to_string()) {
            let text = _argv
                .iter()
                .position(|part| part == "--text")
                .map(|at| _argv[at + 1].clone())
                .unwrap_or_default();
            if text == "/clear" {
                write_registry(
                    &registry_path,
                    json!([registry_row(
                        "bp-xbdb9-retask",
                        "codex",
                        "new-session",
                        json!(["old-session"])
                    )]),
                );
                return Ok(out(0, "", ""));
            }
            return Ok(out(
                23,
                "",
                "fno mux pane send: pane 12 is the portal for bp-xbdb9-retask (attach deadbee1) but its child runs claude agents; the viewer left that session\n",
            ));
        }
        if _argv.contains(&"read".to_string()) {
            let frame = reads.borrow_mut().pop_front().unwrap_or_default();
            return Ok(out(0, &frame, ""));
        }
        Ok(out(0, "", ""))
    });
    let receipt = run_transaction(&row, &codex_target(), &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("view_left_worker"));
    assert_eq!(receipt["cleared"], json!(true));
    assert_eq!(receipt["session_restamped"], json!(true));
    assert_eq!(receipt["registry_name"], json!("t-bbbb"));
}

#[test]
fn test_run_retask_timeout_mid_transaction_reports_the_true_pane_state() {
    let dir = tempdir("timeout-mid");
    let row = codex_row();
    let mut seams = seams_with(&dir, row.clone(), move |_argv, _secs, _cwd| {
        // Die while settling the /clear: reads and sends land, the wait dies.
        if _argv.contains(&"wait".to_string()) {
            return Err(TransportFailure {
                reason: "pane_wait_timeout".to_string(),
                detail: None,
            });
        }
        Ok(out(0, CODEX_PROMPT, ""))
    });
    let receipt = run_transaction(&row, &codex_target(), &mut seams);

    assert_eq!(receipt["status"], json!("refused"));
    assert_eq!(receipt["reason"], json!("pane_wait_timeout"));
    assert_eq!(receipt["cleared"], json!(true));
    assert_eq!(receipt["session_restamped"], json!(false));
}

#[test]
fn test_run_retask_retasks_a_claude_thread_worker_whose_title_is_none() {
    let dir = tempdir("claude-thread");
    write_registry(
        &dir.path().join("registry.json"),
        json!([registry_row(
            "bp-xbdb9-retask",
            "claude",
            "old-session",
            json!(["old-session"])
        )]),
    );
    let registry_path = dir.path().join("registry.json");
    let row = RetaskRow {
        harness: "claude".to_string(),
        model: Some("old-model".to_string()),
        effort: Some("high".to_string()),
        substrate: Some("thread".to_string()),
        mux: None,
        thread_id: Some("F".to_string()),
        ..codex_row()
    };
    let target = RetaskTarget {
        harness: "claude".to_string(),
        model: Some("old-model".to_string()),
        ..codex_target()
    };
    let sends: std::rc::Rc<RefCell<Vec<String>>> = std::rc::Rc::new(RefCell::new(Vec::new()));
    let sends_in = std::rc::Rc::clone(&sends);
    // Read 1 paints the idle composer box (its bottom rule is 24 dashes, the
    // shape prompt_box_body scopes); read 2 carries the codex-style resume
    // receipt naming the predecessor; read 3 is the verified status frame.
    let reads = RefCell::new(VecDeque::from(vec![
        "─── t-name ─\n❯ \n────────────────────────\n".to_string(),
        "To continue this session, run codex resume old-session".to_string(),
        "Model: old-model (reasoning effort high)".to_string(),
    ]));
    let mut seams = seams_with(&dir, row.clone(), move |_argv, _secs, _cwd| {
        if _argv.contains(&"send".to_string()) {
            let text = _argv
                .iter()
                .position(|part| part == "--text")
                .map(|at| _argv[at + 1].clone())
                .unwrap_or_default();
            sends_in.borrow_mut().push(text.clone());
            if text == "/clear" {
                write_registry(
                    &registry_path,
                    json!([registry_row(
                        "bp-xbdb9-retask",
                        "claude",
                        "new-session",
                        json!(["old-session"])
                    )]),
                );
            }
            return Ok(out(0, "", ""));
        }
        if _argv.contains(&"ls".to_string()) {
            // No title row for this pane: the portal view reads None.
            return Ok(out(0, "[]", ""));
        }
        if _argv.contains(&"read".to_string()) {
            let frame = reads.borrow_mut().pop_front().unwrap_or_default();
            return Ok(out(0, &frame, ""));
        }
        Ok(out(0, "", ""))
    });
    seams.session = "main".to_string();
    seams.pane = "7".to_string();
    let receipt = run_transaction(&row, &target, &mut seams);

    assert_eq!(receipt["status"], json!("retasked"), "{receipt}");
    assert!(!receipt.to_string().contains("pane title unreadable"));
    eprintln!("SENDS={:?}", sends.borrow());
    assert!(sends.borrow().iter().any(|text| text == "/clear"));
}
