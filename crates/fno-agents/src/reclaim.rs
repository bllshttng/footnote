//! The machine janitor: `fno-agents reclaim`, surfaced as `fno doctor reclaim`.
//! Removes disk bloat footnote development piles up:
//! plugin-cache build copies, leaked test HOMEs, stale test scratch, and
//! unregistered worktree targets, then prunes the shared uv cache under a
//! timeout. Everything removed is rebuilt or downloaded again on demand.
//!
//! Dry run by default; `--apply` removes and rewrites
//! `<state-root>/reclaim/last-run.json` with the bytes each lane reclaimed.
//! The daemon's daily sweep gates on that receipt's own mtime, so gate and
//! effect share one file and a crashed run just retries next tick.
//!
//! The Python leg lived at `cli/src/fno/reclaim.py` until this port; the
//! lanes are characterised in the in-module tests below.
use serde_json::json;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use crate::paths::{canonical_repo_root, dirs_home, AgentsHome};

/// A running test keeps its own fake HOME; two hours means nobody is using it.
const LEAKED_HOME_MINUTES: u64 = 120;
/// No fno runtime state lives under an fno-* name in the temp dir; only tests write it.
const SCRATCH_MINUTES: u64 = 24 * 60;
/// uv's prune waits for every running uv process first, so the wait is capped.
const UV_PRUNE_TIMEOUT: Duration = Duration::from_secs(120);
/// Age at which the daemon's daily gate re-runs the sweep.
const DAILY_SECS: u64 = 24 * 3600;

const LEAKED_HOME_MARKERS: &[&str] = &[
    ".fno",
    ".cache/fno-bootstrap",
    ".cache/uv",
    ".local/share/uv",
    ".claude.json",
];

struct Lane {
    name: &'static str,
    paths: Vec<PathBuf>,
    bytes: u64,
    note: String,
}

impl Lane {
    fn new(name: &'static str, paths: Vec<PathBuf>) -> Self {
        Lane {
            name,
            paths,
            bytes: 0,
            note: String::new(),
        }
    }
}

/// Recursive byte count that never follows a symlink out of the tree it was
/// given: a link's target is someone else's tree, not this path's bulk.
fn tree_bytes(path: &Path) -> u64 {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            total += tree_bytes(&entry.path());
        }
    }
    total
}

fn temp_root() -> PathBuf {
    if let Some(root) = std::env::var_os("FNO_RECLAIM_TEMP_ROOT") {
        return PathBuf::from(root);
    }
    std::env::temp_dir()
}

/// `<state-root>/reclaim/last-run.json`: the state root is the agents home's
/// parent in the default layout. The env override lets tests and custom state
/// roots point the gate and the receipt at the same place.
fn receipt_path(home: &AgentsHome) -> PathBuf {
    if let Some(root) = std::env::var_os("FNO_RECLAIM_STATE_ROOT") {
        return PathBuf::from(root).join("reclaim").join("last-run.json");
    }
    match home.root().parent() {
        Some(state_root) => state_root.join("reclaim").join("last-run.json"),
        None => home.root().join("reclaim").join("last-run.json"),
    }
}

fn tagged_crate_targets(root: &Path) -> Vec<PathBuf> {
    // Tagged by CACHEDIR.TAG, never name-matched: cli/src/fno/target and
    // friends are source dirs (a name-based sweep deleted 66 of them once).
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("crates")) {
        for entry in entries.flatten() {
            let target = entry.path().join("target");
            if target.join("CACHEDIR.TAG").is_file() {
                found.push(target);
            }
        }
    }
    found
}

fn plugin_cache_copies() -> Vec<PathBuf> {
    // Cargo output and worktrees copied into harness plugin caches. The
    // plugin runs from these copies, but nothing reads a cargo target or a
    // worktree there.
    let home = dirs_home();
    let mut found = Vec::new();
    for cache_root in [
        home.join(".claude/plugins/cache"),
        home.join(".codex/plugins/cache"),
    ] {
        let Ok(harnesses) = std::fs::read_dir(&cache_root) else {
            continue;
        };
        for harness in harnesses.flatten() {
            let Ok(checkouts) = std::fs::read_dir(harness.path().join("fno")) else {
                continue;
            };
            for checkout in checkouts.flatten() {
                let root = checkout.path();
                found.extend(tagged_crate_targets(&root));
                for lane in [".claude/worktrees", ".tmp-worktrees"] {
                    if let Ok(trees) = std::fs::read_dir(root.join(lane)) {
                        found.extend(trees.flatten().map(|e| e.path()));
                    }
                }
            }
        }
    }
    found
}

fn is_old_leaked_home(entry: &Path, cutoff: SystemTime) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(entry) else {
        return false;
    };
    if !meta.is_dir() || meta.modified().map(|m| m > cutoff).unwrap_or(false) {
        return false;
    }
    LEAKED_HOME_MARKERS
        .iter()
        .any(|marker| entry.join(marker).exists())
}

