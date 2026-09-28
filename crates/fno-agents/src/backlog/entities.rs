//! The entity tables: harnesses, models and agent_sessions, owned here
//! (ruling 4: no SQL against these outside this file). Every harness, model
//! and session id column in the store REFERENCES one of them. The parent
//! rows are made by triggers on the referencing tables, so every writer
//! (including an older binary still running during a rollout) gets a valid
//! parent with no code of its own, and the foreign key can never refuse an
//! id the store has not seen.

use super::schema_v4::{stamps, touch};
use rusqlite::Connection;

const TABLES: [&str; 3] = ["harnesses", "models", "agent_sessions"];

pub fn ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS harnesses (
           id TEXT PRIMARY KEY CONSTRAINT harnesses_id_nonempty CHECK (id <> ''){}
         );
         CREATE TABLE IF NOT EXISTS models (
           id TEXT PRIMARY KEY CONSTRAINT models_id_nonempty CHECK (id <> ''){}
         );
         CREATE TABLE IF NOT EXISTS agent_sessions (
           id TEXT PRIMARY KEY CONSTRAINT agent_sessions_id_nonempty CHECK (id <> ''),
           harness_id TEXT REFERENCES harnesses(id){}
         );",
        stamps("harnesses"),
        stamps("models"),
        stamps("agent_sessions"),
    )
}

/// The statements one referencing row runs: a parent row for each harness,
/// model and session id it names. A session id first seen with no harness
/// takes the first harness a later row names; a conflicting harness never
/// overwrites it.
fn parent_inserts(
    harnesses: &[&str],
    models: &[&str],
    sessions: &[(&str, Option<&str>)],
) -> String {
    let mut body = String::new();
    for column in harnesses {
        body.push_str(&format!(
            "INSERT INTO harnesses(id) SELECT NEW.{column} WHERE NEW.{column} IS NOT NULL
               ON CONFLICT(id) DO NOTHING;\n"
        ));
    }
    for column in models {
        body.push_str(&format!(
            "INSERT INTO models(id) SELECT NEW.{column} WHERE NEW.{column} IS NOT NULL
               ON CONFLICT(id) DO NOTHING;\n"
        ));
    }
    for (column, harness) in sessions {
        let harness = harness.map_or("NULL".to_string(), |h| format!("NEW.{h}"));
        body.push_str(&format!(
            "INSERT INTO agent_sessions(id, harness_id) SELECT NEW.{column}, {harness}
               WHERE NEW.{column} IS NOT NULL
               ON CONFLICT(id) DO UPDATE SET harness_id = excluded.harness_id
               WHERE agent_sessions.harness_id IS NULL AND excluded.harness_id IS NOT NULL;\n"
        ));
    }
    body
}

/// (table, harness columns, model columns, (session column, its harness
/// column)) for every table that names an entity.
type Referrer = (
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
    &'static [(&'static str, Option<&'static str>)],
);

const REFERRERS: &[Referrer] = &[
    ("nodes", &[], &[], &[("session_id", None)]),
    (
        "node_claims",
        &["harness"],
        &[],
        &[("harness_session", Some("harness"))],
    ),
    ("node_dispatch", &[], &["model"], &[]),
    (
        "node_provenance",
        &["source_harness", "spawned_by_harness"],
        &[],
        &[
            ("source_session_id", Some("source_harness")),
            ("spawned_by_session", Some("spawned_by_harness")),
            ("think_session_id", None),
        ],
    ),
    (
        "sessions",
        &["harness"],
        &[],
        &[("session_id", Some("harness"))],
    ),
    (
        "comments",
        &["source_harness"],
        &[],
        &[("source_session_id", Some("source_harness"))],
    ),
    (
        "encounters",
        &["harness"],
        &["model"],
        &[("session_id", Some("harness"))],
    ),
    ("node_costs", &[], &[], &[("session_id", None)]),
    (
        "findings",
        &["source_harness"],
        &[],
        &[
            ("source_session_id", Some("source_harness")),
            ("resolved_by_session_id", None),
        ],
    ),
];

