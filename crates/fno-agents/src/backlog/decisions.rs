//! Decision records and their node join rows, owned by the graph store.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use std::path::Path;

const DECISION_EVENT: &str = "operator_decision";
const RETRACTION_EVENT: &str = "decision_retracted";

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS decisions (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  event_id TEXT NOT NULL UNIQUE,
  event_type TEXT NOT NULL,
  ts TEXT NOT NULL,
  source TEXT,
  data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS decisions_event_id ON decisions(event_id);
CREATE TABLE IF NOT EXISTS node_decisions (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  event_id TEXT NOT NULL REFERENCES decisions(event_id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  PRIMARY KEY (node_id, event_id)
);
CREATE INDEX IF NOT EXISTS node_decisions_order ON node_decisions(node_id, seq);";

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(DDL)
        .map_err(|error| error.to_string())
}

/// Import the durable machine-wide decision journal once, then validate every
/// decision reference already present on a node. The journal is the event
/// source; a node reference without its event is a hard error, not an empty
/// decision list.
pub fn import_if_needed(connection: &mut Connection, graph: &Path) -> Result<(), String> {
    if super::meta(connection, "decisions_imported")?.is_some() {
        return Ok(());
    }

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let journal = graph.with_file_name("decisions.jsonl");
    if journal.exists() {
        let text = std::fs::read_to_string(&journal).map_err(|error| {
            format!(
                "decisions import: cannot read {}: {error}",
                journal.display()
            )
        })?;
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            // A torn append is dead weight, not an incident: refusing open()
            // over it would brick every store read (decisions_imported never
            // stamps) and hide the good rows behind one bad line. Skip it
            // loudly; the Python legacy scan still counts it as damaged and
            // names `fno backlog decide-reindex` as the recovery.
            let event: Value = match serde_json::from_str(line) {
                Ok(event) => event,
                Err(error) => {
                    eprintln!(
                        "warning: decisions import: {} line {} did not parse ({}); it is NOT folded",
                        journal.display(),
                        line_number + 1,
                        error
                    );
                    continue;
                }
            };
            insert_event(&transaction, &event)
                .map_err(|error| format!("decisions import: line {}: {error}", line_number + 1))?;
        }
    }

    if graph.exists() {
        let text = std::fs::read_to_string(graph).map_err(|error| error.to_string())?;
        if !text.trim().is_empty() {
            let document: Value = serde_json::from_str(&text)
                .map_err(|error| format!("{} is invalid JSON: {error}", graph.display()))?;
            if let Some(entries) = document.get("entries").and_then(Value::as_array) {
                for entry in entries {
                    let Some(node_id) = entry.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(decisions) = entry.get("decisions").and_then(Value::as_array) else {
                        continue;
                    };
                    for (position, reference) in decisions.iter().enumerate() {
                        let Some(event_id) = reference.get("decision_id").and_then(Value::as_str)
                        else {
                            return Err(format!(
                                "decisions import: node {node_id} decision at position {} has no decision_id",
                                position + 1
                            ));
                        };
                        if !event_exists(&transaction, event_id)? {
                            return Err(format!(
                                "decisions import: node {node_id} references missing decision {event_id}"
                            ));
                        }
                        attach_node(&transaction, node_id, event_id)?;
                    }
                }
            }
        }
    }

    super::stamp_meta(&transaction, "decisions_imported", "1")?;
    transaction.commit().map_err(|error| error.to_string())
}

/// Record one event on a connection that is already owned by a caller.
/// Primarily used by in-process Rust consumers and tests.
pub fn record(connection: &Connection, event: &Value) -> Result<(), String> {
    let event_id = insert_event(connection, event)?;
    attach_subject(connection, event, &event_id)
}

pub fn retract(connection: &Connection, event: &Value) -> Result<(), String> {
    record(connection, event)
}

/// Record an event and its subject join under one immediate transaction. The
/// API mutation counter moves only after the event and join have landed.
pub fn record_connected(
    connection: &mut Connection,
    event: &Value,
    _mutation: &str,
) -> Result<(), String> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let event_id = insert_event(&transaction, event)?;
    attach_subject(&transaction, event, &event_id)?;
    super::stamp_version(&transaction, &format!("decision:{event_id}"))?;
    transaction.commit().map_err(|error| error.to_string())
}

/// Return flattened event rows in journal order and a count of malformed rows.
pub fn read_rows(connection: &Connection) -> Result<(Vec<Value>, usize), String> {
    let mut statement = connection
        .prepare("SELECT event_type, ts, data FROM decisions ORDER BY seq")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut output = Vec::new();
    let mut damaged = 0;
    for row in rows {
        let (event_type, ts, data_raw) = row.map_err(|error| error.to_string())?;
        let Ok(data) = serde_json::from_str::<Value>(&data_raw) else {
            damaged += 1;
            continue;
        };
        let Some(mut flattened) = data.as_object().cloned() else {
            damaged += 1;
            continue;
        };
        flattened.insert("ts".to_string(), Value::String(ts));
        flattened.insert("_event_type".to_string(), Value::String(event_type));
        output.push(Value::Object(flattened));
    }
    Ok((output, damaged))
}

