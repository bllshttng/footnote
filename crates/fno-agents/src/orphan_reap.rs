//! Orphaned cargo test binaries: detect, confirm, reap.
//!
//! A wedged cargo test binary never exits and never reaps its children; its
//! dead children stack up as `<defunct>` rows until the machine runs out of
//! pids. A deps binary that is parentless (ppid 1) or holds a zombie pile at
//! any ppid is always wrong. Confirmation demands a CACHEDIR.TAG in the
//! owning target dir, because a path-shape match alone is a name match:
//! `cli/src/fno/target` is a source tree. This module owns the whole question
//! natively so the Python compatibility shell does not grow it.

use crate::events::EventEmitter;
use regex::Regex;
use serde::Serialize;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub const ORPHAN_MIN_ZOMBIES: usize = 20;
pub const ENV_MIN_ELAPSED: &str = "FNO_TEST_ORPHAN_MIN_ELAPSED_SECONDS";

#[derive(Debug, PartialEq, Serialize)]
pub struct OrphanedTestBinary {
    pub pid: u32,
    pub command: String,
    pub zombies: usize,
    pub elapsed_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct ReapRow {
    pub pid: u32,
    pub elapsed_seconds: u64,
    pub zombies: usize,
    pub command: String,
    pub reaped: bool,
    pub reason: String,
}

fn deps_binary_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"/target/(?:debug|release)/deps/[A-Za-z0-9_]+-[0-9a-f]{16}$").unwrap()
    })
}

fn argv0(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or("")
}

fn is_deps_test_binary(command: &str) -> bool {
    deps_binary_re().is_match(argv0(command))
}

/// Parse the `ps etime` forms `DD-HH:MM:SS`, `HH:MM:SS`, `MM:SS`.
fn elapsed_seconds(value: &str) -> Option<u64> {
    let (days, rest) = match value.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, value),
    };
    let parts: Vec<u64> = rest
        .split(':')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    match parts.as_slice() {
        [h, m, s] => Some(days * 86400 + h * 3600 + m * 60 + s),
        [m, s] => Some(days * 86400 + m * 60 + s),
        _ => None,
    }
}

/// Detect candidates from one `ps -Ao pid,ppid,etime,%cpu,rss,command`
/// snapshot: a deps test binary that is parentless or holds a zombie pile,
/// with its dead children counted from the same snapshot. Rows that do not
/// parse are skipped; they carry no candidate.
pub fn detect(ps_output: &str) -> Vec<OrphanedTestBinary> {
    let mut processes: Vec<(u32, u32, u64, String)> = Vec::new();
    for line in ps_output.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() || line.starts_with("PID ") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 {
            continue;
        }
        let (Ok(pid), Ok(ppid)) = (fields[0].parse::<u32>(), fields[1].parse::<u32>()) else {
            continue;
        };
        let Some(elapsed) = elapsed_seconds(fields[2]) else {
            continue;
        };
        // The command is the tail; rejoin so argv words survive.
        let command = fields[5..].join(" ");
        processes.push((pid, ppid, elapsed, command));
    }
    // A defunct row is a dead child: count it against its PPID.
    let mut zombies_of: std::collections::BTreeMap<u32, usize> = std::collections::BTreeMap::new();
    for (_, ppid, _, command) in &processes {
        if command == "<defunct>" {
            *zombies_of.entry(*ppid).or_insert(0) += 1;
        }
    }
    let mut candidates: Vec<OrphanedTestBinary> = processes
        .iter()
        .filter(|(pid, ppid, _, command)| {
            if !is_deps_test_binary(command) {
                return false;
            }
            *ppid == 1 || zombies_of.get(pid).copied().unwrap_or(0) >= ORPHAN_MIN_ZOMBIES
        })
        .map(|(pid, _, elapsed, command)| OrphanedTestBinary {
            pid: *pid,
            zombies: zombies_of.get(pid).copied().unwrap_or(0),
            elapsed_seconds: *elapsed,
            command: command.clone(),
        })
        .collect();
    candidates.sort_by(|a, b| b.zombies.cmp(&a.zombies).then(a.pid.cmp(&b.pid)));
    candidates
}

/// A candidate is confirmed only when its owning target dir carries
/// CACHEDIR.TAG. A path-shape match alone is a name match.
pub fn confirmed(orphan: &OrphanedTestBinary) -> bool {
    let path = Path::new(argv0(&orphan.command));
    let Some(target) = path.parent().and_then(Path::parent).and_then(Path::parent) else {
        return false;
    };
    target.file_name().map(|n| n == "target").unwrap_or(false)
        && target.join("CACHEDIR.TAG").is_file()
}

/// `config.test.orphan_min_elapsed_seconds` (900); the env override lets a
/// positive control plant a fresh orphan and still exercise the kill.
pub fn min_elapsed_secs(cwd: &Path) -> u64 {
    if let Some(v) = std::env::var(ENV_MIN_ELAPSED)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        return v;
    }
    crate::agents_config::orphan_min_elapsed_secs(cwd)
}

