//! Client-side `agy -p` ask path (Phase C, agy harness).
//!
//! `agy` (Google's Antigravity CLI) is a one-shot `agy -p <prompt>` subprocess
//! (like gemini/codex, NOT a daemon PTY pane on this path). The cleavage from
//! gemini is the OUTPUT SHAPE: agy v1.0.x has **no `--output-format json`**, so
//! stdout is the model's reply as PLAIN TEXT. There is no session id in the
//! output, so this path is STATELESS — `dispatch_agy_once` runs one prompt and
//! returns the reply; there is no registry row and no `ask`-by-id resume (agy's
//! own `--continue`/`--conversation` resume is a daemon/interactive concern, not
//! this headless one-shot).
//!
//! Ported gotchas from the battle-tested MIT wrapper
//! `antigravity-for-claude-code/scripts/agy-delegate.sh`:
//! - **stdin is `/dev/null`**: `agy -p` silently drops stdout when stdin is a
//!   non-TTY waiting for input; detaching stdin avoids the empty-output hang.
//! - **outer wall-clock guard**: without a console agy v1.0.x can hard-hang
//!   *before* its own `--print-timeout` engages, so the shared
//!   [`crate::subprocess_ask::AskWatchdog`] bounds the call (the Rust analogue
//!   of the wrapper's outer `timeout`/`gtimeout`).
//! - **structured failure classification**: agy emits no machine-readable error;
//!   the wrapper scans STDERR for quota/auth/timeout and maps to distinct exit
//!   codes. [`AgyAskError`] mirrors that map (2 failed / 3 empty / 10 quota /
//!   11 auth / 12 timeout / 13 missing / 130 interrupted).
//!
//! The subprocess primitives (SIGINT forwarding, process-group kill, grace
//! reap, watchdog, cwd resolution) are reused from `crate::subprocess_ask`,
//! shared with codex/gemini.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::claude_ask::emit_event;
use crate::paths::AgentsHome;

/// Default one-shot timeout (matches the wrapper's outer-guard intent; the
/// caller can override via the `timeout` param).
const DEFAULT_ASK_TIMEOUT: Duration = Duration::from_secs(600);
/// Inner `--print-timeout` handed to agy (the OUTER watchdog is larger).
const AGY_PRINT_TIMEOUT: &str = "5m";
/// Cap on stderr captured for classification (bounds memory on a runaway loop).
const STDERR_CAP: usize = 256 * 1024;

/// First 200 *characters* of `s` (forensic head for parse failures).
fn raw_head(s: &str) -> String {
    s.chars().take(200).collect()
}

// ===========================================================================
// Pure-fn helpers (no I/O, fully unit-testable)
// ===========================================================================

/// Prepend `[from: <from_name>]\n\n` to `prompt` (mirror of the sibling asks).
pub fn inject_from_name(prompt: &str, from_name: &str) -> String {
    format!("[from: {}]\n\n{}", from_name, prompt)
}

/// Yolo posture flag. agy's full-auto / never-prompt is
/// `--dangerously-skip-permissions`; the headless one-shot ALWAYS passes it (an
/// autonomous worker must not wedge on agy's first approval prompt). Returned as
/// a vec so the argv builder can splice it cleanly.
fn agy_yolo_flags() -> Vec<String> {
    vec!["--dangerously-skip-permissions".to_string()]
}

