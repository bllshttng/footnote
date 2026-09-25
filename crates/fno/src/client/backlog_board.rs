//! The experimental backlog board: the operator's kanban over the one
//! read model ([`crate::backlog_model`]). Lanes accordion, cards follow the
//! model's order, the drill-down reads the same gathered `Inputs` (no
//! process), and every write calls an existing verb with its own last line
//! as the notice. Off until the operator flips the sidebar menu's
//! experimental toggle.
//!
//! The fold copies [`super::feed_view`]'s single-flight/generation
//! discipline: a cheap store-version probe answers `Unchanged` when nothing
//! moved; a moved (or failed) probe runs the full `backlog_model::gather`.

use super::*;
use crate::backlog_model;
use crate::backlog_model::{unavailable_features, Board, Lane};
use crate::backlog_view::graph_path;
use crate::store_client;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

/// Right-pad one cell segment so side-by-side columns align.
fn pad(seg: &str, w: usize) -> String {
    let n = seg.chars().count();
    if n >= w {
        trunc(seg, w)
    } else {
        format!("{seg}{}", " ".repeat(w - n))
    }
}

/// How often the kick re-probes the store version while the board is open.
const PROBE_EVERY: Duration = Duration::from_secs(2);
/// The stacked layout below this width, side-by-side cells at or above it.
const WIDE_CELLS_AT: usize = 90;

/// The kick result channel: one message per kicked task. The channel
/// tuple's generation guards the fold; the variants carry only payloads.
pub(crate) enum BoardMsg {
    /// The version probe read the same version inside the regather window.
    Unchanged,
    /// A full gather landed; the view derives from it.
    Gathered { inputs: backlog_model::Inputs },
    /// A queued write verb finished; its last line is the notice.
    VerbDone { notice: String },
}

pub(crate) type BoardTx = tokio::sync::mpsc::UnboundedSender<(u64, BoardMsg)>;

/// The board's active filters, held as route pairs so the model's own
/// [`backlog_model::Query::from_pairs`] parses them - one parser, no second
/// filter semantics.
#[derive(Debug, Default, Clone)]
pub(crate) struct QueryState {
    pub(crate) lanes: backlog_model::LanesBy,
    pub(crate) project: Option<String>,
    pub(crate) epic: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) priority: Option<String>,
    pub(crate) size: Option<String>,
    pub(crate) king: Option<String>,
    pub(crate) q: Option<String>,
}

impl QueryState {
    /// The model's parsed query for this state.
    pub(crate) fn to_query(&self) -> Result<backlog_model::Query, String> {
        let mut p: HashMap<String, String> = HashMap::new();
        let lanes = match self.lanes {
            backlog_model::LanesBy::Project => "project",
            backlog_model::LanesBy::Epic => "epic",
            backlog_model::LanesBy::None => "none",
        };
        p.insert("lanes".into(), lanes.into());
        for (k, v) in [
            ("project", &self.project),
            ("epic", &self.epic),
            ("status", &self.status),
            ("priority", &self.priority),
            ("size", &self.size),
            ("king", &self.king),
            ("q", &self.q),
        ] {
            if let Some(v) = v {
                p.insert(k.to_string(), v.clone());
            }
        }
        backlog_model::Query::from_pairs(&p)
    }

    /// `priority=p1 · find: "mux"` style summary for the query line.
    pub(crate) fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let lanes = match self.lanes {
            backlog_model::LanesBy::Project => "project",
            backlog_model::LanesBy::Epic => "epic",
            backlog_model::LanesBy::None => "none",
        };
        parts.push(format!("lanes: {lanes}"));
        for (k, v) in [
            ("project", &self.project),
            ("epic", &self.epic),
            ("status", &self.status),
            ("priority", &self.priority),
            ("size", &self.size),
            ("king", &self.king),
        ] {
            if let Some(v) = v {
                parts.push(format!("{k}={v}"));
            }
        }
        if let Some(q) = &self.q {
            parts.push(format!("find: \"{q}\""));
        }
        parts.join(" · ")
    }
}

/// The board overlay's state. One at a time; `None` on the View means the
/// board is closed. Cursor `(lane, col, row)` indexes lanes, the six
/// columns, and a cell's painted cards; clamped against the current board
/// before every use.
pub(crate) struct BoardView {
    /// The gathered inputs the board and the drill-down both read.
    pub(crate) inputs: Option<backlog_model::Inputs>,
    /// The last good derived board. `None` before the first gather.
    pub(crate) body: Option<backlog_model::Board>,
    /// Lines from the last failed gather; the last good board stays
    /// painted over them.
    pub(crate) errors: Vec<String>,
    pub(crate) query: QueryState,
    /// Cursor into the rendered lanes.
    pub(crate) lane: usize,
    pub(crate) col: usize,
    pub(crate) row: usize,
    pub(crate) want: bool,
    pub(crate) inflight: bool,
    pub(crate) gen: u64,
    /// The store version the last gather read; the probe compares.
    stamp: Option<i64>,
    last_gather: Option<Instant>,
    last_probe: Option<Instant>,
    force: bool,
    /// A one-line input, when open: the find query, the title editor, or
    /// the details append. The typed buffer rides along.
    pub(crate) input: Option<(BoardInputKind, String)>,
    input_esc: Vec<u8>,
    /// A p/s/S field picker, when open.
    pub(crate) pick: Option<PickState>,
    /// `f` facet picker: cursor per level (facet, then value).
    pub(crate) facet: Option<FacetPick>,
    /// Pending escape bytes in board-key mode (split-arrow safety).
    board_esc: Vec<u8>,
    /// The drill-down overlay, when open (wave 4).
    pub(crate) detail: Option<node_detail::NodeDetailOverlay>,
    pub(crate) detail_esc: Vec<u8>,
    /// The write verb queued for the run loop (one at a time).
    pub(crate) write_action: Option<WriteAction>,
}

/// Which one-line input the board's input line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoardInputKind {
    /// The `/` find filter.
    Find,
    /// The `e` title editor (pre-filled with the current title).
    Title,
    /// The `D` details-append text.
    Append,
}

/// The board's queued write verb.
#[derive(Debug, Clone)]
pub(crate) enum WriteAction {
    /// Run these argv (plus optional stdin) through the bounded shell-out.
    Args(Vec<String>, Option<String>),
    /// The `D` append: read the CURRENT details off the store first so a
    /// concurrent edit is never overwritten by the gathered copy.
    Append { id: String, text: String },
}

/// The p/s/S field picker's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickKind {
    Priority,
    Size,
    Status,
}

