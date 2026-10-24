//! A blocking MSP client: Muse Session Protocol, newline-delimited JSON-RPC
//! 2.0 over the stdio of one `muse serve` child. A reader thread turns stdout
//! lines into frames; `next` hands them out in order, and `call` waits for
//! one response while it keeps every other frame for `next`.
//!
//! Every MSP command carries a client-minted UUIDv7 `commandId` (schema:
//! "Required; the server never mints one"). A server-initiated request
//! (`approval/request`, `userInput/request`) is answered at once with the
//! empty presentation receipt; the decision travels later as a command.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const STDERR_KEEP: usize = 40;

/// An MSP error, typed by `error.data.kind`, never by message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MspError {
    /// -32021: another host holds the session's writer lease.
    SessionInUse(String),
    /// -32030: the command was refused; `reason` is the schema's word.
    CommandRejected {
        reason: String,
        message: String,
    },
    /// -32602: the params did not match the schema.
    InvalidParams(String),
    NotInitialized(String),
    /// Any other typed error: its kind and message.
    Other {
        kind: String,
        message: String,
    },
    /// The child closed stdout or the pipe broke.
    Closed(String),
    Timeout(String),
}

impl std::fmt::Display for MspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MspError::SessionInUse(m) => write!(f, "sessionInUse: {m}"),
            MspError::CommandRejected { reason, message } => {
                write!(f, "commandRejected ({reason}): {message}")
            }
            MspError::InvalidParams(m) => write!(f, "invalidParams: {m}"),
            MspError::NotInitialized(m) => write!(f, "notInitialized: {m}"),
            MspError::Other { kind, message } => write!(f, "{kind}: {message}"),
            MspError::Closed(m) => write!(f, "muse serve closed: {m}"),
            MspError::Timeout(m) => write!(f, "no answer to {m}"),
        }
    }
}

impl MspError {
    /// The typed error a JSON-RPC `error` object names.
    pub fn from_error(error: &Value) -> MspError {
        let message = error["message"].as_str().unwrap_or("").to_string();
        let kind = error["data"]["kind"].as_str().unwrap_or("");
        match kind {
            "sessionInUse" => MspError::SessionInUse(message),
            "commandRejected" => MspError::CommandRejected {
                reason: error["data"]["reason"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| rejected_reason(&message)),
                message,
            },
            "invalidParams" => MspError::InvalidParams(message),
            "notInitialized" => MspError::NotInitialized(message),
            "" => MspError::Other {
                kind: format!("code {}", error["code"]),
                message,
            },
            k => MspError::Other {
                kind: k.to_string(),
                message,
            },
        }
    }
}

/// Muse 1.4.12 puts the rejection reason last in the message
/// ("... rejected: missing_run") and not in `data`.
fn rejected_reason(message: &str) -> String {
    message
        .rsplit_once("rejected: ")
        .map(|(_, r)| r.trim().to_string())
        .unwrap_or_default()
}

/// One frame from the server.
#[derive(Debug, Clone)]
pub enum Incoming {
    Response {
        id: u64,
        result: Result<Value, MspError>,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// A server-initiated request. The client has already sent its receipt.
    Request {
        method: String,
        params: Value,
    },
}

/// Sort one decoded frame. None for a frame that is none of the three.
pub fn classify(frame: &Value) -> Option<Incoming> {
    let method = frame["method"].as_str();
    match (method, frame.get("id")) {
        (Some(m), Some(_)) => Some(Incoming::Request {
            method: m.to_string(),
            params: frame["params"].clone(),
        }),
        (Some(m), None) => Some(Incoming::Notification {
            method: m.to_string(),
            params: frame["params"].clone(),
        }),
        (None, Some(id)) => Some(Incoming::Response {
            id: id.as_u64()?,
            result: match frame.get("error") {
                Some(e) => Err(MspError::from_error(e)),
                None => Ok(frame["result"].clone()),
            },
        }),
        (None, None) => None,
    }
}

/// A caller-assigned UUIDv7: 48 bits of Unix milliseconds, then random bits,
/// with the version and variant nibbles set.
pub fn mint_uuid_v7() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable");
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    bytes[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: mpsc::Receiver<Value>,
    backlog: VecDeque<Incoming>,
    next_id: u64,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Client {
    /// Start `cmd` with piped stdio, in its own process group.
    pub fn spawn(mut cmd: Command) -> Result<Client, MspError> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| MspError::Closed(format!("cannot start {:?}: {e}", cmd.get_program())))?;
        let (tx, frames) = mpsc::channel();
        let stdout = child.stdout.take().expect("piped stdout");
        std::thread::spawn(move || read_frames(stdout, tx));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(pipe) = child.stderr.take() {
            let lines = Arc::clone(&stderr);
            std::thread::spawn(move || keep_stderr(pipe, lines));
        }
        Ok(Client {
            stdin: child.stdin.take(),
            child,
            frames,
            backlog: VecDeque::new(),
            next_id: 0,
            stderr,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn stderr_text(&self) -> String {
        self.stderr.lock().map(|l| l.join("\n")).unwrap_or_default()
    }

    fn write(&mut self, frame: &Value) -> Result<(), MspError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| MspError::Closed("stdin closed".into()))?;
        let mut line = frame.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| MspError::Closed(e.to_string()))
    }

