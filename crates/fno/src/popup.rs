//! The shared anchored/centered popup overlay widget (x-8ccf US1): the single
//! component behind the row context menu, the which-key keybinds modal, and the
//! NEW|MENU / settings popups. It owns positioning (clamp + edge-flip), the row
//! anatomy (glyph · label · right-aligned hint, headers, rules, a full-width
//! entry, a spatial grid), and the shared selection grammar (arrow move,
//! Enter/click execute, Esc dismiss). Each consumer supplies the rows and maps
//! the selected target to its own action; positioning, rendering, and
//! navigation live here so the surfaces cannot drift.
//!
//! Rendering matches the existing overlay idiom (`draw_lines_overlay`): a padded
//! INVERSE block fully overwrites the cells beneath it, so the popup is opaque
//! (the herdr "cover the middle, no bleed" requirement) without a real bg color.
//! The selected target renders as a normal-video cut-out in the inverse block.

use crate::proto::{cell_flags, Cell, Color};

/// Popup content never renders wider than this (herdr: fixed max width); longer
/// lines ellipsize. Anchored menus are usually far narrower.
pub const WIDTH_CAP: usize = 60;

/// Where a popup anchors. `At` opens at a screen cell (pointer / button cell)
/// and clamps + flips to stay fully on-screen; `Center` centers a fixed block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    At { row: u16, col: u16 },
    Center,
}

/// One cell of a spatial grid row (the 2x2 split block). `glyph` reads as the
/// direction; `label` is the accessible name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridCell {
    pub glyph: String,
    pub label: String,
}

/// A popup row. `Header`/`Rule` are inert; `Entry`/`FullWidth`/`Grid` carry
/// selectable targets (a `Grid` contributes one target per cell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupRow {
    /// Section header, rendered in an accent style, not selectable.
    Header(String),
    /// A horizontal rule separator, not selectable.
    Rule,
    /// A selectable action: glyph + label + right-aligned key hint.
    Entry {
        glyph: String,
        label: String,
        hint: String,
    },
    /// A selectable full-width entry (e.g. "New Tab" spanning the block top).
    FullWidth(String),
    /// A row of selectable grid cells (the 2x2 split block = two Grid rows).
    Grid(Vec<GridCell>),
}

impl PopupRow {
    /// How many selectable targets this row contributes.
    fn cells(&self) -> usize {
        match self {
            PopupRow::Grid(cells) => cells.len(),
            PopupRow::Entry { .. } | PopupRow::FullWidth(_) => 1,
            PopupRow::Header(_) | PopupRow::Rule => 0,
        }
    }
}

/// A directional selection move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDir {
    Up,
    Down,
    Left,
    Right,
}

/// The popup widget: its rows, where it anchors, and the current selection (a
/// flat index into [`Popup::targets`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Popup {
    pub rows: Vec<PopupRow>,
    pub anchor: Anchor,
    /// Flat index into `targets()`. Clamped on read; 0 lands on the first
    /// selectable target (or nothing when there are none).
    pub sel: usize,
}

/// One laid-out line ready to draw, plus its style and the selected sub-span
/// (whole line for an Entry/FullWidth, a single cell for a Grid).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedLine {
    pub text: String,
    pub header: bool,
    /// `(col_offset, len)` within the block that renders normal-video (the
    /// selection cut-out), if the selected target is on this line.
    pub sel_span: Option<(usize, usize)>,
    /// The selectable targets on this line as `(flat_target, col_offset, len)`,
    /// for mouse hit-testing a click/hover to a target.
    pub hits: Vec<(usize, usize, usize)>,
}

/// A fully laid-out popup: where it sits and the lines to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub origin: (usize, usize),
    pub width: usize,
    pub lines: Vec<RenderedLine>,
}

impl Popup {
    pub fn new(rows: Vec<PopupRow>, anchor: Anchor) -> Self {
        Popup {
            rows,
            anchor,
            sel: 0,
        }
    }

    /// Every selectable target as `(row_index, cell_index_within_row)`, in
    /// render order.
    pub fn targets(&self) -> Vec<(usize, usize)> {
        let mut t = Vec::new();
        for (ri, row) in self.rows.iter().enumerate() {
            for ci in 0..row.cells() {
                t.push((ri, ci));
            }
        }
        t
    }

    /// The selected `(row_index, cell_index)`, or `None` when nothing is
    /// selectable.
    pub fn selected(&self) -> Option<(usize, usize)> {
        let targets = self.targets();
        targets
            .get(self.sel.min(targets.len().saturating_sub(1)))
            .copied()
    }

