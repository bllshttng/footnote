//! The one backlog read model: gather once, answer board and node queries
//! purely, and let two renderers draw the same structs.
use crate::backlog_view::{
    board_scope_from_spawn_env, external_backend_selected, has_open_dependency, kanban_column,
    read_snapshot, run_claim_sweep, BoardScope, KANBAN_COLUMNS,
};
use crate::proto::AgentRow;
use crate::store_client;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The ledger the flow aggregate reads. Under test the proto root is not
/// compiled, so point at a path that reads as absent: no ledger, no rows.
#[cfg(not(test))]
fn ledger_path() -> PathBuf {
    crate::proto::mux_sidecar_root().join("ledger.json")
}

#[cfg(test)]
fn ledger_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!("fno-model-ledger-{}", std::process::id()))
        .join("ledger.json")
}

pub const REGATHER_AFTER: Duration = Duration::from_secs(30);

/// How lanes group cards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LanesBy {
    /// One lane per project; the unscoped lane last.
    #[default]
    Project,
    /// One lane per epic (open, parented cards sit in their epic's lane).
    Epic,
    /// One lane.
    #[serde(rename = "none")]
    None,
}

/// How the list view asks for its rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum View {
    /// The kanban grid: cells keep their per-column caps.
    #[default]
    Kanban,
    /// The list: lanes carry every card, uncapped, so no row is hidden.
    List,
}

/// The board query, parsed from the route's pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Query {
    pub lanes: LanesBy,
    #[serde(skip_serializing_if = "View::is_kanban", default)]
    pub view: View,
    pub project: Vec<String>,
    epic: Vec<String>,
    status: Vec<String>,
    priority: Vec<String>,
    size: Vec<String>,
    king: Vec<String>,
    kind: Vec<String>,
    tag: Vec<String>,
    q: Option<String>,
    all: bool,
}

impl View {
    fn is_kanban(v: &View) -> bool {
        *v == View::Kanban
    }
}

/// One value per repeated key, first-seen order, duplicates dropped.
fn push_unique(set: &mut Vec<String>, val: Option<&str>) {
    if let Some(v) = val {
        if !set.iter().any(|s| s == v) {
            set.push(v.to_string());
        }
    }
}

impl Query {
    /// Parse the route's query pairs. Unknown keys (the page's `t` and
    /// `node`) are ignored, an empty value reads as unset, repeated filter
    /// keys accumulate any-of sets, and `all` is true for `1`, `true` or an
    /// empty value.
    pub fn from_pairs(p: &[(String, String)]) -> Result<Query, String> {
        let mut q = Query::default();
        for (k, v) in p {
            let val = (!v.is_empty()).then(|| v.as_str());
            match k.as_str() {
                "lanes" => match val.unwrap_or_default() {
                    "" | "project" => q.lanes = LanesBy::Project,
                    "epic" => q.lanes = LanesBy::Epic,
                    "none" => q.lanes = LanesBy::None,
                    other => {
                        return Err(format!(
                            "unknown lanes '{other}'; use project, epic or none"
                        ))
                    }
                },
                "view" => match val.unwrap_or_default() {
                    "" | "kanban" => q.view = View::Kanban,
                    "list" => q.view = View::List,
                    other => return Err(format!("unknown view '{other}'; use kanban or list")),
                },
                "project" => push_unique(&mut q.project, val),
                "epic" => push_unique(&mut q.epic, val),
                "status" => push_unique(&mut q.status, val),
                "priority" => push_unique(&mut q.priority, val),
                "size" => push_unique(&mut q.size, val),
                "king" => push_unique(&mut q.king, val),
                "type" => push_unique(&mut q.kind, val),
                "tag" => push_unique(&mut q.tag, val),
                "q" => q.q = val.map(str::to_string),
                "all" => q.all = matches!(v.as_str(), "1" | "true" | ""),
                _ => {}
            }
        }
        Ok(q)
    }
}

/// The crowned row that rules a node's territory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct King {
    pub name: String,
    pub level: u32,
}

/// One board card.
#[derive(Debug, Clone, Serialize)]
pub struct Card {
    pub id: String,
    pub slug: Option<String>,
    /// Falls back to the slug when the row carries no title.
    pub title: String,
    /// The authority's column name ([`KANBAN_COLUMNS`]).
    pub column: &'static str,
    /// The card's index in the model's total order.
    pub order: usize,
    pub rank: Option<f64>,
    pub priority: Option<String>,
    pub size: Option<String>,
    pub status: Option<String>,
    pub project: Option<String>,
    pub parent: Option<String>,
    /// The row's `type` field.
    pub kind: Option<String>,
    /// The row's `tags` array, empty when absent.
    pub tags: Vec<String>,
    pub blocked: bool,
    pub claimed: bool,
    pub king: Option<King>,
    /// True when any of the node's sessions joins a roster row.
    pub live: bool,
    /// The row's `created_at`, for the list view's date column.
    pub created_at: Option<String>,
}

/// A link to another node the read holds; an id-only link names a node the
/// read does not hold.
#[derive(Debug, Clone, Serialize)]
pub struct Link {
    pub id: String,
    pub title: Option<String>,
    pub column: Option<&'static str>,
    pub status: Option<String>,
}

/// One column cell within a lane.
#[derive(Debug, Clone, Serialize)]
pub struct Cell {
    pub column: &'static str,
    pub total: usize,
    pub cards: Vec<Card>,
}

