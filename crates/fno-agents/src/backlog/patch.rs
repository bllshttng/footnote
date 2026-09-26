//! `fno-agents backlog-update` : the one patch door for stored node
//! fields, status included. Status is never a stored input - the derivation
//! ladder rewrites it from facts on every write - so `--status X` changes the
//! facts that derive X, recomputes, and refuses unless the readback agrees.
//! Every carried lifecycle verb (CLI defer/retract/undefer/unsupersede, the
//! typed `node_update` status arm) is a transport over this planner.
//!
//! The cycle: resolve -> read rows + version -> patch a working copy -> build
//! the full plan-rung map -> recompute -> validate (old, new) -> commit with
//! `base_version` (retry on conflict) -> emit the receipt from the READBACK,
//! never from the intent. That readback is what makes the false-receipt family
//! (: `Undeferred` printed while the node stayed superseded)
//! unconstructible here.

use crate::backlog::model::Priority;
use crate::backlog_ready;
use crate::graph_get::{default_graph_path, field_eq};
use crate::graph_store::{
    self, entry_id, now_isoformat, recompute_statuses_with_plan_rungs, MutateInput, StoreError,
    DEFAULT_LOCK_TIMEOUT, TERMINAL_RUNGS,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Ten deferred kinds (`_constants.DEFERRED_KINDS`); a parity test pins this
/// table against the Python tuple, so the two vocabularies cannot drift.
pub const DEFERRED_KINDS: &[&str] = &[
    "expired",
    "blocked",
    "wont_do",
    "retracted",
    "superseded",
    "later",
    "contingent",
    "carveout",
    "internal_only",
    "junk",
];

/// Exact-string classifier (`_constants.DEFERRED_KIND_BY_REASON`): the
/// machine-stamped reasons only, byte for byte. THE Rust table - the keeper's
/// op and the door both classify through here.
pub fn classify_deferred_reason(reason: &str) -> Option<&'static str> {
    match reason {
        "stale >30d, drained by maintain" => Some("expired"),
        "stale-quarantine (guard)" => Some("expired"),
        _ => None,
    }
}

/// One parsed invocation of the patch door.
#[derive(Debug)]
pub struct PatchRequest {
    pub node: String,
    pub status: Option<String>,
    pub leave: Option<String>,
    /// Ordered `field=value` pairs; the literal `null` clears.
    pub sets: Vec<(String, String)>,
}

#[derive(Debug, serde::Serialize)]
pub struct Change {
    pub id: String,
    pub field: String,
    pub old: Value,
    pub new: Value,
}

#[derive(Debug, serde::Serialize)]
pub struct StatusPair {
    pub from: String,
    pub to: String,
}

#[derive(Debug, serde::Serialize)]
pub struct PatchReceipt {
    pub node: String,
    pub changes: Vec<Change>,
    pub status: StatusPair,
    pub version: String,
    pub unchanged: bool,
}

/// A refusal exits 2 with one stderr line; an error (unreadable store, lock
/// timeout) exits 1. `message` is the full line.
#[derive(Debug)]
pub struct PatchRefusal {
    pub message: String,
    pub exit: i32,
}

