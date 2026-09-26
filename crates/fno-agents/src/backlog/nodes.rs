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
//! "supersession_extras".
//!
//! Schema 4: `nodes.version` and `nodes_raw.version` count the writes of
//! that row. The row's own write bumps it, and so does the status pass when
//! it moves a status or a defect; the keeper's commit_rows compares them
//! (see [`versions`]). A row moving between the two tables starts one above
//! its old version, so the move itself reads as a write.
//! The claim's lock time is the claim row's created_at. A schema-3 binary
//! still writes request_origin/origin_evidence into extras, so load reads
//! the provenance columns first and those extras keys second.

use super::model::{
    Dispatch, Lifecycle, Node, NodeClaim, OwnershipDefect, Priority, Provenance, PullRequest,
    Relations, Status, Supersession,
};
use super::schema_v4::{iso, norm_sql, stamps, touch, updated, NOW};
use super::{comments, costs, encounters, findings, pull_requests, relations, sessions};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The tables this module owns, for the updated_at triggers.
const TABLES: [&str; 6] = [
    "nodes",
    "nodes_raw",
    "node_claims",
    "node_dispatch",
    "node_provenance",
    "supersessions",
];

/// Schema 4. nodes and node_claims keep a wire `created_at` (a claim's is
/// its lock time) and gain only updated_at; the other tables get both.
pub fn ddl() -> String {
    let nodes_iso = [
        "created_at",
        "touched_at",
        "completed_at",
        "deferred_at",
        "queued_at",
        "reopened_at",
        "archived_at",
    ]
    .map(|column| iso("nodes", column))
    .join(",\n  ");
    format!(
        "CREATE TABLE IF NOT EXISTS nodes (
  id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, slug TEXT NOT NULL UNIQUE, title TEXT NOT NULL,
  kind TEXT, status TEXT NOT NULL,
  priority TEXT NOT NULL CHECK (priority IN ('p0','p1','p2','p3')), rank REAL,
  project TEXT, cwd TEXT, domain TEXT, estimate TEXT, difficulty TEXT,
  description TEXT, plan_path TEXT, parent_id TEXT, contained_in TEXT, superseded_by TEXT,
  caused_by TEXT, fixes_pr INTEGER, ownership_defect TEXT, created_at TEXT,
  touched_at TEXT, completed_at TEXT, completion_note TEXT,
  deferred_at TEXT, deferred_reason TEXT, deferred_kind TEXT,
  queued_at TEXT, queued_reason TEXT, reopened_at TEXT, reopened_reason TEXT,
  archived_at TEXT, session_id TEXT REFERENCES agent_sessions(id), has_brief INTEGER,
  blocks_everything INTEGER, cost_usd REAL, vision_path TEXT,
  artifact_url TEXT, extras TEXT NOT NULL DEFAULT '{{}}',
  version INTEGER NOT NULL DEFAULT 0 CONSTRAINT nodes_version_nonnegative CHECK (version >= 0){},
  {nodes_iso}
);
CREATE INDEX IF NOT EXISTS nodes_parent ON nodes(parent_id);
CREATE INDEX IF NOT EXISTS nodes_status ON nodes(status, project);
CREATE INDEX IF NOT EXISTS nodes_archive ON nodes(archived_at);
CREATE TABLE IF NOT EXISTS node_claims (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  locked_by TEXT, harness TEXT REFERENCES harnesses(id),
  harness_session TEXT REFERENCES agent_sessions(id), created_at TEXT{},
  {}
);
CREATE TABLE IF NOT EXISTS node_dispatch (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  verb TEXT, brief TEXT, model TEXT REFERENCES models(id){}
);
CREATE TABLE IF NOT EXISTS node_provenance (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  source TEXT, source_kind TEXT, source_project TEXT,
  source_session_id TEXT REFERENCES agent_sessions(id),
  source_harness TEXT REFERENCES harnesses(id), source_cwd TEXT, source_node_id TEXT,
  source_plan_path TEXT, source_inbox_msg TEXT,
  spawned_by_session TEXT REFERENCES agent_sessions(id),
  spawned_by_harness TEXT REFERENCES harnesses(id), spawned_by_cwd TEXT,
  think_session_id TEXT REFERENCES agent_sessions(id), think_output_path TEXT,
  request_origin TEXT, origin_evidence TEXT{}
);
CREATE TABLE IF NOT EXISTS supersessions (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  successor_id TEXT, cause TEXT, reason TEXT, verified_at TEXT, evidence_pr INTEGER,
  surfaces TEXT, matched_surfaces TEXT{},
  {}
);
CREATE TABLE IF NOT EXISTS nodes_raw (
  id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, body TEXT NOT NULL,
  version INTEGER NOT NULL DEFAULT 0 CONSTRAINT nodes_raw_version_nonnegative CHECK (version >= 0){}
);",
        updated("nodes"),
        updated("node_claims"),
        iso("node_claims", "created_at"),
        stamps("node_dispatch"),
        stamps("node_provenance"),
        stamps("supersessions"),
        iso("supersessions", "verified_at"),
        stamps("nodes_raw"),
    )
}

