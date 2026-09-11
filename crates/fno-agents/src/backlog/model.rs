//! The typed node model and its one JSON mapping. `Node::from_json` is the
//! only place that knows graph.json's key names; `Node::to_json` is the only
//! place that emits them. Import, export, the keeper wire replies, and
//! parity all go through these two.
//!
//! Round-trip contract (AC8-HP): for every row in graph.json and
//! graph-archive.json, `to_json(from_json(row))` equals the row after
//! null-valued keys are removed. Two rules make that hold:
//! - a key with no typed field lands in `extras` verbatim, in the order it
//!   appeared (AC9-EDGE);
//! - `None` fields are never emitted, so an explicit null in the input and
//!   an absent key compare equal once nulls are stripped on both sides.
//! List fields are `Option<Vec<_>>` so an empty array present in the input
//! stays present on output (tags is `[]` on every live row).

use serde_json::{json, Map, Value};

/// The status vocabulary. `claimed` migrates to `in_progress` on read
/// (statuses.STATUS_MIGRATION).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Idea,
    Design,
    Ready,
    InProgress,
    InReview,
    Blocked,
    Deferred,
    Done,
    Superseded,
}

impl Status {
    pub fn parse(value: &str) -> Result<Self, ModelError> {
        Ok(match value {
            "idea" => Self::Idea,
            "design" => Self::Design,
            "ready" => Self::Ready,
            "in_progress" | "claimed" => Self::InProgress,
            "in_review" => Self::InReview,
            "blocked" => Self::Blocked,
            "deferred" => Self::Deferred,
            "done" => Self::Done,
            "superseded" => Self::Superseded,
            other => return Err(ModelError(format!("unknown status {other:?}"))),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idea => "idea",
            Self::Design => "design",
            Self::Ready => "ready",
            Self::InProgress => "in_progress",
            Self::InReview => "in_review",
            Self::Blocked => "blocked",
            Self::Deferred => "deferred",
            Self::Done => "done",
            Self::Superseded => "superseded",
        }
    }
}

/// The priority vocabulary, with the legacy words migrated (constants.
/// PRIORITY_MIGRATION).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

impl Priority {
    pub fn parse(value: &str) -> Result<Self, ModelError> {
        Ok(match value {
            "p0" => Self::P0,
            "p1" | "high" => Self::P1,
            "p2" | "medium" => Self::P2,
            "p3" | "low" => Self::P3,
            other => return Err(ModelError(format!("unknown priority {other:?}"))),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::P0 => "p0",
            Self::P1 => "p1",
            Self::P2 => "p2",
            Self::P3 => "p3",
        }
    }
}

/// The Linear-style state groups a status belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateType {
    Unstarted,
    Started,
    Completed,
    Canceled,
}

pub fn state_type(status: Status) -> StateType {
    match status {
        Status::Idea | Status::Design | Status::Ready => StateType::Unstarted,
        Status::InProgress | Status::InReview | Status::Blocked => StateType::Started,
        Status::Done => StateType::Completed,
        Status::Deferred | Status::Superseded => StateType::Canceled,
    }
}

/// The relation kinds between two nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelationType {
    Blocks,
    Related,
    Supersedes,
}

impl RelationType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocks => "blocks",
            Self::Related => "related",
            Self::Supersedes => "supersedes",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModelError(pub String);

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ModelError {}

// ---------------------------------------------------------------------------
// Field extraction helpers. Each takes the row and the JSON key; a wrong
// type is a ModelError naming both.
// ---------------------------------------------------------------------------

fn opt<'a>(row: &'a Value, key: &str) -> Result<Option<&'a Value>, ModelError> {
    match row.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => Ok(Some(v)),
    }
}

fn opt_str(row: &Value, key: &str) -> Result<Option<String>, ModelError> {
    match opt(row, key)? {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(ModelError(format!("key {key:?} is not a string: {other}"))),
    }
}

fn req_str(row: &Value, key: &str) -> Result<String, ModelError> {
    opt_str(row, key)?.ok_or_else(|| ModelError(format!("missing required key {key:?}")))
}

fn opt_f64(row: &Value, key: &str) -> Result<Option<f64>, ModelError> {
    match opt(row, key)? {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_f64()
            .map(Some)
            .ok_or_else(|| ModelError(format!("key {key:?} is not a number"))),
        Some(other) => Err(ModelError(format!("key {key:?} is not a number: {other}"))),
    }
}

fn opt_i64(row: &Value, key: &str) -> Result<Option<i64>, ModelError> {
    match opt(row, key)? {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| ModelError(format!("key {key:?} is not an integer"))),
        Some(other) => Err(ModelError(format!(
            "key {key:?} is not an integer: {other}"
        ))),
    }
}

fn opt_bool(row: &Value, key: &str) -> Result<Option<bool>, ModelError> {
    match opt(row, key)? {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(ModelError(format!("key {key:?} is not a bool: {other}"))),
    }
}

fn opt_value(row: &Value, key: &str) -> Result<Option<Value>, ModelError> {
    Ok(opt(row, key)?.cloned())
}

fn opt_str_list(row: &Value, key: &str) -> Result<Option<Vec<String>>, ModelError> {
    match opt(row, key)? {
        None => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return Err(ModelError(format!(
                            "key {key:?} holds a non-string item: {item}"
                        )))
                    }
                }
            }
            Ok(Some(out))
        }
        Some(other) => Err(ModelError(format!("key {key:?} is not a list: {other}"))),
    }
}

fn sub_opt_str(typed: &Map<String, Value>, key: &str) -> Option<String> {
    match typed.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn sub_opt_f64(typed: &Map<String, Value>, key: &str) -> Option<f64> {
    typed.get(key).and_then(Value::as_f64)
}

fn sub_opt_i64(typed: &Map<String, Value>, key: &str) -> Option<i64> {
    typed.get(key).and_then(Value::as_i64)
}

fn sub_opt_str_list(typed: &Map<String, Value>, key: &str) -> Option<Vec<String>> {
    match typed.get(key) {
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        ),
        _ => None,
    }
}

