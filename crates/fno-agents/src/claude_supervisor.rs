//! Clean-birth guard for the claude supervisor (`claude daemon run`).
//!
//! The supervisor is ONE long-lived process per `CLAUDE_CONFIG_DIR`; it takes
//! its whole env from the first client that births it and forks every hosted
//! session from that env. A value naming one session, route or delegation is
//! therefore never right there. One measured birth carried
//! `FNO_AGENTS_RUNTIME`, `FNO_WAKE_MSG`, `FNO_ROUTE_PROVIDER=zai` and the z.ai
//! endpoint/token; it disabled every Rust-only `fno agents` verb in every
//! hosted session for the supervisor's whole life and left the birth
//! session's identity readable in `ps eww` output.
//!
//! Python `bg_create` enters through `run_birth_exec`, which applies this
//! guard before the client command. The rule is birth-scoped, not key-scoped:
//! nothing is stripped from the client; only a newly born supervisor is
//! cleaned. A supervisor that is already running keeps its env until restart.
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
const SUPERVISOR_WIDE_FNO_KEYS: [&str; 34] = [
    "FNO_AGENTS_BIN",
    "FNO_AGENTS_DAEMON_BIN",
    "FNO_AGENTS_FRONT",
    "FNO_AGENTS_HOME",
    "FNO_BIN",
    "FNO_BUS_DIR",
    "FNO_CARGO_TARGETS_BASE",
    "FNO_CC_DAEMON_RV_ROOT",
    "FNO_CLAIMS_ROOT",
    "FNO_CLAUDE_DAEMON_DIR",
    "FNO_CLAUDE_PROJECTS_DIR",
    "FNO_CODEX_BIN",
    "FNO_CODEX_SESSIONS_DIR",
    "FNO_CONFIG",
    "FNO_EVENTS_PATH",
    "FNO_GLOBAL_SETTINGS_PATH",
    "FNO_HOME",
    "FNO_KILLCHECK_GIT_BIN",
    "FNO_LOOPCHECK_FNO_BIN",
    "FNO_LOOPCHECK_GH_BIN",
    "FNO_LOOPCHECK_GIT_BIN",
    "FNO_LOOPS_MAIL_BIN",
    "FNO_MUX_DIR",
    "FNO_OPERATOR_CAPTURE_DIR",
    "FNO_PLANS_DIRS_CACHE_DIR",
    "FNO_PR_STATUS_CACHE_DIR",
    "FNO_PY",
    "FNO_RECLAIM_STATE_ROOT",
    "FNO_ROUTE_SETTINGS_DIR",
    "FNO_RUNTIME_STATE_PATH",
    "FNO_SPACES_DIR",
    "FNO_SPAWN_GATE",
    "FNO_TRACKER_BACKEND",
    "FNO_VERIFY_GIT_BIN",
];

// A new FNO_ registry row fails classification until it lands in this list or
// crates/fno-agents/src/claude_supervisor_held_keys.txt. The default is held.

