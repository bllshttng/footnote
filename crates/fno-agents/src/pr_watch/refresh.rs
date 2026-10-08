//! The `refresh` verb: re-render the watcher's plist onto the current binary
//! and bounce the LaunchAgent. Ported from `cli/src/fno/pr_watch/_install.py`
//! (`refresh_watcher`, `bounce`, `_record_bounce`, `_tick_in_flight`); the
//! plist renderer rides along because refresh is the first verb that rewrites
//! the file. The Python `bounce` survives beside this port only for callers a
//! later wave moves (`groom.py`, the `heal` leaf) and dies with the package
//! in wave 6; from here on this module is the native leg.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::pr_watch::status::{LABEL, PLIST_FILENAME};

const BOUNCE_SIDECAR: &str = "pr-watch-bounce.json";
// A wedged job's `launchctl kickstart` was observed to HANG indefinitely; every
// launchctl call in the bounce is timeout-guarded so a hung fix command can't be
// worse than no fix. 10s is generous for a local launchctl round-trip.
const LAUNCHCTL_TIMEOUT_S: u64 = 10;
// `launchctl bootout` is asynchronous: it returns before launchd finishes
// removing the service from the domain, so an immediate bootstrap can race the
// still-present label and fail (rc=5). Retry the bootstrap a few times with a
// short backoff to survive that settle window.
const BOOTSTRAP_RETRIES: usize = 4;

const PLIST_TEMPLATE: &str = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<!--
  Global PR-state watcher LaunchAgent.  ONE agent polls ~/.fno/graph.json
  for open-PR backlog nodes and fires /fno:ship pr check or /fno:ship pr merged.
  RunAtLoad is false: review the rendered plist and run
    launchctl load {plist_path}
  yourself (human gate).
-->
<plist version=\"1.0\">
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
";

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// launchd PATH capture: the binary's dir, then the cargo bin dir, then the
// fixed install dirs; launchd PATH without cargo bin fails every tick at
// binary lookup.
fn default_agent_path(fno_binary: &str) -> String {
    let mut entries: Vec<String> = Vec::new();
    if fno_binary.contains('/') {
        if let Some(parent) = Path::new(fno_binary).parent() {
            entries.push(parent.display().to_string());
        }
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".cargo"));
    for p in [
        cargo_home.join("bin").display().to_string(),
        dirs_home().join(".local/bin").display().to_string(),
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
    ] {
        if !entries.contains(&p) {
            entries.push(p);
        }
    }
    entries.join(":")
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn augment_path(install_path: &str) -> String {
    let mut entries: Vec<String> = install_path
        .split(':')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    for extra in [
        dirs_home().join(".local/bin").display().to_string(),
        "/opt/homebrew/bin".to_string(),
    ] {
        if !entries.contains(&extra) {
            entries.push(extra);
        }
    }
    entries.join(":")
}

pub(crate) fn render_plist(
    launch_agents_dir: &Path,
    fno_binary: &str,
    install_path: Option<&str>,
    interval: i64,
) -> String {
    let home = dirs_home();
    let fno_state = home.join(".fno");
    let log_out = fno_state.join("logs/pr-watcher.out.log");
    let log_err = fno_state.join("logs/pr-watcher.err.log");
    let augmented = augment_path(
        &install_path
            .map(str::to_string)
            .unwrap_or_else(|| default_agent_path(fno_binary)),
    );
    PLIST_TEMPLATE
        .replace(
            "{plist_path}",
            &xml_escape(&launch_agents_dir.join(PLIST_FILENAME).display().to_string()),
        )
        .replace("{label}", &xml_escape(LABEL))
        .replace("{fno_binary}", &xml_escape(fno_binary))
        .replace("{path}", &xml_escape(&augmented))
        .replace("{home}", &xml_escape(&home.display().to_string()))
        .replace("{interval}", &interval.to_string())
        .replace("{log_out}", &xml_escape(&log_out.display().to_string()))
        .replace("{log_err}", &xml_escape(&log_err.display().to_string()))
}

fn write_if_changed(plist_path: &Path, plist_text: &str) -> std::io::Result<bool> {
    if let Ok(current) = std::fs::read_to_string(plist_path) {
        if current == plist_text {
            return Ok(false);
        }
    }
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(plist_path, plist_text)?;
    Ok(true)
}

