//! The attention answer endpoint: `fno mux serve --attention-api` serves the
//! three paths of the attention OpenAPI contract on 127.0.0.1 with its own
//! router and port, beside (never inside) the read-only web bridge. The
//! projection is the same `fno-agents needs --items --json` fold every other
//! surface reads, the answer row is the same `attention_answer` envelope the
//! attention arm appends, and the clear is the same `fno inbox outstanding
//! clear` the mux overlay runs. Authority stays `sink`: a bearer token
//! identifies a sink, never a person, and a Tailscale Serve identity header
//! (loopback bind only) is recorded as evidence, never as authority.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The contract's default port (the OpenAPI servers block).
pub const DEFAULT_ATTENTION_PORT: u16 = 8724;

/// The items fold's budget, feed_overlay's figure for a comparable
/// multi-store read.
const ITEMS_TIMEOUT: Duration = Duration::from_secs(10);

/// The clear's budget, needs_overlay's WRITE_TIMEOUT for the same verb.
const CLEAR_TIMEOUT: Duration = Duration::from_secs(45);

/// One configured sink's answer credential. The token identifies the sink;
/// it never attests a person.
#[derive(Debug, Clone, PartialEq)]
pub struct SinkAuth {
    pub name: String,
    pub token: String,
}

/// The POST body. Exactly one of `option`, `words` or `done`.
#[derive(Debug, Clone, Deserialize)]
pub struct AnswerRequest {
    pub option: Option<u32>,
    pub words: Option<String>,
    pub done: Option<bool>,
    pub idempotency_key: String,
    pub evidence: Option<String>,
}

/// The IO seam the handlers take, so the AC tests drive everything without
/// disk, a subprocess or a listener.
pub trait ApiIo {
    /// The projection: `(as_of, items)` from `fno-agents needs --items --json`.
    fn items(&mut self) -> Result<(String, Vec<Value>), String>;
    /// The question journal text (`questions.jsonl`).
    fn journal_text(&mut self) -> Result<String, String>;
    /// Append one `attention_answer` envelope to the journal.
    fn append_row(&mut self, row: &Value) -> Result<(), String>;
    /// `fno inbox outstanding clear <id> --answer <text>`.
    fn clear(&mut self, id: &str, answer_text: &str) -> Result<(), String>;
}

fn problem(status: u16, title: &str, detail: String) -> (u16, Value) {
    (
        status,
        json!({"type": "about:blank", "title": title, "status": status, "detail": detail}),
    )
}

/// Envelope-agnostic scalar read, the same data-then-top-level order
/// `fno-agents` attention.rs keeps.
fn row_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get("data")
        .and_then(|d| d.get(key))
        .or_else(|| v.get(key))
        .and_then(Value::as_str)
}

fn row_bool(v: &Value, key: &str) -> Option<bool> {
    v.get("data")
        .and_then(|d| d.get(key))
        .or_else(|| v.get(key))
        .and_then(Value::as_bool)
}

/// Every syntactically valid journal line, torn tails skipped.
fn journal_rows(text: &str) -> impl Iterator<Item = Value> + '_ {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
}

/// Whether the journal ever asked this question id.
fn journal_asked(text: &str, id: &str) -> bool {
    journal_rows(text).any(|v| {
        v.get("type").and_then(Value::as_str) == Some("operator_question")
            && row_str(&v, "question_id") == Some(id)
    })
}

/// What the journal says about earlier answers for this item.
enum Prior {
    /// A row with this exact idempotency key: replay its receipt, write none.
    Replay(Value),
    /// An unsuperseded row under another key: first answer already won.
    Won,
    /// No row at all.
    None,
}

fn prior_answer(text: &str, id: &str, idempotency_key: &str) -> Prior {
    let mut won = false;
    for v in journal_rows(text) {
        if v.get("type").and_then(Value::as_str) != Some("attention_answer")
            || row_str(&v, "item_id") != Some(id)
        {
            continue;
        }
        if row_str(&v, "idempotency_key") == Some(idempotency_key) {
            return Prior::Replay(v);
        }
        if row_bool(&v, "superseded") != Some(true) {
            won = true;
        }
    }
    if won {
        Prior::Won
    } else {
        Prior::None
    }
}

