//! PostToolUse diff-budget: the plan's diff budget prints at the commit,
//! not at review.
//!
//! A branch that overruns its diff budget used to learn so in the review
//! round, after the growth was fully built. This entry fires on the Bash
//! call that just committed, reads the bound plan's budget (plan frontmatter
//! `diff_budget`, else `plan.default_diff_budget`), and prints cumulative
//! added/removed lines against it as additionalContext the model reads in
//! the same turn. At 70 percent it repeats the simplicity rule and asks for
//! a smaller design before the next task. It never blocks: the commit
//! already happened, so every finding is advice.
//!
//! Silence is the no-op: a non-commit command, a failed commit, no target
//! manifest, no resolvable budget, or an empty branch diff (a commit on the
//! base branch) all print nothing. Shell shim: `hooks/diff-budget-commit.sh`.

use serde_json::Value;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The share of the budget at which the simplicity rule prints.
const WARN_PCT: u64 = 70;

/// The PostToolUse side: one budget line when this Bash call committed.
pub fn run(_args: &[String]) -> i32 {
    let payload: Value = serde_json::from_str(super::read_stdin().trim()).unwrap_or(Value::Null);
    if let Some(line) = budget_line(&payload) {
        let context = serde_json::to_string(&line).unwrap_or_default();
        if !context.is_empty() {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(
                stdout,
                "{{\"hookSpecificOutput\":{{\"hookEventName\":\"PostToolUse\",\"additionalContext\":{context}}}}}"
            );
        }
    }
    0
}

