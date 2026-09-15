//! The single bounded node-state owner (x-920a wave 1).
//!
//! A node carries ONE `current_state` object in its extras: `{body, revision,
//! updated_at, source_session_id, source_harness}`. A note REPLACES it under
//! revision-checked optimistic concurrency; the exact pre-image is journaled
//! first (see `note_history`), so the hot node holds one current state and
//! every prior stays readable.
use crate::backlog::note_history;
use crate::graph_store::{self, MutateInput, StoreError};
use serde_json::{json, Value};
use std::path::Path;

/// Combined `details` + `current_state.body` budget, in Unicode scalars,
/// counted after outer-whitespace and CRLF normalization.
pub const PROSE_LIMIT: usize = 5000;

/// Extras key holding the bounded current-state object.
pub const STATE_KEY: &str = "current_state";
/// Extras key marking a row as migrated: history is authoritative for its
/// priors, and `progress_notes` must never grow on such a row.
pub const HISTORY_MARKER_KEY: &str = "note_history";

/// A view over one row's `current_state`.
#[derive(Debug, Clone, PartialEq)]
pub struct CurrentStateView {
    pub revision: u64,
    pub body: String,
    pub updated_at: Option<String>,
    pub source_session_id: Option<String>,
    pub source_harness: Option<String>,
}

