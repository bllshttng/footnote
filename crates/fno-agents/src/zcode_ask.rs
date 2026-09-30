//! zcode's headless lane: a one-shot `-p` create whose minted `sess_<uuid>`
//! becomes a persistent registry identity, and `ask` resumes by name with
//! `--resume`. Measured 2026-09-29 (ZCode.app 3.14.3): create and resume both
//! run one turn per process; the stream-json event stream tees to the
//! worker's log, so a run is watchable live and steerable between turns via
//! ask. The interactive TUI is unbuildable on that install (no `@zcode/tui`
//! beside zcode.cjs), so there is no keeper child and no pane lane; the row
//! is an identity row, not a liveness claim.

use std::io::{BufRead, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::claude_ask::py_repr;
use crate::claude_ask::AskOutcome;
use crate::state::{load_registry, update_registry, RegistryEntry};

/// The ask/create turn's ceiling when the caller passes none. A seed turn of
/// a real task outlives this only when the caller names a longer --timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

impl AskOutcome {
    fn error(msg: impl Into<String>, code: i32) -> Self {
        Self {
            stdout: String::new(),
            stderr: format!("{}\n", msg.into()),
            exit_code: code,
        }
    }
    fn reply(text: String) -> Self {
        Self {
            stdout: format!("{text}\n"),
            stderr: String::new(),
            exit_code: 0,
        }
    }
}

/// The headless turn's argv. `--mode yolo` is zcode's own headless default
/// and the bypass carrier (run.ts DEFAULT_HEADLESS_PROMPT_MODE); an explicit
/// --permission-mode replaces it (the gate admits row-mapped modes, and the
/// lane honors them rather than silently downgrading). `--output-format
/// stream-json` makes every event land in the log as it happens - the live
/// view the lane exists to provide. Passthrough args ride last, after every
/// fno-owned flag; the identity and posture flags are REFUSED so passthrough
/// can never swap the session id or demote the posture (grok's fence, same
/// reason).
fn build_argv(
    session: Option<&str>,
    prompt: &str,
    permission_mode: Option<&str>,
    yolo: bool,
    harness_args: &[String],
) -> Result<Vec<String>, String> {
    const FORBIDDEN: [&str; 7] = [
        "-p",
        "--prompt",
        "--resume",
        "--mode",
        "--output-format",
        "-m",
        "--model",
    ];
    for flag in harness_args {
        // Match the bare token too, so --mode=plan trips the same refusal as
        // a bare --mode: the owner check is on the axis, not the spelling.
        let bare = flag.split('=').next().unwrap_or(flag);
        if FORBIDDEN.contains(&bare) {
            return Err(format!(
                "zcode headless owns {bare}; passthrough may not set it"
            ));
        }
    }
    let mode = match permission_mode.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) if !yolo => m.to_string(),
        _ => "yolo".to_string(),
    };
    let mut argv = vec![
        "zcode".to_string(),
        "-p".to_string(),
        prompt.to_string(),
        "--mode".to_string(),
        mode,
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];
    if let Some(session_id) = session {
        argv.push("--resume".to_string());
        argv.push(session_id.to_string());
    }
    argv.extend(harness_args.iter().cloned());
    Ok(argv)
}

