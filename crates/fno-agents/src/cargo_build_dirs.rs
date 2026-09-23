//! Which cargo build-base hash dirs may go, for BOTH bases whatever the
//! caller's env. Cargo writes intermediates at `<base>/<h2>/<hash>` under
//! `build.build-dir`; the tracked `.cargo/config.toml` template names
//! `{cargo-cache-home}/build/{workspace-path-hash}`, so any cargo run without
//! `CARGO_BUILD_BUILD_DIR` lands in `~/.cargo/build` and every env-dependent
//! resolver only ever saw the base its own env named. This module answers
//! env-independently: it resolves every workspace manifest both ways and
//! classifies each tagged hash dir through four lanes (fresh, orphan, age,
//! cap), guarded by an exclusive `flock` on each profile's `.cargo-lock`
//! before any delete. Over the cap the lane reaps owned rows least recently
//! used first, fresh rows included, down to a short `CAP_MIN_QUIET_SECS`
//! floor it never crosses; a live cargo holding that lock keeps a row, and
//! so does a row whose tree a currently running cargo process's cwd falls
//! under - the lock alone misses the gap between two test binaries in one
//! `cargo test` run, where nothing is locked or open yet the run still needs
//! the dir. Rows are matched by `CACHEDIR.TAG`, base, and member fingerprint
//! - never by name.
//!
//! Surfaced as the `cargo_build_dirs` lane of `fno doctor reclaim`, the
//! `fno-agents reclaim cargo-build-dirs` / `remove-for` subcommands, and the
//! in-process `remove_for` the merge reaper calls.
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// A build quieter than this is live: it keeps a row out of the orphan and
/// age lanes and orders the cap lane (quieter sorts last), but never vetoes
/// a cap reap.
const FRESH_SECS: u64 = 6 * 3600;
/// An owned build quiet for 3 days is reaped by the age lane.
const AGE_SECS: u64 = 3 * 24 * 3600;
/// The cap lane's own floor, far short of `FRESH_SECS` so a busy fleet's rows
/// still clear it: a row touched within this window never goes under cap
/// pressure, live-cargo detection or not. A held `.cargo-lock` and a live
/// process's cwd are the precise signals; this is the backstop for when
/// neither fires - `lsof` absent, an odd process name - a lock gap must
/// never read as idle.
const CAP_MIN_QUIET_SECS: u64 = 15 * 60;
/// The cap lane never defends more than this, and no more than half the free
/// space (whichever is smaller) - the same shape as the bash sweep's
/// `min(--cap-bytes, free-share-pct% of free)`.
const CAP_CEILING_BYTES: u64 = 24 * 1024 * 1024 * 1024;
const CAP_FREE_SHARE_PCT: u64 = 50;

const CACHEDIR_TAG: &str = "CACHEDIR.TAG";

// --- base resolution ---------------------------------------------------------

/// The fno build base: `FNO_CARGO_TARGETS_BASE`, else
/// `paths.cargo_targets_base`, else the state root's `cargo-build`. Same
/// precedence as Python `fno.paths.cargo_build_dir_value`; the
/// `FNO_RECLAIM_STATE_ROOT` seam keeps the reclaim tests pointing at sandboxes.
pub fn fno_build_base(root: &Path) -> PathBuf {
    if let Some(raw) = std::env::var_os("FNO_CARGO_TARGETS_BASE") {
        return expand_home(&PathBuf::from(raw));
    }
    if let Some(raw) = crate::agents_config::config_lookup(root, &["paths", "cargo_targets_base"])
        .and_then(|v| v.as_str().map(str::to_string))
    {
        return expand_home(&PathBuf::from(raw));
    }
    let state = std::env::var_os("FNO_RECLAIM_STATE_ROOT")
        .map(PathBuf::from)
        .or_else(|| crate::agents_config::state_dir(root))
        .unwrap_or_else(|| home().unwrap_or_else(|| PathBuf::from("/.fno")));
    state.join("cargo-build")
}

fn expand_home(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~") {
        if let Some(home) = home() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Cargo wherever it lives. The daemon's PATH carries no cargo, so the PATH
/// scan alone would strand every merge reap: `$CARGO_HOME/bin/cargo` and the
/// bare `~/.cargo/bin/cargo` installs are probed after it.
pub(crate) fn cargo_bin() -> Option<PathBuf> {
    if let Some(c) = std::env::var_os("CARGO") {
        if !c.is_empty() {
            return Some(PathBuf::from(c));
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let c = dir.join("cargo");
            if c.is_file() {
                return Some(c);
            }
        }
    }
    for base in [
        std::env::var_os("CARGO_HOME").map(PathBuf::from),
        home().map(|h| h.join(".cargo")),
    ] {
        if let Some(base) = base {
            let c = base.join("bin").join("cargo");
            if c.is_file() {
                return Some(c);
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let c = dir.join("cargo");
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

/// Cargo's build and in-checkout target directories plus package names for one
/// manifest, with
/// `CARGO_BUILD_BUILD_DIR` forced to `<base>/{workspace-path-hash}` (`Some`)
/// or removed (`None`) for the call, so both bases answer from the same env.
/// `Err` carries the manifest path: the receipt names what could not answer.
pub(crate) struct Resolved {
    build_dir: PathBuf,
    target_dir: Option<PathBuf>,
    names: Vec<String>,
}

pub(crate) fn resolve(manifest: &Path, fno_base: Option<&Path>) -> Result<Resolved, String> {
    let bin = cargo_bin().ok_or_else(|| manifest.display().to_string())?;
    let mut cmd = Command::new(bin);
    cmd.arg("metadata")
        .args(["--format-version", "1", "--no-deps", "--manifest-path"])
        .arg(manifest);
    match fno_base {
        Some(base) => {
            cmd.env(
                "CARGO_BUILD_BUILD_DIR",
                format!("{}/{{workspace-path-hash}}", base.display()),
            );
        }
        None => {
            cmd.env_remove("CARGO_BUILD_BUILD_DIR");
        }
    }
    let out = cmd.output().map_err(|_| manifest.display().to_string())?;
    if !out.status.success() {
        return Err(manifest.display().to_string());
    }
    let value: Value =
        serde_json::from_slice(&out.stdout).map_err(|_| manifest.display().to_string())?;
    let build_dir = PathBuf::from(
        value
            .get("build_directory")
            .and_then(Value::as_str)
            .ok_or_else(|| manifest.display().to_string())?,
    );
    let target_dir = value
        .get("target_directory")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let names = value
        .get("packages")
        .and_then(Value::as_array)
        .map(|pkgs| {
            pkgs.iter()
                .filter_map(|p| p.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(Resolved {
        build_dir,
        target_dir,
        names,
    })
}

// --- tree resolution ---------------------------------------------------------

/// This repo's registered trees, canonical root first when git cannot answer
/// (a fixture repo in tests, a checkout mid-teardown).
fn registered_trees(root: &Path) -> Vec<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "list", "--porcelain"])
        .output();
    let trees: Vec<PathBuf> = match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    };
    if trees.is_empty() {
        vec![root.to_path_buf()]
    } else {
        trees
    }
}

pub(crate) fn workspace_manifests(tree: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(tree.join("crates")) {
        for entry in entries.flatten() {
            let manifest = entry.path().join("Cargo.toml");
            if manifest.is_file() {
                found.push(manifest);
            }
        }
    }
    found.sort();
    found
}

struct TreeAnswer {
    dirs: Vec<PathBuf>,
    names: BTreeSet<String>,
}

/// Both resolutions of every workspace manifest of one tree. A manifest that
/// cannot answer fails the tree: the sweep then disables only the orphan lane
/// (a blind orphan reap is the one mistake this lane cannot undo).
fn answer_tree(tree: &Path, fno_base: &Path) -> Result<TreeAnswer, String> {
    let mut dirs = BTreeSet::new();
    let mut names = BTreeSet::new();
    for manifest in workspace_manifests(tree) {
        for base in [Some(fno_base), None] {
            let resolved = resolve(&manifest, base)?;
            dirs.insert(resolved.build_dir);
            names.extend(resolved.names);
        }
    }
    Ok(TreeAnswer {
        dirs: dirs.into_iter().collect(),
        names,
    })
}

/// Read-only form: every build dir this tree's manifests resolve to, both
/// ways. Used at tree-removal time, where the manifests can still answer.
pub(crate) fn list_for(tree: &Path) -> Result<Vec<PathBuf>, String> {
    let fno_base = fno_build_base(&repo_root_for(tree));
    Ok(answer_tree(tree, &fno_base)?.dirs)
}

fn tracked_files(tree: &Path, dir: &Path) -> bool {
    let Ok(relative) = dir.strip_prefix(tree) else {
        return true;
    };
    let Ok(output) = Command::new("git")
        .current_dir(tree)
        .args(["ls-files", "-z", "--"])
        .arg(relative)
        .output()
    else {
        return true;
    };
    !output.status.success() || !output.stdout.is_empty()
}

/// Cargo's identity answer for a member's target directory, filtered before
/// any removal: it must live below that member, carry Cargo's cache marker,
/// and contain no tracked file. A manifest that cannot answer refuses the
/// whole tree; an unsafe answer simply is not a reclaim candidate.
fn tree_target_dirs(tree: &Path) -> Result<Vec<PathBuf>, String> {
    let mut dirs = BTreeSet::new();
    for manifest in workspace_manifests(tree) {
        let resolved = resolve(&manifest, None)?;
        let Some(target) = resolved.target_dir else {
            continue;
        };
        let target = phys(&target);
        let Some(member) = manifest.parent().map(phys) else {
            continue;
        };
        if !target.starts_with(&member)
            || !target.join(CACHEDIR_TAG).is_file()
            || tracked_files(tree, &target)
        {
            continue;
        }
        dirs.insert(target);
    }
    Ok(dirs.into_iter().collect())
}

fn repo_root_for(tree: &Path) -> PathBuf {
    crate::paths::canonical_repo_root(tree).unwrap_or_else(|| tree.to_path_buf())
}

// --- bases -------------------------------------------------------------------

/// The bases a sweep may delete under. The fno base always; the fallback base
/// (where env-unset cargo runs land) only when the env-free resolution of the
/// canonical checkout's first manifest has the hash-dir shape, sits outside
/// every registered tree, and is none of `/`, `$HOME`, or the fno base.
pub(crate) fn managed_bases(root: &Path, trees: &[PathBuf]) -> Vec<PathBuf> {
    let fno_base = fno_build_base(root);
    let mut bases = vec![fno_base.clone()];
    let reject = |bases: &mut Vec<PathBuf>| {
        bases.truncate(1);
        bases.to_vec()
    };
    let Some(manifest) = workspace_manifests(root).into_iter().next() else {
        return bases;
    };
    let Ok(resolved) = resolve(&manifest, None) else {
        return reject(&mut bases);
    };
    let dir = resolved.build_dir;
    let Some(base) = dir.parent().and_then(Path::parent) else {
        return reject(&mut bases);
    };
    if !hash_dir_shape(&dir) {
        return reject(&mut bases);
    }
    if base == Path::new("/")
        || base
            .components()
            .filter(|c| !matches!(c, std::path::Component::RootDir))
            .count()
            < 3
    {
        return reject(&mut bases);
    }
    if let Some(home) = home() {
        if base == home {
            return reject(&mut bases);
        }
    }
    if base == fno_base {
        return reject(&mut bases);
    }
    if trees.iter().any(|tree| phys(base).starts_with(phys(tree))) {
        return reject(&mut bases);
    }
    bases.push(base.to_path_buf());
    bases
}

/// `<2-hex shard>/<hex hash>`: the shape the build-dir template produces.
fn hash_dir_shape(dir: &Path) -> bool {
    fn hex(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
    }
    let Some(hash) = dir.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let Some(shard) = dir
        .parent()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
    else {
        return false;
    };
    shard.len() == 2 && hex(shard) && hex(hash)
}

fn phys(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn under_any(path: &Path, bases: &[PathBuf]) -> bool {
    let p = phys(path);
    bases.iter().any(|b| p.starts_with(phys(b)))
}

// --- rows --------------------------------------------------------------------

/// Newest mtime within two levels of `dir` (the hash dir, each profile child,
/// each grandchild): mtime tracks birth AND use, so any build activity reads
/// fresh.
fn quiet_of(dir: &Path, now: SystemTime) -> Duration {
    fn newest(dir: &Path, depth: u32, acc: &mut SystemTime) {
        let Ok(meta) = std::fs::symlink_metadata(dir) else {
            return;
        };
        if let Ok(m) = meta.modified() {
            if m > *acc {
                *acc = m;
            }
        }
        if depth == 0 {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                newest(&entry.path(), depth - 1, acc);
            }
        }
    }
    let mut acc = SystemTime::UNIX_EPOCH;
    newest(dir, 2, &mut acc);
    now.duration_since(acc).unwrap_or(Duration::ZERO)
}

/// Tagged hash dirs at `<base>/<shard>/<hash>` (never followed, never
/// name-matched), plus the shard dirs already empty.
fn inventory(base: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut rows = Vec::new();
    let mut empty_shards = Vec::new();
    let Ok(shards) = std::fs::read_dir(base) else {
        return (rows, empty_shards);
    };
    for shard in shards.flatten() {
        let shard_path = shard.path();
        if !std::fs::symlink_metadata(&shard_path)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(hashes) = std::fs::read_dir(&shard_path) else {
            empty_shards.push(shard_path);
            continue;
        };
        let mut entries = 0usize;
        for hash in hashes.flatten() {
            entries += 1;
            let path = hash.path();
            let tagged = std::fs::symlink_metadata(&path)
                .map(|m| m.is_dir())
                .unwrap_or(false)
                && path.join(CACHEDIR_TAG).is_file();
            if tagged {
                rows.push(path);
            }
        }
        if entries == 0 {
            empty_shards.push(shard_path);
        }
    }
    rows.sort();
    (rows, empty_shards)
}

/// Does the dir carry a fingerprint of a package this repo's workspaces build?
fn has_membership(dir: &Path, names: &BTreeSet<String>) -> bool {
    if names.is_empty() {
        return false;
    }
    let Ok(profiles) = std::fs::read_dir(dir) else {
        return false;
    };
    for profile in profiles.flatten() {
        let Ok(fps) = std::fs::read_dir(profile.path().join(".fingerprint")) else {
            continue;
        };
        for fp in fps.flatten() {
            if let Some(name) = fp.file_name().to_str().and_then(|f| {
                f.rsplit_once('-').map(|(stem, hex)| {
                    let _ = hex;
                    stem.to_string()
                })
            }) {
                if names.contains(&name) {
                    return true;
                }
            }
        }
    }
    false
}

/// The live-build test, shared by `guard_remove` and the cap lane's dry-run
/// probe: `flock(LOCK_EX | LOCK_NB)` on every profile's `.cargo-lock`. Any
/// held lock returns `Err("build-in-progress")`; otherwise the open lock
/// files come back, and the locks last while they live.
fn take_locks(dir: &Path) -> Result<Vec<std::fs::File>, &'static str> {
    let mut locks = Vec::new();
    if let Ok(profiles) = std::fs::read_dir(dir) {
        for profile in profiles.flatten() {
            if let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .open(profile.path().join(".cargo-lock"))
            {
                locks.push(file);
            }
        }
    }
    for lock in &locks {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err("build-in-progress");
        }
    }
    Ok(locks)
}

/// Last gate before a delete. `recheck_quiet` (the orphan and age lanes:
/// classification and deletion are separate walks, and a row touched in
/// between is being written) refuses rows quiet under the fresh window; the
/// cap lane and the tree-removal path (`remove_for`) skip it - there the
/// flock alone is the build-in-progress test. Lock fds stay open until the
/// removal returns so the guard cannot be released underneath it.
fn guard_remove(dir: &Path, now: SystemTime, recheck_quiet: bool) -> Result<(), &'static str> {
    if recheck_quiet && quiet_of(dir, now) < Duration::from_secs(FRESH_SECS) {
        return Err("build-in-progress");
    }
    let _locks = take_locks(dir)?;
    std::fs::remove_dir_all(dir).map_err(|_| "delete-failed")
}

fn remove_empty_shard(dir: &Path) -> bool {
    if let Some(shard) = dir.parent() {
        return std::fs::remove_dir(shard).is_ok();
    }
    false
}

/// Every currently running process matching `command`'s cwd, via one `lsof`
/// read (the same tool `pane_stop.rs` already relies on; works on macOS and
/// Linux). A missing command filter reads all process cwds for the merge-tree
/// guard; the cargo lane keeps its narrower filter.
///
/// `FNO_TEST_LIVE_CARGO_CWDS` (colon-separated paths) substitutes for the
/// real read in tests: a process whose kernel-reported name is genuinely
/// `cargo` can't be spawned to order (the name comes from the executed
/// file's own path, not argv0 or a script's shebang target), so the seam is
/// what lets a test drive the tree-to-shard mapping below deterministically.
///
/// `Err` only when `lsof` itself could not be run (missing binary): a normal
/// "no matching process right now" read is `Ok(vec![])`, never an error.
pub(crate) fn live_cwds(command: Option<&str>) -> Result<Vec<PathBuf>, ()> {
    if command.is_some() {
        if let Ok(raw) = std::env::var("FNO_TEST_LIVE_CARGO_CWDS") {
            return Ok(raw
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect());
        }
    }
    let mut cmd = Command::new("lsof");
    cmd.args(["-a", "-d", "cwd"]);
    if let Some(command) = command {
        cmd.args(["-c", command]);
    }
    cmd.arg("-Fn");
    let Ok(output) = cmd.output() else {
        return Err(());
    };
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .map(PathBuf::from)
        .collect())
}