fn sub_opt_value(typed: &Map<String, Value>, key: &str) -> Option<Value> {
    typed.get(key).filter(|v| !v.is_null()).cloned()
}

// ---------------------------------------------------------------------------
// The parts
// ---------------------------------------------------------------------------

/// deferred_*/queued_*/reopened_* timestamps and reasons.
#[derive(Clone, Debug, Default)]
pub struct Lifecycle {
    pub deferred_at: Option<String>,
    pub deferred_reason: Option<String>,
    pub deferred_kind: Option<String>,
    pub queued_at: Option<String>,
    pub queued_reason: Option<String>,
    pub reopened_at: Option<String>,
    pub reopened_reason: Option<String>,
}

/// The node lock fields (one node_claims row).
#[derive(Clone, Debug, Default)]
pub struct NodeClaim {
    pub locked_by: Option<String>,
    pub harness: Option<String>,
    pub harness_session: Option<String>,
    pub locked_at: Option<String>,
}

/// The dispatch fields (one node_dispatch row).
#[derive(Clone, Debug, Default)]
pub struct Dispatch {
    pub verb: Option<String>,
    pub brief: Option<String>,
    pub model: Option<String>,
}

/// The source*/spawned_by_*/think_* fields (one node_provenance row).
#[derive(Clone, Debug, Default)]
pub struct Provenance {
    pub source: Option<String>,
    pub source_kind: Option<String>,
    pub source_project: Option<String>,
    pub source_session_id: Option<String>,
    pub source_harness: Option<String>,
    pub source_cwd: Option<String>,
    pub source_node_id: Option<String>,
    pub source_plan_path: Option<String>,
    pub source_inbox_msg: Option<String>,
    pub spawned_by_session: Option<String>,
    pub spawned_by_harness: Option<String>,
    pub spawned_by_cwd: Option<String>,
    pub think_session_id: Option<String>,
    pub think_output_path: Option<String>,
    pub origin_evidence: Option<String>,
    pub request_origin: Option<String>,
}

/// One pull request: seq 0 is the primary (pr_number/pr_url/merge_status),
/// the rest mirror additional_prs.
#[derive(Clone, Debug, Default)]
pub struct PullRequest {
    pub number: Option<i64>,
    pub url: Option<String>,
    pub merge_status: Option<String>,
    pub note: Option<String>,
    pub extras: Map<String, Value>,
}

/// One sessions[] item.
#[derive(Clone, Debug)]
pub struct SessionRecord {
    pub phase: String,
    pub harness: String,
    pub session_id: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub ended_by: Option<String>,
    /// JSON values per the schema (observed_model is a dict on most rows).
    pub effort: Option<Value>,
    pub at: Option<Value>,
    pub claimed_at: Option<Value>,
    pub observed_model: Option<Value>,
    pub merge_grant: Option<Value>,
    pub extras: Map<String, Value>,
}

/// One progress_notes[] item (a comments row; ts -> created_at, text -> body).
#[derive(Clone, Debug)]
pub struct Comment {
    pub created_at: Option<String>,
    pub body: Option<String>,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub details: Option<String>,
    pub difficulty: Option<String>,
    pub source: Option<String>,
    pub source_session_id: Option<String>,
    pub source_harness: Option<String>,
    pub extras: Map<String, Value>,
}

/// One encounters[] item.
#[derive(Clone, Debug)]
pub struct Encounter {
    pub ts: String,
    pub evidence: String,
    pub session_id: Option<String>,
    pub voter_key: Option<String>,
    pub voter_kind: Option<String>,
    pub harness: Option<String>,
    pub fno_id: Option<String>,
    pub effort: Option<String>,
    pub model: Option<String>,
    pub extras: Map<String, Value>,
}

/// One decisions[] item. Waves 4 to 11 store these in node extras; wave 12
/// promotes them to decisions rows, so the typed surface stays the identity
/// and the body rides `extras` until then.
#[derive(Clone, Debug)]
pub struct DecisionRef {
    pub decision_id: Option<String>,
    pub ts: Option<String>,
    pub extras: Map<String, Value>,
}

/// One supersession{} object.
#[derive(Clone, Debug, Default)]
pub struct Supersession {
    pub successor: Option<String>,
    pub cause: Option<String>,
    pub reason: Option<String>,
    pub surfaces: Option<Vec<String>>,
    pub verified_at: Option<String>,
    pub evidence_pr: Option<i64>,
    pub matched_surfaces: Option<Vec<String>>,
    pub extras: Map<String, Value>,
}

/// One cost_sessions[] item (a node_costs row).
#[derive(Clone, Debug)]
pub struct CostRecord {
    pub session_id: String,
    pub cost_usd: f64,
    pub timestamp: Option<String>,
    pub extras: Map<String, Value>,
}

/// One difficulty_history[]/priority_history[] item (a node_history row).
#[derive(Clone, Debug, Default)]
pub struct FieldChange {
    pub value: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub source: Option<String>,
    pub ts: Option<String>,
    pub extras: Map<String, Value>,
}

/// One tasks[] item (a node_tasks row).
#[derive(Clone, Debug, Default)]
pub struct NodeTask {
    pub id: String,
    pub status: Option<String>,
    pub owner: Option<String>,
    pub claimed_at: Option<String>,
    pub extras: Map<String, Value>,
}

/// The ownership_defect object (a nodes column holding JSON).
#[derive(Clone, Debug, Default)]
pub struct OwnershipDefect {
    pub kind: String,
    pub node_id: Option<String>,
    pub holder: Option<String>,
    pub liveness: Option<String>,
    pub extras: Map<String, Value>,
}