/// One lane: a key (project or epic id), a title, one cell per column.
#[derive(Debug, Clone, Serialize)]
pub struct Lane {
    pub key: String,
    pub title: String,
    pub cells: Vec<Cell>,
}

/// Per-column open totals.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnTotal {
    pub column: &'static str,
    pub total: usize,
}

/// One status's card count for the list view's count tiles.
#[derive(Debug, Clone, Serialize)]
pub struct StatusTotal {
    pub status: String,
    pub total: usize,
}

/// The board's aggregates.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    /// Open totals per column, counted over the filtered card set.
    pub open: Vec<ColumnTotal>,
    /// Per-status totals over the pre-filter set: a tile's count is what
    /// selecting it would show.
    pub statuses: Vec<StatusTotal>,
    /// Totals per column over the filtered set, Done included, for the
    /// board-wide header counts.
    pub totals: Vec<ColumnTotal>,
    /// The classifier's `flow` verbatim, or `{"available": false, ...}`.
    pub flow: Value,
}

/// Values present after the scope and before the model's own filters, for
/// filter menus. `kinds` is the set of row `type` values; `tags` the set of
/// row `tags`, empty while no row carries one.
#[derive(Debug, Clone, Serialize)]
pub struct Facets {
    pub projects: Vec<String>,
    pub epics: Vec<EpicRef>,
    pub kings: Vec<String>,
    pub priorities: Vec<String>,
    pub sizes: Vec<String>,
    pub statuses: Vec<String>,
    pub kinds: Vec<String>,
    pub tags: Vec<String>,
}

/// An epic named in the facets.
#[derive(Debug, Clone, Serialize)]
pub struct EpicRef {
    pub id: String,
    pub title: Option<String>,
}

/// A feature the current backend cannot answer, for renderer controls.
#[derive(Debug, Clone, Serialize)]
pub struct Unavailable {
    pub feature: &'static str,
    pub reason: String,
}

/// Unavailable feature names, one spelling renderers compare against.
pub mod unavailable_features {
    pub const CARD_MOVES: &str = "card moves";
    pub const FIELD_EDITS: &str = "field edits";
    pub const SIZE_FILTER: &str = "size filter";
    pub const DETAILS: &str = "details";
    pub const EPIC_LANES: &str = "epic lanes";
    pub const BLOCKERS: &str = "blockers";
    pub const SESSIONS: &str = "sessions";
    pub const DONE_COLUMN: &str = "Done column";
}

/// The whole board answer.
#[derive(Debug, Clone, Serialize)]
pub struct Board {
    pub schema: u8,
    pub version: Option<i64>,
    pub backend: String,
    pub scope: String,
    pub query: Query,
    pub lanes: Vec<Lane>,
    pub stats: Stats,
    pub facets: Facets,
    pub unavailable: Vec<Unavailable>,
    pub errors: Vec<String>,
}

/// One graph session row joined to the roster.
#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub phase: Option<String>,
    pub harness: Option<String>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    /// The joined roster row's name, when the join held.
    pub agent: Option<String>,
    /// `attach`, `resume` or `none`.
    pub action: String,
    /// The dim reason when the action is `none`.
    pub reason: Option<String>,
}

/// A PR bound to a node.
#[derive(Debug, Clone, Serialize)]
pub struct Pr {
    pub number: u64,
    pub url: Option<String>,
    pub merge_status: Option<String>,
}

/// A progress note.
#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub ts: Option<String>,
    pub text: String,
    pub kind: Option<String>,
}

/// One node's whole answer.
#[derive(Debug, Clone, Serialize)]
pub struct NodeView {
    pub card: Card,
    pub details: Option<Value>,
    pub current_state: Value,
    pub plan_path: Option<String>,
    pub cwd: Option<String>,
    pub difficulty: Option<String>,
    /// The row's `type` field.
    pub kind: Option<String>,
    pub created_at: Option<String>,
    pub completed_at: Option<String>,
    pub children: Vec<Link>,
    pub contained: Vec<Link>,
    pub blocked_by: Vec<Link>,
    pub blocks: Vec<Link>,
    pub related: Vec<Link>,
    /// The node's parent, one link (the row's `parent` id).
    pub parent: Vec<Link>,
    pub sessions: Vec<SessionView>,
    pub prs: Vec<Pr>,
    pub notes: Vec<Note>,
    pub decisions: Vec<Value>,
    pub unavailable: Vec<Unavailable>,
}

/// Everything one gather read, shared by both renderers.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub backend: String,
    pub rows: Vec<Value>,
    pub rows_error: Option<String>,
    pub version: Option<i64>,
    /// Every id the keeper named, in selection order.
    pub order: Vec<String>,
    pub underway: HashSet<String>,
    pub effective_priority: HashMap<String, String>,
    /// The claim sweep's node id -> holder map.
    pub live_claims: HashMap<String, String>,
    pub agents: Vec<AgentRow>,
    pub flow: Value,
    pub scope: BoardScope,
    pub scope_reason: String,
    pub errors: Vec<String>,
}

#[cfg(test)]
pub(crate) fn fixture(rows: Vec<Value>) -> Inputs {
    Inputs {
        backend: "graph".into(),
        order: rows
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
            .collect(),
        flow: json!({"available": false, "reason": "fixture"}),
        rows,
        ..Default::default()
    }
}

