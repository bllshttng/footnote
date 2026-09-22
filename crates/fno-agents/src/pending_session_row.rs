//! `pending-session-row`: the deferred sessions-row park and open.
//!
//! A spawn whose harness session id does not exist yet cannot open the
//! node's `sessions` row; the owed payload (`phase`, plus `merge_grant`
//! when the phase is `do`) parks on the worker's registry row here, and
//! SessionStart's first id observation opens the row through the same
//! idempotent append the claim path leans on, then clears the field. Both
//! halves live in this crate because both are registry mutations, and the
//! typed `RegistryEntry` is the writer that must not silently drop the
//! field (the `node`/`substrate` lesson). Transport-only, like
//! `launch-workdir`: registers no client action (the shrink law allows
//! none); Python reaches it through `verb_call`. One JSON payload in on
//! stdin, one JSON answer out - the route-slot transport shape.

use crate::backlog::api::{self, Store};
use crate::graph_get::default_graph_path;
use crate::paths::AgentsHome;
use crate::state::update_registry;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;

/// The session phases a parked row may name, mirroring Python store's
/// `_SESSION_PHASES`. A phase out of this vocabulary is a producer bug and
/// refuses rather than stamping a row no reader understands.
const PHASES: &[&str] = &["think", "blueprint", "do", "review", "ship"];

/// The transport arm. Reads the JSON payload on stdin, dispatches to
/// [`park`]/[`open`], prints the JSON answer, and maps the result to an
/// exit code: 0 answered, 1 failed, 2 misuse.
pub fn run(rest: &[String]) -> i32 {
    if let Some(arg) = rest.first() {
        eprintln!("fno-agents: pending-session-row takes no arguments (got: {arg})");
        return 2;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("pending-session-row: stdin read failed");
        return 2;
    }
    let payload: Value = match serde_json::from_str(&input) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("pending-session-row: bad payload: {e}");
            return 2;
        }
    };
    let registry = AgentsHome::from_env().registry_json();
    match payload.get("action").and_then(Value::as_str) {
        Some("park") => match park(&registry, &payload) {
            Ok(answer) => {
                println!("{answer}");
                0
            }
            Err(e) => {
                eprintln!("pending-session-row: park failed: {e}");
                1
            }
        },
        Some("open") => match open(&registry, &payload) {
            Ok(answer) => {
                println!("{answer}");
                0
            }
            Err(e) => {
                eprintln!("pending-session-row: open failed: {e}");
                1
            }
        },
        other => {
            eprintln!("pending-session-row: unknown action {other:?}");
            2
        }
    }
}

/// Park the owed payload on the named worker's registry row. Answers
/// `{"parked": bool}`; first park wins, so a retried spawn never rewrites
/// the payload a live worker's SessionStart is about to consume.
fn park(registry: &Path, payload: &Value) -> Result<Value, String> {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .ok_or("park payload carries no name")?;
    let phase = payload
        .get("phase")
        .and_then(Value::as_str)
        .ok_or("park payload carries no phase")?;
    if !PHASES.contains(&phase) {
        return Err(format!("park phase {phase:?} is not in the vocabulary"));
    }
    let grant = payload.get("merge_grant").cloned().filter(|v| !v.is_null());
    let name = name.to_string();
    let parked = update_registry(registry, |reg| {
        for entry in reg.entries.iter_mut() {
            if entry.name != name {
                continue;
            }
            if entry.pending_session_row.is_none() {
                entry.pending_session_row = Some(json!({ "phase": phase, "merge_grant": grant }));
                return true;
            }
            return false;
        }
        false
    })
    .map_err(|e| e.to_string())?;
    Ok(json!({ "parked": parked }))
}

