//! The server axis (x-f209): which mux server does this call address?
//! One server is one socket and its workspaces; a session is one harness
//! transcript. `--server` / `FNO_SERVER` name the axis, and the retired
//! `--session` / `FNO_SESSION` spellings stay as aliases that work and warn,
//! because live worker payloads and long-running servers still send them.
//! The shared `--server <s>` / `--json` flag prefix every small verb parses
//! rides here too, beside the resolver it feeds.

use std::ffi::OsString;

use crate::proto::DEFAULT_SESSION;

/// The mux server axis, ruled 2026-09-10 (x-f209): a server is one socket and
/// its workspaces; a session is one harness transcript. `FNO_SESSION` and
/// `--session` stay as deprecated aliases that work and warn, because live
/// worker payloads and long-running servers still send the old spellings.
pub const SERVER_ENV: &str = "FNO_SERVER";
pub const LEGACY_SERVER_ENV: &str = "FNO_SESSION";

/// The value [`env_server`] took from `FNO_SESSION`, when it did, so
/// [`resolve_session`] can tell an env-decided server from a flag-decided one.
static LEGACY_ENV_VALUE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static LEGACY_ENV_NOTED: std::sync::Once = std::sync::Once::new();

/// One stderr line when a parser consumed the retired `--session` spelling.
pub fn note_server_flag(tok: &str) {
    if tok == "--session" {
        eprintln!("warning: --session is deprecated; use --server instead. The alias will be removed in a future release.");
    }
}

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// `FNO_SERVER`, else `FNO_SESSION`, else None. Silent: the note fires in
/// [`resolve_session`], and only when the legacy value decided the server.
pub fn env_server() -> Option<String> {
    nonempty_env(SERVER_ENV).or_else(|| {
        nonempty_env(LEGACY_SERVER_ENV).map(|v| {
            let _ = LEGACY_ENV_VALUE.set(v.clone());
            v
        })
    })
}

/// Resolve the target session: explicit flag/arg > `FNO_SESSION` (set in
/// every pane the server spawns) > the default. Pure in its return value, so
/// precedence is unit-testable (Locked 7).
pub fn resolve_session(explicit: Option<&str>, env: Option<&str>) -> String {
    let resolved = explicit
        .map(str::to_string)
        .or_else(|| env.filter(|s| !s.is_empty()).map(str::to_string))
        .unwrap_or_else(|| DEFAULT_SESSION.to_string());
    // The FNO_SESSION note only when the legacy var is the value that decided
    // the server: an explicit flag or a set FNO_SERVER never prints it.
    if explicit.is_none() {
        if let (Some(legacy), Some(env)) = (LEGACY_ENV_VALUE.get(), env) {
            if env == legacy {
                LEGACY_ENV_NOTED.call_once(|| {
                    eprintln!("warning: FNO_SESSION is deprecated; use FNO_SERVER instead. The alias will be removed in a future release.");
                });
            }
        }
    }
    resolved
}

/// Read the value of a `--flag value` pair, advancing `i` past the value.
pub fn flag_value(args: &[OsString], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .and_then(|a| a.to_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{flag} needs a value"))
}

/// Split off a leading `--session <s>` / `--json` prefix shared by the small
/// `tab`/`layout` verbs, returning the rest for verb-specific parsing.
pub fn take_common_flags(args: &[OsString]) -> Result<(Option<String>, bool, Vec<String>), String> {
    let mut session = None;
    let mut json = false;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let tok = args[i]
            .to_str()
            .ok_or_else(|| "non-UTF-8 argument".to_string())?;
        match tok {
            "--json" => json = true,
            "--server" | "--session" => {
                note_server_flag(tok);
                session = Some(flag_value(args, &mut i, tok)?)
            }
            other => rest.push(other.to_string()),
        }
        i += 1;
    }
    Ok((session, json, rest))
}