/// Drive one `zcode -p` turn: stdout streams line by line into the worker's
/// log (the live view) while the full text is captured for the parser;
/// stderr is captured for classification. Mirrors run_agy's tee and grok's
/// reap/grace ordering.
fn run_turn(
    argv: &[String],
    log_path: &Path,
    cwd: &Path,
    timeout: Option<Duration>,
    name: &str,
    session: Option<&str>,
) -> Result<crate::zcode::ZcodeTurn, (String, i32)> {
    use std::process::Stdio;

    let tee_fh = crate::subprocess_ask::open_tee(log_path)
        .map_err(|e| (format!("cannot open zcode output tee: {e}"), 2))?;
    let tee = std::sync::Arc::new(std::sync::Mutex::new(tee_fh));

    let argv = crate::spawn_gate::qos_wrap(cwd, argv.to_vec());
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // Detached stdin: a headless turn never reads input.
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.current_dir(cwd);
    crate::claims::stamp_command_env(&mut cmd, Some(name), "zcode", session);
    // Own process group so SIGTERM/SIGKILL/SIGINT reach zcode's children.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let mut child =
        match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err((
                "zcode binary not found on PATH; the one-time setup puts a zcode launcher on PATH"
                    .to_string(),
                13,
            )),
            Err(e) => return Err((format!("OSError invoking zcode: {e}"), 2)),
        };

    let pid = child.id();
    let _sigint_guard = crate::subprocess_ask::SigintForwarder::install(pid);
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");

    // stdout: tee every line as it arrives (live view), capture for parse.
    let tee_stdout = tee.clone();
    let stdout_handle = std::thread::spawn(move || {
        let mut captured = String::new();
        let mut reader = std::io::BufReader::new(stdout_pipe);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    captured.push_str(&line);
                    if let Ok(mut guard) = tee_stdout.lock() {
                        if guard.write_all(line.as_bytes()).is_ok() {
                            let _ = guard.flush();
                        }
                    }
                }
            }
        }
        captured
    });
    // stderr: captured for classification (bounded), never mixed into stdout.
    let stderr_handle = std::thread::spawn(move || {
        let mut text = String::new();
        let mut bounded = std::io::BufReader::new(stderr_pipe)
            .take((crate::zcode::TURN_STDERR_TAIL_CAP as u64) + 1);
        let _ = bounded.read_to_string(&mut text);
        text
    });

    let mut watchdog =
        crate::subprocess_ask::AskWatchdog::spawn(pid, Some(timeout.unwrap_or(DEFAULT_TIMEOUT)));
    let stdout_text = stdout_handle.join().unwrap_or_default();
    watchdog.cancel();
    let (exit_code, sigkill_escalated) =
        crate::subprocess_ask::wait_with_grace(pid, &mut child, 5.0);
    watchdog.join();
    let stderr_text = stderr_handle.join().unwrap_or_default();
    let was_timed_out = watchdog.timed_out() || sigkill_escalated;

    if crate::subprocess_ask::ask_interrupted() {
        return Err(("interrupted".to_string(), 130));
    }
    if was_timed_out {
        return Err(("zcode turn timed out".to_string(), 12));
    }
    crate::zcode::parse_turn_output(exit_code == 0, Some(exit_code), &stdout_text, &stderr_text)
        .map_err(|e| (e, 2))
}

fn derive_log_path(home: &AgentsHome, name: &str) -> PathBuf {
    home.root()
        .join("agents")
        .join("logs")
        .join(format!("{name}.jsonl"))
}

