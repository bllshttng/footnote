use super::backlog_style::{BLine, BRole, BSeg};
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
    segs: Vec<BSeg>,
    depth: usize,
    selected: Selected,
}
fn row(key: String, depth: usize, segs: Vec<BSeg>, selected: Selected) -> Row {
    Row {
        key,
        text: segs.iter().map(|s| s.text.as_str()).collect(),
        segs,
        depth,
        selected,
    }
}
fn seg(text: impl Into<String>, role: BRole) -> BSeg {
    BSeg {
        text: text.into(),
        role,
    }
}
fn fold_mark(collapsed: bool) -> BSeg {
    seg(if collapsed { "▸ " } else { "▾ " }, BRole::Meta)
}
type GraphMemo = (
    u64,
    usize,
    OrgSessions,
    String,
    super::org_graph::Graph,
    Vec<(usize, String)>,
);
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
    departures: HashSet<String>,
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
            departures: HashSet::new(),
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
        for lead in &tree.leads {
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
                    let segs = session_segs(session, now());
                    let label: String = segs.iter().map(|s| s.text.as_str()).collect();
                    if node_match || label.to_lowercase().contains(&query) {
                        sessions.push(row(
                            format!("session:{}:{}", node.view.card.id, sessions.len()),
                            2,
                            segs,
                            Selected::Session(session.clone()),
                        ));
                    }
                }
                if !node_match && sessions.is_empty() {
                    continue;
                }
                let key = format!("node:{}", node.view.card.id);
                let mut segs = vec![
                    fold_mark(self.collapsed.contains(&key)),
                    seg(node.view.card.id.clone(), BRole::Label),
                ];
                if let Some(status) = &node.view.card.status {
                    segs.push(seg(format!(" {status}"), BRole::Pill));
                }
                segs.push(seg(format!(" {}", node.view.card.title), BRole::Body));
                if let Some(pr) = node.view.prs.first() {
                    segs.push(seg(format!(" #{}", pr.number), BRole::Meta));
                }
                children.push(row(key.clone(), 1, segs, Selected::Node(node.view.clone())));
                if self.mode != OrgMode::Tree || !self.collapsed.contains(&key) {
                    children.extend(sessions);
                }
            }
            if !lead_match && children.is_empty() {
                continue;
            }
            let key = format!("lead:{}", lead.holder.name);
            let mut segs = vec![
                fold_mark(self.collapsed.contains(&key)),
                seg(lead.holder.name.clone(), BRole::Head),
                seg(format!(" L{} {}", lead.level, lead.scope), BRole::Meta),
            ];
            let counts = counts_line(&lead.owned_counts);
            for part in [
                Some(counts).filter(|c| !c.is_empty()),
                lead.stuck_line.clone(),
            ]
            .into_iter()
            .flatten()
            {
                segs.push(seg(format!(" · {part}"), BRole::Meta));
            }
            rows.push(row(
                key.clone(),
                0,
                segs,
                Selected::Lead(lead.holder.clone()),
            ));
            if self.mode != OrgMode::Tree || !self.collapsed.contains(&key) {
                rows.extend(children);
                if !lead.left.is_empty() {
                    rows.push(row(
                        format!("left-team:{}", lead.scope),
                        1,
                        vec![
                            fold_mark(!self.departures.contains(&lead.scope)),
                            seg(
                                format!("left the team (24h): {}", lead.left.len()),
                                BRole::Meta,
                            ),
                        ],
                        Selected::Lead(lead.holder.clone()),
                    ));
                    for node in lead
                        .left
                        .iter()
                        .filter(|_| self.departures.contains(&lead.scope))
                    {
                        let last = node.current.iter().chain(&node.former).max_by_key(|s| {
                            s.view.ended_at.as_deref().or(s.view.started_at.as_deref())
                        });
                        let mut segs = vec![
                            seg(node.view.card.id.clone(), BRole::Label),
                            seg(format!(" {}", node.view.card.title), BRole::Body),
                        ];
                        if let Some(last) = last {
                            segs.push(seg(" · last session: ", BRole::Meta));
                            segs.extend(session_segs(last, now()));
                        }
                        rows.push(row(
                            format!("left:{}", node.view.card.id),
                            2,
                            segs,
                            Selected::Node(node.view.clone()),
                        ));
                    }
                }
            }
        }
        let mut unowned_index = 0;
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
                    command: None,
                },
                agent: Some(agent.clone()),
            };
            let mut segs = vec![seg("Unowned · ", BRole::Meta)];
            segs.extend(session_segs(&session, now()));
            let unowned = row(
                super::org_graph::unowned_key(agent, unowned_index),
                0,
                segs,
                Selected::Session(session),
            );
            if unowned.text.to_lowercase().contains(&query) {
                rows.push(unowned);
                unowned_index += 1;
            }
        }
        rows
    }
    /// Key hints lead so a narrow pane cuts the counts first.
    pub(crate) fn footer_hints(&self, focused: bool) -> String {
        format!(
            "{}j/k move · enter act · tab mode · s sessions · / find · F full · ? keys · esc close · {}",
            if focused { "" } else { "tap to focus · " },
            self.footer()
        )
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
            filtered.unowned.retain(|a| self.session_shown(true) && visible.iter().any(|r|matches!(&r.selected,Selected::Session(s) if s.agent.as_ref().is_some_and(|r|r == a))));
            let layout = super::org_graph::layout(&filtered, width);
            let keys = visible
                .iter()
                .enumerate()
                .filter(|(_, r)| layout.boxes.iter().any(|p| p.key == r.key))
                .map(|(i, r)| (i, r.key.clone()))
                .collect();
            *memo = Some((
                self.body_gen,
                width,
                self.filter,
                self.query.clone(),
                layout,
                keys,
            ));
        }
        let cached = memo.as_ref().expect("graph memo filled");
        let selected = cached
            .5
            .iter()
            .find(|(i, _)| *i == self.cursor)
            .map(|(_, key)| key.as_str());
        super::org_graph::lines(&cached.4, width, height, self.pan, selected)
    }

    fn move_graph_cursor(&mut self, down: bool) {
        let memo = self.graph.borrow();
        let Some(cached) = memo
            .as_ref()
            .filter(|m| m.0 == self.body_gen && m.2 == self.filter && m.3 == self.query)
        else {
            return;
        };
        let next = if down {
            cached.5.iter().find(|(i, _)| *i > self.cursor)
        } else {
            cached.5.iter().rev().find(|(i, _)| *i < self.cursor)
        };
        if let Some((index, _)) = next {
            self.cursor = *index;
        }
    }
    pub(crate) fn lines(&self, width: usize, height: usize) -> Vec<BLine> {
        if let Some(detail) = &self.detail {
            return detail.lines(width);
        }
        if self.keys_help {
            return [
                "Org keys",
                "hjkl/arrows move · h/l fold/open",
                "Tab tree/table/graph · s current/former/all",
                "F full screen · / find · esc agents",
                "enter or a second tap acts · space peek · x stop · P portal · d detail",
            ]
            .into_iter()
            .map(BLine::plain)
            .collect();
        }
        let mut header = Vec::new();
        for (i, (_, mode)) in mode_tabs().into_iter().enumerate() {
            if i > 0 {
                header.push(seg(" │ ", BRole::Meta));
            }
            let role = if mode == self.mode {
                BRole::Head
            } else {
                BRole::Meta
            };
            header.push(seg(format!("{mode:?}"), role));
        }
        header.push(seg(
            format!(" · {}", format!("{:?}", self.filter).to_lowercase()),
            BRole::Meta,
        ));
        let query = self.input.as_deref().unwrap_or(&self.query);
        if self.input.is_some() || !query.is_empty() {
            header.push(seg(format!(" · /{query}"), BRole::Meta));
        }
        if self.mode == OrgMode::Graph {
            let graph = self.graph_lines(width, height.saturating_sub(2));
            let memo = self.graph.borrow();
            if let Some(cached) = memo.as_ref() {
                if let Some((_, key)) = cached.5.iter().find(|(i, _)| *i == self.cursor) {
                    if let Some(placed) = cached.4.boxes.iter().find(|p| &p.key == key) {
                        let label = placed.text.trim_matches(['┌', '┐', '├', '└', ' ']);
                        header.push(seg(format!(" · ▶ {label}"), BRole::Meta));
                    }
                }
            }
            let mut lines = vec![BLine::of(&header)];
            lines.extend(graph.into_iter().map(BLine::plain));
            return lines;
        }
        let mut lines = vec![BLine::of(&header)];
        if self.mode == OrgMode::Table {
            lines.push(BLine::meta(table_header(width)));
        }
        let rows = self.rows();
        let guides = tree_guides(&rows);
        for (i, row) in rows.iter().enumerate() {
            let mut line = if self.mode == OrgMode::Table {
                BLine::plain(table_row(row, width))
            } else {
                let mut segs = vec![seg(guides[i].clone(), BRole::Meta)];
                segs.extend(row.segs.iter().cloned());
                BLine::of(&segs)
            };
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
        None => "",
    }
}
/// The header's mode tabs: one list feeds both the paint and the tap target.
fn mode_tabs() -> [(std::ops::Range<usize>, OrgMode); 3] {
    let mut col = 0;
    [OrgMode::Tree, OrgMode::Table, OrgMode::Graph].map(|mode| {
        let end = col + format!("{mode:?}").len();
        let tab = (col..end, mode);
        col = end + " │ ".chars().count();
        tab
    })
}
/// Owned counts as words, busiest state first: `4 working · 2 in review`.
fn counts_line(counts: &serde_json::Value) -> String {
    const ORDER: [&str; 4] = ["in_progress", "in_review", "ready", "blocked"];
    let Some(map) = counts.as_object() else {
        return String::new();
    };
    let mut items: Vec<(&str, u64)> = map
        .iter()
        .filter_map(|(k, v)| v.as_u64().filter(|n| *n > 0).map(|n| (k.as_str(), n)))
        .collect();
    items.sort_by_key(|(k, _)| (ORDER.iter().position(|o| o == k).unwrap_or(ORDER.len()), *k));
    items
        .into_iter()
        .map(|(k, n)| match k {
            "in_progress" => format!("{n} working"),
            k => format!("{n} {}", k.replace('_', " ")),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}
/// `├ `/`└ ` per row from depth alone, so a filtered or folded tree still
/// closes each branch on its real last child.
fn tree_guides(rows: &[Row]) -> Vec<String> {
    // Walk back once: a row is last when no later sibling precedes its parent's end.
    let mut sibling_after = [false; 3];
    let mut last = vec![true; rows.len()];
    for (i, r) in rows.iter().enumerate().rev() {
        let d = r.depth.min(2);
        last[i] = !sibling_after[d];
        sibling_after[d] = true;
        sibling_after[d + 1..].fill(false);
    }
    let mut parent_last = true;
    rows.iter()
        .zip(last)
        .map(|(r, last)| {
            let tee = if last { "└ " } else { "├ " };
            match r.depth {
                0 => String::new(),
                1 => {
                    parent_last = last;
                    tee.into()
                }
                _ => format!("{}{tee}", if parent_last { "  " } else { "│ " }),
            }
        })
        .collect()
}
fn session_segs(session: &OrgSession, now: u64) -> Vec<BSeg> {
    let Some(a) = &session.agent else {
        let view = &session.view;
        let runtime = [view.harness.as_deref(), view.model.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("/");
        let parts = [
            Some("former".to_string()),
            view.phase.clone(),
            Some(runtime).filter(|r| !r.is_empty()),
            view.session_id
                .as_deref()
                .map(|id| id.chars().take(8).collect()),
        ];
        return vec![seg(
            parts.into_iter().flatten().collect::<Vec<_>>().join(" · "),
            BRole::Meta,
        )];
    };
    let glyph = crate::lattice::status_glyph(agent_lattice_state(a));
    let mut text = format!("{glyph} {}", a.name);
    let parts = [
        super::card_line::harness_model(a),
        a.context_used_pct
            .map(|p| super::row_meter::ctx_bar(Some(p)).trim_end().to_string()),
        a.last_activity_age_s.map(|age| {
            format!(
                "{} ago",
                super::row_meter::up_cell(Some(now.saturating_sub(age)), now)
            )
        }),
        a.mail_unread
            .filter(|n| *n > 0)
            .map(|n| format!("{n} unread")),
        a.pr.map(|n| format!("#{n}")),
        a.tail.clone().filter(|t| !t.is_empty()),
    ];
    for part in parts.into_iter().flatten() {
        text.push_str(" · ");
        text.push_str(&part);
    }
    vec![seg(text, BRole::Meta)]
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
    ("MAIL", 4),
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
            values[6] = n.card.status.clone().unwrap_or_default();
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
                .unwrap_or_default();
            values[3] = s.view.harness.clone().unwrap_or_default();
            values[4] = s.view.model.clone().unwrap_or_default();
            values[6] = "former".into();
            if let Some(a) = &s.agent {
                values[1] = a.node.clone().unwrap_or_default();
                values[4] = a.model.clone().unwrap_or_default();
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
                    .unwrap_or_default();
                values[9] = a.mail_unread.map(|n| n.to_string()).unwrap_or_default();
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
        SidelineView::Messages => {} // restored by the caller's messages arm
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
        &[BLine::meta(b.footer_hints(matches!(
            view.input_owner(),
            super::region_focus::RegionOwner::Board
        )))],
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
    let graph_width = if view.board_full {
        view.term.1 as usize
    } else {
        view.panel_w().saturating_sub(1) as usize
    };
    let graph_height = (view.term.0 as usize).saturating_sub(2);
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
                if b.mode == OrgMode::Graph {
                    b.graph_lines(graph_width, graph_height);
                    b.move_graph_cursor(false);
                } else {
                    b.cursor = b.cursor.saturating_sub(1);
                }
            }
            ModalKey::Down | ModalKey::Byte(b'j') => {
                if b.mode == OrgMode::Graph {
                    b.graph_lines(graph_width, graph_height);
                    b.move_graph_cursor(true);
                } else {
                    b.cursor = (b.cursor + 1).min(b.rows().len().saturating_sub(1));
                }
            }
            ModalKey::Left | ModalKey::Byte(b'h') => {
                if let Some(r) = b.rows().get(b.cursor) {
                    if let Some(scope) = r.key.strip_prefix("left-team:") {
                        b.departures.remove(scope);
                    } else {
                        b.collapsed.insert(r.key.clone());
                    }
                }
            }
            ModalKey::Right | ModalKey::Byte(b'l') => {
                if let Some(r) = b.rows().get(b.cursor) {
                    if let Some(scope) = r.key.strip_prefix("left-team:") {
                        b.departures.insert(scope.into());
                    } else {
                        b.collapsed.remove(&r.key);
                    }
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
#[path = "tests/org_board_fixture.rs"]
mod fixtures;
#[cfg(test)]
pub(super) use fixtures::check_fixture;

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
        let offset = if b.mode == OrgMode::Table { 2 } else { 1 };
        let len = rows.len() + offset;
        let start = if b.mode == OrgMode::Graph || len <= height {
            0
        } else {
            (b.cursor + offset)
                .saturating_sub(height.saturating_sub(1))
                .min(len - height)
        };
        if rep.row as usize + start == 0 {
            if let Some((_, mode)) = mode_tabs()
                .into_iter()
                .find(|(cols, _)| cols.contains(&(rep.col as usize)))
            {
                b.mode = mode;
                crate::view_store::save_org_prefs(b.mode, b.filter);
            }
            return Ok(());
        }
        let index = if b.mode == OrgMode::Graph {
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
            index
        } else {
            let clicked = rep.row as usize + start;
            if clicked < offset || clicked - offset >= rows.len() {
                return Ok(());
            }
            clicked - offset
        };
        // A tap selects; a tap on the row already selected acts like Enter.
        if index != b.cursor {
            b.cursor = index;
            return Ok(());
        }
        if let Some(selected) = b.selected() {
            dispatch(view, selected, b'\r', sock).await?;
        }
    }
    Ok(())
}
