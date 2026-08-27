//! Stdio app-server driver for persistent Codex threads.
//!
//! Codex's app-server is a newline-delimited JSON protocol on stdin/stdout.
//! This module owns one app-server child, keeps its full thread id, and turns
//! `turn/completed` notifications into structured turn receipts. The daemon
//! holds the driver; no pane or daemon-owned WebSocket is involved.

use crate::codex_inject::{
    initialize_request_json, initialized_notification_json, parse_review_start_response,
    review_start_request_json, ReviewDelivery, ReviewTarget,
};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

/// Total budget for one id-matched request/response exchange (handshake,
/// turn/start, steer, interrupt, review). Frames unrelated to the id arrive
/// interleaved and are parked, so the budget bounds the whole exchange, not
/// any single frame.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Total budget for one whole turn. Codex streams a turn as an unbounded
/// burst of notification frames with quiet gaps while the model thinks, so
/// this deadline is the ONLY bound on the wait for `turn/completed`.
const TURN_TIMEOUT: Duration = Duration::from_secs(600);
/// How long the daemon's `ask` waits on a submitter's reply before answering
/// `in_flight`. Comfortably under the client's 120s `RESPONSE_DEADLINE`
/// (crates/fno-agents/src/bin/client.rs) so a bounded receipt, not a silent
/// transport failure, is what the caller sees. Env-overridable so tests can
/// exercise the expiry path without sleeping 90s.
pub fn ask_wait() -> Duration {
    std::env::var("FNO_CODEX_ASK_WAIT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map_or(Duration::from_secs(90), Duration::from_millis)
}
/// Bound on waiting for a turn's `turn/completed` AFTER an interrupt ack (or a
/// steer-precondition failure): the same outer-deadline shape as the daemon's
/// switchboard drive (request budget + grace) backstopping a wedged child.
const TURN_SETTLE_TIMEOUT: Duration = Duration::from_secs(65);
/// Outer bound around the daemon's WHOLE stop exchange (interrupt RPC + turn
/// settle), kept just under the client's 120s `RESPONSE_DEADLINE` so a wedged
/// child yields `timeout-child-killed`, not a dead socket.
pub fn stop_settle_bound() -> Duration {
    Duration::from_secs(115)
}
/// Parked-but-unclaimed turn receipts are telemetry only (the rollout on disk
/// is the durable record), so the map is capped and overflow drops entries.
const COMPLETED_PARK_CAP: usize = 8;
const THREAD_CHANNEL_CAP: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadStartError {
    InvalidResponse,
    NotConfirmed,
    Server(String),
}

impl std::fmt::Display for ThreadStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidResponse => f.write_str("invalid thread response"),
            Self::NotConfirmed => f.write_str("thread identity was not confirmed"),
            Self::Server(message) => write!(f, "server-error: {message}"),
        }
    }
}

impl std::error::Error for ThreadStartError {}

