use std::path::Path;
use std::process::Command;

/// Spawn `fno --server <socket>` detached: its own session (setsid) so the
/// server never receives the terminal's SIGHUP, stderr to a per-session log.
/// Two clients racing here both spawn; the bind is the lock, the losing
/// server exits 0, and both clients attach to the winner (AC4-EDGE).
pub(super) fn spawn_server(path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(super::log_path(path))
        .map_err(|e| format!("cannot open server log: {e}"))?;
    let mut cmd = crate::process_admission::std_command(exe);
    cmd.arg("--server")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    stamp_sandbox_owner(&mut cmd);
    // The pure-Rust server does not read config.toml. Resolve this once on the
    // client, which already pays for the bounded config lookup.
    if std::env::var_os("FNO_MUX_SHELL_INTEGRATION").is_none() && shell_integration_off() {
        cmd.env("FNO_MUX_SHELL_INTEGRATION", "off");
    }
    if std::env::var_os("FNO_BOARD_SCOPE").is_none() {
        // The server must not shell out for config on its SIGTERM-critical
        // startup path; the client resolves the scope and passes it by env.
        let (scope, _why) =
            crate::backlog_view::resolve_board_scope(crate::config_defaults::lookup_key);
        cmd.env(
            "FNO_BOARD_SCOPE",
            crate::backlog_view::board_scope_wire(&scope),
        );
    }
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    crate::process_admission::std_spawn(&mut cmd)
        .map(|_| ())
        .map_err(|e| format!("cannot spawn the mux server: {e}"))
}

/// A Codex companion session is the explicit sandbox marker for an owner-bound
/// mux server. A caller may pass an outer script's pid; otherwise this client
/// process owns the detached server for the duration of its attach.
pub(super) fn stamp_sandbox_owner(cmd: &mut Command) {
    const OWNER_KEYS: [&str; 3] = ["FNO_OWNER_PID", "FNO_OWNER_BIRTH", "FNO_OWNER_SESSION"];
    for key in OWNER_KEYS {
        cmd.env_remove(key);
    }
    let session = std::env::var("FNO_OWNER_SESSION")
        .ok()
        .filter(|v| valid_owner_session(v))
        .or_else(|| {
            std::env::var("CODEX_COMPANION_SESSION_ID")
                .ok()
                .filter(|v| valid_owner_session(v))
        });
    let Some(session) = session else { return };
    let owner_pid = match std::env::var("FNO_OWNER_PID") {
        Ok(raw) => match raw.parse::<u32>() {
            Ok(pid) if pid > 1 => pid,
            _ => return,
        },
        Err(_) => std::process::id(),
    };
    let birth = match std::env::var("FNO_OWNER_BIRTH") {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(birth) if birth > 0 => birth,
            _ => return,
        },
        Err(_) => match crate::proto::pid_start_time(owner_pid) {
            Some(birth) => birth,
            None => return,
        },
    };
    if crate::proto::pid_start_time(owner_pid) != Some(birth) {
        return;
    }
    cmd.env("FNO_OWNER_PID", owner_pid.to_string())
        .env("FNO_OWNER_BIRTH", birth.to_string())
        .env("FNO_OWNER_SESSION", session);
}

fn valid_owner_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 128
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Whether the interactive path must disable OSC 133 injection. Fail-open
/// through the native config read ([`crate::config_defaults::lookup_key`]):
/// an absent file, an unreadable one, or a non-`off` value all leave
/// injection on (the default). Runs synchronously inside `spawn_server`,
/// *before* the client's spawn-connect wait loop exists - the native read is
/// one bounded file parse, where the retired `fno config get` subprocess
/// cold-started the Python shim and could freeze `fno` startup for seconds.
fn shell_integration_off() -> bool {
    crate::config_defaults::lookup_key("mux.shell_integration")
        .as_deref()
        .map(config_says_off)
        .unwrap_or(false)
}

/// The one off-switch, matched exactly like the Rust pane-spawn side
/// (`pty::integration_disabled`): only a trimmed `off` disables injection.
pub(crate) fn config_says_off(stdout: &str) -> bool {
    stdout.trim() == "off"
}
