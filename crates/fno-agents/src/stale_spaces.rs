//! The stale-spaces lane of `fno-agents reclaim`. The spaces root holds one
//! dir per canonical checkout that ever resolved state, plus one
//! `worktrees/<name>` slice per worktree, and nothing removed them when the
//! source went away: reaped worktrees, job tmp dirs, and per-boot temp dirs
//! each left their space behind. Measured 2026-10-08 on one install: 12,220
//! dirs, 1.8 GB, most of it temp-dir slugs whose source is long gone.
//!
//! A space is judged dead only when its source dir is provably gone. The slug
//! is the canonical path with `/` swapped for `-`, and the swap is lossy when
//! a real segment contains a dash (`my-repo` decodes three ways), so decode
//! by descent: the slug names a live dir when ANY segmentation of it walks to
//! an existing directory. Every stat must answer; a failed read or a spent
//! probe budget keeps the space.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A space whose source is gone waits this long before the lane judges it:
/// long enough that no live session can still be writing its events store
/// through a deleted cwd, short enough that the backlog ages out in days.
pub(crate) const SPACE_GRACE_SECS: u64 = 24 * 3600;

/// Stat budget per slug. The descent can branch where a path has many dash
/// segments; a slug that spends this many probes without an answer is treated
/// as unprovable, never as dead.
const MAX_PROBES_PER_SLUG: usize = 8192;

pub(crate) struct SpacesReport {
    pub(crate) reaped: Vec<PathBuf>,
    pub(crate) bytes: u64,
    pub(crate) note: String,
}

/// The descent-decode verdict for one slug.
enum Decode {
    /// At least one segmentation names an existing dir. The vec holds every
    /// full decode found, since a dash segment can decode more than one way.
    Live(Vec<PathBuf>),
    /// Every probe answered cleanly and none named an existing dir.
    Gone,
    /// A probe failed with something other than not-found, or the budget ran
    /// out. Unknown is never evidence of absence.
    Unproven,
}

