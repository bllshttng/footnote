//! The backlog board's panes: one paint for both hosts (the full-screen
//! `draw_board` and the windowed sideline column). Regions, top to bottom:
//! the filter bar, the board pane and the detail pane side by side (or
//! stacked when narrow), and the wrapped hint. Focus reads on the region
//! title rows (`backlog_style::paint_title_row`); no region draws a frame.

use super::backlog_board::{cursor_card_id, filter_bar_lines, render, BoardView, PaneDoc};
use super::backlog_style::{self, BLine, BRole, BSeg};
use super::node_detail::{self, Sel};
use crate::chrome;
use crate::proto::{cell_flags, Cell, Color};
use crate::theme::Theme;

/// The three region rectangles one host rect lays out as. One home for the
/// geometry so painting and any hit testing read the same layout (`paint`
/// is the only painter today).
pub(crate) struct PaneRegions {
    pub(crate) bar: (usize, usize, usize, usize),
    pub(crate) board: (usize, usize, usize, usize),
    pub(crate) detail: (usize, usize, usize, usize),
}

/// The layout `paint` computes for a host rect: the filter bar, the board
/// and detail panes side by side at w >= 100 (stacked below it). `hint_h`
/// is the caller's wrapped hint height (see [`hint_lines`]).
pub(crate) fn regions(rect: (usize, usize, usize, usize), hint_h: usize) -> PaneRegions {
    let (top, left, h, w) = rect;
    // Region heights: the hint rows the text needs (capped by the caller),
    // the filter bar's two-row body, the rest split between the panes.
    let bar_h = 2.min(h.saturating_sub(hint_h));
    let panes_h = h.saturating_sub(bar_h + hint_h);
    let side_by_side = w >= 100;
    let board_w = if side_by_side { w * 55 / 100 } else { w };
    let detail_w = w.saturating_sub(board_w);
    let (board, detail) = if side_by_side {
        (
            (top + bar_h, left, panes_h, board_w),
            (top + bar_h, left + board_w, panes_h, detail_w),
        )
    } else {
        let half = panes_h / 2;
        (
            (top + bar_h, left, half, w),
            (top + bar_h + half, left, panes_h - half, w),
        )
    };
    PaneRegions {
        bar: (top, left, bar_h, w),
        board,
        detail,
    }
}