/// Orchestrate one zcode `spawn --substrate headless`: validate, collision
/// check, run the create turn, and bind the minted session id into a registry
/// row. The row is an IDENTITY row: the turn's process exits when the turn
/// ends, so the row reads Exited (retained until rm) - `ask` resumes the
/// zcode session by name, the log keeps the live record, and the reaper
/// retires a done worker like any other.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_zcode_once(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    yolo: bool,
    permission_mode: Option<&str>,
    timeout: Option<Duration>,
    harness_args: &[String],
    node: Option<&str>,
) -> AskOutcome {
    if let Err(msg) = crate::claude_ask::validate_spawn_inputs(name, from_name) {
        return AskOutcome::error(msg, 2);
    }
    let events = home.events_jsonl();
    let registry = match load_registry(&home.registry_json()) {
        Ok(registry) => registry,
        Err(error) => return AskOutcome::error(format!("registry read failed: {error}"), 12),
    };
    if registry.find(name).is_some() {
        return AskOutcome::error(
            format!(
                "agent {} already exists; use 'fno agents rm {}' first or pick another name",
                py_repr(name),
                name
            ),
            2,
        );
    }

    let prompt = if message.is_empty() { "hello" } else { message };
    let log_path = derive_log_path(home, name);
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let argv = match build_argv(None, prompt, permission_mode, yolo, harness_args) {
        Ok(argv) => argv,
        Err(error) => return AskOutcome::error(error, 2),
    };
    let turn = match run_turn(&argv, &log_path, cwd, timeout, name, None) {
        Ok(turn) => turn,
        Err((error, code)) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "zcode-once".into()),
                    ("name", name.into()),
                    ("provider", "zcode".into()),
                    ("error", error.clone().into()),
                ],
            );
            return AskOutcome::error(error, code);
        }
    };
    let session_id = turn
        .session_id
        .expect("run_turn refuses a turn without a usable session id");

    // Identity row. Written after the turn because the id is read back from
    // the turn's own stream (callee-minted); the process is already gone, so
    // the row is born Exited and carries no pid.
    let spawned_by = crate::spawn_lineage::ambient_lineage();
    let new_entry = RegistryEntry {
        route_provider_id: None,
        model_name: None,
        account_record_id: Some("default".to_string()),
        node: node.filter(|v| !v.is_empty()).map(str::to_string),
        substrate: Some("headless".into()),
        name: name.to_string(),
        short_id: session_id.clone(),
        legacy_provider: String::new(),
        launch_account: None,
        related_session_id: None,
        provider: Some("zcode".to_string()),
        model: None,
        model_basis: None,
        effort: None,
        requested_model: None,
        requested_provider: None,
        requested_effort: None,
        harness: Some("zcode".to_string()),
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        cwd: cwd.to_string_lossy().to_string(),
        project_root: String::new(),
        session_id: Some(session_id.clone()),
        origin: Some("spawn".to_string()),
        spawn_trigger: None,
        legacy_claude_short_id: None,
        claude_session_uuid: None,
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        host_mode: None,
        cc_session_id: None,
        status: crate::AgentStatus::Exited,
        last_message_at: None,
        created_at: crate::daemon::now_rfc3339_like(),
        pid: None,
        pid_start_time: None,
        keeper_child_pid: None,
        log_path: Some(log_path.to_string_lossy().to_string()),
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: Some(session_id.clone()),
        delivery_policy: None,
        sandbox_posture: None,
        git_grant: None,
        ..RegistryEntry::new(Some(session_id.clone()), spawned_by)
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
            // A concurrent spawn took the name mid-turn; the row wins, this
            // turn's output already landed in its own log.
            return AskOutcome::error(
                format!("agent {name} already exists (registered while the turn ran)"),
                2,
            );
        }
        Err(error) => {
            return AskOutcome::error(format!("registry write failed: {error}"), 12);
        }
    }

    emit_event(
        &events,
        "agent_ask_done",
        &[
            ("stage", "dispatch".into()),
            ("name", name.into()),
            ("provider", "zcode".into()),
            ("session_id", session_id.clone().into()),
            ("posture", "yolo".into()),
        ],
    );
    let reply = turn.reply.unwrap_or_else(|| {
        format!(
            "session_id={session_id} posture=yolo log={}",
            log_path.display()
        )
    });
    AskOutcome::reply(reply)
}