/// stdout of `argv`, or None when the command is missing, hangs, or fails.
/// An unread answer never blocks a cure (fail-open, like the claim read it
/// replaced).
fn stdout_of(argv: &[&str], timeout_s: u64) -> String {
    let Ok(mut child) = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return String::new();
    };
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return String::new();
                }
                let mut out = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = std::io::Read::read_to_string(&mut pipe, &mut out);
                }
                return out;
            }
            Ok(None) if Instant::now() >= deadline => {
                kill_group(&child);
                let _ = child.wait();
                return String::new();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return String::new(),
        }
    }
}

fn kill_group(child: &std::process::Child) {
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
}

/// `[[dd-]hh:]mm:ss` from `ps -o etime=`; None on anything else.
fn etime_seconds(etime: &str) -> Option<i64> {
    let etime = etime.trim();
    let (dd, rest) = match etime.split_once('-') {
        Some((d, r)) => (d.parse::<i64>().ok()?, r),
        None => (0, etime),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let (hh, mm, ss) = match parts.as_slice() {
        [mm, ss] => (0i64, mm.parse::<i64>().ok()?, ss.parse::<i64>().ok()?),
        [hh, mm, ss] => (
            hh.parse::<i64>().ok()?,
            mm.parse::<i64>().ok()?,
            ss.parse::<i64>().ok()?,
        ),
        _ => return None,
    };
    if !(1..=99).contains(&mm) || !(0..=59).contains(&ss) {
        return None;
    }
    Some(((dd * 24 + hh) * 60 + mm) * 60 + ss)
}

/// PID of a tick process younger than one StartInterval (600s), else None.
///
/// launchd owns this answer: the old cwd-routed `pr-watch:tick` claim covered
/// only the sweep phase and read free while merge or recovery ran.
fn tick_in_flight() -> Option<i32> {
    if let Ok(pin) = std::env::var("FNO_TEST_PR_WATCH_TICK_PID") {
        return pin.parse().ok().filter(|pid| *pid > 0);
    }
    let listing = stdout_of(&["launchctl", "list", LABEL], LAUNCHCTL_TIMEOUT_S);
    let pid: i32 = {
        let idx = listing.find("\"PID\" = ")? + "\"PID\" = ".len();
        let digits: String = listing[idx..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()?
    };
    let etime = stdout_of(
        &["ps", "-o", "etime=", "-p", &pid.to_string()],
        LAUNCHCTL_TIMEOUT_S,
    );
    let age = etime_seconds(&etime)?;
    (age < 600).then_some(pid)
}

/// Name this bounce so the next killed tick can name its sender.
///
/// Writes `pr-watch-bounce.json` in the state dir (a deferred bounce writes
/// no sidecar: there is no kill to join) and emits `pr_watch_bounce` so
/// deferrals are countable. Never panics; a receipt must not block a cure.
fn record_bounce(caller: &str, deferred: bool, state_root: &Path) {
    let parent = stdout_of(
        &["ps", "-o", "command=", "-p", &libc::getppid().to_string()],
        LAUNCHCTL_TIMEOUT_S,
    )
    .trim()
    .chars()
    .take(160)
    .collect::<String>();
    let data = serde_json::json!({
        "caller": caller,
        "pid": libc::getpid(),
        "ppid": libc::getppid(),
        "parent": parent,
        "deferred": deferred,
    });
    if !deferred {
        let sidecar = state_root.join(BOUNCE_SIDECAR);
        let _ = std::fs::create_dir_all(&sidecar.parent().unwrap_or(state_root));
        let envelope = serde_json::json!({
            "ts": now_micros_z(),
            "caller": caller,
            "pid": libc::getpid(),
            "ppid": libc::getppid(),
            "parent": parent,
            "deferred": deferred,
        });
        let tmp = sidecar.with_name(format!("{BOUNCE_SIDECAR}.tmp"));
        if std::fs::File::create(&tmp)
            .and_then(|mut f| {
                writeln!(
                    f,
                    "{}",
                    serde_json::to_string(&envelope).unwrap_or_default()
                )
            })
            .is_ok()
        {
            let _ = std::fs::rename(&tmp, &sidecar);
        }
    }
    let event = serde_json::json!({
        "ts": now_micros_z(),
        "type": "pr_watch_bounce",
        "source": "daemon",
        "data": data,
    });
    let events = state_root.join("events.jsonl");
    let _ = std::fs::create_dir_all(events.parent().unwrap_or(state_root));
    let _ = crate::event_store::append_envelope(&events, &event.to_string(), None);
}

fn now_micros_z() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}

/// launchctl under a hard timeout: `(returncode, timed_out)`. A HANG reports
/// timed_out so the bounce can name the wedged step; a normal nonzero rc is
/// data (bootout of an unloaded job fails). The test script pin
/// (`FNO_TEST_PR_WATCH_LAUNCHCTL`, a JSON `[[rc, timed], ...]` consumed in
/// invocation order) stands in for the real tool, the way the load-state pin
/// does for `launchctl list`.
fn run_launchctl_timed(args: &[&str], timeout_s: u64) -> (i32, bool) {
    if let Ok(script) = std::env::var("FNO_TEST_PR_WATCH_LAUNCHCTL") {
        let idx = LAUNCHCTL_CALL.fetch_add(1, Ordering::Relaxed);
        let parsed: Option<Vec<Vec<serde_json::Value>>> = serde_json::from_str(&script).ok();
        let step = parsed
            .as_ref()
            .and_then(|steps| steps.get(idx))
            .and_then(|step| Some((step.first()?.as_i64()? as i32, step.get(1)?.as_bool()?)));
        return step.unwrap_or((-1, false));
    }
    let Ok(mut child) = std::process::Command::new("launchctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    else {
        return (-1, false);
    };
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code().unwrap_or(-1), false),
            Ok(None) if Instant::now() >= deadline => {
                kill_group(&child);
                let _ = child.wait();
                return (-1, true);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return (-1, false),
        }
    }
}

