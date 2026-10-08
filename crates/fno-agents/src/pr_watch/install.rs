//! The `pr-watch install` verb: render the global LaunchAgent plist, gate on
//! human confirmation, write it, and bounce the agent - plus the
//! non-interactive `ensure_activated` coupling the `pr_watch.enabled` config
//! write fires. Port of the Python `install` / `ensure_activated` /
//! `retire_legacy_postmerge_agents` legs in `cli/src/fno/pr_watch/_install.py`
//! (port protocol, docs/architecture/dual-implementation-inventory.md); the
//! parity test `tests/pr_watch_install_parity.rs` freezes the rendered plist
//! bytes, the PATH augmentation, the confirm gate and the legacy-agent
//! retirement against goldens captured from the Python leg.
//!
//! macOS-only by design: install drives `launchctl`, exactly as the Python
//! leg did, and keeps its refusal behavior off other hosts (launchctl absent
//! reads as a failed activation, never a crash).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::status::{
    as_bool, cfg_lookup, launch_agents_dir, launchctl_is_loaded, state_root, PLIST_FILENAME,
};

const LABEL: &str = "sh.fno.pr-watcher";

/// The plist template, byte-for-byte the Python `_PLIST_TEMPLATE`.
const PLIST_TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!--
  Global PR-state watcher LaunchAgent.  ONE agent polls ~/.fno/graph.json
  for open-PR backlog nodes and fires /fno:ship pr check or /fno:ship pr merged.
  RunAtLoad is false: review the rendered plist and run
    launchctl load {plist_path}
  yourself (human gate).
-->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>

  <key>ProgramArguments</key>
  <array>
    <string>{fno_binary}</string>
    <string>do</string>
    <string>pr</string>
    <string>watch</string>
    <string>tick</string>
  </array>

  <!-- launchd launches with a minimal PATH.  Capture install-time PATH so
       gh / claude / uv are resolvable without a login shell. -->
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
    <key>HOME</key>
    <string>{home}</string>
  </dict>

  <!-- Poll every N seconds.  Default 600 (10 min). -->
  <key>StartInterval</key>
  <integer>{interval}</integer>

  <!-- Do NOT fire on agent load; wait for the first StartInterval. -->
  <key>RunAtLoad</key>
  <false/>

  <key>ProcessType</key>
  <string>Standard</string>

  <!-- Belt-and-suspenders: set cwd to $HOME so any code that constructs a
       relative path at least lands somewhere writable rather than in /.
       The primary fix is that _emit_event now anchors to state_dir()
       explicitly, but WorkingDirectory is a cheap additional safety net. -->
  <key>WorkingDirectory</key>
  <string>{home}</string>

  <key>StandardOutPath</key>
  <string>{log_out}</string>

  <key>StandardErrorPath</key>
  <string>{log_err}</string>
</dict>
</plist>
"#;

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The fixed PATH an agent runs with: the fno binary's own dir when it carries
/// one, then the cargo bin dir, then the standard install roots. The caller's
/// env never leaks in - a launchd PATH without the cargo bin failed every tick
/// at binary lookup.
pub(crate) fn default_agent_path(fno_binary: &str) -> String {
    let home = home_dir();
    let mut entries: Vec<String> = Vec::new();
    if fno_binary.contains('/') {
        if let Some(parent) = Path::new(fno_binary).parent() {
            entries.push(parent.display().to_string());
        }
    }
    // An empty CARGO_HOME reads as unset, the way the Python `or` default did.
    let cargo_home = std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"));
    let candidates = [
        cargo_home.join("bin").display().to_string(),
        home.join(".local").join("bin").display().to_string(),
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
    ];
    for candidate in candidates {
        if !entries.contains(&candidate) {
            entries.push(candidate);
        }
    }
    entries.join(":")
}