impl PatchRefusal {
    fn refused(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit: 2,
        }
    }
    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Field policy: every stored field maps to exactly one policy
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Text,
    PriorityWord,
    NodeType,
    SizeWord,
    Float,
    Flag,
    DeferredKind,
    ClearOnly,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Policy {
    /// Writable through `--set` with this value kind.
    Settable(Kind),
    /// Settable, and the lifecycle validators run on the result.
    StatusFact(Kind),
    /// Read-only; the string is the reason a caller sees in the refusal.
    Derived(&'static str),
    /// Another verb or flag owns the write; the refusal names it.
    Owned(&'static str),
}

const BIRTH_RECORD: &str = "birth-record field, written once at node creation";

/// One row per stored field name: `CANONICAL_FIELD_ORDER` plus the model key
/// groups that live outside it (`CLAIM_KEYS` are all canonical; the extra
/// names here are the dispatch and think pairs). The coverage test asserts
/// every union name appears exactly once, so a new stored field cannot land
/// without a policy.
const FIELD_POLICY: &[(&str, Policy)] = &[
    ("title", Policy::Settable(Kind::Text)),
    ("priority", Policy::Settable(Kind::PriorityWord)),
    ("type", Policy::Settable(Kind::NodeType)),
    ("size", Policy::Settable(Kind::SizeWord)),
    ("rank", Policy::Settable(Kind::Float)),
    ("has_brief", Policy::Settable(Kind::Flag)),
    ("project", Policy::Settable(Kind::Text)),
    ("cwd", Policy::Settable(Kind::Text)),
    ("domain", Policy::Settable(Kind::Text)),
    ("details", Policy::Settable(Kind::Text)),
    ("batch", Policy::Settable(Kind::Text)),
    ("roadmap_id", Policy::Settable(Kind::Text)),
    ("vision_path", Policy::Settable(Kind::Text)),
    ("artifact_url", Policy::Settable(Kind::Text)),
    ("dispatch_verb", Policy::Settable(Kind::Text)),
    ("dispatch_brief", Policy::Settable(Kind::Text)),
    ("model", Policy::Settable(Kind::Text)),
    ("deferred_reason", Policy::StatusFact(Kind::Text)),
    ("deferred_kind", Policy::StatusFact(Kind::DeferredKind)),
    ("id", Policy::Derived("ids are immutable")),
    (
        "status",
        Policy::Derived("status is derived from facts; use --status"),
    ),
    ("slug", Policy::Derived("slugs are immutable")),
    (
        "children",
        Policy::Derived("children are derived from each child's parent; use --parent on the child"),
    ),
    (
        "blocked_reason",
        Policy::Derived("blocked is derived on read from blocked_by; use --add-blocker"),
    ),
    (
        "ownership_defect",
        Policy::Derived("an ownership diagnostic the derivation stamps and clears"),
    ),
    (
        "touched_at",
        Policy::Derived("the store stamps touched_at on every write"),
    ),
    (
        "created_at",
        Policy::Derived("created_at is written once at birth"),
    ),
    (
        "session_id",
        Policy::Derived(
            "mirror of the node:<id> claim; use fno agents claim acquire|release node:<id>",
        ),
    ),
    (
        "dep",
        Policy::Derived("legacy dependency field kept for old readers"),
    ),
    (
        "stub_against",
        Policy::Derived("the reconcile machinery writes stub_against"),
    ),
    (
        "contract_version",
        Policy::Derived("the reconcile contract stamps contract_version"),
    ),
    ("source", Policy::Derived(BIRTH_RECORD)),
    ("source_kind", Policy::Derived(BIRTH_RECORD)),
    ("source_project", Policy::Derived(BIRTH_RECORD)),
    ("source_session_id", Policy::Derived(BIRTH_RECORD)),
    ("source_harness", Policy::Derived(BIRTH_RECORD)),
    ("source_cwd", Policy::Derived(BIRTH_RECORD)),
    ("source_node_id", Policy::Derived(BIRTH_RECORD)),
    ("source_plan_path", Policy::Derived(BIRTH_RECORD)),
    ("source_inbox_msg", Policy::Derived(BIRTH_RECORD)),
    ("spawned_by_session", Policy::Derived(BIRTH_RECORD)),
    ("spawned_by_harness", Policy::Derived(BIRTH_RECORD)),
    ("spawned_by_cwd", Policy::Derived(BIRTH_RECORD)),
    ("request_origin", Policy::Derived(BIRTH_RECORD)),
    ("origin_evidence", Policy::Derived(BIRTH_RECORD)),
    ("think_session_id", Policy::Derived(BIRTH_RECORD)),
    ("think_output_path", Policy::Derived(BIRTH_RECORD)),
    (
        "locked_by",
        Policy::Derived(
            "projection of the node:<id> claim; use fno agents claim acquire|release node:<id>",
        ),
    ),
    (
        "locked_by_harness",
        Policy::Derived(
            "projection of the node:<id> claim; use fno agents claim acquire|release node:<id>",
        ),
    ),
    (
        "locked_by_harness_session",
        Policy::Derived(
            "projection of the node:<id> claim; use fno agents claim acquire|release node:<id>",
        ),
    ),
    (
        "locked_at",
        Policy::Derived(
            "projection of the node:<id> claim; use fno agents claim acquire|release node:<id>",
        ),
    ),
    (
        "completed_at",
        Policy::Owned("fno backlog done, or fno backlog reopen to leave done"),
    ),
    (
        "completion_note",
        Policy::Owned("fno backlog done, or fno backlog reopen to leave done"),
    ),
    (
        "deferred_at",
        Policy::Owned("--status deferred, or --leave deferred"),
    ),
    ("plan_path", Policy::Owned("--plan-path")),
    ("pr_number", Policy::Owned("--pr-number")),
    ("pr_url", Policy::Owned("--pr-number")),
    ("additional_prs", Policy::Owned("--pr-number")),
    ("merge_status", Policy::Settable(Kind::ClearOnly)),
    ("parent", Policy::Owned("--parent")),
    (
        "blocked_by",
        Policy::Owned("--add-blocker / --remove-blocker"),
    ),
    ("related", Policy::Owned("--related")),
    ("difficulty", Policy::Owned("--difficulty")),
    ("sessions", Policy::Owned("fno backlog session")),
    ("progress_notes", Policy::Owned("fno backlog note")),
    ("encounters", Policy::Owned("fno backlog encounter")),
    ("queued_at", Policy::Owned("fno backlog queue")),
    ("queued_reason", Policy::Owned("fno backlog queue")),
    ("contained_in", Policy::Owned("fno backlog contain")),
    ("cost_usd", Policy::Owned("the execution ledger")),
    ("cost_sessions", Policy::Owned("the execution ledger")),
    (
        "collisions_acknowledged",
        Policy::Owned("--acknowledge-collisions"),
    ),
    (
        "supersedes",
        Policy::Owned("fno backlog supersede, or --leave superseded"),
    ),
    (
        "superseded_by",
        Policy::Owned("fno backlog supersede, or --leave superseded"),
    ),
    (
        "supersession",
        Policy::Owned("fno backlog supersede, or --leave superseded"),
    ),
];

/// Names the policy table covers beyond `CANONICAL_FIELD_ORDER` (the model
/// key groups' extra members). The coverage test keys on this; the table
/// itself is runtime code, so the list stays test-only.
#[cfg(test)]
const MODEL_EXTRA_KEYS: &[&str] = &[
    "dispatch_verb",
    "dispatch_brief",
    "model",
    "think_session_id",
    "think_output_path",
];

fn policy_for(name: &str) -> Option<Policy> {
    FIELD_POLICY
        .iter()
        .find(|(field, _)| *field == name)
        .map(|(_, policy)| *policy)
}

fn settable_names() -> Vec<&'static str> {
    FIELD_POLICY
        .iter()
        .filter(|(_, p)| matches!(p, Policy::Settable(_) | Policy::StatusFact(_)))
        .map(|(name, _)| *name)
        .collect()
}

/// Coerce one `--set` raw value. The literal `null` clears text, floats,
/// flags, sizes and kinds; an enum word that cannot clear (priority) refuses.
fn coerce(id: &str, field: &str, kind: Kind, raw: &str) -> Result<Value, PatchRefusal> {
    let refused = |msg: String| PatchRefusal::refused(format!("refused: {id} {msg}"));
    if raw == "null" {
        return match kind {
            Kind::PriorityWord => Err(refused(format!(
                "priority is required; supply a word: --set {field}=<p0..p3>"
            ))),
            _ => Ok(Value::Null),
        };
    }
    Ok(match kind {
        Kind::Text => Value::String(raw.to_string()),
        Kind::PriorityWord => {
            let word = Priority::parse(raw)
                .map_err(|_| {
                    refused(format!(
                        "priority must be one of p0, p1, p2, p3. Supply it with: --set {field}=<word>"
                    ))
                })?
                .as_str();
            Value::String(word.to_string())
        }
        Kind::NodeType => {
            if matches!(raw, "feature" | "epic" | "bug") {
                Value::String(raw.to_string())
            } else {
                return Err(refused(format!(
                    "type must be one of feature, epic, bug. Supply it with: --set {field}=<word>"
                )));
            }
        }
        Kind::SizeWord => {
            if matches!(raw, "S" | "M" | "L") {
                Value::String(raw.to_string())
            } else {
                return Err(refused(format!(
                    "size must be one of S, M, L. Supply it with: --set {field}=<word>"
                )));
            }
        }
        Kind::Float => {
            let n: f64 = raw.parse().map_err(|_| {
                refused(format!(
                    "rank must be a number. Supply it with: --set {field}=<n>"
                ))
            })?;
            json!(n)
        }
        Kind::Flag => match raw {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => {
                return Err(refused(format!(
                    "{field} must be true or false. Supply it with: --set {field}=<bool>"
                )))
            }
        },
        Kind::DeferredKind => {
            if DEFERRED_KINDS.contains(&raw) {
                Value::String(raw.to_string())
            } else {
                return Err(refused(format!(
                    "deferred_kind must be one of {}. Supply it with: --set {field}=<kind>",
                    DEFERRED_KINDS.join(", ")
                )));
            }
        }
        Kind::ClearOnly => {
            return Err(refused(format!(
                "merge_status is written from forge state by fno do pr merge and reconcile; only --set merge_status=null clears it"
            )));
        }
    })
}

// ---------------------------------------------------------------------------
// Status words and refusals
// ---------------------------------------------------------------------------

/// The `--status` vocabulary; `claimed` is the legacy spelling of
/// `in_progress` (the store migrates it on read).
fn normalize_status_word(word: &str) -> Option<&'static str> {
    match word {
        "idea" => Some("idea"),
        "design" => Some("design"),
        "ready" => Some("ready"),
        "deferred" => Some("deferred"),
        "superseded" => Some("superseded"),
        "done" => Some("done"),
        "in_review" => Some("in_review"),
        "in_progress" | "claimed" => Some("in_progress"),
        "blocked" => Some("blocked"),
        _ => None,
    }
}

/// The moves the door refuses and their owners (the design table): entering
/// superseded owns the chain refusal, done owns the gh merge cross-check,
/// in_review owns the gh resolver, in_progress lives in lockfiles, blocked is
/// derived on read.
fn target_owner(word: &str, id: &str) -> Option<String> {
    let owned = match word {
        "superseded" => format!("fno backlog supersede <new-id> --replaces {id} --cause --surface"),
        "done" => format!("fno backlog done {id}"),
        "in_review" => format!("fno backlog update {id} --pr-number <n>"),
        "in_progress" => format!("fno do target start {id}"),
        "blocked" => {
            "derived on read from blocked_by; supply it with --add-blocker <id>".to_string()
        }
        _ => return None,
    };
    Some(owned)
}

