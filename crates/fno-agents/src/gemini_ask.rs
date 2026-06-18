//! Client-side `gemini -p` ask path (ab-73da4ac2).
//!
//! `gemini` is a one-shot `gemini --skip-trust -p ... --output-format json`
//! subprocess (NOT a PTY agent): it emits a SINGLE JSON object to stdout at
//! EOF, the Rust client reads the whole blob and `json.loads`-parses it, and
//! returns the `response` field. The fno daemon cannot handle this path
//! because `handle_ask` renders a PTY screen; byte-parity with Python's
//! `providers/gemini.py` requires a direct subprocess approach.
//!
//! **Byte-parity is the contract.** The observable behavior (stdout reply,
//! exit code, events.jsonl fields) must match Python's implementation.
//!
//! # Cleavage from codex (`codex_ask.rs`)
//!
//! - codex emits a per-line JSONL stream; gemini emits ONE JSON object at EOF.
//!   So `parse_response` is a single `serde_json::from_str` over the full
//!   stdout, not a line iterator.
//! - codex merges stderr into stdout (LD12); gemini drains stderr on a SEPARATE
//!   thread (gemini writes structural warnings — Ripgrep/MCP/skill — to stderr
//!   that would corrupt the JSON parse). Both tee to `output.jsonl`.
//!
//! The subprocess primitives (SIGINT forwarding, process-group kill, grace
//! reap, watchdog, cwd resolution) live in `crate::subprocess_ask`, shared with
//! codex so the PR #371/#372 hardening carveouts apply once.
//!
//! # Locked Decisions (from Python gemini.py)
//!
//! - argv: `gemini --skip-trust -p <full_prompt> --output-format json <sandbox>`
//!   with `--session-id <uuid>` (create, when supplied) or `--resume <uuid>`
//!   (resume). cwd is set via `Command::current_dir`, not a `-C` flag.
//! - posture: bounded `--approval-mode yolo --sandbox` (default; never-prompt +
//!   sandboxed) with a `--approval-mode yolo` fallback when no sandbox provider,
//!   or bare `--yolo` (explicit full-auto). Never `default`/`auto_edit`.
//! - `inject_from_name` is plain `[from: X]\n\n<prompt>`, no escaping.
//! - `output.jsonl` is append-only tee of stdout blob + stderr (LD11).
//! - schema-drift guards: `session_id` (str|null), `response` (present +
//!   str|null), `stats` (present) — any drift is a parse error.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::claude_ask::{emit_event, validate_inputs};
use crate::paths::AgentsHome;
use crate::state::{load_registry, update_registry, RegistryEntry};
use crate::AgentStatus;

/// Lock-acquisition ceiling (mirror of codex/claude `LOCK_ACQUIRE_TIMEOUT`).
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
/// Default followup timeout (mirror of Python dispatch.py default).
const DEFAULT_FOLLOWUP_TIMEOUT: Duration = Duration::from_secs(600);
/// Cap on stderr tee'd during one invocation (mirror of gemini.py
/// `_DEFAULT_STDERR_CAP`); bounds output.jsonl on a runaway tool loop.
const DEFAULT_STDERR_CAP: usize = 256 * 1024;

// ===========================================================================
// Pinned JSON schema (gemini 0.42.0 smoke capture, mirror of gemini.py)
// ===========================================================================

/// Top-level key carrying the session UUID.
const KEY_SESSION: &str = "session_id";
/// Top-level key carrying the assistant reply.
const KEY_RESPONSE: &str = "response";
/// Top-level key that must be PRESENT (its absence is schema drift, Codex P2
/// PR #317: a missing `stats` would otherwise let a degraded payload land as a
/// successful empty reply).
const KEY_STATS: &str = "stats";

/// First 200 *characters* (not bytes) of `s`, mirroring Python's `s[:200]`.
fn raw_head(s: &str) -> String {
    s.chars().take(200).collect()
}

// ===========================================================================
// Pure-fn helpers (no I/O, fully unit-testable)
// ===========================================================================

/// Prepend `[from: <from_name>]\n\n` to `prompt`. Plain concatenation; no
/// escaping (mirror of `gemini.inject_from_name` / `codex_ask::inject_from_name`).
pub fn inject_from_name(prompt: &str, from_name: &str) -> String {
    format!("[from: {}]\n\n{}", from_name, prompt)
}

/// Is `name` an executable on `$PATH`? Minimal, self-contained `which`.
fn gemini_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|p| p.join(name).is_file()))
}