/// Ensure ~/.local/bin and /opt/homebrew/bin are in PATH.
fn augment_path(install_path: &str) -> String {
    let home = home_dir();
    let mut entries: Vec<String> = install_path
        .split(':')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    let extras = [
        home.join(".local").join("bin").display().to_string(),
        "/opt/homebrew/bin".to_string(),
    ];
    for extra in extras {
        if !entries.contains(&extra) {
            entries.push(extra);
        }
    }
    entries.join(":")
}

/// Write only when the bytes differ. Returns whether the file changed.
/// Unused until the refresh wave lands: its only Rust caller is the refresh
/// verb, so this sits quiet rather than moving twice.
#[allow(dead_code)]
pub(crate) fn write_if_changed(plist_path: &Path, plist_text: &str) -> std::io::Result<bool> {
    if plist_path.is_file() && std::fs::read_to_string(plist_path)? == plist_text {
        return Ok(false);
    }
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(plist_path, plist_text)?;
    Ok(true)
}

/// Render the plist XML string. No filesystem writes.
pub(crate) fn render_plist(
    launch_agents_dir: &Path,
    fno_binary: &str,
    install_path: Option<&str>,
    interval: i64,
) -> String {
    let home = home_dir();
    let fno_state = home.join(".fno");
    let log_out = fno_state.join("logs").join("pr-watcher.out.log");
    let log_err = fno_state.join("logs").join("pr-watcher.err.log");

    let base = install_path
        .map(str::to_string)
        .unwrap_or_else(|| default_agent_path(fno_binary));
    let augmented_path = augment_path(&base);

    PLIST_TEMPLATE
        .replace("{label}", &xml_escape(LABEL))
        .replace("{fno_binary}", &xml_escape(fno_binary))
        .replace("{path}", &xml_escape(&augmented_path))
        .replace("{home}", &xml_escape(&home.display().to_string()))
        .replace("{interval}", &interval.to_string())
        .replace("{log_out}", &xml_escape(&log_out.display().to_string()))
        .replace("{log_err}", &xml_escape(&log_err.display().to_string()))
        .replace(
            "{plist_path}",
            &xml_escape(&launch_agents_dir.join(PLIST_FILENAME).display().to_string()),
        )
}

/// Run launchctl; return exit code. Best-effort: never panics.
fn run_launchctl(args: &[&str]) -> i32 {
    match Command::new("launchctl").args(args).output() {
        Ok(out) => out.status.code().unwrap_or(-1),
        Err(_) => -1,
    }
}

/// A wedged job's `launchctl kickstart` was observed to HANG indefinitely; every
/// launchctl call in the bounce is timeout-guarded so a hung fix command can't
/// be worse than no fix. 10s is generous for a local launchctl round-trip.
const LAUNCHCTL_TIMEOUT_S: u64 = 10;

/// Run launchctl with a timeout. Returns `(returncode, timed_out)`.
fn run_launchctl_timed(args: &[&str], timeout_s: u64) -> (i32, bool) {
    let mut child = match Command::new("launchctl")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        // An OSError (missing binary) reads as a normal nonzero rc.
        Err(_) => return (-1, false),
    };
    let start = Instant::now();
    let deadline = Duration::from_secs(timeout_s);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code().unwrap_or(-1), false),
            Ok(None) => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (-1, true);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return (-1, false),
        }
    }
}

