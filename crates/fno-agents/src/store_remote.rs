//! The opt-in remote store: one libSQL (sqld) primary that every machine
//! dials out to.
//!
//! `store.remote_url` in the per-machine global config turns it on. Unset,
//! nothing here runs and no process opens a socket. Set, the claim keys that
//! decide dispatch (see `claim_store::SHARED_PREFIXES`) run their statements
//! on the primary, so two machines never hold one node. Every other store
//! stays local. The client speaks sqld's Hrana-over-HTTP pipeline over plain
//! HTTP: the primary sits on a private network (the tailnet), which owns
//! remote reach, as `fno web` leaves it.
//!
//! A primary that cannot be reached fails closed: the call returns an error
//! that names the URL and the key, and nothing is written anywhere.

use base64::Engine;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const CONNECT_WAIT: Duration = Duration::from_secs(3);
const REPLY_WAIT: Duration = Duration::from_secs(15);
const KEY: &str = "store.remote_url";
const UNREACHABLE: &str = "is unreachable";

#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlValue {
    pub fn text(&self) -> Option<&str> {
        match self {
            SqlValue::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn integer(&self) -> Option<i64> {
        match self {
            SqlValue::Integer(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn to_hrana(&self) -> Value {
        match self {
            SqlValue::Null => json!({"type": "null"}),
            SqlValue::Integer(v) => json!({"type": "integer", "value": v.to_string()}),
            SqlValue::Real(v) => json!({"type": "float", "value": v}),
            SqlValue::Text(v) => json!({"type": "text", "value": v}),
            SqlValue::Blob(v) => json!({
                "type": "blob",
                "base64": base64::engine::general_purpose::STANDARD_NO_PAD.encode(v)
            }),
        }
    }

    pub(crate) fn from_hrana(value: &Value) -> Result<Self, String> {
        let field = |name: &str| value.get(name);
        Ok(match field("type").and_then(Value::as_str) {
            Some("null") => SqlValue::Null,
            Some("integer") => SqlValue::Integer(
                field("value")
                    .and_then(Value::as_str)
                    .and_then(|v| v.parse().ok())
                    .ok_or("integer value without digits")?,
            ),
            Some("float") => {
                SqlValue::Real(field("value").and_then(Value::as_f64).ok_or("bad float")?)
            }
            Some("text") => SqlValue::Text(
                field("value")
                    .and_then(Value::as_str)
                    .ok_or("text value without text")?
                    .to_string(),
            ),
            Some("blob") => SqlValue::Blob(
                base64::engine::general_purpose::STANDARD_NO_PAD
                    .decode(
                        field("base64")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim_end_matches('='),
                    )
                    .map_err(|e| e.to_string())?,
            ),
            other => return Err(format!("unknown value type {other:?}")),
        })
    }
}

impl rusqlite::ToSql for SqlValue {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::{ToSqlOutput, ValueRef};
        Ok(ToSqlOutput::Borrowed(match self {
            SqlValue::Null => ValueRef::Null,
            SqlValue::Integer(v) => ValueRef::Integer(*v),
            SqlValue::Real(v) => ValueRef::Real(*v),
            SqlValue::Text(v) => ValueRef::Text(v.as_bytes()),
            SqlValue::Blob(v) => ValueRef::Blob(v),
        }))
    }
}

impl From<rusqlite::types::ValueRef<'_>> for SqlValue {
    fn from(value: rusqlite::types::ValueRef<'_>) -> Self {
        use rusqlite::types::ValueRef;
        match value {
            ValueRef::Null => SqlValue::Null,
            ValueRef::Integer(v) => SqlValue::Integer(v),
            ValueRef::Real(v) => SqlValue::Real(v),
            ValueRef::Text(v) => SqlValue::Text(String::from_utf8_lossy(v).into_owned()),
            ValueRef::Blob(v) => SqlValue::Blob(v.to_vec()),
        }
    }
}

/// One statement's reply: the column names, the rows, and the change count.
#[derive(Debug, Default)]
pub struct Reply {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<SqlValue>>,
    pub affected: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Remote {
    url: String,
    host: String,
    port: u16,
    path: String,
    token: Option<String>,
}

impl Remote {
    /// Parse `http://host[:port][/prefix]`. Any other scheme refuses: this
    /// client carries no TLS, and the primary is meant to sit on a private
    /// network.
    pub fn parse(url: &str, token: Option<String>) -> Result<Self, String> {
        let url = url.trim().trim_end_matches('/');
        let rest = url.strip_prefix("http://").ok_or_else(|| {
            format!("{KEY} = {url:?}: only http:// URLs on a private network are supported")
        })?;
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (rest, String::new()),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse()
                    .map_err(|_| format!("{KEY} = {url:?}: bad port {port:?}"))?,
            ),
            None => (authority, 80),
        };
        if host.is_empty() {
            return Err(format!("{KEY} = {url:?}: no host"));
        }
        Ok(Self {
            url: url.to_string(),
            host: host.to_string(),
            port,
            path,
            token: token.filter(|t| !t.is_empty()),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn execute(&self, sql: &str, args: &[SqlValue]) -> Result<Reply, String> {
        let mut replies = self.pipeline(vec![json!({
            "type": "execute",
            "stmt": {"sql": sql, "args": args.iter().map(SqlValue::to_hrana).collect::<Vec<_>>()},
        })])?;
        Ok(replies.pop().unwrap_or_default())
    }

    /// Run a multi-statement script (DDL) in one request.
    pub fn script(&self, sql: &str) -> Result<(), String> {
        self.pipeline(vec![json!({"type": "sequence", "sql": sql})])
            .map(drop)
    }

    /// Run `steps` as one transaction in one request. Each step runs only
    /// when the step before it succeeded, and a failed step rolls the batch
    /// back, so the primary keeps all of the steps or none. A refusal
    /// carries the index of the step that failed, or `None` when the
    /// request itself failed.
    pub fn transaction(
        &self,
        steps: &[(String, Vec<SqlValue>)],
    ) -> Result<Vec<Reply>, (Option<usize>, String)> {
        let stmt = |sql: &str, args: &[SqlValue]| json!({"sql": sql, "args": args.iter().map(SqlValue::to_hrana).collect::<Vec<_>>()});
        let commit = steps.len() + 1;
        let mut batch = vec![json!({"stmt": stmt("BEGIN IMMEDIATE", &[])})];
        for (i, (sql, args)) in steps.iter().enumerate() {
            batch.push(json!({"stmt": stmt(sql, args), "condition": {"type": "ok", "step": i}}));
        }
        batch.push(
            json!({"stmt": stmt("COMMIT", &[]), "condition": {"type": "ok", "step": commit - 1}}),
        );
        batch.push(json!({
            "stmt": stmt("ROLLBACK", &[]),
            "condition": {"type": "not", "cond": {"type": "ok", "step": commit}},
        }));
        let response = self
            .responses(vec![json!({"type": "batch", "batch": {"steps": batch}})])
            .map_err(|error| (None, error))?
            // The batch answers first; the pipeline's close answers last.
            .into_iter()
            .next()
            .unwrap_or_default();
        let result = response.get("result").cloned().unwrap_or_default();
        let errors = result.get("step_errors").and_then(Value::as_array);
        for index in 0..=commit {
            let Some(error) = errors.and_then(|e| e.get(index)).filter(|e| !e.is_null()) else {
                continue;
            };
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("statement failed");
            let step = (1..commit).contains(&index).then(|| index - 1);
            return Err((step, format!("remote store {}: {message}", self.url)));
        }
        let results = result.get("step_results").and_then(Value::as_array);
        if results
            .and_then(|r| r.get(commit))
            .is_none_or(Value::is_null)
        {
            return Err((
                None,
                format!("remote store {}: the batch did not commit", self.url),
            ));
        }
        (1..commit)
            .map(|index| {
                let step = results
                    .and_then(|r| r.get(index))
                    .cloned()
                    .unwrap_or_default();
                self.reply(&step).map_err(|error| (Some(index - 1), error))
            })
            .collect()
    }

    fn pipeline(&self, requests: Vec<Value>) -> Result<Vec<Reply>, String> {
        self.responses(requests)?
            .iter()
            .filter_map(|response| response.get("result"))
            .map(|rows| self.reply(rows))
            .collect()
    }

    /// Send one pipeline and return each request's `response`. The first
    /// request that failed fails the call.
    fn responses(&self, mut requests: Vec<Value>) -> Result<Vec<Value>, String> {
        requests.push(json!({"type": "close"}));
        let body = json!({"baton": null, "requests": requests}).to_string();
        let reply = self.post("/v2/pipeline", &body).map_err(|error| {
            // A 4xx is the primary answering, a bad token say: a fault to
            // name, not an outage to hold through.
            if error.starts_with("HTTP 4") {
                format!(
                    "remote store {} ({KEY}) refused the request: {error}",
                    self.url
                )
            } else {
                self.unreachable(&error)
            }
        })?;
        let reply: Value = serde_json::from_str(&reply)
            .map_err(|error| format!("remote store {}: bad reply: {error}", self.url))?;
        let results = reply
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("remote store {}: reply has no results", self.url))?;
        let mut out = Vec::new();
        for result in results {
            if result.get("type").and_then(Value::as_str) != Some("ok") {
                let message = result
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("statement failed");
                return Err(format!("remote store {}: {message}", self.url));
            }
            if let Some(response) = result.get("response") {
                out.push(response.clone());
            }
        }
        Ok(out)
    }

    /// One statement result: its columns, rows and change count.
    fn reply(&self, rows: &Value) -> Result<Reply, String> {
        let columns = rows
            .get("cols")
            .and_then(Value::as_array)
            .map(|cols| {
                cols.iter()
                    .map(|c| c.get("name").and_then(Value::as_str).unwrap_or("").into())
                    .collect()
            })
            .unwrap_or_default();
        let rows_out = rows
            .get("rows")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        row.as_array()
                            .into_iter()
                            .flatten()
                            .map(SqlValue::from_hrana)
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .map_err(|error| format!("remote store {}: {error}", self.url))?
            .unwrap_or_default();
        Ok(Reply {
            columns,
            rows: rows_out,
            affected: rows
                .get("affected_row_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        })
    }

    fn unreachable(&self, error: &str) -> String {
        format!(
            "remote store {} ({KEY}) {UNREACHABLE}: {error}. Nothing was written locally. \
             A request that timed out can still have reached the primary. \
             Unset {KEY} in the global config to use the local store.",
            self.url
        )
    }

    fn post(&self, route: &str, body: &str) -> Result<String, String> {
        let address = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .next()
            .ok_or("the host name resolves to no address")?;
        let mut stream =
            TcpStream::connect_timeout(&address, CONNECT_WAIT).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(REPLY_WAIT))
            .and_then(|()| stream.set_write_timeout(Some(REPLY_WAIT)))
            .map_err(|e| e.to_string())?;
        let auth = self
            .token
            .as_ref()
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "POST {}{route} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n{auth}Connection: close\r\n\r\n",
            self.path,
            self.host,
            self.port,
            body.len()
        );
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(body.as_bytes()))
            .map_err(|e| e.to_string())?;
        read_response(BufReader::new(stream))
    }
}

/// Is this error the fail-closed refusal of an unreachable primary?
pub fn is_unreachable(error: &str) -> bool {
    error.starts_with("remote store ") && error.contains(UNREACHABLE)
}

fn read_response(mut reader: impl BufRead) -> Result<String, String> {
    let mut status = String::new();
    reader.read_line(&mut status).map_err(|e| e.to_string())?;
    let code = status.split_whitespace().nth(1).unwrap_or("");
    let (mut length, mut chunked) = (None, false);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse::<usize>().ok(),
                "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
                _ => {}
            }
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).map_err(|e| e.to_string())?;
            let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| "bad chunk size in reply".to_string())?;
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).map_err(|e| e.to_string())?;
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else if let Some(length) = length {
        body.resize(length, 0);
        reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    } else {
        reader.read_to_end(&mut body).map_err(|e| e.to_string())?;
    }
    let body = String::from_utf8_lossy(&body).into_owned();
    if code != "200" {
        return Err(format!("HTTP {code}: {}", body.trim()));
    }
    Ok(body)
}