/// Build the one-shot argv: `agy --print-timeout <dur> --add-dir <cwd>
/// --dangerously-skip-permissions [--model <m>] -p <full_prompt>`.
///
/// NOTE: in agy, `-p`/`--print` takes the prompt as its VALUE, so it must come
/// LAST with the prompt attached (the wrapper's load-bearing ordering rule).
/// `full_prompt` should already be built via [`inject_from_name`]. cwd is passed
/// BOTH as `--add-dir` (agy's workspace) and via `Command::current_dir`.
///
/// A user `--add-dir` (x-b6e2) is ADDITIVE: it appends a second `--add-dir
/// <dir>` after the internal cwd injection, never replacing it (Locked Decision
/// 5). Empty/None leaves the argv byte-for-byte as before.
pub fn build_argv_once(
    full_prompt: &str,
    cwd: &Path,
    model: Option<&str>,
    add_dir: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        "agy".to_string(),
        "--print-timeout".to_string(),
        AGY_PRINT_TIMEOUT.to_string(),
        "--add-dir".to_string(),
        cwd.to_string_lossy().into_owned(),
    ];
    if let Some(d) = add_dir.filter(|d| !d.is_empty()) {
        argv.push("--add-dir".to_string());
        argv.push(d.to_string());
    }
    argv.extend(agy_yolo_flags());
    if let Some(m) = model {
        if !m.is_empty() {
            argv.push("--model".to_string());
            argv.push(m.to_string());
        }
    }
    // -p LAST, prompt as its value.
    argv.push("-p".to_string());
    argv.push(full_prompt.to_string());
    argv
}

/// Parse agy's PLAIN-TEXT stdout into the reply. agy has no JSON; the whole
/// stdout (trimmed) IS the reply. Whitespace-only output is the wrapper's
/// "empty" case (exit 3) — the model produced nothing usable.
pub fn parse_response(stdout_text: &str) -> Result<String, AgyAskError> {
    let trimmed = stdout_text.trim();
    if trimmed.is_empty() {
        return Err(AgyAskError::Empty {
            raw_head: raw_head(stdout_text),
        });
    }
    Ok(trimmed.to_string())
}

/// Classify a non-zero agy exit by scanning its STDERR (never stdout — the
/// model's reply could contain trigger words). Patterns mirror
/// `agy-delegate.sh`'s case block. Returns the most specific [`AgyAskError`];
/// the generic [`AgyAskError::Invocation`] is the safe fallback.
pub fn classify_failure(stderr_text: &str, exit_code: i32) -> AgyAskError {
    let blob = stderr_text.to_ascii_lowercase();
    if blob.contains("quota") || blob.contains("rate limit") || blob.contains("resource exhausted")
    {
        return AgyAskError::Quota;
    }
    if blob.contains("unauthenticated")
        || blob.contains("unauthorized")
        || blob.contains("sign in")
        || blob.contains("please authenticate")
        || blob.contains("reauth")
    {
        return AgyAskError::Auth;
    }
    if blob.contains("timed out")
        || blob.contains("deadline exceeded")
        || blob.contains("print-timeout")
    {
        return AgyAskError::Timeout { timeout_sec: 0.0 };
    }
    AgyAskError::Invocation { exit_code }
}

// ===========================================================================
// Error enum + exit-code map (mirror of agy-delegate.sh's exit codes)
// ===========================================================================

/// Errors from the agy ask path. Exit codes mirror `agy-delegate.sh`:
/// 2 failed / 3 empty / 10 quota / 11 auth / 12 timeout / 13 missing /
/// 130 interrupted. (1 for a non-ENOENT spawn OSError, matching the siblings.)
#[derive(Debug)]
pub enum AgyAskError {
    /// `agy` binary not found at spawn (ErrorKind::NotFound) — exit 13.
    NotFound,
    /// Clean exit but whitespace-only output (model declined) — exit 3.
    Empty { raw_head: String },
    /// agy quota / rate limit (classified from stderr) — exit 10.
    Quota,
    /// agy not authenticated (classified from stderr) — exit 11.
    Auth,
    /// Wall-clock / print timeout — exit 12.
    Timeout { timeout_sec: f64 },
    /// agy exited non-zero for an unclassified reason — exit 2.
    Invocation { exit_code: i32 },
    /// Non-ENOENT OSError at spawn — exit 1.
    OsError { message: String },
    /// Operator SIGINT (Ctrl-C) forwarded to the agy group — exit 130.
    Interrupted,
}