pub fn node_decisions(connection: &Connection, node_id: &str) -> Result<Vec<Value>, String> {
    let mut statement = connection
        .prepare(
            "SELECT d.event_type, d.ts, d.data
             FROM node_decisions nd
             JOIN decisions d ON d.event_id = nd.event_id
             WHERE nd.node_id = ?1
             ORDER BY nd.seq, d.seq",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![node_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    flatten_rows(rows)
}

fn flatten_rows<I>(rows: I) -> Result<Vec<Value>, String>
where
    I: Iterator<Item = Result<(String, String, String), rusqlite::Error>>,
{
    let mut output = Vec::new();
    for row in rows {
        let (event_type, ts, data_raw) = row.map_err(|error| error.to_string())?;
        let data: Value = serde_json::from_str(&data_raw).map_err(|error| error.to_string())?;
        let Some(mut flattened) = data.as_object().cloned() else {
            return Err("decision data is not an object".to_string());
        };
        flattened.insert("ts".to_string(), Value::String(ts));
        flattened.insert("_event_type".to_string(), Value::String(event_type));
        output.push(Value::Object(flattened));
    }
    Ok(output)
}

fn insert_event(connection: &Connection, event: &Value) -> Result<String, String> {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "event has no type".to_string())?;
    if event_type != DECISION_EVENT && event_type != RETRACTION_EVENT {
        return Err(format!("unsupported event type {event_type:?}"));
    }
    let data = event
        .get("data")
        .filter(|value| value.is_object())
        .ok_or_else(|| "event data is not an object".to_string())?;
    let event_id = match event_type {
        DECISION_EVENT => data
            .get("decision_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "operator_decision has no decision_id".to_string())?,
        RETRACTION_EVENT => data
            .get("retraction_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "decision_retracted has no retraction_id".to_string())?,
        _ => unreachable!(),
    };
    let ts = event.get("ts").and_then(Value::as_str).unwrap_or_default();
    let source = event.get("source").and_then(Value::as_str);
    let data_json = serde_json::to_string(data).map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT OR IGNORE INTO decisions (event_id, event_type, ts, source, data)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![event_id, event_type, ts, source, data_json],
        )
        .map_err(|error| error.to_string())?;
    Ok(event_id.to_string())
}

fn event_exists(connection: &Connection, event_id: &str) -> Result<bool, String> {
    connection
        .query_row(
            "SELECT 1 FROM decisions WHERE event_id = ?1",
            params![event_id],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(|error| error.to_string())
}

fn attach_subject(connection: &Connection, event: &Value, event_id: &str) -> Result<(), String> {
    let Some(node_id) = event
        .get("data")
        .and_then(|data| data.get("subject"))
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let node_exists: bool = connection
        .query_row(
            "SELECT 1 FROM nodes WHERE id = ?1",
            params![node_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .is_some();
    if node_exists {
        attach_node(connection, node_id, event_id)?;
    }
    Ok(())
}

fn attach_node(connection: &Connection, node_id: &str, event_id: &str) -> Result<(), String> {
    let next_seq: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM node_decisions WHERE node_id = ?1",
            params![node_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT OR IGNORE INTO node_decisions (node_id, event_id, seq)
             VALUES (?1, ?2, ?3)",
            params![node_id, event_id, next_seq],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE graph_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        ensure_table(&connection).unwrap();
        connection
    }

    fn event(id: &str) -> Value {
        serde_json::json!({
            "ts": "2026-09-16T00:00:01Z",
            "type": DECISION_EVENT,
            "source": "target",
            "data": {
                "decision_id": id,
                "decision": "store decisions in graph.db",
                "subject": "x-node",
            },
        })
    }

    #[test]
    fn decisions_record_flattens_and_joins_subject_node() {
        let connection = connection();
        let mut decision = event("d-one");
        decision["data"].as_object_mut().unwrap().remove("subject");
        record(&connection, &decision).unwrap();

        let (rows, damaged) = read_rows(&connection).unwrap();
        assert_eq!(damaged, 0);
        assert_eq!(rows[0]["decision_id"], "d-one");
    }

    #[test]
    fn decisions_import_rejects_an_orphan_node_reference() {
        let temp = TempDir::new().unwrap();
        let graph = temp.path().join("graph.json");
        std::fs::write(
            &graph,
            serde_json::json!({
                "entries": [{
                    "id": "x-node",
                    "decisions": [{"decision_id": "d-missing"}]
                }]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            temp.path().join("decisions.jsonl"),
            serde_json::to_string(&event("d-present")).unwrap() + "\n",
        )
        .unwrap();
        let mut connection = connection();

        let error = import_if_needed(&mut connection, &graph).unwrap_err();

        assert!(error.contains("x-node"));
        assert!(error.contains("d-missing"));
        assert!(crate::backlog::meta(&connection, "decisions_imported")
            .unwrap()
            .is_none());
    }
}