impl PickKind {
    /// The picker's rows (label, write value); [`crate::backlog_write`]'s
    /// lists, so both boards offer the same values the verb accepts.
    pub(crate) fn values(self) -> &'static [&'static str] {
        match self {
            PickKind::Priority => crate::backlog_write::PRIORITIES,
            PickKind::Size => crate::backlog_write::SIZES,
            PickKind::Status => crate::backlog_write::STATUSES,
        }
    }
}

/// An open p/s/S picker: kind + cursor.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PickState {
    pub(crate) kind: PickKind,
    pub(crate) sel: usize,
}

/// The `f` facet picker's open state: the facet list first, then that
/// facet's value list.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FacetPick {
    /// Index into the fixed facet list (project, epic, status, priority,
    /// size, king).
    pub(crate) facet: usize,
    /// The facet-list cursor.
    pub(crate) sel: usize,
    /// `Some` once a facet is picked: the value-list cursor.
    pub(crate) value_sel: Option<usize>,
}

impl BoardView {
    /// A fresh, closed-board state: want armed so the first kick gathers.
    pub(crate) fn new(gen: u64) -> Self {
        BoardView {
            inputs: None,
            body: None,
            errors: Vec::new(),
            query: QueryState::default(),
            lane: 0,
            col: 0,
            row: 0,
            want: true,
            inflight: false,
            gen,
            stamp: None,
            last_gather: None,
            last_probe: None,
            force: false,
            input: None,
            input_esc: Vec::new(),
            pick: None,
            facet: None,
            board_esc: Vec::new(),
            detail: None,
            detail_esc: Vec::new(),
            write_action: None,
        }
    }
}

/// At most ONE fold in flight, armed by `want` or by the 2 s probe clock
/// (the feed fold's discipline). Runs each run-loop turn beside
/// `feed_view::maybe_kick`.
pub(crate) fn maybe_kick(view: &mut View, tx: &BoardTx) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    if b.inflight {
        return;
    }
    let due = b.want || b.last_probe.is_none_or(|t| t.elapsed() >= PROBE_EVERY);
    if !due {
        return;
    }
    b.want = false;
    b.inflight = true;
    b.last_probe = Some(Instant::now());
    let tx = tx.clone();
    let gen = b.gen;
    let force = b.force;
    let stamp = b.stamp;
    let fresh = b
        .last_gather
        .is_some_and(|t| t.elapsed() < backlog_model::REGATHER_AFTER);
    let agents = view.layout.agents.clone();
    tokio::spawn(async move {
        let version = tokio::task::spawn_blocking(|| store_client::version(&graph_path()))
            .await
            .unwrap_or(Err("the version probe task failed".into()));
        if !force && fresh {
            if let (Ok(v), Some(s)) = (version, stamp) {
                if v == s {
                    let _ = tx.send((gen, BoardMsg::Unchanged));
                    return;
                }
            }
        }
        let inputs = backlog_model::gather(&graph_path(), agents).await;
        let _ = tx.send((gen, BoardMsg::Gathered { inputs }));
    });
}

/// A fold landed: apply only to the still-open, same-generation board
/// (the feed fold's contract, one consumer in the run loop).
pub(crate) fn apply_fold(view: &mut View, gen: u64, msg: BoardMsg) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    if gen != b.gen {
        return;
    }
    let mut notice: Option<String> = None;
    match msg {
        BoardMsg::Unchanged => b.inflight = false,
        BoardMsg::VerbDone { notice: n } => {
            notice = Some(n);
            b.force = true;
            b.want = true;
        }
        BoardMsg::Gathered { inputs } => {
            b.inflight = false;
            b.force = false;
            b.last_gather = Some(Instant::now());
            b.stamp = inputs.version;
            let focus = cursor_card_id(b);
            b.inputs = Some(inputs);
            let Ok(q) = b.query.to_query() else {
                return;
            };
            let board = backlog_model::board(b.inputs.as_ref().expect("set one line above"), &q);
            b.errors = board.errors.clone();
            // A failed read (errors with no lanes) never repaints a good
            // board empty; a filter matching nothing (empty errors) does.
            if !board.lanes.is_empty() || board.errors.is_empty() || b.body.is_none() {
                b.body = Some(board);
            }
            focus_card(b, focus.as_deref());
        }
    }
    if let Some(n) = notice {
        view.set_notice(n);
    }
}

/// The card under the cursor, resolved against the current body. The row
/// clamps to the painted (capped) cards.
pub(crate) fn card_at(b: &BoardView) -> Option<&backlog_model::Card> {
    let board = b.body.as_ref()?;
    let cell = board.lanes.get(b.lane)?.cells.get(b.col)?;
    let total = cell.cards.len();
    if total == 0 {
        return None;
    }
    let row = b.row.min(total - 1);
    cell.cards.get(row)
}

/// The card id under the cursor.
pub(crate) fn cursor_card_id(b: &BoardView) -> Option<String> {
    card_at(b).map(|c| c.id.clone())
}

/// Put the cursor on `id` when the board still shows it, else on the first
/// painted card anywhere.
fn focus_card(b: &mut BoardView, id: Option<&str>) {
    let Some(id) = id else {
        first_card(b);
        return;
    };
    let board = b.body.as_ref();
    let Some(board) = board else {
        return;
    };
    for (li, lane) in board.lanes.iter().enumerate() {
        for (ci, cell) in lane.cells.iter().enumerate() {
            for (ri, card) in cell.cards.iter().enumerate() {
                if card.id == id {
                    (b.lane, b.col, b.row) = (li, ci, ri);
                    return;
                }
            }
        }
    }
    first_card(b);
}

/// Park the cursor on the first painted card of the first lane that has
/// one, leaving it at the origin when the board is empty.
fn first_card(b: &mut BoardView) {
    let Some(board) = b.body.as_ref() else {
        return;
    };
    for (li, lane) in board.lanes.iter().enumerate() {
        for (ci, cell) in lane.cells.iter().enumerate() {
            if !cell.cards.is_empty() {
                (b.lane, b.col, b.row) = (li, ci, 0);
                return;
            }
        }
    }
}

