//! Poll-observation coalescing for the runtime event store.
//!
//! Two declared poll types ([`OBSERVATION_HEARTBEATS`]) are heartbeat
//! coalesced at the store's insert paths: an identical healthy poll for a
//! subject inside its heartbeat window inserts no row. The pending
//! occurrence is counted in `event_observation_pending`, and when the
//! window closes (heartbeat expiry or a fingerprint change) a summary row
//! carries the pending occurrences with an explicit `occurrence_count`,
//! `window_started_ms`, and `window_finished_ms`. Blocks, malformed
//! observations, undeclared types, and every always-audit shape still
//! insert one ordinary row each.

use rusqlite::{params, Connection, Transaction};
use sha2::{Digest, Sha256};

use super::{hex, insert_v2_row, RowInput};

/// The event types the poll-observation policy covers, with each subject's
/// heartbeat window in milliseconds. The policy is declared-types-only: a
/// `guard_decision` block, a missing or unknown decision, a malformed
/// payload, and every undeclared type always insert ordinary rows.
pub const OBSERVATION_HEARTBEATS: &[(&str, i64)] = &[
    ("guard_decision", 5 * 60_000),
    ("advance_skipped", 30 * 60_000),
];

/// Subject key separator. The ASCII unit separator cannot appear in the
/// payload strings the subjects are built from, so the join is unambiguous.
const SUBJECT_SEP: char = '\u{1f}';

/// One declared poll observation's identity: the subject it polls, the
/// fingerprint that must stay identical for suppression, and the heartbeat
/// window that bounds the suppression.
struct ObservationPolicy {
    subject: String,
    fingerprint: String,
    heartbeat_ms: i64,
}

/// The declared policy for one envelope's `data`, or `None` when the
/// observation must always emit an ordinary row: an undeclared type, a
/// `guard_decision` that is not a healthy allow poll (each block is a
/// separate attempted action), or a payload missing the fields the subject
/// is built from.
fn observation_policy(ty: &str, data: &serde_json::Value) -> Option<ObservationPolicy> {
    let heartbeat_ms = OBSERVATION_HEARTBEATS
        .iter()
        .find(|(t, _)| *t == ty)
        .map(|(_, h)| *h)?;
    let get = |key: &str| -> String {
        data.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
            .to_string()
    };
    match ty {
        "guard_decision" => {
            if data.get("decision").and_then(|v| v.as_str()) != Some("allow") {
                return None;
            }
            let guard = get("guard");
            let tool = get("tool");
            if guard.is_empty() || tool.is_empty() {
                return None;
            }
            // The subject is the guard alone: a tool change is a fingerprint
            // change within the same subject, so the pending window flushes
            // and the new tool's first poll emits its own transition row.
            // The full payload (tool included) is the fingerprint.
            Some(ObservationPolicy {
                subject: guard,
                fingerprint: canonical_json(data),
                heartbeat_ms,
            })
        }
        "advance_skipped" => {
            let subject = format!(
                "{}{SUBJECT_SEP}{}{SUBJECT_SEP}{}{SUBJECT_SEP}{}",
                get("rank"),
                get("closed_node_id"),
                get("node_id"),
                get("mission"),
            );
            if subject.trim_matches(SUBJECT_SEP).is_empty() {
                return None;
            }
            // The fingerprint carries every field that distinguishes one
            // skip's meaning from another's; the subject carries the fields
            // that name what was polled.
            let mut fp = serde_json::Map::new();
            for key in ["reason", "provider", "retry_at", "exit_code", "detail"] {
                if let Some(v) = data.get(key).filter(|v| !v.is_null()) {
                    fp.insert(key.to_string(), v.clone());
                }
            }
            Some(ObservationPolicy {
                subject,
                fingerprint: canonical_json(&serde_json::Value::Object(fp)),
                heartbeat_ms,
            })
        }
        _ => None,
    }
}