pub(crate) fn paint(
    b: &BoardView,
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    rect: (usize, usize, usize, usize),
    owner: bool,
    theme: &Theme,
) {
    let started = std::time::Instant::now();
    let (top, _left, h, w) = rect;
    if h == 0 || w == 0 {
        return;
    }
    // The board region holds the keyboard when the owner is the board; its
    // detail pane takes the focus presentation while a drill-down is open.
    let focus_pane = owner && b.detail.is_some();
    // The hint sheet is sized first: its wrapped height is the layout's
    // floor, and the cap keeps the panes their rows (change 4: nothing
    // drops off the bottom unseen).
    let hint = hint_lines(b, focus_pane, w);
    let hint_h = if h >= 6 { hint.len().min(4) } else { 0 };
    let laid_out = regions(rect, hint_h);
    let bar_h = laid_out.bar.2;
    // The filter bar: the capped two-row cell wrap, unframed, carrying the
    // board's one esc chip on its top-right (it owned the framed title row
    // before the frames left).
    let bar_body: Vec<BLine> = b
        .body
        .as_ref()
        .map(|board| filter_bar_lines(b, board, w))
        .unwrap_or_default();
    backlog_style::paint_panel(cells, rows, cols, top, w, bar_h, &bar_body, None, theme);
    paint_esc_chip(cells, rows, cols, top, _left, w, theme);
    let board_rect = laid_out.board;
    let detail_rect = laid_out.detail;
    // The board pane: the kanban render or the uncapped list, banded only
    // while the board owns typing. The memo (the frame-cost measurement)
    // still serves a recompose: it re-blits the cached lines instead of
    // re-rendering every card.
    let board_inner_w = board_rect.3;
    let bkey = crate::client::backlog_board::BodyKey {
        gen: b.body_gen,
        lane: b.lane,
        col: b.col,
        row: b.row,
        w: board_inner_w,
        list: b.query.view == crate::backlog_model::View::List,
        query: b.query.q.clone(),
        errors: b.errors.len(),
        columns: b.layout.columns.clone(),
    };
    let follow = b.board_body_cached(bkey, || {
        if b.query.view == crate::backlog_model::View::List {
            list_lines(b, board_inner_w)
        } else {
            render(b, board_inner_w)
        }
    });
    // The board pane wears the cursor band while the board owns the
    // keyboard and no drill-down took it; an inactive board keeps its plain
    // selection glyph, never the band. The band rides the cached BLine
    // (line.band), so an unowned board strips it before the panel paint.
    let band_on = owner && !focus_pane;
    backlog_style::paint_title_row(
        cells,
        rows,
        cols,
        board_rect.0,
        board_rect.1,
        board_rect.3,
        &format!(
            "backlog \u{b7} {}",
            if b.query.view == crate::backlog_model::View::List {
                "list"
            } else {
                "kanban"
            }
        ),
        band_on,
        theme,
    );
    let mut board_lines = b.board_lines_cached();
    if !band_on {
        for l in &mut board_lines {
            l.band = false;
        }
    }
    backlog_style::paint_panel(
        cells,
        rows,
        cols,
        board_rect.0 + 1,
        board_rect.3,
        board_rect.2.saturating_sub(1),
        &board_lines,
        follow,
        theme,
    );
    // The detail pane: the focused node, else the cursor card's node,
    // scrolled by the pane's scroll offset.
    let node = b
        .detail
        .as_ref()
        .map(|d| d.node_id.clone())
        .or_else(|| cursor_card_id(b))
        .unwrap_or_default();
    let detail_inner_w = detail_rect.3;
    let dkey = crate::client::backlog_board::DetailKey {
        gen: b.body_gen,
        node: node.clone(),
        sel: b.detail.as_ref().map(|d| d.sel),
        w: detail_inner_w,
        doc: b
            .doc
            .as_ref()
            .map(|d| (d.node_id.clone(), d.path.clone(), d.mtime, d.error.clone())),
    };
    let is_empty_node = node.is_empty();
    let dfollow_pre = b.detail_lines_cached(dkey, || {
        if is_empty_node {
            return (vec![BLine::meta("no card under the cursor")], None);
        }
        let sel = b.detail.as_ref().map(|d| d.sel);
        node_detail::pane_lines(b, &node, sel, detail_inner_w)
    });
    // The scroll rides after the memo read (a skip over the cached lines).
    let scroll = b.detail.as_ref().map(|d| d.scroll).unwrap_or(0);
    let dfollow_pre = dfollow_pre.map(|i| i.saturating_sub(scroll));
    // The detail pane wears its follow line only while it holds focus.
    let dfollow = if focus_pane { dfollow_pre } else { None };
    backlog_style::paint_title_row(
        cells,
        rows,
        cols,
        detail_rect.0,
        detail_rect.1,
        detail_rect.3,
        &format!("details \u{b7} {node}"),
        focus_pane,
        theme,
    );
    let mut dlines = b.detail_lines_raw();
    if !focus_pane {
        for l in &mut dlines {
            l.band = false;
        }
    }
    let dlines = &dlines[scroll.min(dlines.len())..];
    backlog_style::paint_panel_at(
        cells,
        rows,
        cols,
        detail_rect.1,
        detail_rect.0 + 1,
        detail_rect.3,
        detail_rect.2.saturating_sub(1),
        dlines,
        dfollow,
        theme,
    );
    // The hint: the wrapped sheet, painted once more in the new key-role
    // style the hint_lines builder produced.
    let hint_top = top + h - hint_h;
    backlog_style::paint_panel(cells, rows, cols, hint_top, w, hint_h, &hint, None, theme);
    let micros = started.elapsed().as_micros();
    b.record_paint(micros);
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn framed_region(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    rect: (usize, usize, usize, usize),
    chrome: &chrome::Chrome,
    body: &[chrome::BodyLine],
    follow: Option<usize>,
    band: Option<usize>,
    theme: &Theme,
) {
    let (top, left, h, w) = rect;
    if h == 0 || w == 0 {
        return;
    }
    let layout = overlay_paint::layout_body_overlay(
        (top, left),
        (h, w),
        chrome,
        &body,
        follow,
        overlay_paint::OverlayAnchor::At {
            row: top,
            col: left,
        },
    );
    overlay_paint::draw_overlay_layout(cells, rows, cols, &layout, theme);
    if let Some(band) = band {
        let (start, take) = layout.window;
        if band >= start && band < start + take {
            backlog_style::paint_framed_band(
                cells,
                rows,
                cols,
                layout.origin,
                layout.framed.width,
                layout.body_top + (band - start),
                theme,
            );
        }
    }
}

use super::overlay_paint;

/// The board's one esc chip, top-right of the filter bar row. Painted text
/// plus the recorded hit span the tap gestures read; the framed chrome used
/// to own this, and the chip outlives the frames.
pub(crate) fn paint_esc_chip(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    row: usize,
    left: usize,
    w: usize,
    theme: &Theme,
) {
    let chip = " esc ";
    if row >= rows || w <= chip.len() {
        return;
    }
    let c0 = left + w - chip.len();
    for (i, ch) in chip.chars().enumerate() {
        if c0 + i < cols {
            cells[row * cols + c0 + i] = Cell {
                c: ch,
                fg: theme.brand,
                bg: Color::Default,
                flags: cell_flags::BOLD,
            };
        }
    }
    crate::chrome::record_close_spans(
        (rows, cols),
        (row, left),
        w,
        &[(crate::chrome::ESC_CLOSE_HIT, w - 4, 3)],
    );
}
/// The shown node's cached markdown document read: refreshed only when
/// the shown node, the path or the mtime changed.
pub(crate) fn sync_doc(b: &mut BoardView) {
    // The shown node: the focused pane's node, else the cursor card.
    let id = match b.detail.as_ref() {
        Some(d) => d.node_id.clone(),
        None => match cursor_card_id(b) {
            Some(id) => id,
            None => {
                b.doc = None;
                return;
            }
        },
    };
    let Some(inputs) = b.inputs.as_ref() else {
        return;
    };
    let Some(nv) = crate::backlog_model::node(inputs, &id) else {
        b.doc = None;
        return;
    };
    let Some(path) = nv.plan_path.clone() else {
        b.doc = None;
        return;
    };
    // A relative plan path joins the node's cwd.
    let p = std::path::Path::new(&path);
    let full = if p.is_absolute() {
        std::path::PathBuf::from(&path)
    } else {
        nv.cwd
            .as_deref()
            .map(|cwd| std::path::PathBuf::from(cwd).join(&path))
            .unwrap_or_else(|| std::path::PathBuf::from(&path))
    };
    let mtime = std::fs::metadata(&full).and_then(|m| m.modified()).ok();
    let unchanged = b
        .doc
        .as_ref()
        .is_some_and(|d| d.node_id == id && d.path == path && d.mtime == mtime);
    if unchanged {
        return;
    }
    let big_cap: u64 = 256 * 1024;
    let (lines_src, error) = match (std::fs::metadata(&full), std::fs::read(&full)) {
        (Ok(m), Ok(bytes)) if m.len() <= big_cap => {
            (String::from_utf8_lossy(&bytes).into_owned(), String::new())
        }
        (Ok(_m), Ok(_)) => (String::new(), format!("file larger than {big_cap} bytes")),
        (Ok(_), Err(e)) | (Err(e), _) => (String::new(), e.to_string()),
    };
    b.doc = Some(PaneDoc {
        node_id: id,
        path,
        mtime,
        lines_src,
        error,
    });
}

/// The list view's flat body: each lane's shown cells in column order as
/// one list, a dim sub-header per column, one row per card. The follow
/// index tracks the cursor card.
pub(crate) fn list_lines(b: &BoardView, w: usize) -> (Vec<BLine>, Option<usize>) {
    let Some(board) = b.body.as_ref() else {
        return (vec![BLine::plain("reading board...")], None);
    };
    let mut lines: Vec<BLine> = Vec::new();
    let mut follow: Option<usize> = None;
    for (li, lane) in board.lanes.iter().enumerate() {
        if board.lanes.len() > 1 {
            lines.push(BLine::head(trunc_line(&format!("{}", lane.title), w)));
        }
        for name in b.layout.columns.iter() {
            let Some(cell) = lane.cells.iter().find(|c| &c.column == name) else {
                continue;
            };
            if cell.cards.is_empty() {
                continue;
            }
            lines.push(BLine::meta(trunc_line(&cell.column.to_string(), w)));
            for (ri, card) in cell.cards.iter().enumerate() {
                let sel = li == b.lane
                    && *name
                        == b.layout
                            .columns
                            .get(b.col)
                            .map(|s| s.as_str())
                            .unwrap_or("")
                    && ri == b.row;
                let glyph = if card.claimed {
                    "\u{25cf}"
                } else if card.blocked {
                    "\u{2298}"
                } else {
                    " "
                };
                let mark = if sel { "\u{25b8}" } else { " " };
                let mut line = BLine::of(&[
                    BSeg {
                        text: format!("{mark}{glyph} "),
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
                lines.push(line.trunc(w));
                if sel {
                    follow = Some(lines.len() - 1);
                }
            }
        }
    }
    (lines, follow)
}

/// Truncate one line to `w` chars.
fn trunc_line(s: &str, w: usize) -> String {
    s.chars().take(w).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC7: a hint wider than the width wraps, every word placed.
    #[test]
    fn hint_wraps_every_word() {
        let rows = hint_rows("alpha beta gamma delta", 12);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows.join(" "), "alpha beta gamma delta");
    }

    // AC7: no word is ever cut - wrap() breaks only at spaces - and no
    // row outgrows the width.
    #[test]
    fn hint_keeps_whole_words() {
        let long = "word ".repeat(60);
        let rows = hint_rows(long.trim(), 20);
        assert!(
            rows.iter().all(|r| r.split(' ').all(|w| w == "word")),
            "no partial word survives: {rows:?}"
        );
        assert!(rows.iter().all(|r| r.chars().count() <= 20));
    }

    // AC6-HP list half: the flat body lists every card under its column
    // sub-header, and the cursor card is the follow line.
    #[test]
    fn list_body_lists_cards_under_column_headers() {
        let rows = vec![
            serde_json::json!({"id": "x-1", "status": "ready", "priority": "p2"}),
            serde_json::json!({"id": "x-2", "status": "in_progress", "priority": "p2"}),
        ];
        let mut b = BoardView::new(0);
        b.inputs = Some(crate::backlog_model::fixture(rows));
        let q = b.query.to_query().expect("the default query parses");
        b.body = Some(crate::backlog_model::board(b.inputs.as_ref().unwrap(), &q));
        b.query.view = crate::backlog_model::View::List;
        let (lines, follow) = list_lines(&b, 120);
        assert!(lines.iter().any(|l| l.contains("x-1")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("x-2")));
        assert!(follow.is_some(), "the cursor card is the follow line");
    }
}

/// The two-row hint: words wrap onto two rows; a word that fits in neither
/// drops whole, no marker.
pub(crate) fn hint_rows(text: &str, w: usize) -> Vec<String> {
    BLine::meta(text)
        .wrap(w.max(1))
        .iter()
        .map(|l| l.text.trim_end().to_string())
        .collect()
}

/// The detail hint's `enter <word>`: the selected row's action, named the
/// way the footer says it. A link opens; a session names its action; a
/// dead session names its reason (AC6: the footer says why it does
/// nothing).
fn enter_word(b: &BoardView) -> String {
    let Some(d) = b.detail.as_ref() else {
        return "details".into();
    };
    let Some(inputs) = b.inputs.as_ref() else {
        return "details".into();
    };
    let Some(nv) = crate::backlog_model::node(inputs, &d.node_id) else {
        return "details".into();
    };
    let sels = node_detail::sel_list(&nv);
    match sels.get(d.sel.min(sels.len().saturating_sub(1))) {
        Some(Sel::Link(_)) => "open".into(),
        Some(Sel::Session(i)) => {
            let Some(s) = nv.sessions.get(*i) else {
                return "none".into();
            };
            match s.action.as_str() {
                "attach" => "attach".into(),
                "resume" => "resume".into(),
                _ => s.reason.clone().unwrap_or_else(|| "none".into()),
            }
        }
        None => "details".into(),
    }
}

/// The hint sheet: the key list for the board or the focused details.
/// Every word lands (nothing drops off the bottom unseen); a sheet longer
/// than 4 rows ends its last row in `? keys`, the full keys overlay one
/// press away. Key words read bold, the text plain - nothing dims.
pub(crate) fn hint_lines(b: &BoardView, detail_focus: bool, w: usize) -> Vec<BLine> {
    let text = if detail_focus {
        format!(
            "j/k scroll \u{b7} tab link \u{b7} enter {} \u{b7} y copy id \u{b7} Y copy cmd \u{b7} PgUp/PgDn scroll \u{b7} esc board \u{b7} b blueprint \u{b7} t target \u{b7} A lead \u{b7} c comment \u{b7} ? keys",
            enter_word(b)
        )
    } else {
        "hjkl move \u{b7} [ ] lane \u{b7} L lanes \u{b7} Tab list/kanban \u{b7} / search \u{b7} f filter \u{b7} enter details \u{b7} c cols \u{b7} b blueprint \u{b7} t target \u{b7} A lead \u{b7} T/K/J rank \u{b7} F full \u{b7} esc close \u{b7} ? keys"
        .to_string()
    };
    let mut sheet = BLine {
        text: String::new(),
        roles: Vec::new(),
        default_role: BRole::Body,
        band: false,
    };
    for (i, part) in text.split(" \u{b7} ").enumerate() {
        if i > 0 {
            sheet.push_line(BLine::plain(" \u{b7} "));
        }
        match part.split_once(' ') {
            Some((k, rest)) => {
                sheet.push_line(BLine::head(k));
                sheet.push_line(BLine::plain(format!(" {rest}")));
            }
            None => sheet.push_line(BLine::head(part)),
        }
    }
    let mut rows: Vec<BLine> = sheet.wrap(w.max(1));
    const HINT_MAX_ROWS: usize = 4;
    if rows.len() > HINT_MAX_ROWS {
        let keep = w.saturating_sub(7);
        let last = &mut rows[HINT_MAX_ROWS - 1];
        *last = BLine::plain(format!(
            "{} ? keys",
            last.text.chars().take(keep).collect::<String>()
        ));
        rows.truncate(HINT_MAX_ROWS);
    }
    rows
}
