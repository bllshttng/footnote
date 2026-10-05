//! `fno do pr create` (binary verb `pr-create`) -- the duplicate-guarded
//! create. On 2026-10-04 three PRs fixed one bug on the same file within an
//! hour: the graph was down, so no claim or fold check could see the others.
//! This verb reads open PRs from the GitHub REST API (no graph read: the
//! outage is exactly when the guard must work) and refuses when an open PR
//! touches the same changed files with an overlapping subject. The refusal
//! names the PR, its branch and its author; an explicit `--not-duplicate <n>`
//! passes it. On pass it execs `gh pr create --title --body-file`.
//!
//! Exit codes:
//! * `0` the PR was created (gh's output passes through)
//! * `3` a duplicate refused
//! * `2` usage or local read error
//! * `4` a GitHub read failed
//! * otherwise gh's own create exit code

use crate::pr_push::{gh_api, run_labeled, READ_TIMEOUT};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CREATE_TIMEOUT: Duration = Duration::from_secs(300);
/// Open-PR pages read before the scan gives up expanding (a warning names the
/// bound); 100 rows per page.
const MAX_PAGES: usize = 3;
const USAGE: &str = "usage: fno do pr create --title <t> --body-file <path> [--base <branch>] [--not-duplicate <pr>]...";

#[derive(Debug)]
struct Args {
    title: String,
    body_file: String,
    base: String,
    not_duplicates: Vec<i64>,
    cwd: PathBuf,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut a = Args {
        title: String::new(),
        body_file: String::new(),
        base: "main".to_string(),
        not_duplicates: Vec::new(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let mut i = 0;
    while i < argv.len() {
        let take = |name: &str| -> Result<String, String> {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match argv[i].as_str() {
            "--title" => {
                a.title = take("--title")?;
                i += 1;
            }
            "--body-file" => {
                a.body_file = take("--body-file")?;
                i += 1;
            }
            "--base" => {
                a.base = take("--base")?;
                i += 1;
            }
            "--not-duplicate" => {
                let raw = take("--not-duplicate")?;
                let n: i64 = raw
                    .trim()
                    .parse()
                    .map_err(|_| format!("--not-duplicate wants a PR number, got {raw}"))?;
                a.not_duplicates.push(n);
                i += 1;
            }
            // Test seam, same as pr-body-check's.
            "--cwd" => {
                a.cwd = PathBuf::from(take("--cwd")?);
                i += 1;
            }
            other => return Err(format!("unknown flag: {other}\n{USAGE}")),
        }
        i += 1;
    }
    if a.title.is_empty() {
        return Err(format!("--title is required\n{USAGE}"));
    }
    if a.body_file.is_empty() {
        return Err(format!("--body-file is required\n{USAGE}"));
    }
    Ok(a)
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let (ok, out, err) = run_labeled("pr-create", "git", args, cwd, READ_TIMEOUT)?;
    if ok {
        Ok(out.trim().to_string())
    } else {
        Err(if err.trim().is_empty() {
            out.trim().to_string()
        } else {
            err.trim().to_string()
        })
    }
}

/// Title words that decide an "overlapping subject": conventional-commit
/// prefix (`type(scope):`) stripped, lowercased, split on non-alphanumerics,
/// tokens under three characters dropped.
fn subject_tokens(title: &str) -> Vec<String> {
    let body = match title.find(": ") {
        Some(idx) if idx < 40 => &title[idx + 2..],
        _ => title,
    };
    body.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(String::from)
        .collect()
}

fn subjects_overlap(a: &str, b: &str) -> bool {
    let (ta, tb) = (subject_tokens(a), subject_tokens(b));
    if ta.is_empty() || tb.is_empty() {
        return false;
    }
    let shared = ta.iter().filter(|t| tb.contains(t)).count();
    shared >= 2 && shared * 2 >= ta.len().min(tb.len())
}

struct OpenPr {
    number: i64,
    title: String,
    branch: String,
    author: String,
}

/// One GitHub REST read: a repo-relative path with its query. Prod wires
/// `pr_push::gh_api`; tests inject.
type Gh<'a> = &'a dyn Fn(&str) -> Result<String, String>;

fn open_prs(slug: &str, gh: Gh) -> Result<Vec<OpenPr>, String> {
    let mut rows = Vec::new();
    for page in 1..=MAX_PAGES {
        let raw = gh(&format!(
            "repos/{slug}/pulls?state=open&per_page=100&page={page}"
        ))?;
        let parsed: Value = serde_json::from_str(&raw)
            .map_err(|_| format!("gh api pulls list page {page} returned non-JSON"))?;
        let Some(items) = parsed.as_array() else {
            return Err(format!(
                "gh api pulls list page {page} returned a non-array"
            ));
        };
        for item in items {
            let number = item.get("number").and_then(Value::as_i64);
            let title = item.get("title").and_then(Value::as_str);
            let branch = item
                .get("head")
                .and_then(|h| h.get("ref"))
                .and_then(Value::as_str);
            let author = item
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(Value::as_str);
            match (number, title, branch, author) {
                (Some(n), Some(t), Some(b), Some(a)) => rows.push(OpenPr {
                    number: n,
                    title: t.to_string(),
                    branch: b.to_string(),
                    author: a.to_string(),
                }),
                _ => {
                    return Err(format!(
                        "gh api pulls list page {page} carried a malformed row"
                    ))
                }
            }
        }
        if items.len() < 100 {
            break;
        }
        if page == MAX_PAGES {
            eprintln!(
                "pr-create: open-PR scan stopped at {} pages ({} rows); later pages unread",
                MAX_PAGES,
                rows.len()
            );
        }
    }
    Ok(rows)
}

fn pr_files(slug: &str, number: i64, gh: Gh) -> Result<Vec<String>, String> {
    let raw = gh(&format!("repos/{slug}/pulls/{number}/files?per_page=100"))?;
    let parsed: Value =
        serde_json::from_str(&raw).map_err(|_| format!("gh api files for #{number} non-JSON"))?;
    let Some(items) = parsed.as_array() else {
        return Err(format!("gh api files for #{number} a non-array"));
    };
    Ok(items
        .iter()
        .filter_map(|f| f.get("filename").and_then(Value::as_str))
        .map(String::from)
        .collect())
}

/// The first open PR that shares changed files with an overlapping subject,
/// or None when the create may proceed.
fn find_duplicate(
    title: &str,
    my_files: &[String],
    branch: &str,
    not_duplicates: &[i64],
    gh: Gh,
    slug: &str,
) -> Result<Option<(OpenPr, Vec<String>)>, String> {
    for pr in open_prs(slug, gh)? {
        if pr.branch == branch || not_duplicates.contains(&pr.number) {
            continue;
        }
        if !subjects_overlap(title, &pr.title) {
            continue;
        }
        let their_files = pr_files(slug, pr.number, gh)?;
        let shared: Vec<String> = my_files
            .iter()
            .filter(|f| their_files.contains(f))
            .cloned()
            .collect();
        if !shared.is_empty() {
            return Ok(Some((pr, shared)));
        }
    }
    Ok(None)
}

pub fn run_pr_create_verb(argv: &[String]) -> i32 {
    let a = match parse_args(argv) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("pr-create: {msg}");
            return 2;
        }
    };
    run(&a, &|path| gh_api("gh", &a.cwd, path, &[]))
}

fn run(a: &Args, gh: Gh) -> i32 {
    let branch = match git(&a.cwd, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok(b) if !b.is_empty() && b != "HEAD" => b,
        _ => {
            eprintln!("pr-create: could not read the head branch (detached HEAD?)");
            return 2;
        }
    };
    let base = crate::pr_body_check::base_ref(&a.base);
    let my_files = match git(&a.cwd, &["diff", "--name-only", &format!("{base}...HEAD")]) {
        Ok(f) if !f.is_empty() => f.lines().map(String::from).collect::<Vec<_>>(),
        Ok(_) => {
            eprintln!("pr-create: no changed files against {base}; nothing to guard");
            return 2;
        }
        Err(msg) => {
            eprintln!("pr-create: could not diff {base}...HEAD: {msg} (fetched the base ref?)");
            return 2;
        }
    };
    let slug = match crate::pr_list::origin_slug(&a.cwd, None) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("pr-create: could not resolve owner/repo: {msg}");
            return 2;
        }
    };
    match find_duplicate(&a.title, &my_files, &branch, &a.not_duplicates, gh, &slug) {
        Err(msg) => {
            eprintln!("pr-create: {msg}");
            4
        }
        Ok(Some((pr, shared))) => {
            let shown = shared
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let more = if shared.len() > 5 {
                format!(" (and {} more)", shared.len() - 5)
            } else {
                String::new()
            };
            eprintln!(
                "pr-create: REFUSED: open PR #{} already touches the same changed files with an overlapping subject",
                pr.number
            );
            eprintln!("  PR:     #{} {}", pr.number, pr.title);
            eprintln!("  branch: {}", pr.branch);
            eprintln!("  author: {}", pr.author);
            eprintln!("  files:  {shown}{more}");
            eprintln!(
                "  this:   {} (branch {branch}, {} files)",
                a.title,
                my_files.len()
            );
            eprintln!("  pass anyway: add --not-duplicate {}", pr.number);
            3
        }
        Ok(None) => {
            let cmd: Vec<String> = vec![
                "gh".to_string(),
                "pr".to_string(),
                "create".to_string(),
                "--title".to_string(),
                a.title.clone(),
                "--body-file".to_string(),
                a.body_file.clone(),
            ];
            match crate::org_board::budget::run_with_timeout(&cmd, &a.cwd, CREATE_TIMEOUT) {
                Ok(out) => {
                    print!("{}", String::from_utf8_lossy(&out));
                    0
                }
                Err(e) if e.over_budget() => {
                    eprintln!("pr-create: gh pr create timed out after 300s");
                    4
                }
                Err(e) => {
                    eprintln!("pr-create: gh pr create failed: {}", e.message());
                    2
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn subjects_overlap_needs_two_shared_tokens_and_half_the_shorter_title() {
        assert!(subjects_overlap(
            "fix(backlog): underscore the unused record param on the close stamp",
            "fix: underscore unused record param",
        ));
        assert!(!subjects_overlap(
            "fix(backlog): underscore the unused record param on the close stamp",
            "feat: port events family control to the binary",
        ));
        // One shared token is not an overlap even at full ratio.
        assert!(!subjects_overlap("graph repair", "graph claim"));
        assert!(!subjects_overlap("", "fix the graph"));
    }

    #[test]
    fn a_second_branch_editing_one_file_refuses_and_names_the_first() {
        let gh = |path: &str| {
            if path.contains("/pulls?state=open") {
                Ok(json_array(vec![pr_json(
                    3050,
                    "fix(backlog): repair the graph door",
                    "graph-door-hotfix",
                    "bllshttng",
                )]))
            } else if path.contains("/pulls/3050/files") {
                Ok(json_array(vec![file_json(
                    "crates/fno-agents/src/backlog/entities.rs",
                )]))
            } else {
                Err(format!("unexpected gh read: {path}"))
            }
        };
        let dup = find_duplicate(
            "fix(backlog): repair the graph door",
            &["crates/fno-agents/src/backlog/entities.rs".to_string()],
            "feature/duplicate-guard",
            &[],
            &gh,
            "o/r",
        )
        .unwrap();
        let (pr, shared) = dup.expect("the open twin refuses");
        assert_eq!(pr.number, 3050);
        assert_eq!(pr.branch, "graph-door-hotfix");
        assert_eq!(pr.author, "bllshttng");
        assert_eq!(shared, vec!["crates/fno-agents/src/backlog/entities.rs"]);
    }

    #[test]
    fn disjoint_files_or_a_whitelisted_number_passes_without_a_file_read() {
        let gh = |path: &str| {
            if path.contains("/pulls?state=open") {
                Ok(json_array(vec![pr_json(
                    7,
                    "same words in every title here",
                    "other-branch",
                    "someone",
                )]))
            } else if path.contains("/pulls/7/files") {
                Ok(json_array(vec![file_json("docs/other.md")]))
            } else {
                Err(format!("unexpected gh read: {path}"))
            }
        };
        let dup = find_duplicate(
            "same words in every title here",
            &["src/mine.rs".to_string()],
            "mine",
            &[],
            &gh,
            "o/r",
        )
        .unwrap();
        assert!(dup.is_none(), "disjoint files pass");

        // Whitelisted: the twin is skipped BEFORE its files are read, so a
        // files read for #7 would panic the runner.
        let gh = |path: &str| {
            assert!(path.contains("/pulls?state=open"));
            Ok(json_array(vec![pr_json(
                7,
                "same words in every title here",
                "other-branch",
                "someone",
            )]))
        };
        let dup = find_duplicate(
            "same words in every title here",
            &["docs/other.md".to_string()],
            "mine",
            &[7],
            &gh,
            "o/r",
        )
        .unwrap();
        assert!(dup.is_none(), "--not-duplicate passes");
    }

    #[test]
    fn a_failed_github_read_is_exit_4_and_a_usage_error_is_exit_2() {
        // Exit 4: a real repo whose gh read fails, so the guard's refusal
        // never silently degrades to a create.
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["remote", "add", "origin", "git@github.com:o/r.git"]);
        std::fs::write(dir.path().join("f.txt"), "base\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&["checkout", "-q", "-b", "feature/guard"]);
        std::fs::write(dir.path().join("f.txt"), "changed\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-q", "-m", "change"]);
        let repo_args = Args {
            title: "fix: the guard subject".into(),
            body_file: "b".into(),
            base: "main".into(),
            not_duplicates: vec![],
            cwd: dir.path().to_path_buf(),
        };
        let gh = |path: &str| {
            assert!(path.contains("/pulls?state=open"));
            Err("HTTP 502".to_string())
        };
        assert_eq!(run(&repo_args, &gh), 4);
        // Exit 2: a non-repo cwd fails the branch read before any gh call.
        let a = Args {
            title: "t".into(),
            body_file: "b".into(),
            base: "main".into(),
            not_duplicates: vec![],
            cwd: PathBuf::from("/"),
        };
        let gh = |_: &str| Err("HTTP 502".to_string());
        // `/` has no git repo: the branch read fails first (exit 2).
        assert_eq!(run(&a, &gh), 2);
        assert!(parse_args(&args(&["--nope"])).is_err());
        assert!(parse_args(&args(&["--title", "t"])).is_err());
        assert!(parse_args(&args(&[
            "--title",
            "t",
            "--body-file",
            "b",
            "--not-duplicate",
            "x",
        ]))
        .is_err());
        // The happy parse maps every flag, repeats --not-duplicate, and
        // expands a bare base to origin/<base>.
        let a = parse_args(&args(&[
            "--title",
            "t",
            "--body-file",
            "b",
            "--base",
            "release/9",
            "--not-duplicate",
            "7",
            "--not-duplicate",
            "8",
        ]))
        .unwrap();
        assert_eq!(a.base, "release/9");
        assert_eq!(a.not_duplicates, vec![7, 8]);
        assert_eq!(crate::pr_body_check::base_ref("release/9"), "release/9");
        assert_eq!(crate::pr_body_check::base_ref("main"), "origin/main");
    }

    // ── fixtures ────────────────────────────────────────────────────────────

    fn pr_json(number: i64, title: &str, branch: &str, author: &str) -> Value {
        serde_json::json!({
            "number": number,
            "title": title,
            "head": {"ref": branch},
            "user": {"login": author},
        })
    }

    fn file_json(name: &str) -> Value {
        serde_json::json!({"filename": name})
    }

    fn json_array(items: Vec<Value>) -> String {
        Value::Array(items).to_string()
    }
}
