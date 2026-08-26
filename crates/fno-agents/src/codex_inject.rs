//! `mail-inject --harness codex`: LIVE delivery into a running codex session
//! over the codex app-server daemon socket (US8, node x-d899). The codex sibling
//! of [`crate::mail_inject`]'s claude `control.sock` path. Python's send path
//! (`_mail_inject_codex`) runs this as a subprocess and falls back to the durable
//! bus ONLY when it reports not-delivered (live-inject-first).
//!
//! Transport = JSON-RPC text frames over a WebSocket over a Unix socket (mirrors
//! [`crate::logs_client`]). Unlike the claude path, the `turn/start` RPC RESPONSE
//! is itself the delivery confirmation (the daemon accepts and queues the turn
//! synchronously), so there is no transcript growth-poll.
//!
//! # The daemon prerequisite (why this can be a no-op)
//!
//! A default `codex` TUI runs its app-server IN-PROCESS with no socket on disk.
//! The socket exists ONLY when a codex app-server daemon is running
//! (`codex app-server daemon start`, standalone install + ChatGPT login); TUIs
//! launched afterward auto-attach to it. Absent that daemon, `deliver_via_codex_daemon`
//! returns `"no-daemon"` and the caller writes the durable floor. e2e verification
//! needs the user's daemon; the pure builders + `classify_turn_start_response`
//! below are the correct-by-construction unit-tested core.
//!
//! Protocol map verified against `~/code/tools/codex/codex-rs/` (app-server-protocol
//! rpc.rs / v2/turn.rs, app-server-client remote.rs initialize handshake).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

/// Overall budget for connect + handshake + turn/start round-trip. The daemon
/// responds promptly; this only bounds a wedged socket so the verb cannot hang.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Frame-skip ceiling per read: notifications and unrelated ids are skipped, but
/// a chatty (or silent) socket must not loop forever before the timeout fires.
const MAX_FRAMES: usize = 64;

const LOADED_PAGE_SIZE: u32 = 100;
const MAX_LOADED_PAGES: usize = 64;

#[derive(Debug, serde::Serialize, PartialEq, Eq)]
pub struct LoadedThread {
    pub session_id: String,
    pub cwd: String,
}

/// The shared codex app-server control socket: `$CODEX_HOME/app-server-control/
/// app-server-control.sock` (CODEX_HOME defaults to `~/.codex`). Absent unless a
/// codex app-server daemon is running.
pub fn codex_app_server_socket_path() -> PathBuf {
    let home = std::env::var("CODEX_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}/.codex", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(home)
        .join("app-server-control")
        .join("app-server-control.sock")
}

/// The Codex daemon's recorded process identity. The state file is provider
/// owned, so a pid without `processStartTime` is unreadable rather than a
/// positive liveness signal.
pub struct CodexDaemonAdapter {
    provider_state_path: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
    socket_path: PathBuf,
}

impl CodexDaemonAdapter {
    pub fn new(provider_state_path: PathBuf, lock_path: PathBuf, socket_path: PathBuf) -> Self {
        let state_path = provider_state_path
            .parent()
            .map(|parent| parent.join("fno-harness-daemon.json"))
            .unwrap_or_else(|| PathBuf::from("fno-harness-daemon.json"));
        Self {
            provider_state_path,
            state_path,
            lock_path,
            socket_path,
        }
    }

    pub fn from_environment() -> Self {
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
            .unwrap_or_else(|| PathBuf::from(".codex"));
        Self::new(
            home.join("app-server-daemon").join("app-server.pid"),
            home.join("app-server-daemon")
                .join("fno-harness-daemon.lock"),
            home.join("app-server-control")
                .join("app-server-control.sock"),
        )
    }

    fn provider_state(&self) -> Result<crate::harness_daemon::DaemonState, String> {
        let raw = std::fs::read_to_string(&self.provider_state_path)
            .map_err(|error| format!("read Codex daemon state: {error}"))?;
        <Self as crate::harness_daemon::HarnessDaemonAdapter>::parse_state(self, &raw)
    }
}

impl crate::harness_daemon::HarnessDaemonAdapter for CodexDaemonAdapter {
    fn harness(&self) -> &str {
        "codex"
    }

    fn state_path(&self) -> &Path {
        &self.state_path
    }

    fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    fn parse_state(&self, raw: &str) -> Result<crate::harness_daemon::DaemonState, String> {
        let mut value: serde_json::Value =
            serde_json::from_str(raw).map_err(|error| error.to_string())?;
        let pid = value
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .filter(|pid| *pid > 0)
            .ok_or_else(|| "missing pid".to_string())? as u32;
        let process_start_time = ["processStartToken", "process_start_time", "startTime"]
            .into_iter()
            .find_map(|key| value.get(key).and_then(json_u64))
            .or_else(|| {
                value
                    .get("processStartTime")
                    .and_then(serde_json::Value::as_u64)
            })
            .or_else(|| {
                value
                    .get("processStartTime")
                    .and_then(serde_json::Value::as_str)
                    .filter(|start| !start.is_empty())
                    .and_then(|_| crate::daemon::process_start_time(pid))
            })
            .filter(|start| *start > 0)
            .ok_or_else(|| "missing process start time".to_string())?;
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "processStartToken".to_string(),
                serde_json::Value::from(process_start_time),
            );
        }
        crate::harness_daemon::DaemonState::new(
            value,
            pid,
            process_start_time,
            self.socket_path.to_string_lossy(),
            format!("{pid}:{process_start_time}"),
        )
    }

    fn is_healthy(&self, state: &crate::harness_daemon::DaemonState) -> bool {
        let process_is_current = state
            .pid
            .is_some_and(|pid| crate::daemon::pid_is_ours(pid, state.process_start_time));
        process_is_current && self.socket_path.exists() && probe_codex_app_server(&self.socket_path)
    }

    fn may_replace(&self) -> bool {
        false
    }

    fn may_refresh_unhealthy(&self) -> bool {
        true
    }

    fn unreadable_state_may_boot(&self) -> bool {
        true
    }

    fn boot(&self) -> Result<crate::harness_daemon::DaemonState, String> {
        if let Ok(state) = self.provider_state() {
            if self.is_healthy(&state) {
                return Ok(state);
            }
        }
        // The daemon is a detached service: its startup output must never
        // land on the spawning client's stdout, whose bytes are the reply
        // stream by contract (`.status()` inherits, and an inherited child
        // printed its banner straight into a one-shot spawn's reply).
        let status = std::process::Command::new("codex")
            .args(["app-server", "daemon", "start"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|error| format!("codex app-server daemon start: {error}"))?;
        if !status.success() {
            return Err(format!(
                "codex app-server daemon start exited {}",
                status.code().unwrap_or(1)
            ));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(state) = self.provider_state() {
                if self.is_healthy(&state) {
                    return Ok(state);
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "Codex daemon did not pass initialize handshake within 15s ({})",
                    self.socket_path.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn json_u64(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

/// Ensure the Codex app-server daemon before a client-side spawn.
pub fn ensure_codex_daemon() -> Result<crate::harness_daemon::DaemonReceipt, String> {
    crate::harness_daemon::ensure_harness_daemon(&CodexDaemonAdapter::from_environment())
        .map(|result| result.receipt)
        .map_err(|error| error.to_string())
}

/// Positive Codex app-server health: the control socket must complete the
/// initialize handshake. Socket existence alone is deliberately insufficient.
pub fn probe_codex_app_server(socket_path: &Path) -> bool {
    if tokio::runtime::Handle::try_current().is_ok() {
        let socket_path = socket_path.to_path_buf();
        return std::thread::spawn(move || probe_codex_app_server_fresh(&socket_path))
            .join()
            .unwrap_or(false);
    }
    probe_codex_app_server_fresh(socket_path)
}

fn probe_codex_app_server_fresh(socket_path: &Path) -> bool {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return false,
    };
    runtime.block_on(async {
        codex_initialize_handshake_with_timeout(socket_path, HANDSHAKE_TIMEOUT)
            .await
            .is_ok()
    })
}

async fn codex_initialize_handshake_with_timeout(
    socket_path: &Path,
    timeout: Duration,
) -> Result<(), &'static str> {
    match tokio::time::timeout(timeout, codex_initialize_handshake(socket_path)).await {
        Ok(result) => result,
        Err(_) => Err("timeout"),
    }
}

async fn codex_initialize_handshake(socket_path: &Path) -> Result<(), &'static str> {
    let conn = UnixStream::connect(socket_path)
        .await
        .map_err(|_| "connect-failed")?;
    let ws = tokio_tungstenite::client_async("ws://localhost/rpc", conn)
        .await
        .map_err(|_| "handshake-failed")?;
    let (mut sink, mut stream) = ws.0.split();
    sink.send(Message::Text(initialize_request_json().into()))
        .await
        .map_err(|_| "send-failed")?;
    read_until_id(&mut stream, &serde_json::json!("init")).await?;
    sink.send(Message::Text(initialized_notification_json().into()))
        .await
        .map_err(|_| "send-failed")?;
    Ok(())
}

/// The `initialize` request frame. Local socket needs no auth/pairing — just the
/// handshake. `id` is the string `"init"` (matched on the response). Note the
/// absence of a `"jsonrpc":"2.0"` field: the codex app-server carries bare
/// JSON-RPC text frames.
pub fn initialize_request_json() -> String {
    serde_json::json!({
        "id": "init",
        "method": "initialize",
        "params": {
            "clientInfo": {"name": "fno-mail-inject", "version": "0.1.0"},
            "capabilities": {"experimentalApi": true}
        }
    })
    .to_string()
}

/// The `initialized` notification (no `id`) sent after the initialize response.
pub fn initialized_notification_json() -> String {
    serde_json::json!({"method": "initialized"}).to_string()
}

/// The `turn/start` request injecting `text` into `thread_id` as a text input
/// item. `id` is `1` (matched on the response). `text` is injected verbatim —
/// the `<fno_mail>` envelope is rendered caller-side (Python), so this is a dumb
/// transport, mirroring [`crate::mail_inject`].
pub fn turn_start_request_json(thread_id: &str, text: &str) -> String {
    serde_json::json!({
        "id": 1,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{"type": "text", "text": text}]
        }
    })
    .to_string()
}

/// `review/start` target: what the codex reviewer diffs against. Mirrors the
/// app-server protocol's `ReviewTarget` enum (verified live against the daemon,
/// node x-c24d): a structured target beats a typed `/review <prose>`, which only
/// yields `{type:custom, instructions}` with no computed merge base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTarget {
    UncommittedChanges,
    BaseBranch { branch: String },
    Commit { sha: String, title: Option<String> },
    Custom { instructions: String },
}

