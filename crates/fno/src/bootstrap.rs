//! The Python-CLI forwarding path (the original `fno` cargo bootstrapper).
//!
//! Any `fno <args>` invocation that is not a mux role (see `main.rs`
//! role-select) lands here. The job is unchanged from the pre-mux shim: make
//! the *real* `fno` CLI (the Python Typer CLI plus the three `fno-agents*`
//! Rust binaries, shipped as the `fno` PyPI wheel) available and then forward
//! to it. The CLI itself is never reimplemented here (foundation Locked
//! Decision 12).
//!
//! First-run flow:
//!   1. ensure `uv` is present (download Astral's standalone uv if absent),
//!   2. `uv tool install fno` (the PyPI platform wheel, binaries bundled),
//!   3. verify the installed package is *ours* before running it,
//!   4. `exec` the wheel's `fno-py` console script by ABSOLUTE path.
//!
//! Subsequent runs read a sentinel and forward immediately - no network.
//!
//! A provision that installs cleanly but yields no usable `fno` writes no
//! sentinel (there is no verified binary to point at), so it also records a
//! short-lived failure stamp beside it. Without that, every later invocation
//! repeats the whole `uv tool install --force` to reach the same error, turning a
//! one-time breakage into a permanent per-call tax. The stamp is keyed to a
//! digest of the install source, so changing what you install from takes effect
//! immediately and a credential-bearing source is never written to the cache.
//!
//! The wheel's Python CLI ships as the `fno-py` console script (this Rust
//! binary owns `fno`), and the shim execs it by absolute path, NEVER via a PATH
//! lookup. Two guards against a self-loop, either sufficient: the target is a
//! different name (`fno-py`, not `fno`), and it is reached by absolute path, not
//! a PATH search - so it holds even when `~/.cargo/bin` and uv's tool bin both
//! carry an `fno`.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// A bootstrap failure: a human-facing message plus the exit code to use.
/// Every failure path produces one of these so the shim never panics on an
/// expected condition (no network, foreign package, exec failure).
#[derive(Debug)]
struct BootErr {
    msg: String,
    code: i32,
}

impl BootErr {
    fn new(code: i32, msg: impl Into<String>) -> Self {
        BootErr {
            msg: msg.into(),
            code,
        }
    }
}

type BootResult<T> = Result<T, BootErr>;

/// The uv tool name for the wheel. `uv tool install fno` (by name) and
/// `uv tool install /path/fno-*.whl` (local wheel) both register the tool as
/// `fno`, so resolution keys on this constant either way.
const TOOL_NAME: &str = "fno";

/// Forward `args` to the provisioned wheel `fno`. Diverges: on success the
/// process is replaced via exec; on failure it prints the error and exits.
pub fn forward(args: &[OsString]) -> ! {
    match run(args) {
        // run() either execs (diverges) or returns an error; Ok is unreachable.
        Ok(()) => unreachable!("run() must exec the wheel fno or return an error"),
        Err(e) => {
            eprintln!("fno: {}", e.msg);
            std::process::exit(e.code);
        }
    }
}

fn run(args: &[OsString]) -> BootResult<()> {
    // Fast path: a recorded sentinel from a prior successful provision. No uv
    // call, no network - the common case after first run (AC4-HP). The sentinel
    // also records the mtime of the binary at the moment we verified it: an
    // UNCHANGED binary is the one we already vouched for, so we forward
    // instantly; a CHANGED binary (e.g. a same-path `uv tool install --force`
    // of a different package) is re-verified before exec, so the "never run a
    // foreign fno" invariant still holds after the first bootstrap.
    if let Some((real, recorded_mtime)) = read_sentinel() {
        if is_executable(&real) {
            if file_mtime(&real) == Some(recorded_mtime) {
                return Err(exec_real(&real, args)); // unchanged: already verified
            }
            // Changed (or mtime unreadable): re-verify before trusting it again.
            if verify_ours(&real).is_ok() {
                return Err(record_and_exec(&real, args));
            }
            // A foreign package now sits at our path: drop the sentinel and fall
            // through to re-provision (which re-verifies and aborts on mismatch).
            let _ = fs::remove_file(sentinel_path());
        } else {
            // Stale sentinel (wheel uninstalled): drop it and re-provision.
            let _ = fs::remove_file(sentinel_path());
        }
    }

    // Already provisioned by another channel (`uv tool install fno`, or a
    // pip install that uv can see) but no sentinel yet - adopt it without a
    // redundant reinstall (AC4-EDGE). Still verify before trusting it (AC3).
    if let Some(real) = resolve_via_uv_tool_dir() {
        if is_executable(&real) {
            verify_ours(&real)?;
            return Err(record_and_exec(&real, args));
        }
    }

    // Resolve the install source before anything expensive. It decides both what
    // we are about to install and whether a remembered failure still applies, and
    // resolving it first means a bad `config.dev.source` pin errors without first
    // downloading uv.
    let source = install_source(
        env::var("FNO_BOOTSTRAP_WHEEL").ok().as_deref(),
        read_dev_source_pin().as_deref(),
    )?;

    // A provision that installs cleanly but yields no usable fno leaves nothing
    // behind: the sentinel is written only by `record_and_exec`, which every
    // failure path below returns before reaching. So the next invocation repeats
    // the whole `uv tool install --force` from scratch, and the banner's "one
    // time" becomes false forever. Remember the failure briefly instead, so a
    // broken environment costs one reinstall rather than one per call. Both fast
    // paths run ABOVE this, so an install that starts working is adopted
    // immediately and the cooldown never delays a recovery.
    if let Some(msg) = read_failure_stamp(&source) {
        return Err(BootErr::new(1, msg));
    }

    // Provision. Progress line BEFORE the slow step so the first run never
    // looks like a hang (AC1-UI).
    let uv = ensure_uv()?;
    eprintln!(
        "fno: first run - provisioning the fno CLI via uv (one time, may take a few seconds)..."
    );
    install_wheel(&uv, &source)?;

    let real = match resolve_via_uv_tool_dir() {
        Some(p) if is_executable(&p) => p,
        candidate => {
            // Name the path we built, why we rejected it, and what we installed
            // from. Without those three facts the failure is unfalsifiable from
            // the terminal, and the generic message sent two separate diagnoses
            // down the wrong path.
            let present = candidate.as_deref().is_some_and(|p| p.exists());
            let msg = locate_failure_message(
                candidate.as_deref(),
                present,
                &source,
                diagnose_locate_failure(),
            );
            write_failure_stamp(&source, &msg);
            return Err(BootErr::new(1, msg));
        }
    };
    if let Err(e) = verify_ours(&real) {
        // Same reasoning: a foreign package at our path is a stable condition, so
        // re-downloading 18 packages to reach the same refusal helps nobody.
        write_failure_stamp(&source, &e.msg);
        return Err(e);
    }
    Err(record_and_exec(&real, args))
}

/// Compose the post-install locate-failure message. Pure (the caller does the
/// filesystem probe) so the wording is unit-testable. `candidate` is the console
/// script path we constructed, or `None` when `uv tool dir` itself was unreadable
/// and no path could be built; `present` is whether anything exists there;
/// `stale` is the optional stale-wheel diagnosis, which stays the closing remedy
/// when it applies because it names a concrete fix.
fn locate_failure_message(
    candidate: Option<&Path>,
    present: bool,
    source: &str,
    stale: Option<String>,
) -> String {
    let mut msg = String::from("provisioned the wheel but could not locate the installed fno.");
    // Which rung installed matters: `fno` is the PyPI default, anything else is a
    // local checkout or wheel from `config.dev.source` / FNO_BOOTSTRAP_WHEEL, and
    // the two fail for entirely different reasons. Redacting HERE rather than at
    // the call site means no future caller can leak a credential-bearing source
    // by forgetting to.
    msg.push_str(&format!("\n  installed from: {}", redact_source(source)));
    match candidate {
        Some(p) => {
            msg.push_str(&format!("\n  looked for: {}", p.display()));
            msg.push_str(if present {
                "\n  rejected because: something is there but it is not an executable file"
            } else {
                "\n  rejected because: nothing exists at that path"
            });
        }
        None => msg
            .push_str("\n  looked for: (no path built - `uv tool dir` failed or printed nothing)"),
    }
    match stale {
        Some(s) => {
            msg.push('\n');
            msg.push_str(&s);
        }
        None => msg.push_str(
            "\nInstall it manually to see uv's own error: \
             `uv tool install --force --compile-bytecode fno`, or from a checkout: \
             `uv tool install --force --compile-bytecode --from <repo>/cli fno`",
        ),
    }
    msg
}

