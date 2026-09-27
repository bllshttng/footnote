//! The Linear backend over Linear's GraphQL API, the third tracker backend
//! (docs/architecture/external-tracker.md orders it after github).
//!
//! id shape `TEAM-123` (the Linear identifier, which never carries the `:`
//! claim-key partition character). All HTTP I/O is one POST through
//! [`LinearHttp`], the same subprocess seam the github backend's [`super::github::GhRun`]
//! uses, so tests feed recorded responses; the real impl shells `curl`
//! bounded at 30 s.
//! Auth rides the `FNO_TRACKER_LINEAR_API_KEY` env var, named here and in no
//! config key; `FNO_TRACKER_LINEAR_TEAM` (the team key, e.g. `ENG`) scopes
//! `list_open` / `list_closed_since`, the same role `FNO_TRACKER_GITHUB_REPO`
//! plays for github.
//!
//! Mappings: Linear state types `completed`/`canceled` read closed, everything
//! else open; Linear priority 1-4 maps to p0-p3 with no-priority defaulting to
//! p2; the estimate points map to S (<3), M (<8), L (>=8). Parent, blockers,
//! priority, estimate, description and url all read real - Linear is the
//! cleanest data model of the three. Linear has no footnote rank, so card
//! moves stay disabled: `rank` is always `None` and the trait carries no move
//! operation.

use super::{Candidate, State, Tracker, TrackerError, TrackerNode};
use serde_json::{json, Value};

const API_URL: &str = "https://api.linear.app/graphql";

/// One issue's worth of fields, shared by every list query via the fragment.
const ISSUE_FIELDS: &str = "fragment IssueFields on Issue { \
    identifier title priority createdAt completedAt canceledAt description url \
    state { type } parent { identifier } estimate { value } \
    blockedBy { nodes { issue { identifier } relatedIssue { identifier } } } \
}";

const ISSUE_BY_ID: &str = "query LinearIssueById($id: String!) { \
    issues(filter: { identifier: { eq: $id } }, first: 1) { nodes { ...IssueFields } } }";

const OPEN_PAGE: &str = "query TeamOpenIssues($key: String!, $after: String) { \
    team(key: $key) { issues(filter: { state: { type: { nin: [\"completed\", \"canceled\"] } } }, \
    first: 250, after: $after) { nodes { ...IssueFields } pageInfo { hasNextPage endCursor } } } }";

const CLOSED_PAGE: &str =
    "query TeamClosedIssues($key: String!, $after: String, $since: DateTime!) { \
    team(key: $key) { issues(filter: { or: [ { completedAt: { gte: $since } }, \
    { canceledAt: { gte: $since } } ] }, first: 250, after: $after) { nodes { ...IssueFields } \
    pageInfo { hasNextPage endCursor } } } }";

/// Append the shared fragment definition to a query that spreads it.
fn with_fields(query: &str) -> String {
    format!("{query} {ISSUE_FIELDS}")
}

const CLOSE_STATE: &str = "query LinearCloseState($id: String!) { \
    issues(filter: { identifier: { eq: $id } }, first: 1) { nodes { \
    id team { workflowStates { nodes { id type } } } } } }";

const CLOSE_MUTATION: &str = "mutation LinearIssueClose($id: String!, $stateId: String!) { \
    issueUpdate(id: $id, input: { stateId: $stateId }) { success } }";

/// The one I/O seam: one POST of a GraphQL request document, answered as
/// (exit code, stdout, stderr). The real impl bounds `curl` at 30 s through
/// `bounded_cmd::output_with_timeout_result`, SIGKILL at the bound.
pub trait LinearHttp: Send + Sync {
    fn post(&self, body: &str) -> Result<(i32, String, String), String>;
}

struct RealLinear {
    api_key: String,
}