impl ReviewTarget {
    fn to_json(&self) -> serde_json::Value {
        match self {
            ReviewTarget::UncommittedChanges => json!({"type": "uncommittedChanges"}),
            ReviewTarget::BaseBranch { branch } => json!({"type": "baseBranch", "branch": branch}),
            ReviewTarget::Commit { sha, title } => {
                let mut o = json!({"type": "commit", "sha": sha});
                if let Some(t) = title {
                    o["title"] = json!(t);
                }
                o
            }
            ReviewTarget::Custom { instructions } => {
                json!({"type": "custom", "instructions": instructions})
            }
        }
    }
}

/// `review/start` delivery: `inline` runs the review on the worker's own thread
/// (its current cwd), `detached` forks a new thread that inherits the session's
/// ORIGINAL cwd - the cwd trap (node x-c24d). Default inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDelivery {
    Inline,
    Detached,
}

impl ReviewDelivery {
    fn to_json(self) -> &'static str {
        match self {
            ReviewDelivery::Inline => "inline",
            ReviewDelivery::Detached => "detached",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewStartError {
    Reason(&'static str),
    Server(String),
}

impl std::fmt::Display for ReviewStartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reason(reason) => formatter.write_str(reason),
            Self::Server(message) => write!(formatter, "server-error: {message}"),
        }
    }
}

impl std::error::Error for ReviewStartError {}

impl From<&'static str> for ReviewStartError {
    fn from(reason: &'static str) -> Self {
        Self::Reason(reason)
    }
}

/// The `review/start` request. `id` is `1` (matched on the response). The
/// response is an OUTCOME receipt (a `Turn` with a status plus a
/// `reviewThreadId` to read findings from), strictly better than the inject
/// path's transport boolean.
pub fn review_start_request_json(
    thread_id: &str,
    target: &ReviewTarget,
    delivery: ReviewDelivery,
) -> String {
    json!({
        "id": 1,
        "method": "review/start",
        "params": {
            "threadId": thread_id,
            "target": target.to_json(),
            "delivery": delivery.to_json()
        }
    })
    .to_string()
}

/// Parse a `review/start` response into the outcome receipt.
/// `.result.turn.id` + `.result.reviewThreadId` is accepted. Protocol reason
/// tokens and server-provided messages remain separate error variants.
pub fn parse_review_start_response(raw: &str) -> Result<(String, String), ReviewStartError> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| ReviewStartError::Reason("invalid-response"))?;
    if let Some(result) = v.get("result") {
        let turn_id = result
            .get("turn")
            .and_then(|t| t.get("id"))
            .and_then(|id| id.as_str())
            .unwrap_or("");
        let review_thread = result
            .get("reviewThreadId")
            .and_then(|id| id.as_str())
            .unwrap_or("");
        if !turn_id.is_empty() && !review_thread.is_empty() {
            return Ok((turn_id.to_string(), review_thread.to_string()));
        }
        if !turn_id.is_empty() || !review_thread.is_empty() {
            return Err(ReviewStartError::Reason("not-confirmed"));
        }
    }
    if let Some(error) = v.get("error") {
        let message = error
            .get("message")
            .and_then(|message| message.as_str())
            .filter(|message| !message.is_empty())
            .map(str::to_string)
            .ok_or(ReviewStartError::Reason("unexpected-response"))?;
        return Err(ReviewStartError::Server(message));
    }
    Err(ReviewStartError::Reason("unexpected-response"))
}

/// Drive a `review/start` over the app-server daemon socket. `Ok((turn_id,
/// review_thread_id))` on accept; every `Err(reason)` is a clean not-delivered
/// signal. Socket absent -> `"no-daemon"`; a pre-send wedge -> `"io-error"`;
/// a lost response after the request was sent -> `"not-confirmed"`.
pub async fn deliver_via_codex_review_start(
    thread_id: &str,
    target: &ReviewTarget,
    delivery: ReviewDelivery,
) -> Result<(String, String), ReviewStartError> {
    let sock = codex_app_server_socket_path();
    if !sock.exists() {
        return Err(ReviewStartError::Reason("no-daemon"));
    }
    let mut request_in_flight = false;
    match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        review_start_round_trip(&sock, thread_id, target, delivery, &mut request_in_flight),
    )
    .await
    {
        Ok(r) => r,
        Err(_) if request_in_flight => Err(ReviewStartError::Reason("not-confirmed")),
        Err(_) => Err(ReviewStartError::Reason("io-error")),
    }
}

