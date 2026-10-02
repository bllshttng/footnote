//! Localhost OTLP/http-json receiver for exact per-session cost.
//!
//! Claude Code exports an `api_request` log record per API call with
//! `cost_usd_micros`, `session.id` and skill/plugin attribution when
//! `CLAUDE_CODE_ENABLE_TELEMETRY=1` and an OTLP logs exporter points at a
//! collector. fno births every hosted supervisor, so one listener on
//! 127.0.0.1 that the daemon owns turns that on for the whole fleet: the
//! receiver keeps only the named columns in `<agents home>/otel/otel.db`
//! (tool details carry command text and are dropped) and nothing leaves the
//! machine. The table and columns are harness-neutral: any OTLP speaker can
//! feed the same port.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use serde_json::Value;

use crate::paths::AgentsHome;

/// Exporter bodies are bounded; a bigger POST is refused unread.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// Bind 127.0.0.1:0, publish the port at `<home>/otel/port`, serve
/// `POST /v1/logs` until `shutdown`, then remove the port file so a
/// supervisor born later never reads a dead port.
pub async fn run(home: AgentsHome, shutdown: Arc<AtomicBool>) {
    let dir = home.otel_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await else {
        return;
    };
    let Some(port) = listener
        .local_addr()
        .ok()
        .map(|a| a.port())
        .filter(|p| *p != 0)
    else {
        return;
    };
    let port_file = dir.join("port");
    if std::fs::write(&port_file, port.to_string()).is_err() {
        return;
    }
    let db = dir.join("otel.db");
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let db = db.clone();
                        tokio::spawn(handle_conn(stream, db));
                    }
                    Err(_) => break,
                }
            }
        }
    }
    let _ = std::fs::remove_file(&port_file);
}