/// stdout of `argv`, or "" when the command is missing, hangs, or fails.
/// An unread answer never blocks a cure (fail-open).
fn stdout_of(argv: &[&str]) -> String {
    let Ok(output) = Command::new(argv[0]).args(&argv[1..]).output() else {
        return String::new();
    };
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn current_uid() -> u32 {
    // SAFETY: getuid is async-signal-safe and cannot fail.
    unsafe { libc::getuid() }
}

/// Name this bounce so the next killed tick can name its sender: the
/// `pr-watch-bounce.json` sidecar in the state dir plus a `pr_watch_bounce`
/// event. Never blocks a cure: any failure is swallowed.
fn record_bounce(caller: &str, state: &Path) {
    // SAFETY: the pid getters are async-signal-safe and cannot fail.
    let ppid = unsafe { libc::getppid() };
    let parent_raw = stdout_of(&["ps", "-o", "command=", "-p", &ppid.to_string()]);
    let parent: String = parent_raw.trim().chars().take(160).collect();
    let pid = unsafe { libc::getpid() };
    let data = serde_json::json!({
        "caller": caller,
        "pid": pid,
        "ppid": ppid,
        "parent": parent,
        "deferred": false,
    });
    // Sidecar: tmp + rename so a reader never sees a half line. The ts rides
    // first the way the Python dict inserted it.
    let sidecar = state.join("pr-watch-bounce.json");
    if let Ok(line) = serde_json::to_string(&data) {
        let with_ts = format!("{{\"ts\": {}, {}", epoch_f64(), &line[1..]);
        let tmp = state.join(format!(".pr-watch-bounce.json.tmp-{pid}"));
        if std::fs::write(&tmp, with_ts).is_ok() {
            let _ = std::fs::rename(&tmp, &sidecar);
        }
    }
    // Event: countable deferrals and joins for the tick's kill handler.
    let envelope = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        "type": "pr_watch_bounce",
        "source": "daemon",
        "data": data,
    });
    if let Ok(line) = serde_json::to_string(&envelope) {
        let _ = crate::event_store::append_envelope(&state.join("events.jsonl"), &line, None);
    }
}

fn epoch_f64() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:.6}", now.as_secs_f64())
}

/// bootout -> bootstrap -> kickstart to cure a wedged launchd job; the
/// install-time activation path of the Python `bounce`. Idempotent: safe on a
/// healthy job (restart) and on a not-loaded one (bootout failure tolerated).
/// Every call is timeout-guarded; on a hang it reports the wedged step and a
/// nonzero exit code. Returns `(message, exit_code)`.
pub(crate) fn bounce(plist_path: &Path, caller: &str, state: &Path) -> (String, i32) {
    let uid = current_uid();
    let domain = format!("gui/{uid}");
    let target = format!("{domain}/{LABEL}");

    // Receipt before bootout: only this sidecar joins the SIGTERM to its sender.
    record_bounce(caller, state);

    // 1. bootout: a nonzero rc is EXPECTED when the job is not loaded, so only
    //    a hang is fatal here.
    let (_, timed) = run_launchctl_timed(&["bootout", &target], LAUNCHCTL_TIMEOUT_S);
    if timed {
        return (
            format!("`launchctl bootout {target}` timed out after {LAUNCHCTL_TIMEOUT_S}s"),
            1,
        );
    }

    // 2. bootstrap: bootout is asynchronous, so an immediate bootstrap can lose
    //    to the still-settling label (rc=5). Retry with a short backoff.
    const BOOTSTRAP_RETRIES: u32 = 4;
    let mut rc = -1;
    for attempt in 0..BOOTSTRAP_RETRIES {
        let (code, timed) = run_launchctl_timed(
            &["bootstrap", &domain, &plist_path.display().to_string()],
            LAUNCHCTL_TIMEOUT_S,
        );
        if timed {
            return (
                format!("`launchctl bootstrap {domain}` timed out after {LAUNCHCTL_TIMEOUT_S}s"),
                1,
            );
        }
        rc = code;
        if rc == 0 {
            break;
        }
        if attempt + 1 < BOOTSTRAP_RETRIES {
            std::thread::sleep(Duration::from_millis(250 * (attempt as u64 + 1)));
        }
    }
    if rc != 0 {
        return (
            format!(
                "`launchctl bootstrap {domain} {}` failed (rc={rc})",
                plist_path.display()
            ),
            1,
        );
    }

    // 3. kickstart -k restarts if running; forces the first run so a fresh tick
    //    confirms liveness rather than waiting a full StartInterval.
    let (code, timed) = run_launchctl_timed(&["kickstart", "-k", &target], LAUNCHCTL_TIMEOUT_S);
    if timed {
        return (
            format!("`launchctl kickstart -k {target}` timed out after {LAUNCHCTL_TIMEOUT_S}s"),
            1,
        );
    }
    if code != 0 {
        return (
            format!("`launchctl kickstart -k {target}` failed (rc={code})"),
            1,
        );
    }

    (format!("bounced {target}; awaiting first tick"), 0)
}