/// The list-valued relations: blocked_by/related/supersedes (relations rows).
#[derive(Clone, Debug, Default)]
pub struct Relations {
    pub blocked_by: Option<Vec<String>>,
    pub related: Option<Vec<String>>,
    pub supersedes: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Node
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    /// File position; assigned by the importer, never a JSON key.
    pub ordinal: i64,
    pub slug: String,
    pub title: String,
    /// JSON "type".
    pub kind: String,
    pub status: Status,
    pub priority: Priority,
    pub rank: Option<f64>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub domain: Option<String>,
    /// JSON "size".
    pub estimate: Option<String>,
    pub difficulty: Option<String>,
    /// JSON "details".
    pub description: Option<String>,
    pub plan_path: Option<String>,
    pub parent: Option<String>,
    pub contained_in: Option<String>,
    pub superseded_by: Option<String>,
    pub caused_by: Option<String>,
    pub fixes_pr: Option<i64>,
    /// The schema is NOT NULL; raw pre-default rows (goldens) may lack it,
    /// so the model stays lenient and the importer enforces the column.
    pub created_at: Option<String>,
    pub touched_at: Option<String>,
    pub completed_at: Option<String>,
    pub completion_note: Option<String>,
    pub session_id: Option<String>,
    pub has_brief: Option<bool>,
    pub blocks_everything: Option<bool>,
    pub cost_usd: Option<f64>,
    pub vision_path: Option<String>,
    pub artifact_url: Option<String>,
    pub archived_at: Option<String>,
    pub lifecycle: Lifecycle,
    pub ownership_defect: Option<OwnershipDefect>,
    pub claim: NodeClaim,
    pub dispatch: Dispatch,
    pub provenance: Provenance,
    /// The primary pull request (pr_number/pr_url/merge_status).
    pub primary_pr: Option<PullRequest>,
    /// JSON additional_prs.
    pub additional_prs: Option<Vec<PullRequest>>,
    pub sessions: Option<Vec<SessionRecord>>,
    pub comments: Option<Vec<Comment>>,
    pub encounters: Option<Vec<Encounter>>,
    pub decisions: Option<Vec<DecisionRef>>,
    pub relations: Relations,
    pub supersession: Option<Supersession>,
    pub costs: Option<Vec<CostRecord>>,
    /// difficulty_history[] items.
    pub difficulty_history: Option<Vec<FieldChange>>,
    /// priority_history[] items.
    pub priority_history: Option<Vec<FieldChange>>,
    /// JSON tags.
    pub labels: Option<Vec<String>>,
    /// JSON collisions_acknowledged.
    pub collision_acks: Option<Vec<String>>,
    pub tasks: Option<Vec<NodeTask>>,
    /// Keys with no column, verbatim and in their original order.
    pub extras: Map<String, Value>,
}

const CLAIM_KEYS: &[&str] = &[
    "locked_by",
    "locked_by_harness",
    "locked_by_harness_session",
    "locked_at",
];
const DISPATCH_KEYS: &[&str] = &["dispatch_verb", "dispatch_brief", "model"];
const PROVENANCE_KEYS: &[&str] = &[
    "source",
    "source_kind",
    "source_project",
    "source_session_id",
    "source_harness",
    "source_cwd",
    "source_node_id",
    "source_plan_path",
    "source_inbox_msg",
    "spawned_by_session",
    "spawned_by_harness",
    "spawned_by_cwd",
    "think_session_id",
    "think_output_path",
    "origin_evidence",
    "request_origin",
];
const LIFECYCLE_KEYS: &[&str] = &[
    "deferred_at",
    "deferred_reason",
    "deferred_kind",
    "queued_at",
    "queued_reason",
    "reopened_at",
    "reopened_reason",
];

