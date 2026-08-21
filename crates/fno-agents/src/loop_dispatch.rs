//! Shellout dispatcher that wraps the bash driver-lib contract.
//!
//! ## Design: the shellout seam (grilled decision 8)
//!
//! The Rust `Dispatcher` trait exists so a future daemon/PTY implementation can
//! be wired in as a drop-in replacement without touching the loop runtime or the
//! `TargetQueue`. This file implements the bash-shellout side only: it sources
//! `driver-<name>.sh` and calls `driver_invoke`, delegating all session logic to
//! the bash lib. The Rust side NEVER reimplements driver behavior; it only manages
//! process lifecycle, env passthrough, and exit-code collection.
//!
//! The seam is stable once the trait is locked (Task 1.1). A future PTY
//! dispatcher can implement `Dispatcher` + `Session` and be swapped in by the
//! CLI flag `--dispatcher pty` without changing any other code.
//!
//! ## Binary resolution (preflight)
//!
//! Mirrors `scripts/run-target-loop.sh:144-150`. The Rust side validates the
//! driver whitelist and binary availability before any dispatch, so a missing
//! binary fails loudly at startup rather than inside iteration N.

use crate::loop_runtime::{DispatchCtx, Dispatcher, LoopError, Session, Unit};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

// ── public API ─────────────────────────────────────────────────────────────────

/// Validate the driver name and confirm the driver lib file exists, the
/// driver binary is on PATH, and the lib defines `driver_invoke`.
///
/// `driver`: one of `claude-code`, `hermes`, `openclaw`, `opencode`
///   (whitelist-enforced).
/// `lib_dir`: directory containing `driver-<driver>.sh`.
/// `cli_alias`: optional CLI alias from `--cli` flag (F2). Precedence for
///   binary resolution: `$CLAUDE_CLI` env > `cli_alias` > `$CLI` env > "claude".
///
/// Returns the resolved path to the driver lib file on success.
/// Returns `LoopError::Config` for whitelist/path/function errors,
/// `LoopError::Dispatch` for a missing binary (the caller maps that to exit 77).
pub fn preflight(
    driver: &str,
    lib_dir: &Path,
    cli_alias: Option<&str>,
) -> Result<PathBuf, LoopError> {
    // Whitelist enforced exactly like run-target-loop.sh:144-150 to prevent
    // path traversal and shell injection via driver names.
    const ALLOWED: &[&str] = &["claude-code", "hermes", "openclaw", "opencode"];
    if !ALLOWED.contains(&driver) {
        return Err(LoopError::Config(format!(
            "invalid dispatcher '{driver}': must be one of {:?} (whitelist)",
            ALLOWED
        )));
    }

    // Lib file must exist.
    let lib_path = lib_dir.join(format!("driver-{driver}.sh"));
    if !lib_path.exists() {
        return Err(LoopError::Config(format!(
            "driver lib not found: {}",
            lib_path.display()
        )));
    }

    // F2: binary resolution uses cli_alias (not process env CLI) so preflight
    // checks the same binary the dispatcher will actually use.
    let binary = resolve_driver_binary(driver, cli_alias);
    if which_binary(&binary).is_none() {
        return Err(LoopError::Dispatch(format!(
            "missing binary '{binary}': required by dispatcher '{driver}' but not found on PATH"
        )));
    }

    // F5: probe that the lib defines driver_invoke (a lib without it produces
    // an infinite budget-burning re-dispatch loop; fail loudly at preflight).
    {
        let lib_str = lib_path.to_str().ok_or_else(|| {
            LoopError::Config(format!(
                "driver lib path is not valid UTF-8: {}",
                lib_path.display()
            ))
        })?;
        let probe_script = r#"source "$1" && type driver_invoke >/dev/null 2>&1"#;
        let probe = std::process::Command::new("bash")
            .arg("-c")
            .arg(probe_script)
            .arg("_")
            .arg(lib_str)
            .output()
            .map_err(|e| LoopError::Config(format!("driver_invoke probe bash failed: {e}")))?;
        if !probe.status.success() {
            return Err(LoopError::Config(format!(
                "driver lib '{}' does not define driver_invoke (required function missing)",
                lib_path.display()
            )));
        }
    }

    Ok(lib_path)
}

