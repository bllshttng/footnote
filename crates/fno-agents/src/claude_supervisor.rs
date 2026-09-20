//! Clean-birth guard for the claude supervisor (`claude daemon run`).
//!
//! The supervisor is ONE long-lived process per `CLAUDE_CONFIG_DIR`; it takes
//! its whole env from the first client that births it and forks every hosted
//! session from that env. A value naming one session, route or delegation is
//! therefore never right there: the measured specimen (`FNO_AGENTS_RUNTIME`
//! pinned `python` by the delegated wake, plus `FNO_WAKE_MSG`,
//! `FNO_ROUTE_PROVIDER=zai` and the z.ai endpoint/token) disabled every
//! Rust-only `fno agents` verb inside every hosted session for the
//! supervisor's whole life, and left the birth session's identity readable in
//! its `ps eww` output.
//!
//! The anti-recursion pin is CORRECT on a delegated one-verb child (the
//! `run_resume` delegation execs `fno agents resume` pinned to the Python
//! front door; without the pin that exec re-enters this binary and loops),
//! and wrong on a supervisor that lives for hours. The rule here is
//! birth-scoped, not key-scoped: nothing is stripped from any client, we only
//! make sure a supervisor we can see coming is born clean. A supervisor that
//! is already running keeps its env until the operator restarts it.
//!
//! Measured 2026-09-19 (throwaway `CLAUDE_CONFIG_DIR`): no
//! fno-reachable `claude` verb births a supervisor when the verb itself
//! fails; a detached `daemon run` survives 30s+ with zero clients; a later
//! dirty-env `attach` connects to it same-pid and leaves it clean; and
//! claude serializes two `daemon run` spawns on one dir itself.

use std::path::{Path, PathBuf};

use crate::model_env_scrub::MODEL_ENV_KEYS;

/// The `FNO_*` keys that configure the MACHINE rather than one session: state
/// roots, config paths and binary pins. A supervisor must keep these. Holding
/// them back would move every session it forks to the default state root,
/// where it reads a different registry and backlog than the operator
/// configured and writes state outside the configured tree - silently, since
/// a default root is a working root.
///
/// The direction is deliberate: an `FNO_*` key not named here is held back.
/// A new per-session stamp then defaults to the safe side, and a new
/// machine-wide key that belongs here announces itself as a setting that did
/// not take effect, which is the failure an operator can see and fix.
const SUPERVISOR_WIDE_FNO_KEYS: [&str; 15] = [
    "FNO_AGENTS_BIN",
    "FNO_AGENTS_HOME",
    "FNO_BIN",
    "FNO_CLAIMS_ROOT",
    "FNO_CONFIG",
    "FNO_EVENTS_PATH",
    "FNO_GLOBAL_SETTINGS_PATH",
    "FNO_HOME",
    "FNO_LOOPCHECK_FNO_BIN",
    "FNO_PY",
    "FNO_ROUTE_SETTINGS_DIR",
    "FNO_RUNTIME_STATE_PATH",
    "FNO_SPACES_DIR",
    "FNO_SPAWN_GATE",
    "FNO_TRACKER_BACKEND",
];

/// Every key that must never reach a supervisor's birth env: per-session
/// identity, route and delegation stamps (`FNO_*` outside the machine-wide
/// set, `CODEX_COMPANION_*`), route and credential carriers, the model vars,
/// and the birth session's own claude identity. `CLAUDE_CONFIG_DIR` is
/// deliberately absent: it selects WHICH supervisor and credential store a
/// client addresses, and stripping it would move the session to another
/// account.
pub fn is_poison(key: &str) -> bool {
    if SUPERVISOR_WIDE_FNO_KEYS.contains(&key) || key.starts_with("FNO_TEST_") {
        return false;
    }
    key.starts_with("FNO_")
        || key.starts_with("CODEX_COMPANION_")
        || matches!(
            key,
            "ANTHROPIC_BASE_URL"
                | "ANTHROPIC_AUTH_TOKEN"
                | "ANTHROPIC_API_KEY"
                | "CLAUDE_CODE_OAUTH_TOKEN"
                | "CLAUDE_PID"
                | "CLAUDE_JOB_DIR"
                | "CLAUDE_EFFORT"
                | "CLAUDE_CODE_MESSAGING_SOCKET"
                | "CLAUDE_CODE_MESSAGING_TOKEN"
        )
        || MODEL_ENV_KEYS.contains(&key)
}

