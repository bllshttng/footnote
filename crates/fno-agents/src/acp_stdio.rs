use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use thiserror::Error;

#[derive(Debug)]
pub struct AcpProfile {
    pub tool: &'static str,
    pub agent_name: Option<&'static str>,
    pub auth_markers: &'static [&'static str],
    pub auth_refusal: &'static str,
    pub request_timeout: Duration,
    pub list_params_empty: bool,
    pub stderr_settle: Duration,
}

pub const GROK_PROFILE: AcpProfile = AcpProfile {
    tool: "grok",
    agent_name: None,
    auth_markers: &["not authenticated", "not signed in", "authentication required"],
    auth_refusal: "Grok authentication is required; run `grok login --device-code` or provide an operator-owned XAI_API_KEY.",
    request_timeout: Duration::from_secs(180),
    list_params_empty: false,
    stderr_settle: Duration::ZERO,
};

pub const KIMI_PROFILE: AcpProfile = AcpProfile {
    tool: "kimi",
    agent_name: Some("Kimi Code CLI"),
    auth_markers: &[
        "authentication required",
        "no provider configured",
        "not authenticated",
        "not signed in",
    ],
    auth_refusal: "kimi onboarding is incomplete: run `kimi login` (device code, --region mainland-cn or global), then complete provider setup in the TUI with /login, or add a provider via `kimi provider catalog` and set default_model in config.toml. A login alone is measured insufficient.",
    request_timeout: Duration::from_secs(180),
    list_params_empty: true,
    stderr_settle: Duration::from_secs(2),
};

pub const DSH_PROFILE: AcpProfile = AcpProfile {
    tool: "dsh",
    agent_name: Some("deepseek-harness-acp"),
    auth_markers: &["no api key for provider route"],
    auth_refusal: "dsh has no DeepSeek credential: export an operator-owned DEEPSEEK_API_KEY in the launching environment, or store it through the dsh credentials service. fno never synthesizes the key.",
    request_timeout: Duration::from_secs(180),
    list_params_empty: false,
    stderr_settle: Duration::ZERO,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    Refuse,
    AllowOnce,
    RejectOnce,
}

#[derive(Debug, Error)]
pub enum AcpError {
    #[error("could not start ACP child: {0}")]
    Spawn(String),
    #[error("ACP I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("ACP returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{tool} ACP read exceeded {seconds:.0}s with no response; stderr: {stderr}")]
    ReadTimeout {
        tool: &'static str,
        seconds: f64,
        stderr: String,
    },
    #[error("{tool} ACP stream ended before response id {id} for {method:?}; stderr: {stderr}")]
    StreamEnded {
        tool: &'static str,
        id: u64,
        method: String,
        stderr: String,
    },
    #[error("{tool} ACP child is gone (broken pipe, code {code}); stderr: {stderr}")]
    BrokenPipe {
        tool: &'static str,
        code: Option<i32>,
        stderr: String,
    },
    #[error("{tool} ACP {method} returned no positive result")]
    NoPositiveResult { tool: &'static str, method: String },
    #[error("{message} {detail}")]
    AuthRequired {
        message: &'static str,
        detail: String,
    },
    #[error("{tool} ACP {method} failed: {detail}")]
    Protocol {
        tool: &'static str,
        method: String,
        detail: String,
    },
    #[error("{tool} ACP permission refused for {tool_call}; options: {options}")]
    PermissionRefused {
        tool: &'static str,
        tool_call: String,
        options: String,
    },
    #[error("{tool} ACP does not support server request {method:?}")]
    ServerRequest { tool: &'static str, method: String },
    #[error("{tool} ACP session is not started")]
    NotStarted { tool: &'static str },
    #[error("{tool} ACP session has no session id")]
    NoSessionId { tool: &'static str },
}

impl AcpError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::AuthRequired { .. } | Self::PermissionRefused { .. } => 13,
            Self::BrokenPipe { code, .. } => match code {
                Some(code) if *code != 0 => *code,
                _ => 1,
            },
            Self::Io(_) | Self::Spawn(_) | Self::Json(_) => 1,
            _ => 2,
        }
    }
}

type Frame = Result<Value, String>;

pub struct AcpSession {
    profile: &'static AcpProfile,
    cwd: PathBuf,
    policy: PermissionPolicy,
    child: Arc<Mutex<Option<Child>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    frames: Mutex<mpsc::Receiver<Frame>>,
    stderr: Arc<Mutex<Vec<String>>>,
    notifications: Mutex<Vec<Value>>,
    next_id: AtomicU64,
    session_id: Arc<Mutex<Option<String>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct AcpCancelHandle {
    tool: &'static str,
    session_id: String,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    child: Arc<Mutex<Option<Child>>>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl AcpCancelHandle {
    pub fn cancel(&self) -> Result<(), AcpError> {
        send_json(
            self.tool,
            &self.stdin,
            &json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": {"sessionId": self.session_id},
            }),
            &self.child,
            &self.stderr,
        )
    }
}

impl AcpSession {
    pub fn start(
        profile: &'static AcpProfile,
        argv: Vec<String>,
        cwd: &Path,
        env: Option<HashMap<String, String>>,
    ) -> Result<Self, AcpError> {
        Self::start_with_policy(profile, argv, cwd, env, PermissionPolicy::Refuse)
    }