impl Node {
    /// The one dict -> typed mapping. Every key the row carries is either
    /// consumed by a typed field or kept in `extras` verbatim; nothing is
    /// dropped.
    pub fn from_json(row: &Value) -> Result<Self, ModelError> {
        let obj = row
            .as_object()
            .ok_or_else(|| ModelError(format!("row is not an object: {}", short(row))))?;
        let mut extras = Map::new();
        for (k, v) in obj {
            if !is_known_key(k) {
                extras.insert(k.clone(), v.clone());
            }
        }
        let claim_part = split_part(row, CLAIM_KEYS)?;
        let dispatch_part = split_part(row, DISPATCH_KEYS)?;
        let provenance_part = split_part(row, PROVENANCE_KEYS)?;
        let lifecycle_part = split_part(row, LIFECYCLE_KEYS)?;
        let node = Node {
            id: req_str(row, "id")?,
            ordinal: 0,
            slug: req_str(row, "slug")?,
            title: req_str(row, "title")?,
            kind: req_str(row, "type")?,
            status: Status::parse(&req_str(row, "status")?)?,
            priority: Priority::parse(&req_str(row, "priority")?)?,
            rank: opt_f64(row, "rank")?,
            project: opt_str(row, "project")?,
            cwd: opt_str(row, "cwd")?,
            domain: opt_str(row, "domain")?,
            estimate: opt_str(row, "size")?,
            difficulty: opt_str(row, "difficulty")?,
            description: opt_str(row, "details")?,
            plan_path: opt_str(row, "plan_path")?,
            parent: opt_str(row, "parent")?,
            contained_in: opt_str(row, "contained_in")?,
            superseded_by: opt_str(row, "superseded_by")?,
            caused_by: opt_str(row, "caused_by")?,
            fixes_pr: opt_i64(row, "fixes_pr")?,
            created_at: opt_str(row, "created_at")?,
            touched_at: opt_str(row, "touched_at")?,
            completed_at: opt_str(row, "completed_at")?,
            completion_note: opt_str(row, "completion_note")?,
            session_id: opt_str(row, "session_id")?,
            has_brief: opt_bool(row, "has_brief")?,
            blocks_everything: opt_bool(row, "blocks_everything")?,
            cost_usd: opt_f64(row, "cost_usd")?,
            vision_path: opt_str(row, "vision_path")?,
            artifact_url: opt_str(row, "artifact_url")?,
            archived_at: opt_str(row, "archived_at")?,
            lifecycle: Lifecycle {
                deferred_at: sub_opt_str(&lifecycle_part, "deferred_at"),
                deferred_reason: sub_opt_str(&lifecycle_part, "deferred_reason"),
                deferred_kind: sub_opt_str(&lifecycle_part, "deferred_kind"),
                queued_at: sub_opt_str(&lifecycle_part, "queued_at"),
                queued_reason: sub_opt_str(&lifecycle_part, "queued_reason"),
                reopened_at: sub_opt_str(&lifecycle_part, "reopened_at"),
                reopened_reason: sub_opt_str(&lifecycle_part, "reopened_reason"),
            },
            ownership_defect: match opt_value(row, "ownership_defect")? {
                None => None,
                Some(v) => {
                    let kind = v
                        .get("kind")
                        .and_then(Value::as_str)
                        .ok_or_else(|| ModelError("ownership_defect needs a kind".into()))?
                        .to_string();
                    Some(OwnershipDefect {
                        kind,
                        node_id: sub_opt_str(v.as_object().unwrap_or(&Map::new()), "node_id"),
                        holder: sub_opt_str(v.as_object().unwrap_or(&Map::new()), "holder"),
                        liveness: sub_opt_str(v.as_object().unwrap_or(&Map::new()), "liveness"),
                        extras: leftovers(
                            v.as_object().unwrap_or(&Map::new()),
                            &["kind", "node_id", "holder", "liveness"],
                        ),
                    })
                }
            },
            claim: NodeClaim {
                locked_by: sub_opt_str(&claim_part, "locked_by"),
                harness: sub_opt_str(&claim_part, "locked_by_harness"),
                harness_session: sub_opt_str(&claim_part, "locked_by_harness_session"),
                locked_at: sub_opt_str(&claim_part, "locked_at"),
            },
            dispatch: Dispatch {
                verb: sub_opt_str(&dispatch_part, "dispatch_verb"),
                brief: sub_opt_str(&dispatch_part, "dispatch_brief"),
                model: sub_opt_str(&dispatch_part, "model"),
            },
            provenance: Provenance {
                source: sub_opt_str(&provenance_part, "source"),
                source_kind: sub_opt_str(&provenance_part, "source_kind"),
                source_project: sub_opt_str(&provenance_part, "source_project"),
                source_session_id: sub_opt_str(&provenance_part, "source_session_id"),
                source_harness: sub_opt_str(&provenance_part, "source_harness"),
                source_cwd: sub_opt_str(&provenance_part, "source_cwd"),
                source_node_id: sub_opt_str(&provenance_part, "source_node_id"),
                source_plan_path: sub_opt_str(&provenance_part, "source_plan_path"),
                source_inbox_msg: sub_opt_str(&provenance_part, "source_inbox_msg"),
                spawned_by_session: sub_opt_str(&provenance_part, "spawned_by_session"),
                spawned_by_harness: sub_opt_str(&provenance_part, "spawned_by_harness"),
                spawned_by_cwd: sub_opt_str(&provenance_part, "spawned_by_cwd"),
                think_session_id: sub_opt_str(&provenance_part, "think_session_id"),
                think_output_path: sub_opt_str(&provenance_part, "think_output_path"),
                origin_evidence: sub_opt_str(&provenance_part, "origin_evidence"),
                request_origin: sub_opt_str(&provenance_part, "request_origin"),
            },
            primary_pr: pr_list(row)?.0,
            additional_prs: pr_list(row)?.1,
            sessions: session_list(row)?,
            comments: comment_list(row)?,
            encounters: encounter_list(row)?,
            decisions: decision_list(row)?,
            relations: Relations {
                blocked_by: opt_str_list(row, "blocked_by")?,
                related: opt_str_list(row, "related")?,
                supersedes: opt_str_list(row, "supersedes")?,
            },
            supersession: match opt_value(row, "supersession")? {
                None => None,
                Some(v) => {
                    let empty = Map::new();
                    let obj = v.as_object().unwrap_or(&empty);
                    Some(Supersession {
                        successor: sub_opt_str(obj, "successor"),
                        cause: sub_opt_str(obj, "cause"),
                        reason: sub_opt_str(obj, "reason"),
                        surfaces: sub_opt_str_list(obj, "surfaces"),
                        verified_at: sub_opt_str(obj, "verified_at"),
                        evidence_pr: sub_opt_i64(obj, "evidence_pr"),
                        matched_surfaces: sub_opt_str_list(obj, "matched_surfaces"),
                        extras: leftovers(
                            obj,
                            &[
                                "successor",
                                "cause",
                                "reason",
                                "surfaces",
                                "verified_at",
                                "evidence_pr",
                                "matched_surfaces",
                            ],
                        ),
                    })
                }
            },
            costs: cost_list(row)?,
            difficulty_history: history_list(row, "difficulty_history", false)?,
            priority_history: history_list(row, "priority_history", true)?,
            labels: opt_str_list(row, "tags")?,
            collision_acks: opt_str_list(row, "collisions_acknowledged")?,
            tasks: task_list(row)?,
            extras,
        };
        Ok(node)
    }