/// Connect + initialize handshake + `review/start` round-trip. Split out so the
/// timeout wrapper in [`deliver_via_codex_review_start`] bounds a wedged socket.
async fn review_start_round_trip(
    sock: &Path,
    thread_id: &str,
    target: &ReviewTarget,
    delivery: ReviewDelivery,
    request_in_flight: &mut bool,
) -> Result<(String, String), ReviewStartError> {
    let conn = UnixStream::connect(sock)
        .await
        .map_err(|_| ReviewStartError::Reason("io-error"))?;
    let ws = match tokio_tungstenite::client_async("ws://localhost/rpc", conn).await {
        Ok((ws, _resp)) => ws,
        Err(_) => return Err(ReviewStartError::Reason("handshake-failed")),
    };
    let (mut sink, mut stream) = ws.split();
    sink.send(Message::Text(initialize_request_json().into()))
        .await
        .map_err(|_| ReviewStartError::Reason("io-error"))?;
    read_until_id(&mut stream, &serde_json::json!("init")).await?;
    sink.send(Message::Text(initialized_notification_json().into()))
        .await
        .map_err(|_| ReviewStartError::Reason("io-error"))?;
    *request_in_flight = true;
    sink.send(Message::Text(
        review_start_request_json(thread_id, target, delivery).into(),
    ))
    .await
    // A failed SEND is a pre-send wedge (nothing delivered), not a lost
    // response; only the timeout layer's request_in_flight remap and the read
    // below may claim "not-confirmed".
    .map_err(|_| ReviewStartError::Reason("io-error"))?;
    let resp = read_until_id(&mut stream, &serde_json::json!(1))
        .await
        .map_err(|_| ReviewStartError::Reason("not-confirmed"))?;
    parse_review_start_response(&resp)
}

fn emit_review_invocation_fields(
    emitter: &crate::events::EventEmitter,
    fields: serde_json::Map<String, serde_json::Value>,
) {
    let _ = emitter.emit_fields("review_invocation", fields);
}

#[cfg(test)]
fn review_start_audit_fields(
    thread_id: &str,
    target_raw: &str,
    delivery: ReviewDelivery,
    audit_payload: Option<&str>,
    audit_sender: Option<&str>,
    audit_target_cwd: Option<&str>,
    failure_reason: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    review_start_audit_fields_with_origin(
        thread_id,
        target_raw,
        delivery,
        audit_payload,
        audit_sender,
        audit_target_cwd,
        failure_reason,
        None,
    )
}

fn review_start_audit_fields_with_origin(
    thread_id: &str,
    target_raw: &str,
    delivery: ReviewDelivery,
    audit_payload: Option<&str>,
    audit_sender: Option<&str>,
    audit_target_cwd: Option<&str>,
    failure_reason: Option<&str>,
    audit_origin: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut fields = serde_json::Map::new();
    fields.insert("target_session".into(), thread_id.into());
    fields.insert(
        "payload".into(),
        audit_payload
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "review/start {} delivery={}",
                    target_raw,
                    delivery.to_json()
                )
            })
            .into(),
    );
    fields.insert("harness".into(), "codex".into());
    fields.insert("lane".into(), "codex-review-start".into());
    fields.insert("confirmed".into(), failure_reason.is_none().into());
    if let Some(sender) = audit_sender {
        fields.insert("sender".into(), sender.into());
    }
    if let Some(cwd) = audit_target_cwd {
        fields.insert("target_cwd".into(), cwd.into());
    }
    if let Some(origin) = audit_origin {
        fields.insert("origin".into(), origin.into());
    }
    if let Some(reason) = failure_reason {
        fields.insert("reason".into(), reason.into());
    }
    fit_review_start_audit_fields(&mut fields);
    fields
}

fn fit_review_start_audit_fields(fields: &mut serde_json::Map<String, serde_json::Value>) {
    if review_start_audit_payload_len(fields) <= crate::events::MAX_EVENT_PAYLOAD_BYTES {
        return;
    }
    for key in [
        "reason",
        "target_cwd",
        "sender",
        "origin",
        "payload",
        "target_session",
    ] {
        shrink_review_start_audit_field(fields, key);
        if review_start_audit_payload_len(fields) <= crate::events::MAX_EVENT_PAYLOAD_BYTES {
            return;
        }
    }
}

fn shrink_review_start_audit_field(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) {
    let Some(value) = fields
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_string)
    else {
        return;
    };
    fields.insert(format!("{key}_truncated"), true.into());
    let boundaries: Vec<usize> = value
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(value.len()))
        .collect();
    let mut low = 0;
    let mut high = boundaries.len() - 1;
    while low < high {
        let middle = (low + high + 1) / 2;
        fields.insert(key.to_string(), value[..boundaries[middle]].into());
        if review_start_audit_payload_len(fields) <= crate::events::MAX_EVENT_PAYLOAD_BYTES {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    fields.insert(key.to_string(), value[..boundaries[low]].into());
}

fn review_start_audit_payload_len(fields: &serde_json::Map<String, serde_json::Value>) -> usize {
    serde_json::to_string(fields)
        .map(|payload| payload.len())
        .unwrap_or(usize::MAX)
}

fn review_invocation_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("ri-{nanos:x}-{}", std::process::id())
}

fn review_invocation_sidecar(session_id: &str) -> Option<std::path::PathBuf> {
    if session_id.is_empty()
        || std::path::Path::new(session_id).file_name()?.to_str()? != session_id
    {
        return None;
    }
    // FNO_HOME first: the Python writer and both shell hooks key the sidecar
    // off that override, so reading a different root silently breaks the
    // join on every configured machine.
    let root = std::env::var_os("FNO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            crate::paths::AgentsHome::from_env()
                .root()
                .parent()
                .map(std::path::Path::to_path_buf)
        })?;
    Some(
        root.join("review-invocations")
            .join(format!("{session_id}.json")),
    )
}