/// Render the overlay body: the lines and the cursor row for the painter's
/// follow. `w` is the chrome's inner width; every line truncates to it.
pub(crate) fn render(b: &BoardView, w: usize) -> (Vec<String>, Option<usize>) {
    let mut lines: Vec<String> = Vec::new();
    let mut follow: Option<usize> = None;
    let Some(board) = b.body.as_ref() else {
        if b.errors.is_empty() {
            lines.push("reading board...".into());
        } else {
            for e in &b.errors {
                lines.push(format!("! {e}"));
            }
        }
        return (lines, None);
    };
    push_stats_line(b, &mut lines, board, w);
    push_query_line(b, &mut lines, board, w);
    for e in &b.errors {
        lines.push(format!("! {e}"));
    }
    // A filter matching nothing says so over a zeroed board; it never
    // falls back to the unfiltered body (AC11 shape).
    let shown: usize = board
        .lanes
        .iter()
        .map(|l| l.cells.iter().map(|c| c.total).sum::<usize>())
        .sum();
    if shown == 0 {
        lines.push(match b.query.q.as_deref() {
            Some(q) => format!("no cards match: {q}"),
            None => "no cards match".into(),
        });
    }
    push_lanes(b, &mut lines, &mut follow, board, w);
    (lines, follow)
}

/// The totals line: each column's open total, then the flow aggregate.
fn push_stats_line(b: &BoardView, lines: &mut Vec<String>, board: &Board, w: usize) {
    let _ = (b, w);
    let mut line = String::new();
    for t in &board.stats.open {
        if !line.is_empty() {
            line.push_str(" · ");
        }
        line.push_str(&format!("{} {}", t.column, t.total));
    }
    let done = done_total(board);
    line.push_str(&format!(" · Done {done}"));
    line.push_str(" │ ");
    line.push_str(&flow_line(&board.stats.flow));
    lines.push(trunc(&line, w));
}

/// The uncapped Done total, summed from the lanes' own cells.
fn done_total(board: &Board) -> usize {
    let mut total = 0;
    for lane in &board.lanes {
        if let Some(cell) = lane.cells.last() {
            total += cell.total;
        }
    }
    total
}

