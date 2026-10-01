//! The backlog board's framed panes: one paint for both hosts (the
//! full-screen `draw_board` and the windowed sideline column). Regions,
//! top to bottom: the filter bar, the board pane and the detail pane side
//! by side (or stacked when narrow), and the two-row hint.

use super::backlog_board::{cursor_card_id, filter_bar_lines, render, BoardView, PaneDoc};
use super::backlog_style::{self, BLine, BRole, BSeg};
use super::node_detail;
use crate::chrome;
use crate::proto::Cell;
use crate::theme::Theme;

/// The three region rectangles one host rect lays out as, plus whether the
/// panes carry frames. One home for the geometry so painting and any hit
/// testing read the same layout (`paint` is the only painter today).
pub(crate) struct PaneRegions {
    pub(crate) bar: (usize, usize, usize, usize),
    pub(crate) board: (usize, usize, usize, usize),
    pub(crate) detail: (usize, usize, usize, usize),
    pub(crate) framed: bool,
}

/// The layout `paint` computes for a host rect: the filter bar, the board
/// and detail panes side by side at w >= 100 (stacked below it), under the
/// same region heights paint has always used.
pub(crate) fn regions(rect: (usize, usize, usize, usize)) -> PaneRegions {
    let (top, left, h, w) = rect;
    // Region heights: hint 2 rows, the filter bar framed around its 2-row
    // body, the rest split between the panes.
    let hint_h = if h >= 6 { 2 } else { 0 };
    let bar_h = 4.min(h.saturating_sub(hint_h));
    let panes_h = h.saturating_sub(bar_h + hint_h);
    // Side by side at w >= 100 when the panes keep their frames; under 16
    // rows they drop their frames so each keeps at least 3 body rows; the
    // unframed painter has no left offset, so a short-wide rect stacks too.
    let framed = panes_h >= 10;
    let side_by_side = w >= 100 && framed;
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
        framed,
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
    let hint_h = if h >= 6 { 2 } else { 0 };
    let laid_out = regions(rect);
    let bar_h = laid_out.bar.2;
    let panes_h = h.saturating_sub(bar_h + hint_h);
    if panes_h == 0 {
        return;
    }
    // The board region holds the keyboard when the owner is the board; its
    // detail pane takes the focus presentation while a drill-down is open.
    let focus_pane = owner && b.detail.is_some();
    // The filter bar: framed around the capped two-row cell wrap.
    let bar_body: Vec<chrome::BodyLine> = b
        .body
        .as_ref()
        .map(|board| filter_bar_lines(b, board, w.saturating_sub(2)))
        .unwrap_or_default()
        .iter()
        .map(|l| {
            let mut line = l.clone();
            line = line.pad_to(w.saturating_sub(2));
            backlog_style::to_body_line(&line)
        })
        .collect();
    // The filter bar spans the top row, so it holds the board's one esc
    // chip; the board and details panes below it carry none.
    let bar_chrome = chrome::Chrome::new("filters", crate::popup::Anchor::Center).flat();
    framed_region(
        cells,
        rows,
        cols,
        laid_out.bar,
        &bar_chrome,
        &bar_body,
        None,
        None,
        theme,
    );
    let framed = laid_out.framed;
    let board_rect = laid_out.board;
    let detail_rect = laid_out.detail;
    // The board pane: the kanban render or the uncapped list, banded only
    // while the pane holds focus. Both shapes go through the paint memo
    // (the frame-cost measurement): a frame recompose re-blits the cached
    // lines instead of re-rendering every card.
    let board_inner_w = if framed {
        board_rect.3.saturating_sub(chrome::Chrome::FRAME_COLS)
    } else {
        board_rect.3
    };
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
    let (board_body_ref, f2) = b.board_body_cached(bkey, || {
        let (lines, follow) = if b.query.view == crate::backlog_model::View::List {
            list_lines(b, board_inner_w)
        } else {
            render(b, board_inner_w)
        };
        let body = lines
            .iter()
            .map(|l| backlog_style::to_body_line(&l.clone().pad_to(board_inner_w)))
            .collect();
        (lines, body, follow)
    });
    let follow = f2;
    // The board pane wears the cursor band while the board owns the
    // keyboard and no drill-down took it; an inactive board keeps its plain
    // selection glyph, never the band.
    let band = if owner && !focus_pane { follow } else { None };
    let board_chrome = chrome::Chrome::new("backlog", crate::popup::Anchor::Center)
        .tabs(vec![
            (
                "kanban".to_string(),
                b.query.view == crate::backlog_model::View::Kanban,
            ),
            (
                "list".to_string(),
                b.query.view == crate::backlog_model::View::List,
            ),
        ])
        .flat()
        .without_close();
    if framed {
        framed_region(
            cells,
            rows,
            cols,
            board_rect,
            &board_chrome,
            &board_body_ref,
            follow,
            band,
            theme,
        );
    } else {
        let board_lines = b.board_lines_cached();
        backlog_style::paint_panel(
            cells,
            rows,
            cols,
            board_rect.0,
            board_rect.3,
            board_rect.2,
            &board_lines,
            follow,
            theme,
        );
    }
    // The detail pane: the focused node, else the cursor card's node,
    // scrolled by the pane's scroll offset.
    let node = b
        .detail
        .as_ref()
        .map(|d| d.node_id.clone())
        .or_else(|| cursor_card_id(b))
        .unwrap_or_default();
    let detail_inner_w = if framed {
        detail_rect.3.saturating_sub(chrome::Chrome::FRAME_COLS)
    } else {
        detail_rect.3
    };
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
    let (detail_body_ref, dfollow_pre) = b.detail_lines_cached(dkey, || {
        if is_empty_node {
            return (
                vec![BLine::meta("no card under the cursor")],
                Vec::new(),
                None,
            );
        }
        let sel = b.detail.as_ref().map(|d| d.sel);
        let (ls, f) = node_detail::pane_lines(b, &node, sel, detail_inner_w);
        let body = ls
            .iter()
            .map(|l| backlog_style::to_body_line(&l.clone().pad_to(detail_inner_w)))
            .collect();
        (ls, body, f)
    });
    // The scroll rides after the memo read (a skip over the cached lines).
    let scroll = b.detail.as_ref().map(|d| d.scroll).unwrap_or(0);
    let dfollow_pre = dfollow_pre.map(|i| i.saturating_sub(scroll));
    // The detail pane wears its follow line only while it holds focus.
    let dfollow = if focus_pane { dfollow_pre } else { None };
    let detail_chrome = chrome::Chrome::new(
        format!("details \u{b7} {node}"),
        crate::popup::Anchor::Center,
    )
    .flat()
    .without_close();
    if framed {
        framed_region(
            cells,
            rows,
            cols,
            detail_rect,
            &detail_chrome,
            &detail_body_ref[scroll.min(detail_body_ref.len())..],
            dfollow,
            dfollow,
            theme,
        );
    } else {
        let dlines = b.detail_lines_raw();
        let dlines = &dlines[scroll.min(dlines.len())..];
        backlog_style::paint_panel(
            cells,
            rows,
            cols,
            detail_rect.0,
            detail_rect.3,
            detail_rect.2,
            dlines,
            dfollow,
            theme,
        );
    }
    // The hint bar: two unframed rows of the wrapped hint text.
    let hint_top = top + h - hint_h;
    let hint = if focus_pane {
        "j/k link · enter open · PgUp/PgDn scroll · esc board · e/p/s/S edit · D append · N note · E editor · b blueprint · t target · A king · T/K/J rank · c cols · F full · ? keys"
    } else {
        "hjkl move · [ ] lane · L lanes · Tab list/kanban · / search · f filter · enter details · e/p/s/S edit · D append · N note · E editor · b blueprint · t target · A king · T/K/J rank · c cols · F full · ? keys"
    };
    let [a, b2] = hint_rows(hint, w);
    let hint_lines = [BLine::meta(a), BLine::meta(b2)];
    backlog_style::paint_panel(
        cells,
        rows,
        cols,
        hint_top,
        w,
        hint_h,
        &hint_lines,
        None,
        theme,
    );
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

    // AC13-HP: a hint wider than the width fills two rows.
    #[test]
    fn hint_wraps_to_two_rows() {
        let [a, b] = hint_rows("alpha beta gamma delta", 12);
        assert!(!a.is_empty() && !b.is_empty(), "{a:?} {b:?}");
        assert_eq!(hint_rows("tiny", 40), ["tiny".to_string(), String::new()]);
    }

    // AC13-HP: the ellipsis lands only when two rows still cannot hold it.
    #[test]
    fn hint_ellipsizes_when_two_rows_cannot_hold_it() {
        let long = "word ".repeat(60);
        let [a, b] = hint_rows(long.trim(), 20);
        assert!(b.ends_with('\u{2026}'), "{b:?}");
        assert!(a.chars().count() <= 20);
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

/// The two-row hint: words wrap onto two rows; row two ends with an
/// ellipsis only when two rows cannot hold the hint.
pub(crate) fn hint_rows(text: &str, w: usize) -> [String; 2] {
    if w == 0 {
        return [String::new(), String::new()];
    }
    let mut rows = [String::new(), String::new()];
    let mut row = 0usize;
    let mut used = 0usize;
    for word in text.split(' ') {
        let cw = word.chars().count();
        if used + cw > w {
            if row == 0 {
                row = 1;
                used = 0;
            } else {
                // Two rows cannot hold the hint: mark the cut.
                rows[1].push_str(" \u{2026}");
                break;
            }
        }
        if used > 0 {
            rows[row].push(' ');
            used += 1;
        }
        rows[row].push_str(word);
        used += cw;
    }
    rows
}
