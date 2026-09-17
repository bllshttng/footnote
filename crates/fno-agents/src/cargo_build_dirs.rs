//! Which cargo build-base hash dirs may go, for BOTH bases whatever the
//! caller's env. Cargo writes intermediates at `<base>/<h2>/<hash>` under
//! `build.build-dir`; the tracked `.cargo/config.toml` template names
//! `{cargo-cache-home}/build/{workspace-path-hash}`, so any cargo run without
//! `CARGO_BUILD_BUILD_DIR` lands in `~/.cargo/build` and every env-dependent
//! resolver only ever saw the base its own env named. This module answers
//! env-independently: it resolves every workspace manifest both ways and
//! classifies each tagged hash dir through four lanes (fresh, orphan, age,
//! cap), guarded by an exclusive `flock` on each profile's `.cargo-lock`
//! before any delete. Rows are matched by `CACHEDIR.TAG`, base, and member
//! fingerprint - never by name.
//!
//! Surfaced as the `cargo_build_dirs` lane of `fno doctor reclaim`, the
//! `fno-agents reclaim cargo-build-dirs` / `remove-for` subcommands, and the
//! in-process `remove_for` the merge reaper calls.
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// A build quieter than this is live: cargo touched it inside the window.
const FRESH_SECS: u64 = 6 * 3600;
/// An owned build quiet for 3 days is reaped by the age lane.
const AGE_SECS: u64 = 3 * 24 * 3600;
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

