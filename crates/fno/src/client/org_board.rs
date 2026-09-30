use super::*;
use crate::org_model::{OrgInputs, OrgSession, OrgSnapshot};
use crate::view_store::{OrgMode, OrgSessions, SidelineView};
use std::collections::HashSet;

pub(crate) type OrgTx = tokio::sync::mpsc::UnboundedSender<(u64, super::org_detail::OrgMsg)>;
#[derive(Clone)]
pub(crate) enum Selected {
    Lead(AgentRow),
    Node(crate::backlog_model::NodeView),
    Session(OrgSession),
}
struct Row {
    key: String,
    text: String,
    selected: Selected,
}
type GraphMemo = (u64, usize, OrgSessions, String, super::org_graph::Graph);
pub(crate) struct OrgBoard {
    pub(crate) snapshot: OrgSnapshot,
    pub(crate) inputs: Option<crate::backlog_model::Inputs>,
    pub(crate) cursor: usize,
    pub(crate) mode: OrgMode,
    pub(crate) filter: OrgSessions,
    pub(crate) query: String,
    pub(crate) gen: u64,
    pub(crate) detail: Option<super::org_detail::WorkerDetail>,
    pub(crate) detail_request: u64,
    pub(crate) tx: Option<OrgTx>,
    inflight: bool,
    last_read: Option<Instant>,
    collapsed: HashSet<String>,
    input: Option<String>,
    keys_help: bool,
    esc: Vec<u8>,
    pan: (usize, usize),
    body_gen: u64,
    graph: std::cell::RefCell<Option<GraphMemo>>,
}
impl OrgBoard {
    pub(crate) fn new(gen: u64) -> Self {
        let (mode, filter) = crate::view_store::load_org_prefs();
        Self {
            snapshot: OrgSnapshot::default(),
            inputs: None,
            cursor: 0,
            mode,
            filter,
            query: String::new(),
            gen,
            detail: None,
            detail_request: 0,
            tx: None,
            inflight: false,
            last_read: None,
            collapsed: HashSet::new(),
            input: None,
            keys_help: false,
            esc: Vec::new(),
            pan: (0, 0),
            body_gen: 0,
            graph: std::cell::RefCell::new(None),
        }
    }
    pub(crate) fn selected(&self) -> Option<Selected> {
        self.rows().get(self.cursor).map(|r| r.selected.clone())
    }
    fn session_shown(&self, current: bool) -> bool {
        self.filter == OrgSessions::All
            || (current && self.filter == OrgSessions::Current)
            || (!current && self.filter == OrgSessions::Former)
    }
    fn rows(&self) -> Vec<Row> {
        let Some(tree) = &self.snapshot.tree else {
            return Vec::new();
        };
        let query = self.query.to_lowercase();
        let mut rows = Vec::new();
        for (li, lead) in tree.leads.iter().enumerate() {
            let lead_match = format!("{} {}", lead.holder.name, lead.scope)
                .to_lowercase()
                .contains(&query);
            let mut children = Vec::new();
            for node in &lead.nodes {
                let node_match = lead_match
                    || format!("{} {}", node.view.card.id, node.view.card.title)
                        .to_lowercase()
                        .contains(&query);
                let mut sessions = Vec::new();
                for (current, session) in node
                    .current
                    .iter()
                    .map(|s| (true, s))
                    .chain(node.former.iter().map(|s| (false, s)))
                {
                    if !self.session_shown(current) {
                        continue;
                    }
                    let label = session_line(session, now());
                    if node_match || label.to_lowercase().contains(&query) {
                        sessions.push(Row {
                            key: format!("session:{}:{}", node.view.card.id, sessions.len()),
                            text: format!("    {label}"),
                            selected: Selected::Session(session.clone()),
                        });
                    }
                }
                if !node_match && sessions.is_empty() {
                    continue;
                }
                let key = format!("node:{}", node.view.card.id);
                children.push(Row {
                    key: key.clone(),
                    text: format!(
                        "  {} {} {} {} PR{} claim:{} {}",
                        if self.collapsed.contains(&key) {
                            "▸"
                        } else {
                            "▾"
                        },
                        node.view.card.id,
                        node.view.card.status.as_deref().unwrap_or("unobserved"),
                        node.view.card.title,
                        node.view
                            .prs
                            .first()
                            .map(|p| p.number.to_string())
                            .unwrap_or_else(|| "-".into()),
                        node.claim_state,
                        node.claim_holder.as_deref().unwrap_or("-")
                    ),
                    selected: Selected::Node(node.view.clone()),
                });
                if self.mode != OrgMode::Tree || !self.collapsed.contains(&key) {
                    children.extend(sessions);
                }
            }
            if !lead_match && children.is_empty() {
                continue;
            }
            let key = format!("lead:{}", lead.holder.name);
            rows.push(Row {
                key: key.clone(),
                text: format!(
                    "{} {} L{} {} {} owned:{} {}",
                    if self.collapsed.contains(&key) {
                        "▸"
                    } else {
                        "▾"
                    },
                    lead.holder.name,
                    lead.level,
                    lead.scope,
                    lead.counts,
                    lead.owned_counts,
                    lead.stuck_line.as_deref().unwrap_or("")
                ),
                selected: Selected::Lead(lead.holder.clone()),
            });
            if self.mode != OrgMode::Tree || !self.collapsed.contains(&key) {
                rows.extend(children);
                if !lead.left.is_empty() {
                    rows.push(Row {
                        key: format!("left:{li}"),
                        text: format!("  left the team (24h): {}", lead.left.len()),
                        selected: Selected::Lead(lead.holder.clone()),
                    });
                    for node in &lead.left {
                        rows.push(Row {
                            key: format!("left:{}", node.view.card.id),
                            text: format!("    {} {}", node.view.card.id, node.view.card.title),
                            selected: Selected::Node(node.view.clone()),
                        });
                    }
                }
            }
        }
        for agent in &tree.unowned {
            if !self.session_shown(true) {
                continue;
            }
            let session = OrgSession {
                view: crate::backlog_model::SessionView {
                    phase: None,
                    harness: agent.harness.clone(),
                    session_id: agent.harness_session_id.clone(),
                    model: agent.model.clone(),
                    started_at: None,
                    ended_at: None,
                    agent: Some(agent.name.clone()),
                    action: "none".into(),
                    reason: None,
                },
                agent: Some(agent.clone()),
            };
            let text = format!("Unowned · {}", session_line(&session, now()));
            if text.to_lowercase().contains(&query) {
                rows.push(Row {
                    key: format!("unowned:{}", agent.name),
                    text,
                    selected: Selected::Session(session),
                });
            }
        }
        rows
    }
    pub(crate) fn footer(&self) -> String {
        let error = self
            .snapshot
            .error
            .as_ref()
            .map(|e| {
                format!(
                    " · {e} (failed {}s ago)",
                    now().saturating_sub(self.snapshot.error_at.unwrap_or(now()))
                )
            })
            .unwrap_or_default();
        match &self.snapshot.tree {
            None => format!(
                "{}{}",
                if self.inflight {
                    "reading org"
                } else {
                    "org not read"
                },
                error
            ),
            Some(tree) => {
                let current: usize = tree
                    .leads
                    .iter()
                    .flat_map(|l| &l.nodes)
                    .map(|n| n.current.len())
                    .sum::<usize>()
                    + tree.unowned.len();
                let former: usize = tree
                    .leads
                    .iter()
                    .flat_map(|l| &l.nodes)
                    .map(|n| n.former.len())
                    .sum();
                format!(
                    "leads {} · current {current} · former {former} · read {}s ago{error}",
                    tree.leads.len(),
                    now().saturating_sub(tree.measured_at)
                )
            }
        }
    }
    fn graph_lines(&self, width: usize, height: usize) -> Vec<String> {
        let Some(tree) = &self.snapshot.tree else {
            return vec![];
        };
        let mut memo = self.graph.borrow_mut();
        let changed = memo.as_ref().is_none_or(|m| {
            m.0 != self.body_gen || m.1 != width || m.2 != self.filter || m.3 != self.query
        });
        if changed {
            let mut filtered = tree.clone();
            let visible = self.rows();
            for lead in &mut filtered.leads {
                lead.nodes.retain(|n| {
                    visible.iter().any(
                        |r| matches!(&r.selected,Selected::Node(v) if v.card.id==n.view.card.id),
                    )
                });
                for node in &mut lead.nodes {
                    if !self.session_shown(true) {
                        node.current.clear();
                    }
                    if !self.session_shown(false) {
                        node.former.clear();
                    }
                    let matches = |s: &OrgSession| {
                        visible.iter().any(|r| match &r.selected {
                            Selected::Session(v) => {
                                v.view.session_id == s.view.session_id
                                    && v.view.agent == s.view.agent
                            }
                            _ => false,
                        })
                    };
                    node.current.retain(matches);
                    node.former.retain(matches);
                }
            }
            filtered
                .leads
                .retain(|l| !l.nodes.is_empty() || self.query.is_empty());
            filtered.unowned.retain(|a| self.session_shown(true) && visible.iter().any(|r|matches!(&r.selected,Selected::Session(s) if s.agent.as_ref().is_some_and(|r|r.name==a.name))));
            *memo = Some((
                self.body_gen,
                width,
                self.filter,
                self.query.clone(),
                super::org_graph::layout(&filtered, width),
            ));
        }
        super::org_graph::lines(
            &memo.as_ref().expect("graph memo filled").4,
            width,
            height,
            self.pan,
        )
    }
    pub(crate) fn lines(&self, width: usize, height: usize) -> Vec<super::backlog_style::BLine> {
        use super::backlog_style::BLine;
        if let Some(detail) = &self.detail {
            return detail.lines(width);
        }
        if self.keys_help {
            return [
                "Org keys",
                "hjkl/arrows move · h/l fold/open",
                "Tab tree/table/graph · s current/former/all",
                "F full screen · / find · esc agents",
                "enter row · space peek · x stop · P portal · d detail",
            ]
            .into_iter()
            .map(BLine::plain)
            .collect();
        }
        let mut lines = vec![BLine::meta(format!(
            "Org {:?} · {:?} · {}",
            self.mode,
            self.filter,
            self.input.as_deref().unwrap_or(&self.query)
        ))];
        if self.mode == OrgMode::Graph {
            lines.extend(
                self.graph_lines(width, height.saturating_sub(2))
                    .into_iter()
                    .map(BLine::plain),
            );
            return lines;
        }
        if self.mode == OrgMode::Table {
            lines.push(BLine::meta(table_header(width)));
        }
        for (i, row) in self.rows().iter().enumerate() {
            let text = if self.mode == OrgMode::Table {
                table_row(row, width)
            } else {
                row.text.clone()
            };
            let mut line = BLine::plain(text);
            line.band = i == self.cursor;
            lines.push(line);
        }
        lines
    }
}
fn now() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}
fn status(a: &AgentRow) -> &'static str {
    if a.exited {
        return "former";
    }
    match a.badge {
        Some(AgentBadge::Working) => "working",
        Some(AgentBadge::Blocked) => "blocked",
        Some(AgentBadge::Done) => "done",
        None => "unobserved",
    }
}
fn session_line(session: &OrgSession, now: u64) -> String {
    match &session.agent {
        Some(a) => format!(
            "{} {} {} {} {} up {} age {}s Q{} {}",
            if a.exited { "former" } else { status(a) },
            a.name,
            a.harness.as_deref().unwrap_or("unobserved"),
            a.model.as_deref().unwrap_or("unobserved"),
            super::row_meter::ctx_cell(a.context_used_pct),
            super::row_meter::up_cell(a.started_at, now),
            a.last_activity_age_s
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            a.mail_unread
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            a.tail.as_deref().unwrap_or("")
        ),
        None => format!(
            "former {} {} {} {}",
            session.view.phase.as_deref().unwrap_or("unobserved"),
            session.view.session_id.as_deref().unwrap_or("unobserved"),
            session.view.harness.as_deref().unwrap_or("unobserved"),
            session.view.model.as_deref().unwrap_or("unobserved")
        ),
    }
}
const COLUMNS: [(&str, usize); 12] = [
    ("LEAD", 12),
    ("NODE", 10),
    ("SESSION", 18),
    ("RT", 8),
    ("MODEL", 15),
    ("CTX", 8),
    ("STATE", 12),
    ("UP", 6),
    ("AGE", 7),
    ("Q", 4),
    ("PR", 7),
    ("LAST", 36),
];
fn cells(values: &[String], width: usize) -> String {
    let mut text = String::new();
    for ((_, w), value) in COLUMNS.iter().zip(values) {
        if text.chars().count() + w + 1 > width {
            break;
        }
        let trimmed = value.chars().take(*w).collect::<String>();
        text.push_str(&trimmed);
        text.push_str(&" ".repeat(w - trimmed.chars().count() + 1));
    }
    text
}
fn table_header(width: usize) -> String {
    cells(
        &COLUMNS
            .iter()
            .map(|(n, _)| n.to_string())
            .collect::<Vec<_>>(),
        width,
    )
}
fn table_row(row: &Row, width: usize) -> String {
    let mut values = vec![String::new(); 12];
    match &row.selected {
        Selected::Lead(a) => {
            values[0] = a.name.clone();
        }
        Selected::Node(n) => {
            values[1] = n.card.id.clone();
            values[6] = n.card.status.clone().unwrap_or_else(|| "unobserved".into());
            values[10] = n
                .prs
                .first()
                .map(|p| p.number.to_string())
                .unwrap_or_default();
            values[11] = n.card.title.clone();
        }
        Selected::Session(s) => {
            values[2] = s
                .agent
                .as_ref()
                .map(|a| a.name.clone())
                .or(s.view.session_id.clone())
                .unwrap_or_else(|| "unobserved".into());
            values[3] = s
                .view
                .harness
                .clone()
                .unwrap_or_else(|| "unobserved".into());
            values[4] = s.view.model.clone().unwrap_or_else(|| "unobserved".into());
            values[6] = "former".into();
            if let Some(a) = &s.agent {
                values[1] = a.node.clone().unwrap_or_default();
                values[4] = a.model.clone().unwrap_or_else(|| "unobserved".into());
                values[5] = super::row_meter::ctx_cell(a.context_used_pct);
                values[6] = if a.exited {
                    "former".into()
                } else {
                    status(a).into()
                };
                values[7] = super::row_meter::up_cell(a.started_at, now());
                values[8] = a
                    .last_activity_age_s
                    .map(|n| format!("{n}s"))
                    .unwrap_or_else(|| "-".into());
                values[9] = a
                    .mail_unread
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".into());
                values[10] = a.pr.map(|n| n.to_string()).unwrap_or_default();
                values[11] = a.tail.clone().unwrap_or_default();
            }
        }
    }
    cells(&values, width)
}
pub(crate) fn open(view: &mut View) {
    view.org_generation = view.org_generation.wrapping_add(1);
    view.org_board = Some(OrgBoard::new(view.org_generation));
    view.backlog_board = None;
    view.region_owner = super::region_focus::RegionOwner::Board;
    super::backlog_board::set_sideline_view(view, SidelineView::Org);
}
pub(crate) fn restore(view: &mut View) {
    match view.sideline_view {
        SidelineView::Org => open(view),
        SidelineView::Backlog if view.experimental_backlog => View::open(view),
        SidelineView::Backlog => {
            super::backlog_board::set_sideline_view(view, SidelineView::Agents)
        }
        SidelineView::Agents => {}
    }
}
pub(crate) fn maybe_kick(view: &mut View, tx: &OrgTx) {
    let Some(b) = view.org_board.as_mut() else {
        return;
    };
    b.tx = Some(tx.clone());
    if b.inflight
        || b.last_read
            .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
    {
        return;
    }
    b.inflight = true;
    b.last_read = Some(Instant::now());
    let gen = b.gen;
    let agents = view.layout.agents.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let inputs = crate::org_model::gather(&crate::backlog_view::graph_path(), agents).await;
        let _ = tx.send((gen, super::org_detail::OrgMsg::Gather(inputs)));
    });
}
pub(crate) fn apply_fold(view: &mut View, gen: u64, inputs: OrgInputs) {
    let Some(b) = view.org_board.as_mut().filter(|b| b.gen == gen) else {
        return;
    };
    b.inflight = false;
    b.snapshot.apply(&inputs, now());
    if b.snapshot.error.is_none() {
        b.inputs = Some(inputs.backlog);
        b.body_gen = b.body_gen.wrapping_add(1);
        *b.graph.borrow_mut() = None;
    }
    b.cursor = b.cursor.min(b.rows().len().saturating_sub(1));
}
pub(crate) fn paint(
    view: &View,
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    width: usize,
    height: usize,
) {
    let Some(b) = &view.org_board else {
        return;
    };
    let width = width.min(cols);
    let height = height.min(rows);
    if width == 0 || height == 0 {
        return;
    }
    for r in 0..height {
        for c in 0..width {
            cells[r * cols + c] = Cell::default();
        }
    }
    let lines = b.lines(width, height);
    super::backlog_style::paint_panel(
        cells,
        rows,
        cols,
        0,
        width,
        height.saturating_sub(1),
        &lines,
        if b.detail.is_some() {
            None
        } else {
            Some(b.cursor + if b.mode == OrgMode::Table { 2 } else { 1 })
        },
        &view.theme,
    );
    super::backlog_style::paint_panel(
        cells,
        rows,
        cols,
        height - 1,
        width,
        1,
        &[super::backlog_style::BLine::meta(b.footer())],
        None,
        &view.theme,
    );
}
pub(crate) async fn route_keys(
    view: &mut View,
    scanner: &mut Scanner,
    bytes: &[u8],
    sock: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    scanner.disarm_repeat();
    for event in scanner.scan(bytes, Instant::now()) {
        match event {
            Event::Forward(chunk) => {
                keys(view, &chunk, sock).await?;
            }
            event => match dispatch_event(view, event, sock).await? {
                DispatchFlow::Continue => {}
                DispatchFlow::Break => break,
                DispatchFlow::Detach => return Ok(StdinFlow::Detach),
            },
        }
    }
    Ok(StdinFlow::Continue)
}
pub(crate) async fn keys(
    view: &mut View,
    bytes: &[u8],
    sock: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let Some(b) = view.org_board.as_mut() else {
        return Ok(StdinFlow::Continue);
    };
    if let Some(input) = b.input.as_mut() {
        for byte in bytes {
            match byte {
                27 => {
                    b.input = None;
                    break;
                }
                b'\r' | b'\n' => {
                    b.query = input.clone();
                    b.input = None;
                    b.cursor = 0;
                    break;
                }
                127 | 8 => {
                    input.pop();
                }
                32..=126 => input.push(*byte as char),
                _ => {}
            }
        }
        return Ok(StdinFlow::Continue);
    }
    let tokens = if bytes == [27] && b.esc.is_empty() {
        vec![ModalKey::Esc]
    } else {
        fold_modal_keys(&mut b.esc, bytes)
    };
    for token in tokens {
        let Some(b) = view.org_board.as_mut() else {
            break;
        };
        if let Some(detail) = b.detail.as_mut() {
            match token {
                ModalKey::Esc | ModalKey::Byte(b'q') => {
                    b.detail = None;
                }
                ModalKey::Up | ModalKey::Byte(b'k') => {
                    detail.scroll = detail.scroll.saturating_sub(1)
                }
                ModalKey::Down | ModalKey::Byte(b'j') => {
                    detail.scroll = detail.scroll.saturating_add(1)
                }
                _ => {}
            }
            continue;
        }
        match token {
            ModalKey::Esc | ModalKey::Byte(b'q') => {
                if b.keys_help {
                    b.keys_help = false;
                } else {
                    view.org_board = None;
                    super::backlog_board::set_sideline_view(view, SidelineView::Agents);
                }
            }
            ModalKey::Byte(b'V') => super::backlog_board::cycle_sideline_view(view),
            ModalKey::Byte(b'F') => {
                view.board_full = !view.board_full;
                crate::view_store::save_board_full(view.board_full);
            }
            ModalKey::Byte(b'\t') => {
                b.mode = b.mode.next();
                crate::view_store::save_org_prefs(b.mode, b.filter);
            }
            ModalKey::Byte(b's') => {
                b.filter = b.filter.next();
                b.cursor = 0;
                crate::view_store::save_org_prefs(b.mode, b.filter);
            }
            ModalKey::Byte(b'/') => {
                b.input = Some(b.query.clone());
            }
            ModalKey::Byte(b'?') => {
                b.keys_help = !b.keys_help;
            }
            ModalKey::Up if b.mode == OrgMode::Graph => {
                b.pan.1 = b.pan.1.saturating_sub(1);
            }
            ModalKey::Down if b.mode == OrgMode::Graph => {
                b.pan.1 = b.pan.1.saturating_add(1);
            }
            ModalKey::Left if b.mode == OrgMode::Graph => {
                b.pan.0 = b.pan.0.saturating_sub(4);
            }
            ModalKey::Right if b.mode == OrgMode::Graph => {
                b.pan.0 = b.pan.0.saturating_add(4);
            }
            ModalKey::Up | ModalKey::Byte(b'k') => {
                b.cursor = b.cursor.saturating_sub(1);
            }
            ModalKey::Down | ModalKey::Byte(b'j') => {
                b.cursor = (b.cursor + 1).min(b.rows().len().saturating_sub(1));
            }
            ModalKey::Left | ModalKey::Byte(b'h') => {
                if let Some(r) = b.rows().get(b.cursor) {
                    b.collapsed.insert(r.key.clone());
                }
            }
            ModalKey::Right | ModalKey::Byte(b'l') => {
                if let Some(r) = b.rows().get(b.cursor) {
                    b.collapsed.remove(&r.key);
                }
            }
            ModalKey::Enter => {
                if let Some(selected) = b.selected() {
                    dispatch(view, selected, b'\r', sock).await?;
                }
            }
            ModalKey::Byte(key @ (b' ' | b'x' | b'P' | b'd')) => {
                if let Some(selected) = b.selected() {
                    dispatch(view, selected, key, sock).await?;
                }
            }
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}
pub(crate) use super::org_detail::dispatch;

#[cfg(test)]
pub(super) fn check_fixture(view: &mut View) {
    use serde_json::json;
    let lead = AgentRow {
        name: "finch".into(),
        crown_scope: Some("team".into()),
        crown_level: Some(1),
        ..Default::default()
    };
    let worker = |name: &str, sid: &str, node: &str| AgentRow {
        name: name.into(),
        harness_session_id: Some(sid.into()),
        node: Some(node.into()),
        ..Default::default()
    };
    let backlog = crate::backlog_model::Inputs {
        rows: vec![
            json!({"id":"x-1","status":"ready","title":"First","sessions":[{"session_id":"s1"},{"session_id":"old1"},{"session_id":"old2"}]}),
            json!({"id":"x-2","status":"ready","title":"Second","blocked_by":["x-1"],"sessions":[{"session_id":"s2"},{"session_id":"s3"}]}),
        ],
        agents: vec![
            lead,
            worker("first", "s1", "x-1"),
            worker("second", "s2", "x-2"),
            worker("third", "s3", "x-2"),
        ],
        ..Default::default()
    };
    open(view);
    let gen = view.org_generation;
    apply_fold(
        view,
        gen,
        OrgInputs {
            backlog,
            fold: Ok(
                json!({"scope_nodes":{"team":{"status":"ok","nodes":[{"id":"x-1"},{"id":"x-2"}]}},"owned_scopes":{"x-1":"team","x-2":"team"}}),
            ),
            measured_at: now(),
        },
    );
    let b = view.org_board.as_mut().unwrap();
    b.mode = OrgMode::Tree;
    b.filter = OrgSessions::Current;
    let texts = b
        .lines(60, 24)
        .into_iter()
        .map(|l| l.text)
        .collect::<Vec<_>>();
    assert!(texts[1].contains("finch"));
    assert!(texts[2].contains("x-1"));
    assert!(texts[3].contains("first"));
    assert!(texts[4].contains("x-2"));
    assert!(b.footer().starts_with("leads 1 · current 3"));
    b.filter = OrgSessions::Former;
    assert_eq!(
        b.rows()
            .iter()
            .filter(|r| matches!(&r.selected, Selected::Session(_)))
            .count(),
        2
    );
    b.filter = OrgSessions::All;
    assert_eq!(
        b.rows()
            .iter()
            .filter(|r| matches!(&r.selected, Selected::Session(_)))
            .count(),
        5
    );
    b.filter = OrgSessions::Current;
    b.mode = OrgMode::Graph;
    let first = b.graph_lines(100, 20);
    let allocation = b.graph.borrow().as_ref().unwrap().4.boxes.as_ptr();
    assert_eq!(first, b.graph_lines(100, 20));
    assert_eq!(
        allocation,
        b.graph.borrow().as_ref().unwrap().4.boxes.as_ptr(),
        "same frame reuses layout storage"
    );
    assert!(
        first.iter().any(|l| l.contains('◀')),
        "dependency edge has a visible endpoint"
    );
    b.query = "first".into();
    let graph = b.graph_lines(100, 20);
    assert!(!graph.iter().any(|l| l.contains("second")));
    b.query.clear();
    b.mode = OrgMode::Tree;
    let generation = view.org_generation;
    view.org_board = None;
    open(view);
    assert!(view.org_generation > generation);
    let mut stale = crate::org_model::OrgInputs {
        backlog: Default::default(),
        fold: Err("stale".into()),
        measured_at: 0,
    };
    apply_fold(view, generation, stale.clone());
    assert!(view.org_board.as_ref().unwrap().snapshot.error.is_none());
    stale.fold = Err("fresh failure".into());
    let generation = view.org_generation;
    apply_fold(view, generation, stale);
    assert_eq!(
        view.org_board.as_ref().unwrap().snapshot.error.as_deref(),
        Some("fresh failure")
    );
}
pub(crate) async fn mouse(
    view: &mut View,
    rep: crate::mouse::MouseReport,
    sock: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let width = if view.board_full {
        view.term.1 as usize
    } else {
        view.panel_w().saturating_sub(1) as usize
    };
    let height = (view.term.0 as usize).saturating_sub(if view.board_full {
        1
    } else {
        1 + view.bottom_row_is_chrome() as usize
    });
    if matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
        view.region_owner = super::region_focus::RegionOwner::Board;
    }
    let Some(b) = view.org_board.as_mut() else {
        return Ok(());
    };
    if b.detail.is_some() {
        return Ok(());
    }
    if matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
        let rows = b.rows();
        if b.mode == OrgMode::Graph {
            b.graph_lines(width, height.saturating_sub(1));
            let memo = b.graph.borrow();
            let x = rep.col as usize + b.pan.0;
            let y = (rep.row as usize).saturating_sub(1) + b.pan.1;
            let hit = memo.as_ref().and_then(|m| {
                m.4.boxes
                    .iter()
                    .find(|p| p.y == y && x >= p.x && x < p.x + p.text.chars().count())
            });
            let Some(index) = hit.and_then(|p| rows.iter().position(|r| r.key == p.key)) else {
                return Ok(());
            };
            b.cursor = index;
        } else {
            let offset = if b.mode == OrgMode::Table { 2 } else { 1 };
            let len = rows.len() + offset;
            let start = if len > height {
                (b.cursor + offset)
                    .saturating_sub(height.saturating_sub(1))
                    .min(len - height)
            } else {
                0
            };
            let clicked = rep.row as usize + start;
            if clicked < offset || clicked - offset >= rows.len() {
                return Ok(());
            }
            b.cursor = clicked - offset;
        }
        if let Some(selected) = b.selected() {
            dispatch(view, selected, b'\r', sock).await?;
        }
    }
    Ok(())
}