/// Bootout + remove retired per-repo post-merge watcher plists. Best-effort
/// and idempotent, never fails the caller: called on every global-watcher
/// install/activate so an operator who once loaded the per-repo watcher does
/// not keep a launchd job firing a deleted script. Returns receipt lines.
pub(crate) fn retire_legacy_postmerge_agents(launch_agents_dir: &Path) -> Vec<String> {
    let mut receipts = Vec::new();
    let mut legacy: Vec<PathBuf> = match std::fs::read_dir(launch_agents_dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| glob_match_postmerge(n))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => return receipts,
    };
    legacy.sort();
    for plist in legacy {
        let Some(label) = plist.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let target = format!("gui/{}/{}", current_uid(), label);
        let _ = run_launchctl_timed(&["bootout", &target], 10);
        match std::fs::remove_file(&plist) {
            Ok(()) => receipts.push(format!("retired legacy post-merge watcher: {label}")),
            Err(exc) => receipts.push(format!(
                "could not remove legacy watcher plist {}: {exc}",
                plist.display()
            )),
        }
    }
    receipts
}

/// `com.fno.postmerge*.plist` without a glob crate: the prefix, the suffix,
/// and `*` matching zero or more characters, the way Python's glob did.
fn glob_match_postmerge(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".plist") else {
        return false;
    };
    stem.strip_prefix("com.fno.postmerge").is_some()
}

/// The `Heal:` readout line the install leaf prints last: unarmed answers
/// statically (the arm command is the whole answer), armed reads the healer's
/// own renderer in-process - the one readout, no second spelling.
fn heal_status_line(cwd: &Path) -> String {
    let armed = as_bool(cfg_lookup(cwd, &["auto_heal", "enabled"]), false);
    if !armed {
        return "Heal: unarmed (auto_heal.enabled=false; arm with: fno config set auto_heal.enabled true)".to_string();
    }
    let events = super::status::state_root(cwd).join("events.jsonl");
    crate::heal::status_readout(true, &events)
}

/// The interactive confirm gate, click-shaped: `{text} [y/N]: ` on stdout,
/// y/yes proceeds, n/no refuses, anything else asks again, EOF aborts.
fn confirm(text: &str) -> Option<bool> {
    loop {
        print!("{text} [y/N]: ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).unwrap_or(0) == 0 {
            return None;
        }
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Some(true),
            "n" | "no" => return Some(false),
            "" => return Some(false),
            _ => continue,
        }
    }
}