// ---------------------------------------------------------------------------
// uv discovery + install
// ---------------------------------------------------------------------------

/// Locate a usable `uv`: prefer one on PATH, else the well-known install dirs
/// Astral's installer uses. Returns the command to invoke (`uv` when on PATH,
/// otherwise an absolute path).
fn find_uv() -> Option<PathBuf> {
    if Command::new("uv")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Some(PathBuf::from("uv"));
    }
    uv_fallback_paths()
        .into_iter()
        .find(|cand| is_executable(cand))
}

fn uv_fallback_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = home_dir() {
        v.push(home.join(".local/bin/uv"));
        v.push(home.join(".cargo/bin/uv"));
    }
    v
}

/// Ensure `uv` is available, downloading Astral's standalone installer if not.
/// A failed download exits non-zero with an actionable message (AC1-ERR).
fn ensure_uv() -> BootResult<PathBuf> {
    if let Some(uv) = find_uv() {
        return Ok(uv);
    }
    eprintln!("fno: uv not found - installing the standalone uv (one time)...");
    // Astral's published installer; a single static binary, no Python needed.
    let status = Command::new("sh")
        .arg("-c")
        .arg("curl -LsSf https://astral.sh/uv/install.sh | sh")
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => {
            return Err(BootErr::new(
                1,
                "could not install uv (network unreachable or the installer failed). \
                 Install uv from https://docs.astral.sh/uv/ and re-run.",
            ));
        }
    }
    find_uv().ok_or_else(|| {
        BootErr::new(
            1,
            "uv installed but is not on PATH; add ~/.local/bin to PATH and re-run, \
             or install uv from https://docs.astral.sh/uv/.",
        )
    })
}

/// `uv tool install <source>`, where `source` was resolved by [`install_source`]:
/// `fno` (PyPI by name) by default, or `FNO_BOOTSTRAP_WHEEL` (a local wheel path
/// or any uv install spec) so the channel is testable before the PyPI publish
/// lands, or a maintainer's pinned checkout (`config.dev.source`) so editing
/// source never re-provisions the stale published wheel. Every rung registers the
/// tool under the same uv name (`TOOL_NAME`), so resolution is source-agnostic.
fn install_wheel(uv: &Path, source: &str) -> BootResult<()> {
    // --force so a half-built or stale tool venv is repaired rather than failing
    // with "already installed" (AC4-FR: never trust a half-provisioned state).
    // We only reach here when no usable install was found, so --force never does
    // a redundant reinstall over a healthy one.
    // --compile-bytecode so the venv ships its own .pyc and no later process
    // (pr-watch, hooks, any caller) writes into a tree a reinstall may be
    // deleting; see docs/architecture/cli-lazy-imports.md.
    //
    // The install is retried on exactly one failure signature: ENOTEMPTY from
    // uv's removal walk racing a concurrent importer's bytecode rewrite (the
    // residual window the doc names). Any other non-zero exit - auth, network,
    // disk - fails immediately, because retrying it would only reprint the same
    // error. Each attempt's uv output is re-emitted verbatim, and success is
    // accepted only after a positive marker (entrypoint + shipped bytecode),
    // never on the exit code alone.
    for attempt in 1..=INSTALL_ATTEMPTS {
        let out = match Command::new(uv)
            .args(["tool", "install", "--force", "--compile-bytecode", source])
            .env("NO_COLOR", "1")
            .env("UV_NO_COLOR", "1")
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                return Err(BootErr::new(
                    1,
                    format!("could not run uv to install the fno wheel: {e}"),
                ))
            }
        };
        // Re-emit both streams exactly as an inherited-status run would have,
        // so the user sees uv's own error (the better pointer) on every attempt.
        let _ = std::io::stdout().write_all(&out.stdout);
        let _ = std::io::stderr().write_all(&out.stderr);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if out.status.success() {
            return match install_verified_within(uv, VERIFY_ATTEMPTS, VERIFY_POLL) {
                Ok(()) => Ok(()),
                Err(m) => Err(BootErr::new(
                    1,
                    format!("uv reported success but the install does not verify: {m}"),
                )),
            };
        }
        let code = out.status.code().unwrap_or(1);
        let raced = stderr_is_enotempty(&stderr);
        if !raced || attempt == INSTALL_ATTEMPTS {
            // A capped race gets the same words the fno.sh / postinstall twins
            // die with, so every provisioning path explains it identically; any
            // other failure defers to uv's own error, printed just above.
            let msg = if raced {
                format!("{RACE_CAP_MSG}\n{}", install_failure_message(source))
            } else {
                install_failure_message(source)
            };
            return Err(BootErr::new(code, msg));
        }
        // Give the racing importer a moment to finish its pass before the
        // next removal walk starts under it.
        thread::sleep(Duration::from_millis(300));
    }
    unreachable!("the loop returns on success, non-signature failure, or final attempt")
}

/// How many times `install_wheel` may run uv before giving up. Three: one
/// real attempt plus two retries, enough to absorb the measured intermittent
/// race without turning a genuinely broken environment into a slow one.
const INSTALL_ATTEMPTS: u32 = 3;

/// What a capped ENOTEMPTY race means and what to do about it. Word-for-word
/// the message `scripts/install/fno.sh`, the plugin postinstall, and the
/// `fno update` shell wrapper print, so the remedy does not depend on which
/// provisioning path the user happened to hit.
const RACE_CAP_MSG: &str = "uv tool install hit the directory race (os error 66) three times. \
     A concurrent fno process is rewriting bytecode into the venv mid-removal. \
     Stop fno processes and re-run.";

/// The retryable signature: uv's closing `rmdir` hit a directory a concurrent
/// writer refilled behind its walk. Pure for testing.
fn stderr_is_enotempty(stderr: &str) -> bool {
    stderr.contains("Directory not empty") && stderr.contains("os error 66")
}

/// Ceiling and poll interval for [`install_verified_within`].
///
/// 15 * 200ms = 3s, deliberately the SAME budget `update.py`'s `_await_binary`
/// spends on the same file. The two are a pair: one Rust, one emitted shell,
/// guarding the two provisioning paths. They cannot share an implementation
/// across that language boundary, so they share the numbers and a test that
/// pins the shape instead. Change one and change the other.
const VERIFY_ATTEMPTS: u32 = 15;
const VERIFY_POLL: Duration = Duration::from_millis(200);

/// [`install_verified`], retried on a disk RE-CHECK until it passes or the
/// budget runs out.
///
/// Why this exists: uv exits before its own artifacts settle. The console
/// script `<tools>/fno/bin/fno-py` is deleted and recreated across an install,
/// and `docs/architecture/cli-lazy-imports.md` measured it absent for ~490ms,
/// with the gap closing only ~40ms before uv exited on an idle machine. A
/// verify firing the instant uv returns therefore races the install it is
/// verifying and reports the script missing while it is about to appear. That
/// is the whole bug: `fno` refused every verb, ran a full reinstall each time,
/// and told the operator the install did not verify.
///
/// `update.py` was given a bounded wait for exactly this race. This is that
/// remedy reaching the second provisioning path, which never got one.
///
/// This is NOT the sleep-retry the doc rejects. That rejection is about waiting
/// on an ABSENT module in the hope it appears; the doc draws the line itself,
/// at whether the disk is re-checked. This re-runs the full predicate every
/// pass and returns the moment it passes, so a genuinely broken install still
/// fails, with the same message it always did plus what we waited.
fn install_verified_within(uv: &Path, attempts: u32, poll: Duration) -> Result<(), String> {
    let mut last = install_verified(uv);
    for _ in 0..attempts {
        if last.is_ok() {
            return last;
        }
        thread::sleep(poll);
        last = install_verified(uv);
    }
    last.map_err(|m| {
        let waited = poll.as_millis() * u128::from(attempts);
        format!("{m} (still absent after waiting {waited}ms for uv's own artifact)")
    })
}

/// Confirm the install with markers only a real provisioned venv produces:
/// the `fno-py` console script AND shipped bytecode under its lib tree. A
/// zero exit from uv proves nothing about what landed.
///
/// Pure and single-shot on purpose: [`install_verified_within`] owns the
/// waiting, so this stays a predicate a test can drive against a fixed tree.
fn install_verified(uv: &Path) -> Result<(), String> {
    let tool_dir = uv_tool_dir(uv).ok_or("uv tool dir unreadable")?;
    let venv = tool_dir.join(TOOL_NAME);
    let entry = venv.join("bin").join("fno-py");
    if !entry.exists() {
        return Err(format!("no console script at {}", entry.display()));
    }
    let lib = venv.join("lib");
    if count_pyc(&lib) == 0 {
        return Err(format!(
            "no compiled bytecode under {} (--compile-bytecode install ships it)",
            lib.display()
        ));
    }
    Ok(())
}