/// Client interceptor for the `ask` verb on a zcode row: resume by name. The
/// turn runs fresh (`-p --resume <sess>`), tees into the same log, and the
/// reply prints. Returns `None` for non-zcode targets (fall through).
pub fn maybe_run_zcode_ask(
    home: &AgentsHome,
    params: &serde_json::Value,
    name: &str,
) -> Option<i32> {
    let provider_param = params.get("provider").and_then(|v| v.as_str());
    let registry = match load_registry(&home.registry_json()) {
        Ok(registry) => registry,
        Err(e) => {
            eprintln!(
                "fno-agents: cannot read agents registry at {:?}: {}",
                home.registry_json(),
                e
            );
            return Some(12);
        }
    };
    let existing_provider = registry
        .find_name_or_full_session_id(name)
        .map(|e| e.harness_name().to_string());
    let resolved = existing_provider.as_deref().or(provider_param);
    if resolved != Some("zcode") {
        return None; // not a zcode target; fall through
    }

    let entry = match registry.find_name_or_full_session_id(name) {
        Some(entry) => entry,
        None => {
            eprintln!("fno-agents: no zcode agent {name:?} in the registry; spawn one first");
            return Some(2);
        }
    };
    let Some(session_id) = entry
        .harness_session_id
        .as_deref()
        .filter(|id| crate::zcode::is_session_id(id))
        .map(str::to_string)
    else {
        eprintln!(
            "fno-agents: zcode agent {name:?} carries no usable session id; rm and spawn it again"
        );
        return Some(2);
    };

    let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let cwd = crate::subprocess_ask::resolve_ask_cwd(params.get("cwd").and_then(|v| v.as_str()));
    let timeout = params
        .get("timeout")
        .and_then(|v| v.as_u64())
        .map(std::time::Duration::from_secs);
    let permission_mode = params.get("permission_mode").and_then(|v| v.as_str());
    let yolo = params
        .get("yolo")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let log_path = match entry.log_path.as_deref().map(PathBuf::from) {
        Some(path) => path,
        None => derive_log_path(home, name),
    };
    let argv = match build_argv(Some(&session_id), message, permission_mode, yolo, &[]) {
        Ok(argv) => argv,
        Err(error) => {
            eprintln!("fno-agents: {error}");
            return Some(2);
        }
    };
    match run_turn(&argv, &log_path, &cwd, timeout, name, Some(&session_id)) {
        Ok(turn) => {
            print!("{}", turn.reply.unwrap_or_default());
            let _ = crate::state::update_registry(&home.registry_json(), |reg| {
                if let Some(e) = reg.find_mut(name) {
                    e.last_message_at = Some(crate::daemon::now_rfc3339_like());
                    true
                } else {
                    false
                }
            });
            Some(0)
        }
        Err((error, code)) => {
            eprintln!("fno-agents: {error}");
            Some(code)
        }
    }
}

use crate::paths::AgentsHome;

fn emit_event(events_path: &Path, kind: &str, fields: &[(&str, serde_json::Value)]) {
    crate::claude_ask::emit_event(events_path, kind, fields);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_argv_carries_the_measured_shape() {
        let argv = build_argv(None, "seed", None, true, &[]).unwrap();
        assert_eq!(argv[0], "zcode");
        assert_eq!(argv[1], "-p");
        assert_eq!(argv[2], "seed");
        assert_eq!(argv[3], "--mode");
        assert_eq!(argv[4], "yolo");
        assert_eq!(argv[5], "--output-format");
        assert_eq!(argv[6], "stream-json");
        assert!(!argv.contains(&"--resume".to_string()));
    }

    #[test]
    fn resume_argv_resumes_by_session_id() {
        let argv = build_argv(
            Some("sess_60de086c-9278-4b56-addb-39445b2e6636"),
            "continue",
            None,
            true,
            &[],
        )
        .unwrap();
        let resume_at = argv
            .iter()
            .position(|a| a == "--resume")
            .expect("resume carries the flag");
        assert_eq!(
            argv[resume_at + 1],
            "sess_60de086c-9278-4b56-addb-39445b2e6636"
        );
    }

    #[test]
    fn passthrough_args_ride_last() {
        let argv = build_argv(None, "seed", None, true, &["--verbose".to_string()]).unwrap();
        assert_eq!(argv.last().map(String::as_str), Some("--verbose"));
    }

    #[test]
    fn fenced_args_cannot_replace_the_identity_or_posture() {
        // Passthrough is for tuning flags, not for swapping the identity or
        // the posture fno owns.
        for flag in ["--resume", "--mode=plan", "--output-format=json"] {
            let error = build_argv(None, "seed", None, true, &[flag.to_string()])
                .expect_err("an owned flag in passthrough refuses");
            assert!(error.contains("zcode headless owns"), "{flag}: {error}");
        }
    }
}