/// The flow aggregate as one `shipped · cycle · open PRs` fragment, or the
/// reason a window is unusable. Never invents a number.
fn flow_line(flow: &Value) -> String {
    let unavailable = || -> Option<String> {
        let available = flow.get("available")?.as_bool()?;
        (!available).then(|| {
            flow.get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unavailable")
                .to_string()
        })
    };
    if let Some(reason) = unavailable() {
        return format!("flow: {reason}");
    }
    let weeks = flow
        .get("deliveries")
        .and_then(|d| d.get("weeks"))
        .and_then(Value::as_array);
    let shipped = weeks
        .and_then(|w| w.last())
        .and_then(|e| {
            let code = e.get("code").and_then(Value::as_u64).unwrap_or(0);
            let doc = e.get("doc").and_then(Value::as_u64).unwrap_or(0);
            Some(code + doc)
        })
        .unwrap_or(0);
    let cycle = flow
        .get("cycle")
        .and_then(|c| c.get("median_days"))
        .and_then(Value::as_f64);
    let cycle = match cycle {
        Some(d) => format!("cycle {d:.1}d median"),
        None => "cycle n/a".into(),
    };
    let prs = flow
        .get("open_prs")
        .and_then(|p| p.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    format!("{shipped} shipped this week · {cycle} · {prs} open PRs")
}

/// The query line: the active lanes/filters, the backend when it is not
/// the graph, and every feature the backend cannot answer.
fn push_query_line(b: &BoardView, lines: &mut Vec<String>, board: &Board, w: usize) {
    let mut line = b.query.describe();
    if board.backend != "graph" {
        line.push_str(&format!(" · backend: {}", board.backend));
    }
    for u in &board.unavailable {
        line.push_str(&format!(" · {}: {}", u.feature, u.reason));
    }
    lines.push(trunc(&line, w));
}

/// Truncate one line to `w` chars (the painter wraps nothing).
pub(crate) fn trunc(s: &str, w: usize) -> String {
    s.chars().take(w).collect()
}

/// The lanes: an accordion - the cursor's lane expanded below its header,
/// the rest collapsed to one header line each. With one lane (the model's
/// `lanes: none`), no header, always expanded.
fn push_lanes(
    b: &BoardView,
    lines: &mut Vec<String>,
    follow: &mut Option<usize>,
    board: &Board,
    w: usize,
) {
    for (li, lane) in board.lanes.iter().enumerate() {
        let expanded = board.lanes.len() == 1 || li == b.lane;
        if board.lanes.len() > 1 {
            let arrow = if expanded { "▾" } else { "▸" };
            let mut header = format!("{arrow} {}  ", lane.title);
            for (ci, cell) in lane.cells.iter().enumerate() {
                let _ = ci;
                header.push_str(&cell_total_line(cell));
                if ci + 1 < lane.cells.len() {
                    header.push_str(" · ");
                }
            }
            lines.push(trunc(&header, w));
        }
        if expanded {
            if w >= WIDE_CELLS_AT {
                push_wide_cells(b, lines, follow, lane, li, w);
            } else {
                push_stacked_cells(b, lines, follow, lane, li, w);
            }
        }
    }
}

/// `Now 3` per cell, `Done 20/900` when the cap hides cards.
fn cell_total_line(cell: &backlog_model::Cell) -> String {
    if cell.total > cell.cards.len() {
        format!("{} {}/{}", cell.column, cell.cards.len(), cell.total)
    } else {
        format!("{} {}", cell.column, cell.total)
    }
}

/// One card row: the cursor's `▸`, a claim/block glyph, the short id, and
/// the title, truncated to the cell width.
fn card_line(b: &BoardView, li: usize, ci: usize, ri: usize, w: usize) -> String {
    let sel = li == b.lane && ci == b.col && ri == b.row;
    let Some(card) = b
        .body
        .as_ref()
        .and_then(|bd| bd.lanes.get(li))
        .and_then(|l| l.cells.get(ci))
        .and_then(|c| c.cards.get(ri))
    else {
        return String::new();
    };
    let glyph = if card.claimed {
        "●"
    } else if card.blocked {
        "⊘"
    } else {
        " "
    };
    let sel_mark = if sel { "▸" } else { " " };
    trunc(&format!("{sel_mark}{glyph} {} {}", card.id, card.title), w)
}

/// The expanded lane at `w >= WIDE_CELLS_AT`: the six cells side by side.
fn push_wide_cells(
    b: &BoardView,
    lines: &mut Vec<String>,
    follow: &mut Option<usize>,
    lane: &Lane,
    li: usize,
    w: usize,
) {
    let cell_w = (w.saturating_sub(4) / 6).max(12);
    let mut cols: Vec<Vec<String>> = Vec::new();
    for (ci, cell) in lane.cells.iter().enumerate() {
        let mut col: Vec<String> = vec![format!("{} {}", cell.column, cell.total)];
        for (ri, _) in cell.cards.iter().enumerate() {
            col.push(card_line(b, li, ci, ri, cell_w));
        }
        if cell.total > cell.cards.len() {
            col.push(format!("+{} more", cell.total - cell.cards.len()));
        }
        cols.push(col);
    }
    interleave(lines, follow, b, cols, lane, li, w, cell_w);
}

/// Row-merge the cells' line columns, tracking the cursor row for follow.
fn interleave(
    lines: &mut Vec<String>,
    follow: &mut Option<usize>,
    b: &BoardView,
    cols: Vec<Vec<String>>,
    lane: &Lane,
    li: usize,
    w: usize,
    cell_w: usize,
) {
    let height = cols.iter().map(|c| c.len()).max().unwrap_or(0);
    let start = lines.len();
    for row in 0..height {
        let mut line = String::new();
        for col in &cols {
            let seg = col.get(row).cloned().unwrap_or_default();
            line.push_str(&pad(&seg, cell_w));
            line.push(' ');
        }
        lines.push(trunc(&line, w));
    }
    // The cursor's card row sits at: header row (row 0) + the cursor's row.
    if lane.cells.get(b.col).is_some_and(|c| !c.cards.is_empty()) {
        let row = b.row.min(lane.cells[b.col].cards.len() - 1);
        *follow = Some(start + 1 + row);
    }
    let _ = li;
}

/// The expanded lane below `WIDE_CELLS_AT`: the cells stacked, the old
/// `build_kanban`'s shape - a `Now  12` header, card rows, `+N more`.
fn push_stacked_cells(
    b: &BoardView,
    lines: &mut Vec<String>,
    follow: &mut Option<usize>,
    lane: &backlog_model::Lane,
    li: usize,
    w: usize,
) {
    for (ci, cell) in lane.cells.iter().enumerate() {
        let header_at = lines.len();
        lines.push(trunc(&format!("{}  {}", cell.column, cell.total), w));
        for (ri, _) in cell.cards.iter().enumerate() {
            lines.push(card_line(b, li, ci, ri, w));
        }
        if cell.total > cell.cards.len() {
            lines.push(trunc(
                &format!("+{} more", cell.total - cell.cards.len()),
                w,
            ));
        }
        if ci == b.col && !cell.cards.is_empty() {
            let row = b.row.min(cell.cards.len() - 1);
            *follow = Some(header_at + 1 + row);
        }
    }
}

/// Keys while the board owns the keyboard. The drill-down, the find input,
/// and the facet picker each consume a whole chunk; otherwise the folded
/// modal keys drive the board itself.
pub(crate) async fn board_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let Some(b) = view.backlog_board.as_mut() else {
        return Ok(StdinFlow::Continue);
    };
    if b.detail.is_some() {
        return node_detail::detail_keys(view, bytes, sock_w).await;
    }
    if b.input.is_some() {
        input_keys(view, bytes);
        return Ok(StdinFlow::Continue);
    }
    if b.pick.is_some() {
        pick_keys(view, bytes);
        return Ok(StdinFlow::Continue);
    }
    if b.facet.is_some() {
        facet_keys(view, bytes);
        return Ok(StdinFlow::Continue);
    }
    // A lone-Esc chunk closes at once (the old node-detail contract: the
    // modal fold would otherwise hold the byte pending a sequence).
    if bytes == [0x1b] && b.board_esc.is_empty() {
        close_board(view);
        return Ok(StdinFlow::Continue);
    }
    let mut esc = std::mem::take(&mut b.board_esc);
    let toks = fold_modal_keys(&mut esc, bytes);
    let b = view.backlog_board.as_mut().expect("re-read after take");
    b.board_esc = esc;
    for tok in toks {
        if view.backlog_board.is_none() {
            break;
        }
        match tok {
            ModalKey::Esc | ModalKey::Byte(b'q') => close_board(view),
            ModalKey::Up | ModalKey::Byte(b'k') => move_row(view, false),
            ModalKey::Down | ModalKey::Byte(b'j') => move_row(view, true),
            ModalKey::Left | ModalKey::Byte(b'h') => move_col(view, false),
            ModalKey::Right | ModalKey::Byte(b'l') => move_col(view, true),
            ModalKey::Byte(b'[') => move_lane(view, false),
            ModalKey::PageUp => move_row(view, false),
            ModalKey::PageDown => move_row(view, true),
            ModalKey::Byte(b']') => move_lane(view, true),
            ModalKey::Byte(b'L') => cycle_lanes(view),
            ModalKey::Byte(b'r') => {
                if let Some(b) = view.backlog_board.as_mut() {
                    b.force = true;
                    b.want = true;
                }
            }
            ModalKey::Byte(b'/') => open_find(view),
            ModalKey::Byte(b'f') => open_facet(view),

            ModalKey::Byte(b'b') => dispatch_plan(view, sock_w).await?,
            ModalKey::Byte(b't') => launch_target(view, sock_w).await?,
            ModalKey::Byte(b'A') => ask_the_king(view, sock_w).await?,
            ModalKey::Byte(b'e') => edit_title(view)?,
            ModalKey::Byte(b'p') => edit_priority(view)?,
            ModalKey::Byte(b's') => edit_size(view)?,
            ModalKey::Byte(b'S') => edit_status(view)?,
            ModalKey::Byte(b'D') => append_details(view)?,
            ModalKey::Byte(b'T') => rank_move(view, "top", None)?,
            ModalKey::Byte(b'K') => rank_move(view, "before", Some(true))?,
            ModalKey::Byte(b'J') => rank_move(view, "after", Some(false))?,
            ModalKey::Enter => open_detail(view),
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// Close the board (and any drill-down inside it). The board is an
/// overlay, not a panel: no Resize travels.
fn close_board(view: &mut View) {
    view.backlog_board = None;
}

/// Move the card cursor within the cell (clamped to the painted cards).
fn move_row(view: &mut View, down: bool) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let Some(cell) = b
        .body
        .as_ref()
        .and_then(|bd| bd.lanes.get(b.lane))
        .and_then(|l| l.cells.get(b.col))
    else {
        return;
    };
    let len = cell.cards.len();
    if len == 0 {
        return;
    }
    b.row = if down {
        (b.row + 1).min(len - 1)
    } else {
        b.row.saturating_sub(1)
    };
}

/// Move the column cursor (clamped to the six cells); the row clamps at
/// render time.
fn move_col(view: &mut View, right: bool) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    if b.body.is_none() {
        return;
    }
    b.col = if right {
        (b.col + 1).min(5)
    } else {
        b.col.saturating_sub(1)
    };
    b.row = 0;
}

/// Jump to the previous/next lane and land on its first non-empty cell.
fn move_lane(view: &mut View, next: bool) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let Some(board) = b.body.as_ref() else {
        return;
    };
    let lanes = board.lanes.len();
    if lanes < 2 {
        return;
    }
    let next_lane = if next {
        (b.lane + 1).min(lanes - 1)
    } else {
        b.lane.saturating_sub(1)
    };
    b.lane = next_lane;
    if let Some(lane) = board.lanes.get(next_lane) {
        if let Some((ci, _)) = lane
            .cells
            .iter()
            .enumerate()
            .find(|(_, c)| !c.cards.is_empty())
        {
            b.col = ci;
            b.row = 0;
        }
    }
}

