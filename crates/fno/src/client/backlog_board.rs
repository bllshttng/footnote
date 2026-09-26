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

use super::backlog_style::{self, BLine, BRole, BSeg};
use super::*;
use crate::backlog_model;
use crate::backlog_model::{unavailable_features, Board, Lane};
use crate::backlog_view::graph_path;
use crate::store_client;
use serde_json::Value;
use std::time::Duration;

/// The prefix chord's gate: the board opens only while the experimental
/// pref is on; otherwise a notice names the menu row that turns it on.
pub(crate) fn open_pref_gated(view: &mut super::View) {
    if view.experimental_backlog {
        super::View::open(view);
    } else {
        view.set_notice("backlog board is off: sideline menu > experimental: backlog view".into());
    }
}

/// Right-pad one cell segment so side-by-side columns align.
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

/// The shown node's cached markdown document read: refreshed only when
/// the shown node, the path or the mtime changed.
#[derive(Debug, Clone)]
pub(crate) struct PaneDoc {
    pub(crate) node_id: String,
    pub(crate) path: String,
    pub(crate) mtime: Option<std::time::SystemTime>,
    pub(crate) lines_src: String,
    /// Non-empty when the read failed: the pane names it.
    pub(crate) error: String,
}

/// The board's active filters, held as route pairs so the model's own
/// [`backlog_model::Query::from_pairs`] parses them - one parser, no second
/// filter semantics.
#[derive(Debug, Default, Clone)]
pub(crate) struct QueryState {
    pub(crate) lanes: backlog_model::LanesBy,
    /// Multi-select sets by facet name; a set filters any-of.
    pub(crate) sets: std::collections::BTreeMap<&'static str, Vec<String>>,
    pub(crate) q: Option<String>,
    pub(crate) view: backlog_model::View,
}

impl QueryState {
    /// The model's parsed query for this state.
    pub(crate) fn to_query(&self) -> Result<backlog_model::Query, String> {
        let lanes = match self.lanes {
            backlog_model::LanesBy::Project => "project",
            backlog_model::LanesBy::Epic => "epic",
            backlog_model::LanesBy::None => "none",
        };
        let mut p: Vec<(String, String)> = vec![("lanes".into(), lanes.into())];
        for (k, vals) in &self.sets {
            for v in vals {
                p.push((k.to_string(), v.clone()));
            }
        }
        if self.view == backlog_model::View::List {
            p.push(("view".into(), "list".into()));
        }
        if let Some(q) = &self.q {
            p.push(("q".into(), q.clone()));
        }
        backlog_model::Query::from_pairs(&p)
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
    /// The column layout (which columns, order, focus width), D4.
    pub(crate) layout: BoardLayout,
    /// The `c` column picker's open state.
    pub(crate) colpick: Option<ColPick>,
    /// The `?` keys overlay, when open.
    pub(crate) keys_overlay: bool,
    /// Pending escape bytes in board-key mode (split-arrow safety).
    board_esc: Vec<u8>,
    /// The drill-down overlay, when open (wave 4). `Some` = the detail
    /// pane holds focus; `None` = the board pane holds it.
    pub(crate) detail: Option<node_detail::NodeDetailOverlay>,
    pub(crate) detail_esc: Vec<u8>,
    /// The shown node's cached markdown document read.
    pub(crate) doc: Option<PaneDoc>,
    /// The write verb queued for the run loop (one at a time).
    pub(crate) write_action: Option<WriteAction>,
}

/// The `c` column picker's state.
pub(crate) struct ColPick {
    /// The cursor.
    pub(crate) sel: usize,
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
    /// The `N` note text.
    Note,
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
            layout: crate::view_store::load_board_layout(),
            colpick: None,
            keys_overlay: false,
            board_esc: Vec::new(),
            detail: None,
            detail_esc: Vec::new(),
            doc: None,
            write_action: None,
        }
    }
}

/// The board's column layout type lives in the view store next to its
/// persistence; the board reads it from there.
pub(crate) use crate::view_store::BoardLayout;

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
    backlog_panes::sync_doc(b);
    if let Some(n) = notice {
        view.set_notice(n);
    }
}

