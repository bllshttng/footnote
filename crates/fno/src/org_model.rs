use crate::{
    backlog_model::{Inputs, NodeView, SessionView},
    proto::AgentRow,
};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct OrgInputs {
    pub backlog: Inputs,
    pub fold: Result<Value, String>,
    pub measured_at: u64,
}
#[derive(Debug, Clone)]
pub struct OrgSession {
    pub view: SessionView,
    pub agent: Option<AgentRow>,
}
#[derive(Debug, Clone)]
pub struct OrgNode {
    pub view: NodeView,
    pub claim_state: String,
    pub claim_holder: Option<String>,
    pub age_s: Option<u64>,
    pub current: Vec<OrgSession>,
    pub former: Vec<OrgSession>,
}
#[derive(Debug, Clone)]
pub struct OrgLead {
    pub holder: AgentRow,
    pub scope: String,
    pub level: u32,
    pub grantor: Option<String>,
    pub counts: Value,
    pub owned_counts: Value,
    pub epics: Value,
    pub stuck_line: Option<String>,
    pub nodes: Vec<OrgNode>,
    pub left: Vec<OrgNode>,
}
#[derive(Debug, Clone)]
pub struct OrgTree {
    pub leads: Vec<OrgLead>,
    pub unowned: Vec<AgentRow>,
    pub measured_at: u64,
}
#[derive(Debug, Clone, Default)]
pub struct OrgSnapshot {
    pub tree: Option<OrgTree>,
    pub error: Option<String>,
    pub error_at: Option<u64>,
}
impl OrgSnapshot {
    pub fn apply(&mut self, inputs: &OrgInputs, now: u64) {
        match derive(inputs, now) {
            Ok(tree) => {
                self.tree = Some(tree);
                self.error = None;
                self.error_at = None;
            }
            Err(reason) => {
                self.error = Some(reason);
                self.error_at = Some(now);
            }
        }
    }
}
pub async fn gather(graph: &std::path::Path, agents: Vec<AgentRow>) -> OrgInputs {
    let crowns: Vec<Value> = agents.iter().filter(|a| !a.exited).filter_map(|a| {
        Some(serde_json::json!({"scope": a.crown_scope.as_ref()?, "level": a.crown_level?, "holder": a.name}))
    }).collect();
    let mut command = tokio::process::Command::new("fno-agents");
    command
        .args(["court-fold", "--graph"])
        .arg(graph)
        .args([
            "--crowns-json",
            &serde_json::to_string(&crowns).expect("JSON values always serialize"),
            "--format",
            "json",
        ])
        .kill_on_drop(true);
    let (backlog, output) = tokio::join!(
        crate::backlog_model::gather(graph, agents),
        tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
    );
    let fold = match output {
        Err(_) => Err("court-fold timed out after 30s".into()),
        Ok(Err(error)) => Err(format!("court-fold could not run: {error}")),
        Ok(Ok(output)) if !output.status.success() => Err(format!(
            "court-fold exited {}: {}",
            output
                .status
                .code()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "by signal".into()),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Ok(Ok(output)) => serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("court-fold invalid JSON: {e}")),
    };
    OrgInputs {
        backlog,
        fold,
        measured_at: chrono::Utc::now().timestamp().max(0) as u64,
    }
}
fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}
fn epoch(value: Option<&str>) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(value?)
        .ok()
        .map(|v| v.timestamp().max(0) as u64)
}
fn worker_node<'a>(agent: &AgentRow, inputs: &'a Inputs) -> Option<&'a str> {
    if let Some(node) = agent.node.as_deref() {
        return inputs
            .rows
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .find(|id| *id == node);
    }
    inputs
        .rows
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str))
        .find(|id| {
            agent.name.match_indices(id).any(|(at, _)| {
                (at == 0 || agent.name.as_bytes()[at - 1] == b'-')
                    && agent
                        .name
                        .as_bytes()
                        .get(at + id.len())
                        .is_none_or(|b| *b == b'-')
            })
        })
}
fn joined<'a>(session: &SessionView, agents: &'a [AgentRow]) -> Option<&'a AgentRow> {
    let sid = session.session_id.as_deref()?;
    let eligible = |a: &&AgentRow| {
        a.crown_level.is_none()
            && session
                .harness
                .as_deref()
                .is_none_or(|h| a.harness.as_deref().is_none_or(|ah| h == ah))
    };
    if let Some(exact) = agents
        .iter()
        .filter(eligible)
        .find(|a| a.harness_session_id.as_deref() == Some(sid))
    {
        return Some(exact);
    }
    if sid.len() != 8 {
        return None;
    }
    let mut candidates = agents.iter().filter(eligible).filter(|a| {
        a.harness_session_id
            .as_deref()
            .is_some_and(|full| full.starts_with(sid))
    });
    let first = candidates.next()?;
    candidates.next().is_none().then_some(first)
}
fn action(agent: Option<&AgentRow>) -> (String, Option<String>) {
    match crate::backlog_model::session_action(agent) {
        crate::backlog_model::SessionAction::Attach => ("attach".into(), None),
        crate::backlog_model::SessionAction::Resume => ("resume".into(), None),
        crate::backlog_model::SessionAction::Dim(reason) => ("none".into(), Some(reason)),
    }
}
fn org_node(inputs: &Inputs, id: &str, fold: &Value, now: u64) -> Result<OrgNode, String> {
    let view = crate::backlog_model::node(inputs, id)
        .ok_or_else(|| format!("court-fold node {id} missing from graph read"))?;
    let mut current = Vec::new();
    let mut former = Vec::new();
    for mut session in view.sessions.clone() {
        let agent = joined(&session, &inputs.agents);
        (session.action, session.reason) = action(agent);
        session.agent = agent.map(|a| a.name.clone());
        let row = OrgSession {
            view: session,
            agent: agent.cloned(),
        };
        if agent.is_some_and(|a| !a.exited) {
            current.push(row);
        } else {
            former.push(row);
        }
    }
    for agent in inputs
        .agents
        .iter()
        .filter(|a| !a.exited && a.crown_level.is_none())
    {
        if worker_node(agent, inputs) != Some(id)
            || current
                .iter()
                .any(|s| s.agent.as_ref().is_some_and(|a| a.name == agent.name))
        {
            continue;
        }
        let (action, reason) = action(Some(agent));
        current.push(OrgSession {
            view: SessionView {
                phase: None,
                harness: agent.harness.clone(),
                session_id: agent.harness_session_id.clone(),
                model: agent.model.clone(),
                started_at: None,
                ended_at: None,
                agent: Some(agent.name.clone()),
                action,
                reason,
            },
            agent: Some(agent.clone()),
        });
    }
    former.sort_by(|a, b| {
        b.view
            .ended_at
            .as_ref()
            .or(b.view.started_at.as_ref())
            .cmp(&a.view.ended_at.as_ref().or(a.view.started_at.as_ref()))
    });
    let age_s = fold
        .get("age_hours")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| (n * 3600.0) as u64)
        .or_else(|| epoch(view.created_at.as_deref()).map(|t| now.saturating_sub(t)));
    Ok(OrgNode {
        view,
        claim_state: text(fold, "claim_state").unwrap_or_else(|| "unreadable".into()),
        claim_holder: text(fold, "worker"),
        age_s,
        current,
        former,
    })
}
pub fn derive(inputs: &OrgInputs, now: u64) -> Result<OrgTree, String> {
    let fold = inputs.fold.as_ref().map_err(Clone::clone)?;
    if let Some(reason) = &inputs.backlog.rows_error {
        return Err(reason.clone());
    }
    let scopes = fold
        .get("scope_nodes")
        .and_then(Value::as_object)
        .ok_or("court-fold scope_nodes missing")?;
    let owners = fold
        .get("owned_scopes")
        .and_then(Value::as_object)
        .ok_or("court-fold ownership unavailable")?;
    let mut leads = Vec::new();
    for holder in inputs.backlog.agents.iter().filter(|a| !a.exited) {
        let (Some(scope), Some(level)) = (&holder.crown_scope, holder.crown_level) else {
            continue;
        };
        let own = scopes
            .get(scope)
            .ok_or_else(|| format!("court-fold scope {scope} missing"))?;
        if own.get("status").and_then(Value::as_str) != Some("ok") {
            return Err(
                text(own, "reason").unwrap_or_else(|| format!("court-fold scope {scope} failed"))
            );
        }
        let records = own
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("court-fold scope {scope} nodes missing"))?;
        let mut nodes = Vec::new();
        for record in records {
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .ok_or("court-fold node id missing")?;
            // Ancestor folds overlap; the owner read chooses the team.
            if owners.get(id).and_then(Value::as_str) != Some(scope) {
                continue;
            }
            nodes.push(org_node(&inputs.backlog, id, record, now)?);
        }
        let mut left = Vec::new();
        for row in &inputs.backlog.rows {
            let Some(id) = row.get("id").and_then(Value::as_str) else {
                continue;
            };
            if owners.get(id).and_then(Value::as_str) != Some(scope)
                || nodes.iter().any(|n| n.view.card.id == id)
            {
                continue;
            }
            if row.get("status").and_then(Value::as_str) == Some("done")
                && epoch(row.get("completed_at").and_then(Value::as_str))
                    .is_some_and(|t| t <= now && now - t <= 86400)
            {
                left.push(org_node(&inputs.backlog, id, &Value::Null, now)?);
            }
        }
        left.sort_by(|a, b| b.view.completed_at.cmp(&a.view.completed_at));
        leads.push(OrgLead {
            holder: holder.clone(),
            scope: scope.clone(),
            level,
            grantor: text(own, "grantor"),
            counts: own["counts"].clone(),
            owned_counts: own["owned_counts"].clone(),
            epics: own["epics"].clone(),
            stuck_line: text(own, "stuck_line"),
            nodes,
            left,
        });
    }
    let unowned = inputs
        .backlog
        .agents
        .iter()
        .filter(|a| !a.exited && a.crown_level.is_none())
        .filter(|a| {
            !leads
                .iter()
                .flat_map(|l| &l.nodes)
                .flat_map(|n| &n.current)
                .any(|s| s.agent.as_ref().is_some_and(|r| r.name == a.name))
        })
        .cloned()
        .collect();
    Ok(OrgTree {
        leads,
        unowned,
        measured_at: inputs.measured_at,
    })
}
