//! Dispatch credit: who answers for an autonomously dispatched node worker.
//!
//! Two asks on the `spawn-axes` verb (the shrink law allows no new client
//! action), each failing open:
//!
//! - `dispatch_credit`: the live crown whose scope covers the node. The
//!   spawn door stamps it as the worker's owner when no producer carrier
//!   named one, so the row names the lead that owns the territory and the
//!   spawn counts in that lead's share.
//! - `launch_credit_mail`: one mail to that lead at launch, naming the
//!   node, the worker and the resolved axes. Best-effort by contract: a
//!   delivery fault never fails a launch that already happened.
//!
//! A kingless node keeps today's shape: no owner override, no mail.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::{json, Value};

use crate::loop_lead::territory_members;
use crate::org_board::scope::project_map;

/// The mail budget: a wrapped notice, never a `--raw` command.
const MAIL_TIMEOUT: Duration = Duration::from_secs(30);

/// The system sender for the launch notice; the bare `fno` name refuses at
/// the mail door (R13), so the arm spells itself.
const MAIL_SENDER: &str = "fno/dispatch-credit";

/// The node row's territory member: its epic (parent), else the node id
/// itself. Crown scopes are epic-member sets, so a node under an epic
/// answers to whatever crown holds that epic.
fn territory_member(row: &Value) -> String {
    let parent = row
        .get("parent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    parent
        .or_else(|| row.get("id").and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

fn node_project(row: &Value) -> String {
    row.get("project")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The graph row for a node id or its slug, or `None` on an unreadable
/// store: the same id-or-slug resolution every backlog reader accepts.
fn graph_row(node: &str) -> Option<Value> {
    let rows = crate::graph_store::read_rows_where(
        &crate::graph_get::default_graph_path(),
        &crate::backlog::RowQuery {
            filter: crate::backlog::api::NodeFilter {
                id_in: Some(vec![node.to_string()]),
                ..Default::default()
            },
            with_blockers: true,
            ..Default::default()
        },
    )
    .ok()?;
    crate::graph_get::find_entry(&rows, node).cloned()
}

/// The registry as row values, empty on a read fault (fail open).
fn registry_rows() -> Vec<Value> {
    let path = match crate::paths::AgentsHome::from_env_opt() {
        Some(home) => home.registry_json(),
        None => return Vec::new(),
    };
    crate::state::load_registry(&path)
        .map(|registry| {
            registry
                .entries
                .iter()
                .filter_map(|e| serde_json::to_value(e).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The covering-crown ask: one answer naming the owner and (when the crown
/// row carries a live session) the lead to mail.
pub fn covering_crown(payload: &Value) -> Value {
    let node = payload
        .get("node")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if node.is_empty() {
        return json!({"node": Value::Null, "owner": Value::Null, "lead": Value::Null});
    }
    let node_row = graph_row(&node);
    let projects = project_map(&std::env::current_dir().unwrap_or_default()).unwrap_or_default();
    covering_crown_in(&node, node_row.as_ref(), &registry_rows(), &projects)
}

/// [`covering_crown`] over handed-in rows: pure, so a test pins fixtures and
/// the ask never touches a store under test.
pub fn covering_crown_in(
    node: &str,
    node_row: Option<&Value>,
    registry_rows: &[Value],
    projects: &HashMap<String, String>,
) -> Value {
    let kingless = || json!({"node": node, "owner": Value::Null, "lead": Value::Null});
    let Some(row) = node_row else {
        return kingless();
    };
    let member = territory_member(row);
    if member.is_empty() {
        return kingless();
    }
    // The node answers to an epic crown through its territory member and to
    // a project or portfolio crown through its project, aliases
    // canonicalized both ways.
    let project = node_project(row);
    let canonical_project = projects.get(project.as_str()).cloned();
    let covers = |members: &HashSet<String>| {
        members.contains(&member)
            || canonical_project
                .as_ref()
                .is_some_and(|p| members.contains(p))
    };
    // Live rows holding a scope whose canonical members cover the node's
    // territory. Terminal rows are dead crowns, never a covering lead.
    let mut best: Option<(&Value, String, HashSet<String>)> = None;
    for entry in registry_rows {
        let scope = entry
            .get("crown_scope")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(scope) = scope else { continue };
        if crate::announce::row_terminal(entry) {
            continue;
        }
        let members: HashSet<String> = territory_members(scope, projects).into_iter().collect();
        if !covers(&members) {
            continue;
        }
        // Most specific crown wins: the smallest covering member set, so a
        // crown over the epic itself beats a portfolio that merely holds it.
        let better = match &best {
            None => true,
            Some((_, _, prev)) => members.len() < prev.len(),
        };
        if better {
            best = Some((entry, scope.to_string(), members));
        }
    }
    let Some((entry, scope, _)) = best else {
        return kingless();
    };
    let lead_session = entry
        .get("harness_session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let lead = lead_session.map(|session| {
        json!({
            "session": session,
            "name": entry.get("name").and_then(Value::as_str).unwrap_or(""),
            "harness": entry.get("harness").and_then(Value::as_str).unwrap_or(""),
        })
    });
    json!({
        "node": node,
        "owner": {"kind": "crown", "project": project, "scope": scope},
        "lead": lead,
    })
}

/// The launch notice: one wrapped line naming what launched and who answers.
pub fn launch_mail_text(node: &str, worker: &Value) -> String {
    let s = |k: &str| {
        worker
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let effort = s("effort");
    let effort = if effort.is_empty() {
        "default".to_string()
    } else {
        effort
    };
    format!(
        "Launch notice: {name} started on {node} ({harness}, model {model}, effort {effort}). Your crown scope covers the node, so the worker counts in your share; the worker reports through its own loop and this is the one launch notice.",
        name = s("name"),
        harness = s("harness"),
        model = s("model"),
    )
}

/// The `launch_credit_mail` ask: resolve the covering crown's lead and
/// deliver the notice through the mail verb the scheduler itself uses.
pub fn launch_credit_mail(payload: &Value) -> Value {
    let node = payload
        .get("node")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let worker = payload.get("worker").cloned().unwrap_or(json!({}));
    let credit = covering_crown(&json!({"node": node}));
    let lead = credit.get("lead").cloned().unwrap_or(Value::Null);
    let session = lead
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if session.is_empty() {
        return json!({"node": node, "mailed": false, "reason": "kingless"});
    }
    let text = launch_mail_text(&node, &worker);
    let cmd = vec![
        "fno".to_string(),
        "agents".to_string(),
        "mail".to_string(),
        "send".to_string(),
        "--from-name".to_string(),
        MAIL_SENDER.to_string(),
        "--origin".to_string(),
        "scheduler".to_string(),
        session.clone(),
        text,
    ];
    match crate::org_board::budget::run_with_timeout(&cmd, std::path::Path::new("."), MAIL_TIMEOUT)
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out).into_owned();
            json!({
                "node": node,
                "mailed": crate::mail_inject::mail_send_accepted(0, &stdout),
                "receipt": crate::mail_inject::mail_send_receipt(&stdout),
            })
        }
        Err(e) => json!({"node": node, "mailed": false, "reason": e.message()}),
    }
}
