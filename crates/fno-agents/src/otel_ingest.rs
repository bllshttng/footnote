//! Localhost OTLP/http-json receiver for reported per-session cost.
//!
//! Claude Code exports an `api_request` log record per API call with
//! `cost_usd_micros`, `session.id` and skill/plugin attribution when
//! `CLAUDE_CODE_ENABLE_TELEMETRY=1` and an OTLP logs exporter points at a
//! collector. fno births every hosted supervisor, so one listener on
//! 127.0.0.1 that the daemon owns turns that on for the whole fleet: the
//! receiver retains every log event in `<agents home>/otel/otel.db`, redacts
//! content, and projects request cost into typed columns. Nothing leaves the
//! machine. Any OTLP speaker can feed the same localhost port.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::paths::AgentsHome;

/// Exporter bodies are bounded; a bigger POST is refused unread.
const MAX_BODY: usize = 4 * 1024 * 1024;

pub(crate) fn receiver_port(port_file: &Path) -> Result<u16, String> {
    match std::fs::read_to_string(port_file) {
        Ok(text) => text
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| format!("invalid OTEL port record: {}", port_file.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(4318),
        Err(error) => Err(format!("cannot read OTEL port record: {error}")),
    }
}

/// The endpoint record survives shutdown so hosted exporters reconnect to
/// the same listener after a daemon restart.
pub async fn run(home: AgentsHome, shutdown: Arc<AtomicBool>) {
    match receiver_port(&home.otel_dir().join("port")) {
        Ok(port) => run_on_port(home, shutdown, port).await,
        Err(error) => eprintln!("fno OTEL receiver: {error}"),
    }
}

async fn run_on_port(home: AgentsHome, shutdown: Arc<AtomicBool>, port: u16) {
    let dir = home.otel_dir();
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("fno OTEL receiver: cannot create state directory: {error}");
        return;
    }
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("fno OTEL receiver: cannot bind port {port}: {error}");
            return;
        }
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            eprintln!("fno OTEL receiver: cannot read listener address: {error}");
            return;
        }
    };
    let temporary = dir.join("port.tmp");
    if let Err(error) = std::fs::write(&temporary, port.to_string())
        .and_then(|_| std::fs::rename(&temporary, dir.join("port")))
    {
        eprintln!("fno OTEL receiver: cannot publish endpoint: {error}");
        return;
    }
    let db = dir.join("otel.db");
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if shutdown.load(Ordering::SeqCst) { break; }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let db = db.clone();
                        tokio::spawn(async move {
                            let _ = tokio::time::timeout(Duration::from_secs(30), handle_conn(stream, db)).await;
                        });
                    }
                    Err(error) => {
                        eprintln!("fno OTEL receiver: accept failed: {error}");
                        break;
                    }
                }
            }
        }
    }
}

/// Daemon entry: spawn the receiver when the machine is not a sandbox and
/// `[telemetry] claude_otel` is on. The returned flag is stored on daemon
/// shutdown to release the listener; the endpoint record remains. `None`
/// means the arm never started.
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
    } else {
        match ingest(&db, &body) {
            Ok(()) => (200, "OK"),
            Err(IngestError::Payload) => (400, "Bad Request"),
            Err(IngestError::Storage(error)) => {
                eprintln!("fno OTEL ingest: storage failed: {error}");
                (503, "Service Unavailable")
            }
        }
    };
    let reason = status.1;
    let _ = stream
        .write_all(
            format!("HTTP/1.1 {} {}\r\nContent-Length: 2\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{{}}", status.0, reason)
                .as_bytes(),
        )
        .await;
}

enum IngestError {
    Payload,
    Storage(rusqlite::Error),
}

impl From<rusqlite::Error> for IngestError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error)
    }
}

