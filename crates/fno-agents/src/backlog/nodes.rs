//! The node aggregate: the nodes row plus its single-row mirrors
//! (node_claims, node_dispatch, node_provenance, supersessions), owned here
//! (ruling 4: each aggregate's module owns its tables - no SQL against these
//! outside this file).
//!
//! Two deliberate deviations from the v2 schema, both for lossless
//! round-trips of legacy rows: node_claims.locked_by and
//! node_provenance.source_kind are NOT NULL there but nullable here. Child
//! aggregates (sessions, comments, encounters, pull requests) live in their
//! own tables, whose rows cannot distinguish an absent JSON key from an
//! empty array, so save() records the present child lists in the extras
//! column under "child_lists_present" and load() trusts that marker; a
//! primary pull request is marked "primary_pr" in the same list. The
//! supersession's unknown keys ride the extras column under
//! "supersession_extras"; origin_evidence/request_origin have no provenance
//! columns and ride the residual JSON.

use super::model::{
    Dispatch, Lifecycle, Node, NodeClaim, OwnershipDefect, Priority, Provenance, PullRequest,
    Relations, Status, Supersession,
};
use super::{comments, encounters, pull_requests, relations, sessions};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS nodes (
  id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, slug TEXT NOT NULL UNIQUE, title TEXT NOT NULL,
  kind TEXT, status TEXT NOT NULL,
  priority TEXT NOT NULL CHECK (priority IN ('p0','p1','p2','p3')), rank REAL,
  project TEXT, cwd TEXT, domain TEXT, estimate TEXT, difficulty TEXT,
  description TEXT, plan_path TEXT, parent_id TEXT, contained_in TEXT, superseded_by TEXT,
  caused_by TEXT, fixes_pr INTEGER, ownership_defect TEXT, created_at TEXT,
  touched_at TEXT, completed_at TEXT, completion_note TEXT,
  deferred_at TEXT, deferred_reason TEXT, deferred_kind TEXT,
  queued_at TEXT, queued_reason TEXT, reopened_at TEXT, reopened_reason TEXT,
  archived_at TEXT, session_id TEXT, has_brief INTEGER,
  blocks_everything INTEGER, cost_usd REAL, vision_path TEXT,
  artifact_url TEXT, extras TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS nodes_parent ON nodes(parent_id);
CREATE INDEX IF NOT EXISTS nodes_status ON nodes(status, project);
CREATE INDEX IF NOT EXISTS nodes_archive ON nodes(archived_at);
CREATE TABLE IF NOT EXISTS node_claims (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  locked_by TEXT, harness TEXT, harness_session TEXT, locked_at TEXT
);
CREATE TABLE IF NOT EXISTS node_dispatch (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  verb TEXT, brief TEXT, model TEXT
);
CREATE TABLE IF NOT EXISTS node_provenance (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  source TEXT, source_kind TEXT, source_project TEXT, source_session_id TEXT,
  source_harness TEXT, source_cwd TEXT, source_node_id TEXT, source_plan_path TEXT,
  source_inbox_msg TEXT, spawned_by_session TEXT, spawned_by_harness TEXT,
  spawned_by_cwd TEXT, think_session_id TEXT, think_output_path TEXT
);
CREATE TABLE IF NOT EXISTS supersessions (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  successor_id TEXT, cause TEXT, reason TEXT, verified_at TEXT, evidence_pr INTEGER,
  surfaces TEXT, matched_surfaces TEXT
);";

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(DDL)
        .map_err(|error| error.to_string())
}