/// The poison keys present in the ambient env - the birth command holds these
/// back. Names only, never values.
fn held_poison_keys() -> Vec<String> {
    std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| is_poison(k))
        .collect()
}

/// The `CLAUDE_CONFIG_DIR` an about-to-run client overlay carries, if any.
fn overlay_config_dir<'a, I>(overlay: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    overlay
        .into_iter()
        .find(|(k, v)| *k == "CLAUDE_CONFIG_DIR" && !v.trim().is_empty())
        .map(|(_, v)| PathBuf::from(v))
}

fn supervisor_birth_command(config_dir: Option<&Path>) -> std::process::Command {
    let mut cmd = std::process::Command::new("claude");
    cmd.args(["daemon", "run"]);
    if let Some(dir) = config_dir {
        cmd.arg("--json-path").arg(dir.join("daemon.json"));
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    for key in held_poison_keys() {
        cmd.env_remove(key);
    }
    cmd
}

fn supervisor_running(config_dir: Option<&Path>) -> bool {
    use std::process::Stdio;
    let mut cmd = std::process::Command::new("claude");
    cmd.args(["daemon", "status"]);
    if let Some(dir) = config_dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    // Null every stream: this probe runs before an ordinary client spawn, and
    // `daemon status` prints pid/version/uptime lines that would otherwise
    // land in the CLIENT's stdout, where a caller is parsing a receipt.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// True when the detached supervisor was spawned. False means the spawn
/// itself failed (no `claude` on PATH, fork refused), and the caller must not
/// then wait on a birth that was never started.
fn spawn_detached_supervisor(config_dir: Option<&Path>) -> bool {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let mut cmd = supervisor_birth_command(config_dir);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: pre_exec runs in the forked child before exec; setsid is
    // async-signal-safe and no other process state is touched here.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    match cmd.spawn() {
        // Reap asynchronously: the client calling this guard can live as long
        // as an attach TUI (hours), and an unreaped child would sit as a
        // zombie the whole time.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

fn wait_for_supervisor(config_dir: Option<&Path>) -> bool {
    for _ in 0..20 {
        if supervisor_running(config_dir) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    false
}

/// The one call every Rust site that can birth a supervisor makes, right
/// before it spawns or execs the `claude` client. `overlay` is the env that
/// site is about to apply, read only for the `CLAUDE_CONFIG_DIR` naming which
/// supervisor is addressed; the ambient value stands in when it carries none.
///
/// Arm A (the measured pick): when no supervisor serves that dir, start one
/// detached with every poison key held back, wait up to 5 s for it to serve,
/// and print one stderr line naming what was held back. The client's own
/// command is never touched, which is why this takes no `Command` - the
/// delegation in `run_resume` must keep its `FNO_AGENTS_RUNTIME=python` pin,
/// and only a host that reads that pin is this guard's problem.
///
/// Every failure degrades to silence and lets the client run: a guard that
/// refused here would break a working lane over a condition the client cannot
/// fix from inside itself.
pub fn guard_birth<'a, I>(overlay: I)
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let config_dir = overlay_config_dir(overlay);
    let config_dir = config_dir.as_deref();
    if supervisor_running(config_dir) {
        return;
    }
    let held = held_poison_keys();
    if !spawn_detached_supervisor(config_dir) {
        return;
    }
    if !wait_for_supervisor(config_dir) {
        return;
    }
    if !held.is_empty() {
        eprintln!(
            "fno: held {} out of the claude supervisor's birth env: a supervisor \
             serves every hosted session in the config dir, so no key naming one \
             session, route or delegation is ever right there. A supervisor \
             already running keeps its env until it is restarted.",
            held.join(", ")
        );
    }
}

/// `guard_birth` for the common site whose env is a reentry plan's map.
pub fn guard_birth_for_plan(env: &std::collections::BTreeMap<String, String>) {
    guard_birth(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(cmd: &std::process::Command) -> Vec<(String, Option<String>)> {
        cmd.get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect()
    }

    #[test]
    fn poison_covers_the_measured_birth_env_but_not_the_config_dir() {
        for key in [
            "FNO_AGENTS_RUNTIME",
            "FNO_WAKE_MSG",
            "FNO_ROUTE_PROVIDER",
            "FNO_PLAN_PROBE",
            "FNO_PANE",
            "CODEX_COMPANION_SESSION_ID",
            "CODEX_COMPANION_TRANSCRIPT_PATH",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "CLAUDE_PID",
            "CLAUDE_JOB_DIR",
            "CLAUDE_EFFORT",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
        ] {
            assert!(is_poison(key), "{key} must be poison");
        }
        assert!(!is_poison("CLAUDE_CONFIG_DIR"));
        assert!(!is_poison("PATH"));
        assert!(!is_poison("HOME"));
    }

    #[test]
    fn machine_wide_fno_config_survives_the_birth() {
        // A supervisor that lost these would fork every session onto the
        // DEFAULT state root: a different registry and backlog than the
        // operator configured, and state written outside the configured tree.
        // Per-session stamps in the same namespace are still held back.
        for key in SUPERVISOR_WIDE_FNO_KEYS {
            assert!(
                !is_poison(key),
                "{key} configures the machine, not a session"
            );
        }
        assert!(!is_poison("FNO_TEST_HERMETIC"));
        for key in [
            "FNO_AGENTS_RUNTIME",
            "FNO_SESSION",
            "FNO_NODE",
            "FNO_AGENT_SELF",
            "FNO_HARNESS_SESSION_ID",
            "FNO_REPO_ROOT",
            "FNO_WAKE_MSG",
        ] {
            assert!(is_poison(key), "{key} names one session, never the machine");
        }
    }

    #[test]
    fn a_pinned_birth_command_holds_every_poison_key_back() {
        // AC1-HP: the AC1 key set, plus the prefix cases and one model var.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let poison = [
            "FNO_AGENTS_RUNTIME",
            "FNO_WAKE_MSG",
            "FNO_ROUTE_PROVIDER",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_MODEL",
            "CLAUDE_PID",
            "CODEX_COMPANION_SESSION_ID",
        ];
        let prior: Vec<_> = poison.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for k in poison {
            std::env::set_var(k, "probe-value");
        }
        let dir = std::env::temp_dir().join(format!("fno-sup-birth-{}", std::process::id()));
        let cmd = supervisor_birth_command(Some(&dir));
        for (k, v) in prior {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let envs = env_of(&cmd);
        for k in poison {
            assert!(
                envs.iter().any(|(n, v)| n == k && v.is_none()),
                "poison key {k} was not held back, got {envs:?}"
            );
        }
        assert_eq!(
            envs.iter()
                .find(|(n, _)| n == "CLAUDE_CONFIG_DIR")
                .unwrap()
                .1,
            Some(dir.to_string_lossy().to_string())
        );
        // PATH and HOME are inherited, never explicitly stripped.
        assert!(envs
            .iter()
            .all(|(n, v)| (n != "PATH" && n != "HOME") || v.is_some()));
    }

    #[test]
    fn a_clean_birth_command_changes_nothing_that_is_not_poison() {
        // AC2-EDGE, pure half: whatever the ambient env holds, the only
        // explicit entries the birth command carries are poison removals -
        // nothing else is set, stripped or rewritten. With a poison-free
        // ambient env that is the empty set, and the notice (driven by the
        // same held list in guard_birth) prints nothing.
        let cmd = supervisor_birth_command(None);
        for (n, v) in env_of(&cmd) {
            assert!(
                v.is_none() && is_poison(&n),
                "non-poison key {n} was changed: {v:?}"
            );
        }
    }

    #[test]
    fn overlay_config_dir_reads_the_overlay_not_the_ambient() {
        use std::collections::BTreeMap;
        let mut plan_env = BTreeMap::new();
        plan_env.insert("CLAUDE_CONFIG_DIR".to_string(), "/tmp/one".to_string());
        plan_env.insert("FNO_NODE".to_string(), "x-1".to_string());
        let got = overlay_config_dir(plan_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        assert_eq!(got, Some(PathBuf::from("/tmp/one")));
        // no overlay key -> None (the caller then lets guard_birth resolve the
        // ambient dir itself)
        let got = overlay_config_dir([("FNO_NODE", "x-1")]);
        assert_eq!(got, None);
        // a blank overlay value is no dir
        let got = overlay_config_dir([("CLAUDE_CONFIG_DIR", "  ")]);
        assert_eq!(got, None);
    }
}