/// Best-effort: is a gemini `--sandbox` provider available? Never panics.
/// Ladder (mirror of `gemini.py::_gemini_sandbox_available`): macOS Seatbelt
/// (`sandbox-exec`) -> Docker/Podman when `GEMINI_SANDBOX` selects it AND the
/// daemon is reachable -> none. Any failure degrades to `false` (the caller
/// then uses the unsandboxed-but-never-prompt fallback).
fn gemini_sandbox_available() -> bool {
    if cfg!(target_os = "macos") && gemini_on_path("sandbox-exec") {
        return true;
    }
    if let Ok(sel) = std::env::var("GEMINI_SANDBOX") {
        let sel = sel.trim().to_ascii_lowercase();
        if (sel == "docker" || sel == "podman") && gemini_on_path(&sel) {
            return std::process::Command::new(&sel)
                .arg("info")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
        }
    }
    false
}

/// Argv tokens for gemini's create-path posture (bounded-posture amendment).
/// - bounded (default): `["--approval-mode", "yolo", "--sandbox"]` when a
///   sandbox provider exists; else fall back to `["--approval-mode", "yolo"]`
///   (never-prompt, unsandboxed) with a warning. NEVER `default`/`auto_edit`.
/// - full yolo (explicit): `["--yolo"]` (unsandboxed full-auto).
/// `sandbox_available` is injectable for tests; `None` runs the detection
/// ladder. Mirror of `gemini.py::sandbox_flag`. gemini uses the SAME selector
/// on create and resume.
pub fn sandbox_flag(yolo: bool, sandbox_available: Option<bool>) -> Vec<String> {
    if yolo {
        return vec!["--yolo".to_string()];
    }
    let avail = sandbox_available.unwrap_or_else(gemini_sandbox_available);
    if avail {
        vec![
            "--approval-mode".to_string(),
            "yolo".to_string(),
            "--sandbox".to_string(),
        ]
    } else {
        eprintln!(
            "warning: no gemini sandbox provider (sandbox-exec / docker); launching --approval-mode yolo UNSANDBOXED (still never-prompt, no hang)"
        );
        vec!["--approval-mode".to_string(), "yolo".to_string()]
    }
}

/// Build the create argv: `gemini --skip-trust -p <full_prompt> --output-format
/// json <sandbox> [--session-id <uuid>]`. `full_prompt` should already be built
/// via `inject_from_name`. The subprocess cwd is set via `Command::current_dir`
/// (gemini pins sessions to cwd), NOT an argv flag. `session_id` is appended
/// only when truthy (Python: `if session_id:`); a `None`/empty id lets gemini
/// auto-generate and we capture it from the response.
pub fn build_argv_create(full_prompt: &str, yolo: bool, session_id: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "gemini".to_string(),
        "--skip-trust".to_string(),
        "-p".to_string(),
        full_prompt.to_string(),
        "--output-format".to_string(),
        "json".to_string(),
    ];
    argv.extend(sandbox_flag(yolo, None));
    if let Some(sid) = session_id {
        if !sid.is_empty() {
            argv.push("--session-id".to_string());
            argv.push(sid.to_string());
        }
    }
    argv
}

/// Build the resume argv: `gemini --skip-trust -p <full_prompt> --output-format
/// json --resume <session_id> <sandbox>`. cwd is pinned via
/// `Command::current_dir` to the registry-recorded cwd (gemini sessions are
/// cwd-pinned; resume from elsewhere fails with "Invalid session identifier").
pub fn build_argv_resume(session_id: &str, full_prompt: &str, yolo: bool) -> Vec<String> {
    let mut argv = vec![
        "gemini".to_string(),
        "--skip-trust".to_string(),
        "-p".to_string(),
        full_prompt.to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--resume".to_string(),
        session_id.to_string(),
    ];
    argv.extend(sandbox_flag(yolo, None));
    argv
}

/// Parse the single-blob JSON output. Returns `(session_id, reply)`.
///
/// Byte-parity port of `gemini._parse_response`:
/// - empty / whitespace-only stdout -> `Parse` (the upstream parse failed).
/// - non-JSON / non-object -> `Parse`.
/// - `session_id`: present + (null|string); a non-null non-string is drift.
/// - `response`: MUST be present; null -> "" (model declined), string -> reply,
///   any other type is drift.
/// - `stats`: MUST be present (Codex P2 PR #317), else drift.
pub fn parse_response(stdout_text: &str) -> Result<(Option<String>, String), GeminiAskError> {
    let head = raw_head(stdout_text);
    if stdout_text.trim().is_empty() {
        return Err(GeminiAskError::Parse { raw_head: head });
    }
    let parsed: serde_json::Value = match serde_json::from_str(stdout_text) {
        Ok(v) => v,
        Err(_) => return Err(GeminiAskError::Parse { raw_head: head }),
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => return Err(GeminiAskError::Parse { raw_head: head }),
    };

    // session_id: missing or explicit null -> None; string -> Some; other ->
    // drift (a future gemini returning an int/bool here breaks registry writes).
    let session_id = match obj.get(KEY_SESSION) {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(GeminiAskError::Parse { raw_head: head }),
    };

    // response + stats must be PRESENT (missing either is schema drift).
    if !obj.contains_key(KEY_RESPONSE) {
        return Err(GeminiAskError::Parse { raw_head: head });
    }
    if !obj.contains_key(KEY_STATS) {
        return Err(GeminiAskError::Parse { raw_head: head });
    }

    let reply = match obj.get(KEY_RESPONSE) {
        Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        // present but non-string non-null -> drift (same guard as session_id).
        _ => return Err(GeminiAskError::Parse { raw_head: head }),
    };

    Ok((session_id, reply))
}

