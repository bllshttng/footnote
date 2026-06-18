//! `fno` cargo bootstrapper.
//!
//! `cargo install fno` compiles this tiny shim to `~/.cargo/bin/fno`. Its only
//! job is to make the *real* `fno` (the Python Typer CLI plus the three
//! `fno-agents*` Rust binaries, shipped as the `fno` PyPI wheel) available and
//! then forward to it. The CLI itself is never reimplemented here
//! (foundation Locked Decision 12).
//!
//! First-run flow:
//!   1. ensure `uv` is present (download Astral's standalone uv if absent),
//!   2. `uv tool install fno` (the PyPI platform wheel, binaries bundled),
//!   3. verify the installed package is *ours* before running it,
//!   4. `exec` the wheel `fno` by ABSOLUTE path.
//!
//! Subsequent runs read a sentinel and forward immediately - no network.
//!
//! The shim execs the wheel `fno` by absolute path, NEVER via a PATH lookup,
//! so it can never self-exec into an infinite loop even when `~/.cargo/bin`
//! and uv's tool bin both carry an `fno`.

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

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    match run(&args) {
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
            BootErr::new(
                1,
                "provisioned the wheel but could not locate the installed fno; \
                 try `uv tool install fno` manually",
            )
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
/// spec) so the channel is testable before the PyPI publish lands.
fn install_wheel(uv: &Path) -> BootResult<()> {
    let source = install_source(env::var("FNO_BOOTSTRAP_WHEEL").ok().as_deref());
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

/// Choose the `uv tool install` source: the `FNO_BOOTSTRAP_WHEEL` override when
/// set and non-empty, else the by-name PyPI package `fno`. Pure for testing.
fn install_source(override_val: Option<&str>) -> String {
    match override_val {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => "fno".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Resolution: find the wheel `fno` absolute path
// ---------------------------------------------------------------------------

/// Resolve the wheel `fno` console script inside uv's tool venv:
/// `<uv tool dir>/fno/bin/fno`. This is ALWAYS the wheel CLI, never this shim,
/// so execing it by absolute path cannot self-loop. Returns `None` when uv is
/// absent or the tool dir cannot be read; the caller then provisions.
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
            .join("fno"),
    )
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
    fn install_source_defaults_to_by_name() {
        assert_eq!(install_source(None), "fno");
        assert_eq!(install_source(Some("")), "fno");
        assert_eq!(install_source(Some("   ")), "fno");
    }

    #[test]
    fn install_source_honors_override() {
        assert_eq!(
            install_source(Some("/tmp/fno-0.1.0-py3-none-any.whl")),
            "/tmp/fno-0.1.0-py3-none-any.whl"
        );
        assert_eq!(install_source(Some("  fno==0.1.0  ")), "fno==0.1.0");
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