fn leaked_test_homes() -> Vec<PathBuf> {
    let cutoff = SystemTime::now() - Duration::from_secs(LEAKED_HOME_MINUTES * 60);
    let root = temp_root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(".tmp"))
                .unwrap_or(false)
                && is_old_leaked_home(p, cutoff)
        })
        .collect()
}

fn stale_test_scratch() -> Vec<PathBuf> {
    let cutoff = SystemTime::now() - Duration::from_secs(SCRATCH_MINUTES * 60);
    let root = temp_root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("fno-"))
                .unwrap_or(false)
                && std::fs::symlink_metadata(p)
                    .map(|m| m.modified().map(|m| m <= cutoff).unwrap_or(false))
                    .unwrap_or(false)
        })
        .collect()
}

fn registered_worktrees() -> Vec<String> {
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(str::to_string)
        .collect()
}

fn untracked_worktree_targets() -> Vec<PathBuf> {
    // Cargo output under a worktree dir git no longer tracks (a removed or
    // half-made worktree). Only target/ goes: the rest may hold work. The
    // repo root is the CANONICAL checkout (git lists it first from any
    // worktree), so the lane answers the same from every checkout.
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(repo) = canonical_repo_root(&cwd) else {
        return Vec::new();
    };
    let registered = registered_worktrees();
    let home = dirs_home();
    let mut found = Vec::new();
    let repo_name = repo.file_name().unwrap_or_default().to_string_lossy();
    let bases = [
        repo.join(".claude/worktrees"),
        home.join(".fno/worktrees").join(repo_name.as_ref()),
    ];
    for base in bases {
        let Ok(trees) = std::fs::read_dir(&base) else {
            continue;
        };
        for tree in trees.flatten() {
            let path = tree.path();
            if !path.is_dir() {
                continue;
            }
            // Both sides canonicalised: macOS aliases /var to /private/var,
            // and a raw spelling mismatch would read a LIVE worktree as
            // unregistered and sweep its build tree.
            let Ok(candidate) = path.canonicalize() else {
                continue;
            };
            let registered_here = registered
                .iter()
                .filter_map(|r| std::fs::canonicalize(r).ok())
                .any(|r| r == candidate);
            if registered_here {
                continue;
            }
            found.extend(tagged_crate_targets(&path));
        }
    }
    found
}

