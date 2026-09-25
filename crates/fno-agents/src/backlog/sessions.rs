//! The sessions aggregate: one node's sessions[] rows, owned here (ruling 4:
//! each aggregate's module owns its tables - no SQL against `sessions`
//! outside this file).

use super::model::SessionRecord;
use super::schema_v4::{iso, iso_sql, norm_sql, stamps, touch, NOW};
use rusqlite::{params, Connection};
use serde_json::Value;

/// Schema 4. The legacy `at` and `claimed_at` columns are gone: both held a
/// stamp time, the fact started_at carries (user ruling 2026-09-23).
pub fn ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS sessions (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  phase TEXT NOT NULL, harness TEXT NOT NULL REFERENCES harnesses(id),
  session_id TEXT NOT NULL REFERENCES agent_sessions(id),
  started_at TEXT, ended_at TEXT, ended_by TEXT, effort TEXT,
  observed_model TEXT, merge_grant TEXT, extras TEXT NOT NULL DEFAULT '{{}}'{},
  {},
  {},
  PRIMARY KEY (node_id, seq)
);
CREATE INDEX IF NOT EXISTS sessions_by_session ON sessions(session_id, harness);",
        stamps("sessions"),
        iso("sessions", "started_at"),
        iso("sessions", "ended_at"),
    )
}

pub fn triggers() -> String {
    touch("sessions")
}

pub fn ensure_table(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(&ddl())
        .map_err(|error| error.to_string())
}

/// Schema 3: the unknown item keys the typed model keeps in `extras` gain a
/// column. The schema-4 migration runs it first, so a schema-2 store still
/// carries its rows across.
pub(crate) fn migrate_add_extras(connection: &Connection) -> Result<(), String> {
    let has_extras: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'extras'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if has_extras > 0 {
        return Ok(());
    }
    if let Err(error) = connection
        .execute_batch("ALTER TABLE sessions ADD COLUMN extras TEXT NOT NULL DEFAULT '{}'")
    {
        // Two first opens can race the ALTER; losing it means the other
        // writer already added the column.
        if !error.to_string().contains("duplicate column name") {
            return Err(error.to_string());
        }
    }
    Ok(())
}

/// Schema-4 migration copy from `sessions_v3`. A legacy `at` or
/// `claimed_at` string that is UTC ISO-8601 folds into started_at when
/// started_at is empty (the 17 JSON-quoted `at` values unquote here); any
/// other legacy value moves into the row's extras, so nothing is lost.
/// Returns (values folded into started_at, values kept in extras).
pub(crate) fn copy_from_v3(connection: &Connection) -> Result<(i64, i64), String> {
    // A legacy column held JSON text (Value::to_string); a plain string
    // from an older writer reads as itself.
    let unquote = |column: &str| {
        format!(
            "CASE WHEN {column} IS NULL THEN NULL
                  WHEN json_valid({column}) THEN
                    CASE WHEN json_type({column}) = 'text' THEN json_extract({column}, '$') END
                  ELSE {column} END"
        )
    };
    let keep = |column: &str, plain: &str, into: &str| {
        format!(
            "CASE WHEN s.{column} IS NOT NULL AND NOT COALESCE({ok}, 0)
                  THEN json_set({into}, '$.{column}',
                       CASE WHEN json_valid(s.{column}) THEN json(s.{column}) ELSE s.{column} END)
                  ELSE {into} END",
            ok = iso_sql(plain)
        )
    };
    let fold = |plain: &str| format!("CASE WHEN {} THEN {plain} END", iso_sql(plain));
    let extras = keep("claimed_at", "s.c", &keep("at", "s.a", "s.extras"));
    let created = format!(
        "COALESCE({}, {}, {}, {NOW})",
        norm_sql("s.started_at"),
        norm_sql("s.a"),
        norm_sql("n.created_at")
    );
    let before: i64 = count(connection, "SELECT COUNT(*) FROM sessions_v3")?;
    let folded: i64 = count(
        connection,
        &format!(
            "SELECT COUNT(*) FROM (SELECT started_at, {a} AS a, {c} AS c FROM sessions_v3) s
             WHERE s.started_at IS NULL AND COALESCE({fa}, {fc}) IS NOT NULL",
            a = unquote("at"),
            c = unquote("claimed_at"),
            fa = fold("s.a"),
            fc = fold("s.c"),
        ),
    )?;
    let kept: i64 = count(
        connection,
        &format!(
            "SELECT COUNT(*) FROM (SELECT at, claimed_at, {a} AS a, {c} AS c FROM sessions_v3) s
             WHERE (s.at IS NOT NULL AND NOT COALESCE({oa}, 0))
                OR (s.claimed_at IS NOT NULL AND NOT COALESCE({oc}, 0))",
            a = unquote("at"),
            c = unquote("claimed_at"),
            oa = iso_sql("s.a"),
            oc = iso_sql("s.c"),
        ),
    )?;
    connection
        .execute_batch(&format!(
            "INSERT INTO sessions (rowid, node_id, seq, phase, harness, session_id, started_at,
                 ended_at, ended_by, effort, observed_model, merge_grant, extras,
                 created_at, updated_at)
             SELECT s.rowid, s.node_id, s.seq, s.phase, s.harness, s.session_id,
                    COALESCE(s.started_at, {fa}, {fc}), s.ended_at, s.ended_by, s.effort,
                    s.observed_model, s.merge_grant, {extras}, {created}, {created}
             FROM (SELECT rowid, *, {a} AS a, {c} AS c FROM sessions_v3) s
             LEFT JOIN nodes_v3 n ON n.id = s.node_id;",
            fa = fold("s.a"),
            fc = fold("s.c"),
            a = unquote("at"),
            c = unquote("claimed_at"),
        ))
        .map_err(|error| format!("schema v4 sessions copy: {error}"))?;
    let after: i64 = count(connection, "SELECT COUNT(*) FROM sessions")?;
    if after != before {
        return Err(format!(
            "schema v4 sessions copy: {before} rows in, {after} rows out"
        ));
    }
    Ok((folded, kept))
}