/// `L`: cycle the lanes query project -> epic -> none -> project, keeping
/// the cursor on the same card when the new grouping still shows it.
fn cycle_lanes(view: &mut View) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    cycle_lanes_b(b);
}

/// The pure half of the lane cycle, so tests exercise it without a View.
fn cycle_lanes_b(b: &mut BoardView) {
    b.query.lanes = match b.query.lanes {
        backlog_model::LanesBy::Project => backlog_model::LanesBy::Epic,
        backlog_model::LanesBy::Epic => backlog_model::LanesBy::None,
        backlog_model::LanesBy::None => backlog_model::LanesBy::Project,
    };
    let focus = cursor_card_id(b);
    rederive(b);
    focus_card(b, focus.as_deref());
}

/// `/`: open the one-line find input.
fn open_find(view: &mut View) {
    if let Some(b) = view.backlog_board.as_mut() {
        b.input = Some((BoardInputKind::Find, String::new()));
    }
}

/// The one-line input's keys: Enter applies per mode, Esc cancels,
/// backspace pops, printable ASCII appends. Mirrors [`super::mail_input`].
fn input_keys(view: &mut View, bytes: &[u8]) {
    let mut esc = {
        let b = view.backlog_board.as_mut().expect("input open");
        std::mem::take(&mut b.input_esc)
    };
    let keys = fold_search_input(&mut esc, bytes);
    let b = view.backlog_board.as_mut().expect("input open");
    b.input_esc = esc;
    for key in keys {
        let Some(b) = view.backlog_board.as_mut() else {
            break;
        };
        if b.input.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                b.input = None;
                b.input_esc.clear();
            }
            SearchKey::Byte(b'\r' | b'\n') => input_commit(view),
            SearchKey::Byte(0x7f | 0x08) => {
                if let Some((_, buf)) = b.input.as_mut() {
                    buf.pop();
                }
            }
            SearchKey::Byte(c @ 0x20..=0x7e) => {
                if let Some((_, buf)) = b.input.as_mut() {
                    if buf.len() < 200 {
                        buf.push(c as char);
                    }
                }
            }
            SearchKey::Byte(_) => {}
        }
    }
}

/// Enter on the one-line input, per mode.
fn input_commit(view: &mut View) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let Some((kind, text)) = b.input.take() else {
        return;
    };
    b.input_esc.clear();
    let text = text.trim().to_string();
    match kind {
        BoardInputKind::Find => {
            b.query.q = (!text.is_empty()).then_some(text);
            let focus = cursor_card_id(b);
            rederive(b);
            focus_card(b, focus.as_deref());
        }
        BoardInputKind::Title => {
            if text.is_empty() {
                view.set_notice("title: nothing to set".into());
                return;
            }
            let Some(id) = edit_target(b) else {
                return;
            };
            let args = match crate::backlog_write::field_argv(
                &id,
                crate::backlog_write::Field::Title,
                &text,
                "the mux backlog view",
            ) {
                Ok(args) => args,
                Err(e) => {
                    view.set_notice(e);
                    return;
                }
            };
            queue_write(b, WriteAction::Args(args, None));
        }
        BoardInputKind::Append => {
            if text.is_empty() {
                view.set_notice("details: nothing to append".into());
                return;
            }
            let Some(id) = edit_target(b) else {
                return;
            };
            queue_write(b, WriteAction::Append { id, text });
        }
    }
}

/// Queue one write verb for the run loop (one at a time).
fn queue_write(b: &mut BoardView, action: WriteAction) -> bool {
    if b.write_action.is_some() {
        return false;
    }
    b.write_action = Some(action);
    true
}

/// The write's target: the drill-down's node when open, else the cursor
/// card. The board's own keys and the drill-down's edit keys share it.
pub(crate) fn edit_target(b: &BoardView) -> Option<String> {
    if let Some(d) = &b.detail {
        return Some(d.node_id.clone());
    }
    cursor_card_id(b)
}

/// The write gate: when the backend cannot answer this key's feature,
/// its reason is the answer. `Some(reason)` = refused; the caller shows
/// the notice and starts no process.
fn gated(b: &BoardView, feature: &str) -> Option<String> {
    b.body
        .as_ref()
        .and_then(|bd| unavailable_reason(bd, feature))
}

/// `e`: the title editor, pre-filled with the current title.
pub(crate) fn edit_title(view: &mut View) -> Result<(), String> {
    let Some(b) = view.backlog_board.as_mut() else {
        return Ok(());
    };
    if let Some(reason) = gated(b, unavailable_features::FIELD_EDITS) {
        view.set_notice(reason);
        return Ok(());
    }
    let Some(id) = edit_target(b) else {
        return Ok(());
    };
    let title = card_title(b, &id);
    b.input = Some((BoardInputKind::Title, title));
    Ok(())
}

/// The target card's current title prefill (the title editor's starting
/// text). Empty when the read no longer shows the card.
fn card_title(b: &BoardView, id: &str) -> String {
    find_card(b, id)
        .map(|c| c.title.clone())
        .unwrap_or_default()
}

/// `p`: the priority picker (p0-p3).
pub(crate) fn edit_priority(view: &mut View) -> Result<(), String> {
    let Some(b) = view.backlog_board.as_mut() else {
        return Ok(());
    };
    if gated(b, unavailable_features::FIELD_EDITS).is_none() && edit_target(b).is_some() {
        b.pick = Some(PickState {
            kind: PickKind::Priority,
            sel: 0,
        });
    }
    Ok(())
}

/// `s`: the size picker (S, M, L).
pub(crate) fn edit_size(view: &mut View) -> Result<(), String> {
    let Some(b) = view.backlog_board.as_mut() else {
        return Ok(());
    };
    if gated(b, unavailable_features::FIELD_EDITS).is_none() && edit_target(b).is_some() {
        b.pick = Some(PickState {
            kind: PickKind::Size,
            sel: 0,
        });
    }
    Ok(())
}

