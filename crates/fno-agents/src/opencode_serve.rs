//! The serve-HTTP opencode worker lane (x-d9f9): `spawn --harness opencode
//! --substrate bg` as a real, unattended, persistent provider.
//!
//! opencode hosts two unattended shapes before this module: the headless
//! one-shot (`opencode_ask::dispatch_opencode_once`, stateless, 600s cap) and
//! the PTY pane (attended; every pane capability bit is false, so autonomous
//! dispatch refuses). A worker needs a third shape: a session that outlives the
//! spawn call, runs long turns without an operator, and captures structured
//! output. `opencode serve` is that shape - the node's reframe from PTY pane to
//! HTTP API, verified against opencode v1.14.50 (live probes 2026-08-23):
//!
//! - `opencode serve --port 0 --print-logs` prints `listening on
//!   http://127.0.0.1:<port>`; `OPENCODE_CONFIG=<file path>` merges an extra
//!   config FILE into the server (verified: a file granting
//!   `permission."*" = "allow"` lands in GET /config), which is how the
//!   unattended permission posture reaches a server that has no
//!   `--dangerously-skip-permissions` flag of its own. (Inline JSON would be
//!   `OPENCODE_CONFIG_CONTENT`; this module writes the file form.)
//! - `POST /session?directory=<abs>` mints a session bound to that directory,
//!   so ONE shared serve hosts workers across worktrees (a payload `directory`
//!   field is ignored - only the query param binds; probed both ways).
//! - `POST /session/:id` merges per-session permission rules
//!   (`Permission.merge`, handlers/session.ts:186); rule eval is wildcard on
//!   both name and pattern, findLast wins, unmatched defaults to `ask`
//!   (permission/evaluate.ts) - which is why tool permission must come from
//!   the serve config, not from per-session rules alone: an `ask` nobody
//!   answers is a hang, and a worker has no human.
//! - `opencode run --attach <url> --session <id> [--command ...]` drives one
//!   turn on the shared serve with native command-template expansion. The
//!   spawn launches that as a detached writer and returns immediately: no
//!   template text is duplicated into this crate (x-de43 keeps biting anyone
//!   who routes a slash command as prose). The attach writer prints NOTHING on
//!   stdout (verified live; `--format json` is dead on this path) - structured
//!   capture is the serve's own message readback (`GET /session/:id/message`,
//!   role/model/parts), fetched over HTTP, never scraped from a pane or pipe.
//!
//! Serve sessions land in the same global store as every other opencode
//! session, so the existing reachability probe (`opencode_reachable_with`)
//! covers serve rows unchanged.
//!
//! The writable-dirs grant (the double-writer hazard, decision d-06d56d5c
//! blocker two): opencode has no additive CLI dir flag (`--dir` SETS cwd), so
//! the pane lane cannot carry the computed set. The serve lane can: the
//! Python seam already publishes `FNO_WORKER_ADD_DIRS` for every non-pane
//! substrate, and this module turns that set into per-session
//! `external_directory` allow rules - the codex `--add-dir` pattern through
//! opencode's native cell. The rules are the scoped record riding on top of
//! the serve-level allow; they become load-bearing the day that blanket is
//! narrowed.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::claude_ask::{emit_event, validate_spawn_inputs};
use crate::opencode_ask::AskOutcome;
use crate::paths::AgentsHome;
use crate::state::{load_registry, update_registry, RegistryEntry};
use crate::AgentStatus;

/// Everything the spawn needs from a live serve: the loopback base URL, the
/// auth token the serve was booted with, and the pid (with its start time, so
/// a displaced-serve reap can never SIGTERM a recycled pid).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeHandle {
    pub base_url: String,
    pub token: String,
    pub pid: u32,
    pub pid_start: Option<u64>,
}

/// The unattended permission posture, written to the generated OPENCODE_CONFIG
/// before the serve starts. Same stance as `--dangerously-skip-permissions` on
/// the headless lane and every other fno worker lane; without it every tool
/// call that opencode does not allow by config evaluates to `ask`, and an
/// unanswered ask is a hang, not a refusal.
const SERVE_CONFIG_JSON: &str = r#"{"permission":{"*":"allow"}}"#;

/// Budgets: how long to wait for a fresh serve to print its port, and the
/// per-call HTTP timeouts. A serve boot is a node CLI start (~1-2s measured);
/// 15s leaves room for a cold JS runtime without wedging a spawn.
const SERVE_BOOT_BUDGET: Duration = Duration::from_secs(15);
const HTTP_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

// ===========================================================================
// Minimal HTTP/1.1 client (loopback, `Connection: close`)
// ===========================================================================

