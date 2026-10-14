//! The serve-HTTP opencode worker lane : `spawn --harness opencode
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
//!   template text is duplicated into this crate (keeps biting anyone
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
use crate::opencode_ask::{apply_opencode_variant, AskOutcome, OPENCODE_DEFAULT_MODEL};
use crate::paths::AgentsHome;
use crate::state::load_registry;

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
///
/// The key is contract-specific: 1.x reads a `permission` MAP; 2.x reads an
/// ordered `permissions` RULE LIST (Config.md "Permissions", rule shape
/// `{action, resource, effect}`), so a v2 serve handed the 1.x map reads no
/// rules and every tool call lands on `ask` - a hang, not a refusal.
const SERVE_CONFIG_JSON_V1: &str = r#"{"permission":{"*":"allow"}}"#;
const SERVE_CONFIG_JSON_V2: &str =
    r#"{"permissions":[{"action":"*","resource":"*","effect":"allow"}]}"#;

/// The serve config for the contract the serve binary reports. One bounded
/// `--version`; an unknown version renders the 2.x posture, matching the
/// installer's v2-default.
fn serve_config_json(bin: &str) -> &'static str {
    match crate::opencode_install::classify_contract(bin) {
        (crate::opencode_install::OpencodeContract::V2, _) => SERVE_CONFIG_JSON_V2,
        (crate::opencode_install::OpencodeContract::V1, _) => SERVE_CONFIG_JSON_V1,
    }
}

/// Budgets: how long to wait for a fresh serve to print its port, and the
/// per-call HTTP timeouts. A serve boot is a node CLI start (~1-2s measured);
/// 15s leaves room for a cold JS runtime without wedging a spawn.
const SERVE_BOOT_BUDGET: Duration = Duration::from_secs(15);
const HTTP_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
const STEER_POLL_INTERVAL: Duration = Duration::from_millis(250);

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
        let token = value
            .get("token")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let base_url = value
            .get("base_url")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let pid = value
            .get("pid")
            .and_then(|value| value.as_u64())
            .filter(|pid| *pid > 0)
            .map(|pid| pid as u32);
        let pid_start = value
            .get("pid_start")
            .and_then(|value| value.as_u64())
            .filter(|start| *start > 0);
        let endpoint = if base_url.is_empty() {
            "unresolved://opencode".to_string()
        } else {
            base_url
        };
        let incarnation = match (pid, pid_start) {
            (Some(pid), Some(start)) => format!("{pid}:{start}"),
            _ if !token.is_empty() && endpoint != "unresolved://opencode" => endpoint.clone(),
            _ => "opencode-unresolved".to_string(),
        };
        crate::harness_daemon::DaemonState::with_optional_process_identity(
            value,
            pid,
            pid_start,
            endpoint,
            incarnation,
        )
    }

    fn is_healthy(&self, state: &crate::harness_daemon::DaemonState) -> bool {
        let token = state
            .raw
            .get("token")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        !token.is_empty()
            && state.endpoint != "unresolved://opencode"
            && serve_healthy(&state.endpoint, token)
    }

    fn boot(&self) -> Result<crate::harness_daemon::DaemonState, String> {
        let config = serve_config_path(self.home);
        if let Some(parent) = config.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir for serve config: {e}"))?;
        }
        std::fs::write(&config, serve_config_json("opencode"))
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
                        let incarnation = pid_start
                            .map(|start| format!("{pid}:{start}"))
                            .unwrap_or_else(|| base_url.clone());
                        return crate::harness_daemon::DaemonState::with_optional_process_identity(
                            record,
                            Some(pid),
                            pid_start,
                            base_url,
                            incarnation,
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

/// The steering-writer argv: `opencode run --attach <serve> --session <sid>`
/// with the one-shot lane's own tail builder. Launches ride the contract
/// template instead (`[harness.opencode.launch]`); this argv stays for the
/// ask path, which feeds an EXISTING session. No `--format json`: the attach
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

/// Start one detached attach writer for an existing serve session. Launch and
/// steering both use this path so command templating, permissions, logging,
/// and process-group setup cannot drift between the two entry points.
fn spawn_attach_writer(
    serve: &ServeHandle,
    session_id: &str,
    message: &str,
    model: Option<&str>,
    cwd: &Path,
    log_path: &Path,
    opencode_bin: &str,
    name: &str,
) -> Result<u32, String> {
    let mut argv = writer_argv(serve, session_id, message, model);
    argv[0] = opencode_bin.to_string();
    let argv = crate::spawn_gate::qos_wrap(cwd, argv);
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null());
    if let Ok(fh) = out {
        cmd.stdout(Stdio::from(fh));
        if let Ok(err_fh) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            cmd.stderr(Stdio::from(err_fh));
        }
    }
    cmd.current_dir(cwd);
    // opencode resolves its project from $PWD, not the process cwd.
    cmd.env("PWD", cwd);
    crate::claims::stamp_command_env(&mut cmd, Some(name), "opencode", Some(session_id));
    cmd.env_remove("NO_COLOR");
    cmd.env_remove("FORCE_COLOR");
    cmd.env("OPENCODE_SERVER_PASSWORD", &serve.token);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .map(|child| {
            let pid = child.id();
            std::mem::forget(child);
            pid
        })
        .map_err(|e| e.to_string())
}

