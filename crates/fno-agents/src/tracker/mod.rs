//! The work-item tracker seam: one interface, the graph, GitHub and Linear
//! backends, and the stdin JSON door on `graph-get` that serves them.
//!
//! The Python seam (`cli/src/fno/tracker/`) was the only implementation of
//! this surface, and `cli/src/fno` bars new Python, so the seam lives here and
//! Python became a thin exec client. A backend is one file implementing
//! [`Tracker`] plus one arm in [`backend`]; refusals ride in the door's JSON
//! payload with exit 0, the same contract `gh_budget` uses on
//! `fleet-incident`, so the Python `verb_call` never mistakes a refusal for a
//! crash.
//!
//! A new top-level verb is not allowed (law d-fe66560a), so the door rides the
//! existing `graph-get` action: no positional ids plus a non-terminal stdin
//! carrying `{"tracker": <op>, ...}`.

use crate::tracker::{github::GitHubTracker, graph::GraphTracker, linear::LinearTracker};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod github;
pub mod graph;
pub mod linear;
pub mod sidecar;
pub mod snapshot;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;

/// The only state vocabulary a backend supplies; footnote derives its own
/// rung from the plan and the PR exactly as it always has.
#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Open,
    Closed,
}

/// The read projection of one work item. The five historical fields plus the
/// display-only reads (`details`, `url`, `size`) no sidecar field shares a
/// name with; the partition gate inspects these names.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TrackerNode {
    pub id: String,
    pub title: Option<String>,
    pub state: State,
    pub parent: Option<String>,
    pub blocked_by: Vec<String>,
    pub details: Option<String>,
    pub url: Option<String>,
    pub size: Option<String>,
}

/// What `list_open` and `list_closed_since` return: the node plus exactly the
/// ordering inputs footnote's selection sort consumes.
#[derive(Serialize, Clone, Debug)]
pub struct Candidate {
    #[serde(flatten)]
    pub node: TrackerNode,
    pub priority: String,
    pub rank: Option<f64>,
    pub created_at: Option<String>,
    pub closed_at: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Backend(String),
    #[error("{0}")]
    Refused(String),
}

/// Read five-plus-display fields, enumerate the open set, and close. That is
/// the whole contract; the sidecar join happens above it, in [`snapshot`].
pub trait Tracker {
    fn name(&self) -> &str;
    fn read(&self, id: &str) -> Result<TrackerNode, TrackerError>;
    fn list_open(&self) -> Result<Vec<Candidate>, TrackerError>;
    /// None: the backend has no closed window, and the model says so.
    fn list_closed_since(&self, _days: u32) -> Option<Result<Vec<Candidate>, TrackerError>> {
        None
    }
    /// Sidecar fields for one id. The default reads the per-id sidecar file;
    /// a backend that stores sidecar fields in the row overrides it.
    fn sidecar(&self, id: &str) -> Result<serde_json::Map<String, Value>, TrackerError> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        sidecar::load(&sidecar::root(&cwd), id)
    }
    fn close(&self, id: &str) -> Result<(), TrackerError>;
}

/// The backend registry: a new backend is one file plus one arm here.
pub fn backend(name: &str) -> Result<Box<dyn Tracker>, TrackerError> {
    match name {
        "graph" => Ok(Box::new(GraphTracker::new())),
        "github" => Ok(Box::new(GitHubTracker::from_env())),
        "linear" => Ok(Box::new(LinearTracker::from_env())),
        other => Err(TrackerError::Refused(format!(
            "unknown tracker backend: {other}. Available: graph, github, linear"
        ))),
    }
}

/// The selected backend: explicit wins, else `FNO_TRACKER_BACKEND`, else
/// "graph" - the same resolution the Python `active_backend_name` applies.
pub fn backend_name(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| {
            std::env::var("FNO_TRACKER_BACKEND")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "graph".to_string())
}

/// The graph-get stdin door's tracker dispatch. Every op answers one JSON
/// object and the caller exits 0; refusals ride in the payload.
pub fn run_door(payload: &Value) -> Value {
    let op = payload.get("tracker").and_then(Value::as_str).unwrap_or("");
    let name = backend_name(payload.get("backend").and_then(Value::as_str));
    let id = payload.get("id").and_then(Value::as_str);
    if let Some(id) = id {
        if id.contains(':') {
            return error_value(&TrackerError::Refused(format!(
                "id {id:?} contains ':' - the claim-key partition character"
            )));
        }
    }
    let tracker = match backend(&name) {
        Ok(t) => t,
        Err(e) => return error_value(&e),
    };
    match op {
        "read" => match id {
            Some(id) => match tracker.read(id) {
                Ok(node) => json!({ "node": node }),
                Err(e) => error_value(&e),
            },
            None => error_value(&TrackerError::Refused("read needs an id".into())),
        },
        "list-open" => match tracker.list_open() {
            Ok(candidates) => json!({ "candidates": candidates }),
            Err(e) => error_value(&e),
        },
        "snapshot" => snapshot::door_snapshot(
            tracker.as_ref(),
            &name,
            payload
                .get("stale_ok")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
        "close" => match id {
            Some(id) => match tracker.close(id) {
                Ok(()) => json!({ "closed": id }),
                Err(e) => error_value(&e),
            },
            None => error_value(&TrackerError::Refused("close needs an id".into())),
        },
        other => json!({ "error": format!("unknown tracker op {other}") }),
    }
}

/// `NotFound` is a distinct payload key; every other error is a message.
pub fn error_value(e: &TrackerError) -> Value {
    match e {
        TrackerError::NotFound(_) => json!({ "not_found": true }),
        _ => json!({ "error": e.to_string() }),
    }
}
