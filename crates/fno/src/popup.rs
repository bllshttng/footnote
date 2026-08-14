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

use crate::chrome::{self, BodyLine, Chrome, FramedLine, Scroll};
use crate::proto::Cell;
use crate::theme::{cell_style, Role, Theme};

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
    /// First visible row when the block is taller than the terminal (the
    /// which-key modal scrolls; short anchored menus keep this 0). Clamped in
    /// [`Popup::render`] so it can never scroll past the last screenful.
    pub scroll: usize,
    /// The chrome every modal wears. Its level is derived from `anchor` and
    /// private (no setter), so every centered modal is Full and every anchored
    /// menu is Bare with no way for a call site to disagree.
    pub chrome: Chrome,
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
    /// for mouse hit-testing a click/hover to a target. Offsets are in FRAMED
    /// coordinates (past the left border) once [`Popup::render`] frames.
    pub hits: Vec<(usize, usize, usize)>,
    /// One [`Role`] per char, set by [`chrome::frame`]. Empty only on an
    /// unframed body line (tests); the production draw path colors each char by
    /// `roles[j]` via [`crate::theme::cell_style`].
    pub roles: Vec<Role>,
}

/// A fully laid-out popup: where it sits and the lines to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub origin: (usize, usize),
    pub width: usize,
    pub lines: Vec<RenderedLine>,
}

impl Rendered {
    /// Whether a screen cell falls inside the laid-out block. The click router
    /// uses this to tell a click ON the popup that hit no target (a header, a
    /// rule, a border) from a click OFF it: the former is swallowed, the latter
    /// dismisses. Without it, clicking a `Header` closed the menu because the
    /// header contributes no hit target and `None` read as "off the popup".
    pub fn contains(&self, row: u16, col: u16) -> bool {
        let (r0, c0) = self.origin;
        let h = self.lines.len();
        let row = row as usize;
        let col = col as usize;
        row >= r0 && row < r0 + h && col >= c0 && col < c0 + self.width
    }
}

impl Popup {
    pub fn new(rows: Vec<PopupRow>, anchor: Anchor) -> Self {
        Popup {
            rows,
            chrome: Chrome::new("", anchor),
            anchor,
            sel: 0,
            scroll: 0,
        }
    }

    /// Set the chrome title (the modal's heading).
    pub fn title(mut self, t: impl Into<String>) -> Self {
        self.chrome.title = t.into();
        self
    }
    pub fn subtitle(mut self, s: impl Into<String>) -> Self {
        self.chrome.subtitle = Some(s.into());
        self
    }
    pub fn tabs(mut self, tabs: Vec<(String, bool)>) -> Self {
        self.chrome.tabs = tabs;
        self
    }
    pub fn footer(mut self, f: impl Into<String>) -> Self {
        self.chrome.footer = Some(f.into());
        self
    }