/// Orchestrate one `spawn --harness opencode --substrate bg`: validate,
/// fail-closed registry + collision check, ensure the shared serve, then exec
/// the lane's native line from the contract template. opencode creates its
/// own session (the title names it); the launch record goes first and the
/// row binds at the worker's session report. Fire-and-forget by design: the
/// turn runs on the serve; the launched process only drives it.
pub fn dispatch_opencode_serve(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    model: Option<&str>,
    effort: Option<&str>,
    node: Option<&str>,
) -> AskOutcome {
    dispatch_opencode_serve_inner(
        home, name, message, from_name, cwd, model, effort, node, "opencode",
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
    effort: Option<&str>,
    node: Option<&str>,
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

    if let Some(effort) = effort {
        let selected_model = model
            .filter(|model| !model.is_empty())
            .unwrap_or(OPENCODE_DEFAULT_MODEL);
        if let Err(error) = apply_opencode_variant(selected_model, effort) {
            return AskOutcome::err(error, 2);
        }
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

    // The lane's native line, from the contract template: opencode creates
    // its own session (the title names it) and the turn runs on the serve.
    // The row binds at the worker's session report, the same posture the
    // claude native lane holds, so the launch record goes first and the
    // spawn-side registry write and birth emit stay gone.
    let effective_message = crate::provider::render_verb_seed(
        if message.is_empty() { "hello" } else { message },
        "opencode",
    );
    let full_prompt = crate::opencode_ask::opencode_envelope(&effective_message, from_name);
    let template = match crate::spawn_compose::resolve_template("opencode", None, None) {
        Ok(template) => template,
        Err(error) => return AskOutcome::err(error, 2),
    };
    let model_value = model
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .or_else(|| {
            effort
                .filter(|e| !e.is_empty())
                .map(|_| OPENCODE_DEFAULT_MODEL.to_string())
        })
        .unwrap_or_default();
    let ask = serde_json::json!({
        "name": name,
        "payload": full_prompt,
        "model": model_value,
        "serve_url": serve.base_url,
    });
    let values = crate::spawn_compose::fill_values(&ask);
    let mut argv = match crate::spawn_compose::fill_launch(&template, &values) {
        Ok(argv) => argv,
        Err(error) => return AskOutcome::err(error, 2),
    };
    if argv.is_empty() {
        return AskOutcome::err("the filled argv is empty".to_string(), 2);
    }
    // {payload} expands through opencode_run_tail, not the plain hole fill: a
    // slash seed rides --command with args split, prose stays positional. The
    // fill's trailing run - value, plus the "--" fence - gives way to it.
    let mut cut = 1;
    if argv.len() >= 2 && argv[argv.len() - 1] == full_prompt && argv[argv.len() - 2] == "--" {
        cut = 2;
    }
    argv.truncate(argv.len() - cut);
    argv.extend(crate::provider::opencode_run_tail(&full_prompt));
    argv[0] = opencode_bin.to_string();

    // The record first: the launch reservation and the parent edge the adopt
    // names the worker from, so a session that never reports still holds the
    // lane until its window lapses.
    let lineage = crate::spawn_lineage::ambient_lineage();
    if let Err(error) = crate::launch_record::write_record(
        home.root(),
        crate::launch_record::Launch {
            harness: "opencode".to_string(),
            name: Some(name.to_string()),
            node: node.filter(|v| !v.is_empty()).map(str::to_string),
            slot: None,
            provider: Some("opencode".to_string()),
            cwd: cwd.to_string_lossy().into_owned(),
        },
        lineage.session.as_deref().unwrap_or(""),
        lineage.harness.as_deref(),
        lineage.cwd.as_deref().unwrap_or(""),
        crate::claims::now_ms(),
    ) {
        return AskOutcome::err(format!("launch record: {error}"), 12);
    }

    // The detached exec: one turn's writer, the same log and detach mechanics
    // the steering writer uses, over the template's argv.
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let log_path = home
        .root()
        .join("agents")
        .join("logs")
        .join(format!("{name}.jsonl"));
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null());
    if let Ok(fh) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
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
    // opencode resolves its project from $PWD, not the process cwd.
    cmd.env("PWD", cwd);
    crate::claims::stamp_command_env(&mut cmd, Some(name), "opencode", None);
    cmd.env_remove("NO_COLOR");
    cmd.env_remove("FORCE_COLOR");
    cmd.env("OPENCODE_SERVER_PASSWORD", &serve.token);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Err(error) = cmd.spawn() {
        return AskOutcome::err(format!("opencode launch: {error}"), 2);
    }

    AskOutcome::ok_reply(
        serde_json::json!({
            "ok": true,
            "name": name,
            "serve_url": serve.base_url,
            "session_id": "",
            "short_id": "",
            // The generated serve config writes `{"permission":{"*":"allow"}}`;
            // the receipt NAMES that posture instead of leaving it unnamed.
            "permission_posture": "explicit allow-all (serve config permission.{*}=allow)",
            "durability": "daemon-owned",
            "incarnation": format!("{}:{}", serve.pid, serve.pid_start.unwrap_or_default()),
            "endpoint": serve.base_url,
            "log_path": log_path.to_string_lossy(),
            // No session id at launch: opencode mints it and the worker's
            // session report binds the row.
            "binding": "session-report",
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

/// Read liveness for a serve row from the serve's session endpoint. A 404 is
/// a definitive absent session; an unreadable state file, unreachable server,
/// or malformed response is UNKNOWN and must not be converted to dead.
pub fn serve_session_reachable(session_id: &str, timeout: Duration) -> Result<bool, String> {
    serve_session_reachable_in(&AgentsHome::from_env(), session_id, timeout)
}

fn serve_session_reachable_in(
    home: &AgentsHome,
    session_id: &str,
    timeout: Duration,
) -> Result<bool, String> {
    if !crate::provider::is_opencode_session_id(session_id) {
        return Err(format!("malformed opencode session id {session_id:?}"));
    }
    let raw = std::fs::read_to_string(serve_state_path(&home))
        .map_err(|e| format!("serve state unreadable: {e}"))?;
    let state: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("serve state parse: {e}"))?;
    let base_url = state
        .get("base_url")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "serve state has no base_url".to_string())?;
    let token = state
        .get("token")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "serve state has no token".to_string())?;
    let (status, body) = http_json(
        base_url,
        "GET",
        &format!("/session/{session_id}"),
        None,
        Some(token),
        timeout,
    )?;
    if status == 404 {
        return Ok(false);
    }
    if status != 200 {
        return Err(format!(
            "GET session answered {status}: {}",
            tail_reason(&body, status)
        ));
    }
    let session: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("session body parse: {e}"))?;
    if session.get("id").and_then(|value| value.as_str()) == Some(session_id) {
        Ok(true)
    } else {
        Err(format!("session readback id does not match {session_id}"))
    }
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

/// Send one turn to an already-minted serve session and confirm it by a new
/// message id in the serve readback. The writer's exit is not delivery proof.
pub fn steer_existing_session(
    home: &AgentsHome,
    session_id: &str,
    message: &str,
    cwd: &Path,
    model: Option<&str>,
    timeout: Duration,
) -> Result<String, String> {
    steer_existing_session_with_binary(home, session_id, message, cwd, model, timeout, "opencode")
}

fn steer_existing_session_with_binary(
    home: &AgentsHome,
    session_id: &str,
    message: &str,
    cwd: &Path,
    model: Option<&str>,
    timeout: Duration,
    opencode_bin: &str,
) -> Result<String, String> {
    if !crate::provider::is_opencode_session_id(session_id) {
        return Err(format!(
            "opencode serve steering refused session {session_id:?}: expected a full ses_<alnum> id"
        ));
    }
    let serve = ensure_serve(home)?;
    let before = fetch_messages(&serve.base_url, &serve.token, session_id)?;
    let log_path = home
        .root()
        .join("agents")
        .join("logs")
        .join(format!("steer-{session_id}.jsonl"));
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("steering log directory: {e}"))?;
    }
    spawn_attach_writer(
        &serve,
        session_id,
        message,
        model,
        cwd,
        &log_path,
        opencode_bin,
        "fno-opencode-steer",
    )
    .map_err(|error| {
        format!("opencode serve steering writer launch failed for session {session_id}: {error}")
    })?;

    let deadline = std::time::Instant::now() + timeout;
    let mut last_seen = latest_message_id(&before);
    while std::time::Instant::now() < deadline {
        if let Ok(messages) = fetch_messages(&serve.base_url, &serve.token, session_id) {
            if let Some(id) = new_message_id_after(&messages, &before) {
                return Ok(id);
            }
            last_seen = latest_message_id(&messages);
        }
        std::thread::sleep(STEER_POLL_INTERVAL);
    }
    Err(format!(
        "opencode serve steering UNKNOWN for session {session_id}: no new message within {}s; last message id {}",
        timeout.as_secs(),
        last_seen.as_deref().unwrap_or("<none>")
    ))
}

