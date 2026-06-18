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
/// `driver`: one of `claude-code`, `hermes`, `openclaw` (whitelist-enforced).
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
    const ALLOWED: &[&str] = &["claude-code", "hermes", "openclaw"];
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

// ── ShelloutDispatcher ────────────────────────────────────────────────────────

/// A live session wrapping a bash `driver_invoke` child process.
pub struct ShelloutSession {
    child: Child,
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

        let child = cmd
            .spawn()
            .map_err(|e| LoopError::Dispatch(format!("spawn bash driver_invoke: {e}")))?;

        Ok(Box::new(ShelloutSession { child }))
    }
}