/// The first fact that would still hold a node out of a rung target, with the
/// flag or verb that clears it.
fn holding_fact(entry: &Value, id: &str) -> Option<String> {
    let truthy = |key: &str| entry.get(key).map(|v| !v.is_null()).unwrap_or(false);
    if truthy("completed_at") {
        return Some(format!(
            "completed_at holds. Supply it with: fno backlog reopen {id}"
        ));
    }
    if truthy("superseded_by") {
        return Some(format!(
            "superseded_by holds. Supply it with: fno backlog update {id} --leave superseded"
        ));
    }
    if truthy("deferred_at") {
        return Some(format!(
            "deferred_at holds. Supply it with: fno backlog update {id} --leave deferred"
        ));
    }
    if truthy("pr_number") {
        return Some(format!(
            "pr_number holds. Supply it with: fno backlog update {id} --pr-number null"
        ));
    }
    if truthy("locked_by") {
        return Some(format!(
            "locked_by holds (a node:{id} claim). Release it with: fno agents claim release node:{id} --holder <holder>"
        ));
    }
    let open_do = entry
        .get("sessions")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .any(|r| r.get("ended_at").map(|v| v.is_null()).unwrap_or(false))
        })
        .unwrap_or(false);
    if open_do {
        return Some("an open do session row holds; end the session".to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// The planner core
// ---------------------------------------------------------------------------

struct Planned {
    node_id: String,
    changes: Vec<Change>,
    old_status: String,
    rungs: BTreeMap<String, String>,
    working: Vec<Value>,
}

/// Match one token against `id` first, then `slug` (the read resolver's
/// contract), refusing ambiguity by name.
fn resolve_index(rows: &[Value], token: &str) -> Result<usize, PatchRefusal> {
    let id_hits: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, e)| field_eq(e, "id", token))
        .map(|(i, _)| i)
        .collect();
    if id_hits.len() == 1 {
        return Ok(id_hits[0]);
    }
    if id_hits.len() > 1 {
        return Err(PatchRefusal::refused(format!(
            "refused: {token} matches {n} nodes by id; name one",
            n = id_hits.len()
        )));
    }
    let slug_hits: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, e)| field_eq(e, "slug", token))
        .map(|(i, _)| i)
        .collect();
    match slug_hits.len() {
        1 => Ok(slug_hits[0]),
        0 => Err(PatchRefusal::refused(format!(
            "refused: no node resolves to '{token}'"
        ))),
        n => {
            let names: Vec<String> = slug_hits
                .iter()
                .filter_map(|&i| entry_id(&rows[i]).map(str::to_string))
                .collect();
            Err(PatchRefusal::refused(format!(
                "refused: {token} matches {n} nodes by slug ({}). Name one by id",
                names.join(", ")
            )))
        }
    }
}

/// Record one key write, skipping no-ops (old == new). An absent key reads as
/// null.
fn put_key(
    obj: &mut Map<String, Value>,
    changes: &mut Vec<Change>,
    id: &str,
    field: &str,
    new: Value,
) {
    let old = obj.get(field).cloned().unwrap_or(Value::Null);
    if old == new {
        return;
    }
    if new.is_null() {
        obj.insert(field.to_string(), Value::Null);
    } else {
        obj.insert(field.to_string(), new.clone());
    }
    changes.push(Change {
        id: id.to_string(),
        field: field.to_string(),
        old,
        new,
    });
}

/// Clear the supersession facts and the replacer's `supersedes` backref (the
/// stale claim would keep the chain reading as live). The deferral triple is
/// NOT touched: `--leave superseded` may legally land on deferred.
fn clear_supersession_facts(rows: &mut [Value], idx: usize, changes: &mut Vec<Change>) {
    let id = entry_id(&rows[idx]).unwrap_or_default().to_string();
    let replacer = rows[idx]
        .get("superseded_by")
        .and_then(Value::as_str)
        .map(str::to_string);
    let obj = rows[idx].as_object_mut().unwrap();
    put_key(obj, changes, &id, "superseded_by", Value::Null);
    put_key(obj, changes, &id, "supersession", Value::Null);
    if let Some(replacer) = replacer {
        if let Some(row) = rows.iter_mut().find(|r| field_eq(r, "id", &replacer)) {
            let rid = entry_id(row).unwrap_or_default().to_string();
            if let Some(obj) = row.as_object_mut() {
                let trimmed = match obj.get("supersedes") {
                    Some(Value::Array(list)) => Value::Array(
                        list.iter()
                            .filter(|s| s.as_str() != Some(id.as_str()))
                            .cloned()
                            .collect(),
                    ),
                    _ => Value::Array(vec![]),
                };
                put_key(obj, changes, &rid, "supersedes", trimmed);
            }
        }
    }
}

/// Clear every park fact: the deferral triple plus the supersession pair and
/// its backref. This is the `--status <word>` clear - a live status means no
/// park survives the write.
fn clear_park_facts(rows: &mut [Value], idx: usize, changes: &mut Vec<Change>) {
    let id = entry_id(&rows[idx]).unwrap_or_default().to_string();
    let obj = rows[idx].as_object_mut().unwrap();
    put_key(obj, changes, &id, "deferred_at", Value::Null);
    put_key(obj, changes, &id, "deferred_reason", Value::Null);
    put_key(obj, changes, &id, "deferred_kind", Value::Null);
    // The borrow of rows[idx] ends here; the supersession clear re-enters
    // rows for the replacer's backref.
    clear_supersession_facts(rows, idx, changes);
}

/// Apply the `--set` pairs through the field policy. Unknown and read-only
/// names refuse before anything is written.
fn apply_sets(
    entry: &mut Value,
    id: &str,
    sets: &[(String, String)],
    changes: &mut Vec<Change>,
) -> Result<(), PatchRefusal> {
    for (field, raw) in sets {
        let policy = policy_for(field).ok_or_else(|| {
            PatchRefusal::refused(format!(
                "refused: {id} unknown field '{field}'. Settable fields: {}",
                settable_names().join(", ")
            ))
        })?;
        let value = match policy {
            Policy::Settable(kind) | Policy::StatusFact(kind) => coerce(id, field, kind, raw)?,
            Policy::Derived(why) => {
                return Err(PatchRefusal::refused(format!(
                    "refused: {id} '{field}' is read-only: {why}"
                )))
            }
            Policy::Owned(owner) => {
                return Err(PatchRefusal::refused(format!(
                    "refused: {id} '{field}' is owned by another door: {owner}"
                )))
            }
        };
        let obj = entry.as_object_mut().unwrap();
        put_key(obj, changes, id, field, value);
    }
    Ok(())
}

/// The `--status deferred` facts, shared with the keeper's defer op: stamp
/// `deferred_at`, clear the lock and completion facts. The reason must
/// already be present (via `--set`), and it is checked HERE so every defer
/// leg refuses a blank reason.
fn stamp_deferred(
    entry: &mut Value,
    id: &str,
    changes: &mut Vec<Change>,
) -> Result<(), PatchRefusal> {
    let reason_blank = entry
        .get("deferred_reason")
        .map(|v| v.as_str().map(str::trim).unwrap_or("").is_empty() || v.is_null())
        .unwrap_or(true);
    if reason_blank {
        return Err(PatchRefusal::refused(format!(
            "refused: {id} deferred_reason cannot be blank. Supply it with: --set deferred_reason=<why>"
        )));
    }
    let obj = entry.as_object_mut().unwrap();
    put_key(obj, changes, id, "locked_by", Value::Null);
    put_key(obj, changes, id, "locked_at", Value::Null);
    // Cleared PER NODE, not hoisted: the ladder is done > deferred, so a
    // skipped clear makes deferring a done node a silent no-op.
    put_key(obj, changes, id, "completed_at", Value::Null);
    put_key(obj, changes, id, "deferred_at", json!(now_isoformat()));
    Ok(())
}

