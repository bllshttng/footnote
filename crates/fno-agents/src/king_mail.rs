//! The shared in-process king-mail send: one durable bus envelope per live
//! crown holder, appended under the same sidecar flock the Python appender
//! holds - no `fno` child. The daemon's mail-wake arm delivers undrained
//! durable mail exactly as it always has, so a caller loses its seam
//! crossing without losing the mail.
//!
//! Envelope key order mirrors `bus/log.py::to_json_line`; the durable shape
//! is the no-`delivery` row Python's `is_deliverable` reads as pending.
//! Address resolution mirrors `crown.py::resolve_to_king`: live rows whose
//! crown answers the scope, one row per holder, read at send time.

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The sender stamp `fno agents mail send --from-name` used to carry on this
/// lane; the settle arm's rows stay attributable after the shell-out goes.
pub const SETTLE_SENDER: &str = "king-settle";

/// Live crown holders for `scope`, by row name, sorted and deduped - the
/// port of `resolve_to_king` over the JSON rows: `crown_level` present, the
/// held scope answers, status not terminal.
fn king_holders(
    registry: &[Value],
    scope: &str,
    projects: Option<&HashMap<String, String>>,
) -> Vec<String> {
    let mut names: Vec<String> = registry
        .iter()
        .filter(|row| !crate::announce::row_terminal(row))
        .filter(|row| {
            row.get("crown_level")
                .map(|c| !c.is_null())
                .unwrap_or(false)
        })
        .filter(|row| {
            crate::announce::crown_answers(
                crate::announce::row_str(row, "crown_scope"),
                scope,
                projects,
            )
        })
        .filter_map(|row| crate::announce::row_str(row, "name").map(str::to_string))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Append one durable envelope under the bus sidecar flock. Body rides raw:
/// the bus is the delivery, and a fleet-plain row renders in every drain.
fn append_mail(bus_live: &Path, sender: &str, holder: &str, text: &str) -> Result<(), String> {
    let id = crate::announce::new_msg_id();
    let word_count = text.split_whitespace().count() as i64;
    let mut obj = Map::new();
    obj.insert("v".into(), json!(1));
    obj.insert("id".into(), json!(&id));
    obj.insert("ts".into(), json!(crate::announce::now_iso()));
    obj.insert("thread".into(), json!(&id));
    obj.insert("from".into(), json!(sender));
    obj.insert("to".into(), json!(holder));
    obj.insert("kind".into(), json!("send"));
    obj.insert("to_kind".into(), json!("name"));
    obj.insert("word_count".into(), json!(word_count));
    obj.insert("body".into(), json!(text));
    crate::announce::append_line(bus_live, &Value::Object(obj))
}

/// Send one king mail in process: resolve the scope's live holders and
/// append one durable envelope each. A vacant or split crown refuses, the
/// same rule `--to-king` applied, so a send names why nothing moved.
/// Answers the holder names mailed.
pub fn send_at(
    registry: &Path,
    bus_live: &Path,
    sender: &str,
    scope: &str,
    text: &str,
) -> Result<Vec<String>, String> {
    let rows = crate::client_verbs::load_registry_entries(registry)
        .map_err(|e| format!("king mail: {e}"))?;
    let projects =
        crate::king_board::scope::project_map(&std::env::current_dir().unwrap_or_default()).ok();
    let holders = king_holders(&rows, scope, projects.as_ref());
    match holders.len() {
        0 => Err(format!("king mail: no live crown answers {scope:?}")),
        1 => {
            append_mail(bus_live, sender, &holders[0], text)?;
            Ok(holders)
        }
        n => Err(format!(
            "king mail: {scope:?} is a split crown: {n} live rows hold it; no single owner to address"
        )),
    }
}

/// [`send_at`] with the ambient agents home. `None` under a test process
/// with no declared home - in-process callers read the send as failed
/// instead of panicking on the fence.
pub fn send(sender: &str, scope: &str, text: &str) -> Result<Vec<String>, String> {
    let Some(home) = crate::paths::AgentsHome::from_env_opt() else {
        return Err("king mail: no agents home; nothing is addressable".to_string());
    };
    let dot_fno = home
        .root()
        .parent()
        .unwrap_or_else(|| home.root())
        .to_path_buf();
    send_at(
        &home.registry_json(),
        &dot_fno.join("bus").join("messages.jsonl"),
        sender,
        scope,
        text,
    )
}

/// The bus path `send_at` is pointed at in production, for tests that want
/// to seed and then read the real location.
pub fn bus_live_path() -> Option<PathBuf> {
    let home = crate::paths::AgentsHome::from_env_opt()?;
    let dot_fno = home
        .root()
        .parent()
        .unwrap_or_else(|| home.root())
        .to_path_buf();
    Some(dot_fno.join("bus").join("messages.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn registry_path(tmp: &Path) -> PathBuf {
        tmp.join("registry.json")
    }

    fn bus_path(tmp: &Path) -> PathBuf {
        tmp.join("bus").join("messages.jsonl")
    }

    fn write_registry(tmp: &Path, rows: Value) {
        let doc = json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "agents": rows,
        });
        std::fs::write(registry_path(tmp), doc.to_string()).unwrap();
    }

    fn crown_row(name: &str, scope: &str, level: u8, status: &str) -> Value {
        json!({
            "name": name, "status": status, "crown_scope": scope,
            "crown_level": level, "cwd": "/repo", "harness": "claude",
            "harness_session_id": format!("sess-{name}"),
            "log_path": format!("/tmp/{name}.log"),
            "created_at": "2026-09-25T12:00:00Z",
        })
    }

    fn worker_row(name: &str) -> Value {
        json!({
            "name": name, "status": "live", "cwd": "/repo", "harness": "claude",
            "harness_session_id": format!("sess-{name}"),
            "log_path": format!("/tmp/{name}.log"),
            "created_at": "2026-09-25T12:00:00Z",
        })
    }

    fn read_bus(tmp: &Path) -> Vec<Value> {
        std::fs::read_to_string(bus_path(tmp))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn one_live_holder_gets_one_durable_envelope_addressed_to_its_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(
            tmp.path(),
            json!([
                crown_row("king-a", "x-aaaa", 2, "live"),
                worker_row("worker-1")
            ]),
        );
        let mailed = send_at(
            &registry_path(tmp.path()),
            &bus_path(tmp.path()),
            SETTLE_SENDER,
            "x-aaaa",
            "PR #7 on x-1 settled green at abc123.",
        )
        .unwrap();
        assert_eq!(mailed, ["king-a"]);
        let rows = read_bus(tmp.path());
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row["from"], json!(SETTLE_SENDER));
        assert_eq!(row["to"], json!("king-a"));
        assert_eq!(row["kind"], json!("send"));
        assert_eq!(row["to_kind"], json!("name"));
        assert_eq!(row["word_count"], json!(8));
        // The durable shape: no delivery field is the pending row the
        // Python reader (is_deliverable) drains.
        assert!(row.get("delivery").is_none());
        assert_eq!(row["body"], json!("PR #7 on x-1 settled green at abc123."));
        for key in ["v", "id", "ts", "thread"] {
            assert!(row.get(key).is_some(), "{key} missing");
        }
    }

    #[test]
    fn a_vacant_crown_refuses_and_writes_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(tmp.path(), json!([worker_row("worker-1")]));
        let err = send_at(
            &registry_path(tmp.path()),
            &bus_path(tmp.path()),
            SETTLE_SENDER,
            "x-bbbb",
            "text",
        )
        .unwrap_err();
        assert!(err.contains("no live crown answers"), "{err}");
        assert!(!bus_path(tmp.path()).exists());
    }

    #[test]
    fn a_split_crown_refuses_instead_of_mailing_two_holders() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(
            tmp.path(),
            json!([
                crown_row("king-a", "x-cccc", 2, "live"),
                crown_row("king-b", "x-cccc", 2, "live"),
            ]),
        );
        let err = send_at(
            &registry_path(tmp.path()),
            &bus_path(tmp.path()),
            SETTLE_SENDER,
            "x-cccc",
            "text",
        )
        .unwrap_err();
        assert!(err.contains("split crown"), "{err}");
        assert!(!bus_path(tmp.path()).exists());
    }

    #[test]
    fn terminal_and_uncrowned_rows_never_address() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_registry(
            tmp.path(),
            json!([
                crown_row("king-a", "x-aaaa", 2, "live"),
                crown_row("dead", "x-aaaa", 2, "exited"),
                worker_row("worker-1"),
            ]),
        );
        let mailed = send_at(
            &registry_path(tmp.path()),
            &bus_path(tmp.path()),
            SETTLE_SENDER,
            "x-aaaa",
            "text",
        )
        .unwrap();
        assert_eq!(mailed, ["king-a"]);
        assert_eq!(read_bus(tmp.path()).len(), 1);
    }
}