impl AgyAskError {
    /// Wrapper-compatible exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            AgyAskError::NotFound => 13,
            AgyAskError::Empty { .. } => 3,
            AgyAskError::Quota => 10,
            AgyAskError::Auth => 11,
            AgyAskError::Timeout { .. } => 12,
            AgyAskError::Invocation { exit_code } => {
                if *exit_code != 0 {
                    2
                } else {
                    1
                }
            }
            AgyAskError::OsError { .. } => 1,
            AgyAskError::Interrupted => 130,
        }
    }
}

impl std::fmt::Display for AgyAskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgyAskError::NotFound => {
                write!(
                    f,
                    "agy binary not found on PATH — install the Antigravity CLI"
                )
            }
            AgyAskError::Empty { raw_head } => write!(
                f,
                "agy returned empty output; first {} chars: {:?}",
                raw_head.chars().count(),
                raw_head
            ),
            AgyAskError::Quota => write!(f, "agy quota / rate limit exhausted"),
            AgyAskError::Auth => write!(f, "agy not authenticated — run `agy` once to sign in"),
            AgyAskError::Timeout { timeout_sec } => {
                write!(f, "agy timed out after {}s", timeout_sec)
            }
            AgyAskError::Invocation { exit_code } => write!(f, "agy exited {}", exit_code),
            AgyAskError::OsError { message } => {
                write!(f, "agy provider: OSError invoking agy: {}", message)
            }
            AgyAskError::Interrupted => write!(f, "agy interrupted by SIGINT (Ctrl-C)"),
        }
    }
}

impl std::error::Error for AgyAskError {}

// ===========================================================================
// Folder-trust pre-grant
// ===========================================================================

/// Grant agy folder-trust for `cwd` by upserting it into
/// `~/.gemini/trustedFolders.json` — the record agy reads. agy shares Gemini's
/// `~/.gemini/` config root but, unlike gemini, exposes no `--skip-trust` flag,
/// so the spawn cannot bypass the prompt with an argv flag the way every gemini
/// spawn does. This is the agy analogue of gemini's unconditional `--skip-trust`.
///
/// Without it, an INTERACTIVE agy worker launched in a not-yet-trusted cwd blocks
/// on agy's "Do you trust this folder?" modal, which eats the relay's priming
/// steer (`relay_prime_failed`) and the worker is born unusable.
/// `--dangerously-skip-permissions` (already on the agy spawn) auto-approves tool
/// calls only; it does not touch folder trust.
///
/// Best-effort: any I/O or parse failure logs once and returns, leaving agy to
/// prompt exactly as before (no regression, no panic). Idempotent: a no-op when
/// the cwd is already trusted or sits under a `TRUST_PARENT` ancestor.
pub fn ensure_agy_folder_trusted(cwd: &Path) {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return; // HOME unset: best-effort, agy prompts as before.
    };
    ensure_trusted_at(&home.join(".gemini").join("trustedFolders.json"), cwd);
}

