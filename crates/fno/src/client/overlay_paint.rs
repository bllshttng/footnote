//! The family-B overlay paint plumbing: anchors, layout, and the string and
//! body blitters, moved out of `client.rs` under the shrink-only file
//! budget. The backlog panels' variant carries the per-char role segs and
//! the cursor band; the string variant stays byte-identical for the seven
//! family-B overlays.

use super::blank_straddling_pair;
use crate::chrome;
use crate::popup;
use crate::proto::Cell;
use crate::theme::Theme;

#[derive(Debug, Clone, Copy)]
pub(crate) enum OverlayAnchor {
    Center,
    At { row: usize, col: usize },
}

/// One family-B overlay layout. Drawing and mouse hit-testing consume this same
/// framed block and origin, so a close chip cannot drift away from the glyph it
/// paints.
#[derive(Debug, Clone)]
pub(crate) struct OverlayLayout {
    pub(crate) origin: (usize, usize),
    pub(crate) framed: chrome::Framed,
    /// The body window the layout chose: first visible body line, and how
    /// many it took.
    pub(crate) window: (usize, usize),
    /// The framed-line index the first body line paints at.
    pub(crate) body_top: usize,
}

impl OverlayLayout {
    pub(crate) fn hit_at(&self, row: u16, col: u16) -> Option<usize> {
        chrome::framed_hit_at(&self.framed, self.origin, row as usize, col as usize)
    }

    /// A layout built from parts a test already holds (no window, no body
    /// offset): the literal shape the click-router tests assert against.
    #[cfg(test)]
    pub(crate) fn from_parts(origin: (usize, usize), framed: chrome::Framed) -> Self {
        OverlayLayout {
            origin,
            framed,
            window: (0, 0),
            body_top: 0,
        }
    }
}

pub(crate) fn family_b_origin(
    anchor: OverlayAnchor,
    block_w: usize,
    block_h: usize,
    content_origin: (usize, usize),
    content_dims: (usize, usize),
) -> (usize, usize) {
    let (base_r, base_c) = content_origin;
    let (content_rows, content_cols) = content_dims;
    let max_r = base_r + content_rows.saturating_sub(block_h);
    let max_c = base_c + content_cols.saturating_sub(block_w);
    match anchor {
        OverlayAnchor::Center => (
            base_r + content_rows.saturating_sub(block_h) / 2,
            base_c + content_cols.saturating_sub(block_w) / 2,
        ),
        OverlayAnchor::At { row, col } => {
            let origin_r = if row.saturating_add(block_h) <= base_r + content_rows {
                row.max(base_r).min(max_r)
            } else {
                row.saturating_sub(block_h).max(base_r).min(max_r)
            };
            (origin_r, col.max(base_c).min(max_c))
        }
    }
}

/// Lay out family-B overlay lines in the content viewport. The body window,
/// frame, origin, and hit spans are calculated once for both drawing and input.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_lines_overlay<S: AsRef<str>>(
    content_origin: (usize, usize),
    content_dims: (usize, usize),
    chrome: &chrome::Chrome,
    lines: &[S],
    follow: Option<usize>,
    anchor: OverlayAnchor,
) -> OverlayLayout {
    let (content_rows, content_cols) = content_dims;
    // Body width: the widest line (across the whole body, windowed-out rows
    // included), capped to the viewport minus the side borders.
    let body_w = lines
        .iter()
        .map(|l| l.as_ref().chars().count())
        .max()
        .unwrap_or(0)
        .min(content_cols.saturating_sub(chrome::Chrome::FRAME_COLS));
    // Reserve the chrome overhead and window the body to the rows that remain.
    // Before chrome the body had the whole viewport; the frame borrows `overhead`
    // rows for its border/footer, so without windowing a body that filled the
    // viewport loses its tail off-screen while those rows stay selectable. Top-
    // pin matches the pre-chrome posture (centered when it fits, clipped at the
    // top when it does not); the scrollbar marks the cut.
    let overhead = chrome.rows_overhead();
    let body_budget = content_rows.saturating_sub(overhead);
    let total = lines.len();
    let (start, take, scroll) = if total > body_budget {
        // Covers body_budget == 0 (a viewport shorter than the chrome
        // overhead): windows to zero body rows instead of painting the whole
        // body plus its border past the content viewport.
        //
        // `follow` is the body index that MUST stay visible - a cursor. Without
        // it the window is top-pinned, which is right for a static body and
        // wrong for one the operator drives: the tenth row of a fourteen-row
        // picker on a short terminal would be selectable and invisible, which is
        // the same "you cannot reach it" defect as truncating the list. The
        // window scrolls by the minimum needed to contain the cursor, so it only
        // moves at the edges. `pos` then reports where the window really is,
        // making the scrollbar thumb truthful rather than always parked at 0.
        let start = match follow.filter(|_| body_budget > 0) {
            Some(f) => f.saturating_sub(body_budget - 1).min(total - body_budget),
            None => 0,
        };
        (
            start,
            body_budget,
            Some(chrome::Scroll {
                pos: start,
                total,
                visible: body_budget,
            }),
        )
    } else {
        (0, total, None)
    };
    let body: Vec<chrome::BodyLine> = lines[start..start + take]
        .iter()
        .map(|l| chrome::BodyLine::plain(l.as_ref()))
        .collect();
    let framed = chrome::frame(&body, chrome, body_w, scroll);
    let box_h = framed.lines.len().min(content_rows);
    let box_w = framed.width.min(content_cols);
    let origin = family_b_origin(anchor, box_w, box_h, content_origin, content_dims);
    OverlayLayout {
        origin,
        framed,
        window: (start, take),
        body_top: chrome.rows_above(),
    }
}

