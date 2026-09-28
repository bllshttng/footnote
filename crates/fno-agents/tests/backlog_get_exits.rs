//! The native `backlog get` exit contract: 0 hit, 1 clean miss (naming the
//! served store, no unreadable diagnostics), 3 unreadable (naming the read
//! failure, never asserting absence). Ported off the Python surface's
//! exit-code tests when its graph ladder came down.

use std::path::PathBuf;
use std::process::Command;

fn sandbox() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("config.toml");
    std::fs::write(&config, format!("state_dir = \"{}\"", dir.path().display()))
        .expect("write config");
    let graph = dir.path().join("graph.json");
    (dir, config, graph)
}

fn get(config: &PathBuf, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args([&["backlog", "get"], args].concat())
        .env("FNO_CONFIG", config)
        .env("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
        .env("FNO_TRACKER_BACKEND", "graph")
        // A client spawned by a test dies with the test run, never idling
        // as a daemon orphan (x-5533's contract, guard-enforced).
        .envs(fno_agents::test_run::self_owner_env())
        .output()
        .expect("run fno-agents backlog get");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn seed(graph: &PathBuf, entries: &[serde_json::Value]) {
    fno_agents::graph_store::seed_rows(graph, entries).expect("seed store");
}

#[test]
fn a_present_node_resolves_and_field_prints_raw() {
    let (_dir, config, graph) = sandbox();
    seed(
        &graph,
        &[serde_json::json!({
            "id": "x-aaaa1111", "title": "n", "status": "ready",
            "project": "fno", "slug": "n"
        })],
    );
    let (code, stdout, stderr) = get(&config, &["--strict", "x-aaaa1111", "--field", "id"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert_eq!(stdout.trim(), "x-aaaa1111");
}

#[test]
fn a_clean_miss_is_exit_1_naming_the_served_store_without_read_diagnostics() {
    let (_dir, config, graph) = sandbox();
    seed(
        &graph,
        &[serde_json::json!({"id": "x-aaaa1111", "title": "n"})],
    );
    let (code, stdout, stderr) = get(&config, &["--strict", "x-zzzz9999"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("No node matching 'x-zzzz9999' (id/slug/bare-hex)"),
        "{combined}"
    );
    let db = graph.with_extension("db");
    assert!(combined.contains(&db.display().to_string()), "{combined}");
    assert!(
        !combined.to_lowercase().contains("unreadable"),
        "{combined}"
    );
    assert!(!combined.to_lowercase().contains("corrupt"), "{combined}");
}

#[test]
fn an_empty_store_is_a_clean_miss_not_an_unreadable_one() {
    let (_dir, config, graph) = sandbox();
    seed(&graph, &[]);
    let (code, _stdout, stderr) = get(&config, &["x-d157"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("No node matching"), "{stderr}");
}

#[test]
fn an_unreadable_store_is_the_distinct_exit_3_naming_the_failure_never_absence() {
    let (_dir, config, graph) = sandbox();
    let db = graph.with_extension("db");
    seed(
        &graph,
        &[serde_json::json!({"id": "x-aaaa1111", "title": "n"})],
    );
    std::fs::write(&db, b"not a database at all").expect("corrupt db");
    let (code, stdout, stderr) = get(&config, &["x-d157"]);
    assert_eq!(
        code,
        fno_agents::backlog::get_cli::GRAPH_UNREADABLE_EXIT,
        "{stdout}{stderr}"
    );
    assert_ne!(code, 1);
    let combined = format!("{stdout}{stderr}");
    assert!(!combined.contains("No node matching"), "{combined}");
    assert!(
        combined.contains("Could not read the graph cleanly"),
        "{combined}"
    );
}