    pub fn start_with_policy(
        profile: &'static AcpProfile,
        argv: Vec<String>,
        cwd: &Path,
        env: Option<HashMap<String, String>>,
        policy: PermissionPolicy,
    ) -> Result<Self, AcpError> {
        if argv.is_empty() {
            return Err(AcpError::Spawn("empty argv".into()));
        }
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(env) = env {
            command.envs(env);
        }
        let mut child = command
            .spawn()
            .map_err(|error| AcpError::Spawn(format!("{}: {error}", argv[0])))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpError::Spawn(format!("{} child has no stdin pipe", profile.tool)))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpError::Spawn(format!("{} child has no stdout pipe", profile.tool)))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| AcpError::Spawn(format!("{} child has no stderr pipe", profile.tool)))?;
        let (tx, rx) = mpsc::channel();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_drain = Arc::clone(&stderr);
        let reader = thread::spawn(move || read_frames(stdout, tx));
        let stderr_reader = thread::spawn(move || drain_stderr(stderr_pipe, stderr_drain));
        Ok(Self {
            profile,
            cwd: cwd.to_path_buf(),
            policy,
            child: Arc::new(Mutex::new(Some(child))),
            stdin: Arc::new(Mutex::new(Some(stdin))),
            frames: Mutex::new(rx),
            stderr,
            notifications: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
            session_id: Arc::new(Mutex::new(None)),
            threads: Mutex::new(vec![reader, stderr_reader]),
        })
    }

    pub fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|lines| lines.join("\n"))
            .unwrap_or_default()
    }

    pub fn notifications(&self) -> Vec<Value> {
        self.notifications
            .lock()
            .map(|items| items.clone())
            .unwrap_or_default()
    }

    pub fn request(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let deadline = Instant::now() + self.profile.request_timeout;
        send_json(
            self.profile.tool,
            &self.stdin,
            &json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}),
            &self.child,
            &self.stderr,
        )?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.read_timeout());
            }
            let received = self
                .frames
                .lock()
                .map_err(|_| AcpError::Protocol {
                    tool: self.profile.tool,
                    method: method.into(),
                    detail: "ACP frame receiver was poisoned".into(),
                })?
                .recv_timeout(remaining);
            let frame = match received {
                Ok(Ok(frame)) => frame,
                Ok(Err(error)) => {
                    return Err(AcpError::Protocol {
                        tool: self.profile.tool,
                        method: method.into(),
                        detail: format!("invalid ACP frame: {error}"),
                    })
                }
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(self.read_timeout()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(AcpError::StreamEnded {
                        tool: self.profile.tool,
                        id,
                        method: method.into(),
                        stderr: self.stderr_text_or_empty(),
                    })
                }
            };
            if frame.get("method").is_some() && !frame.get("id").unwrap_or(&Value::Null).is_null() {
                self.answer_server_request(&frame)?;
                continue;
            }
            if frame.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(frame);
            }
            if let Ok(mut notifications) = self.notifications.lock() {
                notifications.push(frame);
            }
        }
    }

    fn answer_server_request(&self, frame: &Value) -> Result<(), AcpError> {
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
        if method == "session/request_permission" {
            let options = params
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let wanted = match self.policy {
                PermissionPolicy::Refuse => None,
                PermissionPolicy::AllowOnce => Some("allow_once"),
                PermissionPolicy::RejectOnce => Some("reject_once"),
            };
            let selected = wanted.and_then(|kind| {
                options
                    .iter()
                    .find(|option| option.get("kind").and_then(Value::as_str) == Some(kind))
            });
            if let Some(option) = selected {
                let option_id = option.get("optionId").cloned().unwrap_or(Value::Null);
                send_json(
                    self.profile.tool,
                    &self.stdin,
                    &json!({"jsonrpc":"2.0", "id":id, "result":{"outcome":{"outcome":"selected", "optionId":option_id}}}),
                    &self.child,
                    &self.stderr,
                )?;
                return Ok(());
            }
            send_json(
                self.profile.tool,
                &self.stdin,
                &json!({"jsonrpc":"2.0", "id":id, "result":{"outcome":{"outcome":"cancelled"}}}),
                &self.child,
                &self.stderr,
            )?;
            return Err(AcpError::PermissionRefused {
                tool: self.profile.tool,
                tool_call: params
                    .pointer("/toolCall/title")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown tool call>")
                    .to_string(),
                options: serde_json::to_string(&options).unwrap_or_default(),
            });
        }
        send_json(
            self.profile.tool,
            &self.stdin,
            &json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32601, "message":"Method not found"}}),
            &self.child,
            &self.stderr,
        )?;
        Err(AcpError::ServerRequest {
            tool: self.profile.tool,
            method: method.into(),
        })
    }

    pub fn result(&self, response: Value, method: &str) -> Result<Value, AcpError> {
        if let Some(error) = response.get("error").and_then(Value::as_object) {
            let detail = [
                error.get("message").map(error_part),
                error.get("data").map(error_part),
            ]
            .into_iter()
            .flatten()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
            let lowered = detail.to_lowercase();
            if self
                .profile
                .auth_markers
                .iter()
                .any(|marker| lowered.contains(marker))
            {
                let mut settled = self.stderr_text();
                if settled.is_empty() && !self.profile.stderr_settle.is_zero() {
                    let until = Instant::now() + self.profile.stderr_settle;
                    while settled.is_empty() && Instant::now() < until {
                        thread::sleep(Duration::from_millis(50));
                        settled = self.stderr_text();
                        if self
                            .child
                            .lock()
                            .ok()
                            .and_then(|mut child| {
                                child
                                    .as_mut()
                                    .and_then(|child| child.try_wait().ok().flatten())
                            })
                            .is_some()
                        {
                            break;
                        }
                    }
                }
                let detail = if settled.is_empty() {
                    detail
                } else {
                    format!("{detail}; stderr: {settled}")
                };
                return Err(AcpError::AuthRequired {
                    message: self.profile.auth_refusal,
                    detail,
                });
            }
            return Err(AcpError::Protocol {
                tool: self.profile.tool,
                method: method.into(),
                detail,
            });
        }
        response
            .get("result")
            .and_then(Value::as_object)
            .map(|_| response.get("result").cloned().unwrap_or_else(|| json!({})))
            .ok_or_else(|| AcpError::NoPositiveResult {
                tool: self.profile.tool,
                method: method.into(),
            })
    }

    pub fn initialize(&self) -> Result<Value, AcpError> {
        let result = self.result(
            self.request("initialize", initialize_params())?,
            "initialize",
        )?;
        if result.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
            return Err(AcpError::Protocol {
                tool: self.profile.tool,
                method: "initialize".into(),
                detail: "initialize did not return protocolVersion 1".into(),
            });
        }
        if let Some(expected) = self.profile.agent_name {
            if result.pointer("/agentInfo/name").and_then(Value::as_str) != Some(expected) {
                return Err(AcpError::Protocol {
                    tool: self.profile.tool,
                    method: "initialize".into(),
                    detail: format!("initialize agentInfo.name did not equal {expected:?}"),
                });
            }
        }
        Ok(result)
    }

    pub fn session_new(&self, params: Value) -> Result<String, AcpError> {
        let result = self.result(self.request("session/new", params)?, "session/new")?;
        let id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| AcpError::NoPositiveResult {
                tool: self.profile.tool,
                method: "session/new".into(),
            })?
            .to_string();
        if let Ok(mut slot) = self.session_id.lock() {
            *slot = Some(id.clone());
        }
        Ok(id)
    }

    pub fn session_load(&self, session_id: &str) -> Result<Value, AcpError> {
        let result =
            self.session_verb("session/load", session_id, json!({"sessionId":session_id}))?;
        if let Ok(mut slot) = self.session_id.lock() {
            *slot = Some(session_id.to_string());
        }
        Ok(result)
    }

    pub fn session_resume(&self, session_id: &str) -> Result<Value, AcpError> {
        let result = self.session_verb(
            "session/resume",
            session_id,
            json!({"sessionId":session_id, "cwd":self.cwd, "mcpServers":[]}),
        )?;
        if let Ok(mut slot) = self.session_id.lock() {
            *slot = Some(session_id.to_string());
        }
        Ok(result)
    }

    pub fn session_close(&self, session_id: &str) -> Result<Value, AcpError> {
        let result =
            self.session_verb("session/close", session_id, json!({"sessionId":session_id}))?;
        if let Ok(mut slot) = self.session_id.lock() {
            if slot.as_deref() == Some(session_id) {
                *slot = None;
            }
        }
        Ok(result)
    }

    fn session_verb(
        &self,
        method: &str,
        session_id: &str,
        params: Value,
    ) -> Result<Value, AcpError> {
        let _ = session_id;
        self.result(self.request(method, params)?, method)
    }

    pub fn session_list(&self) -> Result<Value, AcpError> {
        let params = if self.profile.list_params_empty {
            json!({})
        } else {
            json!({"cwd": self.cwd})
        };
        self.result(self.request("session/list", params)?, "session/list")
    }

    pub fn prompt(&self, text: &str) -> Result<Value, AcpError> {
        let session_id = self
            .session_id
            .lock()
            .map_err(|_| AcpError::NoSessionId {
                tool: self.profile.tool,
            })?
            .clone()
            .ok_or(AcpError::NoSessionId {
                tool: self.profile.tool,
            })?;
        let result = self.request(
            "session/prompt",
            json!({"sessionId":session_id, "prompt":[{"type":"text", "text":text}]}),
        )?;
        self.result(result, "session/prompt")
    }

    pub fn cancel_handle(&self) -> Result<AcpCancelHandle, AcpError> {
        let session_id = self
            .session_id
            .lock()
            .map_err(|_| AcpError::NoSessionId {
                tool: self.profile.tool,
            })?
            .clone()
            .ok_or(AcpError::NoSessionId {
                tool: self.profile.tool,
            })?;
        Ok(AcpCancelHandle {
            tool: self.profile.tool,
            session_id,
            stdin: Arc::clone(&self.stdin),
            child: Arc::clone(&self.child),
            stderr: Arc::clone(&self.stderr),
        })
    }

    fn stderr_text_or_empty(&self) -> String {
        let text = self.stderr_text();
        if text.is_empty() {
            "<empty>".into()
        } else {
            text
        }
    }

    fn read_timeout(&self) -> AcpError {
        AcpError::ReadTimeout {
            tool: self.profile.tool,
            seconds: self.profile.request_timeout.as_secs_f64(),
            stderr: self.stderr_text_or_empty(),
        }
    }
}