fn receipt(row: &Value, sink: &str, superseded: bool) -> Value {
    let data = row.get("data").cloned().unwrap_or(Value::Null);
    json!({
        "item_id": data.get("item_id").cloned().unwrap_or(Value::Null),
        "option": data.get("option").cloned().unwrap_or(Value::Null),
        "words": data.get("words").cloned().unwrap_or(json!("")),
        "sink": sink,
        "answered_at": data.get("answered_at").cloned().unwrap_or(Value::Null),
        "superseded": superseded,
        "authority": data.get("authority").cloned().unwrap_or(json!("sink")),
        "attested_by": data.get("attested_by").cloned().unwrap_or(Value::Null),
        "decision_id": Value::Null,
    })
}

/// The answer text the clear receives, mirroring the arm's mapping: the
/// option's text for a tick, the words, `done` for a pin.
fn answer_text_of(item: &Value, req: &AnswerRequest) -> String {
    if let Some(n) = req.option {
        let text = item
            .get("options")
            .and_then(Value::as_array)
            .and_then(|opts| {
                opts.iter()
                    .find(|o| o.get("n").and_then(Value::as_u64) == Some(n as u64))
                    .map(|o| {
                        o.get("text")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("option {n}"))
                    })
                    .or_else(|| {
                        opts.get((n as usize).saturating_sub(1))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
            })
            .unwrap_or_else(|| format!("option {n}"));
        return text;
    }
    if let Some(w) = &req.words {
        return w.clone();
    }
    "done".to_string()
}

/// GET /v1/attention/items, filtered by `state`, `kind`, `ready`, `project`.
pub fn handle_list(io: &mut dyn ApiIo, query: &HashMap<String, String>) -> (u16, Value) {
    match io.items() {
        Ok((as_of, items)) => {
            let kept: Vec<&Value> =
                items
                    .iter()
                    .filter(|i| {
                        query.get("state").is_none_or(|s| {
                            i.get("state").and_then(Value::as_str) == Some(s.as_str())
                        }) && query.get("kind").is_none_or(|k| {
                            i.get("kind").and_then(Value::as_str) == Some(k.as_str())
                        }) && query.get("ready").is_none_or(|r| {
                            r.parse::<bool>()
                                .map(|b| i.get("ready").and_then(Value::as_bool) == Some(b))
                                .unwrap_or(true)
                        }) && query.get("project").is_none_or(|p| {
                            i.get("project").and_then(Value::as_str) == Some(p.as_str())
                        })
                    })
                    .collect();
            (200, json!({"as_of": as_of, "items": kept}))
        }
        Err(e) => problem(503, "projection unavailable", e),
    }
}

/// GET /v1/attention/items/{id}.
pub fn handle_get(io: &mut dyn ApiIo, id: &str) -> (u16, Value) {
    match io.items() {
        Ok((_, items)) => match items
            .iter()
            .find(|i| i.get("id").and_then(Value::as_str) == Some(id))
        {
            Some(item) => (200, item.clone()),
            None => problem(404, "no such item", format!("no attention item {id}")),
        },
        Err(e) => problem(503, "projection unavailable", e),
    }
}

/// POST /v1/attention/items/{id}/answer. Auth first, then validation, then
/// the row, then the clear: a refused shape never lands a row, a landed row
/// is durable even when the clear fails.
pub fn handle_answer(
    io: &mut dyn ApiIo,
    sinks: &[SinkAuth],
    id: &str,
    token: Option<&str>,
    identity: Option<&str>,
    body: &[u8],
) -> (u16, Value) {
    let Some(token) = token else {
        return problem(
            401,
            "unauthorized",
            "a bearer sink token is required".into(),
        );
    };
    let Some(sink) = sinks.iter().find(|s| s.token == token) else {
        return problem(
            401,
            "unauthorized",
            "the bearer token matches no configured sink".into(),
        );
    };
    let req: AnswerRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return problem(
                422,
                "invalid request",
                format!("not a valid AnswerRequest: {e}"),
            )
        }
    };
    let provided =
        req.option.is_some() as u8 + req.words.is_some() as u8 + req.done.unwrap_or(false) as u8;
    if provided != 1 {
        return problem(
            422,
            "invalid request",
            "exactly one of option, words or done is required".into(),
        );
    }
    if req.idempotency_key.trim().is_empty() {
        return problem(422, "invalid request", "idempotency_key is required".into());
    }
    let (_, items) = match io.items() {
        Ok(p) => p,
        Err(e) => return problem(503, "projection unavailable", e),
    };
    let Some(item) = items
        .iter()
        .find(|i| i.get("id").and_then(Value::as_str) == Some(id))
        .cloned()
    else {
        // Absent from the open set: asked-ever decides 404 vs 409.
        return match io.journal_text() {
            Ok(text) if journal_asked(&text, id) => problem(
                409,
                "already closed",
                format!("item {id} is closed; nothing was recorded"),
            ),
            Ok(_) => problem(404, "no such item", format!("no attention item {id}")),
            Err(e) => problem(503, "journal unreadable", e),
        };
    };
    // Every refusal below lands no row (AC15-ERR).
    if let Some(n) = req.option {
        let len = item
            .get("options")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        if n == 0 || n as usize > len {
            return problem(
                422,
                "option out of range",
                format!("item {id} has {len} options"),
            );
        }
    }
    if req.done == Some(true) && item.get("kind").and_then(Value::as_str) != Some("pin") {
        return problem(
            422,
            "invalid request",
            "done answers a pin; name an option or write words".into(),
        );
    }
    let journal = match io.journal_text() {
        Ok(t) => t,
        Err(e) => return problem(503, "journal unreadable", e),
    };
    match prior_answer(&journal, id, &req.idempotency_key) {
        Prior::Replay(row) => {
            let superseded = row_bool(&row, "superseded").unwrap_or(false);
            let sink_name = row_str(&row, "sink").unwrap_or(&sink.name).to_string();
            (200, receipt(&row, &sink_name, superseded))
        }
        Prior::Won => {
            let row = answer_row(id, &sink.name, &req, identity, true);
            if let Err(e) = io.append_row(&row) {
                return problem(500, "row append failed", e);
            }
            (200, receipt(&row, &sink.name, true))
        }
        Prior::None => {
            let row = answer_row(id, &sink.name, &req, identity, false);
            if let Err(e) = io.append_row(&row) {
                return problem(500, "row append failed", e);
            }
            // The row is the durable half; a failed clear is logged and the
            // receipt still reads recorded, exactly like the file lane.
            let text = answer_text_of(&item, &req);
            if let Err(e) = io.clear(id, &text) {
                eprintln!("fno attention api: clear failed for {id}: {e}");
            }
            (200, receipt(&row, &sink.name, false))
        }
    }
}

