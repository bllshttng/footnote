//! Session identity, row actions and the bounded worker-detail read.

use super::backlog_style::BLine;
use super::org_board::Selected;
use super::*;
use serde_json::Value;

pub(crate) enum OrgMsg {
    Gather(crate::org_model::OrgInputs),
    Detail {
        request: u64,
        identity: String,
        result: Result<Value, String>,
    },
}

pub(crate) struct WorkerDetail {
    row: AgentRow,
    pub(crate) request: u64,
    pub(crate) identity: String,
    roster: Option<Result<Value, String>>,
    pub(crate) scroll: usize,
}

fn identity(a: &AgentRow) -> String {
    format!(
        "{}:{:?}:{:?}:{:?}",
        a.harness.as_deref().unwrap_or(""),
        a.harness_session_id,
        a.attach_id,
        a.pane_id
    )
}

pub(crate) fn apply(view: &mut View, gen: u64, msg: OrgMsg) {
    match msg {
        OrgMsg::Gather(inputs) => super::org_board::apply_fold(view, gen, inputs),
        OrgMsg::Detail {
            request,
            identity,
            result,
        } => {
            let Some(detail) = view
                .org_board
                .as_mut()
                .filter(|b| b.gen == gen)
                .and_then(|b| b.detail.as_mut())
                .filter(|d| d.request == request && d.identity == identity)
            else {
                return;
            };
            detail.roster = Some(result);
        }
    }
}

pub(crate) async fn dispatch(
    view: &mut View,
    selected: Selected,
    key: u8,
    sock: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let (sid, harness, captured) = match &selected {
        Selected::Node(node) => {
            if !matches!(key, b'\r' | b'd') {
                return Ok(());
            }
            let inputs = view.org_board.as_ref().and_then(|b| b.inputs.clone());
            let Some(inputs) = inputs else {
                view.set_notice("org nodes not read".into());
                return Ok(());
            };
            let mut board = super::backlog_board::BoardView::new(view.org_generation);
            board.inputs = Some(inputs);
            let query = board.query.to_query()?;
            board.body = Some(crate::backlog_model::board(
                board.inputs.as_ref().unwrap(),
                &query,
            ));
            board.detail = Some(super::node_detail::NodeDetailOverlay {
                node_id: node.card.id.clone(),
                trail: vec![],
                sel: 0,
                scroll: 0,
            });
            view.org_board = None;
            view.backlog_board = Some(board);
            super::backlog_board::set_sideline_view(view, crate::view_store::SidelineView::Backlog);
            return Ok(());
        }
        Selected::Lead(a) => (
            a.harness_session_id.as_deref(),
            a.harness.as_deref(),
            Some(a),
        ),
        Selected::Session(s) => (
            s.view.session_id.as_deref(),
            s.view.harness.as_deref(),
            s.agent.as_ref(),
        ),
    };
    if key == b'\r' {
        return super::node_detail::press_session(view, sid, harness, captured, sock).await;
    }
    let Some(agent) = super::node_detail::resolve_session(view, sid, harness, captured) else {
        view.set_notice("no registry row".into());
        return Ok(());
    };
    if key == b'd' {
        open(view, agent);
        return Ok(());
    }
    let action = match key {
        b' ' => MenuAction::Peek,
        b'x' => MenuAction::Stop,
        b'P' => MenuAction::PortalPicker,
        _ => return Ok(()),
    };
    super::row_menu::execute_row_menu_action(
        view,
        action,
        MenuTarget::Agent(AgentIdent::of(&agent)),
        sock,
    )
    .await
}

fn open(view: &mut View, row: AgentRow) {
    let Some(board) = view.org_board.as_mut() else {
        return;
    };
    board.detail_request = board.detail_request.wrapping_add(1);
    let request = board.detail_request;
    let identity = identity(&row);
    board.detail = Some(WorkerDetail {
        row: row.clone(),
        request,
        identity: identity.clone(),
        roster: None,
        scroll: 0,
    });
    let Some(tx) = board.tx.clone() else {
        board.detail.as_mut().unwrap().roster = Some(Err("roster read channel unavailable".into()));
        return;
    };
    let gen = board.gen;
    tokio::spawn(async move {
        let result = read_roster(&row).await;
        let _ = tx.send((
            gen,
            OrgMsg::Detail {
                request,
                identity,
                result,
            },
        ));
    });
}

async fn read_roster(row: &AgentRow) -> Result<Value, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("fno-agents")
            .args(["list", "--json"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "workers not read: fno-agents timed out after 30s".to_string())?
    .map_err(|e| format!("workers not read: fno-agents spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "workers not read: fno-agents exited {}: {}",
            output
                .status
                .code()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "signal".into()),
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(400)
                .collect::<String>()
        ));
    }
    select_roster(&output.stdout, row)
}