/// Upsert one node's row and its mirrors. The caller owns the transaction;
/// child aggregates (sessions, comments, encounters, pull requests,
/// relations) are the caller's job through their own modules.
pub fn save(connection: &Connection, node: &Node) -> Result<(), String> {
    // The extras column holds node.extras, then the residual aggregates with
    // no v2 table (extras wins on the in-practice-disjoint collision), the
    // supersession's unknown keys, and the child-list presence marker.
    let mut extras_map = node.extras.clone();
    for (key, value) in node.residual_json() {
        extras_map.entry(key).or_insert(value);
    }
    if let Some(supersession) = &node.supersession {
        if !supersession.extras.is_empty() {
            extras_map.insert(
                "supersession_extras".to_string(),
                Value::Object(supersession.extras.clone()),
            );
        }
    }
    let mut child_lists_present: Vec<Value> = Vec::new();
    for (present, name) in [
        (node.primary_pr.is_some(), "primary_pr"),
        (node.additional_prs.is_some(), "additional_prs"),
        (node.sessions.is_some(), "sessions"),
        (node.comments.is_some(), "comments"),
        (node.encounters.is_some(), "encounters"),
        (node.relations.blocked_by.is_some(), "blocked_by"),
        (node.relations.related.is_some(), "related"),
        (node.relations.supersedes.is_some(), "supersedes"),
    ] {
        if present {
            child_lists_present.push(Value::String(name.to_string()));
        }
    }
    if !child_lists_present.is_empty() {
        extras_map.insert(
            "child_lists_present".to_string(),
            Value::Array(child_lists_present),
        );
    }
    let extras_json =
        serde_json::to_string(&Value::Object(extras_map)).map_err(|error| error.to_string())?;
    let ownership_defect_json = match &node.ownership_defect {
        None => None,
        Some(defect) => {
            let mut object = Map::new();
            object.insert("kind".to_string(), Value::String(defect.kind.clone()));
            for (key, value) in [
                ("node_id", &defect.node_id),
                ("holder", &defect.holder),
                ("liveness", &defect.liveness),
            ] {
                if let Some(text) = value {
                    object.insert(key.to_string(), Value::String(text.clone()));
                }
            }
            for (key, value) in &defect.extras {
                object.insert(key.clone(), value.clone());
            }
            Some(serde_json::to_string(&Value::Object(object)).map_err(|error| error.to_string())?)
        }
    };
    connection
        .execute(
            "INSERT OR REPLACE INTO nodes (id, ordinal, slug, title, kind, status, priority,
                 rank, project, cwd, domain, estimate, difficulty, description, plan_path,
                 parent_id, contained_in, superseded_by, caused_by, fixes_pr, ownership_defect,
                 created_at, touched_at, completed_at, completion_note, deferred_at,
                 deferred_reason, deferred_kind, queued_at, queued_reason, reopened_at,
                 reopened_reason, archived_at, session_id, has_brief, blocks_everything,
                 cost_usd, vision_path, artifact_url, extras)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31,
                 ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40)",
            params![
                node.id,
                node.ordinal,
                node.slug,
                node.title,
                node.kind,
                node.status.as_str(),
                node.priority.as_str(),
                node.rank,
                node.project,
                node.cwd,
                node.domain,
                node.estimate,
                node.difficulty,
                node.description,
                node.plan_path,
                node.parent,
                node.contained_in,
                node.superseded_by,
                node.caused_by,
                node.fixes_pr,
                ownership_defect_json,
                node.created_at,
                node.touched_at,
                node.completed_at,
                node.completion_note,
                node.lifecycle.deferred_at,
                node.lifecycle.deferred_reason,
                node.lifecycle.deferred_kind,
                node.lifecycle.queued_at,
                node.lifecycle.queued_reason,
                node.lifecycle.reopened_at,
                node.lifecycle.reopened_reason,
                node.archived_at,
                node.session_id,
                node.has_brief.map(|value| value as i64),
                node.blocks_everything.map(|value| value as i64),
                node.cost_usd,
                node.vision_path,
                node.artifact_url,
                extras_json,
            ],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "DELETE FROM node_claims WHERE node_id = ?1",
            params![node.id],
        )
        .map_err(|error| error.to_string())?;
    let claim = &node.claim;
    if claim.locked_by.is_some()
        || claim.harness.is_some()
        || claim.harness_session.is_some()
        || claim.locked_at.is_some()
    {
        connection
            .execute(
                "INSERT INTO node_claims (node_id, locked_by, harness, harness_session, locked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    node.id,
                    claim.locked_by,
                    claim.harness,
                    claim.harness_session,
                    claim.locked_at,
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    connection
        .execute(
            "DELETE FROM node_dispatch WHERE node_id = ?1",
            params![node.id],
        )
        .map_err(|error| error.to_string())?;
    let dispatch = &node.dispatch;
    if dispatch.verb.is_some() || dispatch.brief.is_some() || dispatch.model.is_some() {
        connection
            .execute(
                "INSERT INTO node_dispatch (node_id, verb, brief, model)
                 VALUES (?1, ?2, ?3, ?4)",
                params![node.id, dispatch.verb, dispatch.brief, dispatch.model],
            )
            .map_err(|error| error.to_string())?;
    }
    connection
        .execute(
            "DELETE FROM node_provenance WHERE node_id = ?1",
            params![node.id],
        )
        .map_err(|error| error.to_string())?;
    let provenance = &node.provenance;
    let provenance_any = [
        &provenance.source,
        &provenance.source_kind,
        &provenance.source_project,
        &provenance.source_session_id,
        &provenance.source_harness,
        &provenance.source_cwd,
        &provenance.source_node_id,
        &provenance.source_plan_path,
        &provenance.source_inbox_msg,
        &provenance.spawned_by_session,
        &provenance.spawned_by_harness,
        &provenance.spawned_by_cwd,
        &provenance.think_session_id,
        &provenance.think_output_path,
    ]
    .iter()
    .any(|value| value.is_some());
    if provenance_any {
        connection
            .execute(
                "INSERT INTO node_provenance (node_id, source, source_kind, source_project,
                     source_session_id, source_harness, source_cwd, source_node_id,
                     source_plan_path, source_inbox_msg, spawned_by_session,
                     spawned_by_harness, spawned_by_cwd, think_session_id, think_output_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    node.id,
                    provenance.source,
                    provenance.source_kind,
                    provenance.source_project,
                    provenance.source_session_id,
                    provenance.source_harness,
                    provenance.source_cwd,
                    provenance.source_node_id,
                    provenance.source_plan_path,
                    provenance.source_inbox_msg,
                    provenance.spawned_by_session,
                    provenance.spawned_by_harness,
                    provenance.spawned_by_cwd,
                    provenance.think_session_id,
                    provenance.think_output_path,
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    connection
        .execute(
            "DELETE FROM supersessions WHERE node_id = ?1",
            params![node.id],
        )
        .map_err(|error| error.to_string())?;
    if let Some(supersession) = &node.supersession {
        let surfaces = match &supersession.surfaces {
            None => None,
            Some(list) => Some(serde_json::to_string(list).map_err(|error| error.to_string())?),
        };
        let matched_surfaces = match &supersession.matched_surfaces {
            None => None,
            Some(list) => Some(serde_json::to_string(list).map_err(|error| error.to_string())?),
        };
        connection
            .execute(
                "INSERT INTO supersessions (node_id, successor_id, cause, reason, verified_at,
                     evidence_pr, surfaces, matched_surfaces)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    node.id,
                    supersession.successor,
                    supersession.cause,
                    supersession.reason,
                    supersession.verified_at,
                    supersession.evidence_pr,
                    surfaces,
                    matched_surfaces,
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Delete one node's row. The mirrors cascade; child aggregates go through
/// their own modules' delete.
pub fn delete(connection: &Connection, node_id: &str) -> Result<(), String> {
    connection
        .execute("DELETE FROM nodes WHERE id = ?1", params![node_id])
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// One node with its mirrors and child aggregates, or None when the id is
/// unknown.
pub fn load(connection: &Connection, id: &str) -> Result<Option<Node>, String> {
    let mut statement = connection
        .prepare(
            "SELECT id, ordinal, slug, title, kind, status, priority, rank, project, cwd,
                    domain, estimate, difficulty, description, plan_path, parent_id,
                    contained_in, superseded_by, caused_by, fixes_pr, ownership_defect,
                    created_at, touched_at, completed_at, completion_note, deferred_at,
                    deferred_reason, deferred_kind, queued_at, queued_reason, reopened_at,
                    reopened_reason, archived_at, session_id, has_brief, blocks_everything,
                    cost_usd, vision_path, artifact_url, extras
             FROM nodes WHERE id = ?1",
        )
        .map_err(|error| error.to_string())?;
    let mut rows = statement
        .query_map(params![id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<f64>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, Option<String>>(14)?,
                row.get::<_, Option<String>>(15)?,
                row.get::<_, Option<String>>(16)?,
                row.get::<_, Option<String>>(17)?,
                row.get::<_, Option<String>>(18)?,
                row.get::<_, Option<i64>>(19)?,
                row.get::<_, Option<String>>(20)?,
                row.get::<_, Option<String>>(21)?,
                row.get::<_, Option<String>>(22)?,
                row.get::<_, Option<String>>(23)?,
                row.get::<_, Option<String>>(24)?,
                row.get::<_, Option<String>>(25)?,
                row.get::<_, Option<String>>(26)?,
                row.get::<_, Option<String>>(27)?,
                row.get::<_, Option<String>>(28)?,
                row.get::<_, Option<String>>(29)?,
                row.get::<_, Option<String>>(30)?,
                row.get::<_, Option<String>>(31)?,
                row.get::<_, Option<String>>(32)?,
                row.get::<_, Option<String>>(33)?,
                row.get::<_, Option<i64>>(34)?,
                row.get::<_, Option<i64>>(35)?,
                row.get::<_, Option<f64>>(36)?,
                row.get::<_, Option<String>>(37)?,
                row.get::<_, Option<String>>(38)?,
                row.get::<_, String>(39)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let (
        node_id,
        ordinal,
        slug,
        title,
        kind,
        status,
        priority,
        rank,
        project,
        cwd,
        domain,
        estimate,
        difficulty,
        description,
        plan_path,
        parent_id,
        contained_in,
        superseded_by,
        caused_by,
        fixes_pr,
        ownership_defect,
        created_at,
        touched_at,
        completed_at,
        completion_note,
        deferred_at,
        deferred_reason,
        deferred_kind,
        queued_at,
        queued_reason,
        reopened_at,
        reopened_reason,
        archived_at,
        session_id,
        has_brief,
        blocks_everything,
        cost_usd,
        vision_path,
        artifact_url,
        extras,
    ) = match rows.next() {
        Some(row) => row.map_err(|error| error.to_string())?,
        None => return Ok(None),
    };
    let status = Status::parse(&status).map_err(|error| error.to_string())?;
    let priority = Priority::parse(&priority).map_err(|error| error.to_string())?;
    let ownership_defect = match ownership_defect {
        None => None,
        Some(text) => Some(parse_ownership_defect(&text)?),
    };
    // The extras column feeds apply_residual; the two bookkeeping keys this
    // module wrote at save time are ours, so pop them before it runs.
    let mut residual: Map<String, Value> =
        serde_json::from_str(&extras).map_err(|error| error.to_string())?;
    let supersession_extras = match residual.remove("supersession_extras") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(map)) => map,
        Some(other) => return Err(format!("supersession_extras is not an object: {other}")),
    };
    let child_lists_present: Vec<String> = match residual.remove("child_lists_present") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(other) => return Err(format!("child_lists_present is not a list: {other}")),
    };
    let mut node = Node {
        id: node_id,
        ordinal,
        slug,
        title,
        kind,
        status,
        priority,
        rank,
        project,
        cwd,
        domain,
        estimate,
        difficulty,
        description,
        plan_path,
        parent: parent_id,
        contained_in,
        superseded_by,
        caused_by,
        fixes_pr,
        created_at,
        touched_at,
        completed_at,
        completion_note,
        session_id,
        has_brief: has_brief.map(|value| value != 0),
        blocks_everything: blocks_everything.map(|value| value != 0),
        cost_usd,
        vision_path,
        artifact_url,
        archived_at,
        lifecycle: Lifecycle {
            deferred_at,
            deferred_reason,
            deferred_kind,
            queued_at,
            queued_reason,
            reopened_at,
            reopened_reason,
        },
        ownership_defect,
        claim: NodeClaim::default(),
        dispatch: Dispatch::default(),
        provenance: Provenance::default(),
        primary_pr: None,
        additional_prs: None,
        sessions: None,
        comments: None,
        encounters: None,
        decisions: None,
        relations: Relations::default(),
        supersession: None,
        costs: None,
        difficulty_history: None,
        priority_history: None,
        labels: None,
        collision_acks: None,
        tasks: None,
        extras: Map::new(),
    };
    node.apply_residual(residual)
        .map_err(|error| error.to_string())?;
    let mut claim_statement = connection
        .prepare(
            "SELECT locked_by, harness, harness_session, locked_at
             FROM node_claims WHERE node_id = ?1",
        )
        .map_err(|error| error.to_string())?;
    node.claim = claim_statement
        .query_row(params![id], |row| {
            Ok(NodeClaim {
                locked_by: row.get(0)?,
                harness: row.get(1)?,
                harness_session: row.get(2)?,
                locked_at: row.get(3)?,
            })
        })
        .optional()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let mut dispatch_statement = connection
        .prepare("SELECT verb, brief, model FROM node_dispatch WHERE node_id = ?1")
        .map_err(|error| error.to_string())?;
    node.dispatch = dispatch_statement
        .query_row(params![id], |row| {
            Ok(Dispatch {
                verb: row.get(0)?,
                brief: row.get(1)?,
                model: row.get(2)?,
            })
        })
        .optional()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    // The 14 columns overlay what apply_residual parsed; the two provenance
    // fields with no columns (origin_evidence, request_origin) survive from it.
    let mut provenance_statement = connection
        .prepare(
            "SELECT source, source_kind, source_project, source_session_id, source_harness,
                    source_cwd, source_node_id, source_plan_path, source_inbox_msg,
                    spawned_by_session, spawned_by_harness, spawned_by_cwd, think_session_id,
                    think_output_path
             FROM node_provenance WHERE node_id = ?1",
        )
        .map_err(|error| error.to_string())?;
    let provenance_row = provenance_statement
        .query_row(params![id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
            ))
        })
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some((
        source,
        source_kind,
        source_project,
        source_session_id,
        source_harness,
        source_cwd,
        source_node_id,
        source_plan_path,
        source_inbox_msg,
        spawned_by_session,
        spawned_by_harness,
        spawned_by_cwd,
        think_session_id,
        think_output_path,
    )) = provenance_row
    {
        let origin_evidence = node.provenance.origin_evidence.clone();
        let request_origin = node.provenance.request_origin.clone();
        node.provenance = Provenance {
            source,
            source_kind,
            source_project,
            source_session_id,
            source_harness,
            source_cwd,
            source_node_id,
            source_plan_path,
            source_inbox_msg,
            spawned_by_session,
            spawned_by_harness,
            spawned_by_cwd,
            think_session_id,
            think_output_path,
            origin_evidence,
            request_origin,
        };
    }
    let mut supersession_statement = connection
        .prepare(
            "SELECT successor_id, cause, reason, verified_at, evidence_pr, surfaces,
                    matched_surfaces
             FROM supersessions WHERE node_id = ?1",
        )
        .map_err(|error| error.to_string())?;
    node.supersession = supersession_statement
        .query_row(params![id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .optional()
        .map_err(|error| error.to_string())?
        .map(
            |(successor, cause, reason, verified_at, evidence_pr, surfaces, matched_surfaces)| {
                Supersession {
                    successor,
                    cause,
                    reason,
                    verified_at,
                    evidence_pr,
                    surfaces: surfaces.and_then(|text| serde_json::from_str(&text).ok()),
                    matched_surfaces: matched_surfaces
                        .and_then(|text| serde_json::from_str(&text).ok()),
                    extras: supersession_extras,
                }
            },
        );
    // Child aggregates. The tables cannot tell an empty list from an absent
    // key, so the "child_lists_present" marker decides Some vs None.
    let present = |name: &str| child_lists_present.iter().any(|listed| listed == name);
    let loaded = sessions::load(connection, id)?;
    node.sessions = if present("sessions") {
        Some(loaded)
    } else {
        None
    };
    let loaded = comments::load(connection, id)?;
    node.comments = if present("comments") {
        Some(loaded)
    } else {
        None
    };
    let loaded = encounters::load(connection, id)?;
    node.encounters = if present("encounters") {
        Some(loaded)
    } else {
        None
    };
    let pr_rows = pull_requests::load(connection, id)?;
    let primary = if present("primary_pr") {
        pr_rows.first().cloned()
    } else {
        None
    };
    let rest: Vec<PullRequest> = if primary.is_some() {
        pr_rows.into_iter().skip(1).collect()
    } else {
        pr_rows
    };
    node.primary_pr = primary;
    node.additional_prs = if present("additional_prs") {
        Some(rest)
    } else {
        None
    };
    node.relations = relations::load_grouped(connection, id)?;
    // An empty relation list leaves no rows; the marker restores presence.
    if present("blocked_by") && node.relations.blocked_by.is_none() {
        node.relations.blocked_by = Some(Vec::new());
    }
    if present("related") && node.relations.related.is_none() {
        node.relations.related = Some(Vec::new());
    }
    if present("supersedes") && node.relations.supersedes.is_none() {
        node.relations.supersedes = Some(Vec::new());
    }
    Ok(Some(node))
}

fn parse_ownership_defect(text: &str) -> Result<OwnershipDefect, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "ownership_defect is not an object".to_string())?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "ownership_defect needs a kind".to_string())?
        .to_string();
    let field = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_string);
    let extras: Map<String, Value> = object
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "kind" | "node_id" | "holder" | "liveness"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Ok(OwnershipDefect {
        kind,
        node_id: field("node_id"),
        holder: field("holder"),
        liveness: field("liveness"),
        extras,
    })
}