/// The `attention_answer` envelope: the arm's row shape plus the endpoint's
/// provenance keys (open data keys validate; see the events schema contract).
fn answer_row(
    id: &str,
    sink_name: &str,
    req: &AnswerRequest,
    identity: Option<&str>,
    superseded: bool,
) -> Value {
    let now = chrono::Utc::now().to_rfc3339();
    let evidence = req
        .evidence
        .clone()
        .unwrap_or_else(|| req.idempotency_key.clone());
    json!({
        "ts": now,
        "type": "attention_answer",
        "source": "endpoint",
        "data": {
            "item_id": id,
            "sink": sink_name,
            "option": req.option,
            "words": req.words.clone().unwrap_or_default(),
            "done": req.done.unwrap_or(false),
            "answered_at": now,
            "authority": "sink",
            "attested_by": format!("{sink_name}:{evidence}"),
            "mapped_by": "attention_api",
            "superseded": superseded,
            "decision_id": Value::Null,
            "idempotency_key": req.idempotency_key,
            "identity": identity,
        }
    })
}

// ---------------------------------------------------------------------------
// Config and IO
// ---------------------------------------------------------------------------

/// The `.fno` state base, the same anchor as agents_view's (`FNO_AGENTS_HOME`'s
/// parent > `$HOME/.fno`). Mirrored here: agents_view is over the shrink-only
/// line budget, so it takes no new surface.
fn fno_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_AGENTS_HOME") {
        return PathBuf::from(&v)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".fno"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".fno")
}