// ===========================================================================
// Error enum + exit-code map (mirror of dispatch.py's gemini failure->exit map)
// ===========================================================================

/// Errors from the gemini ask path. Each variant maps to a Python-compatible
/// exit code via [`GeminiAskError::exit_code`].
#[derive(Debug)]
pub enum GeminiAskError {
    /// gemini binary not found at spawn (ErrorKind::NotFound) — exit 14.
    /// Python's `dispatch_ask` checks `is_provider_available` first and exits
    /// 14 for "gemini not on PATH"; the Rust client has no upfront check, so
    /// the spawn-ENOENT maps to 14 directly (mirrors codex's `NotFound`).
    NotFound,
    /// Malformed JSON, non-object, schema drift, or missing session id on the
    /// create path — exit 11. Carries the first 200 chars for forensics + the
    /// `raw_head` event field (gemini.py `GeminiParseError`).
    Parse { raw_head: String },
    /// Cannot open the output.jsonl tee — exit 12 (gemini.py
    /// `GeminiInvocationError(12)`).
    TeeOpen { message: String },
    /// Wall-clock timeout — exit 15 (gemini.py `GeminiTimeoutError`).
    Timeout { timeout_sec: f64 },
    /// gemini exited non-zero (incl. a SIGKILL escalation) — exit = exit_code,
    /// or 1 when exit_code is 0. Mirrors gemini.py folding sigkill escalation
    /// into `GeminiInvocationError(exit_code if exit_code != 0 else 1)`.
    Invocation { exit_code: i32 },
    /// Non-ENOENT OSError at spawn — exit 1 (gemini.py
    /// `GeminiInvocationError(1)` after the stderr WARN).
    OsError { message: String },
    /// Operator SIGINT (Ctrl-C) forwarded to the gemini group — exit 130.
    /// Mirrors Python re-raising KeyboardInterrupt -> CPython exit 130
    /// (cv-cfdb7a56), symmetric with codex's `Interrupted`.
    Interrupted,
}

impl GeminiAskError {
    /// Python-compatible exit code for this error (see `dispatch.py`'s
    /// `_gemini_create_path` / `_gemini_followup_path` failure->exit map).
    pub fn exit_code(&self) -> i32 {
        match self {
            GeminiAskError::NotFound => 14,
            GeminiAskError::Parse { .. } => 11,
            GeminiAskError::TeeOpen { .. } => 12,
            GeminiAskError::Timeout { .. } => 15,
            GeminiAskError::Invocation { exit_code } => {
                if *exit_code != 0 {
                    *exit_code
                } else {
                    1
                }
            }
            GeminiAskError::OsError { .. } => 1,
            GeminiAskError::Interrupted => 130,
        }
    }
}

impl std::fmt::Display for GeminiAskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeminiAskError::NotFound => write!(f, "gemini binary not found on PATH"),
            GeminiAskError::Parse { raw_head } => write!(
                f,
                "gemini output did not parse as JSON; first {} chars: {:?}",
                raw_head.chars().count(),
                raw_head
            ),
            GeminiAskError::TeeOpen { message } => {
                write!(f, "gemini provider: cannot open output tee: {}", message)
            }
            GeminiAskError::Timeout { timeout_sec } => {
                write!(f, "gemini timed out after {}s", timeout_sec)
            }
            GeminiAskError::Invocation { exit_code } => {
                write!(f, "gemini exited {}", exit_code)
            }
            GeminiAskError::OsError { message } => {
                write!(f, "gemini provider: OSError invoking gemini: {}", message)
            }
            GeminiAskError::Interrupted => {
                write!(f, "gemini interrupted by SIGINT (Ctrl-C)")
            }
        }
    }
}

impl std::error::Error for GeminiAskError {}