/// `Some(true)` when `path` is an existing dir, `Some(false)` when it is
/// provably absent, `None` when the stat failed for any other reason.
fn dir_probe(path: &Path, probes: &mut usize) -> Option<bool> {
    *probes += 1;
    match std::fs::metadata(path) {
        Ok(meta) => Some(meta.is_dir()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

/// Decode a space slug by filesystem descent. The slug is the canonical path
/// with `/` swapped for `-`; the swap is lossy, so try every split and call
/// the slug live if any split walks to an existing directory.
fn decode_slug(slug: &str) -> Decode {
    let Some(first) = slug.strip_prefix('-') else {
        return Decode::Unproven;
    };
    let mut found: Vec<PathBuf> = Vec::new();
    let mut probes = 0usize;
    let mut stack: Vec<(PathBuf, &str)> = vec![(PathBuf::from("/"), first)];
    while let Some((prefix, rest)) = stack.pop() {
        if probes > MAX_PROBES_PER_SLUG {
            return Decode::Unproven;
        }
        if rest.is_empty() {
            match dir_probe(&prefix, &mut probes) {
                Some(true) => found.push(prefix),
                Some(false) => {}
                None => return Decode::Unproven,
            }
            continue;
        }
        let mut splits: Vec<(&str, &str)> = rest
            .match_indices('-')
            .map(|(i, _)| (&rest[..i], &rest[i + 1..]))
            .collect();
        splits.push((rest, ""));
        for (seg, tail) in splits {
            if seg.is_empty() || seg == "." || seg == ".." {
                continue;
            }
            let child = prefix.join(seg);
            match dir_probe(&child, &mut probes) {
                Some(true) => {
                    if tail.is_empty() {
                        found.push(child);
                    } else {
                        stack.push((child, tail));
                    }
                }
                Some(false) => {}
                None => return Decode::Unproven,
            }
            if probes > MAX_PROBES_PER_SLUG {
                return Decode::Unproven;
            }
        }
    }
    if found.is_empty() {
        Decode::Gone
    } else {
        Decode::Live(found)
    }
}

/// Age of the newest entry anywhere under `path`, or `None` when any stat
/// failed: an unreadable probe is never evidence of quiet.
fn newest_age(path: &Path, now: SystemTime) -> Option<Duration> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let mut newest = meta.modified().ok()?;
    let mut budget = 65_536usize;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            budget -= 1;
            if budget == 0 {
                return None;
            }
            let m = std::fs::symlink_metadata(entry.path()).ok()?;
            let modified = m.modified().ok()?;
            if modified > newest {
                newest = modified;
            }
            if m.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    now.duration_since(newest).ok()
}

/// The registered worktree basenames of the first repo in `repos` that
/// answers `git worktree list --porcelain`. `None` when no repo answers: git
/// is the one authority on where a worktree lives, and an unreadable
/// authority keeps every slice.
fn registered_worktree_names(repos: &[PathBuf]) -> Option<Vec<String>> {
    for repo in repos {
        let Ok(out) = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let names = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .filter_map(|p| {
                Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .collect();
        return Some(names);
    }
    None
}

fn past_grace(path: &Path, now: SystemTime) -> bool {
    matches!(newest_age(path, now), Some(age) if age.as_secs() >= SPACE_GRACE_SECS)
}

/// Walk the spaces root and judge every dir. Dry run lists the dead ones;
/// `apply` removes them. A live source, an unprovable decode, a read error,
/// an entry inside the grace window, or a name that is not a path slug is
/// kept.
pub(crate) fn sweep(apply: bool, now: SystemTime) -> SpacesReport {
    let mut rep = SpacesReport {
        reaped: Vec::new(),
        bytes: 0,
        note: String::new(),
    };
    let root = crate::paths::spaces_root();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) => {
            rep.note = format!("spaces root unreadable: {e}");
            return rep;
        }
    };
    let (mut kept_live, mut kept_grace, mut kept_unproven, mut kept_shape) =
        (0usize, 0usize, 0usize, 0usize);
    let mut live_spaces: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            kept_shape += 1;
            continue;
        };
        // Only path slugs are judged: names like `.tmpXXXX-hex` or
        // `project-<hex>` are not minted by `space_slug`, so nothing here
        // knows their source.
        if !name.starts_with('-') {
            kept_shape += 1;
            continue;
        }
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => {
                kept_unproven += 1;
                continue;
            }
        };
        if !meta.is_dir() {
            kept_shape += 1;
            continue;
        }
        match decode_slug(&name) {
            Decode::Live(repos) => {
                kept_live += 1;
                live_spaces.push((path, repos));
            }
            Decode::Gone => {
                if past_grace(&path, now) {
                    candidates.push(path);
                } else {
                    kept_grace += 1;
                }
            }
            Decode::Unproven => kept_unproven += 1,
        }
    }
    // Worktree slices of live spaces: the slice's source is the worktree dir,
    // registered by git wherever it lives. A slice whose name no registration
    // carries and whose files are quiet past the grace is the leftover of a
    // reaped tree.
    for (space, repos) in &live_spaces {
        let Ok(slices) = std::fs::read_dir(space.join("worktrees")) else {
            continue;
        };
        let Some(registered) = registered_worktree_names(repos) else {
            continue;
        };
        for slice in slices.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(slice.path()) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            let sname = slice.file_name().to_string_lossy().into_owned();
            if registered.iter().any(|r| *r == sname) {
                continue;
            }
            if past_grace(&slice.path(), now) {
                candidates.push(slice.path());
            } else {
                kept_grace += 1;
            }
        }
    }
    for candidate in &candidates {
        rep.bytes += crate::reclaim::tree_bytes(candidate);
    }
    if apply {
        for candidate in &candidates {
            // Freshness re-check for whole spaces: the source may have come
            // back between the listing and the delete. Slices skip this; git
            // answered for them moments ago in this same pass.
            if candidate.parent() == Some(root.as_path()) {
                let Some(name) = candidate.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !matches!(decode_slug(name), Decode::Gone) {
                    continue;
                }
            }
            let _ = std::fs::remove_dir_all(candidate);
        }
    }
    rep.reaped = candidates;
    rep.note = format!(
        "kept: {kept_live} live, {kept_grace} in grace, {kept_unproven} unprovable, {kept_shape} not slugs"
    );
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reclaim::tests::ENV_LOCK;

    fn temp_spaces_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fno-stale-spaces-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn age(path: &Path, minutes: u64) {
        let old = SystemTime::now() - Duration::from_secs(minutes * 60);
        let file = std::fs::File::options().read(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }

    struct SpacesPin(PathBuf, Option<std::ffi::OsString>);

    impl SpacesPin {
        fn new(root: &Path) -> Self {
            let old = std::env::var_os("FNO_SPACES_DIR");
            std::env::set_var("FNO_SPACES_DIR", root);
            SpacesPin(root.to_path_buf(), old)
        }
    }

    impl Drop for SpacesPin {
        fn drop(&mut self) {
            match self.1.take() {
                Some(v) => std::env::set_var("FNO_SPACES_DIR", v),
                None => std::env::remove_var("FNO_SPACES_DIR"),
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn dead_source_space_is_reaped_and_live_source_is_kept() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_spaces_root("dead-live");
        let _pin = SpacesPin::new(&root);
        let now = SystemTime::now();

        let live_src = root.join("live-repo");
        std::fs::create_dir_all(&live_src).unwrap();
        let live_slug = format!("-{}", live_src.to_string_lossy().replace('/', "-"));
        let live_space = root.join(&live_slug);
        std::fs::create_dir_all(&live_space).unwrap();
        std::fs::write(live_space.join("events.db"), b"live").unwrap();

        let dead = root.join("-tmp-fno-stale-spaces-test-7q3z-dead");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(dead.join("events.db"), b"dead").unwrap();
        age(&dead, 25 * 60);

        let fresh = root.join("-tmp-fno-stale-spaces-test-7q3z-fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        age(&fresh, 5);

        let dry = sweep(false, now);
        assert!(dry.reaped.contains(&dead), "the dead space is listed");
        assert!(
            !dry.reaped.contains(&live_space),
            "the live space is never listed"
        );
        assert!(
            !dry.reaped.contains(&fresh),
            "a space inside the grace window is kept"
        );
        assert!(
            dead.exists() && live_space.exists() && fresh.exists(),
            "a dry run removes nothing"
        );

        let applied = sweep(true, now);
        assert!(applied.reaped.contains(&dead));
        assert!(!dead.exists(), "apply removes the dead space");
        assert!(live_space.exists(), "apply keeps the live space");
        assert!(
            fresh.exists(),
            "apply keeps a space inside the grace window"
        );
        assert!(
            applied.bytes > 0,
            "the receipt carries the dead space's bytes"
        );
    }

    #[test]
    fn dash_segment_slug_that_decodes_is_kept() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_spaces_root("dash");
        let _pin = SpacesPin::new(&root);
        let now = SystemTime::now();

        // `my-repo` exists, so the slug decodes through the dash segment. The
        // naive whole-string decode lands on `my/repo`, which is gone; the
        // descent must find the live split.
        let src = root.join("my-repo");
        std::fs::create_dir_all(&src).unwrap();
        let slug = format!("-{}", src.to_string_lossy().replace('/', "-"));
        let space = root.join(&slug);
        std::fs::create_dir_all(&space).unwrap();
        age(&space, 25 * 60);

        let dry = sweep(false, now);
        assert!(
            !dry.reaped.contains(&space),
            "a live dash-segment space is kept"
        );
        let applied = sweep(true, now);
        assert!(!applied.reaped.contains(&space));
        assert!(space.exists(), "apply keeps the live space");
    }

    #[test]
    fn entries_that_are_not_path_slugs_are_kept() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_spaces_root("shape");
        let _pin = SpacesPin::new(&root);
        let now = SystemTime::now();

        let latch = root.join(".tmpXXXX-4cfa15a4");
        std::fs::create_dir_all(&latch).unwrap();
        age(&latch, 25 * 60);
        let project = root.join("project-b82ad7cd");
        std::fs::create_dir_all(&project).unwrap();
        age(&project, 25 * 60);

        let dry = sweep(false, now);
        assert!(!dry.reaped.contains(&latch) && !dry.reaped.contains(&project));
        let applied = sweep(true, now);
        assert!(!applied.reaped.contains(&latch) && !applied.reaped.contains(&project));
        assert!(
            latch.exists() && project.exists(),
            "unminted names are never judged"
        );
        assert!(
            applied.note.contains("not slugs"),
            "the note names the kept shape count"
        );
    }
}