/// One request/response exchange against the serve. Blocking, std-only: the
/// serve is always 127.0.0.1, every call here is short (create / merge /
/// health), and the fno-agents spawn path is synchronous - a full HTTP stack
/// would be a dependency bought for nothing. `auth_token` rides every request
/// as Basic auth: the serve is booted with `OPENCODE_SERVER_PASSWORD`, so a
/// loopback listener with an allow-all permission posture is not reachable by
/// arbitrary local processes.
fn http_json(
    base_url: &str,
    method: &str,
    path_and_query: &str,
    body: Option<&serde_json::Value>,
    auth_token: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let after_scheme = base_url
        .strip_prefix("http://")
        .ok_or_else(|| format!("serve base url {base_url:?} is not http:// loopback"))?;
    let authority = after_scheme.trim_end_matches('/');
    let mut stream =
        TcpStream::connect(authority).map_err(|e| format!("connect {authority}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("write timeout: {e}"))?;
    let body_bytes = body.map(|v| v.to_string()).unwrap_or_default();
    let auth_header = auth_token
        .map(|t| {
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(format!("opencode:{t}"));
            format!("Authorization: Basic {encoded}\r\n")
        })
        .unwrap_or_default();
    let request = format!(
        "{method} {path_and_query} HTTP/1.1\r\nHost: {authority}\r\nAccept: application/json\r\nContent-Type: application/json\r\n{auth_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body_bytes.len(),
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(body_bytes.as_bytes()))
        .map_err(|e| format!("write to {authority}: {e}"))?;
    let mut raw = Vec::new();
    // Connection: close means EOF terminates the body; read_to_end is the
    // whole story for every response shape this server produces.
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read from {authority}: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line from {authority}"))?;
    let body_out = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Ok((status, body_out))
}

/// GET /global/health -> true iff `{healthy: true}`. Reused as the gate for a
/// recorded serve, so a single answer never decides a live serve is dead: a
/// busy node event loop can miss one 3s probe while workers run turns, and
/// the unhealthy branch SIGTERMs the serve (killing every worker session on
/// it). Three attempts, 1s apart - slow answers survive, dead ones do not.
fn serve_healthy(base_url: &str, token: &str) -> bool {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        if matches!(
            http_json(
                base_url,
                "GET",
                "/global/health",
                None,
                Some(token),
                HEALTH_TIMEOUT
            ),
            Ok((200, body)) if body.contains("\"healthy\":true")
        ) {
            return true;
        }
    }
    false
}

// ===========================================================================
// Serve lifecycle
// ===========================================================================

fn serve_state_path(home: &AgentsHome) -> std::path::PathBuf {
    home.root().join("opencode-serve.json")
}

fn serve_config_path(home: &AgentsHome) -> std::path::PathBuf {
    home.root().join("opencode-serve-config.json")
}

fn serve_log_path(home: &AgentsHome) -> std::path::PathBuf {
    home.root()
        .join("agents")
        .join("logs")
        .join("opencode-serve.log")
}

/// 32 hex chars from /dev/urandom - the per-serve Basic-auth password. No
/// rand crate; the OS CSPRNG is one read_exact away and the token never needs
/// reproduction.
fn new_serve_token() -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom").map_err(|e| format!("open urandom: {e}"))?;
    let mut buf = [0u8; 16];
    f.read_exact(&mut buf)
        .map_err(|e| format!("read urandom: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Extract the port from a serve log line (`opencode server listening on
/// http://127.0.0.1:59971`, measured shape). Free function so tests feed
/// canned logs; manual scan because the one pattern does not buy a regex
/// dependency this module otherwise avoids.
fn serve_port_from_log(text: &str) -> Option<u16> {
    let marker = "listening on http://127.0.0.1:";
    let idx = text.rfind(marker)?;
    let tail = &text[idx + marker.len()..];
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Ensure a shared serve is up and return its handle. Reuses the recorded
/// instance when it answers `/global/health`; otherwise writes the generated
/// config, boots a NEW serve detached (its own session via setsid, so it
/// outlives this client process - the worker turns continue on it after the
/// spawn returns), tails the boot log for the port, and records the state
/// file. One serve per agents-home, shared across every opencode worker on
/// the machine; it is idle-cheap between turns.
///
/// The whole check-or-boot runs under an flock on a sidecar lock file, so two
/// concurrent spawns can never both boot a replacement and leave the loser's
/// serve running orphaned; a displaced unhealthy serve (recorded pid that no
/// longer answers health) is SIGTERMed before the replacement boots.
pub fn ensure_serve(home: &AgentsHome) -> Result<ServeHandle, String> {
    let adapter = OpenCodeDaemonAdapter::new(home);
    let result = crate::harness_daemon::ensure_harness_daemon(&adapter)
        .map_err(|error| error.to_string())?;
    let record = &result.state.raw;
    Ok(ServeHandle {
        base_url: record
            .get("base_url")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        token: record
            .get("token")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        pid: result.state.pid.unwrap_or_default(),
        pid_start: result.state.process_start_time,
    })
}

struct OpenCodeDaemonAdapter<'a> {
    home: &'a AgentsHome,
    state_path: PathBuf,
    lock_path: PathBuf,
}

impl<'a> OpenCodeDaemonAdapter<'a> {
    fn new(home: &'a AgentsHome) -> Self {
        Self {
            home,
            state_path: serve_state_path(home),
            lock_path: home.root().join("opencode-serve.lock"),
        }
    }
}

impl crate::harness_daemon::HarnessDaemonAdapter for OpenCodeDaemonAdapter<'_> {
    fn harness(&self) -> &str {
        "opencode"
    }

    fn state_path(&self) -> &Path {
        &self.state_path
    }

    fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    fn parse_state(&self, raw: &str) -> Result<crate::harness_daemon::DaemonState, String> {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|error| error.to_string())?;
        let _token = value
            .get("token")
            .and_then(|value| value.as_str())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| "missing serve token".to_string())?;
        let base_url = value
            .get("base_url")
            .and_then(|value| value.as_str())
            .filter(|url| !url.is_empty())
            .map(str::to_string)
            .ok_or_else(|| "missing serve base_url".to_string())?;
        let pid = value
            .get("pid")
            .and_then(|value| value.as_u64())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| "missing serve pid".to_string())? as u32;
        let pid_start = value
            .get("pid_start")
            .and_then(|value| value.as_u64())
            .filter(|start| *start > 0)
            .ok_or_else(|| "missing serve process start time".to_string())?;
        crate::harness_daemon::DaemonState::new(
            value,
            pid,
            pid_start,
            base_url,
            format!("{pid}:{pid_start}"),
        )
    }

    fn is_healthy(&self, state: &crate::harness_daemon::DaemonState) -> bool {
        let token = state
            .raw
            .get("token")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        serve_healthy(&state.endpoint, token)
    }

    fn boot(&self) -> Result<crate::harness_daemon::DaemonState, String> {
        let config = serve_config_path(self.home);
        if let Some(parent) = config.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir for serve config: {e}"))?;
        }
        std::fs::write(&config, SERVE_CONFIG_JSON)
            .map_err(|e| format!("write serve config {:?}: {e}", config))?;
        // Fresh token per serve: Basic-auth material for every request (the serve
        // is booted with OPENCODE_SERVER_PASSWORD), generated from /dev/urandom.
        let token = new_serve_token()?;
        let log_path = serve_log_path(self.home);
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // TRUNCATE, not append: the boot loop below scans this file for the
        // serving port, and a previous boot's `listening on` line would win
        // rfind while the new serve is still starting - recording the OLD url
        // with the NEW pid. Truncating also bounds the file per boot.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .map_err(|e| format!("open serve log {:?}: {e}", log_path))?;
        let log_err = log
            .try_clone()
            .map_err(|e| format!("clone serve log handle: {e}"))?;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new("opencode");
        cmd.args(["serve", "--port", "0", "--print-logs"])
            .env("OPENCODE_CONFIG", &config)
            .env("OPENCODE_SERVER_PASSWORD", &token)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("spawn opencode serve: {e}"))?;
        let pid = child.id();
        // Leak the handle on purpose: the serve must outlive this process. The
        // detach is real (setsid + no wait); dropping the Rust handle only stops
        // reaping, which is the point.
        std::mem::forget(child);
        let pid_start = crate::daemon::process_start_time(pid);

        let deadline = std::time::Instant::now() + SERVE_BOOT_BUDGET;
        loop {
            std::thread::sleep(Duration::from_millis(250));
            if let Ok(log_text) = std::fs::read_to_string(&log_path) {
                if let Some(port) = serve_port_from_log(&log_text) {
                    let base_url = format!("http://127.0.0.1:{port}");
                    if serve_healthy(&base_url, &token) {
                        let record = serde_json::json!({
                            "base_url": base_url,
                            "port": port,
                            "pid": pid,
                            "pid_start": pid_start,
                            "token": token,
                        });
                        let pid_start = pid_start.ok_or_else(|| {
                            "opencode serve did not expose a process start time".to_string()
                        })?;
                        return crate::harness_daemon::DaemonState::new(
                            record,
                            pid,
                            pid_start,
                            base_url,
                            format!("{pid}:{pid_start}"),
                        );
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                "opencode serve did not report a healthy port within {}s (log: {:?}, pid {pid})",
                SERVE_BOOT_BUDGET.as_secs(),
                log_path
            ));
            }
        }
    }
}