impl LinearHttp for RealLinear {
    fn post(&self, body: &str) -> Result<(i32, String, String), String> {
        let mut cmd = std::process::Command::new("curl");
        cmd.arg("-sS")
            .args(["-X", "POST"])
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("-H")
            .arg(format!("Authorization: {}", self.api_key))
            .args(["--data-binary", body, API_URL]);
        match crate::bounded_cmd::output_with_timeout_result(cmd, 30) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(
                "curl binary not found on PATH; install curl to use the linear tracker backend"
                    .into(),
            ),
            Err(e) => Err(format!("curl failed: {e}")),
            Ok(out) => {
                let code = out.status.code().unwrap_or(-1);
                if code == 137 || out.status.code().is_none() {
                    // bounded_cmd SIGKILLs the child's process group at the
                    // bound; a signal death inside the window is the timeout.
                    return Err("curl timed out posting to api.linear.app".into());
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

/// The Linear tracker. `team` scopes `list_open` / `list_closed_since`;
/// without it they warn and return empty, never a silent fallback to another
/// backend. The api key is the auth env var's value, taken once at
/// construction; every op refuses without one.
pub struct LinearTracker {
    api_key: Option<String>,
    team: Option<String>,
    http: Box<dyn LinearHttp>,
}

impl LinearTracker {
    pub fn from_env() -> Self {
        let api_key = std::env::var("FNO_TRACKER_LINEAR_API_KEY").unwrap_or_default();
        Self::new(
            Some(api_key.clone()).filter(|v| !v.trim().is_empty()),
            std::env::var("FNO_TRACKER_LINEAR_TEAM")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            Box::new(RealLinear { api_key }),
        )
    }

    pub fn new(api_key: Option<String>, team: Option<String>, http: Box<dyn LinearHttp>) -> Self {
        Self {
            api_key,
            team,
            http,
        }
    }

    /// One GraphQL round trip: refuse without a key, surface transport
    /// faults as `Backend`, and unwrap the `data` object. GraphQL-level
    /// errors (an unknown team, a malformed filter) answer `data: null`
    /// plus an `errors` array, which becomes `Backend` naming them.
    fn gql(&self, query: &str, variables: Value, label: &str) -> Result<Value, TrackerError> {
        if self.api_key.is_none() {
            return Err(TrackerError::Refused(
                "Linear API key missing: set FNO_TRACKER_LINEAR_API_KEY".into(),
            ));
        }
        let body = json!({ "query": query, "variables": variables }).to_string();
        let (rc, out, err) = self.http.post(&body).map_err(|e| {
            TrackerError::Backend(format!("linear request failed for {label}: {e}"))
        })?;
        if rc != 0 {
            return Err(TrackerError::Backend(format!(
                "linear request failed for {label}: {}",
                err.trim()
            )));
        }
        if out.trim().is_empty() {
            return Err(TrackerError::Backend(format!(
                "linear returned an empty body for {label}"
            )));
        }
        let parsed: Value = serde_json::from_str(&out).map_err(|e| {
            TrackerError::Backend(format!("linear returned non-JSON for {label}: {e}"))
        })?;
        if let Some(errors) = parsed.get("errors") {
            let empty = Vec::new();
            let messages: Vec<String> = errors
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .filter_map(|e| e.get("message").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            if !messages.is_empty() {
                return Err(TrackerError::Backend(format!(
                    "linear refused {label}: {}",
                    messages.join("; ")
                )));
            }
        }
        Ok(parsed.get("data").cloned().unwrap_or(Value::Null))
    }

    fn nodes_of(data: &Value, label: &str) -> Result<Vec<Value>, TrackerError> {
        let nodes = data
            .get("issues")
            .and_then(|i| i.get("nodes"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                TrackerError::Backend(format!("linear answered no issues list for {label}"))
            })?;
        Ok(nodes.clone())
    }

    /// One candidate from one Linear issue node. Blocker direction is read
    /// off the relation: the endpoint that is not this issue blocks it.
    fn candidate_of(node: &Value, closed: bool) -> Candidate {
        let id = node
            .get("identifier")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut blocked_by = Vec::new();
        if let Some(relations) = node
            .get("blockedBy")
            .and_then(|b| b.get("nodes"))
            .and_then(Value::as_array)
        {
            for rel in relations {
                for endpoint in ["issue", "relatedIssue"] {
                    let other = rel
                        .get(endpoint)
                        .and_then(|e| e.get("identifier"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !other.is_empty() && other != id && !blocked_by.contains(&other.to_string())
                    {
                        blocked_by.push(other.to_string());
                    }
                }
            }
            blocked_by.dedup();
        }
        Candidate {
            node: TrackerNode {
                id,
                title: node
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                state: state_of(
                    node.get("state")
                        .and_then(|s| s.get("type"))
                        .and_then(Value::as_str),
                    closed,
                ),
                parent: node
                    .get("parent")
                    .and_then(|p| p.get("identifier"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                blocked_by,
                details: node
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                url: node.get("url").and_then(Value::as_str).map(str::to_string),
                size: size_of(
                    node.get("estimate")
                        .and_then(|e| e.get("value"))
                        .and_then(Value::as_f64),
                ),
            },
            priority: priority_of(node.get("priority").and_then(Value::as_i64)),
            rank: None,
            created_at: node
                .get("createdAt")
                .and_then(Value::as_str)
                .map(str::to_string),
            closed_at: node
                .get("completedAt")
                .or_else(|| node.get("canceledAt"))
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }

    /// Walk one team query to its page cap, the github listing cap's twin.
    fn team_pages(
        &self,
        query: &str,
        key: &str,
        since: Option<String>,
        label: &str,
    ) -> Result<Vec<Candidate>, TrackerError> {
        let mut after: Option<String> = None;
        let mut out = Vec::new();
        loop {
            let mut variables = json!({ "key": key, "after": after });
            if let Some(since) = &since {
                variables["since"] = json!(since);
            }
            let data = self.gql(&with_fields(query), variables, label)?;
            let issues = data.get("team").ok_or_else(|| {
                TrackerError::Backend(format!("linear has no team {key:?} for {label}"))
            })?;
            let page = issues.get("issues").ok_or_else(|| {
                TrackerError::Backend(format!("linear answered no issues for {label}"))
            })?;
            for node in page
                .get("nodes")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                if node
                    .get("identifier")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .is_empty()
                {
                    continue;
                }
                out.push(Self::candidate_of(node, since.is_some()));
            }
            let more = page
                .get("pageInfo")
                .and_then(|p| p.get("hasNextPage"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            after = page
                .get("pageInfo")
                .and_then(|p| p.get("endCursor"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if !more || after.is_none() || out.len() >= 1000 {
                return Ok(out);
            }
        }
    }
}

/// `completed`/`canceled` read closed; everything else (backlog, unstarted,
/// started, triage) reads open.
fn state_of(linear_type: Option<&str>, closed: bool) -> State {
    if closed || matches!(linear_type, Some(t) if t == "completed" || t == "canceled") {
        State::Closed
    } else {
        State::Open
    }
}

/// Linear priority 1-4 to p0-p3; no priority (0) defaults to p2, footnote's
/// own default.
fn priority_of(linear: Option<i64>) -> String {
    match linear {
        Some(1) => "p0".into(),
        Some(2) => "p1".into(),
        Some(3) => "p2".into(),
        Some(4) => "p3".into(),
        _ => "p2".into(),
    }
}

/// Estimate points to S/M/L: under 3 reads S, under 8 reads M, the rest L;
/// no estimate reads None.
fn size_of(estimate: Option<f64>) -> Option<String> {
    match estimate {
        Some(v) if v <= 0.0 => None,
        Some(v) if v < 3.0 => Some("S".into()),
        Some(v) if v < 8.0 => Some("M".into()),
        Some(_) => Some("L".into()),
        None => None,
    }
}

impl Tracker for LinearTracker {
    fn name(&self) -> &str {
        "linear"
    }

    fn read(&self, id: &str) -> Result<TrackerNode, TrackerError> {
        let data = self.gql(&with_fields(ISSUE_BY_ID), json!({ "id": id }), id)?;
        let mut nodes = Self::nodes_of(&data, id)?;
        match nodes.len() {
            0 => Err(TrackerError::NotFound(id.to_string())),
            _ => {
                let node = Self::candidate_of(&nodes.remove(0), false).node;
                if node.id.is_empty() {
                    return Err(TrackerError::NotFound(id.to_string()));
                }
                Ok(node)
            }
        }
    }

    fn list_open(&self) -> Result<Vec<Candidate>, TrackerError> {
        let Some(team) = &self.team else {
            eprintln!(
                "fno tracker: linear backend has no FNO_TRACKER_LINEAR_TEAM scope; \
                 list_open returns nothing. Set it to the team key to enumerate issues."
            );
            return Ok(Vec::new());
        };
        self.team_pages(OPEN_PAGE, team, None, team)
    }

    fn list_closed_since(&self, days: u32) -> Option<Result<Vec<Candidate>, TrackerError>> {
        let Some(team) = self.team.clone() else {
            return Some(Ok(Vec::new()));
        };
        let since = (chrono::Utc::now() - chrono::Duration::days(days as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        Some(self.team_pages(CLOSED_PAGE, &team, Some(since), &team))
    }

    fn close(&self, id: &str) -> Result<(), TrackerError> {
        let data = self.gql(CLOSE_STATE, json!({ "id": id }), id)?;
        let issue = data
            .get("issues")
            .and_then(|i| i.get("nodes"))
            .and_then(Value::as_array)
            .and_then(|n| n.first())
            .ok_or_else(|| TrackerError::NotFound(id.to_string()))?;
        let issue_id = issue
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| TrackerError::Backend(format!("linear answered no id for {id}")))?;
        let states = issue
            .get("team")
            .and_then(|t| t.get("workflowStates"))
            .and_then(|w| w.get("nodes"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let done = states
            .iter()
            .find(|s| s.get("type").and_then(Value::as_str) == Some("completed"))
            .and_then(|s| s.get("id").and_then(Value::as_str))
            .ok_or_else(|| {
                TrackerError::Backend(format!(
                    "linear team of {id} has no completed workflow state to close into"
                ))
            })?;
        let data = self.gql(
            CLOSE_MUTATION,
            json!({ "id": issue_id, "stateId": done }),
            id,
        )?;
        let payload = data
            .get("issueUpdate")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                TrackerError::Backend(format!("linear issueUpdate answered nothing for {id}"))
            })?;
        if payload.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(TrackerError::Backend(format!(
                "linear refused to close {id}: success is not true"
            )));
        }
        Ok(())
    }
}