/// Every key that must never reach a supervisor's birth env: per-session
/// identity, route and delegation stamps (`FNO_*` outside the machine-wide
/// set, `CODEX_COMPANION_*`), route and credential carriers, the model vars,
/// and the birth session's own claude identity. Unknown FNO_ keys are held
/// unless the machine-wide allow list above or the held-key file classifies them.
/// `CLAUDE_CONFIG_DIR` selects which supervisor and credential store a client
/// addresses, so stripping it would move the session to another account.
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
pub(crate) fn held_poison_keys() -> Vec<String> {
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

fn addressed_config_dir(overlay_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = overlay_dir {
        return Some(dir.to_path_buf());
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        if !dir.to_string_lossy().trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"))
}

fn birth_allowed(hermetic: bool, config_dir: Option<&Path>, claude_bin: Option<&Path>) -> bool {
    if claude_bin.is_some_and(crate::paths::under_temp_dir) {
        return true;
    }
    !hermetic && !config_dir.is_some_and(crate::paths::under_temp_dir)
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
/// Python's `bg_create` reaches this guard through `run_birth_exec`.
///
/// Arm A (the measured pick): when no supervisor serves that dir, start one
/// detached with every poison key held back, wait up to 5 s for it to serve,
/// and print one stderr line naming what was held back. The client's own
/// command is never touched; only the detached supervisor's birth env is
/// filtered. `run_birth_exec` leaves the client argv, env, cwd, streams, and
/// pid intact when it replaces itself with Claude.
///
/// Every failure degrades to silence and lets the client run: a guard that
/// refused here would break a working lane over a condition the client cannot
/// fix from inside itself. Test-owned temp config dirs are refused before the
/// probe because no process owns the detached supervisor after the test exits.
pub fn guard_birth<'a, I>(overlay: I)
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let config_dir = overlay_config_dir(overlay);
    let config_dir = config_dir.as_deref();
    if cfg!(test) {
        return;
    }
    let addressed_dir = addressed_config_dir(config_dir);
    let hermetic = std::env::var("FNO_TEST_HERMETIC").ok().as_deref() == Some("1");
    let claude_bin = crate::loop_dispatch::which_binary("claude");
    if !birth_allowed(hermetic, addressed_dir.as_deref(), claude_bin.as_deref()) {
        let dir = addressed_dir
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        let binary = claude_bin
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<not found>".to_string());
        if hermetic
            && !addressed_dir
                .as_deref()
                .is_some_and(crate::paths::under_temp_dir)
        {
            eprintln!(
                "fno: no claude supervisor born for {dir}: hermetic test run does not own a real supervisor"
            );
        } else {
            eprintln!(
                "fno: no claude supervisor born for {dir}: it lies under the temp dir and {binary} is not a test fixture, so nothing would own it"
            );
        }
        return;
    }
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

/// Run a Claude client command after guarding any supervisor it may birth.
/// `--` is a strict fence; the client argv, environment, cwd, and streams pass
/// through unchanged when the process is replaced.
pub fn run_birth_exec(args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("--") || args.len() < 2 || args[1].is_empty() {
        eprintln!("fno-agents claude-birth-exec: expected -- <claude argv...>");
        return 2;
    }
    guard_birth(std::iter::empty::<(&str, &str)>());
    use std::os::unix::process::CommandExt;
    let argv = &args[1..];
    let error = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    eprintln!("claude CLI not found: {error}");
    127
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

    #[test]
    fn a_temp_config_dir_refuses_a_real_binary() {
        let config_dir = std::env::temp_dir().join("fno-supervisor-test/.claude");
        let claude_bin = Path::new("/usr/local/bin/claude");

        assert!(!birth_allowed(false, Some(&config_dir), Some(claude_bin)));
    }

    #[test]
    fn a_temp_fixture_binary_still_births() {
        let config_dir = std::env::temp_dir().join("fno-supervisor-test/.claude");
        let claude_bin = std::env::temp_dir().join("fno-supervisor-test/bin/claude");

        assert!(birth_allowed(false, Some(&config_dir), Some(&claude_bin)));
    }

    #[test]
    fn a_real_config_dir_keeps_the_guard() {
        let config_dir = Path::new("/Users/someone/.claude");
        let claude_bin = Path::new("/usr/local/bin/claude");

        assert!(birth_allowed(false, Some(config_dir), Some(claude_bin)));
    }

    #[test]
    fn no_claude_on_path_under_a_temp_dir_is_refused() {
        let config_dir = std::env::temp_dir().join("fno-supervisor-test/.claude");

        assert!(!birth_allowed(false, Some(&config_dir), None));
    }

    #[test]
    fn a_hermetic_run_refuses_a_real_binary_for_a_real_dir() {
        let config_dir = Path::new("/Users/someone/.claude");
        let claude_bin = Path::new("/usr/local/bin/claude");
        let fixture_bin = std::env::temp_dir().join("fno-supervisor-test/bin/claude");

        assert!(!birth_allowed(true, Some(config_dir), Some(claude_bin)));
        assert!(birth_allowed(true, Some(config_dir), Some(&fixture_bin)));
    }

    #[test]
    fn every_fno_registry_key_is_classified_for_the_supervisor() {
        use std::collections::HashSet;

        let held_text = include_str!("claude_supervisor_held_keys.txt");
        let validate = |registry: &str| -> Result<(), String> {
            let keys: Vec<&str> = registry
                .lines()
                .filter_map(|line| {
                    let name = line.strip_prefix("| `")?.split('`').next()?;
                    name.starts_with("FNO_").then_some(name)
                })
                .collect();
            if keys.len() <= 100 {
                return Err(format!(
                    "expected over 100 FNO_ registry rows, found {}",
                    keys.len()
                ));
            }
            let mut held = HashSet::new();
            for name in held_text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
            {
                if !held.insert(name) {
                    return Err(format!("{name} occurs more than once in the held list"));
                }
                if !keys.contains(&name) {
                    return Err(format!(
                        "{name} is in crates/fno-agents/src/claude_supervisor_held_keys.txt but has no row in docs/env-vars.md"
                    ));
                }
            }
            for name in keys.iter().filter(|name| !name.starts_with("FNO_TEST_")) {
                let kept = !is_poison(name);
                let explicitly_held = held.contains(name);
                if kept == explicitly_held {
                    return Err(format!(
                        "{name} has a row in docs/env-vars.md but no supervisor classification: add it to SUPERVISOR_WIDE_FNO_KEYS in crates/fno-agents/src/claude_supervisor.rs if it configures the machine, or to crates/fno-agents/src/claude_supervisor_held_keys.txt if it names one session"
                    ));
                }
            }
            for name in ["FNO_AGENTS_RUNTIME", "FNO_ROUTE_PROVIDER"] {
                if !held.contains(name) {
                    return Err(format!("{name} must remain in the supervisor held list"));
                }
            }
            if !is_poison("FNO_WAKE_MSG") {
                return Err("FNO_WAKE_MSG must remain poison by the FNO_ default".to_string());
            }
            Ok(())
        };

        let registry = include_str!("../../../docs/env-vars.md");
        validate(registry).unwrap();
        let probe = format!("{registry}\n| `FNO_ZZ_PROBE` | rs | probe |\n");
        let error = validate(&probe).unwrap_err();
        assert!(error.contains("FNO_ZZ_PROBE"));
        assert!(error.contains("SUPERVISOR_WIDE_FNO_KEYS"));
        assert!(error.contains("claude_supervisor_held_keys.txt"));
    }
}