/// Read AND consume a pending id, mirroring the Python adopt: without the
/// unlink a stale sidecar is re-adopted forever and distinct reviews collapse
/// into one invocation id.
fn read_pending_review_invocation(session_id: &str) -> Option<String> {
    let path = review_invocation_sidecar(session_id)?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("invocation_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn write_pending_review_invocation(session_id: &str, invocation_id: &str) {
    let Some(path) = review_invocation_sidecar(session_id) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    else {
        return;
    };
    let value = serde_json::json!({
        "invocation_id": invocation_id,
        "target_session_id": session_id,
    });
    let _ = std::io::Write::write_all(&mut file, value.to_string().as_bytes());
}

fn review_invocation_event_fields(
    invocation_id: &str,
    thread_id: &str,
    audit_payload: Option<&str>,
    audit_sender: Option<&str>,
    audit_origin: Option<&str>,
    confirmed: bool,
    receipt: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let command = audit_payload.unwrap_or("/review").trim();
    let mut parts = command.splitn(2, char::is_whitespace);
    let verb = parts
        .next()
        .filter(|token| token.starts_with('/'))
        .unwrap_or("/review");
    let args_raw = parts.next().unwrap_or("").trim_start();
    let mut args = args_raw.split_whitespace();
    let first_arg = args.next().unwrap_or("");
    let (level, level_source) = match first_arg {
        "ultra" => ("ultra", "ultra_forced"),
        "low" | "medium" | "high" | "xhigh" | "max" => (first_arg, "explicit"),
        _ => ("unset", "fallback"),
    };
    let flags: Vec<&str> = args_raw
        .split_whitespace()
        .filter(|arg| arg.starts_with("--"))
        .collect();
    let initiator = match audit_origin {
        Some("operator") => "operator",
        Some("peer") => "king",
        Some("scheduler" | "recovery") => "daemon",
        _ => "unknown",
    };
    let mut fields = serde_json::Map::new();
    fields.insert("invocation_id".into(), invocation_id.into());
    fields.insert("stage".into(), "sent".into());
    fields.insert("verb".into(), verb.into());
    fields.insert("args_raw".into(), args_raw.into());
    fields.insert("level".into(), level.into());
    fields.insert("level_source".into(), level_source.into());
    fields.insert("flags".into(), json!(flags));
    fields.insert("transport".into(), "codex_rpc".into());
    fields.insert("initiator".into(), initiator.into());
    fields.insert("target_session_id".into(), thread_id.into());
    fields.insert("submit_required".into(), false.into());
    fields.insert("submit_key".into(), "none".into());
    fields.insert("submit_confirmed".into(), confirmed.into());
    fields.insert(
        "receipt".into(),
        receipt.chars().take(240).collect::<String>().into(),
    );
    if let Some(sender) = audit_sender.filter(|sender| !sender.is_empty()) {
        fields.insert("initiator_session_id".into(), sender.into());
    }
    fields
}

fn emit_review_invocation_event(
    thread_id: &str,
    audit_payload: Option<&str>,
    audit_sender: Option<&str>,
    audit_target_cwd: Option<&str>,
    audit_origin: Option<&str>,
    invocation_id: &str,
    confirmed: bool,
    receipt: &str,
) {
    let cwd = audit_target_cwd
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
    let events_path = std::env::var("FNO_EVENTS_PATH")
        .ok()
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::paths::worktree_repo_root(&cwd).join(".fno/events.jsonl"));
    let fields = review_invocation_event_fields(
        invocation_id,
        thread_id,
        audit_payload,
        audit_sender,
        audit_origin,
        confirmed,
        receipt,
    );
    emit_review_invocation_fields(
        &crate::events::EventEmitter::new(events_path, "daemon"),
        fields,
    );
}

/// The `fno-agents review-start` verb entry: parse CLI, drive the round-trip,
/// print the outcome receipt (or a clean not-delivered reason). The socket
/// round-trip needs the user's daemon and stays a manual verification, as
/// `turn/start` already documents for its own path; the pure builders above are
/// the correct-by-construction unit-tested core.
pub async fn run_review_start(rest: &[String]) -> i32 {
    let mut thread_id: Option<String> = None;
    let mut target_raw: Option<String> = None;
    let mut delivery = ReviewDelivery::Inline;
    let mut audit_payload: Option<String> = None;
    let mut audit_sender: Option<String> = None;
    let mut audit_target_cwd: Option<String> = None;
    let mut audit_origin: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => {
                thread_id = it.next().map(String::clone).or_else(|| {
                    eprintln!("review-start: --session needs a value");
                    None
                });
            }
            "--target" => {
                target_raw = it.next().cloned();
            }
            "--delivery" => {
                delivery = match it.next().map(String::as_str) {
                    Some("inline") => ReviewDelivery::Inline,
                    Some("detached") => ReviewDelivery::Detached,
                    _ => {
                        eprintln!("review-start: --delivery must be inline or detached (detached forks a thread that inherits the session's ORIGINAL cwd, not the current one - verify the review thread's cwd before trusting its findings)");
                        return 2;
                    }
                };
            }
            // These values enrich the append-only event after delivery. They
            // are never parsed as a ReviewTarget; the routing door stays
            // limited to the structured --target grammar.
            "--audit-payload" => {
                audit_payload = it.next().cloned();
            }
            "--audit-sender" => {
                audit_sender = it.next().cloned();
            }
            "--audit-target-cwd" => {
                audit_target_cwd = it.next().cloned();
            }
            "--audit-origin" => {
                audit_origin = it.next().cloned();
            }
            other => {
                eprintln!("review-start: unknown flag: {other}");
                return 2;
            }
        }
    }
    let thread_id = match thread_id {
        Some(t) => t,
        None => {
            eprintln!("review-start: --session is required (the codex threadId)");
            return 2;
        }
    };
    let target_raw = match target_raw {
        Some(t) => t,
        None => {
            eprintln!(
                "review-start: --target is required, one of: \
                 uncommittedChanges | baseBranch:<branch> | commit:<sha> | custom:<instructions>"
            );
            return 2;
        }
    };
    let target = match parse_review_target(&target_raw) {
        Some(t) => t,
        None => {
            eprintln!(
                "review-start: could not parse --target {target_raw:?} \
                 (expected uncommittedChanges | baseBranch:<branch> | commit:<sha> | custom:<text>)"
            );
            return 2;
        }
    };
    let invocation_id =
        read_pending_review_invocation(&thread_id).unwrap_or_else(|| review_invocation_id());
    write_pending_review_invocation(&thread_id, &invocation_id);
    let outcome = deliver_via_codex_review_start(&thread_id, &target, delivery).await;
    let failure_reason = outcome.as_ref().err().map(ToString::to_string);
    let receipt = match failure_reason.as_deref() {
        Some(reason) => format!("codex review/start failed: {reason}"),
        None => "review/start delivered".to_string(),
    };
    emit_review_invocation_event(
        &thread_id,
        audit_payload.as_deref(),
        audit_sender.as_deref(),
        audit_target_cwd.as_deref(),
        audit_origin.as_deref(),
        &invocation_id,
        outcome.is_ok(),
        &receipt,
    );

    // Audit floor: `review/start` is the second unwrapped-fire path this crate
    // ships. It carries no `<fno_mail>` envelope and leaves no agent-authored
    // marker in the recipient thread, so it needs the same ledger record the
    // mail-inject lane writes -- a guard on one of two reachable paths is
    // decorative. Best-effort; a write failure never changes the exit code.
    let fields = review_start_audit_fields_with_origin(
        &thread_id,
        &target_raw,
        delivery,
        audit_payload.as_deref(),
        audit_sender.as_deref(),
        audit_target_cwd.as_deref(),
        failure_reason.as_deref(),
        audit_origin.as_deref(),
    );
    let _ = crate::events::EventEmitter::new(
        crate::paths::AgentsHome::from_env().events_jsonl(),
        "daemon",
    )
    .emit_fields("agent_raw_inject", fields);

    match outcome {
        Ok((turn_id, review_thread)) => {
            println!(
                "{}",
                json!({"delivered": true, "turn_id": turn_id, "review_thread_id": review_thread})
            );
            0
        }
        Err(reason) => {
            println!(
                "{}",
                json!({"delivered": false, "reason": reason.to_string()})
            );
            1
        }
    }
}