// ===========================================================================
// Spawn dispatch
// ===========================================================================

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Percent-encode a directory path for the `?directory=` query value: every
/// byte outside the RFC 3986 unreserved set plus `/` (a legal, meaningful
/// separator inside the value) becomes `%XX`. The previous space/?/#-only
/// list let `&` split the param and a stray `%` decode into a different
/// character, binding the session to the wrong directory.
fn encode_query_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The writable-dirs grant as opencode permission rules: TWO
/// `external_directory` allows per computed dir. opencode's wildcard `*` is
/// `.*` (it crosses `/`, util/wildcard.ts), so the earlier single `<dir>*`
/// rule also matched same-prefix SIBLINGS (a grant for `~/.fno` covered
/// `~/.fno-backup`). `<dir>` covers the directory itself (and the
/// load-bearing FILES at the state root's top level resolve under it via the
/// exact path), `<dir>/*` covers everything beneath - and neither reaches a
/// sibling. The pattern language has no tighter form.
fn permission_rules_for(dirs: &[String]) -> serde_json::Value {
    let mut rules = Vec::new();
    for d in dirs.iter().filter(|d| !d.is_empty()) {
        for pattern in [d.clone(), format!("{d}/*")] {
            rules.push(serde_json::json!({
                "permission": "external_directory",
                "pattern": pattern,
                "action": "allow",
            }));
        }
    }
    serde_json::json!(rules)
}