/// Every hash dir a live cargo command might still need, plus whether the
/// read can be trusted. `cargo test` drops its `.cargo-lock` once compiling
/// ends, so the flock reads free for the whole run phase - the gap between
/// one test binary exiting and the next one starting has no lock and no open
/// file under the dir the cap lane is about to judge. The live process's cwd
/// is the only signal left: whichever registered tree it falls under has a
/// cargo command in flight, so every dir that tree's manifests resolve to
/// stays off the cap lane's table. A cwd can sit under more than one
/// registered tree - a worktree nested inside its own checkout - so the
/// LONGEST matching tree owns it, never the first one
/// `git worktree list` happens to print (that would always be the main
/// checkout). `Err` when `lsof` failed to run or a live cwd's own tree
/// cannot answer its manifests: either way the returned set may be missing
/// entries, so the caller must fail closed rather than trust an empty one.
fn live_shards(trees: &[PathBuf], fno_base: &Path) -> Result<BTreeSet<PathBuf>, ()> {
    let phys_trees: Vec<PathBuf> = trees.iter().map(|t| phys(t)).collect();
    let mut shards = BTreeSet::new();
    for cwd in live_cwds(Some("cargo"))? {
        let cwd = phys(&cwd);
        let Some(tree) = owning_tree(&cwd, &phys_trees) else {
            continue;
        };
        let answer = answer_tree(tree, fno_base).map_err(|_| ())?;
        shards.extend(answer.dirs.iter().map(|d| phys(d)));
    }
    Ok(shards)
}

/// The registered tree (already phys) that `path` falls under: the LONGEST
/// match wins, so a worktree nested inside its own checkout owns its own
/// rows, never the outer checkout `git worktree list` happens to print
/// first. Callers phys their tree list once, never per element.
fn owning_tree<'a>(path: &Path, phys_trees: &'a [PathBuf]) -> Option<&'a PathBuf> {
    let p = phys(path);
    phys_trees
        .iter()
        .filter(|t| p.starts_with(*t))
        .max_by_key(|t| t.as_os_str().len())
}

// --- free space --------------------------------------------------------------

fn free_bytes(path: &Path) -> Option<u64> {
    if let Some(v) = std::env::var_os("FNO_CARGO_FREE_BYTES") {
        return v.to_str().and_then(|s| s.parse().ok());
    }
    let out = Command::new("df").arg("-k").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let row = stdout.lines().nth(1)?;
    let kib: u64 = row.split_whitespace().nth(3)?.parse().ok()?;
    Some(kib * 1024)
}

fn effective_cap_bytes(root: &Path) -> u64 {
    match free_bytes(root) {
        Some(free) => CAP_CEILING_BYTES
            .min(free * CAP_FREE_SHARE_PCT / 100)
            .max(1),
        None => CAP_CEILING_BYTES,
    }
}

// --- the sweep ---------------------------------------------------------------

/// One run's reading: the counters behind the `cargo-build-dirs` summary line.
#[derive(Debug, Default)]
pub struct SweepReport {
    pub bases: usize,
    pub rows: usize,
    pub trees_resolved: usize,
    pub orphans: usize,
    pub reaped: usize,
    pub reclaimed_bytes: u64,
    /// Bytes this run reclaimed (`apply`) or would reclaim (dry run).
    pub projected_bytes: u64,
    pub after_bytes: u64,
    pub effective_cap_bytes: u64,
    /// Bytes read across both bases before any lane ran.
    pub before_bytes: u64,
    /// `before_bytes` started over the effective cap.
    pub cap_exceeded: bool,
    /// Why bytes are still over the cap at the end: kept reason -> row count.
    pub cap_held: BTreeMap<&'static str, usize>,
    /// `None` = the orphan lane ran; `Some(manifest)` = disabled, named.
    pub orphan_lane: Option<String>,
    pub shards_removed: usize,
    /// The per-row lines, in print order, for the reclaim receipt's note.
    pub lines: Vec<String>,
}

struct Row {
    path: PathBuf,
    bytes: u64,
    quiet: Duration,
    under_fno: bool,
    membership: bool,
    /// The registered tree whose manifests resolve to this dir; `None` when
    /// no tree did (what the orphan lane acts on).
    owner: Option<PathBuf>,
}

/// Registry rows whose cwd falls under one tree.
#[derive(Debug, Default)]
struct Holders {
    nodes: BTreeSet<String>,
    sessions: BTreeSet<String>,
}

/// Pure join: every registry entry's cwd picks its tree by `owning_tree`;
/// the entry's node (when set) lands in `nodes` and, unless the row is
/// terminal, its name lands in `sessions` - `session=` must mean a session
/// that may still be using the tree.
fn session_join(
    trees: &[PathBuf],
    registry: &crate::state::Registry,
) -> BTreeMap<PathBuf, Holders> {
    let phys_trees: Vec<PathBuf> = trees.iter().map(|t| phys(t)).collect();
    let mut join: BTreeMap<PathBuf, Holders> = BTreeMap::new();
    for entry in &registry.entries {
        let Some(tree) = owning_tree(Path::new(&entry.cwd), &phys_trees) else {
            continue;
        };
        let holders = join.entry(tree.clone()).or_default();
        if let Some(node) = &entry.node {
            holders.nodes.insert(node.clone());
        }
        if !matches!(
            entry.status,
            crate::AgentStatus::Failed
                | crate::AgentStatus::Exited
                | crate::AgentStatus::PermanentDead
        ) {
            holders.sessions.insert(entry.name.clone());
        }
    }
    join
}

/// The registry join a row line renders from. `Unread` when the registry
/// could not be read at all: `none` is what the orphan lane and a person
/// act on, so a read failure must never wear it.
enum HolderView<'a> {
    Read(&'a BTreeMap<PathBuf, Holders>),
    Unread,
}

fn joined_or_none(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        "none".to_string()
    } else {
        set.iter().cloned().collect::<Vec<_>>().join(",")
    }
}

/// One row line, the single form every lane prints. `path=` stays last so a
/// reader that splits on `path=` keeps working.
fn row_line(verb: &str, lane: &str, row: &Row, holders: &HolderView) -> String {
    let owner = match &row.owner {
        Some(tree) => tree.display().to_string(),
        None => "none".to_string(),
    };
    let (nodes, sessions) = match holders {
        HolderView::Read(join) => {
            let empty = Holders::default();
            let h = row
                .owner
                .as_ref()
                .and_then(|t| join.get(&phys(t)))
                .unwrap_or(&empty);
            (joined_or_none(&h.nodes), joined_or_none(&h.sessions))
        }
        HolderView::Unread => ("unread".to_string(), "unread".to_string()),
    };
    format!(
        "cargo-build-dir {verb} lane={lane} bytes={} quiet_h={:.1} owner={owner} node={nodes} session={sessions} path={}",
        row.bytes,
        row.quiet.as_secs_f64() / 3600.0,
        row.path.display()
    )
}

