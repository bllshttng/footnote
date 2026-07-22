//! Client-side `codex exec` ask path (ab-0429c6e1).
//!
//! `codex` is a one-shot `codex exec --json` subprocess (NOT a PTY agent):
//! it emits a JSONL stream to stdout, the Rust client drains and parses it,
//! and returns the reply text. The fno daemon cannot handle this path because
//! `handle_ask` renders a PTY screen; byte-parity with Python's
//! `providers/codex.py` requires a direct subprocess approach.
//!
//! **Byte-parity is the contract.** The observable behavior (stdout reply,
//! exit code, events.jsonl fields) must match Python's implementation.
//!
//! # Architecture
//!
//! - **Wave B1** (this file, pure core): argv builders, `inject_from_name`,
//!   JSONL line parser, error enum + exit-code map. No I/O.
//! - **Wave B2** (this file, subprocess driver): `run_codex` subprocess with
//!   own-pgrp, watchdog SIGTERM->SIGKILL, grace reap, output.jsonl tee;
//!   `codex_create` / `codex_resume`; `dispatch_codex_ask` orchestrator;
//!   `maybe_run_codex_ask` client entry point.
//!
//! # Locked Decisions (from Python codex.py)
//!
//! - bounded default (create): global `--ask-for-approval never` before `exec`
//!   plus the `exec` flag `--sandbox workspace-write` after it. (`-a` is a
//!   top-level codex flag in >= 0.133.0, not an `exec` flag.)
//! - full yolo (explicit): `--dangerously-bypass-approvals-and-sandbox`.
//! - LD7: `inject_from_name` is plain `[from: X]\n\n<prompt>`, no escaping.
//! - LD8: `output.jsonl` is append-only, line-buffered tee (create & resume).
//! - LD11: stdin=DEVNULL.
//! - LD12: stderr merged into stdout (stderr=STDOUT).
//! - LD14 warn-on-drift: NoSessionId carries observed event type names.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::claude_ask::emit_event;
use crate::paths::AgentsHome;
use crate::state::{load_registry, update_registry};
use crate::AgentStatus;

// ===========================================================================
// Constants (pinned from codex 0.130.0 JSONL capture)
// ===========================================================================

/// `thread.started` — carries `thread_id` (UUID), used to capture session_id.
const EV_SESSION: &str = "thread.started";
/// `turn.completed` — end of turn; break the read loop.
const EV_COMPLETE: &str = "turn.completed";
/// `item.completed` — envelope; discriminated by `item.type`.
const EV_ITEM: &str = "item.completed";
/// `agent_message` item type — carries `item.text` (assistant reply).
const ITEM_MESSAGE: &str = "agent_message";
/// `error` item type — carries `item.message` (soft error text).
const ITEM_ERROR: &str = "error";

/// Lock-acquisition ceiling (mirrors claude_ask.rs `LOCK_ACQUIRE_TIMEOUT`).
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default followup timeout (mirrors Python dispatch.py's timeout default).
const DEFAULT_FOLLOWUP_TIMEOUT: Duration = Duration::from_secs(600);

// ===========================================================================
// Pure-fn helpers (Wave B1: no I/O, fully unit-testable)
// ===========================================================================

/// Prepend `[from: <from_name>]\n\n` to `prompt` (Locked Decision 7).
/// Plain concatenation; no escaping of any kind.
pub fn inject_from_name(prompt: &str, from_name: &str) -> String {
    format!("[from: {}]\n\n{}", from_name, prompt)
}

/// Argv tokens for the create-path sandbox posture (bounded-posture amendment).
/// - bounded (default): `["--sandbox", "workspace-write"]` - workspace sandbox.
/// - full yolo (explicit): `["--dangerously-bypass-approvals-and-sandbox"]` -
///   unsandboxed bypass. The two are mutually exclusive; never combine them.
///
/// `--sandbox` is an `exec`-subcommand flag, so these tokens go AFTER `exec`.
/// The approval policy is a SEPARATE global flag, see [`approval_flag`].
/// Mirror of `codex.py::sandbox_flag`.
pub fn sandbox_flag(yolo: bool) -> Vec<String> {
    if yolo {
        vec!["--dangerously-bypass-approvals-and-sandbox".to_string()]
    } else {
        vec!["--sandbox".to_string(), "workspace-write".to_string()]
    }
}

/// Argv tokens for the create-path approval policy (bounded-posture amendment).
/// - bounded (default): `["--ask-for-approval", "never"]` - never prompt (no
///   hang); a blocked action is returned to the model rather than waiting.
/// - full yolo: `[]` - `--dangerously-bypass-approvals-and-sandbox` (emitted by
///   [`sandbox_flag`]) already disables approval, so this stays empty.
///
/// CRITICAL: in codex >= 0.133.0 `-a/--ask-for-approval` is a GLOBAL flag on the
/// top-level `codex` command, NOT a flag on the `exec` subcommand. It must be
/// emitted BEFORE `exec` in the argv; placing it after `exec` makes clap reject
/// it with `error: unexpected argument '--ask-for-approval' found`, which aborts
/// the spawn before any session id is emitted. Mirror of `codex.py::approval_flag`.
pub fn approval_flag(yolo: bool) -> Vec<String> {
    if yolo {
        vec![]
    } else {
        vec!["--ask-for-approval".to_string(), "never".to_string()]
    }
}

/// Argv tokens for the sandbox mode on the resume path.
/// `codex exec resume` only accepts the bypass flag; `--sandbox` is not
/// honored on resume (verified against codex 0.130.0 --help).
/// - default: `[]`  (inherits original session sandbox)
/// - yolo:    `["--dangerously-bypass-approvals-and-sandbox"]`
pub fn sandbox_flag_resume(yolo: bool) -> Vec<String> {
    if yolo {
        vec!["--dangerously-bypass-approvals-and-sandbox".to_string()]
    } else {
        vec![]
    }
}

/// Build the create argv: `codex exec --json -C <cwd> --skip-git-repo-check <sandbox> <full_prompt>`.
/// `full_prompt` should already have been built via `inject_from_name`.
/// The subprocess cwd is NOT set via Popen(cwd=...) on the create path
/// (the `-C` flag handles it); Python codex.create passes `popen_cwd=None`.
pub fn build_argv_create(
    cwd: &Path,
    full_prompt: &str,
    yolo: bool,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    add_dir: Option<&str>,
) -> Vec<String> {
    // Approval is a GLOBAL flag and must precede `exec`; sandbox is an `exec`
    // flag and follows it. See `approval_flag` / `sandbox_flag`.
    let mut argv = vec!["codex".to_string()];
    argv.extend(approval_flag(yolo));
    argv.extend([
        "exec".to_string(),
        "--json".to_string(),
        "-C".to_string(),
        cwd.to_string_lossy().to_string(),
        "--skip-git-repo-check".to_string(),
    ]);
    // x-b6e2: a user `--add-dir` grants extra write access on `codex exec`.
    // codex's own cwd rides `-C` (separate flag), so this is purely additive -
    // no collision. Empty/None = unchanged argv.
    if let Some(d) = add_dir.filter(|d| !d.is_empty()) {
        argv.push("--add-dir".to_string());
        argv.push(d.to_string());
    }
    // x-c772: an explicit --model is forwarded to `codex exec --model <m>`
    // (empty/None = codex default). Exact passthrough, no fuzzy resolution.
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        argv.push("--model".to_string());
        argv.push(m.to_string());
    }
    if let Some(effort) = reasoning_effort.filter(|e| !e.is_empty()) {
        argv.push("-c".to_string());
        argv.push(format!("model_reasoning_effort={effort}"));
    }
    argv.extend(sandbox_flag(yolo));
    argv.push(full_prompt.to_string());
    argv
}

