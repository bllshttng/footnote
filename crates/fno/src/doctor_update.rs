//! `fno doctor update` and root `fno update`, native: reinstall the fno CLI
//! from its source path. Port of the deleted `cli/src/fno/update.py`.
//!
//! Every step is a child process whose exit code is read (the Python leg
//! execvp-ed the installer and never returned; a binary that does not run
//! from the tool venv has no such constraint). Steps run in order; the
//! final exit is 0 only when no step failed, and the last line names the
//! failed step (ruling d-f2bd8d86: a failed record or refresh may no longer
//! exit 0).
//!
//! Long-running installs (cargo, uv) run to completion unbounded, exactly as
//! the exec did. Probe-shaped calls carry the Python timeouts.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::digest_overlay::fno_agents_bin;
use crate::model_catalog::state_dir;

/// The fno-agents triad: client + daemon + worker, one crate, three bins.
/// They MUST stay a coherent same-build set in every install location; a
/// mixed-version pair is the worse bug, so update syncs all three or none
/// per location.
const TRIAD_STEMS: [&str; 3] = ["fno-agents", "fno-agents-daemon", "fno-agents-worker"];

const GUARD_MSG: &str = "[fno doctor update] refused: target-state.md shows status: IN_PROGRESS. Updating mid-loop risks binary skew across subprocesses. Pass --force to override.";

/// Fail-closed text when the native source-pin authority cannot answer: the
/// legacy classifier must NOT reopen the unsafe cache path.
const SOURCE_PIN_UNAVAILABLE: &str = "the deployed fno-agents could not answer source-pin (missing, pre-source-pin, or malformed); re-run the update with an explicit --source pointing at the canonical checkout, or rebuild the rust bins from it, so a stale helper cannot reopen the unsafe cache path";

const UPDATE_CLAIM_KEY: &str = "update:fno";
const UV_INSTALL_ATTEMPTS: u32 = 3;
const UPDATE_BUDGET_SECS: u64 = 60;

fn exe_suffix() -> &'static str {
    if cfg!(windows) {
        ".exe"
    } else {
        ""
    }
}

pub(crate) fn triad_names() -> Vec<String> {
    TRIAD_STEMS
        .iter()
        .map(|s| format!("{s}{}", exe_suffix()))
        .collect()
}

fn install_dir() -> PathBuf {
    state_dir().join("install")
}

fn cache_file() -> PathBuf {
    install_dir().join("source-path")
}

fn installed_rev_file() -> PathBuf {
    install_dir().join("installed-rev")
}

/// The retired `installed-rust-rev` marker's path. Nothing has written it for
/// several releases (the staleness verdict keys on the binary's self-reported
/// crates/ rev instead), so every surviving copy reads stale forever; each
/// deploy removes it so the state root stops carrying a lying file.
fn retired_rust_marker_file() -> PathBuf {
    install_dir().join("installed-rust-rev")
}

fn companion_file() -> PathBuf {
    install_dir().join("source-pin.json")
}