/// Result of a successful gemini create or resume invocation.
#[derive(Debug, Clone)]
pub struct GeminiResult {
    /// Exit code (0 on happy path).
    pub exit_code: i32,
    /// Session UUID parsed from the JSON blob's `session_id` field.
    pub session_id: Option<String>,
    /// Assistant text from `response` ("" when the model emitted no text).
    pub last_msg: String,
    /// Wall-clock elapsed ms.
    pub duration_ms: u64,
}

// ===========================================================================
// Subprocess driver (Wave G3)
// ===========================================================================

/// Open the JSONL tee, tagging an error as gemini's `TeeOpen`.
fn open_tee(log_path: &Path) -> Result<std::fs::File, GeminiAskError> {
    crate::subprocess_ask::open_tee(log_path).map_err(|e| GeminiAskError::TeeOpen {
        message: e.to_string(),
    })
}

/// Shared subprocess driver for create and resume.
///
/// Cleavage from `codex_ask::run_codex`:
/// - stdout is read as a SINGLE blob (`read_to_end`), not a JSONL line loop.
/// - stderr is drained on a SEPARATE thread (NOT merged into stdout) so the
///   single-blob JSON parse stays pure; both stdout and stderr tee to
///   `output_path` under a shared lock.
///
/// Shared with codex via `subprocess_ask`: SIGINT forwarding, the cancelable
/// watchdog, `wait_with_grace`, process-group kill.
fn run_gemini(
    argv: &[String],
    output_path: &Path,
    timeout: Option<Duration>,
    expect_session: bool,
    popen_cwd: Option<&Path>,
    agent_self: Option<&str>,
) -> Result<GeminiResult, GeminiAskError> {
    use std::process::{Command, Stdio};

    let started = Instant::now();

    // Open the tee BEFORE spawn so path/permission errors surface as TeeOpen
    // (exit 12) rather than a panic.
    let tee_fh = open_tee(output_path)?;

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped()); // SEPARATE pipe (divergence from codex LD12)
    if let Some(cwd) = popen_cwd {
        cmd.current_dir(cwd);
    }
    if let Some(name) = agent_self {
        cmd.env("FNO_AGENT_SELF", name);
        cmd.env("FNO_AGENT_PROVIDER", "gemini");
    }
    // Own process group so SIGTERM/SIGKILL/SIGINT reach gemini's subshells.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(GeminiAskError::NotFound);
        }
        Err(e) => {
            // gemini.py warns then raises GeminiInvocationError(1).
            eprintln!("gemini provider: OSError invoking gemini: {}", e);
            return Err(GeminiAskError::OsError {
                message: e.to_string(),
            });
        }
    };

    let pid = child.id();
    // Forward operator Ctrl-C to the gemini process group for this call's
    // lifetime (cv-cfdb7a56, shared driver). gemini is setpgid(0,0).
    // NOTE: `_sigint_guard` is held for the whole function (RAII) -- it is NOT a
    // discard. Do NOT rewrite to `let _ = ...`: a bare `_` drops the guard
    // immediately, uninstalling the SIGINT handler before gemini even runs.
    let _sigint_guard = crate::subprocess_ask::SigintForwarder::install(pid);

    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");

    // The tee handle is shared between the stdout (main thread) and stderr
    // (side thread) writers.
    let tee = Arc::new(Mutex::new(tee_fh));
    let tee_stderr = tee.clone();

    // Side thread: drain stderr line-by-line and tee it (kept OUT of stdout so
    // the JSON blob parse is pure). cv-6cbaa462: warn once per distinct write
    // error rather than dropping the failure silently. Tee'ing is capped at
    // 256KB, but we KEEP DRAINING (read + discard) past the cap rather than
    // breaking: ending the thread would drop `stderr_pipe` and close the read
    // fd, so a still-running gemini writing more stderr would take a SIGPIPE
    // (abrupt non-zero exit) or block on a full pipe (Gemini code-assist HIGH on
    // PR #379). Python leaves the fd open after its cap-break; draining fully is
    // the robust superset and never deadlocks the child.
    let stderr_handle = std::thread::spawn(move || {
        let mut warned: HashSet<String> = HashSet::new();
        let mut total: usize = 0;
        let mut cap_hit = false;
        let mut reader = BufReader::new(stderr_pipe);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) if cap_hit => {
                    // Past the cap: keep the pipe drained, discard the bytes.
                    continue;
                }
                Ok(n) => {
                    total += n;
                    if let Ok(mut guard) = tee_stderr.lock() {
                        if let Err(e) = guard.write_all(line.as_bytes()) {
                            let key = e.to_string();
                            if warned.insert(key) {
                                eprintln!("gemini provider: stderr tee write failed: {}", e);
                            }
                        } else {
                            let _ = guard.flush();
                        }
                    }
                    if total > DEFAULT_STDERR_CAP {
                        // Parity with gemini.py `_drain_pipe_into_list`: leave a
                        // truncation breadcrumb in the tee so `fno agents logs`
                        // shows the cap was hit, not a clean stderr EOF. Then
                        // keep draining (see cap_hit) instead of breaking.
                        let marker = format!("\n[truncated at {} bytes]\n", DEFAULT_STDERR_CAP);
                        if let Ok(mut guard) = tee_stderr.lock() {
                            // Same warn-once contract as the line write above
                            // (cv-6cbaa462): the marker exists FOR observability,
                            // so a failed marker write must not be silently lost.
                            if let Err(e) = guard.write_all(marker.as_bytes()) {
                                let key = e.to_string();
                                if warned.insert(key) {
                                    eprintln!("gemini provider: stderr tee write failed: {}", e);
                                }
                            } else {
                                let _ = guard.flush();
                            }
                        }
                        cap_hit = true;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut watchdog = crate::subprocess_ask::AskWatchdog::spawn(pid, timeout);

    // Main thread: read the WHOLE stdout blob (gemini emits one JSON object at
    // EOF). A forwarded SIGINT tears gemini down, closing stdout, so this
    // returns with whatever was captured; the post-read interrupt check turns
    // that into `Interrupted`. Read bytes + lossy-decode so a stray non-UTF-8
    // byte can't hard-fail the read (gemini JSON is UTF-8 in practice).
    let mut stdout_bytes: Vec<u8> = Vec::new();
    {
        let mut reader = stdout_pipe;
        if let Err(e) = reader.read_to_end(&mut stdout_bytes) {
            // A mid-read error (EIO/EPIPE) truncates the JSON blob. The partial
            // bytes still flow to parse_response (which rejects a truncated
            // single object as Parse / exit 11), but stay observable rather than
            // silent: Python's `proc.stdout.read()` would raise here, and the
            // codex sibling surfaces the same class of stream-read error
            // (cv-54a67325). Mirror that WARN-on-best-effort-failure contract.
            eprintln!("gemini provider: stdout stream read error: {}", e);
        }
    }
    let stdout_text = String::from_utf8_lossy(&stdout_bytes).into_owned();

    // Tee the stdout blob under the shared lock (after EOF) so the stderr
    // thread's line writes never interleave with the single-blob write. A
    // trailing newline is added if missing (mirror of gemini.py).
    if !stdout_text.is_empty() {
        if let Ok(mut guard) = tee.lock() {
            if let Err(e) = guard.write_all(stdout_text.as_bytes()) {
                eprintln!("gemini provider: tee write of stdout failed: {}", e);
            } else {
                if !stdout_text.ends_with('\n') {
                    let _ = guard.write_all(b"\n");
                }
                let _ = guard.flush();
            }
        }
    }

    // Cancel the watchdog BEFORE reaping, then reap with grace, then join.
    watchdog.cancel();
    let (exit_code, sigkill_escalated) =
        crate::subprocess_ask::wait_with_grace(pid, &mut child, 5.0);
    watchdog.join();
    // cv-a8dd2647: surface a drain-thread panic instead of silently dropping it.
    if stderr_handle.join().is_err() {
        eprintln!("gemini provider: stderr drain thread panicked");
    }

    let duration_ms = started.elapsed().as_millis() as u64;
    let was_timed_out = watchdog.timed_out();

    // Operator Ctrl-C wins over every other classification (symmetric with
    // codex; mirrors gemini.py re-raising KeyboardInterrupt before the
    // timeout / parse / exit-code checks).
    if crate::subprocess_ask::ask_interrupted() {
        return Err(GeminiAskError::Interrupted);
    }

    if was_timed_out {
        return Err(GeminiAskError::Timeout {
            timeout_sec: timeout.map(|d| d.as_secs_f64()).unwrap_or(0.0),
        });
    }

    // sigkill escalation folds into Invocation(exit_code or 1) (gemini.py).
    if sigkill_escalated {
        return Err(GeminiAskError::Invocation { exit_code });
    }

    // Parse-first ordering (gemini.py): parse stdout, then on a parse failure
    // decide between invocation error (non-zero exit) and parse error
    // (exit-zero but malformed -> schema drift).
    let (session_id, reply) = match parse_response(&stdout_text) {
        Ok(t) => t,
        Err(parse_err) => {
            if exit_code != 0 {
                return Err(GeminiAskError::Invocation { exit_code });
            }
            return Err(parse_err);
        }
    };

    // Parseable JSON but non-zero exit: the invocation failed; propagate it.
    if exit_code != 0 {
        return Err(GeminiAskError::Invocation { exit_code });
    }

    // expect_session (create) with no/empty session id after a clean exit is a
    // hard contract violation -> parse error (same forensic surface).
    if expect_session && session_id.as_deref().map_or(true, |s| s.is_empty()) {
        return Err(GeminiAskError::Parse {
            raw_head: raw_head(&stdout_text),
        });
    }

    Ok(GeminiResult {
        exit_code,
        session_id,
        last_msg: reply,
        duration_ms,
    })
}

// ===========================================================================
// Public create / resume entry points
// ===========================================================================

/// Spawn `gemini --skip-trust -p ... --output-format json` in `cwd` and parse
/// the single JSON blob. `agent_self` is injected into the spawn env for nested
/// `fno agents ask` attribution. Dispatch passes `session_id=None` so gemini
/// auto-generates and we capture it from the response.
#[allow(clippy::too_many_arguments)]
pub fn gemini_create(
    cwd: &Path,
    prompt: &str,
    from_name: &str,
    yolo: bool,
    output_path: &Path,
    timeout: Option<Duration>,
    agent_self: Option<&str>,
) -> Result<GeminiResult, GeminiAskError> {
    let full_prompt = inject_from_name(prompt, from_name);
    // ab-994222ee: the create/exec path is the autonomous headless lane. Default
    // to no-prompt (--yolo) so a headless gemini cannot wedge on the first
    // approval prompt; config.agents.gemini.headless_yolo=false opts back in.
    let eff = crate::agents_config::effective_yolo(
        yolo,
        crate::agents_config::headless_yolo_enabled("gemini", cwd),
    );
    let argv = build_argv_create(&full_prompt, eff, None);
    // gemini pins sessions to cwd via Popen(cwd=...), so create passes popen_cwd.
    run_gemini(&argv, output_path, timeout, true, Some(cwd), agent_self)
}

/// Spawn `gemini --skip-trust -p ... --resume <uuid> ...` from the
/// registry-recorded `cwd` (gemini sessions are cwd-pinned).
pub fn gemini_resume(
    session_id: &str,
    cwd: &Path,
    prompt: &str,
    from_name: &str,
    yolo: bool,
    output_path: &Path,
    timeout: Option<Duration>,
) -> Result<GeminiResult, GeminiAskError> {
    let full_prompt = inject_from_name(prompt, from_name);
    // ab-994222ee: a resumed autonomous worker is the same headless risk class.
    let eff = crate::agents_config::effective_yolo(
        yolo,
        crate::agents_config::headless_yolo_enabled("gemini", cwd),
    );
    let argv = build_argv_resume(session_id, &full_prompt, eff);
    run_gemini(&argv, output_path, timeout, false, Some(cwd), None)
}

// ===========================================================================
// Dispatch orchestrator (Wave G3)
// ===========================================================================

/// Derive the stable log path for a gemini agent (mirror of codex's
/// `derive_log_path`; the path is stored in the registry row and read back by
/// resume / `fno agents logs`).
fn derive_log_path(home: &AgentsHome, name: &str) -> PathBuf {
    home.root()
        .join("agents")
        .join("logs")
        .join(format!("{}.jsonl", name))
}

/// UTC second-precision timestamp (mirror of `claude_ask::now_iso`).
fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// RAII per-agent flock (same primitive as `codex_ask::AgentLock`).
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
        let _ = self._file.unlock();
    }
}