fn error_part(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        if let Ok(mut stdin) = self.stdin.lock() {
            stdin.take();
        }
        if let Ok(mut child_slot) = self.child.lock() {
            if let Some(child) = child_slot.as_mut() {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(25))
                        }
                        _ => {
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                    }
                }
            }
            child_slot.take();
        }
        if let Ok(mut threads) = self.threads.lock() {
            for handle in threads.drain(..) {
                let _ = handle.join();
            }
        }
    }
}

pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientInfo": {"name":"fno", "version":"0.1.0"},
        "clientCapabilities": {},
    })
}

pub fn session_new_params(cwd: &Path, add_dirs: &[PathBuf]) -> Value {
    let mut params = json!({"cwd":cwd, "mcpServers":[]});
    if !add_dirs.is_empty() {
        params["additionalDirectories"] = json!(add_dirs);
    }
    params
}

pub fn grok_acp_argv(
    model: Option<&str>,
    effort: Option<&str>,
    plugin_dir: Option<&Path>,
) -> Vec<String> {
    let mut argv = vec!["grok".into(), "agent".into()];
    if let Some(dir) = plugin_dir {
        argv.extend(["--plugin-dir".into(), dir.to_string_lossy().into_owned()]);
    }
    argv.extend(["-m".into(), model.unwrap_or("grok-4.6").into()]);
    argv.extend(["--reasoning-effort".into(), effort.unwrap_or("high").into()]);
    argv.push("stdio".into());
    argv
}