/// The primary this machine is configured to use. `Ok(None)` is the stock
/// install: the key is unset and the local file is the store. A malformed
/// value is an error, so a typo never falls back to local in silence.
pub fn configured() -> Result<Option<Remote>, String> {
    #[cfg(test)]
    {
        Ok(None)
    }
    #[cfg(not(test))]
    {
        static CONFIGURED: std::sync::OnceLock<Result<Option<Remote>, String>> =
            std::sync::OnceLock::new();
        CONFIGURED
            .get_or_init(|| {
                let lookup = |key| {
                    crate::agents_config::config_lookup_global(&["store", key])
                        .and_then(|v| v.as_str().map(str::to_string))
                        .filter(|v| !v.trim().is_empty())
                };
                lookup("remote_url")
                    .map(|url| Remote::parse(&url, lookup("remote_token")))
                    .transpose()
            })
            .clone()
    }
}

/// The primary that holds the shared backlog. Both `store.remote_url` and
/// `store.share_backlog = true` must be set; either one unset is the stock
/// install, where every backlog write stays in the local file and no
/// process opens a socket for it.
pub fn share_backlog() -> Result<Option<Remote>, String> {
    #[cfg(test)]
    {
        Ok(None)
    }
    #[cfg(not(test))]
    {
        // Read once per process: every backlog open asks, key on or off.
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let on = *ON.get_or_init(|| {
            crate::agents_config::config_lookup_global(&["store", "share_backlog"])
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });
        if on {
            configured()
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
pub(crate) mod test_primary {
    //! A sqld stand-in for tests: the Hrana pipeline over HTTP, answered by
    //! one SQLite file. Same SQL engine, a real socket.

    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    pub(crate) struct Primary {
        pub remote: Remote,
        pub db: Arc<Mutex<rusqlite::Connection>>,
        _dir: tempfile::TempDir,
    }

    pub(crate) fn start() -> Primary {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Mutex::new(
            rusqlite::Connection::open(dir.path().join("primary.db")).unwrap(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = Arc::clone(&db);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, &served);
            }
        });
        Primary {
            remote: Remote::parse(&format!("http://127.0.0.1:{port}"), None).unwrap(),
            db,
            _dir: dir,
        }
    }

    /// A URL with nothing listening behind it.
    pub(crate) fn dead() -> Remote {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Remote::parse(&format!("http://127.0.0.1:{port}"), None).unwrap()
    }

    fn serve(stream: TcpStream, db: &Mutex<rusqlite::Connection>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        let connection = db.lock().unwrap();
        let results: Vec<Value> = request["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| answer(&connection, r))
            .collect();
        let reply = json!({"baton": null, "results": results}).to_string();
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
            reply.len()
        )
        .unwrap();
    }

    /// A Hrana batch: steps run in order, each only when its condition
    /// holds, and every step reports a result or an error.
    fn batch(connection: &rusqlite::Connection, steps: &[Value]) -> Value {
        let mut outcomes: Vec<Option<bool>> = Vec::new();
        let (mut results, mut errors) = (Vec::new(), Vec::new());
        for step in steps {
            if !step["condition"].is_null() && !holds(&step["condition"], &outcomes) {
                outcomes.push(None);
                results.push(Value::Null);
                errors.push(Value::Null);
                continue;
            }
            match statement(connection, &step["stmt"]) {
                Ok(result) => {
                    outcomes.push(Some(true));
                    results.push(result);
                    errors.push(Value::Null);
                }
                Err(e) => {
                    outcomes.push(Some(false));
                    results.push(Value::Null);
                    errors.push(json!({"message": e.to_string()}));
                }
            }
        }
        json!({"type": "ok", "response": {"type": "batch", "result": {
            "step_results": results, "step_errors": errors,
        }}})
    }

    fn holds(condition: &Value, outcomes: &[Option<bool>]) -> bool {
        let step = |c: &Value| {
            outcomes
                .get(c["step"].as_u64().unwrap() as usize)
                .copied()
                .flatten()
        };
        match condition["type"].as_str() {
            Some("ok") => step(condition) == Some(true),
            Some("error") => step(condition) == Some(false),
            Some("not") => !holds(&condition["cond"], outcomes),
            Some("and") => condition["conds"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| holds(c, outcomes)),
            Some("or") => condition["conds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| holds(c, outcomes)),
            other => panic!("unknown condition {other:?}"),
        }
    }

    fn statement(connection: &rusqlite::Connection, stmt: &Value) -> rusqlite::Result<Value> {
        let args: Vec<SqlValue> = stmt["args"]
            .as_array()
            .map(|args| {
                args.iter()
                    .map(|v| SqlValue::from_hrana(v).unwrap())
                    .collect()
            })
            .unwrap_or_default();
        let mut statement = connection.prepare(stmt["sql"].as_str().unwrap())?;
        let cols: Vec<Value> = statement
            .column_names()
            .iter()
            .map(|n| json!({"name": n}))
            .collect();
        let width = cols.len();
        let mut rows = statement.query(rusqlite::params_from_iter(args.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(
                (0..width)
                    .map(|i| Ok(SqlValue::from(row.get_ref(i)?).to_hrana()))
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        drop(rows);
        Ok(json!({"cols": cols, "rows": out, "affected_row_count": connection.changes()}))
    }

    fn answer(connection: &rusqlite::Connection, request: &Value) -> Value {
        let failed =
            |e: rusqlite::Error| json!({"type": "error", "error": {"message": e.to_string()}});
        match request["type"].as_str() {
            Some("batch") => batch(connection, request["batch"]["steps"].as_array().unwrap()),
            Some("sequence") => match connection.execute_batch(request["sql"].as_str().unwrap()) {
                Ok(()) => json!({"type": "ok", "response": {"type": "sequence"}}),
                Err(e) => failed(e),
            },
            Some("execute") => match statement(connection, &request["stmt"]) {
                Ok(result) => {
                    json!({"type": "ok", "response": {"type": "execute", "result": result}})
                }
                Err(e) => failed(e),
            },
            _ => json!({"type": "ok", "response": {"type": "close"}}),
        }
    }
}
