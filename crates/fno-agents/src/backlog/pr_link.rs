//! PR attribution for the update verb. The url grammar is ported from
//! `cli/src/fno/graph/_reconcile.py` verbatim: the host is anchored, never
//! searched for, and the two-segment path depth is the contract a looser
//! parser would break.

use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

fn pr_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)^(?:[A-Za-z][A-Za-z0-9+.-]*://)?(?:[^/@]*@)?github\.com(?::\d+)?/([^/\s]+)/([^/\s]+?)(?:\.git)?/(?:pull|issues)/(\d+)(?:[/?#]|$)"#,
        )
        .expect("PR url regex")
    })
}
/// `owner/repo` from a GitHub PR url, or None.
pub fn repo_slug_from_url(url: Option<&str>) -> Option<String> {
    let url = url?;
    let m = pr_url_re().captures(url.trim())?;
    Some(format!("{}/{}", &m[1], &m[2]))
}

/// The PR number a GitHub PR url names, or None.
pub fn pr_number_from_url(url: Option<&str>) -> Option<i64> {
    let url = url?;
    let m = pr_url_re().captures(url.trim())?;
    m.get(3)?.as_str().parse().ok()
}

/// The one place a PR url is built, so `repo_slug_from_url` round-trips it.
pub fn pr_url_from_slug(slug: &str, pr: i64) -> String {
    format!("https://github.com/{slug}/pull/{pr}")
}

/// `owner/repo` from a git remote URL (`_slug_from_remote`), or None for a
/// non-GitHub remote. Strips scheme and credentials, anchors on the host,
/// and requires a path exactly two segments deep: an unanchored read turns
/// `ssh://git@github.com:22/o/r.git` into the repo `22/o/r`.
pub fn slug_from_remote(raw: &str) -> Option<String> {
    static SCHEME: OnceLock<Regex> = OnceLock::new();
    static USER: OnceLock<Regex> = OnceLock::new();
    static HOST: OnceLock<Regex> = OnceLock::new();
    let scheme = SCHEME.get_or_init(|| Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*://").expect("scheme"));
    let user = USER.get_or_init(|| Regex::new(r"^[^/@]*@").expect("user"));
    let host =
        HOST.get_or_init(|| Regex::new(r"(?i)^github\.com(?::\d+)?[:/](.+)$").expect("host"));
    let mut s = scheme.replace_all(raw.trim(), "").to_string();
    s = user.replace_all(&s, "").to_string();
    let m = host.captures(&s)?;
    let mut path = m[1].to_string();
    while path.ends_with('/') {
        path.pop();
    }
    if path.ends_with(".git") {
        path.truncate(path.len() - 4);
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    Some(format!("{}/{}", parts[0], parts[1]))
}

/// One subprocess for the repo-slug read, bounded like the Python caller
/// (`GH_QUERY_TIMEOUT_S`): a hung gh must not hang the verb.
fn run_slug_cmd(argv: &[&str], cwd: Option<&str>) -> (i32, String) {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return (127, String::new()),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = pipe.read_to_string(&mut out);
                }
                return (status.code().unwrap_or(127), out);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (127, String::new());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return (127, String::new()),
        }
    }
}

/// Best-effort `owner/repo` for the checkout at `cwd`: git origin first (no
/// network, no auth), then `gh repo view` for the checkout whose remote is
/// not named `origin`. None on every failure - the caller degrades to
/// unscoped resolution, never a wrong stamp.
pub fn resolve_current_repo_slug(cwd: Option<&str>) -> Option<String> {
    let (rc, out) = run_slug_cmd(&["git", "remote", "get-url", "origin"], cwd);
    if rc == 0 {
        if let Some(slug) = slug_from_remote(out.trim()) {
            return Some(slug);
        }
    }
    let (rc, out) = run_slug_cmd(
        &[
            "gh",
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "-q",
            ".nameWithOwner",
        ],
        cwd,
    );
    if rc == 0 {
        let trimmed = out.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn expanduser(path: &str) -> String {
    if let Some(rest) = path
        .strip_prefix("~/")
        .or_else(|| if path == "~" { Some("") } else { None })
    {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

/// The canonical PR url for the checkout at `cwd`, or None. A recorded cwd
/// that is gone refuses rather than resolving against a different checkout.
pub fn pr_url_for_repo(pr: i64, cwd: Option<&str>) -> Option<String> {
    let slug = match cwd {
        Some(cwd) => {
            let expanded = expanduser(cwd);
            if !std::path::Path::new(&expanded).is_dir() {
                eprintln!(
                    "note: recorded cwd {expanded} is gone - refusing to resolve PR \
                     #{pr} against a different checkout"
                );
                return None;
            }
            resolve_current_repo_slug(Some(&expanded))
        }
        None => resolve_current_repo_slug(None),
    };
    slug.map(|slug| pr_url_from_slug(&slug, pr))
}

/// True for a bare `owner/repo` slug, the shape `--repo` accepts.
pub fn is_repo_slug(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let Some((owner, repo)) = value.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !repo.is_empty()
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// The ship lifecycle row stamped when a node is FIRST PR-linked
/// (`_stamp_ship_on_pr_link`). The row records whoever ran the link, no
/// terminal closes it, and a re-stamp of the same (phase, harness, session)
/// collapses - a link event, never occupancy. Best-effort: an unresolvable
/// identity or a graph failure skips with a named stderr reason.
pub fn stamp_ship_on_link(graph: &Path, node_id: &str) {
    let get = |name: &str| std::env::var(name).ok();
    let ident = crate::spawn_context::resolve_self_identity(
        &get,
        None,
        None,
        &crate::paths::AgentsHome::from_env(),
    );
    let harness = ident.harness.unwrap_or_default();
    let session_id = ident.session_id.unwrap_or_default();
    let harness = harness.trim();
    let session_id = session_id.trim();
    if harness.is_empty() || session_id.is_empty() {
        let missing = if harness.is_empty() {
            "harness"
        } else {
            "session_id"
        };
        eprintln!(
            "update: no ambient identity to stamp ship provenance for {node_id} \
             (missing {missing}); run the link inside a session. Skipped."
        );
        return;
    }
    let started = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let run = || -> Result<(), String> {
        let row = crate::graph_keeper::session_row(
            "ship",
            harness,
            session_id,
            None,
            Some(&started),
            None,
            None,
            None,
        )
        .map_err(|e| e.to_string())?;
        // Version anchors before the row read, the conservative order: a
        // concurrent commit between the two makes this attempt's version
        // stale and the store refuses, instead of the stamp publishing a
        // stale snapshot over the newer write.
        let base_version = crate::graph_store::base_version(graph).map_err(|e| e.to_string())?;
        let mut entries = crate::graph_store::read_rows(graph).map_err(|e| e.to_string())?;
        let (found, _added) = crate::graph_keeper::session_append(&mut entries, node_id, row)
            .map_err(|e| e.to_string())?;
        if !found {
            return Ok(());
        }
        let input = crate::graph_store::MutateInput {
            entries,
            canonical_path: None,
            base_version,
            plan_rungs: None,
        };
        crate::graph_store::locked_mutate(graph, input, crate::graph_store::DEFAULT_LOCK_TIMEOUT)
            .map_err(|e| e.to_string())?;
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("update: ship provenance stamp skipped for {node_id}: {e}");
    }
}