/// Parse a `--target` shorthand into a [`ReviewTarget`]. `baseBranch:main`,
/// `commit:abc123`, `custom:focus on concurrency`, or the bare
/// `uncommittedChanges`.
fn parse_review_target(raw: &str) -> Option<ReviewTarget> {
    if raw == "uncommittedChanges" {
        return Some(ReviewTarget::UncommittedChanges);
    }
    if let Some(rest) = raw.strip_prefix("baseBranch:") {
        return Some(ReviewTarget::BaseBranch {
            branch: rest.to_string(),
        });
    }
    if let Some(rest) = raw.strip_prefix("commit:") {
        let (sha, title) = match rest.split_once('|') {
            Some((sha, title)) => (sha.to_string(), Some(title.to_string())),
            None => (rest.to_string(), None),
        };
        return Some(ReviewTarget::Commit { sha, title });
    }
    if let Some(rest) = raw.strip_prefix("custom:") {
        return Some(ReviewTarget::Custom {
            instructions: rest.to_string(),
        });
    }
    None
}

pub fn loaded_list_request_json(id: u64, cursor: Option<&str>) -> String {
    serde_json::json!({
        "id": id,
        "method": "thread/loaded/list",
        "params": {
            "cursor": cursor,
            "limit": LOADED_PAGE_SIZE,
        }
    })
    .to_string()
}

pub fn thread_read_request_json(id: u64, thread_id: &str) -> String {
    serde_json::json!({
        "id": id,
        "method": "thread/read",
        "params": {
            "threadId": thread_id,
            "includeTurns": false,
        }
    })
    .to_string()
}

pub fn parse_loaded_list_response(
    raw: &str,
) -> Result<(Vec<String>, Option<String>), &'static str> {
    let v: serde_json::Value = serde_json::from_str(raw).map_err(|_| "rpc-error")?;
    if v.get("error").is_some() {
        return Err("rpc-error");
    }
    let result = v
        .get("result")
        .and_then(|r| r.as_object())
        .ok_or("rpc-error")?;
    let data = result
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or("rpc-error")?;
    let mut ids = Vec::with_capacity(data.len());
    for item in data {
        let id = item.as_str().filter(|s| !s.is_empty()).ok_or("rpc-error")?;
        ids.push(id.to_string());
    }
    let next_cursor = match result.get("nextCursor") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(value.as_str().ok_or("rpc-error")?.to_string()),
    };
    let next_cursor = next_cursor.filter(|cursor| !cursor.is_empty());
    Ok((ids, next_cursor))
}

pub fn parse_thread_read_cwd(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    v.pointer("/result/thread/cwd")
        .and_then(|cwd| cwd.as_str())
        .map(str::to_string)
}

/// Classify a `turn/start` response frame into delivered / not-delivered.
///
/// - `.result.turn.id` is a string -> `Ok(())` (turn accepted; DELIVERED).
/// - `.error` whose message mentions "not found"/"thread" -> `Err("thread-not-loaded")`
///   (the session is embedded / not attached to the daemon -> durable fallback).
/// - anything else (other rpc error, unparseable) -> `Err("rpc-error")`.
///
/// The `Err` value IS the `mail-inject` JSON `reason` token.
pub fn classify_turn_start_response(raw: &str) -> Result<(), &'static str> {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Err("rpc-error"),
    };
    if v.get("result")
        .and_then(|r| r.get("turn"))
        .and_then(|t| t.get("id"))
        .and_then(|id| id.as_str())
        .is_some()
    {
        return Ok(());
    }
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_lowercase();
        if msg.contains("not found") || msg.contains("thread") {
            return Err("thread-not-loaded");
        }
    }
    Err("rpc-error")
}

/// Deliver `text` into codex `thread_id` over the app-server daemon socket.
/// `Ok(())` == turn accepted (delivered); every `Err(reason)` is a clean
/// not-delivered signal whose value is the `mail-inject` JSON `reason` token.
/// Socket absent -> `Err("no-daemon")`; a wedged socket -> `Err("io-error")`
/// after [`HANDSHAKE_TIMEOUT`].
pub async fn deliver_via_codex_daemon(thread_id: &str, text: &str) -> Result<(), &'static str> {
    let sock = codex_app_server_socket_path();
    if !sock.exists() {
        return Err("no-daemon");
    }
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, inject(&sock, thread_id, text)).await {
        Ok(r) => r,
        Err(_) => Err("io-error"),
    }
}

pub async fn discover_loaded_threads() -> Result<Vec<LoadedThread>, &'static str> {
    let sock = codex_app_server_socket_path();
    if !sock.exists() {
        return Err("no-daemon");
    }
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, discover(&sock)).await {
        Ok(result) => result,
        Err(_) => Err("io-error"),
    }
}

pub async fn run_loaded_thread_discovery() -> i32 {
    let output = match discover_loaded_threads().await {
        Ok(threads) => serde_json::json!({"available": true, "threads": threads}),
        Err(reason) => serde_json::json!({"available": false, "reason": reason}),
    };
    println!("{output}");
    0
}

/// The connect + initialize handshake + `turn/start` round-trip. Split out so
/// [`deliver_via_codex_daemon`] can wrap it in a total timeout.
async fn inject(sock: &Path, thread_id: &str, text: &str) -> Result<(), &'static str> {
    let conn = UnixStream::connect(sock).await.map_err(|_| "io-error")?;
    let ws = match tokio_tungstenite::client_async("ws://localhost/rpc", conn).await {
        Ok((ws, _resp)) => ws,
        Err(_) => return Err("handshake-failed"),
    };
    let (mut sink, mut stream) = ws.split();

    sink.send(Message::Text(initialize_request_json().into()))
        .await
        .map_err(|_| "io-error")?;
    read_until_id(&mut stream, &serde_json::json!("init")).await?;

    sink.send(Message::Text(initialized_notification_json().into()))
        .await
        .map_err(|_| "io-error")?;

    sink.send(Message::Text(
        turn_start_request_json(thread_id, text).into(),
    ))
    .await
    .map_err(|_| "io-error")?;
    let resp = read_until_id(&mut stream, &serde_json::json!(1)).await?;
    classify_turn_start_response(&resp)
}

async fn discover(sock: &Path) -> Result<Vec<LoadedThread>, &'static str> {
    let conn = UnixStream::connect(sock).await.map_err(|_| "io-error")?;
    let ws = match tokio_tungstenite::client_async("ws://localhost/rpc", conn).await {
        Ok((ws, _resp)) => ws,
        Err(_) => return Err("handshake-failed"),
    };
    let (mut sink, mut stream) = ws.split();

    sink.send(Message::Text(initialize_request_json().into()))
        .await
        .map_err(|_| "io-error")?;
    read_until_id(&mut stream, &serde_json::json!("init")).await?;
    sink.send(Message::Text(initialized_notification_json().into()))
        .await
        .map_err(|_| "io-error")?;

    let mut ids = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut seen_cursors = HashSet::new();
    let mut cursor: Option<String> = None;
    let mut request_id = 2_u64;
    let mut complete = false;
    for _ in 0..MAX_LOADED_PAGES {
        sink.send(Message::Text(
            loaded_list_request_json(request_id, cursor.as_deref()).into(),
        ))
        .await
        .map_err(|_| "io-error")?;
        let raw = read_until_id(&mut stream, &serde_json::json!(request_id)).await?;
        let (page, next_cursor) = parse_loaded_list_response(&raw)?;
        for id in page {
            if seen_ids.insert(id.clone()) {
                ids.push(id);
            }
        }
        request_id += 1;
        match next_cursor {
            None => {
                complete = true;
                break;
            }
            Some(next) if seen_cursors.insert(next.clone()) => cursor = Some(next),
            Some(_) => return Err("rpc-error"),
        }
    }
    if !complete {
        return Err("rpc-error");
    }

    let mut threads = Vec::with_capacity(ids.len());
    for session_id in ids {
        sink.send(Message::Text(
            thread_read_request_json(request_id, &session_id).into(),
        ))
        .await
        .map_err(|_| "io-error")?;
        let cwd = match read_until_id(&mut stream, &serde_json::json!(request_id)).await {
            Ok(raw) => parse_thread_read_cwd(&raw).unwrap_or_default(),
            Err(_) => String::new(),
        };
        request_id += 1;
        threads.push(LoadedThread { session_id, cwd });
    }
    Ok(threads)
}