#[derive(Debug, thiserror::Error)]
pub enum ThreadDriverError {
    #[error("codex app-server spawn failed: {0}")]
    Spawn(#[source] io::Error),
    #[error("codex app-server I/O failed: {0}")]
    Io(#[source] io::Error),
    #[error("codex app-server response timed out")]
    Timeout,
    #[error("codex app-server protocol: {0}")]
    Protocol(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnResult {
    pub turn_id: String,
    pub status: String,
    pub text: String,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewResult {
    pub turn_id: String,
    pub review_thread_id: String,
}

/// Build the `thread/start` request with the unattended permission posture.
/// The nested `result.thread.id` is the only accepted identity shape.
pub fn thread_start_request_json(cwd: &str, approval_policy: &str) -> String {
    json!({
        "id": 1,
        "method": "thread/start",
        "params": {
            "cwd": cwd,
            "sandbox": "workspace-write",
            "approvalPolicy": approval_policy,
        }
    })
    .to_string()
}

/// Build the `thread/resume` request. The full thread id and cwd are both
/// required so recovery cannot silently move a worker onto the canonical repo.
pub fn thread_resume_request_json(thread_id: &str, cwd: &str, approval_policy: &str) -> String {
    json!({
        "id": 1,
        "method": "thread/resume",
        "params": {
            "threadId": thread_id,
            "cwd": cwd,
            "sandbox": "workspace-write",
            "approvalPolicy": approval_policy,
        }
    })
    .to_string()
}

/// Build a `turn/start` request for the held driver.
pub fn turn_start_request_json_with_id(id: u64, thread_id: &str, text: &str) -> String {
    turn_start_request_json_with_effort(id, thread_id, text, None)
}

pub fn turn_start_request_json_with_effort(
    id: u64,
    thread_id: &str,
    text: &str,
    effort: Option<&str>,
) -> String {
    let mut params = json!({
        "threadId": thread_id,
        "input": [{"type": "text", "text": text}],
    });
    if let Some(effort) = effort.filter(|effort| !effort.is_empty()) {
        params["effort"] = json!(effort);
    }
    json!({
        "id": id,
        "method": "turn/start",
        "params": params,
    })
    .to_string()
}

/// Build a `turn/steer` request with the server-enforced expected-turn
/// precondition. Keystrokes cannot provide this identity check.
pub fn turn_steer_request_json(
    id: u64,
    thread_id: &str,
    expected_turn_id: &str,
    text: &str,
) -> String {
    json!({
        "id": id,
        "method": "turn/steer",
        "params": {
            "threadId": thread_id,
            "expectedTurnId": expected_turn_id,
            "input": [{"type": "text", "text": text}],
        }
    })
    .to_string()
}

/// Build a `turn/interrupt` request against a full turn id.
pub fn turn_interrupt_request_json(id: u64, thread_id: &str, turn_id: &str) -> String {
    json!({
        "id": id,
        "method": "turn/interrupt",
        "params": {"threadId": thread_id, "turnId": turn_id},
    })
    .to_string()
}

pub fn parse_thread_start_response(raw: &str) -> Result<(String, String), ThreadStartError> {
    let value: Value = serde_json::from_str(raw).map_err(|_| ThreadStartError::InvalidResponse)?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .unwrap_or("unknown app-server error");
        return Err(ThreadStartError::Server(message.to_string()));
    }
    let thread = value
        .pointer("/result/thread")
        .and_then(Value::as_object)
        .ok_or(ThreadStartError::NotConfirmed)?;
    let id = thread
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(ThreadStartError::NotConfirmed)?;
    let path = thread
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or(ThreadStartError::NotConfirmed)?;
    Ok((id.to_string(), path.to_string()))
}

pub fn parse_turn_start_response(raw: &str) -> Result<String, ThreadDriverError> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| ThreadDriverError::Protocol("invalid turn/start response".into()))?;
    parse_turn_start_response_value(&value)
}

/// Value-based twin of [`parse_turn_start_response`] so the actor parses each
/// inbound frame exactly once instead of re-serializing it to parse again.
pub fn parse_turn_start_response_value(value: &Value) -> Result<String, ThreadDriverError> {
    if let Some(error) = value.get("error") {
        return Err(ThreadDriverError::Protocol(server_error(error)));
    }
    value
        .pointer("/result/turn/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ThreadDriverError::Protocol("turn id was not confirmed".into()))
}

/// Parse a `turn/completed` notification. The app-server has emitted both a
/// nested `params.turn` shape and a flattened params shape across versions;
/// accept either while requiring the positive notification marker.
pub fn parse_turn_completed_notification(raw: &str) -> Option<TurnResult> {
    let value: Value = serde_json::from_str(raw).ok()?;
    parse_turn_completed_value(&value)
}

/// Value-based twin of [`parse_turn_completed_notification`]; see
/// [`parse_turn_start_response_value`] for why it exists.
pub fn parse_turn_completed_value(value: &Value) -> Option<TurnResult> {
    if value.get("method").and_then(Value::as_str) != Some("turn/completed") {
        return None;
    }
    let params = value.get("params")?;
    let turn = params.get("turn").unwrap_or(params);
    let turn_id = turn
        .get("id")
        .or_else(|| params.get("turnId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_string();
    let status = turn
        .get("status")
        .or_else(|| params.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("completed")
        .to_string();
    let items = turn
        .get("items")
        .or_else(|| params.get("items"))
        .and_then(Value::as_array);
    let text = items
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    Some(TurnResult {
        turn_id,
        status,
        text,
        raw: value.clone(),
    })
}

pub struct CodexThread {
    child: Child,
    stdin: ChildStdin,
    /// `None` once [`CodexThread::into_actor`] moved stdout into the actor's
    /// read pump; the legacy read paths below then refuse rather than spin.
    stdout: Option<BufReader<ChildStdout>>,
    pending: VecDeque<Value>,
    /// `turn/completed` notifications parsed once at push, keyed by turn id
    /// (Change 10: `take_completed` used to re-serialize every parked entry on
    /// each check). Capped? No: the legacy path claims its entry on match and
    /// `drive_turn` is one turn at a time, so the map holds only the
    /// completions that arrived while a DIFFERENT frame class was awaited.
    completed_turns: HashMap<String, TurnResult>,
    next_id: u64,
    thread_id: String,
    rollout_path: PathBuf,
    cwd: PathBuf,
    effort: Option<String>,
    current_turn_id: Option<String>,
}

/// Park one inbound frame: id-matched responses stay `Value`s in the deque,
/// `turn/completed` notifications are parsed ONCE here and keyed by turn id.
/// Pure so the parse-once contract is unit-testable without a child process.
fn park_frame(
    pending: &mut VecDeque<Value>,
    completed: &mut HashMap<String, TurnResult>,
    frame: Value,
) {
    match parse_turn_completed_value(&frame) {
        Some(turn) => {
            completed.insert(turn.turn_id.clone(), turn);
        }
        None => pending.push_back(frame),
    }
}

impl CodexThread {
    pub async fn start(
        cwd: impl Into<PathBuf>,
        model: Option<&str>,
        yolo: bool,
        effort: Option<&str>,
    ) -> Result<Self, ThreadDriverError> {
        let cwd = cwd.into();
        let mut driver = Self::launch(cwd.clone()).await?;
        driver.initialize().await?;
        let request = thread_start_request_with_options(1, &cwd, model, yolo, "never");
        let response = driver.request(1, request).await?;
        let (thread_id, rollout_path) = parse_thread_start_response(&response)
            .map_err(|error| ThreadDriverError::Protocol(error.to_string()))?;
        driver.thread_id = thread_id;
        driver.rollout_path = PathBuf::from(rollout_path);
        driver.effort = effort
            .filter(|effort| !effort.is_empty())
            .map(str::to_string);
        Ok(driver)
    }

    pub async fn resume(
        cwd: impl Into<PathBuf>,
        thread_id: &str,
        model: Option<&str>,
        yolo: bool,
        effort: Option<&str>,
    ) -> Result<Self, ThreadDriverError> {
        if thread_id.trim().is_empty() {
            return Err(ThreadDriverError::Protocol(
                "harness_session_id is required for codex resume".into(),
            ));
        }
        let cwd = cwd.into();
        let mut driver = Self::launch(cwd.clone()).await?;
        driver.initialize().await?;
        let request = thread_resume_request_with_options(1, thread_id, &cwd, model, yolo, "never");
        let response = driver.request(1, request).await?;
        let (confirmed_id, rollout_path) = parse_thread_start_response(&response)
            .map_err(|error| ThreadDriverError::Protocol(error.to_string()))?;
        if confirmed_id != thread_id {
            return Err(ThreadDriverError::Protocol(format!(
                "thread/resume returned {confirmed_id}, expected {thread_id}"
            )));
        }
        driver.thread_id = confirmed_id;
        driver.rollout_path = PathBuf::from(rollout_path);
        driver.effort = effort
            .filter(|effort| !effort.is_empty())
            .map(str::to_string);
        Ok(driver)
    }

    async fn launch(cwd: PathBuf) -> Result<Self, ThreadDriverError> {
        let mut child = Command::new("codex")
            .arg("app-server")
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(ThreadDriverError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ThreadDriverError::Protocol("app-server stdin missing".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ThreadDriverError::Protocol("app-server stdout missing".into()))?;
        Ok(Self {
            child,
            stdin,
            stdout: Some(BufReader::new(stdout)),
            pending: VecDeque::new(),
            completed_turns: HashMap::new(),
            next_id: 2,
            thread_id: String::new(),
            rollout_path: PathBuf::new(),
            cwd,
            effort: None,
            current_turn_id: None,
        })
    }

    async fn initialize(&mut self) -> Result<(), ThreadDriverError> {
        let request = initialize_request_json();
        let response = self.request_value("init", request).await?;
        if response.get("error").is_some() {
            return Err(ThreadDriverError::Protocol(server_error(
                response.get("error").unwrap_or(&Value::Null),
            )));
        }
        self.stdin
            .write_all(format!("{}\n", initialized_notification_json()).as_bytes())
            .await
            .map_err(ThreadDriverError::Io)?;
        self.stdin.flush().await.map_err(ThreadDriverError::Io)
    }

    async fn request(&mut self, id: u64, request: String) -> Result<String, ThreadDriverError> {
        self.request_value(id, request)
            .await
            .map(|value| value.to_string())
    }

    async fn request_value(
        &mut self,
        id: impl Into<Value>,
        request: String,
    ) -> Result<Value, ThreadDriverError> {
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(ThreadDriverError::Io)?;
        self.stdin.flush().await.map_err(ThreadDriverError::Io)?;
        let id = id.into();
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ThreadDriverError::Timeout);
            }
            let value = self.read_value(remaining).await?;
            if value.get("id") == Some(&id) {
                return Ok(value);
            }
            park_frame(&mut self.pending, &mut self.completed_turns, value);
        }
    }

    /// Read one JSON frame, bounded by `read_timeout`. Callers that wait on a
    /// turn pass their remaining whole-turn budget: a fixed per-frame timeout
    /// would fire during the app-server's quiet gaps and abort a turn that was
    /// still running.
    async fn read_value(&mut self, read_timeout: Duration) -> Result<Value, ThreadDriverError> {
        let Some(stdout) = self.stdout.as_mut() else {
            return Err(ThreadDriverError::Protocol(
                "actor owns the read pump; legacy reads are unavailable".into(),
            ));
        };
        let mut line = String::new();
        let read = tokio::time::timeout(read_timeout, stdout.read_line(&mut line))
            .await
            .map_err(|_| ThreadDriverError::Timeout)?
            .map_err(ThreadDriverError::Io)?;
        if read == 0 {
            let status = self
                .child
                .try_wait()
                .ok()
                .flatten()
                .and_then(|status| status.code())
                .map(|code| format!(" (exit {code})"))
                .unwrap_or_default();
            return Err(ThreadDriverError::Protocol(format!(
                "app-server closed stdout{status}"
            )));
        }
        serde_json::from_str(line.trim())
            .map_err(|_| ThreadDriverError::Protocol("app-server emitted a non-JSON frame".into()))
    }

    async fn take_completed(&mut self, turn_id: &str) -> Result<TurnResult, ThreadDriverError> {
        // Parse-once (Change 10): completions were parked as raw Values and
        // re-serialized per check; they are parsed once at push and claimed
        // from the map here. No entry is ever re-serialized.
        if let Some(turn) = self.completed_turns.remove(turn_id) {
            return Ok(turn);
        }
        // The turn budget is the only ceiling. A frame-count bound aborts
        // turns larger than the constant after the app-server already ran
        // them, and a per-frame timeout aborts turns with quiet gaps.
        let deadline = Instant::now() + TURN_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ThreadDriverError::Timeout);
            }
            let value = self.read_value(remaining).await?;
            park_frame(&mut self.pending, &mut self.completed_turns, value);
            if let Some(turn) = self.completed_turns.remove(turn_id) {
                return Ok(turn);
            }
        }
    }

    pub async fn drive_turn(&mut self, text: &str) -> Result<TurnResult, ThreadDriverError> {
        let request_id = self.next_id;
        self.next_id += 1;
        let request = turn_start_request_json_with_effort(
            request_id,
            &self.thread_id,
            text,
            self.effort.as_deref(),
        );
        let response = self.request(request_id, request).await?;
        let turn_id = parse_turn_start_response(&response)?;
        self.current_turn_id = Some(turn_id.clone());
        let result = self.take_completed(&turn_id).await?;
        // INVARIANT (pinned, do not "fix"): `current_turn_id` is cleared ONLY
        // on this success path. After a client-side timeout the id is the only
        // interrupt handle left, and the actor's Interrupt command depends on
        // it surviving. The actor keeps the same rule: its driving turn id
        // survives a timed-out wait and dies only when the completion actually
        // routes or the child is dropped.
        self.current_turn_id = None;
        Ok(result)
    }

    /// Write-only `turn/start`: returns the request id so the actor can await
    /// the response on its own read pump. Does not touch `current_turn_id`;
    /// the caller pairs this with [`CodexThread::note_turn_started`] once the
    /// response confirms the turn id.
    pub async fn send_turn_start(&mut self, text: &str) -> Result<u64, ThreadDriverError> {
        let request_id = self.next_id;
        self.next_id += 1;
        let request = turn_start_request_json_with_effort(
            request_id,
            &self.thread_id,
            text,
            self.effort.as_deref(),
        );
        self.write_frame(&request).await?;
        Ok(request_id)
    }

    /// Write-only `turn/steer` with the server-enforced precondition.
    pub async fn send_steer(
        &mut self,
        expected_turn_id: &str,
        text: &str,
    ) -> Result<u64, ThreadDriverError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_frame(&turn_steer_request_json(
            id,
            &self.thread_id,
            expected_turn_id,
            text,
        ))
        .await?;
        Ok(id)
    }