/// Inner, fully unit-testable core: upsert `cwd -> "TRUST_FOLDER"` into the
/// trusted-folders map at `file`. Split out so tests can drive it against a temp
/// dir without touching the real `~/.gemini` config.
///
/// Uses the cwd EXACTLY as agy will open it (absolute, NOT canonicalized): agy
/// matches the literal cwd string it is launched in, so resolving symlinks (e.g.
/// `/tmp` -> `/private/tmp` on macOS) would write a key agy never checks.
/// Verified against a live agy `-i` probe.
fn ensure_trusted_at(file: &Path, cwd: &Path) {
    // Absolutize without canonicalizing (see fn doc). The daemon always forwards
    // an absolute cwd; the relative branch is a defensive fallback.
    let cwd_abs = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(cwd))
            .unwrap_or_else(|_| cwd.to_path_buf())
    };
    let cwd_key = cwd_abs.to_string_lossy().into_owned();

    // Read the existing map (absent -> empty). A parse failure or a non-object
    // root is left UNTOUCHED — never clobber the user's real grants (shared with
    // interactive gemini/agy).
    let mut map: serde_json::Map<String, serde_json::Value> = match std::fs::read_to_string(file) {
        Ok(s) if s.trim().is_empty() => serde_json::Map::new(),
        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(serde_json::Value::Object(m)) => m,
            _ => {
                eprintln!(
                    "fno-agents: agy trust: {:?} is not a JSON object; leaving untouched",
                    file
                );
                return;
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(e) => {
            eprintln!("fno-agents: agy trust: cannot read {:?}: {}", file, e);
            return;
        }
    };

    // Coverage check: exact key present (any value), or an ancestor TRUST_PARENT.
    // Either way agy already trusts this cwd — no write.
    if map.contains_key(&cwd_key) {
        return;
    }
    for (k, v) in &map {
        if v.as_str() == Some("TRUST_PARENT") && cwd_abs.starts_with(Path::new(k)) {
            return;
        }
    }

    // Grant only the exact cwd (TRUST_FOLDER, never TRUST_PARENT): never over-trust
    // siblings/children the worker has no business in.
    map.insert(
        cwd_key,
        serde_json::Value::String("TRUST_FOLDER".to_string()),
    );

    let Ok(serialized) = serde_json::to_string_pretty(&map) else {
        return;
    };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Atomic write: temp file alongside the target (same filesystem) + rename, so
    // a reader never sees a half-written file. The temp name must be unique PER
    // INVOCATION, not just per process: the daemon pre-trusts on concurrent tasks
    // (two agy spawns racing on different cwds), and `std::process::id()` is
    // identical across threads — a pid-only temp path would let one write clobber
    // the other's temp before its rename (torn file / spurious I/O error). A
    // process-wide atomic counter makes each writer's temp private, so every
    // rename publishes a COMPLETE valid file. The remaining last-writer-wins on
    // the final file is by design (a lost insert self-heals: the next spawn
    // re-detects the absent cwd and re-inserts).
    static TRUST_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TRUST_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = file.with_extension(format!("tmp.{}.{}", std::process::id(), seq));
    if std::fs::write(&tmp, serialized.as_bytes()).is_err() {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("fno-agents: agy trust: temp write failed near {:?}", file);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, file) {
        let _ = std::fs::remove_file(&tmp);
        eprintln!(
            "fno-agents: agy trust: rename into {:?} failed: {}",
            file, e
        );
    }
}

/// Result of a successful agy one-shot invocation.
#[derive(Debug, Clone)]
pub struct AgyResult {
    pub exit_code: i32,
    pub last_msg: String,
    pub duration_ms: u64,
}

// ===========================================================================
// Subprocess driver
// ===========================================================================

/// Open the JSONL tee, tagging an error as an invocation failure.
fn open_tee(log_path: &Path) -> Result<std::fs::File, AgyAskError> {
    crate::subprocess_ask::open_tee(log_path).map_err(|e| AgyAskError::OsError {
        message: format!("cannot open output tee: {}", e),
    })
}