/// Outcome of `dispatch_gemini_ask` (mirror of codex's `AskOutcome`).
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

/// Orchestrate one gemini `ask`: validate, lock, decide create-vs-resume,
/// stamp the registry, emit events, and return stdout/stderr/exit_code.
/// Byte-parity with `dispatch.py`'s `_gemini_create_path` / `_gemini_followup_path`.
pub fn dispatch_gemini_ask(
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
    if let Err(msg) = validate_inputs(name, message, from_name) {
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
                    ("provider", "gemini".into()),
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
                    ("provider", "gemini".into()),
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
                    ("provider", "gemini".into()),
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

/// Orchestrate one gemini `spawn --once`: validate, lock, collision-check,
/// create + exchange, teardown registry row, return reply on stdout and
/// teardown receipt on stderr.  Reuses `dispatch_create` machinery.
///
/// Tests inject PATH via `std::env::set_var` before calling (the same mutex
/// pattern as `dispatch_gemini_ask` tests), since `run_gemini` inherits the
/// process environment directly.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_gemini_once(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    yolo: bool,
    timeout: Option<Duration>,
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
                    ("provider", "gemini".into()),
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
    );
    if inner.exit_code != 0 {
        return inner;
    }

    // Capture session id from the registry row that dispatch_create wrote.
    let session_or_short_id = load_registry(&registry_path)
        .ok()
        .and_then(|r| r.find(name).and_then(|e| e.gemini_session_id.clone()))
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
            "fno agents spawn: warning: teardown failed for {} (gemini/{}): {}. Peer leaked -- clean up via 'fno agents rm {}'\n",
            py_repr(name),
            session_or_short_id,
            e,
            name
        )
    } else {
        // Teardown receipt on stderr (AC2-UI), byte-parity with Python:
        // f"once: {name} ({provider}/{session_or_short_id}) torn down"
        format!(
            "once: {} (gemini/{}) torn down\n",
            name, session_or_short_id
        )
    };

    AskOutcome {
        stdout: inner.stdout,
        stderr: teardown_receipt,
        exit_code: 0,
    }
}