/// Build the resume argv: `codex exec resume <session_id> --json --skip-git-repo-check <sandbox_resume> <full_prompt>`.
/// `full_prompt` should already have been built via `inject_from_name`.
/// Resume does NOT use `-C`; the subprocess cwd is pinned via `Command::current_dir`.
pub fn build_argv_resume(session_id: &str, full_prompt: &str, yolo: bool) -> Vec<String> {
    let mut argv = vec![
        "codex".to_string(),
        "exec".to_string(),
        "resume".to_string(),
        session_id.to_string(),
        "--json".to_string(),
        "--skip-git-repo-check".to_string(),
    ];
    argv.extend(sandbox_flag_resume(yolo));
    argv.push(full_prompt.to_string());
    argv
}

// ===========================================================================
// JSONL event types (parsed from the codex JSONL stream)
// ===========================================================================

/// Parsed variant of one codex JSONL line.
#[derive(Debug)]
pub enum JsonlEvent {
    /// `thread.started` with a valid `thread_id`.
    ThreadStarted { thread_id: String },
    /// `item.completed` where `item.type == "agent_message"`.
    AgentMessage { text: String },
    /// `item.completed` where `item.type == "error"`.
    SoftError { message: String },
    /// `turn.completed` — end of turn.
    TurnCompleted,
    /// Any other JSON object (unknown event type or malformed known type).
    Other { type_name: Option<String> },
}

/// Parse one raw line from the codex JSONL stream.
///
/// Returns `None` for non-JSON lines (banners, Rust panics on merged stderr,
/// empty lines) — these are tee'd but not control-flow relevant.
/// Returns `Some(JsonlEvent::Other)` for valid JSON objects whose event type
/// is not in the pinned vocabulary, or for malformed known-type events.
pub fn parse_jsonl_line(line: &str) -> Option<JsonlEvent> {
    let line = line.trim_end_matches('\n');
    if line.is_empty() || !line.starts_with('{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if !v.is_object() {
        return None;
    }
    let ev_type = v.get("type").and_then(|t| t.as_str());
    match ev_type {
        Some(t) if t == EV_SESSION => {
            let thread_id = v.get("thread_id").and_then(|x| x.as_str())?;
            Some(JsonlEvent::ThreadStarted {
                thread_id: thread_id.to_string(),
            })
        }
        Some(t) if t == EV_COMPLETE => Some(JsonlEvent::TurnCompleted),
        Some(t) if t == EV_ITEM => {
            let item = v.get("item").and_then(|x| x.as_object());
            match item {
                Some(item) => {
                    let item_type = item.get("type").and_then(|x| x.as_str());
                    match item_type {
                        Some(t) if t == ITEM_MESSAGE => {
                            let text = item.get("text").and_then(|x| x.as_str()).unwrap_or("");
                            Some(JsonlEvent::AgentMessage {
                                text: text.to_string(),
                            })
                        }
                        Some(t) if t == ITEM_ERROR => {
                            let msg = item.get("message").and_then(|x| x.as_str()).unwrap_or("");
                            Some(JsonlEvent::SoftError {
                                message: msg.to_string(),
                            })
                        }
                        _ => Some(JsonlEvent::Other {
                            type_name: ev_type.map(String::from),
                        }),
                    }
                }
                None => Some(JsonlEvent::Other {
                    type_name: ev_type.map(String::from),
                }),
            }
        }
        Some(t) => Some(JsonlEvent::Other {
            type_name: Some(t.to_string()),
        }),
        None => Some(JsonlEvent::Other { type_name: None }),
    }
}

// ===========================================================================
// Error enum + exit-code map
// ===========================================================================

/// Errors from the codex ask path. Each variant maps to a specific Python-
/// compatible exit code.
#[derive(Debug)]
pub enum CodexAskError {
    /// codex binary not found (FileNotFoundError) — exit 127.
    NotFound,
    /// JSONL stream ended without a `thread.started` event — exit 11.
    /// `types_seen` carries the observed event types for forensics (LD14).
    NoSessionId { types_seen: Vec<String> },
    /// Cannot open the output.jsonl tee (EACCES/ENOSPC/etc.) — exit 12.
    TeeOpen { message: String },
    /// Wall-clock timeout — exit 15.
    Timeout { timeout_sec: f64 },
    /// codex exited non-zero and no reply was captured — exit = exit_code.
    /// Also used for other OSError at spawn time — exit 1.
    Invocation { exit_code: i32, message: String },
    /// SIGKILL escalation during reap — always exit 1 regardless of exit_code.
    /// A partial reply + SIGKILL is never a success (Python silent-failure-hunter row 4).
    SigkillEscalated { partial_exit_code: i32 },
    /// Non-transient OSError at Popen time (not NotFound) — exit 1.
    OsError { message: String },
    /// Operator SIGINT (Ctrl-C) forwarded to the codex group — exit 130.
    /// Mirrors Python's KeyboardInterrupt -> CPython exit 130 (ab-e7fdbcb6).
    Interrupted,
}

