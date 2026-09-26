//! The territory-cap tests: live and suspect node claims count toward the
//! cap without registry node fields, and an unreadable claim store refuses
//! as unknown. Split from spawn_gate.rs to keep the over-budget file shrinking.

use super::tests::EnvPin;
use super::*;

#[test]
fn territory_cap_counts_live_and_suspect_node_claims_without_registry_node_fields() {
    let _g = claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = EnvPin::take(&["FNO_HOME", "FNO_CLAIMS_ROOT"]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".fno")).unwrap();
    std::fs::write(dir.path().join(".fno/config.toml"), "schema_version = 1\n").unwrap();
    // Seed through the store: a bare graph.json is an un-imported seed the
    // strict read refuses to retire, never a row source.
    let entries: Vec<Value> = ["x-1", "x-2", "x-3"]
        .iter()
        .map(|id| {
            serde_json::json!({"id": id, "slug": id, "title": id, "type": "feature",
                "status": "idea", "priority": "p2", "project": "proj", "domain": "code",
                "created_at": "2026-09-07T00:00:00Z"})
        })
        .collect();
    crate::graph_store::seed_rows(&dir.path().join("graph.json"), &entries).unwrap();
    std::env::set_var("FNO_HOME", dir.path());
    std::env::set_var("FNO_CLAIMS_ROOT", dir.path());
    let reg = dir.path().join("registry.json");
    std::fs::write(
        &reg,
        serde_json::json!({"schema_version": crate::state::REGISTRY_SCHEMA_VERSION, "agents": [
            {"name": "w-x-1", "status": "busy", "node": null,
             "pid": std::process::id(), "cwd": "/tmp",
             "created_at": "2026-09-07T00:00:00Z", "log_path": ""},
            {"name": "w-x-2", "status": "busy", "node": null,
             "pid": std::process::id(), "cwd": "/tmp",
             "created_at": "2026-09-07T00:00:00Z", "log_path": ""}
        ]})
        .to_string(),
    )
    .unwrap();
    write_live_node_claim(dir.path(), "x-1");
    write_suspect_node_claim(dir.path(), "x-2");

    let err = check_territory_cap(dir.path(), &reg, "x-3", 2).unwrap_err();
    let parsed: serde_json::Value = serde_json::from_str(&err).unwrap();
    assert_eq!(parsed["reason"], serde_json::json!("territory_cap"));
    assert_eq!(parsed["count"], 2);
}

#[test]
fn territory_cap_refuses_unreadable_claim_store_as_unknown() {
    let _g = claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = EnvPin::take(&["FNO_HOME", "FNO_CLAIMS_ROOT"]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".fno")).unwrap();
    std::fs::write(dir.path().join(".fno/config.toml"), "schema_version = 1\n").unwrap();
    std::fs::write(dir.path().join(".fno/claims"), "not a directory").unwrap();
    std::fs::write(
        dir.path().join("graph.json"),
        serde_json::json!({"entries": [{"id": "x-1", "project": "proj", "status": "idea"}]})
            .to_string(),
    )
    .unwrap();
    std::env::set_var("FNO_HOME", dir.path());
    std::env::set_var("FNO_CLAIMS_ROOT", dir.path());
    let reg = dir.path().join("registry.json");
    std::fs::write(&reg, r#"{"schema_version":1,"entries":[]}"#).unwrap();

    let err = check_territory_cap(dir.path(), &reg, "x-1", 1).unwrap_err();
    let parsed: serde_json::Value = serde_json::from_str(&err).unwrap();
    assert_eq!(parsed["reason"], serde_json::json!("territory_unknown"));
}

fn write_live_node_claim(root: &Path, node: &str) {
    let outcome = claims::acquire(
        &format!("node:{node}"),
        "target-session:territory-test",
        claims::AcquireOpts {
            pid: Some(std::process::id()),
            root: Some(root.to_path_buf()),
            events_dir: Some(root.to_path_buf()),
            ..Default::default()
        },
    );
    assert!(
        matches!(outcome, claims::AcquireOutcome::Acquired(_)),
        "{outcome:?}"
    );
}

fn write_suspect_node_claim(root: &Path, node: &str) {
    let outcome = claims::acquire(
        &format!("node:{node}"),
        "target-session:territory-test",
        claims::AcquireOpts {
            pid_unavailable: true,
            ttl_ms: Some(60_000),
            root: Some(root.to_path_buf()),
            events_dir: Some(root.to_path_buf()),
            ..Default::default()
        },
    );
    assert!(
        matches!(outcome, claims::AcquireOutcome::Acquired(_)),
        "{outcome:?}"
    );
}