/// Resolve a full opencode serve row and steer it. This is the shared mail
/// entry point; callers cannot supply a URL or cwd that disagrees with the row.
pub fn steer_registered_session(
    session_id: &str,
    message: &str,
    timeout: Duration,
) -> Result<String, String> {
    let home = AgentsHome::from_env();
    let registry =
        load_registry(&home.registry_json()).map_err(|e| format!("registry read failed: {e}"))?;
    let entry = registry
        .entries
        .iter()
        .find(|entry| {
            entry.harness_name() == "opencode"
                && (entry.harness_session_id.as_deref() == Some(session_id)
                    || entry.session_id.as_deref() == Some(session_id))
        })
        .ok_or_else(|| {
            format!("opencode serve steering refused: no row for session {session_id}")
        })?;
    if entry.substrate.as_deref() != Some("thread") {
        return Err(format!(
            "opencode serve steering refused session {session_id}: row is not a serve thread"
        ));
    }
    let cwd = Path::new(&entry.cwd);
    if !cwd.is_dir() {
        return Err(format!(
            "opencode serve steering refused session {session_id}: cwd {} is not a directory",
            cwd.display()
        ));
    }
    steer_existing_session(
        &home,
        session_id,
        message,
        cwd,
        entry.model.as_deref(),
        timeout,
    )
}