fn count(connection: &Connection, sql: &str) -> Result<i64, String> {
    connection
        .query_row(sql, [], |row| row.get(0))
        .map_err(|error| error.to_string())
}

/// Write one node's session rows: an upsert per list position that leaves an
/// unchanged row untouched, then a delete of the positions past the list's
/// end. The caller owns the transaction; a save touches only this node's
/// rows.
pub fn save(
    connection: &Connection,
    node_id: &str,
    sessions: &[SessionRecord],
) -> Result<(), String> {
    for (seq, session) in sessions.iter().enumerate() {
        let extras = serde_json::to_string(&session.extras).map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO sessions (node_id, seq, phase, harness, session_id, started_at,
                     ended_at, ended_by, effort, observed_model, merge_grant, extras)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(node_id, seq) DO UPDATE SET phase = excluded.phase,
                     harness = excluded.harness, session_id = excluded.session_id,
                     started_at = excluded.started_at, ended_at = excluded.ended_at,
                     ended_by = excluded.ended_by, effort = excluded.effort,
                     observed_model = excluded.observed_model,
                     merge_grant = excluded.merge_grant, extras = excluded.extras
                 WHERE (sessions.phase, sessions.harness, sessions.session_id,
                        sessions.started_at, sessions.ended_at, sessions.ended_by,
                        sessions.effort, sessions.observed_model, sessions.merge_grant,
                        sessions.extras)
                   IS NOT (excluded.phase, excluded.harness, excluded.session_id,
                           excluded.started_at, excluded.ended_at, excluded.ended_by,
                           excluded.effort, excluded.observed_model, excluded.merge_grant,
                           excluded.extras)",
                params![
                    node_id,
                    seq as i64,
                    session.phase,
                    session.harness,
                    session.session_id,
                    session.started_at,
                    session.ended_at,
                    session.ended_by,
                    session.effort.as_ref().map(Value::to_string),
                    session.observed_model.as_ref().map(Value::to_string),
                    session.merge_grant.as_ref().map(Value::to_string),
                    extras,
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    connection
        .execute(
            "DELETE FROM sessions WHERE node_id = ?1 AND seq >= ?2",
            params![node_id, sessions.len() as i64],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn delete(connection: &Connection, node_id: &str) -> Result<(), String> {
    connection
        .execute("DELETE FROM sessions WHERE node_id = ?1", params![node_id])
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// One node's sessions in list order (seq). The extras column round-trips
/// the item keys the typed model keeps as `extras`; an unparsable value
/// reads as empty.
pub fn load(connection: &Connection, node_id: &str) -> Result<Vec<SessionRecord>, String> {
    let mut statement = connection
        .prepare(
            "SELECT phase, harness, session_id, started_at, ended_at, ended_by, effort,
                    observed_model, merge_grant, extras
             FROM sessions WHERE node_id = ?1 ORDER BY seq",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![node_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, String>(9)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (
            phase,
            harness,
            session_id,
            started_at,
            ended_at,
            ended_by,
            effort,
            observed_model,
            merge_grant,
            extras_raw,
        ) = row.map_err(|error| error.to_string())?;
        let extras: serde_json::Map<String, Value> =
            serde_json::from_str(&extras_raw).unwrap_or_default();
        out.push(SessionRecord {
            phase,
            harness,
            session_id,
            started_at,
            ended_at,
            ended_by,
            effort: effort.and_then(|v| serde_json::from_str(&v).ok()),
            observed_model: observed_model.and_then(|v| serde_json::from_str(&v).ok()),
            merge_grant: merge_grant.and_then(|v| serde_json::from_str(&v).ok()),
            extras,
        });
    }
    Ok(out)
}

/// Fill ONE open window on a RAW entry's sessions rows: for callers whose
/// entry the typed model refused but that still owes its close. The caller
/// names the window it is settling; returns whether any row closed.
pub fn fill_open_window_raw(
    entry: &mut Value,
    session_id: &str,
    phase: Option<&str>,
    harness: Option<&str>,
    ended_at: &str,
    ended_by: &str,
) -> bool {
    let Some(list) = entry.get_mut("sessions").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut closed = false;
    for record in list.iter_mut() {
        let Some(obj) = record.as_object_mut() else {
            continue;
        };
        if obj.get("session_id").and_then(Value::as_str) == Some(session_id)
            && obj.get("ended_at").and_then(Value::as_str).is_none()
            && phase.map_or(true, |want| {
                obj.get("phase").and_then(Value::as_str) == Some(want)
            })
            && harness.map_or(true, |want| {
                obj.get("harness").and_then(Value::as_str) == Some(want)
            })
        {
            obj.insert("ended_at".into(), Value::String(ended_at.to_string()));
            obj.insert("ended_by".into(), Value::String(ended_by.to_string()));
            closed = true;
        }
    }
    closed
}