/// Open the parked row. Reads the row's provenance (`node`, `effort`,
/// `harness`, the parked payload), appends the node's sessions row through
/// the idempotent [`api::session_open_parked`], and clears the park - in
/// that order, so a failed graph write keeps the payload. A row with
/// nothing parked, or a node the graph does not carry, answers and moves
/// on: a provenance miss is never an error.
fn open(registry: &Path, payload: &Value) -> Result<Value, String> {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .ok_or("open payload carries no name")?
        .to_string();
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("open payload carries no session_id")?;
    let rows = crate::client_verbs::load_registry_entries(registry)
        .map_err(|e| format!("registry load failed: {e}"))?;
    let Some(row) = rows
        .iter()
        .find(|r| r.get("name").and_then(Value::as_str) == Some(name.as_str()))
    else {
        return Ok(json!({ "opened": false, "reason": "no row" }));
    };
    let Some(parked) = row
        .get("pending_session_row")
        .cloned()
        .filter(|v| v.is_object())
    else {
        return Ok(json!({ "opened": false, "reason": "nothing parked" }));
    };
    let Some(node) = row
        .get("node")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        // No node on the row: the payload can never open. Drop the corpse.
        clear_park(registry, &name).map_err(|e| e.to_string())?;
        return Ok(json!({ "opened": false, "cleared": true, "reason": "no node on row" }));
    };
    let phase = parked
        .get("phase")
        .and_then(Value::as_str)
        .ok_or("parked payload carries no phase")?;
    if !PHASES.contains(&phase) {
        return Err(format!("parked phase {phase:?} is not in the vocabulary"));
    }
    let harness = row.get("harness").and_then(Value::as_str).unwrap_or("");
    let effort = row.get("effort").and_then(Value::as_str);
    let grant = parked.get("merge_grant").cloned().filter(|v| !v.is_null());
    let started = crate::daemon::now_rfc3339_like();
    let store = Store::new(&default_graph_path());
    let found = api::session_open_parked(
        &store, node, phase, harness, session_id, effort, grant, &started,
    )
    .map_err(|e| e.0)?;
    // Clear only after the graph write answered: a node the graph does not
    // carry can never open, so the payload clears there too (found=false).
    let cleared = clear_park(registry, &name)
        .map_err(|e| e.to_string())
        .is_ok();
    Ok(json!({ "opened": found, "cleared": cleared }))
}

