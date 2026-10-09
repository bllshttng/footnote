//! The machine janitor: `fno-agents reclaim`, surfaced as `fno doctor reclaim`.
//! Removes disk bloat footnote development piles up:
//! plugin-cache build copies, Codex converge backups, leaked test HOMEs, stale
//! test scratch, unregistered worktree targets, and state-root backup stamps
//! (report-only until the operator sets a retention), then prunes the shared
//! uv cache under a timeout. Everything removed is rebuilt or downloaded again on demand.
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
/// Dated stamp dirs under `<state-root>/backups` are report-only by
/// default: the state-root inventory holds migration and recovery stamps to
/// the operator's yes, never the janitor's clock. Setting the retention
/// (env, then `reclaim.backups_retention_days`) IS that yes; the newest
/// [`BACKUP_KEEP_NEWEST`] stamps per subdir survive regardless.
const BACKUP_KEEP_NEWEST: usize = 5;
/// The backup subdirs made of one stamp dir per run, all REPORT-only here.
/// Only [`BACKUP_PRUNE_SUBDIR`] is ever deletable, and only under the audit
/// gate; the recovery snapshots can still be referenced by open attention
/// items, so they are never swept. Rotation-managed trees (graph.db.*,
/// claims-table) keep their own keep-N and are not stamp dirs.
const BACKUP_REPORT_SUBDIRS: &[&str] = &[
    "state-root-migration",
    "state-root-recovery",
    "state-recovery",
];
/// The one subdir age retention may reap, under the audit gate.
const BACKUP_PRUNE_SUBDIR: &str = "state-root-migration";

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
pub(crate) fn tree_bytes(path: &Path) -> u64 {
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
    reclaim_state_root(home)
        .join("reclaim")
        .join("last-run.json")
}