    /// Write-only `turn/interrupt` against a full turn id.
    pub async fn send_interrupt(&mut self, turn_id: &str) -> Result<u64, ThreadDriverError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_frame(&turn_interrupt_request_json(id, &self.thread_id, turn_id))
            .await?;
        Ok(id)
    }

    /// Write-only `review/start`.
    pub async fn send_review(
        &mut self,
        target: &ReviewTarget,
        delivery: ReviewDelivery,
    ) -> Result<u64, ThreadDriverError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_frame(&crate::codex_inject::review_start_request_json_with_id(
            id,
            &self.thread_id,
            target,
            delivery,
        ))
        .await?;
        Ok(id)
    }

    async fn write_frame(&mut self, request: &str) -> Result<(), ThreadDriverError> {
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(ThreadDriverError::Io)?;
        self.stdin.flush().await.map_err(ThreadDriverError::Io)
    }

    /// Record the turn the driver is currently driving (actor path; the
    /// legacy path sets this inside `drive_turn`).
    pub fn note_turn_started(&mut self, turn_id: &str) {
        self.current_turn_id = Some(turn_id.to_string());
    }

    /// Clear `current_turn_id` ONLY when the named turn actually completed,
    /// preserving the drive_turn survivor invariant for the actor path too.
    pub fn note_turn_completed(&mut self, turn_id: &str) {
        if self.current_turn_id.as_deref() == Some(turn_id) {
            self.current_turn_id = None;
        }
    }

    pub async fn steer(
        &mut self,
        expected_turn_id: &str,
        text: &str,
    ) -> Result<String, ThreadDriverError> {
        let id = self.send_steer(expected_turn_id, text).await?;
        let response = self.wait_for_response(id).await?;
        parse_turn_start_response_value(&response)
    }

    pub async fn interrupt(&mut self, turn_id: &str) -> Result<(), ThreadDriverError> {
        let id = self.send_interrupt(turn_id).await?;
        let response = self.wait_for_response(id).await?;
        if let Some(error) = response.get("error") {
            return Err(ThreadDriverError::Protocol(server_error(error)));
        }
        Ok(())
    }

    /// Await an id-matched response on the LEGACY in-struct read path (the
    /// write already happened in the `send_*` call).
    async fn wait_for_response(&mut self, id: u64) -> Result<Value, ThreadDriverError> {
        let id = Value::from(id);
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ThreadDriverError::Timeout);
            }
            let value = self.read_value(remaining).await?;
            if value.get("id") == Some(&id) {
                return Ok(value);
            }
            park_frame(&mut self.pending, &mut self.completed_turns, value);
        }
    }

    pub async fn review(
        &mut self,
        target: &ReviewTarget,
        delivery: ReviewDelivery,
    ) -> Result<ReviewResult, ThreadDriverError> {
        let response = self
            .request_value(
                1,
                review_start_request_json(&self.thread_id, target, delivery),
            )
            .await?;
        let (turn_id, review_thread_id) = parse_review_start_response(&response.to_string())
            .map_err(|error| ThreadDriverError::Protocol(error.to_string()))?;
        Ok(ReviewResult {
            turn_id,
            review_thread_id,
        })
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn rollout_path(&self) -> &Path {
        &self.rollout_path
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn current_turn_id(&self) -> Option<&str> {
        self.current_turn_id.as_deref()
    }

    /// Convert this driver into a single-owner actor and hand back the cheap
    /// command handle. This replaces the old `Arc<Mutex<CodexThread>>` shape,
    /// where `drive_turn` held the guard across a whole (up to `TURN_TIMEOUT`)
    /// turn and every follow-up verb queued behind it. One task now owns the
    /// driver exclusively; stdout moves into a dedicated read pump (a
    /// `select!` arm on `read_line` is not cancel-safe: cancellation mid-line
    /// drops buffered bytes and corrupts the NDJSON stream). While a turn is
    /// driving, follow-ups STEER rather than block; only the wait for
    /// `turn/completed` is long, and that wait is the actor's idle loop, not a
    /// lock.
    ///
    /// `on_turn_done` fires once per completed turn, from the actor task, for
    /// every submitter class (ask, seed, mail steer) - the daemon uses it for
    /// the `agent_ask_done` event and the `last_message_at` bump.
    pub fn into_actor(
        mut self,
        on_turn_done: Arc<dyn Fn(TurnReceipt) + Send + Sync>,
    ) -> CodexThreadActor {
        let pid = self.pid();
        let (cmd_tx, cmd_rx) = mpsc::channel(THREAD_CHANNEL_CAP);
        let (frame_tx, frame_rx) = mpsc::channel(THREAD_CHANNEL_CAP);
        let shared = Arc::new(ActorShared {
            turn_id: std::sync::Mutex::new(self.current_turn_id.clone()),
        });
        let stdout = self
            .stdout
            .take()
            .expect("stdout is only taken here, once, at actor birth");
        tokio::spawn(read_pump(stdout, frame_tx));
        tokio::spawn(actor_task(
            self,
            frame_rx,
            cmd_rx,
            Arc::clone(&shared),
            on_turn_done,
        ));
        CodexThreadActor {
            tx: cmd_tx,
            pid,
            shared,
        }
    }
}

/// The actor's outbound command surface. Cloning is cheap; the daemon holds
/// one per registry name in `ctx.codex_threads`.
#[derive(Clone)]
pub struct CodexThreadActor {
    tx: mpsc::Sender<ThreadCommand>,
    pid: Option<u32>,
    shared: Arc<ActorShared>,
}

/// Turn-id cell shared between the actor task (writer) and handle holders
/// (readers): the in_flight ask receipt needs the turn id even after the
/// submitter's 90s wait expired. A std Mutex is fine - no await inside.
struct ActorShared {
    turn_id: std::sync::Mutex<Option<String>>,
}

impl ActorShared {
    fn set_turn_id(&self, turn_id: Option<String>) {
        *self.turn_id.lock().expect("turn-id cell poisoned") = turn_id;
    }

    fn current_turn_id(&self) -> Option<String> {
        self.turn_id.lock().expect("turn-id cell poisoned").clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnReceipt {
    pub turn_id: String,
    pub status: String,
    pub text: String,
}

impl From<TurnResult> for TurnReceipt {
    fn from(result: TurnResult) -> Self {
        Self {
            turn_id: result.turn_id,
            status: result.status,
            text: result.text,
        }
    }
}

pub enum ThreadCommand {
    /// Drive (or steer into) a turn; the reply resolves at completion.
    Submit {
        body: String,
        reply: oneshot::Sender<Result<TurnReceipt, String>>,
    },
    /// Interrupt the in-flight turn and report its terminal state.
    Interrupt {
        ack: oneshot::Sender<InterruptOutcome>,
    },
    /// Start a review turn; the reply is the review receipt.
    Review {
        target: ReviewTarget,
        delivery: ReviewDelivery,
        reply: oneshot::Sender<Result<ReviewResult, String>>,
    },
    /// End the actor task; dropping the driver kills the child
    /// (`kill_on_drop`), and the ack fires only AFTER that drop.
    Shutdown { ack: oneshot::Sender<()> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptOutcome {
    /// No turn was in flight; nothing to interrupt, safe to drop the child.
    NoTurnInFlight,
    /// The interrupted turn reached a terminal status (normally `interrupted`).
    Interrupted(TurnReceipt),
    /// The bounded settle wait expired with the turn still driving; the caller
    /// must drop the actor so `kill_on_drop` kills the child, and must not
    /// report a clean stop over the live turn.
    Timeout,
}

impl CodexThreadActor {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The turn currently driving, if any - the interrupt handle that
    /// survives a caller-side timeout (the pinned drive_turn invariant).
    pub fn current_turn_id(&self) -> Option<String> {
        self.shared.current_turn_id()
    }

    /// Queue a submit; the returned receiver resolves with the turn receipt at
    /// completion. The CALLER bounds the wait (the daemon's ask uses
    /// [`ask_wait`] and answers `in_flight` on expiry).
    pub async fn submit(
        &self,
        body: String,
    ) -> Result<oneshot::Receiver<Result<TurnReceipt, String>>, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ThreadCommand::Submit {
                body,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "codex thread actor is gone".to_string())?;
        Ok(reply_rx)
    }

    pub async fn interrupt(&self) -> Result<InterruptOutcome, String> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(ThreadCommand::Interrupt { ack: ack_tx })
            .await
            .map_err(|_| "codex thread actor is gone".to_string())?;
        ack_rx
            .await
            .map_err(|_| "codex thread actor is gone".to_string())
    }

    pub async fn review(
        &self,
        target: ReviewTarget,
        delivery: ReviewDelivery,
    ) -> Result<ReviewResult, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ThreadCommand::Review {
                target,
                delivery,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "codex thread actor is gone".to_string())?;
        reply_rx
            .await
            .map_err(|_| "codex thread actor is gone".to_string())
            .and_then(|inner| inner)
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(ThreadCommand::Shutdown { ack: ack_tx })
            .await
            .map_err(|_| "codex thread actor is gone".to_string())?;
        ack_rx
            .await
            .map_err(|_| "codex thread actor is gone".to_string())
    }
}

/// Sequential frame pump from the app-server's stdout into the actor. Owns the
/// reader so the actor's `select!` never holds a cancellable `read_line`.
/// Channel close (actor gone) or stdout EOF ends the task.
async fn read_pump(mut stdout: BufReader<ChildStdout>, tx: mpsc::Sender<Value>) {
    let mut line = String::new();
    loop {
        line.clear();
        match stdout.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                if tx.send(value).await.is_err() {
                    break;
                }
            }
        }
    }
}