/// Handle `fno agents ask` for a serve row. Steering confirms the new stored
/// message, then this wrapper waits for a new assistant message and returns its
/// text so ask callers receive the reply rather than the writer receipt.
pub fn ask_registered_session(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    model: Option<&str>,
    timeout: Duration,
) -> AskOutcome {
    let registry = match load_registry(&home.registry_json()) {
        Ok(registry) => registry,
        Err(error) => return AskOutcome::err(format!("registry read failed: {error}"), 12),
    };
    let Some(entry) = registry.find_name_or_full_session_id(name) else {
        return AskOutcome::err(format!("agent {name:?} has no registry row"), 13);
    };
    if entry.harness_name() != "opencode" || entry.substrate.as_deref() != Some("thread") {
        return AskOutcome::err(format!("agent {name:?} is not an opencode serve thread"), 2);
    }
    let Some(session_id) = entry
        .harness_session_id
        .as_deref()
        .or(entry.session_id.as_deref())
    else {
        return AskOutcome::err(
            format!("agent {name:?} is an opencode serve row with no full session id"),
            2,
        );
    };
    let cwd = Path::new(&entry.cwd);
    if !cwd.is_dir() {
        return AskOutcome::err(
            format!(
                "agent {name:?} recorded cwd {} is not a directory",
                cwd.display()
            ),
            13,
        );
    }
    let rendered = crate::provider::render_verb_seed(message, "opencode");
    let full_prompt = crate::opencode_ask::opencode_envelope(&rendered, from_name);
    let serve = match ensure_serve(home) {
        Ok(serve) => serve,
        Err(error) => return AskOutcome::err(error, 13),
    };
    let before = match fetch_messages(&serve.base_url, &serve.token, session_id) {
        Ok(messages) => messages,
        Err(error) => return AskOutcome::err(error, 12),
    };
    if let Err(error) = steer_existing_session(
        home,
        session_id,
        &full_prompt,
        cwd,
        model.or(entry.model.as_deref()),
        timeout,
    ) {
        return AskOutcome::err(error, 12);
    }
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(messages) = fetch_messages(&serve.base_url, &serve.token, session_id) {
            if let Some(reply) = new_assistant_text_after(&messages, &before) {
                return AskOutcome::ok_reply(reply);
            }
        }
        std::thread::sleep(STEER_POLL_INTERVAL);
    }
    AskOutcome::err(
        format!(
            "opencode serve ask UNKNOWN for session {session_id}: no new assistant reply within {}s",
            timeout.as_secs()
        ),
        12,
    )
}

