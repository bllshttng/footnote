//! The GitHub Issues backend over `gh`, the port of the deleted Python
//! `github_backend.py`.
//!
//! id shape `owner/repo#N`; read and close carry their repo in the id, so
//! they need no configuration, while `list_open` needs the
//! `FNO_TRACKER_GITHUB_REPO` scope. Parent and blockers stay degraded
//! (`None` / `[]`): sub-issues and dependencies cost two REST calls per open
//! issue per refresh, and guessing them costs more than the truth. All gh
//! I/O touches the [`GhRun`] seam so tests feed recorded responses.
//! `gh` on PATH is the fno gh proxy shim, which admits `issue` subcommands
//! through the quota broker; `gh api graphql` is refused by the proxy.

use super::{Candidate, State, Tracker, TrackerError, TrackerNode};
use serde_json::Value;
use std::sync::OnceLock;

/// The one I/O seam. The real impl bounds `gh` at 30 s through
/// `bounded_cmd::output_with_timeout_result`, SIGKILL at the bound.
pub trait GhRun: Send + Sync {
    fn run(&self, args: &[String]) -> Result<(i32, String, String), String>;
}

struct RealGh;

impl GhRun for RealGh {
    fn run(&self, args: &[String]) -> Result<(i32, String, String), String> {
        let mut cmd = std::process::Command::new("gh");
        cmd.args(args);
        match crate::bounded_cmd::output_with_timeout_result(cmd, 30) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(
                "gh binary not found on PATH; install GitHub CLI to use the github tracker backend"
                    .into(),
            ),
            Err(e) => Err(format!("gh failed: {e}")),
            Ok(out) => {
                let code = out.status.code().unwrap_or(-1);
                if code == 137 || out.status.code().is_none() {
                    // bounded_cmd SIGKILLs the child's process group at the
                    // bound; a signal death inside the window is the timeout.
                    return Err(format!("gh timed out: gh {}", args.join(" ")));
                }
                Ok((
                    code,
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                ))
            }
        }
    }
}

/// `owner/repo#N`.
pub fn parse_github_id(id: &str) -> Result<(String, String, i64), TrackerError> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^([A-Za-z0-9._-]+)/([A-Za-z0-9._-]+)#([0-9]+)$").unwrap()
    });
    let caps = re.captures(id).ok_or_else(|| {
        TrackerError::Backend(format!(
            "not a GitHub issue id: {id:?} (expected owner/repo#N)"
        ))
    })?;
    Ok((
        caps.get(1).unwrap().as_str().to_string(),
        caps.get(2).unwrap().as_str().to_string(),
        caps.get(3).unwrap().as_str().parse::<i64>().unwrap_or(0),
    ))
}

/// stderr fragments gh emits when an issue does not exist (vs. a network or
/// auth failure, which surfaces as `Backend` so the caller can degrade).
fn not_found_stderr(stderr: &str) -> bool {
    let low = stderr.to_lowercase();
    ["could not resolve", "not found", "no issue"]
        .iter()
        .any(|frag| low.contains(frag))
}

/// gh can return rc=0 with empty or non-JSON stdout (a deprecation notice, a
/// truncated pipe); that is a backend fault, never a parse panic escaping the
/// contract.
fn loads(raw: &str, label: &str) -> Result<Value, TrackerError> {
    if raw.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(raw)
        .map_err(|e| TrackerError::Backend(format!("gh returned non-JSON for {label}: {e}")))
}

fn state_of(raw: Option<&str>) -> State {
    if raw.unwrap_or("").eq_ignore_ascii_case("OPEN") {
        State::Open
    } else {
        State::Closed
    }
}

/// The GitHub Issues tracker. `default_repo` scopes `list_open` /
/// `list_closed_since`; without it they warn and return empty, never a silent
/// fallback to another backend.
pub struct GitHubTracker {
    default_repo: Option<String>,
    gh: Box<dyn GhRun>,
}

impl GitHubTracker {
    pub fn from_env() -> Self {
        Self::new(
            std::env::var("FNO_TRACKER_GITHUB_REPO")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            Box::new(RealGh),
        )
    }

    pub fn new(default_repo: Option<String>, gh: Box<dyn GhRun>) -> Self {
        Self { default_repo, gh }
    }

    fn run_gh(&self, args: Vec<String>) -> Result<(i32, String, String), TrackerError> {
        let id_label = args
            .iter()
            .find(|a| a.contains('#'))
            .cloned()
            .unwrap_or_default();
        self.gh
            .run(&args)
            .map_err(|e| TrackerError::Backend(format!("gh failed for {id_label}: {e}")))
    }
}

impl Tracker for GitHubTracker {
    fn name(&self) -> &str {
        "github"
    }

