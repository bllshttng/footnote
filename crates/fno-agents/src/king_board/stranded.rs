//! The stranded-tree read: which linked worktrees hold uncommitted or
//! unpushed work under an open, king-priority node that no live session
//! drives. Report-only fuel for the `stranded_tree` queue; it never removes
//! a tree (the reaper's contract stays untouched). The node id comes from
//! the tree basename or its `feature/<id>` branch - the two spellings
//! `worktree ensure` and `target start` mint - so a node-keyed door
//! (`fno do target start`, the spawn seam, `advance`) resumes the same tree.

use super::budget::{run_with_timeout, RunFailure};
use super::queues::NODE_ID_BODY;
use super::{is_terminal, s_str, Budget, SourceRead, KING_PRIORITIES};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

/// A tree idle at least this long with work at risk reads stranded. A const,
/// not a knob: a knob comes when someone needs to tune it.
pub(crate) const STRANDED_GRACE_MINUTES: i64 = 60;

/// (tree path, branch) per linked tree, in porcelain order, minus the first
/// (main) entry and any bare registration. `branch` keeps the full ref
/// (`refs/heads/feature/x-58e3`); `None` is detached.
pub(crate) fn parse_worktree_porcelain(text: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut trees: Vec<(PathBuf, Option<String>)> = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut bare = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(p) = path.take() {
                if !bare {
                    trees.push((p, branch.take()));
                }
            }
            path = Some(PathBuf::from(rest));
            bare = false;
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.to_string());
        } else if line.strip_prefix("bare") == Some("") {
            bare = true;
        }
    }
    if let Some(p) = path.take() {
        if !bare {
            trees.push((p, branch.take()));
        }
    }
    trees.into_iter().skip(1).collect()
}

/// The node id a tree belongs to, and whether `worktree ensure --name <id>`
/// can find it again. Exact basename or `feature/`-stripped branch -> the id
/// with `resumable = true`; a basename that EXTENDS a node id (`x-291b-prep`)
/// still names it, with `resumable = false` - only such a tree can hold the
/// node's prep commits, and the king resumes it from inside.
pub(crate) fn tree_node_id(path: &Path, branch: Option<&str>) -> Option<(String, bool)> {
    let name_re = regex::Regex::new(&format!("^{NODE_ID_BODY}$")).ok()?;
    let base = path.file_name()?.to_string_lossy().into_owned();
    if name_re.is_match(&base) {
        return Some((base, true));
    }
    if let Some(b) = branch {
        if let Some(short) = b.strip_prefix("refs/heads/feature/") {
            if name_re.is_match(short) {
                return Some((short.to_string(), true));
            }
        }
    }
    // Successively shorter prefixes at `-` boundaries: `x-291b-prep` finds
    // `x-291b`; a name with no node-id prefix (`worker-04`) finds nothing.
    let mut cut = base.len();
    while let Some(idx) = base[..cut].rfind('-') {
        let candidate = &base[..idx];
        if name_re.is_match(candidate) {
            return Some((candidate.to_string(), false));
        }
        cut = idx;
    }
    None
}

fn short_branch(branch: &Option<String>) -> String {
    branch
        .as_deref()
        .and_then(|b| b.strip_prefix("refs/heads/"))
        .unwrap_or("")
        .to_string()
}

fn run_git(args: &[&str], cwd: &Path, slice: Duration) -> Result<String, RunFailure> {
    let cmd: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    run_with_timeout(&cmd, cwd, slice).map(|out| String::from_utf8_lossy(&out).into_owned())
}

/// One tree's work-at-risk read: dirty count, unpushed count, and idle
/// minutes since the newest write among the index, the reflog, and each
/// dirty path. Any git failure fails the tree, never the source.
fn probe_tree(tree: &Path, branch: &Option<String>) -> Result<Value, String> {
    let unbounded = Duration::from_secs(600);
    let dirty_text = run_git(
        &[
            "git",
            "-C",
            &tree.to_string_lossy(),
            "status",
            "--porcelain",
        ],
        Path::new("."),
        unbounded,
    )
    .map_err(|e| e.message().to_string())?;
    let dirty = dirty_text.lines().filter(|l| !l.trim().is_empty()).count() as i64;
    let unpushed_text = run_git(
        &[
            "git",
            "-C",
            &tree.to_string_lossy(),
            "rev-list",
            "--count",
            "HEAD",
            "--not",
            "--remotes",
        ],
        Path::new("."),
        unbounded,
    )
    .map_err(|e| e.message().to_string())?;
    let unpushed: i64 = unpushed_text.trim().parse().unwrap_or(0);

    // Newest write among the git dir's index + reflog and the dirty paths.
    let mut newest: Option<std::time::SystemTime> = None;
    let git_dir_out = run_git(
        &[
            "git",
            "-C",
            &tree.to_string_lossy(),
            "rev-parse",
            "--git-dir",
        ],
        Path::new("."),
        unbounded,
    )
    .map_err(|e| e.message().to_string())?;
    let git_dir_raw = git_dir_out.trim();
    let git_dir = tree.join(git_dir_raw);
    for tail in ["index", "logs/HEAD"] {
        if let Ok(m) = std::fs::metadata(git_dir.join(tail)) {
            if let Ok(mtime) = m.modified() {
                newest = Some(newest.map_or(mtime, |n: std::time::SystemTime| n.max(mtime)));
            }
        }
    }
    for line in dirty_text.lines() {
        let rel = line.get(3..).unwrap_or("").trim();
        if rel.is_empty() {
            continue;
        }
        if let Ok(m) = std::fs::metadata(tree.join(rel)) {
            if let Ok(mtime) = m.modified() {
                newest = Some(newest.map_or(mtime, |n: std::time::SystemTime| n.max(mtime)));
            }
        }
    }
    let idle_minutes = newest
        .and_then(|n| n.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64 / 60)
        .map(|mtime_mins| {
            let now_mins = std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64 / 60)
                .unwrap_or(mtime_mins);
            (now_mins - mtime_mins).max(0)
        })
        .unwrap_or(0);

    Ok(json!({
        "id": tree_node_id(tree, branch.as_deref()).map(|(id, _)| id).unwrap_or_default(),
        "tree": tree.to_string_lossy(),
        "branch": short_branch(branch),
        "dirty": dirty,
        "unpushed": unpushed,
        "idle_minutes": idle_minutes,
        "resumable": tree_node_id(tree, branch.as_deref()).map(|(_, r)| r).unwrap_or(false),
    }))
}