/// Daemon entry: spawn the receiver when the machine is not a sandbox and
/// `[telemetry] claude_otel` is on. The returned flag is stored on daemon
/// shutdown so the receiver removes `<home>/otel/port`; `None` means the arm
/// never started.
pub fn spawn_for_daemon(home: &AgentsHome, sandbox: bool) -> Option<Arc<AtomicBool>> {
    if sandbox
        || !crate::agents_config::telemetry_claude_otel(
            &std::env::current_dir().unwrap_or_else(|_| home.root().to_path_buf()),
        )
    {
        return None;
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    let owned = home.clone();
    tokio::spawn(run(owned, Arc::clone(&shutdown)));
    Some(shutdown)
}

async fn handle_conn(mut stream: tokio::net::TcpStream, db: PathBuf) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 64 * 1024 {
            return;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok());
    if content_length.is_some_and(|len| len > MAX_BODY) {
        let _ = stream
            .write_all(
                b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
        return;
    }
    let mut body = buf[header_end..].to_vec();
    loop {
        if let Some(len) = content_length {
            if body.len() >= len {
                break;
            }
        } else if body.len() > MAX_BODY {
            // No Content-Length: bound the read instead of buffering forever.
            let _ = stream
                .write_all(
                    b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    if let Some(len) = content_length {
        body.truncate(len);
    }
    let status = if method != "POST" || path != "/v1/logs" {
        (404, "Not Found")
    } else if ingest(&db, &body) {
        (200, "OK")
    } else {
        (400, "Bad Request")
    };
    let reason = status.1;
    let _ = stream
        .write_all(
            format!("HTTP/1.1 {} {}\r\nContent-Length: 2\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{{}}", status.0, reason)
                .as_bytes(),
        )
        .await;
}

/// Parse an OTLP http/json logs body and store its `api_request` records.
/// False means the body was not valid JSON.
fn ingest(db: &Path, body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Ok(conn) = Connection::open(db) else {
        return true;
    };
    // Concurrent POSTs each open their own connection; without a busy
    // timeout the second writer eats SQLITE_BUSY and its rows drop silently.
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    if conn
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS api_requests (
                dedupe_key TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                ts TEXT,
                model TEXT,
                cost_usd_micros INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_creation_tokens INTEGER,
                skill_name TEXT,
                plugin_name TEXT,
                agent_name TEXT
            )",
        )
        .is_err()
    {
        return true;
    }
    for record in api_request_records(&v) {
        let _ = store_record(&conn, &record);
    }
    true
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn api_request_records(v: &Value) -> Vec<&Value> {
    let Some(logs) = v.get("resourceLogs").and_then(Value::as_array) else {
        return Vec::new();
    };
    logs.iter()
        .filter_map(|rl| rl.get("scopeLogs"))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|sl| sl.get("logRecords"))
        .filter_map(Value::as_array)
        .flatten()
        .filter(|record| {
            attrs(record)
                .get("event.name")
                .and_then(|v| value_str(v))
                .is_some_and(|name| name == "api_request")
        })
        .collect()
}

fn attrs(record: &Value) -> HashMap<&str, &Value> {
    record
        .get("attributes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| Some((a.get("key")?.as_str()?, a.get("value")?)))
                .collect()
        })
        .unwrap_or_default()
}

/// OTLP http/json scalars: `{"stringValue": "..."}`. A flat string is
/// tolerated so tests and hand-rolled senders both read.
fn value_str(v: &Value) -> Option<&str> {
    v.get("stringValue")
        .and_then(Value::as_str)
        .or_else(|| v.as_str())
}

/// proto3 JSON maps 64-bit ints to strings and floats to doubles; accept
/// all three spellings.
fn value_i64(v: &Value) -> Option<i64> {
    v.get("intValue")
        .map(|iv| {
            iv.as_i64()
                .or_else(|| iv.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or_else(|| {
            v.as_i64().or_else(|| {
                v.get("doubleValue")
                    .and_then(Value::as_f64)
                    .map(|f| f as i64)
            })
        })
}

struct Row {
    dedupe_key: String,
    session_id: String,
    ts: Option<String>,
    model: Option<String>,
    cost_usd_micros: Option<i64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
    skill_name: Option<String>,
    plugin_name: Option<String>,
    agent_name: Option<String>,
}

fn store_record(conn: &Connection, record: &Value) -> rusqlite::Result<()> {
    let a = attrs(record);
    let str_attr = |k: &str| a.get(k).and_then(|v| value_str(v)).map(str::to_owned);
    let int_attr = |k: &str| a.get(k).and_then(|v| value_i64(v));
    let Some(session_id) = str_attr("session.id") else {
        return Ok(());
    };
    // An exporter retry re-sends the same request_id; `INSERT OR IGNORE` on
    // the dedupe key keeps one row per request. Records without a request_id
    // key on session + sequence + timestamp.
    let dedupe_key = match str_attr("request_id") {
        Some(rid) => format!("rid:{rid}"),
        None => format!(
            "sid:{}:{}:{}",
            session_id,
            str_attr("event.sequence").unwrap_or_default(),
            str_attr("event.timestamp").unwrap_or_default(),
        ),
    };
    // A record missing micros falls back to the float `cost_usd`.
    let cost_usd_micros =
        int_attr("cost_usd_micros").or_else(|| int_attr("cost_usd").map(|usd| usd * 1_000_000));
    let row = Row {
        dedupe_key,
        session_id,
        ts: str_attr("event.timestamp"),
        model: str_attr("model"),
        cost_usd_micros,
        input_tokens: int_attr("input_tokens"),
        output_tokens: int_attr("output_tokens"),
        cache_read_tokens: int_attr("cache_read_tokens"),
        cache_creation_tokens: int_attr("cache_creation_tokens"),
        skill_name: str_attr("skill.name"),
        plugin_name: str_attr("plugin.name"),
        agent_name: str_attr("agent.name"),
    };
    conn.execute(
        "INSERT OR IGNORE INTO api_requests VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            row.dedupe_key,
            row.session_id,
            row.ts,
            row.model,
            row.cost_usd_micros,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_creation_tokens,
            row.skill_name,
            row.plugin_name,
            row.agent_name,
        ],
    )?;
    Ok(())
}

/// Exact session cost in USD from stored OTel rows. `None` when the db is
/// absent or holds no rows for the session - the caller falls back to the
/// ledger estimate.
pub fn session_cost_usd(db: &Path, session_id: &str) -> Option<f64> {
    if !db.is_file() {
        return None;
    }
    let conn = Connection::open(db).ok()?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    conn.query_row(
        "SELECT SUM(cost_usd_micros) FROM api_requests
         WHERE session_id = ?1 AND cost_usd_micros IS NOT NULL",
        [session_id],
        |r| r.get::<_, Option<i64>>(0),
    )
    .ok()?
    .map(|micros| micros as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn home() -> (AgentsHome, tempfile::TempDir) {
        let td = tempfile::tempdir().unwrap();
        (AgentsHome::at(td.path().join("agents")), td)
    }

    fn api_request(request_id: &str, session: &str, micros: i64) -> Value {
        json!({
            "attributes": [
                {"key": "event.name", "value": {"stringValue": "api_request"}},
                {"key": "session.id", "value": {"stringValue": session}},
                {"key": "request_id", "value": {"stringValue": request_id}},
                {"key": "event.sequence", "value": {"intValue": "7"}},
                {"key": "event.timestamp", "value": {"stringValue": "2026-10-02T00:00:00Z"}},
                {"key": "model", "value": {"stringValue": "claude-sonnet-5"}},
                {"key": "cost_usd_micros", "value": {"intValue": micros.to_string()}},
                {"key": "input_tokens", "value": {"intValue": "100"}},
                {"key": "output_tokens", "value": {"intValue": "50"}},
                {"key": "skill.name", "value": {"stringValue": "fno:target"}},
                {"key": "plugin.name", "value": {"stringValue": "fno"}},
                {"key": "agent.name", "value": {"stringValue": "archer"}}
            ]
        })
    }

    fn body(records: Value) -> String {
        json!({"resourceLogs": [{"scopeLogs": [{"logRecords": records}]}]}).to_string()
    }

    async fn post(port: u16, path: &str, payload: &str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut resp)).await;
        let head = String::from_utf8_lossy(&resp).into_owned();
        head.split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0)
    }

    async fn started(home: &AgentsHome, shutdown: Arc<AtomicBool>) -> u16 {
        let h = home.clone();
        let sd = Arc::clone(&shutdown);
        tokio::spawn(async move { run(h, sd).await });
        for _ in 0..100 {
            if let Ok(port) = std::fs::read_to_string(home.otel_dir().join("port")) {
                if let Ok(port) = port.trim().parse() {
                    return port;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("receiver never published a port");
    }

    fn stored(home: &AgentsHome) -> Vec<(String, Option<i64>, Option<String>)> {
        let conn = Connection::open(home.otel_dir().join("otel.db")).unwrap();
        let mut stmt = conn
            .prepare("SELECT session_id, cost_usd_micros, skill_name FROM api_requests")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[tokio::test]
    async fn dedupes_retries_and_sums_exact_cost() {
        let (home, _td) = home();
        let shutdown = Arc::new(AtomicBool::new(false));
        let port = started(&home, Arc::clone(&shutdown)).await;
        let payload = body(json!([
            api_request("req_a", "sess-1", 1_500),
            api_request("req_b", "sess-1", 2_500),
        ]));
        assert_eq!(post(port, "/v1/logs", &payload).await, 200);
        // The exporter retry: same records again must not double-count.
        assert_eq!(post(port, "/v1/logs", &payload).await, 200);
        let rows = stored(&home);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            session_cost_usd(&home.otel_dir().join("otel.db"), "sess-1"),
            Some(0.004)
        );
        assert_eq!(
            session_cost_usd(&home.otel_dir().join("otel.db"), "sess-x"),
            None
        );
        shutdown.store(true, Ordering::SeqCst);
        for _ in 0..100 {
            if !home.otel_dir().join("port").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!home.otel_dir().join("port").exists());
    }

    #[tokio::test]
    async fn keys_no_request_id_records_and_falls_back_to_cost_usd() {
        let (home, _td) = home();
        let shutdown = Arc::new(AtomicBool::new(false));
        let port = started(&home, Arc::clone(&shutdown)).await;
        let payload = body(json!([{
            "attributes": [
                {"key": "event.name", "value": {"stringValue": "api_request"}},
                {"key": "session.id", "value": {"stringValue": "sess-2"}},
                {"key": "event.sequence", "value": {"intValue": "3"}},
                {"key": "event.timestamp", "value": {"stringValue": "2026-10-02T00:00:01Z"}},
                {"key": "cost_usd", "value": {"doubleValue": 2.5}},
            ]
        }]));
        assert_eq!(post(port, "/v1/logs", &payload).await, 200);
        assert_eq!(post(port, "/v1/logs", &payload).await, 200);
        let rows = stored(&home);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, Some(2_500_000));
        shutdown.store(true, Ordering::SeqCst);
    }

    /// Declare a body larger than the cap without sending it: the 413 must
    /// fire on the header, and not sending avoids the RST the server's early
    /// close would deliver mid-body.
    async fn post_oversized(port: u16) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let req = format!(
            "POST /v1/logs HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut resp)).await;
        let head = String::from_utf8_lossy(&resp).into_owned();
        head.split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn refuses_malformed_and_oversized_bodies_and_keeps_serving() {
        let (home, _td) = home();
        let shutdown = Arc::new(AtomicBool::new(false));
        let port = started(&home, Arc::clone(&shutdown)).await;
        assert_eq!(post(port, "/v1/logs", "{not json").await, 400);
        assert_eq!(post(port, "/v1/traces", "{}").await, 404);
        assert!(!home.otel_dir().join("otel.db").exists());
        assert_eq!(post_oversized(port).await, 413);
        // Still serving after all three.
        assert_eq!(
            post(
                port,
                "/v1/logs",
                &body(json!([api_request("req_ok", "sess-3", 5)]))
            )
            .await,
            200
        );
        assert_eq!(stored(&home).len(), 1);
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn no_db_reads_none() {
        let (home, _td) = home();
        assert_eq!(
            session_cost_usd(&home.otel_dir().join("otel.db"), "sess-1"),
            None
        );
    }
}
