//! The merge-evidence gate shared by `done` and `reopen`: does a stored PR
//! ref carry GitHub-confirmed merge evidence? Ported from graph/_reconcile.py
//! (`MergeEvidence`, `resolve_merge_evidence`, the REST state read) so the
//! native lifecycle verbs answer with the same outcomes, exit codes and
//! operator remedies as the Python close they replace. The first MERGED ref
//! closes; an OPEN ref means awaiting merge and outranks an outage, because
//! a live PR is a definite answer where an unreachable ref is not. CI state
//! is deliberately not consulted.

use serde_json::Value;
use std::path::Path;
use std::time::Duration;

use crate::pr_push::run_labeled;

const GH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Merged,
    AwaitingMerge,
    Outage,
    Refused,
}

/// The close decision for a node's PR refs, shared by every done verb.
pub(crate) struct MergeEvidence {
    pub outcome: Outcome,
    pub pr_url: Option<String>,
    pub open_pr_number: Option<i64>,
    pub error: Option<String>,
    pub reason: Option<String>,
    pub failure_kind: Option<String>,
    pub remedy: Option<String>,
}

impl MergeEvidence {
    /// 3 refused / 4 outage / 5 awaiting merge; 0 when it closes.
    pub fn exit_code(&self) -> i32 {
        match self.outcome {
            Outcome::Merged => 0,
            Outcome::AwaitingMerge => 5,
            Outcome::Outage => 4,
            Outcome::Refused => 3,
        }
    }
}

/// A PR-state read failure with a machine-readable operator remedy (the
/// ReconcileError twin). `availability` is the retryable class; every other
/// kind is a policy refusal.
#[derive(Debug)]
pub(crate) struct PrReadError {
    pub message: String,
    pub kind: String,
    pub remedy: Option<String>,
}

impl PrReadError {
    pub fn new(message: impl Into<String>, kind: &str) -> Self {
        let message = message.into();
        let kind = if kind.is_empty() {
            Self::classify(&message)
        } else {
            kind.to_string()
        };
        Self {
            message,
            kind,
            remedy: None,
        }
    }

    pub fn retryable(&self) -> bool {
        self.kind == "availability"
    }

    /// Message-content classification, matching ReconcileError._classify.
    pub fn classify(message: &str) -> String {
        let text = message.to_lowercase();
        if text.contains("not found on path") || text.contains("toolmissing") {
            return "availability".into();
        }
        if text.contains("unconditional")
            || text.contains("routed, never rationed")
            || text.contains("graphql reserve")
            || text.contains("use `fno do pr info")
        {
            return "routing_refusal".into();
        }
        let authed = text.split(|c: char| !c.is_alphanumeric()).any(|w| {
            matches!(
                w,
                "auth" | "authentication" | "authenticated" | "credentials"
            )
        });
        if authed
            || text.contains("bad credentials")
            || text.contains("not logged in")
            || text.contains("http 401")
            || text.contains("status 401")
        {
            return "authentication".into();
        }
        if text.contains("could not resolve owner/repo") || text.contains("no repo context") {
            return "repository_context".into();
        }
        if text.contains("not found")
            || text.contains("404")
            || text.contains("could not resolve to a repository")
            || text.contains("could not resolve to a pullrequest")
        {
            return "not_found".into();
        }
        if text.contains("3,000-file cap") {
            return "evidence_incomplete".into();
        }
        if text.contains("rate limit") || text.contains("quota") {
            return "availability".into();
        }
        if ["http 5", "status 5"].iter().any(|p| text.contains(p))
            || text.contains("bad gateway")
            || text.contains("internal server error")
        {
            return "availability".into();
        }
        if ["not json", "malformed", "parse", "json value", "no output"]
            .iter()
            .any(|t| text.contains(t))
        {
            return "malformed".into();
        }
        if [
            "timeout",
            "timed out",
            "network",
            "transport",
            "connection",
            "unavailable",
        ]
        .iter()
        .any(|t| text.contains(t))
        {
            return "availability".into();
        }
        "reader_error".into()
    }