/// The card's index in the model's total order: the keeper's `ids` first,
/// then ids the keeper did not name, by `created_at`.
pub(crate) fn order_of(inp: &Inputs) -> HashMap<String, usize> {
    let mut map: HashMap<String, usize> = HashMap::new();
    for (i, id) in inp.order.iter().enumerate() {
        map.insert(id.clone(), i);
    }
    // Ids the keeper did not name: append after the named ones, oldest first.
    let mut unlisted: Vec<(&str, &Value)> = inp
        .rows
        .iter()
        .filter(|r| {
            r.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !map.contains_key(id))
        })
        .filter_map(|r| r.get("id").and_then(Value::as_str).map(|id| (id, r)))
        .collect::<Vec<(&str, &Value)>>();
    unlisted.sort_by_key(|(_, r)| r.get("created_at").and_then(Value::as_str).unwrap_or(""));
    let base = inp.order.len();
    for (i, (id, _)) in unlisted.into_iter().enumerate() {
        map.insert(id.to_string(), base + i);
    }
    map
}

/// The crowned row ruling this node's territory, from the roster.
pub(crate) fn king_for(
    inp: &Inputs,
    node_id: &str,
    parent: Option<&str>,
    project: Option<&str>,
) -> Option<King> {
    king_of(&inp.agents, node_id, parent, project).map(|(name, level)| King { name, level })
}