impl CodexAskError {
    /// Python-compatible exit code for this error.
    ///
    /// `NotFound` maps to 14 ("provider unavailable") directly here, mirroring
    /// Python's `dispatch.py` mapping of `CodexInvocationError(127)` ->
    /// `dispatch_create`'s `select_provider` -> 14. Centralizing the remap
    /// on the error type means both dispatch paths (create and resume) see
    /// the same final exit code (sigma-review type-design HIGH: previously
    /// only `dispatch_create` carried the inline `if exit_code == 127 { 14 }`
    /// remap; `dispatch_resume` would have leaked 127 for the same error).
    /// `SigkillEscalated` carries the partial exit code so a SIGKILL'd codex
    /// that exited with codex-reported non-zero before the signal preserves
    /// that code (matches codex.py:524 silent-failure-hunter row 4).
    pub fn exit_code(&self) -> i32 {
        match self {
            CodexAskError::NotFound => 14,
            CodexAskError::NoSessionId { .. } => 11,
            CodexAskError::TeeOpen { .. } => 12,
            CodexAskError::Timeout { .. } => 15,
            CodexAskError::Invocation { exit_code, .. } => {
                if *exit_code != 0 {
                    *exit_code
                } else {
                    1
                }
            }
            CodexAskError::SigkillEscalated { partial_exit_code } => {
                if *partial_exit_code != 0 {
                    *partial_exit_code
                } else {
                    1
                }
            }
            CodexAskError::OsError { .. } => 1,
            CodexAskError::Interrupted => 130,
        }
    }
}

impl std::fmt::Display for CodexAskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodexAskError::NotFound => write!(f, "codex binary not found on PATH"),
            CodexAskError::NoSessionId { types_seen } => write!(
                f,
                "codex did not emit session id; saw events: {:?}; expected one of: [\"thread.started\"]",
                types_seen
            ),
            CodexAskError::TeeOpen { message } => {
                write!(f, "codex provider: cannot open output tee: {}", message)
            }
            CodexAskError::Timeout { timeout_sec } => {
                write!(f, "codex timed out after {}s", timeout_sec)
            }
            CodexAskError::Invocation { exit_code, message } => {
                write!(f, "codex exited {} ({})", exit_code, message)
            }
            CodexAskError::SigkillEscalated { partial_exit_code } => write!(
                f,
                "codex was SIGKILL'd during reap (exit {}); partial reply discarded",
                partial_exit_code
            ),
            CodexAskError::OsError { message } => {
                write!(f, "codex provider: OSError invoking codex: {}", message)
            }
            CodexAskError::Interrupted => {
                write!(f, "codex interrupted by SIGINT (Ctrl-C)")
            }
        }
    }
}

impl std::error::Error for CodexAskError {}

// ===========================================================================
// Subprocess driver (Wave B2)
// ===========================================================================

/// Result of a successful codex create or resume invocation.
#[derive(Debug, Clone)]
pub struct CodexResult {
    /// Exit code (0 on happy path).
    pub exit_code: i32,
    /// Session UUID captured from `thread.started` (None on resume).
    pub session_id: Option<String>,
    /// Last assistant text (`agent_message`) or last soft-error text if no
    /// agent_message was emitted (Python silent-failure-hunter row 5 parity).
    pub last_msg: String,
    /// Wall-clock elapsed ms.
    pub duration_ms: u64,
}

/// Open the JSONL tee in append mode, creating parent dirs. Delegates to the
/// shared `subprocess_ask::open_tee` and tags the error as codex's `TeeOpen`.
fn open_tee(log_path: &Path) -> Result<std::fs::File, CodexAskError> {
    crate::subprocess_ask::open_tee(log_path).map_err(|e| CodexAskError::TeeOpen {
        message: e.to_string(),
    })
}

// SIGINT forwarding (ab-e7fdbcb6 / cv-cfdb7a56) now lives in the shared
// `subprocess_ask` module so codex and gemini share one implementation. See
// `crate::subprocess_ask::{SigintForwarder, ask_interrupted}`.