fn journal_path() -> PathBuf {
    fno_dir().join("questions.jsonl")
}

/// Sink tokens for the answer door: `[[attention.sinks]]` rows in the cwd
/// config then the global config, a name seen once keeps its first token.
pub fn load_sink_auth() -> Vec<SinkAuth> {
    let mut candidates = vec![std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".fno/config.toml")];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".fno/config.toml"));
    }
    let mut out: Vec<SinkAuth> = Vec::new();
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = text.parse::<toml::Value>() else {
            continue;
        };
        let Some(rows) = v
            .get("attention")
            .and_then(|a| a.get("sinks"))
            .and_then(|s| s.as_array())
        else {
            continue;
        };
        for row in rows {
            let Some(name) = row.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            let Some(env_name) = row.get("token_env").and_then(|t| t.as_str()) else {
                continue;
            };
            let Ok(token) = std::env::var(env_name) else {
                continue;
            };
            if token.is_empty() || out.iter().any(|s| s.name == name) {
                continue;
            }
            out.push(SinkAuth {
                name: name.to_string(),
                token,
            });
        }
    }
    out
}

/// One bounded blocking subprocess: stdout to a private temp file (a pipe
/// read blocks on EOF past the child, the file never does), bounded
/// try_wait/kill, the server.rs `config_get` shape.
fn run_bounded(bin: PathBuf, args: &[&str], bound: Duration) -> Result<(bool, String), String> {
    use std::process::Stdio;
    let out_path = std::env::temp_dir().join(format!(
        "fno-attention-api-{}-{}.out",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let out_file = std::fs::File::create(&out_path).map_err(|e| e.to_string())?;
    let mut command = crate::process_admission::std_command(bin);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(out_file)
        .stderr(Stdio::null());
    let mut child = crate::process_admission::std_spawn(&mut command).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + bound;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("timed out".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => break Err(e.to_string()),
        }
    };
    let text = std::fs::read_to_string(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    let success = status.as_ref().is_ok_and(|s| s.success());
    status.map_err(|e| format!("{}: {e}", args.first().unwrap_or(&"child")))?;
    Ok((success, text))
}

/// The real IO: the items fold shells `fno-agents needs --items --json`, the
/// journal is `questions.jsonl`, the clear shells the same verb the mux
/// overlay runs.
#[derive(Default)]
pub struct RealIo;

impl ApiIo for RealIo {
    fn items(&mut self) -> Result<(String, Vec<Value>), String> {
        let (success, text) = run_bounded(
            crate::digest_overlay::fno_agents_bin(),
            &["needs", "--items", "--json"],
            ITEMS_TIMEOUT,
        )?;
        if !success {
            return Err("needs --items exited non-zero".to_string());
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| format!("needs --items returned malformed JSON: {e}"))?;
        let as_of = v
            .get("as_of")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let items = v
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok((as_of, items))
    }

    fn journal_text(&mut self) -> Result<String, String> {
        std::fs::read_to_string(journal_path()).map_err(|e| format!("questions.jsonl: {e}"))
    }

    fn append_row(&mut self, row: &Value) -> Result<(), String> {
        use std::io::Write;
        let path = journal_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        writeln!(f, "{row}").map_err(|e| e.to_string())
    }

    fn clear(&mut self, id: &str, answer_text: &str) -> Result<(), String> {
        let (success, stderr) = run_bounded(
            crate::server::fno_bin(),
            &["inbox", "outstanding", "clear", id, "--answer", answer_text],
            CLEAR_TIMEOUT,
        )?;
        if success {
            return Ok(());
        }
        let stderr = stderr.trim();
        Err(if stderr.is_empty() {
            "clear exited non-zero".to_string()
        } else {
            stderr.chars().take(400).collect()
        })
    }
}

// ---------------------------------------------------------------------------
// axum layer
// ---------------------------------------------------------------------------

pub fn router() -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/v1/attention/items", get(list))
        .route("/v1/attention/items/{id}/answer", post(answer))
        .route("/v1/attention/items/{id}", get(get_one))
}

fn to_response(r: (u16, Value)) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Json};
    let status = StatusCode::from_u16(r.0).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(r.1)).into_response()
}