/// Drive one `agy -p` subprocess: stdin `/dev/null`, capture stdout (plain
/// text) and stderr (for classification), bounded by the outer watchdog. Mirrors
/// `run_gemini`'s reap/grace/interrupt ordering but parses plain text.
fn run_agy(
    argv: &[String],
    output_path: &Path,
    timeout: Option<Duration>,
    popen_cwd: &Path,
    agent_self: Option<&str>,
) -> Result<AgyResult, AgyAskError> {
    use std::process::{Command, Stdio};

    let started = Instant::now();
    let tee_fh = open_tee(output_path)?;

    // QoS (x-c5cc): exec-wrap at background priority (identity when
    // worker_qos=off).
    let argv = crate::spawn_gate::qos_wrap(popen_cwd, argv.to_vec());
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // CRITICAL: detach stdin so agy -p never blocks on a non-TTY waiting for
    // input (the silent-stdout-drop gotcha).
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.current_dir(popen_cwd);
    if let Some(name) = agent_self {
        cmd.env("FNO_AGENT_SELF", name);
        cmd.env("FNO_AGENT_PROVIDER", "agy");
    }
    // Own process group so SIGTERM/SIGKILL/SIGINT reach agy's subshells.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(AgyAskError::NotFound),
        Err(e) => {
            eprintln!("agy provider: OSError invoking agy: {}", e);
            return Err(AgyAskError::OsError {
                message: e.to_string(),
            });
        }
    };

    let pid = child.id();
    // Forward operator Ctrl-C to agy's process group for this call's lifetime.
    // RAII guard held for the whole function — do NOT discard to `_`.
    let _sigint_guard = crate::subprocess_ask::SigintForwarder::install(pid);

    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");

    let tee = Arc::new(Mutex::new(tee_fh));
    let tee_stderr = tee.clone();
    // Capture stderr text (bounded) for failure classification, while also
    // tee'ing it to the log. Kept OFF stdout so the plain-text reply stays pure.
    let stderr_capture: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let capture_handle = stderr_capture.clone();

    let stderr_handle = std::thread::spawn(move || {
        let mut warned: HashSet<String> = HashSet::new();
        let mut total: usize = 0;
        let mut reader = BufReader::new(stderr_pipe);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(n) => {
                    // Tee (best-effort, warn-once on write error).
                    if let Ok(mut guard) = tee_stderr.lock() {
                        if let Err(e) = guard.write_all(line.as_bytes()) {
                            let key = e.to_string();
                            if warned.insert(key) {
                                eprintln!("agy provider: stderr tee write failed: {}", e);
                            }
                        } else {
                            let _ = guard.flush();
                        }
                    }
                    // Capture for classification, capped.
                    if total < STDERR_CAP {
                        if let Ok(mut cap) = capture_handle.lock() {
                            cap.push_str(&line);
                        }
                    }
                    total += n;
                }
                Err(_) => break,
            }
        }
    });

    let mut watchdog = crate::subprocess_ask::AskWatchdog::spawn(pid, timeout);

    // Read the whole stdout blob (agy emits plain text). agy output is UTF-8 in
    // practice, so try a zero-copy `from_utf8` first and only pay the lossy copy
    // on the rare invalid-byte path (a stray byte can't hard-fail the read).
    let mut stdout_bytes: Vec<u8> = Vec::new();
    {
        let mut reader = stdout_pipe;
        if let Err(e) = reader.read_to_end(&mut stdout_bytes) {
            eprintln!("agy provider: stdout stream read error: {}", e);
        }
    }
    let stdout_text = String::from_utf8(stdout_bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());

    // Tee the stdout blob under the shared lock (after EOF).
    if !stdout_text.is_empty() {
        if let Ok(mut guard) = tee.lock() {
            if guard.write_all(stdout_text.as_bytes()).is_ok() {
                if !stdout_text.ends_with('\n') {
                    let _ = guard.write_all(b"\n");
                }
                let _ = guard.flush();
            }
        }
    }

    watchdog.cancel();
    let (exit_code, sigkill_escalated) =
        crate::subprocess_ask::wait_with_grace(pid, &mut child, 5.0);
    watchdog.join();
    if stderr_handle.join().is_err() {
        eprintln!("agy provider: stderr drain thread panicked");
    }

    let duration_ms = started.elapsed().as_millis() as u64;
    let was_timed_out = watchdog.timed_out();
    // The stderr drain thread has been joined, so we are the sole owner — move
    // the captured text out of the mutex instead of cloning up to 256 KB.
    let stderr_text = stderr_capture
        .lock()
        .map(|mut s| std::mem::take(&mut *s))
        .unwrap_or_default();

    // Operator Ctrl-C wins over every other classification.
    if crate::subprocess_ask::ask_interrupted() {
        return Err(AgyAskError::Interrupted);
    }
    if was_timed_out || sigkill_escalated {
        return Err(AgyAskError::Timeout {
            timeout_sec: timeout.map(|d| d.as_secs_f64()).unwrap_or(0.0),
        });
    }
    // Non-zero exit dominates emptiness (wrapper ordering): classify from stderr.
    if exit_code != 0 {
        return Err(classify_failure(&stderr_text, exit_code));
    }
    // Clean exit: parse the plain-text reply (empty -> exit 3).
    let reply = parse_response(&stdout_text)?;
    Ok(AgyResult {
        exit_code,
        last_msg: reply,
        duration_ms,
    })
}