/// Key-sorted canonical form of a JSON value, so the same payload spelled
/// with different key order fingerprints identically.
fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{k:?}:{}", canonical_json(&map[*k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// Create the poll-observation bookkeeping tables when missing. Writers
/// only: readers open through `open_read`, which never creates anything.
/// The tables are additive metadata beside `events`, so no schema-version
/// bump is needed - a v2 reader that predates coalescing ignores them.
pub(super) fn ensure_observation_tables(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS event_observation_state (
             obs_scope TEXT NOT NULL,
             obs_type TEXT NOT NULL,
             subject TEXT NOT NULL,
             fingerprint TEXT NOT NULL,
             window_started_ms INTEGER NOT NULL,
             window_finished_ms INTEGER NOT NULL,
             PRIMARY KEY (obs_scope, obs_type, subject)
         );
         CREATE TABLE IF NOT EXISTS event_observation_pending (
             obs_scope TEXT NOT NULL,
             obs_type TEXT NOT NULL,
             subject TEXT NOT NULL,
             row_hash BLOB NOT NULL,
             ts_ms INTEGER NOT NULL,
             line TEXT NOT NULL,
             PRIMARY KEY (obs_scope, obs_type, subject, row_hash)
         );",
    )
    .map_err(|e| format!("observation tables: {e}"))
}

/// The decision one poll-observation line produced inside the caller's open
/// transaction: insert it as a row, or skip it because its occurrence is
/// already represented by a stored row or a pending count.
pub(super) enum ObservationGate {
    Insert,
    Suppressed { pending: i64 },
}