pub(crate) fn select_roster(bytes: &[u8], row: &AgentRow) -> Result<Value, String> {
    let doc: Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("workers not read: invalid JSON: {e}"))?;
    let rows = doc
        .as_array()
        .or_else(|| doc.get("agents").and_then(Value::as_array))
        .or_else(|| doc.get("entries").and_then(Value::as_array))
        .ok_or_else(|| "workers not read: roster has no rows array".to_string())?;
    let sid = row
        .harness_session_id
        .as_deref()
        .or(row.attach_id.as_deref())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "workers not read: session identity unavailable".to_string())?;
    let matches: Vec<_> = rows
        .iter()
        .filter(|r| {
            let compatible = row.harness.as_deref().is_none_or(|h| {
                r.get("harness")
                    .or_else(|| r.get("provider"))
                    .and_then(Value::as_str)
                    == Some(h)
            });
            compatible
                && ["harness_session_id", "session_id", "short_id"]
                    .iter()
                    .any(|key| {
                        r.get(key)
                            .and_then(Value::as_str)
                            .is_some_and(|id| id == sid || (sid.len() <= 8 && id.starts_with(sid)))
                    })
        })
        .collect();
    match matches.as_slice() {
        [found] => Ok((*found).clone()),
        [] => Err("workers not read: no registry row for selected session".into()),
        _ => Err("workers not read: ambiguous session identity".into()),
    }
}

fn text(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("unobserved (not served)")
        .into()
}
fn based(row: &Value, key: &str) -> String {
    let basis = row.get(format!("{key}_basis")).or_else(|| {
        if key == "status" {
            row.get("basis")
        } else {
            None
        }
    });
    format!(
        "{} ({})",
        text(row, key),
        basis.and_then(Value::as_str).unwrap_or("basis not served")
    )
}
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    digits
        .chars()
        .enumerate()
        .fold(String::new(), |mut s, (i, c)| {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                s.push(',')
            }
            s.push(c);
            s
        })
}
impl WorkerDetail {
    pub(crate) fn lines(&self, width: usize) -> Vec<BLine> {
        let a = &self.row;
        let now = crate::digest_overlay::now_secs();
        let context = a
            .context_used_pct
            .map(|p| format!("{p}% used {}", super::row_meter::ctx_cell(Some(p))))
            .unwrap_or_else(|| "unmeasured (no context reading)".into());
        let tokens = a
            .context_tokens
            .map(|(u, w)| format!("{} of {}", grouped(u), grouped(w)))
            .unwrap_or_else(|| "tokens not served".into());
        let age = a
            .context_measured_at
            .map(|t| format!("{}s ago", now.saturating_sub(t)))
            .unwrap_or_else(|| "never measured".into());
        let mut body = vec![
            format!("Worker {} · esc back · j/k scroll", a.name),
            format!("Context: {context} · {tokens}"),
            format!("Source: transcript probe (liveness sweep) · measured {age}"),
            format!(
                "Runtime: {} · route {} · account {}",
                a.harness.as_deref().unwrap_or("not served"),
                a.route.as_deref().unwrap_or("not served"),
                a.account.as_deref().unwrap_or("not served")
            ),
            format!(
                "Model: {} (served selection; basis not carried)",
                a.model.as_deref().unwrap_or("not served")
            ),
            format!(
                "Up: {} · PR: {}",
                super::row_meter::up_cell(a.started_at, now),
                a.pr.map(|p| format!("#{p}"))
                    .unwrap_or_else(|| "not served".into())
            ),
            format!(
                "Node: {} · spawned by {}",
                a.node.as_deref().unwrap_or("not served"),
                a.spawned_by_name
                    .as_deref()
                    .or(a.spawned_by_session.as_deref())
                    .unwrap_or("not served")
            ),
            format!(
                "Queue: {} unread",
                a.mail_unread
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "not measured".into())
            ),
            format!(
                "Activity: {:?} ({}) · last {}s ago (served age)",
                a.badge,
                a.basis.as_deref().unwrap_or("basis not served"),
                a.last_activity_age_s
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unmeasured".into())
            ),
        ];
        let mut needs = a
            .answerable
            .as_ref()
            .map(|p| p.prompt.clone())
            .or_else(|| {
                (a.badge == Some(AgentBadge::Blocked)).then(|| {
                    a.reason
                        .clone()
                        .unwrap_or_else(|| "blocked (reason not served)".into())
                })
            })
            .unwrap_or_else(|| "nothing observed".into());
        match &self.roster {
            None => body.push("Reading roster; full last message not yet read".into()),
            Some(Err(error)) => body.push(error.clone()),
            Some(Ok(row)) => {
                let observed = row
                    .get("observed_model")
                    .and_then(|r| r.get("model"))
                    .and_then(Value::as_str)
                    .unwrap_or("unobserved");
                let basis = row
                    .get("observed_model")
                    .and_then(|r| r.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or("observation not served");
                body.push(format!("Observed model: {observed} ({basis})"));
                body.push(format!(
                    "Requested model: {}",
                    based(
                        row,
                        if row.get("requested_model").is_some() {
                            "requested_model"
                        } else {
                            "model"
                        }
                    )
                ));
                body.push(format!(
                    "Effort: {} · status: {} · progress: {}",
                    based(row, "effort"),
                    based(row, "status"),
                    based(row, "progress")
                ));
                body.push(format!(
                    "Last activity: {} ({})",
                    text(row, "last_event_at"),
                    text(row, "last_activity_basis")
                ));
                if row.get("progress").and_then(Value::as_str) == Some("awaiting-operator") {
                    needs = text(row, "last_message");
                }
                body.push(format!(
                    "Last message (served in full): {}",
                    text(row, "last_message")
                ));
            }
        }
        body.push(format!("Needs-you: {needs}"));
        let mut wrapped = Vec::new();
        for line in body {
            for paragraph in line.lines() {
                super::node_detail::wrap_line(paragraph, width.max(1), &mut wrapped);
            }
        }
        wrapped
            .into_iter()
            .skip(self.scroll)
            .map(BLine::plain)
            .collect()
    }
}