/// The card under the cursor, resolved against the current body. The row
/// clamps to the painted (capped) cards.
pub(crate) fn card_at(b: &BoardView) -> Option<&backlog_model::Card> {
    let board = b.body.as_ref()?;
    let name = b.layout.columns.get(b.col)?;
    let cell = board
        .lanes
        .get(b.lane)?
        .cells
        .iter()
        .find(|c| &c.column == name)?;
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
        for (shown, name) in b.layout.columns.iter().enumerate() {
            let Some(cell) = lane.cells.iter().find(|c| &c.column == name) else {
                continue;
            };
            for (ri, card) in cell.cards.iter().enumerate() {
                if card.id == id {
                    (b.lane, b.col, b.row) = (li, shown, ri);
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
pub(crate) fn render(b: &BoardView, w: usize) -> (Vec<BLine>, Option<usize>) {
    let mut lines: Vec<BLine> = Vec::new();
    let mut follow: Option<usize> = None;
    let Some(board) = b.body.as_ref() else {
        if b.errors.is_empty() {
            lines.push(BLine::plain("reading board..."));
        } else {
            for e in &b.errors {
                lines.push(BLine::meta(format!("! {e}")));
            }
        }
        return (lines, None);
    };
    push_stats_line(b, &mut lines, board, w);
    for e in &b.errors {
        lines.push(BLine::meta(format!("! {e}")));
    }
    // A filter matching nothing says so over a zeroed board; it never
    // falls back to the unfiltered body (AC11 shape).
    let shown: usize = board
        .lanes
        .iter()
        .map(|l| l.cells.iter().map(|c| c.total).sum::<usize>())
        .sum();
    if shown == 0 {
        let line = match b.query.q.as_deref() {
            Some(q) => format!("no cards match: {q}"),
            None => "no cards match".into(),
        };
        lines.push(BLine::meta(line));
    }
    push_lanes(b, &mut lines, &mut follow, board, w);
    (lines, follow)
}

/// The totals line: each column's open total, then the flow aggregate.
fn push_stats_line(b: &BoardView, lines: &mut Vec<BLine>, board: &Board, w: usize) {
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
    lines.push(BLine::meta(elide_words(&line, w)));
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

/// The persistent filter bar's cells: Search, Status, Type, Priority,
/// Milestone (the epic set), Labels, then Project and King when set.
/// Cells join with ` │ ` and the caller wraps them by width. When no node
/// carries a tag, the Labels cell says so.
pub(crate) fn filter_bar_lines(b: &BoardView, board: &Board, w: usize) -> Vec<BLine> {
    let _ = w;
    let mut cells: Vec<(&str, String)> = Vec::new();
    let q = b.query.q.as_deref().unwrap_or("any");
    cells.push(("Search", q.to_string()));
    let set = |name: &str| -> String {
        let vals = b.query.sets.get(name).map(|v| v.as_slice()).unwrap_or(&[]);
        if vals.is_empty() {
            "any".into()
        } else {
            vals.join(",")
        }
    };
    cells.push(("Status", set("status")));
    cells.push(("Type", set("type")));
    cells.push(("Priority", set("priority")));
    cells.push(("Milestone", set("epic")));
    let labels = set("tag");
    if board.facets.tags.is_empty() {
        cells.push(("Labels", "none on any node".into()));
    } else {
        cells.push(("Labels", labels));
    }
    if !set("project").is_empty() || !set("king").is_empty() {
        cells.push(("Project", set("project")));
        cells.push(("King", set("king")));
    }
    let mut lines: Vec<BLine> = Vec::new();
    let mut line = BLine::plain(String::new());
    let mut used = 0usize;
    for (i, (label, value)) in cells.iter().enumerate() {
        let cell_w = label.chars().count() + 2 + value.chars().count();
        if used > 0 && used + 3 + cell_w > w {
            lines.push(line.trunc(w));
            line = BLine::plain(String::new());
            used = 0;
            line.push_line(BLine::of(&[
                BSeg {
                    text: format!("{label}: "),
                    role: BRole::Meta,
                },
                BSeg {
                    text: value.clone(),
                    role: BRole::Body,
                },
            ]));
            used = cell_w;
            continue;
        }
        if used > 0 {
            line.push_line(BLine::plain(" │ "));
            used += 3;
        }
        line.push_line(BLine::of(&[
            BSeg {
                text: format!("{label}: "),
                role: BRole::Meta,
            },
            BSeg {
                text: value.clone(),
                role: BRole::Body,
            },
        ]));
        used += cell_w;
    }
    lines.push(line.trunc(w));
    lines
}

/// Truncate one line to `w` chars (the painter wraps nothing).
pub(crate) fn trunc(s: &str, w: usize) -> String {
    s.chars().take(w).collect()
}

/// Truncate a summary to `w` chars, but cut after the last whole word that
/// fits and mark the cut with an ellipsis: a summary truncated mid-word
/// (`Nex`) reads as a broken word, not a cut.
pub(crate) fn elide_words(s: &str, w: usize) -> String {
    // Walk by display columns, not chars: a wide glyph is two cells and the
    // ellipsis must stay inside `w` or the painter re-cuts the line and the
    // marker is lost.
    if s.chars().map(backlog_style::char_w).sum::<usize>() <= w {
        return s.to_string();
    }
    let mut used = 0usize;
    let mut cut_byte = s.len();
    let mut last_space = 0usize;
    for (i, ch) in s.char_indices() {
        let cw = backlog_style::char_w(ch);
        if used + cw > w.saturating_sub(1) {
            cut_byte = i;
            break;
        }
        used += cw;
        cut_byte = i + ch.len_utf8();
        if ch == ' ' {
            last_space = cut_byte;
        }
    }
    if last_space > 0 {
        // Drop the space the word boundary sits on, ellipsis takes its slot.
        format!("{}\u{2026}", &s[..last_space - 1])
    } else {
        format!("{}\u{2026}", &s[..cut_byte])
    }
}

/// The lanes: an accordion - the cursor's lane expanded below its header,
/// the rest collapsed to one header line each. With one lane (the model's
/// `lanes: none`), no header, always expanded.
fn push_lanes(
    b: &BoardView,
    lines: &mut Vec<BLine>,
    follow: &mut Option<usize>,
    board: &Board,
    w: usize,
) {
    for (li, lane) in board.lanes.iter().enumerate() {
        let expanded = board.lanes.len() == 1 || li == b.lane;
        if board.lanes.len() > 1 {
            let arrow = if expanded { "\u{25be}" } else { "\u{25b8}" };
            let mut segs: Vec<BSeg> = vec![
                BSeg {
                    text: format!("{arrow} "),
                    role: BRole::Body,
                },
                BSeg {
                    text: lane.title.to_string(),
                    role: BRole::Head,
                },
            ];
            for name in b.layout.columns.iter() {
                let Some(cell) = lane.cells.iter().find(|c| &c.column == name) else {
                    continue;
                };
                segs.push(BSeg {
                    text: format!("  {}", cell_total_line(cell)),
                    role: BRole::Meta,
                });
            }
            lines.push(BLine::of(&segs).trunc(w));
            lines.push(BLine::meta(rule(w)));
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
/// The thin rule under a heading (D4/D5).
pub(crate) fn rule(w: usize) -> String {
    std::iter::repeat(String::from("\u{2500}"))
        .take(w)
        .collect::<String>()
}

/// `Now 3` per cell, `Done 20/900` when the cap hides cards.
fn cell_total_line(cell: &backlog_model::Cell) -> String {
    if cell.total > cell.cards.len() {
        format!("{} {}/{}", cell.column, cell.cards.len(), cell.total)
    } else {
        format!("{} {}", cell.column, cell.total)
    }
}
/// One card row: the cursor `>`, a claim/block glyph, the short id (the
/// accent slot), and the title, truncated to the cell width.
fn card_line(b: &BoardView, li: usize, col_name: &str, ri: usize, w: usize) -> BLine {
    let sel = li == b.lane
        && col_name
            == b.layout
                .columns
                .get(b.col)
                .map(|s| s.as_str())
                .unwrap_or("")
        && ri == b.row;
    let Some(card) = b
        .body
        .as_ref()
        .and_then(|bd| bd.lanes.get(li))
        .and_then(|l| l.cells.iter().find(|c| c.column == col_name))
        .and_then(|c| c.cards.get(ri))
    else {
        return BLine::plain(String::new());
    };
    let glyph = if card.claimed {
        "\u{25cf}"
    } else if card.blocked {
        "\u{2298}"
    } else {
        " "
    };
    let sel_mark = if sel { "\u{25b8}" } else { " " };
    let mut line = BLine::of(&[
        BSeg {
            text: format!("{sel_mark}{glyph} "),
            role: BRole::Body,
        },
        BSeg {
            text: card.id.clone(),
            role: BRole::Label,
        },
        BSeg {
            text: format!(" {}", card.title),
            role: BRole::Body,
        },
    ]);
    line.band = sel;
    line.trunc(w)
}

/// The expanded lane at `w >= WIDE_CELLS_AT`: the shown cells side by
/// side. The cursor's column takes the focus column share of the width
/// (D4); every other shown column shrinks to an id plus a short title.
fn push_wide_cells(
    b: &BoardView,
    lines: &mut Vec<BLine>,
    follow: &mut Option<usize>,
    lane: &Lane,
    li: usize,
    w: usize,
) {
    let shown: Vec<(usize, &backlog_model::Cell)> = b
        .layout
        .columns
        .iter()
        .filter_map(|name| {
            lane.cells
                .iter()
                .enumerate()
                .find(|(_, c)| &c.column == name)
                .map(|(ci, c)| (ci, c))
        })
        .collect();
    let gaps = shown.len().saturating_sub(1);
    let others = shown.len().saturating_sub(1);
    let mut focus_w = (w * b.layout.focus_pct as usize / 100).max(12);
    // Every other column keeps a 12-column floor INSIDE `w`: when the focus
    // share leaves less than that, the focus column shrinks first - a column
    // squeezed past the row's width paints cut off while the cursor can
    // still rest on it.
    let other_w = if others > 0 {
        let share = w.saturating_sub(focus_w + gaps) / others;
        if share < 12 {
            focus_w = w.saturating_sub(12 * others + gaps).max(12);
            w.saturating_sub(focus_w + gaps) / others
        } else {
            share
        }
    } else {
        0
    };
    let mut cols: Vec<(usize, usize, Vec<BLine>)> = Vec::new();
    for (si, (ci, cell)) in shown.iter().enumerate() {
        let cw = if si == b.col { focus_w } else { other_w };
        let mut col: Vec<BLine> = vec![BLine::of(&[
            BSeg {
                text: cell.column.to_string(),
                role: BRole::Head,
            },
            BSeg {
                text: format!(" {}", cell_total(cell)),
                role: BRole::Meta,
            },
        ])];
        for ri in 0..cell.cards.len() {
            col.push(card_line(b, li, &cell.column, ri, cw));
        }
        if cell.total > cell.cards.len() {
            col.push(BLine::meta(format!(
                "+{} more",
                cell.total - cell.cards.len()
            )));
        }
        cols.push((*ci, cw, col));
    }
    interleave(lines, follow, b, cols, lane, li, w);
}

/// Row-merge the cells' line columns, tracking the cursor row for follow.
fn interleave(
    lines: &mut Vec<BLine>,
    follow: &mut Option<usize>,
    b: &BoardView,
    cols: Vec<(usize, usize, Vec<BLine>)>,
    lane: &Lane,
    li: usize,
    w: usize,
) {
    let height = cols.iter().map(|(_, _, c)| c.len()).max().unwrap_or(0);
    let start = lines.len();
    for row in 0..height {
        let mut line = BLine::plain(String::new());
        for (_ci, cw, col) in cols.iter() {
            let seg = col.get(row).cloned().unwrap_or_else(|| BLine::plain(""));
            line.push_line(seg.pad_to(*cw));
            line.push_line(BLine::plain(" "));
        }
        lines.push(line.trunc(w));
    }
    // The cursor's card row sits at: header row (row 0) + the cursor's row.
    if let Some(cell) = cursor_cell(b, lane) {
        if !cell.cards.is_empty() {
            let row = b.row.min(cell.cards.len() - 1);
            *follow = Some(start + 1 + row);
        }
    }
    let _ = li;
}

/// The model cell under the shown cursor column, if the lane carries it.
fn cursor_cell<'a>(b: &BoardView, lane: &'a Lane) -> Option<&'a backlog_model::Cell> {
    let name = b.layout.columns.get(b.col)?;
    lane.cells.iter().find(|c| &c.column == name)
}

/// The expanded lane below `WIDE_CELLS_AT`: the shown cells stacked, the
/// old `build_kanban`'s shape - a `Now  12` header, card rows, `+N more`.
fn push_stacked_cells(
    b: &BoardView,
    lines: &mut Vec<BLine>,
    follow: &mut Option<usize>,
    lane: &backlog_model::Lane,
    li: usize,
    w: usize,
) {
    for si in 0..b.layout.columns.len() {
        let Some(cell) = shown_cell_at(b, lane, si) else {
            continue;
        };
        let header_at = lines.len();
        lines.push(BLine::of(&[
            BSeg {
                text: cell.column.to_string(),
                role: BRole::Head,
            },
            BSeg {
                text: format!("  {}", cell_total(cell)),
                role: BRole::Meta,
            },
        ]));
        for ri in 0..cell.cards.len() {
            lines.push(card_line(b, li, &cell.column, ri, w));
        }
        if cell.total > cell.cards.len() {
            lines.push(BLine::meta(format!(
                "+{} more",
                cell.total - cell.cards.len()
            )));
        }
        if si == b.col && !cell.cards.is_empty() {
            let row = b.row.min(cell.cards.len() - 1);
            *follow = Some(header_at + 1 + row);
        }
    }
}

/// Just the count part of a header: `12`, or `20/900` when the cap hides
/// cards. The column name itself is the bold head.
fn cell_total(cell: &backlog_model::Cell) -> String {
    if cell.total > cell.cards.len() {
        format!("{}/{}", cell.cards.len(), cell.total)
    } else {
        format!("{}", cell.total)
    }
}

/// The shown cell at shown index `si`, or None when the lane lacks it.
fn shown_cell_at<'a>(
    b: &BoardView,
    lane: &'a backlog_model::Lane,
    si: usize,
) -> Option<&'a backlog_model::Cell> {
    let name = b.layout.columns.get(si)?;
    lane.cells.iter().find(|c| &c.column == name)
}

impl View {
    /// All chrome left of the content area: the agent sideline. The
    /// backlog no longer docks beside the content - it lives IN the
    /// sideline column as one of its views - so this is the panel width.
    pub(super) fn left_chrome_w(&self) -> u16 {
        self.panel_w()
    }

    /// The board's whole paint: the drill-down, the pickers, or the
    /// full-screen board. Windowed, the backlog paints inside the sideline
    /// column (the sideline's own draw path), so this paints nothing.
    /// The compose branch in `client.rs` is this one call.
    pub(super) fn draw_board(
        &self,
        cells: &mut [Cell],
        rows: usize,
        cols: usize,
        overlay_origin: (usize, usize),
        overlay_dims: (usize, usize),
    ) {
        let Some(b) = &self.backlog_board else {
            return;
        };
        if b.keys_overlay {
            // Checked before the drill-down: `?` opens this from the detail
            // too, so it must paint over it, and its Esc must land here.
            let m = board_keys_popup();
            draw_popup_overlay(cells, rows, cols, &m, self.term, &self.theme);
        } else if let Some(m) = pick_popup(b) {
            draw_popup_overlay(cells, rows, cols, &m, self.term, &self.theme);
        } else if let Some(m) = facet_popup(b) {
            draw_popup_overlay(cells, rows, cols, &m, self.term, &self.theme);
        } else if let Some(m) = colpick_popup(b) {
            draw_popup_overlay(cells, rows, cols, &m, self.term, &self.theme);
        } else if self.board_full {
            backlog_panes::paint(
                b,
                cells,
                rows,
                cols,
                (0, 0, rows, cols),
                b.detail.is_some(),
                &self.theme,
            );
        }
    }

    /// The sidebar menu's open action: a fresh board view at the next
    /// generation (the stale fold of a previous open can never land), and
    /// the sideline switched to the backlog view - the board IS that view
    /// now, not a floating window.
    pub(crate) fn open(view: &mut View) {
        backlog_board_open_fresh(view);
        set_sideline_view(view, crate::view_store::SidelineView::Backlog);
    }

    /// The menu's enable/disable toggle for the whole experimental view.
    /// Disabling closes the board; the choice persists in the view store.
    pub(crate) fn toggle_enabled(view: &mut View) {
        view.experimental_backlog = !view.experimental_backlog;
        crate::view_store::save_experimental_backlog_view(view.experimental_backlog);
        let on = if view.experimental_backlog {
            "on"
        } else {
            "off"
        };
        if !view.experimental_backlog {
            view.backlog_board = None;
            set_sideline_view(view, crate::view_store::SidelineView::Agents);
        }
        view.set_notice(format!("experimental backlog view: {on}"));
        view.refresh_open_sideline_menu();
    }
}

/// `V`: agents <-> backlog. The backlog leg rides the experimental pref
/// (the off case notices, like every other entry) and opens the board;
/// the agents leg closes it and drops the full flag.
pub(crate) fn cycle_sideline_view(view: &mut View) {
    match view.sideline_view {
        crate::view_store::SidelineView::Backlog => {
            view.backlog_board = None;
            view.board_full = false;
            crate::view_store::save_board_full(false);
            set_sideline_view(view, crate::view_store::SidelineView::Agents);
        }
        crate::view_store::SidelineView::Agents => {
            if !view.experimental_backlog {
                view.set_notice("experimental backlog view is off (sidebar menu)".into());
                return;
            }
            backlog_board_open_fresh(view);
            set_sideline_view(view, crate::view_store::SidelineView::Backlog);
        }
    }
}

/// A fresh board view at the next generation (the stale fold of a
/// previous open can never land).
fn backlog_board_open_fresh(view: &mut View) {
    let gen = view
        .backlog_board
        .as_ref()
        .map(|b| b.gen.wrapping_add(1))
        .unwrap_or(0);
    view.backlog_board = Some(BoardView::new(gen));
}

/// Point the sideline at `v`, persisting the choice.
pub(crate) fn set_sideline_view(view: &mut View, v: crate::view_store::SidelineView) {
    view.sideline_view = v;
    crate::view_store::save_sideline_view(v);
}

/// `F`: full-screen the board - the docked column and the centered overlay
/// both expand to the terminal - and back, persisting the choice.
fn toggle_full(view: &mut View) {
    view.board_full = !view.board_full;
    crate::view_store::save_board_full(view.board_full);
    let word = if view.board_full {
        "full screen"
    } else {
        "windowed"
    };
    view.set_notice(format!("board: {word}"));
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
    if b.colpick.is_some() {
        colpick_keys(view, bytes);
        return Ok(StdinFlow::Continue);
    }
    // A lone-Esc chunk closes at once (the old node-detail contract: the
    // modal fold would otherwise hold the byte pending a sequence).
    if bytes == [0x1b] && b.board_esc.is_empty() {
        if b.keys_overlay {
            b.keys_overlay = false;
        } else {
            esc_or_close(view);
        }
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
            ModalKey::Esc | ModalKey::Byte(b'q') => esc_or_close(view),
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
            ModalKey::Byte(b'N') => add_note(view)?,
            ModalKey::Byte(b'E') => edit_description(view).await?,
            ModalKey::Byte(b'c') => open_colpick(view),
            ModalKey::Byte(b'?') => open_keys_overlay(view),
            ModalKey::Byte(b'\t') => toggle_view(view),
            ModalKey::Byte(b'F') => toggle_full(view),
            ModalKey::Byte(b'T') => rank_move(view, "top", None)?,
            ModalKey::Byte(b'K') => rank_move(view, "before", Some(true))?,
            ModalKey::Byte(b'J') => rank_move(view, "after", Some(false))?,
            ModalKey::Enter => open_detail(view),
            _ => {}
        }
    }
    if let Some(b) = view.backlog_board.as_mut() {
        backlog_panes::sync_doc(b);
    }
    Ok(StdinFlow::Continue)
}

/// Close the board (and any drill-down inside it). The board is an
/// overlay, not a panel: no Resize travels.
fn close_board(view: &mut View) {
    view.backlog_board = None;
    set_sideline_view(view, crate::view_store::SidelineView::Agents);
}

/// Full-screen first folds back to the sideline column; the second Esc
/// closes the board and returns the sideline to the agents view.
fn esc_or_close(view: &mut View) {
    if view.board_full {
        view.board_full = false;
        crate::view_store::save_board_full(false);
        view.set_notice("board: column".into());
    } else {
        close_board(view);
    }
}

/// Move the card cursor within the cell (clamped to the painted cards);
/// in list mode the step walks the flat painted order, past a cell's last
/// card into the next non-empty column.
fn move_row(view: &mut View, down: bool) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    if b.query.view == backlog_model::View::List {
        move_row_list(b, down);
        return;
    }
    let Some(cell) = b
        .body
        .as_ref()
        .and_then(|bd| bd.lanes.get(b.lane))
        .and_then(|l| {
            let name = b.layout.columns.get(b.col)?;
            l.cells.iter().find(|c| &c.column == name)
        })
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

/// The list view's flat cursor walk: one card per (lane, column, row)
/// position in painted order, one step per press.
fn move_row_list(b: &mut BoardView, down: bool) {
    let Some(board) = b.body.as_ref() else {
        return;
    };
    let mut cells: Vec<(usize, usize, usize)> = Vec::new();
    for (li, lane) in board.lanes.iter().enumerate() {
        for si in 0..b.layout.columns.len() {
            let Some(cell) = lane.cells.iter().find(|c| {
                b.layout
                    .columns
                    .get(si)
                    .is_some_and(|name| &c.column == name)
            }) else {
                continue;
            };
            for ri in 0..cell.cards.len() {
                cells.push((li, si, ri));
            }
        }
    }
    if cells.is_empty() {
        return;
    }
    let cur = cells
        .iter()
        .position(|&(l, c, r)| l == b.lane && c == b.col && r == b.row);
    let next = match (cur, down) {
        (Some(i), true) => (i + 1).min(cells.len() - 1),
        (Some(i), false) => i.saturating_sub(1),
        (None, true) => cells.len() - 1,
        (None, false) => 0,
    };
    let (l, c, r) = cells[next];
    (b.lane, b.col, b.row) = (l, c, r);
}

/// Move the column cursor (clamped to the six cells); the row clamps at
/// render time. In list mode the columns are display sub-headers, so the
/// key does nothing.
fn move_col(view: &mut View, right: bool) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    if b.query.view == backlog_model::View::List || b.body.is_none() {
        return;
    }
    let shown = b.layout.columns.len().saturating_sub(1);
    b.col = if right {
        (b.col + 1).min(shown)
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

/// Tab: flip the board between the kanban grid and the uncapped list,
/// keeping the cursor on the card it left.
fn toggle_view(view: &mut View) {
    let focus = view.backlog_board.as_ref().and_then(cursor_card_id);
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    b.query.view = match b.query.view {
        backlog_model::View::Kanban => backlog_model::View::List,
        backlog_model::View::List => backlog_model::View::Kanban,
    };
    rederive(b);
    focus_card(b, focus.as_deref());
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
        BoardInputKind::Note => {
            if text.is_empty() {
                view.set_notice("note: nothing to add".into());
                return;
            }
            let Some(id) = edit_target(b) else {
                return;
            };
            let args: Vec<String> = vec!["backlog".into(), "note".into(), id, text];
            queue_write(b, WriteAction::Args(args, None));
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
            ModalKey::Byte(b' ') if b.facet.as_ref().is_some_and(|p| p.value_sel.is_some()) => {
                facet_toggle(view);
            }
            ModalKey::Enter => facet_commit(view),
            _ => {}
        }
    }
}

/// The fixed facet list, one row of the first-level popup.
pub(crate) const FACET_NAMES: [&str; 8] = [
    "project", "epic", "status", "type", "priority", "size", "king", "tag",
];

/// The facets this backend can answer and whose value list is not empty,
/// as `(FACET_NAMES index, name)`. A facet with no values (Labels while no
/// node carries a tag) hides itself.
fn visible_facets(board: &Board) -> Vec<(usize, &'static str)> {
    (0..FACET_NAMES.len())
        .filter(|&i| unavailable_reason(board, facet_feature(FACET_NAMES[i])).is_none())
        .filter(|&i| !facet_values(board, i).is_empty())
        .map(|i| (i, FACET_NAMES[i]))
        .collect()
}

/// The facet's current filter set, for the first-level row label.
fn facet_current(b: &BoardView, facet: usize) -> String {
    let name = FACET_NAMES.get(facet).copied().unwrap_or("");
    let vals = b.query.sets.get(name).map(|v| v.as_slice()).unwrap_or(&[]);
    if vals.is_empty() {
        "any".into()
    } else {
        vals.join(",")
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
            .kinds
            .iter()
            .map(|k| (k.clone(), k.clone()))
            .collect(),
        4 => board
            .facets
            .priorities
            .iter()
            .map(|p| (p.clone(), p.clone()))
            .collect(),
        5 => board
            .facets
            .sizes
            .iter()
            .map(|s| (s.clone(), s.clone()))
            .collect(),
        6 => board
            .facets
            .kings
            .iter()
            .map(|k| (k.clone(), k.clone()))
            .collect(),
        7 => board
            .facets
            .tags
            .iter()
            .map(|t| (t.clone(), t.clone()))
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
        let set: Vec<String> = b.query.sets.get(name).cloned().unwrap_or_default();
        for (label, value) in facet_values(board, pick.facet) {
            let mark = if set.iter().any(|v| *v == value) {
                "[x] "
            } else {
                "[ ] "
            };
            rows.push(pick_row(&format!("{mark}{label}")));
        }
        let mut popup = Popup::new(rows, Anchor::Center)
            .title(format!("filter: {name}"))
            .footer("space toggle - enter done - esc back");
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

/// Enter at the value level: toggle the row under the cursor, then
/// return to the facet list. At the facet list: descend.
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
    toggle_value(b, pick.facet, vsel);
    b.facet = Some(FacetPick {
        facet: pick.facet,
        sel: pick.sel,
        value_sel: None,
    });
}

/// Toggle one value row (0 = the `any` row, which clears the set) over
/// the facet's set, then re-derive with the cursor kept on its card.
fn toggle_value(b: &mut BoardView, facet: usize, vsel: usize) {
    let facet_name = FACET_NAMES.get(facet).copied().unwrap_or("");
    let values = b
        .body
        .as_ref()
        .map(|bd| facet_values(bd, facet))
        .unwrap_or_default();
    let value = match vsel {
        0 => None,
        n => values.get(n - 1).map(|(_, v)| v.clone()),
    };
    let set = b.query.sets.entry(facet_name).or_default();
    match value {
        None => set.clear(),
        Some(v) => match set.iter().position(|have| *have == v) {
            Some(i) => {
                set.remove(i);
            }
            None => set.push(v),
        },
    }
    if set.is_empty() {
        b.query.sets.remove(facet_name);
    }
    let focus = cursor_card_id(b);
    rederive(b);
    focus_card(b, focus.as_deref());
}

/// Space at the value level: toggle the row under the cursor and stay
/// open, so several boxes tick in one popup session.
fn facet_toggle(view: &mut View) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let Some(pick) = b.facet else {
        return;
    };
    let Some(vsel) = pick.value_sel else {
        return;
    };
    toggle_value(b, pick.facet, vsel);
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
            scroll: 0,
        });
    }
}