    /// Scroll the body by `delta` rows (negative = up), saturating at the top.
    /// The bottom clamp happens in [`Popup::render`] against the live viewport.
    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = (self.scroll as isize + delta).max(0) as usize;
    }

    /// The visible BODY height for a `term_rows`-tall terminal: the rows the
    /// chrome leaves (terminal rows minus the frame overhead), clamped so a
    /// too-short terminal still shows one body row. Must agree with the window
    /// [`Popup::render`] cuts, else `follow_sel`/`clamp_sel` could park the
    /// selection in a body row render() windows out (an invisible Enter target).
    fn viewport_h(&self, term_rows: usize) -> usize {
        let avail = term_rows.saturating_sub(self.chrome.rows_overhead()).max(1);
        self.rows.len().min(avail)
    }

    /// After an arrow move, scroll so the selected row stays visible (a tall
    /// menu/modal must never leave the selection off-screen, where Enter would
    /// run an invisible entry).
    pub fn follow_sel(&mut self, term_rows: usize) {
        let vis_h = self.viewport_h(term_rows);
        if let Some((ri, _)) = self.selected() {
            if ri < self.scroll {
                self.scroll = ri;
            } else if ri >= self.scroll + vis_h {
                self.scroll = ri + 1 - vis_h;
            }
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(vis_h));
    }

    /// After a page/wheel scroll, pull the selection onto a visible row, so a
    /// subsequent Enter can never execute an off-screen target.
    pub fn clamp_sel_to_view(&mut self, term_rows: usize) {
        let vis_h = self.viewport_h(term_rows);
        let scroll = self.scroll.min(self.rows.len().saturating_sub(vis_h));
        let (lo, hi) = (scroll, scroll + vis_h);
        if let Some((ri, _)) = self.selected() {
            if ri < lo || ri >= hi {
                if let Some(idx) = self.targets().iter().position(|(r, _)| *r >= lo && *r < hi) {
                    self.sel = idx;
                }
            }
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
                    roles: vec![],
                },
                PopupRow::Rule => RenderedLine {
                    text: "─".repeat(width),
                    header: false,
                    sel_span: None,
                    hits: vec![],
                    roles: vec![],
                },
                PopupRow::FullWidth(s) => {
                    let ti = target_idx;
                    target_idx += 1;
                    RenderedLine {
                        text: pad(&format!(" {s}"), width),
                        header: false,
                        sel_span: (sel == Some((ri, 0))).then_some((0, width)),
                        hits: vec![(ti, 0, width)],
                        roles: vec![],
                    }
                }
                PopupRow::Entry { glyph, label, hint } => {
                    // The right column is EXACT; the left one ellipsizes. Padding
                    // the whole row and letting `pad` clip from the right ate the
                    // hint on a narrow modal, and in the key modal the hint is the
                    // stable action id an operator types into `config.mux.keys`.
                    // A clipped `grab-…` there is worse than absent, because it
                    // still looks like an id. The label is prose and survives
                    // clipping as something a reader can still recognise.
                    let hint_w = hint.chars().count();
                    let left = format!(" {glyph} {label}");
                    let text = if hint_w == 0 {
                        pad(&left, width)
                    } else {
                        let room = width.saturating_sub(hint_w + 2);
                        pad(&format!("{} {hint} ", pad(&left, room)), width)
                    };
                    let ti = target_idx;
                    target_idx += 1;
                    RenderedLine {
                        text,
                        header: false,
                        sel_span: (sel == Some((ri, 0))).then_some((0, width)),
                        hits: vec![(ti, 0, width)],
                        roles: vec![],
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
                        roles: vec![],
                    }
                }
            };
            lines.push(line);
        }

        // Window the BODY to the space the chrome leaves (terminal rows minus
        // the frame overhead), then frame it. The flip in `origin` is computed
        // on the FRAMED height - computing it on the body height alone would put
        // the bottom border off-screen, which is the single easiest bug to ship
        // here (an anchored menu near the bottom edge flips above its anchor).
        let body_total = lines.len();
        let body_vis_h = self.viewport_h(trows);
        let scroll = self.scroll.min(body_total.saturating_sub(body_vis_h));
        let windowed: Vec<RenderedLine> = lines[scroll..scroll + body_vis_h].to_vec();
        let scroll_state = (body_total > body_vis_h).then_some(Scroll {
            pos: scroll,
            total: body_total,
            visible: body_vis_h,
        });
        // Hand the body to chrome as BodyLines; frame() shifts hit offsets past
        // the left border and adds the chrome rows + scrollbar column.
        let body: Vec<BodyLine> = windowed
            .iter()
            .map(|l| BodyLine {
                text: l.text.clone(),
                header: l.header,
                sel_span: l.sel_span,
                hits: l.hits.clone(),
            })
            .collect();
        let framed = chrome::frame(&body, &self.chrome, width, scroll_state);
        let total_h = framed.lines.len();
        let origin = origin(self.anchor, framed.width, total_h, (trows, tcols));
        // Convert framed lines back to RenderedLines (roles carry styling; hits
        // are now in framed coordinates).
        let lines = framed
            .lines
            .into_iter()
            .map(|fl: FramedLine| RenderedLine {
                text: fl.text,
                header: false,
                sel_span: None,
                hits: fl.hits,
                roles: fl.roles,
            })
            .collect();
        Rendered {
            origin,
            width: framed.width,
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

/// Draw a laid-out, framed popup into the screen cell buffer. Each cell is
/// colored by its [`Role`] (set by [`chrome::frame`]) against `theme`: under the
/// `terminal` theme that is Default + INVERSE/BOLD/DIM (byte-identical to the
/// pre-chrome inverse block), under a named theme the chrome takes the palette.
/// Cell-bounds-checked, so a popup near an edge clips rather than panicking.
pub fn draw(cells: &mut [Cell], rows: usize, cols: usize, r: &Rendered, theme: &Theme) {
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
            let role = line.roles.get(j).copied().unwrap_or(Role::Body);
            let (fg, bg, flags) = cell_style(role, theme);
            cells[sr * cols + sc] = Cell {
                c: ch,
                fg,
                bg,
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
        // Centered = Full chrome: top border + 2 body rows + bottom border.
        assert_eq!(r.lines.len(), 4);
        // The body rows sit after the top border. The first entry is selected by
        // default, so its body cells carry BodySel; the second row's do not.
        let body0 = &r.lines[1];
        let body1 = &r.lines[2];
        assert!(
            body0.roles.contains(&Role::BodySel),
            "selected row's cells are BodySel"
        );
        assert!(body1.roles.iter().all(|&role| role != Role::BodySel));
        // Each body row reports one hit, offset past the left border (+1).
        assert_eq!(body0.hits.len(), 1);
        assert_eq!(body0.hits[0].0, 0);
        assert_eq!(
            body0.hits[0].1, 1,
            "hit offset shifted past the left border"
        );
        assert_eq!(body1.hits[0].0, 1);
    }

    #[test]
    fn render_windows_and_scrolls_a_tall_block() {
        let rows: Vec<PopupRow> = (0..20)
            .map(|i| entry("x", &format!("row{i}"), ""))
            .collect();
        let mut p = Popup::new(rows, Anchor::Center);
        // Terminal 6 rows; Full chrome overhead is 2, so 4 body rows show and the
        // framed block is 6 lines. Body rows start after the top border.
        let r = p.render((6, 80));
        assert_eq!(r.lines.len(), 6);
        assert!(r.lines[1].text.contains("row0"));
        assert!(r.lines[4].text.contains("row3"));
        // Scroll down 5: the body window starts at row5.
        p.scroll_by(5);
        let r = p.render((6, 80));
        assert!(r.lines[1].text.contains("row5"));
        // Over-scroll clamps to the last screenful (body rows 16..20).
        p.scroll_by(100);
        let r = p.render((6, 80));
        assert!(
            r.lines[1].text.contains("row16"),
            "clamped to last screenful"
        );
        assert!(r.lines[4].text.contains("row19"));
    }

    #[test]
    fn follow_sel_scrolls_to_keep_the_selection_visible() {
        // codex P2: a tall menu/modal must scroll so the selected row stays on
        // screen (else Enter runs an invisible entry).
        let rows: Vec<PopupRow> = (0..12).map(|i| entry("x", &format!("r{i}"), "")).collect();
        let mut p = Popup::new(rows, Anchor::Center);
        // Terminal 5 rows tall. Walk selection down past the fold; scroll follows.
        for _ in 0..8 {
            p.nav(NavDir::Down);
            p.follow_sel(5);
        }
        let (ri, _) = p.selected().unwrap();
        assert_eq!(ri, 8);
        let r = p.render((5, 80));
        // The selected row r8 must appear among the rendered body rows (the
        // chrome overhead leaves 3 body rows of the 5-row terminal). An invisible
        // selection is an invisible Enter target - the failure follow_sel prevents.
        let body_start = 1;
        let body_end = r.lines.len().saturating_sub(1);
        assert!(
            r.lines[body_start..body_end]
                .iter()
                .any(|l| l.text.contains("r8")),
            "selected row stays in the viewport after follow_sel"
        );
    }

    #[test]
    fn clamp_sel_to_view_pulls_selection_onto_a_visible_row_after_paging() {
        // codex P2: PageDown moves scroll only; clamp then pulls the selection
        // onto a visible row so a following Enter can't run an off-screen target.
        let rows: Vec<PopupRow> = (0..12).map(|i| entry("x", &format!("r{i}"), "")).collect();
        let mut p = Popup::new(rows, Anchor::Center);
        assert_eq!(p.selected(), Some((0, 0)));
        p.scroll_by(6); // page down
        p.clamp_sel_to_view(5);
        let (ri, _) = p.selected().unwrap();
        assert!(ri >= p.scroll, "selection moved into the scrolled viewport");
    }

    #[test]
    fn render_grid_line_reports_a_hit_per_cell() {
        let p = Popup::new(vec![grid(&["l", "r"])], Anchor::Center);
        let r = p.render((24, 80));
        // Full chrome: top border + 1 grid body row + bottom border.
        assert_eq!(r.lines.len(), 3);
        // The grid body row (after the top border) reports one hit per cell.
        let body = &r.lines[1];
        assert_eq!(body.hits.len(), 2, "one hit target per grid cell");
        // The two cells occupy disjoint, adjacent spans, offset past the border.
        let (_, off0, len0) = body.hits[0];
        let (_, off1, _) = body.hits[1];
        assert_eq!(off0, 1, "first cell past the left border");
        assert_eq!(off1, off0 + len0, "cells are disjoint and adjacent");
    }

    #[test]
    fn contains_distinguishes_an_in_block_miss_from_off_block() {
        // The click router's guard: a click inside the block that hits no target
        // (a header, a border) must read as "inside" so it is swallowed, while a
        // click off the block reads as "outside" so it dismisses.
        let p = Popup::new(
            vec![PopupRow::Header("h".into()), entry("a", "one", "")],
            Anchor::Center,
        );
        let r = p.render((24, 80));
        let (r0, c0) = r.origin;
        // The top border row, leftmost col: inside the block, no target there.
        assert!(r.contains(r0 as u16, c0 as u16));
        // A cell well outside the centered block.
        assert!(!r.contains(0, 0));
    }

    #[test]
    fn terminal_theme_renders_only_default_colors() {
        // The load-bearing byte-identity property: under the terminal theme,
        // drawing a popup writes Default fg/bg on every cell (the flags do all
        // the visual work), so a pre-chrome render is unchanged. A positive
        // marker (the border was drawn) plus the all-Default assertion - never
        // an absence alone, which cannot tell "no color" from "nothing ran".
        let p = Popup::new(vec![entry("a", "one", "x")], Anchor::Center).title("T");
        let r = p.render((24, 80));
        let theme = Theme::default_theme();
        let mut cells = vec![Cell::default(); 24 * 80];
        draw(&mut cells, 24, 80, &r, &theme);
        // Positive control: the popup drew its top-left border corner.
        assert!(cells.iter().any(|c| c.c == '┌'), "drew the border");
        // Byte-identity: every cell is Default-colored.
        for c in cells.iter() {
            assert_eq!(c.fg, crate::proto::Color::Default);
            assert_eq!(c.bg, crate::proto::Color::Default);
        }
    }
}