/// The one per-row card function [`board`] and [`node`] both use.
pub(crate) fn card_of(
    inp: &Inputs,
    e: &Value,
    order: &HashMap<String, usize>,
    blocked: bool,
) -> Option<Card> {
    let id = e.get("id").and_then(Value::as_str)?.to_string();
    let slug = e.get("slug").and_then(Value::as_str).map(str::to_string);
    let title = e
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or(slug.as_deref())
        .unwrap_or_default()
        .to_string();
    let status_claimed = matches!(
        node_status_text(e).as_deref(),
        Some("claimed") | Some("in_progress") | Some("in-progress")
    );
    let claimed = status_claimed || inp.live_claims.contains_key(&id);
    let underway = inp.underway.contains(&id);
    let column = kanban_column(
        e,
        claimed,
        underway,
        inp.effective_priority.get(&id).map(String::as_str),
    )?;
    Some(Card {
        order: order.get(&id).copied().unwrap_or(usize::MAX),
        rank: e.get("rank").and_then(Value::as_f64),
        priority: e
            .get("priority")
            .and_then(Value::as_str)
            .map(str::to_string),
        size: e.get("size").and_then(Value::as_str).map(str::to_string),
        status: node_status_text(e),
        project: e
            .get("project")
            .and_then(Value::as_str)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        parent: e.get("parent").and_then(Value::as_str).map(str::to_string),
        kind: e.get("type").and_then(Value::as_str).map(str::to_string),
        tags: e
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        blocked,
        claimed,
        king: king_for(
            inp,
            &id,
            e.get("parent").and_then(Value::as_str),
            e.get("project").and_then(Value::as_str),
        ),
        live: session_is_live(inp, &id),
        id,
        slug,
        title,
        column,
        created_at: e
            .get("created_at")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The row's status: the persisted field, tolerating the pre-rename key.
pub(crate) fn node_status_text(e: &Value) -> Option<String> {
    e.get("status")
        .or_else(|| e.get("_status"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Whether any of the node's sessions joins a roster row by
/// `harness_session_id`.
fn session_is_live(inp: &Inputs, node_id: &str) -> bool {
    node_sessions(inp, node_id).iter().any(|s| {
        s.get("session_id")
            .and_then(Value::as_str)
            .is_some_and(|sid| {
                inp.agents
                    .iter()
                    .any(|a| a.harness_session_id.as_deref() == Some(sid))
            })
    })
}

/// The node's `sessions[]` rows.
fn node_sessions<'a>(inp: &'a Inputs, node_id: &str) -> Vec<&'a Value> {
    inp.rows
        .iter()
        .filter(|r| r.get("id").and_then(Value::as_str) == Some(node_id))
        .filter_map(|r| r.get("sessions").and_then(&Value::as_array))
        .flatten()
        .collect()
}

/// Whether a timestamp field carries an actual stamp (the Python board's
/// truthiness rule: null and "" are both unstamped).
fn stamped(e: &Value, field: &str) -> bool {
    e.get(field)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
}

/// Whether the filtered card set keeps the row: any-of project, status,
/// priority, size, king-name, type and tag filters, `epic` keeps the
/// epic's own card plus cards whose parent names any selected epic, and
/// `q` is a case-insensitive substring of id, slug, title or details.
fn keeps_query(card: &Card, row: Option<&Value>, q: &Query) -> bool {
    let in_set = |values: &[String], have: Option<&str>| {
        values.is_empty() || values.iter().any(|v| Some(v.as_str()) == have)
    };
    if !in_set(&q.project, card.project.as_deref()) {
        return false;
    }
    if !in_set(&q.status, card.status.as_deref()) {
        return false;
    }
    if !in_set(&q.priority, card.priority.as_deref()) {
        return false;
    }
    if !in_set(&q.size, card.size.as_deref()) {
        return false;
    }
    if !in_set(&q.kind, card.kind.as_deref()) {
        return false;
    }
    if !q.tag.is_empty() && !q.tag.iter().any(|t| card.tags.iter().any(|have| have == t)) {
        return false;
    }
    if !q.king.is_empty()
        && !card
            .king
            .as_ref()
            .is_some_and(|king| q.king.iter().any(|k| king.name == *k))
    {
        return false;
    }
    if !q.epic.is_empty()
        && !q
            .epic
            .iter()
            .any(|epic| card.id == *epic || card.parent.as_deref() == Some(epic.as_str()))
    {
        return false;
    }
    if let Some(needle) = &q.q {
        let needle = needle.to_lowercase();
        let slug = card.slug.as_deref().unwrap_or("");
        let details = row
            .and_then(|r| r.get("details"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let hay = format!("{} {} {} {}", card.id, slug, card.title, details).to_lowercase();
        if !hay.contains(&needle) {
            return false;
        }
    }
    true
}

/// The pure board answer for a query over gathered inputs.
pub fn board(inp: &Inputs, q: &Query) -> Board {
    if let Some(err) = &inp.rows_error {
        return Board {
            schema: 1,
            version: inp.version,
            backend: inp.backend.clone(),
            scope: inp.scope.describe(),
            query: q.clone(),
            lanes: vec![],
            stats: Stats {
                open: vec![],
                statuses: vec![],
                totals: vec![],
                flow: inp.flow.clone(),
            },
            facets: Facets {
                projects: vec![],
                epics: vec![],
                kings: vec![],
                priorities: vec![],
                sizes: vec![],
                statuses: vec![],
                kinds: vec![],
                tags: vec![],
            },
            unavailable: unavailable(inp),
            errors: {
                let mut e = inp.errors.clone();
                e.push(err.clone());
                e
            },
        };
    }
    let order = order_of(inp);
    let mut by_ref: HashMap<&str, &Value> = HashMap::new();
    for r in &inp.rows {
        if let Some(id) = r.get("id").and_then(Value::as_str) {
            by_ref.insert(id, r);
        }
    }
    // Cards first (the column may drop a row), then scope, then facets,
    // then the query's filters, then totals, then the caps.
    let mut cards: Vec<Card> = Vec::new();
    for e in &inp.rows {
        let blocked = has_open_dependency(e, &by_ref);
        if let Some(card) = card_of(inp, e, &order, blocked) {
            cards.push(card);
        }
    }
    // The spawn-env scope is skipped when the query itself scopes.
    let scope_applies = !q.all && q.project.is_empty();
    let scoped: Vec<Card> = if scope_applies {
        cards
            .into_iter()
            .filter(|c| {
                inp.scope
                    .keeps(c.project.as_deref().filter(|p| !p.is_empty()))
            })
            .collect()
    } else {
        cards
    };
    // Facets and per-status totals: values present after scope, before
    // filters. A tile's count is what selecting it would show.
    let facets = facets_of(&scoped, inp);
    let mut status_totals: BTreeMap<String, usize> = BTreeMap::new();
    for c in &scoped {
        if let Some(s) = &c.status {
            *status_totals.entry(s.clone()).or_insert(0) += 1;
        }
    }
    let filtered: Vec<Card> = scoped
        .into_iter()
        .filter(|c| keeps_query(c, by_ref.get(c.id.as_str()).copied(), q))
        .collect();
    // Per-column totals over the filtered set; `open` excludes Done, the
    // header counts keep it.
    let mut open_totals: HashMap<&'static str, usize> = HashMap::new();
    for c in &filtered {
        *open_totals.entry(c.column).or_insert(0) += 1;
    }
    let stats = Stats {
        open: KANBAN_COLUMNS
            .iter()
            .filter(|col| **col != "Done")
            .map(|col| ColumnTotal {
                column: col,
                total: open_totals.get(col).copied().unwrap_or(0),
            })
            .collect(),
        statuses: status_totals
            .into_iter()
            .map(|(status, total)| StatusTotal { status, total })
            .collect(),
        totals: KANBAN_COLUMNS
            .iter()
            .map(|col| ColumnTotal {
                column: col,
                total: open_totals.get(col).copied().unwrap_or(0),
            })
            .collect(),
        flow: inp.flow.clone(),
    };
    let lanes = build_lanes(filtered, &order, inp, &q.lanes, q.view == View::List);
    Board {
        schema: 1,
        version: inp.version,
        backend: inp.backend.clone(),
        scope: inp.scope.describe(),
        query: q.clone(),
        lanes,
        stats,
        facets,
        unavailable: unavailable(inp),
        errors: inp.errors.clone(),
    }
}

/// Filter-menu values present after the scope and before the filters,
/// sorted.
fn facets_of(cards: &[Card], inp: &Inputs) -> Facets {
    let mut projects: BTreeMap<String, ()> = BTreeMap::new();
    let mut epics: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut kings: BTreeMap<String, ()> = BTreeMap::new();
    let mut priorities: BTreeMap<String, ()> = BTreeMap::new();
    let mut sizes: BTreeMap<String, ()> = BTreeMap::new();
    let mut statuses: BTreeMap<String, ()> = BTreeMap::new();
    let mut kinds: BTreeMap<String, ()> = BTreeMap::new();
    let mut tags: BTreeMap<String, ()> = BTreeMap::new();
    for c in cards {
        if let Some(p) = &c.project {
            projects.insert(p.clone(), ());
        }
        if let Some(k) = &c.king {
            kings.insert(k.name.clone(), ());
        }
        if let Some(p) = &c.priority {
            priorities.insert(p.clone(), ());
        }
        if let Some(s) = &c.size {
            sizes.insert(s.clone(), ());
        }
        if let Some(s) = &c.status {
            statuses.insert(s.clone(), ());
        }
        if let Some(k) = &c.kind {
            kinds.insert(k.clone(), ());
        }
        for t in &c.tags {
            tags.insert(t.clone(), ());
        }
    }
    // Epic facets come from the rows, not the cards: an epic card may sit
    // in In Progress but its title facets from its own row.
    let parents: HashSet<String> = inp
        .rows
        .iter()
        .filter_map(|o| o.get("parent").and_then(Value::as_str).map(str::to_string))
        .collect();
    for r in &inp.rows {
        if let Some(id) = r.get("id").and_then(Value::as_str) {
            if parents.contains(id) {
                epics
                    .entry(id.to_string())
                    .or_insert(r.get("title").and_then(Value::as_str).map(str::to_string));
            }
        }
    }
    Facets {
        projects: projects.into_keys().collect(),
        epics: epics
            .into_iter()
            .map(|(id, title)| EpicRef { id, title })
            .collect(),
        kings: kings.into_keys().collect(),
        priorities: priorities.into_keys().collect(),
        sizes: sizes.into_keys().collect(),
        statuses: statuses.into_iter().map(|(s, _)| s).collect(),
        kinds: kinds.into_keys().collect(),
        tags: tags.into_keys().collect(),
    }
}

/// The per-cell card cap.
const CELL_CAP: usize = 50;
/// The Done cap per cell.
const DONE_CAP: usize = 20;

/// Group the filtered cards into lanes and six-column cells, with caps.
fn build_lanes(
    filtered: Vec<Card>,
    order: &HashMap<String, usize>,
    inp: &Inputs,
    lanes_by: &LanesBy,
    uncapped: bool,
) -> Vec<Lane> {
    let by_id: HashMap<String, &Value> = inp
        .rows
        .iter()
        .filter_map(|r| {
            r.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), r))
        })
        .collect();
    // The id each card's parent names; "is an epic" = some row parents to it.
    let parents: HashSet<String> = inp
        .rows
        .iter()
        .filter_map(|o| o.get("parent").and_then(Value::as_str).map(str::to_string))
        .collect();
    // Lane key and title per card id.
    let mut keys: HashMap<String, (String, String)> = HashMap::new();
    for c in &filtered {
        let (key, title) = match lanes_by {
            LanesBy::None => ("all".to_string(), "all".to_string()),
            LanesBy::Project => {
                let key = c.project.clone().unwrap_or_default();
                let title = if key.is_empty() {
                    "unscoped".to_string()
                } else {
                    key.clone()
                };
                (key, title)
            }
            LanesBy::Epic => {
                // A card sits in its parent's lane when the parent row
                // exists and is open; an epic sits in its own lane; loose
                // cards sit in a last lane keyed "" titled "no epic".
                if parents.contains(&c.id) {
                    (c.id.clone(), c.title.clone())
                } else if let Some(parent) = &c.parent {
                    match by_id.get(parent) {
                        Some(prow) if !stamped(prow, "completed_at") => {
                            let title = prow
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or(parent.as_str())
                                .to_string();
                            (parent.clone(), title)
                        }
                        _ => (String::new(), "no epic".to_string()),
                    }
                } else {
                    (String::new(), "no epic".to_string())
                }
            }
        };
        keys.insert(c.id.clone(), (key, title));
    }
    // Group into lanes (first-seen order), then apply the mode's lane order.
    let mut lane_titles: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<String, Vec<Card>> = HashMap::new();
    for c in filtered {
        let (key, title) = keys.remove(&c.id).unwrap_or_default();
        if !groups.contains_key(&key) {
            lane_titles.push((key.clone(), title));
        }
        groups.entry(key).or_default().push(c);
    }
    let lane_order: Vec<(String, String)> = match lanes_by {
        LanesBy::Project => {
            let mut named: Vec<_> = lane_titles
                .iter()
                .filter(|(k, _)| !k.is_empty())
                .cloned()
                .collect();
            named.sort();
            named.extend(lane_titles.iter().filter(|(k, _)| k.is_empty()).cloned());
            named
        }
        LanesBy::Epic => {
            // Epic lanes follow the epic's own order; the loose lane last.
            let epic_order = |k: &str| -> usize {
                if k.is_empty() {
                    usize::MAX
                } else {
                    order.get(k).copied().unwrap_or(usize::MAX - 1)
                }
            };
            let mut by_epic_order = lane_titles;
            by_epic_order.sort_by_key(|(k, _)| epic_order(k));
            by_epic_order
        }
        LanesBy::None => lane_titles,
    };
    // Cells: six per lane, Done newest-first, others in order, capped.
    let completed_at: HashMap<String, String> = inp
        .rows
        .iter()
        .filter_map(|r| {
            r.get("id").and_then(Value::as_str).map(|id| {
                (
                    id.to_string(),
                    r.get("completed_at")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                )
            })
        })
        .collect();
    lane_order
        .into_iter()
        .map(|(key, title)| {
            let cards = groups.remove(&key).unwrap_or_default();
            let mut cells: Vec<Cell> = Vec::with_capacity(KANBAN_COLUMNS.len());
            for col in KANBAN_COLUMNS {
                let mut in_col: Vec<Card> =
                    cards.iter().filter(|c| c.column == col).cloned().collect();
                let total = in_col.len();
                if col == "Done" {
                    in_col.sort_by(|a, b| {
                        completed_at
                            .get(&b.id)
                            .map(String::as_str)
                            .unwrap_or("")
                            .cmp(completed_at.get(&a.id).map(String::as_str).unwrap_or(""))
                    });
                } else {
                    in_col.sort_by_key(|c| c.order);
                }
                // The list view needs every row; the kanban keeps its
                // per-cell caps for wire size.
                if !uncapped {
                    let cap = if col == "Done" { DONE_CAP } else { CELL_CAP };
                    in_col.truncate(cap);
                }
                cells.push(Cell {
                    column: col,
                    total,
                    cards: in_col,
                });
            }
            Lane { key, title, cells }
        })
        .collect()
}

/// The features the current backend cannot answer. Empty for `graph`.
/// `card moves` and `field edits` are unconditional off the graph; each
/// data feature follows a field no row carries with a non-empty value.
fn unavailable(inp: &Inputs) -> Vec<Unavailable> {
    use unavailable_features as f;
    if inp.backend == "graph" {
        return vec![];
    }
    let b = &inp.backend;
    let mut out = vec![
        Unavailable {
            feature: f::CARD_MOVES,
            reason: format!("{b} takes no rank writes"),
        },
        Unavailable {
            feature: f::FIELD_EDITS,
            reason: format!("edit it in {b}"),
        },
    ];
    let carries = |field: &str| -> bool {
        inp.rows.iter().any(|r| {
            r.get(field).is_some_and(|v| match v {
                Value::String(s) => !s.is_empty(),
                Value::Array(a) => !a.is_empty(),
                Value::Object(_) | Value::Bool(true) => true,
                _ => false,
            })
        })
    };
    let add = |out: &mut Vec<Unavailable>, feature: &'static str, field: &str| {
        if !carries(field) {
            out.push(Unavailable {
                feature,
                reason: format!("{b} supplies no {field}"),
            });
        }
    };
    add(&mut out, f::SIZE_FILTER, "size");
    add(&mut out, f::DETAILS, "details");
    add(&mut out, f::EPIC_LANES, "parent");
    add(&mut out, f::BLOCKERS, "blocked_by");
    add(&mut out, f::SESSIONS, "sessions");
    add(&mut out, f::DONE_COLUMN, "completed_at");
    out
}

/// Resolve a node id or slug to its row.
pub(crate) fn resolve_row<'a>(inp: &'a Inputs, id: &str) -> Option<&'a Value> {
    inp.rows
        .iter()
        .find(|r| r.get("id").and_then(Value::as_str) == Some(id))
        .or_else(|| {
            inp.rows
                .iter()
                .find(|r| r.get("slug").and_then(Value::as_str) == Some(id))
        })
}

