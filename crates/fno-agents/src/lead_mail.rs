//! The shared in-process lead-mail send: one durable bus envelope per live
//! team holder, appended under the same sidecar flock the Python appender
//! holds - no `fno` child. The daemon's mail-wake arm delivers undrained
//! durable mail exactly as it always has, so a caller loses its seam
//! crossing without losing the mail.
//!
//! Envelope key order mirrors `bus/log.py::to_json_line`; the durable shape
//! is the no-`delivery` row Python's `is_deliverable` reads as pending.
//! Address resolution mirrors `team.py::resolve_to_lead`: live rows whose
//! team answers the scope, one row per holder, read at send time.

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::Path;

/// The sender stamp `fno agents mail send --from-name` used to carry on this
/// lane; the settle arm's rows stay attributable after the shell-out goes.
pub const SETTLE_SENDER: &str = "fno/lead-settle";

/// Live team holders for `scope`, by row name, sorted and deduped - the
/// port of `resolve_to_lead` over the JSON rows: `role_level` present, the
/// held scope answers, status not terminal.
fn lead_holders(
    registry: &[Value],
    scope: &str,
    projects: Option<&HashMap<String, String>>,
) -> Vec<String> {
    let mut names: Vec<String> = registry
        .iter()
        .filter(|row| !crate::row_verdict::finished_json(row))
        .filter(|row| row.get("role_level").map(|c| !c.is_null()).unwrap_or(false))
        .filter(|row| {
            crate::announce::team_answers(
                crate::announce::row_str(row, "role_scope"),
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
fn append_mail(
    bus_live: &Path,
    chats_dir: &Path,
    sender: &str,
    holder: &str,
    text: &str,
) -> Result<(), String> {
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
    crate::announce::append_line(bus_live, &Value::Object(obj), chats_dir)
}

/// Send one lead mail in process: resolve the scope's live holders and
/// append one durable envelope each. A vacant or split team refuses, the
/// same rule `--to-lead` applied, so a send names why nothing moved.
/// Answers the holder names mailed.
pub fn send_at(
    registry: &Path,
    bus_live: &Path,
    chats_dir: &Path,
    sender: &str,
    scope: &str,
    text: &str,
) -> Result<Vec<String>, String> {
    let rows = crate::client_verbs::load_registry_entries(registry)
        .map_err(|e| format!("lead mail: {e}"))?;
    let projects =
        crate::org_board::scope::project_map(&std::env::current_dir().unwrap_or_default()).ok();
    let holders = lead_holders(&rows, scope, projects.as_ref());
    match holders.len() {
        0 => Err(format!("lead mail: no live team answers {scope:?}")),
        1 => {
            append_mail(bus_live, chats_dir, sender, &holders[0], text)?;
            Ok(holders)
        }
        n => Err(format!(
            "lead mail: {scope:?} is a split team: {n} live rows hold it; no single owner to address"
        )),
    }
}

/// [`send_at`] with the ambient agents home. `None` under a test process
/// with no declared home - in-process callers read the send as failed
/// instead of panicking on the fence.
pub fn send(sender: &str, scope: &str, text: &str) -> Result<Vec<String>, String> {
    let Some(home) = crate::paths::AgentsHome::from_env_opt() else {
        return Err("lead mail: no agents home; nothing is addressable".to_string());
    };
    let dot_fno = home
        .root()
        .parent()
        .unwrap_or_else(|| home.root())
        .to_path_buf();
    send_at(
        &home.registry_json(),
        &dot_fno.join("bus").join("messages.jsonl"),
        &crate::chats::chats_dir(),
        sender,
        scope,
        text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    /// The chats index resolves from the env, not from the passed dirs, so
    /// an unpinned test opens the shared runner chats.db and races its
    /// siblings on it. Fields drop in order: the env comes back before the
    /// lock is released.
    struct Pinned {
        _home: crate::claims::EnvVarGuard,
        _state: crate::claims::EnvVarGuard,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn pin_home(tmp: &Path) -> Pinned {
        let lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dot_fno = tmp.join(".fno");
        let home = dot_fno.join("agents");
        Pinned {
            _home: crate::claims::EnvVarGuard::set("FNO_AGENTS_HOME", &home.to_string_lossy()),
            // The index and the bus the chats import reads both resolve
            // through the state dir, so it is pinned beside the home.
            _state: crate::claims::EnvVarGuard::set("FNO_STATE_DIR", &dot_fno.to_string_lossy()),
            _lock: lock,
        }
    }

    fn registry_path(tmp: &Path) -> PathBuf {
        tmp.join("registry.json")
    }

    fn bus_path(tmp: &Path) -> PathBuf {
        tmp.join("bus").join("messages.jsonl")
    }

    fn chats_dir(tmp: &Path) -> PathBuf {
        tmp.join("chats")
    }

    fn write_registry(tmp: &Path, rows: Value) {
        let doc = json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "agents": rows,
        });
        crate::registry_store::seed_raw(&registry_path(tmp), doc.to_string());
    }

    fn team_row(name: &str, scope: &str, level: u8, status: &str) -> Value {
        json!({
            "name": name, "status": status, "role_scope": scope,
            "role_level": level, "cwd": "/repo", "harness": "claude",
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
        let _pin = pin_home(tmp.path());
        write_registry(
            tmp.path(),
            json!([
                team_row("lead-a", "x-aaaa", 2, "live"),
                worker_row("worker-1")
            ]),
        );
        let mailed = send_at(
            &registry_path(tmp.path()),
            &bus_path(tmp.path()),
            &chats_dir(tmp.path()),
            SETTLE_SENDER,
            "x-aaaa",
            "PR #7 on x-1 settled green at abc123.",
        )
        .unwrap();
        assert_eq!(mailed, ["lead-a"]);
        let rows = read_bus(tmp.path());
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row["from"], json!(SETTLE_SENDER));
        assert_eq!(row["to"], json!("lead-a"));
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
        // The chat mirror follows the passed dir, never the env store:
        // a fixture send records beside the fixture bus.
        let chat_dirs = std::fs::read_dir(chats_dir(tmp.path()))
            .expect("chats dir created beside the fixture bus")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("chat-"))
            .count();
        assert!(chat_dirs >= 1, "mirror landed in the passed chats dir");
        assert!(
            tmp.path().join(".fno/db/chats.db").exists(),
            "the index lives in the test's own home, not the runner's"
        );
    }

    #[test]
    fn a_vacant_team_refuses_and_writes_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _pin = pin_home(tmp.path());
        write_registry(tmp.path(), json!([worker_row("worker-1")]));
        let err = send_at(
            &registry_path(tmp.path()),
            &bus_path(tmp.path()),
            &chats_dir(tmp.path()),
            SETTLE_SENDER,
            "x-bbbb",
            "text",
        )
        .unwrap_err();
        assert!(err.contains("no live team answers"), "{err}");
        assert!(!bus_path(tmp.path()).exists());
    }

    #[test]
    fn a_split_team_refuses_instead_of_mailing_two_holders() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _pin = pin_home(tmp.path());
        write_registry(
            tmp.path(),
            json!([
                team_row("lead-a", "x-cccc", 2, "live"),
                team_row("lead-b", "x-cccc", 2, "live"),
            ]),
        );
        let err = send_at(
            &registry_path(tmp.path()),
            &bus_path(tmp.path()),
            &chats_dir(tmp.path()),
            SETTLE_SENDER,
            "x-cccc",
            "text",
        )
        .unwrap_err();
        assert!(err.contains("split team"), "{err}");
        assert!(!bus_path(tmp.path()).exists());
    }

    #[test]
    fn terminal_and_unpromoted_rows_never_address() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _pin = pin_home(tmp.path());
        write_registry(
            tmp.path(),
            json!([
                team_row("lead-a", "x-aaaa", 2, "live"),
                team_row("dead", "x-aaaa", 2, "exited"),
                worker_row("worker-1"),
            ]),
        );
        let mailed = send_at(
            &registry_path(tmp.path()),
            &bus_path(tmp.path()),
            &chats_dir(tmp.path()),
            SETTLE_SENDER,
            "x-aaaa",
            "text",
        )
        .unwrap();
        assert_eq!(mailed, ["lead-a"]);
        assert_eq!(read_bus(tmp.path()).len(), 1);

        // The reversible word keys on proof: an Orphaned holder with a
        // reaped pid never addresses, one with a live pid still answers
        // its own scope.
        let mut quiet_dead = team_row("quiet-dead", "x-dddd", 2, "orphaned");
        quiet_dead["pid"] = json!(crate::row_verdict::reaped_pid());
        let mut quiet_live = team_row("quiet-live", "x-eeee", 2, "orphaned");
        quiet_live["pid"] = json!(std::process::id());
        write_registry(tmp.path(), json!([quiet_dead, quiet_live]));
        let err = send_at(
            &registry_path(tmp.path()),
            &bus_path(tmp.path()),
            &chats_dir(tmp.path()),
            SETTLE_SENDER,
            "x-dddd",
            "text",
        )
        .unwrap_err();
        assert!(err.contains("no live team answers"), "{err}");
        let mailed = send_at(
            &registry_path(tmp.path()),
            &bus_path(tmp.path()),
            &chats_dir(tmp.path()),
            SETTLE_SENDER,
            "x-eeee",
            "text",
        )
        .unwrap();
        assert_eq!(mailed, ["quiet-live"]);
    }
}