    /// The operator remedy for a failed read of this PR, matching remedy_for.
    pub fn remedy_for(&self, pr_number: i64, repo: Option<&str>) -> String {
        if let Some(remedy) = &self.remedy {
            return remedy.clone();
        }
        match self.kind.as_str() {
            "routing_refusal" => {
                let scope = repo.map(|r| format!(" --repo {r}")).unwrap_or_default();
                format!(
                    "Use `fno do pr info {pr_number}{scope}` as the route. \
                     This refusal is unconditional and is not retryable."
                )
            }
            "authentication" => {
                "Run `gh auth login`; authentication failures are not retryable.".into()
            }
            "not_found" => {
                let stored = repo
                    .map(|r| format!("{r}#{pr_number}"))
                    .unwrap_or_else(|| format!("PR #{pr_number}"));
                format!(
                    "Stored PR reference {stored} was not found. Repair it with \
                     `fno backlog update <node> --pr-url <correct-url>`; do not \
                     retry against the ambient repository."
                )
            }
            "malformed" => {
                "Inspect the malformed GitHub response and fix the reader; this is not retryable."
                    .into()
            }
            "evidence_incomplete" => {
                ("GitHub's REST files endpoint reached its 3,000-file cap without \
                 a complete tail; review the full PR evidence before using \
                 `fno backlog done <node> --force --reason TEXT`.")
                    .into()
            }
            "reader_error" => {
                "The REST reader raised an unexpected error; inspect the reader before retrying."
                    .into()
            }
            _ => format!("Retry the GitHub read for PR #{pr_number} when availability returns."),
        }
    }
}

/// `owner/repo` parsed from a GitHub PR URL, or None. pr_link's parser IS the
/// port of the python twin's `_PR_URL_RE`, so the parity pair reuses it
/// instead of carrying a second, wider parser.
pub(crate) use super::pr_link::repo_slug_from_url;

/// The `(pr_number, pr_url)` pairs for a node, primary first, de-duplicated
/// by number (primary wins). The node_pr_refs twin.
pub(crate) fn node_pr_refs(node: &Value) -> Vec<(i64, Option<String>)> {
    let mut refs: Vec<(i64, Option<String>)> = Vec::new();
    let mut seen: Vec<i64> = Vec::new();
    if let Some(primary) = node.get("pr_number").and_then(Value::as_i64) {
        refs.push((primary, url_string(node.get("pr_url"))));
        seen.push(primary);
    }
    for extra in node
        .get("additional_prs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(num) = extra.get("number").and_then(Value::as_i64) else {
            continue;
        };
        if seen.contains(&num) {
            continue;
        }
        refs.push((num, url_string(extra.get("url"))));
        seen.push(num);
    }
    refs
}

fn url_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// One REST read: `gh api repos/{slug}/pulls/{n}` -> (state, html_url).
pub(crate) fn query_pr_state(
    pr_number: i64,
    repo: Option<&str>,
    cwd: Option<&str>,
) -> Result<(String, Option<String>), PrReadError> {
    let owned_slug;
    let slug = match repo {
        Some(r) => r.to_string(),
        None => {
            owned_slug = super::session_cli::resolve_current_repo_slug().ok_or_else(|| {
                PrReadError::new("could not resolve owner/repo from the checkout", "")
            })?;
            owned_slug
        }
    };
    let path = format!("repos/{slug}/pulls/{pr_number}");
    let empty = Path::new(".");
    let cwd_path = cwd.map(Path::new).unwrap_or(empty);
    let (ok, out, err) = run_labeled(
        "backlog-evidence",
        "gh",
        &["api", "--allow-escape-sequences", &path],
        cwd_path,
        GH_TIMEOUT,
    )
    .map_err(|e| PrReadError::new(e, "availability"))?;
    if !ok {
        let message = if err.trim().is_empty() { out } else { err };
        return Err(PrReadError::new(message, ""));
    }
    let data: Value = serde_json::from_str(out.trim()).map_err(|e| {
        PrReadError::new(
            format!("gh api pulls/{pr_number} returned output that is not JSON: {e}"),
            "malformed",
        )
    })?;
    if !data.is_object() {
        return Err(PrReadError::new(
            "gh api pulls/<n> returned a JSON value that is not an object",
            "malformed",
        ));
    }
    let state = data.get("state").and_then(Value::as_str).ok_or_else(|| {
        PrReadError::new(
            "REST PR info reader omitted required PR number or state",
            "malformed",
        )
    })?;
    // REST never says MERGED: a merged PR is state "closed" with merged true.
    // The flag wins; otherwise the state reads case-insensitively, so a
    // GraphQL-shaped payload still maps.
    let state = if data.get("merged").and_then(Value::as_bool).unwrap_or(false) {
        "MERGED"
    } else {
        match state.to_ascii_uppercase().as_str() {
            "OPEN" => "OPEN",
            "CLOSED" => "CLOSED",
            other => {
                return Err(PrReadError::new(
                    format!("REST PR info reader returned malformed state {other:?}"),
                    "malformed",
                ))
            }
        }
    };
    let url = data
        .get("html_url")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((state.to_string(), url))
}