pub fn run(args: &[String]) -> i32 {
    let mut dry_run = false;
    let mut no_activate = false;
    let mut ensure = false;
    let mut interval: i64 = 0;
    let mut fno_binary = String::from("fno-py");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut take_value = |flag: &str| match iter.next() {
            Some(v) => Some(v.clone()),
            None => {
                eprintln!("fno-agents pr-watch install: {flag} needs a value");
                None
            }
        };
        match arg.as_str() {
            "-N" | "--dry-run" => dry_run = true,
            "--no-activate" => no_activate = true,
            "--ensure" => ensure = true,
            "--interval" => match take_value("--interval") {
                Some(v) => match v.parse::<i64>() {
                    Ok(n) => interval = n,
                    Err(_) => {
                        eprintln!(
                            "fno-agents pr-watch install: --interval wants an integer, got {v}"
                        );
                        return 2;
                    }
                },
                None => return 2,
            },
            other => {
                if let Some(v) = other.strip_prefix("--interval=") {
                    match v.parse::<i64>() {
                        Ok(n) => interval = n,
                        Err(_) => {
                            eprintln!(
                                "fno-agents pr-watch install: --interval wants an integer, got {v}"
                            );
                            return 2;
                        }
                    }
                } else if let Some(v) = other.strip_prefix("--model=") {
                    let _ = v; // accepted for spelling parity; the leaf owns it
                } else if other == "--model" {
                    if take_value("--model").is_none() {
                        return 2;
                    }
                } else if let Some(v) = other.strip_prefix("--fno-binary=") {
                    fno_binary = v.to_string();
                } else if other == "--fno-binary" {
                    match take_value("--fno-binary") {
                        Some(v) => fno_binary = v,
                        None => return 2,
                    }
                } else {
                    eprintln!("fno-agents pr-watch install: unknown argument {other}");
                    return 2;
                }
            }
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if ensure {
        let word = ensure_activated(&launch_agents_dir(), &fno_binary, interval, &cwd);
        println!("{word}");
        return 0;
    }
    install_verb(
        &launch_agents_dir(),
        &fno_binary,
        interval,
        dry_run,
        !no_activate,
        &cwd,
    )
}

/// The poll interval: the explicit flag wins, else the config value the old
/// Python leaf loaded, else 600.
fn cfg_interval(cwd: &Path, interval: i64) -> i64 {
    if interval > 0 {
        return interval;
    }
    match cfg_lookup(cwd, &["pr_watch", "interval_seconds"]) {
        Some(toml::Value::Integer(i)) if i > 0 => i,
        _ => 600,
    }
}

/// The interactive install flow: render, print, gate, write, bounce, retire
/// the legacy agents. Exit 1 on a declined gate, 0 otherwise.
fn install_verb(
    launch_agents_dir: &Path,
    fno_binary: &str,
    interval: i64,
    dry_run: bool,
    activate: bool,
    cwd: &Path,
) -> i32 {
    let interval = cfg_interval(cwd, interval);
    let plist_text = render_plist(launch_agents_dir, fno_binary, None, interval);
    let plist_path = launch_agents_dir.join(PLIST_FILENAME);

    println!("--- Rendered plist ---");
    println!("{plist_text}");

    if dry_run {
        println!("[dry-run] Would write to: {}", plist_path.display());
        println!(
            "[dry-run] Then run: launchctl load {}",
            plist_path.display()
        );
        println!("[dry-run] Nothing written.");
        return 0;
    }

    match confirm(&format!("Write plist to {}?", plist_path.display())) {
        Some(true) => {}
        Some(false) => {
            println!("Not installed.");
            return 1;
        }
        None => {
            // EOF on the gate: click aborts the run.
            eprintln!("Aborted.");
            return 1;
        }
    }

    if let Err(exc) = std::fs::create_dir_all(launch_agents_dir)
        .and_then(|()| std::fs::write(&plist_path, &plist_text))
    {
        eprintln!(
            "ERROR: could not write plist {}: {exc}",
            plist_path.display()
        );
        return 1;
    }
    println!("Written: {}", plist_path.display());

    if activate {
        // bootout+bootstrap+kickstart, not load/unload: `launchctl load` cannot
        // cure the observed wedge, and this is the `dead`-verdict fix command.
        // The bounce is idempotent, so a RE-install of a healthy agent just
        // restarts it.
        let state = state_root(cwd);
        let (msg, rc) = bounce(&plist_path, "install", &state);
        if rc == 0 {
            println!("Activated: {msg}");
        } else {
            // Loud, never silent: SIP/headless contexts can refuse launchctl.
            // The plist is written; doctor's liveness line is the residual guard.
            println!(
                "WARNING: activation failed ({msg}); load it manually: \
                 launchctl bootstrap gui/$(id -u) {}",
                plist_path.display()
            );
        }
    } else {
        println!(
            "To activate: launchctl bootstrap gui/$(id -u) {}",
            plist_path.display()
        );
    }

    for receipt in retire_legacy_postmerge_agents(launch_agents_dir) {
        println!("{receipt}");
    }
    println!(
        "The canonical pr-watch daemon now owns merge detection; no per-repo watcher install is needed."
    );
    println!("{}", heal_status_line(cwd));
    0
}

/// Idempotently install + load the watcher. Non-interactive, never fails the
/// caller: returns one of `already-running`, `activated`, `write-failed`,
/// `load-failed`. A failure is reported by the caller and leaves config
/// enabled so `fno doctor` flags the dead watcher.
pub(crate) fn ensure_activated(
    launch_agents_dir: &Path,
    fno_binary: &str,
    interval: i64,
    cwd: &Path,
) -> String {
    let interval = cfg_interval(cwd, interval);
    let plist_path = launch_agents_dir.join(PLIST_FILENAME);
    let _ = retire_legacy_postmerge_agents(launch_agents_dir);

    if launchctl_is_loaded() {
        return "already-running".to_string();
    }

    // Always (re-)render, whether the plist is absent or a re-enable of an
    // existing one: this rewrite picks up config drift and refreshes the plist
    // mtime so doctor's healthy-pending grace applies until the first fresh
    // tick instead of a transient false "dead".
    let plist_text = render_plist(launch_agents_dir, fno_binary, None, interval);
    if std::fs::create_dir_all(launch_agents_dir).is_err() {
        return "write-failed".to_string();
    }
    let state = state_root(cwd);
    if std::fs::create_dir_all(state.join("logs")).is_err() {
        return "write-failed".to_string();
    }
    if std::fs::write(&plist_path, plist_text).is_err() {
        return "write-failed".to_string();
    }

    let rc = run_launchctl(&["load", &plist_path.display().to_string()]);
    if rc == 0 {
        "activated".to_string()
    } else {
        "load-failed".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_bounce_writes_the_sidecar_into_the_state_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        record_bounce("install", &state);
        let raw = std::fs::read_to_string(state.join("pr-watch-bounce.json")).unwrap();
        assert!(raw.contains("\"caller\":\"install\""), "{raw}");
        assert!(raw.contains("\"deferred\":false"), "{raw}");
        assert!(raw.starts_with("{\"ts\":"), "{raw}");
    }

    #[test]
    fn glob_matches_only_the_legacy_postmerge_family() {
        assert!(glob_match_postmerge("com.fno.postmerge-x.plist"));
        assert!(glob_match_postmerge("com.fno.postmerge.plist"));
        assert!(!glob_match_postmerge("com.fno.postmerge.plist.bak"));
        assert!(!glob_match_postmerge("sh.fno.pr-watcher.plist"));
        assert!(!glob_match_postmerge("com.fno.postmerge"));
    }

    #[test]
    fn render_plist_augments_the_path_in_fixed_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let agents = tmp.path().join("LaunchAgents");
        let binary = tmp.path().join("bin").join("fno-py");
        let text = render_plist(&agents, &binary.display().to_string(), None, 1234);
        // The interval lands in StartInterval and the agent runs the tick.
        assert!(text.contains("<integer>1234</integer>"), "{text}");
        assert!(
            text.contains(&format!(
                "<string>{}</string>\n    <string>do</string>",
                xml_escape(&binary.display().to_string())
            )),
            "{text}"
        );
        // The PATH line carries the cargo bin beside the binary's own dir.
        let cargo_bin = std::env::var_os("CARGO_HOME")
            .filter(|v| !v.is_empty())
            .map(|c| PathBuf::from(c).join("bin"))
            .unwrap_or_else(|| home_dir().join(".cargo").join("bin"));
        let path_line = text
            .lines()
            .find(|l| l.contains(&cargo_bin.display().to_string()))
            .expect("cargo bin in PATH");
        assert!(path_line.contains("<string>"), "{path_line}");
    }
}