    /// Point the selection at a flat target index (mouse hover/click), clamped.
    pub fn select(&mut self, target: usize) {
        let n = self.targets().len();
        if n > 0 {
            self.sel = target.min(n - 1);
        }
    }

    /// Move the selection. Up/Down step between selectable rows (landing on the
    /// cell nearest the current column); Left/Right step between cells within a
    /// grid row (a no-op on single-cell rows). No wrap - a move off the end
    /// stays put, matching every other mux selector.
    pub fn nav(&mut self, dir: NavDir) {
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let cur = self.sel.min(targets.len() - 1);
        let (row, cell) = targets[cur];
        let row_len = |r: usize| targets.iter().filter(|&&(rr, _)| rr == r).count();
        let idx_of = |r: usize, c: usize| targets.iter().position(|&t| t == (r, c));
        let new = match dir {
            NavDir::Left => cell.checked_sub(1).and_then(|c| idx_of(row, c)),
            NavDir::Right => idx_of(row, cell + 1),
            NavDir::Up => targets[..cur]
                .iter()
                .rev()
                .find(|&&(r, _)| r < row)
                .and_then(|&(r, _)| idx_of(r, cell.min(row_len(r).saturating_sub(1)))),
            NavDir::Down => targets[cur + 1..]
                .iter()
                .find(|&&(r, _)| r > row)
                .and_then(|&(r, _)| idx_of(r, cell.min(row_len(r).saturating_sub(1)))),
        };
        if let Some(n) = new {
            self.sel = n;
        }
    }

    /// Lay the popup out against a `(rows, cols)` terminal: compute the block
    /// width from its content, position it (centered or clamped/flipped anchor),
    /// and render each row to a padded line with selection + hit-test spans.
    pub fn render(&self, term: (u16, u16)) -> Rendered {
        let (trows, tcols) = (term.0.max(1) as usize, term.1.max(1) as usize);
        let sel = self.selected();
        // Per-cell width for a grid row: the widest cell content + padding.
        let grid_cell_w = self
            .rows
            .iter()
            .flat_map(|r| match r {
                PopupRow::Grid(cells) => cells
                    .iter()
                    .map(|c| c.glyph.chars().count() + c.label.chars().count() + 3)
                    .collect::<Vec<_>>(),
                _ => vec![],
            })
            .max()
            .unwrap_or(0);
        // Content width: the widest row before padding.
        let content_w = self
            .rows
            .iter()
            .map(|r| match r {
                PopupRow::Header(s) | PopupRow::FullWidth(s) => s.chars().count() + 2,
                PopupRow::Rule => 0,
                PopupRow::Entry { glyph, label, hint } => {
                    // glyph + space + label + gap + hint
                    glyph.chars().count() + 1 + label.chars().count() + 2 + hint.chars().count() + 2
                }
                PopupRow::Grid(cells) => grid_cell_w * cells.len(),
            })
            .max()
            .unwrap_or(0);
        let width = content_w.clamp(1, WIDTH_CAP.min(tcols));

        let mut target_idx = 0usize;
        let mut lines = Vec::with_capacity(self.rows.len());
        for (ri, row) in self.rows.iter().enumerate() {
            let line = match row {
                PopupRow::Header(s) => RenderedLine {
                    text: pad(&format!(" {s}"), width),
                    header: true,
                    sel_span: None,
                    hits: vec![],
                },
                PopupRow::Rule => RenderedLine {
                    text: "─".repeat(width),
                    header: false,
                    sel_span: None,
                    hits: vec![],
                },
                PopupRow::FullWidth(s) => {
                    let ti = target_idx;
                    target_idx += 1;
                    RenderedLine {
                        text: pad(&format!(" {s}"), width),
                        header: false,
                        sel_span: (sel == Some((ri, 0))).then_some((0, width)),
                        hits: vec![(ti, 0, width)],
                    }
                }
                PopupRow::Entry { glyph, label, hint } => {
                    let left = format!(" {glyph} {label}");
                    let left_w = left.chars().count();
                    let hint_w = hint.chars().count();
                    let gap = width.saturating_sub(left_w + hint_w + 1);
                    let text = pad(&format!("{left}{}{hint} ", " ".repeat(gap)), width);
                    let ti = target_idx;
                    target_idx += 1;
                    RenderedLine {
                        text,
                        header: false,
                        sel_span: (sel == Some((ri, 0))).then_some((0, width)),
                        hits: vec![(ti, 0, width)],
                    }
                }
                PopupRow::Grid(cells) => {
                    let mut text = String::new();
                    let mut hits = Vec::new();
                    let mut sel_span = None;
                    for (ci, c) in cells.iter().enumerate() {
                        let cell = center(&format!("{} {}", c.glyph, c.label), grid_cell_w);
                        let off = ci * grid_cell_w;
                        hits.push((target_idx, off, grid_cell_w));
                        if sel == Some((ri, ci)) {
                            sel_span = Some((off, grid_cell_w));
                        }
                        target_idx += 1;
                        text.push_str(&cell);
                    }
                    RenderedLine {
                        text: pad(&text, width),
                        header: false,
                        sel_span,
                        hits,
                    }
                }
            };
            lines.push(line);
        }

        let block_h = lines.len();
        let origin = origin(self.anchor, width, block_h, (trows, tcols));
        Rendered {
            origin,
            width,
            lines,
        }
    }
}