#[cfg(test)]
#[path = "tests/backlog_board_tests.rs"]
mod tests;

#[path = "backlog_panes.rs"]
pub(crate) mod backlog_panes;

/// `c`: the column picker (D4). Which columns show, their order, and the
/// focus width - persisted through the view store on every change.
fn open_colpick(view: &mut View) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    if b.colpick.is_some() {
        b.colpick = None;
        return;
    }
    b.colpick = Some(ColPick { sel: b.col });
}

/// The `c` picker's rows: every model column in stored order, a mark for
/// shown, and the focus-width line the +/- keys edit.
fn colpick_popup(b: &BoardView) -> Option<Popup> {
    let pick = b.colpick.as_ref()?;
    let board = b.body.as_ref()?;
    let mut rows: Vec<PopupRow> = vec![PopupRow::Header("columns".into()), PopupRow::Rule];
    for (i, name) in b.layout.columns.iter().enumerate() {
        let shown = board
            .lanes
            .first()
            .map(|l| l.cells.iter().any(|c| &c.column == name))
            .unwrap_or(false);
        let mark = if shown { "*" } else { "o" };
        rows.push(PopupRow::Entry {
            glyph: mark.into(),
            label: if i == b.col {
                format!("{name}  (focus)")
            } else {
                name.clone()
            },
            hint: String::new(),
            enabled: true,
        });
    }
    for cell in board
        .lanes
        .first()
        .map(|l| l.cells.as_slice())
        .unwrap_or(&[])
    {
        if !b.layout.columns.iter().any(|n| n == &cell.column) {
            rows.push(PopupRow::Entry {
                glyph: "o".into(),
                label: cell.column.to_string(),
                hint: String::new(),
                enabled: true,
            });
        }
    }
    let mut popup = Popup::new(rows, Anchor::Center)
        .title("board columns")
        .footer(format!(
            "enter show/hide - h/l order - +/- focus {}% - esc close",
            b.layout.focus_pct
        ));
    popup.sel = pick.sel;
    Some(popup)
}