/// Shared subprocess driver for create and resume.
///
/// - stdin=DEVNULL (LD11)
/// - stderr merged into stdout (LD12)
/// - child in its own process group (`process::Command::process_group(0)`)
/// - wall-clock watchdog: SIGTERM to pgrp on timeout, SIGKILL after 2s grace
/// - grace reap after read loop: SIGTERM, then SIGKILL after 5s
/// - SIGKILL escalation is always a failure (silent-failure-hunter row 4)
fn run_codex(
    argv: &[String],
    output_path: &Path,
    timeout: Option<Duration>,
    expect_session: bool,
    popen_cwd: Option<&Path>,
    agent_self: Option<&str>,
) -> Result<CodexResult, CodexAskError> {
    use std::process::{Command, Stdio};

    let started = Instant::now();

    // Open the tee BEFORE Popen so path/permission errors surface as
    // structured CodexAskError::TeeOpen rather than a raw panic.
    let tee_fh = open_tee(output_path)?;

    // QoS (x-c5cc): every codex child is an fno-spawned worker process —
    // exec-wrap at background priority (identity when worker_qos=off).
    let argv =
        crate::spawn_gate::qos_wrap(popen_cwd.unwrap_or_else(|| Path::new(".")), argv.to_vec());
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null()); // LD11
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped()); // merged below via thread

    if let Some(cwd) = popen_cwd {
        cmd.current_dir(cwd);
    }

    // Set agent env vars when we know who we are (create path with agent_self).
    if let Some(name) = agent_self {
        cmd.env("FNO_AGENT_SELF", name);
        cmd.env("FNO_AGENT_PROVIDER", "codex");
    }

    // Put the child in its own process group so SIGTERM/SIGKILL propagate
    // to codex's subshells (sandbox tooling). Python uses start_new_session=True.
    unsafe {
        cmd.pre_exec(|| {
            // setpgid(0, 0): put this process in a new process group.
            libc::setpgid(0, 0);
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CodexAskError::NotFound);
        }
        Err(e) => {
            return Err(CodexAskError::OsError {
                message: e.to_string(),
            });
        }
    };

    let pid = child.id();

    // Forward operator Ctrl-C to the codex process group for the lifetime of
    // this call (ab-e7fdbcb6). Dropped at function end, after the child is
    // reaped, which restores the prior SIGINT disposition. codex is
    // `setpgid(0, 0)`, so its pgid equals its pid.
    let _sigint_guard = crate::subprocess_ask::SigintForwarder::install(pid);

    // Merge stderr into stdout via a dedicated drain thread (LD12).
    // We read stdout + stderr in separate threads and merge into the tee.
    // This avoids two-pipe deadlock (sigma-review PR #299 finding).
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");

    // The tee file handle is shared between the stdout drain (main thread)
    // and the stderr drain (side thread). We use a Mutex.
    let tee = std::sync::Arc::new(std::sync::Mutex::new(tee_fh));
    let tee_stderr = tee.clone();

    // Side thread: drain stderr and tee it. Non-fatal on write errors, but
    // cv-a602835d: a tee-write failure here used to be dropped entirely,
    // unlike the stdout drain which warns once per distinct error via
    // `tee_warned`. Mirror that warn-once behavior so a degraded stderr tee
    // is observable instead of silent. A read error ends the drain.
    let stderr_handle = std::thread::spawn(move || {
        let mut stderr_tee_warned: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for line in BufReader::new(stderr_pipe).lines() {
            match line {
                Ok(l) => {
                    let raw = format!("{}\n", l);
                    if let Ok(mut guard) = tee_stderr.lock() {
                        if let Err(e) = guard.write_all(raw.as_bytes()) {
                            let key = e.to_string();
                            if stderr_tee_warned.insert(key) {
                                eprintln!("codex provider: stderr tee write failed: {}", e);
                            }
                        } else {
                            // flush best-effort
                            let _ = guard.flush();
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Watchdog: if timeout fires, SIGTERM the pgrp; escalate to SIGKILL after
    // 2s. Cancelable so a happy-path completion (we `cancel()` before reaping)
    // skips the kill cascade. The full implementation (incl. the
    // `Some(Duration::ZERO)` == disabled Python parity and the recv_timeout
    // cancellation that fixed sigma-review HIGH x2) lives in
    // `subprocess_ask::AskWatchdog`.
    let mut watchdog = crate::subprocess_ask::AskWatchdog::spawn(pid, timeout);

    // Main thread: drain stdout, parse JSONL, tee every line.
    let mut session_id: Option<String> = None;
    let mut last_msg = String::new();
    let mut last_error_msg = String::new();
    let mut types_seen: Vec<String> = Vec::new();
    let mut tee_warned: std::collections::HashSet<String> = std::collections::HashSet::new();
    // cv-54a67325: distinguish a clean EOF (the `lines()` iterator ends with
    // `None`) from a genuine mid-stream read error (`Some(Err(_))`: EPIPE/EIO,
    // or an invalid-UTF-8 line on the stderr-merged stdout). The bare
    // `Err(_) => break` here previously swallowed the latter and returned the
    // partial reply as a "successful" `Ok`, hiding the truncation.
    let mut stream_read_error: Option<String> = None;
    let mut broke_on_complete = false;

    let stdout_reader = BufReader::new(stdout_pipe);
    for raw_line in stdout_reader.lines() {
        let raw = match raw_line {
            Ok(l) => l,
            Err(e) => {
                // Surface the read error (observability) and stop draining.
                // The post-loop guard turns a truncation-without-completion
                // into a loud failure instead of a silent partial success.
                eprintln!("codex provider: stdout stream read error: {}", e);
                stream_read_error = Some(e.to_string());
                break;
            }
        };
        let tee_line = format!("{}\n", raw);
        // Tee every line (LD8); failure is non-fatal per Python parity.
        if let Ok(mut guard) = tee.lock() {
            if let Err(e) = guard.write_all(tee_line.as_bytes()) {
                let key = e.to_string();
                if !tee_warned.contains(&key) {
                    tee_warned.insert(key.clone());
                    eprintln!("codex provider: tee write failed: {}", e);
                }
            } else {
                let _ = guard.flush();
            }
        }

        // Parse control-flow events.
        match parse_jsonl_line(&raw) {
            Some(JsonlEvent::ThreadStarted { thread_id }) => {
                // Record the event type for forensics regardless of whether
                // the id is usable (mirrors Python's `types_seen.add(ev_type)`
                // which runs before the thread_id check).
                types_seen.push(EV_SESSION.to_string());
                // cv-dcd823ce (CRITICAL): an EMPTY thread_id (`""`) passes the
                // `session_id.is_none()` guard as `Some("")`, then sails through
                // the `expect_session && session_id.is_none()` check below and
                // gets written to the registry as `codex_session_id: ""`. Every
                // subsequent `resume` then fails opaquely with "no
                // codex_session_id; cannot follow up". Treat an empty id as "no
                // session captured" so the create path fails closed with
                // NoSessionId (exit 11) instead. (Mirrored in codex.py.)
                if session_id.is_none() && !thread_id.is_empty() {
                    session_id = Some(thread_id);
                }
            }
            Some(JsonlEvent::AgentMessage { text }) => {
                types_seen.push(EV_ITEM.to_string());
                last_msg = text;
            }
            Some(JsonlEvent::SoftError { message }) => {
                types_seen.push(EV_ITEM.to_string());
                last_error_msg = message;
            }
            Some(JsonlEvent::TurnCompleted) => {
                types_seen.push(EV_COMPLETE.to_string());
                broke_on_complete = true;
                break;
            }
            Some(JsonlEvent::Other { type_name }) => {
                if let Some(t) = type_name {
                    if !types_seen.contains(&t) {
                        types_seen.push(t);
                    }
                }
            }
            None => {} // non-JSON banner line; skip
        }
    }

    // Cancel watchdog (drop its sender so the kill cascade is skipped). Must
    // happen BEFORE wait_with_grace so a slow reap doesn't run out the
    // watchdog's recv_timeout window. The watchdog joins below.
    watchdog.cancel();

    // Reap the child with grace: wait up to 5s, then SIGTERM, then SIGKILL.
    let (exit_code, sigkill_escalated) =
        crate::subprocess_ask::wait_with_grace(pid, &mut child, 5.0);

    // Now that the child is reaped and the watchdog has been signaled to
    // cancel, join it so any forensic state inside the thread (timed_out
    // store) is committed before we read it below.
    watchdog.join();

    // Close stderr drain thread; surface a panic (a bug, distinct from the
    // expected write-error case the thread already warns about) instead of
    // swallowing it silently.
    if stderr_handle.join().is_err() {
        eprintln!("codex provider: stderr drain thread panicked");
    }

    let duration_ms = started.elapsed().as_millis() as u64;
    let was_timed_out = watchdog.timed_out();

    // Operator Ctrl-C (ab-e7fdbcb6 / cv-cfdb7a56): the SIGINT-forwarding
    // handler (installed via `_sigint_guard`) already relayed the signal to
    // the codex process group and set this flag. Fail with `Interrupted` (exit
    // 130) BEFORE the timeout / no-session / exit-code checks so a Ctrl-C'd
    // run is reported as an interrupt, not misclassified as a timeout or a
    // missing session id. Mirrors codex.py re-raising KeyboardInterrupt.
    if crate::subprocess_ask::ask_interrupted() {
        return Err(CodexAskError::Interrupted);
    }

    // Check timeout first (Python parity: raise CodexTimeoutError).
    if was_timed_out {
        return Err(CodexAskError::Timeout {
            timeout_sec: timeout.map(|d| d.as_secs_f64()).unwrap_or(0.0),
        });
    }

    // Check for missing session_id on create path.
    if expect_session && session_id.is_none() {
        // LD14: surface observed types for forensics.
        types_seen.sort();
        types_seen.dedup();
        return Err(CodexAskError::NoSessionId { types_seen });
    }

    // SIGKILL escalation is always a failure (silent-failure-hunter row 4).
    if sigkill_escalated {
        return Err(CodexAskError::SigkillEscalated {
            partial_exit_code: exit_code,
        });
    }

    // cv-54a67325: the stdout drain hit a genuine read error and the stream
    // did NOT end on a `turn.completed` event. The reply (if any) is partial
    // and unreliable, so surface it as a hard failure rather than returning a
    // silently-truncated `Ok`. A read error AFTER `turn.completed` is benign
    // (we already broke out with the full reply), hence the `!broke_on_complete`
    // guard. Clean EOF without completion is left to the existing exit-code
    // path below to preserve Python's lenient "return what we captured"
    // behavior for non-error stream ends.
    if let Some(err) = stream_read_error {
        if !broke_on_complete {
            return Err(CodexAskError::Invocation {
                exit_code,
                message: format!(
                    "stream read error before turn.completed: {} (see output.jsonl)",
                    err
                ),
            });
        }
    }

    // Non-zero exit with no captured reply is a hard failure.
    if exit_code != 0 && last_msg.is_empty() {
        return Err(CodexAskError::Invocation {
            exit_code,
            message: format!("see output.jsonl for details"),
        });
    }

    // silent-failure-hunter row 5: promote soft-error text when no agent_message.
    let effective_last_msg = if !last_msg.is_empty() {
        last_msg
    } else {
        last_error_msg
    };

    Ok(CodexResult {
        exit_code,
        session_id,
        last_msg: effective_last_msg,
        duration_ms,
    })
}

// `kill_pgrp` and `wait_with_grace` now live in `subprocess_ask` (shared with
// gemini). See `crate::subprocess_ask::{kill_pgrp, wait_with_grace}`.

// ===========================================================================
// Public create / resume entry points
// ===========================================================================

/// Spawn `codex exec --json -C <cwd> ...` and parse the JSONL stream.
/// `agent_self` is the name of this agent (injected into spawn env for nested
/// `fno agents ask` attribution).
pub fn codex_create(
    cwd: &Path,
    prompt: &str,
    from_name: &str,
    yolo: bool,
    output_path: &Path,
    timeout: Option<Duration>,
    agent_self: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    add_dir: Option<&str>,
) -> Result<CodexResult, CodexAskError> {
    let full_prompt = inject_from_name(prompt, from_name);
    // ab-994222ee: the create/exec path is the autonomous headless lane. codex
    // exec is treated as possibly-blocking, so default to no-prompt
    // (--dangerously-bypass-approvals-and-sandbox); config.agents.codex.headless_yolo=false opts back in.
    let eff = crate::agents_config::effective_yolo(
        yolo,
        crate::agents_config::headless_yolo_enabled("codex", cwd),
    );
    let argv = build_argv_create(cwd, &full_prompt, eff, model, reasoning_effort, add_dir);
    run_codex(&argv, output_path, timeout, true, None, agent_self)
}

/// Spawn `codex exec resume <session_id> --json ...` from `cwd`.
/// Resume does NOT accept `--cd`; cwd is pinned via `Command::current_dir`.
pub fn codex_resume(
    session_id: &str,
    cwd: &Path,
    prompt: &str,
    from_name: &str,
    yolo: bool,
    output_path: &Path,
    timeout: Option<Duration>,
) -> Result<CodexResult, CodexAskError> {
    let full_prompt = inject_from_name(prompt, from_name);
    // ab-994222ee: a resumed autonomous worker is the same headless risk class.
    let eff = crate::agents_config::effective_yolo(
        yolo,
        crate::agents_config::headless_yolo_enabled("codex", cwd),
    );
    let argv = build_argv_resume(session_id, &full_prompt, eff);
    run_codex(&argv, output_path, timeout, false, Some(cwd), None)
}

// ===========================================================================
// Dispatch orchestrator (Wave B2)
// ===========================================================================

/// Derive the stable log path for a codex agent (mirrors Python `_codex_output_path`).
fn derive_log_path(home: &AgentsHome, name: &str) -> PathBuf {
    home.root()
        .join("agents")
        .join("logs")
        .join(format!("{}.jsonl", name))
}

/// UTC second-precision timestamp (mirrors `claude_ask::now_iso`).
fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// RAII per-agent flock (same primitive as `claude_ask::AgentLock`).
struct AgentLock {
    _file: std::fs::File,
}

impl AgentLock {
    fn acquire(home: &AgentsHome, name: &str, timeout: Duration) -> Result<Self, ()> {
        let locks_dir = home.root().join("locks");
        let _ = std::fs::create_dir_all(&locks_dir);
        let path = locks_dir.join(format!("{}.lock", name));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|_| ())?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(_) => {
                    if Instant::now() >= deadline {
                        return Err(());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
}

impl Drop for AgentLock {
    fn drop(&mut self) {
        // std's inherent File::unlock (stable since Rust 1.89; the crate now
        // pins rust-version = 1.89). Mirrors acquire()'s std locking.
        let _ = self._file.unlock();
    }
}

/// Outcome of `dispatch_codex_ask`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl AskOutcome {
    fn ok_reply(reply: String) -> Self {
        Self {
            stdout: reply,
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn err(msg: impl Into<String>, code: i32) -> Self {
        Self {
            stdout: String::new(),
            stderr: format!("{}\n", msg.into()),
            exit_code: code,
        }
    }
}

// Validation reuses `claude_ask::validate_inputs` directly — it is the
// canonical pre-flight gate for `ask` across all providers, mirroring
// Python's `dispatch.py::_validate_inputs` + `_validate_from_name`. The
// previous local copy here was missing three hardening checks the claude
// version enforces: short-id collision rejection (^[0-9a-f]{8}$), forbidden
// env chars in name (\0 \n \r =), and from_name length + XML-safety
// (sigma-review code-reviewer I1).

/// Orchestrate one codex `ask`: validate, lock, decide create-vs-resume,
/// stamp the registry, emit events, and return stdout/stderr/exit_code.
///
/// `extra_env` is unused for codex (codex inherits the process env directly);
/// the parameter exists for API symmetry with `dispatch_claude_ask`.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_codex_ask(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    // Create-only input, retained for API stability after Task 1.3a removed
    // the create branch from `ask` (`spawn --once` owns creation now).
    _cwd: &Path,
    yolo: bool,
    timeout: Option<Duration>,
) -> AskOutcome {
    if let Err(msg) = crate::claude_ask::validate_inputs(name, message, from_name) {
        return AskOutcome::err(msg, 2);
    }

    let events = home.events_jsonl();
    let registry_path = home.registry_json();

    let _lock = match AgentLock::acquire(home, name, LOCK_ACQUIRE_TIMEOUT) {
        Ok(l) => l,
        Err(()) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "lock-timeout".into()),
                    ("name", name.into()),
                    ("provider", "codex".into()),
                ],
            );
            return AskOutcome::err(
                format!(
                    "lock timeout for agent {:?} after {:.1}s",
                    name,
                    LOCK_ACQUIRE_TIMEOUT.as_secs_f64()
                ),
                11,
            );
        }
    };

    let registry = match load_registry(&registry_path) {
        Ok(r) => r,
        Err(e) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "registry-read".into()),
                    ("name", name.into()),
                    ("provider", "codex".into()),
                    ("error", e.to_string().into()),
                ],
            );
            return AskOutcome::err(format!("registry read failed: {}", e), 12);
        }
    };

    let existing = registry.find(name).cloned();

    match existing {
        None => {
            // ask never creates (Task 1.3a): unknown-name -> exit 16, byte-parity
            // with Python's dispatch_ask after Task 1.1.
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "unknown-name".into()),
                    ("name", name.into()),
                    ("provider", "codex".into()),
                ],
            );
            AskOutcome::err(
                format!(
                    "unknown agent {}; spawn it first: fno agents spawn {} -p <provider>",
                    crate::claude_ask::py_repr(name),
                    name
                ),
                16,
            )
        }
        Some(entry) => dispatch_resume(
            home,
            &events,
            &registry_path,
            name,
            &entry,
            message,
            from_name,
            yolo,
            timeout,
        ),
    }
}

