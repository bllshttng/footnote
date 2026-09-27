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
use crate::paths::AgentsHome;
use crate::state::update_registry;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The session phases a parked row may name, mirroring Python store's
/// `_SESSION_PHASES`. A phase out of this vocabulary is a producer bug and
/// refuses rather than stamping a row no reader understands.
const PHASES: &[&str] = &["think", "blueprint", "execute", "review", "ship"];

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
    match payload.get("action").and_then(Value::as_str) {
        Some("park") => match park(&payload) {
            Ok(answer) => {
                println!("{answer}");
                0
            }
            Err(e) => {
                eprintln!("pending-session-row: park failed: {e}");
                1
            }
        },
        Some("open") => match open(&payload) {
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

/// The registry the payload names, or the ambient home when it does not.
/// Python passes `paths.agents_registry_path()` explicitly so a caller
/// pinned to a non-default state root (tests, `config.state_dir`) and the
/// binary agree - the patch door's `--graph` contract.
fn payload_registry(payload: &Value) -> PathBuf {
    match payload
        .get("registry")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(r) => PathBuf::from(r),
        None => AgentsHome::from_env().registry_json(),
    }
}

/// The graph the payload names, or the config-resolved graph when it does
/// not (the same chain Python's `paths.graph_json()` walks).
fn payload_graph(payload: &Value) -> PathBuf {
    match payload
        .get("graph")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(g) => PathBuf::from(g),
        None => crate::king_board::scope::graph_json_path(
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        ),
    }
}