/// The `c` picker's keys. Enter toggles a column in/out of the layout,
/// h/l reorders it, +/- moves the focus share, esc closes. Every accepted
/// edit saves through the view store.
fn colpick_keys(view: &mut View, bytes: &[u8]) {
    let toks = {
        let b = view.backlog_board.as_mut().expect("colpick open");
        let mut esc = std::mem::take(&mut b.board_esc);
        let toks = fold_modal_keys(&mut esc, bytes);
        b.board_esc = esc;
        toks
    };
    for tok in toks {
        let Some(b) = view.backlog_board.as_mut() else {
            break;
        };
        if b.colpick.is_none() {
            break;
        }
        let n = all_column_names(b).len();
        match tok {
            ModalKey::Esc => b.colpick = None,
            ModalKey::Up | ModalKey::Byte(b'k') => {
                let pick = b.colpick.as_mut().expect("colpick open");
                pick.sel = pick.sel.saturating_sub(1);
            }
            ModalKey::Down | ModalKey::Byte(b'j') => {
                let pick = b.colpick.as_mut().expect("colpick open");
                pick.sel = (pick.sel + 1).min(n.saturating_sub(1));
            }
            ModalKey::Enter => colpick_toggle(view),
            ModalKey::Left | ModalKey::Byte(b'h') => colpick_move(view, false),
            ModalKey::Right | ModalKey::Byte(b'l') => colpick_move(view, true),
            ModalKey::Byte(b'+') | ModalKey::Byte(b'=') => colpick_width(view, 10),
            ModalKey::Byte(b'-') => colpick_width(view, -10),
            _ => {}
        }
    }
}