/// `uv cache dir`, when uv is on PATH.
fn uv_cache_dir() -> Option<PathBuf> {
    let out = Command::new("uv")
        .args(["cache", "dir", "--color", "never"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if dir.is_empty() {
        None
    } else {
        Some(PathBuf::from(dir))
    }
}

/// Poll `uv cache prune` under the 120s cap: uv takes its cache lock
/// exclusively for prune, so a busy cache holds the whole sweep.
fn uv_prune_apply() -> bool {
    let Ok(mut child) = Command::new("uv")
        .args(["cache", "prune", "--color", "never"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + UV_PRUNE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(250)),
            Err(_) => return false,
        }
    }
}

fn write_receipt(home: &AgentsHome, lanes: &[Lane]) -> std::io::Result<()> {
    let path = receipt_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lanes_json: serde_json::Map<String, Value> = lanes
        .iter()
        .map(|l| {
            (
                l.name.to_string(),
                json!({"paths": l.paths.len(), "bytes": l.bytes, "note": l.note}),
            )
        })
        .collect();
    let total: u64 = lanes.iter().map(|l| l.bytes).sum();
    let payload = json!({
        "applied_at": now_stamp(),
        "lanes": lanes_json,
        "total_bytes": total,
    });
    std::fs::write(
        path,
        serde_json::to_string_pretty(&payload).unwrap_or_default() + "\n",
    )
}

fn now_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // UTC stamp without pulling a chrono dependency: YYYY-MM-DDTHH:MM:SSZ.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// The daemon's daily gate: run `--apply` when the receipt is missing or
/// older than a day. Receipt and effect are one file, so a crashed child
/// simply reads as stale on the next tick.
pub fn maybe_run_daily(home: &AgentsHome) {
    let stale = match std::fs::metadata(receipt_path(home)) {
        Ok(meta) => meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .map(|age| age.as_secs() > DAILY_SECS)
            .unwrap_or(true),
        Err(_) => true,
    };
    if stale {
        let _ = run_reclaim(&["--apply".to_string()], home);
    }
}

pub fn run_reclaim(args: &[String], home: &AgentsHome) -> i32 {
    let mut apply = false;
    let mut verbose = false;
    for arg in args {
        match arg.as_str() {
            "--apply" => apply = true,
            "-v" | "--verbose" => verbose = true,
            other => {
                eprintln!("fno doctor reclaim: unknown argument: {other}");
                return 2;
            }
        }
    }

    let mut lanes = vec![
        Lane::new("plugin_cache_build_copies", plugin_cache_copies()),
        Lane::new("leaked_test_homes", leaked_test_homes()),
        Lane::new("stale_test_scratch", stale_test_scratch()),
        Lane::new("untracked_worktree_targets", untracked_worktree_targets()),
    ];
    for lane in &mut lanes {
        lane.bytes = lane.paths.iter().map(|p| tree_bytes(p)).sum();
        if apply {
            for path in &lane.paths {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
    let mut uv = Lane::new("uv_cache_prune", Vec::new());
    match uv_cache_dir() {
        None => uv.note = "uv not found".to_string(),
        Some(dir) => {
            uv.bytes = tree_bytes(&dir);
            uv.paths.push(dir);
            uv.note = if !apply {
                "would run: uv cache prune".to_string()
            } else if uv_prune_apply() {
                "pruned".to_string()
            } else {
                format!("prune skipped: uv busy for {}s", UV_PRUNE_TIMEOUT.as_secs())
            };
        }
    }

    if apply {
        // The uv lane never removes the cache dir itself, only prunes it.
        if let Err(e) = write_receipt(home, &lanes) {
            eprintln!("fno doctor reclaim: receipt write failed: {e}");
        }
    }

    println!(
        "fno doctor reclaim: {}",
        if apply {
            "removing"
        } else {
            "dry run (--apply to remove)"
        }
    );
    for lane in lanes.iter().chain(std::iter::once(&uv)) {
        if lane.paths.is_empty() && lane.note.is_empty() {
            println!("       -  {}: nothing", lane.name);
            continue;
        }
        println!(
            "{:6.1} GB  {}: {} path(s){}",
            lane.bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            lane.name,
            lane.paths.len(),
            if lane.note.is_empty() {
                String::new()
            } else {
                format!(" - {}", lane.note)
            }
        );
        if verbose {
            for path in &lane.paths {
                println!("          {}", path.display());
            }
        }
    }
    if apply {
        println!("receipt: {}", receipt_path(home).display());
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that repoint the process-global env: a concurrent
    /// reader can catch the root mid-flip and sweep the REAL temp dir (the
    /// same ENV_LOCK shape king_board's HOME_LOCK uses).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_lane_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fno-reclaim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn age(path: &Path, minutes: u64) {
        let old = SystemTime::now() - Duration::from_secs(minutes * 60);
        // A dir cannot be opened for writing; futimens through a read handle
        // is allowed for the file's owner.
        let file = std::fs::File::options().read(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }

    #[test]
    fn only_old_homes_holding_fno_state_are_leaks() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_lane_root("leaks");
        let old = root.join(".tmpOLD");
        std::fs::create_dir_all(old.join(".cache/fno-bootstrap")).unwrap();
        std::fs::write(old.join(".cache/fno-bootstrap/x"), b"x").unwrap();
        age(&old, 180);
        let fresh = root.join(".tmpNEW");
        std::fs::create_dir_all(fresh.join(".cache/fno-bootstrap")).unwrap();
        std::fs::write(fresh.join(".cache/fno-bootstrap/x"), b"x").unwrap();
        age(&fresh, 1);
        let plain = root.join(".tmpPLAIN");
        std::fs::create_dir_all(&plain).unwrap();
        age(&plain, 180);

        std::env::set_var("FNO_RECLAIM_TEMP_ROOT", &root);
        let found = leaked_test_homes();
        std::env::remove_var("FNO_RECLAIM_TEMP_ROOT");
        assert!(found.contains(&old));
        assert!(!found.contains(&fresh));
        assert!(!found.contains(&plain));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_removes_the_old_and_spares_the_fresh() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_lane_root("apply");
        let old = root.join(".tmpOLD");
        std::fs::create_dir_all(old.join(".cache/uv")).unwrap();
        std::fs::write(old.join(".cache/uv/payload"), vec![0u8; 4096]).unwrap();
        age(&old, 180);
        let fresh = root.join(".tmpNEW");
        std::fs::create_dir_all(fresh.join(".cache/fno-bootstrap")).unwrap();
        std::fs::write(fresh.join(".cache/fno-bootstrap/x"), b"x").unwrap();
        age(&fresh, 1);

        let state = temp_lane_root("state");
        std::env::set_var("FNO_RECLAIM_TEMP_ROOT", &root);
        std::env::set_var("FNO_RECLAIM_STATE_ROOT", &state);
        let home = AgentsHome::at(state.join("agents"));
        let rc = run_reclaim(&["--apply".to_string()], &home);
        std::env::remove_var("FNO_RECLAIM_TEMP_ROOT");
        std::env::remove_var("FNO_RECLAIM_STATE_ROOT");
        assert_eq!(rc, 0);
        assert!(!old.exists(), "the aged fake HOME is removed");
        assert!(fresh.exists(), "the fresh fake HOME survives");
        let receipt = std::fs::read_to_string(state.join("reclaim/last-run.json")).unwrap();
        assert!(receipt.contains("leaked_test_homes"));
        assert!(receipt.contains("total_bytes"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn scratch_lane_uses_one_day() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_lane_root("scratch");
        let junk = root.join("fno-parity-junk");
        std::fs::create_dir_all(&junk).unwrap();
        age(&junk, 25 * 60);
        let fresh = root.join("fno-fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        age(&fresh, 10);
        std::env::set_var("FNO_RECLAIM_TEMP_ROOT", &root);
        let found = stale_test_scratch();
        std::env::remove_var("FNO_RECLAIM_TEMP_ROOT");
        assert_eq!(found, vec![junk]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