/// The detached-writer argv: `opencode run --attach <serve> --session <sid>`
/// with the one-shot lane's own tail builder. No `--format json`: the attach
/// writer prints NOTHING on stdout (verified live - the flag is dead there);
/// structured capture is the serve's message readback, which the log path is
/// not. The log collects the writer's stderr diagnostics.
fn writer_argv(
    serve: &ServeHandle,
    session_id: &str,
    message: &str,
    model: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        "opencode".to_string(),
        "run".to_string(),
        "--attach".to_string(),
        serve.base_url.clone(),
        "--session".to_string(),
        session_id.to_string(),
        "--dangerously-skip-permissions".to_string(),
    ];
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        argv.push("--model".to_string());
        argv.push(m.to_string());
    }
    argv.extend(crate::provider::opencode_run_tail(message));
    argv
}

/// Orchestrate one `spawn --harness opencode --substrate bg`: validate,
/// fail-closed registry + collision check, ensure the shared serve, mint a
/// session bound to the worker cwd, record the computed writable-dirs grant as
/// per-session permission rules, append the registry row, and launch the
/// detached `run --attach` writer. Fire-and-forget by design: the turn runs on
/// the serve; the writer only streams its events to the log.
pub fn dispatch_opencode_serve(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    model: Option<&str>,
) -> AskOutcome {
    dispatch_opencode_serve_inner(
        home,
        name,
        message,
        from_name,
        cwd,
        model,
        crate::claude_ask::state_dirs_from_env(),
        "opencode",
    )
}