/// The lexically-claimed argv: `doctor update ...` (rest = flags) and the
/// root `update ...` spelling (ruling d-5073b562 keeps both). `Some(rest)`
/// runs natively; `None` forwards.
pub fn classify(args: &[std::ffi::OsString]) -> Option<Vec<std::ffi::OsString>> {
    let a0 = args.first()?.to_str()?;
    match a0 {
        "doctor" => {
            if args.get(1)?.to_str()? == "update" {
                Some(args[2..].to_vec())
            } else {
                None
            }
        }
        "update" => Some(args[1..].to_vec()),
        _ => None,
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    pub(crate) source: Option<PathBuf>,
    pub(crate) dry_run: bool,
    pub(crate) force: bool,
    pub(crate) rust: bool,
    pub(crate) no_rust: bool,
    pub(crate) check: bool,
}

pub(crate) fn parse_args(rest: &[std::ffi::OsString]) -> Result<Flags, String> {
    let mut f = Flags::default();
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        let t = tok
            .to_str()
            .ok_or_else(|| "non-UTF-8 argument".to_string())?;
        match t {
            "--source" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--source requires a value".to_string())?;
                f.source = Some(PathBuf::from(v));
            }
            "--dry-run" | "-N" => f.dry_run = true,
            "--force" | "-F" => f.force = true,
            "--rust" => f.rust = true,
            "--no-rust" => f.no_rust = true,
            "--check" => f.check = true,
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(f)
}

/// One blocking subprocess with captured output, NO bound: the installer
/// shape. The deleted exec waited forever, so the port does too; probes keep
/// their timeouts, installs keep none.
fn run_captured(
    bin: &Path,
    args: &[String],
    input: Option<&str>,
) -> Result<(i32, String, String), String> {
    let tag = format!(
        "fno-doctor-update-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    );
    let tmp = |suffix: &str| std::env::temp_dir().join(format!("{tag}.{suffix}"));
    let out_path = tmp("out");
    let err_path = tmp("err");
    let in_path = tmp("in");
    let keep = |p: &Path| {
        let _ = std::fs::remove_file(p);
    };
    let mut command = crate::process_admission::std_command(bin);
    command.args(args).stdout(
        std::fs::File::create(&out_path).map_err(|e| format!("{}: {e}", out_path.display()))?,
    );
    command.stderr(
        std::fs::File::create(&err_path).map_err(|e| format!("{}: {e}", err_path.display()))?,
    );
    if input.is_some() {
        std::fs::write(&in_path, input.unwrap_or_default())
            .map_err(|e| format!("{}: {e}", in_path.display()))?;
        let f = std::fs::File::open(&in_path).map_err(|e| format!("{}: {e}", in_path.display()))?;
        command.stdin(Stdio::from(f));
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = crate::process_admission::std_spawn(&mut command)
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    let status = child
        .wait()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    let out = std::fs::read_to_string(&out_path).unwrap_or_default();
    let err = std::fs::read_to_string(&err_path).unwrap_or_default();
    keep(&out_path);
    keep(&err_path);
    if input.is_some() {
        keep(&in_path);
    }
    Ok((status.code().unwrap_or(1), out, err))
}

/// One bounded blocking subprocess: stdout+stderr to private temp files (a
/// pipe read blocks on EOF past the child; the files never do), stdin from a
/// temp file when fed, a try_wait/kill bound. The server.rs `config_get`
/// shape (attention_api.rs): a dark probe answers "cannot answer", never a
/// hang.
fn run_bounded(
    bin: &Path,
    args: &[String],
    bound: Duration,
    input: Option<&str>,
) -> Result<(i32, String, String), String> {
    let tag = format!(
        "fno-doctor-update-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    );
    let tmp = |suffix: &str| std::env::temp_dir().join(format!("{tag}.{suffix}"));
    let out_path = tmp("out");
    let err_path = tmp("err");
    let in_path = tmp("in");
    let keep = |p: &Path| {
        let _ = std::fs::remove_file(p);
    };
    let mut command = crate::process_admission::std_command(bin);
    command.args(args).stdout(
        std::fs::File::create(&out_path).map_err(|e| format!("{}: {e}", out_path.display()))?,
    );
    command.stderr(
        std::fs::File::create(&err_path).map_err(|e| format!("{}: {e}", err_path.display()))?,
    );
    if input.is_some() {
        std::fs::write(&in_path, input.unwrap_or_default())
            .map_err(|e| format!("{}: {e}", in_path.display()))?;
        let f = std::fs::File::open(&in_path).map_err(|e| format!("{}: {e}", in_path.display()))?;
        command.stdin(Stdio::from(f));
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = crate::process_admission::std_spawn(&mut command)
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    let deadline = Instant::now() + bound;
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("timed out".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => break Err(e.to_string()),
        }
    };
    let out = std::fs::read_to_string(&out_path).unwrap_or_default();
    let err = std::fs::read_to_string(&err_path).unwrap_or_default();
    keep(&out_path);
    keep(&err_path);
    if input.is_some() {
        keep(&in_path);
    }
    let code = outcome
        .map(|s| s.code().unwrap_or(1))
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    Ok((code, out, err))
}

/// Unbounded foreground run for installer-shaped children (cargo, uv, pip):
/// inherit stdio so progress streams, return the exit code. The Python exec
/// had no timeout either.
fn run_inherit(bin: &Path, args: &[String]) -> i32 {
    let status = command_for(bin, args);
    match status {
        Ok(mut cmd) => match cmd.status() {
            Ok(s) => s.code().unwrap_or(1),
            Err(_) => 1,
        },
        Err(_) => 1,
    }
}

/// The compile fallback. `cargo install` skips the checkout's
/// `.cargo/config.toml`, so the admission wrapper it names never ran and an
/// update compiled outside the build:cargo slot. RUSTC_WRAPPER puts it back.
fn run_cargo_install(source: &Path, args: &[String]) -> i32 {
    let mut cmd = crate::process_admission::std_command("cargo");
    cmd.args(args);
    if std::env::var_os("RUSTC_WRAPPER").is_none() {
        let wrapper = source
            .parent()
            .map(|root| root.join("scripts/lib/cargo-rustc-wrapper.sh"))
            .filter(|w| w.is_file());
        if let Some(wrapper) = wrapper {
            cmd.env("RUSTC_WRAPPER", wrapper);
        }
    }
    cmd.status().map(|s| s.code().unwrap_or(1)).unwrap_or(1)
}

/// Install the CI-built tarball for `crates_rev` into `bin_dir`. Err names
/// why there is none, for the compile fallback's line.
fn deploy_prebuilt(crates_rev: &str, bin_dir: &Path, dry_run: bool) -> Result<(), String> {
    use crate::update_prebuilt as pre;
    let platform = pre::platform().ok_or("CI builds no binary for this platform")?;
    let url = pre::asset_url(crates_rev, platform);
    if dry_run {
        println!("Would download: {url} (cargo install only when it is absent)");
        return Ok(());
    }
    println!("fno doctor update: downloading the CI build: {url}");
    let unpacked = pre::fetch(crates_rev, &install_dir())?;
    let swapped = pre::swap_into(&unpacked, bin_dir);
    if let Some(staging) = unpacked.parent() {
        let _ = std::fs::remove_dir_all(staging);
    }
    swapped?;
    println!(
        "fno doctor update: installed the CI build for crates rev {} into {}",
        &crates_rev[..crates_rev.len().min(12)],
        bin_dir.display()
    );
    Ok(())
}

fn command_for(bin: &Path, args: &[String]) -> std::io::Result<Command> {
    let mut cmd = crate::process_admission::std_command(bin);
    cmd.args(args);
    Ok(cmd)
}

/// `git <args>` in `dir`; stdout trimmed, None on any failure.
fn git_in(dir: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = crate::process_admission::std_command("git");
    cmd.arg("-C").arg(dir).args(args);
    let out = cmd.output().ok()?;
    let rev = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !rev.is_empty() {
        Some(rev)
    } else {
        None
    }
}

fn source_rev(source: &Path) -> Option<String> {
    git_in(source, &["rev-parse", "HEAD"])
}

/// The last commit that touched crates/ (not HEAD: Python-only commits never
/// flag the rust bins stale).
fn rust_subtree_rev(source: &Path) -> Option<String> {
    git_in(
        source.parent()?,
        &["log", "-1", "--format=%H", "--", "crates/"],
    )
}

pub(crate) fn read_marker(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Atomic marker write: temp file in the marker's own directory, then
/// rename, so a concurrent `fno doctor` read never sees a torn value.
pub(crate) fn write_marker(path: &Path, rev: &str) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("marker path has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, format!("{rev}\n"))?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// The cargo-installed fno-agents binary, or None. Deliberately the cargo
/// install location, NOT the resolver: a bundled-wheel binary refreshes via
/// pip, not cargo.
fn cargo_installed_bin() -> Option<PathBuf> {
    cargo_bin(&format!("fno-agents{}", exe_suffix()))
}

/// The cargo-installed mux front door (`fno`), same `$CARGO_HOME/bin`.
fn cargo_installed_mux() -> Option<PathBuf> {
    cargo_bin(&format!("fno{}", exe_suffix()))
}

fn cargo_bin(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cargo"));
    let candidate = home.join("bin").join(name);
    candidate.is_file().then_some(candidate)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `which`-style PATH lookup.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// One native `fno-agents source-pin` invocation; None = cannot answer.
fn source_pin_call(sub: &str, extra: &[String], input: Option<&str>) -> Result<Value, String> {
    let mut args = vec!["source-pin".to_string(), sub.to_string()];
    args.extend(extra.iter().cloned());
    let (code, out, err) = run_bounded(&fno_agents_bin(), &args, Duration::from_secs(30), input)?;
    if code != 0 {
        let head: String = err.trim().chars().take(200).collect();
        if head.is_empty() {
            return Err(format!(
                "fno-agents source-pin {sub} exited {code} with no stderr"
            ));
        }
        return Err(head);
    }
    let data: Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("fno-agents source-pin {sub} reply is not JSON: {e}"))?;
    if data.is_object() {
        Ok(data)
    } else {
        Err(format!(
            "fno-agents source-pin {sub} reply is not an object"
        ))
    }
}

/// The machine-global events journal: the store every `update-journal`
/// envelope lands in and `--check` reads back. Same resolution Python's
/// `paths.state_dir() / "events.jsonl"` applied.
fn events_journal() -> PathBuf {
    crate::model_catalog::state_dir().join("events.jsonl")
}

/// One `fno-agents update-journal` door call: best-effort, 60s bounded, a
/// wedged door warns through its own stderr and never blocks the step's
/// verdict. Fields ride as `--flag value` pairs (`-` for `_`).
fn journal_call(type_name: &str, fields: &[(&str, String)], mail_from: Option<&str>) {
    let mut args = vec![
        "update-journal".to_string(),
        "--events".to_string(),
        events_journal().to_string_lossy().into_owned(),
        "--type".to_string(),
        type_name.to_string(),
    ];
    for (flag, value) in fields {
        args.push(format!("--{}", flag.replace('_', "-")));
        args.push(value.clone());
    }
    if let Some(from) = mail_from {
        args.push("--mail-from".into());
        args.push(from.to_string());
    }
    let _ = run_bounded(&fno_agents_bin(), &args, Duration::from_secs(60), None);
}

/// The newest `fno_update_*` row in the machine journal, or None. Bounded
/// read of the last 1 MiB: the journal is machine-global and unbounded, so
/// --check must not read it whole on every TUI refresh. An update row older
/// than 1 MiB of fleet traffic reads as None until the next update writes
/// a fresh one.
fn last_update_event() -> Option<Value> {
    use std::io::{Read, Seek, SeekFrom};

    const WINDOW: u64 = 1024 * 1024;
    let path = events_journal();
    let mut file = std::fs::File::open(&path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(WINDOW);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .rev()
        .filter(|line| line.contains("fno_update_"))
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
}

/// One native resolution: path, eligibility evidence, allow/refuse, warning.
/// The Err carries the machine cause (missing binary, timeout, exit, malformed
/// reply) so no caller prints "failed" without a reason.
fn resolve_source_pin(override_path: Option<&Path>) -> Result<Value, String> {
    let mut extra: Vec<String> = Vec::new();
    if let Some(o) = override_path {
        extra.push("--override".into());
        extra.push(o.to_string_lossy().into_owned());
    }
    if let Some(env_source) = std::env::var_os("FNO_SOURCE") {
        extra.push("--env-source".into());
        extra.push(PathBuf::from(env_source).to_string_lossy().into_owned());
    }
    extra.push("--cache".into());
    extra.push(cache_file().to_string_lossy().into_owned());
    // Search order matters: plugin install first (most users), then dev clone.
    for c in [
        home_dir()
            .join(".claude")
            .join("plugins")
            .join("fno")
            .join("cli"),
        home_dir().join("code").join("me").join("fno").join("cli"),
    ] {
        extra.push("--candidate".into());
        extra.push(c.to_string_lossy().into_owned());
    }
    source_pin_call("resolve", &extra, None)
}

/// Locate the fno CLI source directory via the native source-pin authority.
/// A refused-but-resolved pin still returns its path: `fno doctor` probes the
/// resolved checkout regardless of the update gate; the update enforces the
/// refusal itself.
fn discover_source(override_path: Option<&Path>) -> Result<PathBuf, String> {
    let pin = resolve_source_pin(override_path)
        .map_err(|e| format!("{SOURCE_PIN_UNAVAILABLE} (cause: {e})"))?;
    match pin
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
    {
        Some(p) => Ok(PathBuf::from(p)),
        None => Err(pin
            .get("refusal")
            .and_then(Value::as_str)
            .unwrap_or("source checkout not resolvable")
            .to_string()),
    }
}

/// Record both pins natively (companion + legacy path file). A failed record
/// is a failed step, never a warning; the caller owns the verdict.
fn record_source_pin(pin: &Value) -> Result<(), String> {
    if pin.get("path").and_then(Value::as_str).is_none() {
        return Ok(());
    }
    let body = serde_json::to_string(pin).map_err(|e| e.to_string())?;
    let extra = vec![
        "--cache".into(),
        cache_file().to_string_lossy().into_owned(),
        "--companion".into(),
        companion_file().to_string_lossy().into_owned(),
    ];
    match source_pin_call("record", &extra, Some(&body)) {
        Ok(_) => Ok(()),
        Err(cause) => Err(format!(
            "source-pin record failed; the previous pin stands ({cause})"
        )),
    }
}

/// True when target-state.md in the current repo shows status: IN_PROGRESS.
/// Lenient on a missing or malformed file; an unreadable one hides an active
/// loop, so it fails safe as IN_PROGRESS.
pub(crate) fn target_in_progress() -> bool {
    let repo_root = match repo_root() {
        Some(r) => r,
        None => return false,
    };
    let state_path = repo_root.join(".fno").join("target-state.md");
    let content = match std::fs::read_to_string(&state_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(e) => {
            eprintln!(
                "fno doctor update: target-state.md at {} could not be read ({e}); assuming IN_PROGRESS for safety",
                state_path.display()
            );
            return true;
        }
    };
    // YAML front-matter between the first two `---` lines.
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    if parts.len() < 3 {
        return false;
    }
    parts[1].contains("status: IN_PROGRESS")
}

/// `FNO_REPO_ROOT` env, else `git rev-parse --show-toplevel` from the cwd.
fn repo_root() -> Option<PathBuf> {
    if let Some(r) = std::env::var_os("FNO_REPO_ROOT") {
        if !r.is_empty() {
            return Some(PathBuf::from(r));
        }
    }
    git_in(
        &std::env::current_dir().ok()?,
        &["rev-parse", "--show-toplevel"],
    )
    .map(PathBuf::from)
}

/// The one refusal for a bindir whose probe cannot run at all: the probe
/// lives inside the deployed binary, so a poisoned client path cannot repair
/// anything, including itself. Name the path and the three-command repair.
fn install_exec_dead(verdict_bin: &Path, prefix: &str) -> String {
    let p = verdict_bin.display();
    format!(
        "{prefix}: ERROR: install-exec-dead {p}; repair with: cp {p} {p}.fix && mv {p}.fix {p} && chmod 755 {p}"
    )
}

/// One call to the native component verdict; None when it cannot answer -
/// never fresh.
#[allow(clippy::too_many_arguments)]
fn component_verdict(
    source: &Path,
    subtree: &str,
    bindir: &Path,
    verdict_bin: &Path,
    attempted: bool,
    include_mux: Option<bool>,
    python_tool: Option<(&str, Option<&str>, Option<&str>, Option<&str>)>,
) -> Option<Value> {
    let mut args: Vec<String> = [
        "component-verdict".to_string(),
        "--bindir".into(),
        bindir.to_string_lossy().into_owned(),
        "--expected".into(),
        subtree.to_string(),
        "--agents-dir".into(),
        source
            .parent()?
            .join("crates")
            .join("fno-agents")
            .to_string_lossy()
            .into_owned(),
    ]
    .to_vec();
    let mux_dir_exists = source
        .parent()
        .map(|p| p.join("crates").join("fno").is_dir())
        .unwrap_or(false);
    if include_mux.unwrap_or(mux_dir_exists) {
        args.push("--include-mux".into());
    }
    if attempted {
        args.push("--attempted".into());
    }
    if let Some((rev, expected, evidence, error)) = python_tool {
        args.push("--python-rev".into());
        args.push(rev.to_string());
        for (flag, v) in [
            ("--python-expected", expected),
            ("--python-evidence", evidence),
            ("--python-error", error),
        ] {
            if let Some(v) = v {
                args.push(flag.into());
                args.push(v.to_string());
            }
        }
    }
    let Ok((code, out, err)) = run_bounded(verdict_bin, &args, Duration::from_secs(90), None)
    else {
        return None;
    };
    if code != 0 {
        let head: String = err.trim().chars().take(200).collect();
        eprintln!("component-verdict failed: {head}");
        return None;
    }
    let report: Value = serde_json::from_str(out.trim()).ok()?;
    if report.get("components").and_then(Value::as_array).is_some() {
        Some(report)
    } else {
        None
    }
}

/// Non-fresh rows with their native one-liner. A None report carries no rows
/// here: the verdict-would-not-run refusal names the path at the call site
/// (`install_exec_dead`), because a generic "unavailable" line fails nothing.
pub(crate) fn component_lines(report: Option<&Value>, prefix: &str) -> Vec<String> {
    report
        .and_then(|r| r.get("components").and_then(Value::as_array))
        .map(|rows| {
            rows.iter()
                .filter(|c| {
                    !matches!(
                        c.get("status").and_then(Value::as_str),
                        Some("fresh") | Some("updated")
                    ) && c.get("line").and_then(Value::as_str).is_some()
                })
                .map(|c| {
                    format!(
                        "{prefix}: {}",
                        c.get("line").and_then(Value::as_str).unwrap_or_default()
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Deduped install-location dirs that already host >=1 of the triad bins.
/// NEVER seeds a new location (locked decision 4). Candidates: the uv tool
/// venv bin and PATH. The cargo dir may appear via PATH; the caller skips it.
fn triad_install_dirs() -> Vec<PathBuf> {
    let names = triad_names();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(d) = uv_tool_dir() {
        candidates.push(d);
    }
    if let Some(onpath) = on_path(&names[0]) {
        if let Some(parent) = onpath.parent() {
            candidates.push(parent.to_path_buf());
        }
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    for d in dedup_paths(candidates) {
        let Ok(rd) = d.canonicalize() else {
            continue;
        };
        let hosts = names.iter().any(|n| rd.join(n).is_file());
        if rd.is_dir() && hosts {
            dirs.push(rd);
        }
    }
    dirs
}

/// Order-preserving dedup of path candidates.
fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if !seen.contains(&p) {
            seen.push(p.clone());
            out.push(p);
        }
    }
    out
}

/// Byte comparison, the `filecmp.cmp(shallow=False)` port.
pub(crate) fn file_eq(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Propagate the freshly-built triad from `cargo_bin_dir` into every OTHER
/// live install location that already hosts one of the three bins, so
/// client, daemon, and worker stay a coherent same-build set wherever the
/// resolver might pick one up (locked decisions 3-4). Per-location
/// atomicity: each bin copied to a temp name then renamed, so running
/// processes keep their inode and the next spawn gets the new bin. A
/// location that cannot take the full triad FAILS the step, naming the
/// location and the bins already copied: a mixed-version pair is the worse
/// bug, never left silently half-copied.
fn sync_triad(cargo_bin_dir: &Path, dry_run: bool) -> Result<(), String> {
    let names = triad_names();
    let complete = names.iter().all(|n| cargo_bin_dir.join(n).is_file());
    if !complete {
        return Ok(());
    }
    let cargo_resolved = cargo_bin_dir
        .canonicalize()
        .unwrap_or_else(|_| cargo_bin_dir.to_path_buf());
    for dest in triad_install_dirs() {
        if dest == cargo_resolved {
            continue;
        }
        let hosts_any = names.iter().any(|n| dest.join(n).is_file());
        if !hosts_any {
            continue;
        }
        let identical = names
            .iter()
            .all(|n| file_eq(&cargo_bin_dir.join(n), &dest.join(n)));
        if identical {
            continue;
        }
        if dry_run {
            println!("Would sync leg -> {}", dest.display());
            continue;
        }
        let mut copied: Vec<String> = Vec::new();
        for n in &names {
            let t = dest.join(format!(".{n}.{}.tmp", std::process::id()));
            if let Err(e) = std::fs::copy(cargo_bin_dir.join(n), &t) {
                let _ = std::fs::remove_file(&t);
                return Err(format!(
                    "fno doctor update: ERROR: triad sync FAILED at {} ({}). Copied {} before the failure; this location may now hold a MIXED-VERSION fno-agents triad. Fix the location and re-run `fno doctor update` (or set FNO_AGENTS_DAEMON_BIN to a coherent triad dir).",
                    dest.display(),
                    e,
                    copied_label(&copied),
                ));
            }
            std::fs::rename(&t, dest.join(n))
                .map_err(|e| format!("triad rename failed at {}: {}", dest.display(), e))?;
            copied.push(n.clone());
        }
        println!(
            "fno doctor update: synced fno-agents triad -> {}",
            dest.display()
        );
    }
    Ok(())
}

fn copied_label(copied: &[String]) -> String {
    if copied.is_empty() {
        "none".to_string()
    } else {
        copied.join(", ")
    }
}

/// Best-effort: install the crates/fno mux binary (`fno` on PATH, the front
/// door) into the same --root as the agents bins, then the `footnote` harness
/// binary fno-agents launches for `-H footnote`. Warn-and-continue: the mux
/// is heavier to build (tokio + pty), and an absent/stale mux is a front-door
/// problem `fno doctor` surfaces. Returns the mux install's result.
fn install_mux_front_door(source: &Path, install_root: &Path, dry_run: bool) -> bool {
    let mux = install_crate_bins(
        source,
        install_root,
        "fno",
        "mux front door",
        "fno",
        dry_run,
    );
    install_crate_bins(
        source,
        install_root,
        "footnote",
        "footnote harness",
        "footnote",
        dry_run,
    );
    mux
}

/// `cargo install --path crates/<crate_name> --bins` into `install_root`.
/// False when the crate is absent, on a dry run, or when the install fails.
fn install_crate_bins(
    source: &Path,
    install_root: &Path,
    crate_name: &str,
    what: &str,
    bin: &str,
    dry_run: bool,
) -> bool {
    let Some(src_parent) = source.parent() else {
        return false;
    };
    let crate_dir = src_parent.join("crates").join(crate_name);
    if !crate_dir.is_dir() {
        return false;
    }
    let args: Vec<String> = [
        "install".to_string(),
        "--path".into(),
        crate_dir.to_string_lossy().into_owned(),
        "--bins".into(),
        "--root".into(),
        install_root.to_string_lossy().into_owned(),
    ]
    .to_vec();
    if dry_run {
        println!("Would run: cargo {}", args.join(" "));
        return false;
    }
    println!(
        "fno doctor update: refreshing {what}: cargo {}",
        args.join(" ")
    );
    let code = run_cargo_install(source, &args);
    if code != 0 {
        eprintln!(
            "fno doctor update: WARNING: {what} install failed (exit {code}); `{bin}` may be absent/stale; continuing"
        );
        return false;
    }
    println!("fno doctor update: {what} refreshed (crates/{crate_name} -> `{bin}`)");
    true
}

/// Chain the crate's drift-gated daemon swap; the verb is quiet on fresh or
/// down. A failed swap keeps its last stderr line for the closing verdict.
fn chained_restart_if_drifted(binary: &Path, dry_run: bool) {
    let args: Vec<String> = ["restart".into(), "--if-drifted".into()].to_vec();
    if dry_run {
        println!("Would run: {} {}", binary.display(), args.join(" "));
        return;
    }
    if let Ok((code, _out, err)) = run_bounded(binary, &args, Duration::from_secs(120), None) {
        if code != 0 {
            let cause = err
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            let _ = RESTART_FAILURE.set(format!("the daemon swap exited {code}: {}", cause.trim()));
        }
    }
}

static RESTART_FAILURE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Bring the source checkout to its upstream before anything builds: fetch,
/// then fast-forward, or refuse naming how far behind it is. On 2026-10-07 an
/// update built a checkout 122 commits behind origin/main and called the old
/// bins fresh.
pub(crate) fn sync_source_checkout(source: &Path, dry_run: bool) -> Result<(), String> {
    // A packaged source is no git checkout: there is nothing to sync.
    let Some(repo) = git_in(source, &["rev-parse", "--show-toplevel"]).map(PathBuf::from) else {
        return Ok(());
    };
    let Some(upstream) = git_in(
        &repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    ) else {
        println!(
            "fno doctor update: {} has no upstream branch; building its local HEAD as it stands",
            repo.display()
        );
        return Ok(());
    };
    let git = |args: &[&str], bound: u64| {
        let mut argv = vec!["-C".to_string(), repo.to_string_lossy().into_owned()];
        argv.extend(args.iter().map(|a| a.to_string()));
        run_bounded(Path::new("git"), &argv, Duration::from_secs(bound), None)
    };
    let first_line = |text: &str| {
        text.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    if dry_run {
        println!(
            "Would run: git -C {} fetch, then merge --ff-only {upstream}",
            repo.display()
        );
        return Ok(());
    }
    match git(&["fetch", "--quiet"], 120) {
        Ok((0, _, _)) => {}
        Ok((code, _, err)) => eprintln!(
            "fno doctor update: git fetch exited {code} ({}); checking against the last fetched {upstream}",
            first_line(&err)
        ),
        Err(e) => eprintln!(
            "fno doctor update: git fetch did not finish ({e}); checking against the last fetched {upstream}"
        ),
    }
    let behind = git_in(&repo, &["rev-list", "--count", "HEAD..@{u}"])
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);
    if behind == 0 {
        return Ok(());
    }
    match git(&["merge", "--ff-only", "--quiet", "@{u}"], 120) {
        Ok((0, _, _)) => {
            println!(
                "fno doctor update: fast-forwarded {} by {behind} commit(s) to {upstream}",
                repo.display()
            );
            Ok(())
        }
        Ok((_, out, err)) => Err(format!(
            "fno doctor update: refused: {} is {behind} commit(s) behind {upstream} and cannot fast-forward ({}). Run `git -C {} pull --ff-only`, then re-run.",
            repo.display(),
            first_line(if err.trim().is_empty() { &out } else { &err }),
            repo.display()
        )),
        Err(e) => Err(format!(
            "fno doctor update: refused: {} is {behind} commit(s) behind {upstream} and the fast-forward did not finish ({e}).",
            repo.display()
        )),
    }
}

/// The closing verdict: does the running daemon run the build on disk? Ok
/// when it does or when none runs (the next verb runs the build on disk).
fn daemon_verdict() -> Result<String, String> {
    let bin = fno_agents_bin();
    let rev = run_bounded(
        &bin,
        &["version".into(), "--json".into()],
        Duration::from_secs(30),
        None,
    )
    .ok()
    .and_then(|(_, out, _)| serde_json::from_str::<Value>(out.trim()).ok())
    .and_then(|v| {
        v.get("git_rev")
            .and_then(Value::as_str)
            .map(|r| r.chars().take(12).collect::<String>())
    })
    .unwrap_or_else(|| "unknown".into());
    let restart_failure = RESTART_FAILURE
        .get()
        .map(|f| format!("; {f}"))
        .unwrap_or_default();
    match run_bounded(
        &bin,
        &["status".into(), "--json".into()],
        Duration::from_secs(30),
        None,
    ) {
        Ok((0, out, _)) => {
            let status: Value = serde_json::from_str(out.trim()).unwrap_or(Value::Null);
            let pid = status
                .pointer("/daemon/pid")
                .and_then(Value::as_u64)
                .map_or("?".to_string(), |p| p.to_string());
            match status.get("drift").and_then(Value::as_str) {
                Some("fresh") => Ok(format!("daemon pid {pid} runs the build on disk ({rev})")),
                Some("drifted") => Err(format!(
                    "daemon pid {pid} still runs an older build, not {rev}{restart_failure}; run `fno restart`"
                )),
                _ => Err(format!(
                    "could not confirm which build daemon pid {pid} runs{restart_failure}; run `fno restart`"
                )),
            }
        }
        Ok((13, _, _)) => Ok(format!(
            "no daemon running; the next fno-agents verb runs the build on disk ({rev})"
        )),
        Ok((code, _, err)) => Err(format!(
            "the daemon status read exited {code} ({}){restart_failure}",
            err.lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
        )),
        Err(e) => Err(format!(
            "the daemon status read did not finish ({e}){restart_failure}"
        )),
    }
}

/// Live mux sessions on a wire below the compatibility floor: the `stale`
/// field `fno mux ls --json` computes from each server's .ver sidecar. A
/// pre-sidecar server has no .ver and reads as stale, so the check works
/// across the upgrade that introduces it. Best-effort: any failure yields
/// an empty vec.
fn stale_mux_servers() -> Vec<String> {
    let fno_bin = cargo_installed_mux().or_else(|| on_path("fno"));
    let Some(fno_bin) = fno_bin else {
        return Vec::new();
    };
    let args: Vec<String> = ["mux".into(), "ls".into(), "--json".into()].to_vec();
    let Ok((code, out, _err)) = run_bounded(&fno_bin, &args, Duration::from_secs(5), None) else {
        return Vec::new();
    };
    if code != 0 {
        return Vec::new();
    }
    let rows: Value = serde_json::from_str(out.trim()).unwrap_or(Value::Null);
    match rows.as_array() {
        Some(arr) => stale_sessions_from_rows(arr),
        None => Vec::new(),
    }
}

/// The `stale` verdict fold: only LIVE rows on a wire below the floor. A
/// pre-sidecar server has no .ver and reads as stale, so the check works
/// across the upgrade that introduces it.
pub(crate) fn stale_sessions_from_rows(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .filter_map(|r| {
            let live = r.get("state").and_then(Value::as_str) == Some("live");
            let stale = r.get("stale").and_then(Value::as_bool).unwrap_or(false);
            let session = r.get("session").and_then(Value::as_str);
            if live && stale {
                session.map(str::to_string)
            } else {
                None
            }
        })
        .collect()
}

/// Refresh the cargo-installed fno-agents rust bins if stale. One line of
/// feedback per path. `failed` collects the step names that failed; the
/// caller turns any non-empty vec into exit 1.
fn refresh_rust_bins(
    source: &Path,
    force: bool,
    dry_run: bool,
    failed: &mut Vec<String>,
) -> String {
    let Some(src_parent) = source.parent() else {
        println!("fno doctor update: no crates/fno-agents directory found; skipping rust leg");
        return "skipped-no-crate".into();
    };
    let crate_dir = src_parent.join("crates").join("fno-agents");
    if !crate_dir.is_dir() {
        println!("fno doctor update: no crates/fno-agents directory found; skipping rust leg");
        return "skipped-no-crate".into();
    }
    let installed_bin = cargo_installed_bin();
    if installed_bin.is_none() && !force {
        println!(
            "fno doctor update: no cargo-installed fno-agents binary; skipping rust leg (pass --rust to install)"
        );
        return "skipped-no-binary".into();
    }
    let subtree = rust_subtree_rev(source);
    if subtree.is_none() && !force {
        println!("fno doctor update: could not determine crates/ subtree rev; skipping rust leg");
        return "skipped-no-rev".into();
    }
    // Freshness is proven by the binaries themselves via one native probe:
    // an absent, stale or unanswerable component falls through to cargo.
    let pre = if !force {
        installed_bin.as_ref().and_then(|bin| {
            let subtree = subtree.clone()?;
            component_verdict(
                source,
                &subtree,
                bin.parent().unwrap_or(bin),
                bin,
                false,
                Some(true),
                None,
            )
        })
    } else {
        None
    };
    let pre_rows = component_rows(pre.as_ref());
    let installed_rev = pre_rows
        .get("fno-agents")
        .and_then(|c| c.get("observed_rev"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let all_fresh = installed_bin.is_some()
        && subtree.is_some()
        && triad_names().iter().all(|n| {
            pre_rows
                .get(n.trim_end_matches(exe_suffix()))
                .map(|c| {
                    matches!(
                        c.get("status").and_then(Value::as_str),
                        Some("fresh") | Some("updated")
                    )
                })
                .unwrap_or(false)
        });
    if all_fresh {
        let mux_fresh = pre_rows
            .get("fno")
            .map(|c| {
                matches!(
                    c.get("status").and_then(Value::as_str),
                    Some("fresh") | Some("updated")
                )
            })
            .unwrap_or(false);
        return refresh_fresh_path(
            source,
            subtree.as_deref(),
            installed_bin.as_deref().unwrap(),
            &installed_rev,
            mux_fresh,
            dry_run,
            failed,
        );
    }
    let install_root = match &installed_bin {
        Some(b) => b
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cargo_default_home()),
        None => cargo_default_home(),
    };
    // CI already built this crates/ rev on its main merge: download it. A
    // local compile ran 2 hours at load 297, so it is the fallback only.
    let prebuilt = match subtree.as_deref() {
        Some(st) => deploy_prebuilt(st, &install_root.join("bin"), dry_run),
        None => Err("the crates/ rev is unknown".to_string()),
    };
    if let Err(why) = &prebuilt {
        println!("fno doctor update: no CI build to install ({why}); compiling from source");
    }
    if dry_run && prebuilt.is_ok() {
        return "dry-run".into();
    }
    if prebuilt.is_err() {
        if which_cargo().is_none() {
            eprintln!(
                "fno doctor update: WARNING: rust bins need refresh but cargo is not on PATH; skipping"
            );
            render_component_evidence(source, subtree.as_deref(), &install_root);
            return "skipped-no-cargo".into();
        }
        let args: Vec<String> = [
            "install".to_string(),
            "--path".into(),
            crate_dir.to_string_lossy().into_owned(),
            "--bins".into(),
            "--root".into(),
            install_root.to_string_lossy().into_owned(),
        ]
        .to_vec();
        if dry_run {
            println!("Would run: cargo {}", args.join(" "));
            install_mux_front_door(source, &install_root, true);
            return "dry-run".into();
        }
        println!(
            "fno doctor update: refreshing rust bins: cargo {}",
            args.join(" ")
        );
        if run_cargo_install(source, &args) != 0 {
            eprintln!(
                "fno doctor update: WARNING: cargo install failed; rust bins NOT refreshed; continuing with the install"
            );
            render_component_evidence(source, subtree.as_deref(), &install_root);
            failed.push("rust bins refresh".into());
            return "failed".into();
        }
    }
    // Post-deploy verify: cargo can exit 0 yet deploy stale bytes, so the
    // triad is re-probed natively; the client must prove current first.
    let bin_dir = install_root.join("bin");
    let verdict_bin = bin_dir.join(triad_names()[0].clone());
    let verdict_bin = if verdict_bin.is_file() {
        verdict_bin
    } else {
        installed_bin.clone().unwrap_or_else(|| verdict_bin)
    };
    let post_report = match &subtree {
        Some(st) if verdict_bin.is_file() => {
            component_verdict(source, st, &bin_dir, &verdict_bin, true, None, None)
        }
        _ => None,
    };
    if subtree.is_some() && post_report.is_none() {
        eprintln!("{}", install_exec_dead(&verdict_bin, "fno doctor update"));
        failed.push("rust bins post-deploy verify".into());
        return "failed".into();
    }
    let client_fresh = post_report
        .as_ref()
        .and_then(|r| row_status(r, "fno-agents"))
        .map(|s| matches!(s.as_str(), "fresh" | "updated"))
        .unwrap_or(false);
    if subtree.is_some() && !client_fresh {
        for line in component_lines(post_report.as_ref(), "fno doctor update") {
            eprintln!("{line}");
        }
        eprintln!(
            "fno doctor update: ERROR: post-deploy verify FAILED - the deployed fno-agents did not prove current (details above). NOT continuing."
        );
        failed.push("rust bins post-deploy verify".into());
        return "failed".into();
    }
    // The tarball carries the front door; only a compile builds it apart.
    if prebuilt.is_err() {
        install_mux_front_door(source, &install_root, false);
    }
    if let Err(e) = sync_triad(&bin_dir, false) {
        eprintln!("{e}");
        failed.push("triad sync".into());
        return "failed".into();
    }
    chained_restart_if_drifted(&verdict_bin, false);
    // Final proof: re-probe as finally deployed; an attempted build alone is
    // never freshness.
    let final_report = match &subtree {
        Some(st) if verdict_bin.is_file() => {
            component_verdict(source, st, &bin_dir, &verdict_bin, true, None, None)
        }
        _ => None,
    };
    if subtree.is_some() && final_report.is_none() {
        eprintln!("{}", install_exec_dead(&verdict_bin, "fno doctor update"));
        failed.push("rust bins post-deploy verify".into());
        return "failed".into();
    }
    let post_converged = final_report
        .as_ref()
        .map(|r| r.get("converged").and_then(Value::as_bool).unwrap_or(false))
        .unwrap_or(false);
    if !post_converged {
        for line in component_lines(final_report.as_ref(), "fno doctor update") {
            eprintln!("{line}");
        }
    }
    let outcome = match &subtree {
        None => {
            println!(
                "fno doctor update: rust bins refreshed (marker not written: rev undeterminable)"
            );
            "refreshed-no-marker"
        }
        Some(st) => {
            println!(
                "fno doctor update: rust bins refreshed (rev {})",
                &st[..st.len().min(12)]
            );
            "refreshed"
        }
    };
    // Best-effort mux advisory: a long-running server keeps speaking the OLD
    // proto after this refresh.
    for sess in stale_mux_servers() {
        eprintln!(
            "fno doctor update: note: mux server '{sess}' speaks an OLD wire version (a new client can't attach it); run 'fno agents restart' to heal pane-less servers, or use 'fno agents restart --mux' to force-kill and end live panes"
        );
    }
    if outcome == "refreshed" && !post_converged {
        return "partial".into();
    }
    outcome.into()
}

/// The fresh-path arm: the pre-verdict proved the triad current; refresh a
/// stale/absent mux front door, confirm convergence, sync, restart-chain.
fn refresh_fresh_path(
    source: &Path,
    subtree: Option<&str>,
    installed_bin: &Path,
    installed_rev: &str,
    mux_fresh: bool,
    dry_run: bool,
    failed: &mut Vec<String>,
) -> String {
    println!(
        "fno doctor update: rust bins fresh (rev {} from binary); skipping cargo install",
        &installed_rev[..installed_rev.len().min(12)]
    );
    if !mux_fresh {
        let root = installed_bin
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(cargo_default_home);
        // The tarball's triad is built from the same crates/ rev as the
        // fresh one, so replacing it with the front door is safe.
        let prebuilt = match subtree {
            Some(st) => deploy_prebuilt(st, &root.join("bin"), dry_run),
            None => Err("the crates/ rev is unknown".to_string()),
        };
        if let Err(why) = prebuilt {
            println!("fno doctor update: no CI build to install ({why}); compiling the front door");
            install_mux_front_door(source, &root, dry_run);
        }
    }
    let report = component_verdict(
        source,
        subtree.unwrap_or_default(),
        installed_bin.parent().unwrap_or(installed_bin),
        installed_bin,
        !dry_run,
        None,
        None,
    );
    if report.is_none() {
        eprintln!("{}", install_exec_dead(installed_bin, "fno doctor update"));
        failed.push("rust bins verdict".into());
        return "failed".into();
    }
    let converged = report
        .as_ref()
        .map(|r| r.get("converged").and_then(Value::as_bool).unwrap_or(false))
        .unwrap_or(false);
    if !converged {
        for line in component_lines(report.as_ref(), "fno doctor update") {
            println!("{line}");
        }
    }
    if let Err(e) = sync_triad(installed_bin.parent().unwrap_or(installed_bin), dry_run) {
        eprintln!("{e}");
        failed.push("triad sync".into());
        return "failed".into();
    }
    chained_restart_if_drifted(installed_bin, dry_run);
    if converged {
        "fresh".into()
    } else {
        "partial".into()
    }
}

/// The `status` of one named component row, or None.
pub(crate) fn row_status(report: &Value, component: &str) -> Option<String> {
    report
        .get("components")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|c| c.get("component").and_then(Value::as_str) == Some(component))
                .and_then(|c| c.get("status").and_then(Value::as_str))
                .map(str::to_string)
        })
}

/// Map a verdict report to component name -> row, for row lookups.
pub(crate) fn component_rows(report: Option<&Value>) -> std::collections::HashMap<String, Value> {
    let mut map = std::collections::HashMap::new();
    if let Some(rows) = report
        .and_then(|r| r.get("components"))
        .and_then(Value::as_array)
    {
        for c in rows {
            if let Some(name) = c.get("component").and_then(Value::as_str) {
                map.insert(name.to_string(), c.clone());
            }
        }
    }
    map
}

fn cargo_default_home() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cargo"))
}

fn which_cargo() -> Option<PathBuf> {
    on_path(if cfg!(windows) { "cargo.exe" } else { "cargo" })
}

/// Name what did not converge; the update still proceeds.
fn render_component_evidence(source: &Path, subtree: Option<&str>, install_root: &Path) {
    let Some(subtree) = subtree else {
        return;
    };
    let bin_dir = install_root.join("bin");
    let verdict_bin =
        cargo_installed_bin().unwrap_or_else(|| bin_dir.join(triad_names()[0].clone()));
    let report = component_verdict(source, subtree, &bin_dir, &verdict_bin, false, None, None);
    if report.is_none() {
        eprintln!("{}", install_exec_dead(&verdict_bin, "fno doctor update"));
        return;
    }
    for line in component_lines(report.as_ref(), "fno doctor update") {
        eprintln!("{line}");
    }
}

/// The fno-py console script: PATH first, else the uv tool venv bin.
/// Absolute when found; the bare name is the last resort (PATH may still
/// find it at spawn time).
fn resolve_fno_py() -> PathBuf {
    on_path(&format!("fno-py{}", exe_suffix())).unwrap_or_else(|| {
        uv_tool_dir()
            .map(|d| {
                d.join("fno")
                    .join("bin")
                    .join(format!("fno-py{}", exe_suffix()))
            })
            .unwrap_or_else(|| PathBuf::from("fno-py"))
    })
}

/// Bounded wait for the console script to reappear: uv deletes and recreates
/// `<tools>/fno/bin/fno-py` for roughly half a second mid-install, so the
/// post-install chain waits up to 3s (15 x 0.2s), the budget every
/// provisioning path spends. True when the binary is there.
fn await_fno_py(fno_py: &Path, dry_run: bool) -> bool {
    if dry_run {
        return true;
    }
    for _ in 0..15 {
        if fno_py.is_file() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!(
        "fno doctor update: fno-py never reappeared after the install, so the launchd agents were NOT refreshed onto the new binary. Run by hand: fno do pr watch refresh; fno backlog groom --refresh-agent"
    );
    false
}

/// Best-effort commands chained after a successful install, each verb
/// self-gating. The plugin-stage restage rides last: live sessions exec
/// hooks straight from the stage, so a restage IS the deploy; the codex
/// refresh follows the restage when its channel marker says dev.
fn post_install_steps(resolved: &Path, failed: &mut Vec<String>) {
    let fno_py = resolve_fno_py();
    if !await_fno_py(&fno_py, false) {
        failed.push("fno-py reappearance (launchd refresh skipped)".into());
        return;
    }
    let steps: Vec<(String, Vec<String>)> = vec![
        (
            "pr watch refresh".into(),
            vec!["do".into(), "pr".into(), "watch".into(), "refresh".into()],
        ),
        (
            "groom agent refresh".into(),
            vec!["backlog".into(), "groom".into(), "--refresh-agent".into()],
        ),
    ];
    for (name, args) in steps {
        let code = run_inherit(&fno_py, &args);
        if code != 0 {
            eprintln!(
                "fno doctor update: WARNING: post-install `{name}` exited {code}; continuing"
            );
            failed.push(format!("post-install {name}"));
        }
    }
    if let Some(agents_bin) = cargo_installed_bin() {
        let args: Vec<String> = [
            "plugin-install".into(),
            "--restage".into(),
            "--source".into(),
            resolved.to_string_lossy().into_owned(),
        ]
        .to_vec();
        let code = run_inherit(&agents_bin, &args);
        if code != 0 {
            eprintln!(
                "fno doctor update: WARNING: post-install plugin restage exited {code}; continuing"
            );
            failed.push("post-install plugin restage".into());
        }
        // A restage keeps the version; codex re-copies its cache only on a
        // forced converge, so the dev-channel marker gates the refresh.
        if codex_channel_is_dev() {
            let args: Vec<String> = [
                "config".into(),
                "plugin".into(),
                "install".into(),
                "codex".into(),
                "--force".into(),
            ]
            .to_vec();
            let code = run_inherit(&fno_py, &args);
            if code != 0 {
                eprintln!(
                    "fno doctor update: WARNING: post-install codex refresh exited {code}; continuing"
                );
                failed.push("post-install codex refresh".into());
            }
        }
    }
}

/// The codex dev-channel marker: `$CODEX_HOME/footnote/plugin-channel.json`
/// (env, else ~/.codex), field `channel == "dev"`. Unreadable = not dev.
fn codex_channel_is_dev() -> bool {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".codex"));
    let Ok(text) = std::fs::read_to_string(home.join("footnote").join("plugin-channel.json"))
    else {
        return false;
    };
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    v.get("channel").and_then(Value::as_str) == Some("dev")
}

/// The uv install, run natively with the ENOTEMPTY retry: only the
/// directory-race signature (uv's removal walk racing a concurrent
/// importer's bytecode rewrite) retries, bounded at UV_INSTALL_ATTEMPTS; any
/// other failure prints uv's stderr verbatim and stops. Success is accepted
/// only via a positive marker - the fno-py console script plus shipped
/// bytecode under the tool venv - never the exit code alone, re-checked up
/// to 3s after uv exits (uv exits before its own artifacts settle).
fn uv_install(cmd: &[String]) -> bool {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let (code, _out, err) = match run_captured(Path::new("uv"), &cmd[1..], None) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("fno doctor update: uv transport failed: {e}");
                return false;
            }
        };
        if code == 0 {
            if verify_uv_install() {
                return true;
            }
            eprintln!(
                "fno: uv exited 0 but the install does not verify after waiting 3s (no fno-py script or no shipped bytecode under the tool venv)"
            );
            return false;
        }
        let race = err.contains("Directory not empty") && err.contains("os error 66");
        eprint!("{err}");
        let _ = std::io::stderr().flush();
        if race && attempt < UV_INSTALL_ATTEMPTS {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        if race {
            eprintln!(
                "fno: uv tool install hit the directory race (os error 66) three times. A concurrent fno process is rewriting bytecode into the venv mid-removal. Stop fno processes and re-run."
            );
        }
        return false;
    }
}

/// The positive install marker: the fno-py console script plus at least one
/// shipped .pyc under the tool venv, re-checked up to 15 x 0.2s.
fn verify_uv_install() -> bool {
    let Some(td) = uv_tool_dir() else {
        return false;
    };
    for _ in 0..15 {
        let script = td
            .join("fno")
            .join("bin")
            .join(format!("fno-py{}", exe_suffix()));
        if script.is_file() && has_shipped_pyc(&td) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// `uv tool dir`, or None.
fn uv_tool_dir() -> Option<PathBuf> {
    let mut cmd = crate::process_admission::std_command("uv");
    cmd.args(["tool", "dir"]);
    let out = cmd.output().ok()?;
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !dir.is_empty()).then(|| PathBuf::from(dir))
}

/// At least one *.pyc anywhere under `<tool dir>/fno/lib`.
fn has_shipped_pyc(td: &Path) -> bool {
    let mut stack = vec![td.join("fno").join("lib")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("pyc") {
                return true;
            }
        }
    }
    false
}

/// Acquire the machine-global update claim. Held means another session is
/// updating fno right now: the loser's update either already landed (the
/// marker reads this run's rev) or is under way, so it JOINS that run and
/// exits with its result. It never starts a second install.
fn acquire_update_claim(rev: Option<&str>) -> Result<(), i32> {
    let holder = format!("fno-update-pid{}", std::process::id());
    let args: Vec<String> = [
        "claim".into(),
        "acquire".into(),
        UPDATE_CLAIM_KEY.into(),
        "--holder".into(),
        holder,
        "--reason".into(),
        "machine-global install guard (tool env + cargo bins)".into(),
    ]
    .to_vec();
    match run_bounded(&fno_agents_bin(), &args, Duration::from_secs(60), None) {
        Ok((0, _, _)) => Ok(()),
        Ok((1, out, _err)) => {
            let held: Value = serde_json::from_str(out.trim()).unwrap_or(Value::Null);
            let peer = held
                .get("holder")
                .and_then(Value::as_str)
                .unwrap_or("another session");
            let marker = read_marker(&installed_rev_file());
            if let Some(rev) = rev {
                if marker.as_deref() == Some(rev) {
                    println!(
                        "fno doctor update: revision {rev} already landed by another session; nothing to do."
                    );
                    return Err(0);
                }
            }
            let Some(pid) = update_holder_pid(peer) else {
                println!(
                    "fno doctor update: another session is updating fno right now (claim held by {peer}); skipping. Re-run once it finishes."
                );
                return Err(0);
            };
            Err(join_running_update(pid))
        }
        _ => {
            eprintln!(
                "fno doctor update: the update:fno claim file is corrupt or the helper failed; remove the claim lock and re-run."
            );
            Err(1)
        }
    }
}

/// The pid in an update claim holder (`fno-update-pid<N>`), or None for any
/// other holder.
pub(crate) fn update_holder_pid(holder: &str) -> Option<u32> {
    holder.strip_prefix("fno-update-pid")?.parse().ok()
}

/// Wait for the update that holds the claim, then exit with its result. A
/// second update used to refuse, or before that, wait 52 minutes on cargo's
/// build-dir lock and never say for whom.
fn join_running_update(pid: u32) -> i32 {
    const JOIN_BOUND: Duration = Duration::from_secs(30 * 60);
    let deadline = Instant::now() + JOIN_BOUND;
    // kill(pid, 0) probes liveness without a signal; EPERM still means alive.
    let alive = || {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };
    // A dead holder ran nothing this joiner can wait on: the journal's last
    // row belongs to some earlier update, so it proves nothing about now.
    if !alive() {
        eprintln!(
            "fno doctor update: the update claim names pid {pid}, which is gone; the stale claim frees on its TTL. Re-run then."
        );
        return 1;
    }
    println!("fno doctor update: joining the running update (pid {pid}); waiting for it to finish");
    while alive() {
        if Instant::now() >= deadline {
            eprintln!(
                "fno doctor update: the update (pid {pid}) is still running after 30 minutes; stopped waiting"
            );
            return 1;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let outcome = last_update_event()
        .and_then(|e| e.get("type").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    if outcome == "fno_update_installed" {
        println!("fno doctor update: the update (pid {pid}) installed; nothing left to do.");
        0
    } else {
        eprintln!(
            "fno doctor update: the update (pid {pid}) ended without an install ({}); re-run to retry.",
            if outcome.is_empty() { "no journal row" } else { outcome.as_str() }
        );
        1
    }
}

/// Release the machine-global claim on exit; best-effort, the stale-reclaim
/// heals a missed release.
fn release_update_claim() {
    let holder = format!("fno-update-pid{}", std::process::id());
    let args: Vec<String> = [
        "claim".into(),
        "release".into(),
        UPDATE_CLAIM_KEY.into(),
        "--holder".into(),
        holder,
    ]
    .to_vec();
    if let Ok((code, _out, err)) =
        run_bounded(&fno_agents_bin(), &args, Duration::from_secs(60), None)
    {
        if code != 0 {
            eprintln!(
                "fno doctor update: claim release exited {code}: {}",
                err.trim()
            );
        }
    }
}

/// Run the verb for one argv tail (`doctor update ...` minus the leading
/// words, or the root `update` spelling). Returns the process exit code.
pub fn run(rest: &[std::ffi::OsString]) -> i32 {
    let started = Instant::now();
    let flags = match parse_args(rest) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("fno doctor update: {e}");
            return 2;
        }
    };
    if flags.check {
        if flags.dry_run || flags.rust || flags.force {
            eprintln!("--check cannot be combined with --dry-run, --rust, or --force");
            return 2;
        }
        return run_check(flags.source.as_deref());
    }
    if flags.rust && flags.no_rust {
        eprintln!("fno doctor update: --rust and --no-rust are mutually exclusive");
        return 2;
    }
    if target_in_progress() && !flags.force {
        eprintln!("{GUARD_MSG}");
        return 1;
    }
    let pin = match resolve_source_pin(flags.source.as_deref()) {
        Ok(pin) => pin,
        Err(cause) => {
            eprintln!("{SOURCE_PIN_UNAVAILABLE}");
            eprintln!("cause: {cause}");
            return 1;
        }
    };
    if pin.get("decision").and_then(Value::as_str) == Some("refuse") {
        eprintln!(
            "{}",
            pin.get("refusal")
                .and_then(Value::as_str)
                .unwrap_or("source checkout refused")
        );
        return 1;
    }
    if let Some(warning) = pin.get("warning").and_then(Value::as_str) {
        eprintln!("{warning}");
    }
    let Some(path_str) = pin.get("path").and_then(Value::as_str) else {
        eprintln!("{SOURCE_PIN_UNAVAILABLE}");
        return 1;
    };
    let resolved = PathBuf::from(path_str);
    println!("Reinstalling fno from {}", resolved.display());

    let mut failed: Vec<String> = Vec::new();
    let mut rev = source_rev(&resolved);

    if flags.dry_run {
        let _ = sync_source_checkout(&resolved, true);
        // The rust leg still prints its plan here (a dry run states
        // everything an update would do); it EXECUTES only below, under the
        // claim. A dry run writes NOTHING.
        if !flags.no_rust {
            refresh_rust_bins(&resolved, flags.rust, true, &mut failed);
        }
        if which_uv().is_some() {
            println!(
                "Would run: uv tool install --reinstall-package fno --refresh-package fno --compile-bytecode {}",
                resolved.display()
            );
        } else if which_pip().is_some() {
            println!(
                "Would run: python3 -m pip install --user --force-reinstall {}",
                resolved.display()
            );
        } else {
            eprintln!("Neither `uv` nor `pip` is available on PATH.");
            return 1;
        }
        return 0;
    }

    // Machine-global mutations start here (cargo bins, the uv/pip env), so
    // this is where the machine-scoped guard belongs.
    // Before replacing the tool env in place, name anything still running
    // from it (2026-10-02 gap audit, blockers 1 and 3): the repair is the
    // same move that replaced a live study's CLI unnoticed. Advisory - the
    // update proceeds, the naming is the fix.
    let live_env = live_tool_env_processes();
    if !live_env.is_empty() {
        eprintln!(
            "fno doctor update: {} live process(es) run from the tool env, e.g. {}. This update replaces that env in place; stop them first if the run matters.",
            live_env.len(),
            live_env[0]
        );
    }
    if let Err(code) = acquire_update_claim(rev.as_deref()) {
        return code;
    }
    // Under the claim: a second update must never move the tree that the
    // claim holder's cargo is building from.
    if let Err(refusal) = sync_source_checkout(&resolved, false) {
        release_update_claim();
        eprintln!("{refusal}");
        return 1;
    }
    rev = source_rev(&resolved);

    // The lifecycle journal opens here, matching the deleted Python leg:
    // started after the claim, built on the rust leg's own verdict,
    // installed or failed at the end. The install-build mark rides every
    // cargo child from here on, so the test-run doors admit the update's
    // own builds past the tests hold and the worker slots.
    let old_rev = read_marker(&installed_rev_file());
    let started_fields: Vec<(&str, String)> = {
        let mut f: Vec<(&str, String)> = Vec::new();
        if let Some(r) = &rev {
            f.push(("new_rev", r.clone()));
        }
        if let Some(o) = &old_rev {
            f.push(("old_rev", o.clone()));
        }
        f.push(("source_path", resolved.to_string_lossy().into_owned()));
        f
    };
    journal_call("started", &started_fields, None);
    std::env::set_var("FNO_INSTALL_BUILD", "1");

    if !flags.no_rust {
        refresh_rust_bins(&resolved, flags.rust, false, &mut failed);
        let built_fields: Vec<(&str, String)> = {
            let mut f: Vec<(&str, String)> = Vec::new();
            if let Some(r) = rust_subtree_rev(&resolved) {
                f.push(("rust_rev", r));
            }
            f.push((
                "outcome",
                if failed.is_empty() { "ok" } else { "failed" }.to_string(),
            ));
            f
        };
        journal_call("built", &built_fields, None);
    }

    let cmd: Vec<String> = if which_uv().is_some() {
        vec![
            "uv".into(),
            "tool".into(),
            "install".into(),
            "--reinstall-package".into(),
            "fno".into(),
            "--refresh-package".into(),
            "fno".into(),
            "--compile-bytecode".into(),
            resolved.to_string_lossy().into_owned(),
        ]
    } else if which_pip().is_some() {
        vec![
            "python3".into(),
            "-m".into(),
            "pip".into(),
            "install".into(),
            "--user".into(),
            "--force-reinstall".into(),
            resolved.to_string_lossy().into_owned(),
        ]
    } else {
        eprintln!("Neither `uv` nor `pip` is available on PATH.");
        release_update_claim();
        journal_call("failed", &[("rc", "1".to_string())], None);
        return 1;
    };

    let uv_ok = if cmd[0] == "uv" {
        uv_install(&cmd)
    } else {
        run_inherit(Path::new(&cmd[0]), &cmd[1..]) == 0
    };
    if uv_ok {
        // The marker records the rev we are about to have installed, so
        // `fno doctor` can detect installed-vs-source skew; written only on
        // a successful install.
        if let Some(rev) = &rev {
            if let Err(e) = write_marker(&installed_rev_file(), rev) {
                eprintln!("fno doctor update: WARNING: marker write failed: {e}");
            }
        }
        // Retire the stale `installed-rust-rev` marker: nothing has written
        // or read it for verdicts in releases (the verdict keys on the
        // binary's self-report), so every surviving copy reads stale
        // forever. Best-effort: its absence is the point, not its removal.
        let _ = std::fs::remove_file(retired_rust_marker_file());
        // A failed pin record is a failed step, never a warning.
        if let Err(e) = record_source_pin(&pin) {
            eprintln!("fno doctor update: ERROR: {e}");
            failed.push("source-pin record".into());
        }
        post_install_steps(&resolved, &mut failed);
    } else {
        failed.push("uv install".into());
    }

    release_update_claim();

    let verdict = daemon_verdict();

    // The journal records the install; the daemon verdict only sets the exit.
    let code = if failed.is_empty() {
        let installed_fields: Vec<(&str, String)> = {
            let mut f: Vec<(&str, String)> = Vec::new();
            if let Some(r) = &rev {
                f.push(("new_rev", r.clone()));
            }
            if let Some(o) = &old_rev {
                f.push(("old_rev", o.clone()));
            }
            f
        };
        let front_door = std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        journal_call("installed", &installed_fields, front_door.as_deref());
        0
    } else {
        let rc = "1";
        journal_call("failed", &[("rc", rc.to_string())], None);
        1
    };
    // An update finishes in 60 seconds (user order 2026-10-09). One that runs
    // longer still installs, but exits 1 and names its time, so a slow path
    // such as the compile fallback never passes as healthy.
    let took = started.elapsed().as_secs();
    if took > UPDATE_BUDGET_SECS {
        failed.push(format!("the {UPDATE_BUDGET_SECS}s budget (took {took}s)"));
    }
    // One summary line, always last: the daemon verdict, then any failed step.
    match &verdict {
        Ok(line) if failed.is_empty() => println!("fno update: done; {line}."),
        Err(line) if failed.is_empty() => eprintln!("fno update: FAILED: installed, but {line}."),
        Ok(line) | Err(line) => {
            eprintln!("fno update: FAILED step(s): {}; {line}.", failed.join(", "))
        }
    }
    if verdict.is_err() || !failed.is_empty() {
        1
    } else {
        code
    }
}

fn which_uv() -> Option<PathBuf> {
    on_path(if cfg!(windows) { "uv.exe" } else { "uv" })
}

fn which_pip() -> Option<PathBuf> {
    on_path(if cfg!(windows) { "pip.exe" } else { "pip" })
}

/// Live processes whose argv names a path inside the uv fno tool env. The
/// update replaces that env in place, so `run` names anything still running
/// from it before the claim: a session-start installer once replaced a live
/// study's CLI this way unnoticed (2026-10-02 gap audit, blockers 1 and 3).
/// Over-catching is safe: the line is advisory. Empty when uv, the env, or
/// the process table is unreadable.
fn live_tool_env_processes() -> Vec<String> {
    let Some(uv) = which_uv() else {
        return Vec::new();
    };
    let Ok((0, out, _)) = run_captured(&uv, &["tool".into(), "dir".into()], None) else {
        return Vec::new();
    };
    let dir = out.trim();
    if dir.is_empty() {
        // An unreadable tool dir would leave the needle bare "fno", which
        // matches every fno argv on the machine - refuse to scan instead.
        return Vec::new();
    }
    let needle = PathBuf::from(dir).join("fno").to_string_lossy().to_string();
    let ps = PathBuf::from("ps");
    let Ok((0, ps_out, _)) = run_captured(&ps, &["-axo".into(), "pid=,args=".into()], None) else {
        return Vec::new();
    };
    let me = format!("{} ", std::process::id());
    ps_out
        .lines()
        .map(str::trim)
        .filter(|l| l.contains(&needle))
        .filter(|l| !l.starts_with(&me) && !l.contains(" awk -") && !l.contains(" ps -"))
        .map(String::from)
        .collect()
}

/// `fno doctor update --check`: readiness as JSON on stdout, exit 0. The
/// single resolver: the TUI (client/update_menu.rs) renders this payload and
/// computes nothing itself. Every input degrades independently, so a broken
/// environment still gets an honest guidance line.
pub fn run_check(source: Option<&Path>) -> i32 {
    let payload = update_readiness(source);
    match serde_json::to_string(&payload) {
        Ok(s) => {
            println!("{s}");
            0
        }
        Err(_) => 1,
    }
}

/// `PROTO_VERSION` from the source checkout's crates/fno/src/proto.rs, one
/// read. None on any read/parse failure: the readiness resolver treats that
/// as a degraded input, never a bogus wire.
fn read_source_wire(source: &Path) -> Option<u32> {
    let proto = source
        .parent()?
        .join("crates")
        .join("fno")
        .join("src")
        .join("proto.rs");
    let text = std::fs::read_to_string(proto).ok()?;
    let line = text
        .lines()
        .find(|l| l.starts_with("pub const PROTO_VERSION: u32 = "))?;
    let digits: String = line
        .trim_start_matches("pub const PROTO_VERSION: u32 = ")
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Live (`state == "live"`) rows from `fno mux ls --json`, or None on any
/// failure, so the caller can tell "no live servers" from "could not ask".
fn live_mux_rows() -> Option<Vec<Value>> {
    let fno_bin = cargo_installed_mux().or_else(|| on_path("fno"))?;
    let args: Vec<String> = ["mux".into(), "ls".into(), "--json".into()].to_vec();
    let Ok((code, out, _err)) = run_bounded(&fno_bin, &args, Duration::from_secs(5), None) else {
        return None;
    };
    if code != 0 {
        return None;
    }
    let rows: Value = serde_json::from_str(out.trim()).ok()?;
    let arr = rows.as_array()?;
    Some(
        arr.iter()
            .filter(|r| r.get("state").and_then(Value::as_str) == Some("live"))
            .cloned()
            .collect(),
    )
}

/// Up to ten commit subjects between `installed_rev` and source HEAD.
fn changelog_subjects(installed_rev: &str, source: &Path) -> Vec<String> {
    git_in(
        source,
        &[
            "log",
            "--no-merges",
            "--format=%s",
            &format!("{installed_rev}..HEAD"),
        ],
    )
    .map(|out| {
        out.lines()
            .filter(|l| !l.trim().is_empty())
            .take(10)
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

/// Release notes for the update modal, built by the native leg through the
/// verb seam: one line per merged PR between `installed_rev` and source
/// HEAD. None on any failure; the payload never blocks on it.
fn release_notes_payload(installed_rev: &str, source: &Path) -> Option<Value> {
    let body = serde_json::json!({
        "installed_rev": installed_rev,
        "source": source.to_string_lossy(),
    });
    let args: Vec<String> = ["release-notes".into()].to_vec();
    let Ok((code, out, _err)) = run_bounded(
        &fno_agents_bin(),
        &args,
        Duration::from_secs(30),
        Some(&body.to_string()),
    ) else {
        return None;
    };
    if code != 0 {
        return None;
    }
    let v: Value = serde_json::from_str(out.trim()).ok()?;
    v.get("notes").filter(|n| !n.is_null()).cloned()
}

/// One row per long-lived process from `fno-agents census --json`; None when
/// the census itself could not run: a dark census is not an empty machine.
fn running_components() -> Option<Vec<Value>> {
    let args: Vec<String> = ["census".into(), "--json".into()].to_vec();
    let Ok((code, out, _err)) =
        run_bounded(&fno_agents_bin(), &args, Duration::from_secs(30), None)
    else {
        return None;
    };
    if code != 0 {
        return None;
    }
    let rows: Value = serde_json::from_str(out.trim()).ok()?;
    let arr = rows.as_array()?;
    Some(arr.iter().filter(|r| r.is_object()).cloned().collect())
}

fn wire_label(wires: &[u32]) -> String {
    if wires.is_empty() {
        "unknown".to_string()
    } else {
        wires
            .iter()
            .map(|w| format!("v{w}"))
            .collect::<Vec<_>>()
            .join("/")
    }
}

fn current_but_stale(
    rev_label: &str,
    stale: usize,
    restartable: usize,
    pane_kept: usize,
) -> String {
    format!(
        "installed {rev_label} is current; {stale} running process(es) are older builds - restart cycles {restartable}, keeps {pane_kept} pane keeper(s) on the old build until their panes end",
    )
}

/// The one guidance line, computed rather than authored. Three branches - no
/// bump, bump, degraded - and no fourth. Every branch names a count and a
/// positive outcome; the degraded branch treats an unknown wire as a bump
/// and, when the shell count itself could not be read (a failed mux ls, not
/// just an unreadable wire), says "unknown" rather than a false zero: a
/// count fno never fetched is not evidence of an empty fleet. A degraded
/// input unrelated to the wire must not override a confidently known
/// not-ready state.
#[allow(clippy::too_many_arguments)]
fn build_guidance(
    update_ready: bool,
    revs_known: bool,
    source_rev: Option<&str>,
    wire_known: bool,
    wire_bump: bool,
    running_wires: &[u32],
    source_wire: Option<u32>,
    shells: u64,
    shells_ended: u64,
    shells_known: bool,
    degraded_reason: Option<&str>,
    stale_rows: &[Value],
) -> String {
    let rev_label = source_rev.unwrap_or("unknown");
    let rev_label = &rev_label[..rev_label.len().min(8)];
    let source_label = source_wire
        .map(|w| format!("v{w}"))
        .unwrap_or("unknown".into());
    let running_stale = stale_rows.len();
    let restartable = stale_rows
        .iter()
        .filter(|r| {
            matches!(
                r.get("on_restart").and_then(Value::as_str),
                Some(s) if s.starts_with("restarts") || s.starts_with("cycles")
            )
        })
        .count();
    let pane_kept = stale_rows
        .iter()
        .filter(|r| {
            matches!(
                r.get("component").and_then(Value::as_str),
                Some("pane-keeper") | Some("thread-keeper")
            )
        })
        .count();
    // A degraded input never overrides a confidently known not-ready state:
    // if both revs were read and match, there is no update to warn about.
    if !update_ready && (revs_known || degraded_reason.is_none()) {
        if running_stale > 0 {
            return current_but_stale(rev_label, running_stale, restartable, pane_kept);
        }
        return format!(
            "up to date at {rev_label} - no update pending, {shells} shell(s) unaffected"
        );
    }
    if let Some(reason) = degraded_reason {
        let shells_label = if shells_known {
            format!("{shells} live shell(s)")
        } else {
            "an unknown number of live shells".to_string()
        };
        let wire_label = if wire_known {
            if wire_bump {
                format!("WIRE BUMP {} -> {source_label}", wire_label(running_wires))
            } else {
                "wire unchanged".to_string()
            }
        } else {
            "wire status unknown, treated as a wire bump".to_string()
        };
        return format!("update check degraded ({reason}) - {wire_label}; {shells_label} at risk");
    }
    if wire_bump {
        return format!(
            "update ready {rev_label} - WIRE BUMP {} -> {source_label} - `fno doctor update && fno agents restart --mux` ends {shells_ended} shell(s)",
            wire_label(running_wires)
        );
    }
    format!(
        "update ready {rev_label} - wire unchanged ({source_label}) - detach, `fno doctor update`, reattach; {shells} shell(s) survive"
    )
}

/// Compute the readiness payload: whether an install is waiting, whether it
/// would break the mux wire, and the one guidance line an operator sees.
pub(crate) fn update_readiness(source: Option<&Path>) -> Value {
    let mut degraded: Vec<String> = Vec::new();
    let installed_rev = read_marker(&installed_rev_file());
    if installed_rev.is_none() {
        degraded.push("installed rev marker missing".into());
    }
    let resolved_source = discover_source(source).ok();
    if resolved_source.is_none() {
        degraded.push("source checkout not resolvable".into());
    }
    let pin = match resolve_source_pin(source) {
        Ok(pin) => Some(pin),
        Err(cause) => {
            degraded.push(format!("source pin unresolved ({cause})"));
            None
        }
    };
    let gate_refused = pin.as_ref().is_some_and(|p| {
        p.get("path").and_then(Value::as_str).is_some()
            && p.get("decision").and_then(Value::as_str) == Some("refuse")
    });
    let mut src_rev: Option<String> = None;
    if let Some(src) = &resolved_source {
        src_rev = source_rev(src);
        if src_rev.is_none() {
            degraded.push("source rev unreadable".into());
        }
    }
    let update_ready = if gate_refused {
        false
    } else {
        installed_rev.is_some() && src_rev.is_some() && installed_rev != src_rev
    };
    let source_wire = resolved_source.as_ref().and_then(|s| read_source_wire(s));
    if resolved_source.is_some() && source_wire.is_none() {
        degraded.push("source PROTO_VERSION unreadable".into());
    }
    let live = live_mux_rows();
    let shells_known = live.is_some();
    if live.is_none() {
        degraded.push("fno mux ls --json failed".into());
    }
    let live_rows: Vec<Value> = live.unwrap_or_default();
    // Attachability is decided by the SERVER's gate, not the source consts:
    // a pre-floor generation refuses a wire inside [floor, source_wire). The
    // binary's own stale verdict knows that gate; consume it. A live server
    // NEWER than the source is a bump in its own right: installing the
    // source is a downgrade.
    let wire_bump = match source_wire {
        None => true,
        Some(sw) => live_rows
            .iter()
            .any(|r| match r.get("wire_version").and_then(Value::as_i64) {
                None => true,
                Some(w) => match r.get("stale").and_then(Value::as_bool) {
                    Some(s) => s || w > sw as i64,
                    None => w != sw as i64,
                },
            }),
    };
    let shells: u64 = live_rows
        .iter()
        .filter_map(|r| r.get("panes").and_then(Value::as_i64))
        .map(|p| p.max(0) as u64)
        .sum();
    let sessions = live_rows.len() as u64;
    let mut running_wires: Vec<u32> = live_rows
        .iter()
        .filter_map(|r| r.get("wire_version").and_then(Value::as_i64))
        .filter_map(|w| u32::try_from(w).ok())
        .collect();
    running_wires.sort_unstable();
    running_wires.dedup();
    let shells_ended: u64 = if wire_bump { shells } else { 0 };
    let mut changelog: Vec<String> = Vec::new();
    let mut release_notes: Option<Value> = None;
    if let (Some(installed), Some(src), Some(rev)) = (
        installed_rev.as_deref(),
        resolved_source.as_ref(),
        src_rev.as_deref(),
    ) {
        changelog = changelog_subjects(installed, src);
        release_notes = release_notes_payload(installed, src);
        let _ = rev;
    }
    let census = running_components();
    if census.is_none() {
        degraded.push("running-process census unavailable".into());
    }
    let census_rows: Vec<Value> = census.unwrap_or_default();
    // Census rows; never the name `running`: python_tool owns it.
    let running_rows: Vec<Value> = census_rows
        .iter()
        .filter(|r| r.get("verdict").and_then(Value::as_str) == Some("stale"))
        .cloned()
        .collect();
    let degraded_reason = if degraded.is_empty() {
        None
    } else {
        Some(degraded.join("; "))
    };
    let guidance = match &pin {
        Some(p)
            if gate_refused
                || (p.get("guidance").and_then(Value::as_str).is_some() && !update_ready) =>
        {
            p.get("guidance")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "update blocked: {}",
                        p.get("refusal")
                            .and_then(Value::as_str)
                            .unwrap_or("the resolved source failed the source-pin gate")
                    )
                })
        }
        _ if installed_rev.is_none() && resolved_source.is_none() => {
            // Release install (no source, no marker): name the one command
            // that upgrades it instead of inventing a wire bump.
            "no source checkout and no installed-rev marker; a release install refreshes with `uv tool upgrade fno`".to_string()
        }
        _ => build_guidance(
            update_ready,
            installed_rev.is_some() && src_rev.is_some(),
            src_rev.as_deref(),
            shells_known && source_wire.is_some(),
            wire_bump,
            &running_wires,
            source_wire,
            shells,
            shells_ended,
            shells_known,
            degraded_reason.as_deref(),
            &running_rows,
        ),
    };
    // Name both deployments: the front door's script and this binary.
    let front_script = crate::bootstrap::resolved_python_script();
    let running = std::env::current_exe().ok();
    let same = match (&front_script, &running) {
        (Some(s), Some(r)) => match (s.parent(), r.parent()) {
            (Some(sp), Some(rp)) => sp == rp,
            _ => false,
        },
        _ => false,
    };
    let script_str = front_script.map(|p| p.to_string_lossy().into_owned());
    let running_str = running.map(|p| p.to_string_lossy().into_owned());
    let subtree = resolved_source.as_ref().and_then(|s| rust_subtree_rev(s));
    let components = match (&resolved_source, &subtree, cargo_installed_bin()) {
        (Some(src), Some(st), Some(bin)) => component_verdict(
            src,
            st,
            bin.parent().unwrap_or(&bin),
            &bin,
            false,
            Some(true),
            None,
        )
        .unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let probes = serde_json::json!({
        "installed_rev": installed_rev,
        "rust_subtree_rev": subtree,
        "source_rev": src_rev,
        "source": resolved_source.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "cargo_bin": cargo_installed_bin().is_some(),
        "cargo_bin_path": cargo_installed_bin().map(|p| p.to_string_lossy().into_owned()),
        "cargo_mux": cargo_installed_mux().is_some(),
        "cargo_mux_path": cargo_installed_mux().map(|p| p.to_string_lossy().into_owned()),
        "components": components,
        "running_components": if census_rows.is_empty() { Value::Null } else { Value::Array(census_rows.clone()) },
        "mux_server_stale": stale_mux_servers(),
    });
    serde_json::json!({
        "update_ready": update_ready,
        "source_pin": pin,
        "last_update_event": last_update_event(),
        "installed_rev": installed_rev,
        "source_rev": src_rev,
        "python_tool": {
            "script": script_str,
            "running": running_str,
            "same": same,
        },
        "wire": {
            "running": running_wires,
            "source": source_wire,
            "bump": wire_bump,
        },
        "shells": if shells_known { serde_json::json!(shells) } else { Value::Null },
        "shells_ended": if shells_known { serde_json::json!(shells_ended) } else { Value::Null },
        "sessions": if shells_known { serde_json::json!(sessions) } else { Value::Null },
        "changelog": changelog,
        "release_notes": release_notes,
        "guidance": guidance,
        "degraded": degraded_reason,
        "running": running_rows,
        "running_stale": running_rows.len(),
        "probes": probes,
    })
}