    /// The one typed -> dict mapping. Emitted keys use the graph.json names;
    /// `None` fields are omitted; `extras` rides verbatim in its original
    /// order after the typed keys.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        let mut put = |key: &str, value: Value| {
            if !value.is_null() {
                out.insert(key.to_string(), value);
            }
        };
        put("id", json!(self.id));
        put("status", json!(self.status.as_str()));
        put("slug", json!(self.slug));
        put("title", json!(self.title));
        put("priority", json!(self.priority.as_str()));
        put("rank", self.rank.map(|r| json!(r)).unwrap_or(Value::Null));
        put("type", json!(self.kind));
        put(
            "parent",
            self.parent
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "project",
            self.project
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "cwd",
            self.cwd.clone().map(Value::String).unwrap_or(Value::Null),
        );
        put(
            "domain",
            self.domain
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "size",
            self.estimate
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "difficulty",
            self.difficulty
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "details",
            self.description
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "plan_path",
            self.plan_path
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "contained_in",
            self.contained_in
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "superseded_by",
            self.superseded_by
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "caused_by",
            self.caused_by
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "fixes_pr",
            self.fixes_pr.map(|v| json!(v)).unwrap_or(Value::Null),
        );
        put(
            "created_at",
            self.created_at
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "touched_at",
            self.touched_at
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "completed_at",
            self.completed_at
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "completion_note",
            self.completion_note
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "session_id",
            self.session_id
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "has_brief",
            self.has_brief.map(|v| json!(v)).unwrap_or(Value::Null),
        );
        put(
            "blocks_everything",
            self.blocks_everything
                .map(|v| json!(v))
                .unwrap_or(Value::Null),
        );
        put(
            "cost_usd",
            self.cost_usd.map(|v| json!(v)).unwrap_or(Value::Null),
        );
        put(
            "vision_path",
            self.vision_path
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "artifact_url",
            self.artifact_url
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "archived_at",
            self.archived_at
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "deferred_at",
            self.lifecycle
                .deferred_at
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "deferred_reason",
            self.lifecycle
                .deferred_reason
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "deferred_kind",
            self.lifecycle
                .deferred_kind
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "queued_at",
            self.lifecycle
                .queued_at
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "queued_reason",
            self.lifecycle
                .queued_reason
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "reopened_at",
            self.lifecycle
                .reopened_at
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "reopened_reason",
            self.lifecycle
                .reopened_reason
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "locked_by",
            self.claim
                .locked_by
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "locked_by_harness",
            self.claim
                .harness
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "locked_by_harness_session",
            self.claim
                .harness_session
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "locked_at",
            self.claim
                .locked_at
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "dispatch_verb",
            self.dispatch
                .verb
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "dispatch_brief",
            self.dispatch
                .brief
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put(
            "model",
            self.dispatch
                .model
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        for (key, value) in provenance_pairs(&self.provenance) {
            put(key, value);
        }
        if let Some(primary) = &self.primary_pr {
            put(
                "pr_number",
                primary.number.map(|v| json!(v)).unwrap_or(Value::Null),
            );
            put(
                "pr_url",
                primary
                    .url
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            put(
                "merge_status",
                primary
                    .merge_status
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
        }
        if let Some(rest) = &self.additional_prs {
            let rest: Vec<Value> = rest.iter().map(|pr| pr_to_json(pr)).collect();
            put("additional_prs", Value::Array(rest));
        }
        if let Some(sup) = &self.supersession {
            put("supersession", supersession_to_json(sup));
        }
        if let Some(defect) = &self.ownership_defect {
            let mut obj = Map::new();
            obj.insert("kind".into(), json!(defect.kind));
            for (k, v) in [
                ("node_id", &defect.node_id),
                ("holder", &defect.holder),
                ("liveness", &defect.liveness),
            ] {
                if let Some(s) = v {
                    obj.insert(k.to_string(), json!(s));
                }
            }
            put_extras(&mut obj, &defect.extras);
            put("ownership_defect", Value::Object(obj));
        }
        put(
            "tags",
            self.labels.clone().map(|l| json!(l)).unwrap_or(Value::Null),
        );
        put(
            "collisions_acknowledged",
            self.collision_acks
                .clone()
                .map(|l| json!(l))
                .unwrap_or(Value::Null),
        );
        put(
            "blocked_by",
            self.relations
                .blocked_by
                .clone()
                .map(|l| json!(l))
                .unwrap_or(Value::Null),
        );
        put(
            "related",
            self.relations
                .related
                .clone()
                .map(|l| json!(l))
                .unwrap_or(Value::Null),
        );
        put(
            "supersedes",
            self.relations
                .supersedes
                .clone()
                .map(|l| json!(l))
                .unwrap_or(Value::Null),
        );
        put(
            "sessions",
            self.sessions
                .clone()
                .map(|list| Value::Array(list.iter().map(session_to_json).collect()))
                .unwrap_or(Value::Null),
        );
        put(
            "progress_notes",
            self.comments
                .clone()
                .map(|list| Value::Array(list.iter().map(comment_to_json).collect()))
                .unwrap_or(Value::Null),
        );
        put(
            "encounters",
            self.encounters
                .clone()
                .map(|list| Value::Array(list.iter().map(encounter_to_json).collect()))
                .unwrap_or(Value::Null),
        );
        put(
            "decisions",
            self.decisions
                .clone()
                .map(|list| Value::Array(list.iter().map(decision_to_json).collect()))
                .unwrap_or(Value::Null),
        );
        put(
            "cost_sessions",
            self.costs
                .clone()
                .map(|list| Value::Array(list.iter().map(cost_to_json).collect()))
                .unwrap_or(Value::Null),
        );
        put(
            "difficulty_history",
            self.difficulty_history
                .clone()
                .map(|list| Value::Array(list.iter().map(field_change_to_json).collect()))
                .unwrap_or(Value::Null),
        );
        put(
            "priority_history",
            self.priority_history
                .clone()
                .map(|list| Value::Array(list.iter().map(field_change_to_json).collect()))
                .unwrap_or(Value::Null),
        );
        put(
            "tasks",
            self.tasks
                .clone()
                .map(|list| Value::Array(list.iter().map(task_to_json).collect()))
                .unwrap_or(Value::Null),
        );
        for (k, v) in &self.extras {
            // The null-stripped export rule, recursively: a key whose value
            // is null (at any depth) is indistinguishable from an absent
            // key, so it exports absent.
            if !v.is_null() {
                out.insert(k.clone(), strip_nulls_value(v));
            }
        }
        Value::Object(out)
    }
}

// ---------------------------------------------------------------------------
// List readers and emitters
// ---------------------------------------------------------------------------

/// Reads the primary pull request and additional_prs. The primary exists
/// only when one of its three keys holds a value; an additional list without
/// a primary is legal (12 archive rows).
fn pr_list(row: &Value) -> Result<(Option<PullRequest>, Option<Vec<PullRequest>>), ModelError> {
    let number = opt_i64(row, "pr_number")?;
    let url = opt_str(row, "pr_url")?;
    let merge_status = opt_str(row, "merge_status")?;
    let additional = match row.get("additional_prs") {
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(pr_from_json)
                .collect::<Result<Vec<_>, ModelError>>()?,
        ),
        Some(other) => return Err(ModelError(format!("additional_prs is not a list: {other}"))),
        None => None,
    };
    let primary = if number.is_some() || url.is_some() || merge_status.is_some() {
        Some(PullRequest {
            number,
            url,
            merge_status,
            note: None,
            extras: Map::new(),
        })
    } else {
        None
    };
    if primary.is_none() && additional.is_none() {
        return Ok((None, None));
    }
    Ok((primary, additional))
}

fn pr_from_json(item: &Value) -> Result<PullRequest, ModelError> {
    let obj = item.as_object().ok_or_else(|| {
        ModelError(format!(
            "additional_prs item is not an object: {}",
            short(item)
        ))
    })?;
    Ok(PullRequest {
        number: sub_opt_i64(obj, "number"),
        url: sub_opt_str(obj, "url"),
        merge_status: sub_opt_str(obj, "merge_status"),
        note: sub_opt_str(obj, "note"),
        extras: leftovers(obj, &["number", "url", "merge_status", "note"]),
    })
}

fn pr_to_json(pr: &PullRequest) -> Value {
    let mut obj = Map::new();
    if let Some(n) = pr.number {
        obj.insert("number".into(), json!(n));
    }
    if let Some(v) = &pr.url {
        obj.insert("url".into(), json!(v));
    }
    if let Some(v) = &pr.merge_status {
        obj.insert("merge_status".into(), json!(v));
    }
    if let Some(v) = &pr.note {
        obj.insert("note".into(), json!(v));
    }
    put_extras(&mut obj, &pr.extras);
    Value::Object(obj)
}

macro_rules! obj_list {
    ($fn_name:ident, $key:literal, $item_ty:ty, $from:expr, $to:expr) => {
        fn $fn_name(row: &Value) -> Result<Option<Vec<$item_ty>>, ModelError> {
            match row.get($key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Array(items)) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        let obj = item.as_object().ok_or_else(|| {
                            ModelError(format!(
                                concat!($key, " item is not an object: {}"),
                                short(item)
                            ))
                        })?;
                        out.push($from(obj)?);
                    }
                    Ok(Some(out))
                }
                Some(other) => Err(ModelError(format!(
                    concat!($key, " is not a list: {}"),
                    other
                ))),
            }
        }
    };
}