pub fn triggers() -> String {
    TABLES.iter().map(|table| touch(table)).collect()
}

/// Schema-4 migration copy of the node tables from their `_v3` renames,
/// rowids kept so the search index stays valid. Every row starts at
/// version 0. request_origin and origin_evidence move from nodes.extras
/// into node_provenance columns. Returns how many nodes had one promoted.
pub(crate) fn copy_from_v3(connection: &Connection) -> Result<i64, String> {
    let text = |key: &str| format!("json_type(n.extras, '$.{key}') = 'text'");
    let strip = |key: &str, from: &str| {
        format!(
            "CASE WHEN json_type({from}, '$.{key}') = 'text'
                  THEN json_remove({from}, '$.{key}') ELSE {from} END"
        )
    };
    let extras = strip("origin_evidence", &strip("request_origin", "n.extras"));
    let node_stamp = format!("COALESCE({}, {NOW})", norm_sql("n.created_at"));
    let promoted: i64 = connection
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM nodes_v3 n WHERE {} OR {}",
                text("request_origin"),
                text("origin_evidence")
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let columns = "id, ordinal, slug, title, kind, status, priority, rank, project, cwd, domain,
                   estimate, difficulty, description, plan_path, parent_id, contained_in,
                   superseded_by, caused_by, fixes_pr, ownership_defect, created_at, touched_at,
                   completed_at, completion_note, deferred_at, deferred_reason, deferred_kind,
                   queued_at, queued_reason, reopened_at, reopened_reason, archived_at,
                   session_id, has_brief, blocks_everything, cost_usd, vision_path,
                   artifact_url";
    let selected = columns
        .split(',')
        .map(|column| format!("n.{}", column.trim()))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!(
        "INSERT INTO nodes (rowid, {columns}, extras, version, updated_at)
         SELECT n.rowid, {selected}, {extras}, 0,
                COALESCE({}, {}, {NOW})
         FROM nodes_v3 n;
         INSERT INTO node_claims (node_id, locked_by, harness, harness_session, created_at,
             updated_at)
         SELECT c.node_id, c.locked_by, c.harness, c.harness_session, c.locked_at,
                COALESCE({}, {node_stamp})
         FROM node_claims_v3 c LEFT JOIN nodes_v3 n ON n.id = c.node_id;
         INSERT INTO node_dispatch (node_id, verb, brief, model, created_at, updated_at)
         SELECT d.node_id, d.verb, d.brief, d.model, {node_stamp}, {node_stamp}
         FROM node_dispatch_v3 d LEFT JOIN nodes_v3 n ON n.id = d.node_id;
         INSERT INTO node_provenance (node_id, source, source_kind, source_project,
             source_session_id, source_harness, source_cwd, source_node_id, source_plan_path,
             source_inbox_msg, spawned_by_session, spawned_by_harness, spawned_by_cwd,
             think_session_id, think_output_path, request_origin, origin_evidence,
             created_at, updated_at)
         SELECT n.id, p.source, p.source_kind, p.source_project, p.source_session_id,
                p.source_harness, p.source_cwd, p.source_node_id, p.source_plan_path,
                p.source_inbox_msg, p.spawned_by_session, p.spawned_by_harness,
                p.spawned_by_cwd, p.think_session_id, p.think_output_path,
                CASE WHEN {ro} THEN json_extract(n.extras, '$.request_origin') END,
                CASE WHEN {oe} THEN json_extract(n.extras, '$.origin_evidence') END,
                {node_stamp}, {node_stamp}
         FROM nodes_v3 n LEFT JOIN node_provenance_v3 p ON p.node_id = n.id
         WHERE p.node_id IS NOT NULL OR {ro} OR {oe};
         INSERT INTO supersessions (node_id, successor_id, cause, reason, verified_at,
             evidence_pr, surfaces, matched_surfaces, created_at, updated_at)
         SELECT s.node_id, s.successor_id, s.cause, s.reason, s.verified_at, s.evidence_pr,
                s.surfaces, s.matched_surfaces, COALESCE({}, {node_stamp}),
                COALESCE({}, {node_stamp})
         FROM supersessions_v3 s LEFT JOIN nodes_v3 n ON n.id = s.node_id;",
        norm_sql("n.touched_at"),
        norm_sql("n.created_at"),
        norm_sql("c.locked_at"),
        norm_sql("s.verified_at"),
        norm_sql("s.verified_at"),
        ro = text("request_origin"),
        oe = text("origin_evidence"),
    );
    if super::schema_v4::table_exists(connection, "nodes_raw_v3")? {
        sql.push_str(&format!(
            "INSERT INTO nodes_raw (rowid, id, ordinal, body, version, created_at, updated_at)
             SELECT rowid, id, ordinal, body, 0, {NOW}, {NOW} FROM nodes_raw_v3;"
        ));
    }
    connection
        .execute_batch(&sql)
        .map_err(|error| format!("schema v4 node tables copy: {error}"))?;
    Ok(promoted)
}

