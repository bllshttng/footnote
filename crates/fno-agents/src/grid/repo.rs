//! Repo-rollup resolution for the grid rail (x-cb89, Phase 1).
//!
//! A "sideline" rolls up a repo's agents together: the canonical checkout AND
//! all of its worktrees appear under one rail entry. footnote's worktrees live
//! OUTSIDE the repo path (`~/conductor/workspaces/<repo>/<name>`), so a
//! path-prefix rollup (what `claude agents` uses) cannot work - we resolve each
//! cwd to its canonical repo root via `git --git-common-dir` (a worktree's
//! common-dir points back at the canonical `.git`).
//!
//! This is the ONLY I/O in the grouping path. [`group::group_by`] stays pure by
//! reading the `_repo_root` / `_worktree` fields this module stamps onto the
//! frozen `rail_rows` snapshot - resolved once per distinct cwd via the cache,
//! never per render frame (the Concurrency failure mode: a per-frame `git`
//! spawn would tank the render loop).
//!
//! [`group::group_by`]: super::group::group_by

use serde_json::Value;
use std::collections::HashMap;
use std::process::Command;

/// Sentinel repo-root for a cwd that is not a git repo (or git failed). Every
/// such agent shares the single "ungrouped" sideline (AC1-ERR) and is never
/// dropped from the rail.
pub const UNGROUPED: &str = "ungrouped";

/// Row field carrying the resolved canonical repo root (the grouping key).
pub const REPO_ROOT_FIELD: &str = "_repo_root";
/// Row field carrying the member label ("main" or the worktree name).
pub const WORKTREE_FIELD: &str = "_worktree";

/// Resolved repo identity for one cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// Absolute canonical repo root (parent of the common `.git`). Used as the
    /// grouping key so two repos that share a basename stay distinct (AC1-EDGE).
    pub root: String,
    /// Member label: `"main"` for the canonical checkout, else the worktree's
    /// directory name (US2 / AC1-HP).
    pub worktree: String,
}

/// The last path segment, ignoring trailing slashes. A path with no `/` returns
/// itself (so the `"ungrouped"` / `"unknown"` sentinels survive untouched).
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// The parent path (everything before the final `/`). `None` for a bare segment.
fn parent(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    trimmed
        .rfind('/')
        .map(|i| if i == 0 { "/" } else { &trimmed[..i] })
}

/// The repo root is the parent of the common `.git` dir; `--git-common-dir`
/// yields `<root>/.git` for both the main checkout and its worktrees. A bare
/// repo's common-dir is the repo dir itself (no `/.git` suffix), so fall back to
/// using it directly rather than mis-rooting at its parent.
fn repo_root_from_common_dir(common_dir: &str) -> String {
    let trimmed = common_dir.trim_end_matches('/');
    if basename(trimmed) == ".git" {
        parent(trimmed).unwrap_or(trimmed).to_string()
    } else {
        trimmed.to_string()
    }
}