fn clear_park(registry: &Path, name: &str) -> Result<(), crate::state::StateError> {
    let name = name.to_string();
    update_registry(registry, |reg| {
        for entry in reg.entries.iter_mut() {
            if entry.name == name {
                entry.pending_session_row = None;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Registry;
    use std::fs;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fno-pending-row-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seed_row(path: &Path, name: &str, node: Option<&str>) {
        update_registry(path, |reg: &mut Registry| {
            reg.entries.push(crate::state::RegistryEntry {
                name: name.to_string(),
                harness: Some("claude".into()),
                node: node.map(|n| n.to_string()),
                effort: Some("xhigh".into()),
                ..Default::default()
            });
        })
        .unwrap();
    }

    #[test]
    fn park_stamps_the_row_once_and_a_second_park_keeps_the_first() {
        let dir = tmp_dir("park");
        let registry = dir.join("registry.json");
        seed_row(&registry, "w1", Some("x-1"));
        let payload = json!({"action": "park", "name": "w1", "phase": "do", "merge_grant": null});

        let first = park(&registry, &payload).unwrap();
        assert_eq!(first["parked"], json!(true));
        let rows = crate::client_verbs::load_registry_entries(&registry).unwrap();
        assert_eq!(rows[0]["pending_session_row"]["phase"], json!("do"));

        let second = park(&registry, &payload_with("review")).unwrap();
        assert_eq!(second["parked"], json!(false));
        let rows = crate::client_verbs::load_registry_entries(&registry).unwrap();
        assert_eq!(rows[0]["pending_session_row"]["phase"], json!("do"));
        let _ = fs::remove_dir_all(&dir);
    }

    fn payload_with(phase: &str) -> Value {
        json!({"action": "park", "name": "w1", "phase": phase, "merge_grant": null})
    }

    #[test]
    fn open_appends_one_row_clears_the_park_and_stays_idempotent() {
        let dir = tmp_dir("open");
        let registry = dir.join("registry.json");
        let graph = dir.join("graph.json");
        fs::write(
            &graph,
            json!({"entries": [{"id": "x-defr", "title": "t", "status": "in_progress"}]})
                .to_string(),
        )
        .unwrap();
        std::env::set_var("FNO_HOME", &dir);
        seed_row(&registry, "w1", Some("x-defr"));
        park(&registry, &payload_with("do")).unwrap();

        let answer = open(
            &registry,
            &json!({"action": "open", "name": "w1", "session_id": "sid-1"}),
        )
        .unwrap();
        assert_eq!(answer["opened"], json!(true));
        assert_eq!(answer["cleared"], json!(true));

        let body: Value = serde_json::from_str(&fs::read_to_string(&graph).unwrap()).unwrap();
        let sessions = body["entries"][0]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_id"], json!("sid-1"));
        assert_eq!(sessions[0]["phase"], json!("do"));
        assert_eq!(sessions[0]["effort"], json!("xhigh"));
        let rows = crate::client_verbs::load_registry_entries(&registry).unwrap();
        assert!(rows[0].get("pending_session_row").is_none());

        // A second observation adds no twin row.
        let answer = open(
            &registry,
            &json!({"action": "open", "name": "w1", "session_id": "sid-1"}),
        )
        .unwrap();
        assert_eq!(answer["opened"], json!(false));
        let body: Value = serde_json::from_str(&fs::read_to_string(&graph).unwrap()).unwrap();
        assert_eq!(body["entries"][0]["sessions"].as_array().unwrap().len(), 1);
        std::env::remove_var("FNO_HOME");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_with_a_claim_path_row_already_won_adds_nothing_and_clears() {
        let _lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tmp_dir("claim");
        let registry = dir.join("registry.json");
        let graph = dir.join("graph.json");
        fs::write(
            &graph,
            json!({"entries": [{
                "id": "x-clai", "title": "t", "status": "in_progress",
                "sessions": [{"phase": "do", "harness": "claude", "session_id": "sid-2",
                              "started_at": "2026-09-22T00:00:00Z"}],
            }]})
            .to_string(),
        )
        .unwrap();
        std::env::set_var("FNO_HOME", &dir);
        seed_row(&registry, "w1", Some("x-clai"));
        park(&registry, &payload_with("do")).unwrap();

        let answer = open(
            &registry,
            &json!({"action": "open", "name": "w1", "session_id": "sid-2"}),
        )
        .unwrap();
        assert_eq!(answer["opened"], json!(true));
        assert_eq!(answer["cleared"], json!(true));
        let body: Value = serde_json::from_str(&fs::read_to_string(&graph).unwrap()).unwrap();
        let sessions = body["entries"][0]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "no duplicate row");
        assert_eq!(sessions[0]["started_at"], json!("2026-09-22T00:00:00Z"));
        std::env::remove_var("FNO_HOME");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_with_a_node_absent_from_the_graph_clears_the_corpse() {
        let _lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tmp_dir("absent");
        let registry = dir.join("registry.json");
        let graph = dir.join("graph.json");
        fs::write(&graph, json!({"entries": []}).to_string()).unwrap();
        std::env::set_var("FNO_HOME", &dir);
        seed_row(&registry, "w1", Some("x-gone"));
        park(&registry, &payload_with("review")).unwrap();

        let answer = open(
            &registry,
            &json!({"action": "open", "name": "w1", "session_id": "sid-3"}),
        )
        .unwrap();
        assert_eq!(answer["opened"], json!(false));
        assert_eq!(answer["cleared"], json!(true));
        let rows = crate::client_verbs::load_registry_entries(&registry).unwrap();
        assert!(rows[0].get("pending_session_row").is_none());
        std::env::remove_var("FNO_HOME");
        let _ = fs::remove_dir_all(&dir);
    }
}