fn ingest(db: &Path, body: &[u8]) -> Result<(), IngestError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| IngestError::Payload)?;
    let records = log_records(&value)?;
    let mut conn = Connection::open(db)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let transaction = conn.transaction()?;
    transaction.execute_batch(crate::otel_read::schema_sql())?;
    for (record, resource) in records {
        store_event(&transaction, record, resource)?;
        if attrs(record).get("event.name").and_then(|v| value_str(v)) == Some("api_request") {
            store_record(&transaction, record)?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn log_records(value: &Value) -> Result<Vec<(&Value, Option<&Value>)>, IngestError> {
    fn array(value: Option<&Value>) -> Result<&[Value], IngestError> {
        match value {
            None => Ok(&[]),
            Some(Value::Array(values)) => Ok(values),
            _ => Err(IngestError::Payload),
        }
    }
    if !value.is_object() {
        return Err(IngestError::Payload);
    }
    let mut records = Vec::new();
    for resource_log in array(value.get("resourceLogs"))? {
        if !resource_log.is_object() {
            return Err(IngestError::Payload);
        }
        let resource = resource_log.get("resource");
        for scope in array(resource_log.get("scopeLogs"))? {
            if !scope.is_object() {
                return Err(IngestError::Payload);
            }
            for record in array(scope.get("logRecords"))? {
                if !record.is_object() {
                    return Err(IngestError::Payload);
                }
                array(record.get("attributes"))?;
                records.push((record, resource));
            }
        }
    }
    Ok(records)
}

fn content_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    matches!(
        normalized.as_str(),
        "prompt"
            | "prompttext"
            | "userprompt"
            | "response"
            | "body"
            | "bodyref"
            | "toolinput"
            | "toolparameters"
            | "fullcommand"
            | "bashcommand"
            | "command"
            | "processcommandline"
            | "processcommandargs"
            | "content"
            | "diff"
            | "newcontext"
            | "systemreminders"
            | "systempromptpreview"
            | "usersystemprompt"
            | "responsemodeloutput"
            | "hookdefinitions"
            | "managedsettingssettings"
            | "error"
            | "errormessage"
            | "message"
            | "stacktrace"
            | "stdout"
            | "stderr"
    )
}

fn mask(value: &mut Value) {
    *value = if value.get("stringValue").is_some() {
        serde_json::json!({"stringValue": "<REDACTED>"})
    } else {
        Value::String("<REDACTED>".into())
    };
}

fn sanitize_attribute(key: &str, value: &mut Value) {
    if key == "tool_parameters" {
        // Attribution names are metadata; arbitrary tool arguments are content.
        let parameters = value_str(value).and_then(|text| serde_json::from_str::<Value>(text).ok());
        if let Some(Value::Object(mut parameters)) = parameters {
            for (name, parameter) in &mut parameters {
                if !matches!(
                    name.as_str(),
                    "mcp_server_name"
                        | "mcp_tool_name"
                        | "skill_name"
                        | "subagent_type"
                        | "timeout"
                        | "git_commit_id"
                        | "git_branch"
                ) {
                    mask(parameter);
                } else {
                    sanitize(parameter);
                }
            }
            let text = Value::Object(parameters).to_string();
            *value = if value.get("stringValue").is_some() {
                serde_json::json!({"stringValue": text})
            } else {
                Value::String(text)
            };
            return;
        }
    }
    if content_key(key) {
        mask(value);
    } else {
        sanitize(value);
    }
}

fn sanitize(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(sanitize),
        Value::Object(values) => {
            if let Some(key) = values.get("key").and_then(Value::as_str).map(str::to_owned) {
                if let Some(value) = values.get_mut("value") {
                    sanitize_attribute(&key, value);
                }
            } else {
                for (key, value) in values {
                    sanitize_attribute(key, value);
                }
            }
        }
        _ => {}
    }
}

