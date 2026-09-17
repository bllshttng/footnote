//! `fno-agents pr-list` -- the open, closed, or all PRs of one GitHub repo,
//! with each open row's node binding, behind `fno do pr list`.
//!
//! The repo defaults to the origin of the cwd. A job scratch dir can sit inside
//! a different checkout, so `[]` from there is a true answer about the wrong
//! repo. Every successful listing therefore ends with one stderr receipt that
//! names the repo it read and where that choice came from. A failed read never
//! prints the receipt and exits 4, so an empty repo and a failed read differ.

use crate::king_board::budget::run_with_timeout;
use crate::king_board::prs::pr_binding_verdicts;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 20;

type Runner<'a> = &'a dyn Fn(&[String], &Path) -> Result<Vec<u8>, String>;
type GraphRead<'a> = &'a dyn Fn() -> Result<Vec<Value>, String>;

struct Outcome {
    code: i32,
    stdout: String,
    stderr: Vec<String>,
}

pub fn run_pr_list(args: &[String]) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => {
            println!(
                "{}",
                json!({"error": format!("could not resolve owner/repo: {e}")})
            );
            return 4;
        }
    };
    let gh = |cmd: &[String], cwd: &Path| {
        run_with_timeout(cmd, cwd, Duration::from_secs(30)).map_err(|e| e.message().to_string())
    };
    let graph = || {
        let path = crate::king_board::scope::graph_json_path(&cwd);
        let store = crate::backlog::api::Store::new(&path);
        crate::backlog::api::rows(&store).map_err(|e| e.0)
    };
    let out = list(args, &cwd, None, &gh, &graph);
    println!("{}", out.stdout);
    for line in out.stderr {
        eprintln!("{line}");
    }
    out.code
}

fn fail(code: i32, error: String) -> Outcome {
    Outcome {
        code,
        stdout: json!({ "error": error }).to_string(),
        stderr: Vec::new(),
    }
}

fn list(
    args: &[String],
    cwd: &Path,
    git_ceiling: Option<&Path>,
    gh: Runner,
    graph: GraphRead,
) -> Outcome {
    let mut state = "open".to_string();
    let mut repo: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match (arg.as_str(), it.next()) {
            ("--state", Some(v)) => state = v.clone(),
            ("--repo", Some(v)) => repo = Some(v.clone()),
            _ => {
                return fail(
                    2,
                    format!(
                        "usage: pr-list [--state open|closed|all] [--repo owner/repo]; got {arg}"
                    ),
                )
            }
        }
    }
    if !matches!(state.as_str(), "open" | "closed" | "all") {
        return fail(2, "--state must be open, closed, or all".to_string());
    }
    let (slug, source) = match repo {
        Some(r) => (r, "--repo".to_string()),
        None => match origin_slug(cwd, git_ceiling) {
            Ok(s) => (
                s,
                format!(
                    "origin of {}; pass --repo owner/repo to choose",
                    home_relative(cwd)
                ),
            ),
            Err(reason) => return fail(4, format!("could not resolve owner/repo: {reason}")),
        },
    };
    let mut rows: Vec<Value> = Vec::new();
    let mut full_pages = 0;
    for page in 1..=MAX_PAGES {
        let cmd: Vec<String> = [
            "gh".to_string(),
            "api".to_string(),
            format!("repos/{slug}/pulls?state={state}&per_page={PAGE_SIZE}&page={page}"),
        ]
        .to_vec();
        let raw = match gh(&cmd, cwd) {
            Ok(raw) => raw,
            Err(e) => return fail(4, format!("gh api pulls list page {page} failed: {e}")),
        };
        let parsed: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(_) => {
                return fail(
                    4,
                    format!("gh api pulls list page {page} returned non-JSON"),
                )
            }
        };
        let Some(items) = parsed.as_array() else {
            return fail(
                4,
                format!("gh api pulls list page {page} returned a non-array"),
            );
        };
        for item in items {
            match summary(item) {
                Some(row) => rows.push(row),
                None => {
                    return fail(
                        4,
                        format!("gh api pulls list page {page} carried a malformed row"),
                    )
                }
            }
        }
        if items.len() < PAGE_SIZE {
            break;
        }
        full_pages += 1;
    }
    let mut stderr = Vec::new();
    if full_pages == MAX_PAGES {
        stderr.push(format!(
            "pr list: listing possibly truncated after {} rows",
            rows.len()
        ));
    }
    bind(&mut rows, graph);
    for row in rows.iter_mut() {
        if let Some(obj) = row.as_object_mut() {
            obj.shift_remove("body");
        }
    }
    stderr.push(format!("pr list: {slug} ({source}), {} rows", rows.len()));
    Outcome {
        code: 0,
        stdout: Value::Array(rows).to_string(),
        stderr,
    }
}