/// One link from a row field of ids to the nodes the read holds. The field
/// may be a list (`blocked_by`) or a single id (`parent`).
fn links(inp: &Inputs, e: &Value, field: &str) -> Vec<Link> {
    let ids: Vec<&str> = match e.get(field) {
        Some(Value::Array(arr)) => arr.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(s)) => vec![s.as_str()],
        _ => Vec::new(),
    };
    let order = order_of(inp);
    ids.iter()
        .map(|&id| {
            let title = resolve_row(inp, id)
                .and_then(|r| r.get("title").and_then(Value::as_str))
                .map(str::to_string);
            let (column, status) = match resolve_row(inp, id) {
                Some(r) => {
                    let blocked = false;
                    let card = card_of(inp, r, &order, blocked);
                    (card.as_ref().map(|c| c.column), node_status_text(r))
                }
                None => (None, None),
            };
            Link {
                id: id.to_string(),
                title,
                column,
                status,
            }
        })
        .collect()
}

/// Reverse links: rows whose `field` names this node. The field may be a
/// single id (`parent`, `contained_in`) or a list (`blocked_by`, `related`).
fn reverse_links(inp: &Inputs, id: &str, field: &str) -> Vec<Link> {
    let names = |v: &Value| -> bool {
        match v {
            Value::String(s) => s == id,
            Value::Array(arr) => arr.iter().any(|v| v.as_str() == Some(id)),
            _ => false,
        }
    };
    let order = order_of(inp);
    inp.rows
        .iter()
        .filter(|r| r.get(field).is_some_and(names))
        .filter_map(|r| {
            let rid = r.get("id").and_then(Value::as_str)?;
            let title = r.get("title").and_then(Value::as_str).map(str::to_string);
            let card = card_of(inp, r, &order, false);
            Some(Link {
                id: rid.to_string(),
                title,
                column: card.as_ref().map(|c| c.column),
                status: node_status_text(r),
            })
        })
        .collect()
}

