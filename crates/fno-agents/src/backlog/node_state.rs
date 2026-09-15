//! The single bounded node-state owner (wave 1).
//!
//! A node carries ONE `current_state` object in its extras: `{body, revision,
//! updated_at, source_session_id, source_harness}`. A note REPLACES it under
//! revision-checked optimistic concurrency; the exact pre-image is journaled
//! first (see `note_history`), so the hot node holds one current state and
//! every prior stays readable.
use crate::backlog::note_history;
use crate::graph_store::{self, StoreError};
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
/// with the exact pre-image journaled before publication. The whole cycle
/// runs through `graph_store::mutate_rows`: row lookup, budget and revision
/// checks judge a FRESH read each attempt, and the publish carries the
/// attempt's base version, so two writers with one base revision produce
/// exactly one commit and one retry.
pub fn replace_state(graph: &Path, input: &StateWriteInput) -> Result<StateReceipt, StateError> {
    let body = normalize_prose(&input.body);
    if body.is_empty() {
        return Err(StateError::EmptyBody);
    }
    let node_id = input.node_id.as_str();
    // An explicit stale --if-revision is a caller error, not a race: refuse
    // at once, never retry (note_cli exits 3 on this variant).
    if let Some(submitted) = input.if_revision {
        let current = current_revision(graph, node_id)?;
        if submitted != current {
            return Err(StateError::Conflict {
                current_revision: current,
                submitted_revision: submitted,
            });
        }
    }
    let session = input.source_session_id.clone();
    let harness = input.source_harness.clone();
    let journaled_flag = std::sync::atomic::AtomicBool::new(false);
    // apply and the hook agree on the attempt's own read through these; Cell
    // keeps both closures shared-borrow.
    let seen = std::cell::Cell::new(None::<(usize, u64)>);
    let expected_cell = std::cell::Cell::new(0u64);
    // Under the publication lock: re-verify the row revision, journal the
    // exact pre-image, then allow publication. Any history failure refuses
    // the whole mutation. (The old details/status snapshot check is
    // gone; the base version refuses any foreign change to any row.)
    let mut hook = |raw: &[Value]| -> Result<(), StoreError> {
        let row = raw
            .iter()
            .find(|r| graph_store::entry_id(r) == Some(node_id))
            .ok_or_else(|| {
                StoreError::Invalid(format!("state write target vanished: {node_id}"))
            })?;
        let expected = expected_cell.get();
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
    graph_store::mutate_rows(
        graph,
        std::time::Duration::from_secs(5),
        None,
        Some(&mut hook),
        |rows| {
            let Some(row) = rows
                .iter_mut()
                .find(|r| graph_store::entry_id(r) == Some(node_id))
            else {
                return Err(StoreError::Invalid(format!(
                    "state write target vanished: {node_id}"
                )));
            };
            let details = count_prose(row.get("details").and_then(Value::as_str).unwrap_or(""));
            let state = body.chars().count();
            if details + state > PROSE_LIMIT {
                return Err(StoreError::Invalid(budget_message(
                    node_id,
                    &json!({
                        STATE_KEY: {"body": body},
                        "details": row.get("details").and_then(Value::as_str).unwrap_or(""),
                    }),
                )));
            }
            let expected = row_revision(row);
            let obj = row.as_object_mut().unwrap();
            let mut state_obj = json!({
                "body": body,
                "revision": expected + 1,
                "updated_at": graph_store::now_isoformat(),
                "source_session_id": session,
                "source_harness": harness,
            });
            if let Some(reads) = &input.reads {
                if let Some(obj) = state_obj.as_object_mut() {
                    obj.insert("reads".into(), reads.clone());
                }
            }
            obj.insert(STATE_KEY.into(), state_obj);
            expected_cell.set(expected);
            seen.set(Some((details, expected)));
            Ok(true)
        },
    )?;
    let journaled = journaled_flag.load(std::sync::atomic::Ordering::Relaxed);
    let (details, expected) = seen.take().expect("apply ran at least once");
    Ok(StateReceipt {
        node_id: node_id.to_string(),
        revision: expected + 1,
        total_prose: details + body.chars().count(),
        journaled,
    })
}

/// The named clear action: journals the exact outgoing state, then removes
/// `current_state` from the row. Revision-checked like a replacement, on a
/// fresh read every attempt.
pub fn clear_state(
    graph: &Path,
    node_id: &str,
    if_revision: Option<u64>,
) -> Result<(), StateError> {
    // An explicit stale --if-revision refuses at once, never retries (see
    // replace_state). Also covers NoNode and nothing-to-clear up front.
    let current = current_revision(graph, node_id)?;
    if let Some(submitted) = if_revision {
        if submitted != current {
            return Err(StateError::Conflict {
                current_revision: current,
                submitted_revision: submitted,
            });
        }
    }
    if current == 0 {
        // Nothing to clear.
        return Ok(());
    }
    let mut hook = |raw: &[Value]| -> Result<(), StoreError> {
        let row = raw
            .iter()
            .find(|r| graph_store::entry_id(r) == Some(node_id))
            .ok_or_else(|| {
                StoreError::Invalid(format!("state write target vanished: {node_id}"))
            })?;
        let row_current = row_revision(row);
        if row_current != current {
            return Err(StoreError::Invalid(format!(
                "state-conflict: current revision {row_current} != submitted {current}"
            )));
        }
        if let Some(pre_state) = row.get(STATE_KEY) {
            note_history::append(
                graph,
                node_id,
                note_history::REASON_STATE_CLEARED,
                Some(row_current),
                None,
                pre_state,
                None,
                None,
            )
            .map_err(|e| StoreError::Invalid(format!("history write failed: {e}")))?;
        }
        Ok(())
    };

    graph_store::mutate_rows(
        graph,
        std::time::Duration::from_secs(5),
        None,
        Some(&mut hook),
        |rows| {
            let Some(row) = rows
                .iter_mut()
                .find(|r| graph_store::entry_id(r) == Some(node_id))
            else {
                return Ok(false);
            };
            if let Some(obj) = row.as_object_mut() {
                obj.remove(STATE_KEY);
            }
            Ok(true)
        },
    )?;
    Ok(())
}