struct Driving {
    turn_id: String,
    waiters: Vec<oneshot::Sender<Result<TurnReceipt, String>>>,
}

struct ActorCtx {
    driver: CodexThread,
    /// Inbound frames routed while awaiting a response for another id.
    pending: HashMap<u64, Value>,
    /// Completed turns keyed by id: claimed by waiters on match, parked
    /// (capped) when their waiter is gone, claimable by `await_turn_end`.
    completed: HashMap<String, TurnReceipt>,
    driving: Option<Driving>,
    shared: Arc<ActorShared>,
    on_turn_done: Arc<dyn Fn(TurnReceipt) + Send + Sync>,
}

async fn actor_task(
    driver: CodexThread,
    mut frames: mpsc::Receiver<Value>,
    mut cmds: mpsc::Receiver<ThreadCommand>,
    shared: Arc<ActorShared>,
    on_turn_done: Arc<dyn Fn(TurnReceipt) + Send + Sync>,
) {
    let mut ctx = ActorCtx {
        driver,
        pending: HashMap::new(),
        completed: HashMap::new(),
        driving: None,
        shared,
        on_turn_done,
    };
    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                match cmd {
                    None => {
                        // Every handle dropped; end the actor and kill the child.
                        ctx.fail_waiters("codex thread actor handles all dropped");
                        return;
                    }
                    Some(ThreadCommand::Shutdown { ack }) => {
                        ctx.fail_waiters("codex thread actor shut down");
                        drop(ctx.driver); // kill_on_drop signals the child here,
                        // before the ack, so a caller that waits for the ack
                        // never reads a live pid as stopped.
                        let _ = ack.send(());
                        return;
                    }
                    Some(cmd) => ctx.handle_command(cmd, &mut frames).await,
                }
            }
            frame = frames.recv() => {
                match frame {
                    None => {
                        // stdout closed: the app-server child is gone.
                        ctx.fail_waiters("codex app-server closed stdout");
                        return;
                    }
                    Some(frame) => ctx.route_frame(frame),
                }
            }
        }
    }
}