/// Spawn `agy -p ...` in `cwd` and return the plain-text reply.
pub fn agy_create(
    cwd: &Path,
    prompt: &str,
    from_name: &str,
    model: Option<&str>,
    output_path: &Path,
    timeout: Option<Duration>,
    agent_self: Option<&str>,
    add_dir: Option<&str>,
) -> Result<AgyResult, AgyAskError> {
    let full_prompt = inject_from_name(prompt, from_name);
    let argv = build_argv_once(&full_prompt, cwd, model, add_dir);
    run_agy(&argv, output_path, timeout, cwd, agent_self)
}

// ===========================================================================
// Dispatch (stateless one-shot)
// ===========================================================================

/// Stdout/stderr/exit triple returned to the client (mirror of the sibling
/// `AskOutcome`).
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

/// Derive the stable log path for an agy agent (mirror of the siblings).
fn derive_log_path(home: &AgentsHome, name: &str) -> std::path::PathBuf {
    home.root()
        .join("agents")
        .join("logs")
        .join(format!("{}.jsonl", name))
}

/// Orchestrate one agy `spawn --once`: validate, run `agy -p`, return the reply.
///
/// STATELESS by design — agy emits no session id, so there is no registry row to
/// create/tear-down and no `--continue` resume to wire from here. The `name` is
/// a label for the log path + events only.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_agy_once(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    model: Option<&str>,
    timeout: Option<Duration>,
    add_dir: Option<&str>,
) -> AskOutcome {
    use crate::claude_ask::py_repr;
    if let Err(msg) = crate::claude_ask::validate_spawn_inputs(name, from_name) {
        return AskOutcome::err(msg, 2);
    }

    let events = home.events_jsonl();

    // Authoritative registry read, fail-closed (codex P2): `maybe_run_spawn`'s
    // collision check uses an ADVISORY `unwrap_or_default()`, so a corrupt /
    // unreadable registry would be treated as empty and agy launched anyway. The
    // codex/gemini one-shots surface exit 12 here before running the CLI; agy
    // matches that, and re-checks the name collision under this read.
    let registry = match crate::state::load_registry(&home.registry_json()) {
        Ok(r) => r,
        Err(e) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "registry-read".into()),
                    ("name", name.into()),
                    ("provider", "agy".into()),
                    ("error", e.to_string().into()),
                ],
            );
            return AskOutcome::err(format!("registry read failed: {}", e), 12);
        }
    };
    if registry.find(name).is_some() {
        return AskOutcome::err(
            format!(
                "agent {} already exists; use 'fno agents rm {}' first or pick another name",
                py_repr(name),
                name
            ),
            2,
        );
    }

    // spawn allows an empty initial message; default it to "hello" (Python parity).
    let effective_message = if message.is_empty() { "hello" } else { message };
    let log_path = derive_log_path(home, name);
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let eff_timeout = timeout.or(Some(DEFAULT_ASK_TIMEOUT));
    match agy_create(
        cwd,
        effective_message,
        from_name,
        model,
        &log_path,
        eff_timeout,
        Some(name),
        add_dir,
    ) {
        Ok(res) => AskOutcome::ok_reply(res.last_msg),
        Err(e) => {
            let code = e.exit_code();
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "agy-once".into()),
                    ("name", name.into()),
                    ("provider", "agy".into()),
                    ("error", e.to_string().into()),
                ],
            );
            AskOutcome::err(e.to_string(), code)
        }
    }
}