/// Park the owed payload on the named worker's registry row. Answers
/// `{"parked": bool}`; first park wins, so a retried spawn never rewrites
/// the payload a live worker's SessionStart is about to consume. The row
/// stamps `started_at` here because the park IS the spawn instant - the
/// spawn-side path (spawn_lineage) stamps its open at the same moment - so
/// the deferred open never reads a later start than a spawn-side row.
fn park(payload: &Value) -> Result<Value, String> {
    let registry = payload_registry(payload);
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
    let started = crate::daemon::now_rfc3339_like();
    let name = name.to_string();
    let parked = update_registry(&registry, |reg| {
        for entry in reg.entries.iter_mut() {
            if entry.name != name {
                continue;
            }
            if entry.pending_session_row.is_none() {
                entry.pending_session_row = Some(json!({
                    "phase": phase, "merge_grant": grant, "started_at": started,
                }));
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
fn open(payload: &Value) -> Result<Value, String> {
    let registry = payload_registry(payload);
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .ok_or("open payload carries no name")?
        .to_string();
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("open payload carries no session_id")?;
    let rows = crate::client_verbs::load_registry_entries(&registry)
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
        clear_park(&registry, &name).map_err(|e| e.to_string())?;
        return Ok(json!({ "opened": false, "cleared": true, "reason": "no node on row" }));
    };
    let phase = parked
        .get("phase")
        .and_then(Value::as_str)
        .ok_or("parked payload carries no phase")?;
    // One-release input alias, matching Python's PHASE_INPUT_ALIASES: a
    // payload parked before the rename still opens. Drop the alias when the
    // release window closes.
    let phase = match phase {
        "do" => "execute",
        other => other,
    };
    if !PHASES.contains(&phase) {
        return Err(format!("parked phase {phase:?} is not in the vocabulary"));
    }
    let harness = row.get("harness").and_then(Value::as_str).unwrap_or("");
    let effort = row.get("effort").and_then(Value::as_str);
    let grant = parked.get("merge_grant").cloned().filter(|v| !v.is_null());
    // The park instant, stamped at spawn; a payload parked before this field
    // existed falls back to now rather than failing the open.
    let started = parked
        .get("started_at")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::daemon::now_rfc3339_like());
    let store = Store::new(&payload_graph(payload));
    let found = api::session_open_parked(
        &store, node, phase, harness, session_id, effort, grant, &started,
    )
    .map_err(|e| e.0)?;
    // Clear only after the graph write answered: a node the graph does not
    // carry can never open, so the payload clears there too (found=false).
    let cleared = clear_park(&registry, &name)
        .map_err(|e| e.to_string())
        .is_ok();
    Ok(json!({ "opened": found, "cleared": cleared }))
}

fn clear_park(registry: &Path, name: &str) -> Result<(), crate::state::StateError> {
    let name = name.to_string();
    update_registry(&registry, |reg| {
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
                // A parked row carries no session id yet; the pid leg is its
                // resolvable handle, exactly as a live spawn mints it.
                pid: Some(4_194_301),
                pid_start_time: Some(1_000),
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
        let payload = park_payload(&registry, "execute");

        let first = park(&payload).unwrap();
        assert_eq!(first["parked"], json!(true));
        let rows = crate::client_verbs::load_registry_entries(&registry).unwrap();
        assert_eq!(rows[0]["pending_session_row"]["phase"], json!("execute"));

        let second = park(&park_payload(&registry, "review")).unwrap();
        assert_eq!(second["parked"], json!(false));
        let rows = crate::client_verbs::load_registry_entries(&registry).unwrap();
        assert_eq!(rows[0]["pending_session_row"]["phase"], json!("execute"));
        let _ = fs::remove_dir_all(&dir);
    }

    fn park_payload(registry: &Path, phase: &str) -> Value {
        json!({
            "action": "park", "name": "w1", "phase": phase, "merge_grant": null,
            "registry": registry.to_string_lossy(),
        })
    }

    fn open_payload(registry: &Path, graph: &Path, session_id: &str) -> Value {
        json!({
            "action": "open", "name": "w1", "session_id": session_id,
            "registry": registry.to_string_lossy(),
            "graph": graph.to_string_lossy(),
        })
    }

    #[test]
    fn open_appends_one_row_clears_the_park_and_stays_idempotent() {
        let dir = tmp_dir("open");
        let registry = dir.join("registry.json");
        let graph = dir.join("graph.json");
        crate::graph_store::seed_rows(
            &graph,
            &[json!({"id": "x-defr", "title": "t", "status": "in_progress"})],
        )
        .unwrap();
        seed_row(&registry, "w1", Some("x-defr"));
        park(&park_payload(&registry, "execute")).unwrap();

        let answer = open(&open_payload(&registry, &graph, "sid-1")).unwrap();
        assert_eq!(answer["opened"], json!(true));
        assert_eq!(answer["cleared"], json!(true));

        let rows = crate::graph_store::read_rows(&graph).unwrap();
        let sessions = rows[0]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_id"], json!("sid-1"));
        assert_eq!(sessions[0]["phase"], json!("execute"));
        assert_eq!(sessions[0]["effort"], json!("xhigh"));
        // The row carries the PARK instant as its start, not the open instant.
        assert!(sessions[0]["started_at"].is_string());
        let rows = crate::client_verbs::load_registry_entries(&registry).unwrap();
        assert!(rows[0].get("pending_session_row").is_none());

        // A second observation adds no twin row.
        let answer = open(&open_payload(&registry, &graph, "sid-1")).unwrap();
        assert_eq!(answer["opened"], json!(false));
        let rows = crate::graph_store::read_rows(&graph).unwrap();
        assert_eq!(rows[0]["sessions"].as_array().unwrap().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_with_a_claim_path_row_already_won_adds_nothing_and_clears() {
        let dir = tmp_dir("claim");
        let registry = dir.join("registry.json");
        let graph = dir.join("graph.json");
        crate::graph_store::seed_rows(
            &graph,
            &[json!({
                "id": "x-clai", "title": "t", "status": "in_progress",
                "sessions": [{"phase": "execute", "harness": "claude", "session_id": "sid-2",
                              "started_at": "2026-09-22T00:00:00Z"}],
            })],
        )
        .unwrap();
        seed_row(&registry, "w1", Some("x-clai"));
        park(&park_payload(&registry, "execute")).unwrap();

        let answer = open(&open_payload(&registry, &graph, "sid-2")).unwrap();
        assert_eq!(answer["opened"], json!(true));
        assert_eq!(answer["cleared"], json!(true));
        let rows = crate::graph_store::read_rows(&graph).unwrap();
        let sessions = rows[0]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "no duplicate row");
        assert_eq!(sessions[0]["started_at"], json!("2026-09-22T00:00:00Z"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_with_a_node_absent_from_the_graph_clears_the_corpse() {
        let dir = tmp_dir("absent");
        let registry = dir.join("registry.json");
        let graph = dir.join("graph.json");
        crate::graph_store::seed_rows(&graph, &[]).unwrap();
        seed_row(&registry, "w1", Some("x-gone"));
        park(&park_payload(&registry, "review")).unwrap();

        let answer = open(&open_payload(&registry, &graph, "sid-3")).unwrap();
        assert_eq!(answer["opened"], json!(false));
        assert_eq!(answer["cleared"], json!(true));
        let rows = crate::client_verbs::load_registry_entries(&registry).unwrap();
        assert!(rows[0].get("pending_session_row").is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