/// The backlog panels' variant of [`layout_lines_overlay`]: the caller has
/// already built `chrome::BodyLine`s (with per-char role segs and the
/// panel's pad role), so the layout keeps them instead of flattening
/// strings. Pure: painting is [`draw_body_overlay`].
pub(crate) fn layout_body_overlay(
    content_origin: (usize, usize),
    content_dims: (usize, usize),
    chrome: &chrome::Chrome,
    body: &[chrome::BodyLine],
    follow: Option<usize>,
    anchor: OverlayAnchor,
) -> OverlayLayout {
    let (content_rows, content_cols) = content_dims;
    // Body width: the widest line, capped to the viewport minus the frame.
    let body_w = body
        .iter()
        .map(|l| chrome::str_cols(&l.text))
        .max()
        .unwrap_or(0)
        .min(content_cols.saturating_sub(chrome::Chrome::FRAME_COLS));
    let overhead = chrome.rows_overhead();
    let body_budget = content_rows.saturating_sub(overhead);
    let total = body.len();
    let (start, take, scroll) = if total > body_budget {
        let start = match follow.filter(|_| body_budget > 0) {
            Some(f) => f.saturating_sub(body_budget - 1).min(total - body_budget),
            None => 0,
        };
        (
            start,
            body_budget,
            Some(chrome::Scroll {
                pos: start,
                total,
                visible: body_budget,
            }),
        )
    } else {
        (0, total, None)
    };
    let framed = chrome::frame(&body[start..start + take], chrome, body_w, scroll);
    let box_h = framed.lines.len().min(content_rows);
    let box_w = framed.width.min(content_cols);
    let origin = family_b_origin(anchor, box_w, box_h, content_origin, content_dims);
    OverlayLayout {
        origin,
        framed,
        window: (start, take),
        body_top: chrome.rows_above(),
    }
}

pub(crate) fn draw_overlay_layout(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    layout: &OverlayLayout,
    theme: &Theme,
) {
    let (origin_r, origin_c) = layout.origin;
    // A framed block stamps a SUB-RANGE of each row, so a double-width
    // glyph in the pane content underneath can straddle either edge, leaving one
    // half painted and the row corrupted. The name modal carried this guard when
    // it hand-painted its own block; every family-B overlay needs it for the same
    // reason, so it lives here, once, rather than travelling with one caller.
    for i in 0..layout.framed.lines.len() {
        let r = origin_r + i;
        if r >= rows {
            break;
        }
        // `framed.width`, not `box_w`: `blit` paints the FULL framed width, and
        // `box_w` is that width clamped to the viewport. When the chrome's own
        // minimum (a long title) pushes the frame past the viewport the two
        // differ, and clamping here would leave the real right edge unchecked -
        // stranding a spacer on exactly the overflow this guard exists for.
        blank_straddling_pair(
            cells,
            cols,
            r,
            origin_c,
            (origin_c + layout.framed.width).min(cols),
        );
    }
    chrome::blit(cells, rows, cols, layout.origin, &layout.framed, theme);
}

/// Draw one popup overlay (which-key modal, row menu, aux popup, the dock's child picker).
pub(crate) fn draw_popup_overlay(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    popup: &popup::Popup,
    term: (u16, u16),
    theme: &Theme,
) {
    popup::draw(cells, rows, cols, &popup.render(term), theme);
}

/// Draw overlay lines centered in the content viewport (right of the sideline,
/// above any splits), framed with `chrome` and colored by `theme`. The seven
/// family-B overlays (catch-up, needs-me, move-pick, attach-place, connections,
/// peek, navigator) all route through here, so framing them all is this one
/// change - the point of chrome being a frame function rather than a field on
/// `Popup`. Cell-bounds-checked (a tiny terminal clips rather than panics).
///
/// `content_origin` is `(TAB_BAR_ROWS, panel_w)`; `content_dims` is the content
/// viewport's `(rows, cols)` (status row excluded). The framed block is centered
/// on its FRAMED dimensions (placement; policy).
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_lines_overlay<S: AsRef<str>>(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    content_origin: (usize, usize),
    content_dims: (usize, usize),
    chrome: &chrome::Chrome,
    lines: &[S],
    theme: &Theme,
    follow: Option<usize>,
) {
    let layout = layout_lines_overlay(
        content_origin,
        content_dims,
        chrome,
        lines,
        follow,
        OverlayAnchor::Center,
    );
    draw_overlay_layout(cells, rows, cols, &layout, theme);
}