fn latest_message_id(messages: &serde_json::Value) -> Option<String> {
    messages
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("info"))
        .filter_map(|info| info.get("id"))
        .filter_map(|id| id.as_str())
        .last()
        .map(str::to_string)
}

fn new_assistant_text_after(
    messages: &serde_json::Value,
    high_water: &serde_json::Value,
) -> Option<String> {
    let prior = high_water
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("info"))
        .filter_map(|info| info.get("id"))
        .filter_map(|id| id.as_str())
        .collect::<std::collections::HashSet<_>>();
    messages
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| {
            message
                .get("info")
                .and_then(|info| info.get("role"))
                .and_then(|role| role.as_str())
                == Some("assistant")
        })
        .filter(|message| {
            message
                .get("info")
                .and_then(|info| info.get("id"))
                .and_then(|id| id.as_str())
                .is_some_and(|id| !prior.contains(id))
        })
        .filter_map(|message| message.get("parts"))
        .filter_map(|parts| parts.as_array())
        .flat_map(|parts| parts.iter())
        .filter(|part| part.get("type").and_then(|kind| kind.as_str()) == Some("text"))
        .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
        .map(str::to_string)
        .next()
}

/// Return the first message id that was not present at the caller's
/// high-water mark. A non-empty response is not delivery evidence: the id is
/// the serve's positive marker for a newly stored message.
fn new_message_id_after(
    messages: &serde_json::Value,
    high_water: &serde_json::Value,
) -> Option<String> {
    let prior = high_water
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("info"))
        .filter_map(|info| info.get("id"))
        .filter_map(|id| id.as_str())
        .collect::<std::collections::HashSet<_>>();
    messages
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("info"))
        .filter_map(|info| info.get("id"))
        .filter_map(|id| id.as_str())
        .find(|id| !prior.contains(id))
        .map(str::to_string)
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

// ===========================================================================
// The archive op: a history-preserving active-surface removal
// ===========================================================================