/// One planning pass over a full row set: resolve, patch a working copy,
/// recompute from the plan-rung map, and validate. No I/O, no commit.
fn plan(mut rows: Vec<Value>, req: &PatchRequest) -> Result<Planned, PatchRefusal> {
    if req.status.is_some() && req.leave.is_some() {
        return Err(PatchRefusal::refused(
            "refused: --status and --leave are different moves; run one per call",
        ));
    }
    // Normalize the pre-image first: the door's old-vs-new verdicts and
    // leave no-op checks key on the DERIVED status, and a row whose stored
    // status drifted (a fixture, or a legacy writer) would else mislead.
    let rungs: BTreeMap<String, String> = rows
        .iter()
        .filter_map(|e| {
            entry_id(e).map(|id| (id.to_string(), backlog_ready::plan_rung(e).to_string()))
        })
        .collect();
    recompute_statuses_with_plan_rungs(&mut rows, Some(&rungs));

    let idx = resolve_index(&rows, &req.node)?;
    let id = entry_id(&rows[idx]).unwrap_or_default().to_string();
    let old_status = rows[idx]
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let target = match &req.status {
        Some(word) => Some(normalize_status_word(word).ok_or_else(|| {
            PatchRefusal::refused(format!(
                "refused: {id} unknown status '{word}'. Status words: idea, design, ready, \
                 deferred, superseded, done, in_review, in_progress, blocked"
            ))
        })?),
        None => None,
    };
    let leave = match &req.leave {
        Some(word) => match word.as_str() {
            "deferred" | "superseded" => Some(word.as_str()),
            other => {
                return Err(PatchRefusal::refused(format!(
                    "refused: {id} --leave must be deferred or superseded, got '{other}'"
                )))
            }
        },
        None => None,
    };

    let mut changes: Vec<Change> = Vec::new();

    if let Some(word) = target {
        if let Some(owner) = target_owner(word, &id) {
            return Err(PatchRefusal::refused(format!(
                "refused: {id} --status {word} is owned by another door: {owner}"
            )));
        }
        if old_status == "done" {
            return Err(PatchRefusal::refused(format!(
                "refused: {id} is done; leaving done is owned by another door: fno backlog reopen {id}"
            )));
        }
        clear_park_facts(&mut rows, idx, &mut changes);
        apply_sets(&mut rows[idx], &id, &req.sets, &mut changes)?;
        if word == "deferred" {
            stamp_deferred(&mut rows[idx], &id, &mut changes)?;
        }
    } else if let Some(direction) = leave {
        // Not in the left status: a no-op, honestly receipted as unchanged.
        if old_status != direction {
            return Ok(Planned {
                node_id: id,
                changes: Vec::new(),
                old_status,
                rungs: BTreeMap::new(),
                working: rows,
            });
        }
        if direction == "deferred" {
            let obj = rows[idx].as_object_mut().unwrap();
            put_key(&mut *obj, &mut changes, &id, "deferred_at", Value::Null);
            put_key(&mut *obj, &mut changes, &id, "deferred_reason", Value::Null);
            put_key(&mut *obj, &mut changes, &id, "deferred_kind", Value::Null);
        } else {
            clear_supersession_facts(&mut rows, idx, &mut changes);
        }
        apply_sets(&mut rows[idx], &id, &req.sets, &mut changes)?;
    } else {
        apply_sets(&mut rows[idx], &id, &req.sets, &mut changes)?;
    }

    // The full rung map, the Rust twin of store.py `_plan_rung_map`: a node
    // absent from the map reads as rung none, so every row goes in. The
    // patch itself never touches plan_path (it is owned), so the pre-image
    // map stays valid.
    recompute_statuses_with_plan_rungs(&mut rows, Some(&rungs));

    let new_status = rows[idx]
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if let Some(word) = target {
        if new_status != word {
            return Err(status_mismatch(&rows[idx], &id, word, &new_status, &rungs));
        }
    }
    if let Some(direction) = leave {
        let landed_terminal = TERMINAL_RUNGS.contains(&new_status.as_str());
        if new_status == direction || landed_terminal {
            let why = holding_fact(&rows[idx], &id)
                .unwrap_or_else(|| format!("the derivation still reads {new_status}"));
            return Err(PatchRefusal::refused(format!(
                "refused: {id} --leave {direction} leaves the node {new_status}: {why}"
            )));
        }
    }

    Ok(Planned {
        node_id: id,
        changes,
        old_status,
        rungs,
        working: rows,
    })
}

/// Why the derived status is not the requested word: name the holding fact
/// and the flag or verb that supplies it (AC1-ERR, AC2-HP).
fn status_mismatch(
    entry: &Value,
    id: &str,
    target: &str,
    derived: &str,
    rungs: &BTreeMap<String, String>,
) -> PatchRefusal {
    let plan_path = entry
        .get("plan_path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    if target == "idea" {
        if plan_path.is_some() {
            return PatchRefusal::refused(format!(
                "refused: {id} plan_path holds, so the rung derives {derived}. Supply it with: --plan-path null"
            ));
        }
    } else if matches!(target, "design" | "ready") {
        let rung = rungs.get(id).map(String::as_str).unwrap_or("none");
        match (plan_path, rung) {
            (None, _) => {
                return PatchRefusal::refused(format!(
                    "refused: {id} has no plan_path. Supply it with: --plan-path <path to the plan>"
                ))
            }
            (_, rung) if rung != target => {
                return PatchRefusal::refused(format!(
                    "refused: {id} the plan reads rung {rung}, not {target}. Change the plan's own status line"
                ))
            }
            _ => {}
        }
    }
    let why = holding_fact(entry, id)
        .unwrap_or_else(|| format!("the derivation reads {derived}, not {target}"));
    PatchRefusal::refused(format!("refused: {id} {why}"))
}

// ---------------------------------------------------------------------------
// The commit cycle
// ---------------------------------------------------------------------------

/// The full door: plan, commit with a compare-and-swap version (retrying
/// contention up to 3 times), and read the receipt back from the committed
/// rows.
pub fn apply(graph: &Path, req: &PatchRequest) -> Result<PatchReceipt, PatchRefusal> {
    apply_with_timeout(graph, req, DEFAULT_LOCK_TIMEOUT)
}