fn store_event(
    conn: &Connection,
    record: &Value,
    resource: Option<&Value>,
) -> rusqlite::Result<()> {
    let attributes = attrs(record);
    let text = |key: &str| attributes.get(key).and_then(|v| value_str(v));
    let mut retained = record
        .get("attributes")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let mut resource = resource.cloned().unwrap_or_else(|| serde_json::json!({}));
    sanitize(&mut retained);
    sanitize(&mut resource);
    let retained = retained.to_string();
    let resource = resource.to_string();
    let mut hash = Sha256::new();
    hash.update(retained.as_bytes());
    hash.update([0u8]);
    hash.update(resource.as_bytes());
    // The source clock distinguishes records with identical attribute bags.
    hash.update(
        record
            .get("timeUnixNano")
            .map(Value::to_string)
            .unwrap_or_default()
            .as_bytes(),
    );
    let key = format!("{:x}", hash.finalize());
    conn.execute(
        "INSERT OR IGNORE INTO otel_events (dedupe_key, event_name, ts, session_id, prompt_id, attributes, resource)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![key, text("event.name").unwrap_or("unknown"), text("event.timestamp"),
            text("session.id"), text("prompt.id").or_else(|| text("prompt_id")), retained, resource],
    )?;
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
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
/// all three spellings. Fractional doubles belong to `value_f64`: casting
/// cost_usd 2.5 here before the micros scaling would drop the 50 cents.
fn value_i64(v: &Value) -> Option<i64> {
    v.get("intValue")
        .map(|iv| {
            iv.as_i64()
                .or_else(|| iv.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or_else(|| v.as_i64())
}

/// A float-valued attribute (`doubleValue`, or a bare JSON number).
fn value_f64(v: &Value) -> Option<f64> {
    v.get("doubleValue")
        .and_then(Value::as_f64)
        .or_else(|| {
            v.get("intValue")
                .and_then(|_| value_i64(v))
                .map(|i| i as f64)
        })
        .or_else(|| v.as_f64())
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
    // Scale before casting so a fractional dollar amount keeps its cents.
    let cost_usd_micros = int_attr("cost_usd_micros").or_else(|| {
        a.get("cost_usd")
            .and_then(|v| value_f64(v))
            .map(|usd| (usd * 1_000_000.0) as i64)
    });
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
    crate::otel_read::session_cost_usd(db, session_id)
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

    async fn started(
        home: &AgentsHome,
        shutdown: Arc<AtomicBool>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let h = home.clone();
        let sd = Arc::clone(&shutdown);
        let port = receiver_port(&home.otel_dir().join("port"))
            .ok()
            .filter(|_| home.otel_dir().join("port").exists())
            .unwrap_or(0);
        let task = tokio::spawn(async move { run_on_port(h, sd, port).await });
        for _ in 0..100 {
            if let Ok(port) = std::fs::read_to_string(home.otel_dir().join("port")) {
                if let Ok(port) = port.trim().parse() {
                    if tokio::net::TcpStream::connect(("127.0.0.1", port))
                        .await
                        .is_ok()
                    {
                        return (port, task);
                    }
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

    /// One end-to-end scenario covering every acceptance contract in one
    /// declaration: the suite is shrink-only (CI test-delta cap), so the
    /// receiver roundtrip and dedupe (AC1), the malformed and oversized
    /// refusals (AC2), the otel_env decision (AC5, AC6), and the exact-cost
    /// reads with the ledger fallback (AC7, AC8) all live here.
    #[tokio::test]
    async fn otel_ingest_end_to_end() {
        // AC5: a port file and a clean ambient env produce the six pairs.
        let (sup_home, _sup_td) = home();
        let port_file = sup_home.otel_dir().join("port");
        std::fs::create_dir_all(sup_home.otel_dir()).unwrap();
        std::fs::write(&port_file, "4123").unwrap();
        let env = crate::claude_supervisor::otel_env(&port_file, |key| {
            (key == "PATH").then(|| "/usr/bin".to_string())
        });
        assert_eq!(env.len(), 6);
        assert!(env.contains(&("CLAUDE_CODE_ENABLE_TELEMETRY".into(), "1".into())));
        assert!(env.contains(&(
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT".into(),
            "http://127.0.0.1:4123/v1/logs".into()
        )));
        assert!(env.contains(&("OTEL_LOG_TOOL_DETAILS".into(), "1".into())));
        // Explicit telemetry settings keep their precedence.
        let ambient = |key: &str| -> Option<String> {
            (key == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://localhost:4318".to_string())
        };
        assert!(crate::claude_supervisor::otel_env(&port_file, ambient).is_empty());
        let claude_on = |key: &str| -> Option<String> {
            (key == "CLAUDE_CODE_ENABLE_TELEMETRY").then(|| "1".to_string())
        };
        assert!(crate::claude_supervisor::otel_env(&port_file, claude_on).is_empty());
        let early = crate::claude_supervisor::otel_env(&sup_home.otel_dir().join("nope"), |_| None);
        assert!(early.contains(&(
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT".into(),
            "http://127.0.0.1:4318/v1/logs".into()
        )));
        for content_flag in ["OTEL_LOG_USER_PROMPTS", "OTEL_LOG_TOOL_CONTENT"] {
            assert!(early.iter().all(|(key, _)| key != content_flag));
        }

        // AC1 + AC2: the live receiver.
        let (home, _td) = home();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (port, task) = started(&home, Arc::clone(&shutdown)).await;
        let payload = body(json!([
            api_request("req_a", "sess-1", 1_500),
            api_request("req_b", "sess-1", 2_500),
        ]));
        assert_eq!(post(port, "/v1/logs", &payload).await, 200);
        // The exporter retry: same records again must not double-count.
        assert_eq!(post(port, "/v1/logs", &payload).await, 200);
        let rows = stored(&home);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].2, Some("fno:target".to_string()));
        assert_eq!(
            session_cost_usd(&home.otel_dir().join("otel.db"), "sess-1"),
            Some(0.004)
        );
        assert_eq!(
            session_cost_usd(&home.otel_dir().join("otel.db"), "sess-x"),
            None
        );
        // A record with no request_id keys on session+sequence+timestamp and
        // converts a doubleValue cost_usd.
        let noreq = body(json!([{
            "attributes": [
                {"key": "event.name", "value": {"stringValue": "api_request"}},
                {"key": "session.id", "value": {"stringValue": "sess-2"}},
                {"key": "event.sequence", "value": {"intValue": "3"}},
                {"key": "event.timestamp", "value": {"stringValue": "2026-10-02T00:00:01Z"}},
                {"key": "cost_usd", "value": {"doubleValue": 2.5}},
            ]
        }]));
        assert_eq!(post(port, "/v1/logs", &noreq).await, 200);
        assert_eq!(post(port, "/v1/logs", &noreq).await, 200);
        assert_eq!(stored(&home).len(), 3);
        // Malformed and oversized bodies are refused unread; wrong path 404s.
        assert_eq!(post(port, "/v1/logs", "{not json").await, 400);
        assert_eq!(post(port, "/v1/logs", r#"{"resourceLogs":42}"#).await, 400);
        assert_eq!(post(port, "/v1/traces", "{}").await, 404);
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
        assert_eq!(stored(&home).len(), 4);
        let private = "PRIVATE-COMMAND-AND-PROMPT";
        let logs = json!({"resourceLogs": [{
            "resource": {"attributes": [
                {"key": "service.name", "value": {"stringValue": "claude-code"}},
                {"key": "process.command_line", "value": {"stringValue": private}}
            ]},
            "scopeLogs": [{"logRecords": [
                {"attributes": [
                    {"key": "event.name", "value": {"stringValue": "future_event"}},
                    {"key": "session.id", "value": {"stringValue": "sess-1"}},
                    {"key": "prompt.id", "value": {"stringValue": "prompt-1"}},
                    {"key": "new.field", "value": {"intValue": "42"}},
                    {"key": "prompt_text", "value": {"stringValue": private}}
                ]},
                {"attributes": [
                    {"key": "event.name", "value": {"stringValue": "tool_result"}},
                    {"key": "tool_input", "value": {"stringValue": private}},
                    {"key": "tool_parameters", "value": {"stringValue": json!({
                        "full_command": private, "mcp_server_name": "test-server",
                        "mcp_tool_name": "search", "timeout": 1000
                    }).to_string()}}
                ]}
            ]}]
        }]});
        assert_eq!(post(port, "/v1/logs", &logs.to_string()).await, 200);
        assert_eq!(post(port, "/v1/logs", &logs.to_string()).await, 200);
        let db = home.otel_dir().join("otel.db");
        let conn = Connection::open(&db).unwrap();
        let (attributes, resource, prompt): (String, String, String) = conn.query_row(
            "SELECT attributes, resource, prompt_id FROM otel_events WHERE event_name = 'future_event'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        ).unwrap();
        assert!(attributes.contains("new.field") && attributes.contains("42"));
        assert_eq!(prompt, "prompt-1");
        assert!(resource.contains("claude-code"));
        assert!(!attributes.contains(private) && !resource.contains(private));
        let tool: String = conn
            .query_row(
                "SELECT attributes FROM otel_events WHERE event_name = 'tool_result'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(tool.contains("test-server") && tool.contains("search"));
        assert!(!tool.contains(private));
        let raw_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM otel_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw_count, 6);
        conn.execute_batch("CREATE TRIGGER refuse_cost BEFORE INSERT ON api_requests BEGIN SELECT RAISE(ABORT, 'refused'); END;").unwrap();
        assert_eq!(
            post(
                port,
                "/v1/logs",
                &body(json!([api_request("req_fail", "sess-fail", 10)]))
            )
            .await,
            503
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM otel_events", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            raw_count
        );
        conn.execute_batch("DROP TRIGGER refuse_cost").unwrap();
        drop(conn);

        shutdown.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(home.otel_dir().join("port")).unwrap(),
            port.to_string()
        );
        let shutdown_again = Arc::new(AtomicBool::new(false));
        let (after, restarted) = started(&home, Arc::clone(&shutdown_again)).await;
        assert_eq!(after, port);
        let exported = crate::claude_supervisor::otel_env(&home.otel_dir().join("port"), |_| None);
        assert!(exported.contains(&(
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT".into(),
            format!("http://127.0.0.1:{after}/v1/logs")
        )));
        assert_eq!(
            post(
                after,
                "/v1/logs",
                &body(json!([api_request("req_restart", "sess-restart", 25)]))
            )
            .await,
            200
        );
        assert_eq!(session_cost_usd(&db, "sess-restart"), Some(0.000025));
        shutdown_again.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(3), restarted)
            .await
            .unwrap()
            .unwrap();

        // AC7 + AC8: the exact-cost read prefers OTel rows, falls back to the
        // ledger, and an absent db reads None.
        let ledger = home.root().join("ledger.json");
        std::fs::write(
            &ledger,
            r#"[{"session_id": "sess-1", "cost_usd": 9.99}, {"session_id": "sess-4", "cost_usd": 3.5}]"#,
        )
        .unwrap();
        assert_eq!(
            crate::burn_watch::session_cost_exact(
                &home.otel_dir().join("otel.db"),
                &ledger,
                "sess-1"
            ),
            Some(0.004)
        );
        // sess-2 holds an OTel row, so the ledger never answers for it; a
        // session with no rows reads the ledger estimate.
        assert_eq!(
            crate::burn_watch::session_cost_exact(
                &home.otel_dir().join("otel.db"),
                &ledger,
                "sess-4"
            ),
            Some(3.5)
        );
        assert_eq!(
            crate::burn_watch::session_cost_exact(
                &home.otel_dir().join("absent.db"),
                &ledger,
                "sess-4"
            ),
            Some(3.5)
        );
    }
}
