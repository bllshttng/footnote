//! Pure pane-frame geometry and edge layout: which cells of a pane's rect are
//! border and which are content, and what each frame edge carries at a given
//! width. The name avoids "frame", which already means a pane's grid snapshot
//! (`Frame`, `pane_frame_flow_tests.rs`).
//!
//! A framed pane lives inside its own layout rect: the rect's outer ring is the
//! border, the pty is sized to the inner rect, and today's 1-cell tree divider
//! between neighbouring rects becomes the gap that makes each pane read as its
//! own box. Corners are rounded (`╭ ╮ ╰ ╯`), from the same vocabulary chrome.rs
//! hands its overlays (which keep their square corners via caller-passed
//! corner chars).

use crate::chrome::{char_cols, fit_ellipsis};
use crate::tree::{Rect, MIN_ROWS};

/// A pane frames itself at this width: the grip takes the centre 3 cells, so
/// at 20 columns each half of the top edge still holds a 2-char name plus a
/// status glyph. Below it, today's divider render stays.
pub const FRAME_MIN_COLS: u16 = 20;
/// Frame rows: the border costs 2, and [`MIN_ROWS`] is the smallest usable
/// content.
pub const FRAME_MIN_ROWS: u16 = MIN_ROWS + 2;

/// Whether a pane of this rect wears a frame.
pub fn framed(r: Rect) -> bool {
    r.cols >= FRAME_MIN_COLS && r.rows >= FRAME_MIN_ROWS
}

/// The rect a framed pane's pty and content occupy: inset by the border ring.
/// An unframed pane's content is its whole rect.
pub fn content_rect(r: Rect) -> Rect {
    if framed(r) {
        Rect {
            x: r.x + 1,
            y: r.y + 1,
            rows: r.rows - 2,
            cols: r.cols - 2,
        }
    } else {
        r
    }
}

/// Whether a content-area point sits on `r`'s border ring (framed panes only).
pub fn on_border(r: Rect, cr: u16, cc: u16) -> bool {
    framed(r)
        && cr >= r.y
        && cr < r.y + r.rows
        && cc >= r.x
        && cc < r.x + r.cols
        && (cr == r.y || cr == r.y + r.rows - 1 || cc == r.x || cc == r.x + r.cols - 1)
}

/// What one border cell is painting. Plain data: the client paints, this
/// module only names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// Rules, corners, gaps and separator spaces.
    Border,
    /// The pane's name inside the top-left tab.
    Name,
    /// The focused tab's `▐`/`▌` caps (absent when unfocused).
    Cap,
    /// The status glyph (`●`, `▲`, ...).
    Glyph,
    /// The status word (`Work`, `Input`, ...).
    Word,
    /// The model span (`· opus-5`).
    Model,
    /// The node on the bottom edge.
    Node,
    /// The branch on the bottom edge.
    Branch,
    /// The context-used string on the bottom edge.
    Ctx,
}

/// The fields an edge may carry, plain data with no styling. A missing field
/// (`None`) takes no cells and is skipped in the drop order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeFields<'a> {
    pub name: &'a str,
    /// `(glyph, word)` from the sideline's lattice vocabulary.
    pub status: Option<(char, &'a str)>,
    pub model: Option<&'a str>,
    pub node: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub ctx: Option<&'a str>,
}

/// One pane's two edges: full rows (corners included), each exactly
/// `r.cols` display columns wide, plus the drop step each edge landed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edges {
    pub top: Vec<(char, Part)>,
    pub bottom: Vec<(char, Part)>,
    pub top_step: usize,
    pub bottom_step: usize,
}

fn cols_of(row: &[(char, Part)]) -> usize {
    row.iter().map(|(c, _)| char_cols(*c)).sum()
}

fn seg(row: &mut Vec<(char, Part)>, s: &str, p: Part) {
    for ch in s.chars() {
        row.push((ch, p));
    }
}

