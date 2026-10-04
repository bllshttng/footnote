//! The Messages tab: three columns over the full terminal - the flat chat
//! list of agents and channels, the selected agent's chats, and the
//! selected conversation as a two-sided bubble thread. The model is the
//! `mail-threads` projection; painting runs through the sideline's BLine
//! path; keys and mouse mirror org_board. The strip row
//! (`Agents  Messages`) is the sideline's, not this module's.

use super::backlog_style::{BLine, BRole, BSeg};
use super::*;
use crate::messages_model::MessagesSnapshot;
use crate::org_model::OrgTree;
use crate::view_store::SidelineView;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_MESSAGES_GEN: AtomicU64 = AtomicU64::new(0);

fn seg(text: impl Into<String>, role: BRole) -> BSeg {
    BSeg {
        text: text.into(),
        role,
    }
}

/// A JSON bool the reader trusts; the projection carries real booleans.
fn bool_of(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// The channel row's display word: the retired `kings` scope reads as
/// `agents`, everything else as itself, never with a `#`.
fn channel_label(scope: &str) -> String {
    if scope == "kings" {
        "agents".to_string()
    } else {
        scope.to_string()
    }
}

pub(crate) type MessagesTx =
    tokio::sync::mpsc::UnboundedSender<(u64, Result<Value, String>, Result<OrgTree, String>)>;

/// Which column owns the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Col {
    Tree,
    Chats,
    Thread,
}

impl Col {
    /// Tab's column step.
    fn next(self) -> Self {
        match self {
            Col::Tree => Col::Chats,
            Col::Chats => Col::Thread,
            Col::Thread => Col::Tree,
        }
    }
}

pub(crate) struct MessagesBoard {
    pub(crate) snapshot: MessagesSnapshot,
    pub(crate) col: Col,
    pub(crate) cursors: [usize; 3],
    pub(crate) sel_agent: Option<String>,
    pub(crate) sel_thread: Option<String>,
    pending_message_id: Option<String>,
    selected_message_id: Option<String>,
    pub(crate) detail: Option<super::messages_detail::SessionDetail>,
    pub(super) reply: Option<super::messages_reply::ReplyState>,
    pub(crate) gen: u64,
    pub(crate) inflight: bool,
    last_read: Option<Instant>,
    esc: Vec<u8>,
}

impl MessagesBoard {
    pub(crate) fn new(gen: u64) -> Self {
        Self {
            snapshot: Default::default(),
            col: Col::Tree,
            cursors: [0; 3],
            sel_agent: None,
            sel_thread: None,
            pending_message_id: None,
            selected_message_id: None,
            detail: None,
            reply: None,
            gen,
            inflight: false,
            last_read: None,
            esc: Vec::new(),
        }
    }

    /// The active column's cursor, as an index into that column's rows.
    fn cur(&self) -> usize {
        self.cursors[self.col as usize]
    }
    fn set_cur(&mut self, v: usize) {
        self.cursors[self.col as usize] = v;
    }
    fn step(&mut self, down: bool, len: usize) {
        if len == 0 {
            self.set_cur(0);
            return;
        }
        self.set_cur(if down {
            (self.cur() + 1).min(len - 1)
        } else {
            self.cur().saturating_sub(1)
        });
    }
}

/// Column 1's rows, derived fresh per paint from the projection: a flat
/// chat list, no tree.
#[derive(Debug)]
pub(crate) enum TreeRow {
    /// A broadcast channel, by its scope.
    Channel(String),
    /// One mail participant, live or ended, by name (never a raw session
    /// id: the projection resolves the name from the id).
    Agent {
        key: String,
        name: String,
        live: bool,
    },
}

/// Column 2's rows for the selected agent: chats plus the ONE System row.
#[derive(Debug)]
pub(crate) enum ChatRow {
    /// The session's aggregated system mail, inbound only.
    System { n: usize },
    /// A chat conversation: who, the last summary, unread.
    Thread {
        chat_id: String,
        partner: String,
        partner_key: String,
        last: String,
        unread: bool,
        ts: String,
    },
}

