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
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const APP_SERVER_READ_TIMEOUT: Duration = Duration::from_secs(15);
const TURN_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_FRAMES_PER_RESPONSE: usize = 256;

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
    json!({
        "id": id,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{"type": "text", "text": text}],
        }
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
        raw: value,
    })
}

pub struct CodexThread {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    pending: VecDeque<Value>,
    next_id: u64,
    thread_id: String,
    rollout_path: PathBuf,
    cwd: PathBuf,
    current_turn_id: Option<String>,
}

impl CodexThread {
    pub async fn start(
        cwd: impl Into<PathBuf>,
        model: Option<&str>,
        yolo: bool,
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
        Ok(driver)
    }

    pub async fn resume(
        cwd: impl Into<PathBuf>,
        thread_id: &str,
        model: Option<&str>,
        yolo: bool,
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
            stdout: BufReader::new(stdout),
            pending: VecDeque::new(),
            next_id: 2,
            thread_id: String::new(),
            rollout_path: PathBuf::new(),
            cwd,
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
        for _ in 0..MAX_FRAMES_PER_RESPONSE {
            let value = self.read_value().await?;
            if value.get("id") == Some(&id) {
                return Ok(value);
            }
            self.pending.push_back(value);
        }
        Err(ThreadDriverError::Protocol(
            "response frame ceiling exceeded".into(),
        ))
    }

    async fn read_value(&mut self) -> Result<Value, ThreadDriverError> {
        let mut line = String::new();
        let read = tokio::time::timeout(APP_SERVER_READ_TIMEOUT, self.stdout.read_line(&mut line))
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
        if let Some(index) = self.pending.iter().position(|value| {
            parse_turn_completed_notification(&value.to_string())
                .is_some_and(|turn| turn.turn_id == turn_id)
        }) {
            let value = self.pending.remove(index).expect("position was present");
            return parse_turn_completed_notification(&value.to_string())
                .ok_or_else(|| ThreadDriverError::Protocol("turn completion disappeared".into()));
        }
        for _ in 0..MAX_FRAMES_PER_RESPONSE {
            let value = self.read_value().await?;
            if let Some(turn) = parse_turn_completed_notification(&value.to_string()) {
                if turn.turn_id == turn_id {
                    return Ok(turn);
                }
            } else {
                self.pending.push_back(value);
            }
        }
        Err(ThreadDriverError::Protocol(
            "turn completion frame ceiling exceeded".into(),
        ))
    }

    pub async fn drive_turn(&mut self, text: &str) -> Result<TurnResult, ThreadDriverError> {
        let request_id = self.next_id;
        self.next_id += 1;
        let request = turn_start_request_json_with_id(request_id, &self.thread_id, text);
        let response = tokio::time::timeout(TURN_TIMEOUT, self.request(request_id, request))
            .await
            .map_err(|_| ThreadDriverError::Timeout)??;
        let turn_id = parse_turn_start_response(&response)?;
        self.current_turn_id = Some(turn_id.clone());
        let result = tokio::time::timeout(TURN_TIMEOUT, self.take_completed(&turn_id))
            .await
            .map_err(|_| ThreadDriverError::Timeout)??;
        self.current_turn_id = None;
        Ok(result)
    }

    pub async fn steer(
        &mut self,
        expected_turn_id: &str,
        text: &str,
    ) -> Result<String, ThreadDriverError> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self
            .request(
                id,
                turn_steer_request_json(id, &self.thread_id, expected_turn_id, text),
            )
            .await?;
        parse_turn_start_response(&response)
    }

    pub async fn interrupt(&mut self, turn_id: &str) -> Result<(), ThreadDriverError> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self
            .request(
                id,
                turn_interrupt_request_json(id, &self.thread_id, turn_id),
            )
            .await?;
        let value: Value = serde_json::from_str(&response)
            .map_err(|_| ThreadDriverError::Protocol("invalid interrupt response".into()))?;
        if let Some(error) = value.get("error") {
            return Err(ThreadDriverError::Protocol(server_error(error)));
        }
        Ok(())
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
    fn completion_notification_extracts_positive_turn_marker_and_text() {
        let result = parse_turn_completed_notification(
            r#"{"method":"turn/completed","params":{"turn":{"id":"turn-1","status":"completed","items":[{"type":"agentMessage","text":"recalled TOKEN"}]}}}"#,
        )
        .unwrap();
        assert_eq!(result.turn_id, "turn-1");
        assert_eq!(result.text, "recalled TOKEN");
    }
}