/// The listing row: `number, state, title, headRefName, url`, plus `body`
/// for the binding reader (removed before print). None on a malformed row.
fn summary(item: &Value) -> Option<Value> {
    let number = item.get("number")?.as_i64()?;
    let title = item.get("title")?.as_str()?;
    let head = item.get("head")?.get("ref")?.as_str()?;
    let url = item.get("html_url")?.as_str()?;
    let state = if item.get("merged_at").is_some_and(|v| !v.is_null()) {
        "MERGED"
    } else {
        match item.get("state").and_then(Value::as_str) {
            Some("open") => "OPEN",
            Some("closed") => "CLOSED",
            _ => "UNKNOWN",
        }
    };
    let mut row = Map::new();
    row.insert("number".into(), json!(number));
    row.insert("state".into(), json!(state));
    row.insert("title".into(), json!(title));
    row.insert("headRefName".into(), json!(head));
    row.insert("url".into(), json!(url));
    row.insert(
        "body".into(),
        json!(item.get("body").and_then(Value::as_str).unwrap_or("")),
    );
    Some(Value::Object(row))
}

fn bind(rows: &mut [Value], graph: GraphRead) {
    let is_open = |r: &Value| r.get("state").and_then(Value::as_str) == Some("OPEN");
    let open: Vec<Value> = rows.iter().filter(|r| is_open(r)).cloned().collect();
    if open.is_empty() {
        return;
    }
    let entries = match graph() {
        Ok(e) => e,
        Err(e) => {
            for row in rows.iter_mut().filter(|r| is_open(r)) {
                row["node_binding_error"] = json!(format!("graph binding read failed: {e}"));
            }
            return;
        }
    };
    for v in pr_binding_verdicts(&open, &entries) {
        let Some(row) = rows
            .iter_mut()
            .find(|r| is_open(r) && r.get("number").and_then(Value::as_i64) == Some(v.number))
        else {
            continue;
        };
        row["node_id"] = json!(v.node_id);
        row["node_binding"] = json!(v.verdict);
        if let Some(detail) = v.detail {
            row["node_binding_detail"] = json!(detail);
        }
    }
}

fn origin_slug(cwd: &Path, git_ceiling: Option<&Path>) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(["remote", "get-url", "origin"]).current_dir(cwd);
    if let Some(ceiling) = git_ceiling {
        cmd.env("GIT_CEILING_DIRECTORIES", ceiling);
    }
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("No such remote") {
            return Err("no origin remote".to_string());
        }
        let first = stderr.lines().map(str::trim).find(|l| !l.is_empty());
        return Err(first
            .unwrap_or("git remote get-url origin failed")
            .to_string());
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    parse_origin_slug(&url)
        .ok_or_else(|| format!("origin is not a github remote: {}", redact_userinfo(&url)))
}

/// `owner/repo` from a GitHub remote URL (_ritual._parse_origin_slug):
/// lowercase, strip scheme and `user@`, require host `github.com`, strip a
/// trailing `/` and `.git`, and require exactly `owner/repo`.
fn parse_origin_slug(url: &str) -> Option<String> {
    let re = regex::Regex::new(
        r"^(?:[a-z][a-z0-9+.-]*://)?(?:[^/@]*@)?github\.com(?::[0-9]+)?[:/](.+)$",
    )
    .ok()?;
    let lower = url.trim().to_lowercase();
    let rest = re.captures(&lower)?.get(1)?.as_str().trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/')).then(|| rest.to_string())
}

fn redact_userinfo(url: &str) -> String {
    match (url.find("://"), url.find('@')) {
        (Some(scheme), Some(at)) if at > scheme && !url[scheme + 3..at].contains('/') => {
            format!("{}{}", &url[..scheme + 3], &url[at + 1..])
        }
        _ => url.to_string(),
    }
}

