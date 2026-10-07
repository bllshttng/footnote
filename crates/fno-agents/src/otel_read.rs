//! Shared read-only telemetry queries used by doctor and the lead check-in.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

pub fn schema_sql() -> &'static str {
    include_str!("otel_schema.sql")
}

fn open(db: &Path) -> Result<Option<Connection>, String> {
    match std::fs::metadata(db) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("telemetry database unavailable: {error}")),
        Ok(_) => {}
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("telemetry database unreadable: {error}"))?;
    conn.busy_timeout(Duration::from_secs(1))
        .map_err(|error| error.to_string())?;
    Ok(Some(conn))
}

pub fn session_cost_usd(db: &Path, session: &str) -> Option<f64> {
    let conn = open(db).ok()??;
    conn.query_row(
        "SELECT SUM(cost_usd_micros) FROM api_requests WHERE session_id=?1 AND cost_usd_micros IS NOT NULL",
        [session], |row| row.get::<_, Option<i64>>(0),
    ).ok()?.map(|micros| micros as f64 / 1_000_000.0)
}

fn workers(registry: &Path, now: &DateTime<Utc>) -> Result<(BTreeSet<String>, usize), String> {
    let bytes = match std::fs::read(registry) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((BTreeSet::new(), 0))
        }
        Err(error) => return Err(format!("worker registry unreadable: {error}")),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("worker registry invalid: {error}"))?;
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "worker registry has no entries array".to_string())?;
    let mut live = BTreeSet::new();
    let mut unknown = 0;
    for entry in entries {
        if entry.get("harness").and_then(Value::as_str) != Some("claude") {
            continue;
        }
        let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
        if matches!(
            status,
            "exited" | "failed" | "permanent_dead" | "permanent-dead" | "orphaned"
        ) {
            continue;
        }
        let stamp = entry
            .get("liveness_measured_at")
            .and_then(Value::as_str)
            .and_then(|stamp| DateTime::parse_from_rfc3339(stamp).ok())
            .and_then(|stamp| u64::try_from(stamp.timestamp()).ok());
        let word = crate::served_liveness::served_liveness_word(
            entry.get("liveness").and_then(Value::as_str),
            stamp,
            u64::try_from(now.timestamp()).unwrap_or(0),
        );
        match word {
            Some("alive") => match entry
                .get("harness_session_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(session) => {
                    live.insert(session.to_string());
                }
                None => unknown += 1,
            },
            Some("dead") => {}
            _ => unknown += 1,
        }
    }
    Ok((live, unknown))
}

pub fn health(home: &Path, enabled: bool) -> Result<Value, String> {
    health_at(home, enabled, Utc::now())
}

fn health_at(home: &Path, enabled: bool, now: DateTime<Utc>) -> Result<Value, String> {
    let observed = now.to_rfc3339();
    if !enabled {
        return Ok(json!({"status":"off", "observed_at":observed, "window_seconds":3600}));
    }
    let (live, unknown) = workers(&home.join("registry.json"), &now)?;
    let dir = home.join("otel");
    let port = std::fs::read_to_string(dir.join("port"))
        .ok()
        .and_then(|text| text.trim().parse::<u16>().ok())
        .filter(|port| *port != 0);
    let listening = port.is_some_and(|port| {
        std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(100),
        )
        .is_ok()
    });
    let db = open(&dir.join("otel.db"))?;
    let mut rows = 0i64;
    let mut total = 0i64;
    let mut latest: Option<String> = None;
    let mut events: Option<i64> = None;
    let mut covered = BTreeSet::new();
    let since = (now - chrono::Duration::hours(1)).to_rfc3339();
    if let Some(conn) = &db {
        (total, rows, latest) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(julianday(ts)>=julianday(?1) AND julianday(ts)<=julianday(?2)),0), MAX(ts) FROM api_requests",
            [&since, &observed], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).map_err(|error| format!("request cost schema unreadable: {error}"))?;
        let mut stmt = conn.prepare("SELECT DISTINCT session_id FROM api_requests WHERE julianday(ts)>=julianday(?1) AND julianday(ts)<=julianday(?2)")
            .map_err(|error| error.to_string())?;
        let sessions = stmt
            .query_map([&since, &observed], |r| r.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        for session in sessions {
            covered.insert(session.map_err(|error| error.to_string())?);
        }
        let raw_exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='otel_events')", [], |r| r.get(0))
            .map_err(|error| error.to_string())?;
        if raw_exists {
            events = Some(conn.query_row("SELECT COUNT(*) FROM otel_events WHERE julianday(ts)>=julianday(?1) AND julianday(ts)<=julianday(?2)", [&since, &observed], |r| r.get(0))
                .map_err(|error| error.to_string())?);
        }
    }
    let uncovered: Vec<_> = live.difference(&covered).cloned().collect();
    let status = if db.is_none() {
        "missing"
    } else if !listening {
        "offline"
    } else if unknown > 0 {
        "unknown_workers"
    } else if rows == 0 && !live.is_empty() {
        "zero_rows"
    } else if !uncovered.is_empty() {
        "uncovered_workers"
    } else if live.is_empty() {
        "idle"
    } else {
        "healthy"
    };
    Ok(
        json!({"status":status, "observed_at":observed, "window_seconds":3600,
        "database":if db.is_some(){"present"}else{"missing"}, "rows_last_hour":rows,
        "api_requests_total":total, "events_last_hour":events, "latest_request_at":latest,
        "live_claude_workers":live.len(), "unknown_claude_workers":unknown,
        "uncovered_sessions":uncovered, "receiver_port":port, "receiver_listening":listening}),
    )
}