/// Compute the on-screen top-left `(row, col)` for a block of `w`×`h` cells.
/// Centered blocks center; anchored blocks open at the cell and clamp to the
/// screen, flipping ABOVE the anchor when the block would overflow the bottom.
pub fn origin(anchor: Anchor, w: usize, h: usize, term: (usize, usize)) -> (usize, usize) {
    let (trows, tcols) = term;
    match anchor {
        Anchor::Center => (trows.saturating_sub(h) / 2, tcols.saturating_sub(w) / 2),
        Anchor::At { row, col } => {
            let (r, c) = (row as usize, col as usize);
            // Horizontal: clamp so the right edge stays on-screen.
            let c0 = c.min(tcols.saturating_sub(w));
            // Vertical: open below the anchor; if that overflows, flip above it.
            let r0 = if r + h <= trows {
                r
            } else {
                r.saturating_sub(h).min(trows.saturating_sub(h))
            };
            (r0, c0)
        }
    }
}

/// Truncate to `w` display chars (ellipsizing) and pad with spaces to `w`, so a
/// line is a fixed-width block that fully overwrites the content beneath it.
/// Mirrors `client::pad_to` (kept local so the widget is self-contained).
fn pad(s: &str, w: usize) -> String {
    let count = s.chars().count();
    if count > w {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        let mut t = s.to_string();
        t.push_str(&" ".repeat(w - count));
        t
    }
}

/// Center `s` within `w` (space padded); truncates via [`pad`] when too wide.
fn center(s: &str, w: usize) -> String {
    let count = s.chars().count();
    if count >= w {
        return pad(s, w);
    }
    let left = (w - count) / 2;
    let right = w - count - left;
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
}