#[allow(clippy::too_many_arguments)]
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
) -> AskOutcome {
    let output_path = derive_log_path(home, name);
    let timeout_sec = timeout.unwrap_or(DEFAULT_FOLLOWUP_TIMEOUT);

    let result = match gemini_create(
        cwd,
        message,
        from_name,
        yolo,
        &output_path,
        Some(timeout_sec),
        Some(name),
    ) {
        Ok(r) => r,
        Err(e) => {
            let stage = match &e {
                GeminiAskError::Timeout { .. } => "gemini-timeout",
                GeminiAskError::Parse { .. } => "gemini-parse",
                GeminiAskError::Interrupted => "gemini-interrupted",
                _ => "gemini-subprocess",
            };
            let exit_code = e.exit_code();
            let msg = format!("{} (see {} for details)", e, output_path.display());
            emit_event(
                events,
                "agent_ask_failed",
                &[
                    ("stage", stage.into()),
                    ("name", name.into()),
                    ("provider", "gemini".into()),
                    ("returncode", exit_code.into()),
                ],
            );
            return AskOutcome::err(msg, exit_code);
        }
    };

    // run_gemini with expect_session=true guarantees a non-empty session_id on
    // Ok (else it returns Parse). `expect` converts a contract violation into a
    // loud panic with context rather than a silent empty registry stamp.
    let session_id = result
        .session_id
        .filter(|s| !s.is_empty())
        .expect("gemini_create guarantees a non-empty session_id on success (expect_session=true)");

    let new_entry = RegistryEntry {
        name: name.to_string(),
        short_id: String::new(),
        provider: "gemini".to_string(),
        cwd: cwd.to_string_lossy().to_string(),
        project_root: String::new(),
        session_id: None,
        claude_short_id: None,
        claude_session_uuid: None,
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: Some(session_id.clone()),
        mcp_channel_id: None,
        host_mode: None, // gemini ask = one-shot (not an interactive host)
        cc_session_id: None,
        // Live (resumable via `gemini --resume <uuid>`); mirrors codex.
        status: AgentStatus::Live,
        last_message_at: None,
        created_at: now_iso(),
        pid: None,
        pid_start_time: None,
        log_path: Some(output_path.to_string_lossy().to_string()),
        last_reconciled_at: None,
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
                    ("provider", "gemini".into()),
                    ("gemini_session_id", session_id.clone().into()),
                ],
            );
            return AskOutcome::err(
                format!(
                    "agent {:?} already exists (registered concurrently); orphaned gemini session: {:?}",
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
                    ("provider", "gemini".into()),
                    ("gemini_session_id", session_id.clone().into()),
                ],
            );
            return AskOutcome::err(
                format!(
                    "registry write failed: {}. orphaned gemini session: gemini sessions \
                     persist on disk; clean up via 'gemini --delete-session <index>' if \
                     desired (--list-sessions to find the index)",
                    e
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
            ("provider", "gemini".into()),
            ("gemini_session_id", session_id.clone().into()),
            ("duration_ms", (result.duration_ms).into()),
            ("yolo", yolo.into()),
        ],
    );

    // gemini's create path RETURNS the reply on stdout (kind="followup"
    // semantics: reply verbatim, no banner, no trailing newline added).
    AskOutcome::ok_reply(result.last_msg)
}

