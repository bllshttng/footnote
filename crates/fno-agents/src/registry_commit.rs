//! Atomic read-forward registry write door for Python registry callers.
//!
//! Reads return a table revision. Writes compare it in an immediate transaction,
//! preserving unknown fields and refusing a stale callback result.

use crate::state::REGISTRY_SCHEMA_VERSION;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

pub fn run(args: &[String]) -> i32 {
    if let Some(arg) = args.first() {
        eprintln!("registry-commit takes no arguments (got: {arg})");
        return 2;
    }
    let mut input = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("registry-commit: stdin read failed: {error}");
        return 2;
    }
    let payload: Value = match serde_json::from_str(&input) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("registry-commit: bad payload: {error}");
            return 2;
        }
    };
    let path = match payload.get("path").and_then(Value::as_str) {
        Some(path) if !path.is_empty() => path,
        _ => {
            eprintln!("registry-commit: payload has no path");
            return 2;
        }
    };
    let path = match absolute_path(path) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("registry-commit: cannot resolve path: {error}");
            return 1;
        }
    };
    if payload.get("op").and_then(Value::as_str) == Some("read") {
        return match crate::registry_store::read_versioned(&path) {
            Ok((document, revision)) => {
                println!(
                    "{}",
                    json!({"status":"read","document":document,"revision":revision})
                );
                0
            }
            Err(error) => {
                eprintln!("registry-commit read: {error}");
                1
            }
        };
    }
    let schema_version = match payload.get("schema_version").and_then(Value::as_u64) {
        Some(version) => version,
        None => {
            eprintln!("registry-commit: payload has no integer schema_version");
            return 2;
        }
    };
    let agents = match payload.get("agents").and_then(Value::as_array) {
        Some(agents) if agents.iter().all(Value::is_object) => agents,
        _ => {
            eprintln!("registry-commit: payload agents must be an array of objects");
            return 2;
        }
    };
    let expected = match payload.get("revision").and_then(Value::as_i64) {
        Some(revision) => revision,
        None => {
            eprintln!("registry-commit: read the table revision before writing");
            return 2;
        }
    };
    let transaction = match crate::registry_store::begin(&path) {
        Ok(transaction) => transaction,
        Err(error) => {
            eprintln!("registry-commit: {error}");
            return 1;
        }
    };
    if transaction.revision != expected {
        eprintln!(
            "{}",
            json!({"status":"refused","reason":"revision_conflict","message":"registry changed since read. Reload before applying the mutation again."})
        );
        return 3;
    }
    let before = transaction.document.clone();
    // The schema repair verb drops newer-schema keys on purpose, so a merge
    // that carries disk fields forward would undo it.
    let replace = payload.get("replace").and_then(Value::as_bool) == Some(true);
    let next = if replace {
        Ok(json!({"schema_version": schema_version, "agents": agents}))
    } else {
        merge(before.clone(), &payload, schema_version, agents)
    };
    match next {
        Ok(value) => {
            // A replace is the schema repair: the stored document is AHEAD of
            // this writer by definition, so judge the repaired document alone.
            let baseline = if replace { &value } else { &before };
            if let Err(error) =
                crate::state::validate_registry_document_change(&path, baseline, &value)
            {
                eprintln!("registry-commit: {error}");
                return 3;
            }
            match transaction.commit(value.clone()) {
                Ok(written) => {
                    if let (Ok(before), Ok(after)) = (
                        serde_json::from_value::<crate::state::Registry>(before),
                        serde_json::from_value::<crate::state::Registry>(value),
                    ) {
                        crate::state::account_for_removed_rows(
                            &path,
                            &before.entries,
                            &after.entries,
                        );
                    }
                    let revision = expected + i64::from(written);
                    println!(
                        "{}",
                        json!({"status":"written","revision":revision,"changed":written})
                    );
                    0
                }
                Err(error) => {
                    eprintln!("registry-commit: write failed: {error}");
                    1
                }
            }
        }
        Err((reason, message)) => {
            eprintln!(
                "{}",
                json!({"status":"refused","reason":reason,"message":message})
            );
            3
        }
    }
}

fn absolute_path(path: &str) -> std::io::Result<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

const RETIRED_ROW_KEYS: &[&str] = &[
    "claude_short_id",
    "claude_session_uuid",
    "codex_session_id",
    "gemini_session_id",
];