/// Cut a span to `max_cols` display columns, ending in `…` (keeping the part
/// of the first dropped char) when anything was cut.
fn fit_row(row: &[(char, Part)], max_cols: usize) -> Vec<(char, Part)> {
    if cols_of(row) <= max_cols {
        return row.to_vec();
    }
    let keep = max_cols.saturating_sub(1);
    let mut out = Vec::new();
    let mut used = 0;
    for (c, p) in row {
        if used + char_cols(*c) > keep {
            out.push(('…', *p));
            return out;
        }
        out.push((*c, *p));
        used += char_cols(*c);
    }
    out
}

/// The top-right status span per drop step: 0 ` ● Work · opus-5 `, 3 (model
/// dropped) ` ● Work `, 4 (word dropped) ` ● `. No status, no span.
fn top_right(f: &EdgeFields, step: usize) -> Vec<(char, Part)> {
    let Some((g, word)) = f.status else {
        return Vec::new();
    };
    let mut row = vec![(' ', Part::Border), (g, Part::Glyph)];
    if step < 4 {
        seg(&mut row, &format!(" {word} "), Part::Word);
        if step == 0 {
            if let Some(m) = f.model {
                seg(&mut row, &format!("· {m} "), Part::Model);
            }
        }
    }
    row
}

/// The bottom-left node span: ` node7 · main `. Step 2 drops the branch.
fn bottom_left(f: &EdgeFields, step: usize) -> Vec<(char, Part)> {
    let Some(node) = f.node else {
        return Vec::new();
    };
    let mut row = vec![(' ', Part::Border)];
    seg(&mut row, node, Part::Node);
    if step < 2 {
        if let Some(b) = f.branch {
            seg(&mut row, &format!(" · {b} "), Part::Branch);
        }
    }
    row.push((' ', Part::Border));
    row
}

/// The bottom-right ctx span: ` ctx 49% `, then one fill cell before the
/// corner (the mock's `─╯`).
fn bottom_right(f: &EdgeFields) -> Vec<(char, Part)> {
    let mut row = Vec::new();
    if let Some(c) = f.ctx {
        seg(&mut row, &format!(" ctx {c} "), Part::Ctx);
        row.push(('─', Part::Border));
    }
    row
}

/// The name tab: `▐ name ▌` focused, `─ name ─` otherwise, both the same
/// width so focus never moves the drop step or the grip.
fn tab(name: &str, focused: bool) -> Vec<(char, Part)> {
    let mut row = Vec::new();
    row.push((
        if focused { '▐' } else { '─' },
        if focused { Part::Cap } else { Part::Border },
    ));
    row.push((' ', Part::Border));
    seg(&mut row, name, Part::Name);
    row.push((' ', Part::Border));
    row.push((
        if focused { '▌' } else { '─' },
        if focused { Part::Cap } else { Part::Border },
    ));
    row
}