obj_list!(
    session_list,
    "sessions",
    SessionRecord,
    |o: &Map<String, Value>| -> Result<SessionRecord, ModelError> {
        Ok(SessionRecord {
            phase: sub_opt_str(o, "phase")
                .ok_or_else(|| ModelError("sessions item needs phase".into()))?,
            harness: sub_opt_str(o, "harness")
                .ok_or_else(|| ModelError("sessions item needs harness".into()))?,
            session_id: sub_opt_str(o, "session_id")
                .ok_or_else(|| ModelError("sessions item needs session_id".into()))?,
            started_at: sub_opt_str(o, "started_at"),
            ended_at: sub_opt_str(o, "ended_at"),
            ended_by: sub_opt_str(o, "ended_by"),
            effort: sub_opt_value(o, "effort"),
            at: sub_opt_value(o, "at"),
            claimed_at: sub_opt_value(o, "claimed_at"),
            observed_model: sub_opt_value(o, "observed_model"),
            merge_grant: sub_opt_value(o, "merge_grant"),
            extras: leftovers(
                o,
                &[
                    "phase",
                    "harness",
                    "session_id",
                    "started_at",
                    "ended_at",
                    "ended_by",
                    "effort",
                    "at",
                    "claimed_at",
                    "observed_model",
                    "merge_grant",
                ],
            ),
        })
    },
    session_to_json
);

obj_list!(
    comment_list,
    "progress_notes",
    Comment,
    |o: &Map<String, Value>| -> Result<Comment, ModelError> {
        Ok(Comment {
            created_at: sub_opt_str(o, "ts"),
            body: sub_opt_str(o, "text"),
            kind: sub_opt_str(o, "kind"),
            title: sub_opt_str(o, "title"),
            details: sub_opt_str(o, "details"),
            difficulty: sub_opt_str(o, "difficulty"),
            source: sub_opt_str(o, "source"),
            source_session_id: sub_opt_str(o, "source_session_id"),
            source_harness: sub_opt_str(o, "source_harness"),
            extras: leftovers(
                o,
                &[
                    "ts",
                    "text",
                    "kind",
                    "title",
                    "details",
                    "difficulty",
                    "source",
                    "source_session_id",
                    "source_harness",
                ],
            ),
        })
    },
    comment_to_json
);

obj_list!(
    encounter_list,
    "encounters",
    Encounter,
    |o: &Map<String, Value>| -> Result<Encounter, ModelError> {
        Ok(Encounter {
            ts: sub_opt_str(o, "ts")
                .ok_or_else(|| ModelError("encounters item needs ts".into()))?,
            evidence: sub_opt_str(o, "evidence")
                .ok_or_else(|| ModelError("encounters item needs evidence".into()))?,
            session_id: sub_opt_str(o, "session_id"),
            voter_key: sub_opt_str(o, "voter_key"),
            voter_kind: sub_opt_str(o, "voter_kind"),
            harness: sub_opt_str(o, "harness"),
            fno_id: sub_opt_str(o, "fno_id"),
            effort: sub_opt_str(o, "effort"),
            model: sub_opt_str(o, "model"),
            extras: leftovers(
                o,
                &[
                    "ts",
                    "evidence",
                    "session_id",
                    "voter_key",
                    "voter_kind",
                    "harness",
                    "fno_id",
                    "effort",
                    "model",
                ],
            ),
        })
    },
    encounter_to_json
);