/// Count `*.pyc` files under `dir`, recursively. Pure filesystem, testable.
fn count_pyc(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            n += count_pyc(&path);
        } else if path.extension().is_some_and(|e| e == "pyc") {
            n += 1;
        }
    }
    n
}

/// `uv tool dir` as a PathBuf, or None when uv is absent/fails. Shared by the
/// entrypoint resolution and the post-install marker check.
fn uv_tool_dir(uv: &Path) -> Option<PathBuf> {
    let out = Command::new(uv)
        .args(["tool", "dir"])
        .env("NO_COLOR", "1")
        .env("UV_NO_COLOR", "1")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let tool_dir = strip_ansi(raw.trim());
    if tool_dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(tool_dir))
}

/// Compose the install-failure message. Pure (install_wheel captures uv's
/// output and re-emits it verbatim before returning) so the wording is
/// unit-testable, and `redact_source` is applied HERE so no caller can leak a
/// credential-bearing source by forgetting to.
///
/// This used to say "Check your network / PyPI access". uv exits nonzero for
/// plenty of reasons that are not the network -- the case that prompted this was
/// `failed to remove directory .../lib: Directory not empty (os error 66)`,
/// re-emitted verbatim by install_wheel after every attempt. That signature now
/// has a cause (bytecode writes racing the removal walk) and a bounded retry in
/// install_wheel; `--compile-bytecode` shrinks the window but does not close it
/// (docs/architecture/cli-lazy-imports.md), so uv's own error above is the
/// pointer for the failures we have NOT yet diagnosed.
fn install_failure_message(source: &str) -> String {
    format!(
        "`uv tool install {}` failed; uv's own error is printed above. \
         Re-run it manually to reproduce it (re-supplying any credential your \
         FNO_BOOTSTRAP_WHEEL carries).",
        redact_source(source)
    )
}

/// Choose the `uv tool install` source across three rungs of precedence, pure
/// for testing:
///   1. `FNO_BOOTSTRAP_WHEEL` (`override_val`) when set and non-empty.
///   2. a maintainer's `config.dev.source` pin (`pin`) when set: validated and
///      expanded to its `<checkout>/cli` build dir.
///   3. `"fno"` (PyPI by name; the end-user default, byte-identical to before).
/// A set-but-invalid pin is an error, never a silent PyPI downgrade: a
/// maintainer who pinned source WANTS to know it is broken, not be handed a
/// months-stale wheel (US3/AC3).
fn install_source(override_val: Option<&str>, pin: Option<&str>) -> BootResult<String> {
    if let Some(v) = override_val {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
    }
    if let Some(p) = pin {
        let p = p.trim();
        if !p.is_empty() {
            return resolve_pin(p);
        }
    }
    Ok("fno".to_string())
}

/// Validate a pinned checkout and return its `uv tool install` source
/// (`<checkout>/cli`, the same wheel-build path `fno update` uses, so the venv
/// ships `fno-py`). Validity is the strict "`cli/pyproject.toml` present" check
/// so a pin at the repo root (missing the `cli/` subdir) fails rather than
/// silently building nothing. A bad pin errors naming `config.dev.source` and
/// an escape hatch, never falling through to PyPI (US3/AC3).
fn resolve_pin(pin: &str) -> BootResult<String> {
    // A config value like `~/src/fno` is common; PathBuf::from won't expand it.
    let cli = expand_tilde(pin).join("cli");
    if cli.join("pyproject.toml").is_file() {
        return Ok(cli.to_string_lossy().into_owned());
    }
    Err(BootErr::new(
        1,
        format!(
            "config.dev.source points at '{pin}', which is not an fno checkout \
             (no cli/pyproject.toml). Fix it (`fno config set config.dev.source \
             <checkout>`), clear it (`fno config unset config.dev.source`), or \
             bypass it once (`FNO_BOOTSTRAP_WHEEL=fno`)."
        ),
    ))
}

/// Expand a leading `~`/`~/` to `$HOME` (Rust does not; a config pin like
/// `~/src/fno` is common). `~user` and non-tilde paths pass through literally.
fn expand_tilde(p: &str) -> PathBuf {
    expand_tilde_with(p, home_dir())
}

/// Pure core of `expand_tilde` (home injected for testing without touching the
/// process-global `$HOME`). No home resolvable -> the value passes through.
fn expand_tilde_with(p: &str, home: Option<PathBuf>) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        return home
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(p));
    }
    if p == "~" {
        return home.unwrap_or_else(|| PathBuf::from(p));
    }
    PathBuf::from(p)
}

/// Read the `config.dev.source` pin from `~/.fno/config.toml`, fno-free: we are
/// in recovery precisely because `fno` is broken, so shelling `fno config get`
/// is impossible. Global config only (the bootstrap runs independent of cwd).
/// Best-effort: an absent or malformed file is "no pin" (US2/AC2-ERR).
fn read_dev_source_pin() -> Option<String> {
    let cfg = home_dir()?.join(".fno/config.toml");
    parse_dev_source(&fs::read_to_string(cfg).ok()?)
}