/// The node's answer, or `None` when the read holds no such node.
pub fn node(inp: &Inputs, id: &str) -> Option<NodeView> {
    let e = resolve_row(inp, id)?;
    let node_id = e.get("id").and_then(Value::as_str)?.to_string();
    let order = order_of(inp);
    let blocked = {
        let by_ref: HashMap<&str, &Value> = inp
            .rows
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str).map(|id| (id, r)))
            .collect();
        has_open_dependency(e, &by_ref)
    };
    let card = card_of(inp, e, &order, blocked)?;
    let str_field =
        |k: &str| -> Option<String> { e.get(k).and_then(Value::as_str).map(str::to_string) };
    // Sessions joined to the roster: the model carries the answer.
    let sessions = node_sessions(inp, &node_id)
        .iter()
        .map(|s| {
            let sid = s.get("session_id").and_then(Value::as_str);
            let joined = sid.and_then(|sid| {
                inp.agents
                    .iter()
                    .find(|a| a.harness_session_id.as_deref() == Some(sid))
            });
            let (action, reason) = match session_action(joined) {
                SessionAction::Attach => ("attach", None),
                SessionAction::Resume => ("resume", None),
                SessionAction::Dim(why) => ("none", Some(why)),
            };
            let model = s
                .get("observed_model")
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str)
                .or_else(|| s.get("model").and_then(Value::as_str))
                .map(str::to_string);
            SessionView {
                phase: s.get("phase").and_then(Value::as_str).map(str::to_string),
                harness: s.get("harness").and_then(Value::as_str).map(str::to_string),
                session_id: sid.map(str::to_string),
                model,
                started_at: s
                    .get("started_at")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                ended_at: s
                    .get("ended_at")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                agent: joined.map(|a| a.name.clone()),
                action: action.to_string(),
                reason,
            }
        })
        .collect();
    // PRs: the node's own binding plus any additional PRs.
    let mut prs: Vec<Pr> = Vec::new();
    if let Some(n) = e.get("pr_number").and_then(Value::as_u64) {
        prs.push(Pr {
            number: n,
            url: e.get("pr_url").and_then(Value::as_str).map(str::to_string),
            merge_status: e
                .get("merge_status")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    if let Some(extra) = e.get("additional_prs").and_then(Value::as_array) {
        for p in extra {
            if let Some(n) = p.get("number").and_then(Value::as_u64) {
                prs.push(Pr {
                    number: n,
                    url: p.get("url").and_then(Value::as_str).map(str::to_string),
                    merge_status: p
                        .get("merge_status")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
        }
    }
    // Notes: progress_notes newest first.
    let mut notes: Vec<Note> = e
        .get("progress_notes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|n| Note {
                    ts: n.get("ts").and_then(Value::as_str).map(str::to_string),
                    text: n
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    kind: n.get("kind").and_then(Value::as_str).map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default();
    notes.sort_by(|a, b| {
        b.ts.as_deref()
            .unwrap_or("")
            .cmp(a.ts.as_deref().unwrap_or(""))
    });
    let decisions = e
        .get("decisions")
        .cloned()
        .or_else(|| e.get("decision_ids").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let children = reverse_links(inp, &node_id, "parent");
    let contained = reverse_links(inp, &node_id, "contained_in");
    let blocks = reverse_links(inp, &node_id, "blocked_by");
    Some(NodeView {
        unavailable: unavailable(inp),
        children,
        contained,
        blocked_by: links(inp, e, "blocked_by"),
        blocks,
        related: links(inp, e, "related"),
        parent: links(inp, e, "parent"),
        sessions,
        prs,
        notes,
        decisions,
        card,
        details: e.get("details").cloned(),
        current_state: e.get("current_state").cloned().unwrap_or(Value::Null),
        plan_path: str_field("plan_path"),
        cwd: str_field("cwd"),
        difficulty: str_field("difficulty"),
        kind: e.get("type").and_then(Value::as_str).map(str::to_string),
        created_at: str_field("created_at"),
        completed_at: str_field("completed_at"),
    })
}

// ---------------------------------------------------------------------------
// Gathering
// ---------------------------------------------------------------------------

/// Read everything once. Blocking reads run in `spawn_blocking`; the claim
/// sweep runs first so one snapshot binds the order, the underway epics and
/// the card flags.
pub async fn gather(graph: &Path, agents: Vec<AgentRow>) -> Inputs {
    // One claim snapshot binds the order, the underway epics and the card
    // flags. A failed sweep is an errors line, never an empty board: the
    // keeper then reads the claim store itself and refuses when it cannot.
    let (live_claims, sweep_failed) = match run_claim_sweep().await {
        Some(map) => (map, false),
        None => (HashMap::new(), true),
    };
    let mut errors: Vec<String> = Vec::new();
    if sweep_failed {
        errors.push("claim sweep failed; claims shown from graph status only".to_string());
    }
    let graph = graph.to_path_buf();
    tokio::task::spawn_blocking(move || {
        gather_blocking(&graph, agents, live_claims, errors, sweep_failed)
    })
    .await
    .unwrap_or_else(|e| Inputs {
        errors: vec![format!("the model gather task failed: {e}")],
        ..Default::default()
    })
}

fn external_backend_name() -> String {
    std::env::var("FNO_TRACKER_BACKEND")
        .ok()
        .filter(|v| !v.trim().is_empty() && v.trim() != "graph")
        .unwrap_or_else(|| "graph".to_string())
}

/// The snapshot document's rows and its error surface, split out so tests
/// drive the same parse `gather_blocking` runs. `stale_since` becomes the
/// leading error line; every `errors` line rides after it.
fn parse_snapshot_doc(v: &Value) -> (Vec<Value>, Vec<String>) {
    let mut errors = Vec::new();
    if let Some(ts) = v.get("stale_since").and_then(Value::as_str) {
        errors.push(format!("tracker snapshot stale since {ts}"));
    }
    if let Some(lines) = v.get("errors").and_then(Value::as_array) {
        errors.extend(lines.iter().filter_map(|l| l.as_str()).map(str::to_string));
    }
    let rows = v
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    (rows, errors)
}

fn gather_blocking(
    graph: &Path,
    agents: Vec<AgentRow>,
    live_claims: HashMap<String, String>,
    mut errors: Vec<String>,
    sweep_failed: bool,
) -> Inputs {
    let backend_name = external_backend_name();
    let external = external_backend_selected();
    // (2) The source: the external snapshot, or the store's rows.
    let (rows, rows_error, snapshot_errors): (Vec<Value>, Option<String>, Vec<String>) = if external
    {
        match read_snapshot().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
            Some(v) => {
                let (rows, snapshot_errors) = parse_snapshot_doc(&v);
                (rows, None, snapshot_errors)
            }
            None => (
                Vec::new(),
                Some("the tracker snapshot read failed".to_string()),
                Vec::new(),
            ),
        }
    } else {
        match store_client::rows(graph) {
            Ok(r) => (r, None, Vec::new()),
            Err(e) => (Vec::new(), Some(e), Vec::new()),
        }
    };
    // Snapshot errors (staleness stamp first) lead Inputs.errors, so a board
    // consumer sees why the read is degraded.
    errors.splice(..0, snapshot_errors);
    if rows_error.is_some() {
        return Inputs {
            backend: backend_name,
            rows_error,
            agents,
            errors,
            ..Default::default()
        };
    }
    // (3) The store version, graph backend only.
    let version = if external {
        None
    } else {
        store_client::version(graph).ok()
    };
    // (4) The keeper's board facts. When the sweep failed, send no claimed:
    // the keeper reads the claim store itself and refuses when it cannot.
    let claimed: Vec<String> = if sweep_failed {
        Vec::new()
    } else {
        live_claims.keys().cloned().collect()
    };
    let shipped_entries: Option<Vec<Value>> = if external { Some(rows.clone()) } else { None };
    let (order, underway, effective_priority) =
        match store_client::board_facts(graph, shipped_entries.as_deref(), &claimed) {
            Ok(facts) => (facts.ids, facts.underway, facts.effective_priority),
            Err(e) => {
                errors.push(format!("board order unavailable: {e}"));
                (Vec::new(), HashSet::new(), HashMap::new())
            }
        };
    // (5) The ledger at <state root>/ledger.json.
    let path = ledger_path();
    let ledger: Vec<Value> = match std::fs::read_to_string(&path) {
        Err(_) => Vec::new(),
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => v
                .get("entries")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            Err(e) => {
                errors.push(format!("ledger unreadable: {e}"));
                Vec::new()
            }
        },
    };
    // (6) Flow over the rows and the ledger.
    let flow = if external {
        json!({"available": false, "reason": "no ledger flow on an external backend"})
    } else {
        match store_client::flow(graph, &rows, &ledger) {
            Ok(f) => f,
            Err(e) => json!({"available": false, "reason": e}),
        }
    };
    // (7) The spawn-env scope. Each failure is a named line, never an empty
    // board.
    let (scope, scope_reason) = board_scope_from_spawn_env();
    Inputs {
        backend: backend_name,
        rows,
        rows_error: None,
        version,
        order,
        underway,
        effective_priority,
        live_claims,
        agents,
        flow,
        scope,
        scope_reason,
        errors,
    }
}

// ---------------------------------------------------------------------------
// The moved node-detail helpers (from client/node_detail.rs, deleted upstream)
// ---------------------------------------------------------------------------

/// What a session row can DO, derived from the joined registry row - never
/// a fixed verb. The refusal half carries the reason the row renders dim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionAction {
    /// A live pane here: focus it. A paneless attachable row: attach.
    Attach,
    /// A paneless row its harness can resume.
    Resume,
    /// No launch; the reason renders in the action cell and a press
    /// answers with it as the notice.
    Dim(String),
}

/// Derive the row's action from the joined `AgentRow`. Absent row: no
/// registry row. Present: pane or attach id wins (the reach verbs), then
/// the resume form - but never on a row the registry reports Working or
/// Done, which the resume door refuses anyway; saying so here is the whole
/// point of the state-aware cell.
pub(crate) fn session_action(a: Option<&AgentRow>) -> SessionAction {
    let Some(a) = a else {
        return SessionAction::Dim("no registry row".into());
    };
    if a.pane_id.is_some() || a.attach_id.is_some() {
        return SessionAction::Attach;
    }
    match a.badge {
        Some(crate::proto::AgentBadge::Done) => return SessionAction::Dim("done".into()),
        Some(crate::proto::AgentBadge::Working) => return SessionAction::Dim("working".into()),
        _ => {}
    }
    if a.resumable {
        return SessionAction::Resume;
    }
    SessionAction::Dim("not resumable".into())
}

/// The node's king: the first crowned row whose territory names the node,
/// its parent epic, or its project (crown scopes split on `,`, the level-0
/// separator). Direct membership only - a grandchild epic resolves through
/// no scope here, and the pane says `none` rather than guessing.
pub(crate) fn king_of(
    agents: &[AgentRow],
    node_id: &str,
    parent: Option<&str>,
    project: Option<&str>,
) -> Option<(String, u32)> {
    for a in agents {
        let (Some(scope), Some(level)) = (&a.crown_scope, a.crown_level) else {
            continue;
        };
        for member in scope.split(',') {
            let m = member.trim();
            if m == node_id || parent == Some(m) || project == Some(m) {
                return Some((a.name.clone(), level));
            }
        }
    }
    None
}

#[cfg(test)]
#[path = "backlog_model_tests.rs"]
mod tests;