    /// Send a request; the answer arrives later through `next` with this id.
    pub fn request(&mut self, method: &str, params: Value) -> Result<u64, MspError> {
        self.next_id += 1;
        let id = self.next_id;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    pub fn notify(&mut self, method: &str) -> Result<(), MspError> {
        self.write(&json!({"jsonrpc": "2.0", "method": method}))
    }

    /// The next frame, waiting up to `wait`. Err only when stdout closed.
    pub fn next(&mut self, wait: Duration) -> Result<Option<Incoming>, MspError> {
        if let Some(frame) = self.backlog.pop_front() {
            return Ok(Some(frame));
        }
        let raw = match self.frames.recv_timeout(wait) {
            Ok(v) => v,
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(MspError::Closed(self.stderr_text()))
            }
        };
        if raw["method"].is_string() && raw.get("id").is_some() {
            let receipt = json!({"jsonrpc": "2.0", "id": raw["id"], "result": {}});
            self.write(&receipt)?;
        }
        Ok(classify(&raw))
    }

    /// Send a request and wait for its answer. Other frames stay queued.
    pub fn call(&mut self, method: &str, params: Value, wait: Duration) -> Result<Value, MspError> {
        let id = self.request(method, params)?;
        let deadline = Instant::now() + wait;
        let mut held = Vec::new();
        let answer = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err(MspError::Timeout(method.to_string()));
            }
            match self.next(left) {
                Ok(Some(Incoming::Response { id: got, result })) if got == id => break result,
                Ok(Some(other)) => held.push(other),
                Ok(None) => {}
                Err(e) => break Err(e),
            }
        };
        for frame in held.into_iter().rev() {
            self.backlog.push_front(frame);
        }
        answer
    }

    /// `initialize` then `initialized`. Returns the initialize result, which
    /// names `serverInfo.version` and `schema.fingerprint`.
    pub fn initialize(&mut self, wait: Duration) -> Result<Value, MspError> {
        let info = json!({"name": "fno", "version": env!("CARGO_PKG_VERSION")});
        let result = self.call("initialize", json!({"clientInfo": info}), wait)?;
        self.notify("initialized")?;
        Ok(result)
    }

    /// Close stdin, so `muse serve` ends, and reap it within `wait`.
    pub fn close(&mut self, wait: Duration) {
        self.stdin.take();
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // SAFETY: the child leads its own group (process_group(0) above).
        unsafe { libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

fn read_frames(stdout: impl Read, tx: mpsc::Sender<Value>) {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { return };
        let Ok(frame @ Value::Object(_)) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if tx.send(frame).is_err() {
            return;
        }
    }
}

fn keep_stderr(pipe: impl Read, lines: Arc<Mutex<Vec<String>>>) {
    for line in BufReader::new(pipe).lines().map_while(Result::ok) {
        if let Ok(mut lines) = lines.lock() {
            if lines.len() >= STDERR_KEEP {
                lines.remove(0);
            }
            lines.push(line);
        }
    }
}