fn home_relative(path: &Path) -> String {
    let shown = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && home != "/" => {
            if shown == home {
                "~".to_string()
            } else if let Some(rest) = shown.strip_prefix(&format!("{home}/")) {
                format!("~/{rest}")
            } else {
                shown
            }
        }
        _ => shown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_checkout(origin: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [vec!["init", "-q"], vec!["remote", "add", "origin", origin]] {
            let ok = Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        }
        dir
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn no_graph() -> Result<Vec<Value>, String> {
        panic!("graph read with no open rows")
    }

    #[test]
    fn an_empty_repo_prints_empty_and_names_the_repo_it_read() {
        let dir = git_checkout("git@github.com:other/empty.git");
        let gh = |cmd: &[String], _: &Path| {
            assert_eq!(
                cmd[2],
                "repos/other/empty/pulls?state=open&per_page=100&page=1"
            );
            Ok(b"[]".to_vec())
        };
        let out = list(&[], dir.path(), Some(dir.path()), &gh, &no_graph);
        assert_eq!(out.code, 0);
        assert_eq!(out.stdout, "[]");
        assert_eq!(
            out.stderr,
            vec![format!(
                "pr list: other/empty (origin of {}; pass --repo owner/repo to choose), 0 rows",
                home_relative(dir.path())
            )]
        );
    }

    #[test]
    fn an_open_row_carries_its_binding_in_key_order_and_no_body() {
        let gh = |_: &[String], _: &Path| {
            Ok(json!([{
                "number": 7, "state": "open", "merged_at": null, "title": "t",
                "head": {"ref": "feature/x-aaaa"}, "html_url": "https://github.com/o/r/pull/7",
                "body": "text",
            }])
            .to_string()
            .into_bytes())
        };
        let graph = || {
            Ok(vec![
                json!({"id": "x-aaaa", "pr_number": 7, "pr_url": "https://github.com/o/r/pull/7"}),
            ])
        };
        let out = list(&args(&["--repo", "o/r"]), Path::new("/"), None, &gh, &graph);
        assert_eq!(out.code, 0);
        let rows: Value = serde_json::from_str(&out.stdout).unwrap();
        let row = rows.as_array().unwrap()[0].as_object().unwrap();
        let keys: Vec<&str> = row.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "number",
                "state",
                "title",
                "headRefName",
                "url",
                "node_id",
                "node_binding"
            ]
        );
        assert_eq!(row["node_binding"], "bound");
        assert_eq!(
            out.stderr,
            vec!["pr list: o/r (--repo), 1 rows".to_string()]
        );
    }

    #[test]
    fn a_failed_read_exits_4_with_a_named_error_and_no_receipt() {
        let gh = |_: &[String], _: &Path| Err("exit 1: HTTP 502".to_string());
        let out = list(
            &args(&["--repo", "o/r"]),
            Path::new("/"),
            None,
            &gh,
            &no_graph,
        );
        assert_eq!(out.code, 4);
        let err: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(
            err["error"],
            "gh api pulls list page 1 failed: exit 1: HTTP 502"
        );
        assert!(out.stderr.is_empty());
        for bad in [&b"not json"[..], &b"{}"[..], &br#"[{"number": 1}]"#[..]] {
            let gh = move |_: &[String], _: &Path| Ok(bad.to_vec());
            let out = list(
                &args(&["--repo", "o/r"]),
                Path::new("/"),
                None,
                &gh,
                &no_graph,
            );
            assert_eq!(out.code, 4, "{}", out.stdout);
            assert!(out.stderr.is_empty());
        }
    }

    #[test]
    fn no_checkout_or_a_foreign_origin_refuses_without_leaking_the_token() {
        let gh = |_: &[String], _: &Path| panic!("gh called without a slug");
        let bare = tempfile::tempdir().unwrap();
        let out = list(&[], bare.path(), Some(bare.path()), &gh, &no_graph);
        assert_eq!(out.code, 4);
        let err: Value = serde_json::from_str(&out.stdout).unwrap();
        assert!(err["error"]
            .as_str()
            .unwrap()
            .starts_with("could not resolve owner/repo"));

        let dir = git_checkout("https://x-access-token:secret@gitlab.com/a/b.git");
        let out = list(&[], dir.path(), Some(dir.path()), &gh, &no_graph);
        assert_eq!(out.code, 4);
        assert!(
            out.stdout.contains("origin is not a github remote"),
            "{}",
            out.stdout
        );
        assert!(!out.stdout.contains("secret"));
    }

    #[test]
    fn a_bad_state_exits_2_and_a_graph_failure_marks_open_rows() {
        let gh = |_: &[String], _: &Path| panic!("gh called on a usage error");
        let out = list(
            &args(&["--state", "draft"]),
            Path::new("/"),
            None,
            &gh,
            &no_graph,
        );
        assert_eq!(out.code, 2);
        assert_eq!(
            out.stdout,
            r#"{"error":"--state must be open, closed, or all"}"#
        );

        let gh = |_: &[String], _: &Path| {
            Ok(json!([
                {"number": 1, "state": "open", "title": "a", "head": {"ref": "a"}, "html_url": "u1"},
                {"number": 2, "state": "closed", "merged_at": "2026-09-01", "title": "b", "head": {"ref": "b"}, "html_url": "u2"},
            ])
            .to_string()
            .into_bytes())
        };
        let graph = || Err("locked".to_string());
        let out = list(
            &args(&["--repo", "o/r", "--state", "all"]),
            Path::new("/"),
            None,
            &gh,
            &graph,
        );
        let rows: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(
            rows[0]["node_binding_error"],
            "graph binding read failed: locked"
        );
        assert_eq!(rows[1]["state"], "MERGED");
        assert!(rows[1].get("node_binding_error").is_none());
    }

    #[test]
    fn origin_parse_matches_the_python_rules() {
        assert_eq!(
            parse_origin_slug("git@github.com:O/R.git").as_deref(),
            Some("o/r")
        );
        assert_eq!(
            parse_origin_slug("https://github.com/o/r/").as_deref(),
            Some("o/r")
        );
        assert_eq!(parse_origin_slug("https://github.com/o/r/extra"), None);
        assert_eq!(parse_origin_slug("git@gitlab.com:o/r.git"), None);
    }
}