pub fn kimi_acp_argv(model: Option<&str>) -> Vec<String> {
    let mut argv = vec!["kimi".into(), "acp".into()];
    if let Some(model) = model {
        argv.extend(["--model".into(), model.into()]);
    }
    argv
}

pub fn dsh_acp_argv() -> Vec<String> {
    vec!["dsh".into(), "--profile".into(), "acp".into()]
}

pub fn stage_plugin_dir() -> Option<PathBuf> {
    let stage = crate::plugin_install::state_root().join("plugin-stage/fno");
    stage
        .join(".claude-plugin/plugin.json")
        .is_file()
        .then_some(stage)
}

fn send_json(
    tool: &'static str,
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    value: &Value,
    child: &Arc<Mutex<Option<Child>>>,
    stderr: &Arc<Mutex<Vec<String>>>,
) -> Result<(), AcpError> {
    let mut guard = stdin.lock().map_err(|_| AcpError::NotStarted { tool })?;
    let stream = guard.as_mut().ok_or(AcpError::NotStarted { tool })?;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .and_then(|()| stream.flush())
        .map_err(|error| {
            if error.kind() != std::io::ErrorKind::BrokenPipe {
                return AcpError::Io(error);
            }
            let code = child
                .lock()
                .ok()
                .and_then(|mut child| {
                    child
                        .as_mut()
                        .and_then(|child| child.try_wait().ok().flatten())
                })
                .and_then(|status| status.code());
            let until = Instant::now() + Duration::from_millis(100);
            let mut stderr_text = String::new();
            while stderr_text.is_empty() && Instant::now() < until {
                stderr_text = stderr
                    .lock()
                    .map(|lines| lines.join("\n"))
                    .unwrap_or_default();
                if stderr_text.is_empty() {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            AcpError::BrokenPipe {
                tool,
                code,
                stderr: if stderr_text.is_empty() {
                    "<empty>".into()
                } else {
                    stderr_text
                },
            }
        })
}

fn read_frames(stdout: impl Read, tx: mpsc::Sender<Frame>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match serde_json::from_slice::<Value>(&line) {
                    Ok(Value::Object(frame)) => {
                        if tx.send(Ok(Value::Object(frame))).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
            Err(error) => {
                let _ = tx.send(Err(error.to_string()));
                break;
            }
        }
    }
}

fn drain_stderr(stderr_pipe: impl Read, lines: Arc<Mutex<Vec<String>>>) {
    let reader = BufReader::new(stderr_pipe);
    for line in reader.lines() {
        match line {
            Ok(line) => {
                if let Ok(mut lines) = lines.lock() {
                    lines.push(line);
                }
            }
            Err(_) => break,
        }
    }
}
