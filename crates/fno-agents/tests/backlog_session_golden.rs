//! The native `fno backlog session` lifecycle: the lease contract the port
//! must keep (2h TTL, the passed identity, pid_unavailable when no durable
//! session pid resolves), the handover join ladder, and the close release.
//! Characterization for the deleted Python `graph/_session.py`.

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

fn identity_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("FNO_HARNESS_NAME", "claude"),
        ("FNO_HARNESS_SESSION_ID", "sess-golden-0001"),
    ]
}

fn ambient_markers() -> &'static [&'static str] {
    &[
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
        "OPENCODE_SESSION_ID",
        "CLAUDE_SESSION_ID",
        "FNO_HARNESS_NAME",
        "FNO_HARNESS_SESSION_ID",
        "FNO_NODE_CLAIM_HOLDER",
    ]
}

fn session(
    config: &PathBuf,
    graph: &PathBuf,
    args: &[&str],
    extra_env: &[(&str, &str)],
    strip_markers: bool,
) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    cmd.args([&["backlog", "session"], args].concat())
        .env("FNO_CONFIG", config)
        .env("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
        .env("FNO_TRACKER_BACKEND", "graph")
        .env(
            "FNO_CLAIMS_ROOT",
            graph.parent().unwrap().join("claims-root"),
        )
        .envs(fno_agents::test_run::self_owner_env());
    for (k, v) in identity_env() {
        cmd.env(k, v);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    if strip_markers {
        for marker in ambient_markers() {
            cmd.env_remove(marker);
        }
    }
    let out = cmd.output().expect("run fno-agents backlog session");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn seed(graph: &PathBuf, entries: &[serde_json::Value]) {
    fno_agents::graph_store::seed_rows(graph, entries).expect("seed store");
}

fn lockfile(graph: &PathBuf, node_id: &str) -> Option<String> {
    let path = graph
        .parent()
        .unwrap()
        .join("claims-root")
        .join(".fno")
        .join("claims")
        .join(format!("node%3A{node_id}.lock"));
    std::fs::read_to_string(path).ok()
}

#[test]
fn open_writes_the_two_hour_lease_naming_the_passed_identity() {
    let (_dir, config, graph) = sandbox();
    seed(
        &graph,
        &[serde_json::json!({
            "id": "x-aaaa1111", "title": "n", "status": "design",
            "project": "fno", "slug": "n",
        })],
    );
    let (code, stdout, stderr) = session(
        &config,
        &graph,
        &["open", "x-aaaa1111", "--json"],
        &[],
        false,
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    let receipt: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(receipt["status"], "opened");
    assert_eq!(receipt["holder"], "blueprint-session:sess-golden-0001");
    let raw = lockfile(&graph, "x-aaaa1111").expect("claim lockfile");
    let rec: serde_json::Value = serde_yaml_ng::from_str(&raw).unwrap();
    assert_eq!(rec["holder"], "blueprint-session:sess-golden-0001");
    assert_eq!(rec["harness"], "claude");
    assert_eq!(rec["session_id"], "sess-golden-0001");
    // Either the durable session pid was proven (liveness pid arm) or the
    // lease records pid_unavailable and lives by its TTL. A transient pid
    // is the one forbidden answer.
    let acquired = rec["acquired_at"].as_i64().unwrap();
    assert_eq!(rec["expires_at"].as_i64().unwrap(), acquired + 7_200_000);
    if rec["pid_unavailable"] == true {
        assert!(rec["pid"].is_null(), "{}", raw);
    } else {
        assert!(rec["pid"].is_i64(), "{}", raw);
    }
}

#[test]
fn a_second_open_by_the_same_session_refuses_and_keeps_the_acquire_time() {
    let (_dir, config, graph) = sandbox();
    seed(
        &graph,
        &[serde_json::json!({
            "id": "x-bbbb2222", "title": "n", "status": "design",
            "project": "fno", "slug": "n",
        })],
    );
    let first = session(
        &config,
        &graph,
        &["open", "x-bbbb2222", "--json"],
        &[],
        false,
    );
    assert_eq!(first.0, 0, "{}{}", first.1, first.2);
    let before = lockfile(&graph, "x-bbbb2222").unwrap();
    let r = session(&config, &graph, &["open", "x-bbbb2222"], &[], false);
    assert_eq!(r.0, 1, "{}{}", r.1, r.2);
    assert!(r.2.contains("is already open for this session"), "{}", r.2);
    let after = lockfile(&graph, "x-bbbb2222").unwrap();
    let t =
        |s: &str| serde_yaml_ng::from_str::<serde_json::Value>(s).unwrap()["acquired_at"].clone();
    assert_eq!(t(&before), t(&after));
}

#[test]
fn open_joins_the_spawn_handover_claim_this_session_own() {
    let (_dir, config, graph) = sandbox();
    seed(
        &graph,
        &[serde_json::json!({
            "id": "x-cccc3333", "title": "n", "status": "design",
            "project": "fno", "slug": "n",
        })],
    );
    let root = graph.parent().unwrap().join("claims-root");
    let outcome = fno_agents::claims::acquire(
        "node:x-cccc3333",
        "spawn-handover:worker-a",
        fno_agents::claims::AcquireOpts {
            ttl_ms: Some(60_000),
            root: Some(root),
            ..Default::default()
        },
    );
    assert!(matches!(
        outcome,
        fno_agents::claims::AcquireOutcome::Acquired(_)
    ));
    let (code, stdout, stderr) = session(
        &config,
        &graph,
        &["open", "x-cccc3333", "--json"],
        &[("FNO_NODE_CLAIM_HOLDER", "spawn-handover:worker-a")],
        false,
    );
    assert_eq!(code, 0, "{}{}", stdout, stderr);
    let receipt: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(receipt["status"], "joined");
    assert_eq!(receipt["holder"], "spawn-handover:worker-a");
}

#[test]
fn open_refuses_a_live_foreign_holder_and_leaves_it_intact() {
    let (_dir, config, graph) = sandbox();
    seed(
        &graph,
        &[serde_json::json!({
            "id": "x-dddd4444", "title": "n", "status": "design",
            "project": "fno", "slug": "n",
        })],
    );
    let root = graph.parent().unwrap().join("claims-root");
    let outcome = fno_agents::claims::acquire(
        "node:x-dddd4444",
        "spawn-handover:worker-a",
        fno_agents::claims::AcquireOpts {
            ttl_ms: Some(60_000),
            root: Some(root),
            ..Default::default()
        },
    );
    assert!(matches!(
        outcome,
        fno_agents::claims::AcquireOutcome::Acquired(_)
    ));
    let (code, stdout, stderr) = session(&config, &graph, &["open", "x-dddd4444"], &[], false);
    assert_eq!(code, 1, "{}{}", stdout, stderr);
    assert!(
        stderr.contains("held by spawn-handover:worker-a"),
        "{}",
        stderr
    );
    assert!(stderr.contains("no planner started"), "{}", stderr);
    assert!(
        lockfile(&graph, "x-dddd4444").is_some(),
        "the foreign claim must stay"
    );
}

#[test]
fn open_without_any_identity_refuses_and_claims_nothing() {
    let (_dir, config, graph) = sandbox();
    seed(
        &graph,
        &[serde_json::json!({
            "id": "x-eeee5555", "title": "n", "status": "design",
            "project": "fno", "slug": "n",
        })],
    );
    let (code, stdout, stderr) = session(&config, &graph, &["open", "x-eeee5555"], &[], true);
    assert_eq!(code, 2, "{}{}", stdout, stderr);
    assert!(stderr.contains("no ambient identity"), "{}", stderr);
    assert!(lockfile(&graph, "x-eeee5555").is_none());
}

#[test]
fn close_releases_the_blueprint_holder_and_stamps_the_row() {
    let (_dir, config, graph) = sandbox();
    seed(
        &graph,
        &[serde_json::json!({
            "id": "x-ffff6666", "title": "n", "status": "design",
            "project": "fno", "slug": "n",
        })],
    );
    let opened = session(
        &config,
        &graph,
        &["open", "x-ffff6666", "--json"],
        &[],
        false,
    );
    assert_eq!(opened.0, 0, "{}{}", opened.1, opened.2);
    let receipt: serde_json::Value = serde_json::from_str(opened.1.trim()).unwrap();
    let acquired = receipt["acquired_at"].as_i64().unwrap();
    let (code, stdout, stderr) = session(
        &config,
        &graph,
        &[
            "close",
            "x-ffff6666",
            "--summary",
            "plan ready",
            "--launch",
            "claude /fno:target x-ffff6666",
        ],
        &[],
        false,
    );
    assert_eq!(code, 0, "{}{}", stdout, stderr);
    assert!(
        lockfile(&graph, "x-ffff6666").is_none(),
        "the close releases"
    );
    let rows = fno_agents::graph_store::read_rows(&graph).unwrap();
    let sessions = rows[0]["sessions"].as_array().cloned().unwrap_or_default();
    let blueprint: Vec<&serde_json::Value> = sessions
        .iter()
        .filter(|r| r["phase"] == "blueprint" && r["session_id"] == "sess-golden-0001")
        .collect();
    assert_eq!(
        blueprint.len(),
        1,
        "{}",
        serde_json::to_string(&sessions).unwrap()
    );
    assert!(blueprint[0]["ended_at"].is_string());
    // The claim's acquire time bounds the planning window.
    let started = blueprint[0]["started_at"].as_str().unwrap();
    let parsed = chrono::DateTime::parse_from_rfc3339(&started.replace("Z", "+00:00")).unwrap();
    // The row stamp is second-resolution; the claim's ms acquire time truncates.
    assert_eq!(parsed.timestamp_millis(), (acquired / 1000) * 1000);
}

#[test]
fn reap_open_closes_the_named_node_and_reads_it_back() {
    let (_dir, config, graph) = sandbox();
    seed(
        &graph,
        &[serde_json::json!({
            "id": "x-reap0001", "title": "Reap me", "status": "idea",
            "sessions": [{
                "phase": "execute", "harness": "codex",
                "session_id": "dead-session", "started_at": "2026-08-20T00:00:00Z",
            }],
        })],
    );
    let (code, stdout, stderr) = session(
        &config,
        &graph,
        &[
            "reap-open",
            "x-reap0001",
            "--harness",
            "codex",
            "--session-id",
            "dead-session",
            "--json",
        ],
        &[],
        false,
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    let receipt: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(receipt["settled"], true);
    assert_eq!(receipt["row_removed"], false);
    assert_eq!(receipt["row_closed"], true);
    assert_eq!(receipt["status_after"], "idea");
    assert_eq!(receipt["remaining_open_do"], 0);
    let rows = fno_agents::graph_store::read_rows(&graph).unwrap();
    assert!(rows[0]["sessions"][0]["ended_at"].is_string());
}

#[test]
fn reap_open_without_node_settles_every_node_holding_the_identity() {
    let (_dir, config, graph) = sandbox();
    seed(
        &graph,
        &[
            serde_json::json!({
                "id": "x-reap0002", "title": "First holder",
                "sessions": [{
                    "phase": "ship", "harness": "codex",
                    "session_id": "dead-session", "started_at": "2026-08-20T00:00:00Z",
                }],
            }),
            serde_json::json!({
                "id": "x-reap0003", "title": "Second holder",
                "sessions": [{
                    "phase": "review", "harness": "codex",
                    "session_id": "dead-session", "started_at": "2026-08-20T00:00:00Z",
                }],
            }),
            serde_json::json!({
                "id": "x-reap0004", "title": "Other session",
                "sessions": [{
                    "phase": "ship", "harness": "codex",
                    "session_id": "alive-session", "started_at": "2026-08-20T00:00:00Z",
                }],
            }),
        ],
    );
    let (code, stdout, stderr) = session(
        &config,
        &graph,
        &[
            "reap-open",
            "--harness",
            "codex",
            "--session-id",
            "dead-session",
            "--phase",
            "all",
            "--json",
        ],
        &[],
        false,
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    let receipt: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(receipt["settled"], true);
    assert_eq!(
        receipt["node_ids"],
        serde_json::json!(["x-reap0002", "x-reap0003"])
    );
    assert_eq!(receipt["row_closed"], true);
    let rows = fno_agents::graph_store::read_rows(&graph).unwrap();
    for node_id in ["x-reap0002", "x-reap0003"] {
        let node = rows.iter().find(|row| row["id"] == node_id).unwrap();
        assert!(node["sessions"][0]["ended_at"].is_string());
    }
    let untouched = rows.iter().find(|row| row["id"] == "x-reap0004").unwrap();
    assert!(untouched["sessions"][0]["ended_at"].is_null());
}
