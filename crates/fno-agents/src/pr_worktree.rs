//! Resolve a PR head branch to its checked-out worktree.
//!
//! The caller's directory identifies the repository for `git worktree list`;
//! only an exact branch match selects the returned path. A missing listing or
//! branch is a refusal, never a fallback to the caller's cwd.

use serde_json::{json, Value};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

fn parse_worktrees(stdout: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut entries = Vec::new();
    let mut path = None;
    let mut branch = None;
    for line in stdout.lines() {
        if let Some(value) = line.strip_prefix("worktree ") {
            if let Some(previous) = path.take() {
                entries.push((previous, branch.take()));
            }
            path = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
            branch = Some(value.to_string());
        } else if line.is_empty() {
            if let Some(previous) = path.take() {
                entries.push((previous, branch.take()));
            }
        }
    }
    if let Some(previous) = path {
        entries.push((previous, branch));
    }
    entries
}

pub fn resolve(cwd: &Path, branch: &str) -> Result<PathBuf, String> {
    if branch.trim().is_empty() {
        return Err("PR head branch is empty".to_string());
    }
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output()
        .map_err(|err| format!("git worktree list could not run: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    parse_worktrees(&listing)
        .into_iter()
        .find_map(|(path, found)| {
            (found.as_deref() == Some(branch) && path.is_dir()).then_some(path)
        })
        .ok_or_else(|| format!("no local worktree on PR branch {branch}"))
}

pub fn resolve_pr_with(cwd: &Path, pr: &str, gh_bin: &str) -> Result<PathBuf, String> {
    if pr
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .is_none()
    {
        return Err(format!("invalid PR number: {pr}"));
    }
    let query = format!("repos/{{owner}}/{{repo}}/pulls/{pr}");
    let out = crate::loopcheck::bounded_read(
        gh_bin,
        &["api", query.as_str(), "--jq", ".head.ref"],
        cwd,
        "pr-worktree",
        std::time::Duration::from_secs(30),
    )
    .map_err(|err| crate::loopcheck::bounded_read_diagnostic("pr-worktree", &err))?;
    if !out.status.success() {
        return Err(format!(
            "gh api pulls/{pr} failed: {}",
            String::from_utf8_lossy(&out.stderr_tail).trim()
        ));
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    resolve(cwd, &branch)
}

pub fn run() -> i32 {
    let mut input = String::new();
    if let Err(err) = io::stdin().read_to_string(&mut input) {
        eprintln!("pr-worktree: could not read request: {err}");
        return 2;
    }
    let payload: Value = match serde_json::from_str(&input) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("pr-worktree: invalid request: {err}");
            return 2;
        }
    };
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let pr = payload
        .get("pr")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            payload
                .get("pr")
                .and_then(Value::as_u64)
                .map(|number| number.to_string())
        });
    let result = match pr {
        Some(pr) => resolve_pr_with(&cwd, &pr, "gh"),
        None => payload
            .get("branch")
            .and_then(Value::as_str)
            .ok_or_else(|| "branch or PR number is required".to_string())
            .and_then(|branch| resolve(&cwd, branch)),
    };
    match result {
        Ok(worktree) => {
            println!("{}", json!({"worktree": worktree.to_string_lossy()}));
            0
        }
        Err(err) => {
            eprintln!("pr-worktree: {err}");
            3
        }
    }
}

#[cfg(test)]
mod tests {
    use super::resolve;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::tempdir;

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git is installed for this test");
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn canonical_cwd_resolves_the_pr_branch_worktree() {
        let dir = tempdir().unwrap();
        let canonical = dir.path().join("canonical");
        let feature = dir.path().join("feature-worktree");
        std::fs::create_dir(&canonical).unwrap();
        git(&canonical, &["init", "-q", "-b", "main"]);
        git(&canonical, &["config", "user.email", "test@example.com"]);
        git(&canonical, &["config", "user.name", "test"]);
        git(&canonical, &["commit", "-q", "--allow-empty", "-m", "base"]);
        git(
            &canonical,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/pr-42",
                feature.to_str().unwrap(),
            ],
        );

        assert_eq!(resolve(&canonical, "feature/pr-42").unwrap(), feature);
        assert!(resolve(&canonical, "main").unwrap() == PathBuf::from(&canonical));
        assert!(resolve(&canonical, "feature/missing").is_err());
    }
}