fn merge(
    mut disk: Value,
    payload: &Value,
    payload_schema: u64,
    payload_agents: &[Value],
) -> Result<Value, (String, String)> {
    let object = disk.as_object_mut().ok_or_else(|| {
        (
            "invalid_registry".into(),
            "registry root is not an object".into(),
        )
    })?;
    let disk_schema = object
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            (
                "invalid_registry".into(),
                "schema_version is not an integer".into(),
            )
        })?;
    let floor = object
        .get("min_writer_version")
        .and_then(Value::as_u64)
        .unwrap_or(disk_schema);
    if floor > payload_schema {
        return Err((
            "writer_too_old".into(),
            format!("min_writer_version={floor} exceeds writer schema_version={payload_schema}"),
        ));
    }
    let disk_agents = object
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| ("invalid_registry".into(), "agents is not an array".into()))?;
    let mut consumed = vec![false; disk_agents.len()];
    let mut merged_agents = Vec::with_capacity(payload_agents.len());
    for row in payload_agents {
        let disk_index = matching_disk_row(row, disk_agents, &consumed);
        let mut merged = row.as_object().expect("validated payload row").clone();
        if let Some(index) = disk_index {
            consumed[index] = true;
            if let Some(disk_row) = disk_agents[index].as_object() {
                for (key, value) in disk_row {
                    // Retired keys are read-only backfill; the writer dropped them on purpose.
                    if RETIRED_ROW_KEYS.contains(&key.as_str()) {
                        continue;
                    }
                    merged.entry(key.clone()).or_insert_with(|| value.clone());
                }
            }
        }
        merged_agents.push(Value::Object(merged));
    }
    if disk_schema > payload_schema && consumed.iter().any(|matched| !matched) {
        return Err((
            "row_loss_under_skew".into(),
            "payload omits one or more rows from the newer on-disk registry".into(),
        ));
    }

    let payload_object = payload.as_object().expect("validated payload object");
    let target = disk.as_object_mut().expect("root checked above");
    for (key, value) in payload_object {
        // Request fields steer this verb; they are not registry data.
        if !matches!(
            key.as_str(),
            "path" | "agents" | "schema_version" | "revision" | "replace" | "op"
        ) {
            target.insert(key.clone(), value.clone());
        }
    }
    target.insert("agents".into(), Value::Array(merged_agents));
    target.insert(
        "schema_version".into(),
        json!(disk_schema
            .max(payload_schema)
            .max(REGISTRY_SCHEMA_VERSION as u64)),
    );
    target.insert(
        "min_writer_version".into(),
        json!(floor.max(crate::state::REGISTRY_MIN_WRITER_VERSION as u64)),
    );
    target.insert(
        "writer_rev".into(),
        json!(format!("{}/registry-commit", env!("FNO_AGENTS_GIT_REV"))),
    );
    Ok(disk)
}

fn matching_disk_row(row: &Value, disk: &[Value], consumed: &[bool]) -> Option<usize> {
    let session_id = row
        .get("harness_session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    if let Some(session_id) = session_id {
        if let Some(index) = disk.iter().enumerate().position(|(index, candidate)| {
            !consumed[index]
                && candidate.get("harness_session_id").and_then(Value::as_str) == Some(session_id)
        }) {
            return Some(index);
        }
    }
    let name = row.get("name").and_then(Value::as_str)?;
    // A name collision cannot substitute for a different stable session id;
    // leaving it unmatched lets the skewed row-loss guard refuse the write.
    disk.iter().enumerate().position(|(index, candidate)| {
        if consumed[index] || candidate.get("name").and_then(Value::as_str) != Some(name) {
            return false;
        }
        let disk_session_id = candidate
            .get("harness_session_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        match (session_id, disk_session_id) {
            (Some(payload_id), Some(disk_id)) => payload_id == disk_id,
            _ => true,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::merge;
    use serde_json::json;

    #[test]
    fn merge_preserves_unknown_disk_fields_and_refuses_floor_ahead() {
        assert!(super::absolute_path("registry.json").unwrap().is_absolute());
        let disk = json!({
            "schema_version": 42,
            "min_writer_version": 40,
            "writer_rev": "future-writer",
            "future_top": "preserved",
            "agents": [
                {"name": "worker", "harness_session_id": "sid", "future_row": "preserved"},
                {"name": "by-name", "future_name_row": "preserved"}
            ]
        });
        let payload = json!({
            "schema_version": 40,
            "agents": [
                {"name": "renamed", "harness_session_id": "sid", "status": "idle"},
                {"name": "by-name", "status": "busy"}
            ]
        });
        let merged = merge(
            disk.clone(),
            &payload,
            40,
            payload["agents"].as_array().unwrap(),
        )
        .unwrap();
        assert_eq!(merged["schema_version"], 42);
        assert_eq!(merged["future_top"], "preserved");
        assert_eq!(merged["agents"][0]["future_row"], "preserved");
        assert_eq!(merged["agents"][0]["status"], "idle");
        assert_eq!(merged["agents"][1]["future_name_row"], "preserved");
        assert_eq!(merged["min_writer_version"], 40);
        assert!(merged["writer_rev"]
            .as_str()
            .unwrap()
            .ends_with("/registry-commit"));

        let mut breaking = disk;
        breaking["min_writer_version"] = json!(41);
        let error = merge(
            breaking,
            &payload,
            40,
            payload["agents"].as_array().unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.0, "writer_too_old");

        let incomplete = json!({
            "schema_version": 42,
            "min_writer_version": 40,
            "agents": [{"name": "worker", "harness_session_id": "sid"}, {"name": "unreadable-to-old-writer"}]
        });
        let error = merge(
            incomplete,
            &payload,
            40,
            payload["agents"].as_array().unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.0, "row_loss_under_skew");

        let conflicting_identity = json!({
            "schema_version": 42,
            "min_writer_version": 40,
            "agents": [{"name": "worker-2", "harness_session_id": "old-session", "future": "kept"}]
        });
        let new_session = json!({
            "schema_version": 40,
            "agents": [{"name": "worker-2", "harness_session_id": "new-session", "status": "idle"}]
        });
        let error = merge(
            conflicting_identity,
            &new_session,
            40,
            new_session["agents"].as_array().unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.0, "row_loss_under_skew");
    }
}