async fn list(
    axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
) -> axum::response::Response {
    let (status, body) = tokio::task::spawn_blocking(move || {
        let mut io = RealIo;
        handle_list(&mut io, &query)
    })
    .await
    .unwrap_or_else(|e| problem(500, "handler failed", e.to_string()));
    to_response((status, body))
}

async fn get_one(axum::extract::Path(id): axum::extract::Path<String>) -> axum::response::Response {
    let (status, body) = tokio::task::spawn_blocking(move || {
        let mut io = RealIo;
        handle_get(&mut io, &id)
    })
    .await
    .unwrap_or_else(|e| problem(500, "handler failed", e.to_string()));
    to_response((status, body))
}

async fn answer(
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let token: Option<String> = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let identity: Option<String> = headers
        .get("tailscale-user-login")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let sinks = load_sink_auth();
    let (status, resp) = tokio::task::spawn_blocking(move || {
        let mut io = RealIo;
        handle_answer(
            &mut io,
            &sinks,
            &id,
            token.as_deref(),
            identity.as_deref(),
            &body,
        )
    })
    .await
    .unwrap_or_else(|e| problem(500, "handler failed", e.to_string()));
    to_response((status, resp))
}

/// Bind 127.0.0.1 and serve; loopback is not configurable (the contract
/// binds localhost; remote reach is the user's own tunnel).
pub async fn serve(port: u16) -> Result<(), String> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot bind {addr}: {e}"))?;
    println!("fno attention api: http://{addr}/v1/attention/items (sink token required)");
    axum::serve(listener, router())
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(name: &str, token: &str) -> SinkAuth {
        SinkAuth {
            name: name.to_string(),
            token: token.to_string(),
        }
    }

    fn item_json(id: &str, kind: &str, options: u32) -> Value {
        let opts: Vec<Value> = (1..=options)
            .map(|n| json!({"n": n, "text": format!("option {n} text"), "next": "x", "pros": [], "cons": []}))
            .collect();
        json!({
            "id": id, "kind": kind, "title": format!("title {id}"),
            "project": "fno", "priority": "high", "created_at": "2026-09-26T00:00:00Z",
            "ready": true, "missing": [], "state": "open", "options": opts,
        })
    }

    struct MemIo {
        items: Vec<Value>,
        journal: String,
        rows: Vec<Value>,
        clears: Vec<(String, String)>,
    }

    impl Default for MemIo {
        fn default() -> Self {
            MemIo {
                items: vec![],
                journal: String::new(),
                rows: vec![],
                clears: vec![],
            }
        }
    }

    impl MemIo {
        fn asked(id: &str) -> String {
            format!(
                r#"{{"ts":"2026-09-26T00:00:00Z","type":"operator_question","source":"t","data":{{"question_id":"{id}","question":"why"}}}}"#
            ) + "\n"
        }

        fn closed_row(id: &str) -> String {
            format!(
                r#"{{"ts":"2026-09-26T01:00:00Z","type":"operator_question_closed","source":"t","data":{{"question_id":"{id}"}}}}"#
            )
        }
    }

    impl ApiIo for MemIo {
        fn items(&mut self) -> Result<(String, Vec<Value>), String> {
            Ok(("2026-09-26T02:00:00Z".to_string(), self.items.clone()))
        }
        fn journal_text(&mut self) -> Result<String, String> {
            Ok(self.journal.clone())
        }
        fn append_row(&mut self, row: &Value) -> Result<(), String> {
            self.rows.push(row.clone());
            self.journal.push_str(&format!("{row}\n"));
            Ok(())
        }
        fn clear(&mut self, id: &str, answer_text: &str) -> Result<(), String> {
            self.clears.push((id.to_string(), answer_text.to_string()));
            Ok(())
        }
    }

    fn answer_body(option: u32, key: &str) -> Vec<u8> {
        format!(r#"{{"option": {option}, "idempotency_key": "{key}"}}"#).into_bytes()
    }

    /// AC15-HP: a valid token and an open item -> 200 superseded false, one
    /// attention_answer row naming the sink, and the same clear.
    #[test]
    fn ac15_hp_a_valid_answer_records_one_row_and_clears() {
        let mut io = MemIo::default();
        io.items = vec![item_json("q-1", "question", 2)];
        io.journal = MemIo::asked("q-1");
        let sinks = vec![sink("phone", "tok-1")];
        let (status, body) = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("tok-1"),
            None,
            &answer_body(2, "k"),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["superseded"], false);
        assert_eq!(body["sink"], "phone");
        assert_eq!(body["authority"], "sink");
        assert_eq!(io.rows.len(), 1, "one row");
        assert_eq!(io.rows[0]["type"], "attention_answer");
        assert_eq!(io.rows[0]["data"]["sink"], "phone");
        assert_eq!(io.rows[0]["data"]["superseded"], false);
        assert_eq!(io.clears, vec![("q-1".into(), "option 2 text".into())]);
    }

    /// AC15-ERR: a closed item 409s, a bad token 401s, an out-of-range
    /// option 422s - and none of the three lands a row.
    #[test]
    fn ac15_err_closed_bad_token_and_out_of_range_land_no_row() {
        let sinks = vec![sink("phone", "tok-1")];

        // Closed: asked but absent from the open set.
        let mut io = MemIo::default();
        io.items = vec![];
        io.journal = format!("{}\n{}\n", MemIo::asked("q-c"), MemIo::closed_row("q-c"));
        let (status, body) = handle_answer(
            &mut io,
            &sinks,
            "q-c",
            Some("tok-1"),
            None,
            &answer_body(1, "k"),
        );
        assert_eq!(status, 409, "{body}");
        assert!(io.rows.is_empty());

        // Bad token.
        let mut io = MemIo::default();
        io.items = vec![item_json("q-1", "question", 2)];
        let (status, _) = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("wrong"),
            None,
            &answer_body(1, "k"),
        );
        assert_eq!(status, 401);
        assert!(io.rows.is_empty());

        // Option out of range.
        let (status, body) = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("tok-1"),
            None,
            &answer_body(5, "k"),
        );
        assert_eq!(status, 422, "{body}");
        assert!(io.rows.is_empty());

        // Unknown id, never asked.
        let (status, _) = handle_answer(
            &mut io,
            &sinks,
            "q-none",
            Some("tok-1"),
            None,
            &answer_body(1, "k"),
        );
        assert_eq!(status, 404);
    }

    #[test]
    fn exactly_one_of_option_words_done_is_enforced() {
        let mut io = MemIo::default();
        io.items = vec![item_json("q-1", "question", 2)];
        let sinks = vec![sink("phone", "tok-1")];
        // None of the three.
        let (status, _) = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("tok-1"),
            None,
            br#"{"idempotency_key": "k"}"#,
        );
        assert_eq!(status, 422);
        // Two of the three.
        let (status, _) = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("tok-1"),
            None,
            br#"{"option": 1, "done": true, "idempotency_key": "k"}"#,
        );
        assert_eq!(status, 422);
        // Missing key.
        let (status, _) = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("tok-1"),
            None,
            br#"{"option": 1}"#,
        );
        assert_eq!(status, 422);
        assert!(io.rows.is_empty());
    }

    /// A retry under the same idempotency key replays the receipt and lands
    /// no second row (idempotent on idempotency_key).
    #[test]
    fn a_same_key_retry_replays_without_a_second_row() {
        let mut io = MemIo::default();
        io.items = vec![item_json("q-1", "question", 2)];
        io.journal = MemIo::asked("q-1");
        let sinks = vec![sink("phone", "tok-1")];
        let (s1, b1) = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("tok-1"),
            None,
            &answer_body(1, "k"),
        );
        assert_eq!(s1, 200);
        assert_eq!(io.rows.len(), 1);
        let (s2, b2) = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("tok-1"),
            None,
            &answer_body(1, "k"),
        );
        assert_eq!(s2, 200);
        assert_eq!(io.rows.len(), 1, "no second row");
        assert_eq!(b1["answered_at"], b2["answered_at"]);
        assert_eq!(io.clears.len(), 1, "the clear runs once");
    }

    /// A different key after a won answer lands a superseded marker that
    /// changes nothing and runs no clear.
    #[test]
    fn a_later_answer_records_superseded_and_clears_nothing() {
        let mut io = MemIo::default();
        io.items = vec![item_json("q-1", "question", 2)];
        io.journal = MemIo::asked("q-1");
        let sinks = vec![sink("phone", "tok-1")];
        let _ = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("tok-1"),
            None,
            &answer_body(1, "k1"),
        );
        let (status, body) = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("tok-1"),
            None,
            &answer_body(2, "k2"),
        );
        assert_eq!(status, 200);
        assert_eq!(body["superseded"], true);
        assert_eq!(io.rows.len(), 2);
        assert_eq!(io.rows[1]["data"]["superseded"], true);
        assert_eq!(io.rows[1]["data"]["idempotency_key"], "k2");
        assert_eq!(io.clears.len(), 1, "no second clear");
    }

    #[test]
    fn done_answers_a_pin_and_is_refused_on_a_question() {
        let sinks = vec![sink("phone", "tok-1")];
        let mut io = MemIo::default();
        io.items = vec![item_json("p-1", "pin", 0)];
        io.journal = MemIo::asked("p-1");
        let (status, _) = handle_answer(
            &mut io,
            &sinks,
            "p-1",
            Some("tok-1"),
            None,
            br#"{"done": true, "idempotency_key": "k"}"#,
        );
        assert_eq!(status, 200);
        assert_eq!(io.clears, vec![("p-1".into(), "done".into())]);

        let mut io = MemIo::default();
        io.items = vec![item_json("q-1", "question", 2)];
        let (status, _) = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("tok-1"),
            None,
            br#"{"done": true, "idempotency_key": "k"}"#,
        );
        assert_eq!(status, 422);
        assert!(io.rows.is_empty());
    }

    #[test]
    fn list_filters_by_state_kind_ready_and_project() {
        let mut io = MemIo::default();
        io.items = vec![
            item_json("q-1", "question", 2),
            {
                let mut v = item_json("q-2", "pin", 0);
                v["project"] = json!("other");
                v
            },
            {
                let mut v = item_json("q-3", "question", 2);
                v["ready"] = json!(false);
                v
            },
        ];
        let mut q = HashMap::new();
        q.insert("kind".to_string(), "question".to_string());
        let (status, body) = handle_list(&mut io, &q);
        assert_eq!(status, 200);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);

        let mut q = HashMap::new();
        q.insert("project".to_string(), "other".to_string());
        let (_, body) = handle_list(&mut io, &q);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["items"][0]["id"], "q-2");

        let mut q = HashMap::new();
        q.insert("ready".to_string(), "false".to_string());
        let (_, body) = handle_list(&mut io, &q);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["items"][0]["id"], "q-3");
    }

    #[test]
    fn a_projection_failure_reads_503_and_a_garbage_body_422() {
        struct Broken;
        impl ApiIo for Broken {
            fn items(&mut self) -> Result<(String, Vec<Value>), String> {
                Err("needs: spawn failed".into())
            }
            fn journal_text(&mut self) -> Result<String, String> {
                Err("no journal".into())
            }
            fn append_row(&mut self, _row: &Value) -> Result<(), String> {
                Err("no disk".into())
            }
            fn clear(&mut self, _id: &str, _text: &str) -> Result<(), String> {
                Ok(())
            }
        }
        let mut io = Broken;
        let sinks = vec![sink("phone", "tok-1")];
        let (status, _) = handle_answer(
            &mut io,
            &sinks,
            "q-1",
            Some("tok-1"),
            None,
            &answer_body(1, "k"),
        );
        assert_eq!(status, 503);
        let (status, _) = handle_answer(&mut io, &sinks, "q-1", Some("tok-1"), None, b"not json");
        assert_eq!(status, 422);
    }
}