/// Client interceptor for the `ask` (resume-by-name) verb on an agy target.
///
/// agy is STATELESS here (plain text, no session id), so a stateful `ask` resume
/// is not supported. Returns `None` for non-agy targets (fall through), or a
/// clear error directing the caller to `spawn --provider agy --once`.
pub fn maybe_run_agy_ask(home: &AgentsHome, params: &serde_json::Value, name: &str) -> Option<i32> {
    let provider_param = params.get("provider").and_then(|v| v.as_str());
    let registry = match crate::state::load_registry(&home.registry_json()) {
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
    // Borrow the provider string (lives until the function returns) instead of
    // cloning it (gemini review).
    let existing_provider = registry.find(name).map(|e| e.harness_name());
    let resolved = existing_provider.or(provider_param);
    if resolved != Some("agy") {
        return None; // not an agy target; fall through
    }
    eprintln!(
        "fno-agents: agy does not support stateful 'ask' resume (plain-text output, no session id); \
         use 'fno agents spawn --harness agy --once <name> --message <prompt>' for a one-shot."
    );
    Some(2)
}

#[cfg(test)]
mod trust_tests {
    use super::*;

    fn read_map(file: &Path) -> serde_json::Map<String, serde_json::Value> {
        let s = std::fs::read_to_string(file).unwrap();
        match serde_json::from_str(&s).unwrap() {
            serde_json::Value::Object(m) => m,
            other => panic!("not a JSON object: {other:?}"),
        }
    }

    // AC4-EDGE: absent file -> created with exactly {cwd: "TRUST_FOLDER"}.
    #[test]
    fn absent_file_created_with_trust_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".gemini").join("trustedFolders.json");
        let cwd = dir.path().join("work");
        ensure_trusted_at(&file, &cwd);
        let map = read_map(&file);
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get(cwd.to_string_lossy().as_ref())
                .and_then(|v| v.as_str()),
            Some("TRUST_FOLDER")
        );
    }

    // AC3-UI: exact key already present (any value) -> no rewrite.
    #[test]
    fn exact_key_present_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("trustedFolders.json");
        let cwd = dir.path().join("work");
        let key = cwd.to_string_lossy().into_owned();
        // Pre-seed with a DIFFERENT value to prove no rewrite (would flip to
        // TRUST_FOLDER if we touched it).
        std::fs::write(&file, format!("{{\n  {key:?}: \"TRUST_PARENT\"\n}}")).unwrap();
        let before = std::fs::read_to_string(&file).unwrap();
        ensure_trusted_at(&file, &cwd);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    // AC3-UI: cwd under an ancestor TRUST_PARENT entry -> no write.
    #[test]
    fn under_trust_parent_ancestor_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("trustedFolders.json");
        let parent = dir.path().join("workspaces");
        let cwd = parent.join("proj").join("wt");
        let pkey = parent.to_string_lossy().into_owned();
        std::fs::write(&file, format!("{{\n  {pkey:?}: \"TRUST_PARENT\"\n}}")).unwrap();
        let before = std::fs::read_to_string(&file).unwrap();
        ensure_trusted_at(&file, &cwd);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    // AC2-ERR: corrupt / non-object file -> left untouched, function returns.
    #[test]
    fn corrupt_file_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("trustedFolders.json");
        let corrupt = "this is not json {{{";
        std::fs::write(&file, corrupt).unwrap();
        ensure_trusted_at(&file, &dir.path().join("work"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), corrupt);
    }

    // Invariants: existing unrelated entries preserved after an insert.
    #[test]
    fn existing_entries_preserved_on_insert() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("trustedFolders.json");
        std::fs::write(&file, "{\n  \"/some/other/dir\": \"TRUST_FOLDER\"\n}").unwrap();
        let cwd = dir.path().join("work");
        ensure_trusted_at(&file, &cwd);
        let map = read_map(&file);
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("/some/other/dir").and_then(|v| v.as_str()),
            Some("TRUST_FOLDER")
        );
        assert_eq!(
            map.get(cwd.to_string_lossy().as_ref())
                .and_then(|v| v.as_str()),
            Some("TRUST_FOLDER")
        );
    }
}