/// The test seam: the writable-dir set and the writer binary are injected so
/// unit tests never mutate process-global env (the suite runs parallel in one
/// process; a set_var here races every other env-reading test).
#[allow(clippy::too_many_arguments)]
fn dispatch_opencode_serve_inner(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    model: Option<&str>,
    state_dirs: Vec<String>,
    opencode_bin: &str,
) -> AskOutcome {
    if let Err(msg) = validate_spawn_inputs(name, from_name) {
        return AskOutcome::err(msg, 2);
    }
    let events = home.events_jsonl();

    let registry = match load_registry(&home.registry_json()) {
        Ok(r) => r,
        Err(e) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "registry-read".into()),
                    ("name", name.into()),
                    ("provider", "opencode".into()),
                    ("error", e.to_string().into()),
                ],
            );
            return AskOutcome::err(format!("registry read failed: {e}"), 12);
        }
    };
    if registry.find(name).is_some() {
        return AskOutcome::err(
            format!(
                "agent {name:?} already exists; use 'fno agents rm {name}' first or pick another name"
            ),
            2,
        );
    }

    let serve = match ensure_serve(home) {
        Ok(s) => s,
        Err(msg) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "opencode-serve".into()),
                    ("name", name.into()),
                    ("provider", "opencode".into()),
                    ("error", msg.clone().into()),
                ],
            );
            return AskOutcome::err(msg, 13);
        }
    };

    // Session minted on the shared serve, bound to the worker cwd.
    let effective_message = if message.is_empty() { "hello" } else { message };
    let full_prompt = if effective_message.starts_with('/') {
        effective_message.to_string()
    } else {
        format!("[from: {from_name}]\n\n{effective_message}")
    };
    let create_path = format!(
        "/session?directory={}",
        encode_query_path(&cwd.to_string_lossy())
    );
    let (status, body) = match http_json(
        &serve.base_url,
        "POST",
        &create_path,
        Some(&serde_json::json!({"title": name})),
        Some(&serve.token),
        HTTP_CALL_TIMEOUT,
    ) {
        Ok(r) => r,
        Err(msg) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "session-create".into()),
                    ("name", name.into()),
                    ("provider", "opencode".into()),
                    ("error", msg.clone().into()),
                ],
            );
            return AskOutcome::err(format!("opencode serve session create failed: {msg}"), 12);
        }
    };
    let session_id = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string));
    let session_id = match session_id {
        Some(sid) if crate::provider::is_opencode_session_id(&sid) => sid,
        other => {
            let msg = format!(
                "opencode serve returned {status} with no usable session id ({:?})",
                other.unwrap_or_default()
            );
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "session-create".into()),
                    ("name", name.into()),
                    ("provider", "opencode".into()),
                    ("error", msg.clone().into()),
                ],
            );
            return AskOutcome::err(msg, 12);
        }
    };

    // The writable-dirs grant (codex `--add-dir` pattern, opencode's cell).
    // The serve-level config already allows; these rules are the scoped
    // record, so a merge failure is a named note, not a dead spawn.
    if !state_dirs.is_empty() {
        match http_json(
            &serve.base_url,
            "PATCH",
            &format!("/session/{session_id}"),
            Some(&serde_json::json!({"permission": permission_rules_for(&state_dirs)})),
            Some(&serve.token),
            HTTP_CALL_TIMEOUT,
        ) {
            Ok((204, _)) | Ok((200, _)) => {}
            Ok((code, body)) => eprintln!(
                "note: opencode permission grant for {} dir(s) answered {}: this worker's claim writes may fail",
                state_dirs.len(),
                tail_reason(&body, code)
            ),
            Err(msg) => eprintln!(
                "note: opencode permission grant POST failed: {msg}; this worker's claim writes may fail"
            ),
        }
    }

    // Registry row inputs: the log the writer streams into, and the parent
    // identity stamped at the spawn seam.
    let log_path = home
        .root()
        .join("agents")
        .join("logs")
        .join(format!("{name}.jsonl"));
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let log_file_created = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .is_ok();

    // Detached writer, LAUNCHED BEFORE the registry row (claude bg's order):
    // the row then carries the writer's pid as its liveness axis, and a launch
    // failure leaves no row and no session - never a wedged worker name. The
    // writer streams the turn's JSON events to the log, then exits; the serve
    // keeps the session, so a dead writer is a capture gap, not a dead worker.
    // argv[0] is the writer executable: PATH-resolved `opencode` in
    // production, the test stub's absolute path under test. Swapped BEFORE
    // qos_wrap so the QoS prefix (taskpolicy/nice) stays argv[0].
    let mut argv = writer_argv(&serve, &session_id, &full_prompt, model);
    argv[0] = opencode_bin.to_string();
    let argv = crate::spawn_gate::qos_wrap(cwd, argv);
    let writer_pid = {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.stdin(Stdio::null());
        if let Ok(fh) = out {
            cmd.stdout(Stdio::from(fh));
            if let Ok(err_fh) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                cmd.stderr(Stdio::from(err_fh));
            }
        }
        cmd.current_dir(cwd);
        cmd.env("FNO_AGENT_SELF", name);
        cmd.env("FNO_AGENT_HARNESS", "opencode");
        // A shell exporting both color knobs (NO_COLOR + FORCE_COLOR, common
        // in prompt frameworks) crashes the opencode writer at module load
        // (assertion trace in its color init, seen live 2026-08-23). The
        // writer streams to a log file, where color is noise anyway. Strip
        // both so the child starts from the CLI's own default.
        cmd.env_remove("NO_COLOR");
        cmd.env_remove("FORCE_COLOR");
        // The writer attaches to an authenticated serve; the run client reads
        // its password from this env (`-p` is the flag spelling).
        cmd.env("OPENCODE_SERVER_PASSWORD", &serve.token);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                std::mem::forget(child);
                pid
            }
            Err(e) => {
                // No row exists yet; the minted session is the only leftover,
                // so remove it rather than leaving an unreachable session.
                let _ = delete_session(&serve.base_url, &serve.token, &session_id);
                let code = if e.kind() == std::io::ErrorKind::NotFound {
                    13
                } else {
                    2
                };
                let msg = if e.kind() == std::io::ErrorKind::NotFound {
                    "opencode binary not found on PATH (writer launch; session removed)".to_string()
                } else {
                    format!("opencode writer spawn failed: {e}")
                };
                emit_event(
                    &events,
                    "agent_ask_failed",
                    &[
                        ("stage", "writer-launch".into()),
                        ("name", name.into()),
                        ("provider", "opencode".into()),
                        ("error", msg.clone().into()),
                    ],
                );
                return AskOutcome::err(msg, code);
            }
        }
    };

    // Registry row (the worker identity: harness + session on the serve). The
    // writer's pid + start-time are the row's liveness axis: the spawn gate's
    // slot count and reconcile both read them, so a serve worker counts
    // against the concurrency cap while its turn runs and the row retires
    // when the writer exits (the store-membership probe alone could never
    // retire it - serve sessions persist after the work is done).
    let (parent_session, parent_harness, parent_cwd) = crate::claims::ambient_parent_edge();
    let new_entry = RegistryEntry {
        name: name.to_string(),
        short_id: session_id.clone(),
        legacy_provider: String::new(),
        provider: Some("opencode".to_string()),
        model: model.filter(|m| !m.is_empty()).map(str::to_string),
        effort: None,
        harness: Some("opencode".to_string()),
        harness_session_id: Some(session_id.clone()),
        cwd: cwd.to_string_lossy().to_string(),
        project_root: String::new(),
        session_id: Some(session_id.clone()),
        origin: Some("spawn".to_string()),
        spawn_trigger: None,
        spawned_by_session: parent_session,
        spawned_by_harness: parent_harness,
        spawned_by_cwd: parent_cwd,
        legacy_claude_short_id: None,
        claude_session_uuid: None,
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        host_mode: None,
        cc_session_id: None,
        status: AgentStatus::Live,
        last_message_at: None,
        created_at: now_iso(),
        pid: Some(writer_pid),
        pid_start_time: crate::daemon::process_start_time(writer_pid),
        log_path: log_file_created.then(|| log_path.to_string_lossy().to_string()),
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: None,
        delivery_policy: None,
    };
    let registry_path = home.registry_json();
    match update_registry(&registry_path, |reg| {
        if reg.find(name).is_some() {
            false
        } else {
            reg.entries.push(new_entry.clone());
            true
        }
    }) {
        Ok(true) => {}
        Ok(false) => {
            // Roll the launch back: kill the writer and drop the session so a
            // retry with the same name is not wedged by orphaned state.
            let _ = delete_session(&serve.base_url, &serve.token, &session_id);
            unsafe { libc::kill(writer_pid as i32, libc::SIGTERM) };
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "name-collision".into()),
                    ("name", name.into()),
                    ("provider", "opencode".into()),
                    ("session_id", session_id.clone().into()),
                ],
            );
            return AskOutcome::err(
                format!(
                    "agent {name:?} already exists (registered concurrently); opencode session removed, writer stopped"
                ),
                12,
            );
        }
        Err(e) => {
            let _ = delete_session(&serve.base_url, &serve.token, &session_id);
            unsafe { libc::kill(writer_pid as i32, libc::SIGTERM) };
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "registry-write".into()),
                    ("name", name.into()),
                    ("provider", "opencode".into()),
                    ("error", e.to_string().into()),
                ],
            );
            return AskOutcome::err(format!("registry write failed: {e}"), 12);
        }
    }

    emit_event(
        &events,
        "agent_spawned",
        &[
            ("name", name.into()),
            ("provider", "opencode".into()),
            ("harness", "opencode".into()),
            ("session_id", session_id.clone().into()),
            ("serve_url", serve.base_url.clone().into()),
        ],
    );
    AskOutcome::ok_reply(
        serde_json::json!({
            "ok": true,
            "name": name,
            "session_id": session_id,
            "short_id": session_id,
            "serve_url": serve.base_url.clone(),
            "durability": "daemon-owned",
            "incarnation": format!("{}:{}", serve.pid, serve.pid_start.unwrap_or_default()),
            "endpoint": serve.base_url,
            "log_path": log_path.to_string_lossy(),
        })
        .to_string(),
    )
}

