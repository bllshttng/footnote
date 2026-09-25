use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::claude_ask::{emit_event, py_repr};
use crate::paths::AgentsHome;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl AskOutcome {
    fn reply(stdout: String, receipt: String) -> Self {
        Self {
            stdout,
            stderr: receipt,
            exit_code: 0,
        }
    }

    fn error(message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            stdout: String::new(),
            stderr: format!("{}\n", message.into()),
            exit_code,
        }
    }
}

fn build_argv(
    session_id: &str,
    message: &str,
    from_name: &str,
    model: Option<&str>,
    effort: Option<&str>,
    yolo: bool,
    permission_mode: Option<&str>,
    harness_args: &[String],
) -> Result<Vec<String>, String> {
    let mut argv = crate::harness_capabilities::HarnessContract::packaged()
        .and_then(|contract| {
            contract.render_session_argv("grok", "headless_create", Some(session_id))
        })
        .map_err(|error| error.to_string())?;
    argv.extend(["-m".into(), model.unwrap_or("grok-4.6").into()]);
    argv.extend(["--reasoning-effort".into(), effort.unwrap_or("high").into()]);

    if yolo {
        let bypass = crate::harness_capabilities::HarnessContract::packaged()
            .ok()
            .and_then(|contract| contract.harness.get("grok").cloned())
            .and_then(|caps| caps.keeper)
            .map(|keeper| keeper.bypass_flag)
            .filter(|flag| !flag.is_empty())
            .ok_or_else(|| "grok has no declared bypass flag".to_string())?;
        argv.push(bypass);
    } else if let Some(mode) = permission_mode {
        argv.extend(crate::codex_posture::permission_pane_tokens("grok", mode)?);
    }

    argv.extend(harness_args.iter().cloned());
    argv.extend([
        "-p".into(),
        crate::agy_ask::inject_from_name(message, from_name),
    ]);
    Ok(argv)
}

fn new_session_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| format!("could not mint session id: {error}"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    ))
}

#[allow(clippy::too_many_arguments)]
pub fn dispatch_grok_once(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    model: Option<&str>,
    effort: Option<&str>,
    yolo: bool,
    permission_mode: Option<&str>,
    timeout: Option<Duration>,
    harness_args: &[String],
) -> AskOutcome {
    if let Err(error) = crate::claude_ask::validate_spawn_inputs(name, from_name) {
        return AskOutcome::error(error, 2);
    }
    let events = home.events_jsonl();
    let registry = match crate::state::load_registry(&home.registry_json()) {
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

    let session_id = match new_session_id() {
        Ok(id) => id,
        Err(error) => return AskOutcome::error(error, 2),
    };
    let prompt = if message.is_empty() { "hello" } else { message };
    let mut argv = match build_argv(
        &session_id,
        prompt,
        from_name,
        model,
        effort,
        yolo,
        permission_mode,
        harness_args,
    ) {
        Ok(argv) => argv,
        Err(error) => return AskOutcome::error(error, 2),
    };

    let program = argv.remove(0);
    let mut command = Command::new(program);
    command
        .args(argv)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::claims::stamp_command_env(&mut command, Some(name), "grok", Some(&session_id));
    unsafe {
        command.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return AskOutcome::error("grok binary not found on PATH", 13);
        }
        Err(error) => return AskOutcome::error(format!("OSError invoking grok: {error}"), 2),
    };

    let pid = child.id();
    let _sigint_guard = crate::subprocess_ask::SigintForwarder::install(pid);
    let mut stdout = Vec::new();
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    let stderr_capture = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::BufReader::new(stderr_pipe).read_to_string(&mut text);
        text
    });
    let mut watchdog =
        crate::subprocess_ask::AskWatchdog::spawn(pid, Some(timeout.unwrap_or(DEFAULT_TIMEOUT)));
    let stdout_error = stdout_pipe.read_to_end(&mut stdout).err();
    watchdog.cancel();
    let (status, sigkill_escalated) = crate::subprocess_ask::wait_with_grace(pid, &mut child, 5.0);
    watchdog.join();
    let stderr = stderr_capture.join().unwrap_or_default();
    let was_timed_out = watchdog.timed_out() || sigkill_escalated;

    if crate::subprocess_ask::ask_interrupted() {
        return AskOutcome::error("interrupted", 130);
    }
    if was_timed_out {
        return AskOutcome::error("grok headless run timed out", 12);
    }
    if let Some(error) = stdout_error {
        return AskOutcome::error(format!("could not read grok output: {error}"), 2);
    }
    let reply = String::from_utf8_lossy(&stdout).into_owned();
    if status != 0 {
        let lowered = stderr.to_ascii_lowercase();
        if [
            "not signed in",
            "not authenticated",
            "authentication required",
        ]
        .iter()
        .any(|marker| lowered.contains(marker))
        {
            return AskOutcome::error(
                format!(
                    "Grok authentication is required; run `grok login --device-code`. {}",
                    stderr.trim()
                ),
                11,
            );
        }
        return AskOutcome::error(
            format!("grok headless exited {status}: {}", stderr.trim()),
            2,
        );
    }
    if reply.trim().is_empty() {
        return AskOutcome::error("grok headless returned an empty reply", 3);
    }

    let model = model.unwrap_or("grok-4.6");
    let effort = effort.unwrap_or("high");
    let posture = if yolo {
        "always-approve".to_string()
    } else if let Some(mode) = permission_mode {
        format!("permission-mode={mode}")
    } else {
        "default".to_string()
    };
    let receipt =
        format!("session_id={session_id} model={model} effort={effort} posture={posture}\n");
    emit_event(
        &events,
        "agent_ask_done",
        &[
            ("stage", "dispatch".into()),
            ("name", name.into()),
            ("provider", "grok".into()),
            ("session_id", session_id.into()),
            ("model", model.into()),
            ("effort", effort.into()),
            ("posture", posture.into()),
        ],
    );
    AskOutcome::reply(reply, receipt)
}