/// Orchestrate one codex `spawn --once`: validate, lock, collision-check,
/// create + exchange, teardown registry row, return reply on stdout and
/// teardown receipt on stderr.  Reuses `dispatch_create` machinery.
///
/// Tests inject PATH via `std::env::set_var` before calling (the same mutex
/// pattern as `dispatch_codex_ask` tests), since `run_codex` inherits the
/// process environment directly.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_codex_once(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    yolo: bool,
    timeout: Option<Duration>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    add_dir: Option<&str>,
) -> AskOutcome {
    use crate::claude_ask::py_repr;

    // spawn allows an empty initial message (Python dispatch_spawn parity);
    // the once path defaults it to "hello" below.
    if let Err(msg) = crate::claude_ask::validate_spawn_inputs(name, from_name) {
        return AskOutcome::err(msg, 2);
    }

    let events = home.events_jsonl();
    let registry_path = home.registry_json();

    let _lock = match AgentLock::acquire(home, name, LOCK_ACQUIRE_TIMEOUT) {
        Ok(l) => l,
        Err(()) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "lock-timeout".into()),
                    ("name", name.into()),
                    ("provider", "codex".into()),
                ],
            );
            return AskOutcome::err(
                format!(
                    "lock timeout for agent {} after {:.1}s",
                    py_repr(name),
                    LOCK_ACQUIRE_TIMEOUT.as_secs_f64()
                ),
                11,
            );
        }
    };

    // Collision check INSIDE the lock (mirrors Python dispatch_spawn 4a).
    let registry = match load_registry(&registry_path) {
        Ok(r) => r,
        Err(e) => {
            return AskOutcome::err(format!("registry read failed: {}", e), 12);
        }
    };
    if registry.find(name).is_some() {
        // Python: f"agent {name!r} already exists; ..." -> py_repr, not {:?}.
        return AskOutcome::err(
            format!(
                "agent {} already exists; use 'fno agents rm {}' first or pick another name",
                py_repr(name),
                name
            ),
            2,
        );
    }

    // Create + exchange using the retained dispatch_create machinery.
    // Python parity: dispatch_spawn passes `message or "hello"` on the once
    // paths - Python truthiness, so ONLY the empty string becomes "hello";
    // a whitespace-only message is truthy and passes through unchanged
    // (sigma-review parity finding: trim() here would diverge).
    let effective_message = if message.is_empty() { "hello" } else { message };
    let inner = dispatch_create(
        home,
        &events,
        &registry_path,
        name,
        effective_message,
        from_name,
        cwd,
        yolo,
        timeout,
        model,
        reasoning_effort,
        add_dir,
    );
    if inner.exit_code != 0 {
        // create failed; dispatch_create only writes the registry post-success,
        // so no row was left behind (invariant pinned by test).
        return inner;
    }

    // Capture session id from the registry row that dispatch_create wrote.
    let session_or_short_id = load_registry(&registry_path)
        .ok()
        .and_then(|r| r.find(name).and_then(|e| e.codex_session_id.clone()))
        .unwrap_or_default();

    // Teardown: remove the registry row the create helper wrote.
    let teardown_err = update_registry(&registry_path, |reg| {
        reg.entries.retain(|e| e.name != name);
        true
    })
    .err();

    let teardown_receipt = if let Some(e) = teardown_err {
        // AC2-FR: loud warning, row stays visible, exit 0 still.
        // Python: f"... teardown failed for {name!r} ..." -> py_repr.
        format!(
            "fno agents spawn: warning: teardown failed for {} (codex/{}): {}. Peer leaked -- clean up via 'fno agents rm {}'\n",
            py_repr(name),
            session_or_short_id,
            e,
            name
        )
    } else {
        // Teardown receipt on stderr (AC2-UI), byte-parity with Python:
        // f"once: {name} ({provider}/{session_or_short_id}) torn down"
        format!("once: {} (codex/{}) torn down\n", name, session_or_short_id)
    };

    AskOutcome {
        stdout: inner.stdout,
        stderr: teardown_receipt,
        exit_code: 0,
    }
}

