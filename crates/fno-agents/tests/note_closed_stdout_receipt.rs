//! The p0 repro: a note piped into a reader that closes early (`| head`)
//! must still land, and the verb must still exit 0. The child's stdout is a
//! pipe whose read end is closed before the child starts, so the receipt
//! print hits EPIPE deterministically (Rust std ignores SIGPIPE, so the
//! default `println!` path panics instead of dying quietly).

use std::io::Write;
use std::os::unix::io::FromRawFd;
use std::process::{Command, Stdio};

fn fixture_node(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "slug": format!("slug-{id}"),
        "title": format!("Node {id}"),
        "type": "feature",
        "status": "ready",
        "priority": "p1",
        "details": "premise",
    })
}

#[test]
fn a_closed_stdout_pipe_neither_loses_the_note_nor_fails_the_verb() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    fno_agents::graph_store::seed_rows(&graph, &[fixture_node("t-pipe")]).unwrap();

    let mut fds = [0 as libc::c_int; 2];
    unsafe { assert_eq!(libc::pipe(fds.as_mut_ptr()), 0) };
    let (read_end, write_end) = (fds[0], fds[1]);
    unsafe { libc::close(read_end) };
    let stdout = unsafe { std::fs::File::from_raw_fd(write_end) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args([
            "backlog",
            "note",
            "--graph",
            graph.to_str().unwrap(),
            "--stdin",
            "--json",
            "--node",
            "t-pipe",
        ])
        .envs(fno_agents::test_run::self_owner_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout))
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"the ruling that must survive")
        .unwrap();
    let out = child.wait_with_output().unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = fno_agents::graph_store::read_rows(&graph).unwrap();
    assert!(
        rows[0][fno_agents::backlog::node_state::STATE_KEY]["body"]
            .as_str()
            .unwrap()
            .contains("the ruling that must survive"),
        "note lost, graph now: {rows:?}"
    );
}
