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
//! The wheel's Python CLI ships as the `fno-py` console script (this Rust
//! binary owns `fno`), and the shim execs it by absolute path, NEVER via a PATH
//! lookup. Two guards against a self-loop, either sufficient: the target is a
//! different name (`fno-py`, not `fno`), and it is reached by absolute path, not
//! a PATH search - so it holds even when `~/.cargo/bin` and uv's tool bin both
//! carry an `fno`.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

    // Provision. Progress line BEFORE the slow step so the first run never
    // looks like a hang (AC1-UI).
    let uv = ensure_uv()?;
    eprintln!(
        "fno: first run - provisioning the fno CLI via uv (one time, may take a few seconds)..."
    );
    install_wheel(&uv)?;

    let real = resolve_via_uv_tool_dir()
        .filter(|p| is_executable(p))
        .ok_or_else(|| {
            // A successful install that still has no `fno-py` script almost
            // always means PyPI served a wheel older than the fno->fno-py
            // rename. Name the version and the real remedy instead of the
            // generic locate error, which hid this cause for months.
            let msg = diagnose_locate_failure().unwrap_or_else(|| {
                "provisioned the wheel but could not locate the installed fno; \
                 try `uv tool install fno` manually"
                    .to_string()
            });
            BootErr::new(1, msg)
        })?;
    verify_ours(&real)?;
    Err(record_and_exec(&real, args))
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

/// `uv tool install <source>`. Source is `fno` (PyPI by name) by default, or
/// the value of `FNO_BOOTSTRAP_WHEEL` (a local wheel path or any uv install
/// spec) so the channel is testable before the PyPI publish lands, or a
/// maintainer's pinned checkout (`config.dev.source`) so editing source never
/// re-provisions the stale published wheel.
fn install_wheel(uv: &Path) -> BootResult<()> {
    let source = install_source(
        env::var("FNO_BOOTSTRAP_WHEEL").ok().as_deref(),
        read_dev_source_pin().as_deref(),
    )?;
    // --force so a half-built or stale tool venv is repaired rather than failing
    // with "already installed" (AC4-FR: never trust a half-provisioned state).
    // We only reach here when no usable install was found, so --force never does
    // a redundant reinstall over a healthy one.
    let status = Command::new(uv)
        .args(["tool", "install", "--force", &source])
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(BootErr::new(
            s.code().unwrap_or(1),
            format!(
                "`uv tool install {source}` failed. Check your network / PyPI access \
                 and retry, or run it manually."
            ),
        )),
        Err(e) => Err(BootErr::new(
            1,
            format!("could not run uv to install the fno wheel: {e}"),
        )),
    }
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
    let out = Command::new(&uv)
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
    Some(
        PathBuf::from(tool_dir)
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
         uv tool install --force --from <repo>/cli fno"
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

fn sentinel_path() -> PathBuf {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"));
    base.join("fno-bootstrap").join("real-fno")
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
    exec_real(real, args)
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
            m.contains("uv tool install --force --from <repo>/cli fno"),
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