fn dispatch_create(
    home: &AgentsHome,
    events: &Path,
    registry_path: &Path,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    yolo: bool,
    timeout: Option<Duration>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    add_dir: Option<&str>,
) -> AskOutcome {
    let output_path = derive_log_path(home, name);
    let timeout_sec = timeout.unwrap_or(DEFAULT_FOLLOWUP_TIMEOUT);

    let result = match codex_create(
        cwd,
        message,
        from_name,
        yolo,
        &output_path,
        Some(timeout_sec),
        Some(name),
        model,
        reasoning_effort,
        add_dir,
    ) {
        Ok(r) => r,
        Err(e) => {
            let stage = match &e {
                CodexAskError::NoSessionId { .. } => "codex-no-session",
                CodexAskError::Timeout { .. } => "codex-timeout",
                CodexAskError::Interrupted => "codex-interrupted",
                _ => "codex-subprocess",
            };
            let exit_code = e.exit_code();
            // cv-9bc2abe7: append the output.jsonl path so the operator knows
            // where to find the partial reply / stderr a timeout-killed (or
            // otherwise failed) codex captured before dying. The resume path
            // already does this; the create path previously surfaced the bare
            // error with no log-file breadcrumb.
            let msg = format!("{} (see {} for details)", e, output_path.display());
            emit_event(
                events,
                "agent_ask_failed",
                &[
                    ("stage", stage.into()),
                    ("name", name.into()),
                    ("provider", "codex".into()),
                    ("returncode", exit_code.into()),
                ],
            );
            // exit_code is already mapped to the user-visible code by
            // CodexAskError::exit_code() (NotFound -> 14, SigkillEscalated ->
            // partial code, etc.). No further remap needed here.
            return AskOutcome::err(msg, exit_code);
        }
    };

    // `run_codex` with `expect_session=true` (the create path) guarantees
    // session_id is Some on Ok via the NoSessionId guard above. `expect`
    // converts a silent stamping of an empty `codex_session_id` (which
    // would then fail every subsequent resume with an opaque "cannot
    // follow up") into a loud panic with context (sigma-review type-design
    // HIGH).
    let session_id = result
        .session_id
        .expect("codex_create guarantees session_id on success (expect_session=true)");

    // Build the registry entry.
    use crate::state::RegistryEntry;
    let new_entry = RegistryEntry {
        name: name.to_string(),
        short_id: String::new(),
        legacy_provider: String::new(),
        harness: Some("codex".to_string()),
        harness_session_id: Some(session_id.clone()),
        cwd: cwd.to_string_lossy().to_string(),
        project_root: String::new(),
        session_id: None,
        legacy_claude_short_id: None,
        claude_session_uuid: None,
        messaging_socket_path: None,
        codex_session_id: Some(session_id.clone()),
        gemini_session_id: None,
        mcp_channel_id: None,
        host_mode: None, // codex ask = exec one-shot (not an interactive host)
        cc_session_id: None,
        // Stamped Live at creation: the just-finished one-shot is momentarily
        // live and immediately promotable/visible in `grid --all`, and the row
        // records a resumable session (codex resume <uuid>). It is NOT a
        // permanent Live: `reconcile` settles a finished ask to `Exited` by
        // process-liveness alone (plan ab-70faa65b, Locked Decision #1 -- a
        // surviving session file is "resumable", not "running", so it must not
        // keep the row `live`). promote is unaffected because admit_promote
        // admits a settled `Exited` exec source (see admit_promote_exited_source_
        // is_promotable); the only post-reconcile change is that a settled ask
        // drops out of `grid --all`'s alive-ish tiling (carveout cv-ba2b2048).
        // Supersedes the earlier fu-663c8b "intentionally permanent-Live"
        // rationale.
        status: AgentStatus::Live,
        last_message_at: None,
        created_at: now_iso(),
        pid: None,
        pid_start_time: None,
        log_path: Some(output_path.to_string_lossy().to_string()),
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
    };

    match update_registry(registry_path, |reg| {
        if reg.find(name).is_some() {
            false
        } else {
            reg.entries.push(new_entry.clone());
            true
        }
    }) {
        Ok(true) => {}
        Ok(false) => {
            emit_event(
                events,
                "agent_ask_failed",
                &[
                    ("stage", "name-collision".into()),
                    ("name", name.into()),
                    ("provider", "codex".into()),
                    ("codex_session_id", session_id.clone().into()),
                ],
            );
            return AskOutcome::err(
                format!(
                    "agent {:?} already exists (registered concurrently); orphaned codex session: {:?}",
                    name, session_id
                ),
                12,
            );
        }
        Err(e) => {
            emit_event(
                events,
                "agent_ask_failed",
                &[
                    ("stage", "registry-write".into()),
                    ("name", name.into()),
                    ("provider", "codex".into()),
                    ("codex_session_id", session_id.clone().into()),
                ],
            );
            return AskOutcome::err(
                format!(
                    "registry write failed: {}. orphaned codex session: {:?} (see output.jsonl)",
                    e, session_id
                ),
                12,
            );
        }
    }

    emit_event(
        events,
        "agent_ask_done",
        &[
            ("stage", "dispatch".into()),
            ("name", name.into()),
            ("provider", "codex".into()),
            ("codex_session_id", session_id.clone().into()),
            ("duration_ms", (result.duration_ms as u64).into()),
            ("yolo", yolo.into()),
        ],
    );

    // Codex create returns the reply verbatim (no short_id banner).
    // `kind="followup"` semantics: stdout = reply text, no trailing newline.
    AskOutcome::ok_reply(result.last_msg)
}

