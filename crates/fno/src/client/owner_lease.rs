use std::process::Command;

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