/// `S`: the status picker (idea, design, ready, deferred, done). A
/// deferred move carries the reason the patch door requires.
pub(crate) fn edit_status(view: &mut View) -> Result<(), String> {
    let Some(b) = view.backlog_board.as_mut() else {
        return Ok(());
    };
    if let Some(reason) = gated(b, unavailable_features::FIELD_EDITS) {
        view.set_notice(reason);
        return Ok(());
    }
    if edit_target(b).is_some() {
        b.pick = Some(PickState {
            kind: PickKind::Status,
            sel: 0,
        });
    }
    Ok(())
}

/// `D`: append one paragraph to the node's details. The text is typed
/// now; the fresh store read happens inside the write task.
pub(crate) fn append_details(view: &mut View) -> Result<(), String> {
    let Some(b) = view.backlog_board.as_mut() else {
        return Ok(());
    };
    if let Some(reason) = gated(b, unavailable_features::FIELD_EDITS) {
        view.set_notice(reason);
        return Ok(());
    }
    if edit_target(b).is_some() {
        b.input = Some((BoardInputKind::Append, String::new()));
    }
    Ok(())
}

/// `T`/`K`/`J`: card moves through `fno backlog rank --operator`. `K`/`J`
/// anchor on the nearest card above/below in the same cell with the same
/// parent (the verb's own rank scope, the closest pick the view can
/// make). The verb's refusal shows verbatim.
fn rank_move(view: &mut View, word: &str, up: Option<bool>) -> Result<(), String> {
    let Some(b) = view.backlog_board.as_mut() else {
        return Ok(());
    };
    if let Some(reason) = gated(b, unavailable_features::CARD_MOVES) {
        view.set_notice(reason);
        return Ok(());
    }
    let Some(id) = cursor_card_id(b) else {
        return Ok(());
    };
    let anchor = up.and_then(|u| same_parent_anchor(b, u));
    if up.is_some() && anchor.is_none() {
        let dir = if up == Some(true) { "above" } else { "below" };
        view.set_notice(format!(
            "no card {dir} in the same rank scope; T pins it to the top"
        ));
        return Ok(());
    }
    let place = match (word, up) {
        ("top", _) => crate::backlog_write::Place::Top,
        ("before", _) => crate::backlog_write::Place::Before,
        ("after", _) => crate::backlog_write::Place::After,
        _ => return Ok(()),
    };
    let args = match crate::backlog_write::rank_argv(&id, place, anchor.as_deref()) {
        Ok(args) => args,
        Err(e) => {
            view.set_notice(e);
            return Ok(());
        }
    };
    let _ = queue_write(b, WriteAction::Args(args, None));
    Ok(())
}

/// The nearest card above (`true`) or below (`false`) in the same cell
/// with the same parent - the rank anchor.
fn same_parent_anchor(b: &BoardView, up: bool) -> Option<String> {
    let card = card_at(b)?;
    let parent = card.parent.as_deref();
    let cell = b.body.as_ref()?.lanes.get(b.lane)?.cells.get(b.col)?;
    let idx = b.row.min(cell.cards.len() - 1);
    let mut range: Box<dyn Iterator<Item = &backlog_model::Card>> = if up {
        Box::new(cell.cards[..idx].iter().rev())
    } else {
        Box::new(cell.cards[idx + 1..].iter())
    };
    range
        .find(|c| c.parent.as_deref() == parent)
        .map(|c| c.id.clone())
}

/// `b`: the blueprint spawn door, `Command::DispatchPlan` - the same
/// send the old card menu made. The server's plan branch refuses only an
/// in-flight node; its spawn gate answers through the dispatch-notice path.
pub(crate) async fn dispatch_plan(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let Some(b) = view.backlog_board.as_mut() else {
        return Ok(());
    };
    let Some(id) = edit_target(b) else {
        return Ok(());
    };
    write_msg(
        sock_w,
        &ClientMsg::Command(Command::DispatchPlan {
            node: id,
            account: view.active_account.clone(),
        }),
    )
    .await
    .map_err(|e| format!("plan spawn send failed: {e}"))
}

/// `t`: open the launcher prefilled to launch the drill-down node (or the
/// cursor card) as a target. Nothing spawns here: the board closes, the
/// dock opens with `/fno:target {id}` and the node's project, and the
/// operator picks harness, model and effort before any Launch. The launch
/// carries `--node`, so the door's dispatch guard judges the node and the
/// worker joins its roster row and card. A card already being worked
/// refuses before the dock opens (the plan-refusal wording), and a kept
/// draft is never overwritten.
pub(crate) async fn launch_target(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let (id, cwd, busy) = {
        let b = view
            .backlog_board
            .as_ref()
            .expect("board open while its keys fold");
        let Some(id) = edit_target(b) else {
            return Ok(());
        };
        // No gathered inputs yet: the card flags and the node's cwd are
        // unreadable, so no prefill can be trusted.
        let Some(inputs) = b.inputs.as_ref() else {
            view.set_notice("the board is still loading".to_string());
            return Ok(());
        };
        let nv = backlog_model::node(inputs, &id);
        let busy = nv
            .as_ref()
            .map(|n| n.card.claimed || n.card.live)
            .unwrap_or(false);
        (id, nv.and_then(|n| n.cwd), busy)
    };
    if busy {
        view.set_notice(format!(
            "{id} is already being worked; open its session instead"
        ));
        return Ok(());
    }
    close_board(view);
    if !super::sideline::show_composer(view, sock_w).await? {
        return Ok(());
    }
    if let Err(e) = agent_launcher::open_with(
        view,
        format!("/fno:target {id}"),
        cwd.as_deref(),
        id.clone(),
    ) {
        view.set_notice(e);
        return Ok(());
    }
    if cwd.as_deref().unwrap_or_default().is_empty() {
        view.set_notice(format!("{id} records no project path; pick the project"));
    }
    Ok(())
}