/// The source read over every known repo. A budget kill marks the whole
/// source over-budget (never a clean zero); a per-tree git failure drops
/// just that tree.
pub(crate) fn read_stranded_trees(
    repos: &[PathBuf],
    candidates: &HashSet<String>,
    budget: &mut Budget,
) -> SourceRead {
    let mut rows: Vec<Value> = Vec::new();
    for repo in repos {
        let Some(slice) = budget.start("stranded trees") else {
            return SourceRead::over_budget(budget.spent_error());
        };
        let cmd = vec![
            "git".to_string(),
            "-C".to_string(),
            repo.to_string_lossy().into_owned(),
            "worktree".to_string(),
            "list".to_string(),
            "--porcelain".to_string(),
        ];
        let listing = match run_with_timeout(&cmd, Path::new("."), slice) {
            Ok(out) => String::from_utf8_lossy(&out).into_owned(),
            Err(e) if e.over_budget() => return SourceRead::over_budget(e.message().to_string()),
            Err(e) => return SourceRead::err(e.message().to_string()),
        };
        for (tree, branch) in parse_worktree_porcelain(&listing) {
            let Some((id, resumable)) = tree_node_id(&tree, branch.as_deref()) else {
                continue;
            };
            if !candidates.contains(&id) {
                continue;
            }
            match probe_tree(&tree, &branch) {
                Ok(mut row) => {
                    if let Some(obj) = row.as_object_mut() {
                        obj.insert("id".to_string(), json!(id));
                        obj.insert("resumable".to_string(), json!(resumable));
                    }
                    rows.push(row);
                }
                Err(_) => continue,
            }
        }
    }
    SourceRead::ok(Value::Array(rows))
}

/// The graph-side candidate set: king-priority nodes that are not terminal
/// and not deferred. Pure over the entries so tests can build it directly.
pub(crate) fn stranded_candidates(entries: &[Value]) -> HashSet<String> {
    entries
        .iter()
        .filter(|e| KING_PRIORITIES.contains(&s_str(e, "priority").unwrap_or("")))
        .filter(|e| !super::is_terminal(e))
        .filter(|e| {
            s_str(e, "status")
                .map(|s| s != "deferred" && !s.starts_with("deferred:"))
                .unwrap_or(false)
        })
        .filter_map(|e| s_str(e, "id").map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parses_linked_trees_and_skips_main_and_bare() {
        let text = "\
worktree /repo/main
HEAD abc111
branch refs/heads/main

worktree /base/repo/x-58e3
HEAD abc222
branch refs/heads/feature/x-58e3

worktree /repo/bare
bare

worktree /base/repo/x-detached
HEAD abc333
detached

";
        let trees = parse_worktree_porcelain(text);
        assert_eq!(trees.len(), 2, "{trees:?}");
        assert_eq!(trees[0].0, PathBuf::from("/base/repo/x-58e3"));
        assert_eq!(trees[0].1.as_deref(), Some("refs/heads/feature/x-58e3"));
        assert_eq!(trees[1].0, PathBuf::from("/base/repo/x-detached"));
        assert!(trees[1].1.is_none());
    }

    #[test]
    fn node_id_comes_from_the_basename_the_branch_or_a_prefix() {
        assert_eq!(
            tree_node_id(Path::new("/base/footnote/x-58e3"), None),
            Some(("x-58e3".to_string(), true))
        );
        assert_eq!(
            tree_node_id(
                Path::new("/base/footnote/worker-04"),
                Some("refs/heads/feature/x-7dc2")
            ),
            Some(("x-7dc2".to_string(), true))
        );
        assert_eq!(
            tree_node_id(Path::new("/repo/.claude/worktrees/x-291b-prep"), None),
            Some(("x-291b".to_string(), false))
        );
        assert_eq!(
            tree_node_id(Path::new("/base/footnote/worker-04"), None),
            None
        );
        assert_eq!(tree_node_id(Path::new("/base/footnote/notes"), None), None);
    }

    #[test]
    fn candidates_take_king_priority_live_nodes_only() {
        let entries = vec![
            json!({"id": "x-58e3", "priority": "p1", "status": "ready"}),
            json!({"id": "x-62d8", "priority": "p2", "status": "in_progress"}),
            json!({"id": "x-done", "priority": "p1", "status": "done"}),
            json!({"id": "x-def", "priority": "p1", "status": "deferred"}),
        ];
        let candidates = stranded_candidates(&entries);
        assert!(candidates.contains("x-58e3"));
        assert!(!candidates.contains("x-62d8"));
        assert!(!candidates.contains("x-done"));
        assert!(!candidates.contains("x-def"));
    }
}