/// Read Text frames until one whose `id` equals `want`, returning its raw text.
/// Skips notifications (no `id`) and frames for other ids; ignores non-Text
/// frames. Bounded by [`MAX_FRAMES`]; a read error / closed stream is `"io-error"`.
async fn read_until_id<S>(stream: &mut S, want: &serde_json::Value) -> Result<String, &'static str>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    for _ in 0..MAX_FRAMES {
        match stream.next().await {
            Some(Ok(Message::Text(t))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                    if v.get("id") == Some(want) {
                        return Ok(t.to_string());
                    }
                }
            }
            Some(Ok(_)) => {} // non-Text frame (ping/binary/close-less); skip
            Some(Err(_)) | None => return Err("io-error"),
        }
    }
    Err("io-error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness_daemon::HarnessDaemonAdapter;
    use tokio::net::UnixListener;
    use tokio_tungstenite::{accept_async, WebSocketStream};

    async fn accept_initialized(listener: UnixListener) -> WebSocketStream<UnixStream> {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let init = ws.next().await.unwrap().unwrap().into_text().unwrap();
        let init: serde_json::Value = serde_json::from_str(&init).unwrap();
        assert_eq!(init["method"], "initialize");
        ws.send(Message::Text(r#"{"id":"init","result":{}}"#.into()))
            .await
            .unwrap();
        let initialized = ws.next().await.unwrap().unwrap().into_text().unwrap();
        let initialized: serde_json::Value = serde_json::from_str(&initialized).unwrap();
        assert_eq!(initialized["method"], "initialized");
        ws
    }

    async fn next_request(ws: &mut WebSocketStream<UnixStream>) -> serde_json::Value {
        let raw = ws.next().await.unwrap().unwrap().into_text().unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn initialize_request_is_bare_jsonrpc_with_string_id() {
        let v: serde_json::Value = serde_json::from_str(&initialize_request_json()).unwrap();
        assert_eq!(v["id"], "init");
        assert_eq!(v["method"], "initialize");
        assert_eq!(v["params"]["clientInfo"]["name"], "fno-mail-inject");
        // The codex app-server carries bare JSON-RPC frames: no "jsonrpc" field.
        assert!(v.get("jsonrpc").is_none());
    }

    #[test]
    fn initialized_notification_has_no_id() {
        let v: serde_json::Value = serde_json::from_str(&initialized_notification_json()).unwrap();
        assert_eq!(v["method"], "initialized");
        assert!(v.get("id").is_none());
    }

    #[test]
    fn turn_start_carries_thread_id_and_text_item() {
        let v: serde_json::Value =
            serde_json::from_str(&turn_start_request_json("THREAD-9", "hello MARKER")).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["method"], "turn/start");
        assert_eq!(v["params"]["threadId"], "THREAD-9");
        assert_eq!(v["params"]["input"][0]["type"], "text");
        assert_eq!(v["params"]["input"][0]["text"], "hello MARKER");
    }

    #[test]
    fn review_start_target_variants_and_delivery() {
        // uncommittedChanges + inline
        let v: serde_json::Value = serde_json::from_str(&review_start_request_json(
            "T1",
            &ReviewTarget::UncommittedChanges,
            ReviewDelivery::Inline,
        ))
        .unwrap();
        assert_eq!(v["method"], "review/start");
        assert_eq!(v["params"]["threadId"], "T1");
        assert_eq!(v["params"]["target"]["type"], "uncommittedChanges");
        assert_eq!(v["params"]["delivery"], "inline");

        // baseBranch + detached
        let v: serde_json::Value = serde_json::from_str(&review_start_request_json(
            "T1",
            &ReviewTarget::BaseBranch {
                branch: "main".into(),
            },
            ReviewDelivery::Detached,
        ))
        .unwrap();
        assert_eq!(v["params"]["target"]["type"], "baseBranch");
        assert_eq!(v["params"]["target"]["branch"], "main");
        assert_eq!(v["params"]["delivery"], "detached");

        // commit (with title)
        let v: serde_json::Value = serde_json::from_str(&review_start_request_json(
            "T1",
            &ReviewTarget::Commit {
                sha: "abc".into(),
                title: Some("t".into()),
            },
            ReviewDelivery::Inline,
        ))
        .unwrap();
        assert_eq!(v["params"]["target"]["type"], "commit");
        assert_eq!(v["params"]["target"]["sha"], "abc");
        assert_eq!(v["params"]["target"]["title"], "t");

        // custom
        let v: serde_json::Value = serde_json::from_str(&review_start_request_json(
            "T1",
            &ReviewTarget::Custom {
                instructions: "focus on x".into(),
            },
            ReviewDelivery::Inline,
        ))
        .unwrap();
        assert_eq!(v["params"]["target"]["type"], "custom");
        assert_eq!(v["params"]["target"]["instructions"], "focus on x");
    }

    #[test]
    fn review_start_response_parses_turn_and_thread() {
        let raw = r#"{"id":1,"result":{"turn":{"id":"turn-1","status":"inProgress"},"reviewThreadId":"rt-9"}}"#;
        assert_eq!(
            parse_review_start_response(raw),
            Ok(("turn-1".into(), "rt-9".into()))
        );
        let server_error = parse_review_start_response(
            r#"{"id":1,"error":{"message":"review target must be a base branch"}}"#,
        )
        .unwrap_err();
        let invalid_response = parse_review_start_response("garbage").unwrap_err();
        let unexpected_response =
            parse_review_start_response(r#"{"id":1,"result":{}}"#).unwrap_err();
        assert!(matches!(
            &server_error,
            ReviewStartError::Server(message)
                if message == "review target must be a base branch"
        ));
        assert_eq!(
            server_error.to_string(),
            "server-error: review target must be a base branch"
        );
        assert_eq!(
            parse_review_start_response(r#"{"id":1,"error":{"message":"not-confirmed"}}"#)
                .unwrap_err()
                .to_string(),
            "server-error: not-confirmed"
        );
        assert_eq!(invalid_response.to_string(), "invalid-response");
        assert_eq!(unexpected_response.to_string(), "unexpected-response");
        assert_ne!(server_error, invalid_response);
        assert_ne!(server_error, unexpected_response);
        assert_ne!(invalid_response, unexpected_response);
        assert_eq!(
            parse_review_start_response(r#"{"id":1,"result":{"turn":{"id":"turn-1"}}}"#)
                .unwrap_err(),
            ReviewStartError::Reason("not-confirmed")
        );
    }

    #[test]
    fn review_start_audit_prefers_caller_provenance_without_changing_target() {
        let fields = review_start_audit_fields(
            "thread-1",
            "baseBranch:origin/main",
            ReviewDelivery::Inline,
            Some("/review --base origin/main"),
            Some("sender-1"),
            Some("/repo"),
            None,
        );
        assert_eq!(fields["target_session"], "thread-1");
        assert_eq!(fields["payload"], "/review --base origin/main");
        assert_eq!(fields["sender"], "sender-1");
        assert_eq!(fields["target_cwd"], "/repo");
        assert_eq!(fields["harness"], "codex");
        assert_eq!(fields["lane"], "codex-review-start");
        assert_eq!(fields["confirmed"], true);
        assert!(!fields.contains_key("reason"));
    }

    #[test]
    fn review_start_audit_carries_failure_reason() {
        let fields = review_start_audit_fields(
            "thread-1",
            "baseBranch:origin/main",
            ReviewDelivery::Inline,
            Some("/review --base origin/main"),
            Some("sender-1"),
            Some("/repo"),
            Some("review target must be a base branch"),
        );
        assert_eq!(fields["lane"], "codex-review-start");
        assert_eq!(fields["confirmed"], false);
        assert_eq!(fields["reason"], "review target must be a base branch");
    }

    #[test]
    fn review_invocation_audit_carries_server_message_and_canonical_fields() {
        let fields = review_invocation_event_fields(
            "ri-positive",
            "thread-1",
            Some("/review medium --comment"),
            Some("sender-1"),
            Some("peer"),
            false,
            "codex review/start failed: server-error: review target must be a base branch",
        );

        assert_eq!(fields["stage"], "sent");
        assert_eq!(fields["transport"], "codex_rpc");
        assert_eq!(fields["level"], "medium");
        assert_eq!(fields["level_source"], "explicit");
        assert_eq!(fields["flags"], serde_json::json!(["--comment"]));
        assert_eq!(fields["initiator"], "king");
        assert_eq!(fields["submit_key"], "none");
        assert_eq!(
            fields["receipt"],
            "codex review/start failed: server-error: review target must be a base branch"
        );
        assert!(fields["invocation_id"].as_str().unwrap().starts_with("ri-"));

        let temp = tempfile::tempdir().unwrap();
        emit_review_invocation_fields(
            &crate::events::EventEmitter::new(temp.path().join("events.jsonl"), "daemon"),
            fields,
        );
        let raw = std::fs::read_to_string(temp.path().join("events.jsonl")).unwrap();
        let event: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(event["type"], "review_invocation");
        assert_eq!(
            event["data"]["receipt"],
            "codex review/start failed: server-error: review target must be a base branch"
        );
    }

    #[test]
    fn review_start_audit_truncates_long_reason_without_losing_event() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let long_reason = "x".repeat(340);
        let fields = review_start_audit_fields(
            "thread-1",
            "baseBranch:origin/main",
            ReviewDelivery::Inline,
            Some("/review --base origin/main"),
            Some("sender-1"),
            Some("/repo"),
            Some(&long_reason),
        );
        crate::events::EventEmitter::new(&path, "daemon")
            .emit_fields("agent_raw_inject", fields)
            .unwrap();
        let raw = std::fs::read_to_string(path).unwrap();
        let event: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(event["type"], "agent_raw_inject");
        assert_eq!(event["data"]["target_session"], "thread-1");
        assert_eq!(event["data"]["payload"], "/review --base origin/main");
        assert_eq!(event["data"]["reason_truncated"], true);
        let recorded = event["data"]["reason"].as_str().unwrap();
        assert!(long_reason.starts_with(recorded));
        assert!(recorded.len() < long_reason.len());
    }

    #[test]
    fn review_start_audit_budgets_all_variable_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let large = "x".repeat(512);
        let fields = review_start_audit_fields_with_origin(
            &large,
            "baseBranch:origin/main",
            ReviewDelivery::Inline,
            Some(&large),
            Some(&large),
            Some(&large),
            Some(&large),
            Some(&large),
        );
        crate::events::EventEmitter::new(&path, "daemon")
            .emit_fields("agent_raw_inject", fields)
            .unwrap();
        let raw = std::fs::read_to_string(path).unwrap();
        let event: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(event["type"], "agent_raw_inject");
        assert_eq!(event["data"]["payload_truncated"], true);
        assert_eq!(event["data"]["sender_truncated"], true);
        assert_eq!(event["data"]["target_cwd_truncated"], true);
        assert_eq!(event["data"]["origin_truncated"], true);
        assert_eq!(event["data"]["target_session_truncated"], true);
        assert_eq!(event["data"]["reason_truncated"], true);
    }

    #[tokio::test]
    async fn review_start_close_after_send_is_not_confirmed() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("codex.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let mut ws = accept_initialized(listener).await;
            let request = next_request(&mut ws).await;
            assert_eq!(request["method"], "review/start");
        });
        let mut request_in_flight = false;
        let result = review_start_round_trip(
            &socket,
            "thread-1",
            &ReviewTarget::UncommittedChanges,
            ReviewDelivery::Inline,
            &mut request_in_flight,
        )
        .await;
        server.await.unwrap();
        assert!(request_in_flight);
        assert_eq!(
            result.unwrap_err(),
            ReviewStartError::Reason("not-confirmed")
        );
    }

    #[test]
    fn parse_review_target_shorthand() {
        assert_eq!(
            parse_review_target("uncommittedChanges"),
            Some(ReviewTarget::UncommittedChanges)
        );
        assert_eq!(
            parse_review_target("baseBranch:main"),
            Some(ReviewTarget::BaseBranch {
                branch: "main".into()
            })
        );
        assert_eq!(
            parse_review_target("commit:abc|title"),
            Some(ReviewTarget::Commit {
                sha: "abc".into(),
                title: Some("title".into())
            })
        );
        assert_eq!(
            parse_review_target("commit:abc"),
            Some(ReviewTarget::Commit {
                sha: "abc".into(),
                title: None
            })
        );
        assert_eq!(
            parse_review_target("custom:focus on x"),
            Some(ReviewTarget::Custom {
                instructions: "focus on x".into()
            })
        );
        assert_eq!(parse_review_target("bogus"), None);
    }

    #[test]
    fn loaded_list_request_carries_cursor_and_limit() {
        let v: serde_json::Value =
            serde_json::from_str(&loaded_list_request_json(7, Some("cursor-1"))).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "thread/loaded/list");
        assert_eq!(v["params"]["cursor"], "cursor-1");
        assert_eq!(v["params"]["limit"], LOADED_PAGE_SIZE);
    }

    #[test]
    fn loaded_list_parser_distinguishes_empty_and_malformed() {
        let empty = r#"{"id":2,"result":{"data":[],"nextCursor":null}}"#;
        assert_eq!(parse_loaded_list_response(empty), Ok((vec![], None)));
        let page = r#"{"id":2,"result":{"data":["a","b"],"nextCursor":"b"}}"#;
        assert_eq!(
            parse_loaded_list_response(page),
            Ok((vec!["a".into(), "b".into()], Some("b".into())))
        );
        assert_eq!(
            parse_loaded_list_response(r#"{"id":2,"result":{"data":[1]}}"#),
            Err("rpc-error")
        );
    }

    #[test]
    fn loaded_list_parser_treats_empty_cursor_as_terminal() {
        let raw = r#"{"id":2,"result":{"data":["a"],"nextCursor":""}}"#;
        assert_eq!(
            parse_loaded_list_response(raw),
            Ok((vec!["a".to_string()], None))
        );
    }

    #[test]
    fn thread_read_builder_and_parser_use_metadata_only() {
        let v: serde_json::Value =
            serde_json::from_str(&thread_read_request_json(9, "thread-1")).unwrap();
        assert_eq!(v["method"], "thread/read");
        assert_eq!(v["params"]["threadId"], "thread-1");
        assert_eq!(v["params"]["includeTurns"], false);
        let raw = r#"{"id":9,"result":{"thread":{"id":"thread-1","cwd":"/repo"}}}"#;
        assert_eq!(parse_thread_read_cwd(raw).as_deref(), Some("/repo"));
        assert_eq!(
            parse_thread_read_cwd(r#"{"id":9,"error":{"message":"gone"}}"#),
            None
        );
    }

    #[tokio::test]
    async fn discovery_paginates_deduplicates_and_keeps_failed_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("codex.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let mut ws = accept_initialized(listener).await;

            let first = next_request(&mut ws).await;
            assert_eq!(first["params"]["cursor"], serde_json::Value::Null);
            ws.send(Message::Text(
                r#"{"id":2,"result":{"data":["thread-a"],"nextCursor":"thread-a"}}"#.into(),
            ))
            .await
            .unwrap();

            let second = next_request(&mut ws).await;
            assert_eq!(second["params"]["cursor"], "thread-a");
            ws.send(Message::Text(
                r#"{"id":3,"result":{"data":["thread-a","thread-b"],"nextCursor":null}}"#.into(),
            ))
            .await
            .unwrap();

            let read_a = next_request(&mut ws).await;
            assert_eq!(read_a["params"]["threadId"], "thread-a");
            ws.send(Message::Text(
                r#"{"id":4,"result":{"thread":{"cwd":"/repo/a"}}}"#.into(),
            ))
            .await
            .unwrap();

            let read_b = next_request(&mut ws).await;
            assert_eq!(read_b["params"]["threadId"], "thread-b");
            ws.send(Message::Text(
                r#"{"id":5,"error":{"message":"metadata unavailable"}}"#.into(),
            ))
            .await
            .unwrap();
        });

        let threads = discover(&socket).await.unwrap();
        assert_eq!(
            threads,
            vec![
                LoadedThread {
                    session_id: "thread-a".into(),
                    cwd: "/repo/a".into(),
                },
                LoadedThread {
                    session_id: "thread-b".into(),
                    cwd: String::new(),
                },
            ]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn discovery_rejects_repeated_cursor_without_partial_results() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("codex.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let mut ws = accept_initialized(listener).await;
            let _first = next_request(&mut ws).await;
            ws.send(Message::Text(
                r#"{"id":2,"result":{"data":["thread-a"],"nextCursor":"thread-a"}}"#.into(),
            ))
            .await
            .unwrap();
            let _second = next_request(&mut ws).await;
            ws.send(Message::Text(
                r#"{"id":3,"result":{"data":["thread-b"],"nextCursor":"thread-a"}}"#.into(),
            ))
            .await
            .unwrap();
        });

        assert_eq!(discover(&socket).await, Err("rpc-error"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn discovery_distinguishes_successful_empty_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("codex.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let mut ws = accept_initialized(listener).await;
            let _list = next_request(&mut ws).await;
            ws.send(Message::Text(
                r#"{"id":2,"result":{"data":[],"nextCursor":null}}"#.into(),
            ))
            .await
            .unwrap();
        });

        assert_eq!(discover(&socket).await, Ok(vec![]));
        server.await.unwrap();
    }

    #[test]
    fn classify_delivered_on_result_turn_id() {
        let raw = r#"{"id":1,"result":{"turn":{"id":"turn-abc","status":"inProgress"}}}"#;
        assert_eq!(classify_turn_start_response(raw), Ok(()));
    }

    #[test]
    fn classify_thread_not_loaded_on_thread_error() {
        let raw = r#"{"id":1,"error":{"code":-32000,"message":"thread not found"}}"#;
        assert_eq!(classify_turn_start_response(raw), Err("thread-not-loaded"));
    }

    #[test]
    fn classify_rpc_error_on_other_error_or_garbage() {
        let other = r#"{"id":1,"error":{"code":-32601,"message":"method not implemented"}}"#;
        assert_eq!(classify_turn_start_response(other), Err("rpc-error"));
        assert_eq!(classify_turn_start_response("not json"), Err("rpc-error"));
        // A result without a string turn id is not a confirmed delivery.
        let no_turn = r#"{"id":1,"result":{}}"#;
        assert_eq!(classify_turn_start_response(no_turn), Err("rpc-error"));
    }

    #[test]
    fn socket_path_honors_codex_home() {
        // Sanity: the tail is fixed; the head follows CODEX_HOME/HOME. We only
        // assert the stable suffix to avoid mutating process env in a unit test.
        let p = codex_app_server_socket_path();
        assert!(p.ends_with("app-server-control/app-server-control.sock"));
    }

    #[test]
    fn codex_daemon_state_requires_pid_and_process_start_time() {
        let adapter = CodexDaemonAdapter::new(
            PathBuf::from("/tmp/codex.pid"),
            PathBuf::from("/tmp/codex.lock"),
            PathBuf::from("/tmp/codex.sock"),
        );
        assert!(adapter
            .parse_state(r#"{"pid":123}"#)
            .expect_err("bare pid must be unreadable")
            .contains("process start"));
    }

    #[test]
    fn codex_daemon_state_receipt_names_endpoint_and_incarnation() {
        let adapter = CodexDaemonAdapter::new(
            PathBuf::from("/tmp/codex.pid"),
            PathBuf::from("/tmp/codex.lock"),
            PathBuf::from("/tmp/codex.sock"),
        );
        let state = adapter
            .parse_state(r#"{"pid":123,"processStartTime":456}"#)
            .unwrap();
        assert_eq!(state.endpoint, "/tmp/codex.sock");
        assert_eq!(state.incarnation, "123:456");
    }

    #[test]
    fn codex_daemon_state_accepts_provider_date_string() {
        let adapter = CodexDaemonAdapter::new(
            PathBuf::from("/tmp/codex.pid"),
            PathBuf::from("/tmp/codex.lock"),
            PathBuf::from("/tmp/codex.sock"),
        );
        let pid = std::process::id();
        let state = adapter
            .parse_state(&format!(
                r#"{{"pid":{pid},"processStartTime":"Tue Aug 25 05:29:46 2026"}}"#
            ))
            .expect("the provider's live date-string format is readable");
        assert_eq!(state.pid, Some(pid));
        assert_eq!(
            state.process_start_time,
            crate::daemon::process_start_time(pid)
        );
    }

    #[test]
    fn codex_uses_an_fno_sidecar_and_never_replaces_the_provider_process() {
        use crate::harness_daemon::HarnessDaemonAdapter;
        let provider_state = PathBuf::from("/tmp/app-server.pid");
        let adapter = CodexDaemonAdapter::new(
            provider_state.clone(),
            PathBuf::from("/tmp/codex.lock"),
            PathBuf::from("/tmp/codex.sock"),
        );
        assert_ne!(adapter.state_path(), provider_state.as_path());
        assert!(!adapter.may_replace());
    }

    #[test]
    fn codex_socket_without_initialize_handshake_is_not_healthy() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("codex.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        assert!(!probe_codex_app_server(&socket));
    }

    #[tokio::test]
    async fn codex_health_handshake_times_out_when_the_server_stops_replying() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("codex.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let _initialize = ws.next().await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        });

        let result =
            codex_initialize_handshake_with_timeout(&socket, Duration::from_millis(20)).await;

        assert_eq!(result, Err("timeout"));
        server.abort();
    }

    #[tokio::test]
    async fn synchronous_probe_inside_runtime_does_not_start_a_nested_runtime() {
        let missing =
            std::env::temp_dir().join(format!("missing-codex-probe-{}", std::process::id()));
        assert!(!probe_codex_app_server(&missing));
    }
}
