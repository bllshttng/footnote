//! The typed backlog API: every query and mutation a caller needs, in
//! Linear's shape, with the backend switch inside this file. A caller moves
//! onto this surface once and never learns which store answered; the JSON
//! read arm and the shadow write are what task 17.1 deletes at the flip.
//!
//! Composition rule (AC12): no SQL lives here and no table is named in a
//! string. Reads go through `graph_store`'s read pipeline (JSON backend) or
//! the owning modules' export (SQLite backend); every write routes through
//! `graph_store::locked_mutate`, which writes the JSON leg plus the
//! relational shadow, or the backend-owned tables, per the backend the
//! store names right now. One transaction per mutation; the store's
//! mutation counter (`graph_meta.api_version`) moves by one per write, so
//! `version` grows across legacy writers too.

// The contract's vocabulary: api speaks (and re-exports) the model types,
// so a caller imports them from here and the names stay single-sourced.
pub use crate::backlog::model::{
    Comment, Dispatch, Encounter, Finding, Node, Priority, PullRequest, RelationType,
    SessionRecord, StateType, Status,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MUTATE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct ApiError(pub String);

impl From<crate::graph_store::StoreError> for ApiError {
    fn from(error: crate::graph_store::StoreError) -> Self {
        Self(error.to_string())
    }
}

impl From<ApiError> for crate::graph_store::StoreError {
    fn from(error: ApiError) -> Self {
        Self::Invalid(error.0)
    }
}

impl From<String> for ApiError {
    fn from(error: String) -> Self {
        Self(error)
    }
}

/// One store, named by its graph.json path. The keeper builds one per
/// request; tests build one per fixture.
#[derive(Clone, Debug)]
pub struct Store {
    pub graph: PathBuf,
}

impl Store {
    pub fn new(graph: &Path) -> Self {
        Self {
            graph: graph.to_path_buf(),
        }
    }
}

pub use crate::backlog::model::state_type;
pub use crate::backlog::search::search;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderBy {
    #[default]
    Ordinal,
    CreatedAt,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Page {
    #[serde(default)]
    pub first: Option<usize>,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(default)]
    pub order_by: OrderBy,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            first: None,
            after: None,
            include_archived: false,
            order_by: OrderBy::Ordinal,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct PageInfo {
    pub has_next_page: bool,
    pub has_previous_page: bool,
    pub end_cursor: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Connection<T> {
    pub nodes: Vec<T>,
    pub page_info: PageInfo,
}

#[derive(Clone, Debug)]
pub struct Payload<T> {
    pub success: bool,
    pub node: Option<T>,
    pub version: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct NodeFilter {
    #[serde(default)]
    pub id_in: Option<Vec<String>>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub status_in: Option<Vec<String>>,
    #[serde(default)]
    pub state_type: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub claimed: Option<bool>,
    #[serde(default)]
    pub session_id: Option<String>,
}

// -- inputs ----------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize)]
pub struct NodeCreateInput {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub plan_path: Option<String>,
    #[serde(default)]
    pub estimate: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct NodeUpdateInput {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub plan_path: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub estimate: Option<String>,
}

impl NodeUpdateInput {
    /// Apply the sparse update to a typed row. `Err` is a domain refusal
    /// (unknown priority word): the caller answers success:false and the
    /// store stays untouched. Status is NOT here: it is a derived column,
    /// so the caller routes it through the patch planner (`patch::
    /// plan_status_on_rows`) before this runs.
    fn apply(&self, node: &mut Node) -> Result<(), String> {
        if let Some(title) = &self.title {
            node.title = title.clone();
        }
        if let Some(priority) = &self.priority {
            node.priority = Priority::parse(priority).map_err(|e| e.0)?;
        }
        if let Some(project) = &self.project {
            node.project = Some(project.clone());
        }
        if let Some(parent) = &self.parent {
            node.parent = Some(parent.clone());
        }
        if let Some(plan_path) = &self.plan_path {
            node.plan_path = Some(plan_path.clone());
        }
        if let Some(description) = &self.description {
            node.description = Some(description.clone());
        }
        if let Some(estimate) = &self.estimate {
            node.estimate = Some(estimate.clone());
        }
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.status.is_none()
            && self.priority.is_none()
            && self.project.is_none()
            && self.parent.is_none()
            && self.plan_path.is_none()
            && self.description.is_none()
            && self.estimate.is_none()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct CommentCreateInput {
    pub body: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PullRequestInput {
    pub number: i64,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct EncounterInput {
    pub evidence: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

// -- cursor ----------------------------------------------------------------

/// Opaque base64 of `(ordinal, id)`: enough to resume a page after a
/// concurrent insert without exposing the ordering implementation.
fn encode_cursor(node: &Node) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{}:{}", node.ordinal, node.id))
}

fn decode_cursor(cursor: &str) -> Option<(i64, String)> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (ordinal, id) = text.split_once(':')?;
    Some((ordinal.parse().ok()?, id.to_string()))
}

// -- reads -----------------------------------------------------------------

/// Every row, in store order (file position under the JSON leg, ordinal
/// order under SQLite). The position in this list IS the ordinal the
/// pagination cursors name, in both arms. The rows surface defaulted
/// through the one backend switch, `graph_store::read_rows`.
fn read_rows(store: &Store) -> Result<Vec<Value>, ApiError> {
    Ok(crate::graph_store::read_rows(&store.graph)?)
}

/// The pure read halves, over rows the caller already holds: the keeper
/// feeds them from the cache, the store-reading functions delegate here so
/// the two halves cannot drift. `defaulted` is read_rows' tail; `node_in`,
/// `nodes_in` and `rows_in` are `node`, `nodes` and `rows` without the
/// store read. A row the model cannot represent is skipped, the import's
/// rule (the JSON leg stays authoritative and parity surfaces the gap);
/// `rows_in` carries such a row through VERBATIM, so a reader seam cannot
/// silently drop it.
pub fn defaulted(mut rows: Vec<Value>) -> Vec<Value> {
    crate::graph_store::apply_defaults(&mut rows, false);
    rows
}

pub fn node_in(rows: &[Value], id: &str) -> Option<Node> {
    rows.iter()
        .enumerate()
        .filter_map(|(ordinal, row)| {
            Node::from_json(row).ok().map(|mut node| {
                node.ordinal = ordinal as i64;
                node
            })
        })
        .find(|n| n.id == id)
}

/// Filter, then drop archived rows unless asked. Ordering and pagination
/// happen in [`nodes`], so `first` counts the rows the caller would see.
pub fn nodes_in(rows: &[Value], filter: &NodeFilter, page: &Page) -> Connection<Node> {
    let mut rows: Vec<Node> = rows
        .iter()
        .enumerate()
        .filter_map(|(ordinal, row)| {
            Node::from_json(row).ok().map(|mut node| {
                node.ordinal = ordinal as i64;
                node
            })
        })
        .filter(|n| filter_matches(n, filter))
        .filter(|n| page.include_archived || n.archived_at.is_none())
        .collect();
    match page.order_by {
        OrderBy::Ordinal => rows.sort_by_key(|n| n.ordinal),
        OrderBy::CreatedAt => rows.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.ordinal.cmp(&b.ordinal))
        }),
    }
    paginate(rows, page)
}

fn filter_matches(node: &Node, filter: &NodeFilter) -> bool {
    if let Some(ids) = &filter.id_in {
        if !ids.iter().any(|id| id == &node.id) {
            return false;
        }
    }
    if let Some(project) = &filter.project {
        if node.project.as_deref() != Some(project.as_str()) {
            return false;
        }
    }
    if let Some(statuses) = &filter.status_in {
        let parsed: Result<Vec<Status>, _> = statuses.iter().map(|s| Status::parse(s)).collect();
        match parsed {
            Ok(wanted) if !wanted.contains(&node.status) => return false,
            Err(_) => return false,
            _ => {}
        }
    }
    if let Some(name) = &filter.state_type {
        let wanted = match name.as_str() {
            "unstarted" => StateType::Unstarted,
            "started" => StateType::Started,
            "completed" => StateType::Completed,
            "canceled" => StateType::Canceled,
            _ => return false,
        };
        if state_type(node.status) != wanted {
            return false;
        }
    }
    if let Some(parent) = &filter.parent {
        if node.parent.as_deref() != Some(parent.as_str()) {
            return false;
        }
    }
    if let Some(label) = &filter.label {
        let hit = node
            .labels
            .as_ref()
            .is_some_and(|tags| tags.iter().any(|tag| tag == label));
        if !hit {
            return false;
        }
    }
    if let Some(claimed) = &filter.claimed {
        let held = node.claim.locked_by.is_some();
        if held != *claimed {
            return false;
        }
    }
    if let Some(session) = &filter.session_id {
        let hit = node
            .sessions
            .as_ref()
            .is_some_and(|rows| rows.iter().any(|row| &row.session_id == session));
        if !hit {
            return false;
        }
    }
    true
}

pub fn node(store: &Store, id: &str) -> Result<Option<Node>, ApiError> {
    Ok(node_in(&read_rows(store)?, id))
}

fn paginate(rows: Vec<Node>, page: &Page) -> Connection<Node> {
    let total = rows.len();
    let start = match page.after.as_deref().and_then(decode_cursor) {
        // Identity match, not an ordering scan: the resume must hold under
        // every OrderBy, and a cursor whose row vanished between pages
        // degrades to a restart, never to a silent skip.
        Some((ordinal, id)) => rows
            .iter()
            .position(|n| n.ordinal == ordinal && n.id == id)
            .map_or(0, |i| i + 1),
        None => 0,
    };
    let end = match page.first {
        Some(first) => (start + first).min(total),
        None => total,
    };
    let nodes: Vec<Node> = rows[start..end].to_vec();
    let end_cursor = nodes.last().map(encode_cursor);
    Connection {
        nodes,
        page_info: PageInfo {
            has_next_page: end < total,
            has_previous_page: start > 0,
            end_cursor,
        },
    }
}

pub fn nodes(
    store: &Store,
    filter: &NodeFilter,
    page: &Page,
) -> Result<Connection<Node>, ApiError> {
    Ok(nodes_in(&read_rows(store)?, filter, page))
}

/// Every working-graph row through the store, in ordinal order. The one
/// read the wave-8 board readers share: a caller that folds over `Value`
/// rows asks the store instead of opening graph.json. The typed
/// normalization applies where the model can carry the row; a row it cannot
/// represent rides through VERBATIM, so the reader seam stays total and a
/// legacy-shaped row can never silently vanish from a board fold (the typed
/// queries drop it, the import's rule; a reader must not). Archived rows
/// come back; the caller filters.
pub fn rows(store: &Store) -> Result<Vec<Value>, ApiError> {
    Ok(rows_in(&read_rows(store)?))
}

/// Every row, round-tripped through the model where it fits, verbatim
/// where it does not - the pure half of [`rows`].
pub fn rows_in(rows: &[Value]) -> Vec<Value> {
    rows.iter()
        .map(|row| {
            Node::from_json(row)
                .map(|node| node.to_json())
                .unwrap_or_else(|_| row.clone())
        })
        .collect()
}

/// The progress notes of one node, newest last (store order), paged.
pub fn comments(
    store: &Store,
    node_id: &str,
    page: &Page,
) -> Result<Connection<Comment>, ApiError> {
    let node = node(store, node_id)?.ok_or_else(|| ApiError(format!("no node {node_id}")))?;
    let rows: Vec<Comment> = node.comments.clone().unwrap_or_default();
    let total = rows.len();
    let start = match page.after.as_deref().and_then(decode_cursor) {
        Some((ordinal, _)) => (ordinal as usize + 1).min(total),
        None => 0,
    };
    let end = match page.first {
        Some(first) => (start + first).min(total),
        None => total,
    };
    let slice: Vec<Comment> = rows[start..end].to_vec();
    let end_cursor = slice.last().map(|_| {
        use base64::Engine as _;
        // Same `(ordinal, id)` codec as the row cursor, with the node id in
        // the id slot, so decode_cursor can always split it.
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{}:{}", end - 1, node_id))
    });
    Ok(Connection {
        nodes: slice,
        page_info: PageInfo {
            has_next_page: end < total,
            has_previous_page: start > 0,
            end_cursor,
        },
    })
}

/// The store's mutation counter: one bump per write, legacy writers
/// included. Absent store reads 0.
pub fn version(store: &Store) -> Result<i64, ApiError> {
    Ok(crate::backlog::api_version(&store.graph)?)
}

// -- writes ----------------------------------------------------------------

/// Read, apply the typed mutation to the named rows, publish through
/// `graph_store::mutate_rows` (the one optimistic cycle every whole-graph
/// writer shares). `Ok(false)` from `apply` is a domain refusal: nothing is
/// written and the counter stays put, which is AC15's failed-mutation arm.
pub fn decisions(
    store: &Store,
    node: Option<&str>,
    decision_id: Option<&str>,
) -> Result<Vec<Value>, ApiError> {
    let connection = crate::backlog::open(&store.graph)?;
    let rows = match node {
        Some(node_id) => crate::backlog::decisions::node_decisions(&connection, node_id)?,
        None => {
            let mut rows = crate::backlog::decisions::read_rows(&connection)?.0;
            if let Some(decision_id) = decision_id {
                rows.retain(|row| {
                    row.get("decision_id").and_then(Value::as_str) == Some(decision_id)
                });
            }
            rows
        }
    };
    Ok(rows)
}

pub fn decision_record(store: &Store, event: Value) -> Result<Payload<Value>, ApiError> {
    let mut connection = crate::backlog::open(&store.graph)?;
    crate::backlog::decisions::record_connected(&mut connection, &event, "decision_record")
        .map_err(ApiError)?;
    Ok(Payload {
        success: true,
        node: Some(event),
        version: fresh_version(store),
    })
}

pub fn decision_retract(store: &Store, event: Value) -> Result<Payload<Value>, ApiError> {
    let mut connection = crate::backlog::write_connection(&store.graph)?;
    crate::backlog::decisions::record_connected(&mut connection, &event, "decision_retract")
        .map_err(ApiError)?;
    Ok(Payload {
        success: true,
        node: Some(event),
        version: fresh_version(store),
    })
}

fn mutate(
    store: &Store,
    mutation: &str,
    mut apply: impl FnMut(&mut Vec<Value>) -> Result<bool, String>,
) -> Result<bool, ApiError> {
    // The store owns a pre-materialization seed too: with no db on disk the
    // single-row path's first open folds the seed, so an unnamed store never
    // falls to the json leg just for being young. Only a store explicitly
    // named json keeps the whole-graph rollback cycle.
    let named_json = crate::backlog::backend(&store.graph) == crate::backlog::Backend::Json
        && crate::backlog::database_path(&store.graph).exists();
    if !named_json {
        // Single-row path: the immediate transaction reads
        // the current authoritative rows, writes only the changed node's
        // aggregates, and the gate event names this mutation (AC24).
        return crate::backlog::mutate_single_row(&store.graph, mutation, |rows| apply(rows))
            .map_err(ApiError);
    }
    let landed =
        crate::graph_store::mutate_rows(&store.graph, MUTATE_TIMEOUT, None, None, |rows| {
            apply(rows).map_err(crate::graph_store::StoreError::Invalid)
        })?;
    Ok(landed.is_some())
}

fn fresh_version(store: &Store) -> i64 {
    version(store).unwrap_or(0)
}

fn refusal<T>(store: &Store) -> Result<Payload<T>, ApiError> {
    Ok(Payload {
        success: false,
        node: None,
        version: fresh_version(store),
    })
}

pub fn node_create(store: &Store, input: NodeCreateInput) -> Result<Payload<Node>, ApiError> {
    let status = match &input.status {
        Some(word) => match Status::parse(word) {
            Ok(status) => status,
            Err(_) => return refusal(store),
        },
        None => Status::Idea,
    };
    let priority = match &input.priority {
        Some(word) => match Priority::parse(word) {
            Ok(priority) => priority,
            Err(_) => return refusal(store),
        },
        None => Priority::P2,
    };
    let mut row = json!({
        "id": input.id,
        "slug": crate::graph_store::derive_base_slug(&input.title),
        "title": input.title,
        "type": "feature",
        "status": status.as_str(),
        "priority": priority.as_str(),
        "domain": "code",
        "created_at": crate::graph_store::now_isoformat(),
    });
    if let Some(project) = &input.project {
        row["project"] = json!(project);
    }
    if let Some(parent) = &input.parent {
        row["parent"] = json!(parent);
    }
    if let Some(plan_path) = &input.plan_path {
        row["plan_path"] = json!(plan_path);
    }
    if let Some(estimate) = &input.estimate {
        row["size"] = json!(estimate);
    }
    if let Some(description) = &input.description {
        row["details"] = json!(description);
    }
    let id = row["id"].as_str().unwrap_or_default().to_string();
    let mut created: Option<Node> = None;
    let ok = mutate(store, "node_create", |rows| {
        if rows
            .iter()
            .any(|row| crate::graph_store::entry_id(row) == Some(id.as_str()))
        {
            return Ok(false);
        }
        rows.push(row.clone());
        created = Node::from_json(&row).ok();
        Ok(true)
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: created,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

pub fn node_update(
    store: &Store,
    id: &str,
    input: NodeUpdateInput,
) -> Result<Payload<Node>, ApiError> {
    if input.is_empty() {
        return refusal(store);
    }
    let mut updated: Option<Node> = None;
    let ok = mutate(store, "node_update", |rows| {
        // The status arm goes through the patch door: status is derived, so
        // the planner changes the facts and validates the readback. A
        // refusal (a done node, a plan-less ready, an owned transition)
        // answers success:false with the rows untouched (AC9).
        if let Some(word) = &input.status {
            if crate::backlog::patch::plan_status_on_rows(rows, id, word).is_err() {
                return Ok(false);
            }
        }
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(id) {
                continue;
            }
            let mut parsed = match Node::from_json(row) {
                Ok(parsed) => parsed,
                Err(_) => return Ok(false),
            };
            if input.apply(&mut parsed).is_err() {
                return Ok(false);
            }
            *row = parsed.to_json();
            updated = Some(parsed);
            break;
        }
        Ok(updated.is_some())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

pub fn node_batch_update(
    store: &Store,
    ids: &[String],
    input: NodeUpdateInput,
) -> Result<Payload<Vec<Node>>, ApiError> {
    if input.is_empty() || ids.is_empty() {
        return refusal(store);
    }
    let wanted: std::collections::BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    let mut updated: Vec<Node> = Vec::new();
    let ok = mutate(store, "node_batch_update", |rows| {
        // Same door as the single update: every wanted id's status moves
        // through the planner, and one refusal refuses the batch.
        if let Some(word) = &input.status {
            for id in &wanted {
                if crate::backlog::patch::plan_status_on_rows(rows, id, word).is_err() {
                    return Ok(false);
                }
            }
        }
        for row in rows.iter_mut() {
            let Some(id) = crate::graph_store::entry_id(row) else {
                continue;
            };
            if !wanted.contains(id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                continue;
            };
            if input.apply(&mut parsed).is_err() {
                return Ok(false);
            }
            *row = parsed.to_json();
            updated.push(parsed);
        }
        Ok(updated.len() == wanted.len())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: Some(updated),
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

pub fn node_archive(store: &Store, id: &str) -> Result<Payload<Node>, ApiError> {
    stamp_archived(store, id, true)
}

pub fn node_unarchive(store: &Store, id: &str) -> Result<Payload<Node>, ApiError> {
    stamp_archived(store, id, false)
}

fn stamp_archived(store: &Store, id: &str, archived: bool) -> Result<Payload<Node>, ApiError> {
    let mut updated: Option<Node> = None;
    let name = if archived {
        "node_archive"
    } else {
        "node_unarchive"
    };
    let ok = mutate(store, name, |rows| {
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                return Ok(false);
            };
            if archived {
                parsed.archived_at = Some(crate::graph_store::now_isoformat());
            } else {
                parsed.archived_at = None;
            }
            *row = parsed.to_json();
            updated = Some(parsed);
            break;
        }
        Ok(updated.is_some())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

pub fn node_delete(store: &Store, id: &str) -> Result<Payload<Node>, ApiError> {
    let mut deleted: Option<Node> = None;
    let mut kept: Vec<Value> = Vec::new();
    let ok = mutate(store, "node_delete", |rows| {
        for row in rows.drain(..) {
            if crate::graph_store::entry_id(&row) == Some(id) {
                deleted = Node::from_json(&row).ok();
            } else {
                kept.push(row);
            }
        }
        rows.append(&mut kept);
        Ok(deleted.is_some())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: deleted,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

fn edge_list(
    store: &Store,
    node_id: &str,
    related: &str,
    t: RelationType,
    add: bool,
) -> Result<Payload<Node>, ApiError> {
    let mut updated: Option<Node> = None;
    let name = if add {
        "relation_create"
    } else {
        "relation_delete"
    };
    let ok = mutate(store, name, |rows| {
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(node_id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                return Ok(false);
            };
            let bucket = match t {
                RelationType::Blocks => &mut parsed.relations.blocked_by,
                RelationType::Related => &mut parsed.relations.related,
                RelationType::Supersedes => &mut parsed.relations.supersedes,
            };
            let list = bucket.get_or_insert_with(Vec::new);
            if add {
                if !list.iter().any(|id| id == related) {
                    list.push(related.to_string());
                }
            } else {
                list.retain(|id| id != related);
            }
            *row = parsed.to_json();
            updated = Some(parsed);
            break;
        }
        Ok(updated.is_some())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

pub fn relation_create(
    store: &Store,
    node_id: &str,
    related: &str,
    t: RelationType,
) -> Result<Payload<Node>, ApiError> {
    edge_list(store, node_id, related, t, true)
}

pub fn relation_delete(
    store: &Store,
    node_id: &str,
    related: &str,
    t: RelationType,
) -> Result<Payload<Node>, ApiError> {
    edge_list(store, node_id, related, t, false)
}

fn label_mutation(
    store: &Store,
    id: &str,
    name: &str,
    add: bool,
) -> Result<Payload<Node>, ApiError> {
    let mut updated: Option<Node> = None;
    let gate_mutation = if add { "label_add" } else { "label_remove" };
    let ok = mutate(store, gate_mutation, |rows| {
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                return Ok(false);
            };
            let list = parsed.labels.get_or_insert_with(Vec::new);
            if add {
                if !list.iter().any(|tag| tag == name) {
                    list.push(name.to_string());
                }
            } else {
                list.retain(|tag| tag != name);
            }
            *row = parsed.to_json();
            updated = Some(parsed);
            break;
        }
        Ok(updated.is_some())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

pub fn label_add(store: &Store, id: &str, name: &str) -> Result<Payload<Node>, ApiError> {
    label_mutation(store, id, name, true)
}

pub fn label_remove(store: &Store, id: &str, name: &str) -> Result<Payload<Node>, ApiError> {
    label_mutation(store, id, name, false)
}

pub fn comment_create(
    store: &Store,
    id: &str,
    input: CommentCreateInput,
) -> Result<Payload<Node>, ApiError> {
    let mut updated: Option<Node> = None;
    let ok = mutate(store, "comment_create", |rows| {
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                return Ok(false);
            };
            parsed.comments.get_or_insert_with(Vec::new).push(Comment {
                created_at: Some(crate::graph_store::now_isoformat()),
                body: Some(input.body.clone()),
                kind: input.kind.clone(),
                title: input.title.clone(),
                details: None,
                difficulty: None,
                source: None,
                source_session_id: None,
                source_harness: None,
                extras: serde_json::Map::new(),
            });
            *row = parsed.to_json();
            updated = Some(parsed);
            break;
        }
        Ok(updated.is_some())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct FindingInput {
    pub body: String,
    #[serde(default)]
    pub block_cmd: Option<String>,
    #[serde(default)]
    pub block_excerpt: Option<String>,
    #[serde(default)]
    pub source_session_id: Option<String>,
    #[serde(default)]
    pub source_harness: Option<String>,
}

#[derive(Clone, Debug)]
pub struct FindingReceipt {
    pub finding_id: String,
    pub node_id: String,
    pub version: i64,
}

#[derive(Clone, Debug)]
pub struct ResolveReceipt {
    pub finding_id: String,
    pub status: String,
    pub resolved_at: String,
    pub version: i64,
}

/// One blocking review finding on a node. The finding id mints inside the
/// mutation (re-minting while it collides with any stored finding), and the
/// receipt returns only after the commit and the readback both succeed: a
/// write that did not persist is an error, never a receipt.
pub fn finding_create(
    store: &Store,
    node_id: &str,
    input: FindingInput,
) -> Result<FindingReceipt, ApiError> {
    const EXCERPT_LIMIT: usize = 2048;
    let body = crate::backlog::node_state::normalize_prose(&input.body);
    if body.is_empty() {
        return Err(ApiError("finding body is empty".into()));
    }
    if crate::backlog::node_state::count_prose(&body) > crate::backlog::node_state::PROSE_LIMIT {
        return Err(ApiError(format!(
            "finding body exceeds the {} character prose limit",
            crate::backlog::node_state::PROSE_LIMIT
        )));
    }
    let mut excerpt = input.block_excerpt.clone();
    if let Some(text) = &mut excerpt {
        if text.chars().count() > EXCERPT_LIMIT {
            *text = text.chars().take(EXCERPT_LIMIT).collect();
        }
    }
    let mut minted = String::new();
    let ok = mutate(store, "finding_create", |rows| {
        if !rows
            .iter()
            .any(|row| crate::graph_store::entry_id(row) == Some(node_id))
        {
            return Ok(false);
        }
        loop {
            minted = mint_finding_id();
            let taken = rows
                .iter()
                .any(|row| finding_ids_in(row).iter().any(|id| id == &minted));
            if !taken {
                break;
            }
        }
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(node_id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                return Ok(false);
            };
            parsed.findings.get_or_insert_with(Vec::new).push(Finding {
                finding_id: minted.clone(),
                created_at: Some(crate::graph_store::now_isoformat()),
                body: body.clone(),
                block_cmd: input.block_cmd.clone(),
                block_excerpt: excerpt.clone(),
                source_session_id: input.source_session_id.clone(),
                source_harness: input.source_harness.clone(),
                resolved_at: None,
                resolved_by_session_id: None,
            });
            *row = parsed.to_json();
            break;
        }
        Ok(true)
    })?;
    if !ok {
        return Err(ApiError(format!("node {node_id} not found")));
    }
    let rows = read_rows(store)?;
    let stored = rows
        .iter()
        .find(|row| crate::graph_store::entry_id(row) == Some(node_id))
        .and_then(|row| row.get("findings").and_then(Value::as_array))
        .map(|items| {
            items
                .iter()
                .any(|item| item.get("finding_id").and_then(Value::as_str) == Some(minted.as_str()))
        })
        .unwrap_or(false);
    if !stored {
        return Err(ApiError(format!(
            "finding {minted} committed but not read back"
        )));
    }
    Ok(FindingReceipt {
        finding_id: minted,
        node_id: node_id.to_string(),
        version: fresh_version(store),
    })
}

fn mint_finding_id() -> String {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn finding_ids_in(row: &Value) -> Vec<String> {
    row.get("findings")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("finding_id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Stamp `resolved_at` on a finding. An unknown id is an error; an
/// already-resolved id returns `already_resolved` with the prior
/// resolved_at and wrote nothing this call.
pub fn finding_resolve(
    store: &Store,
    finding_id: &str,
    session: Option<&str>,
) -> Result<ResolveReceipt, ApiError> {
    let mut seen = false;
    let mut prior: Option<String> = None;
    let mut stamped: Option<String> = None;
    mutate(store, "finding_resolve", |rows| {
        for row in rows.iter_mut() {
            let Ok(mut parsed) = Node::from_json(row) else {
                continue;
            };
            let Some(list) = &mut parsed.findings else {
                continue;
            };
            let Some(finding) = list.iter_mut().find(|f| f.finding_id == finding_id) else {
                continue;
            };
            seen = true;
            if let Some(when) = &finding.resolved_at {
                prior = Some(when.clone());
                return Ok(false);
            }
            let now = crate::graph_store::now_isoformat();
            finding.resolved_at = Some(now.clone());
            finding.resolved_by_session_id = session.map(str::to_string);
            stamped = Some(now);
            *row = parsed.to_json();
            return Ok(true);
        }
        Ok(false)
    })?;
    let (status, resolved_at) = match (stamped, prior) {
        (Some(now), _) => ("resolved", now),
        (None, Some(before)) => ("already_resolved", before),
        (None, None) => return Err(ApiError(format!("unknown finding {finding_id}"))),
    };
    Ok(ResolveReceipt {
        finding_id: finding_id.to_string(),
        status: status.into(),
        resolved_at,
        version: fresh_version(store),
    })
}

/// Open or resolved findings, optionally scoped to one node. A store read
/// error is Err, never an empty list.
pub fn findings(
    store: &Store,
    node_id: Option<&str>,
    open_only: bool,
) -> Result<Vec<Finding>, ApiError> {
    let mut out: Vec<Finding> = Vec::new();
    for row in read_rows(store)? {
        let Ok(node) = Node::from_json(&row) else {
            continue;
        };
        if let Some(id) = node_id {
            if node.id != id {
                continue;
            }
        }
        for finding in node.findings.unwrap_or_default() {
            if open_only && finding.resolved_at.is_some() {
                continue;
            }
            out.push(finding);
        }
    }
    Ok(out)
}

pub fn pull_request_attach(
    store: &Store,
    id: &str,
    input: PullRequestInput,
) -> Result<Payload<Node>, ApiError> {
    let pr = PullRequest {
        number: Some(input.number),
        url: input.url,
        merge_status: None,
        note: input.note,
        extras: serde_json::Map::new(),
    };
    let mut updated: Option<Node> = None;
    let ok = mutate(store, "pull_request_attach", |rows| {
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                return Ok(false);
            };
            if parsed.primary_pr.is_none() {
                parsed.primary_pr = Some(pr.clone());
            } else {
                parsed
                    .additional_prs
                    .get_or_insert_with(Vec::new)
                    .push(pr.clone());
            }
            *row = parsed.to_json();
            updated = Some(parsed);
            break;
        }
        Ok(updated.is_some())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

/// Stamp one `additional_prs` entry's outcome. Matches one entry by
/// `number`, and by `url` when the caller passes one. An entry that is
/// already settled (`merged` or `closed`), or no match at all, writes
/// nothing and returns `success: false`.
pub fn pull_request_stamp(
    store: &Store,
    id: &str,
    number: i64,
    url: Option<&str>,
    merge_status: &str,
) -> Result<Payload<Node>, ApiError> {
    let mut updated: Option<Node> = None;
    let ok = mutate(store, "pull_request_stamp", |rows| {
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                return Ok(false);
            };
            let Some(extras) = parsed.additional_prs.as_mut() else {
                return Ok(false);
            };
            let matched = extras.iter_mut().find(|extra| {
                if extra.number != Some(number) {
                    return false;
                }
                match url {
                    Some(want) => {
                        extra
                            .url
                            .as_deref()
                            .map(crate::additional_prs::normalize_url)
                            == Some(crate::additional_prs::normalize_url(want))
                    }
                    None => true,
                }
            });
            let Some(extra) = matched else {
                return Ok(false);
            };
            if matches!(
                extra.merge_status.as_deref(),
                Some("merged") | Some("closed")
            ) {
                return Ok(false);
            }
            extra.merge_status = Some(merge_status.to_string());
            *row = parsed.to_json();
            updated = Some(parsed);
            return Ok(true);
        }
        Ok(false)
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

/// Stamp the primary PR's outcome. Matches the node's own `primary_pr` by
/// `number`, and by `url` when the caller passes one. Only an unrecorded
/// `merge_status` is filled: any recorded value, a mismatched number or
/// url, or no primary at all writes nothing and returns `success: false`.
pub fn primary_pr_stamp(
    store: &Store,
    id: &str,
    number: i64,
    url: Option<&str>,
    merge_status: &str,
) -> Result<Payload<Node>, ApiError> {
    let mut updated: Option<Node> = None;
    let ok = mutate(store, "primary_pr_stamp", |rows| {
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                return Ok(false);
            };
            let Some(primary) = parsed.primary_pr.as_mut() else {
                return Ok(false);
            };
            if primary.number != Some(number) {
                return Ok(false);
            }
            if let Some(want) = url {
                let matches = primary
                    .url
                    .as_deref()
                    .map(crate::additional_prs::normalize_url)
                    == Some(crate::additional_prs::normalize_url(want));
                if !matches {
                    return Ok(false);
                }
            }
            if primary.merge_status.is_some() {
                return Ok(false);
            }
            primary.merge_status = Some(merge_status.to_string());
            *row = parsed.to_json();
            updated = Some(parsed);
            return Ok(true);
        }
        Ok(false)
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

pub fn session_append(
    store: &Store,
    id: &str,
    row: SessionRecord,
) -> Result<Payload<Node>, ApiError> {
    let mut updated: Option<Node> = None;
    let ok = mutate(store, "session_append", |rows| {
        for row_json in rows.iter_mut() {
            if crate::graph_store::entry_id(row_json) != Some(id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row_json) else {
                return Ok(false);
            };
            parsed
                .sessions
                .get_or_insert_with(Vec::new)
                .push(row.clone());
            *row_json = parsed.to_json();
            updated = Some(parsed);
            break;
        }
        Ok(updated.is_some())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

/// The deferred sessions-row open (the `pending-session-row` transport arm).
/// Builds the record through the keeper's validating constructor and appends
/// through the keeper's idempotent append - one implementation of the
/// semantics, not a second one. Answers whether the node was found at all.
/// The caller clears the registry park AFTER this returns Ok, so a failed
/// graph write keeps the payload.
#[allow(clippy::too_many_arguments)]
pub fn session_open_parked(
    store: &Store,
    node_id: &str,
    phase: &str,
    harness: &str,
    session_id: &str,
    effort: Option<&str>,
    merge_grant: Option<Value>,
    started_at: &str,
) -> Result<bool, ApiError> {
    let record = crate::graph_keeper::session_row(
        phase,
        harness,
        session_id,
        effort,
        Some(started_at),
        None,
        None,
        merge_grant.as_ref(),
    )
    .map_err(|e| ApiError(e.to_string()))?;
    let mut found = false;
    mutate(store, "session_append", |rows| {
        let (node_found, _added) =
            crate::graph_keeper::session_append(rows, node_id, record.clone())
                .map_err(|e| e.to_string())?;
        found = node_found;
        Ok(node_found)
    })?;
    Ok(found)
}

pub fn session_end(
    store: &Store,
    id: &str,
    session_id: &str,
    ended_by: &str,
    phase: Option<&str>,
    harness: Option<&str>,
    ended_at: Option<&str>,
) -> Result<Payload<Node>, ApiError> {
    // A session may hold several open rows on one node (one per phase), so a
    // settle that matches on session_id alone would fabricate ended_at on
    // unrelated think/review/ship provenance. Callers name the phase and
    // harness they are settling; a record only matches when they agree.
    let matches_window = |rec_phase: Option<&str>, rec_harness: Option<&str>| -> bool {
        phase.map_or(true, |want| rec_phase == Some(want))
            && harness.map_or(true, |want| rec_harness == Some(want))
    };
    // One resolved instant feeds both fill branches so they cannot drift;
    // the fallback is the Z form the session rows read, not now_isoformat's
    // microsecond offset form.
    let stamp = ended_at.map_or_else(
        || chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        str::to_string,
    );
    let mut updated: Option<Node> = None;
    let ok = mutate(store, "session_end", |rows| {
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                // A row the typed model cannot represent still owes its
                // close: fill ended_at on the raw session rows rather than
                // skipping the settle forever (the sweep would re-list the
                // same stale row every pass). The payload carries no typed
                // node on this path. The sessions aggregate owns the raw
                // row shape, so the fill composes through it.
                return Ok(crate::backlog::sessions::fill_open_window_raw(
                    row,
                    session_id,
                    phase,
                    harness,
                    stamp.as_str(),
                    ended_by,
                ));
            };
            let Some(list) = &mut parsed.sessions else {
                return Ok(false);
            };
            let mut closed = false;
            for record in list.iter_mut() {
                if record.session_id == session_id
                    && record.ended_at.is_none()
                    && matches_window(Some(&record.phase), Some(&record.harness))
                {
                    record.ended_at = Some(stamp.clone());
                    record.ended_by = Some(ended_by.to_string());
                    closed = true;
                }
            }
            if !closed {
                return Ok(false);
            }
            *row = parsed.to_json();
            updated = Some(parsed);
            break;
        }
        Ok(updated.is_some())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

/// One session record's recorded stamps, for [`session_backfill`].
pub struct SessionFill {
    pub phase: String,
    pub harness: String,
    pub session_id: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub ended_by: String,
}

/// Fill missing `started_at` and `ended_at` on session records from recorded
/// sources, all in one write. A stamp already there is never overwritten; a
/// fill matches its node, phase, harness and session id. Returns the stamps
/// written. A node that rides the raw carry takes none.
pub fn session_backfill(store: &Store, fills: &[(String, SessionFill)]) -> Result<usize, ApiError> {
    let mut by_node: HashMap<&str, Vec<&SessionFill>> = HashMap::new();
    for (node, fill) in fills {
        by_node.entry(node.as_str()).or_default().push(fill);
    }
    let mut written = 0;
    if by_node.is_empty() {
        return Ok(written);
    }
    mutate(store, "session_backfill", |rows| {
        written = 0;
        for row in rows.iter_mut() {
            let Some(mine) = crate::graph_store::entry_id(row).and_then(|id| by_node.get(id))
            else {
                continue;
            };
            let Ok(mut parsed) = Node::from_json(row) else {
                continue;
            };
            let before = written;
            for record in parsed.sessions.iter_mut().flatten() {
                for fill in mine {
                    if record.phase != fill.phase
                        || record.harness != fill.harness
                        || record.session_id != fill.session_id
                    {
                        continue;
                    }
                    if record.started_at.as_deref().is_none_or(str::is_empty)
                        && fill.started_at.is_some()
                    {
                        record.started_at = fill.started_at.clone();
                        written += 1;
                    }
                    if record.ended_at.as_deref().is_none_or(str::is_empty)
                        && fill.ended_at.is_some()
                    {
                        record.ended_at = fill.ended_at.clone();
                        record.ended_by = Some(fill.ended_by.clone());
                        written += 1;
                    }
                }
            }
            if written > before {
                *row = parsed.to_json();
            }
        }
        Ok(written > 0)
    })?;
    Ok(written)
}

/// Fill `ended_at` on every `phase` record that has none, whoever opened it,
/// for each (node, ended_at, ended_by), all in one write: a ship record ends
/// at the merge, whichever session linked the PR. The first end named for a
/// node wins. Returns the records ended. A node that rides the raw carry
/// takes none.
pub fn phase_end(
    store: &Store,
    phase: &str,
    ends: &[(String, String, &str)],
) -> Result<usize, ApiError> {
    let mut by_node: HashMap<&str, (&str, &str)> = HashMap::new();
    for (node, at, by) in ends {
        by_node.entry(node.as_str()).or_insert((at.as_str(), *by));
    }
    let mut ended = 0;
    if by_node.is_empty() {
        return Ok(ended);
    }
    mutate(store, "phase_end", |rows| {
        ended = 0;
        for row in rows.iter_mut() {
            let Some(&(at, by)) = crate::graph_store::entry_id(row).and_then(|id| by_node.get(id))
            else {
                continue;
            };
            let Ok(mut parsed) = Node::from_json(row) else {
                continue;
            };
            let before = ended;
            for record in parsed.sessions.iter_mut().flatten() {
                if record.phase == phase && record.ended_at.as_deref().is_none_or(str::is_empty) {
                    record.ended_at = Some(at.to_string());
                    record.ended_by = Some(by.to_string());
                    ended += 1;
                }
            }
            if ended > before {
                *row = parsed.to_json();
            }
        }
        Ok(ended > 0)
    })?;
    Ok(ended)
}

pub fn encounter_create(
    store: &Store,
    id: &str,
    input: EncounterInput,
) -> Result<Payload<Node>, ApiError> {
    let mut updated: Option<Node> = None;
    let ok = mutate(store, "encounter_create", |rows| {
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                return Ok(false);
            };
            parsed
                .encounters
                .get_or_insert_with(Vec::new)
                .push(Encounter {
                    created_at: crate::graph_store::now_isoformat(),
                    evidence: input.evidence.clone(),
                    session_id: input.session_id.clone(),
                    voter_key: None,
                    voter_kind: None,
                    harness: None,
                    fno_id: None,
                    effort: None,
                    model: None,
                    extras: serde_json::Map::new(),
                });
            *row = parsed.to_json();
            updated = Some(parsed);
            break;
        }
        Ok(updated.is_some())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

pub fn dispatch_set(
    store: &Store,
    id: &str,
    d: Option<Dispatch>,
) -> Result<Payload<Node>, ApiError> {
    let mut updated: Option<Node> = None;
    let ok = mutate(store, "dispatch_set", |rows| {
        for row in rows.iter_mut() {
            if crate::graph_store::entry_id(row) != Some(id) {
                continue;
            }
            let Ok(mut parsed) = Node::from_json(row) else {
                return Ok(false);
            };
            parsed.dispatch = d.clone().unwrap_or(Dispatch {
                verb: None,
                brief: None,
                model: None,
            });
            *row = parsed.to_json();
            updated = Some(parsed);
            break;
        }
        Ok(updated.is_some())
    })?;
    if ok {
        Ok(Payload {
            success: true,
            node: updated,
            version: fresh_version(store),
        })
    } else {
        refusal(store)
    }
}

// The keeper builds the wire JSON (reply keys are its contract); api.rs
// speaks only typed structs.

#[cfg(test)]
mod tests;