obj_list!(
    decision_list,
    "decisions",
    DecisionRef,
    |o: &Map<String, Value>| -> Result<DecisionRef, ModelError> {
        Ok(DecisionRef {
            decision_id: sub_opt_str(o, "decision_id"),
            ts: sub_opt_str(o, "ts"),
            extras: leftovers(o, &["decision_id", "ts"]),
        })
    },
    decision_to_json
);

obj_list!(
    cost_list,
    "cost_sessions",
    CostRecord,
    |o: &Map<String, Value>| -> Result<CostRecord, ModelError> {
        Ok(CostRecord {
            session_id: sub_opt_str(o, "session_id")
                .ok_or_else(|| ModelError("cost_sessions item needs session_id".into()))?,
            cost_usd: o
                .get("cost_usd")
                .and_then(Value::as_f64)
                .ok_or_else(|| ModelError("cost_sessions item needs a numeric cost_usd".into()))?,
            timestamp: sub_opt_str(o, "timestamp"),
            extras: leftovers(o, &["session_id", "cost_usd", "timestamp"]),
        })
    },
    cost_to_json
);

obj_list!(
    task_list,
    "tasks",
    NodeTask,
    |o: &Map<String, Value>| -> Result<NodeTask, ModelError> {
        Ok(NodeTask {
            id: sub_opt_str(o, "id").ok_or_else(|| ModelError("tasks item needs id".into()))?,
            status: sub_opt_str(o, "status"),
            owner: sub_opt_str(o, "owner"),
            claimed_at: sub_opt_str(o, "claimed_at"),
            extras: leftovers(o, &["id", "status", "owner", "claimed_at"]),
        })
    },
    task_to_json
);

fn history_list(
    row: &Value,
    key: &str,
    priority: bool,
) -> Result<Option<Vec<FieldChange>>, ModelError> {
    match row.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let obj = item.as_object().ok_or_else(|| {
                    ModelError(format!("{key} item is not an object: {}", short(item)))
                })?;
                out.push(if priority {
                    FieldChange {
                        value: None,
                        from: sub_opt_str(obj, "from"),
                        to: sub_opt_str(obj, "to"),
                        source: sub_opt_str(obj, "source"),
                        ts: sub_opt_str(obj, "ts"),
                        extras: leftovers(obj, &["from", "to", "source", "ts"]),
                    }
                } else {
                    FieldChange {
                        value: sub_opt_str(obj, "value"),
                        from: None,
                        to: None,
                        source: sub_opt_str(obj, "source"),
                        ts: sub_opt_str(obj, "ts"),
                        extras: leftovers(obj, &["value", "source", "ts"]),
                    }
                });
            }
            Ok(Some(out))
        }
        Some(other) => Err(ModelError(format!("{key} is not a list: {other}"))),
    }
}

fn session_to_json(s: &SessionRecord) -> Value {
    let mut obj = Map::new();
    obj.insert("phase".into(), json!(s.phase));
    obj.insert("harness".into(), json!(s.harness));
    obj.insert("session_id".into(), json!(s.session_id));
    if let Some(v) = &s.started_at {
        obj.insert("started_at".into(), json!(v));
    }
    if let Some(v) = &s.ended_at {
        obj.insert("ended_at".into(), json!(v));
    }
    if let Some(v) = &s.ended_by {
        obj.insert("ended_by".into(), json!(v));
    }
    for (key, value) in [
        ("effort", &s.effort),
        ("at", &s.at),
        ("claimed_at", &s.claimed_at),
        ("observed_model", &s.observed_model),
        ("merge_grant", &s.merge_grant),
    ] {
        if let Some(v) = value {
            obj.insert(key.to_string(), v.clone());
        }
    }
    put_extras(&mut obj, &s.extras);
    Value::Object(obj)
}

fn comment_to_json(c: &Comment) -> Value {
    let mut obj = Map::new();
    if let Some(v) = &c.created_at {
        obj.insert("ts".into(), json!(v));
    }
    if let Some(v) = &c.body {
        obj.insert("text".into(), json!(v));
    }
    for (key, value) in [
        ("kind", &c.kind),
        ("title", &c.title),
        ("details", &c.details),
        ("difficulty", &c.difficulty),
        ("source", &c.source),
        ("source_session_id", &c.source_session_id),
        ("source_harness", &c.source_harness),
    ] {
        if let Some(v) = value {
            obj.insert(key.to_string(), json!(v));
        }
    }
    put_extras(&mut obj, &c.extras);
    Value::Object(obj)
}

fn encounter_to_json(e: &Encounter) -> Value {
    let mut obj = Map::new();
    obj.insert("ts".into(), json!(e.ts));
    obj.insert("evidence".into(), json!(e.evidence));
    for (key, value) in [
        ("session_id", &e.session_id),
        ("voter_key", &e.voter_key),
        ("voter_kind", &e.voter_kind),
        ("harness", &e.harness),
        ("fno_id", &e.fno_id),
        ("effort", &e.effort),
        ("model", &e.model),
    ] {
        if let Some(v) = value {
            obj.insert(key.to_string(), json!(v));
        }
    }
    put_extras(&mut obj, &e.extras);
    Value::Object(obj)
}

fn decision_to_json(d: &DecisionRef) -> Value {
    let mut obj = Map::new();
    if let Some(v) = &d.decision_id {
        obj.insert("decision_id".into(), json!(v));
    }
    if let Some(v) = &d.ts {
        obj.insert("ts".into(), json!(v));
    }
    put_extras(&mut obj, &d.extras);
    Value::Object(obj)
}

fn cost_to_json(c: &CostRecord) -> Value {
    let mut obj = Map::new();
    obj.insert("session_id".into(), json!(c.session_id));
    obj.insert("cost_usd".into(), json!(c.cost_usd));
    if let Some(v) = &c.timestamp {
        obj.insert("timestamp".into(), json!(v));
    }
    put_extras(&mut obj, &c.extras);
    Value::Object(obj)
}