/// Query `driver_default_max()` from the driver lib via a single bash shellout.
///
/// Parses stdout as `u64`. Used when `--max-iterations` is absent.
pub fn driver_default_max(lib: &Path) -> Result<u64, LoopError> {
    let lib_str = lib.to_str().ok_or_else(|| {
        LoopError::Config(format!(
            "driver lib path is not valid UTF-8: {}",
            lib.display()
        ))
    })?;
    let script = format!("source {:?} && driver_default_max", lib_str);
    let out = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .output()
        .map_err(|e| LoopError::Dispatch(format!("bash shellout for driver_default_max: {e}")))?;
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    raw.parse::<u64>().map_err(|_| {
        LoopError::Dispatch(format!(
            "driver_default_max returned non-integer stdout: {:?}",
            raw
        ))
    })
}

// ── shared `fno` shellout helpers ─────────────────────────────────────────────

/// Build a [`Command`] for the `fno` binary.
///
/// Binary resolution: `fno_bin` (the path/name given by the caller, overridden
/// by `$FNO_BIN` for tests). If `FNO_BIN` is set and non-empty it wins;
/// otherwise `fno_bin` is used as-is (callers pass `"fno"` for production and a
/// tempdir stub path for tests).
pub(crate) fn fno_cmd(fno_bin: &str) -> Command {
    let binary = std::env::var("FNO_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fno_bin.to_string());
    Command::new(binary)
}

/// Run a spawn closure, retrying briefly on ETXTBSY ("Text file busy", os error
/// 26). The spawned file is the `fno` / `fno-agents` binary: a concurrent
/// `fno doctor update` relinks it in place, and under `cargo test` a sibling thread
/// that just wrote+exec'd a stub leaves a transient write-fd open in another
/// thread's fork window; either way the kernel can refuse the exec with
/// ETXTBSY. The condition clears within microseconds once the writing fd closes,
/// so a bounded retry turns a hard spawn failure into a short wait. Any other
/// error, and the successful value, passes through unchanged.
pub(crate) fn retry_etxtbsy<T>(
    mut spawn: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    const MAX_RETRIES: u32 = 5;
    let mut attempt: u32 = 0;
    loop {
        match spawn() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempt < MAX_RETRIES => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(2 * u64::from(attempt)));
            }
            other => return other,
        }
    }
}

/// Resolve the binary name for a given driver name.
///
/// F2: takes an explicit `cli_alias` parameter (from `--cli` flag) instead of
/// reading only the process-global `CLI` env var. Precedence (mirrors
/// driver-claude-code.sh binary resolution):
///   1. `$CLAUDE_CLI` env var (explicit override)
///   2. `cli_alias` (from `--cli` flag, placed in child env as `CLI`)
///   3. `$CLI` env var (legacy path)
///   4. `"claude"` default
///
/// Passing `cli_alias` explicitly avoids `set_var` (process-global mutation
/// that is a footgun in tests). The child env receives `CLI=<alias>` via the
/// static env list; this function reflects that same value without touching the
/// parent process environment.
pub fn resolve_driver_binary(driver: &str, cli_alias: Option<&str>) -> String {
    match driver {
        "claude-code" => {
            // 1. $CLAUDE_CLI env var.
            if let Ok(v) = std::env::var("CLAUDE_CLI") {
                if !v.is_empty() {
                    return v;
                }
            }
            // 2. Explicit cli_alias from --cli flag.
            if let Some(a) = cli_alias {
                if !a.is_empty() {
                    return a.to_string();
                }
            }
            // 3. $CLI env var (legacy).
            if let Ok(v) = std::env::var("CLI") {
                if !v.is_empty() {
                    return v;
                }
            }
            // 4. Default.
            "claude".to_string()
        }
        "hermes" => "hermes-agent".to_string(),
        "openclaw" => "openclaw".to_string(),
        "opencode" => "opencode".to_string(),
        _ => "claude".to_string(), // unreachable after whitelist check
    }
}