/// Decide whether a node's PR refs are closing evidence. The resolve_merge_
/// evidence twin: the first MERGED ref closes; OPEN outranks an outage; the
/// first non-retryable read failure is a policy refusal.
pub(crate) fn resolve_merge_evidence(
    refs: &[(i64, Option<String>)],
    cwd: Option<&str>,
) -> MergeEvidence {
    if refs.is_empty() {
        return MergeEvidence {
            outcome: Outcome::Refused,
            pr_url: None,
            open_pr_number: None,
            error: None,
            reason: Some("no PR ref to evidence".into()),
            failure_kind: None,
            remedy: None,
        };
    }
    let mut refusal_reason: Option<String> = None;
    let mut refusal_kind: Option<String> = None;
    let mut refusal_remedy: Option<String> = None;
    let mut outage_error: Option<String> = None;
    let mut outage_remedy: Option<String> = None;
    let mut open_pr_number: Option<i64> = None;
    let (first_pr_number, first_pr_url) = &refs[0];
    let repo = repo_slug_from_url(first_pr_url.as_deref());

    for (pr_number, pr_url) in refs {
        let pr_repo = repo_slug_from_url(pr_url.as_deref()).or_else(|| repo.clone());
        let pr_cwd = if pr_repo.is_none() { cwd } else { None };
        match query_pr_state(*pr_number, pr_repo.as_deref(), pr_cwd) {
            Err(exc) => {
                let remedy = exc.remedy_for(*pr_number, pr_repo.as_deref());
                if exc.retryable() {
                    outage_error = Some(exc.message.clone());
                    outage_remedy = Some(remedy);
                } else if refusal_kind.is_none() {
                    refusal_reason = Some(exc.message.clone());
                    refusal_kind = Some(exc.kind.clone());
                    refusal_remedy = Some(remedy);
                }
            }
            Ok((state, url)) => {
                if state == "MERGED" {
                    return MergeEvidence {
                        outcome: Outcome::Merged,
                        pr_url: url.or_else(|| pr_url.clone()),
                        open_pr_number: None,
                        error: None,
                        reason: None,
                        failure_kind: None,
                        remedy: None,
                    };
                }
                if state == "OPEN" {
                    if open_pr_number.is_none() {
                        open_pr_number = Some(*pr_number);
                    }
                } else if refusal_reason.is_none() && refusal_kind.is_none() {
                    refusal_reason = Some(format!("PR #{pr_number} state={state} (not merged)"));
                }
            }
        }
    }

    if let Some(kind) = refusal_kind {
        return MergeEvidence {
            outcome: Outcome::Refused,
            pr_url: None,
            open_pr_number: None,
            error: None,
            reason: refusal_reason,
            failure_kind: Some(kind),
            remedy: refusal_remedy,
        };
    }
    if let Some(open) = open_pr_number {
        return MergeEvidence {
            outcome: Outcome::AwaitingMerge,
            pr_url: None,
            open_pr_number: Some(open),
            error: outage_error,
            reason: None,
            failure_kind: None,
            remedy: outage_remedy,
        };
    }
    if outage_error.is_some() {
        return MergeEvidence {
            outcome: Outcome::Outage,
            pr_url: None,
            open_pr_number: None,
            error: outage_error,
            reason: None,
            failure_kind: None,
            remedy: outage_remedy,
        };
    }
    MergeEvidence {
        outcome: Outcome::Refused,
        pr_url: None,
        open_pr_number: None,
        error: None,
        reason: Some(
            refusal_reason
                .clone()
                .unwrap_or_else(|| format!("PR #{first_pr_number}: no merged evidence")),
        ),
        failure_kind: refusal_kind,
        remedy: refusal_remedy,
    }
}