/// Every model column name (the picker lists all of them, shown or not).
fn all_column_names(b: &BoardView) -> Vec<String> {
    let mut names: Vec<String> = b.layout.columns.clone();
    if let Some(board) = b.body.as_ref() {
        for lane in &board.lanes {
            for cell in &lane.cells {
                if !names.iter().any(|n| n == &cell.column) {
                    names.push(cell.column.to_string());
                }
            }
        }
    }
    if names.is_empty() {
        names = crate::backlog_view::KANBAN_COLUMNS
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    names
}

/// Enter in the picker: show/hide the cursor column. Hiding the focus
/// column moves the cursor to the next shown column.
fn colpick_toggle(view: &mut View) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let sel = b.colpick.as_ref().map(|p| p.sel).unwrap_or(0);
    let names = all_column_names(b);
    let Some(name) = names.get(sel).cloned() else {
        return;
    };
    if let Some(pos) = b.layout.columns.iter().position(|n| n == &name) {
        if b.layout.columns.len() <= 1 {
            view.set_notice("columns: at least one must show".into());
            return;
        }
        b.layout.columns.remove(pos);
        if b.col >= b.layout.columns.len() {
            b.col = b.layout.columns.len().saturating_sub(1);
        }
    } else {
        b.layout.columns.push(name);
    }
    crate::view_store::save_board_layout(&b.layout);
    rederive(b);
}