static LAUNCHCTL_CALL: AtomicUsize = AtomicUsize::new(0);

/// bootout -> bootstrap -> kickstart to cure a wedged launchd job.
///
/// This is the `dead`-verdict fix: the observed wedge (job loaded, state
/// `spawn scheduled`, never spawns, `kickstart` hangs) is only curable by
/// tearing the service out of its domain (`bootout`) and re-bootstrapping it.
/// Idempotent: safe on a healthy job (restart) and on a not-loaded one
/// (bootout failure tolerated). Returns `(message, exit_code)`.
///
/// `kickstart=false` stops after bootstrap, for a job whose tick is not a
/// harmless poll.
///
/// `defer_when_ticking`: a bounce fired mid-tick SIGTERMs that very tick, so
/// heal, refresh and doctor all pass the flag; a deferred refresh leaves its
/// rewritten plist for the next bounce.
fn bounce(
    plist_path: &Path,
    label: &str,
    kickstart: bool,
    defer_when_ticking: bool,
    caller: &str,
    state_root: &Path,
) -> (String, i32) {
    let uid = unsafe { libc::getuid() };
    if defer_when_ticking {
        if let Some(pid) = tick_in_flight() {
            if label == LABEL {
                record_bounce(caller, true, state_root);
            }
            return (format!("tick in flight (pid {pid}); bounce deferred"), 0);
        }
    }
    let domain = format!("gui/{uid}");
    let target = format!("{domain}/{label}");

    // Receipt before bootout: only this sidecar joins the SIGTERM back to its sender.
    if label == LABEL {
        record_bounce(caller, false, state_root);
    }

    // 1. bootout: a nonzero rc is EXPECTED when the job is not loaded, so only a
    //    hang is fatal here.
    let (_, timed) = run_launchctl_timed(&["bootout", &target], LAUNCHCTL_TIMEOUT_S);
    if timed {
        return (
            format!("`launchctl bootout {target}` timed out after {timeout_s}s"),
            1,
        );
    }

    // 2. bootstrap the plist back into the GUI domain. bootout (above) is
    //    asynchronous, so a bootstrap fired immediately after can lose to the
    //    still-settling label (rc=5). Retry with a short backoff so the refresh
    //    survives that window instead of reporting a spurious failure.
    let mut rc = -1;
    for attempt in 0..BOOTSTRAP_RETRIES {
        let plist = plist_path.display().to_string();
        let step = run_launchctl_timed(&["bootstrap", &domain, &plist], LAUNCHCTL_TIMEOUT_S);
        rc = step.0;
        if step.1 {
            return (
                format!("`launchctl bootstrap {domain}` timed out after {timeout_s}s"),
                1,
            );
        }
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
                "`launchctl bootstrap {domain} {plist}` failed (rc={rc})",
                plist = plist_path.display()
            ),
            1,
        );
    }

    if !kickstart {
        return (
            format!("bootstrapped {target}; first run at its scheduled time"),
            0,
        );
    }

    // 3. kickstart -k restarts if running; forces the first run so a fresh tick
    //    confirms liveness rather than waiting a full StartInterval.
    let (rc, timed) = run_launchctl_timed(&["kickstart", "-k", &target], LAUNCHCTL_TIMEOUT_S);
    if timed {
        return (
            format!("`launchctl kickstart -k {target}` timed out after {timeout_s}s"),
            1,
        );
    }
    if rc != 0 {
        return (
            format!("`launchctl kickstart -k {target}` failed (rc={rc})"),
            1,
        );
    }

    (format!("bounced {target}; awaiting first tick"), 0)
}