/// One row per confirmed orphan: named, and killed only when asked. SIGKILL
/// goes to exactly the pids the confirmed detector named, never an argv match.
pub fn reap_rows(ps_output: &str, apply: bool, min_elapsed: u64) -> Vec<ReapRow> {
    let mut rows = Vec::new();
    for orphan in detect(ps_output) {
        if !confirmed(&orphan) {
            continue;
        }
        let mut row = ReapRow {
            pid: orphan.pid,
            elapsed_seconds: orphan.elapsed_seconds,
            zombies: orphan.zombies,
            command: orphan.command.clone(),
            reaped: false,
            reason: String::new(),
        };
        if orphan.elapsed_seconds < min_elapsed {
            row.reason = format!(
                "held: elapsed {}s is under the {}s guard (may be a live run)",
                orphan.elapsed_seconds, min_elapsed
            );
        } else if !apply {
            row.reason = "dry-run: pass --apply to kill".into();
        } else {
            // SAFETY: SIGKILL to a ppid-1 deps binary the confirmed detector named.
            let rc = unsafe { libc::kill(orphan.pid as libc::pid_t, libc::SIGKILL) };
            if rc == 0 {
                row.reaped = true;
                row.reason = "SIGKILL sent".into();
            } else {
                row.reason = match std::io::Error::last_os_error().kind() {
                    std::io::ErrorKind::NotFound => "already gone".into(),
                    other => format!("kill failed: {other}"),
                };
            }
        }
        rows.push(row);
    }
    rows
}

fn read_ps() -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-Ao", "pid,ppid,etime,%cpu,rss,command"])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Cadence of the daemon's orphan reap sweep.
pub const ORPHAN_SWEEP_SECS: Duration = Duration::from_secs(300);

/// The daemon tick's whole orphan-reap arm: throttled to [`ORPHAN_SWEEP_SECS`],
/// one-in-flight behind `in_flight`, run off the accept loop on the blocking
/// pool. Kills confirmed orphans past the age guard and emits one event per
/// kill. The daemon's own `waitpid` sweep only ever sees its own children; a
/// wedged deps/ test binary at ppid 1 holding zombie corpses is invisible to
/// it, and this arm is what reaches that shape.
pub fn maybe_sweep(last_sweep: &mut Instant, in_flight: &Arc<AtomicBool>, events: PathBuf) {
    if last_sweep.elapsed() < ORPHAN_SWEEP_SECS || in_flight.swap(true, Ordering::SeqCst) {
        return;
    }
    *last_sweep = Instant::now();
    let flag = Arc::clone(in_flight);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let min_elapsed = min_elapsed_secs(&cwd);
        let emitter = EventEmitter::new(events, "daemon");
        for row in reap_sweep_once(true, min_elapsed) {
            let _ = emitter.emit("orphan_test_binary_reaped", &row);
        }
    });
}

/// Non-blocking reap of any exited worker child the daemon spawned, so a
/// worker that exits while the daemon lives never lingers as a `<defunct>`
/// zombie. The daemon spawns nothing but workers, so a `waitpid(-1, WNOHANG)`
/// sweep is safe.
pub fn reap_daemon_children() {
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: waitpid with WNOHANG only reaps already-exited children and
        // returns 0 (none ready) or -1 (no children) without blocking.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
    }
}

/// The daemon sweep's one pass: detect, confirm, guard, kill, and return one
/// serializable row per reaped pid for the caller to emit.
pub fn reap_sweep_once(apply: bool, min_elapsed: u64) -> Vec<ReapRow> {
    let Some(ps) = read_ps() else {
        return Vec::new();
    };
    reap_rows(&ps, apply, min_elapsed)
        .into_iter()
        .filter(|row| row.reaped)
        .collect()
}