/// h/l in the picker: move the cursor column one slot in the order.
fn colpick_move(view: &mut View, right: bool) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let sel = b.colpick.as_ref().map(|p| p.sel).unwrap_or(0);
    let names = all_column_names(b);
    let Some(name) = names.get(sel).cloned() else {
        return;
    };
    let Some(pos) = b.layout.columns.iter().position(|n| n == &name) else {
        return;
    };
    let target = if right {
        pos + 1
    } else {
        pos.saturating_sub(1)
    };
    if target >= b.layout.columns.len() || target == pos {
        return;
    }
    b.layout.columns.swap(pos, target);
    if b.col == pos {
        b.col = target;
    } else if b.col == target {
        b.col = pos;
    }
    crate::view_store::save_board_layout(&b.layout);
}

/// +/- in the picker: the focus share, clamped 25..=75.
fn colpick_width(view: &mut View, delta: i16) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let pct = (b.layout.focus_pct as i16 + delta).clamp(25, 75) as u16;
    b.layout.focus_pct = pct;
    crate::view_store::save_board_layout(&b.layout);
}

/// `?` while the board or the drill-down owns the keyboard: the board's
/// own key table (the edit keys included, D6).
fn open_keys_overlay(view: &mut View) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    b.keys_overlay = !b.keys_overlay;
}