/// The message for this fire, or None when nothing applies. Every failure
/// path inside is silent: an unreadable manifest, an unresolvable base or a
/// 0 budget are none of a post-commit hook's business.
fn budget_line(payload: &Value) -> Option<String> {
    if !committed(payload) {
        return None;
    }
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    let manifest = manifest_path(&cwd)?;
    let budget = resolve_budget(&manifest, &cwd)?;
    if budget == 0 {
        return None;
    }
    let (added, removed) = branch_numstat(&cwd)?;
    if added + removed == 0 {
        return None; // a commit on the base branch measures nothing
    }
    let branch = git_out(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
    Some(build_message(added, removed, budget, &branch))
}

/// The one budget line, from measured numbers. Pure so the threshold
/// contract is testable without a repo: 70 percent names the simplicity
/// rule and a smaller design, 100 names the refactor-in-this-PR remedy.
fn build_message(added: u64, removed: u64, budget: u32, branch: &str) -> String {
    let pct = added * 100 / u64::from(budget);
    let mut line = format!(
        "diff budget: +{added}/-{removed} of +{budget} added lines ({pct}%) since the merge base on {branch}."
    );
    if pct >= 100 {
        line.push_str(
            " Budget blown: refactor growth away in THIS PR before the next task; the review round reads this budget and a blown one invites a blocking finding.",
        );
    } else if pct >= WARN_PCT {
        line.push_str(
            " Minimum code that solves the problem, no speculative features: at 70 percent of the diff budget, ask for a smaller design before the next task.",
        );
    }
    line
}

/// True when this Bash call was a `git commit` that succeeded. The word
/// check reads the char after the phrase so `git commit-tree` never fires.
fn committed(payload: &Value) -> bool {
    let Some(command) = payload
        .get("tool_input")
        .and_then(|ti| ti.get("command"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    if !is_commit_command(command) {
        return false;
    }
    let exit = payload
        .get("tool_response")
        .and_then(|r| r.get("exit_code").or_else(|| r.get("exitCode")))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    exit == 0
}

fn is_commit_command(command: &str) -> bool {
    let mut from = 0;
    while let Some(at) = command[from..].find("git commit") {
        let start = from + at + "git commit".len();
        match command[start..].chars().next() {
            None => return true,
            Some(c) if c.is_alphanumeric() || c == '-' || c == '_' => {}
            Some(_) => return true,
        }
        from = start;
    }
    false
}

/// The live target manifest for this cwd: the space's worktree row, else
/// the legacy in-tree copy. None outside a target session.
fn manifest_path(cwd: &Path) -> Option<PathBuf> {
    let wt = crate::paths::worktree_space_dir(cwd).join("target-state.md");
    if wt.exists() {
        return Some(wt);
    }
    let legacy = crate::paths::worktree_repo_root(cwd)
        .join(".fno")
        .join("target-state.md");
    legacy.exists().then_some(legacy)
}

/// The effective budget: the bound plan's frontmatter `diff_budget` when a
/// plan is bound and declares one (0 = the plan turns the guard off), else
/// the configured default.
fn resolve_budget(manifest: &Path, cwd: &Path) -> Option<u32> {
    plan_path(manifest, cwd)
        .and_then(|p| plan_frontmatter_budget(&p))
        .or_else(|| Some(crate::agents_config::default_diff_budget(cwd)))
}

/// The manifest's `plan_path` value, unquoted; None while empty. A relative
/// path resolves against the worktree root, where plans live (the `internal/`
/// vault link is a checkout child), never against the space root.
fn plan_path(manifest: &Path, cwd: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(manifest).ok()?;
    for line in text.lines() {
        let Some(value) = line.strip_prefix("plan_path:") else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim();
        if value.is_empty() {
            return None;
        }
        let path = PathBuf::from(value);
        return Some(if path.is_absolute() {
            path
        } else {
            let root = crate::paths::worktree_repo_root(cwd);
            if root.is_dir() {
                root.join(&path)
            } else {
                path
            }
        });
    }
    None
}

/// The plan frontmatter's `diff_budget` key. A missing or unparseable key
/// is None so the config default answers; `diff_budget: 0` reads as the
/// plan turning the guard off.
fn plan_frontmatter_budget(plan: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(plan).ok()?;
    let mut closed = false;
    for line in text.lines() {
        if !closed {
            if line.trim() == "---" {
                if closed {
                    break;
                }
                closed = true;
                continue;
            }
            continue;
        }
        if line.trim() == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix("diff_budget:") {
            return value.trim().parse::<u32>().ok();
        }
    }
    None
}

/// Cumulative added/removed lines for the branch against its merge base
/// with origin/main (main when origin has none). None outside a repo, on a
/// base that does not resolve, or when git fails.
fn branch_numstat(cwd: &Path) -> Option<(u64, u64)> {
    let base = ["origin/main", "main"]
        .iter()
        .find(|r| !git_out(cwd, &["rev-parse", "--verify", "--quiet", r]).is_empty())?;
    let merge_base = git_out(cwd, &["merge-base", base, "HEAD"]);
    if merge_base.is_empty() {
        return None;
    }
    let out = git_out(cwd, &["diff", "--numstat", &format!("{merge_base}..HEAD")]);
    let mut added = 0u64;
    let mut removed = 0u64;
    for row in out.lines() {
        let mut parts = row.split('\t');
        match (parts.next(), parts.next()) {
            (Some(a), Some(d)) if a != "-" => {
                added += a.parse::<u64>().unwrap_or(0);
                removed += d.parse::<u64>().unwrap_or(0);
            }
            _ => {}
        }
    }
    Some((added, removed))
}

fn git_out(cwd: &Path, args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| {
            if o.status.success() {
                String::from_utf8_lossy(&o.stdout).trim().to_string()
            } else {
                String::new()
            }
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn commit_detection_reads_the_word_boundary() {
        assert!(is_commit_command("git commit -m \"fix: x\""));
        assert!(is_commit_command("cd /repo && git commit --amend"));
        assert!(!is_commit_command("git commit-tree HEAD^{tree}"));
        assert!(!is_commit_command("git add -A && git status"));
        assert!(!is_commit_command("cargo commitish-not-a-verb"));
    }

    #[test]
    fn plan_frontmatter_budget_reads_a_declared_budget() {
        let dir = std::env::temp_dir().join("fno-diff-budget-test");
        std::fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("plan.md");
        std::fs::write(&plan, "---\ntitle: t\ndiff_budget: 100\n---\n# body\n").unwrap();
        assert_eq!(plan_frontmatter_budget(&plan), Some(100));
        std::fs::write(&plan, "---\ntitle: t\n---\n# body\n").unwrap();
        assert_eq!(plan_frontmatter_budget(&plan), None);
        let _ = std::fs::remove_file(&plan);
    }

    #[test]
    fn seventy_five_of_hundred_prints_the_design_line() {
        // The named verify: a fixture plan with budget 100 and a 75-line
        // commit prints the 70 percent line.
        let line = build_message(75, 10, 100, "feature/x");
        assert!(line.contains("(75%)"), "{line}");
        assert!(
            line.contains("ask for a smaller design before the next task"),
            "{line}"
        );
        // Under the threshold the advice stays silent; at the cap it names
        // the refactor remedy instead.
        assert!(!build_message(30, 5, 100, "feature/x").contains("smaller design"));
        let blown = build_message(120, 0, 100, "feature/x");
        assert!(blown.contains("refactor growth away in THIS PR"), "{blown}");
    }

    #[test]
    fn silence_without_a_target_manifest() {
        let payload = json!({
            "cwd": std::env::temp_dir().join("fno-diff-budget-test-missing"),
            "tool_input": {"command": "git commit -m x"},
            "tool_response": {"exit_code": 0}
        });
        // No manifest under a missing cwd: silent, which is also the guard
        // against measuring a non-target session.
        assert_eq!(budget_line(&payload), None);
    }
}