/// `A`: ask the king for a blueprint. The king is the model's
/// `card.king`; none means a notice and no mail. Sent wrapped through
/// `Command::MailAgent` (law d-f6570dc9: never `--raw`).
pub(crate) async fn ask_the_king(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let (id, king, title) = {
        let b = view.backlog_board.as_mut().expect("board open");
        let Some(id) = edit_target(b) else {
            return Ok(());
        };
        let Some(king) = card_king_name(b, &id) else {
            view.set_notice(format!("no king rules {id}, its epic or its project"));
            return Ok(());
        };
        (id.clone(), king, card_title(b, &id))
    };
    let mut text = format!("operator asks: please run /fno:blueprint subagent {id} for {id}");
    let cut: String = title.chars().take(80).collect();
    if !cut.is_empty() {
        text.push_str(&format!(" ({cut})"));
    }
    text.push_str(". Sent from the mux backlog view.");
    write_msg(
        sock_w,
        &ClientMsg::Command(Command::MailAgent { name: king, text }),
    )
    .await
    .map_err(|e| format!("king mail send failed: {e}"))
}

/// The target node's king name, from the model's card.
fn card_king_name(b: &BoardView, id: &str) -> Option<String> {
    let card = find_card(b, id)?;
    card.king.as_ref().map(|k| k.name.clone())
}

/// Find a card by id anywhere on the board.
fn find_card<'a>(b: &'a BoardView, id: &str) -> Option<&'a backlog_model::Card> {
    let board = b.body.as_ref()?;
    board
        .lanes
        .iter()
        .flat_map(|l| l.cells.iter())
        .flat_map(|c| c.cards.iter())
        .find(|c| c.id == id)
}

/// Re-derive the board from the current query over the gathered inputs,
/// with no re-gather. Filters and lane cycling share this one path.
fn rederive(b: &mut BoardView) {
    let Some(inputs) = b.inputs.as_ref() else {
        return;
    };
    let Ok(q) = b.query.to_query() else {
        return;
    };
    let board = backlog_model::board(inputs, &q);
    b.errors = board.errors.clone();
    // A filter that matches nothing has empty errors and REPLACES the
    // body: the zeroed board is the honest answer. Only a failed read
    // (errors with no lanes) keeps the last good body.
    if !board.lanes.is_empty() || board.errors.is_empty() || b.body.is_none() {
        b.body = Some(board);
    }
}

/// The reason a board feature is off this external backend, from the
/// model's `unavailable` list.
fn unavailable_reason(board: &Board, feature: &str) -> Option<String> {
    board
        .unavailable
        .iter()
        .find(|u| u.feature == feature)
        .map(|u| u.reason.clone())
}

/// `f`: open the facet picker at the facet list.
fn open_facet(view: &mut View) {
    if let Some(b) = view.backlog_board.as_mut() {
        b.facet = Some(FacetPick {
            facet: 0,
            sel: 0,
            value_sel: None,
        });
    }
}

/// Keys for the `f` facet picker: arrows move the active level's cursor,
/// Enter picks, Esc backs out one level (or closes at the first).
fn facet_keys(view: &mut View, bytes: &[u8]) {
    let toks = {
        let b = view.backlog_board.as_mut().expect("facet open");
        let mut esc = std::mem::take(&mut b.board_esc);
        let toks = fold_modal_keys(&mut esc, bytes);
        b.board_esc = esc;
        toks
    };
    for tok in toks {
        let Some(b) = view.backlog_board.as_mut() else {
            break;
        };
        if b.facet.is_none() {
            break;
        }
        match tok {
            ModalKey::Esc => {
                let pick = b.facet.as_mut().expect("facet open");
                if pick.value_sel.is_none() {
                    b.facet = None;
                } else {
                    pick.value_sel = None;
                }
            }
            ModalKey::Up => shift_sel(view, false),
            ModalKey::Down => shift_sel(view, true),
            ModalKey::Enter => facet_commit(view),
            _ => {}
        }
    }
}

/// The fixed facet list, one row of the first-level popup.
pub(crate) const FACET_NAMES: [&str; 6] = ["project", "epic", "status", "priority", "size", "king"];

/// The facets this backend can answer, as `(FACET_NAMES index, name)`.
fn visible_facets(board: &Board) -> Vec<(usize, &'static str)> {
    (0..FACET_NAMES.len())
        .filter(|&i| unavailable_reason(board, facet_feature(FACET_NAMES[i])).is_none())
        .map(|i| (i, FACET_NAMES[i]))
        .collect()
}

/// The facet's current filter value, for the first-level row label.
fn facet_current(b: &BoardView, facet: usize) -> String {
    let empty = String::new();
    let v = match facet {
        0 => b.query.project.as_ref().unwrap_or(&empty),
        1 => b.query.epic.as_ref().unwrap_or(&empty),
        2 => b.query.status.as_ref().unwrap_or(&empty),
        3 => b.query.priority.as_ref().unwrap_or(&empty),
        4 => b.query.size.as_ref().unwrap_or(&empty),
        5 => b.query.king.as_ref().unwrap_or(&empty),
        _ => &empty,
    };
    if v.is_empty() {
        "any".into()
    } else {
        v.clone()
    }
}

/// Which `unavailable` feature gates this facet, if any.
fn facet_feature(facet: &str) -> &'static str {
    match facet {
        "size" => unavailable_features::SIZE_FILTER,
        _ => "",
    }
}