fn board_keys_popup() -> Popup {
    let rows: Vec<PopupRow> = vec![
        PopupRow::Header("read".into()),
        PopupRow::Rule,
        pick_row("hjkl move - [ ] lane - L lanes"),
        pick_row("/ find - f filter - r re-read"),
        pick_row("Tab list/kanban - space toggle (in f)"),
        pick_row("enter node detail - F full screen"),
        PopupRow::Header("edit".into()),
        PopupRow::Rule,
        pick_row("e title - p priority - s size - S status"),
        pick_row("D append details - N note - E edit description in $EDITOR"),
        PopupRow::Header("layout".into()),
        PopupRow::Rule,
        pick_row("c columns (show, order, focus width)"),
        PopupRow::Header("write".into()),
        PopupRow::Rule,
        pick_row("b blueprint - t target - A ask the king"),
        pick_row("T top rank - K before - J after"),
    ];
    Popup::new(rows, Anchor::Center)
        .title("backlog keys")
        .footer("esc close")
}

/// `N`: a note on the target node through `fno backlog note`.
pub(crate) fn add_note(view: &mut View) -> Result<(), String> {
    let Some(b) = view.backlog_board.as_mut() else {
        return Ok(());
    };
    if let Some(reason) = gated(b, unavailable_features::FIELD_EDITS) {
        view.set_notice(reason);
        return Ok(());
    }
    if edit_target(b).is_some() {
        b.input = Some((BoardInputKind::Note, String::new()));
    }
    Ok(())
}