fn dispatch_resume(
    _home: &AgentsHome,
    events: &Path,
    registry_path: &Path,
    name: &str,
    entry: &crate::state::RegistryEntry,
    message: &str,
    from_name: &str,
    yolo: bool,
    timeout: Option<Duration>,
) -> AskOutcome {
    let session_id = match entry.codex_session_id.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return AskOutcome::err(
                format!(
                    "registry entry {:?} has no codex_session_id; cannot follow up. \
                     Remove with 'fno agents rm {}' and recreate.",
                    name, name
                ),
                11,
            );
        }
    };

    let log_path = match entry.log_path.as_deref() {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => {
            return AskOutcome::err(
                format!(
                    "registry entry {:?} has empty log_path; run 'fno agents rm {}' and recreate.",
                    name, name
                ),
                11,
            );
        }
    };

    let registered_cwd = match entry.cwd.as_str() {
        "" => {
            return AskOutcome::err(
                format!(
                    "registry entry {:?} has empty cwd; codex sessions are cwd-pinned. \
                     Run 'fno agents rm {}' and recreate.",
                    name, name
                ),
                11,
            );
        }
        c => PathBuf::from(c),
    };

    emit_event(
        events,
        "agent_followup_started",
        &[
            ("name", name.into()),
            ("provider", "codex".into()),
            ("codex_session_id", session_id.clone().into()),
            ("yolo", yolo.into()),
        ],
    );

    let timeout_sec = timeout.unwrap_or(DEFAULT_FOLLOWUP_TIMEOUT);
    let result = match codex_resume(
        &session_id,
        &registered_cwd,
        message,
        from_name,
        yolo,
        &log_path,
        Some(timeout_sec),
    ) {
        Ok(r) => r,
        Err(e) => {
            let stage = match &e {
                CodexAskError::Timeout { .. } => "codex-timeout",
                CodexAskError::Interrupted => "codex-interrupted",
                _ => "codex-subprocess",
            };
            let exit_code = e.exit_code();
            emit_event(
                events,
                "agent_followup_failed",
                &[
                    ("stage", stage.into()),
                    ("name", name.into()),
                    ("provider", "codex".into()),
                    ("codex_session_id", session_id.clone().into()),
                    ("returncode", exit_code.into()),
                ],
            );
            let msg = format!(
                "{} (see {} for details). If the session was lost, run 'fno agents rm {}' then re-ask.",
                e, log_path.display(), name
            );
            return AskOutcome::err(msg, exit_code);
        }
    };

    // Stamp status=live + last_message_at on success.
    if let Err(e) = update_registry(registry_path, |reg| {
        if let Some(en) = reg.find_mut(name) {
            en.status = AgentStatus::Live;
            en.last_message_at = Some(now_iso());
        }
    }) {
        emit_event(
            events,
            "agent_followup_failed",
            &[
                ("stage", "registry-write".into()),
                ("name", name.into()),
                ("provider", "codex".into()),
                ("codex_session_id", session_id.clone().into()),
                ("error", e.to_string().into()),
            ],
        );
        return AskOutcome::err(
            format!(
                "registry write failed: {}. NOTE: message was already delivered; do not retry. \
                 (agent={:?} session={:?})",
                e, name, session_id
            ),
            12,
        );
    }

    emit_event(
        events,
        "agent_followup_done",
        &[
            ("stage", "followup".into()),
            ("name", name.into()),
            ("provider", "codex".into()),
            ("codex_session_id", session_id.clone().into()),
            (
                "reply_chars",
                (result.last_msg.chars().count() as u64).into(),
            ),
            ("yolo", yolo.into()),
        ],
    );

    AskOutcome::ok_reply(result.last_msg)
}