/// Re-render the plist onto the current binary, then bounce. Post-update hook.
///
/// Unlike heal (bounce the existing plist), this REWRITES the plist first so
/// the daemon picks up the freshly-installed binary path, a fresh captured
/// PATH, and a new mtime (so doctor's `healthy-pending` grace applies until
/// the next tick instead of a transient false `dead` - unless the recent ends
/// are a broken streak, which reads `wedged`). Called by the refresh verb at
/// the tail of an update so an update leaves an enabled watcher running the
/// new binary and un-wedges a job a mid-tick reinstall may have broken.
pub(crate) fn refresh_watcher(
    launch_agents_dir: &Path,
    fno_binary: &str,
    interval: i64,
    defer_when_ticking: bool,
    caller: &str,
    force_bounce: bool,
    state_root: &Path,
) -> (String, i32) {
    let plist_path = launch_agents_dir.join(PLIST_FILENAME);
    let plist_text = render_plist(launch_agents_dir, fno_binary, None, interval);
    let changed = match write_if_changed(&plist_path, &plist_text) {
        Ok(changed) => changed,
        Err(e) => {
            return (
                format!("failed to write plist {}: {e}", plist_path.display()),
                1,
            )
        }
    };
    if !changed && !force_bounce {
        return (format!("plist unchanged; not re-registered ({caller})"), 0);
    }
    bounce(
        &plist_path,
        LABEL,
        true,
        defer_when_ticking,
        caller,
        state_root,
    )
}

/// The leaf contract the Python `fno do pr watch refresh` command forwards
/// to: the disabled skip line, else the refresh message plus the one `Heal:`
/// readout line. The leaf never fails loud, so the exit code only names a
/// plist the refresh could not write.
pub(crate) fn leaf_output(args: &[String]) -> (String, i32) {
    let mut force_bounce = false;
    let mut caller = "refresh".to_string();
    let mut fno_binary: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--force-bounce" => force_bounce = true,
            "--caller" if i + 1 < args.len() => {
                i += 1;
                caller = args[i].clone();
            }
            "--fno-binary" if i + 1 < args.len() => {
                i += 1;
                fno_binary = Some(args[i].clone());
            }
            _ => {
                return (
                    "usage: fno-agents pr-watch refresh [--force-bounce] [--caller <name>] [--fno-binary <path>]\n"
                        .to_string(),
                    2,
                );
            }
        }
        i += 1;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let enabled = crate::pr_watch::status::as_bool(
        crate::pr_watch::status::cfg_lookup(&cwd, &["pr_watch", "enabled"]),
        false,
    );
    if !enabled {
        return ("pr-watch: disabled; nothing to refresh.\n".to_string(), 0);
    }
    let interval = crate::pr_watch::status::cfg_lookup(&cwd, &["pr_watch", "interval_seconds"])
        .and_then(|v| v.as_integer())
        .filter(|n| *n > 0)
        .unwrap_or(600);
    let launch_agents_dir = std::env::var_os("FNO_TEST_PR_WATCH_LAUNCH_AGENTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join("Library").join("LaunchAgents"));
    let state_root = crate::pr_watch::status::state_root(&cwd);
    let fno_binary =
        fno_binary.unwrap_or_else(|| crate::scrape::fno_py().to_string_lossy().into_owned());
    let (msg, rc) = refresh_watcher(
        &launch_agents_dir,
        &fno_binary,
        interval,
        true,
        &caller,
        force_bounce,
        &state_root,
    );
    // The one `Heal:` readout the Python leaf appended: the arm bit and the
    // journal are the only inputs, read through the same pr-heal renderer.
    let armed = crate::pr_watch::status::as_bool(
        crate::pr_watch::status::cfg_lookup(&cwd, &["auto_heal", "enabled"]),
        false,
    );
    let events = crate::paths::AgentsHome::from_env()
        .root()
        .parent()
        .map(|p| p.join("events.jsonl"))
        .unwrap_or_else(|| PathBuf::from(".fno/events.jsonl"));
    let heal = crate::heal::status_readout(armed, &events);
    (format!("pr-watch refresh: {msg}\n{heal}\n"), rc)
}

/// `fno-agents pr-watch refresh` dispatches here from `pr_watch::run`.
pub fn run(args: &[String]) -> i32 {
    let (out, code) = leaf_output(args);
    print!("{out}");
    code
}