/// The poll-observation gate for one line about to be inserted. Declared
/// poll types consult the observation state; everything else always
/// inserts. Runs inside the caller's `BEGIN IMMEDIATE` transaction, so two
/// concurrent identical first polls serialize: the second reads the first's
/// state and is counted pending instead of inserting a second transition
/// row.
pub(super) fn observation_gate(
    tx: &Transaction,
    ty: &str,
    scope: Option<&str>,
    data: &serde_json::Value,
    line: &str,
    row_hash: &[u8],
    ts_ms: i64,
) -> Result<ObservationGate, String> {
    let Some(policy) = observation_policy(ty, data) else {
        return Ok(ObservationGate::Insert);
    };
    let scope_key = scope.unwrap_or("");
    // A stored row with this hash means an earlier import already
    // represented the line (a gc rewrite or a lost cursor); it must not
    // count twice.
    let already_stored: i64 = tx
        .query_row(
            "SELECT 1 FROM events WHERE row_hash = ?1",
            params![row_hash],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if already_stored == 1 {
        return Ok(ObservationGate::Suppressed {
            pending: pending_count(tx, scope_key, ty, &policy.subject),
        });
    }
    // An already-counted pending hash means this exact line was suppressed
    // by an earlier import that lost its cursor before committing the file
    // range; counting it again would inflate the window total.
    let already_pending: i64 = tx
        .query_row(
            "SELECT 1 FROM event_observation_pending WHERE
             obs_scope = ?1 AND obs_type = ?2 AND subject = ?3 AND row_hash = ?4",
            params![scope_key, ty, policy.subject, row_hash],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if already_pending == 1 {
        return Ok(ObservationGate::Suppressed {
            pending: pending_count(tx, scope_key, ty, &policy.subject),
        });
    }
    match tx
        .query_row(
            "SELECT fingerprint, window_started_ms FROM event_observation_state
             WHERE obs_scope = ?1 AND obs_type = ?2 AND subject = ?3",
            params![scope_key, ty, policy.subject],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .ok()
    {
        // First observation of the subject: it is the transition row, and
        // it opens the window.
        None => {
            tx.execute(
                "INSERT INTO event_observation_state
                 (obs_scope, obs_type, subject, fingerprint, window_started_ms, window_finished_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![scope_key, ty, policy.subject, policy.fingerprint, ts_ms],
            )
            .map_err(|e| e.to_string())?;
            Ok(ObservationGate::Insert)
        }
        Some((fingerprint, window_started)) => {
            if fingerprint == policy.fingerprint
                && ts_ms <= window_started.saturating_add(policy.heartbeat_ms)
            {
                tx.execute(
                    "INSERT INTO event_observation_pending
                     (obs_scope, obs_type, subject, row_hash, ts_ms, line)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![scope_key, ty, policy.subject, row_hash, ts_ms, line],
                )
                .map_err(|e| e.to_string())?;
                tx.execute(
                    "UPDATE event_observation_state SET window_finished_ms = ?4
                     WHERE obs_scope = ?1 AND obs_type = ?2 AND subject = ?3",
                    params![scope_key, ty, policy.subject, ts_ms],
                )
                .map_err(|e| e.to_string())?;
                Ok(ObservationGate::Suppressed {
                    pending: pending_count(tx, scope_key, ty, &policy.subject),
                })
            } else {
                // Heartbeat expiry or a fingerprint change: the pending
                // occurrences flush as one summary row, then this
                // observation opens the next window as a transition row.
                flush_pending_window(tx, scope_key, ty, &policy.subject)?;
                tx.execute(
                    "UPDATE event_observation_state
                     SET fingerprint = ?4, window_started_ms = ?5, window_finished_ms = ?5
                     WHERE obs_scope = ?1 AND obs_type = ?2 AND subject = ?3",
                    params![scope_key, ty, policy.subject, policy.fingerprint, ts_ms],
                )
                .map_err(|e| e.to_string())?;
                Ok(ObservationGate::Insert)
            }
        }
    }
}

fn pending_count(tx: &Transaction, scope_key: &str, ty: &str, subject: &str) -> i64 {
    tx.query_row(
        "SELECT count(*) FROM event_observation_pending
         WHERE obs_scope = ?1 AND obs_type = ?2 AND subject = ?3",
        params![scope_key, ty, subject],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Insert one summary row for a closed observation window and clear its
/// pending occurrences. The summary carries the last pending observation's
/// payload with an explicit `occurrence_count`, so a reader summing
/// `occurrence_count` (default 1 when absent) recovers the represented
/// invocation total exactly. The summary precedes the transition row that
/// triggered the flush, which the caller inserts after this returns.
fn flush_pending_window(
    tx: &Transaction,
    scope_key: &str,
    ty: &str,
    subject: &str,
) -> Result<(), String> {
    let (window_started, window_finished, last_ts_ms, last_line): (i64, i64, i64, String) = tx
        .query_row(
            "SELECT s.window_started_ms, s.window_finished_ms, p.ts_ms, p.line
             FROM event_observation_state s
             JOIN event_observation_pending p
               ON p.obs_scope = s.obs_scope AND p.obs_type = s.obs_type
              AND p.subject = s.subject
             WHERE s.obs_scope = ?1 AND s.obs_type = ?2 AND s.subject = ?3
             ORDER BY p.ts_ms DESC LIMIT 1",
            params![scope_key, ty, subject],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|e| e.to_string())?;
    let count = pending_count(tx, scope_key, ty, subject);
    let mut value: serde_json::Value = serde_json::from_str(&last_line)
        .map_err(|e| format!("pending observation line is not JSON: {e}"))?;
    {
        let obj = value
            .as_object_mut()
            .ok_or_else(|| "pending observation line is not an object".to_string())?;
        if let Some(data) = obj.get_mut("data").and_then(|d| d.as_object_mut()) {
            data.insert("occurrence_count".into(), count.into());
            data.insert("window_started_ms".into(), window_started.into());
            data.insert("window_finished_ms".into(), window_finished.into());
        }
    }
    let summary_line = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    let digest = Sha256::digest(summary_line.as_bytes());
    insert_v2_row(
        tx,
        "events",
        &RowInput {
            event_id: format!("evt:{}", hex(&digest)),
            row_hash: digest.to_vec(),
            ts_ms: last_ts_ms,
            type_: ty.to_string(),
            source: value
                .get("source")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            scope: if scope_key.is_empty() {
                None
            } else {
                Some(scope_key.to_string())
            },
            reject_reason: None,
            line: summary_line,
        },
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM event_observation_pending
         WHERE obs_scope = ?1 AND obs_type = ?2 AND subject = ?3",
        params![scope_key, ty, subject],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