pub fn health_line(value: &Value) -> String {
    if value.get("status").and_then(Value::as_str) == Some("off") {
        return "telemetry: off".to_string();
    }
    let rows = value
        .get("rows_last_hour")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let live = value
        .get("live_claude_workers")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let unknown = value
        .get("unknown_claude_workers")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let uncovered = value
        .get("uncovered_sessions")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let recovery = if matches!(status, "missing" | "zero_rows" | "uncovered_workers") {
        "; check the receiver and restart an uninstrumented supervisor after its hosted sessions finish"
    } else {
        ""
    };
    format!("telemetry: {rows} API rows in the last hour, {live} live Claude workers, {uncovered} uncovered, {unknown} unknown ({status}){recovery}")
}

fn csv_cell(value: &str) -> String {
    if value.chars().any(|c| matches!(c, ',' | '"' | '\r' | '\n')) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn micros_text(micros: i64) -> String {
    let absolute = micros.unsigned_abs();
    format!(
        "{}{}.{:06}",
        if micros < 0 { "-" } else { "" },
        absolute / 1_000_000,
        absolute % 1_000_000
    )
}

pub fn export_csv(db: &Path) -> Result<(String, usize), String> {
    let mut output = "day,session_id,model,skill_name,requests,unpriced_requests,cost_usd,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens\n".to_string();
    let Some(conn) = open(db)? else {
        return Ok((output, 0));
    };
    let mut stmt = conn.prepare("SELECT COALESCE(date(ts),''),session_id,COALESCE(model,''),COALESCE(skill_name,''),COUNT(*),COUNT(*)-COUNT(cost_usd_micros),SUM(cost_usd_micros),SUM(input_tokens),SUM(output_tokens),SUM(cache_read_tokens),SUM(cache_creation_tokens) FROM api_requests GROUP BY 1,2,3,4 ORDER BY 1,2,3,4")
        .map_err(|error| format!("cannot export request costs: {error}"))?;
    let rows = stmt
        .query_map([], |r| {
            let mut fields: Vec<String> = (0..4)
                .map(|i| r.get::<_, String>(i))
                .collect::<rusqlite::Result<_>>()?;
            fields.push(r.get::<_, i64>(4)?.to_string());
            fields.push(r.get::<_, i64>(5)?.to_string());
            fields.push(
                r.get::<_, Option<i64>>(6)?
                    .map(micros_text)
                    .unwrap_or_default(),
            );
            for i in 7..11 {
                fields.push(
                    r.get::<_, Option<i64>>(i)?
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                );
            }
            Ok(fields)
        })
        .map_err(|error| error.to_string())?;
    let mut count = 0;
    for row in rows {
        let row = row.map_err(|error| error.to_string())?;
        output.push_str(
            &row.iter()
                .map(|s| csv_cell(s))
                .collect::<Vec<_>>()
                .join(","),
        );
        output.push('\n');
        count += 1;
    }
    Ok((output, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_preserves_grouping_unknown_prices_and_csv_text() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("otel.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(schema_sql()).unwrap();
        for (key, cost) in [
            ("one", Some(1_250_000i64)),
            ("two", Some(750_000)),
            ("three", None),
        ] {
            conn.execute("INSERT INTO api_requests (dedupe_key,session_id,ts,model,skill_name,cost_usd_micros) VALUES (?1,'session','2026-10-06T23:00:00-02:00','model',?2,?3)", rusqlite::params![key,"skill,\"quoted\"",cost]).unwrap();
        }
        conn.execute("INSERT INTO api_requests (dedupe_key,session_id,ts,model) VALUES ('unknown','other','2026-10-06T00:00:00Z','unknown')", []).unwrap();
        let (csv, groups) = export_csv(&db).unwrap();
        assert_eq!(groups, 2);
        assert!(csv.contains("2026-10-07,session,model,\"skill,\"\"quoted\"\"\",3,1,2.000000"));
        assert!(csv.contains("2026-10-06,other,unknown,,1,1,,,,,"));
        assert_eq!(session_cost_usd(&db, "session"), Some(2.0));
        assert_eq!(session_cost_usd(&db, "other"), None);
        assert_eq!(export_csv(&dir.path().join("missing")).unwrap().1, 0);
        std::fs::write(dir.path().join("broken"), b"not sqlite").unwrap();
        assert!(export_csv(&dir.path().join("broken")).is_err());
    }

    #[test]
    fn missing_and_stale_worker_coverage_are_not_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let now = DateTime::parse_from_rfc3339("2026-10-06T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        std::fs::write(home.join("registry.json"), json!({"entries":[
            {"harness":"claude","harness_session_id":"live","status":"working","liveness":"alive","liveness_measured_at":"2026-10-06T19:59:30Z"},
            {"harness":"claude","harness_session_id":"stale","status":"working","liveness":"alive","liveness_measured_at":"2026-10-06T19:00:00Z"}
        ]}).to_string()).unwrap();
        let missing = health_at(home, true, now).unwrap();
        assert_eq!(missing["rows_last_hour"], 0);
        assert_eq!(missing["live_claude_workers"], 1);
        assert_eq!(missing["unknown_claude_workers"], 1);
        assert_eq!(missing["uncovered_sessions"], json!(["live"]));
        assert_ne!(missing["status"], "healthy");
        assert!(health_line(&missing).contains("0 API rows"));
        assert_eq!(health_at(home, false, now).unwrap()["status"], "off");
        std::fs::write(home.join("registry.json"), "broken").unwrap();
        assert!(health_at(home, true, now).is_err());
    }
}