/// The state root the reclaim surfaces read: the agents home's parent in the
/// default layout, overridable by env so tests and custom roots can point the
/// gate, the receipt, the drift lane, and the check-in reading at one place.
pub(crate) fn reclaim_state_root(home: &AgentsHome) -> PathBuf {
    if let Some(root) = std::env::var_os("FNO_RECLAIM_STATE_ROOT") {
        return PathBuf::from(root);
    }
    home.root()
        .parent()
        .unwrap_or_else(|| home.root())
        .to_path_buf()
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

fn fno_plugin_dir(root: &Path) -> PathBuf {
    root.join("fno")
}

fn plugin_cache_copies() -> Vec<PathBuf> {
    // Cargo output and worktrees copied into harness plugin caches. The
    // plugin runs from these copies, but nothing reads a cargo target or a
    // worktree there.
    let home = dirs_home();
    let mut found = Vec::new();
    let mut cache_roots = vec![home.join(".claude/plugins/cache")];
    if let Some(codex_home) = crate::codex_store::codex_home() {
        cache_roots.push(codex_home.join("plugins/cache"));
    }
    for cache_root in cache_roots {
        let Ok(harnesses) = std::fs::read_dir(&cache_root) else {
            continue;
        };
        for harness in harnesses.flatten() {
            let Ok(checkouts) = std::fs::read_dir(fno_plugin_dir(&harness.path())) else {
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

fn codex_cache_quarantines() -> (Lane, Option<std::fs::File>) {
    let mut lane = Lane::new("codex_cache_quarantines", Vec::new());
    let Some(home) = crate::codex_store::codex_home() else {
        lane.note = "kept: codex home unresolved".to_string();
        return (lane, None);
    };
    let quarantine_root = home.join("footnote");
    if !quarantine_root.is_dir() {
        return (lane, None);
    }

    let lock_path = quarantine_root.join("plugin-channel.lock");
    let Ok(lock) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
    else {
        lane.note = "kept: codex plugin converge in flight".to_string();
        return (lane, None);
    };
    if lock.try_lock().is_err() {
        lane.note = "kept: codex plugin converge in flight".to_string();
        return (lane, None);
    }

    let marker_path = quarantine_root.join("plugin-channel.json");
    let Some(marker) = std::fs::read_to_string(&marker_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned())
        .and_then(|object| {
            Some((
                object.get("marketplace")?.as_str()?.to_string(),
                object.get("source")?.as_str()?.to_string(),
            ))
        })
    else {
        lane.note = "kept: live copy unreadable (plugin-channel.json)".to_string();
        return (lane, None);
    };
    let (marketplace, source) = marker;
    if marketplace != "footnote" && marketplace != "footnote-dev" {
        lane.note = "kept: live copy unreadable (plugin-channel.json)".to_string();
        return (lane, None);
    }

    let live_cache = fno_plugin_dir(&home.join("plugins/cache").join(&marketplace));
    if !live_cache.is_dir() {
        lane.note = format!("kept: live cache {} not found", live_cache.display());
        return (lane, None);
    }
    match std::fs::symlink_metadata(quarantine_root.join("rollback-failure.json")) {
        Ok(_) => {
            lane.note = "kept: rollback-failure.json present".to_string();
            return (lane, Some(lock));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            lane.note = "kept: rollback-failure.json present".to_string();
            return (lane, Some(lock));
        }
    }

    let Ok(live_cache) = live_cache.canonicalize() else {
        lane.note = format!("kept: live cache {} not found", live_cache.display());
        return (lane, Some(lock));
    };
    let source = PathBuf::from(source);
    let source = source
        .is_dir()
        .then(|| source.canonicalize().ok())
        .flatten();
    let protected = source
        .as_deref()
        .into_iter()
        .chain(std::iter::once(live_cache.as_path()))
        .collect::<Vec<_>>();

    let Ok(entries) = std::fs::read_dir(&quarantine_root) else {
        return (lane, Some(lock));
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let suffix = name
            .strip_prefix(".footnote.")
            .or_else(|| name.strip_prefix(".footnote-dev."));
        let Some(suffix) = suffix else {
            continue;
        };
        if suffix.len() != 32
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            continue;
        }
        if !fno_plugin_dir(&path).is_dir() {
            continue;
        }
        let Ok(candidate) = path.canonicalize() else {
            continue;
        };
        if protected.iter().any(|protected| {
            candidate == *protected
                || candidate.starts_with(protected)
                || protected.starts_with(&candidate)
        }) {
            continue;
        }
        lane.paths.push(path);
    }
    (lane, Some(lock))
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

/// Every root this run's cargo-build-dirs lane sweeps: every registry root,
/// plus the cwd's canonical repo when `include_cwd_root` says so. That extra
/// root is a HAND-run convenience only (a person typing `fno doctor reclaim
/// --apply` from inside a repo wants that repo swept even if it never
/// registered) - the daemon's own daily tick passes `false` so an arbitrary
/// launch cwd, real or a test's, never adds a sweep root the registry did
/// not already name.
fn cargo_build_dirs_roots(home: &AgentsHome, include_cwd_root: bool) -> Vec<String> {
    let mut roots = crate::daemon::worktree_sweep::registry_repo_roots(home);
    if include_cwd_root {
        if let Some(repo) = canonical_repo_root(&std::env::current_dir().unwrap_or_default()) {
            let repo = repo.to_string_lossy().into_owned();
            if !roots.contains(&repo) {
                roots.push(repo);
            }
        }
    }
    roots
}

/// The build-base lane: `cargo_build_dirs::sweep` per root from
/// [`cargo_build_dirs_roots`]. Roots with no `crates/*/Cargo.toml` are
/// skipped - sweeping a manifest-less root under a shared base could only
/// ever misjudge rows it cannot resolve. The note carries each root's summary
/// line; bytes are reclaimed (apply) or projected (dry run).
fn cargo_build_dirs_lane(home: &AgentsHome, apply: bool, include_cwd_root: bool) -> Lane {
    let mut lane = Lane::new("cargo_build_dirs", Vec::new());
    let roots = cargo_build_dirs_roots(home, include_cwd_root);
    let mut notes: Vec<String> = Vec::new();
    for root in roots {
        let root = PathBuf::from(&root);
        if crate::cargo_build_dirs::workspace_manifests(&root).is_empty() {
            continue;
        }
        let rep = crate::cargo_build_dirs::sweep(&root, apply, SystemTime::now());
        lane.bytes += rep.projected_bytes;
        if let Some(summary) = rep.lines.last() {
            notes.push(summary.clone());
        }
    }
    lane.note = notes.join("; ");
    lane
}

/// The Claude config tmp lane: orphaned `.claude.json.tmp.*` atomic-write
/// temp files (a claude process killed mid-write), reaped only when the file
/// is past [`crate::claude_config_tmp::TMP_MIN_AGE`] and its pid is dead.
fn claude_config_tmp_lane(apply: bool) -> Lane {
    let rep = crate::claude_config_tmp::sweep(apply, SystemTime::now());
    let note = rep.note(apply);
    let mut lane = Lane::new("claude_config_tmp", rep.reaped);
    lane.bytes = rep.bytes_reaped;
    lane.note = note;
    lane
}

/// The operator's retention days for the stamp dirs, `None` when unset: env
/// override first, then the global `reclaim.backups_retention_days`. Both
/// read zero as disabled, because a zero-day retention that deleted on the
/// next sweep is not what anyone setting 0 could mean. Unset reads as
/// report-only, because the state-root inventory holds these stamps to the
/// operator's yes, never the janitor's clock.
fn backups_retention_days() -> Option<u64> {
    if let Some(days) = std::env::var_os("FNO_RECLAIM_BACKUPS_DAYS") {
        return days
            .to_str()
            .and_then(|d| d.parse().ok())
            .filter(|v| *v > 0);
    }
    crate::agents_config::config_lookup_global(&["reclaim", "backups_retention_days"])
        .and_then(|v| v.as_integer())
        .map(|v| v.max(0) as u64)
        .filter(|v| *v > 0)
}

/// The audit verdict the inventory doc requires before a migration stamp may
/// go: `FNO_RECLAIM_BACKUPS_AUDIT` names a file holding the JSON verdict of
/// the state-recovery audit. The audit has no persisted receipt of its own,
/// so the operator redirects one there; a file that does not parse, or reads
/// `zero_missing` false, deletes nothing. Only stamps OLDER than the
/// verdict's mtime are prunable: the audit saw those and no others.
fn audit_verdict_mtime() -> Option<SystemTime> {
    let path = std::env::var_os("FNO_RECLAIM_BACKUPS_AUDIT")?;
    let meta = std::fs::metadata(&path).ok()?;
    let modified = meta.modified().ok()?;
    let verdict: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    verdict.get("zero_missing")?.as_bool()?.then_some(modified)
}

/// The backups-retention lane: the migration stamp pile under
/// `<state-root>/backups/state-root-migration`, bounded only when three
/// gates agree. The operator sets a retention (the inventory doc's "yes"),
/// supplies a zero-missing audit verdict the stamp predates (the doc's
/// recovery-receipt requirement), and the [`BACKUP_KEEP_NEWEST`] floor
/// always holds. A stamp may hold the only copy of a parked conflict, and a
/// recovery snapshot can still be referenced by open attention items, so
/// the recovery subdirs (`state-root-recovery`, `state-recovery`) are
/// report-only forever: they appear in the pile reading and are never
/// deleted here.
fn backups_retention_lane(home: &AgentsHome, apply: bool) -> Lane {
    let mut lane = Lane::new("backups_retention", Vec::new());
    let days = backups_retention_days();
    let (count, bytes) = stamp_pile(&reclaim_state_root(home).join("backups"));
    let Some(days) = days else {
        lane.note = format!(
            "report only: {count} stamp dir(s), {:.1} GB; set reclaim.backups_retention_days (or FNO_RECLAIM_BACKUPS_DAYS) and a zero-missing audit verdict (FNO_RECLAIM_BACKUPS_AUDIT) to reap past a retention, newest {BACKUP_KEEP_NEWEST} always kept",
            bytes as f64 / (1024.0 * 1024.0 * 1024.0)
        );
        return lane;
    };
    let Some(audited_at) = audit_verdict_mtime() else {
        lane.note = format!(
            "report only: {count} stamp dir(s), {:.1} GB; retention {days}d is set but no fresh zero-missing audit verdict (FNO_RECLAIM_BACKUPS_AUDIT) authorizes deletion",
            bytes as f64 / (1024.0 * 1024.0 * 1024.0)
        );
        return lane;
    };
    let cutoff = SystemTime::now() - Duration::from_secs(days * 24 * 3600);
    let kept = reap_stamps(
        &reclaim_state_root(home).join("backups"),
        &mut lane,
        cutoff,
        audited_at,
        apply,
    );
    lane.note = format!(
        "kept {kept}, {} past {days}d retention audited at the verdict's stamp (newest {BACKUP_KEEP_NEWEST} always kept)",
        if apply { "reaped" } else { "would reap" },
    );
    lane
}

/// Every stamp dir in the retention's report scope, with its count and
/// bytes. Rotation-managed siblings (graph.db.*, claims-table) keep their
/// own keep-N and are not stamp dirs.
fn stamp_pile(backups_root: &Path) -> (usize, u64) {
    let mut count = 0usize;
    let mut bytes = 0u64;
    for subdir in BACKUP_REPORT_SUBDIRS {
        let Ok(entries) = std::fs::read_dir(backups_root.join(subdir)) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            count += 1;
            bytes += tree_bytes(&path);
        }
    }
    (count, bytes)
}

/// The age-plus-audit-plus-keep-floor pass over the migration stamps. A
/// stamp survives when it is fresh, inside the keep floor, or newer than
/// the audit verdict; only a stamp the audit actually saw may go. Returns
/// how many stamps survive, and (on `apply`) pushes every reaped path and
/// its bytes into `lane`.
fn reap_stamps(
    backups_root: &Path,
    lane: &mut Lane,
    cutoff: SystemTime,
    audited_at: SystemTime,
    apply: bool,
) -> usize {
    let mut kept = 0usize;
    let Ok(entries) = std::fs::read_dir(backups_root.join(BACKUP_PRUNE_SUBDIR)) else {
        return 0;
    };
    let mut stamps: Vec<(PathBuf, SystemTime)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|p| {
            std::fs::symlink_metadata(&p)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|m| (p, m))
        })
        .collect();
    // Newest first, so the keep floor protects exactly the latest runs.
    stamps.sort_by(|a, b| b.1.cmp(&a.1));
    for (rank, (path, modified)) in stamps.into_iter().enumerate() {
        if modified > cutoff || rank < BACKUP_KEEP_NEWEST || modified > audited_at {
            kept += 1;
            continue;
        }
        let bytes = tree_bytes(&path);
        if apply {
            if std::fs::remove_dir_all(&path).is_err() {
                kept += 1;
                continue;
            }
        }
        lane.bytes += bytes;
        lane.paths.push(path);
    }
    kept
}

/// The state-layout lane: the daemon's migration retry. Plain kinds move,
/// merge, park or delete-locks per [`crate::state_layout::migrate`]; sqlite
/// rows read pending until the backup-API protocol lands (change 4.1) and
/// mux rows wait for their server. Dry runs only report.
fn state_layout_lane(home: &AgentsHome, apply: bool) -> Lane {
    let receipt = crate::state_layout::migrate(&reclaim_state_root(home), apply);
    let mut lane = Lane::new("state_layout", Vec::new());
    lane.note = receipt.summary();
    lane
}

/// The state-root drift lane: a READING, never a sweeper. It reports the
/// top-level entries the inventory doc does not name and removes nothing;
/// anything with a deleter has it named in the doc's own rows. Gate and
/// check-in share the reading through [`crate::state_root_drift::drift_report`].
fn state_root_drift_lane(home: &AgentsHome) -> Lane {
    let mut lane = Lane::new("state_root_drift", Vec::new());
    match crate::state_root_drift::drift_report(&reclaim_state_root(home)) {
        Ok(rep) => {
            lane.note = if rep.entries.is_empty() {
                "clean: every top-level entry is documented".to_string()
            } else {
                let preview: Vec<String> = rep.entries.iter().take(5).cloned().collect();
                format!(
                    "{} undocumented top-level entr(y|ies): {}{}",
                    rep.entries.len(),
                    preview.join(", "),
                    if rep.entries.len() > 5 { ", ..." } else { "" }
                )
            };
        }
        Err(e) => lane.note = format!("reading failed: {e}"),
    }
    lane
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
/// simply reads as stale on the next tick. Goes straight to
/// [`run_reclaim_lanes`] with `include_cwd_root = false`: the daemon's launch
/// cwd is not a repo a person asked to sweep, it is wherever the process
/// happened to start (a test's checkout, an arbitrary shell) - the cwd
/// fallback in `cargo_build_dirs_roots` is a hand-run convenience only.
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
        let (_rc, summary) = run_reclaim_lanes(home, true, false, false);
        emit_sweep_event(home, &summary);
    }
}

/// What one sweep did, carried from the lane run to the daemon's event.
#[derive(Debug, Default)]
pub(crate) struct ReclaimSummary {
    pub applied: bool,
    pub total_bytes: u64,
    pub lane_count: usize,
    /// The lane with the largest reading, so one event line answers "what moved".
    pub top_lane: Option<String>,
    pub top_lane_bytes: u64,
}

/// One event per sweep. The receipt file holds the per-lane detail; the event
/// proves the sweep ran and names the biggest reading, so a quiet machine is
/// never mistaken for a sweep that did not run.
fn emit_sweep_event(home: &AgentsHome, summary: &ReclaimSummary) {
    let journal = crate::loop_runtime::Journal::new_raw(
        home.events_jsonl(),
        crate::daemon::global_events_path(home),
    );
    let _ = journal.append(
        "reclaim_sweep",
        json!({
            "applied": summary.applied,
            "total_bytes": summary.total_bytes,
            "lane_count": summary.lane_count,
            "top_lane": summary.top_lane,
            "top_lane_bytes": summary.top_lane_bytes,
        }),
    );
}

pub fn run_reclaim(args: &[String], home: &AgentsHome) -> i32 {
    // Leading subcommands. `remove-for <tree> [--json]` is the best-effort
    // tree-removal reclaim; it never errors. `cargo-build-dirs [--apply]`
    // runs only the build-base lane for the cwd's canonical repo.
    match args.first().map(String::as_str) {
        Some("remove-for") => {
            let Some(tree) = args.get(1) else {
                eprintln!("fno doctor reclaim remove-for: usage: remove-for <tree> [--json]");
                return 2;
            };
            let json = args.iter().skip(2).any(|a| crate::json_output::is_flag(a));
            let removed = crate::cargo_build_dirs::remove_for(Path::new(tree));
            if json {
                println!("{{\"removed\": {removed}}}");
            } else {
                println!("removed {removed} build dir(s)");
            }
            return 0;
        }
        // `occupancy-login`: the idle login shell verdict (R6). An argument of
        // the reclaim action, never a new action (law d-fe66560a); the
        // occupancy bridge shells here before the Python classifier.
        Some("occupancy-login") => {
            return crate::occupancy_login::run(&args[1..]);
        }
        Some("cargo-build-dirs") => {
            let apply = args.iter().skip(1).any(|a| a == "--apply");
            let cwd = std::env::current_dir().unwrap_or_default();
            let root = canonical_repo_root(&cwd).unwrap_or(cwd);
            crate::cargo_build_dirs::sweep(&root, apply, SystemTime::now());
            crate::cargo_build_dirs::reclaim_idle_trees(&root, apply, SystemTime::now());
            return 0;
        }
        _ => {}
    }

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
    // A hand run: the cwd is wherever the person typing this ran it from, so
    // the cargo-build-dirs lane sweeps that repo too even if unregistered.
    // The summary rides the daemon path only; a hand run's record is the
    // receipt it prints.
    run_reclaim_lanes(home, apply, verbose, true).0
}

/// The full lane set (every `run_reclaim` lane but the leading subcommands),
/// shared by the hand-run CLI path and the daemon's daily tick. Only
/// `include_cwd_root` differs between the two callers - see
/// [`cargo_build_dirs_roots`]. Returns the exit code plus the summary the
/// daemon's sweep event carries.
fn run_reclaim_lanes(
    home: &AgentsHome,
    apply: bool,
    verbose: bool,
    include_cwd_root: bool,
) -> (i32, ReclaimSummary) {
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
    let (mut codex_quarantines, codex_lock) = codex_cache_quarantines();
    codex_quarantines.bytes = codex_quarantines.paths.iter().map(|p| tree_bytes(p)).sum();
    if apply {
        for path in &codex_quarantines.paths {
            let _ = std::fs::remove_dir_all(path);
        }
    }
    lanes.push(codex_quarantines);
    drop(codex_lock);
    lanes.push(cargo_build_dirs_lane(home, apply, include_cwd_root));
    lanes.push(claude_config_tmp_lane(apply));
    lanes.push(backups_retention_lane(home, apply));
    lanes.push(state_layout_lane(home, apply));
    lanes.push(state_root_drift_lane(home));
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
    let every = lanes.iter().chain(std::iter::once(&uv)).collect::<Vec<_>>();
    let top = every
        .iter()
        .max_by_key(|lane| lane.bytes)
        .map(|lane| (lane.name.to_string(), lane.bytes));
    let summary = ReclaimSummary {
        applied: apply,
        total_bytes: every.iter().map(|lane| lane.bytes).sum(),
        lane_count: every.len(),
        top_lane: top.as_ref().map(|(name, _)| name.clone()),
        top_lane_bytes: top.map_or(0, |(_, bytes)| bytes),
    };
    (0, summary)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Serializes the tests that repoint the process-global env: a concurrent
    /// reader can catch the root mid-flip and sweep the REAL temp dir (the
    /// same ENV_LOCK shape org_board's HOME_LOCK uses). Shared with
    /// cargo_build_dirs' tests, which mutate the same vars.
    pub(crate) static ENV_LOCK: std::sync::LazyLock<&'static std::sync::Mutex<()>> =
        std::sync::LazyLock::new(crate::claims::test_env_lock);

    fn temp_lane_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fno-reclaim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn install_fake_uv(root: &Path) {
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        crate::write_exec_stub(
            &bin,
            "uv",
            "#!/bin/sh\nif [ \"$1\" = cache ] && [ \"$2\" = dir ]; then printf '/dev/null\\n'; fi\nexit 0\n",
        );
        let old = std::env::var_os("PATH");
        let paths = std::iter::once(bin)
            .chain(std::env::split_paths(old.as_deref().unwrap_or_default()))
            .collect::<Vec<_>>();
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
    }

    struct ReclaimTestEnv {
        cwd: PathBuf,
        vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl ReclaimTestEnv {
        fn new() -> Self {
            const VARS: &[&str] = &[
                "PATH",
                "HOME",
                "CODEX_HOME",
                "FNO_RECLAIM_TEMP_ROOT",
                "FNO_RECLAIM_STATE_ROOT",
                "FNO_RECLAIM_BACKUPS_DAYS",
                "FNO_RECLAIM_BACKUPS_AUDIT",
                "CARGO",
                "CBD_FNO",
                "CBD_FB",
                "FNO_CARGO_TARGETS_BASE",
            ];
            ReclaimTestEnv {
                cwd: std::env::current_dir().unwrap(),
                vars: VARS
                    .iter()
                    .map(|name| (*name, std::env::var_os(name)))
                    .collect(),
            }
        }
    }

    impl Drop for ReclaimTestEnv {
        fn drop(&mut self) {
            for (name, value) in self.vars.drain(..) {
                if let Some(value) = value {
                    std::env::set_var(name, value);
                } else {
                    std::env::remove_var(name);
                }
            }
            let _ = std::env::set_current_dir(&self.cwd);
        }
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
        let codex = state.join("codex");
        let fake_home = state.join("home");
        let process_env = ReclaimTestEnv::new();
        install_fake_uv(&root);
        std::env::set_current_dir(&root).unwrap();
        std::env::set_var("HOME", &fake_home);
        std::env::set_var("FNO_RECLAIM_TEMP_ROOT", &root);
        std::env::set_var("FNO_RECLAIM_STATE_ROOT", &state);
        std::env::set_var("CODEX_HOME", &codex);
        let home = AgentsHome::at(state.join("agents"));
        let rc = run_reclaim(&["--apply".to_string()], &home);
        drop(process_env);
        assert_eq!(rc, 0);
        assert!(!old.exists(), "the aged fake HOME is removed");
        assert!(fresh.exists(), "the fresh fake HOME survives");
        let receipt = std::fs::read_to_string(state.join("reclaim/last-run.json")).unwrap();
        assert!(receipt.contains("leaked_test_homes"));
        assert!(receipt.contains("codex_cache_quarantines"));
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

    /// AC6: the daily run's receipt carries the lane's reclaimed bytes and
    /// the orphan hash dir is gone. Registry seed + fake cargo + sandbox
    /// bases, per cargo_build_dirs' harness.
    #[test]
    fn apply_runs_the_cargo_build_dirs_lane_and_receipts_the_bytes() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_lane_root("cargo-lane");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("crates/fake")).unwrap();
        std::fs::write(
            repo.join("crates/fake/Cargo.toml"),
            "[package]\nname = 'fakepkg'\nversion = '0.1.0'\n",
        )
        .unwrap();
        let git_init = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(git_init.success(), "the sandbox repo must initialize");
        let fno_base = root.join("fno-base");
        let fb_base = root.join("fb-base");
        std::fs::create_dir_all(&fno_base).unwrap();
        std::fs::create_dir_all(&fb_base).unwrap();
        // Fake cargo keyed on CARGO_BUILD_BUILD_DIR, like the sibling suite.
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let script = crate::write_exec_stub(
            &root.join("bin"),
            "cargo",
            "#!/bin/sh\nif [ -n \"$CARGO_BUILD_BUILD_DIR\" ]; then\n\
             printf '{\"build_directory\":\"%s\",\"packages\":[{\"name\":\"fakepkg\"}]}\\n' \"$CBD_FNO\"\n\
             else\n\
             printf '{\"build_directory\":\"%s\",\"packages\":[{\"name\":\"fakepkg\"}]}\\n' \"$CBD_FB\"\n\
             fi\n",
        );

        // One orphan: fingerprinted, quiet 7h, no tree resolves to it.
        let orphan = fb_base.join("00").join("cafefe12");
        std::fs::create_dir_all(orphan.join("debug/deps")).unwrap();
        std::fs::create_dir_all(orphan.join("debug/.fingerprint")).unwrap();
        std::fs::write(orphan.join("CACHEDIR.TAG"), b"Signature: x\n").unwrap();
        std::fs::write(orphan.join("debug/deps/payload"), vec![0u8; 4096]).unwrap();
        std::fs::write(orphan.join("debug/.fingerprint/fakepkg-beef"), b"").unwrap();
        let expected = tree_bytes(&orphan);
        let old = SystemTime::now() - Duration::from_secs(7 * 3600);
        for entry in ["", "debug", "debug/deps", "debug/.fingerprint"] {
            let p = if entry.is_empty() {
                orphan.clone()
            } else {
                orphan.join(entry)
            };
            let file = std::fs::File::options().read(true).open(&p).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        for name in [
            "debug/deps/payload",
            "debug/.fingerprint/fakepkg-beef",
            "CACHEDIR.TAG",
        ] {
            let file = std::fs::File::options()
                .read(true)
                .open(orphan.join(name))
                .unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }

        let state = temp_lane_root("cargo-lane-state");
        let codex = state.join("codex");
        let fake_home = state.join("home");
        let process_env = ReclaimTestEnv::new();
        install_fake_uv(&root);
        std::env::set_current_dir(&repo).unwrap();
        std::env::set_var("HOME", &fake_home);
        std::env::set_var("FNO_RECLAIM_TEMP_ROOT", &root);
        std::env::set_var("FNO_RECLAIM_STATE_ROOT", &state);
        std::env::set_var("CODEX_HOME", &codex);
        std::env::set_var("CARGO", &script);
        std::env::set_var("CBD_FNO", fno_base.join("00").join("aaaa11"));
        std::env::set_var("CBD_FB", fb_base.join("00").join("bbbb22"));
        std::env::set_var("FNO_CARGO_TARGETS_BASE", &fno_base);
        // Seed the registry so the lane finds the sandbox repo normally.
        let home = AgentsHome::at(state.join("agents"));
        home.ensure_root().unwrap();
        std::fs::write(
            home.registry_json(),
            serde_json::to_string(&serde_json::json!({
                "entries": [{"name": "seed", "cwd": repo.display().to_string()}]
            }))
            .unwrap(),
        )
        .unwrap();

        let rc = run_reclaim(&["--apply".to_string()], &home);

        drop(process_env);
        assert_eq!(rc, 0);
        assert!(!orphan.exists(), "the orphan hash dir is reaped");
        let receipt: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(state.join("reclaim/last-run.json")).unwrap(),
        )
        .unwrap();
        let bytes = receipt["lanes"]["cargo_build_dirs"]["bytes"]
            .as_u64()
            .unwrap();
        assert_eq!(bytes, expected, "receipt carries the reclaimed bytes");
        assert!(
            receipt["lanes"]["cargo_build_dirs"]["note"]
                .as_str()
                .unwrap()
                .contains("cargo-build-dirs mode=apply"),
            "the note names each root's summary line"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    fn codex_home_fixture(tag: &str) -> PathBuf {
        let home = temp_lane_root(tag);
        std::fs::create_dir_all(home.join("footnote")).unwrap();
        std::fs::File::create(home.join("footnote/plugin-channel.lock")).unwrap();
        home
    }

    fn write_codex_marker_for_channel(home: &Path, channel: &str, marketplace: &str, source: &str) {
        std::fs::write(
            home.join("footnote/plugin-channel.json"),
            serde_json::to_vec(&json!({
                "channel": channel,
                "marketplace": marketplace,
                "source": source,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn write_codex_marker(home: &Path, marketplace: &str, source: &str) {
        write_codex_marker_for_channel(home, "dev", marketplace, source);
    }

    fn codex_live_cache(home: &Path, marketplace: &str) -> PathBuf {
        let live = home
            .join("plugins/cache")
            .join(marketplace)
            .join("fno/0.3.2");
        std::fs::create_dir_all(&live).unwrap();
        live
    }

    fn codex_backup(home: &Path, prefix: &str, suffix: &str) -> PathBuf {
        let backup = home.join("footnote").join(format!("{prefix}{suffix}"));
        std::fs::create_dir_all(backup.join("fno")).unwrap();
        std::fs::write(backup.join("fno/payload"), b"stale").unwrap();
        backup
    }

    fn run_codex_reclaim(codex: &Path, state: &Path, temp: &Path) -> String {
        let process_env = ReclaimTestEnv::new();
        install_fake_uv(temp);
        std::env::set_current_dir(temp).unwrap();
        std::env::set_var("HOME", temp);
        std::env::set_var("CODEX_HOME", codex);
        assert_eq!(
            PathBuf::from(std::env::var_os("CODEX_HOME").unwrap()),
            codex
        );
        std::env::set_var("FNO_RECLAIM_STATE_ROOT", state);
        std::env::set_var("FNO_RECLAIM_TEMP_ROOT", temp);
        let home = AgentsHome::at(state.join("agents"));
        let rc = run_reclaim(&["--apply".to_string()], &home);
        let receipt = std::fs::read_to_string(state.join("reclaim/last-run.json"));
        drop(process_env);
        assert_eq!(rc, 0);
        let receipt = receipt.unwrap();
        receipt
    }

    #[test]
    fn codex_quarantine_reclaims_valid_backups_and_receipts_bytes() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = temp_lane_root("codex-valid-temp");
        let state = temp_lane_root("codex-valid-state");
        let codex = codex_home_fixture("codex-valid-home");
        let live = codex_live_cache(&codex, "footnote");
        write_codex_marker(&codex, "footnote", "https://example.invalid/fno.git");
        let backup = codex_backup(&codex, ".footnote.", "0123456789abcdef0123456789abcdef");
        let dev_backup = codex_backup(&codex, ".footnote-dev.", "abcdef0123456789abcdef0123456789");
        let expected = tree_bytes(&backup) + tree_bytes(&dev_backup);

        let receipt = run_codex_reclaim(&codex, &state, &temp);

        assert!(!backup.exists());
        assert!(!dev_backup.exists());
        assert!(live.exists());
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(
            receipt["lanes"]["codex_cache_quarantines"]["bytes"],
            expected
        );
        let _ = std::fs::remove_dir_all(temp);
        let _ = std::fs::remove_dir_all(state);
        let _ = std::fs::remove_dir_all(codex);
    }

    #[test]
    fn codex_quarantine_keeps_unreadable_markers() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for (tag, marker) in [("missing", None), ("invalid", Some(b"not json".as_slice()))] {
            let temp = temp_lane_root(&format!("codex-{tag}-temp"));
            let state = temp_lane_root(&format!("codex-{tag}-state"));
            let codex = codex_home_fixture(&format!("codex-{tag}-home"));
            codex_live_cache(&codex, "footnote");
            let backup = codex_backup(&codex, ".footnote.", "0123456789abcdef0123456789abcdef");
            if let Some(marker) = marker {
                std::fs::write(codex.join("footnote/plugin-channel.json"), marker).unwrap();
            }
            let receipt = run_codex_reclaim(&codex, &state, &temp);
            assert!(backup.exists());
            assert!(receipt.contains("kept: live copy unreadable (plugin-channel.json)"));
            let _ = std::fs::remove_dir_all(temp);
            let _ = std::fs::remove_dir_all(state);
            let _ = std::fs::remove_dir_all(codex);
        }
    }

    #[test]
    fn codex_quarantine_keeps_backups_while_converge_holds_lock() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = temp_lane_root("codex-lock-temp");
        let state = temp_lane_root("codex-lock-state");
        let codex = codex_home_fixture("codex-lock-home");
        codex_live_cache(&codex, "footnote");
        write_codex_marker(&codex, "footnote", "https://example.invalid/fno.git");
        let backup = codex_backup(&codex, ".footnote.", "0123456789abcdef0123456789abcdef");
        let lock_path = codex.join("footnote/plugin-channel.lock");
        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(lock_path)
            .unwrap();
        lock_file.try_lock().unwrap();

        let receipt = run_codex_reclaim(&codex, &state, &temp);

        assert!(backup.exists());
        assert!(receipt.contains("kept: codex plugin converge in flight"));
        let _ = std::fs::remove_dir_all(temp);
        let _ = std::fs::remove_dir_all(state);
        let _ = std::fs::remove_dir_all(codex);
    }

    #[test]
    fn codex_quarantine_keeps_backups_with_rollback_failure() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for dangling_link in [false, true] {
            let tag = if dangling_link {
                "codex-failure-link"
            } else {
                "codex-failure-file"
            };
            let temp = temp_lane_root(&format!("{tag}-temp"));
            let state = temp_lane_root(&format!("{tag}-state"));
            let codex = codex_home_fixture(&format!("{tag}-home"));
            codex_live_cache(&codex, "footnote");
            write_codex_marker(&codex, "footnote", "https://example.invalid/fno.git");
            let receipt_path = codex.join("footnote/rollback-failure.json");
            if dangling_link {
                std::os::unix::fs::symlink("missing-receipt", &receipt_path).unwrap();
            } else {
                std::fs::write(&receipt_path, b"{} ").unwrap();
            }
            let backup = codex_backup(&codex, ".footnote.", "0123456789abcdef0123456789abcdef");

            let receipt = run_codex_reclaim(&codex, &state, &temp);

            assert!(backup.exists());
            assert!(receipt.contains("kept: rollback-failure.json present"));
            let _ = std::fs::remove_dir_all(temp);
            let _ = std::fs::remove_dir_all(state);
            let _ = std::fs::remove_dir_all(codex);
        }
    }

    #[test]
    fn codex_quarantine_spares_live_symlink_and_source_dir() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = temp_lane_root("codex-safety-temp");
        let state = temp_lane_root("codex-safety-state");
        let codex = codex_home_fixture("codex-safety-home");
        let live = codex_live_cache(&codex, "footnote");
        let source = codex_backup(&codex, ".footnote.", "abcdefabcdefabcdefabcdefabcdefab");
        write_codex_marker(&codex, "footnote", &source.display().to_string());
        let link = codex
            .join("footnote")
            .join(".footnote.0123456789abcdef0123456789abcdef");
        std::os::unix::fs::symlink(&live, &link).unwrap();
        let stale = codex_backup(&codex, ".footnote.", "fedcbafedcbafedcbafedcbafedcbafe");

        let _ = run_codex_reclaim(&codex, &state, &temp);

        assert!(live.exists());
        assert!(source.exists());
        assert!(link.exists());
        assert!(!stale.exists());
        let _ = std::fs::remove_dir_all(temp);
        let _ = std::fs::remove_dir_all(state);
        let _ = std::fs::remove_dir_all(codex);
    }

    #[test]
    fn codex_quarantine_handles_release_and_foreign_marketplaces() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = temp_lane_root("codex-release-temp");
        let state = temp_lane_root("codex-release-state");
        let codex = codex_home_fixture("codex-release-home");
        codex_live_cache(&codex, "footnote");
        write_codex_marker_for_channel(
            &codex,
            "release",
            "footnote",
            "https://github.com/example/fno.git",
        );
        let release_backup = codex_backup(&codex, ".footnote.", "0123456789abcdef0123456789abcdef");
        let _ = run_codex_reclaim(&codex, &state, &temp);
        assert!(!release_backup.exists());

        write_codex_marker_for_channel(
            &codex,
            "release",
            "other",
            "https://github.com/example/other.git",
        );
        let foreign_backup = codex_backup(&codex, ".footnote.", "abcdef0123456789abcdef0123456789");
        let receipt = run_codex_reclaim(&codex, &state, &temp);
        assert!(foreign_backup.exists());
        assert!(receipt.contains("kept: live copy unreadable (plugin-channel.json)"));
        let _ = std::fs::remove_dir_all(temp);
        let _ = std::fs::remove_dir_all(state);
        let _ = std::fs::remove_dir_all(codex);
    }

    #[test]
    fn plugin_cache_copies_resolves_codex_home_override() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _process_env = ReclaimTestEnv::new();
        let codex = codex_home_fixture("codex-cache-home");
        let target = codex.join("plugins/cache/footnote/fno/0.3.2/crates/fno-agents/target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("CACHEDIR.TAG"), b"Signature: x\n").unwrap();
        std::env::set_var("CODEX_HOME", &codex);
        let found = plugin_cache_copies();
        std::env::remove_var("CODEX_HOME");
        assert!(found.contains(&target));
        let _ = std::fs::remove_dir_all(codex);
    }

    #[test]
    fn plugin_cache_copies_ignores_missing_codex_cache() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _process_env = ReclaimTestEnv::new();
        let codex = codex_home_fixture("codex-empty-cache-home");
        std::env::set_var("CODEX_HOME", &codex);
        assert_eq!(
            PathBuf::from(std::env::var_os("CODEX_HOME").unwrap()),
            codex
        );
        let found = plugin_cache_copies();
        std::env::remove_var("CODEX_HOME");
        assert!(!found.iter().any(|path| path.starts_with(&codex)));
        let _ = std::fs::remove_dir_all(codex);
    }

    #[test]
    fn codex_quarantine_ignores_near_miss_names_and_source_trees() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = temp_lane_root("codex-near-miss-temp");
        let state = temp_lane_root("codex-near-miss-state");
        let codex = codex_home_fixture("codex-near-miss-home");
        codex_live_cache(&codex, "footnote");
        write_codex_marker(&codex, "footnote", "https://example.invalid/fno.git");
        let names = [
            ".footnote.ts.123.tmp",
            ".footnote.0123456789abcdef0123456789abcde",
            ".footnote.0123456789abcdef0123456789ABCDEF",
            "footnote.0123456789abcdef0123456789abcdef",
            ".footnote.0123456789abcdef0123456789abcdef",
        ];
        let mut survivors = Vec::new();
        for name in names {
            let path = codex.join("footnote").join(name);
            std::fs::create_dir_all(&path).unwrap();
            survivors.push(path);
        }
        let regular = codex.join("footnote/.footnote.abcdef0123456789abcdef0123456789");
        std::fs::write(&regular, b"not a directory").unwrap();
        survivors.push(regular);
        let source_tree = codex.join("footnote/src/fno/target");
        std::fs::create_dir_all(&source_tree).unwrap();
        std::fs::write(source_tree.join("CACHEDIR.TAG"), b"Signature: x\n").unwrap();

        let _ = run_codex_reclaim(&codex, &state, &temp);

        for survivor in survivors {
            assert!(
                survivor.exists(),
                "near-miss survived: {}",
                survivor.display()
            );
        }
        assert!(source_tree.exists());
        let _ = std::fs::remove_dir_all(temp);
        let _ = std::fs::remove_dir_all(state);
        let _ = std::fs::remove_dir_all(codex);
    }

    /// The incident this guards against: a test daemon's first idle tick ran
    /// `maybe_run_daily` with its cwd still inside the real checkout, and
    /// `cargo_build_dirs_lane` added that checkout as a sweep root the
    /// registry never named. With an empty registry the cwd fallback is the
    /// ONLY way a root can appear, so `include_cwd_root = false` (the
    /// daemon's own path) must come back empty every time, whatever the
    /// process's actual cwd is.
    #[test]
    fn cargo_build_dirs_roots_excludes_cwd_unless_asked() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = temp_lane_root("cargo-roots");
        let home = AgentsHome::at(state.join("agents"));
        home.ensure_root().unwrap();
        std::fs::write(
            home.registry_json(),
            serde_json::to_string(&serde_json::json!({"entries": []})).unwrap(),
        )
        .unwrap();

        let daemon_roots = cargo_build_dirs_roots(&home, false);
        let hand_roots = cargo_build_dirs_roots(&home, true);

        assert!(
            daemon_roots.is_empty(),
            "the daemon path must never add the cwd repo root: {daemon_roots:?}"
        );
        if let Some(this_repo) = canonical_repo_root(&std::env::current_dir().unwrap()) {
            assert!(
                hand_roots.contains(&this_repo.to_string_lossy().into_owned()),
                "a hand run still sweeps its own cwd repo: {hand_roots:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&state);
    }

    /// Unset retention is report-only: the pile is named in the note, and
    /// nothing is removed, even when `--apply` runs. The inventory doc holds
    /// migration and recovery stamps to the operator's yes.
    #[test]
    fn backups_retention_defaults_to_report_only() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = temp_lane_root("backups-report");
        let migration = state.join("backups/state-root-migration");
        std::fs::create_dir_all(migration.join("20260101T000000Z")).unwrap();
        std::fs::write(migration.join("20260101T000000Z/payload"), vec![0u8; 512]).unwrap();
        age(&migration.join("20260101T000000Z"), 40 * 24 * 60);

        std::env::set_var("FNO_RECLAIM_STATE_ROOT", &state);
        std::env::remove_var("FNO_RECLAIM_BACKUPS_DAYS");
        let home = AgentsHome::at(state.join("agents"));
        let lane = backups_retention_lane(&home, true);
        std::env::remove_var("FNO_RECLAIM_STATE_ROOT");

        assert!(lane.paths.is_empty(), "nothing is reaped without the yes");
        assert_eq!(lane.bytes, 0);
        assert!(
            lane.note.contains("report only"),
            "the note names the unset retention: {}",
            lane.note
        );
        assert!(
            migration.join("20260101T000000Z").exists(),
            "the aged stamp survives report-only"
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    /// Zero days is disabled, not "delete on the next sweep": both the env
    /// override and the config knob filter non-positive values to the
    /// report-only path.
    #[test]
    fn backups_retention_zero_days_reads_as_disabled() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = temp_lane_root("backups-zero");
        let migration = state.join("backups/state-root-migration");
        std::fs::create_dir_all(migration.join("20260101T000000Z")).unwrap();
        age(&migration.join("20260101T000000Z"), 40 * 24 * 60);

        std::env::set_var("FNO_RECLAIM_STATE_ROOT", &state);
        std::env::set_var("FNO_RECLAIM_BACKUPS_DAYS", "0");
        let home = AgentsHome::at(state.join("agents"));
        let lane = backups_retention_lane(&home, true);
        std::env::remove_var("FNO_RECLAIM_STATE_ROOT");
        std::env::remove_var("FNO_RECLAIM_BACKUPS_DAYS");

        assert!(lane.paths.is_empty(), "zero days deletes nothing");
        assert!(lane.note.contains("report only"), "{}", lane.note);
        assert!(migration.join("20260101T000000Z").exists());
        let _ = std::fs::remove_dir_all(&state);
    }

    /// A retention without a zero-missing audit verdict deletes nothing:
    /// the inventory doc requires the recovery receipt, and a stamp can
    /// hold the only copy of a parked conflict.
    #[test]
    fn backups_retention_without_an_audit_verdict_is_report_only() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = temp_lane_root("backups-noaudit");
        let migration = state.join("backups/state-root-migration");
        std::fs::create_dir_all(migration.join("20260101T000000Z")).unwrap();
        age(&migration.join("20260101T000000Z"), 40 * 24 * 60);

        std::env::set_var("FNO_RECLAIM_STATE_ROOT", &state);
        std::env::set_var("FNO_RECLAIM_BACKUPS_DAYS", "7");
        std::env::remove_var("FNO_RECLAIM_BACKUPS_AUDIT");
        let home = AgentsHome::at(state.join("agents"));
        let lane = backups_retention_lane(&home, true);
        std::env::remove_var("FNO_RECLAIM_STATE_ROOT");
        std::env::remove_var("FNO_RECLAIM_BACKUPS_DAYS");

        assert!(lane.paths.is_empty(), "no audit, no deletion");
        assert!(
            lane.note.contains("zero-missing audit"),
            "the note names the missing gate: {}",
            lane.note
        );
        assert!(migration.join("20260101T000000Z").exists());
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The keep floor is what bounds a hot pile: a migration that parks a
    /// full copy every sweep never lets anything age past retention, so the
    /// newest `BACKUP_KEEP_NEWEST` survive and the rest go, but only stamps
    /// the zero-missing audit verdict predates. Rotation-managed siblings
    /// keep their own keep-N and are never touched here.
    #[test]
    fn backups_retention_keeps_the_newest_and_reaps_stale_stamps() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = temp_lane_root("backups-retention");
        let migration = state.join("backups/state-root-migration");
        std::fs::create_dir_all(&migration).unwrap();
        // Six aged stamps plus one fresh one, each a full day apart: distinct
        // mtimes make the newest-first sort total. The fresh dir ranks first,
        // so the keep floor protects only four of the aged stamps and the
        // oldest two go.
        for i in 0..6 {
            let stamp = migration.join(format!("2026010{i}T000000Z"));
            std::fs::create_dir_all(&stamp).unwrap();
            std::fs::write(stamp.join("payload"), vec![0u8; 512]).unwrap();
            age(&stamp, (40 + i) * 24 * 60);
        }
        let doomed = migration.join("20260104T000000Z");
        let oldest = migration.join("20260105T000000Z");
        let expected = tree_bytes(&doomed) + tree_bytes(&oldest);
        let fresh = migration.join("20260201T000000Z");
        std::fs::create_dir_all(&fresh).unwrap();
        // A recovery snapshot: in the report scope, never in the prune scope.
        let snapshot = state.join("backups/state-recovery/preparation-20260101T000000Z");
        std::fs::create_dir_all(&snapshot).unwrap();
        age(&snapshot, 60 * 24 * 60);
        let rotated = state.join("backups/graph.db.20260101T000000000000");
        std::fs::create_dir_all(&rotated).unwrap();
        age(&rotated, 60 * 24 * 60);
        // The operator's audit verdict, written after every stamp existed.
        let audit_path = state.join("audit-verdict.json");
        std::fs::write(&audit_path, r#"{"zero_missing": true}"#).unwrap();

        std::env::set_var("FNO_RECLAIM_STATE_ROOT", &state);
        std::env::set_var("FNO_RECLAIM_BACKUPS_DAYS", "7");
        std::env::set_var("FNO_RECLAIM_BACKUPS_AUDIT", &audit_path);
        let home = AgentsHome::at(state.join("agents"));
        let lane = backups_retention_lane(&home, true);
        std::env::remove_var("FNO_RECLAIM_STATE_ROOT");
        std::env::remove_var("FNO_RECLAIM_BACKUPS_DAYS");
        std::env::remove_var("FNO_RECLAIM_BACKUPS_AUDIT");

        assert_eq!(
            lane.paths,
            vec![doomed.clone(), oldest.clone()],
            "everything past the keep floor goes, oldest last"
        );
        assert_eq!(
            lane.bytes, expected,
            "the receipt carries the reaped stamps' bytes"
        );
        assert!(lane.note.contains("newest 5"));
        for i in 0..4 {
            assert!(
                migration.join(format!("2026010{i}T000000Z")).exists(),
                "keep floor holds stamp {i}"
            );
        }
        assert!(fresh.exists(), "a fresh stamp is never aged out");
        assert!(snapshot.exists(), "recovery snapshots are never swept");
        assert!(
            rotated.exists(),
            "rotation-managed backups keep their own keep-N"
        );
        assert!(!doomed.exists());
        assert!(!oldest.exists());
        let _ = std::fs::remove_dir_all(&state);
    }
}