/// The opencode version the archive op was measured against (v1.14.50,
/// 2026-09-08). `PATCH /session/{id}` with `{"time":{"archived":<epoch_ms>}}`
/// answers 200, a separate GET still returns the session with the field set
/// and its messages intact, and `session.list` drops it from the active
/// listing. Below this version the field is UNVERIFIED, not known-absent, so a
/// caller skips instead of guessing: an older serve that ignored the field
/// would turn a retirement that works today into one that holds its row
/// forever. Above [`OPENCODE_V2_FLOOR`] the argument binds upward: the 2.x
/// HTTP API is a different contract, and an unverified version is skipped,
/// not guessed at.
pub const ARCHIVE_MIN_VERSION: (u32, u32, u32) = (1, 14, 50);

/// First opencode 2.x release. The archive op was measured against the 1.x
/// HTTP API only, so a serve at or above this version is skipped and told why.
pub const OPENCODE_V2_FLOOR: (u32, u32, u32) = (2, 0, 0);

/// What one archive attempt measured. `Survived` is an outcome, not an error:
/// the write was accepted and the stored record still carries no
/// `time.archived`, which is a positive finding about the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveOutcome {
    Archived,
    AlreadyArchived,
    Gone,
    Survived,
}

/// Compare the leading three dotted integers of `reported` against `min`.
/// Anything before the first digit is skipped (`opencode v2.0.19`, bare
/// `1.18.33`); an unparseable version answers false: a version nobody can
/// read is not evidence of a supported one.
pub(crate) fn version_at_least(reported: &str, min: (u32, u32, u32)) -> bool {
    let trimmed = reported.trim();
    let from_digit = match trimmed.find(|c: char| c.is_ascii_digit()) {
        Some(i) => &trimmed[i..],
        None => "",
    };
    let nums: Vec<u32> = from_digit
        .split('.')
        .take(3)
        .map(|part| {
            part.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
        })
        .filter_map(|digits| digits.parse::<u32>().ok())
        .collect();
    nums.len() == 3 && (nums[0], nums[1], nums[2]) >= min
}

fn is_archived(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("time")
                .and_then(|time| time.get("archived"))
                .cloned()
        })
        .is_some_and(|value| !value.is_null())
}

/// The recorded serve, when it answers health AND reports a version carrying
/// the archive op. Returns `None` for every other case, including a missing
/// state file and an unreadable one.
///
/// It never boots a serve: a GC sweep must not start a server as a side
/// effect. One health read answers both questions, because the health body
/// carries the version, and one read is the right budget here - a false
/// "unhealthy" only skips the archive, which is what the caller did before
/// this op existed. The three-attempt [`serve_healthy`] belongs to the spawn
/// path, where the unhealthy branch SIGTERMs the serve.
pub fn archive_capable_serve(home: &AgentsHome) -> Option<ServeHandle> {
    let raw = std::fs::read_to_string(serve_state_path(home)).ok()?;
    let state: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let base_url = state.get("base_url")?.as_str()?.to_string();
    let token = state.get("token")?.as_str()?.to_string();
    if base_url.is_empty() || token.is_empty() {
        return None;
    }
    let (status, body) = http_json(
        &base_url,
        "GET",
        "/global/health",
        None,
        Some(&token),
        HEALTH_TIMEOUT,
    )
    .ok()?;
    if status != 200 {
        return None;
    }
    let health: serde_json::Value = serde_json::from_str(&body).ok()?;
    if health.get("healthy") != Some(&serde_json::Value::Bool(true)) {
        return None;
    }
    let version = health.get("version")?.as_str()?.to_string();
    if !version_at_least(&version, ARCHIVE_MIN_VERSION) {
        return None;
    }
    if version_at_least(&version, OPENCODE_V2_FLOOR) {
        eprintln!(
            "opencode serve reports {version}; the archive op is measured on 1.14.50 up to but not including 2.0.0, so it is skipped"
        );
        return None;
    }
    Some(ServeHandle {
        base_url,
        token,
        pid: state.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        pid_start: state
            .get("pid_start")
            .and_then(|v| v.as_u64())
            .filter(|start| *start > 0),
    })
}