/// The entity triggers (a BEFORE INSERT and a BEFORE UPDATE OF pair per
/// referencing table) plus the entity tables' own updated_at triggers.
pub fn triggers() -> String {
    let mut out = String::new();
    for table in TABLES {
        out.push_str(&touch(table));
    }
    for (table, harnesses, models, sessions) in REFERRERS {
        let body = parent_inserts(harnesses, models, sessions);
        let mut columns: Vec<&str> = harnesses.iter().chain(models.iter()).copied().collect();
        columns.extend(sessions.iter().map(|(column, _)| *column));
        out.push_str(&format!(
            "CREATE TRIGGER IF NOT EXISTS {table}_entities_bi BEFORE INSERT ON {table} BEGIN
             {body}END;
             CREATE TRIGGER IF NOT EXISTS {table}_entities_bu BEFORE UPDATE OF {} ON {table} BEGIN
             {body}END;\n",
            columns.join(", ")
        ));
    }
    out
}

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(&ddl())
        .map_err(|error| error.to_string())
}

/// Every table that names an entity must exist before its triggers can.
pub fn ensure_triggers(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(&triggers())
        .map_err(|error| error.to_string())
}

/// Schema-4 migration backfill from the renamed schema-3 tables: one
/// parent row per id any referencing column names. A row's created_at is
/// the earliest time the store saw that id, else the migration time. A
/// session's harness is the one on its earliest row that names one.
/// Returns (entity rows made, legacy session ids with no harness, session
/// ids seen with more than one harness).
pub fn backfill_from_v3(connection: &Connection) -> Result<(i64, i64, i64), String> {
    let at = |column: &str| format!("strftime('%Y-%m-%dT%H:%M:%fZ', {column})");
    // A schema-3 store made before findings existed has no findings_v3.
    let findings = if super::schema_v4::table_exists(connection, "findings_v3")? {
        format!(
            "UNION ALL SELECT source_session_id, source_harness, NULL, {created} FROM findings_v3
             UNION ALL SELECT resolved_by_session_id, NULL, NULL, {resolved} FROM findings_v3",
            created = at("created_at"),
            resolved = at("resolved_at"),
        )
    } else {
        String::new()
    };
    let refs = format!(
        "CREATE TEMP TABLE v4_entity_refs AS
         SELECT session_id AS sid, harness, NULL AS model,
                COALESCE({started}, {unquoted_at}) AS ts FROM sessions_v3
         UNION ALL SELECT harness_session, harness, NULL, {locked} FROM node_claims_v3
         UNION ALL SELECT source_session_id, source_harness, NULL, {comment} FROM comments_v3
         UNION ALL SELECT session_id, harness, model, {encounter} FROM encounters_v3
         UNION ALL SELECT source_session_id, source_harness, NULL, NULL FROM node_provenance_v3
         UNION ALL SELECT spawned_by_session, spawned_by_harness, NULL, NULL
                   FROM node_provenance_v3
         UNION ALL SELECT think_session_id, NULL, NULL, NULL FROM node_provenance_v3
         UNION ALL SELECT NULL, NULL, model, NULL FROM node_dispatch_v3
         {findings}
         UNION ALL SELECT session_id, NULL, NULL, {node} FROM nodes_v3
         UNION ALL SELECT json_extract(j.value, '$.session_id'), NULL, NULL,
                          {cost} FROM nodes_v3, json_each(nodes_v3.extras, '$.cost_sessions') j
                   WHERE json_type(nodes_v3.extras, '$.cost_sessions') = 'array'
                     AND json_type(j.value, '$.session_id') = 'text';",
        started = at("started_at"),
        unquoted_at = at(
            "CASE WHEN json_valid(at) AND json_type(at) = 'text' THEN json_extract(at, '$') END"
        ),
        locked = at("locked_at"),
        comment = at("created_at"),
        encounter = at("ts"),
        node = at("created_at"),
        cost = at("json_extract(j.value, '$.timestamp')"),
    );
    let now = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";
    let before: i64 = count_all(connection)?;
    connection
        .execute_batch(&format!(
            "{refs}
             CREATE INDEX temp.v4_entity_refs_sid ON v4_entity_refs(sid, ts);
             INSERT INTO harnesses(id, created_at, updated_at)
               SELECT harness, COALESCE(MIN(ts), {now}), COALESCE(MIN(ts), {now})
               FROM v4_entity_refs WHERE harness IS NOT NULL GROUP BY harness
               ON CONFLICT(id) DO NOTHING;
             INSERT INTO models(id, created_at, updated_at)
               SELECT model, COALESCE(MIN(ts), {now}), COALESCE(MIN(ts), {now})
               FROM v4_entity_refs WHERE model IS NOT NULL GROUP BY model
               ON CONFLICT(id) DO NOTHING;
             INSERT INTO agent_sessions(id, harness_id, created_at, updated_at)
               SELECT sid,
                      (SELECT r.harness FROM v4_entity_refs r
                       WHERE r.sid = v4_entity_refs.sid AND r.harness IS NOT NULL
                       ORDER BY r.ts IS NULL, r.ts LIMIT 1),
                      COALESCE(MIN(ts), {now}), COALESCE(MIN(ts), {now})
               FROM v4_entity_refs WHERE sid IS NOT NULL GROUP BY sid
               ON CONFLICT(id) DO NOTHING;"
        ))
        .map_err(|error| format!("schema v4 entity backfill: {error}"))?;
    let made = count_all(connection)? - before;
    let legacy: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM agent_sessions WHERE harness_id IS NULL",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let conflicts: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM (SELECT sid FROM v4_entity_refs
               WHERE sid IS NOT NULL AND harness IS NOT NULL
               GROUP BY sid HAVING COUNT(DISTINCT harness) > 1)",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute_batch("DROP TABLE v4_entity_refs;")
        .map_err(|error| error.to_string())?;
    Ok((made, legacy, conflicts))
}