/// Lay out both edges for one pane. `grip` marks a multi-pane tab whose
/// 3-cell grip owns the top edge's centre; it splits the top edge's free
/// cells into a left zone (the name tab) and a right zone (the status), each
/// one cell away from the grip. A lone pane has no grip and shares the inner
/// width, keeping at least one fill cell between tab and status.
///
/// Drop order (operator's): the top edge walks 0 (everything), 3 (drop the
/// model), 4 (drop the word; the glyph stays), 5 (ellipsize the name); the
/// bottom edge walks 0, 1 (drop ctx), 2 (drop the branch), 5 (ellipsize the
/// node). Each edge stops at the first step that fits; a narrow top edge
/// never costs the bottom edge its ctx.
pub fn edges(f: &EdgeFields, r: Rect, grip: bool, focused: bool) -> Edges {
    let w = r.cols as usize;
    let inner = w - 2; // between the corners
                       // Grip placement mirrors client grip_span: the centre 3 cells.
    let g0 = grip.then(|| (inner - 3) / 2); // inner-relative

    // -- top edge -----------------------------------------------------
    // Zone widths (inner-relative): with a grip, [1, g0) and [g0+4, inner);
    // without, the whole inner with >= 1 fill between the spans.
    let avail_right = match g0 {
        Some(g) => inner - (g + 4),
        None => inner,
    };
    let mut top_step = 0;
    let mut right = top_right(f, 0);
    // The full tab (the whole name) claims its cells first; the right span
    // drops through its steps only while the full tab still fits beside it.
    // When nothing fits beside the full tab, the bare glyph stays and the
    // name cut (the plan's step 5) is the last resort.
    let name_cols = crate::chrome::str_cols(f.name);
    let tab_full = name_cols + 4;
    let mut found = false;
    for step in [0usize, 3, 4] {
        let cand = top_right(f, step);
        let fits = match g0 {
            Some(_) => cols_of(&cand) <= avail_right,
            None => cols_of(&cand) + usize::from(!cand.is_empty()) + tab_full <= inner,
        };
        if fits {
            top_step = step;
            right = cand;
            found = true;
            break;
        }
    }
    if !found {
        top_step = 4;
        right = top_right(f, 4);
    }
    let tab_room = match g0 {
        Some(g) => g - 1,
        None => inner - cols_of(&right) - usize::from(!right.is_empty()),
    };
    // The tab costs 4 around the name (caps + spaces); the name itself gets
    // the rest, down to 1 char + ellipsis.
    let name_max = tab_room.saturating_sub(4).max(1);
    let top = {
        let mut row = vec![('╭', Part::Border)];
        row.extend(tab(&fit_ellipsis(f.name, name_max), focused));
        if let Some(g) = g0 {
            let used = cols_of(&row) - 1;
            for _ in used..g - 1 {
                row.push(('─', Part::Border));
            }
            // the grip's cells and their one-cell shoulders
            for _ in g - 1..g + 4 {
                row.push(('─', Part::Border));
            }
            let used = cols_of(&row) - 1;
            for _ in used..inner - cols_of(&right) {
                row.push(('─', Part::Border));
            }
            row.extend(right);
        } else {
            let used = cols_of(&row) - 1;
            for _ in used..inner - cols_of(&right) {
                row.push(('─', Part::Border));
            }
            row.extend(right);
        }
        row.push(('╮', Part::Border));
        row
    };

    // -- bottom edge ----------------------------------------------------
    // No grip on the bottom edge: left span, fill, right span.
    let ctx = bottom_right(f);
    let mut bottom_step = 0;
    let mut found = false;
    // `1` before the left span (the mock's `╰─`), `1` after the ctx cell.
    let fixed = 1 + cols_of(&ctx);
    for step in [0usize, 1, 2] {
        let cand = bottom_left(f, step);
        let need = 1 + cols_of(&cand) + cols_of(&ctx);
        if need <= inner {
            bottom_step = step;
            found = true;
            break;
        }
    }
    if !found {
        // Even the bare node cannot sit beside the ctx span: drop the branch
        // AND the ctx and cut the node (the plan's steps 2 and 5 together).
        bottom_step = 2;
    }
    let node_max = inner.saturating_sub(fixed).max(1);
    let bottom = {
        let mut row = vec![('╰', Part::Border)];
        row.push(('─', Part::Border));
        row.extend(fit_row(&bottom_left(f, bottom_step), node_max));
        let used = cols_of(&row) - 1;
        for _ in used..inner - cols_of(&ctx) {
            row.push(('─', Part::Border));
        }
        row.extend(ctx);
        row.push(('╯', Part::Border));
        row
    };

    Edges {
        top,
        bottom,
        top_step,
        bottom_step,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(cols: u16, rows: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            rows,
            cols,
        }
    }

    fn s(row: &[(char, Part)]) -> String {
        row.iter().map(|(c, _)| c).collect()
    }

    fn full<'a>() -> EdgeFields<'a> {
        EdgeFields {
            name: "king-5317-succeed-g3",
            status: Some(('●', "Work")),
            model: Some("opus-5"),
            node: Some("node7"),
            branch: Some("main"),
            ctx: Some("49%"),
        }
    }

    // AC1-HP
    #[test]
    fn content_rect_insets_a_framed_rect() {
        let r = rect(40, 12);
        assert!(framed(r));
        let c = content_rect(r);
        assert_eq!((c.x, c.y, c.cols, c.rows), (1, 1, 38, 10));
    }

    // AC1-EDGE
    #[test]
    fn narrow_or_short_rects_stay_unframed() {
        for r in [rect(19, 12), rect(40, 3)] {
            assert!(!framed(r));
            assert_eq!(content_rect(r), r);
        }
    }

    // AC2-HP
    #[test]
    fn full_fields_lay_out_both_edges_on_step_zero() {
        let e = edges(&full(), rect(100, 12), false, true);
        let top = s(&e.top);
        assert!(top.starts_with("╭▐ king-5317-succeed-g3 ▌─"), "{top}");
        assert!(top.ends_with(" ● Work · opus-5 ╮"), "{top}");
        assert_eq!(cols_of(&e.top), 100);
        let bottom = s(&e.bottom);
        assert!(
            bottom.starts_with("╰─ node7 · main ─") && bottom.ends_with(" ctx 49% ─╯"),
            "{bottom}"
        );
        assert_eq!(cols_of(&e.bottom), 100);
        assert_eq!((e.top_step, e.bottom_step), (0, 0));
    }

    // AC2-EDGE, one test per step
    #[test]
    fn bottom_drops_ctx_first_when_only_it_overflows() {
        // 24 cols: the ctx span (9) plus node (15) plus 2 fixed fill = 26 > 22
        // inner, and nothing else can give: ctx drops.
        let e = edges(&full(), rect(24, 12), false, true);
        assert_eq!(e.bottom_step, 1, "{:?}", s(&e.bottom));
        assert!(!s(&e.bottom).contains("49%"));
        assert!(s(&e.bottom).contains("node7 · main"));
    }

    #[test]
    fn bottom_drops_branch_next() {
        // 20 cols with a 7-char node: ` x-abc12 ` (9) plus ctx (10) plus the
        // `╰─` (2) = 21 > 18 inner; dropping ctx, the branch still cannot sit
        // (` · main ` spans 9), so the span falls to the bare node.
        let f = EdgeFields {
            node: Some("x-abc12"),
            ..full()
        };
        let e = edges(&f, rect(20, 12), true, true);
        assert_eq!(e.bottom_step, 2, "{:?}", s(&e.bottom));
        assert!(s(&e.bottom).contains(" x-abc12 "), "{:?}", s(&e.bottom));
        assert!(!s(&e.bottom).contains("main"), "{:?}", s(&e.bottom));
        assert!(!s(&e.bottom).contains("49%"), "{:?}", s(&e.bottom));
    }

    #[test]
    fn bottom_ellipsizes_the_node_last() {
        // 10 cols: even the bare node span cannot fit; it cuts to `…`.
        let e = edges(&full(), rect(10, 12), false, true);
        assert!(s(&e.bottom).contains('…'), "{:?}", s(&e.bottom));
        assert_eq!(cols_of(&e.bottom), 10);
    }

    #[test]
    fn top_drops_the_model_first() {
        // 40 cols: the full tab (25) plus the full status span (17) needs 43
        // of 38 inner cells; dropping the model frees 9 and the whole name
        // survives.
        let e = edges(&full(), rect(40, 12), false, true);
        assert_eq!(e.top_step, 3, "{:?}", s(&e.top));
        assert!(s(&e.top).contains("● Work"), "{:?}", s(&e.top));
        assert!(!s(&e.top).contains("opus-5"), "{:?}", s(&e.top));
        assert!(
            s(&e.top).contains("king-5317-succeed-g3"),
            "{:?}",
            s(&e.top)
        );
    }

    #[test]
    fn top_drops_the_word_next_glyph_survives() {
        // 35 cols: ` ● Work ` beside the full tab needs 34 of 33 inner.
        let e = edges(&full(), rect(35, 12), false, true);
        assert_eq!(e.top_step, 4, "{:?}", s(&e.top));
        assert!(s(&e.top).contains('●'), "{:?}", s(&e.top));
        assert!(!s(&e.top).contains("Work"), "{:?}", s(&e.top));
        assert!(
            s(&e.top).contains("king-5317-succeed-g3"),
            "{:?}",
            s(&e.top)
        );
    }

    #[test]
    fn top_cuts_the_name_last() {
        // 30 cols: even the bare glyph cannot sit beside the full tab; the
        // word drops AND the name is cut (step 5, the last resort).
        let e = edges(&full(), rect(30, 12), false, true);
        assert_eq!(e.top_step, 4, "{:?}", s(&e.top));
        assert!(
            s(&e.top).contains("king-5317-succeed-g…"),
            "{:?}",
            s(&e.top)
        );
        assert!(s(&e.top).contains('●'), "{:?}", s(&e.top));
        assert_eq!(cols_of(&e.top), 30);
    }

    #[test]
    fn top_ellipsizes_the_name_at_the_floor() {
        // 20 cols WITH a grip: the tab zone is 6, the name gets 2 => `k…`.
        let e = edges(&full(), rect(20, 12), true, true);
        assert!(s(&e.top).contains("k…"), "{:?}", s(&e.top));
        assert!(s(&e.top).contains('●'), "{:?}", s(&e.top));
        assert_eq!(cols_of(&e.top), 20);
    }

    #[test]
    fn only_top_overflow_keeps_ctx_on_the_bottom() {
        // 34 cols: the tab (25) + status (17) cannot both fit, but the bottom
        // edge has room for everything.
        let e = edges(&full(), rect(34, 12), false, true);
        assert!(e.top_step > 0, "{:?}", s(&e.top));
        assert_eq!(e.bottom_step, 0, "{:?}", s(&e.bottom));
        assert!(s(&e.bottom).contains("49%"));
    }

    // AC2-FOCUS
    #[test]
    fn focus_swaps_the_caps_never_the_width() {
        let on = edges(&full(), rect(60, 12), true, true);
        let off = edges(&full(), rect(60, 12), true, false);
        assert_eq!(s(&on.top).replace('▐', "─").replace('▌', "─"), s(&off.top));
        assert_eq!(on.top_step, off.top_step);
        assert!(on
            .top
            .iter()
            .any(|(c, p)| *p == Part::Cap && (*c == '▐' || *c == '▌')));
        assert!(!off.top.iter().any(|(_, p)| *p == Part::Cap));
    }

    // AC2-ERR
    #[test]
    fn long_and_cjk_names_cut_to_exactly_the_rect_width() {
        let long = EdgeFields {
            name: "k".repeat(200).leak(),
            ..full()
        };
        let e = edges(&long, rect(40, 12), true, true);
        assert_eq!(cols_of(&e.top), 40);
        assert_eq!(s(&e.top).chars().next(), Some('╭'));
        assert_eq!(s(&e.top).chars().last(), Some('╮'));
        assert!(s(&e.top).contains('…'));
        let cjk = EdgeFields {
            name: "패널프레임테스트 라벨",
            ..full()
        };
        let e = edges(&cjk, rect(30, 12), false, true);
        assert_eq!(cols_of(&e.top), 30);
        assert!(s(&e.top).contains('…'));
        assert_eq!(cols_of(&e.bottom), 30);
    }

    // AC3-HP
    #[test]
    fn a_plain_shell_shows_only_its_name() {
        let bare = EdgeFields {
            name: "zsh",
            status: None,
            model: None,
            node: None,
            branch: None,
            ctx: None,
        };
        let e = edges(&bare, rect(40, 12), false, true);
        let top = s(&e.top);
        assert!(top.starts_with("╭▐ zsh ▌─") && top.ends_with('╮'), "{top}");
        assert_eq!(cols_of(&e.top), 40);
        assert_eq!(s(&e.bottom), format!("╰{}╯", "─".repeat(38)));
    }
}