#[allow(clippy::too_many_arguments)]
fn dispatch_resume(
    events: &Path,
    registry_path: &Path,
    name: &str,
    entry: &RegistryEntry,
    message: &str,
    from_name: &str,
    yolo: bool,
    timeout: Option<Duration>,
) -> AskOutcome {
    let session_id = match entry.gemini_session_id.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return AskOutcome::err(
                format!(
                    "registry entry {:?} has no gemini_session_id; cannot follow up. \
                     Remove with 'fno agents rm {}' and recreate.",
                    name, name
                ),
                11,
            );
        }
    };

    // agent_followup_started is emitted after the session-id guard, before the
    // log_path / cwd guards (mirror of gemini.py ordering).
    emit_event(
        events,
        "agent_followup_started",
        &[
            ("name", name.into()),
            ("provider", "gemini".into()),
            ("gemini_session_id", session_id.clone().into()),
            ("yolo", yolo.into()),
        ],
    );

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
                    "registry entry {:?} has empty cwd; gemini sessions are cwd-pinned and \
                     resume cannot proceed. Run 'fno agents rm {}' and recreate.",
                    name, name
                ),
                11,
            );
        }
        c => PathBuf::from(c),
    };

    let timeout_sec = timeout.unwrap_or(DEFAULT_FOLLOWUP_TIMEOUT);
    let result = match gemini_resume(
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
                GeminiAskError::Timeout { .. } => "gemini-timeout",
                GeminiAskError::Parse { .. } => "gemini-parse",
                GeminiAskError::Interrupted => "gemini-interrupted",
                _ => "gemini-subprocess",
            };
            let exit_code = e.exit_code();
            // Failure events on the followup path omit `provider` (parity with
            // gemini.py's `_gemini_followup_path` emits).
            emit_event(
                events,
                "agent_followup_failed",
                &[
                    ("stage", stage.into()),
                    ("name", name.into()),
                    ("gemini_session_id", session_id.clone().into()),
                    ("returncode", exit_code.into()),
                ],
            );
            let msg = format!(
                "{} (see {} for details). If the session was deleted (e.g. \
                 'gemini --delete-session'), run 'fno agents rm {}' then re-ask.",
                e,
                log_path.display(),
                name
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
                ("gemini_session_id", session_id.clone().into()),
                ("error", e.to_string().into()),
                ("error_type", "RegistryWriteError".into()),
            ],
        );
        return AskOutcome::err(
            format!(
                "registry write failed: {}. NOTE: message was already delivered; do not retry.",
                e
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
            ("provider", "gemini".into()),
            ("gemini_session_id", session_id.clone().into()),
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

/// Route a gemini `ask` to the client-side `gemini -p` path, bypassing the
/// daemon (mirror of `maybe_run_codex_ask`).
///
/// Returns `Some(exit_code)` when the target is gemini, or `None` to let the
/// caller fall through (to the next provider hook, then the unresolvable
/// exit-2 surface).
pub fn maybe_run_gemini_ask(
    home: &AgentsHome,
    params: &serde_json::Value,
    name: &str,
) -> Option<i32> {
    let provider_param = params.get("provider").and_then(|v| v.as_str());
    // A corrupt registry must surface exit 12 (Python parity), not silently
    // degrade to an empty registry and mis-route.
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
    let existing_provider = registry.find(name).map(|e| e.provider.clone());

    // Provider mismatch guard (mirrors codex/claude).
    if let (Some(ep), Some(pp)) = (existing_provider.as_deref(), provider_param) {
        if ep == "gemini" && pp != "gemini" {
            eprintln!(
                "fno-agents: agent {:?} already exists with provider 'gemini'; \
                 refusing to override with --provider {}",
                name, pp
            );
            return Some(2);
        }
    }

    let resolved = existing_provider.as_deref().or(provider_param);
    if resolved != Some("gemini") {
        return None; // not a gemini target; fall through
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
        .map(Duration::from_secs);
    let yolo = params
        .get("yolo")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let outcome = dispatch_gemini_ask(home, name, message, from_name, &cwd, yolo, timeout);
    if !outcome.stderr.is_empty() {
        eprint!("{}", outcome.stderr);
    }
    if !outcome.stdout.is_empty() {
        print!("{}", outcome.stdout);
    }
    Some(outcome.exit_code)
}

#[cfg(test)]
mod sandbox_posture_tests {
    use super::sandbox_flag;

    #[test]
    fn full_yolo_is_bare_yolo() {
        // explicit yolo -> bare --yolo (unsandboxed full-auto)
        assert_eq!(sandbox_flag(true, Some(true)), vec!["--yolo".to_string()]);
        assert_eq!(sandbox_flag(true, Some(false)), vec!["--yolo".to_string()]);
    }

    #[test]
    fn bounded_with_provider_is_yolo_plus_sandbox() {
        assert_eq!(
            sandbox_flag(false, Some(true)),
            vec![
                "--approval-mode".to_string(),
                "yolo".to_string(),
                "--sandbox".to_string()
            ]
        );
    }

    #[test]
    fn bounded_without_provider_falls_back_never_prompting() {
        let fb = sandbox_flag(false, Some(false));
        assert_eq!(fb, vec!["--approval-mode".to_string(), "yolo".to_string()]);
        // the fallback NEVER emits a prompting mode
        assert!(!fb.iter().any(|t| t == "default" || t == "auto_edit"));
        assert!(!fb.contains(&"--sandbox".to_string()));
    }
}