    fn read(&self, id: &str) -> Result<TrackerNode, TrackerError> {
        let (owner, repo, number) = parse_github_id(id)?;
        let (rc, out, err) = self.run_gh(vec![
            "issue".into(),
            "view".into(),
            number.to_string(),
            "-R".into(),
            format!("{owner}/{repo}"),
            "--json".into(),
            "title,state,body,url".into(),
        ])?;
        if rc != 0 {
            if not_found_stderr(&err) {
                return Err(TrackerError::NotFound(id.to_string()));
            }
            return Err(TrackerError::Backend(format!(
                "gh issue view failed for {id}: {}",
                err.trim()
            )));
        }
        let data = loads(&out, id)?;
        Ok(TrackerNode {
            id: id.to_string(),
            title: data
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            state: state_of(data.get("state").and_then(Value::as_str)),
            parent: None,
            blocked_by: vec![],
            details: data.get("body").and_then(Value::as_str).map(str::to_string),
            url: data.get("url").and_then(Value::as_str).map(str::to_string),
            size: None,
        })
    }

    fn list_open(&self) -> Result<Vec<Candidate>, TrackerError> {
        let Some(repo) = &self.default_repo else {
            eprintln!(
                "fno tracker: github backend has no FNO_TRACKER_GITHUB_REPO scope; \
                 list_open returns nothing. Set it to enumerate issues."
            );
            return Ok(Vec::new());
        };
        let (rc, out, err) = self.run_gh(vec![
            "issue".into(),
            "list".into(),
            "-R".into(),
            repo.clone(),
            "--state".into(),
            "open".into(),
            "--json".into(),
            "number,title,state,createdAt,body,url".into(),
            "--limit".into(),
            "1000".into(),
        ])?;
        if rc != 0 {
            return Err(TrackerError::Backend(format!(
                "gh issue list failed for {repo}: {}",
                err.trim()
            )));
        }
        let data = loads(&out, repo)?;
        let items = match data {
            Value::Array(items) => items,
            _ => Vec::new(),
        };
        Ok(items
            .iter()
            .map(|it| Candidate {
                node: TrackerNode {
                    id: format!(
                        "{repo}#{}",
                        it.get("number").and_then(Value::as_i64).unwrap_or(0)
                    ),
                    title: it.get("title").and_then(Value::as_str).map(str::to_string),
                    state: state_of(it.get("state").and_then(Value::as_str)),
                    parent: None,
                    blocked_by: vec![],
                    details: it.get("body").and_then(Value::as_str).map(str::to_string),
                    url: it.get("url").and_then(Value::as_str).map(str::to_string),
                    size: None,
                },
                priority: "p2".to_string(),
                rank: None,
                created_at: it
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                closed_at: None,
            })
            .collect())
    }

    fn list_closed_since(&self, days: u32) -> Option<Result<Vec<Candidate>, TrackerError>> {
        let inner = || -> Result<Vec<Candidate>, TrackerError> {
            let Some(repo) = self.default_repo.as_deref() else {
                return Ok(Vec::new());
            };
            let since =
                (chrono::Utc::now() - chrono::Duration::days(days as i64)).format("%Y-%m-%d");
            let (rc, out, err) = self.run_gh(vec![
                "issue".into(),
                "list".into(),
                "-R".into(),
                repo.to_string(),
                "--state".into(),
                "closed".into(),
                "--search".into(),
                format!("closed:>={since}"),
                "--json".into(),
                "number,title,state,createdAt,closedAt,body,url".into(),
                "--limit".into(),
                "200".into(),
            ])?;
            if rc != 0 {
                return Err(TrackerError::Backend(format!(
                    "gh issue list failed for {repo}: {}",
                    err.trim()
                )));
            }
            let data = loads(&out, repo)?;
            let items = match data {
                Value::Array(items) => items,
                _ => Vec::new(),
            };
            Ok(items
                .iter()
                .map(|it| Candidate {
                    node: TrackerNode {
                        id: format!(
                            "{repo}#{}",
                            it.get("number").and_then(Value::as_i64).unwrap_or(0)
                        ),
                        title: it.get("title").and_then(Value::as_str).map(str::to_string),
                        state: State::Closed,
                        parent: None,
                        blocked_by: vec![],
                        details: it.get("body").and_then(Value::as_str).map(str::to_string),
                        url: it.get("url").and_then(Value::as_str).map(str::to_string),
                        size: None,
                    },
                    priority: "p2".to_string(),
                    rank: None,
                    created_at: it
                        .get("createdAt")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    closed_at: it
                        .get("closedAt")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
                .collect())
        };
        Some(inner())
    }

    fn close(&self, id: &str) -> Result<(), TrackerError> {
        let (owner, repo, number) = parse_github_id(id)?;
        let (rc, _out, err) = self.run_gh(vec![
            "issue".into(),
            "close".into(),
            number.to_string(),
            "-R".into(),
            format!("{owner}/{repo}"),
        ])?;
        if rc != 0 {
            if not_found_stderr(&err) {
                return Err(TrackerError::NotFound(id.to_string()));
            }
            return Err(TrackerError::Backend(format!(
                "gh issue close failed for {id}: {}",
                err.trim()
            )));
        }
        Ok(())
    }
}