fn task_to_json(t: &NodeTask) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), json!(t.id));
    if let Some(v) = &t.status {
        obj.insert("status".into(), json!(v));
    }
    if let Some(v) = &t.owner {
        obj.insert("owner".into(), json!(v));
    }
    if let Some(v) = &t.claimed_at {
        obj.insert("claimed_at".into(), json!(v));
    }
    put_extras(&mut obj, &t.extras);
    Value::Object(obj)
}

fn supersession_to_json(s: &Supersession) -> Value {
    let mut obj = Map::new();
    if let Some(v) = &s.successor {
        obj.insert("successor".into(), json!(v));
    }
    if let Some(v) = &s.cause {
        obj.insert("cause".into(), json!(v));
    }
    if let Some(v) = &s.reason {
        obj.insert("reason".into(), json!(v));
    }
    if let Some(v) = &s.surfaces {
        obj.insert("surfaces".into(), json!(v));
    }
    if let Some(v) = &s.verified_at {
        obj.insert("verified_at".into(), json!(v));
    }
    if let Some(v) = s.evidence_pr {
        obj.insert("evidence_pr".into(), json!(v));
    }
    if let Some(v) = &s.matched_surfaces {
        obj.insert("matched_surfaces".into(), json!(v));
    }
    put_extras(&mut obj, &s.extras);
    Value::Object(obj)
}

fn field_change_to_json(c: &FieldChange) -> Value {
    let mut obj = Map::new();
    if let Some(v) = &c.value {
        obj.insert("value".into(), json!(v));
    }
    if let Some(v) = &c.from {
        obj.insert("from".into(), json!(v));
    }
    if let Some(v) = &c.to {
        obj.insert("to".into(), json!(v));
    }
    if let Some(v) = &c.source {
        obj.insert("source".into(), json!(v));
    }
    if let Some(v) = &c.ts {
        obj.insert("ts".into(), json!(v));
    }
    put_extras(&mut obj, &c.extras);
    Value::Object(obj)
}

fn provenance_pairs(p: &Provenance) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    let fields: [(&'static str, &Option<String>); 16] = [
        ("source", &p.source),
        ("source_kind", &p.source_kind),
        ("source_project", &p.source_project),
        ("source_session_id", &p.source_session_id),
        ("source_harness", &p.source_harness),
        ("source_cwd", &p.source_cwd),
        ("source_node_id", &p.source_node_id),
        ("source_plan_path", &p.source_plan_path),
        ("source_inbox_msg", &p.source_inbox_msg),
        ("request_origin", &p.request_origin),
        ("origin_evidence", &p.origin_evidence),
        ("spawned_by_session", &p.spawned_by_session),
        ("spawned_by_harness", &p.spawned_by_harness),
        ("spawned_by_cwd", &p.spawned_by_cwd),
        ("think_session_id", &p.think_session_id),
        ("think_output_path", &p.think_output_path),
    ];
    for (key, value) in fields {
        out.push((key, value.clone().map(Value::String).unwrap_or(Value::Null)));
    }
    out
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The row's known keys split out into a map; unconsumed known keys with
/// null values stay so the emitter can re-omit them.
fn split_part(row: &Value, keys: &[&str]) -> Result<Map<String, Value>, ModelError> {
    let obj = row
        .as_object()
        .ok_or_else(|| ModelError(format!("row is not an object: {}", short(row))))?;
    let mut typed = Map::new();
    for key in keys {
        if let Some(v) = obj.get(*key) {
            typed.insert((*key).to_string(), v.clone());
        }
    }
    Ok(typed)
}

/// Re-emit an item's extras with the null-stripped export rule.
fn put_extras(obj: &mut Map<String, Value>, extras: &Map<String, Value>) {
    for (k, v) in extras {
        if !v.is_null() {
            obj.insert(k.clone(), strip_nulls_value(v));
        }
    }
}

fn leftovers(obj: &Map<String, Value>, known: &[&str]) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in obj {
        if !known.contains(&k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn is_known_key(key: &str) -> bool {
    matches!(
        key,
        "id" | "slug"
            | "title"
            | "type"
            | "status"
            | "priority"
            | "rank"
            | "project"
            | "cwd"
            | "domain"
            | "size"
            | "difficulty"
            | "details"
            | "plan_path"
            | "parent"
            | "contained_in"
            | "superseded_by"
            | "caused_by"
            | "fixes_pr"
            | "created_at"
            | "touched_at"
            | "completed_at"
            | "completion_note"
            | "session_id"
            | "has_brief"
            | "blocks_everything"
            | "cost_usd"
            | "vision_path"
            | "artifact_url"
            | "archived_at"
            | "ownership_defect"
            | "locked_by"
            | "locked_by_harness"
            | "locked_by_harness_session"
            | "locked_at"
            | "dispatch_verb"
            | "dispatch_brief"
            | "model"
            | "source"
            | "source_kind"
            | "source_project"
            | "source_session_id"
            | "source_harness"
            | "source_cwd"
            | "source_node_id"
            | "source_plan_path"
            | "source_inbox_msg"
            | "spawned_by_session"
            | "spawned_by_harness"
            | "spawned_by_cwd"
            | "think_session_id"
            | "think_output_path"
            | "origin_evidence"
            | "request_origin"
            | "pr_number"
            | "pr_url"
            | "merge_status"
            | "additional_prs"
            | "sessions"
            | "progress_notes"
            | "encounters"
            | "decisions"
            | "blocked_by"
            | "related"
            | "supersedes"
            | "supersession"
            | "cost_sessions"
            | "difficulty_history"
            | "priority_history"
            | "tags"
            | "collisions_acknowledged"
            | "tasks"
            | "deferred_at"
            | "deferred_reason"
            | "deferred_kind"
            | "queued_at"
            | "queued_reason"
            | "reopened_at"
            | "reopened_reason"
    )
}

/// Recursively remove null-valued object keys, the null-stripped export
/// rule the parity compare shares.
fn strip_nulls_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if !v.is_null() {
                    out.insert(k.clone(), strip_nulls_value(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(strip_nulls_value).collect()),
        _ => value.clone(),
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 80 {
        format!("{}...", &s[..80])
    } else {
        s
    }
}