/// Parse `[dev].source` from a flat config.toml body. Pure (mirrors
/// `digest_overlay::read_mux_value`); malformed toml, absent key, or an
/// empty/whitespace value all resolve to `None`.
fn parse_dev_source(content: &str) -> Option<String> {
    let t = content.parse::<toml::Table>().ok()?;
    match t.get("dev")?.as_table()?.get("source")? {
        toml::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Resolution: find the wheel `fno` absolute path
// ---------------------------------------------------------------------------

/// Resolve the wheel Python CLI console script inside uv's tool venv:
/// `<uv tool dir>/fno/bin/fno-py`. The wheel's `[project.scripts]` names the
/// Python CLI `fno-py` (this Rust binary owns `fno`), so the forward target is
/// `fno-py`, never `fno` - which is what makes the self-loop impossible by
/// construction: this shim is `fno`, the thing it execs is `fno-py`, a
/// different name even when both live on PATH. Returns `None` when uv is absent
/// or the tool dir cannot be read; the caller then provisions.
fn resolve_via_uv_tool_dir() -> Option<PathBuf> {
    let uv = find_uv()?;
    let tool_dir = uv_tool_dir(&uv)?;
    Some(
        tool_dir
            .join(TOOL_NAME)
            .join("bin")
            // The console script is `fno-py` (see the wheel's [project.scripts]);
            // TOOL_NAME above is the uv *tool* name (still `fno`), which is a
            // different axis from the script name.
            .join("fno-py"),
    )
}

// ---------------------------------------------------------------------------
// Stale-wheel diagnostics: a successful install with no `fno-py` script
// ---------------------------------------------------------------------------

/// Probe the uv tool venv after the post-install locate failed and, when the
/// cause is a published wheel too old to ship `fno-py`, return an actionable
/// message. Returns None when nothing is readable - the caller then falls back
/// to the generic locate error.
fn diagnose_locate_failure() -> Option<String> {
    let uv = find_uv()?;
    let out = Command::new(&uv)
        .args(["tool", "dir"])
        .env("NO_COLOR", "1")
        .env("UV_NO_COLOR", "1")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let tool_dir = strip_ansi(String::from_utf8_lossy(&out.stdout).trim());
    if tool_dir.is_empty() {
        return None;
    }
    let bin = PathBuf::from(tool_dir).join(TOOL_NAME).join("bin");
    // The pre-rename wheel ships `bin/fno`; the current one ships `bin/fno-py`.
    let pre_rename_script = bin.join("fno").exists();
    let version = read_installed_version(&bin);
    stale_wheel_message(pre_rename_script, version.as_deref())
}

/// Read the installed `fno` version from the tool venv's own metadata. None when
/// the venv python or the metadata is unreadable.
fn read_installed_version(bin: &Path) -> Option<String> {
    let python = bin.join("python");
    if !is_executable(&python) {
        return None;
    }
    let out = Command::new(&python)
        .args([
            "-c",
            "import importlib.metadata as m; print(m.version('fno'))",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// A published version predating the `fno`->`fno-py` rename, i.e. one that ships
/// no `fno-py` script. `fno-py` first ships in 0.3.0, so the pre-rename line is
/// everything under it: 0.0.x / 0.1.x / 0.2.x (the only versions that ever
/// existed before this release). A prefix check is enough for that real history
/// and avoids a semver dependency.
fn is_pre_rename_version(v: &str) -> bool {
    v.starts_with("0.0.") || v.starts_with("0.1.") || v.starts_with("0.2.")
}

/// Pure locate-failure classifier, unit-tested without a venv: the caller does
/// the venv probe and passes the results. `pre_rename_script` is whether the old
/// `bin/fno` script is present; `installed_version` is the venv-reported version.
/// Returns the stale-wheel message only when the evidence says the wheel is
/// genuinely pre-rename (old `bin/fno` present, or a pre-0.3.0 version); a modern
/// version that merely lacks `fno-py` is a broken install, not a stale wheel, so
/// it falls through to None and the caller keeps the honest generic error.
fn stale_wheel_message(pre_rename_script: bool, installed_version: Option<&str>) -> Option<String> {
    let is_stale = pre_rename_script || installed_version.is_some_and(is_pre_rename_version);
    if !is_stale {
        return None;
    }
    let head = match installed_version {
        Some(v) => format!("the published fno wheel ({v}) predates this shim"),
        None => "the published fno wheel predates this shim".to_string(),
    };
    Some(format!(
        "{head} - it has no fno-py script.\n\
         A newer fno release must be published; meanwhile install from source:\n\
         uv tool install --force --compile-bytecode --from <repo>/cli fno"
    ))
}

// ---------------------------------------------------------------------------
// Identity verification (AC3): never run a foreign `fno`
// ---------------------------------------------------------------------------

/// Verify the installed `fno` is THIS project's package before executing it.
/// Probes the tool venv's own Python via `importlib.metadata`, keying on a
/// package-specific signal we own (the author), not merely the binary name
/// `fno` which a squatter could also publish (AC3-EDGE). On a mismatch it
/// aborts without recording the sentinel (AC3-ERR / AC3-FR).
fn verify_ours(real: &Path) -> BootResult<()> {
    let venv_python = real
        .parent()
        .map(|bin| bin.join("python"))
        .filter(|p| is_executable(p))
        .ok_or_else(|| {
            BootErr::new(
                1,
                "cannot verify the installed fno: its tool venv python is missing; \
                 refusing to run an unverified fno.",
            )
        })?;

    // Fall back to `Author-email` when `Author` is absent: a PEP 621 author
    // with an email (`{name, email}`) makes the build backend emit only
    // `Author-email: Jason Noah Choi <...>` and drop the bare `Author` field.
    // The owner's name travels in both, so the substring match in
    // decide_identity still holds and a routine pyproject edit can't lock the
    // legitimate package out.
    let probe = "import importlib.metadata as m\n\
                 md = m.metadata('fno')\n\
                 print(md['Name'])\n\
                 print(md.get('Author') or md.get('Author-email') or '')\n\
                 print(md['Version'])\n";
    let out = Command::new(&venv_python)
        .args(["-c", probe])
        .output()
        .map_err(|e| BootErr::new(1, format!("could not run the identity probe: {e}")))?;
    if !out.status.success() {
        return Err(BootErr::new(
            1,
            "the installed fno has no readable package metadata; \
             refusing to run an unverified fno.",
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let name = lines.next().unwrap_or("").trim();
    let author = lines.next().unwrap_or("").trim();
    let version = lines.next().unwrap_or("").trim();

    decide_identity(name, author).map_err(|why| {
        BootErr::new(
            1,
            format!(
                "the installed `fno` is not this project's package ({why}); \
                 refusing to run a foreign fno."
            ),
        )
    })?;
    // Report what we accepted so the user can audit what ran (AC3-UI).
    eprintln!("fno: verified fno {version} (this project's package).");
    Ok(())
}

/// Pure identity decision: the package must be named `fno` AND authored by this
/// project's owner. Factored out so the accept/reject rule is unit-testable.
fn decide_identity(name: &str, author: &str) -> Result<(), String> {
    if !name.eq_ignore_ascii_case("fno") {
        return Err(format!("name={name}"));
    }
    if !author.contains("Jason Noah Choi") {
        return Err(format!("author={author}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Exec
// ---------------------------------------------------------------------------

/// Replace this process with the wheel `fno` at `real`. On success this never
/// returns (signals + exit code pass through unchanged); it only returns when
/// the exec itself fails, which we surface as a BootErr.
fn exec_real(real: &Path, args: &[OsString]) -> BootErr {
    let err = Command::new(real).args(args).exec();
    BootErr::new(
        126,
        format!(
            "failed to exec the provisioned fno at {}: {err}",
            real.display()
        ),
    )
}

// ---------------------------------------------------------------------------
// Sentinel (fast path)
// ---------------------------------------------------------------------------

fn sentinel_dir() -> PathBuf {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"));
    base.join("fno-bootstrap")
}

fn sentinel_path() -> PathBuf {
    sentinel_dir().join("real-fno")
}

/// Sentinel format: the verified binary's mtime (nanos since epoch) on the
/// first line, then the binary's path as RAW bytes (Unix paths are arbitrary
/// byte sequences, not necessarily UTF-8, so we never round-trip through a lossy
/// `String`). Returns `(path, recorded_mtime)`.
fn read_sentinel() -> Option<(PathBuf, u128)> {
    let bytes = fs::read(sentinel_path()).ok()?;
    let nl = bytes.iter().position(|&b| b == b'\n')?;
    let mtime: u128 = std::str::from_utf8(&bytes[..nl])
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let path_bytes = &bytes[nl + 1..];
    if path_bytes.is_empty() {
        return None;
    }
    Some((PathBuf::from(OsStr::from_bytes(path_bytes)), mtime))
}

/// Best-effort: record the verified wheel path + its mtime so the next run skips
/// uv entirely (and re-verifies only if the binary later changes). A write
/// failure is non-fatal - the next run just re-resolves.
fn write_sentinel(real: &Path, mtime: u128) {
    let path = sentinel_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut buf = format!("{mtime}\n").into_bytes();
    buf.extend_from_slice(real.as_os_str().as_bytes());
    let _ = fs::write(&path, buf);
}

/// The binary's mtime as nanos since the Unix epoch, or `None` if unreadable.
/// Used to detect a same-path reinstall so the fast path can re-verify it.
fn file_mtime(p: &Path) -> Option<u128> {
    fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos())
}

/// Record the (just-verified) binary's path + mtime, then exec it. Diverges on
/// success; returns the exec error otherwise.
fn record_and_exec(real: &Path, args: &[OsString]) -> BootErr {
    if let Some(m) = file_mtime(real) {
        write_sentinel(real, m);
    }
    // We hold a verified, working fno, so any remembered provision failure is
    // moot. Clearing here (the single place we prove success) means a recovered
    // environment never serves a stale refusal.
    clear_failure_stamp();
    exec_real(real, args)
}

// ---------------------------------------------------------------------------
// Provision-failure stamp (negative cache)
// ---------------------------------------------------------------------------

/// How long a failed provision suppresses the next reinstall attempt. Long
/// enough that a scripted burst of `fno` calls pays the 18-package install once,
/// short enough that a walked-away operator is never wedged for long - and the
/// refusal names the file to delete for an immediate retry either way.
const FAILURE_COOLDOWN_SECS: u64 = 600;

/// The provision-failure stamp: a short-lived negative cache beside the
/// sentinel. Its only job is to keep one failed provision from being repeated on
/// every subsequent invocation.
fn failure_stamp_path() -> PathBuf {
    sentinel_dir().join("provision-failed")
}

/// The cache identity of an install source: a digest, never the source itself.
/// `FNO_BOOTSTRAP_WHEEL` may be an authenticated URL, and the stamp is a file in
/// a shared cache dir, so the raw source must never be persisted. Canonicalizing
/// first means the same wheel named relatively from two working directories keys
/// the same, and two different wheels that share a relative name do not.
fn source_key(source: &str) -> String {
    blake3::hash(canonical_source(source).as_bytes())
        .to_hex()
        .to_string()
}

/// Canonicalize a path-shaped source. Only a source containing a separator is a
/// path candidate: canonicalizing a bare spec like `fno` would resolve against
/// the cwd whenever a directory of that name happened to sit there (this repo has
/// one at `crates/fno`), making the by-name rung's key depend on where it ran.
/// A URL and a non-existent path both fail canonicalization and pass through.
fn canonical_source(source: &str) -> String {
    if source.contains('/') {
        if let Ok(p) = fs::canonicalize(source) {
            return p.to_string_lossy().into_owned();
        }
    }
    source.to_string()
}

/// A display-safe rendering of an install source. This string is printed and
/// persisted, and uv accepts a credential in exactly two places within a URL:
/// the userinfo and the query. Both are replaced rather than dropped, so the
/// reader still sees that a credential was in play. A package spec or a
/// filesystem path passes through unchanged: those are what an operator needs in
/// order to recognise which rung ran, and neither is a secret channel.
fn redact_source(source: &str) -> String {
    let Some((scheme, rest)) = source.split_once("://") else {
        return source.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let host = match authority.rsplit_once('@') {
        Some((_userinfo, h)) => format!("<redacted>@{h}"),
        None => authority.to_string(),
    };
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let (path, query) = tail.split_at(path_end);
    let suffix = if query.is_empty() { "" } else { "?<redacted>" };
    format!("{scheme}://{host}{path}{suffix}")
}

fn now_secs() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Read the stamp and, when it is still inside the cooldown AND was recorded for
/// the source we are about to install from, return the message to fail with.
fn read_failure_stamp(source: &str) -> Option<String> {
    let raw = fs::read_to_string(failure_stamp_path()).ok()?;
    let (original, age) = decide_cached_failure(&raw, now_secs()?, &source_key(source))?;
    Some(cached_failure_message(
        &original,
        age,
        &failure_stamp_path(),
    ))
}

/// Pure core of [`read_failure_stamp`]: decide from the raw stamp body, the
/// current time, and the source we are about to install from, returning
/// `(original message, age in seconds)`.
///
/// The stamp is keyed on the install source because the three rungs are separate
/// channels: a PyPI failure says nothing about a local checkout, and a maintainer
/// who repoints `config.dev.source` (or sets `FNO_BOOTSTRAP_WHEEL`) is asking for
/// a genuinely different install and must never be served a refusal about the old
/// one. A source change therefore invalidates the cache immediately.
///
/// An absent, malformed, expired, future-dated, or other-source stamp is `None`
/// so we re-provision normally - a negative cache must never be able to wedge the
/// bootstrap shut, which is why every unreadable shape fails OPEN, not closed.
fn decide_cached_failure(raw: &str, now: u64, source_key: &str) -> Option<(String, u64)> {
    let (header, msg) = raw.split_once('\n')?;
    // Header is `<unix-secs>\t<source-key>`, where the key is a digest so a
    // credential-bearing source is never written here. Neither field can contain
    // a tab, so a malformed header simply fails to parse, i.e. re-provisions.
    let (stamped, stamped_key) = header.split_once('\t')?;
    if stamped_key != source_key {
        return None;
    }
    let stamped: u64 = stamped.trim().parse().ok()?;
    // checked_sub: a stamp dated in the future (clock skew, a restored backup)
    // yields None, i.e. re-provision, never a cooldown that outlives the clock.
    let age = now.checked_sub(stamped)?;
    if age > FAILURE_COOLDOWN_SECS || msg.trim().is_empty() {
        return None;
    }
    Some((msg.to_string(), age))
}

/// The fast-fail text for a remembered provision failure. Pure so the contract
/// that matters - we are deliberately NOT reinstalling, and here is how to retry
/// right now - is unit-testable.
fn cached_failure_message(original: &str, age_secs: u64, stamp: &Path) -> String {
    format!(
        "{original}\n\n\
         (repeat of a provision failure {age_secs}s ago; fno is deliberately not \
         reinstalling on every call.\n\
         Force a fresh attempt now: rm {})",
        stamp.display()
    )
}

/// Best-effort: remember that this provision failed, keyed to a digest of the
/// source it failed for. A write failure is non-fatal - the next run just
/// re-provisions, which is today's behaviour.
fn write_failure_stamp(source: &str, msg: &str) {
    let Some(now) = now_secs() else { return };
    let path = failure_stamp_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&path, format!("{}\t{}\n{msg}", now, source_key(source))).is_ok() {
        // Owner-only: the body is already redacted, but this file records local
        // paths in a cache dir that is not guaranteed to be private.
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
}

/// Drop the negative cache.
fn clear_failure_stamp() {
    let _ = fs::remove_file(failure_stamp_path());
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

fn is_executable(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

/// Strip ANSI CSI escape sequences (`ESC [ ... <final>`) from a string. `uv`
/// colorizes some output when it detects a TTY; we capture via a pipe and pass
/// NO_COLOR, but strip defensively so a stray escape never corrupts a path.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // ESC: consume an optional '[' and everything up to the final byte
            // (a char in the @..~ range), which ends a CSI sequence.
            if chars.peek() == Some(&'[') {
                chars.next();
                for cc in chars.by_ref() {
                    if ('@'..='~').contains(&cc) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_accepts_our_package() {
        assert!(decide_identity("fno", "Jason Noah Choi").is_ok());
        // case-insensitive name, author embedded in a longer string
        assert!(decide_identity("FNO", "Jason Noah Choi <j@x>").is_ok());
    }

    #[test]
    fn identity_rejects_foreign_name() {
        let e = decide_identity("notfno", "Jason Noah Choi").unwrap_err();
        assert!(e.contains("name=notfno"), "{e}");
    }

    #[test]
    fn identity_rejects_foreign_author() {
        // A squatter could publish a package literally named `fno`; the author
        // marker is what stops us running it (AC3-EDGE).
        let e = decide_identity("fno", "Mallory").unwrap_err();
        assert!(e.contains("author=Mallory"), "{e}");
    }

    #[test]
    fn identity_rejects_empty_author() {
        assert!(decide_identity("fno", "").is_err());
    }

    #[test]
    fn stale_wheel_names_version_and_remedy() {
        // AC1-EDGE: readable version -> named version + source-install fallback.
        let m = stale_wheel_message(true, Some("0.2.1")).unwrap();
        assert!(m.contains("(0.2.1)"), "{m}");
        assert!(m.contains("no fno-py script"), "{m}");
        assert!(
            m.contains("uv tool install --force --compile-bytecode --from <repo>/cli fno"),
            "{m}"
        );
    }

    #[test]
    fn stale_wheel_pre_rename_script_without_version() {
        // The old `bin/fno` is present but metadata unreadable: still a stale
        // wheel, message omits the version clause rather than faking one.
        let m = stale_wheel_message(true, None).unwrap();
        assert!(m.contains("predates this shim"), "{m}");
        assert!(!m.contains("()"), "{m}");
    }

    #[test]
    fn stale_wheel_old_version_without_script() {
        // A pre-0.3.0 version with no readable `bin/fno` is still stale.
        let m = stale_wheel_message(false, Some("0.2.1")).unwrap();
        assert!(m.contains("(0.2.1)"), "{m}");
    }

    #[test]
    fn stale_wheel_none_when_not_stale() {
        // Neither signal readable -> None, so the caller keeps the generic error.
        assert!(stale_wheel_message(false, None).is_none());
        // A modern version (>= 0.3.0) with no pre-rename script is a broken
        // install, not a stale wheel: fall through to the honest generic error.
        assert!(stale_wheel_message(false, Some("0.3.0")).is_none());
        assert!(stale_wheel_message(false, Some("1.0.0")).is_none());
    }

    /// A unique temp dir laid out as a valid fno checkout (`cli/pyproject.toml`).
    fn valid_checkout() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let root = env::temp_dir().join(format!(
            "fno-boot-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("cli")).unwrap();
        fs::write(root.join("cli/pyproject.toml"), "[project]\nname=\"fno\"\n").unwrap();
        root
    }

    #[test]
    fn install_source_defaults_to_by_name() {
        // US4/AC (end-user path): no env, no pin -> "fno", byte-identical.
        assert_eq!(install_source(None, None).unwrap(), "fno");
        assert_eq!(install_source(Some(""), None).unwrap(), "fno");
        assert_eq!(install_source(Some("   "), Some("  ")).unwrap(), "fno");
    }

    #[test]
    fn install_source_honors_override() {
        assert_eq!(
            install_source(Some("/tmp/fno-0.1.0-py3-none-any.whl"), None).unwrap(),
            "/tmp/fno-0.1.0-py3-none-any.whl"
        );
        assert_eq!(
            install_source(Some("  fno==0.1.0  "), None).unwrap(),
            "fno==0.1.0"
        );
    }

    #[test]
    fn install_source_env_wins_over_pin() {
        // AC4-EDGE: rung-1 env override beats a set rung-2 pin.
        let root = valid_checkout();
        assert_eq!(
            install_source(Some("/env/wheel.whl"), Some(root.to_str().unwrap())).unwrap(),
            "/env/wheel.whl"
        );
    }

    #[test]
    fn install_source_valid_pin_expands_to_cli() {
        // US1/AC1-HP: a valid pin -> `<checkout>/cli` (the wheel-build path).
        let root = valid_checkout();
        assert_eq!(
            install_source(None, Some(root.to_str().unwrap())).unwrap(),
            root.join("cli").to_string_lossy()
        );
    }

    #[test]
    fn install_source_invalid_pin_fails_loud() {
        // US3/AC3-FR: a set-but-invalid pin errors naming config.dev.source and
        // the bad path; it does NOT fall through to "fno".
        let e = install_source(None, Some("/no/such/checkout"))
            .unwrap_err()
            .msg;
        assert!(e.contains("config.dev.source"), "{e}");
        assert!(e.contains("/no/such/checkout"), "{e}");
    }

    #[test]
    fn install_source_pin_at_repo_root_without_cli_fails() {
        // A pin to a dir that exists but lacks cli/pyproject.toml is invalid
        // (strict check catches "pinned the repo root, not cli/").
        let root = env::temp_dir().join(format!("fno-boot-bare-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        assert!(install_source(None, Some(root.to_str().unwrap())).is_err());
    }

    #[test]
    fn expand_tilde_expands_leading_home() {
        let home = PathBuf::from("/home/me");
        assert_eq!(
            expand_tilde_with("~/src/fno", Some(home.clone())),
            home.join("src/fno")
        );
        assert_eq!(expand_tilde_with("~", Some(home.clone())), home);
        // absolute + `~user` (no slash) pass through unchanged.
        assert_eq!(
            expand_tilde_with("/abs/fno", Some(home.clone())),
            PathBuf::from("/abs/fno")
        );
        assert_eq!(expand_tilde_with("~foo", Some(home)), PathBuf::from("~foo"));
        // no home -> literal, never a panic.
        assert_eq!(expand_tilde_with("~/x", None), PathBuf::from("~/x"));
    }

    #[test]
    fn parse_dev_source_reads_the_pin() {
        // US2: pure parse of [dev].source from a flat config.toml body.
        assert_eq!(
            parse_dev_source("[dev]\nsource = \"/home/me/fno\"\n").as_deref(),
            Some("/home/me/fno")
        );
        // trims whitespace-padded value
        assert_eq!(
            parse_dev_source("[dev]\nsource = \"  /p  \"\n").as_deref(),
            Some("/p")
        );
    }

    #[test]
    fn parse_dev_source_degrades_on_missing_and_malformed() {
        // AC2-ERR: malformed/absent config is "no pin", never fatal.
        assert_eq!(parse_dev_source("not valid toml {{{"), None);
        assert_eq!(parse_dev_source(""), None);
        assert_eq!(parse_dev_source("[other]\nkey = 1\n"), None);
        assert_eq!(parse_dev_source("[dev]\nsource = \"\"\n"), None);
    }

    #[test]
    fn locate_failure_names_the_path_and_the_reason() {
        // The whole point: the terminal must show WHICH path was built and WHY it
        // was rejected. The generic message named neither, which is how two
        // separate sessions concluded the resolver looks for `fno` when it looks
        // for `fno-py`.
        let p = PathBuf::from("/u/.local/share/uv/tools/fno/bin/fno-py");
        let m = locate_failure_message(Some(&p), false, "fno", None);
        assert!(m.contains("/u/.local/share/uv/tools/fno/bin/fno-py"), "{m}");
        assert!(m.contains("nothing exists at that path"), "{m}");
    }

    #[test]
    fn enotempty_signature_matches_the_incident_string_and_only_it() {
        // The exact line uv printed in the 2026-08-14 incident.
        let incident = "error: failed to remove directory `/x/uv/tools/fno/lib`: \
                        Directory not empty (os error 66)";
        assert!(stderr_is_enotempty(incident));
        // Near-misses that must NOT retry: wrong errno, no errno, unrelated.
        assert!(!stderr_is_enotempty("Directory not empty (os error 39)"));
        assert!(!stderr_is_enotempty("Directory not empty"));
        assert!(!stderr_is_enotempty(
            "error: failed to remove directory: os error 66"
        ));
        assert!(!stderr_is_enotempty(
            "No solution found when resolving dependencies"
        ));
    }

    #[test]
    fn count_pyc_walks_recursively_and_skips_other_suffixes() {
        let root = env::temp_dir().join(format!("fno-pyc-{}", std::process::id()));
        let pkg = root.join("lib/python3.13/site-packages/fno/__pycache__");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("cli.cpython-313.pyc"), "").unwrap();
        fs::write(pkg.join("stray.py"), "").unwrap();
        fs::write(root.join("lib/other.txt"), "").unwrap();
        assert_eq!(count_pyc(&root.join("lib")), 1);
        // Absent dir is 0, not a panic.
        assert_eq!(count_pyc(&root.join("nope")), 0);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_wheel_retries_only_the_enotempty_signature_and_verifies_the_marker() {
        // A fake uv that fails twice with the incident signature, then succeeds
        // and materializes the marker tree (entrypoint + one .pyc). install_wheel
        // must absorb both failures, run three attempts total, and accept the
        // result only because the marker verifies.
        let root = env::temp_dir().join(format!("fno-uvfake-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let tool_dir = root.join("tools");
        fs::create_dir_all(tool_dir.join("fno/bin")).unwrap();
        fs::create_dir_all(tool_dir.join("fno/lib/python3.13/site-packages/fno/__pycache__"))
            .unwrap();
        fs::write(
            tool_dir.join("fno/lib/python3.13/site-packages/fno/__pycache__/x.pyc"),
            "",
        )
        .unwrap();
        fs::write(tool_dir.join("fno/bin/fno-py"), "#!/bin/sh\n").unwrap();
        let counter = root.join("attempts");
        let script = format!(
            "#!/bin/sh\n\
             case \"$1 $2\" in\n\
             'tool dir') echo '{}'; exit 0;;\n\
             'tool install') n=$(cat '{c}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{c}'; \
             if [ $n -lt 3 ]; then \
             echo 'error: failed to remove directory `/x/fno/lib`: Directory not empty (os error 66)' >&2; exit 2; \
             fi; exit 0;;\n\
             esac; exit 64\n",
            tool_dir.display(),
            c = counter.display()
        );
        let uv = root.join("uv");
        fs::write(&uv, script).unwrap();
        fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();

        install_wheel(&uv, "fno").expect("retry absorbs the signature race");
        assert_eq!(fs::read_to_string(&counter).unwrap().trim(), "3");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_wheel_fails_fast_on_a_non_signature_error() {
        // Same fake, but the failure is NOT the ENOTEMPTY signature: one
        // attempt, no retry, and the error must carry uv's exit code.
        let root = env::temp_dir().join(format!("fno-uvfake2-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let counter = root.join("attempts");
        let script = format!(
            "#!/bin/sh\n\
             case \"$1 $2\" in\n\
             'tool dir') exit 0;;\n\
             'tool install') n=$(cat '{c}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{c}'; \
             echo 'error: invalid credential' >&2; exit 7;;\n\
             esac; exit 64\n",
            c = counter.display()
        );
        let uv = root.join("uv");
        fs::write(&uv, script).unwrap();
        fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();

        let e = install_wheel(&uv, "fno").unwrap_err();
        assert_eq!(e.code, 7, "uv's own exit code is preserved");
        assert_eq!(fs::read_to_string(&counter).unwrap().trim(), "1");
        // The race remedy belongs to the race only: a credential failure must
        // not send the operator off to kill fno processes.
        assert!(!e.msg.contains("Stop fno processes"), "{}", e.msg);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_wheel_names_the_race_remedy_when_the_cap_is_hit() {
        // Every attempt races. The capped error must carry the same remedy the
        // fno.sh / postinstall twins print, or the Rust leg is the one
        // provisioning path that leaves the user with no next step.
        let root = env::temp_dir().join(format!("fno-uvfake4-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let script = "#!/bin/sh\n\
             case \"$1 $2\" in\n\
             'tool dir') exit 0;;\n\
             'tool install') echo 'error: failed to remove directory `/x/fno/lib`: \
             Directory not empty (os error 66)' >&2; exit 2;;\n\
             esac; exit 64\n";
        let uv = root.join("uv");
        fs::write(&uv, script).unwrap();
        fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();

        let e = install_wheel(&uv, "fno").unwrap_err();
        assert_eq!(e.code, 2, "uv's own exit code is preserved");
        assert!(e.msg.contains("Stop fno processes and re-run"), "{}", e.msg);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_wheel_rejects_a_success_that_does_not_verify() {
        // uv exits 0 but the marker tree is absent (no pyc shipped): the exit
        // code alone must not certify the install.
        let root = env::temp_dir().join(format!("fno-uvfake3-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let tool_dir = root.join("tools");
        fs::create_dir_all(tool_dir.join("fno/bin")).unwrap();
        fs::write(tool_dir.join("fno/bin/fno-py"), "#!/bin/sh\n").unwrap();
        let script = format!(
            "#!/bin/sh\n\
             case \"$1 $2\" in\n\
             'tool dir') echo '{}'; exit 0;;\n\
             'tool install') exit 0;;\n\
             esac; exit 64\n",
            tool_dir.display()
        );
        let uv = root.join("uv");
        fs::write(&uv, script).unwrap();
        fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();

        let e = install_wheel(&uv, "fno").unwrap_err();
        assert!(e.msg.contains("does not verify"), "{}", e.msg);
        assert!(e.msg.contains("no compiled bytecode"), "{}", e.msg);
        fs::remove_dir_all(&root).ok();
    }

    /// A fake `uv` that only answers `tool dir`, pointing at `tool_dir`.
    fn uv_reporting_tool_dir(root: &Path, tool_dir: &Path) -> PathBuf {
        let script = format!(
            "#!/bin/sh\n\
             case \"$1 $2\" in\n\
             'tool dir') echo '{}'; exit 0;;\n\
             esac; exit 64\n",
            tool_dir.display()
        );
        let uv = root.join("uv");
        fs::write(&uv, script).unwrap();
        fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();
        uv
    }

    #[test]
    fn verify_waits_for_a_console_script_that_lands_after_uv_exits() {
        // The bug this closes: uv exits before its own artifacts settle. The
        // console script is absent for ~490ms across an install, so a verify
        // firing the instant uv returns raced the install it was verifying and
        // refused every fno verb.
        let root = env::temp_dir().join(format!("fno-uvwait1-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let tool_dir = root.join("tools");
        let venv = tool_dir.join("fno");
        fs::create_dir_all(venv.join("bin")).unwrap();
        fs::create_dir_all(venv.join("lib")).unwrap();
        // Bytecode marker is already there; only the console script is late.
        fs::write(venv.join("lib/x.pyc"), "").unwrap();
        let uv = uv_reporting_tool_dir(&root, &tool_dir);

        // Falsification first: without the wait this case FAILS. If this
        // assertion ever stops holding, the test below proves nothing.
        assert!(
            install_verified(&uv).is_err(),
            "single-shot verify must fail while the script is still absent"
        );

        let entry = venv.join("bin/fno-py");
        let late = entry.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            fs::write(&late, "#!/bin/sh\n").unwrap();
        });

        let got = install_verified_within(&uv, 15, Duration::from_millis(50));
        writer.join().unwrap();
        assert!(
            got.is_ok(),
            "verify must wait for uv's own artifact: {got:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn verify_gives_up_bounded_and_says_what_it_waited_for() {
        // The wait is bounded and falsifiable: a genuinely broken install still
        // fails, with the same marker message plus what we waited. Nothing is
        // masked, which is what separates this from the sleep-retry the
        // lazy-imports doc rejects.
        let root = env::temp_dir().join(format!("fno-uvwait2-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let tool_dir = root.join("tools");
        fs::create_dir_all(tool_dir.join("fno/bin")).unwrap();
        let uv = uv_reporting_tool_dir(&root, &tool_dir);

        let e = install_verified_within(&uv, 3, Duration::from_millis(10)).unwrap_err();
        assert!(e.contains("no console script at"), "{e}");
        assert!(e.contains("still absent after waiting 30ms"), "{e}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn verify_budget_matches_the_python_half() {
        // The two provisioning paths cannot share an implementation across the
        // Rust/shell boundary, so they share the numbers and this test. The
        // shell twin is `_await_binary` in cli/src/fno/update.py: 15 iterations
        // of `sleep 0.2`, a 3s ceiling. Drift fails here rather than in review.
        assert_eq!(VERIFY_ATTEMPTS, 15);
        assert_eq!(VERIFY_POLL, Duration::from_millis(200));
        assert_eq!(
            VERIFY_POLL * VERIFY_ATTEMPTS,
            Duration::from_secs(3),
            "ceiling must stay the 3s update.py spends on the same file"
        );
    }

    #[test]
    fn install_failure_message_defers_to_uv_instead_of_blaming_the_network() {
        // uv exits nonzero for plenty of non-network reasons (the case that
        // prompted this was `Directory not empty (os error 66)`, printed verbatim
        // one line above). Asserting a subsystem we have not tested sends the
        // reader to diagnose the wrong one.
        let m = install_failure_message("/home/me/footnote/cli");
        assert!(m.contains("uv's own error is printed above"), "{m}");
        assert!(!m.to_lowercase().contains("network"), "{m}");
        assert!(!m.contains("PyPI"), "{m}");
        // Still names what failed and how to reproduce it by hand.
        assert!(
            m.contains("`uv tool install /home/me/footnote/cli` failed"),
            "{m}"
        );
        assert!(m.contains("FNO_BOOTSTRAP_WHEEL"), "{m}");
    }

    #[test]
    fn install_failure_message_redacts_credentials_in_the_source() {
        // Redaction lives inside the builder so no caller can leak by forgetting.
        let m = install_failure_message("https://user:tok@example.com/wheels/fno.whl");
        assert!(!m.contains("tok"), "{m}");
        assert!(m.contains("<redacted>@example.com"), "{m}");
    }

    #[test]
    fn locate_failure_names_the_install_source() {
        // PyPI-by-name and a local checkout fail for different reasons, so the
        // message must say which rung ran. `fno` is the PyPI default; anything
        // else came from config.dev.source / FNO_BOOTSTRAP_WHEEL.
        let p = PathBuf::from("/tools/fno/bin/fno-py");
        let pypi = locate_failure_message(Some(&p), false, "fno", None);
        assert!(pypi.contains("installed from: fno"), "{pypi}");
        let local = locate_failure_message(Some(&p), false, "/home/me/footnote/cli", None);
        assert!(
            local.contains("installed from: /home/me/footnote/cli"),
            "{local}"
        );
    }

    #[test]
    fn locate_failure_never_prints_a_credential() {
        // FNO_BOOTSTRAP_WHEEL can be an authenticated URL, and this message is
        // both printed and persisted. Redaction lives INSIDE the builder so no
        // caller can leak by forgetting; assert through the builder, not the
        // helper, so that stays true.
        let p = PathBuf::from("/tools/fno/bin/fno-py");
        let m = locate_failure_message(
            Some(&p),
            false,
            "https://ci:s3cr3t@pkgs.example.com/fno.whl?token=deadbeef",
            None,
        );
        assert!(!m.contains("s3cr3t"), "{m}");
        assert!(!m.contains("deadbeef"), "{m}");
        assert!(m.contains("pkgs.example.com"), "{m}");
    }

    #[test]
    fn redact_source_strips_userinfo_and_query_only() {
        // Both credential channels uv accepts inside a URL, replaced (not
        // dropped) so the reader can see a credential was in play.
        assert_eq!(
            redact_source("https://ci:tok@host/fno.whl"),
            "https://<redacted>@host/fno.whl"
        );
        assert_eq!(
            redact_source("https://host/fno.whl?token=abc"),
            "https://host/fno.whl?<redacted>"
        );
        assert_eq!(
            redact_source("https://ci:tok@host/a.whl?t=x"),
            "https://<redacted>@host/a.whl?<redacted>"
        );
        // A clean URL keeps every part an operator needs to recognise it.
        assert_eq!(
            redact_source("https://host/fno.whl"),
            "https://host/fno.whl"
        );
        // The two non-URL rungs are not secret channels and must pass through,
        // or the message stops identifying which rung ran.
        assert_eq!(redact_source("fno"), "fno");
        assert_eq!(redact_source("fno==0.1.0"), "fno==0.1.0");
        assert_eq!(
            redact_source("/home/me/footnote/cli"),
            "/home/me/footnote/cli"
        );
    }

    #[test]
    fn source_key_is_a_digest_not_the_source() {
        // The stamp lives in a shared cache dir, so the raw source must never
        // reach it. A digest also means the key cannot be reversed into a token.
        let secret = "https://ci:s3cr3t@host/fno.whl";
        let k = source_key(secret);
        assert!(!k.contains("s3cr3t"), "{k}");
        assert!(!k.contains("host"), "{k}");
        assert_eq!(k.len(), 64, "expected a blake3 hex digest, got {k}");
        // Still a stable identity: same source in, same key out.
        assert_eq!(source_key(secret), k);
        assert_ne!(source_key("fno"), k);
    }

    #[test]
    fn source_key_resolves_relative_paths_before_keying() {
        // A relative FNO_BOOTSTRAP_WHEEL names different artifacts from different
        // working directories. Keying on the literal string would let a broken
        // install under checkout A suppress a valid first install under checkout
        // B for the whole cooldown.
        let root = valid_checkout();
        let a = root.join("cli");
        let b = root.join("cli/.");
        // Two spellings of ONE path agree.
        assert_eq!(
            source_key(a.to_str().unwrap()),
            source_key(b.to_str().unwrap())
        );
        // Two different paths do not.
        let other = valid_checkout();
        assert_ne!(
            source_key(a.to_str().unwrap()),
            source_key(other.join("cli").to_str().unwrap())
        );
    }

    #[test]
    fn source_key_does_not_canonicalize_a_bare_spec() {
        // `fno` is a PyPI name, not a path. Canonicalizing it would resolve
        // against the cwd whenever a file or dir of that name sits there (this
        // repo has `crates/fno`), making the by-name rung's key cwd-dependent.
        assert_eq!(canonical_source("fno"), "fno");
        assert_eq!(canonical_source("fno==0.1.0"), "fno==0.1.0");
        // A URL contains separators but is not a path: it must pass through.
        assert_eq!(
            canonical_source("https://host/fno.whl"),
            "https://host/fno.whl"
        );
        // A path-shaped source that does not exist also passes through.
        assert_eq!(canonical_source("/no/such/wheel.whl"), "/no/such/wheel.whl");
    }

    #[test]
    fn locate_failure_distinguishes_present_from_absent() {
        // Present-but-unusable and absent are different bugs with different
        // fixes, so they must not collapse into one message.
        let p = PathBuf::from("/tools/fno/bin/fno-py");
        let present = locate_failure_message(Some(&p), true, "fno", None);
        assert!(present.contains("not an executable file"), "{present}");
        assert!(!present.contains("nothing exists"), "{present}");
    }

    #[test]
    fn locate_failure_without_a_tool_dir_says_so() {
        // `uv tool dir` unreadable -> no path was ever built; claiming we
        // "looked for" a path would be a lie.
        let m = locate_failure_message(None, false, "fno", None);
        assert!(m.contains("no path built"), "{m}");
        assert!(m.contains("uv tool dir"), "{m}");
    }

    #[test]
    fn locate_failure_keeps_the_stale_wheel_remedy() {
        // When the stale-wheel diagnosis applies it is the actionable one, so it
        // must survive alongside the new path/reason lines.
        let p = PathBuf::from("/tools/fno/bin/fno-py");
        let stale = stale_wheel_message(true, Some("0.2.1"));
        let m = locate_failure_message(Some(&p), false, "fno", stale);
        assert!(m.contains("/tools/fno/bin/fno-py"), "{m}");
        assert!(m.contains("(0.2.1)"), "{m}");
    }

    #[test]
    fn cached_failure_is_honest_about_not_reinstalling() {
        // AC: the fast-fail must preserve the original diagnosis, say it is a
        // repeat, and name the exact file to remove for an immediate retry.
        let stamp = PathBuf::from("/c/fno-bootstrap/provision-failed");
        let m = cached_failure_message("could not locate the installed fno", 42, &stamp);
        assert!(m.contains("could not locate the installed fno"), "{m}");
        assert!(m.contains("42s ago"), "{m}");
        assert!(
            m.contains("not \nreinstalling") || m.contains("not reinstalling"),
            "{m}"
        );
        assert!(m.contains("rm /c/fno-bootstrap/provision-failed"), "{m}");
    }

    #[test]
    fn cached_failure_honored_inside_the_cooldown() {
        // The severity multiplier this node exists for: a second invocation
        // inside the window must NOT re-run the 18-package install.
        let (msg, age) = decide_cached_failure("1000\tfno\nboom", 1000 + 30, "fno").unwrap();
        assert_eq!(msg, "boom");
        assert_eq!(age, 30);
        // still honored at the exact boundary
        assert!(
            decide_cached_failure("1000\tfno\nboom", 1000 + FAILURE_COOLDOWN_SECS, "fno").is_some()
        );
    }

    #[test]
    fn cached_failure_expires() {
        // One second past the window re-provisions, so a transient breakage
        // heals on its own without the operator knowing the cache exists.
        assert!(
            decide_cached_failure("1000\tfno\nboom", 1000 + FAILURE_COOLDOWN_SECS + 1, "fno")
                .is_none()
        );
    }

    #[test]
    fn cached_failure_is_keyed_to_the_install_source() {
        // The three install rungs are separate channels. A PyPI failure must not
        // suppress a local-checkout install, and repointing config.dev.source (or
        // setting FNO_BOOTSTRAP_WHEEL) must take effect on the very next call
        // rather than waiting out a cooldown earned by a different source.
        // Composed through source_key, which is what run() actually passes.
        let pypi = format!("1000\t{}\nboom", source_key("fno"));
        assert!(decide_cached_failure(&pypi, 1030, &source_key("fno")).is_some());
        assert!(decide_cached_failure(&pypi, 1030, &source_key("/home/me/footnote/cli")).is_none());

        let local = format!("1000\t{}\nboom", source_key("/home/me/footnote/cli"));
        assert!(
            decide_cached_failure(&local, 1030, &source_key("/home/me/footnote/cli")).is_some()
        );
        assert!(decide_cached_failure(&local, 1030, &source_key("fno")).is_none());
        // A different checkout is a different source too.
        assert!(decide_cached_failure(&local, 1030, &source_key("/home/me/other/cli")).is_none());
    }

    #[test]
    fn cached_failure_fails_open_on_every_unreadable_shape() {
        // A negative cache that can wedge the bootstrap shut is worse than the
        // bug it fixes, so anything we cannot read means "re-provision".
        assert!(decide_cached_failure("", 2000, "fno").is_none()); // empty
        assert!(decide_cached_failure("no newline", 2000, "fno").is_none()); // no body
        assert!(decide_cached_failure("1000\nboom", 2000, "fno").is_none()); // no source field
        assert!(decide_cached_failure("nan\tfno\nboom", 2000, "fno").is_none()); // bad ts
        assert!(decide_cached_failure("1000\tfno\n", 2000, "fno").is_none()); // empty body
        assert!(decide_cached_failure("1000\tfno\n   \n", 2000, "fno").is_none()); // blank body
                                                                                   // Future-dated (clock skew / restored backup): never a cooldown that
                                                                                   // outlives the clock.
        assert!(decide_cached_failure("9999\tfno\nboom", 1000, "fno").is_none());
    }

    #[test]
    fn cached_failure_multiline_body_survives_intact() {
        // The stamped message is itself multi-line (source + path + reason +
        // remedy), so only the FIRST newline may end the header.
        let (msg, _) =
            decide_cached_failure("1000\tfno\nline one\nline two\nline three", 1010, "fno")
                .unwrap();
        assert_eq!(msg, "line one\nline two\nline three");
    }

    #[test]
    fn failure_stamp_round_trips_through_its_own_writer_format() {
        // Guards the writer/reader pair against drifting apart: the exact bytes
        // write_failure_stamp emits must be what decide_cached_failure accepts.
        let source = "https://ci:s3cr3t@pkgs.example.com/fno.whl?token=deadbeef";
        let msg = locate_failure_message(
            Some(Path::new("/tools/fno/bin/fno-py")),
            false,
            source,
            None,
        );
        let raw = format!("{}\t{}\n{}", 1000, source_key(source), msg);
        // No part of the persisted stamp may carry the credential.
        assert!(!raw.contains("s3cr3t"), "{raw}");
        assert!(!raw.contains("deadbeef"), "{raw}");
        let (back, age) = decide_cached_failure(&raw, 1005, &source_key(source)).unwrap();
        assert_eq!(back, msg);
        assert_eq!(age, 5);
    }

    #[test]
    fn failure_stamp_sits_beside_the_sentinel() {
        // Same cache dir, distinct name: one operator-facing place to look, and
        // `rm -rf` of that dir is a complete reset.
        assert_eq!(failure_stamp_path().parent(), sentinel_path().parent());
        assert_ne!(failure_stamp_path(), sentinel_path());
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        // matches the real `uv tool dir` colorized output shape
        let colored = "\u{1b}[36m/Users/me/.local/share/uv/tools\u{1b}[39m";
        assert_eq!(strip_ansi(colored), "/Users/me/.local/share/uv/tools");
    }

    #[test]
    fn strip_ansi_leaves_plain_text() {
        assert_eq!(strip_ansi("/plain/path"), "/plain/path");
    }
}