/// Short failure reason for a non-2xx merge answer (HTTP codes carry no text
/// of their own here; the body's first line does).
fn tail_reason(body: &str, code: u16) -> String {
    let first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let trimmed = first.trim().chars().take(120).collect::<String>();
    if trimmed.is_empty() {
        format!("HTTP {code}")
    } else {
        format!("HTTP {code}: {trimmed}")
    }
}

// ===========================================================================
// Read/teardown surface (the journey test's eyes and broom)
// ===========================================================================

/// `GET /session/:id` as parsed JSON. The session object carries the merged
/// `permission` rules, so a caller can assert the writable-dirs grant landed.
pub fn fetch_session(
    base_url: &str,
    token: &str,
    session_id: &str,
) -> Result<serde_json::Value, String> {
    let (status, body) = http_json(
        base_url,
        "GET",
        &format!("/session/{session_id}"),
        None,
        Some(token),
        HTTP_CALL_TIMEOUT,
    )?;
    if status != 200 {
        return Err(format!(
            "GET session answered {status}: {}",
            tail_reason(&body, status)
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("session body parse: {e}"))
}

/// `GET /session/:id/message` as parsed JSON (structured capture readback).
pub fn fetch_messages(
    base_url: &str,
    token: &str,
    session_id: &str,
) -> Result<serde_json::Value, String> {
    let (status, body) = http_json(
        base_url,
        "GET",
        &format!("/session/{session_id}/message"),
        None,
        Some(token),
        HTTP_CALL_TIMEOUT,
    )?;
    if status != 200 {
        return Err(format!(
            "GET messages answered {status}: {}",
            tail_reason(&body, status)
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("messages body parse: {e}"))
}

/// `DELETE /session/:id` - best-effort teardown; an error is returned, not
/// panicked on, so a caller can still finish its other cleanup.
pub fn delete_session(base_url: &str, token: &str, session_id: &str) -> Result<(), String> {
    let (status, body) = http_json(
        base_url,
        "DELETE",
        &format!("/session/{session_id}"),
        None,
        Some(token),
        HTTP_CALL_TIMEOUT,
    )?;
    if status == 200 || status == 204 {
        Ok(())
    } else {
        Err(format!(
            "DELETE session answered {status}: {}",
            tail_reason(&body, status)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(dir: &Path) -> AgentsHome {
        AgentsHome::at(dir.to_path_buf())
    }

    #[test]
    fn port_scan_finds_the_measured_log_line() {
        let log = "INFO stuff\nwarn noise\nopencode server listening on http://127.0.0.1:59971\nlater lines";
        assert_eq!(serve_port_from_log(log), Some(59971));
        assert_eq!(serve_port_from_log("no port here"), None);
        assert_eq!(serve_port_from_log(""), None);
    }

    #[test]
    fn permission_rules_cover_dir_children_and_no_siblings() {
        let rules = permission_rules_for(&["/Users/x/.fno".to_string()]);
        let arr = rules.as_array().unwrap();
        // TWO rules per dir: exact + children. opencode's `*` is `.*` (crosses
        // `/`), so the pair covers the dir, its top-level files (graph.json),
        // and nested state - while a single `<dir>*` would also have matched
        // the sibling `/Users/x/.fno-backup`.
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["pattern"], "/Users/x/.fno");
        assert_eq!(arr[1]["pattern"], "/Users/x/.fno/*");
        for rule in arr {
            assert_eq!(rule["permission"], "external_directory");
            assert_eq!(rule["action"], "allow");
        }
        assert!(permission_rules_for(&[]).as_array().unwrap().is_empty());
        assert!(permission_rules_for(&[String::new()])
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn writer_argv_is_attach_session_bypass_then_tail() {
        let serve = ServeHandle {
            base_url: "http://127.0.0.1:59971".to_string(),
            token: "test-token".to_string(),
            pid: 1,
            pid_start: None,
        };
        // Slash command rides --command (x-de43). No --format json: the attach
        // writer prints nothing on stdout (verified live), so the flag is dead.
        assert_eq!(
            writer_argv(&serve, "ses_abc123", "/fno:target --no-merge x-1", None),
            vec![
                "opencode",
                "run",
                "--attach",
                "http://127.0.0.1:59971",
                "--session",
                "ses_abc123",
                "--dangerously-skip-permissions",
                "--command",
                "fno:target",
                "--",
                "--no-merge",
                "x-1"
            ]
        );
        // Prose stays one positional; model threads before the tail.
        assert_eq!(
            writer_argv(&serve, "ses_abc123", "do the thing", Some("zai/glm-5.3")),
            vec![
                "opencode",
                "run",
                "--attach",
                "http://127.0.0.1:59971",
                "--session",
                "ses_abc123",
                "--dangerously-skip-permissions",
                "--model",
                "zai/glm-5.3",
                "--",
                "do the thing"
            ]
        );
    }

    #[test]
    fn query_encoding_covers_the_path_breakers() {
        assert_eq!(encode_query_path("/a/b c"), "/a/b%20c");
        assert_eq!(encode_query_path("/a?b#c"), "/a%3Fb%23c");
        // `&` would split the param; `%` would decode into a different byte.
        assert_eq!(encode_query_path("/a&b%c"), "/a%26b%25c");
        assert_eq!(encode_query_path("/plain/path-1.~_x"), "/plain/path-1.~_x");
    }

    // A fake serve: answers health, session create (echoes a canned ses id),
    // and the permission merge; records every request line it saw.
    struct FakeServe {
        addr: std::net::SocketAddr,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        auth_seen: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
    }

    impl FakeServe {
        fn start(session_id: &str) -> FakeServe {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let auth_seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = requests.clone();
            let auth_tx = auth_seen.clone();
            let sid = session_id.to_string();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    // Read the full request (headers + body may arrive in
                    // separate segments). Short timeout: stop reading at the
                    // first quiet gap, which for these tiny requests is the
                    // whole request. Closing with unread bytes would RST and
                    // the client's read_to_end loses the response.
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
                    let mut req = String::new();
                    let mut buf = [0u8; 8192];
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => req.push_str(&String::from_utf8_lossy(&buf[..n])),
                            Err(_) => break,
                        }
                    }
                    let line = req.lines().next().unwrap_or("").to_string();
                    seen.lock().unwrap().push(line.clone());
                    auth_tx
                        .lock()
                        .unwrap()
                        .push(req.contains("Authorization: Basic "));
                    let body_start = req.split_once("\r\n\r\n").map(|(_, b)| b.to_string());
                    let (status, body) = if line.starts_with("GET /global/health") {
                        ("200 OK", "{\"healthy\":true}".to_string())
                    } else if line.starts_with("POST /session?") {
                        (
                            "200 OK",
                            format!("{{\"id\":\"{sid}\",\"directory\":\"/w\"}}"),
                        )
                    } else if line.starts_with("PATCH /session/") {
                        // Permission merge: assert the body carried rules.
                        let carried = body_start
                            .as_deref()
                            .unwrap_or("")
                            .contains("external_directory");
                        let body = if carried {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        };
                        ("204 No Content", body)
                    } else {
                        ("404 Not Found", "{}".to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    // FIN so the client's read_to_end sees a clean EOF.
                    let _ = stream.shutdown(std::net::Shutdown::Write);
                }
            });
            FakeServe {
                addr,
                requests,
                auth_seen,
            }
        }
        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    #[test]
    fn http_json_round_trips_against_a_fake_serve() {
        let fake = FakeServe::start("ses_roundtrip1abc");
        let (status, body) = http_json(
            &fake.base_url(),
            "GET",
            "/global/health",
            None,
            Some("tok"),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(status, 200);
        assert!(body.contains("\"healthy\":true"));
        let (status, body) = http_json(
            &fake.base_url(),
            "POST",
            "/session?directory=/w",
            Some(&serde_json::json!({"title": "wk"})),
            Some("tok"),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(status, 200);
        assert!(body.contains("ses_roundtrip1abc"));
        assert_eq!(
            fake.requests.lock().unwrap()[0],
            "GET /global/health HTTP/1.1"
        );
        assert!(fake.requests.lock().unwrap()[1].starts_with("POST /session?directory=/w"));
        // Every authenticated request carried the Basic header.
        assert!(fake.auth_seen.lock().unwrap().iter().all(|s| *s));
    }

    #[test]
    fn dispatch_creates_session_grants_dirs_and_rows_registry() {
        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        let fake = FakeServe::start("ses_dispatchtest1");

        // Point ensure_serve at the fake by pre-writing the state file, so no
        // real opencode boots in a unit test.
        let record = serde_json::json!({
            "base_url": fake.base_url(),
            "port": fake.addr.port(),
            "pid": 4242,
            "pid_start": 1,
            "token": "test-token",
        });
        std::fs::write(serve_state_path(&h), record.to_string()).unwrap();

        // A stub writer binary: the injected seam points argv[0] at it, so no
        // PATH mutation and no real `opencode` run.
        let stub = dir.path().join("opencode-stub");
        std::fs::write(&stub, "#!/bin/sh\nsleep 60\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cwd = dir.path().join("w");
        std::fs::create_dir_all(&cwd).unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let outcome = dispatch_opencode_serve_inner(
            &h,
            "wk-serve",
            "hello there",
            "op",
            &cwd,
            None,
            vec![state_dir.to_string_lossy().to_string()],
            &stub.to_string_lossy(),
        );

        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        let receipt: serde_json::Value = serde_json::from_str(outcome.stdout.trim()).unwrap();
        assert_eq!(receipt["session_id"], "ses_dispatchtest1");
        assert_eq!(receipt["ok"], true);
        // The registry row exists with the harness session bound AND the
        // writer's pid as its liveness axis (spawn-gate cap + reconcile).
        let reg = load_registry(&h.registry_json()).unwrap();
        let row = reg.find("wk-serve").expect("row appended");
        assert_eq!(row.harness.as_deref(), Some("opencode"));
        assert_eq!(row.harness_session_id.as_deref(), Some("ses_dispatchtest1"));
        let writer_pid = row.pid.expect("writer pid on the row");
        assert!(row.pid_start_time.is_some(), "pid start-time captured");
        // The stub writer sleeps; do not leak it past the test.
        unsafe { libc::kill(writer_pid as i32, libc::SIGKILL) };
        // The permission merge carried the external_directory rules.
        let requests = fake.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .any(|r| r.starts_with("PATCH /session/ses_dispatchtest1 ")),
            "permission merge fired; saw {:?}",
            *requests
        );
    }

    #[test]
    fn dispatch_refuses_name_collision_and_bad_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        std::fs::create_dir_all(h.registry_json().parent().unwrap()).unwrap();
        std::fs::write(
            &h.registry_json(),
            serde_json::json!({
                "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
                "agents": [{
                    "name": "wk-taken",
                    "harness": "opencode",
                    "cwd": "/x",
                    "status": "live",
                    "created_at": "2026-01-01T00:00:00Z",
                }],
            })
            .to_string(),
        )
        .unwrap();
        let out = dispatch_opencode_serve_inner(
            &h,
            "wk-taken",
            "m",
            "op",
            Path::new("/tmp"),
            None,
            Vec::new(),
            "opencode",
        );
        assert_eq!(out.exit_code, 2);

        // A serve answering with a non-ses id refuses rather than rowing junk.
        let fake = FakeServe::start("not-an-opencode-id");
        let record = serde_json::json!({
            "base_url": fake.base_url(),
            "pid": 1,
            "pid_start": 1,
            "token": "test-token"
        });
        std::fs::write(serve_state_path(&h), record.to_string()).unwrap();
        let out = dispatch_opencode_serve_inner(
            &h,
            "wk-fresh",
            "m",
            "op",
            Path::new("/tmp"),
            None,
            Vec::new(),
            "opencode",
        );
        assert_eq!(out.exit_code, 12);
        assert!(
            out.stderr.contains("no usable session id"),
            "stderr: {}",
            out.stderr
        );
    }
}