fn count_all(connection: &Connection) -> Result<i64, String> {
    connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM harnesses) + (SELECT COUNT(*) FROM models)
                  + (SELECT COUNT(*) FROM agent_sessions)",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use crate::backlog::model::Node;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn store() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, b"{\"entries\": []}").unwrap();
        let connection = crate::backlog::open(&graph).unwrap();
        (dir, connection)
    }

    fn save(connection: &Connection, node: serde_json::Value) {
        let node = Node::from_json(&node).unwrap();
        crate::backlog::save_aggregate(connection, &node).unwrap();
    }

    fn base(extra: serde_json::Value) -> serde_json::Value {
        let mut node = serde_json::json!({
            "id": "x-a", "slug": "a", "title": "A", "type": "feature",
            "status": "ready", "priority": "p2", "domain": "code",
            "created_at": "2026-09-11T00:00:00+00:00",
        });
        for (key, value) in extra.as_object().unwrap() {
            node[key] = value.clone();
        }
        node
    }

    fn harness_of(connection: &Connection, session: &str) -> Option<String> {
        connection
            .query_row(
                "SELECT harness_id FROM agent_sessions WHERE id = ?1",
                [session],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn a_saved_session_makes_its_harness_and_session_rows() {
        let (_dir, connection) = store();
        save(
            &connection,
            base(serde_json::json!({"sessions": [
                {"phase": "do", "harness": "codex", "session_id": "abc"}
            ]})),
        );
        let harnesses: Vec<String> = connection
            .prepare("SELECT id FROM harnesses")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(harnesses, vec!["codex".to_string()]);
        assert_eq!(harness_of(&connection, "abc"), Some("codex".into()));
    }

    #[test]
    fn the_first_named_harness_wins_a_session_id() {
        let (_dir, connection) = store();
        save(
            &connection,
            base(serde_json::json!({"progress_notes": [
                {"ts": "2026-09-11T00:00:00+00:00", "text": "n", "source_session_id": "abc"}
            ]})),
        );
        assert_eq!(harness_of(&connection, "abc"), None);
        save(
            &connection,
            base(serde_json::json!({
                "progress_notes": [
                    {"ts": "2026-09-11T00:00:00+00:00", "text": "n", "source_session_id": "abc"}
                ],
                "sessions": [{"phase": "do", "harness": "claude", "session_id": "abc"}],
            })),
        );
        assert_eq!(harness_of(&connection, "abc"), Some("claude".into()));
        save(
            &connection,
            base(serde_json::json!({
                "sessions": [
                    {"phase": "do", "harness": "claude", "session_id": "abc"},
                    {"phase": "review", "harness": "codex", "session_id": "abc"}
                ],
            })),
        );
        assert_eq!(harness_of(&connection, "abc"), Some("claude".into()));
    }

    #[test]
    fn an_empty_harness_is_refused_by_name() {
        let (_dir, connection) = store();
        let node = Node::from_json(&base(serde_json::json!({"sessions": [
            {"phase": "do", "harness": "", "session_id": "abc"}
        ]})))
        .unwrap();
        let error = crate::backlog::save_aggregate(&connection, &node).unwrap_err();
        assert!(error.contains("harnesses_id_nonempty"), "{error}");
    }
}