/// Classify and (on `apply`) reap both build bases' tagged hash dirs. Dry run
/// by contract: `apply=false` only reports. Prints one line per row plus the
/// `cargo-build-dirs` summary line.
pub fn sweep(root: &Path, apply: bool, now: SystemTime) -> SweepReport {
    let mut rep = SweepReport::default();
    let trees = registered_trees(root);
    let bases = managed_bases(root, &trees);
    rep.bases = bases.len();
    let fno_base = fno_build_base(root);
    rep.effective_cap_bytes = effective_cap_bytes(root);

    let mut names: BTreeSet<String> = BTreeSet::new();
    // phys dir -> the tree whose manifests resolve to it (phys tree). When
    // two trees resolve the same dir the LONGER tree path wins, the same
    // rule `owning_tree` applies.
    let mut owner_of: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for tree in &trees {
        match answer_tree(tree, &fno_base) {
            Ok(answer) => {
                rep.trees_resolved += 1;
                names.extend(answer.names);
                let tree_phys = phys(tree);
                for d in &answer.dirs {
                    let dir = phys(d);
                    let longer_wins = match owner_of.get(&dir) {
                        Some(existing) => existing.as_os_str().len() >= tree_phys.as_os_str().len(),
                        None => false,
                    };
                    if !longer_wins {
                        owner_of.insert(dir, tree_phys.clone());
                    }
                }
            }
            Err(manifest) => {
                rep.orphan_lane.get_or_insert(manifest);
            }
        }
    }
    // A live `cargo` (build or test) holds `.cargo-lock` only while it
    // compiles; `cargo test` drops it before running the built binaries, so
    // the gap between one test binary exiting and the next one starting has
    // no lock and no open file under the dir the cap lane is about to judge.
    // The live process itself is the only signal left standing: whichever
    // tree its cwd falls under is a tree with a cargo command in flight, so
    // every dir that tree's manifests resolve to is off the table this
    // sweep, cap pressure or not. An unreadable live set (`live_reliable =
    // false`) is never treated as "nothing is live" - the cap lane falls
    // back to the far more conservative `FRESH_SECS` floor instead.
    let (live, live_reliable) = match live_shards(&trees, &fno_base) {
        Ok(shards) => (shards, true),
        Err(()) => (BTreeSet::new(), false),
    };

    // One registry read per sweep, never per row; either step giving nothing
    // (no home under test, a read error) leaves the join unread.
    let holders_join = crate::paths::AgentsHome::from_env_opt()
        .and_then(|home| crate::state::load_registry(&home.registry_json()).ok())
        .map(|registry| session_join(&trees, &registry));
    let holders_view = match &holders_join {
        Some(join) => HolderView::Read(join),
        None => HolderView::Unread,
    };

    let mut rows: Vec<Row> = Vec::new();
    for base in &bases {
        let (paths, _) = inventory(base);
        for path in paths {
            let bytes = crate::reclaim::tree_bytes(&path);
            let quiet = quiet_of(&path, now);
            let membership = has_membership(&path, &names);
            rows.push(Row {
                under_fno: phys(&path).starts_with(phys(&fno_base)),
                owner: owner_of.get(&phys(&path)).cloned(),
                path,
                bytes,
                quiet,
                membership,
            });
        }
    }
    rep.rows = rows.len();
    let before_bytes: u64 = rows.iter().map(|r| r.bytes).sum();
    rep.before_bytes = before_bytes;
    rep.cap_exceeded = before_bytes > rep.effective_cap_bytes;

    // Lanes, first match wins: fresh, orphan, age. Cap runs after, over
    // whatever is still standing.
    enum Decision {
        Keep(&'static str),
        Reap(&'static str),
    }
    let mut decisions: Vec<Decision> = Vec::with_capacity(rows.len());
    let mut planned: Vec<usize> = Vec::new();
    let mut planned_bytes: u64 = 0;
    let mut refusal_of: BTreeMap<usize, &'static str> = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let decision = if live.contains(&phys(&row.path)) {
            Decision::Keep("cargo-live")
        } else if row.quiet < Duration::from_secs(FRESH_SECS) {
            Decision::Keep("fresh")
        } else if rep.orphan_lane.is_none()
            && !owner_of.contains_key(&phys(&row.path))
            && row.membership
        {
            rep.orphans += 1;
            planned.push(i);
            planned_bytes += row.bytes;
            Decision::Reap("orphan")
        } else {
            let owned = row.under_fno || row.membership;
            if owned && row.quiet >= Duration::from_secs(AGE_SECS) {
                planned.push(i);
                planned_bytes += row.bytes;
                Decision::Reap("age")
            } else if owned {
                Decision::Keep("within-age")
            } else {
                Decision::Keep("foreign")
            }
        };
        decisions.push(decision);
    }

    for &i in &planned {
        let row = &rows[i];
        let lane = match &decisions[i] {
            Decision::Reap(lane) => *lane,
            Decision::Keep(_) => unreachable!("planned rows are reaps"),
        };
        if !apply {
            let line = row_line("would-reap", lane, row, &holders_view);
            println!("{line}");
            rep.lines.push(line);
            continue;
        }
        match guard_remove(&row.path, SystemTime::now(), true) {
            Ok(()) => {
                let line = row_line("reaped", lane, row, &holders_view);
                println!("{line}");
                rep.lines.push(line);
                rep.reaped += 1;
                rep.reclaimed_bytes += row.bytes;
                if remove_empty_shard(&row.path) {
                    rep.shards_removed += 1;
                }
            }
            Err(reason) => {
                refusal_of.insert(i, reason);
                let line = row_line("kept", reason, row, &holders_view);
                println!("{line}");
                rep.lines.push(line);
            }
        }
    }

    // Cap: while the bytes actually left exceed the effective ceiling, reap
    // owned rows least recently used first, rows past CAP_MIN_QUIET_SECS
    // included - FRESH_SECS instead when the live-cargo read cannot be
    // trusted, since an empty live set is then not proof nothing is live.
    // The flock, the live-cargo check, and that floor are the only vetoes
    // here (no FRESH_SECS recheck on TOP of that in the trustworthy case),
    // and a refused row never ends the loop - the next-oldest candidate
    // still goes. Both the live set and each candidate's own quiet are
    // re-read right before its `guard_remove`: the walk above and the
    // orphan/age deletes both take real time, and a cargo command starting
    // mid-sweep is exactly the gap this lane exists to close.
    let cap_floor_secs = if live_reliable {
        CAP_MIN_QUIET_SECS
    } else {
        FRESH_SECS
    };
    let mut remaining = if apply {
        before_bytes.saturating_sub(rep.reclaimed_bytes)
    } else {
        before_bytes.saturating_sub(planned_bytes)
    };
    if remaining > rep.effective_cap_bytes {
        let mut candidates: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                (r.under_fno || r.membership)
                    && !planned.contains(i)
                    && !live.contains(&phys(&r.path))
                    && r.quiet >= Duration::from_secs(cap_floor_secs)
            })
            .map(|(i, _)| i)
            .collect();
        candidates.sort_by(|a, b| rows[*b].quiet.cmp(&rows[*a].quiet));
        for i in candidates {
            if remaining <= rep.effective_cap_bytes {
                break;
            }
            let row = &rows[i];
            let now2 = SystemTime::now();
            let (recheck_live, recheck_reliable) = match live_shards(&trees, &fno_base) {
                Ok(shards) => (shards, true),
                Err(()) => (BTreeSet::new(), false),
            };
            let recheck_floor = if recheck_reliable {
                CAP_MIN_QUIET_SECS
            } else {
                FRESH_SECS
            };
            let outcome = if recheck_live.contains(&phys(&row.path)) {
                Err("cargo-live")
            } else if quiet_of(&row.path, now2) < Duration::from_secs(recheck_floor) {
                Err("build-in-progress")
            } else if apply {
                guard_remove(&row.path, now2, false)
            } else {
                take_locks(&row.path).map(|_| ())
            };
            match outcome {
                Ok(()) => {
                    planned.push(i);
                    planned_bytes += row.bytes;
                    remaining -= row.bytes;
                    decisions[i] = Decision::Reap("cap");
                    let verb = if apply { "reaped" } else { "would-reap" };
                    let line = row_line(verb, "cap", row, &holders_view);
                    println!("{line}");
                    rep.lines.push(line);
                    if apply {
                        rep.reaped += 1;
                        rep.reclaimed_bytes += row.bytes;
                        if remove_empty_shard(&row.path) {
                            rep.shards_removed += 1;
                        }
                    }
                }
                Err(reason) => {
                    refusal_of.insert(i, reason);
                    let line = row_line("kept", reason, row, &holders_view);
                    println!("{line}");
                    rep.lines.push(line);
                }
            }
        }
    }

    // Keep lines print after the cap pass so a row the cap reaped never also
    // reads as kept.
    for (i, row) in rows.iter().enumerate() {
        if let Decision::Keep(lane) = &decisions[i] {
            let line = row_line("kept", lane, row, &holders_view);
            println!("{line}");
            rep.lines.push(line);
        }
    }

    // Shard dirs already empty: removed on apply, counted the same way.
    for base in &bases {
        let (_, empty_shards) = inventory(base);
        for shard in empty_shards {
            if apply && std::fs::remove_dir(&shard).is_ok() {
                rep.shards_removed += 1;
            }
        }
    }

    rep.after_bytes = if apply {
        let mut after = 0u64;
        for base in &bases {
            let (paths, _) = inventory(base);
            for path in paths {
                after += crate::reclaim::tree_bytes(&path);
            }
        }
        after
    } else {
        before_bytes.saturating_sub(planned_bytes)
    };

    rep.projected_bytes = if apply {
        rep.reclaimed_bytes
    } else {
        planned_bytes
    };

    let orphan_lane = match &rep.orphan_lane {
        None => "on".to_string(),
        Some(manifest) => format!("disabled:{manifest}"),
    };
    // cap_held answers "why are bytes still over the cap": counted only when
    // the sweep ended over it, with every standing row's reason - refusals
    // named by the reason they were refused, keeps by their lane.
    if rep.after_bytes > rep.effective_cap_bytes {
        for i in 0..rows.len() {
            if matches!(decisions[i], Decision::Reap(_)) {
                continue;
            }
            let reason = refusal_of.get(&i).copied().unwrap_or(match &decisions[i] {
                Decision::Keep(lane) => *lane,
                Decision::Reap(_) => unreachable!("reap rows are skipped above"),
            });
            *rep.cap_held.entry(reason).or_insert(0) += 1;
        }
    }
    let cap_held = if rep.cap_held.is_empty() {
        "-".to_string()
    } else {
        rep.cap_held
            .iter()
            .map(|(reason, count)| format!("{reason}:{count}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let summary = format!(
        "cargo-build-dirs mode={} bases={} rows={} trees_resolved={} orphans={} reaped={} reclaimed_bytes={} after_bytes={} effective_cap_bytes={} orphan_lane={orphan_lane} shards_removed={} before_bytes={} cap_exceeded={} cap_held={cap_held}",
        if apply { "apply" } else { "dry-run" },
        rep.bases,
        rep.rows,
        rep.trees_resolved,
        rep.orphans,
        rep.reaped,
        rep.reclaimed_bytes,
        rep.after_bytes,
        rep.effective_cap_bytes,
        rep.shards_removed,
        rep.before_bytes,
        rep.cap_exceeded,
    );
    println!("{summary}");
    rep.lines.push(summary);
    rep
}

/// Remove the build dirs `tree`'s manifests resolve to, when each is tagged
/// and under a managed base. Membership holds by construction (the tree is
/// live enough to answer). Best-effort by contract: never errors, returns the
/// removal count.
pub fn remove_for(tree: &Path) -> usize {
    let Ok(dirs) = list_for(tree) else {
        return 0;
    };
    let base_root = repo_root_for(tree);
    let bases = managed_bases(&base_root, &registered_trees(&base_root));
    let mut removed = 0;
    for dir in dirs {
        if !under_any(&dir, &bases) {
            continue;
        }
        if !dir.join(CACHEDIR_TAG).is_file() {
            continue;
        }
        if guard_remove(&dir, SystemTime::now(), false).is_ok() {
            removed += 1;
            remove_empty_shard(&dir);
        }
    }
    removed
}

#[derive(Debug, Default)]
pub struct TreeReclaim {
    pub dirs: Vec<(PathBuf, u64)>,
    pub kept: Vec<(PathBuf, &'static str)>,
    pub unread: Option<String>,
}

/// Reclaim the build-base shards and in-checkout target directories named by
/// one still-readable tree. The target identity checks happen before this
/// function is allowed to delete anything; locks and a final quiet check are
/// the last build-in-progress fence.
pub fn reclaim_tree_build_output(tree: &Path, now: SystemTime, apply: bool) -> TreeReclaim {
    let mut report = TreeReclaim::default();
    let Ok(build_dirs) = list_for(tree) else {
        report.unread = Some(tree.display().to_string());
        return report;
    };
    let Ok(target_dirs) = tree_target_dirs(tree) else {
        report.unread = Some(tree.display().to_string());
        return report;
    };
    let root = repo_root_for(tree);
    let bases = managed_bases(&root, &registered_trees(&root));
    let mut candidates = BTreeSet::new();
    for dir in build_dirs {
        if under_any(&dir, &bases) && dir.join(CACHEDIR_TAG).is_file() {
            candidates.insert(phys(&dir));
        }
    }
    candidates.extend(target_dirs);

    for dir in candidates {
        let bytes = crate::reclaim::tree_bytes(&dir);
        if !apply {
            report.dirs.push((dir, bytes));
            continue;
        }
        match guard_remove(&dir, now, true) {
            Ok(()) => {
                report.dirs.push((dir.clone(), bytes));
                remove_empty_shard(&dir);
            }
            Err(reason) => report.kept.push((dir, reason)),
        }
    }
    report
}

#[derive(Debug, Default)]
pub struct IdleTreeReport {
    pub candidates: usize,
    pub occupied: usize,
    pub reclaimed: usize,
    pub reclaimed_bytes: u64,
    pub candidate_bytes: u64,
    pub unread: Option<String>,
    pub lines: Vec<String>,
}

fn candidate_target_dirs(tree: &Path, now: SystemTime) -> Vec<PathBuf> {
    workspace_manifests(tree)
        .into_iter()
        .filter_map(|manifest| manifest.parent().map(|member| member.join("target")))
        .filter(|dir| {
            dir.join(CACHEDIR_TAG).is_file()
                && quiet_of(dir, now) >= Duration::from_secs(FRESH_SECS)
        })
        .collect()
}

fn idle_candidates(root: &Path, now: SystemTime) -> Vec<(PathBuf, u64)> {
    registered_trees(root)
        .into_iter()
        .filter(|tree| phys(tree) != phys(root))
        .filter_map(|tree| {
            let dirs = candidate_target_dirs(&tree, now);
            (!dirs.is_empty()).then(|| {
                let bytes = dirs.iter().map(|dir| crate::reclaim::tree_bytes(dir)).sum();
                (tree, bytes)
            })
        })
        .collect()
}

fn empty_idle_report(apply: bool) -> IdleTreeReport {
    let mut report = IdleTreeReport::default();
    let line = format!(
        "idle-trees mode={} candidates=0 occupied=0 reclaimed=0 reclaimed_bytes=0 candidate_bytes=0 unread=-",
        if apply { "apply" } else { "dry-run" }
    );
    println!("{line}");
    report.lines.push(line);
    report
}

/// Trees held by the registry or Claude roster. The roster and every
/// placement needed to prove absence are fail-closed: an unread snapshot or
/// an unplaced live row means no tree may be reclaimed.
pub fn occupied_trees(
    trees: &[PathBuf],
    registry: &crate::state::Registry,
    roster: &crate::claude_roster::ClaudeAgentsSnapshot,
    ages: Option<&HashMap<String, Option<i64>>>,
    grace_secs: i64,
) -> Result<BTreeSet<PathBuf>, &'static str> {
    let mut occupied: BTreeSet<PathBuf> = session_join(trees, registry)
        .into_iter()
        .filter_map(|(tree, holders)| (!holders.sessions.is_empty()).then_some(tree))
        .collect();
    let rows = match roster {
        crate::claude_roster::ClaudeAgentsSnapshot::Known { rows, warnings } => {
            if !warnings.is_empty() {
                return Err("roster-unread");
            }
            rows
        }
        crate::claude_roster::ClaudeAgentsSnapshot::Unknown { .. } => return Err("roster-unread"),
    };
    let phys_trees: Vec<PathBuf> = trees.iter().map(|tree| phys(tree)).collect();
    for row in rows {
        let terminal = row
            .state
            .as_deref()
            .map(crate::claude_roster::is_terminal_roster_state)
            .unwrap_or(false);
        let Some(cwd) = row.cwd.as_deref() else {
            if !terminal {
                return Err("roster-row-unplaced");
            }
            continue;
        };
        let Some(tree) = owning_tree(Path::new(cwd), &phys_trees) else {
            continue;
        };
        if !terminal {
            occupied.insert(tree.clone());
            continue;
        }
        let recent = ages.is_none()
            || row
                .session_id
                .as_ref()
                .and_then(|id| ages.and_then(|all| all.get(id)))
                .map(|age| age.map(|seconds| seconds < grace_secs).unwrap_or(true))
                .unwrap_or(true);
        if recent {
            occupied.insert(tree.clone());
        }
    }
    Ok(occupied)
}

fn idle_trees_with(
    root: &Path,
    apply: bool,
    now: SystemTime,
    registry: Result<crate::state::Registry, String>,
    roster: crate::claude_roster::ClaudeAgentsSnapshot,
    ages: &dyn Fn(&[String]) -> Option<HashMap<String, Option<i64>>>,
    grace_secs: i64,
) -> IdleTreeReport {
    let mut report = IdleTreeReport::default();
    let candidates = idle_candidates(root, now);
    report.candidates = candidates.len();
    report.candidate_bytes = candidates.iter().map(|(_, bytes)| *bytes).sum();
    if candidates.is_empty() {
        return empty_idle_report(apply);
    }

    let registry = match registry {
        Ok(registry) => registry,
        Err(_) => {
            report.unread = Some("registry-unread".to_string());
            let line = format!(
                "idle-trees mode={} candidates={} occupied=0 reclaimed=0 reclaimed_bytes=0 candidate_bytes={} unread=registry-unread",
                if apply { "apply" } else { "dry-run" },
                report.candidates,
                report.candidate_bytes
            );
            println!("{line}");
            report.lines.push(line);
            return report;
        }
    };
    let candidate_trees: Vec<PathBuf> = candidates.iter().map(|(tree, _)| tree.clone()).collect();
    let ids: Vec<String> = match &roster {
        crate::claude_roster::ClaudeAgentsSnapshot::Known { rows, .. } => rows
            .iter()
            .filter(|row| {
                row.state
                    .as_deref()
                    .map(crate::claude_roster::is_terminal_roster_state)
                    .unwrap_or(false)
                    && row
                        .cwd
                        .as_deref()
                        .map(|cwd| owning_tree(Path::new(cwd), &candidate_trees).is_some())
                        .unwrap_or(false)
            })
            .filter_map(|row| row.session_id.clone())
            .collect(),
        crate::claude_roster::ClaudeAgentsSnapshot::Unknown { .. } => Vec::new(),
    };
    let ages = ages(&ids);
    let occupied = match occupied_trees(
        &candidate_trees,
        &registry,
        &roster,
        ages.as_ref(),
        grace_secs,
    ) {
        Ok(occupied) => occupied,
        Err(reason) => {
            report.unread = Some(reason.to_string());
            let line = format!(
                "idle-trees mode={} candidates={} occupied=0 reclaimed=0 reclaimed_bytes=0 candidate_bytes={} unread={reason}",
                if apply { "apply" } else { "dry-run" },
                report.candidates,
                report.candidate_bytes
            );
            println!("{line}");
            report.lines.push(line);
            return report;
        }
    };
    let live_cwds = match live_cwds(Some("cargo")) {
        Ok(cwds) => cwds.into_iter().map(|cwd| phys(&cwd)).collect::<Vec<_>>(),
        Err(()) => {
            report.unread = Some("lsof-unread".to_string());
            let line = format!(
                "idle-trees mode={} candidates={} occupied={} reclaimed=0 reclaimed_bytes=0 candidate_bytes={} unread=lsof-unread",
                if apply { "apply" } else { "dry-run" },
                report.candidates,
                occupied.len(),
                report.candidate_bytes
            );
            println!("{line}");
            report.lines.push(line);
            return report;
        }
    };
    report.occupied = occupied.len();
    let phys_candidates: Vec<PathBuf> = candidate_trees.iter().map(|tree| phys(tree)).collect();
    let mut live_trees = HashSet::new();
    for cwd in live_cwds {
        if let Some(tree) = owning_tree(&cwd, &phys_candidates) {
            live_trees.insert(tree.clone());
        }
    }
    for (tree, _) in candidates {
        let tree = phys(&tree);
        if occupied.contains(&tree) {
            let line = format!(
                "idle-tree kept tree={} dirs=0 bytes=0 reason=occupied",
                tree.display()
            );
            println!("{line}");
            report.lines.push(line);
            continue;
        }
        if live_trees.contains(&tree) {
            let line = format!(
                "idle-tree kept tree={} dirs=0 bytes=0 reason=live-cargo",
                tree.display()
            );
            println!("{line}");
            report.lines.push(line);
            continue;
        }
        let reclaimed = reclaim_tree_build_output(&tree, now, apply);
        if let Some(reason) = reclaimed.unread {
            report.unread = Some(format!("unread:{reason}"));
            let line = format!(
                "idle-tree kept tree={} dirs=0 bytes=0 reason=unread:{reason}",
                tree.display()
            );
            println!("{line}");
            report.lines.push(line);
            continue;
        }
        let bytes: u64 = reclaimed.dirs.iter().map(|(_, bytes)| *bytes).sum();
        if !reclaimed.dirs.is_empty() {
            if apply {
                report.reclaimed += reclaimed.dirs.len();
                report.reclaimed_bytes += bytes;
            }
            let verb = if apply { "reclaimed" } else { "would-reclaim" };
            let line = format!(
                "idle-tree {verb} tree={} dirs={} bytes={} reason=-",
                tree.display(),
                reclaimed.dirs.len(),
                bytes
            );
            println!("{line}");
            report.lines.push(line);
        }
        for (dir, reason) in reclaimed.kept {
            let line = format!(
                "idle-tree kept tree={} dirs=1 bytes={} reason={reason} path={}",
                tree.display(),
                crate::reclaim::tree_bytes(&dir),
                dir.display()
            );
            println!("{line}");
            report.lines.push(line);
        }
    }
    let unread = report.unread.as_deref().unwrap_or("-");
    let line = format!(
        "idle-trees mode={} candidates={} occupied={} reclaimed={} reclaimed_bytes={} candidate_bytes={} unread={unread}",
        if apply { "apply" } else { "dry-run" },
        report.candidates,
        report.occupied,
        report.reclaimed,
        report.reclaimed_bytes,
        report.candidate_bytes
    );
    println!("{line}");
    report.lines.push(line);
    report
}

pub fn reclaim_idle_trees(root: &Path, apply: bool, now: SystemTime) -> IdleTreeReport {
    if idle_candidates(root, now).is_empty() {
        return empty_idle_report(apply);
    }
    let registry = crate::paths::AgentsHome::from_env_opt()
        .ok_or_else(|| "registry-unread".to_string())
        .and_then(|home| {
            crate::state::load_registry(&home.registry_json())
                .map_err(|_| "registry-unread".to_string())
        });
    let roster = crate::claude_roster::read_all_agents_union();
    let grace_secs = crate::agents_config::retire_grace_secs(root) as i64;
    idle_trees_with(
        root,
        apply,
        now,
        registry,
        roster,
        &|ids| {
            if ids.is_empty() {
                return Some(HashMap::new());
            }
            let (probes, outcome) = crate::truth_probe::family1_truth_probe_many_measured(ids);
            match outcome {
                crate::truth_probe::BatchOutcome::Measured => Some(
                    ids.iter()
                        .map(|id| {
                            (
                                id.clone(),
                                probes.get(id).and_then(|probe| {
                                    probe.last_activity_age_s.map(|age| age.max(0.0) as i64)
                                }),
                            )
                        })
                        .collect(),
                ),
                crate::truth_probe::BatchOutcome::NotMeasured => None,
            }
        },
        grace_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes every test that repoints the process-global env (CARGO,
    /// FNO_CARGO_TARGETS_BASE, the fake-cargo answer vars). One lock shared
    /// with reclaim's tests: both suites mutate the same vars.
    use crate::reclaim::tests::ENV_LOCK;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fno-cbd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn age_every(dir: &Path, secs: u64) {
        let old = SystemTime::now() - Duration::from_secs(secs);
        fn walk(path: &Path, old: SystemTime) {
            if let Ok(file) = std::fs::File::options().read(true).open(path) {
                let _ = file.set_times(std::fs::FileTimes::new().set_modified(old));
            }
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    walk(&entry.path(), old);
                }
            }
        }
        walk(dir, old);
    }

    fn plant(base: &Path, shard: &str, hash: &str, quiet_secs: u64, fingerprint: bool) -> PathBuf {
        let dir = base.join(shard).join(hash);
        std::fs::create_dir_all(dir.join("debug/deps")).unwrap();
        std::fs::write(
            dir.join(CACHEDIR_TAG),
            b"Signature: 8a477f597d28d172789f068868ba2775\n",
        )
        .unwrap();
        std::fs::write(dir.join("debug/deps/payload"), vec![0u8; 4096]).unwrap();
        if fingerprint {
            std::fs::create_dir_all(dir.join("debug/.fingerprint")).unwrap();
            std::fs::write(dir.join("debug/.fingerprint/fakepkg-beef"), b"").unwrap();
        }
        age_every(&dir, quiet_secs);
        dir
    }

    fn fake_cargo(dir: &Path, broken_token: &str) {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("cargo");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in *{broken_token}*) exit 1 ;; esac; done\n\
                 if [ -n \"$CARGO_BUILD_BUILD_DIR\" ]; then\n\
                 build=\"$CBD_FNO_ANSWER\"\n\
                 else\n\
                 build=\"$CBD_FB_ANSWER\"\n\
                 fi\n\
                 if [ -n \"$CBD_TARGET_ANSWER\" ]; then\n\
                 printf '{{\"build_directory\":\"%s\",\"target_directory\":\"%s\",\"packages\":[{{\"name\":\"fakepkg\"}}]}}\\n' \"$build\" \"$CBD_TARGET_ANSWER\"\n\
                 else\n\
                 printf '{{\"build_directory\":\"%s\",\"packages\":[{{\"name\":\"fakepkg\"}}]}}\\n' \"$build\"\n\
                 fi\n"
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    struct Env {
        root: PathBuf,
        fno_base: PathBuf,
        fb_base: PathBuf,
        fb_parent: PathBuf,
    }

    /// Repo + both sandbox bases + the fake cargo wired through CARGO.
    fn setup(tag: &str, broken_token: &str) -> Env {
        let root = temp_root(tag);
        std::fs::create_dir_all(root.join("crates/fake")).unwrap();
        std::fs::write(
            root.join("crates/fake/Cargo.toml"),
            "[package]\nname = 'fakepkg'\nversion = '0.1.0'\n",
        )
        .unwrap();
        let fno_base = root.join("fno-base");
        // The fallback base must sit OUTSIDE every registered tree (the repo
        // root itself when git cannot answer), exactly as on a real machine -
        // a base under the root is rejected by design. Nested one level so
        // the >=3-component guard passes on Linux too, where /tmp/<name>
        // would be only two.
        let fb_parent = temp_root(&format!("{tag}-fb"));
        let fb_base = fb_parent.join("base");
        std::fs::create_dir_all(&fno_base).unwrap();
        std::fs::create_dir_all(&fb_base).unwrap();
        fake_cargo(&root, broken_token);
        std::env::set_var("CARGO", root.join("bin/cargo"));
        std::env::set_var("CBD_FNO_ANSWER", fno_base.join("00").join("aaaa11"));
        std::env::set_var("CBD_FB_ANSWER", fb_base.join("00").join("bbbb22"));
        std::env::set_var("FNO_CARGO_TARGETS_BASE", &fno_base);
        // Pin the free-space read: the cap lane now reaps fresh rows, so a
        // nearly full host would shrink the cap to 1 byte and reap the very
        // rows these tests mean to keep.
        std::env::set_var("FNO_CARGO_FREE_BYTES", (1u64 << 50).to_string());
        Env {
            root,
            fno_base,
            fb_base,
            fb_parent,
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            std::env::remove_var("CARGO");
            std::env::remove_var("CBD_FNO_ANSWER");
            std::env::remove_var("CBD_FB_ANSWER");
            std::env::remove_var("CBD_TARGET_ANSWER");
            std::env::remove_var("FNO_CARGO_TARGETS_BASE");
            std::env::remove_var("FNO_CARGO_FREE_BYTES");
            std::env::remove_var("FNO_TEST_LIVE_CARGO_CWDS");
            let _ = std::fs::remove_dir_all(&self.root);
            let _ = std::fs::remove_dir_all(&self.fb_parent);
        }
    }

    fn seven_h() -> u64 {
        7 * 3600
    }

    fn init_git_repo(root: &Path) {
        assert!(Command::new("git")
            .current_dir(root)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(root)
            .args(["add", "crates/fake/Cargo.toml"])
            .status()
            .unwrap()
            .success());
    }

    fn plant_target(dir: &Path, quiet_secs: u64) {
        std::fs::create_dir_all(dir.join("debug/deps")).unwrap();
        std::fs::write(dir.join(CACHEDIR_TAG), b"Signature: target\n").unwrap();
        std::fs::write(dir.join("debug/deps/payload"), vec![0u8; 4096]).unwrap();
        age_every(dir, quiet_secs);
    }

    #[test]
    fn reclaim_tree_build_output_removes_target_and_build_dirs_with_bytes() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("tree-output", "never-broken");
        init_git_repo(&env.root);
        let target = env.root.join("crates/fake/target");
        plant_target(&target, seven_h());
        let build = plant(&env.fno_base, "00", "tree0011", seven_h(), true);
        std::env::set_var("CBD_TARGET_ANSWER", &target);

        let report = reclaim_tree_build_output(&env.root, SystemTime::now(), true);

        assert!(report.unread.is_none(), "{report:?}");
        assert_eq!(report.dirs.len(), 2, "{report:?}");
        assert!(report.dirs.iter().all(|(_, bytes)| *bytes > 0));
        assert!(!target.exists(), "target output is reclaimed");
        assert!(!build.exists(), "build shard is reclaimed");
    }

    #[test]
    fn reclaim_tree_build_output_keeps_a_locked_target() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("tree-locked", "never-broken");
        init_git_repo(&env.root);
        let target = env.root.join("crates/fake/target");
        plant_target(&target, seven_h());
        std::env::set_var("CBD_TARGET_ANSWER", &target);
        let lock_path = target.join("debug/.cargo-lock");
        std::fs::write(&lock_path, b"").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .open(&lock_path)
            .unwrap();
        unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) };

        let report = reclaim_tree_build_output(&env.root, SystemTime::now(), true);

        assert!(target.exists(), "a build in progress keeps target output");
        assert!(report
            .kept
            .iter()
            .any(|(path, reason)| path == &phys(&target) && *reason == "build-in-progress"));
        drop(lock);
    }

    #[test]
    fn target_identity_rejects_outside_and_tracked_directories() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("tree-identity", "never-broken");
        init_git_repo(&env.root);
        let outside = env.fb_parent.join("outside-target");
        plant_target(&outside, seven_h());
        std::env::set_var("CBD_TARGET_ANSWER", &outside);
        let report = reclaim_tree_build_output(&env.root, SystemTime::now(), true);
        assert!(report.dirs.is_empty(), "{report:?}");
        assert!(outside.exists(), "a target outside its member stays");

        let tracked = env.root.join("crates/fake/target");
        plant_target(&tracked, seven_h());
        std::fs::write(tracked.join("tracked.bin"), b"source").unwrap();
        assert!(Command::new("git")
            .current_dir(&env.root)
            .args(["add", "crates/fake/target/tracked.bin"])
            .status()
            .unwrap()
            .success());
        std::env::set_var("CBD_TARGET_ANSWER", &tracked);
        let report = reclaim_tree_build_output(&env.root, SystemTime::now(), true);
        assert!(report.dirs.is_empty(), "{report:?}");
        assert!(tracked.exists(), "tracked target output stays");
    }

    #[test]
    fn occupied_trees_fails_closed_for_roster_and_respects_nested_tree() {
        let root = temp_root("occupied");
        let tree = root.join("wt");
        let nested = tree.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let reg = sj_registry(vec![sj_entry(
            "busy",
            &nested,
            Some("x-node"),
            crate::AgentStatus::Busy,
        )]);
        let mut row = crate::claude_roster::ClaudeAgentRow::new("done", Some("done"));
        row.session_id = Some("session".to_string());
        row.cwd = Some(tree.display().to_string());
        let roster = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![row]);
        let ages = HashMap::from([("session".to_string(), Some(5_000))]);

        let occupied = occupied_trees(
            &[root.clone(), tree.clone(), nested.clone()],
            &reg,
            &roster,
            Some(&ages),
            1_200,
        )
        .unwrap();

        assert!(occupied.contains(&phys(&nested)), "{occupied:?}");
        assert!(!occupied.contains(&phys(&tree)), "{occupied:?}");
        assert!(occupied_trees(
            &[root.clone(), tree.clone()],
            &reg,
            &crate::claude_roster::ClaudeAgentsSnapshot::unknown("test"),
            None,
            1_200,
        )
        .is_err());

        let partial = crate::claude_roster::ClaudeAgentsSnapshot::Known {
            rows: Vec::new(),
            warnings: vec!["partial".to_string()],
        };
        assert_eq!(
            occupied_trees(&[], &sj_registry(Vec::new()), &partial, None, 1_200),
            Err("roster-unread")
        );

        let mut unplaced = crate::claude_roster::ClaudeAgentRow::new("working", Some("working"));
        unplaced.cwd = None;
        assert_eq!(
            occupied_trees(
                &[root, tree],
                &reg,
                &crate::claude_roster::ClaudeAgentsSnapshot::known(vec![unplaced]),
                None,
                1_200,
            ),
            Err("roster-row-unplaced")
        );
    }

    #[test]
    fn idle_tree_reclaim_preserves_dirty_and_untracked_source_files() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("idle-tree", "never-broken");
        init_git_repo(&env.root);
        let source = env.root.join("crates/fake/src.rs");
        std::fs::write(&source, b"modified source\n").unwrap();
        let untracked = env.root.join("crates/fake/untracked.txt");
        std::fs::write(&untracked, b"untracked source\n").unwrap();
        assert!(Command::new("git")
            .current_dir(&env.root)
            .args(["add", "crates/fake/src.rs"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(&env.root)
            .args(["commit", "-qm", "fixture"])
            .status()
            .unwrap()
            .success());
        let linked = temp_root("idle-tree-linked");
        std::fs::remove_dir_all(&linked).unwrap();
        assert!(Command::new("git")
            .current_dir(&env.root)
            .args(["worktree", "add", "-q"])
            .arg(&linked)
            .arg("HEAD")
            .status()
            .unwrap()
            .success());
        let linked_source = linked.join("crates/fake/src.rs");
        std::fs::write(&linked_source, b"dirty source\n").unwrap();
        let linked_untracked = linked.join("crates/fake/untracked.txt");
        std::fs::write(&linked_untracked, b"kept untracked\n").unwrap();
        let target = linked.join("crates/fake/target");
        plant_target(&target, seven_h());
        std::env::set_var("CBD_TARGET_ANSWER", &target);
        std::env::set_var("FNO_TEST_LIVE_CARGO_CWDS", "");

        let dry_run = idle_trees_with(
            &env.root,
            false,
            SystemTime::now(),
            Ok(sj_registry(Vec::new())),
            crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
            &|_| Some(HashMap::new()),
            1_200,
        );
        assert_eq!(dry_run.reclaimed, 0, "{dry_run:?}");
        assert!(target.exists(), "dry run deletes nothing");

        let report = idle_trees_with(
            &env.root,
            true,
            SystemTime::now(),
            Ok(sj_registry(Vec::new())),
            crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
            &|_| Some(HashMap::new()),
            1_200,
        );

        assert_eq!(report.reclaimed, 1, "{report:?}");
        assert!(!target.exists(), "only build output is removed");
        assert_eq!(std::fs::read(&linked_source).unwrap(), b"dirty source\n");
        assert_eq!(
            std::fs::read(&linked_untracked).unwrap(),
            b"kept untracked\n"
        );
        assert!(report
            .lines
            .iter()
            .any(|line| line.contains("idle-tree reclaimed")));
        let _ = Command::new("git")
            .current_dir(&env.root)
            .args(["worktree", "remove", "--force"])
            .arg(&linked)
            .status();
        let _ = std::fs::remove_dir_all(&linked);
    }

    #[test]
    fn idle_tree_pass_skips_registry_and_roster_when_no_tree_is_a_candidate() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("idle-tree-empty", "never-broken");

        let report = idle_trees_with(
            &env.root,
            true,
            SystemTime::now(),
            Err("must not read".to_string()),
            crate::claude_roster::ClaudeAgentsSnapshot::unknown("must not read"),
            &|_| panic!("must not probe ages"),
            1_200,
        );

        assert_eq!(report.candidates, 0);
        assert!(report.unread.is_none(), "{report:?}");
        assert!(report
            .lines
            .iter()
            .any(|line| line.contains("candidates=0") && line.contains("unread=-")));
    }

    #[test]
    fn idle_tree_pass_fails_closed_for_unread_registry_roster_and_lsof() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("idle-tree-errors", "never-broken");
        init_git_repo(&env.root);
        assert!(Command::new("git")
            .current_dir(&env.root)
            .args(["commit", "-qm", "fixture"])
            .status()
            .unwrap()
            .success());
        let linked = temp_root("idle-tree-errors-linked");
        std::fs::remove_dir_all(&linked).unwrap();
        assert!(Command::new("git")
            .current_dir(&env.root)
            .args(["worktree", "add", "-q"])
            .arg(&linked)
            .arg("HEAD")
            .status()
            .unwrap()
            .success());
        let target = linked.join("crates/fake/target");
        plant_target(&target, seven_h());
        std::env::set_var("CBD_TARGET_ANSWER", &target);

        let registry_error = idle_trees_with(
            &env.root,
            true,
            SystemTime::now(),
            Err("read failed".to_string()),
            crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
            &|_| Some(HashMap::new()),
            1_200,
        );
        assert_eq!(registry_error.unread.as_deref(), Some("registry-unread"));
        assert!(target.exists());

        let roster_error = idle_trees_with(
            &env.root,
            true,
            SystemTime::now(),
            Ok(sj_registry(Vec::new())),
            crate::claude_roster::ClaudeAgentsSnapshot::unknown("read failed"),
            &|_| Some(HashMap::new()),
            1_200,
        );
        assert_eq!(roster_error.unread.as_deref(), Some("roster-unread"));
        assert!(target.exists());

        std::env::set_var("FNO_TEST_LIVE_CARGO_CWDS", linked.display().to_string());
        let cargo_error = idle_trees_with(
            &env.root,
            true,
            SystemTime::now(),
            Ok(sj_registry(Vec::new())),
            crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
            &|_| Some(HashMap::new()),
            1_200,
        );
        assert!(cargo_error.unread.is_none(), "{cargo_error:?}");
        assert!(cargo_error
            .lines
            .iter()
            .any(|line| line.contains("reason=live-cargo")));
        assert!(target.exists());

        let _ = Command::new("git")
            .current_dir(&env.root)
            .args(["worktree", "remove", "--force"])
            .arg(&linked)
            .status();
        let _ = std::fs::remove_dir_all(&linked);
    }

    /// AC1-HP: tagged hash dir under the fallback base, fingerprinted, quiet
    /// 7h, no tree resolves to it - reaped by the orphan lane, shard gone.
    #[test]
    fn orphan_lane_reaps_a_quiet_fingerprinted_dir_under_the_fallback_base() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("orphan", "never-broken");
        let orphan = plant(&env.fb_base, "00", "cafefe12", seven_h(), true);

        let rep = sweep(&env.root, true, SystemTime::now());

        assert_eq!(rep.bases, 2, "fno base + fallback base");
        assert!(rep.orphan_lane.is_none());
        assert_eq!(rep.orphans, 1);
        assert_eq!(rep.reaped, 1);
        assert!(!orphan.exists(), "the orphan is gone");
        assert!(
            !orphan.parent().unwrap().exists(),
            "the emptied shard dir is removed"
        );
        assert!(rep
            .lines
            .iter()
            .any(|l| l.contains("lane=orphan") && l.contains("reaped lane=orphan")));
        let summary = rep.lines.last().unwrap();
        assert!(summary.contains("orphans=1"), "{summary}");
        assert!(summary.contains("orphan_lane=on"), "{summary}");
    }

    /// AC2-EDGE: quiet 2h is fresh; no fingerprint under the fallback base is
    /// foreign; a held `.cargo-lock` is build-in-progress. Nothing is reaped.
    #[test]
    fn fresh_foreign_and_locked_rows_are_kept() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("keeps", "never-broken");
        let fresh = plant(&env.fb_base, "00", "fresh01", 2 * 3600, true);
        let foreign = plant(&env.fb_base, "00", "foreig33", seven_h(), false);
        let locked = plant(&env.fb_base, "00", "locked44", seven_h(), true);
        std::fs::create_dir_all(locked.join("debug")).unwrap();
        let lock_path = locked.join("debug/.cargo-lock");
        std::fs::write(&lock_path, b"").unwrap();
        // A held lock file with a fresh mtime reads as an active build via the
        // quiet guard alone; age it so this test exercises the flock itself.
        age_every(&locked, seven_h());
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .open(&lock_path)
            .unwrap();
        unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) };

        let rep = sweep(&env.root, true, SystemTime::now());

        assert_eq!(rep.reaped, 0, "nothing may go");
        assert!(fresh.exists());
        assert!(foreign.exists());
        assert!(locked.exists(), "a held cargo lock protects the row");
        assert!(rep
            .lines
            .iter()
            .any(|l| l.contains("lane=fresh") && l.contains("fresh01")));
        assert!(rep
            .lines
            .iter()
            .any(|l| l.contains("lane=foreign") && l.contains("foreig33")));
        assert!(rep
            .lines
            .iter()
            .any(|l| l.contains("lane=build-in-progress")));
        drop(lock);
    }

    /// AC3-ERR: one tree whose cargo metadata exits non-zero disables the
    /// orphan lane for the run and names the manifest in the summary.
    #[test]
    fn a_failing_manifest_disables_the_orphan_lane_and_names_itself() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("broken", "broken");
        std::fs::create_dir_all(env.root.join("crates/broken")).unwrap();
        std::fs::write(
            env.root.join("crates/broken/Cargo.toml"),
            "[package]\nname = 'broken'\nversion = '0.1.0'\n",
        )
        .unwrap();
        let orphan = plant(&env.fb_base, "00", "cafefe99", seven_h(), true);

        let rep = sweep(&env.root, true, SystemTime::now());

        assert_eq!(rep.orphans, 0);
        assert_eq!(rep.reaped, 0);
        assert!(
            orphan.exists(),
            "a blind orphan sweep is the one mistake this lane cannot undo"
        );
        let named = env.root.join("crates/broken/Cargo.toml");
        assert!(
            rep.orphan_lane.as_deref() == Some(named.to_str().unwrap()),
            "{:?}",
            rep.orphan_lane
        );
        let summary = rep.lines.last().unwrap();
        assert!(
            summary.contains(&format!("orphan_lane=disabled:{}", named.display())),
            "{summary}"
        );
    }

    /// AC4-HP: remove_for takes one resolved dir under EACH base and never a
    /// tagged dir under neither.
    #[test]
    fn remove_for_takes_both_resolved_dirs_and_nothing_else() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("removefor", "never-broken");
        let under_fno = plant(&env.fno_base, "00", "aaaa11", 0, true);
        let under_fb = plant(&env.fb_base, "00", "bbbb22", 0, true);
        let outsider_dir = temp_root("removefor-outsider");
        let outsider = plant(&outsider_dir, "00", "cccc33", 0, true);

        let removed = remove_for(&env.root);

        assert_eq!(removed, 2, "one dir under each managed base");
        assert!(!under_fno.exists());
        assert!(!under_fb.exists());
        assert!(outsider.exists(), "a tagged dir under neither base stays");
        let _ = std::fs::remove_dir_all(&outsider_dir);
    }

    /// AC5-ERR: an env-free resolution at `/x/y` or inside a registered tree
    /// leaves the fno base as the only managed base.
    #[test]
    fn a_bad_fallback_resolution_leaves_only_the_fno_base() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("badbase", "never-broken");
        // /x/y/aa/bb: shape passes, `/x/y` has fewer than 3 components.
        std::env::set_var("CBD_FB_ANSWER", "/x/y/aa/bb");
        let bases = managed_bases(&env.root, &registered_trees(&env.root));
        assert_eq!(bases, vec![env.fno_base.clone()], "{bases:?}");

        // Inside the registered tree: the base IS a tree, outside-every-tree
        // fails.
        std::env::set_var("CBD_FB_ANSWER", env.root.join("00/abc123"));
        let bases = managed_bases(&env.root, &registered_trees(&env.root));
        assert_eq!(bases, vec![env.fno_base.clone()], "{bases:?}");
    }

    /// AC8-EDGE: cargo absent from PATH but present at
    /// `$CARGO_HOME/bin/cargo` is still found, so the daemon's merge reap
    /// (no cargo on PATH) reclaims hash dirs again.
    #[test]
    fn cargo_bin_finds_cargo_via_cargo_home_when_path_has_none() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_root("cargo-home");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let script = dir.join("bin/cargo");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '{\"build_directory\":\"%s\",\"packages\":[]}\\n' \"$CBD_FB\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        // No CARGO, a PATH with no cargo in it, CARGO_HOME pointing at the
        // sandbox: the fallback probe must land on the sandbox cargo.
        std::env::remove_var("CARGO");
        let real_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", &dir);
        std::env::set_var("CARGO_HOME", &dir);
        std::env::set_var("CBD_FB", dir.join("00").join("abcd11"));

        let found = cargo_bin();

        // Restore before asserting: a panicking assert must not leak the
        // mutated env into the later tests this lock serializes.
        std::env::set_var("PATH", &real_path);
        std::env::remove_var("CARGO_HOME");
        std::env::remove_var("CBD_FB");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found.as_deref(), Some(script.as_path()), "{found:?}");
    }

    /// AC1-HP: 18 owned rows all quiet under the fresh window, none locked,
    /// total over the cap - the cap lane reaps exactly the oldest-quiet rows
    /// needed to drop under the cap, and every survivor is younger than every
    /// row reaped.
    #[test]
    fn cap_reaps_fresh_rows_least_recently_used_first_until_under_cap() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("caplru", "never-broken");
        let mut rows = Vec::new();
        for i in 0..18 {
            let hash = format!("f{i:04x}");
            let quiet = 1100 * (i + 1); // 0.3h..5.5h: every row under 6h
            rows.push((quiet, plant(&env.fno_base, "00", &hash, quiet, false)));
        }
        // Each planted row carries its CACHEDIR.TAG too, so size one row the
        // way the sweep does and set the cap to exactly 8 rows' bytes.
        let unit = crate::reclaim::tree_bytes(&rows[0].1);
        std::env::set_var("FNO_CARGO_FREE_BYTES", (unit * 16).to_string());

        let rep = sweep(&env.root, true, SystemTime::now());

        assert_eq!(rep.before_bytes, unit * 18, "{rep:?}");
        assert_eq!(rep.effective_cap_bytes, unit * 8, "{rep:?}");
        assert!(rep.cap_exceeded);
        assert_eq!(rep.reaped, 10, "{rep:?}");
        assert_eq!(rep.after_bytes, unit * 8, "{rep:?}");
        assert!(rep.after_bytes <= rep.effective_cap_bytes, "{rep:?}");
        let reaped: Vec<u64> = rows
            .iter()
            .filter(|(_, p)| !p.exists())
            .map(|(q, _)| *q)
            .collect();
        let kept: Vec<u64> = rows
            .iter()
            .filter(|(_, p)| p.exists())
            .map(|(q, _)| *q)
            .collect();
        assert_eq!(reaped.len(), 10);
        assert_eq!(kept.len(), 8);
        assert_eq!(*reaped.iter().min().unwrap(), 1100 * 9, "{reaped:?}");
        assert_eq!(*kept.iter().max().unwrap(), 1100 * 8, "{kept:?}");
        assert!(rep.lines.iter().any(|l| l.contains("reaped lane=cap")));
        assert!(rep.lines.iter().any(|l| l.contains("kept lane=fresh")));
        let summary = rep.lines.last().unwrap();
        assert!(summary.contains("cap_exceeded=true"), "{summary}");
        assert!(summary.contains("cap_held=-"), "{summary}");
    }

    /// AC1-ERR: two owned fresh rows over the cap, the older one's
    /// `.cargo-lock` flock-held - the locked row stays build-in-progress, the
    /// loop continues, the unlocked row goes lane=cap, and the summary names
    /// every standing row's reason (AC3-ERR: a refusal and a foreign keep).
    #[test]
    fn cap_keeps_a_locked_row_and_says_so() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("caplock", "never-broken");
        let older = plant(&env.fno_base, "00", "cafe0001", 2 * 3600, false);
        let younger = plant(&env.fno_base, "00", "cafe0002", 1 * 3600, false);
        let foreign = plant(&env.fb_base, "00", "cafe0004", seven_h(), false);
        std::fs::create_dir_all(older.join("debug")).unwrap();
        let lock_path = older.join("debug/.cargo-lock");
        std::fs::write(&lock_path, b"").unwrap();
        // A held lock file with a fresh mtime reads as an active build via the
        // quiet guard alone; age it so this test exercises the flock itself.
        age_every(&older, 2 * 3600);
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .open(&lock_path)
            .unwrap();
        unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) };
        // Cap at half a row's bytes: even after the younger row goes, the
        // held row keeps the total over the cap, so cap_held must say why.
        let unit = crate::reclaim::tree_bytes(&younger);
        std::env::set_var("FNO_CARGO_FREE_BYTES", unit.to_string());

        let rep = sweep(&env.root, true, SystemTime::now());

        assert!(older.exists(), "a held cargo lock protects the row");
        assert!(foreign.exists(), "a foreign row is never a candidate");
        assert!(!younger.exists(), "the unlocked row is reaped lane=cap");
        assert_eq!(rep.reaped, 1, "{rep:?}");
        assert!(rep.lines.iter().any(|l| l.contains("reaped lane=cap")));
        assert!(rep
            .lines
            .iter()
            .any(|l| l.contains("kept lane=build-in-progress")));
        let summary = rep.lines.last().unwrap();
        assert!(summary.contains("cap_exceeded=true"), "{summary}");
        assert!(
            summary.contains("cap_held=build-in-progress:1,foreign:1"),
            "{summary}"
        );
        drop(lock);
    }

    /// The CI incident this guards against: `cargo test` drops `.cargo-lock`
    /// once compiling ends, so the gap between one test binary exiting and
    /// the next one starting holds no lock and no open file - the cap lane
    /// reaped the build dir a live `cargo test` was still running out of.
    /// The live process's own tree stays off the cap lane's table even when
    /// it is the only row standing between the sweep and the cap.
    #[test]
    fn cap_keeps_a_row_a_live_cargo_process_owns() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("caplive", "never-broken");
        // The fake cargo always answers CBD_FNO_ANSWER for this tree, so
        // this is the exact dir a live cargo process running from
        // `env.root` would own. Quiet past CAP_MIN_QUIET_SECS, and OLDER
        // than `other`, so the oldest-first sort would pick this row before
        // `other` if the live veto ever broke - at quiet 0 the floor alone
        // would save it, and the veto itself would go untested.
        let live = plant(&env.fno_base, "00", "aaaa11", 2 * 3600, false);
        let other = plant(&env.fno_base, "00", "other01", 3600, false);
        let unit = crate::reclaim::tree_bytes(&live);
        std::env::set_var("FNO_CARGO_FREE_BYTES", unit.to_string());
        std::env::set_var("FNO_TEST_LIVE_CARGO_CWDS", &env.root);

        let rep = sweep(&env.root, true, SystemTime::now());

        assert!(
            live.exists(),
            "a live cargo process's own build dir is never a cap candidate"
        );
        assert!(
            !other.exists(),
            "an unrelated row still goes under cap pressure"
        );
        assert_eq!(rep.reaped, 1, "{rep:?}");
        assert!(rep.lines.iter().any(|l| l.contains("kept lane=cargo-live")));
        assert!(rep.lines.iter().any(|l| l.contains("reaped lane=cap")));
        let summary = rep.lines.last().unwrap();
        assert!(summary.contains("cap_exceeded=true"), "{summary}");
        assert!(summary.contains("cap_held=cargo-live:1"), "{summary}");
    }

    /// AC-ERR: a live cargo cwd whose OWN tree cannot answer its manifests
    /// makes the live read itself untrustworthy - an empty live set is then
    /// not proof nothing is live. The cap lane must fail closed to
    /// `FRESH_SECS`, not fall back to the far shorter `CAP_MIN_QUIET_SECS`
    /// floor a trustworthy read would use.
    #[test]
    fn an_unreadable_live_tree_falls_the_cap_floor_back_to_fresh_secs() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("livefail", "broken");
        std::fs::create_dir_all(env.root.join("crates/broken")).unwrap();
        std::fs::write(
            env.root.join("crates/broken/Cargo.toml"),
            "[package]\nname = 'broken'\nversion = '0.1.0'\n",
        )
        .unwrap();
        // A live cargo cwd inside `env.root` itself: `answer_tree` cannot
        // answer this tree (the broken manifest), so `live_shards` comes
        // back `Err`, not an empty `Ok`.
        std::env::set_var("FNO_TEST_LIVE_CARGO_CWDS", &env.root);
        // Quiet past CAP_MIN_QUIET_SECS but short of FRESH_SECS: a
        // trustworthy live read would let the cap lane reap this row.
        let row = plant(&env.fno_base, "00", "cafe5678", 1000, false);
        std::env::set_var("FNO_CARGO_FREE_BYTES", "0");

        let rep = sweep(&env.root, true, SystemTime::now());

        assert!(
            row.exists(),
            "an unreadable live read must fail closed to FRESH_SECS, not CAP_MIN_QUIET_SECS"
        );
        assert_eq!(rep.reaped, 0, "{rep:?}");
    }

    /// Defense in depth: with no live cargo process detected at all (`lsof`
    /// absent, an odd process name, the test seam left empty), a row touched
    /// two minutes ago is still never a cap candidate - `CAP_MIN_QUIET_SECS`
    /// is the backstop for whatever the live-cargo check misses. A lock gap
    /// must never read as idle.
    #[test]
    fn cap_never_crosses_its_own_min_quiet_floor() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("capfloor", "never-broken");
        let recent = plant(&env.fno_base, "00", "recent1", 120, false);
        let other = plant(&env.fno_base, "00", "other02", 3600, false);
        let unit = crate::reclaim::tree_bytes(&recent);
        std::env::set_var("FNO_CARGO_FREE_BYTES", unit.to_string());

        let rep = sweep(&env.root, true, SystemTime::now());

        assert!(
            recent.exists(),
            "a row inside the cap's own min-quiet floor is never a cap candidate"
        );
        assert!(
            !other.exists(),
            "an unrelated row past the floor still goes under cap pressure"
        );
        assert_eq!(rep.reaped, 1, "{rep:?}");
        assert!(rep.lines.iter().any(|l| l.contains("kept lane=fresh")));
        assert!(rep.lines.iter().any(|l| l.contains("reaped lane=cap")));
    }

    /// Run git in `dir`, panicking with git's own stderr when it fails.
    /// Ambient config is pinned to `/dev/null` so a signing key or template
    /// on this machine can never make the fixture commit fail.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} in {dir:?} did not run: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} in {dir:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    /// The bug this guards: `live_shards` matched a live cwd against the
    /// FIRST registered tree whose path prefixes it, which for a worktree
    /// nested inside its own checkout is always the outer, main checkout -
    /// `git worktree list` prints that one first. A cargo
    /// process running inside the NESTED tree then had its build dir
    /// credited to the OUTER tree, so the outer tree's row was the one kept
    /// live and the nested tree's own build dir - the one actually in use -
    /// went to the cap lane. The fix picks the LONGEST matching tree.
    #[test]
    fn cap_keeps_a_nested_worktrees_live_row_not_the_outer_trees() {
        if !git_available() {
            return;
        }
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Tag carries no "nested" substring - it would otherwise land in the
        // outer tree's own manifest path and defeat the keyed fake cargo
        // below.
        let env = setup("wtlive", "never-broken");

        git(&env.root, &["init", "-q"]);
        git(&env.root, &["config", "user.email", "t@t"]);
        git(&env.root, &["config", "user.name", "t"]);
        git(&env.root, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let nested = env.root.join("wt/nested");
        git(
            &env.root,
            &[
                "worktree",
                "add",
                "-q",
                nested.to_str().unwrap(),
                "-b",
                "nested-live",
            ],
        );
        std::fs::create_dir_all(nested.join("crates/fake")).unwrap();
        std::fs::write(
            nested.join("crates/fake/Cargo.toml"),
            "[package]\nname = 'nestedpkg'\nversion = '0.1.0'\n",
        )
        .unwrap();

        // Keyed by `--manifest-path`, not by which tree invoked cargo: the
        // outer tree's manifest and the nested tree's manifest must resolve
        // to DIFFERENT build dirs, so each tree gets its own row.
        std::fs::write(
            env.root.join("bin/cargo"),
            "#!/bin/sh\n\
             manifest=\"\"\n\
             prev=\"\"\n\
             for a in \"$@\"; do\n\
             if [ \"$prev\" = \"--manifest-path\" ]; then manifest=\"$a\"; fi\n\
             prev=\"$a\"\n\
             done\n\
             case \"$manifest\" in\n\
             *nested*) fno=\"$CBD_FNO_NESTED\"; fb=\"$CBD_FB_NESTED\"; pkg=nestedpkg ;;\n\
             *) fno=\"$CBD_FNO_BASE\"; fb=\"$CBD_FB_BASE\"; pkg=basepkg ;;\n\
             esac\n\
             if [ -n \"$CARGO_BUILD_BUILD_DIR\" ]; then\n\
             printf '{\"build_directory\":\"%s\",\"packages\":[{\"name\":\"%s\"}]}\\n' \"$fno\" \"$pkg\"\n\
             else\n\
             printf '{\"build_directory\":\"%s\",\"packages\":[{\"name\":\"%s\"}]}\\n' \"$fb\" \"$pkg\"\n\
             fi\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            env.root.join("bin/cargo"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        // Both under the fno base, so ownership never depends on membership.
        // The nested row is the OLDER (larger quiet) of the two, so the cap
        // lane's oldest-first sort tries it before the outer row - the case
        // that would go untested if the live veto were checked only after
        // the min-quiet floor already saved it.
        let outer_row = plant(&env.fno_base, "00", "aaaa11", 3600, false);
        let nested_row = plant(&env.fno_base, "00", "bbbb22", 2 * 3600, false);
        std::env::set_var("CBD_FNO_BASE", &outer_row);
        std::env::set_var("CBD_FNO_NESTED", &nested_row);
        std::env::set_var("CBD_FB_BASE", env.fb_base.join("00").join("cccc33"));
        std::env::set_var("CBD_FB_NESTED", env.fb_base.join("00").join("dddd44"));
        let unit = crate::reclaim::tree_bytes(&nested_row);
        std::env::set_var("FNO_CARGO_FREE_BYTES", unit.to_string());
        // The live cwd is the nested tree's own path - a `starts_with`
        // prefix match for BOTH the outer tree and the nested tree, so only
        // the longest match may claim it.
        std::env::set_var("FNO_TEST_LIVE_CARGO_CWDS", &nested);

        let rep = sweep(&env.root, true, SystemTime::now());

        std::env::remove_var("CBD_FNO_BASE");
        std::env::remove_var("CBD_FNO_NESTED");
        std::env::remove_var("CBD_FB_BASE");
        std::env::remove_var("CBD_FB_NESTED");

        assert!(
            nested_row.exists(),
            "the nested worktree's own live build dir must survive the cap"
        );
        assert!(
            !outer_row.exists(),
            "the outer tree's unrelated row still goes under cap pressure"
        );
        assert_eq!(rep.reaped, 1, "{rep:?}");
        assert!(rep.lines.iter().any(|l| l.contains("kept lane=cargo-live")));
        assert!(rep.lines.iter().any(|l| l.contains("reaped lane=cap")));
    }

    /// AC4-HP: a dry run that plans one age reap reports the bytes it WOULD
    /// reclaim, not the bytes left behind.
    #[test]
    fn dry_run_projects_reclaimable_bytes() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("capproj", "never-broken");
        let aged = plant(&env.fno_base, "00", "cafe0003", 4 * 24 * 3600, false);
        let unit = crate::reclaim::tree_bytes(&aged);

        let rep = sweep(&env.root, false, SystemTime::now());

        assert!(aged.exists(), "a dry run deletes nothing");
        assert_eq!(rep.projected_bytes, unit, "{rep:?}");
        assert_eq!(rep.after_bytes, 0, "after_bytes is the bytes left");
    }

    fn sj_entry(
        name: &str,
        cwd: &Path,
        node: Option<&str>,
        status: crate::AgentStatus,
    ) -> crate::state::RegistryEntry {
        crate::state::RegistryEntry {
            name: name.to_string(),
            cwd: cwd.display().to_string(),
            node: node.map(str::to_string),
            status,
            ..Default::default()
        }
    }

    fn sj_registry(entries: Vec<crate::state::RegistryEntry>) -> crate::state::Registry {
        crate::state::Registry {
            entries,
            ..Default::default()
        }
    }

    /// AC2-HP: a busy registry row whose cwd sits in tree T joins T to the
    /// row's node and name.
    #[test]
    fn session_join_maps_a_busy_row_to_its_tree() {
        let root = temp_root("sjhp");
        let tree = root.join("wt/a");
        std::fs::create_dir_all(&tree).unwrap();
        let reg = sj_registry(vec![sj_entry(
            "w1",
            &tree,
            Some("x-test"),
            crate::AgentStatus::Busy,
        )]);

        let join = session_join(&[root.clone(), tree.clone()], &reg);

        let h = join.get(&phys(&tree)).expect("the tree joins");
        assert!(h.nodes.contains("x-test"), "{h:?}");
        assert!(h.sessions.contains("w1"), "{h:?}");
        assert!(
            !join.contains_key(&phys(&root)),
            "no row's cwd falls in root, so root holds nobody"
        );
    }

    /// AC2-EDGE: a terminal row contributes its node but never its name; a
    /// cwd nested inside root joins the NESTED tree, not the outer root.
    #[test]
    fn session_join_drops_terminal_names_and_joins_nested_cwds_to_the_nested_tree() {
        let root = temp_root("sjedge");
        let tree = root.join("wt/a");
        let nested = root.join("wt/a/wt/nested");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        let reg = sj_registry(vec![
            sj_entry("done1", &tree, Some("x-done"), crate::AgentStatus::Exited),
            sj_entry("w1", &tree, Some("x-test"), crate::AgentStatus::Busy),
            sj_entry("w2", &nested, Some("x-nested"), crate::AgentStatus::Busy),
        ]);

        let join = session_join(&[root.clone(), tree.clone(), nested.clone()], &reg);

        let h = join.get(&phys(&tree)).expect("the tree joins");
        assert!(
            h.nodes.contains("x-done") && h.nodes.contains("x-test"),
            "{h:?}"
        );
        assert!(h.sessions.contains("w1"), "{h:?}");
        assert!(
            !h.sessions.contains("done1"),
            "a terminal row never reads as a live session: {h:?}"
        );
        let hn = join.get(&phys(&nested)).expect("the nested tree joins");
        assert!(
            hn.sessions.contains("w2") && hn.nodes.contains("x-nested"),
            "{hn:?}"
        );
        assert!(
            !hn.sessions.contains("w1"),
            "the outer tree's session must not leak into the nested tree"
        );
        assert!(!join.contains_key(&phys(&root)), "{join:?}");
    }

    /// AC1-HP: a row of a resolved tree prints `owner=<tree>`.
    #[test]
    fn a_resolved_row_prints_its_owner() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("ownerhp", "never-broken");
        plant(&env.fno_base, "00", "aaaa11", seven_h(), true);

        let rep = sweep(&env.root, false, SystemTime::now());

        let owner = phys(&env.root).display().to_string();
        assert!(
            rep.lines
                .iter()
                .any(|l| { l.contains("path=") && l.contains(&format!("owner={owner}")) }),
            "{:?}",
            rep.lines
        );
    }

    /// AC2-ERR: an unread join prints `node=unread session=unread`, never
    /// `none` - `none` is what the orphan lane and a person act on.
    #[test]
    fn an_unread_join_prints_unread_never_none() {
        let dir = temp_root("unread");
        std::fs::create_dir_all(&dir).unwrap();
        let row = Row {
            path: dir.join("base/00/cafe0001"),
            bytes: 1,
            quiet: Duration::from_secs(7 * 3600),
            under_fno: false,
            owner: None,
            membership: false,
        };

        let line = row_line("kept", "within-age", &row, &HolderView::Unread);

        assert!(line.contains("owner=none"), "{line}");
        assert!(line.contains("node=unread session=unread"), "{line}");
        assert!(!line.contains("node=none"), "{line}");
    }

    /// AC1-ERR: a dir no tree resolves still reaps by the orphan lane and its
    /// line reads `owner=none`.
    #[test]
    fn an_orphan_row_prints_owner_none() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("ownernone", "never-broken");
        plant(&env.fb_base, "00", "cafefe12", seven_h(), true);

        let rep = sweep(&env.root, false, SystemTime::now());

        assert_eq!(rep.orphans, 1);
        assert!(
            rep.lines
                .iter()
                .any(|l| { l.contains("would-reap lane=orphan") && l.contains("owner=none") }),
            "{:?}",
            rep.lines
        );
    }

    /// AC1-EDGE: with the orphan lane disabled by one failing manifest, rows
    /// of the tree that did resolve still print their owner and unresolved
    /// rows still print `owner=none`.
    #[test]
    fn a_disabled_orphan_lane_still_prints_owners() {
        if !git_available() {
            return;
        }
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The outer tree's manifest (crates/fake) fails ONLY its fno-base
        // answer, so the tree fails and the orphan lane disables while the
        // env-free answer still admits the fallback base. The nested tree's
        // crates/real resolves both ways.
        let env = setup("owneredge", "never-broken");
        std::fs::write(
            env.root.join("bin/cargo"),
            "#!/bin/sh\n\
             manifest=\"\"\n\
             prev=\"\"\n\
             for a in \"$@\"; do\n\
             if [ \"$prev\" = \"--manifest-path\" ]; then manifest=\"$a\"; fi\n\
             prev=\"$a\"\n\
             done\n\
             case \"$manifest\" in\n\
             *fake*) [ -n \"$CARGO_BUILD_BUILD_DIR\" ] && exit 1 ;;\n\
             esac\n\
             if [ -n \"$CARGO_BUILD_BUILD_DIR\" ]; then\n\
             printf '{\"build_directory\":\"%s\",\"packages\":[{\"name\":\"fakepkg\"}]}\\n' \"$CBD_FNO_ANSWER\"\n\
             else\n\
             printf '{\"build_directory\":\"%s\",\"packages\":[{\"name\":\"fakepkg\"}]}\\n' \"$CBD_FB_ANSWER\"\n\
             fi\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            env.root.join("bin/cargo"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        git(&env.root, &["init", "-q"]);
        git(&env.root, &["config", "user.email", "t@t"]);
        git(&env.root, &["config", "user.name", "t"]);
        git(&env.root, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let nested = env.root.join("wt/nested");
        git(
            &env.root,
            &[
                "worktree",
                "add",
                "-q",
                nested.to_str().unwrap(),
                "-b",
                "nested-owner",
            ],
        );
        std::fs::create_dir_all(nested.join("crates/real")).unwrap();
        std::fs::write(
            nested.join("crates/real/Cargo.toml"),
            "[package]\nname = 'realpkg'\nversion = '0.1.0'\n",
        )
        .unwrap();
        let resolved = plant(&env.fno_base, "00", "aaaa11", seven_h(), true);
        let unresolved = plant(&env.fb_base, "00", "cafefe12", seven_h(), true);

        let rep = sweep(&env.root, false, SystemTime::now());

        assert!(rep.orphan_lane.is_some(), "the outer manifest failed");
        let nested_owner = phys(&nested).display().to_string();
        assert!(
            rep.lines
                .iter()
                .any(|l| { l.contains("path=") && l.contains(&format!("owner={nested_owner}")) }),
            "the resolved tree's row names its owner: {:?}",
            rep.lines
        );
        let unresolved_line = rep
            .lines
            .iter()
            .find(|l| l.contains("cafefe12"))
            .expect("the unresolved row still prints");
        assert!(unresolved_line.contains("owner=none"), "{unresolved_line}");
        assert!(
            resolved.exists() && unresolved.exists(),
            "a dry run deletes nothing"
        );
    }

    /// AC3-HP + AC3-ERR: source dirs named `target` that carry a CACHEDIR.TAG
    /// survive a maximum-pressure apply sweep and remove_for, and no row line
    /// names any of them.
    #[test]
    fn sweep_never_touches_a_source_dir_named_target() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup("safetargets", "never-broken");
        // Cap of 1 byte: every lane is under maximum pressure.
        std::env::set_var("FNO_CARGO_FREE_BYTES", "1");
        let sources = [
            env.root.join("cli/src/fno/target"),
            env.root.join("skills/target"),
            env.root.join("tests/target"),
        ];
        for src in &sources {
            std::fs::create_dir_all(src).unwrap();
            std::fs::write(
                src.join(CACHEDIR_TAG),
                b"Signature: 8a477f597d28d172789f068868ba2775\n",
            )
            .unwrap();
            std::fs::write(src.join("payload"), vec![0u8; 4096]).unwrap();
            age_every(src, seven_h());
        }
        // Real build rows so the lanes have work to do under the 1-byte cap.
        let under_fno = plant(&env.fno_base, "00", "aaaa11", seven_h(), true);
        let under_fb = plant(&env.fb_base, "00", "bbbb22", seven_h(), true);

        let rep = sweep(&env.root, true, SystemTime::now());
        let _ = remove_for(&env.root);

        for src in &sources {
            assert!(src.exists(), "{} must survive", src.display());
            assert!(
                src.join("payload").is_file(),
                "{} payload must survive",
                src.display()
            );
        }
        assert!(
            !under_fno.exists() && !under_fb.exists(),
            "the lanes actually ran under the 1-byte cap"
        );
        assert!(
            rep.lines.iter().all(|l| !l.contains("/target")),
            "no row line names a source dir named target: {:?}",
            rep.lines
        );
    }
}