/// Walk `$PATH` to find a binary. Returns `Some(path)` on success.
/// Does not use an external crate; pure std.
pub fn which_binary(name: &str) -> Option<PathBuf> {
    // If the name contains a path separator, check it directly.
    if name.contains('/') {
        let p = PathBuf::from(name);
        if p.is_file() {
            return Some(p);
        }
        return None;
    }
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = PathBuf::from(dir).join(name);
        if candidate.is_file() {
            // Check any executable bit (owner, group, or other) so that
            // root-owned binaries with mode 0o555 are recognised correctly.
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&candidate) {
                if meta.permissions().mode() & 0o111 != 0 {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

// ── launch-time headroom picking (x-7d45) ─────────────────────────────────────

/// The single env var a picked account contributes to the driver's environment.
const PICKED_ENV_KEY: &str = "CLAUDE_CONFIG_DIR";

/// The exact verb this file shells, as one named constant.
///
/// It is a constant so a cross-language test can assert this argv still resolves
/// to a real command. That check is not ceremony: this verb was spelled
/// `fno providers pick` until the surface was renamed to `fno config accounts`,
/// and because every failure here is advisory the loop would have degraded
/// silently forever rather than failing loudly once.
pub const PICK_ARGV: [&str; 5] = ["config", "accounts", "pick", "--if-armed", "--print-env"];

/// One picked account's complete env overlay: `(key, value)` pairs where an
/// EMPTY value means "clear this variable in the child".
type PickedEnv = Vec<(String, String)>;

/// Interpret a `fno config accounts pick --if-armed --print-env` result.
///
/// Pure, so the advisory contract is testable without a live `fno`. Success is
/// exit 0 plus at least one `CLAUDE_CONFIG_DIR=<non-empty>` line; the verb also
/// emits the auth vars to clear as `KEY=` and those are carried through, because
/// applying half an overlay is what lets an inherited ANTHROPIC_API_KEY bill a
/// different account than the receipt names. The verb's non-zero exits (3 = every
/// launchable candidate exhausted, 4 = no launchable candidate, 5 = picking not
/// armed) are ordinary answers here, not errors.
fn interpret_pick(ok: bool, stdout: &str, stderr: &str) -> Result<PickedEnv, String> {
    if !ok {
        let reason = stderr
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .next_back()
            .unwrap_or("no reason given");
        return Err(reason.to_string());
    }
    let mut env: PickedEnv = Vec::new();
    let mut pinned = false;
    for line in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match line.split_once('=') {
            Some((k, v)) if !k.is_empty() => {
                if !v.is_empty() {
                    pinned = true;
                }
                env.push((k.to_string(), v.to_string()));
            }
            _ => return Err(format!("unparseable pick output: {line:?}")),
        }
    }
    if !pinned {
        // A drifted verb must never have its output half-applied: with nothing
        // but clear-lines there is no account, only a scrubbed environment.
        // The pin is ANY value-carrying key, not CLAUDE_CONFIG_DIR specifically
        // - a claude api_key record's overlay is an ANTHROPIC_API_KEY and is
        // just as valid an account, and requiring the config dir would have the
        // loop reject an overlay Python accepts.
        return Err("pick output carried no account pin".to_string());
    }
    Ok(env)
}

/// True when this dispatcher drives `claude`, the only harness that reads
/// `CLAUDE_CONFIG_DIR`.
///
/// An opencode / hermes / openclaw loop would gain nothing from a claude
/// account pin, and applying the overlay would still CLEAR that run's inherited
/// Anthropic credentials while logging "account picked" - a receipt describing
/// something that did not happen, to a worker that cannot act on it.
fn drives_claude(driver_lib: &Path) -> bool {
    driver_lib
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "driver-claude-code.sh")
}

/// True when applying `picked` would silently undo a route this run pins.
///
/// A loop launched with an explicit provider route (an `ANTHROPIC_BASE_URL` +
/// `ANTHROPIC_AUTH_TOKEN` pair for a non-Anthropic endpoint, or a pinned model
/// tier) is already committed. The overlay's clear-list names exactly those
/// vars, so a static env that sets one is a deliberate routing decision the pick
/// would scrub mid-flight - moving the run to a claude account while its receipt
/// claimed only to have picked one. Deriving the check FROM the clear-list is
/// what keeps it from becoming a second, drifting copy of that list here.
///
/// The mirror of the Python seam declining to pick for a `--route`/`--role`
/// spawn, for the same reason: endpoint, auth and model are one route, and
/// half-composing it is what bills the wrong account.
fn pick_would_undo_a_route(picked: &[(String, String)], static_env: &[(String, String)]) -> bool {
    pick_would_undo_a_route_with(picked, static_env, |k| std::env::var_os(k))
}

// The ambient lookup is injected so the tests below are deterministic: a shell
// that exports ANTHROPIC_BASE_URL (any provider-routed lane) would otherwise
// flip the unrouted-loop case through the real process environment.
fn pick_would_undo_a_route_with(
    picked: &[(String, String)],
    static_env: &[(String, String)],
    ambient: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> bool {
    picked.iter().filter(|(_, v)| v.is_empty()).any(|(k, _)| {
        // The static passthrough list is only half the picture: a loop started
        // from a shell that already exported ANTHROPIC_BASE_URL inherits it
        // through the process environment without it ever appearing here, and
        // clearing it would move that run to a different provider just the same.
        static_env.iter().any(|(ek, _)| ek == k) || ambient(k).is_some_and(|v| !v.is_empty())
    })
}

/// Ask `fno config accounts pick` which account the next iteration should launch on.
///
/// Shells the verb rather than reimplementing the predicate: headroom, combo
/// order, launchability AND the `pick_on_launch` opt-in have exactly one
/// implementation, and it is not this one - `--if-armed` is what lets the verb
/// honor the knob on this caller's behalf, so a default-off install can never
/// have the loop change which account it bills. Every failure mode - a stale
/// `fno`, an absent `fno`, a refusal - is an `Err` the caller logs and ignores,
/// so the loop cannot be wedged by it.
fn pick_account_env() -> Result<PickedEnv, String> {
    let out = Command::new("fno")
        .args(PICK_ARGV)
        .output()
        .map_err(|e| format!("could not run `fno config accounts pick`: {e}"))?;
    interpret_pick(
        out.status.success(),
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
    )
}

// ── ShelloutDispatcher ────────────────────────────────────────────────────────

/// A live session wrapping a bash `driver_invoke` child process.
pub struct ShelloutSession {
    child: Child,
    /// Path the driver redirects claude stdout+stderr into (env `OUTPUT_FILE`),
    /// read after exit to classify a claude bg-guard refusal (x-4504). `None`
    /// when the dispatcher env carried no `OUTPUT_FILE`. The driver truncates
    /// this file at the start of every `driver_invoke`, so after `wait()` it
    /// holds exactly this iteration's output.
    output_file: Option<PathBuf>,
}

impl Session for ShelloutSession {
    fn wait(&mut self) -> Result<i32, LoopError> {
        let status = self.child.wait().map_err(LoopError::Io)?;
        // F4: when status.code() is None the process died by signal. Use the
        // shell convention 128+N (e.g. SIGTERM=15 -> 143, SIGKILL=9 -> 137)
        // so consumers can distinguish signal deaths from clean non-zero exits.
        // This value is recorded in the node_failed event's exit_code field.
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
    }

    fn output_tail(&self) -> Option<String> {
        use std::io::{Read, Seek, SeekFrom};
        // The guard message is short and near the end; read only the last 8 KiB
        // (seek, don't slurp) so a large transcript can never balloon memory.
        const MAX_TAIL: u64 = 8 * 1024;
        let path = self.output_file.as_ref()?;
        let mut file = std::fs::File::open(path).ok()?;
        // A metadata failure is not proof the file is gone (could be transient);
        // fall through and read from the start rather than bailing.
        if let Ok(len) = file.metadata().map(|m| m.len()) {
            let _ = file.seek(SeekFrom::Start(len.saturating_sub(MAX_TAIL)));
        }
        let mut buf = Vec::new();
        file.take(MAX_TAIL).read_to_end(&mut buf).ok()?;
        Some(String::from_utf8_lossy(&buf).into_owned())
    }
}

/// Dispatcher that sources a driver lib and calls `driver_invoke` in bash.
///
/// Static env vars are wired once at construction; `CURRENT_ITER` is injected
/// per-dispatch by `Dispatcher::run`.
pub struct ShelloutDispatcher {
    /// Resolved path to `driver-<name>.sh`.
    driver_lib: PathBuf,
    /// Static env vars passed to every invocation.
    env: Vec<(String, String)>,
    /// Working directory for the bash process.
    cwd: PathBuf,
}

impl ShelloutDispatcher {
    /// Construct a ShelloutDispatcher. `driver_lib` must be the resolved lib path
    /// (from `preflight`); `env` is the static passthrough list; `cwd` is the
    /// project root.
    pub fn new(driver_lib: PathBuf, env: Vec<(String, String)>, cwd: PathBuf) -> Self {
        Self {
            driver_lib,
            env,
            cwd,
        }
    }
}

impl Dispatcher for ShelloutDispatcher {
    fn run(&self, _unit: &Unit, ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
        let lib_str = self
            .driver_lib
            .to_str()
            .ok_or_else(|| LoopError::Dispatch("driver lib path is not valid UTF-8".to_string()))?;

        // Source the driver lib, call driver_invoke in a subshell so that an
        // `exit` inside driver_invoke terminates only the subshell (not the outer
        // bash -c process). Capture its exit code, then best-effort call
        // driver_persist_history. driver_persist_history populates HISTORY_FILE so
        // the NEXT iteration carries the prior transcript (hermes/openclaw contract,
        // mirrors run-target-loop.sh:451). It runs after EVERY iteration including
        // terminal ones (on terminal iterations the loop exits anyway so it is
        // harmless) -- keeping the shellout branch-free. The >/dev/null redirect
        // suppresses any incidental output; || true prevents a non-existent or
        // failing persist function from aborting the script (not all drivers
        // define it, and failure is non-fatal).
        let script = r#"source "$FNO_DRIVER_LIB" && (driver_invoke); rc=$?; driver_persist_history >/dev/null 2>&1 || true; exit $rc"#;

        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(script);
        cmd.env("FNO_DRIVER_LIB", lib_str);
        cmd.env("CURRENT_ITER", ctx.iteration.to_string());
        cmd.current_dir(&self.cwd);

        // Passthrough static env vars.
        for (k, v) in &self.env {
            cmd.env(k, v);
        }

        // Launch-time headroom picking. A fresh process is a fresh credential
        // read, so the iteration boundary already IS the pre-emptive handoff
        // moment - no threshold, watcher, or new trigger machinery needed. This
        // is the ONE call site: every driver's harness process is a child of the
        // bash spawned below, so all of them inherit the pick, whereas wiring it
        // into `driver_invoke` would mean one copy per driver lib. An
        // operator-pinned CLAUDE_CONFIG_DIR in the static env always wins, and a
        // refusal is advisory - the iteration proceeds on today's env.
        if drives_claude(&self.driver_lib) && !self.env.iter().any(|(k, _)| k == PICKED_ENV_KEY) {
            let iter = ctx.iteration;
            match pick_account_env() {
                // A loop launched with an explicit route (ANTHROPIC_BASE_URL +
                // ANTHROPIC_AUTH_TOKEN for a non-Anthropic endpoint, or a pinned
                // model tier) is already committed to a provider. Applying a
                // pick would scrub exactly those vars and silently move the run
                // to a claude account mid-flight. The verb's own clear-list is
                // what identifies them, so there is no second copy of it here -
                // the mirror of the Python seam declining to pick for a --route
                // or --role spawn.
                Ok(picked) if pick_would_undo_a_route(&picked, &self.env) => {
                    eprintln!(
                        "loop: iteration {iter} account not picked \
                         (this run pins its own provider route)"
                    );
                }
                Ok(picked) => {
                    // Name the pin whatever it is. Reporting only on
                    // CLAUDE_CONFIG_DIR would let an api_key account's overlay
                    // change which account is billed with no receipt at all,
                    // and an unannounced billing change is the one thing this
                    // feature must never do.
                    if let Some((key, value)) = picked.iter().find(|(_, v)| !v.is_empty()) {
                        // Announce every pick, but NEVER the pin's value unless
                        // it is the config dir. An api_key account's pin IS its
                        // ANTHROPIC_API_KEY, so echoing the value would write
                        // the secret to the loop's log on every iteration.
                        if key == PICKED_ENV_KEY {
                            eprintln!("loop: iteration {iter} account picked -> {value}");
                        } else {
                            eprintln!("loop: iteration {iter} account picked -> pinned via {key}");
                        }
                    }
                    for (key, value) in &picked {
                        // An empty value is the verb saying "clear this": an
                        // inherited ANTHROPIC_API_KEY or routed base URL outranks
                        // CLAUDE_CONFIG_DIR, so leaving one behind would bill an
                        // account the receipt does not name.
                        if value.is_empty() {
                            cmd.env_remove(key);
                        } else {
                            cmd.env(key, value);
                        }
                    }
                }
                Err(reason) => {
                    eprintln!("loop: iteration {iter} account not picked ({reason})");
                }
            }
        }

        let child = cmd
            .spawn()
            .map_err(|e| LoopError::Dispatch(format!("spawn bash driver_invoke: {e}")))?;

        // Capture OUTPUT_FILE (the driver's stdout+stderr sink) so the walk can
        // classify a claude bg-guard refusal after exit (x-4504).
        let output_file = self
            .env
            .iter()
            .find(|(k, _)| k == "OUTPUT_FILE")
            .map(|(_, v)| PathBuf::from(v));

        Ok(Box::new(ShelloutSession { child, output_file }))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        interpret_pick, pick_would_undo_a_route, pick_would_undo_a_route_with,
        resolve_driver_binary, retry_etxtbsy, PICKED_ENV_KEY,
    };

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn retry_etxtbsy_passes_success_through_without_retry() {
        let mut calls = 0u32;
        let r: std::io::Result<u8> = retry_etxtbsy(|| {
            calls += 1;
            Ok(7)
        });
        assert_eq!(r.unwrap(), 7);
        assert_eq!(calls, 1, "a successful spawn must not retry");
    }

    #[test]
    fn retry_etxtbsy_retries_then_succeeds() {
        // Simulate ETXTBSY clearing after a couple of attempts.
        let mut calls = 0u32;
        let r: std::io::Result<u8> = retry_etxtbsy(|| {
            calls += 1;
            if calls < 3 {
                Err(std::io::Error::from_raw_os_error(libc::ETXTBSY))
            } else {
                Ok(42)
            }
        });
        assert_eq!(r.unwrap(), 42);
        assert_eq!(calls, 3, "must retry past transient ETXTBSY");
    }

    #[test]
    fn retry_etxtbsy_does_not_swallow_other_errors() {
        // A non-ETXTBSY error returns immediately, no retry.
        let mut calls = 0u32;
        let r: std::io::Result<u8> = retry_etxtbsy(|| {
            calls += 1;
            Err(std::io::Error::from_raw_os_error(libc::ENOENT))
        });
        assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::ENOENT));
        assert_eq!(calls, 1, "a non-ETXTBSY error must not retry");
    }

    #[test]
    fn retry_etxtbsy_gives_up_after_max_retries() {
        // Persistent ETXTBSY surfaces after the bounded retry budget (1 initial
        // + 5 retries = 6 calls) rather than spinning forever.
        let mut calls = 0u32;
        let r: std::io::Result<u8> = retry_etxtbsy(|| {
            calls += 1;
            Err(std::io::Error::from_raw_os_error(libc::ETXTBSY))
        });
        assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::ETXTBSY));
        assert_eq!(calls, 6, "1 initial attempt + MAX_RETRIES(5)");
    }

    // A GLM/zai loop pins ANTHROPIC_BASE_URL + ANTHROPIC_AUTH_TOKEN. The pick
    // would scrub both and pin a claude config dir, silently moving the run to a
    // different provider mid-flight while claiming only to have picked an
    // account.
    #[test]
    fn a_pick_that_would_scrub_a_pinned_route_is_declined() {
        let picked = vec![
            pair("ANTHROPIC_BASE_URL", ""),
            pair("ANTHROPIC_AUTH_TOKEN", ""),
            pair("CLAUDE_CONFIG_DIR", "/alt"),
        ];
        let routed = vec![
            pair(
                "ANTHROPIC_BASE_URL",
                "https://open.bigmodel.cn/api/anthropic",
            ),
            pair("OUTPUT_FILE", "/tmp/out"),
        ];
        assert!(pick_would_undo_a_route(&picked, &routed));
    }

    #[test]
    fn an_unrouted_loop_still_gets_its_pick() {
        // The empty-pick key must be a key NO shell exports: the predicate also
        // consults the live process env, so ANTHROPIC_BASE_URL here made the
        // test fail on any machine that routes through a gateway (the value is
        // ambient, not part of the fixture).
        let picked = vec![
            pair("FNO_TEST_SYNTHETIC_UNROUTED", ""),
            pair("CLAUDE_CONFIG_DIR", "/alt"),
        ];
        let plain = vec![pair("OUTPUT_FILE", "/tmp/out"), pair("CLI", "claude")];
        assert!(!pick_would_undo_a_route_with(&picked, &plain, |_| None));
    }

    #[test]
    fn an_ambient_export_blocks_the_pick_the_static_env_never_named() {
        // The half of the picture the static list cannot see: a shell that
        // already exported the route var. A provider-routed lane must still
        // decline the pick even though nothing in static_env names it.
        let picked = vec![pair("ANTHROPIC_BASE_URL", "")];
        let plain = vec![pair("OUTPUT_FILE", "/tmp/out")];
        assert!(pick_would_undo_a_route_with(&picked, &plain, |k| {
            (k == "ANTHROPIC_BASE_URL").then(|| std::ffi::OsString::from("https://routed.example"))
        }));
    }

    #[test]
    fn only_the_clear_list_blocks_a_pick_not_the_pin_itself() {
        // CLAUDE_CONFIG_DIR arrives with a VALUE, so it is not part of the
        // clear-list and must not make every pick look like a route conflict.
        let picked = vec![pair("CLAUDE_CONFIG_DIR", "/alt")];
        let same_key = vec![pair("CLAUDE_CONFIG_DIR", "/other")];
        assert!(!pick_would_undo_a_route(&picked, &same_key));
    }

    #[test]
    fn a_picked_account_yields_its_config_dir() {
        let env = interpret_pick(true, "CLAUDE_CONFIG_DIR=/Users/x/.claude-alt\n", "")
            .expect("a pinned config dir is a successful pick");
        assert_eq!(
            env,
            vec![(
                "CLAUDE_CONFIG_DIR".to_string(),
                "/Users/x/.claude-alt".to_string()
            )]
        );
    }

    // The scrub half of the overlay: an inherited ANTHROPIC_API_KEY or routed
    // base URL outranks CLAUDE_CONFIG_DIR, so applying only the pin would let
    // the worker bill an account the receipt does not name.
    #[test]
    fn auth_vars_to_clear_are_carried_as_empty_values() {
        let stdout = "ANTHROPIC_API_KEY=\nANTHROPIC_BASE_URL=\nCLAUDE_CONFIG_DIR=/alt\n";
        let env = interpret_pick(true, stdout, "").expect("overlay parses");
        assert_eq!(env.len(), 3);
        assert_eq!(env[0], ("ANTHROPIC_API_KEY".to_string(), String::new()));
        assert_eq!(env[1], ("ANTHROPIC_BASE_URL".to_string(), String::new()));
        assert_eq!(
            env[2],
            ("CLAUDE_CONFIG_DIR".to_string(), "/alt".to_string())
        );
    }

    // AC: the opt-in is honored on this path too. Exit 5 is the verb declining
    // because providers.quota.pick_on_launch is false, and it must read as an
    // ordinary "not picked", never as a pick.
    #[test]
    fn a_disarmed_picker_declines_with_its_reason() {
        let stderr = "pick: launch picking is not armed (providers.quota.pick_on_launch = false)\n";
        assert_eq!(
            interpret_pick(false, "", stderr),
            Err(
                "pick: launch picking is not armed (providers.quota.pick_on_launch = false)"
                    .to_string()
            )
        );
    }

    // AC13-ERR: a refusing picker is an answer, not a wedge. The reason reaches
    // the log so an operator can tell "exhausted" from "not set up".
    #[test]
    fn a_refusal_surfaces_its_reason_instead_of_erroring_out() {
        let stderr = "  readyrule: exhausted\npick: every launchable candidate is exhausted\n";
        assert_eq!(
            interpret_pick(false, "", stderr),
            Err("pick: every launchable candidate is exhausted".to_string())
        );
        assert!(interpret_pick(false, "", "").is_err());
    }

    // A receipt must never carry credential material. For an api_key account
    // the pin IS the secret, so the announcement names the KEY and stops; only
    // CLAUDE_CONFIG_DIR, a filesystem path, is safe to echo. This pins the
    // decision the receipt code makes, so a later edit cannot re-introduce the
    // value into the log.
    #[test]
    fn only_the_config_dir_pin_is_safe_to_echo() {
        assert_eq!(PICKED_ENV_KEY, "CLAUDE_CONFIG_DIR");
        for secret_key in ["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"] {
            assert_ne!(
                secret_key, PICKED_ENV_KEY,
                "a secret-bearing pin must not take the echo-the-value branch"
            );
        }
    }

    #[test]
    fn unparseable_output_is_declined_rather_than_guessed() {
        // A drifted verb must not have its output half-applied: with no pinned
        // config dir there is no account, only a scrubbed environment.
        assert!(interpret_pick(true, "readyrule\n", "").is_err());
        assert!(interpret_pick(true, "CLAUDE_CONFIG_DIR=\n", "").is_err());
        assert!(interpret_pick(true, "ANTHROPIC_API_KEY=\n", "").is_err());
        assert!(interpret_pick(true, "=/tmp\n", "").is_err());
        assert!(interpret_pick(true, "", "").is_err());
    }

    // AC12-CON: one call site, all harnesses. A picker call inside any driver
    // lib would be a second copy of one decision - the shape this wiring exists
    // to avoid.
    #[test]
    fn no_driver_lib_calls_the_picker_itself() {
        let lib_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/lib");
        let mut checked = 0;
        for entry in std::fs::read_dir(&lib_dir).expect("scripts/lib is readable") {
            let path = entry.expect("dir entry").path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if !name.starts_with("driver-") || !name.ends_with(".sh") {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("driver lib is readable");
            assert!(
                !body.contains("providers pick"),
                "{name} calls the picker itself; the loop dispatcher is the one call site"
            );
            checked += 1;
        }
        // Positive control: a glob that stops matching would otherwise pass
        // vacuously and report coverage it does not have.
        assert!(checked >= 4, "expected the driver libs, scanned {checked}");
    }

    // AC1-EDGE: opencode resolves to the `opencode` binary (loop-wrapper path,
    // x-6007). The loop-wrapper drivers have fixed binary names (no env/alias
    // precedence, unlike claude-code).
    #[test]
    fn loop_wrapper_drivers_resolve_to_fixed_binaries() {
        assert_eq!(resolve_driver_binary("opencode", None), "opencode");
        assert_eq!(resolve_driver_binary("openclaw", None), "openclaw");
        assert_eq!(resolve_driver_binary("hermes", None), "hermes-agent");
    }
}