enum TurnEnd {
    /// The turn reached a terminal state; waiters were resolved by routing.
    Ended,
    /// The bound expired with the turn still driving.
    TimedOut,
}

impl ActorCtx {
    /// File one inbound frame without parsing anything twice: id-matched
    /// responses park by id; `turn/completed` notifications settle the driving
    /// turn (or park, capped, when nobody waits on it).
    fn route_frame(&mut self, frame: Value) {
        if frame.get("id").is_some() {
            if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                self.pending.insert(id, frame);
            }
            return;
        }
        if let Some(result) = parse_turn_completed_value(&frame) {
            self.complete_turn(result);
        }
        // Every other notification (turn/event noise) is dropped: the rollout
        // on disk is the durable record.
    }

    fn complete_turn(&mut self, result: TurnResult) {
        let turn_id = result.turn_id.clone();
        let receipt: TurnReceipt = result.into();
        self.park_completed(turn_id.clone(), receipt.clone());
        let matched = self
            .driving
            .take()
            .filter(|driving| driving.turn_id == turn_id);
        if let Some(driving) = matched {
            self.driver.note_turn_completed(&turn_id);
            self.shared.set_turn_id(None);
            for waiter in driving.waiters {
                let _ = waiter.send(Ok(receipt.clone()));
            }
            (self.on_turn_done)(receipt);
        }
    }

    fn park_completed(&mut self, turn_id: String, receipt: TurnReceipt) {
        if self.completed.len() >= COMPLETED_PARK_CAP {
            // Drop an arbitrary parked entry: an unclaimed completion is
            // telemetry (the rollout holds the real record), and an unbounded
            // map would grow on every waiter-gone completion.
            if let Some(oldest) = self.completed.keys().next().cloned() {
                self.completed.remove(&oldest);
            }
        }
        self.completed.insert(turn_id, receipt);
    }

    fn fail_waiters(&mut self, message: &str) {
        if let Some(driving) = self.driving.take() {
            self.shared.set_turn_id(None);
            for waiter in driving.waiters {
                let _ = waiter.send(Err(message.to_string()));
            }
        }
    }

    fn turn_ended(&self, turn_id: &str) -> bool {
        match &self.driving {
            Some(driving) => driving.turn_id != turn_id,
            None => true,
        }
    }

    /// Await one id-matched response, routing every frame that arrives first
    /// (a completion for the driving turn still resolves its waiters).
    async fn await_response(
        &mut self,
        id: u64,
        frames: &mut mpsc::Receiver<Value>,
    ) -> Result<Value, String> {
        if let Some(value) = self.pending.remove(&id) {
            return Ok(value);
        }
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("codex app-server response timed out".into());
            }
            let frame = match tokio::time::timeout(remaining, frames.recv()).await {
                Ok(Some(frame)) => frame,
                Ok(None) => return Err("codex app-server closed stdout".into()),
                Err(_) => return Err("codex app-server response timed out".into()),
            };
            self.route_frame(frame);
            if let Some(value) = self.pending.remove(&id) {
                return Ok(value);
            }
        }
    }

    /// Await a turn's terminal state. `Ended` means routing already resolved
    /// the waiters; the receipt itself stays claimable from the completed map.
    async fn await_turn_end(
        &mut self,
        turn_id: &str,
        bound: Duration,
        frames: &mut mpsc::Receiver<Value>,
    ) -> TurnEnd {
        if self.turn_ended(turn_id) {
            return TurnEnd::Ended;
        }
        let deadline = Instant::now() + bound;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return TurnEnd::TimedOut;
            }
            match tokio::time::timeout(remaining, frames.recv()).await {
                Ok(Some(frame)) => {
                    self.route_frame(frame);
                    if self.turn_ended(turn_id) {
                        return TurnEnd::Ended;
                    }
                }
                // Channel closed: no further frame can end the turn.
                Ok(None) => {
                    return if self.turn_ended(turn_id) {
                        TurnEnd::Ended
                    } else {
                        TurnEnd::TimedOut
                    }
                }
                // Elapsed slice with no frame; loop re-checks the deadline.
                Err(_) => continue,
            }
        }
    }

    async fn handle_command(&mut self, cmd: ThreadCommand, frames: &mut mpsc::Receiver<Value>) {
        match cmd {
            ThreadCommand::Submit { body, reply } => self.handle_submit(body, reply, frames).await,
            ThreadCommand::Interrupt { ack } => {
                let outcome = self.handle_interrupt(frames).await;
                let _ = ack.send(outcome);
            }
            ThreadCommand::Review {
                target,
                delivery,
                reply,
            } => {
                let result = match self.driver.send_review(&target, delivery).await {
                    Ok(id) => self.await_response(id, frames).await.and_then(|response| {
                        parse_review_start_response(&response.to_string())
                            .map(|(turn_id, review_thread_id)| ReviewResult {
                                turn_id,
                                review_thread_id,
                            })
                            .map_err(|error| error.to_string())
                    }),
                    Err(error) => Err(error.to_string()),
                };
                let _ = reply.send(result);
            }
            ThreadCommand::Shutdown { .. } => unreachable!("handled in the select arm"),
        }
    }

    async fn handle_submit(
        &mut self,
        body: String,
        reply: oneshot::Sender<Result<TurnReceipt, String>>,
        frames: &mut mpsc::Receiver<Value>,
    ) {
        let driving_turn = self.driving.as_ref().map(|driving| driving.turn_id.clone());
        match driving_turn {
            // Idle: drive a fresh turn. The reply resolves when the completion
            // routes in the main loop.
            None => self.start_turn(body, reply, frames).await,
            Some(expected) => {
                // Driving: steer into the in-flight turn instead of queueing
                // behind it. The steer ack returns in milliseconds; the
                // submitter rides the shared completion.
                let sent = self.driver.send_steer(&expected, &body).await;
                let ack = match sent {
                    Ok(id) => self.await_response(id, frames).await,
                    Err(error) => Err(error.to_string()),
                };
                // An error PAYLOAD is an Err here: a failed expectedTurnId
                // precondition arrives as a well-formed response with an
                // `error` body, not as a transport failure.
                let ack = ack.and_then(|response| {
                    if response.get("error").is_some() {
                        Err("turn/steer was refused".to_string())
                    } else {
                        Ok(response)
                    }
                });
                match ack {
                    Ok(_) => {
                        // The turn may have COMPLETED while the steer exchange
                        // ran (routing resolved the waiters and cleared
                        // `driving`); only attach the waiter to a live turn.
                        let still_driving = self
                            .driving
                            .as_mut()
                            .filter(|driving| driving.turn_id == expected);
                        match still_driving {
                            Some(driving) => driving.waiters.push(reply),
                            None => self.start_turn(body, reply, frames).await,
                        }
                    }
                    Err(_) => {
                        // Steer failed its expectedTurnId precondition: the
                        // turn completed in the race window. Drain the old
                        // completion (bounded) so its waiters resolve, then
                        // retry ONCE as a fresh turn/start - the submitter
                        // still gets a reply either way.
                        match self
                            .await_turn_end(&expected, TURN_SETTLE_TIMEOUT, frames)
                            .await
                        {
                            TurnEnd::Ended => {}
                            TurnEnd::TimedOut => {
                                let message = format!(
                                    "turn {expected} completed without a receipt and the \
                                     completion never arrived"
                                );
                                if let Some(driving) = self.driving.take() {
                                    self.shared.set_turn_id(None);
                                    for waiter in driving.waiters {
                                        let _ = waiter.send(Err(message.clone()));
                                    }
                                }
                            }
                        }
                        self.start_turn(body, reply, frames).await;
                    }
                }
            }
        }
    }

    async fn start_turn(
        &mut self,
        body: String,
        reply: oneshot::Sender<Result<TurnReceipt, String>>,
        frames: &mut mpsc::Receiver<Value>,
    ) {
        let sent = self.driver.send_turn_start(&body).await;
        let response = match sent {
            Ok(id) => self.await_response(id, frames).await,
            Err(error) => Err(error.to_string()),
        };
        match response
            .and_then(|value| parse_turn_start_response_value(&value).map_err(|e| e.to_string()))
        {
            Ok(turn_id) => {
                self.driver.note_turn_started(&turn_id);
                self.shared.set_turn_id(Some(turn_id.clone()));
                self.driving = Some(Driving {
                    turn_id,
                    waiters: vec![reply],
                });
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    async fn handle_interrupt(&mut self, frames: &mut mpsc::Receiver<Value>) -> InterruptOutcome {
        let Some(turn_id) = self.driving.as_ref().map(|driving| driving.turn_id.clone()) else {
            return InterruptOutcome::NoTurnInFlight;
        };
        // See the pinned drive_turn invariant: `turn_id` here IS the surviving
        // interrupt handle, also after any caller-side timeout.
        let sent = self.driver.send_interrupt(&turn_id).await;
        let ack = match sent {
            Ok(id) => self.await_response(id, frames).await,
            Err(error) => Err(error.to_string()),
        };
        match ack {
            Ok(response) if response.get("error").is_none() => {
                match self
                    .await_turn_end(&turn_id, TURN_SETTLE_TIMEOUT, frames)
                    .await
                {
                    TurnEnd::Ended => {
                        let receipt = self.completed.remove(&turn_id).unwrap_or(TurnReceipt {
                            turn_id: turn_id.clone(),
                            status: "interrupted".into(),
                            text: String::new(),
                        });
                        InterruptOutcome::Interrupted(receipt)
                    }
                    TurnEnd::TimedOut => InterruptOutcome::Timeout,
                }
            }
            _ => {
                // The interrupt RPC failed - commonly because the turn
                // completed and routing already ended it mid-exchange.
                if self.turn_ended(&turn_id) {
                    let receipt = self.completed.remove(&turn_id).unwrap_or(TurnReceipt {
                        turn_id: turn_id.clone(),
                        status: "completed".into(),
                        text: String::new(),
                    });
                    InterruptOutcome::Interrupted(receipt)
                } else {
                    InterruptOutcome::Timeout
                }
            }
        }
    }
}

fn thread_start_request_with_options(
    id: u64,
    cwd: &Path,
    model: Option<&str>,
    yolo: bool,
    approval_policy: &str,
) -> String {
    let mut params = json!({
        "cwd": cwd,
        "sandbox": if yolo { "danger-full-access" } else { "workspace-write" },
        "approvalPolicy": approval_policy,
    });
    if let Some(model) = model.filter(|model| !model.is_empty()) {
        params["model"] = json!(model);
    }
    json!({"id": id, "method": "thread/start", "params": params}).to_string()
}

fn thread_resume_request_with_options(
    id: u64,
    thread_id: &str,
    cwd: &Path,
    model: Option<&str>,
    yolo: bool,
    approval_policy: &str,
) -> String {
    let mut params = json!({
        "threadId": thread_id,
        "cwd": cwd,
        "sandbox": if yolo { "danger-full-access" } else { "workspace-write" },
        "approvalPolicy": approval_policy,
    });
    if let Some(model) = model.filter(|model| !model.is_empty()) {
        params["model"] = json!(model);
    }
    json!({"id": id, "method": "thread/resume", "params": params}).to_string()
}

fn server_error(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or("unknown app-server error")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_resume_carries_full_id_and_worktree() {
        let value: Value = serde_json::from_str(&thread_resume_request_json(
            "thread-1",
            "/tmp/worktree",
            "never",
        ))
        .unwrap();
        assert_eq!(value["method"], "thread/resume");
        assert_eq!(value["params"]["threadId"], "thread-1");
        assert_eq!(value["params"]["cwd"], "/tmp/worktree");
    }

    #[test]
    fn steer_requires_expected_turn_id() {
        let value: Value = serde_json::from_str(&turn_steer_request_json(
            7, "thread-1", "turn-1", "continue",
        ))
        .unwrap();
        assert_eq!(value["params"]["expectedTurnId"], "turn-1");
    }

    #[test]
    fn turn_start_carries_reasoning_effort_when_requested() {
        let value: Value = serde_json::from_str(&turn_start_request_json_with_effort(
            7,
            "thread-1",
            "continue",
            Some("high"),
        ))
        .unwrap();
        assert_eq!(value["params"]["effort"], "high");
    }

    #[test]
    fn completion_notification_extracts_positive_turn_marker_and_text() {
        let result = parse_turn_completed_notification(
            r#"{"method":"turn/completed","params":{"turn":{"id":"turn-1","status":"completed","items":[{"type":"agentMessage","text":"recalled TOKEN"}]}}}"#,
        )
        .unwrap();
        assert_eq!(result.turn_id, "turn-1");
        assert_eq!(result.text, "recalled TOKEN");
    }

    /// AC18: completed turns are parsed ONCE at push and claimed from a map.
    /// The old `take_completed` re-serialized every parked entry
    /// (`value.to_string()`) on each completed-turn check; this pins the
    /// replacement structure - two completions parked while a third is awaited
    /// come back from the map by turn id, and no parked entry is a
    /// `turn/completed` Value at all.
    #[test]
    fn parked_frames_parse_completions_once_and_claim_by_turn_id() {
        let mut pending = VecDeque::new();
        let mut completed = HashMap::new();
        for turn in ["turn-1", "turn-2", "turn-3"] {
            park_frame(
                &mut pending,
                &mut completed,
                serde_json::json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": turn, "status": "completed",
                                        "items": [{"type": "agentMessage", "text": "done"}]}}
                }),
            );
        }
        park_frame(
            &mut pending,
            &mut completed,
            serde_json::json!({"id": 9, "result": {"turn": {"id": "turn-9"}}}),
        );
        assert_eq!(completed.len(), 3, "each completion parsed once at push");
        assert_eq!(
            pending.len(),
            1,
            "id-matched responses stay Values in the deque"
        );
        assert!(
            !pending
                .iter()
                .any(|value| value.get("method").is_some_and(|m| m == "turn/completed")),
            "no completion is parked as a raw Value to re-serialize later"
        );
        let claimed = completed.remove("turn-2").expect("claimed from the map");
        assert_eq!(claimed.text, "done");
        assert_eq!(claimed.status, "completed");
        assert!(
            completed.contains_key("turn-3"),
            "unclaimed turns stay parked"
        );
    }
}