/// `apply` with the lock deadline made explicit. Every contention kind
/// (version conflict, lock timeout) retries, then refuses; the timeout
/// parameter exists so the exhaustion path is testable in milliseconds.
pub fn apply_with_timeout(
    graph: &Path,
    req: &PatchRequest,
    lock_timeout: Duration,
) -> Result<PatchReceipt, PatchRefusal> {
    const ATTEMPTS: usize = 3;
    for attempt in 0..ATTEMPTS {
        let version =
            graph_store::base_version(graph).map_err(|e| PatchRefusal::error(e.to_string()))?;
        let rows = graph_store::read_rows(graph)
            .map_err(|e| PatchRefusal::error(format!("graph read failed: {e}")))?;
        let planned = plan(rows, req)?;
        if planned.changes.is_empty() {
            return Ok(PatchReceipt {
                node: planned.node_id,
                changes: Vec::new(),
                status: StatusPair {
                    from: planned.old_status.clone(),
                    to: planned.old_status,
                },
                version,
                unchanged: true,
            });
        }
        match graph_store::locked_mutate(
            graph,
            MutateInput {
                entries: planned.working,
                canonical_path: None,
                base_version: version,
                plan_rungs: Some(planned.rungs),
            },
            lock_timeout,
        ) {
            Ok(outcome) => {
                // The receipt comes from the READBACK, never the intent: the
                // store's own recompute had the last word on status.
                let readback = outcome
                    .entries
                    .iter()
                    .find(|e| field_eq(e, "id", &planned.node_id));
                let status_to = readback
                    .and_then(|e| e.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                return Ok(PatchReceipt {
                    node: planned.node_id,
                    changes: planned.changes,
                    status: StatusPair {
                        from: planned.old_status,
                        to: status_to,
                    },
                    version: outcome.version,
                    unchanged: false,
                });
            }
            // Contention: re-read, re-plan, re-commit. A mid-flight writer
            // must surface as a retry, never as a lost write.
            Err(StoreError::Conflict | StoreError::LockTimeout(..)) if attempt + 1 < ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(StoreError::Conflict) => {
                return Err(PatchRefusal::refused(format!(
                    "refused: {} graph changed under the write after {ATTEMPTS} attempts",
                    planned.node_id
                )));
            }
            Err(StoreError::LockTimeout(..)) => {
                return Err(PatchRefusal::refused(format!(
                    "refused: {} the graph lock stayed busy across {ATTEMPTS} attempts",
                    planned.node_id
                )));
            }
            Err(e) => return Err(PatchRefusal::error(format!("graph write failed: {e}"))),
        }
    }
    unreachable!("every loop arm returns")
}

/// The defer leg the keeper's op shares: validate the reason and kind, then
/// stamp the same keys `fno backlog defer` has always written. Runs inside an
/// existing locked cycle; no I/O.
pub fn defer_facts(
    entries: &mut [Value],
    node_id: &str,
    reason: &str,
    kind: Option<&str>,
) -> Result<(), StoreError> {
    let idx = entries
        .iter()
        .position(|e| entry_id(e) == Some(node_id))
        .ok_or_else(|| StoreError::Invalid(format!("no node resolves to '{node_id}'")))?;
    if reason.trim().is_empty() {
        return Err(StoreError::Invalid(format!(
            "refused: {node_id} deferred_reason cannot be blank. Supply it with: --reason <why>"
        )));
    }
    if let Some(kind) = kind {
        if !DEFERRED_KINDS.contains(&kind) {
            return Err(StoreError::Invalid(format!(
                "deferred_kind must be one of {}",
                DEFERRED_KINDS.join(", ")
            )));
        }
    }
    let obj = entries[idx].as_object_mut().unwrap();
    obj.insert("locked_by".to_string(), Value::Null);
    obj.insert("locked_at".to_string(), Value::Null);
    obj.insert("completed_at".to_string(), Value::Null);
    obj.insert("deferred_at".to_string(), Value::String(now_isoformat()));
    obj.insert(
        "deferred_reason".to_string(),
        Value::String(reason.to_string()),
    );
    match kind {
        Some(k) => {
            obj.insert("deferred_kind".to_string(), Value::String(k.to_string()));
        }
        None => {
            obj.shift_remove("deferred_kind");
        }
    }
    Ok(())
}

/// The pure status door for the typed API's `node_update` status arm: the
/// planner plans against rows the caller already holds (inside its mutation
/// closure) and writes the derived facts. A refusal means success:false with
/// the store untouched.
pub(crate) fn plan_status_on_rows(
    rows: &mut Vec<Value>,
    token: &str,
    word: &str,
) -> Result<(), PatchRefusal> {
    // Single-row scope: the typed API writes ONLY the named row, so leaving
    // superseded (which drops the replacer's backref, a second-row edit)
    // refuses here and names its own door. The backlog-update door clears
    // the chain in the same write; the typed API must not do that silently.
    let old_status = rows
        .iter()
        .find(|e| field_eq(e, "id", token))
        .and_then(|e| e.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if old_status == "superseded" {
        return Err(PatchRefusal::refused(format!(
            "refused: {token} is superseded; leaving is owned by another door: fno backlog unsupersede {token}"
        )));
    }
    let req = PatchRequest {
        node: token.to_string(),
        status: Some(word.to_string()),
        leave: None,
        sets: Vec::new(),
    };
    plan(std::mem::take(rows), &req).map(|planned| {
        *rows = planned.working;
    })
}

// ---------------------------------------------------------------------------
// The native action
// ---------------------------------------------------------------------------

fn parse_args(args: &[String]) -> Result<(PatchRequest, Option<PathBuf>, bool), (i32, String)> {
    let mut req = PatchRequest {
        node: String::new(),
        status: None,
        leave: None,
        sets: Vec::new(),
    };
    let mut graph: Option<PathBuf> = None;
    let mut json_out = false;
    // `--flag=value` is the same flag as `--flag value`: the Python forward
    // condition admits the =-spelling, so the door parses it too.
    let mut flat: Vec<String> = Vec::with_capacity(args.len());
    for a in args {
        match a.split_once('=') {
            Some((f, v))
                if matches!(f, "--graph" | "--node" | "--status" | "--leave" | "--set") =>
            {
                flat.push(f.to_string());
                flat.push(v.to_string());
            }
            _ => flat.push(a.clone()),
        }
    }
    let mut i = 0;
    while i < flat.len() {
        match flat[i].as_str() {
            "--graph" => {
                i += 1;
                let v = flat.get(i).ok_or((1, "--graph needs a path".to_string()))?;
                graph = Some(PathBuf::from(v));
            }
            "--node" => {
                i += 1;
                req.node = flat
                    .get(i)
                    .ok_or((1, "--node needs an id".to_string()))?
                    .clone();
            }
            "--status" => {
                i += 1;
                req.status = Some(
                    flat.get(i)
                        .ok_or((1, "--status needs a word".to_string()))?
                        .clone(),
                );
            }
            "--leave" => {
                i += 1;
                req.leave = Some(
                    flat.get(i)
                        .ok_or((1, "--leave needs deferred or superseded".to_string()))?
                        .clone(),
                );
            }
            "--set" => {
                i += 1;
                let v = flat
                    .get(i)
                    .ok_or((1, "--set needs field=value".to_string()))?;
                let (field, value) = v
                    .split_once('=')
                    .ok_or((1, format!("--set needs field=value, got '{v}'")))?;
                req.sets.push((field.to_string(), value.to_string()));
            }
            "--json" | "-J" => json_out = true,
            other if other.starts_with('-') => {
                return Err((1, format!("unknown flag {other}")));
            }
            other => {
                if req.node.is_empty() {
                    req.node = other.to_string();
                } else {
                    return Err((1, format!("unexpected argument {other}")));
                }
            }
        }
        i += 1;
    }
    if req.node.is_empty() {
        return Err((1, "needs a node (--node <id-or-slug>)".to_string()));
    }
    Ok((req, graph, json_out))
}

fn value_word(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Text receipt: one line per changed key (`<id> <field>: <old> -> <new>`),
/// then the status line. Nothing changed: `<id> unchanged`.
pub fn render_text(receipt: &PatchReceipt) -> String {
    if receipt.unchanged {
        return format!("{} unchanged", receipt.node);
    }
    let mut lines: Vec<String> = receipt
        .changes
        .iter()
        .map(|c| {
            format!(
                "{} {}: {} -> {}",
                c.id,
                c.field,
                value_word(&c.old),
                value_word(&c.new)
            )
        })
        .collect();
    if receipt.status.from != receipt.status.to {
        lines.push(format!(
            "{} status: {} -> {}",
            receipt.node, receipt.status.from, receipt.status.to
        ));
    }
    lines.join("\n")
}

/// `fno-agents backlog-update`: exit 0 applied or unchanged, 2 refused, 1
/// usage or store error.
pub fn run_update(args: &[String]) -> i32 {
    let (req, graph, json_out) = match parse_args(args) {
        Ok(parsed) => parsed,
        Err((code, message)) => {
            eprintln!("fno-agents backlog-update: {message}");
            return code;
        }
    };
    let graph = graph.unwrap_or_else(default_graph_path);
    match apply(&graph, &req) {
        Ok(receipt) => {
            if json_out {
                crate::backlog::receipt::emit_line(
                    &serde_json::to_string(&receipt).unwrap_or_else(|_| "{}".into()),
                );
            } else {
                crate::backlog::receipt::emit_line(&render_text(&receipt));
            }
            0
        }
        Err(refusal) => {
            eprintln!("{refusal}", refusal = refusal.message);
            refusal.exit
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_store::CANONICAL_FIELD_ORDER;
    use std::io::Write;

    #[test]
    fn parse_args_reads_the_equals_spelling() {
        let s = |x: &str| x.to_string();
        let (req, graph, json_out) = parse_args(&[
            s("--node=x-1"),
            s("--graph=/tmp/g.json"),
            s("--status=idea"),
            s("--set=title=New"),
            s("--leave=deferred"),
        ])
        .expect("=-spelling parses");
        assert_eq!(req.node, "x-1");
        assert_eq!(graph.as_deref(), Some(std::path::Path::new("/tmp/g.json")));
        assert_eq!(req.status.as_deref(), Some("idea"));
        assert_eq!(req.leave.as_deref(), Some("deferred"));
        assert_eq!(req.sets, vec![("title".to_string(), "New".to_string())]);
        assert!(!json_out);

        // A value containing '=' survives: only the FIRST '=' splits.
        let (req2, _, _) =
            parse_args(&[s("--node=x-1"), s("--set=details=a=b")]).expect("nested = parses");
        assert_eq!(req2.sets, vec![("details".to_string(), "a=b".to_string())]);

        // An unknown =-flag still refuses by name.
        let err = parse_args(&[s("--node=x-1"), s("--nope=1")]).expect_err("unknown refuses");
        assert_eq!(err.0, 1);
        assert!(err.1.contains("--nope"));
    }

    fn write_graph(entries: &[Value]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.json");
        crate::graph_store::seed_rows(&path, entries).expect("seed graph.db");
        (dir, path)
    }

    /// A plan file whose frontmatter carries `status` (or none).
    fn plan_file(dir: &tempfile::TempDir, name: &str, status: Option<&str>) -> String {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).expect("create plan");
        let fm = match status {
            Some(s) => format!("status: {s}\n"),
            None => String::new(),
        };
        write!(f, "---\n{fm}---\n\n# plan\n").expect("write plan");
        path.to_string_lossy().to_string()
    }

    fn node(id: &str, extra: Value) -> Value {
        let mut base = serde_json::json!({
            "id": id,
            "slug": format!("slug-{id}"),
            "title": id,
            "status": "idea",
            "priority": "p2",
            "type": "feature",
        });
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }
        base
    }

    fn req(node: &str, status: Option<&str>, sets: &[(&str, &str)]) -> PatchRequest {
        PatchRequest {
            node: node.to_string(),
            status: status.map(str::to_string),
            leave: None,
            sets: sets
                .iter()
                .map(|(f, v)| (f.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn refusal_of(graph: &Path, req: &PatchRequest) -> String {
        match apply(graph, req) {
            Err(e) => {
                assert_eq!(
                    e.exit, 2,
                    "expected a refusal, got exit {}: {}",
                    e.exit, e.message
                );
                e.message
            }
            Ok(receipt) => panic!("expected a refusal, got {receipt:?}"),
        }
    }

    fn status_of(graph: &Path, id: &str) -> String {
        // graph.db is the only store: read the store, not the frozen mirror.
        let rows = crate::graph_store::read_rows(graph).expect("read back");
        rows.iter()
            .find(|e| field_eq(e, "id", id))
            .and_then(|e| e.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    // AC1-HP
    #[test]
    fn status_idea_on_a_superseded_plan_less_node_clears_the_chain_in_one_write() {
        // Named: the TempDir must outlive the graph reads inside the test.
        let _dir = tempfile::tempdir().unwrap();
        let replacer = node("x-aaaa", serde_json::json!({ "supersedes": ["x-bbbb"] }));
        let victim = node(
            "x-bbbb",
            serde_json::json!({
                "superseded_by": "x-aaaa",
                "supersession": {"cause": "moved", "session_id": "s1"},
                "deferred_at": "2026-01-01T00:00:00+00:00",
            }),
        );
        let (_d, graph) = write_graph(&[replacer, victim]);
        let receipt = apply(&graph, &req("x-bbbb", Some("idea"), &[])).expect("applied");
        assert!(!receipt.unchanged);
        assert_eq!(receipt.status.from, "superseded");
        assert_eq!(receipt.status.to, "idea");
        let fields: Vec<&str> = receipt.changes.iter().map(|c| c.field.as_str()).collect();
        assert!(fields.contains(&"superseded_by"));
        assert!(fields.contains(&"supersession"));
        assert!(fields.contains(&"deferred_at"));
        // The replacer's backref dropped, named as its own change.
        let backref = receipt
            .changes
            .iter()
            .find(|c| c.field == "supersedes")
            .expect("backref change");
        assert_eq!(backref.id, "x-aaaa");
        assert_eq!(status_of(&graph, "x-bbbb"), "idea");
        let rows = crate::graph_store::read_rows(&graph).unwrap();
        let repl = rows.iter().find(|e| field_eq(e, "id", "x-aaaa")).unwrap();
        assert_eq!(repl.get("supersedes"), Some(&json!([])));
    }

    // AC1-ERR
    #[test]
    fn status_idea_on_a_node_whose_plan_reads_ready_refuses_naming_plan_path() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_file(&dir, "ready-plan.md", Some("ready"));
        let victim = node(
            "x-bbbb",
            serde_json::json!({ "superseded_by": "x-aaaa", "plan_path": plan }),
        );
        let replacer = node("x-aaaa", serde_json::json!({ "supersedes": ["x-bbbb"] }));
        let (_d, graph) = write_graph(&[replacer, victim]);
        let before = graph_store::base_version(&graph).unwrap();
        let message = refusal_of(&graph, &req("x-bbbb", Some("idea"), &[]));
        assert!(message.contains("plan_path"), "{message}");
        assert!(message.contains("--plan-path null"), "{message}");
        assert_eq!(graph_store::base_version(&graph).unwrap(), before);
    }

    // AC2-HP
    #[test]
    fn status_ready_on_a_plan_less_node_refuses_naming_plan_path() {
        let (_d, graph) = write_graph(&[node("x-1", json!({}))]);
        let message = refusal_of(&graph, &req("x-1", Some("ready"), &[]));
        assert!(message.contains("plan_path"), "{message}");
        assert!(message.contains("--plan-path"), "{message}");
    }

    // AC2-EDGE
    #[test]
    fn status_ready_on_a_deferred_node_clears_the_park_and_reads_ready() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_file(&dir, "p.md", Some("ready"));
        let victim = node(
            "x-2",
            serde_json::json!({
                "deferred_at": "2026-01-01T00:00:00+00:00",
                "deferred_reason": "waiting",
                "deferred_kind": "later",
                "plan_path": plan,
            }),
        );
        let (_d, graph) = write_graph(&[victim]);
        let receipt = apply(&graph, &req("x-2", Some("ready"), &[])).expect("applied");
        assert_eq!(receipt.status.to, "ready");
        assert_eq!(status_of(&graph, "x-2"), "ready");
        let rows = crate::graph_store::read_rows(&graph).unwrap();
        let row = rows.iter().find(|e| field_eq(e, "id", "x-2")).unwrap();
        // Canonical store form: a cleared field is absent or null, never stale.
        assert!(row.get("deferred_at").map_or(true, Value::is_null));
    }

    // AC3-HP
    #[test]
    fn an_unknown_field_refuses_naming_the_settable_set() {
        let (_d, graph) = write_graph(&[node("x-1", json!({}))]);
        let message = refusal_of(&graph, &req("x-1", None, &[("nonsense", "1")]));
        assert!(message.contains("nonsense"), "{message}");
        assert!(message.contains("title"), "{message}");
    }

    // AC3-ERR
    #[test]
    fn derived_and_owned_fields_refuse_naming_their_owner() {
        let (_d, graph) = write_graph(&[node("x-1", json!({}))]);
        let status = refusal_of(&graph, &req("x-1", None, &[("status", "done")]));
        assert!(status.contains("--status"), "{status}");
        let children = refusal_of(&graph, &req("x-1", None, &[("children", "[]")]));
        assert!(children.contains("--parent"), "{children}");
        let completed = refusal_of(&graph, &req("x-1", None, &[("completed_at", "2026-09-13")]));
        assert!(completed.contains("fno backlog done"), "{completed}");
    }

    #[test]
    fn merge_status_can_be_cleared_without_changing_pr_identity_or_status() {
        let victim = node(
            "x-1",
            json!({
                "pr_number": 1060,
                "pr_url": "https://github.com/o/r/pull/1060",
                "merge_status": "merged",
            }),
        );
        let (_d, graph) = write_graph(&[victim]);

        let receipt =
            apply(&graph, &req("x-1", None, &[("merge_status", "null")])).expect("clear applied");
        assert_eq!(receipt.status.from, "in_review");
        assert_eq!(receipt.status.to, "in_review");
        let rows = crate::graph_store::read_rows(&graph).unwrap();
        let row = rows.iter().find(|e| field_eq(e, "id", "x-1")).unwrap();
        // Canonical store form: a cleared field is absent or null, never stale.
        assert!(row.get("merge_status").map_or(true, Value::is_null));
        assert_eq!(row.get("pr_number"), Some(&json!(1060)));
        assert_eq!(
            row.get("pr_url"),
            Some(&json!("https://github.com/o/r/pull/1060"))
        );
    }

    #[test]
    fn merge_status_refuses_any_value_other_than_null() {
        let (_d, graph) = write_graph(&[node("x-1", json!({}))]);
        let message = refusal_of(&graph, &req("x-1", None, &[("merge_status", "merged")]));
        assert!(
            message.contains("fno do pr merge and reconcile"),
            "{message}"
        );
        assert!(message.contains("--set merge_status=null"), "{message}");
    }

    // AC3-EDGE
    #[test]
    fn two_sets_land_in_one_write_and_a_bad_priority_refuses() {
        let (_d, graph) = write_graph(&[node("x-1", json!({}))]);
        let receipt = apply(
            &graph,
            &req("x-1", None, &[("title", "New"), ("priority", "p1")]),
        )
        .expect("applied");
        assert_eq!(receipt.changes.len(), 2);
        assert_eq!(receipt.changes[0].field, "title");
        assert_eq!(receipt.changes[1].field, "priority");
        let message = refusal_of(&graph, &req("x-1", None, &[("priority", "p9")]));
        assert!(message.contains("p0"), "{message}");
        assert!(message.contains("p3"), "{message}");
    }

    // AC4-HP: every stored field maps to exactly one policy.
    #[test]
    fn the_field_policy_covers_every_canonical_and_model_key_exactly_once() {
        let mut seen = std::collections::BTreeSet::new();
        for (name, _) in FIELD_POLICY {
            assert!(seen.insert(*name), "duplicate policy for {name}");
        }
        for name in CANONICAL_FIELD_ORDER {
            assert!(seen.contains(name), "no policy for canonical field {name}");
        }
        for name in [
            "locked_by",
            "locked_by_harness",
            "locked_by_harness_session",
            "locked_at",
        ] {
            assert!(seen.contains(name), "no policy for claim key {name}");
        }
        for name in MODEL_EXTRA_KEYS {
            assert!(seen.contains(name), "no policy for model key {name}");
        }
        for name in [
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
            "request_origin",
            "origin_evidence",
        ] {
            assert!(seen.contains(name), "no policy for provenance key {name}");
        }
        // And nothing beyond the union plus the two design-table names that
        // are not stored keys (blocked_reason is derived on read;
        // difficulty lives outside CANONICAL_FIELD_ORDER).
        let mut union = std::collections::BTreeSet::new();
        union.extend(CANONICAL_FIELD_ORDER.iter().copied());
        union.extend(MODEL_EXTRA_KEYS.iter().copied());
        union.insert("blocked_reason");
        union.insert("difficulty");
        for name in seen {
            assert!(union.contains(name), "policy for unknown field {name}");
        }
    }

    #[test]
    fn claim_fields_are_derived_from_the_node_claim_lockfile() {
        let reason =
            "projection of the node:<id> claim; use fno agents claim acquire|release node:<id>";
        for field in [
            "locked_by",
            "locked_by_harness",
            "locked_by_harness_session",
            "locked_at",
        ] {
            assert!(matches!(policy_for(field), Some(Policy::Derived(actual)) if actual == reason));
        }
        assert_eq!(
            holding_fact(&json!({ "locked_by": "s1" }), "x-t1").as_deref(),
            Some("locked_by holds (a node:x-t1 claim). Release it with: fno agents claim release node:x-t1 --holder <holder>")
        );
    }

    // AC7-HP (door half)
    #[test]
    fn status_deferred_clears_the_lock_and_needs_a_reason() {
        let victim = node(
            "x-3",
            serde_json::json!({ "status": "ready", "locked_by": "sess-1", "locked_at": "2026-01-01T00:00:00+00:00" }),
        );
        let (_d, graph) = write_graph(&[victim]);
        let message = refusal_of(&graph, &req("x-3", Some("deferred"), &[]));
        assert!(message.contains("deferred_reason"), "{message}");
        let receipt = apply(
            &graph,
            &req(
                "x-3",
                Some("deferred"),
                &[("deferred_reason", "waiting on x")],
            ),
        )
        .expect("applied");
        assert_eq!(receipt.status.to, "deferred");
        let rows = crate::graph_store::read_rows(&graph).unwrap();
        let row = rows.iter().find(|e| field_eq(e, "id", "x-3")).unwrap();
        // Canonical store form: a cleared field is absent or null, never stale.
        assert!(row.get("locked_by").map_or(true, Value::is_null));
        assert!(row.get("locked_at").map_or(true, Value::is_null));
        assert!(row.get("deferred_at").and_then(Value::as_str).is_some());
    }

    // AC7-ERR
    #[test]
    fn a_bogus_deferred_kind_refuses_listing_the_vocabulary() {
        let (_d, graph) = write_graph(&[node("x-1", json!({}))]);
        let message = refusal_of(
            &graph,
            &req(
                "x-1",
                Some("deferred"),
                &[("deferred_reason", "why"), ("deferred_kind", "bogus")],
            ),
        );
        assert!(message.contains("expired"), "{message}");
        assert!(message.contains("junk"), "{message}");
    }

    // AC8-HP
    #[test]
    fn the_owned_transitions_refuse_naming_their_owners() {
        let (_d, graph) = write_graph(&[node("x-1", json!({}))]);
        for (word, owner) in [
            ("superseded", "fno backlog supersede"),
            ("done", "fno backlog done"),
            ("in_review", "--pr-number"),
            ("in_progress", "fno do target start"),
            ("blocked", "--add-blocker"),
        ] {
            let message = refusal_of(&graph, &req("x-1", Some(word), &[]));
            assert!(message.contains(owner), "{word}: {message}");
        }
        let done_node = node(
            "x-9",
            json!({ "completed_at": "2026-01-01T00:00:00+00:00" }),
        );
        let (_d, graph) = write_graph(&[done_node]);
        let message = refusal_of(&graph, &req("x-9", Some("idea"), &[]));
        assert!(message.contains("fno backlog reopen"), "{message}");
    }

    // AC11 (door half)
    #[test]
    fn a_slug_resolves_and_an_ambiguous_token_refuses_naming_candidates() {
        let a = node("x-1", json!({ "slug": "same-slug" }));
        let b = node("x-2", json!({ "slug": "same-slug" }));
        let message = resolve_index(&[a, b], "same-slug").unwrap_err().message;
        assert!(
            message.contains("x-1") && message.contains("x-2"),
            "{message}"
        );
        let single = node("x-3", json!({ "slug": "only-slug" }));
        let (_d, graph) = write_graph(&[single]);
        let receipt = apply(&graph, &req("only-slug", Some("ready"), &[]));
        // Plan-less: the door refuses ready, which proves the slug resolved.
        assert!(receipt.is_err());
    }

    // AC5 leave semantics at the door: a plain deferred node leaves cleanly,
    // and a leave that lands terminal refuses naming the holding fact.
    #[test]
    fn leave_deferred_clears_the_park_and_leave_superseded_may_land_deferred() {
        let deferred = node(
            "x-4",
            serde_json::json!({
                "deferred_at": "2026-01-01T00:00:00+00:00",
                "deferred_reason": "waiting",
            }),
        );
        let (_d, graph) = write_graph(&[deferred]);
        let receipt = apply(&graph, &leave_req("x-4", "deferred")).expect("applied");
        assert_eq!(receipt.status.to, "idea");
        // Already left: unchanged, exit 0.
        let receipt = apply(&graph, &leave_req("x-4", "deferred")).expect("applied");
        assert!(receipt.unchanged);

        let both = node(
            "x-5",
            serde_json::json!({
                "superseded_by": "x-r",
                "supersession": {"cause": "c"},
                "deferred_at": "2026-01-01T00:00:00+00:00",
                "deferred_reason": "waiting",
            }),
        );
        let replacer = node("x-r", json!({ "supersedes": ["x-5"] }));
        let (_d, graph) = write_graph(&[both, replacer]);
        let receipt = apply(&graph, &leave_req("x-5", "superseded")).expect("applied");
        // The unsupersede contract: the pre-supersession park survives.
        assert_eq!(receipt.status.to, "deferred");
    }

    fn leave_req(node: &str, direction: &str) -> PatchRequest {
        PatchRequest {
            node: node.to_string(),
            status: None,
            leave: Some(direction.to_string()),
            sets: Vec::new(),
        }
    }

    // AC12-EDGE: contention across every attempt exhausts the retries and
    // refuses without a partial write. Deterministic via the lock: a holder
    // keeps `<graph>.lock` past every retry deadline, so all three attempts
    // time out and the exhaustion arm fires.
    #[test]
    fn a_graph_that_keeps_changing_refuses_after_three_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        let plan = plan_file(&dir, "ready-plan.md", Some("ready"));
        let target = node(
            "x-1",
            json!({
                "plan_path": plan,
                "deferred_at": "2026-01-01T00:00:00+00:00",
                "deferred_reason": "waiting",
                "status": "deferred",
            }),
        );
        graph_store::seed_rows(&graph, &[target]).expect("seed graph.db");
        // Hold the store's own lock well past three 100ms deadlines.
        let holder = {
            let graph = graph.clone();
            std::thread::spawn(move || {
                let lock = graph_store::BoundedLock::acquire(&graph, Duration::from_secs(10))
                    .expect("test lock");
                std::thread::sleep(Duration::from_millis(1500));
                drop(lock);
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        let result = apply_with_timeout(
            &graph,
            &req("x-1", Some("ready"), &[]),
            Duration::from_millis(100),
        );
        holder.join().expect("holder finished");
        match result {
            Err(e) => {
                assert_eq!(e.exit, 2);
                assert!(e.message.contains("stayed busy"), "{e:?}");
            }
            Ok(receipt) => panic!("expected contention refusal, got {receipt:?}"),
        }
    }

    // AC10: the shared defer leg refuses a blank reason and stamps the same
    // keys the keeper op always wrote.
    #[test]
    fn defer_facts_refuses_a_blank_reason_and_stamps_the_six_keys() {
        let mut rows = vec![node("x-1", json!({ "locked_by": "s1" }))];
        assert!(defer_facts(&mut rows, "x-1", "   ", None).is_err());
        defer_facts(&mut rows, "x-1", "waiting", Some("later")).expect("stamped");
        let obj = rows[0].as_object().unwrap();
        assert_eq!(obj.get("locked_by"), Some(&Value::Null));
        assert_eq!(obj.get("completed_at"), Some(&Value::Null));
        assert!(obj.get("deferred_at").and_then(Value::as_str).is_some());
        assert_eq!(obj.get("deferred_reason"), Some(&json!("waiting")));
        assert_eq!(obj.get("deferred_kind"), Some(&json!("later")));
        // No kind: the key is REMOVED, so a re-deferral clears a stale stamp.
        defer_facts(&mut rows, "x-1", "again", None).expect("stamped");
        assert!(!rows[0].as_object().unwrap().contains_key("deferred_kind"));
    }

    // AC10: the keeper op reaches the door through apply_op's defer arm; the
    // classify table it shares lives here too.
    #[test]
    fn the_exact_match_classifier_knows_only_the_machine_stamps() {
        assert_eq!(
            classify_deferred_reason("stale >30d, drained by maintain"),
            Some("expired")
        );
        assert_eq!(
            classify_deferred_reason("stale-quarantine (guard)"),
            Some("expired")
        );
        assert_eq!(classify_deferred_reason("stale >14d"), None);
    }

    /// The ten kinds must match the Python tuple byte for byte: the CLI
    /// validates against the Python side, the door against this one. A drift
    /// would let one side accept a kind the other refuses.
    #[test]
    fn deferred_kinds_match_the_python_vocabulary() {
        const PY_CONSTANTS: &str = include_str!("../../../../cli/src/fno/graph/_constants.py");
        let header = "DEFERRED_KINDS: tuple[str, ...] = (";
        let start = PY_CONSTANTS
            .find(header)
            .expect("DEFERRED_KINDS tuple in _constants.py");
        let open = start + header.len() - 1;
        let end = PY_CONSTANTS[open..]
            .find("\n)")
            .expect("tuple close on its own line")
            + open;
        let block = &PY_CONSTANTS[open..end];
        for kind in DEFERRED_KINDS {
            assert!(
                block.contains(&format!("\"{kind}\",")),
                "kind {kind} missing from the Python DEFERRED_KINDS"
            );
        }
        // And no extra Python kind is missing here: count quoted words.
        let py_kinds: Vec<&str> = block
            .lines()
            .filter_map(|line| line.trim().strip_prefix('"'))
            .filter_map(|line| line.split('"').next())
            .collect();
        assert_eq!(py_kinds.len(), DEFERRED_KINDS.len(), "{py_kinds:?}");
    }

    #[test]
    fn unknown_status_words_and_leave_directions_refuse_by_name() {
        let (_d, graph) = write_graph(&[node("x-1", json!({}))]);
        let message = refusal_of(&graph, &req("x-1", Some("shipped"), &[]));
        assert!(message.contains("shipped"), "{message}");
        let mut r = req("x-1", None, &[]);
        r.leave = Some("parked".to_string());
        let message = refusal_of(&graph, &r);
        assert!(message.contains("deferred or superseded"), "{message}");
    }

    #[test]
    fn render_text_names_each_field_then_the_status_line() {
        let receipt = PatchReceipt {
            node: "x-1".into(),
            changes: vec![Change {
                id: "x-1".into(),
                field: "priority".into(),
                old: json!("p2"),
                new: json!("p1"),
            }],
            status: StatusPair {
                from: "idea".into(),
                to: "idea".into(),
            },
            version: "v".into(),
            unchanged: false,
        };
        let text = render_text(&receipt);
        assert_eq!(text, "x-1 priority: p2 -> p1");
        let unchanged = PatchReceipt {
            unchanged: true,
            ..receipt
        };
        assert_eq!(render_text(&unchanged), "x-1 unchanged");
    }
}