// ===========================================================================
// Client entry point (called from bin/client.rs)
// ===========================================================================

/// Route a codex `ask` to the client-side `codex exec` path, bypassing the
/// daemon (mirrors `maybe_run_claude_ask` in client.rs).
///
/// Returns `Some(exit_code)` when the target is codex, or `None` to fall
/// through to the daemon RPC for gemini.
pub fn maybe_run_codex_ask(
    home: &AgentsHome,
    params: &serde_json::Value,
    name: &str,
) -> Option<i32> {
    let provider_param = params.get("provider").and_then(|v| v.as_str());
    // A corrupt registry must NOT silently degrade to "empty registry" for
    // the routing decision: that path could mis-route a codex agent
    // already registered with provider=codex to the Python dispatch (or
    // worse, route a registered claude agent to the codex branch if
    // --provider codex is supplied). Surface the failure via stderr WARN
    // and fall through to None (let Python handle it -- the same dispatch
    // there will surface a structured error). Sigma-review silent-failure
    // HIGH.
    //
    // Codex PR #371 follow-up: returning None here does NOT actually fall
    // through to Python -- once `fno` has exec'd the Rust client we're
    // already in-process, so None falls through to the daemon RPC path
    // (whose own registry read defaults to an empty registry, then spawns
    // a fresh PTY worker). That diverges from Python's contract, which
    // surfaces "registry read failed" as exit 12 before any side effect.
    // Surface the failure as exit 12 + stderr error, matching Python.
    let registry = match load_registry(&home.registry_json()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "fno-agents: cannot read agents registry at {:?}: {}",
                home.registry_json(),
                e
            );
            return Some(12);
        }
    };
    let existing_provider = registry.find(name).map(|e| e.harness_name().to_string());

    // Provider mismatch guard (mirrors claude path).
    if let (Some(ep), Some(pp)) = (existing_provider.as_deref(), provider_param) {
        if ep == "codex" && pp != "codex" {
            eprintln!(
                "fno-agents: agent {:?} already exists with provider 'codex'; \
                 refusing to override with --provider {}",
                name, pp
            );
            return Some(2);
        }
    }

    let resolved = existing_provider.as_deref().or(provider_param);
    if resolved != Some("codex") {
        return None; // not a codex target; fall through
    }

    let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let from_name = params
        .get("from_name")
        .and_then(|v| v.as_str())
        .unwrap_or("abilities");
    // cv-16eb2200: resolve_ask_cwd warns at the canonicalize-fallback point.
    let cwd = crate::subprocess_ask::resolve_ask_cwd(params.get("cwd").and_then(|v| v.as_str()));
    let timeout = params
        .get("timeout")
        .and_then(|v| v.as_u64())
        .map(std::time::Duration::from_secs);
    let yolo = params
        .get("yolo")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let outcome = dispatch_codex_ask(home, name, message, from_name, &cwd, yolo, timeout);
    if !outcome.stderr.is_empty() {
        eprint!("{}", outcome.stderr);
    }
    if !outcome.stdout.is_empty() {
        print!("{}", outcome.stdout);
    }
    Some(outcome.exit_code)
}