/// Resolve a cwd to its canonical repo root + worktree label via one
/// `git rev-parse`. Returns `None` when the cwd is not a git repo or git fails
/// (AC1-ERR / AC1-FR) - the caller buckets such agents under [`UNGROUPED`].
pub fn resolve_repo(cwd: &str) -> Option<RepoInfo> {
    let out = Command::new("git")
        .args([
            "-C",
            cwd,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--git-dir",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let common_dir = lines.next()?.trim();
    let git_dir = lines.next()?.trim();
    if common_dir.is_empty() {
        return None;
    }
    // git-dir == common-dir for the main checkout; a linked worktree's git-dir
    // is `<common>/.git/worktrees/<name>`, so its basename is the worktree name.
    let worktree = if git_dir == common_dir || git_dir.is_empty() {
        "main".to_string()
    } else {
        basename(git_dir).to_string()
    };
    Some(RepoInfo {
        root: repo_root_from_common_dir(common_dir),
        worktree,
    })
}

/// Per-distinct-cwd resolution cache. `None` memoizes a non-git / failed cwd so
/// it is not re-spawned (AC1-FR: resolution attempted at most once).
pub type RepoCache = HashMap<String, Option<RepoInfo>>;

/// Stamp `_repo_root` + `_worktree` onto one row from its `cwd`, resolving once
/// per distinct cwd via `cache`. A row with no/empty cwd, or whose cwd fails to
/// resolve, is stamped `_repo_root = "ungrouped"` (and no `_worktree`, so the
/// renderer falls back to the agent name) - it shares the single ungrouped
/// sideline and is never dropped.
pub fn stamp_row(row: &mut Value, cache: &mut RepoCache) {
    let cwd = row
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let info = if cwd.is_empty() {
        None
    } else {
        cache
            .entry(cwd.clone())
            .or_insert_with(|| resolve_repo(&cwd))
            .clone()
    };
    let Some(obj) = row.as_object_mut() else {
        return;
    };
    match info {
        Some(RepoInfo { root, worktree }) => {
            obj.insert(REPO_ROOT_FIELD.into(), Value::String(root));
            obj.insert(WORKTREE_FIELD.into(), Value::String(worktree));
        }
        None => {
            obj.insert(REPO_ROOT_FIELD.into(), Value::String(UNGROUPED.into()));
        }
    }
}

/// Stamp every row in the frozen snapshot. Called once at rail-snapshot build
/// (and per live-added row via [`stamp_row`]), so git runs at most once per
/// distinct cwd for the whole session.
pub fn stamp_rows(rows: &mut [Value], cache: &mut RepoCache) {
    for row in rows.iter_mut() {
        stamp_row(row, cache);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::Command;

    // ── Pure path helpers ────────────────────────────────────────────────────

    #[test]
    fn basename_strips_dirs_and_trailing_slash() {
        assert_eq!(basename("/a/b/footnote"), "footnote");
        assert_eq!(basename("/a/b/footnote/"), "footnote");
        assert_eq!(basename("footnote"), "footnote");
        assert_eq!(basename(UNGROUPED), "ungrouped");
        assert_eq!(basename("unknown"), "unknown");
    }

    #[test]
    fn repo_root_strips_dot_git() {
        assert_eq!(
            repo_root_from_common_dir("/code/footnote/footnote/.git"),
            "/code/footnote/footnote"
        );
        // Bare repo: common-dir is the repo dir itself.
        assert_eq!(repo_root_from_common_dir("/srv/bare.git"), "/srv/bare.git");
    }

    // ── stamp_row: non-git fallback + caching (AC1-ERR / AC1-FR) ──────────────

    #[test]
    fn stamp_non_git_cwd_buckets_ungrouped() {
        let mut cache = RepoCache::new();
        let mut row = json!({ "name": "wkA", "cwd": "/definitely/not/a/repo/xyzzy" });
        stamp_row(&mut row, &mut cache);
        assert_eq!(row[REPO_ROOT_FIELD], json!(UNGROUPED));
        assert!(
            row.get(WORKTREE_FIELD).is_none(),
            "no worktree label on a non-git agent; renderer falls back to name"
        );
    }

    #[test]
    fn stamp_missing_cwd_buckets_ungrouped() {
        let mut cache = RepoCache::new();
        let mut row = json!({ "name": "wkA" });
        stamp_row(&mut row, &mut cache);
        assert_eq!(row[REPO_ROOT_FIELD], json!(UNGROUPED));
    }

    #[test]
    fn resolution_is_cached_per_cwd() {
        // A failed resolution memoizes `None` so the same cwd is not re-spawned
        // (AC1-FR: attempted at most once).
        let mut cache = RepoCache::new();
        let cwd = "/definitely/not/a/repo/xyzzy";
        let mut row = json!({ "name": "wkA", "cwd": cwd });
        stamp_row(&mut row, &mut cache);
        assert!(cache.contains_key(cwd), "cwd memoized after first resolve");
        assert_eq!(cache[cwd], None, "non-git cwd memoizes None");
    }

    // ── resolve_repo against a real git repo + worktree (AC1-HP) ──────────────

    fn git(args: &[&str], cwd: &std::path::Path) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git runs")
            .status
            .success();
        assert!(ok, "git {args:?} failed in {cwd:?}");
    }

    #[test]
    fn main_checkout_and_worktree_roll_up_to_one_root() {
        // AC1-HP: a repo's main checkout and a worktree (placed OUTSIDE the repo
        // path, as footnote's are) resolve to the SAME repo root, labeled `main`
        // and the worktree name respectively.
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("footnote");
        std::fs::create_dir(&repo).unwrap();
        git(&["init", "-q"], &repo);
        git(&["config", "user.email", "t@t"], &repo);
        git(&["config", "user.name", "t"], &repo);
        git(&["commit", "-q", "--allow-empty", "-m", "init"], &repo);

        // Worktree OUTSIDE the repo directory (mirrors ~/conductor/workspaces/...).
        let wt = tmp.path().join("workspaces").join("e5c-layout");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        git(
            &["worktree", "add", "-q", wt.to_str().unwrap(), "HEAD"],
            &repo,
        );

        let main = resolve_repo(repo.to_str().unwrap()).expect("main resolves");
        let leaf = resolve_repo(wt.to_str().unwrap()).expect("worktree resolves");

        // `git init` may canonicalize the path (e.g. /private on macOS); compare
        // by basename + equality across the two, which is the rollup invariant.
        assert_eq!(main.root, leaf.root, "both roll up to one repo root");
        assert_eq!(basename(&main.root), "footnote");
        assert_eq!(main.worktree, "main", "canonical checkout labeled main");
        assert_eq!(leaf.worktree, "e5c-layout", "worktree labeled by its name");
    }

    #[test]
    fn non_git_dir_resolves_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(resolve_repo(tmp.path().to_str().unwrap()), None);
    }
}