fn text_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

impl MessagesBoard {
    fn projection(&self) -> Option<&Value> {
        self.snapshot.projection.as_ref()
    }

    /// Column 1's rows: channels, then every non-system participant as a
    /// flat chat list - live first, then by name, one row per key. Ended
    /// agents stay in the list; there is no Archive and no fold.
    pub(crate) fn tree_rows(&self) -> Vec<TreeRow> {
        let mut rows: Vec<TreeRow> = Vec::new();
        let proj = self.projection();
        if let Some(chans) = proj
            .and_then(|p| p.get("channels"))
            .and_then(Value::as_array)
        {
            let mut scopes: Vec<&str> = chans
                .iter()
                .filter_map(|c| c.get("scope").and_then(Value::as_str))
                .collect();
            scopes.sort_unstable();
            scopes.dedup();
            for scope in scopes {
                rows.push(TreeRow::Channel(scope.to_string()));
            }
        }
        let mut agents: Vec<&Value> = proj
            .and_then(|p| p.get("participants"))
            .and_then(Value::as_array)
            .map(|ps| {
                ps.iter()
                    .filter(|p| !bool_of(p, "system"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        agents.sort_by(|a, b| {
            bool_of(b, "live")
                .cmp(&bool_of(a, "live"))
                .then(text_of(a, "name").cmp(text_of(b, "name")))
                .then(text_of(a, "key").cmp(text_of(b, "key")))
        });
        let mut seen: HashSet<&str> = HashSet::new();
        for p in agents {
            if !seen.insert(text_of(p, "key")) {
                continue;
            }
            rows.push(TreeRow::Agent {
                key: text_of(p, "key").to_string(),
                name: text_of(p, "name").to_string(),
                live: bool_of(p, "live"),
            });
        }
        rows
    }

    /// Column 2's rows for `agent`: the ONE System row first (AC14-HP,
    /// inbound only), then one row per chat.
    pub(crate) fn chat_rows(&self, agent: &str) -> Vec<ChatRow> {
        let mut rows: Vec<ChatRow> = Vec::new();
        let proj = self.projection();
        let marks = crate::view_store::load_messages_read_marks();
        if let Some(sys) = proj
            .and_then(|p| p.get("system"))
            .and_then(Value::as_object)
            .and_then(|m| m.get(agent))
            .and_then(Value::as_array)
        {
            if !sys.is_empty() {
                rows.push(ChatRow::System { n: sys.len() });
            }
        }
        if let Some(threads) = proj
            .and_then(|p| p.get("threads"))
            .and_then(Value::as_array)
        {
            for t in threads {
                let parts = t.get("participants").and_then(Value::as_array);
                let has_me =
                    parts.is_some_and(|ps| ps.iter().filter_map(Value::as_str).any(|s| s == agent));
                if !has_me {
                    continue;
                }
                let other = parts
                    .map(|ps| {
                        ps.iter()
                            .filter_map(Value::as_str)
                            .find(|s| *s != agent)
                            .unwrap_or(agent)
                    })
                    .unwrap_or(agent);
                let other_system = proj
                    .and_then(|p| p.get("participants"))
                    .and_then(Value::as_array)
                    .and_then(|ps| ps.iter().find(|p| text_of(p, "key") == other))
                    .is_some_and(|p| bool_of(p, "system"));
                if other_system {
                    continue;
                }
                let other_name = self.participant_name(other);
                let chat_id = text_of(t, "chat_id");
                let rows_of = t.get("rows").and_then(Value::as_array);
                let last_row = rows_of.and_then(|rs| rs.last());
                let last = last_row.map(|r| text_of(r, "summary")).unwrap_or("");
                let ts = last_row.map(|r| text_of(r, "ts")).unwrap_or("");
                let unread =
                    !chat_id.is_empty() && marks.get(chat_id).map(String::as_str) < Some(ts);
                rows.push(ChatRow::Thread {
                    chat_id: chat_id.to_string(),
                    partner: other_name,
                    partner_key: other.to_string(),
                    last: last.to_string(),
                    unread,
                    ts: ts.to_string(),
                });
            }
        }
        rows
    }

    /// The display name for a participant key, falling back to the raw key.
    pub(super) fn participant_name(&self, key: &str) -> String {
        let proj = self.projection();
        proj.and_then(|p| p.get("participants"))
            .and_then(Value::as_array)
            .and_then(|ps| ps.iter().find(|p| text_of(p, "key") == key))
            .map(|p| text_of(p, "name").to_string())
            .unwrap_or_else(|| key.to_string())
    }

    /// The thread content the third column shows: rows behind the current
    /// selection - a stored pair thread, a `# channel`, or the System
    /// exchange.
    pub(super) fn conversation_rows(&self) -> Vec<&Value> {
        let Some(sel) = self.sel_thread.as_deref() else {
            return Vec::new();
        };
        let Some(proj) = self.projection() else {
            return Vec::new();
        };
        if let Some(scope) = sel.strip_prefix("channel:") {
            return proj
                .get("channels")
                .and_then(Value::as_array)
                .and_then(|cs| cs.iter().find(|c| text_of(c, "scope") == scope))
                .and_then(|c| c.get("rows"))
                .and_then(Value::as_array)
                .map(|rs| rs.iter().collect())
                .unwrap_or_default();
        }
        if let Some(agent) = sel.strip_prefix("system:") {
            return proj
                .get("system")
                .and_then(Value::as_object)
                .and_then(|m| m.get(agent))
                .and_then(Value::as_array)
                .map(|rs| rs.iter().collect())
                .unwrap_or_default();
        }
        proj.get("threads")
            .and_then(Value::as_array)
            .and_then(|ts| ts.iter().find(|t| text_of(t, "chat_id") == sel))
            .and_then(|t| t.get("rows"))
            .and_then(Value::as_array)
            .map(|rs| rs.iter().collect())
            .unwrap_or_default()
    }

    /// The three columns' lines.
    pub(crate) fn columns(&self, w: usize) -> (Vec<BLine>, Vec<BLine>, Vec<BLine>) {
        let tree = self.tree_column(w);
        let chats = self.chats_column();
        let (tree_w, part_w) = split(w);
        let content = self.thread_lines(w.saturating_sub(tree_w + part_w)).0;
        (tree, chats, content)
    }

    /// Column 1: the flat chat list. Band rides the cursor row.
    fn tree_column(&self, _w: usize) -> Vec<BLine> {
        let rows = self.tree_rows();
        let mut lines = vec![
            BLine::meta("Agents"),
            BLine::meta(self.snapshot.error_line(board_now())),
        ];
        if self.snapshot.projection.is_none() {
            lines.push(BLine::meta("(not read yet - opening gathers once)"));
        }
        for (i, row) in rows.iter().enumerate() {
            let mut line = match row {
                TreeRow::Channel(scope) => BLine::of(&[seg(channel_label(scope), BRole::Label)]),
                TreeRow::Agent { name, live, .. } => BLine::of(&[
                    seg("  ", BRole::Meta),
                    seg(name.clone(), if *live { BRole::Body } else { BRole::Meta }),
                ]),
            };
            line.band = i == self.cursors[0];
            lines.push(line);
        }
        lines
    }

    /// Column 2: the System row first, then one row per chat.
    fn chats_column(&self) -> Vec<BLine> {
        let mut lines = vec![BLine::meta("Chats")];
        let Some(agent) = self.sel_agent.as_deref() else {
            lines.push(BLine::meta("(select an agent in column 1)"));
            return lines;
        };
        for (i, row) in self.chat_rows(agent).iter().enumerate() {
            let mut line = match row {
                ChatRow::System { n } => BLine::of(&[
                    seg("System", BRole::Body),
                    seg(format!(" · fno/<arm> x{n}"), BRole::Meta),
                ]),
                ChatRow::Thread {
                    partner,
                    last,
                    unread,
                    ..
                } => BLine::of(&[
                    seg(if *unread { "*" } else { " " }, BRole::Meta),
                    seg(partner.clone(), BRole::Body),
                    seg(format!(" \u{b7} {last}"), BRole::Meta),
                ]),
            };
            line.band = i == self.cursors[1];
            lines.push(line);
        }
        lines
    }
}
impl MessagesBoard {
    /// The local HH:MM a stored ts shows under.
    fn ts_time(ts: &str) -> String {
        chrono::DateTime::parse_from_rfc3339(ts)
            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default()
    }

    /// Minutes from one stored ts to the next; `None` when either is
    /// unreadable.
    fn ts_gap_minutes(from: &str, to: &str) -> Option<i64> {
        Some(
            (chrono::DateTime::parse_from_rfc3339(to).ok()?
                - chrono::DateTime::parse_from_rfc3339(from).ok()?)
            .num_minutes(),
        )
    }

    /// Column 3's lines, with the conversation row each line belongs to
    /// (`None` for the title and the time and sender separators) so the
    /// click map mirrors the paint. A bubble thread: the other side on the
    /// left, the selected agent on the right, one centered HH:MM before a
    /// gap of five minutes or more, and the body only - no name headers,
    /// no envelopes, no delivered-header lines.
    pub(crate) fn thread_lines(&self, w: usize) -> (Vec<BLine>, Vec<Option<usize>>) {
        let mut lines = vec![BLine::meta("Thread")];
        let mut owners: Vec<Option<usize>> = vec![None];
        let Some(_sel) = self.sel_thread.as_deref() else {
            lines.push(BLine::meta("(select a chat in column 2)"));
            owners.push(None);
            return (lines, owners);
        };
        let rows = self.conversation_rows();
        if rows.is_empty() {
            lines.push(BLine::meta("(no messages)"));
            owners.push(None);
            return (lines, owners);
        }
        let mine = self.sel_agent.as_deref().unwrap_or("");
        // Channel and System views hold more than two voices, so a sender
        // change gets a name line; a pair thread speaks left and right.
        let named = self
            .sel_thread
            .as_deref()
            .is_some_and(|s| s.starts_with("channel:") || s.starts_with("system:"));
        let wrap_w = w.saturating_sub(2).min(((w * 7) / 10).max(12)).max(1);
        let mut last_ts = String::new();
        let mut last_sender = String::new();
        for (index, r) in rows.iter().enumerate() {
            let selected = self.selected_message_id.as_deref() == Some(text_of(r, "id"))
                || index == self.cursors[2];
            let ts = text_of(r, "ts");
            if last_ts.is_empty() || Self::ts_gap_minutes(&last_ts, ts).is_none_or(|g| g >= 5) {
                let time = Self::ts_time(ts);
                if !time.is_empty() {
                    let pad = w.saturating_sub(time.len()) / 2;
                    lines.push(BLine::meta(format!("{}{time}", " ".repeat(pad))));
                    owners.push(None);
                }
            }
            last_ts = ts.to_string();
            let sender = if bool_of(r, "system") {
                text_of(r, "from").to_string()
            } else {
                self.participant_name(text_of(r, "from_key"))
            };
            if named && sender != last_sender {
                lines.push(BLine::of(&[
                    seg("  ", BRole::Meta),
                    seg(sender.clone(), BRole::Meta),
                ]));
                owners.push(None);
                last_sender = sender;
            }
            let right = text_of(r, "from_key") == mine;
            let wrapped = BLine::plain(text_of(r, "body")).wrap(wrap_w);
            for mut line in wrapped {
                if right {
                    let pad = w
                        .saturating_sub(1)
                        .saturating_sub(line.text.chars().count());
                    line.text = format!("{}{}", " ".repeat(pad), line.text);
                }
                line.band = selected;
                lines.push(line);
                owners.push(Some(index));
            }
        }
        (lines, owners)
    }
}

/// The full-surface paint: three columns from row 1 (the strip owns
/// row 0), footer hints on the last row.
pub(crate) fn paint(
    view: &View,
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    width: usize,
    height: usize,
) {
    let Some(b) = &view.messages_board else {
        return;
    };
    let width = width.min(cols);
    let height = height.min(rows);
    if width == 0 || height < 2 {
        return;
    }
    for r in 1..height {
        for c in 0..width {
            cells[r * cols + c] = Cell::default();
        }
    }
    let (tree, partners, thread) = b.columns(width);
    let tree_w = (width / 4).max(14).min(width / 2);
    let part_w = (width / 4).max(14).min(width.saturating_sub(tree_w) / 2);
    let thread_w = width.saturating_sub(tree_w + part_w);
    let body_h = height.saturating_sub(2);
    // The cursor's painted line follows the window, so a long tree keeps
    // the selection visible and the click map's window matches the paint.
    super::backlog_style::paint_panel_at(
        cells,
        rows,
        cols,
        0,
        1,
        tree_w,
        body_h,
        &tree,
        Some(2 + b.cursors[0]),
        &view.theme,
    );
    super::backlog_style::paint_panel_at(
        cells,
        rows,
        cols,
        tree_w,
        1,
        part_w,
        body_h,
        &partners,
        Some(1 + b.cursors[1]),
        &view.theme,
    );
    super::backlog_style::paint_panel_at(
        cells,
        rows,
        cols,
        tree_w + part_w,
        1,
        thread_w,
        body_h,
        &thread,
        Some(
            thread
                .iter()
                .rposition(|line| line.band)
                .unwrap_or_else(|| thread.len().saturating_sub(1)),
        ),
        &view.theme,
    );
}

/// Lifecycle.
pub(crate) fn open(view: &mut View) {
    let gen = NEXT_MESSAGES_GEN.fetch_add(1, Ordering::Relaxed);
    view.messages_board = Some(MessagesBoard::new(gen));
    view.backlog_board = None;
    view.org_board = None;
    view.region_owner = super::region_focus::RegionOwner::Board;
    super::backlog_board::set_sideline_view(view, SidelineView::Messages);
}

/// Open the Messages tab with a pending message id. The projection gather
/// resolves it to a chat before the thread is selected.
pub(crate) fn open_message(view: &mut View, id: String) {
    open(view);
    if let Some(board) = view.messages_board.as_mut() {
        board.pending_message_id = Some(id);
    }
}

/// Restore after launch: a persisted Messages sideline reopens it.
pub(crate) fn restore(view: &mut View) {
    if view.sideline_view == SidelineView::Messages {
        open(view);
    }
}

/// A landed gather: apply under the gen guard. The org tree result rides
/// the same channel and is not used by the flat chat list.
pub(crate) fn apply_gather(
    view: &mut View,
    gen: u64,
    mail: Result<Value, String>,
    _tree: Result<OrgTree, String>,
) {
    let Some(b) = view.messages_board.as_mut().filter(|b| b.gen == gen) else {
        return;
    };
    b.inflight = false;
    let mut notice = None;
    match mail {
        Ok(v) => {
            b.snapshot.apply(v);
            if let Some(id) = b.pending_message_id.take() {
                let target = b.projection().and_then(|projection| {
                    let pair = projection
                        .get("threads")?
                        .as_array()?
                        .iter()
                        .find_map(|thread| {
                            let rows = thread.get("rows")?.as_array()?;
                            let row = rows.iter().find(|row| text_of(row, "id") == id)?;
                            Some((
                                text_of(thread, "chat_id").to_string(),
                                Some(text_of(row, "to_key").to_string()),
                            ))
                        });
                    if pair.is_some() {
                        return pair;
                    }
                    let channel =
                        projection
                            .get("channels")?
                            .as_array()?
                            .iter()
                            .find_map(|channel| {
                                let rows = channel.get("rows")?.as_array()?;
                                rows.iter().any(|row| text_of(row, "id") == id).then(|| {
                                    (format!("channel:{}", text_of(channel, "scope")), None)
                                })
                            });
                    if channel.is_some() {
                        return channel;
                    }
                    projection
                        .get("system")?
                        .as_object()?
                        .iter()
                        .find_map(|(agent, rows)| {
                            rows.as_array()?
                                .iter()
                                .any(|row| text_of(row, "id") == id)
                                .then(|| (format!("system:{agent}"), Some(agent.clone())))
                        })
                });
                if let Some((thread, agent)) = target.filter(|(thread, _)| !thread.is_empty()) {
                    b.sel_thread = Some(thread.clone());
                    if let Some(agent) = agent {
                        b.sel_agent = Some(agent.clone());
                        if let Some(i) = b.chat_rows(&agent).iter().position(
                            |row| matches!(row, ChatRow::Thread { chat_id, .. } if chat_id == &thread),
                        ) {
                            b.cursors[1] = i;
                        }
                    }
                    b.cursors[2] = b
                        .conversation_rows()
                        .iter()
                        .position(|row| text_of(row, "id") == id)
                        .unwrap_or(0);
                    b.selected_message_id = Some(id);
                    b.col = Col::Thread;
                } else {
                    notice = Some(format!("message {id}: no conversation found"));
                }
            }
        }
        Err(reason) => b.snapshot.fail(reason, board_now()),
    }
    if let Some(notice) = notice {
        view.set_notice(notice);
    }
}

/// Kick a gather at most every 60s while the board is open.
pub(crate) fn maybe_kick(view: &mut View, tx: &MessagesTx) {
    let Some(b) = view.messages_board.as_mut() else {
        return;
    };
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
        let graph = crate::backlog_view::graph_path();
        let inputs = crate::org_model::gather(&graph, agents).await;
        let tree = crate::org_model::derive(&inputs, board_now());
        let mail = crate::messages_model::gather().await;
        let _ = tx.send((gen, mail, tree));
    });
}

/// The column widths the paint and the click map share.
fn split(width: usize) -> (usize, usize) {
    let tree_w = (width / 4).max(14).min(width / 2);
    let part_w = (width / 4).max(14).min(width.saturating_sub(tree_w) / 2);
    (tree_w, part_w)
}

/// The screen rows a column's lines painted at, given its scroll start.
fn column_rect(lines: usize, body_h: usize, follow_line: usize) -> (usize, usize) {
    let len = lines.max(1);
    let start = if len <= body_h {
        0
    } else {
        follow_line
            .saturating_sub(body_h - 1)
            .min(len.saturating_sub(body_h))
    };
    (start, body_h.min(len))
}

/// Keys while the board owns the keyboard: the org board's shape, with
/// Tab as the column step.
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
    if super::messages_reply::active(view) {
        return super::messages_reply::keys(view, bytes, sock).await;
    }
    let Some(b) = view.messages_board.as_mut() else {
        return Ok(StdinFlow::Continue);
    };
    if let Some(detail) = b.detail.as_mut() {
        if super::messages_detail::detail_keys(detail, bytes) {
            b.detail = None;
        }
        return Ok(StdinFlow::Continue);
    }
    let tokens = if bytes == [27] && b.esc.is_empty() {
        vec![ModalKey::Esc]
    } else {
        fold_modal_keys(&mut b.esc, bytes)
    };
    for token in tokens {
        let Some(b) = view.messages_board.as_mut() else {
            break;
        };
        match token {
            ModalKey::Esc | ModalKey::Byte(b'q') => {
                view.messages_board = None;
                super::backlog_board::set_sideline_view(view, SidelineView::Agents);
            }
            ModalKey::Byte(b'V') => {
                super::backlog_board::cycle_sideline_view(view);
            }
            ModalKey::Byte(b'\t') => {
                b.col = b.col.next();
            }
            ModalKey::Up | ModalKey::Byte(b'k') => {
                let len = column_len(b);
                b.step(false, len);
            }
            ModalKey::Down | ModalKey::Byte(b'j') => {
                let len = column_len(b);
                b.step(true, len);
            }
            ModalKey::Enter => act(view, sock).await?,
            ModalKey::Byte(b'd') => open_detail(view),
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// The active column's row count.
fn column_len(b: &MessagesBoard) -> usize {
    match b.col {
        Col::Tree => b.tree_rows().len(),
        Col::Chats => b
            .sel_agent
            .as_deref()
            .map(|a| b.chat_rows(a).len())
            .unwrap_or(0),
        Col::Thread => b.conversation_rows().len(),
    }
}

/// Enter on the active column's row.
async fn act(
    view: &mut View,
    _sock: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let Some(b) = view.messages_board.as_mut() else {
        return Ok(());
    };
    match b.col {
        Col::Tree => {
            let rows = b.tree_rows();
            let Some(row) = rows.get(b.cursors[0]) else {
                return Ok(());
            };
            match row {
                TreeRow::Channel(scope) => {
                    b.sel_thread = Some(format!("channel:{scope}"));
                    b.cursors[2] = b.conversation_rows().len().saturating_sub(1);
                    b.col = Col::Thread;
                }
                TreeRow::Agent { key, .. } => {
                    b.sel_agent = Some(key.clone());
                    b.cursors[1] = 0;
                    b.sel_thread = None;
                    b.col = Col::Chats;
                }
            }
        }
        Col::Chats => {
            let Some(agent) = b.sel_agent.clone() else {
                return Ok(());
            };
            let rows = b.chat_rows(&agent);
            let Some(row) = rows.get(b.cursors[1]) else {
                return Ok(());
            };
            match row {
                ChatRow::System { .. } => {
                    b.sel_thread = Some(format!("system:{agent}"));
                    b.cursors[2] = b.conversation_rows().len().saturating_sub(1);
                    b.col = Col::Thread;
                }
                ChatRow::Thread { chat_id, ts, .. } => {
                    crate::view_store::save_messages_read_mark(chat_id, ts);
                    b.sel_thread = Some(chat_id.clone());
                    b.cursors[2] = b.conversation_rows().len().saturating_sub(1);
                    b.col = Col::Thread;
                }
            }
        }
        Col::Thread => {
            let row = b
                .conversation_rows()
                .get(b.cursors[2])
                .map(|row| (**row).clone());
            if let Some(row) = row {
                super::messages_reply::open(view, row);
            }
        }
    }
    Ok(())
}

/// d on a chat-list or chat row: the session details modal.
fn open_detail(view: &mut View) {
    let key = {
        let Some(b) = view.messages_board.as_ref() else {
            return;
        };
        match b.col {
            Col::Tree => b.tree_rows().get(b.cursors[0]).and_then(tree_row_agent_key),
            Col::Chats => b.sel_agent.clone().and_then(|a| {
                b.chat_rows(&a)
                    .into_iter()
                    .nth(b.cursors[1])
                    .and_then(|row| match row {
                        ChatRow::Thread { partner_key, .. } => Some(partner_key),
                        ChatRow::System { .. } => None,
                    })
            }),
            Col::Thread => None,
        }
    };
    if let Some(key) = key {
        super::messages_detail::open(view, key);
    }
}

/// The agent key a chat-list row resolves to, for the details modal.
fn tree_row_agent_key(row: &TreeRow) -> Option<String> {
    match row {
        TreeRow::Agent { key, .. } => Some(key.clone()),
        TreeRow::Channel(_) => None,
    }
}

/// Mouse: a tap selects; a tap on the row already selected acts like
/// Enter. The click map mirrors the paint's geometry (strip at row 0,
/// each column's header line at row 1).
pub(crate) async fn mouse(
    view: &mut View,
    rep: crate::mouse::MouseReport,
    sock: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    if !matches!(
        rep.kind,
        crate::proto::MouseKind::Press(crate::proto::MouseButton::Left)
    ) {
        return Ok(());
    }
    view.region_owner = super::region_focus::RegionOwner::Board;
    if rep.row == 0 {
        // The strip row: only its words act (R15).
        for (start, w, view_switch) in view.top_row_spans() {
            if (rep.col as usize) >= start && (rep.col as usize) < start + w {
                apply_hit(view, ChromeHit::TopRow(view_switch), sock).await?;
            }
        }
        return Ok(());
    }
    if view
        .messages_board
        .as_ref()
        .is_some_and(|b| b.detail.is_some() || b.reply.is_some())
    {
        // Modals own the pointer; clicking outside dismisses a detail card.
        if let Some(d) = view.messages_board.as_ref().and_then(|b| b.detail.as_ref()) {
            if !d.popup.render(view.term).contains(rep.row, rep.col) {
                if let Some(b) = view.messages_board.as_mut() {
                    b.detail = None;
                }
            }
        }
        return Ok(());
    }
    let (tree_w, part_w) = split(view.term.1 as usize);
    let col = if (rep.col as usize) < tree_w {
        Col::Tree
    } else if (rep.col as usize) < tree_w + part_w {
        Col::Chats
    } else {
        Col::Thread
    };
    if let Some(b) = view.messages_board.as_mut() {
        b.col = col;
    }
    let body_h = (view.term.0 as usize).saturating_sub(3);
    let index = match col {
        Col::Tree => {
            let len = view
                .messages_board
                .as_ref()
                .map(|b| b.tree_rows().len())
                .unwrap_or(0);
            let (start, _) = column_rect(
                len + 2,
                body_h,
                2 + view
                    .messages_board
                    .as_ref()
                    .map(|b| b.cursors[0])
                    .unwrap_or(0),
            );
            // The tree column paints its two header lines at rows 1 to 2,
            // so row r holds line r - 1 - 2 (strip at 0).
            (rep.row as usize).checked_sub(3).map(|i| i + start)
        }
        Col::Chats => {
            let agent = view
                .messages_board
                .as_ref()
                .and_then(|b| b.sel_agent.clone());
            let len = agent
                .map(|a| {
                    view.messages_board
                        .as_ref()
                        .map(|b| b.chat_rows(&a).len())
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            let (start, _) = column_rect(
                len + 1,
                body_h,
                1 + view
                    .messages_board
                    .as_ref()
                    .map(|b| b.cursors[1])
                    .unwrap_or(0),
            );
            // One header line at row 1.
            (rep.row as usize).checked_sub(2).map(|i| i + start)
        }
        Col::Thread => {
            let (tree_w, part_w) = split(view.term.1 as usize);
            let thread_w = (view.term.1 as usize).saturating_sub(tree_w + part_w);
            let Some(board) = view.messages_board.as_ref() else {
                return Ok(());
            };
            let (lines, owners) = board.thread_lines(thread_w);
            let follow = lines
                .iter()
                .rposition(|line| line.band)
                .unwrap_or_else(|| lines.len().saturating_sub(1));
            let painted_body_h = (view.term.0 as usize).saturating_sub(2);
            let (start, _) = column_rect(lines.len(), painted_body_h, follow);
            (rep.row as usize)
                .checked_sub(1)
                .map(|i| i + start)
                .and_then(|line| owners.get(line).copied().flatten())
        }
    };
    let Some(b) = view.messages_board.as_mut() else {
        return Ok(());
    };
    let in_range = match col {
        Col::Tree => index.is_some_and(|i| i < b.tree_rows().len()),
        Col::Chats => index.is_some_and(|i| {
            b.sel_agent
                .as_deref()
                .map(|a| i < b.chat_rows(a).len())
                .unwrap_or(false)
        }),
        Col::Thread => index.is_some_and(|i| i < b.conversation_rows().len()),
    };
    if !in_range {
        return Ok(());
    }
    let i = index.unwrap_or(0);
    let slot = match col {
        Col::Tree => 0,
        Col::Chats => 1,
        Col::Thread => 2,
    };
    if col == Col::Thread {
        let row = b.conversation_rows().get(i).map(|row| (**row).clone());
        b.cursors[2] = i;
        if let Some(row) = row {
            super::messages_reply::open(view, row);
        }
        return Ok(());
    }
    if b.cursors[slot] != i {
        b.cursors[slot] = i;
        return Ok(());
    }
    act(view, sock).await
}

#[cfg(test)]
#[path = "tests/messages_fixture.rs"]
mod fixtures;

fn board_now() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}