/// `E`: the full description in $EDITOR. Suspends the mux around the child
/// (cooked mode, the primary screen), reads the result back, and queues
/// the write only when the text actually changed.
pub(crate) async fn edit_description(view: &mut View) -> Result<(), String> {
    let Some(id) = view.backlog_board.as_ref().and_then(|b| edit_target(b)) else {
        return Ok(());
    };
    if let Some(reason) = gated(
        view.backlog_board.as_ref().expect("board open"),
        unavailable_features::FIELD_EDITS,
    ) {
        view.set_notice(reason);
        return Ok(());
    }
    let graph = crate::backlog_view::graph_path();
    let id_for_read = id.clone();
    let text = tokio::task::spawn_blocking(move || {
        crate::store_client::node(&graph, &id_for_read)
            .ok()
            .flatten()
            .and_then(|n| {
                n.get("details")
                    .and_then(|d| d.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    let text_before = text.clone();
    let Some(edited) = tokio::task::spawn_blocking(move || run_editor(&text))
        .await
        .ok()
        .flatten()
    else {
        view.set_notice("editor: cancelled".into());
        return Ok(());
    };
    if edited.is_empty() || edited == text_before {
        view.set_notice("description: unchanged, nothing written".into());
        return Ok(());
    }
    let Some(b) = view.backlog_board.as_mut() else {
        return Ok(());
    };
    let args: Vec<String> = vec![
        "backlog".into(),
        "update".into(),
        id,
        "--details-file".into(),
        "-".into(),
    ];
    queue_write(b, WriteAction::Args(args, Some(edited)));
    Ok(())
}

/// Suspend the mux, run $EDITOR on `text`, restore the mux, return the
/// edited text. `None` when the editor failed or exited non-zero.
fn run_editor(text: &str) -> Option<String> {
    use crossterm::{cursor, execute, terminal};
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let dir = std::env::temp_dir().join("fno-board-edit");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("details-{}.md", std::process::id()));
    std::fs::write(&path, text).ok()?;
    let _ = execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = match std::process::Command::new(&editor).arg(&path).status() {
        Ok(status) => status,
        // The terminal is already suspended: restore it on THIS path too,
        // or the client keeps running cooked and unpainted.
        Err(_) => {
            let _ = terminal::enable_raw_mode();
            let _ = execute!(out, terminal::EnterAlternateScreen, cursor::Hide);
            return None;
        }
    };
    let _ = terminal::enable_raw_mode();
    let _ = execute!(out, terminal::EnterAlternateScreen, cursor::Hide);
    let edited = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    let _ = out.flush();
    match status.success() {
        true => edited,
        false => None,
    }
}
