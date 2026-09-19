//! The stranded-tree read: which linked worktrees hold uncommitted or
//! unpushed work under an open, king-priority node that no live session
//! drives. Report-only fuel for the `stranded_tree` queue; it never removes
//! a tree (the reaper's contract stays untouched). The node id comes from
//! the tree basename or its `feature/<id>` branch - the two spellings
//! `worktree ensure` and `target start` mint - so a node-keyed door
//! (`fno do target start`, the spawn seam, `advance`) resumes the same tree.

use super::budget::{run_with_timeout, RunFailure};
use super::queues::NODE_ID_BODY;
use super::{s_str, Budget, SourceRead, KING_PRIORITIES};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

/// A tree idle at least this long with work at risk reads stranded. A const,
/// not a knob: a knob comes when someone needs to tune it.
pub(crate) const STRANDED_GRACE_MINUTES: i64 = 60;

const BUDGET_EXHAUSTED: &str = "stranded tree probe: killed at its slice of the board budget";

/// One linked tree: its path and its full branch ref (`None` = detached).
type TreeEntry = (PathBuf, Option<String>);

/// (tree path, branch) per linked tree, in porcelain order, minus the first
/// (main) entry. Bare registrations parse but never surface. `branch` keeps
/// the full ref (`refs/heads/feature/x-eeee`); `None` is detached.
pub(crate) fn parse_worktree_porcelain(text: &str) -> Vec<TreeEntry> {
    // One slot per porcelain entry, bare entries included, so the skip(1)
    // below drops the MAIN entry whatever it is: filtering bare entries out
    // first would drop the first LINKED tree on a bare-main repo instead.
    let mut entries: Vec<Option<TreeEntry>> = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut bare = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(p) = path.take() {
                entries.push(if bare { None } else { Some((p, branch.take())) });
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
        entries.push(if bare { None } else { Some((p, branch.take())) });
    }
    entries.into_iter().skip(1).flatten().collect()
}

/// The node id a tree belongs to, and whether `worktree ensure --name <id>`
/// can find it again. Exact basename or `feature/`-stripped branch -> the id
/// with `resumable = true`; a basename that EXTENDS a node id (`x-2222-prep`)
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
    // Successively shorter prefixes at `-` boundaries: `x-2222-prep` finds
    // `x-2222`; a name with no node-id prefix (`worker-04`) finds nothing.
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

/// One tree's work-at-risk read: dirty count, unpushed count, and idle
/// minutes since the newest write among the index, the reflog, and each
/// dirty path. Every git call takes its own slice of the board's shared
/// deadline, so three probes can never sum past the one whole-board budget;
/// a budget kill propagates as over-budget (never a clean zero), any other
/// git failure fails the tree.
fn run_git(args: &[String], cwd: &Path, slice: Duration) -> Result<String, RunFailure> {
    run_with_timeout(args, cwd, slice).map(|out| String::from_utf8_lossy(&out).into_owned())
}

fn probe_tree(
    tree: &Path,
    branch: &Option<String>,
    budget: &mut Budget,
) -> Result<Value, RunFailure> {
    let mut git = |args: Vec<&str>, budget: &mut Budget| -> Result<String, RunFailure> {
        let slice = match budget.start("stranded tree probe") {
            Some(s) => s,
            None => return Err(RunFailure::KilledAtSlice(BUDGET_EXHAUSTED.to_string())),
        };
        let mut cmd: Vec<String> = vec![
            "git".to_string(),
            "-C".to_string(),
            tree.to_string_lossy().into_owned(),
        ];
        cmd.extend(args.into_iter().map(str::to_string));
        run_git(&cmd, Path::new("."), slice)
    };
    let dirty_text = git(vec!["status", "--porcelain"], budget)?;
    let dirty = dirty_text.lines().filter(|l| !l.trim().is_empty()).count() as i64;
    let unpushed_text = git(
        vec!["rev-list", "--count", "HEAD", "--not", "--remotes"],
        budget,
    )?;
    let unpushed: i64 = unpushed_text.trim().parse().unwrap_or(0);

    // Newest write among the git dir's index + reflog and the dirty paths.
    let mut newest: Option<std::time::SystemTime> = None;
    let git_dir_out = git(vec!["rev-parse", "--git-dir"], budget)?;
    let git_dir = tree.join(git_dir_out.trim());
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
        "tree": tree.to_string_lossy(),
        "branch": short_branch(branch),
        "dirty": dirty,
        "unpushed": unpushed,
        "idle_minutes": idle_minutes,
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
            match probe_tree(&tree, &branch, budget) {
                Ok(mut row) => {
                    if let Some(obj) = row.as_object_mut() {
                        obj.insert("id".to_string(), json!(id));
                        obj.insert("resumable".to_string(), json!(resumable));
                    }
                    rows.push(row);
                }
                Err(e) if e.over_budget() => {
                    return SourceRead::over_budget(e.message().to_string())
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

worktree /base/repo/x-eeee
HEAD abc222
branch refs/heads/feature/x-eeee

worktree /repo/bare
bare

worktree /base/repo/x-detached
HEAD abc333
detached

";
        let trees = parse_worktree_porcelain(text);
        assert_eq!(trees.len(), 2, "{trees:?}");
        assert_eq!(trees[0].0, PathBuf::from("/base/repo/x-eeee"));
        assert_eq!(trees[0].1.as_deref(), Some("refs/heads/feature/x-eeee"));
        assert_eq!(trees[1].0, PathBuf::from("/base/repo/x-detached"));
        assert!(trees[1].1.is_none());
    }

    #[test]
    fn a_bare_main_entry_skips_itself_never_the_first_linked_tree() {
        let text = "\
worktree /repo/main
bare

worktree /base/repo/x-eeee
HEAD abc222
branch refs/heads/feature/x-eeee

";
        let trees = parse_worktree_porcelain(text);
        assert_eq!(trees.len(), 1, "{trees:?}");
        assert_eq!(trees[0].0, PathBuf::from("/base/repo/x-eeee"));
    }

    #[test]
    fn node_id_comes_from_the_basename_the_branch_or_a_prefix() {
        assert_eq!(
            tree_node_id(Path::new("/base/footnote/x-eeee"), None),
            Some(("x-eeee".to_string(), true))
        );
        assert_eq!(
            tree_node_id(
                Path::new("/base/footnote/worker-04"),
                Some("refs/heads/feature/x-ffff")
            ),
            Some(("x-ffff".to_string(), true))
        );
        assert_eq!(
            tree_node_id(Path::new("/base/repo/x-2222-prep"), None),
            Some(("x-2222".to_string(), false))
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
            json!({"id": "x-eeee", "priority": "p1", "status": "ready"}),
            json!({"id": "x-9999", "priority": "p2", "status": "in_progress"}),
            json!({"id": "x-dddd", "priority": "p1", "status": "done"}),
            json!({"id": "x-defer", "priority": "p1", "status": "deferred"}),
        ];
        let candidates = stranded_candidates(&entries);
        assert!(candidates.contains("x-eeee"));
        assert!(!candidates.contains("x-9999"));
        assert!(!candidates.contains("x-dddd"));
        assert!(!candidates.contains("x-defer"));
    }
}