/// The picked facet's `(label, value)` pairs from the board's facets.
fn facet_values(board: &Board, facet: usize) -> Vec<(String, String)> {
    match facet {
        0 => board
            .facets
            .projects
            .iter()
            .map(|p| (p.clone(), p.clone()))
            .collect(),
        1 => board
            .facets
            .epics
            .iter()
            .map(|e| match &e.title {
                Some(t) => (format!("{}  {}", e.id, t), e.id.clone()),
                None => (e.id.clone(), e.id.clone()),
            })
            .collect(),
        2 => board
            .facets
            .statuses
            .iter()
            .map(|s| (s.clone(), s.clone()))
            .collect(),
        3 => board
            .facets
            .priorities
            .iter()
            .map(|p| (p.clone(), p.clone()))
            .collect(),
        4 => board
            .facets
            .sizes
            .iter()
            .map(|s| (s.clone(), s.clone()))
            .collect(),
        5 => board
            .facets
            .kings
            .iter()
            .map(|k| (k.clone(), k.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// The facet picker's popup for the ACTIVE level: the facet list, or the
/// picked facet's values with `any` first. A facet the model names in
/// `unavailable` is left out of the first level.
pub(crate) fn facet_popup(b: &BoardView) -> Option<Popup> {
    let pick = b.facet.as_ref()?;
    let board = b.body.as_ref()?;
    let mut rows: Vec<PopupRow> = vec![PopupRow::Header("filter".into()), PopupRow::Rule];
    if let Some(vsel) = pick.value_sel {
        let name = *FACET_NAMES.get(pick.facet)?;
        rows.push(PopupRow::Header(name.into()));
        rows.push(PopupRow::Rule);
        rows.push(pick_row("any"));
        for (label, _) in facet_values(board, pick.facet) {
            rows.push(pick_row(&label));
        }
        let mut popup = Popup::new(rows, Anchor::Center)
            .title(format!("filter: {name}"))
            .footer("enter filter · esc back");
        popup.sel = vsel;
        Some(popup)
    } else {
        for (i, name) in visible_facets(board) {
            let current = facet_current(b, i);
            rows.push(PopupRow::Entry {
                glyph: " ".into(),
                label: format!("{name}: {current}"),
                hint: String::new(),
                enabled: true,
            });
        }
        let mut popup = Popup::new(rows, Anchor::Center)
            .title("backlog filters")
            .footer("enter pick · esc close");
        popup.sel = pick.sel;
        Some(popup)
    }
}

/// One selectable popup row with a space glyph.
fn pick_row(label: &str) -> PopupRow {
    PopupRow::Entry {
        glyph: " ".into(),
        label: label.into(),
        hint: String::new(),
        enabled: true,
    }
}

/// Move the facet picker's active level's cursor.
fn shift_sel(view: &mut View, down: bool) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let facet_n = b
        .body
        .as_ref()
        .map(|bd| visible_facets(bd).len())
        .unwrap_or(FACET_NAMES.len());
    let Some(pick) = b.facet.as_mut() else {
        return;
    };
    match pick.value_sel {
        None => {
            let n = facet_n;
            pick.sel = if down {
                (pick.sel + 1) % n
            } else {
                pick.sel.saturating_sub(1)
            };
        }
        Some(vsel) => {
            let n = b
                .body
                .as_ref()
                .map(|bd| facet_values(bd, pick.facet).len())
                .unwrap_or(0);
            pick.value_sel = Some(if down {
                (vsel + 1) % (n + 1)
            } else {
                vsel.saturating_sub(1)
            });
        }
    }
}

/// Enter at the value level: set the picked filter (`any` clears), close
/// the picker, re-derive with the cursor kept on its card when still
/// shown.
fn facet_commit(view: &mut View) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let Some(pick) = b.facet else {
        return;
    };
    let Some(vsel) = pick.value_sel else {
        let facet = b
            .body
            .as_ref()
            .and_then(|bd| visible_facets(bd).get(pick.sel).copied())
            .map(|(i, _)| i)
            .unwrap_or(pick.facet);
        b.facet = Some(FacetPick {
            facet,
            sel: pick.sel,
            value_sel: Some(0),
        });
        return;
    };
    let values = b
        .body
        .as_ref()
        .map(|bd| facet_values(bd, pick.facet))
        .unwrap_or_default();
    let value = match vsel {
        0 => None,
        n => values.get(n - 1).map(|(_, v)| v.clone()),
    };
    let facet_name = FACET_NAMES.get(pick.facet).copied().unwrap_or("");
    match (facet_name, value) {
        ("project", v) => b.query.project = v,
        ("epic", v) => b.query.epic = v,
        ("status", v) => b.query.status = v,
        ("priority", v) => b.query.priority = v,
        ("size", v) => b.query.size = v,
        ("king", v) => b.query.king = v,
        _ => {}
    }
    b.facet = None;
    let focus = cursor_card_id(b);
    rederive(b);
    focus_card(b, focus.as_deref());
}

/// The p/s/S picker's popup for the compose pass.
pub(crate) fn pick_popup(b: &BoardView) -> Option<Popup> {
    let pick = b.pick?;
    let values = pick.kind.values();
    let mut rows: Vec<PopupRow> = vec![PopupRow::Header("pick".into()), PopupRow::Rule];
    for v in values {
        rows.push(PopupRow::Entry {
            glyph: " ".into(),
            label: (*v).into(),
            hint: String::new(),
            enabled: true,
        });
    }
    let mut popup = Popup::new(rows, Anchor::Center)
        .title("backlog edit")
        .footer("enter set · esc close");
    popup.sel = pick.sel.min(values.len() - 1);
    Some(popup)
}

/// The p/s/S picker's keys: Up/Down move the cursor, Enter commits, Esc
/// closes.
fn pick_keys(view: &mut View, bytes: &[u8]) {
    let toks = {
        let b = view.backlog_board.as_mut().expect("pick open");
        let mut esc = std::mem::take(&mut b.board_esc);
        let toks = fold_modal_keys(&mut esc, bytes);
        b.board_esc = esc;
        toks
    };
    for tok in toks {
        let Some(b) = view.backlog_board.as_mut() else {
            break;
        };
        if b.pick.is_none() {
            break;
        }
        match tok {
            ModalKey::Esc => b.pick = None,
            ModalKey::Up => {
                if let Some(p) = b.pick.as_mut() {
                    p.sel = p.sel.saturating_sub(1);
                }
            }
            ModalKey::Down => {
                if let Some(p) = b.pick.as_mut() {
                    p.sel = (p.sel + 1).min(p.kind.values().len() - 1);
                }
            }
            ModalKey::Enter => pick_commit(view),
            _ => {}
        }
    }
}

/// Enter on the p/s/S picker: queue the `fno backlog update` write for
/// the run loop. A deferred status move carries the reason the patch door
/// requires.
fn pick_commit(view: &mut View) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let Some(p) = b.pick.take() else {
        return;
    };
    let Some(id) = edit_target(b) else {
        return;
    };
    let value = p.kind.values().get(p.sel).copied().unwrap_or("");
    if value.is_empty() {
        return;
    }
    let field = match p.kind {
        PickKind::Priority => crate::backlog_write::Field::Priority,
        PickKind::Size => crate::backlog_write::Field::Size,
        PickKind::Status => crate::backlog_write::Field::Status,
    };
    match crate::backlog_write::field_argv(&id, field, value, "the mux backlog view") {
        Ok(args) => {
            if !queue_write(b, WriteAction::Args(args, None)) {
                view.set_notice("a write is already queued".into());
            }
        }
        Err(e) => view.set_notice(e),
    }
}

/// Enter on a board card: open the drill-down for the cursor's card,
/// held inside the board (a re-open of the SAME node keeps the trail).
pub(crate) fn open_detail(view: &mut View) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let Some(id) = cursor_card_id(b) else {
        return;
    };
    let same = b.detail.as_ref().is_some_and(|d| d.node_id == id);
    if !same {
        b.detail = Some(node_detail::NodeDetailOverlay {
            node_id: id,
            trail: Vec::new(),
            sel: 0,
            details_open: false,
        });
    }
}

#[cfg(test)]
#[path = "tests/backlog_board_tests.rs"]
mod tests;