/// `build_directory` and package names for one manifest, with
/// `CARGO_BUILD_BUILD_DIR` forced to `<base>/{workspace-path-hash}` (`Some`)
/// or removed (`None`) for the call, so both bases answer from the same env.
/// `Err` carries the manifest path: the receipt names what could not answer.
pub(crate) fn resolve(
    manifest: &Path,
    fno_base: Option<&Path>,
) -> Result<(PathBuf, Vec<String>), String> {
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
    let dir = PathBuf::from(
        value
            .get("build_directory")
            .and_then(Value::as_str)
            .ok_or_else(|| manifest.display().to_string())?,
    );
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
    Ok((dir, names))
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
            let (dir, pkg_names) = resolve(&manifest, base)?;
            dirs.insert(dir);
            names.extend(pkg_names);
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
    let Ok((dir, _)) = resolve(&manifest, None) else {
        return reject(&mut bases);
    };
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
    if trees.iter().any(|tree| base.starts_with(tree)) {
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

/// Last gate before a delete. `recheck_quiet` (the sweep path: classification
/// and deletion are separate walks, and a row touched in between is being
/// written) refuses rows quiet under the fresh window; the tree-removal path
/// (`remove_for`) skips it - a tree being removed was active until now, so
/// the flock is the build-in-progress test there. `flock(LOCK_EX |
/// LOCK_NB)` on every profile's `.cargo-lock`: any held lock keeps the row.
/// Lock fds stay open until the removal returns so the guard cannot be
/// released underneath it.
fn guard_remove(dir: &Path, now: SystemTime, recheck_quiet: bool) -> Result<(), &'static str> {
    if recheck_quiet && quiet_of(dir, now) < Duration::from_secs(FRESH_SECS) {
        return Err("build-in-progress");
    }
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
    std::fs::remove_dir_all(dir).map_err(|_| "delete-failed")
}

fn remove_empty_shard(dir: &Path) -> bool {
    if let Some(shard) = dir.parent() {
        return std::fs::remove_dir(shard).is_ok();
    }
    false
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
    let mut resolved: BTreeSet<PathBuf> = BTreeSet::new();
    for tree in trees {
        match answer_tree(&tree, &fno_base) {
            Ok(answer) => {
                rep.trees_resolved += 1;
                names.extend(answer.names);
                resolved.extend(answer.dirs.iter().map(|d| phys(d)));
            }
            Err(manifest) => {
                rep.orphan_lane.get_or_insert(manifest);
            }
        }
    }

    let mut rows: Vec<Row> = Vec::new();
    for base in &bases {
        let (paths, _) = inventory(base);
        for path in paths {
            let bytes = crate::reclaim::tree_bytes(&path);
            let quiet = quiet_of(&path, now);
            let membership = has_membership(&path, &names);
            rows.push(Row {
                under_fno: phys(&path).starts_with(phys(&fno_base)),
                path,
                bytes,
                quiet,
                membership,
            });
        }
    }
    rep.rows = rows.len();
    let before_bytes: u64 = rows.iter().map(|r| r.bytes).sum();

    // Lanes, first match wins: fresh, orphan, age. Cap runs after, over
    // whatever is still standing.
    enum Decision {
        Keep(&'static str),
        Reap(&'static str),
    }
    let mut decisions: Vec<Decision> = Vec::with_capacity(rows.len());
    let mut planned: Vec<usize> = Vec::new();
    let mut planned_bytes: u64 = 0;
    for (i, row) in rows.iter().enumerate() {
        let decision = if row.quiet < Duration::from_secs(FRESH_SECS) {
            Decision::Keep("fresh")
        } else if rep.orphan_lane.is_none()
            && !resolved.contains(&phys(&row.path))
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

    // Cap: while the total still exceeds the effective ceiling, reap owned
    // rows quiet 6h or more, oldest quiet first. Fresh rows never qualify.
    let mut remaining = before_bytes.saturating_sub(planned_bytes);
    if remaining > rep.effective_cap_bytes {
        let mut candidates: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                (r.under_fno || r.membership)
                    && r.quiet >= Duration::from_secs(FRESH_SECS)
                    && !planned.contains(i)
            })
            .map(|(i, _)| i)
            .collect();
        candidates.sort_by(|a, b| rows[*b].quiet.cmp(&rows[*a].quiet));
        for i in candidates {
            if remaining <= rep.effective_cap_bytes {
                break;
            }
            planned.push(i);
            planned_bytes += rows[i].bytes;
            remaining -= rows[i].bytes;
            decisions[i] = Decision::Reap("cap");
        }
    }

    for (i, row) in rows.iter().enumerate() {
        match &decisions[i] {
            Decision::Keep(lane) => {
                let line = format!(
                    "cargo-build-dir kept lane={lane} bytes={} quiet_h={:.1} path={}",
                    row.bytes,
                    row.quiet.as_secs_f64() / 3600.0,
                    row.path.display()
                );
                println!("{line}");
                rep.lines.push(line);
            }
            Decision::Reap(_) => {}
        }
    }

    for &i in &planned {
        let row = &rows[i];
        let lane = match &decisions[i] {
            Decision::Reap(lane) => *lane,
            Decision::Keep(_) => unreachable!("planned rows are reaps"),
        };
        if !apply {
            let line = format!(
                "cargo-build-dir would-reap lane={lane} bytes={} quiet_h={:.1} path={}",
                row.bytes,
                row.quiet.as_secs_f64() / 3600.0,
                row.path.display()
            );
            println!("{line}");
            rep.lines.push(line);
            continue;
        }
        match guard_remove(&row.path, SystemTime::now(), true) {
            Ok(()) => {
                let line = format!(
                    "cargo-build-dir reaped lane={lane} bytes={} quiet_h={:.1} path={}",
                    row.bytes,
                    row.quiet.as_secs_f64() / 3600.0,
                    row.path.display()
                );
                println!("{line}");
                rep.lines.push(line);
                rep.reaped += 1;
                rep.reclaimed_bytes += row.bytes;
                if remove_empty_shard(&row.path) {
                    rep.shards_removed += 1;
                }
            }
            Err(reason) => {
                let line = format!(
                    "cargo-build-dir kept lane={reason} bytes={} quiet_h={:.1} path={}",
                    row.bytes,
                    row.quiet.as_secs_f64() / 3600.0,
                    row.path.display()
                );
                println!("{line}");
                rep.lines.push(line);
            }
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
        before_bytes.saturating_sub(planned_bytes)
    };

    let orphan_lane = match &rep.orphan_lane {
        None => "on".to_string(),
        Some(manifest) => format!("disabled:{manifest}"),
    };
    let summary = format!(
        "cargo-build-dirs mode={} bases={} rows={} trees_resolved={} orphans={} reaped={} reclaimed_bytes={} after_bytes={} effective_cap_bytes={} orphan_lane={orphan_lane} shards_removed={}",
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
                 printf '{{\"build_directory\":\"%s\",\"packages\":[{{\"name\":\"fakepkg\"}}]}}\\n' \"$CBD_FNO_ANSWER\"\n\
                 else\n\
                 printf '{{\"build_directory\":\"%s\",\"packages\":[{{\"name\":\"fakepkg\"}}]}}\\n' \"$CBD_FB_ANSWER\"\n\
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
            std::env::remove_var("FNO_CARGO_TARGETS_BASE");
            let _ = std::fs::remove_dir_all(&self.root);
            let _ = std::fs::remove_dir_all(&self.fb_parent);
        }
    }

    fn seven_h() -> u64 {
        7 * 3600
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

        std::env::set_var("PATH", &real_path);
        std::env::remove_var("CARGO_HOME");
        std::env::remove_var("CBD_FB");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found.as_deref(), Some(script.as_path()), "{found:?}");
    }
}