/// Binary-direct verb: `fno-agents orphan-reap [--apply] [--json]`. Dry-run by
/// default; prints one line per confirmed orphan and its disposition.
pub fn run_orphan_reap(args: &[String]) -> i32 {
    let apply = args.iter().any(|a| a == "--apply");
    let json = args.iter().any(|a| a == "--json");
    let min_elapsed =
        min_elapsed_secs(&std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf()));
    let Some(ps) = read_ps() else {
        eprintln!("fno-agents orphan-reap: ps unavailable");
        return 4;
    };
    let rows = reap_rows(&ps, apply, min_elapsed);
    if json {
        println!(
            "{}",
            serde_json::json!({ "orphan_test_binaries": rows, "exit_code": 0 })
        );
        return 0;
    }
    if rows.is_empty() {
        println!("no orphaned cargo test binaries");
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for row in &rows {
        let _ = writeln!(
            out,
            "orphaned test binary: pid {}, elapsed {}:{:02}:{:02}, zombies {}: {}",
            row.pid,
            row.elapsed_seconds / 3600,
            (row.elapsed_seconds % 3600) / 60,
            row.elapsed_seconds % 60,
            row.zombies,
            row.command
        );
        let _ = writeln!(out, "  {}", row.reason);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(orphan_ppid: u32) -> String {
        snapshot_with(
            orphan_ppid,
            "/w/preflight/crates/fno/target/debug/deps/fno-aa7282e99eecb046 portal",
        )
    }

    fn snapshot_with(orphan_ppid: u32, command: &str) -> String {
        let mut rows = vec![
            "PID PPID ELAPSED %CPU RSS COMMAND".to_string(),
            format!("59929 {orphan_ppid} 03:07:00 0.0 4096 {command}"),
        ];
        for i in 0..227 {
            rows.push(format!("{} 59929 00:00:10 0.0 0 <defunct>", 70000 + i));
        }
        rows.push("900 1 01:00:00 0.1 1024 fno-agents-daemon --serve".to_string());
        rows.join("\n")
    }

    /// A deps binary inside a real CACHEDIR.TAG-tagged target dir, so the
    /// confirmer accepts it.
    fn confirmed_snapshot() -> String {
        let dir = temp_dir("reap");
        let deps = dir.join("crates/x/target/debug/deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(dir.join("crates/x/target/CACHEDIR.TAG"), "Signature: x").unwrap();
        let bin = deps.join("probe-0123456789abcdef").display().to_string();
        snapshot_with(1, &bin)
    }

    #[test]
    fn names_a_parentless_deps_binary_and_counts_its_zombies() {
        let found = detect(&snapshot(1));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, 59929);
        assert_eq!(found[0].zombies, 227);
        assert_eq!(found[0].elapsed_seconds, 3 * 3600 + 7 * 60);
    }

    #[test]
    fn names_a_wedged_reaper_at_any_ppid() {
        // The measured live case: cargo parent still alive, pipe-blocked.
        let found = detect(&snapshot(4321));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, 59929);
    }

    #[test]
    fn skips_a_childless_deps_binary_with_a_parent() {
        let ps = "PID PPID ELAPSED %CPU RSS COMMAND\n\
                  59929 4321 03:07:00 0.0 4096 /w/crates/fno/target/debug/deps/fno-aa7282e99eecb046 portal\n";
        assert!(detect(ps).is_empty());
    }

    #[test]
    fn holds_below_the_zombie_bar_when_parented() {
        let mut rows = vec![
            "PID PPID ELAPSED %CPU RSS COMMAND".to_string(),
            "59929 4321 03:07:00 0.0 4096 /w/crates/fno/target/debug/deps/fno-aa7282e99eecb046"
                .to_string(),
        ];
        for i in 0..(ORPHAN_MIN_ZOMBIES - 1) {
            rows.push(format!("{} 59929 00:00:10 0.0 0 <defunct>", 70000 + i));
        }
        assert!(detect(&rows.join("\n")).is_empty());
    }

    #[test]
    fn parses_etime_forms() {
        assert_eq!(elapsed_seconds("03:07"), Some(187));
        assert_eq!(elapsed_seconds("01:00:00"), Some(3600));
        assert_eq!(
            elapsed_seconds("2-03:07:00"),
            Some(2 * 86400 + 3 * 3600 + 420)
        );
        assert_eq!(elapsed_seconds("nope"), None);
    }

    #[test]
    fn confirmation_demands_a_cachedir_tag() {
        let dir = temp_dir("confirm");
        let deps = dir.join("crates/x/target/debug/deps");
        std::fs::create_dir_all(&deps).unwrap();
        let mk = || OrphanedTestBinary {
            pid: 1,
            command: format!("{} 300", deps.join("probe-0123456789abcdef").display()),
            zombies: 0,
            elapsed_seconds: 3600,
        };
        assert!(
            !confirmed(&mk()),
            "no tag yet: shape alone must not confirm"
        );
        std::fs::write(dir.join("crates/x/target/CACHEDIR.TAG"), "Signature: x").unwrap();
        assert!(confirmed(&mk()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirmation_refuses_the_source_tree_shape() {
        // cli/src/fno/target is a source dir: no CACHEDIR.TAG, never confirmed.
        let orphan = OrphanedTestBinary {
            pid: 1,
            command: "/w/cli/src/fno/target/debug/deps/fno-aa7282e99eecb046".into(),
            zombies: 0,
            elapsed_seconds: 3600,
        };
        assert!(!confirmed(&orphan));
    }

    #[test]
    fn reap_holds_below_the_guard_and_reports_dry_run() {
        let ps = confirmed_snapshot();
        let rows = reap_rows(&ps, false, 36_000);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].reaped, "{}", rows[0].reason);
        assert!(rows[0].reason.contains("guard"), "{}", rows[0].reason);
        let dry = reap_rows(&ps, false, 0);
        assert!(dry[0].reason.contains("dry-run"));
    }

    /// A unique per-test tempdir so parallel tests never share a fixture.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("fno-orphan-reap-{}-{tag}", std::process::id()))
    }
}