/// The shared actionable outcome for every close front door (the
/// render_merge_evidence_failure twin).
pub(crate) fn render_merge_evidence_failure(
    task_id: &str,
    evidence: &MergeEvidence,
    stays: &str,
) -> String {
    if evidence.outcome == Outcome::Outage {
        let action = evidence
            .remedy
            .clone()
            .unwrap_or_else(|| "Retry the GitHub read when availability returns.".into());
        return format!(
            "Error: gh cross-check failed for {task_id}: {}\n{action} Node stays {stays}.",
            evidence.error.clone().unwrap_or_default()
        );
    }
    let message = evidence
        .reason
        .clone()
        .unwrap_or_else(|| "no merged evidence".into());
    if let Some(remedy) = &evidence.remedy {
        return format!("Refused: {task_id} cross-check failed: {message}\n{remedy}");
    }
    format!("Refused: {task_id} cross-check failed: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slug_parses_from_pr_urls_only() {
        assert_eq!(
            repo_slug_from_url(Some("https://github.com/acme/widget/pull/7")),
            Some("acme/widget".into())
        );
        assert_eq!(
            repo_slug_from_url(Some("https://github.com/acme/widget/issues/9")),
            Some("acme/widget".into())
        );
        // A bare repo url is not a PR ref: the twin answers None and the
        // caller falls back to the cwd's git remote.
        assert_eq!(
            repo_slug_from_url(Some("https://github.com/acme/widget")),
            None
        );
        assert_eq!(repo_slug_from_url(Some("https://gitlab.com/a/b")), None);
        assert_eq!(repo_slug_from_url(None), None);
    }

    #[test]
    fn refs_combine_primary_and_additional_without_duplicates() {
        let node = json!({
            "pr_number": 7,
            "pr_url": "https://github.com/acme/widget/pull/7",
            "additional_prs": [
                {"number": 7, "url": "dupe"},
                {"number": 8, "url": "https://github.com/acme/widget/pull/8"},
                {"number": "bad"}
            ]
        });
        let refs = node_pr_refs(&node);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].0, 7);
        assert_eq!(refs[1].0, 8);
    }

    #[test]
    fn error_classification_matches_the_python_kinds() {
        assert_eq!(
            PrReadError::classify("gh: not found on path"),
            "availability"
        );
        assert_eq!(PrReadError::classify("HTTP 404: Not Found"), "not_found");
        assert_eq!(PrReadError::classify("bad credentials"), "authentication");
        assert_eq!(
            PrReadError::classify("API rate limit exceeded"),
            "availability"
        );
        assert_eq!(PrReadError::classify("request timed out"), "availability");
        assert_eq!(PrReadError::classify("no output from reader"), "malformed");
        assert_eq!(PrReadError::classify("something odd"), "reader_error");
    }

    #[test]
    fn exit_codes_match_the_python_contract() {
        let mut ev = MergeEvidence {
            outcome: Outcome::AwaitingMerge,
            pr_url: None,
            open_pr_number: None,
            error: None,
            reason: None,
            failure_kind: None,
            remedy: None,
        };
        assert_eq!(ev.exit_code(), 5);
        ev.outcome = Outcome::Outage;
        assert_eq!(ev.exit_code(), 4);
        ev.outcome = Outcome::Refused;
        assert_eq!(ev.exit_code(), 3);
    }

    #[test]
    fn render_names_the_outage_and_the_refusal() {
        let ev = MergeEvidence {
            outcome: Outcome::Outage,
            pr_url: None,
            open_pr_number: None,
            error: Some("gh down".into()),
            reason: None,
            failure_kind: None,
            remedy: Some("retry later".into()),
        };
        let text = render_merge_evidence_failure("ab-1234abcd", &ev, "open");
        assert!(
            text.starts_with("Error: gh cross-check failed for ab-1234abcd: gh down"),
            "{text}"
        );
        assert!(text.ends_with("Node stays open."), "{text}");
        let ev = MergeEvidence {
            outcome: Outcome::Refused,
            pr_url: None,
            open_pr_number: None,
            error: None,
            reason: Some("PR #7 state=CLOSED (not merged)".into()),
            failure_kind: Some("not_found".into()),
            remedy: None,
        };
        let text = render_merge_evidence_failure("ab-1234abcd", &ev, "open");
        assert_eq!(
            text,
            "Refused: ab-1234abcd cross-check failed: PR #7 state=CLOSED (not merged)"
        );
    }

    #[test]
    fn empty_refs_refuse_without_a_read() {
        let ev = resolve_merge_evidence(&[], None);
        assert_eq!(ev.outcome, Outcome::Refused);
        assert_eq!(ev.reason.as_deref(), Some("no PR ref to evidence"));
    }
}