/// Read one row's `current_state` view. `None` when absent (revision 0).
pub fn read_state(row: &Value) -> Option<CurrentStateView> {
    let st = row.get(STATE_KEY)?.as_object()?;
    Some(CurrentStateView {
        revision: st.get("revision").and_then(Value::as_u64).unwrap_or(0),
        body: st
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        updated_at: st
            .get("updated_at")
            .and_then(Value::as_str)
            .map(str::to_string),
        source_session_id: st
            .get("source_session_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        source_harness: st
            .get("source_harness")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The current revision of one row's state; 0 when the row has none.
pub fn row_revision(row: &Value) -> u64 {
    read_state(row).map(|s| s.revision).unwrap_or(0)
}

/// Normalize outer whitespace and CRLF once, for counting and for storage.
pub fn normalize_prose(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_string()
}

/// Unicode scalars after normalization.
pub fn count_prose(text: &str) -> usize {
    normalize_prose(text).chars().count()
}

/// Combined `details` + `current_state.body` budget, in Unicode scalars,
/// counted after outer-whitespace and CRLF normalization.
pub fn prose_total(row: &Value) -> usize {
    let details = row.get("details").and_then(Value::as_str).unwrap_or("");
    let body = row
        .get(STATE_KEY)
        .and_then(|s| s.get("body"))
        .and_then(Value::as_str)
        .unwrap_or("");
    count_prose(details) + count_prose(body)
}

/// The over-budget refusal the ACs name; prints the accounting the operator
/// asked for: `details=N state=M total=T limit=5000`.
pub fn budget_message(row_id: &str, row: &Value) -> String {
    let details = count_prose(row.get("details").and_then(Value::as_str).unwrap_or(""));
    let state = count_prose(
        row.get(STATE_KEY)
            .and_then(|s| s.get("body"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let total = details + state;
    format!("prose budget exceeded on {row_id}: details={details} state={state} total={total} limit={PROSE_LIMIT}. Put long evidence in a file, then note its path.")
}

/// Error surface of the state owner.
#[derive(Debug)]
pub enum StateError {
    /// No row resolves to the node id.
    NoNode(String),
    /// The submitted revision is not the current one.
    Conflict {
        current_revision: u64,
        submitted_revision: u64,
    },
    /// Body empty after normalization (the named clear action exists).
    EmptyBody,
    /// The journal refused; nothing published.
    History(String),
    /// Underlying store error.
    Store(StoreError),
}

impl From<StoreError> for StateError {
    fn from(e: StoreError) -> Self {
        StateError::Store(e)
    }
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::NoNode(id) => write!(f, "no node resolves to '{id}'"),
            StateError::Conflict {
                current_revision,
                submitted_revision,
            } => write!(
                f,
                "state conflict: current revision {current_revision} != submitted {submitted_revision}; re-read with `fno backlog note <node> --read` and resubmit"
            ),
            StateError::EmptyBody => write!(
                f,
                "note text is empty; to clear current state use the named clear action"
            ),
            StateError::History(e) => write!(f, "history write failed, state unchanged: {e}"),
            StateError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StateError {}

/// One replacement request.
#[derive(Debug, Clone)]
pub struct StateWriteInput {
    pub node_id: String,
    /// New state body. Empty after normalization refuses; use `clear_state`.
    pub body: String,
    /// Optimistic-concurrency guard. The caller fetches the current revision
    /// and submits it; a mismatch refuses before anything is written.
    pub if_revision: Option<u64>,
    pub source_session_id: Option<String>,
    pub source_harness: Option<String>,
    /// Citation evidence rows (`note --read` provenance), stored beside the
    /// body inside the state object. `None` leaves the key absent.
    pub reads: Option<Value>,
}

/// The outcome of one successful replacement.
#[derive(Debug, Clone)]
pub struct StateReceipt {
    pub node_id: String,
    pub revision: u64,
    pub total_prose: usize,
    /// Whether a pre-image was journaled (absent on the node's first state).
    pub journaled: bool,
}

fn read_rows_for(graph: &Path) -> Result<Vec<Value>, StateError> {
    let mut rows = graph_store::read_rows(graph)?;
    graph_store::apply_defaults(&mut rows, false);
    Ok(rows)
}

/// The node's current state revision; 0 when absent.
pub fn current_revision(graph: &Path, node_id: &str) -> Result<u64, StateError> {
    let rows = read_rows_for(graph)?;
    rows.iter()
        .find(|r| graph_store::entry_id(r) == Some(node_id))
        .map(row_revision)
        .ok_or_else(|| StateError::NoNode(node_id.to_string()))
}

/// Journal records for one node (paged), through `note_history::read`.
pub fn history_page(
    graph: &Path,
    node_id: &str,
    offset: usize,
    limit: usize,
) -> Result<(Vec<Value>, usize), String> {
    note_history::read(graph, Some(node_id), offset, limit)
}

/// Replace (or first-write) one node's `current_state`, revision-checked,
/// with the exact pre-image journaled before publication. Budget and conflict
/// checks run inside the publication lock, so two writers with one base
/// revision produce exactly one commit and one conflict.
pub fn replace_state(graph: &Path, input: &StateWriteInput) -> Result<StateReceipt, StateError> {
    let body = normalize_prose(&input.body);
    if body.is_empty() {
        return Err(StateError::EmptyBody);
    }
    // x-385e: stamp the base BEFORE the read, so a writer that lands between
    // the two makes the publish Conflict instead of blessing stale rows.
    let base = graph_store::base_version(graph)?;
    let rows = read_rows_for(graph)?;
    let node_id = input.node_id.as_str();
    let row = rows
        .iter()
        .find(|r| graph_store::entry_id(r) == Some(node_id))
        .ok_or_else(|| StateError::NoNode(node_id.to_string()))?;

    // Budget fast-fail with the exact accounting the ACs name. The seam
    // re-enforces it under the lock for every writer.
    let details = count_prose(row.get("details").and_then(Value::as_str).unwrap_or(""));
    let state = body.chars().count();
    if details + state > PROSE_LIMIT {
        return Err(StateError::Store(StoreError::Invalid(budget_message(
            node_id,
            &json!({
                STATE_KEY: {"body": body},
                "details": row.get("details").and_then(Value::as_str).unwrap_or(""),
            }),
        ))));
    }

    // Optimistic concurrency: the caller-submitted revision must equal the
    // revision we can see outside the lock; the hook re-checks inside it.
    let fetched = row_revision(row);
    if let Some(submitted) = input.if_revision {
        if submitted != fetched {
            return Err(StateError::Conflict {
                current_revision: fetched,
                submitted_revision: submitted,
            });
        }
    }
    let expected = input.if_revision.unwrap_or(fetched);

    let session = input.source_session_id.clone();
    let harness = input.source_harness.clone();
    let journaled_flag = std::sync::atomic::AtomicBool::new(false);
    // The sibling fields the candidate carries from the outside-the-lock
    // read: if another writer changed them meanwhile, publishing the stale
    // candidate would clobber them, so the hook refuses instead. Snapshots
    // come from the RAW file (the hook sees raw rows too); the defaulted
    // read synthesizes status for rows that lack one, which would read as a
    // phantom change.
    let raw_rows = match graph_store::read_raw(graph)? {
        graph_store::RawRead::Entries(v) => v,
        _ => Vec::new(),
    };
    let raw_row = raw_rows
        .iter()
        .find(|r| graph_store::entry_id(r) == Some(node_id));
    let snapshot_details: Option<Value> = raw_row.and_then(|r| r.get("details")).cloned();
    let snapshot_status: Option<Value> = raw_row.and_then(|r| r.get("status")).cloned();

    // Under the publication lock: re-verify the row revision against the raw
    // snapshot, journal the exact pre-image, then allow publication. Any
    // history failure refuses the whole mutation.
    let mut hook = |raw: &[Value]| -> Result<(), StoreError> {
        let row = raw
            .iter()
            .find(|r| graph_store::entry_id(r) == Some(node_id))
            .ok_or_else(|| {
                StoreError::Invalid(format!("state write target vanished: {node_id}"))
            })?;
        let current = row_revision(row);
        if current != expected {
            return Err(StoreError::Invalid(format!(
                "state-conflict: current revision {current} != submitted {expected}"
            )));
        }
        if row.get("details") != snapshot_details.as_ref()
            || row.get("status") != snapshot_status.as_ref()
        {
            return Err(StoreError::Invalid(
                "state-conflict: the node changed while this write was in flight; re-read and resubmit"
                    .to_string(),
            ));
        }
        if let Some(pre_state) = row.get(STATE_KEY) {
            note_history::append(
                graph,
                node_id,
                note_history::REASON_STATE_REPLACED,
                Some(current),
                None,
                pre_state,
                session.as_deref(),
                harness.as_deref(),
            )
            .map_err(|e| StoreError::Invalid(format!("history write failed: {e}")))?;
            journaled_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    };

    let mut working = rows.clone();
    for row in working.iter_mut() {
        if graph_store::entry_id(row) != Some(node_id) {
            continue;
        }
        let obj = row.as_object_mut().unwrap();
        let mut state = json!({
            "body": body,
            "revision": expected + 1,
            "updated_at": graph_store::now_isoformat(),
            "source_session_id": session,
            "source_harness": harness,
        });
        if let Some(reads) = &input.reads {
            if let Some(state_obj) = state.as_object_mut() {
                state_obj.insert("reads".into(), reads.clone());
            }
        }
        obj.insert(STATE_KEY.into(), state);
        break;
    }
    graph_store::locked_mutate_with_hook(
        graph,
        MutateInput {
            entries: working,
            canonical_path: None,
            base_version: base,
            plan_rungs: None,
        },
        std::time::Duration::from_secs(5),
        Some(&mut hook),
    )?;
    let journaled = journaled_flag.load(std::sync::atomic::Ordering::Relaxed);
    Ok(StateReceipt {
        node_id: node_id.to_string(),
        revision: expected + 1,
        total_prose: details + state,
        journaled,
    })
}

/// The named clear action: journals the exact outgoing state, then removes
/// `current_state` from the row. Revision-checked like a replacement.
pub fn clear_state(
    graph: &Path,
    node_id: &str,
    if_revision: Option<u64>,
) -> Result<(), StateError> {
    // x-385e: stamp the base BEFORE the read (see replace_state).
    let base = graph_store::base_version(graph)?;
    let rows = read_rows_for(graph)?;
    let row = rows
        .iter()
        .find(|r| graph_store::entry_id(r) == Some(node_id))
        .ok_or_else(|| StateError::NoNode(node_id.to_string()))?;
    let fetched = row_revision(row);
    if let Some(submitted) = if_revision {
        if submitted != fetched {
            return Err(StateError::Conflict {
                current_revision: fetched,
                submitted_revision: submitted,
            });
        }
    }
    let expected = if_revision.unwrap_or(fetched);
    if fetched == 0 {
        // Nothing to clear.
        return Ok(());
    }
    let session: Option<String> = None;
    let harness: Option<String> = None;
    let mut hook = |raw: &[Value]| -> Result<(), StoreError> {
        let row = raw
            .iter()
            .find(|r| graph_store::entry_id(r) == Some(node_id))
            .ok_or_else(|| {
                StoreError::Invalid(format!("state write target vanished: {node_id}"))
            })?;
        let current = row_revision(row);
        if current != expected {
            return Err(StoreError::Invalid(format!(
                "state-conflict: current revision {current} != submitted {expected}"
            )));
        }
        if let Some(pre_state) = row.get(STATE_KEY) {
            note_history::append(
                graph,
                node_id,
                note_history::REASON_STATE_CLEARED,
                Some(current),
                None,
                pre_state,
                session.as_deref(),
                harness.as_deref(),
            )
            .map_err(|e| StoreError::Invalid(format!("history write failed: {e}")))?;
        }
        Ok(())
    };

    let mut working = rows.clone();
    for row in working.iter_mut() {
        if graph_store::entry_id(row) != Some(node_id) {
            continue;
        }
        if let Some(obj) = row.as_object_mut() {
            obj.remove(STATE_KEY);
        }
        break;
    }
    graph_store::locked_mutate_with_hook(
        graph,
        MutateInput {
            entries: working,
            canonical_path: None,
            base_version: base,
            plan_rungs: None,
        },
        std::time::Duration::from_secs(5),
        Some(&mut hook),
    )?;
    Ok(())
}