/// Archive one session: stamp `time.archived` and read the STORED record back.
///
/// The PATCH response body is the server's own projection of a write it has
/// not necessarily persisted, so the second GET is the positive marker. An
/// `Err` here means the attempt could not be judged; a write that was accepted
/// and left the session unarchived answers [`ArchiveOutcome::Survived`].
pub fn archive_session(
    base_url: &str,
    token: &str,
    session_id: &str,
) -> Result<ArchiveOutcome, String> {
    let path = format!("/session/{session_id}");
    let (status, body) = http_json(base_url, "GET", &path, None, Some(token), HTTP_CALL_TIMEOUT)?;
    if status == 404 {
        return Ok(ArchiveOutcome::Gone);
    }
    if status != 200 {
        return Err(format!(
            "GET session answered {status}: {}",
            tail_reason(&body, status)
        ));
    }
    if is_archived(&body) {
        return Ok(ArchiveOutcome::AlreadyArchived);
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|e| format!("clock before the epoch: {e}"))?;
    let (status, body) = http_json(
        base_url,
        "PATCH",
        &path,
        Some(&serde_json::json!({"time": {"archived": now_ms}})),
        Some(token),
        HTTP_CALL_TIMEOUT,
    )?;
    if status != 200 && status != 204 {
        return Err(format!(
            "PATCH archive answered {status}: {}",
            tail_reason(&body, status)
        ));
    }
    let (status, body) = http_json(base_url, "GET", &path, None, Some(token), HTTP_CALL_TIMEOUT)?;
    if status != 200 {
        return Err(format!(
            "archive readback answered {status}: {}",
            tail_reason(&body, status)
        ));
    }
    if is_archived(&body) {
        Ok(ArchiveOutcome::Archived)
    } else {
        Ok(ArchiveOutcome::Survived)
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
    fn serve_config_follows_the_contract() {
        let base =
            std::env::temp_dir().join(format!("fno-ocserve-contract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let stub = |name: &str, version: &str| {
            crate::write_exec_stub(&base, name, &format!("#!/bin/sh\necho '{version}'\n"))
        };
        let v1 = stub("oc-v1", "1.18.33");
        let v2 = stub("oc-v2", "2.0.19");
        // The serve config is byte-identical to the shipped 1.x and 2.x
        // postures: the allow-all rows ride the config, never a per-session
        // PATCH (the launch mints no session to patch).
        assert_eq!(
            serve_config_json(v1.to_str().unwrap()),
            SERVE_CONFIG_JSON_V1
        );
        assert_eq!(
            serve_config_json(v2.to_str().unwrap()),
            SERVE_CONFIG_JSON_V2
        );
    }

    #[test]
    fn writer_argv_is_attach_session_bypass_then_tail() {
        let serve = ServeHandle {
            base_url: "http://127.0.0.1:59971".to_string(),
            token: "test-token".to_string(),
            pid: 1,
            pid_start: None,
        };
        // Slash command rides --command. No --format json: the attach
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
    fn steering_readback_requires_a_message_id_after_high_water_mark() {
        let before = serde_json::json!([
            {"info": {"id": "msg_old"}, "parts": []}
        ]);
        let after = serde_json::json!([
            {"info": {"id": "msg_old"}, "parts": []},
            {"info": {"id": "msg_new"}, "parts": []}
        ]);

        assert_eq!(
            new_message_id_after(&after, &before).as_deref(),
            Some("msg_new")
        );
        assert_eq!(new_message_id_after(&before, &before), None);
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
                    } else if line.starts_with("GET /session/") {
                        ("200 OK", format!("{{\"id\":\"{sid}\"}}"))
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
    fn serve_session_reachability_uses_session_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        let fake = FakeServe::start("ses_reachable123");
        std::fs::write(
            serve_state_path(&h),
            serde_json::json!({
                "base_url": fake.base_url(),
                "token": "test-token"
            })
            .to_string(),
        )
        .unwrap();

        assert!(
            serve_session_reachable_in(&h, "ses_reachable123", Duration::from_secs(2)).unwrap()
        );
        assert!(fake
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|line| line == "GET /session/ses_reachable123 HTTP/1.1"));
    }

    #[test]
    fn healthy_legacy_state_without_pid_start_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        let fake = FakeServe::start("ses_legacy_state1");
        let record = serde_json::json!({
            "base_url": fake.base_url(),
            "pid": 4242,
            "pid_start": null,
            "token": "test-token",
        });
        std::fs::write(serve_state_path(&h), record.to_string()).unwrap();

        let serve =
            ensure_serve(&h).expect("a positive health probe outranks a missing reap token");

        assert_eq!(serve.base_url, fake.base_url());
        assert_eq!(serve.token, "test-token");
        assert_eq!(serve.pid_start, None);
    }

    #[test]
    fn unreadable_opencode_state_does_not_authorize_a_replacement() {
        use crate::harness_daemon::HarnessDaemonAdapter;

        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        let adapter = OpenCodeDaemonAdapter::new(&h);

        assert!(!adapter.unreadable_state_may_boot());
    }

    #[test]
    fn dispatch_launches_the_template_line_and_writes_the_record() {
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

        // A stub launch binary that records its argv beside the worker cwd:
        // the seam points argv[0] at it, so no real `opencode` runs.
        let stub = crate::write_exec_stub(
            dir.path(),
            "opencode-stub",
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > argv.txt\n",
        );

        let cwd = dir.path().join("w");
        std::fs::create_dir_all(&cwd).unwrap();
        let outcome = dispatch_opencode_serve_inner(
            &h,
            "wk-serve",
            "hello there",
            "op",
            &cwd,
            None,
            None,
            Some("x-aaaa"),
            &stub.to_string_lossy(),
        );

        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        let receipt: serde_json::Value = serde_json::from_str(outcome.stdout.trim()).unwrap();
        assert_eq!(receipt["ok"], true);
        assert_eq!(receipt["binding"], "session-report");
        assert_eq!(
            receipt["session_id"], "",
            "opencode mints the id; the report binds it"
        );
        // No row at launch: the adopt owns it.
        let reg = load_registry(&h.registry_json()).unwrap();
        assert!(
            reg.find("wk-serve").is_none(),
            "the spawn writes no registry row"
        );
        // The launch record is the reservation and the adopt's parent edge.
        let pending = crate::launch_record::pending_for_provider(
            h.root(),
            "opencode",
            crate::claims::now_ms(),
        );
        assert_eq!(
            pending,
            vec!["wk-serve".to_string()],
            "the record holds the lane"
        );
        // The exec was the filled contract template line: opencode creates
        // its own session, so the fake saw no session create and the argv
        // carries the title, not a pre-minted id.
        let requests = fake.requests.lock().unwrap();
        assert!(
            !requests.iter().any(|r| r.starts_with("POST /session")),
            "opencode mints its own session; saw {:?}",
            *requests
        );
        drop(requests);
        let launched = std::fs::read_to_string(cwd.join("argv.txt")).unwrap();
        let lines: Vec<&str> = launched.lines().collect();
        let stub_str = stub.to_string_lossy().into_owned();
        assert_eq!(
            &lines[..6],
            &[
                stub_str.as_str(),
                "run",
                "--attach",
                fake.base_url().as_str(),
                "--title",
                "wk-serve",
            ][..],
            "the template line, filled: {launched}"
        );
        assert!(
            lines.contains(&"--dangerously-skip-permissions"),
            "the serve posture rides the line: {launched}"
        );
        assert!(
            !lines.iter().any(|t| *t == "--session"),
            "no pre-minted session rides the launch: {launched}"
        );
        assert!(
            !lines.iter().any(|t| *t == "--command"),
            "the payload rides the tail, never one --command blob (/ codex P1): {launched}"
        );
        assert_eq!(
            lines[lines.len() - 2],
            "--",
            "prose stays the message positional behind the fence: {launched}"
        );
        assert!(
            lines.last().unwrap().contains("hello there"),
            "the payload fills the tail: {launched}"
        );
    }

    #[test]
    fn dispatch_refuses_name_collision() {
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
            None,
            None,
            "opencode",
        );
        assert_eq!(out.exit_code, 2);
    }
}

#[cfg(test)]
#[path = "opencode_archive_tests.rs"]
mod opencode_archive_tests;