/// Draw a laid-out popup into the screen cell buffer: an INVERSE block, headers
/// bold, the selected sub-span rendered normal-video (a cut-out that reads as
/// the cursor against the inverse block). Cell-bounds-checked, so a popup near
/// an edge clips rather than panicking.
pub fn draw(cells: &mut [Cell], rows: usize, cols: usize, r: &Rendered) {
    let (r0, c0) = r.origin;
    for (i, line) in r.lines.iter().enumerate() {
        let sr = r0 + i;
        if sr >= rows {
            break;
        }
        for (j, ch) in line.text.chars().enumerate() {
            let sc = c0 + j;
            if sc >= cols {
                break;
            }
            let selected = line
                .sel_span
                .is_some_and(|(off, len)| j >= off && j < off + len);
            let mut flags = if selected { 0 } else { cell_flags::INVERSE };
            if line.header {
                flags |= cell_flags::BOLD;
            }
            cells[sr * cols + sc] = Cell {
                c: ch,
                fg: Color::Default,
                bg: Color::Default,
                flags,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(g: &str, l: &str, h: &str) -> PopupRow {
        PopupRow::Entry {
            glyph: g.into(),
            label: l.into(),
            hint: h.into(),
        }
    }
    fn grid(labels: &[&str]) -> PopupRow {
        PopupRow::Grid(
            labels
                .iter()
                .map(|l| GridCell {
                    glyph: "x".into(),
                    label: (*l).into(),
                })
                .collect(),
        )
    }

    #[test]
    fn centered_origin_centers() {
        // A 20x4 block in an 80x24 terminal centers.
        assert_eq!(origin(Anchor::Center, 20, 4, (24, 80)), (10, 30));
    }

    #[test]
    fn anchored_origin_clamps_right_and_flips_bottom() {
        // Opens at the cell when it fits.
        assert_eq!(
            origin(Anchor::At { row: 2, col: 5 }, 10, 4, (24, 80)),
            (2, 5)
        );
        // Near the right edge: clamp so the block's right edge stays on-screen.
        assert_eq!(
            origin(Anchor::At { row: 2, col: 78 }, 10, 4, (24, 80)),
            (2, 70)
        );
        // Near the bottom edge: flip ABOVE the anchor.
        assert_eq!(
            origin(Anchor::At { row: 22, col: 5 }, 10, 4, (24, 80)),
            (18, 5)
        );
    }

    #[test]
    fn anchored_origin_degrades_when_block_exceeds_screen() {
        // A block taller/wider than the terminal clamps to 0 rather than
        // underflowing (the caller Notices "terminal too small" separately).
        assert_eq!(
            origin(Anchor::At { row: 5, col: 5 }, 200, 100, (24, 80)),
            (0, 0)
        );
    }

    #[test]
    fn targets_skip_headers_and_rules() {
        let p = Popup::new(
            vec![
                PopupRow::Header("h".into()),
                entry("a", "one", ""),
                PopupRow::Rule,
                entry("b", "two", ""),
            ],
            Anchor::Center,
        );
        // Two selectable targets, at rows 1 and 3.
        assert_eq!(p.targets(), vec![(1, 0), (3, 0)]);
    }

    #[test]
    fn nav_up_down_walk_selectable_rows() {
        let mut p = Popup::new(
            vec![entry("a", "one", ""), PopupRow::Rule, entry("b", "two", "")],
            Anchor::Center,
        );
        assert_eq!(p.selected(), Some((0, 0)));
        p.nav(NavDir::Down);
        assert_eq!(p.selected(), Some((2, 0)), "skips the rule");
        p.nav(NavDir::Down);
        assert_eq!(p.selected(), Some((2, 0)), "no wrap past the end");
        p.nav(NavDir::Up);
        assert_eq!(p.selected(), Some((0, 0)));
        p.nav(NavDir::Up);
        assert_eq!(p.selected(), Some((0, 0)), "no wrap past the start");
    }

    #[test]
    fn nav_left_right_walk_grid_cells_then_down_leaves_the_grid() {
        // Layout: FullWidth, then a 2-cell grid row, then an entry.
        let mut p = Popup::new(
            vec![
                PopupRow::FullWidth("New Tab".into()),
                grid(&["left", "right"]),
                entry("p", "peek", ""),
            ],
            Anchor::Center,
        );
        // FullWidth (0,0), grid cells (1,0)(1,1), entry (2,0).
        assert_eq!(p.selected(), Some((0, 0)));
        p.nav(NavDir::Down);
        assert_eq!(p.selected(), Some((1, 0)), "into the grid, first cell");
        p.nav(NavDir::Right);
        assert_eq!(p.selected(), Some((1, 1)), "across the grid");
        p.nav(NavDir::Right);
        assert_eq!(p.selected(), Some((1, 1)), "no wrap off the grid row");
        p.nav(NavDir::Left);
        assert_eq!(p.selected(), Some((1, 0)));
        // Down from a grid cell lands on the next row, same-or-nearest column.
        p.nav(NavDir::Down);
        assert_eq!(p.selected(), Some((2, 0)), "out of the grid to the entry");
    }

    #[test]
    fn render_marks_selected_line_and_hits() {
        let p = Popup::new(
            vec![entry("a", "one", "x"), entry("b", "two", "y")],
            Anchor::Center,
        );
        let r = p.render((24, 80));
        assert_eq!(r.lines.len(), 2);
        // First entry selected by default: its whole line is the cut-out span.
        assert_eq!(r.lines[0].sel_span, Some((0, r.width)));
        assert_eq!(r.lines[1].sel_span, None);
        // Each entry line reports exactly one hit target.
        assert_eq!(r.lines[0].hits.len(), 1);
        assert_eq!(r.lines[0].hits[0].0, 0);
        assert_eq!(r.lines[1].hits[0].0, 1);
    }

    #[test]
    fn render_grid_line_reports_a_hit_per_cell() {
        let p = Popup::new(vec![grid(&["l", "r"])], Anchor::Center);
        let r = p.render((24, 80));
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].hits.len(), 2, "one hit target per grid cell");
        // The two cells occupy disjoint, adjacent spans.
        let (_, off0, len0) = r.lines[0].hits[0];
        let (_, off1, _) = r.lines[0].hits[1];
        assert_eq!(off0, 0);
        assert_eq!(off1, off0 + len0);
    }
}
