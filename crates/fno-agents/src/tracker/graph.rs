//! The default backend: the graph store's rows, projected.
//!
//! graph.json is already "a sidecar plus a tracker merged into one record",
//! so this backend is a projection, not a rewrite. Rows are read once per
//! instance and answered from memory. `close` is refused: every production
//! close runs under an external backend, and the graph closes through
//! `fno backlog done`, which derives status from `completed_at` - a second
//! close path here would be a second writer to the same derivation.

use super::{Candidate, State, Tracker, TrackerError, TrackerNode};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;

pub struct GraphTracker {
    path: PathBuf,
    rows: OnceLock<Result<Vec<Value>, String>>,
}

impl GraphTracker {
    pub fn new() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::with_path(crate::king_board::scope::graph_json_path(&cwd))
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self {
            path,
            rows: OnceLock::new(),
        }
    }

    fn load(&self) -> Result<&[Value], TrackerError> {
        match self.rows.get_or_init(|| {
            crate::backlog::api::rows(&crate::backlog::api::Store::new(&self.path))
                .map(|mut rows| {
                    crate::graph_store::apply_readiness_overlay(&mut rows);
                    rows
                })
                .map_err(|e| e.0)
        }) {
            Ok(rows) => Ok(rows),
            Err(e) => Err(TrackerError::Backend(e.clone())),
        }
    }

    fn find<'a>(&self, rows: &'a [Value], id: &str) -> Option<&'a Value> {
        rows.iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(id))
    }

    fn node_of(row: &Value) -> TrackerNode {
        let status = row.get("status").and_then(Value::as_str).unwrap_or("");
        TrackerNode {
            id: row
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            title: row.get("title").and_then(Value::as_str).map(str::to_string),
            state: if crate::graph_store::TERMINAL_RUNGS.contains(&status) {
                State::Closed
            } else {
                State::Open
            },
            parent: row
                .get("parent")
                .and_then(Value::as_str)
                .map(str::to_string),
            blocked_by: row
                .get("blocked_by")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            details: row
                .get("details")
                .and_then(Value::as_str)
                .map(str::to_string),
            url: None,
            size: row.get("size").and_then(Value::as_str).map(str::to_string),
        }
    }

    fn candidate_of(row: &Value) -> Candidate {
        Candidate {
            node: Self::node_of(row),
            priority: row
                .get("priority")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .unwrap_or("p2")
                .to_string(),
            rank: row.get("rank").and_then(Value::as_f64),
            created_at: row
                .get("created_at")
                .and_then(Value::as_str)
                .map(str::to_string),
            closed_at: row
                .get("completed_at")
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }
}

impl Tracker for GraphTracker {
    fn name(&self) -> &str {
        "graph"
    }

    fn read(&self, id: &str) -> Result<TrackerNode, TrackerError> {
        self.load()?
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(id))
            .map(Self::node_of)
            .ok_or_else(|| TrackerError::NotFound(id.to_string()))
    }

    fn list_open(&self) -> Result<Vec<Candidate>, TrackerError> {
        let rows = self.load()?;
        Ok(rows
            .iter()
            .filter(|row| {
                let status = row.get("status").and_then(Value::as_str).unwrap_or("");
                !crate::graph_store::TERMINAL_RUNGS.contains(&status)
            })
            .map(Self::candidate_of)
            .collect())
    }

    fn list_closed_since(&self, days: u32) -> Option<Result<Vec<Candidate>, TrackerError>> {
        let rows = match self.load() {
            Ok(rows) => rows,
            Err(e) => return Some(Err(e)),
        };
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
        let mut out = Vec::new();
        for row in rows {
            let status = row.get("status").and_then(Value::as_str).unwrap_or("");
            if !crate::graph_store::TERMINAL_RUNGS.contains(&status) {
                continue;
            }
            let Some(ts) = row.get("completed_at").and_then(Value::as_str) else {
                continue;
            };
            let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else {
                continue;
            };
            if t.with_timezone(&chrono::Utc) < cutoff {
                continue;
            }
            out.push(Self::candidate_of(row));
        }
        Some(Ok(out))
    }

    fn sidecar(&self, id: &str) -> Result<serde_json::Map<String, Value>, TrackerError> {
        let mut out = serde_json::Map::new();
        if let Some(row) = self.find(self.load()?, id) {
            if let Some(obj) = row.as_object() {
                for key in super::sidecar::SIDECAR_KEYS {
                    if let Some(v) = obj.get(*key) {
                        out.insert((*key).to_string(), v.clone());
                    }
                }
            }
        }
        Ok(out)
    }

    fn close(&self, _id: &str) -> Result<(), TrackerError> {
        Err(TrackerError::Refused(
            "the graph backend closes through fno backlog done".into(),
        ))
    }
}