/// Schema-4 migration step: drop `key` from the extras of `ids` once its
/// value has moved to a table of its own.
pub(crate) fn strip_extras_key(
    connection: &Connection,
    key: &str,
    ids: &[String],
) -> Result<(), String> {
    let mut statement = connection
        .prepare("UPDATE nodes SET extras = json_remove(extras, ?1) WHERE id = ?2")
        .map_err(|error| error.to_string())?;
    let path = format!("$.{key}");
    for id in ids {
        statement
            .execute(params![path, id])
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Carry a row the node model cannot represent: its verbatim JSON is the
/// stored form, so a read hands it back exactly as the caller wrote it.
pub fn save_raw(
    connection: &Connection,
    id: &str,
    ordinal: i64,
    body: &Value,
) -> Result<(), String> {
    let text = serde_json::to_string(body).map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT INTO nodes_raw(id, ordinal, body, version)
             VALUES(?1, ?2, ?3, (SELECT COALESCE(MAX(version) + 1, 0) FROM nodes WHERE id = ?1))
             ON CONFLICT(id) DO UPDATE SET ordinal = excluded.ordinal, body = excluded.body,
                 version = nodes_raw.version + 1",
            params![id, ordinal, text],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// The stored write count of each node and raw-carried row, restricted to
/// `ids` when given. The keeper compares these under its write gate: a
/// row whose version moved since a writer's begin was written by someone
/// else in between.
pub fn versions(
    connection: &Connection,
    ids: Option<&[&str]>,
) -> Result<BTreeMap<String, i64>, String> {
    const SQLITE_BIND_BATCH: usize = 900;
    let mut out = BTreeMap::new();
    for table in ["nodes", "nodes_raw"] {
        let batches: Vec<&[&str]> = match ids {
            None => vec![&[]],
            Some(ids) => ids.chunks(SQLITE_BIND_BATCH).collect(),
        };
        for batch in batches {
            let filter = match ids {
                None => String::new(),
                Some(_) => format!(
                    " WHERE id IN ({})",
                    std::iter::repeat_n("?", batch.len())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            };
            let mut statement = connection
                .prepare(&format!("SELECT id, version FROM {table}{filter}"))
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(batch.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|error| error.to_string())?;
            for row in rows {
                let (id, version) = row.map_err(|error| error.to_string())?;
                out.insert(id, version);
            }
        }
    }
    Ok(out)
}

pub fn delete_raw(connection: &Connection, id: &str) -> Result<(), String> {
    connection
        .execute("DELETE FROM nodes_raw WHERE id = ?1", params![id])
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Raw-carried rows as (id, ordinal, body), ordinal order.
pub fn raw_rows(connection: &Connection) -> Result<Vec<(String, i64, Value)>, String> {
    let mut statement = connection
        .prepare("SELECT id, ordinal, body FROM nodes_raw ORDER BY ordinal, id")
        .map_err(|error| error.to_string())?;
    let found = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut rows = Vec::new();
    for row in found {
        let (id, ordinal, body) = row.map_err(|error| error.to_string())?;
        let body: Value =
            serde_json::from_str(&body).map_err(|error| format!("nodes_raw {id}: {error}"))?;
        rows.push((id, ordinal, body));
    }
    Ok(rows)
}

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(&ddl())
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
    // A cost list node_costs can hold lives there (costs::save, called by
    // the aggregate save); any other rides extras as before.
    if node.costs.as_deref().is_some_and(costs::holds) {
        extras_map.remove("cost_sessions");
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
        (node.findings.is_some(), "findings"),
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
    // An upsert, never INSERT OR REPLACE: a REPLACE deletes the old row, and
    // under foreign_keys=ON that delete cascades to every node-owned child
    // row (node_decisions among them). No change guard here: write_changed
    // reaches this only for a changed aggregate, and the row write is the one
    // statement every aggregate change runs, so it is where the version
    // moves. A guard would skip the bump when only a child row changed.
    connection
        .execute(
            "INSERT INTO nodes (id, ordinal, slug, title, kind, status, priority,
                 rank, project, cwd, domain, estimate, difficulty, description, plan_path,
                 parent_id, contained_in, superseded_by, caused_by, fixes_pr, ownership_defect,
                 created_at, touched_at, completed_at, completion_note, deferred_at,
                 deferred_reason, deferred_kind, queued_at, queued_reason, reopened_at,
                 reopened_reason, archived_at, session_id, has_brief, blocks_everything,
                 cost_usd, vision_path, artifact_url, extras, version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31,
                 ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40,
                 (SELECT COALESCE(MAX(version) + 1, 0) FROM nodes_raw WHERE id = ?1))
             ON CONFLICT(id) DO UPDATE SET ordinal = excluded.ordinal, slug = excluded.slug,
                 title = excluded.title, kind = excluded.kind, status = excluded.status,
                 priority = excluded.priority, rank = excluded.rank,
                 project = excluded.project, cwd = excluded.cwd, domain = excluded.domain,
                 estimate = excluded.estimate, difficulty = excluded.difficulty,
                 description = excluded.description, plan_path = excluded.plan_path,
                 parent_id = excluded.parent_id, contained_in = excluded.contained_in,
                 superseded_by = excluded.superseded_by, caused_by = excluded.caused_by,
                 fixes_pr = excluded.fixes_pr, ownership_defect = excluded.ownership_defect,
                 created_at = excluded.created_at, touched_at = excluded.touched_at,
                 completed_at = excluded.completed_at,
                 completion_note = excluded.completion_note,
                 deferred_at = excluded.deferred_at, deferred_reason = excluded.deferred_reason,
                 deferred_kind = excluded.deferred_kind, queued_at = excluded.queued_at,
                 queued_reason = excluded.queued_reason, reopened_at = excluded.reopened_at,
                 reopened_reason = excluded.reopened_reason,
                 archived_at = excluded.archived_at, session_id = excluded.session_id,
                 has_brief = excluded.has_brief,
                 blocks_everything = excluded.blocks_everything,
                 cost_usd = excluded.cost_usd, vision_path = excluded.vision_path,
                 artifact_url = excluded.artifact_url, extras = excluded.extras,
                 version = nodes.version + 1",
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
    // The single-row mirrors upsert with a change guard, so an unchanged row
    // is never rewritten; an absent mirror deletes its row.
    let claim = &node.claim;
    if claim.locked_by.is_some()
        || claim.harness.is_some()
        || claim.harness_session.is_some()
        || claim.locked_at.is_some()
    {
        connection
            .execute(
                "INSERT INTO node_claims (node_id, locked_by, harness, harness_session, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(node_id) DO UPDATE SET locked_by = excluded.locked_by,
                     harness = excluded.harness, harness_session = excluded.harness_session,
                     created_at = excluded.created_at
                 WHERE (node_claims.locked_by, node_claims.harness,
                        node_claims.harness_session, node_claims.created_at)
                   IS NOT (excluded.locked_by, excluded.harness, excluded.harness_session,
                           excluded.created_at)",
                params![
                    node.id,
                    claim.locked_by,
                    claim.harness,
                    claim.harness_session,
                    claim.locked_at,
                ],
            )
            .map_err(|error| error.to_string())?;
    } else {
        connection
            .execute(
                "DELETE FROM node_claims WHERE node_id = ?1",
                params![node.id],
            )
            .map_err(|error| error.to_string())?;
    }
    let dispatch = &node.dispatch;
    if dispatch.verb.is_some() || dispatch.brief.is_some() || dispatch.model.is_some() {
        connection
            .execute(
                "INSERT INTO node_dispatch (node_id, verb, brief, model)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(node_id) DO UPDATE SET verb = excluded.verb,
                     brief = excluded.brief, model = excluded.model
                 WHERE (node_dispatch.verb, node_dispatch.brief, node_dispatch.model)
                   IS NOT (excluded.verb, excluded.brief, excluded.model)",
                params![node.id, dispatch.verb, dispatch.brief, dispatch.model],
            )
            .map_err(|error| error.to_string())?;
    } else {
        connection
            .execute(
                "DELETE FROM node_dispatch WHERE node_id = ?1",
                params![node.id],
            )
            .map_err(|error| error.to_string())?;
    }
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
        &provenance.request_origin,
        &provenance.origin_evidence,
    ]
    .iter()
    .any(|value| value.is_some());
    if provenance_any {
        connection
            .execute(
                "INSERT INTO node_provenance (node_id, source, source_kind, source_project,
                     source_session_id, source_harness, source_cwd, source_node_id,
                     source_plan_path, source_inbox_msg, spawned_by_session,
                     spawned_by_harness, spawned_by_cwd, think_session_id, think_output_path,
                     request_origin, origin_evidence)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17)
                 ON CONFLICT(node_id) DO UPDATE SET source = excluded.source,
                     source_kind = excluded.source_kind,
                     source_project = excluded.source_project,
                     source_session_id = excluded.source_session_id,
                     source_harness = excluded.source_harness,
                     source_cwd = excluded.source_cwd,
                     source_node_id = excluded.source_node_id,
                     source_plan_path = excluded.source_plan_path,
                     source_inbox_msg = excluded.source_inbox_msg,
                     spawned_by_session = excluded.spawned_by_session,
                     spawned_by_harness = excluded.spawned_by_harness,
                     spawned_by_cwd = excluded.spawned_by_cwd,
                     think_session_id = excluded.think_session_id,
                     think_output_path = excluded.think_output_path,
                     request_origin = excluded.request_origin,
                     origin_evidence = excluded.origin_evidence
                 WHERE (node_provenance.source, node_provenance.source_kind,
                        node_provenance.source_project, node_provenance.source_session_id,
                        node_provenance.source_harness, node_provenance.source_cwd,
                        node_provenance.source_node_id, node_provenance.source_plan_path,
                        node_provenance.source_inbox_msg, node_provenance.spawned_by_session,
                        node_provenance.spawned_by_harness, node_provenance.spawned_by_cwd,
                        node_provenance.think_session_id, node_provenance.think_output_path,
                        node_provenance.request_origin, node_provenance.origin_evidence)
                   IS NOT (excluded.source, excluded.source_kind, excluded.source_project,
                           excluded.source_session_id, excluded.source_harness,
                           excluded.source_cwd, excluded.source_node_id,
                           excluded.source_plan_path, excluded.source_inbox_msg,
                           excluded.spawned_by_session, excluded.spawned_by_harness,
                           excluded.spawned_by_cwd, excluded.think_session_id,
                           excluded.think_output_path, excluded.request_origin,
                           excluded.origin_evidence)",
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
                    provenance.request_origin,
                    provenance.origin_evidence,
                ],
            )
            .map_err(|error| error.to_string())?;
    } else {
        connection
            .execute(
                "DELETE FROM node_provenance WHERE node_id = ?1",
                params![node.id],
            )
            .map_err(|error| error.to_string())?;
    }
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
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(node_id) DO UPDATE SET successor_id = excluded.successor_id,
                     cause = excluded.cause, reason = excluded.reason,
                     verified_at = excluded.verified_at, evidence_pr = excluded.evidence_pr,
                     surfaces = excluded.surfaces, matched_surfaces = excluded.matched_surfaces
                 WHERE (supersessions.successor_id, supersessions.cause, supersessions.reason,
                        supersessions.verified_at, supersessions.evidence_pr,
                        supersessions.surfaces, supersessions.matched_surfaces)
                   IS NOT (excluded.successor_id, excluded.cause, excluded.reason,
                           excluded.verified_at, excluded.evidence_pr, excluded.surfaces,
                           excluded.matched_surfaces)",
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
    } else {
        connection
            .execute(
                "DELETE FROM supersessions WHERE node_id = ?1",
                params![node.id],
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
        findings: None,
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
    let stored_costs = costs::load(connection, id)?;
    if !stored_costs.is_empty() {
        node.costs = Some(stored_costs);
    }
    let mut claim_statement = connection
        .prepare(
            "SELECT locked_by, harness, harness_session, created_at
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
    // The columns overlay what apply_residual parsed. request_origin and
    // origin_evidence fall back to the extras keys a schema-3 writer left.
    let mut provenance_statement = connection
        .prepare(
            "SELECT source, source_kind, source_project, source_session_id, source_harness,
                    source_cwd, source_node_id, source_plan_path, source_inbox_msg,
                    spawned_by_session, spawned_by_harness, spawned_by_cwd, think_session_id,
                    think_output_path, request_origin, origin_evidence
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
                row.get::<_, Option<String>>(14)?,
                row.get::<_, Option<String>>(15)?,
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
        request_origin,
        origin_evidence,
    )) = provenance_row
    {
        let request_origin = request_origin.or(node.provenance.request_origin.take());
        let origin_evidence = origin_evidence.or(node.provenance.origin_evidence.take());
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
    let loaded = findings::load(connection, id)?;
    node.findings = if present("findings") {
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

/// The single-row recompute: one projection query loads the
/// status-relevant columns of every live node, the derivation runs over that
/// projection in memory, and only rows whose status changed are UPDATEd, in
/// the caller's transaction. The plan-rung ladder (idea/design/ready) is
/// deliberately absent: the whole-graph pass derives it from a plan-rung map
/// the caller supplies, and a single-row write has none, so a stored rung
/// keeps. Container rollup mirrors the whole-graph pass, deepest parents
/// first, over the derived statuses.
pub fn recompute_status(connection: &Connection) -> Result<(), String> {
    struct Driver {
        id: String,
        parent: Option<String>,
        status: String,
        stored: String,
        completed: bool,
        superseded: bool,
        deferred: bool,
        locked: bool,
        locked_at: Option<String>,
        holder: Option<String>,
        open_do: bool,
        has_pr: bool,
        defect: Option<String>,
    }
    let mut statement = connection
        .prepare(
            "SELECT n.id, n.parent_id, n.status,
                    CASE WHEN n.completed_at LIKE 'deferred:%' THEN 0
                         ELSE n.completed_at IS NOT NULL END,
                    n.superseded_by IS NOT NULL,
                    CASE WHEN n.completed_at LIKE 'deferred:%' THEN 1
                         ELSE n.deferred_at IS NOT NULL END,
                    COALESCE((SELECT c.locked_by FROM node_claims c
                              WHERE c.node_id = n.id), '') <> '',
                    (SELECT c.created_at FROM node_claims c WHERE c.node_id = n.id),
                    (SELECT c.locked_by FROM node_claims c WHERE c.node_id = n.id),
                    EXISTS(SELECT 1 FROM sessions s WHERE s.node_id = n.id
                           AND s.phase = 'do' AND s.ended_at IS NULL),
                    EXISTS(SELECT 1 FROM pull_requests p WHERE p.node_id = n.id
                           AND p.seq = 0 AND p.number IS NOT NULL),
                    n.ownership_defect
             FROM nodes n",
        )
        .map_err(|error| error.to_string())?;
    let mut rows: Vec<Driver> = statement
        .query_map([], |row| {
            Ok(Driver {
                id: row.get(0)?,
                parent: row.get(1)?,
                stored: row.get::<_, String>(2)?,
                status: row.get(2)?,
                completed: row.get::<_, i64>(3)? != 0,
                superseded: row.get::<_, i64>(4)? != 0,
                deferred: row.get::<_, i64>(5)? != 0,
                locked: row.get::<_, i64>(6)? != 0,
                locked_at: row.get(7)?,
                holder: row.get(8)?,
                open_do: row.get::<_, i64>(9)? != 0,
                has_pr: row.get::<_, i64>(10)? != 0,
                defect: row.get(11)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    // Open do rows with their timestamps, for the ownership-defect quality
    // read (quality port): one query for the whole projection.
    let mut do_rows: std::collections::HashMap<String, Vec<(String, String)>> = Default::default();
    let mut do_statement = connection
        .prepare(
            "SELECT node_id, COALESCE(started_at, ''), COALESCE(session_id, '')
             FROM sessions WHERE phase = 'do' AND ended_at IS NULL",
        )
        .map_err(|error| error.to_string())?;
    let do_list = do_statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    for (node_id, started_at, holder) in do_list {
        do_rows
            .entry(node_id)
            .or_default()
            .push((started_at, holder));
    }
    // Driver pass, same precedence as the whole-graph derivation: completed,
    // superseded, deferred, PR, held. Otherwise the stored status keeps.
    // The ownership defect clears every pass and restamps only for a live
    // (non-terminal) row whose lock or open-do timestamp reads stale, the
    // same diagnostic the whole-graph pass writes.
    let fresh_defect = std::collections::HashMap::<String, Option<String>>::new();
    let mut stamp_defect: std::collections::HashMap<String, Option<String>> = fresh_defect;
    for row in &mut rows {
        row.status = if row.completed {
            "done".into()
        } else if row.superseded {
            "superseded".into()
        } else if row.deferred {
            "deferred".into()
        } else if row.has_pr {
            "in_review".into()
        } else if row.locked || row.open_do {
            "in_progress".into()
        } else {
            row.status.clone()
        };
        if row.completed || row.superseded || row.deferred {
            stamp_defect.insert(row.id.clone(), None);
            continue;
        }
        let quality = if row.locked {
            lock_quality(row.locked_at.as_deref())
                .map(|q| (q, row.holder.clone().unwrap_or_default()))
        } else if row.open_do {
            do_quality(&do_rows.get(&row.id).cloned().unwrap_or_default())
        } else {
            None
        };
        let defect = quality.map(|(kind, holder)| {
            serde_json::json!({
                "kind": if kind == "old" {
                    if row.locked { "stale-active-owner-unverified" } else { "stale-open-do-unverified" }
                } else {
                    if row.locked { "lock-timestamp-unreadable" } else { "do-row-timestamp-unreadable" }
                },
                "node_id": row.id,
                "holder": holder,
                "liveness": "unverified",
            })
            .to_string()
        });
        stamp_defect.insert(row.id.clone(), defect);
    }
    // Container rollup, deepest parents first, over the derived statuses.
    let by_id: std::collections::HashMap<String, usize> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| (row.id.clone(), index))
        .collect();
    let valid_parents: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|row| row.parent.clone())
        .filter(|parent| by_id.contains_key(parent))
        .collect();
    let mut depth: std::collections::HashMap<String, usize> = Default::default();
    // `visiting` breaks a corrupted parent cycle: the whole-graph rollup
    // defends the same way, and a cycle here would recurse to overflow.
    fn depth_of(
        id: &str,
        by_id: &std::collections::HashMap<String, usize>,
        rows: &[Driver],
        depth: &mut std::collections::HashMap<String, usize>,
        visiting: &mut std::collections::HashSet<String>,
    ) -> usize {
        if let Some(known) = depth.get(id) {
            return *known;
        }
        if !visiting.insert(id.to_string()) {
            return 0;
        }
        let parent = by_id.get(id).and_then(|index| rows[*index].parent.clone());
        let value = match parent {
            Some(parent) if by_id.contains_key(&parent) => {
                1 + depth_of(&parent, by_id, rows, depth, visiting)
            }
            _ => 0,
        };
        visiting.remove(id);
        depth.insert(id.to_string(), value);
        value
    }
    let mut parents: Vec<(String, usize)> = valid_parents
        .into_iter()
        .map(|pid| {
            let mut visiting = std::collections::HashSet::new();
            let d = depth_of(&pid, &by_id, &rows, &mut depth, &mut visiting);
            (pid, d)
        })
        .collect();
    parents.sort_by(|a, b| b.1.cmp(&a.1));
    for (pid, _) in parents {
        let pidx = by_id[&pid];
        if rows[pidx].completed || rows[pidx].superseded || rows[pidx].deferred {
            continue;
        }
        let child_statuses: Vec<String> = rows
            .iter()
            .filter(|row| row.parent.as_deref() == Some(pid.as_str()))
            .map(|row| row.status.clone())
            .collect();
        let own_work_live = matches!(rows[pidx].status.as_str(), "in_review" | "in_progress");
        if !child_statuses.is_empty() && child_statuses.iter().all(|s| s == "done") {
            if !own_work_live {
                rows[pidx].status = "done".into();
            }
        } else if child_statuses
            .iter()
            .any(|s| s == "in_review" || s == "in_progress")
        {
            rows[pidx].status = "in_progress".into();
        }
    }
    // Only rows whose status or defect moved are written.
    for row in &rows {
        let fresh_defect = stamp_defect.get(&row.id).cloned().flatten();
        // String compare: the UPDATE stores this exact spelling, so the
        // compare converges after at most one write per row.
        let defect_changed = fresh_defect.as_deref() != row.defect.as_deref();
        if row.status == row.stored && !defect_changed {
            continue;
        }
        connection
            .execute(
                "UPDATE nodes SET status = ?1, ownership_defect = ?2, version = version + 1
                 WHERE id = ?3 AND (status IS NOT ?1
                    OR COALESCE(ownership_defect, '') IS NOT COALESCE(?2, ''))",
                params![row.status, fresh_defect, row.id],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// The lock route of `graph_store::lock_timestamp_quality`, over the
/// projection's `locked_at` column: past the TTL reads "old", absent or
/// unparusable reads "unreadable", otherwise "fresh" (which stamps nothing).
fn lock_quality(locked_at: Option<&str>) -> Option<&'static str> {
    let Some(ts) = locked_at else {
        return Some("unreadable");
    };
    let parsed = chrono::DateTime::parse_from_rfc3339(&ts.replace('Z', "+00:00"));
    match parsed {
        Err(_) => Some("unreadable"),
        Ok(parsed) => {
            let elapsed = (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc)).num_seconds();
            if (elapsed as f64) / 3600.0 > crate::graph_store::lock_ttl_hours() {
                Some("old")
            } else {
                None
            }
        }
    }
}

/// The open-do route of `graph_store::open_do_quality_and_holder`: any open
/// do row past the do TTL reads ("old", holder); an unparusable row reads
/// ("unreadable", holder); otherwise nothing stamps.
fn do_quality(rows: &[(String, String)]) -> Option<(&'static str, String)> {
    let mut unreadable: Option<String> = None;
    for (started_at, holder) in rows {
        let parsed = chrono::DateTime::parse_from_rfc3339(&started_at.replace('Z', "+00:00"));
        match parsed {
            Err(_) => {
                if unreadable.is_none() {
                    unreadable = Some(holder.clone());
                }
            }
            Ok(parsed) => {
                let elapsed =
                    (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc)).num_seconds();
                if (elapsed as f64) / 3600.0 > crate::graph_store::do_ttl_hours() {
                    return Some(("old", holder.clone()));
                }
            }
        }
    }
    unreadable.map(|h| ("unreadable", h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, b"{\"entries\": []}").unwrap();
        let connection = crate::backlog::open(&graph).unwrap();
        (dir, connection)
    }

    fn node(sessions: Value) -> Node {
        Node::from_json(&serde_json::json!({
            "id": "x-a", "slug": "a", "title": "A", "type": "feature",
            "status": "ready", "priority": "p2", "domain": "code",
            "created_at": "2026-09-11T00:00:00+00:00",
            "sessions": sessions,
            "blocked_by": ["x-a"],
        }))
        .unwrap()
    }

    fn two_sessions() -> Value {
        serde_json::json!([
            {"phase": "do", "harness": "claude", "session_id": "s-1"},
            {"phase": "review", "harness": "codex", "session_id": "s-2"},
        ])
    }

    fn column(connection: &Connection, sql: &str) -> Vec<i64> {
        let mut statement = connection.prepare(sql).unwrap();
        let rows = statement.query_map([], |row| row.get(0)).unwrap();
        rows.collect::<Result<Vec<i64>, _>>().unwrap()
    }

    #[test]
    fn a_resave_keeps_node_owned_rows_and_their_rowids() {
        let (_dir, connection) = store();
        crate::backlog::save_aggregate(&connection, &node(two_sessions())).unwrap();
        crate::backlog::decisions::record(
            &connection,
            &serde_json::json!({
                "ts": "2026-09-16T00:00:01Z",
                "type": "operator_decision",
                "data": {"decision_id": "d-one", "decision": "keep", "subject": "x-a"},
            }),
        )
        .unwrap();
        let sessions_before = column(
            &connection,
            "SELECT rowid FROM sessions WHERE node_id = 'x-a' ORDER BY seq",
        );
        crate::backlog::save_aggregate(&connection, &node(two_sessions())).unwrap();
        assert_eq!(
            column(
                &connection,
                "SELECT COUNT(*) FROM node_decisions WHERE node_id = 'x-a'"
            ),
            vec![1],
            "a re-save must not cascade away the ruling join row"
        );
        assert_eq!(
            column(
                &connection,
                "SELECT rowid FROM sessions WHERE node_id = 'x-a' ORDER BY seq"
            ),
            sessions_before
        );
    }

    #[test]
    fn a_note_stamp_that_is_not_utc_saves_and_keeps_its_bytes() {
        let (_dir, connection) = store();
        let notes = serde_json::json!([
            {"ts": "T1", "text": "a"},
            {"ts": "2026-09-11T00:00:00Z", "text": "b"},
        ]);
        let mut row = node(two_sessions()).to_json();
        row["progress_notes"] = notes.clone();
        crate::backlog::save_aggregate(&connection, &Node::from_json(&row).unwrap()).unwrap();
        let exported = crate::backlog::export_rows(&connection).unwrap();
        assert_eq!(exported[0]["progress_notes"].to_string(), notes.to_string());
        let stamped = column(
            &connection,
            "SELECT COUNT(created_at) FROM comments WHERE node_id = 'x-a'",
        );
        assert_eq!(stamped, vec![1], "only the UTC stamp fills the column");
    }

    #[test]
    fn a_status_the_pass_moves_counts_as_a_write() {
        let (_dir, connection) = store();
        let row = |id: &str, extra: Value| {
            let mut row = serde_json::json!({
                "id": id, "slug": id, "title": id, "type": "feature",
                "status": "ready", "priority": "p2",
            });
            row.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            Node::from_json(&row).unwrap()
        };
        crate::backlog::save_aggregate(&connection, &row("x-p", serde_json::json!({}))).unwrap();
        let child = serde_json::json!({"parent": "x-p", "completed_at": "2026-09-01T00:00:00Z"});
        crate::backlog::save_aggregate(&connection, &row("x-c", child)).unwrap();
        let version =
            |connection: &Connection| versions(connection, Some(&["x-p"])).unwrap()["x-p"];
        let before = version(&connection);
        recompute_status(&connection).unwrap();
        let status: String = connection
            .query_row("SELECT status FROM nodes WHERE id = 'x-p'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "done", "the parent rolls up from its done child");
        assert_eq!(version(&connection), before + 1);
        recompute_status(&connection).unwrap();
        assert_eq!(
            version(&connection),
            before + 1,
            "a pass that moves nothing writes nothing"
        );
    }

    #[test]
    fn a_shorter_list_deletes_only_the_tail() {
        let (_dir, connection) = store();
        crate::backlog::save_aggregate(&connection, &node(two_sessions())).unwrap();
        let first = column(
            &connection,
            "SELECT rowid FROM sessions WHERE node_id = 'x-a' AND seq = 0",
        );
        let one = serde_json::json!([{"phase": "do", "harness": "claude", "session_id": "s-1"}]);
        crate::backlog::save_aggregate(&connection, &node(one)).unwrap();
        assert_eq!(
            column(
                &connection,
                "SELECT rowid FROM sessions WHERE node_id = 'x-a'"
            ),
            first
        );
        let loaded = load(&connection, "x-a").unwrap().unwrap();
        assert_eq!(loaded.sessions.unwrap().len(), 1);
        assert_eq!(loaded.relations.blocked_by, Some(vec!["x-a".to_string()]));
    }
}
