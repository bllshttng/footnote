//! The shared anchored/centered popup overlay widget (US1): the single
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
//! (cover the middle, no bleed) without a real bg color.
//! The selected target renders as a normal-video cut-out in the inverse block.

use crate::chrome::{self, BodyLine, Chrome, FramedLine, Scroll};
use crate::proto::{Cell, Color};
use crate::theme::{Role, Theme};

/// Popup content never renders wider than this (fixed max width); longer
/// lines ellipsize. Anchored menus are usually far narrower.
pub const WIDTH_CAP: usize = 60;

/// Cells of air between a row's right-most content and the right border: the
/// two-cell inset the review asked for, so key hints and clipped hints never
/// touch the edge.
const BODY_RIGHT_GAP_COLS: usize = 2;

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
    /// A read-only field row: the label in the fixed key column (accent, the
    /// bold left column a field list reads by), the value plain. No target
    /// and no hit span: a modal's provenance text, never an action.
    Info { label: String, value: String },
    /// A selectable action: glyph + label + right-aligned key hint.
    Entry {
        glyph: String,
        label: String,
        hint: String,
        /// `false` for a greyed, inert entry (LD7: state-blocked is greyed with
        /// the reason carried as `hint`; config-off is absent, never greyed).
        /// `cells()` returns 0, so arrows skip it, Enter cannot fire, and a click
        /// is swallowed rather than dismissed.
        enabled: bool,
    },
    /// A selectable full-width entry (e.g. "New Tab" spanning the block top).
    FullWidth(String),
    /// A row of selectable grid cells (the 2x2 split block = two Grid rows).
    Grid(Vec<GridCell>),
    /// A selectable entry with a literal-color swatch beside the label (the
    /// settings Colors tab). The color is content, not chrome: it paints
    /// through `Role::Swatch` and never shifts with the theme.
    SwatchEntry {
        glyph: String,
        label: String,
        hint: String,
        /// Same contract as `Entry::enabled`.
        enabled: bool,
        color: Color,
    },
}

impl PopupRow {
    /// How many selectable targets this row contributes.
    fn cells(&self) -> usize {
        match self {
            PopupRow::Grid(cells) => cells.len(),
            // A disabled Entry is inert: 0 cells means targets() never lists it,
            // nav() skips it, selected() cannot land on it, and render() emits no
            // hit span so a click resolves to no target (then swallowed by
            // Rendered::contains rather than dismissing the menu). All three
            // reachable paths, not one.
            PopupRow::Entry { enabled: false, .. }
            | PopupRow::SwatchEntry { enabled: false, .. } => 0,
            PopupRow::Entry { .. } | PopupRow::SwatchEntry { .. } | PopupRow::FullWidth(_) => 1,
            PopupRow::Header(_) | PopupRow::Rule | PopupRow::Info { .. } => 0,
        }
    }
}

/// Wrap rows wider than `w` content columns instead of ellipsizing them. A
/// Header splits at word bounds into several Headers; a plain-body Entry keeps
/// its key and the label's first line, and the rest of the label follows as
/// inert continuation Headers indented under the label column. Returns each
/// output row's source index, so a caller with a parallel vector (a modal's
/// row events) can follow the rows.
pub fn wrap_rows(rows: Vec<PopupRow>, w: usize) -> (Vec<PopupRow>, Vec<usize>) {
    let kw = rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { glyph, .. } => Some(chrome::str_cols(glyph)),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let wrap = |text: &str, width: usize| {
        let mut out = Vec::new();
        crate::client::wrap_line(text, width.max(1), &mut out);
        out
    };
    let mut out = Vec::with_capacity(rows.len());
    let mut src = Vec::with_capacity(rows.len());
    for (i, row) in rows.into_iter().enumerate() {
        match row {
            // A Header renders as " {s}" with two cells of air after it.
            PopupRow::Header(s) if chrome::str_cols(&s) + 2 > w => {
                for line in wrap(&s, w.saturating_sub(2)) {
                    out.push(PopupRow::Header(line));
                    src.push(i);
                }
            }
            // An Entry: pad + key column + gap + label + air in a plain body,
            // glyph + space + label + gap + air otherwise; kw + 5 covers both.
            PopupRow::Entry {
                glyph,
                label,
                hint,
                enabled,
            } if kw + 5 + chrome::str_cols(&label) > w => {
                let label_w = w.saturating_sub(kw + 5);
                let mut lines = wrap(&label, label_w).into_iter();
                out.push(PopupRow::Entry {
                    glyph,
                    label: lines.next().unwrap_or_default(),
                    hint,
                    enabled,
                });
                src.push(i);
                for line in lines {
                    out.push(PopupRow::Header(format!("{}{line}", " ".repeat(kw + 1))));
                    src.push(i);
                }
            }
            row => {
                out.push(row);
                src.push(i);
            }
        }
    }
    (out, src)
}

/// Menu glyphs must stay in the BMP. Astral symbols render as tofu on
/// terminals whose fonts lack the supplemental-plane glyph, which makes a
/// destructive action look absent rather than visibly unsupported.
pub fn menu_glyph_is_bmp(glyph: &str) -> bool {
    !glyph.is_empty() && glyph.chars().all(|ch| (ch as u32) <= 0xffff)
}

fn validate_menu_glyphs(rows: &[PopupRow]) {
    for row in rows {
        match row {
            PopupRow::Entry { glyph, .. } | PopupRow::SwatchEntry { glyph, .. } => assert!(
                menu_glyph_is_bmp(glyph),
                "menu glyph must be a non-empty BMP string: {glyph:?}"
            ),
            PopupRow::Grid(cells) => {
                for cell in cells {
                    assert!(
                        menu_glyph_is_bmp(&cell.glyph),
                        "menu glyph must be a non-empty BMP string: {:?}",
                        cell.glyph
                    );
                }
            }
            PopupRow::Header(_)
            | PopupRow::Rule
            | PopupRow::FullWidth(_)
            | PopupRow::Info { .. } => {}
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
    /// Extend selectable entry rows to the inner width required by the
    /// chrome, including a longer footer or title.
    full_width_selection: bool,
    /// The chrome every modal wears. Its level is derived from `anchor` and
    /// private (no setter), so every centered modal is Full and every anchored
    /// menu is Bare with no way for a call site to disagree.
    pub chrome: Chrome,
    /// A floor the caller pins so a TABBED modal keeps one width across its
    /// tabs (tabbed-modal width): the widest tab's content width, capped by
    /// [`WIDTH_CAP`] in [`Popup::render`]. `0` (the default) measures only
    /// this tab's own rows.
    pub min_width: usize,
    /// Opt this popup into the plain-body anatomy: no inverse ground, the key
    /// column bold accent ([`Role::BodyAccent`]), the cursor row the one
    /// filled band ([`Role::BodyCursor`]), section headings accent text. The
    /// which-key modal's shape (the unreadable-inverse-body review).
    pub plain_body: bool,
    /// A viewport ceiling as a percent of terminal rows, `0` (the default) =
    /// uncapped. The which-key modal caps at 60 so a tall table scrolls in a
    /// fixed window instead of growing one row per binding.
    pub body_cap_pct: usize,
    /// Keep the LABEL column whole on a narrow block and clip the HINT
    /// instead. The default protects the hint (the key modal's stable
    /// action id); a picker whose hint is a prose error needs the opposite:
    /// the diagnosis label must never ellipsize.
    pub label_first: bool,
    /// The content width ceiling, [`WIDTH_CAP`] by default. The screen still
    /// caps it in [`Popup::render`].
    pub width_cap: usize,
}

/// One laid-out line ready to draw, plus its style and the selected sub-span
/// (whole line for an Entry/FullWidth, the key column of a plain-body Entry, a
/// single cell for a Grid).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedLine {
    pub text: String,
    /// Whether this line is a greyed, inert `Entry`. Computed once here (the
    /// only place that reads `enabled`) and carried through rather than
    /// re-derived by re-indexing `self.rows` later - two derivations of the
    /// same fact can only agree by coincidence once either side changes.
    pub disabled: bool,
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
    /// `(char_offset, len, role)` spans styling the line's chars. A char no
    /// span covers takes [`RenderedLine::pad_role`]. Set only on a plain-body
    /// popup's rows (the key column, a section heading, the cursor band); the
    /// framed pass translates them into the per-char `roles`.
    pub segs: Vec<(usize, usize, Role)>,
    /// The role for chars no seg covers: the theme ground
    /// ([`Role::PanelBody`]) under every theme - one body treatment.
    pub pad_role: Role,
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
        validate_menu_glyphs(&rows);
        Popup {
            rows,
            chrome: Chrome::new("", anchor),
            anchor,
            sel: 0,
            scroll: 0,
            min_width: 0,
            full_width_selection: false,
            plain_body: false,
            body_cap_pct: 0,
            label_first: false,
            width_cap: WIDTH_CAP,
        }
    }

    /// Raise (or lower) the content width ceiling (see the field doc).
    pub fn width_cap(mut self, w: usize) -> Self {
        self.width_cap = w;
        self
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
    /// Pin the width floor for a tabbed modal (see the field doc).
    pub fn min_width(mut self, w: usize) -> Self {
        self.min_width = w;
        self
    }

    /// Opt this popup into the plain-body anatomy (see the field doc).
    pub fn plain_body(mut self) -> Self {
        self.plain_body = true;
        self
    }

    /// Cap the viewport at a percent of terminal rows (see the field doc).
    pub fn body_cap_pct(mut self, pct: usize) -> Self {
        self.body_cap_pct = pct;
        self
    }

    /// Keep the label column whole and clip the hint instead (see the field
    /// doc).
    pub fn label_first(mut self) -> Self {
        self.label_first = true;
        self
    }

    /// The content width this popup's own rows measure to, BEFORE the
    /// clamp/frame: the number a tabbed caller takes as one tab's
    /// contribution to the shared floor.
    pub fn content_width(&self) -> usize {
        self.measure_content_w()
    }

    /// Opt this anchored menu into the full chrome (title row + footer row).
    /// `Anchor::At` fixes `Level::Bare` - border-only, the selector menus'
    /// shape - but the composer's pickers carry their grammar in words: the
    /// live filter query rides the title and the list keys the footer, so
    /// both rows must render while the menu stays anchored to its chip.
    pub fn full_chrome(mut self) -> Self {
        self.chrome = self.chrome.full();
        self
    }

    /// Make entry hit targets and selection spans fill the rendered inner
    /// width. Useful when the footer is wider than every row.
    pub fn full_width_selection(mut self) -> Self {
        self.full_width_selection = true;
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
        let avail = if self.body_cap_pct > 0 {
            avail.min((term_rows * self.body_cap_pct / 100).max(1))
        } else {
            avail
        };
        self.rows.len().min(avail)
    }

    /// The fixed key-column width for a plain-body popup: the widest entry
    /// glyph or Info label, so descriptions start on one column. Non-plain
    /// popups never call it.
    fn key_col_w(&self) -> usize {
        self.rows
            .iter()
            .filter_map(|r| match r {
                PopupRow::Entry { glyph, .. } | PopupRow::SwatchEntry { glyph, .. } => {
                    Some(chrome::str_cols(glyph))
                }
                PopupRow::Info { label, .. } => Some(chrome::str_cols(label)),
                _ => None,
            })
            .max()
            .unwrap_or(0)
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

    /// Per-cell width for a grid row: the widest cell content + padding.
    fn grid_cell_w(&self) -> usize {
        self.rows
            .iter()
            .flat_map(|r| match r {
                PopupRow::Grid(cells) => cells
                    .iter()
                    .map(|c| chrome::str_cols(&c.glyph) + chrome::str_cols(&c.label) + 3)
                    .collect::<Vec<_>>(),
                _ => vec![],
            })
            .max()
            .unwrap_or(0)
    }

    /// The widest row before padding (the content width). Shared by
    /// [`Popup::render`] and [`Popup::content_width`].
    fn measure_content_w(&self) -> usize {
        let kw = if self.plain_body { self.key_col_w() } else { 0 };
        self.rows
            .iter()
            .map(|r| match r {
                PopupRow::Header(s) | PopupRow::FullWidth(s) => chrome::str_cols(s) + 2,
                PopupRow::Rule => 0,
                PopupRow::Info { label: _, value } => 1 + kw + 1 + chrome::str_cols(value) + 2,
                PopupRow::Entry {
                    glyph, label, hint, ..
                } => {
                    if self.plain_body {
                        // The two-column shape: leading pad + fixed key column
                        // + gap + label; the hint column is dropped.
                        1 + kw + 1 + chrome::str_cols(label) + 2
                    } else {
                        // glyph + space + label + gap + hint
                        chrome::str_cols(glyph)
                            + 1
                            + chrome::str_cols(label)
                            + 2
                            + chrome::str_cols(hint)
                            + 2
                    }
                }
                PopupRow::Grid(cells) => self.grid_cell_w() * cells.len(),
                PopupRow::SwatchEntry {
                    glyph, label, hint, ..
                } => {
                    let _ = hint;
                    if self.plain_body {
                        // Two-column shape + the two swatch cells.
                        1 + kw + 1 + 2 + 1 + chrome::str_cols(label) + 2
                    } else {
                        chrome::str_cols(glyph) + 1 + chrome::str_cols(label) + 2 + 2 + 2
                    }
                }
            })
            .max()
            .unwrap_or(0)
    }

    /// Lay the popup out against a `(rows, cols)` terminal: compute the block
    /// width from its content, position it (centered or clamped/flipped anchor),
    /// and render each row to a padded line with selection + hit-test spans.
    pub fn render(&self, term: (u16, u16)) -> Rendered {
        let (trows, tcols) = (term.0.max(1) as usize, term.1.max(1) as usize);
        let sel = self.selected();
        // Content width: the widest row before padding, raised to the caller's
        // tabbed floor and the chrome's own minimum (title/footer/tabs), so
        // the frame never pads the extra columns with body background the
        // selection span does not cover (popup width rules).
        let content_w = self
            .measure_content_w()
            .max(self.min_width)
            .max(self.chrome.min_inner_w());
        // The framer paints its two pad cells INSIDE the body width, so the
        // framed block spans `width + 4`: cap the builder width to the
        // terminal minus the borders and the pad, or the right border leaves
        // a full-width screen.
        let cap = self
            .width_cap
            .min(tcols.saturating_sub(chrome::Chrome::FRAME_COLS * 2).max(1));
        let width = if self.full_width_selection {
            content_w
                .min(cap)
                .max(self.chrome.min_inner_w().min(tcols))
                .max(1)
        } else {
            content_w.clamp(1, cap)
        };

        let mut target_idx = 0usize;
        let mut lines = Vec::with_capacity(self.rows.len());
        let kw = self.key_col_w();
        for (ri, row) in self.rows.iter().enumerate() {
            let line = match row {
                PopupRow::Header(s) => {
                    let text = pad(&format!(" {s}"), width);
                    // A section heading is accent TEXT on the theme ground,
                    // never a band; an empty header is the inter-section
                    // spacer line. One body treatment under every theme.
                    let segs = (!s.is_empty())
                        .then(|| vec![(0usize, text.chars().count(), Role::BodyAccent)])
                        .unwrap_or_default();
                    RenderedLine {
                        text,
                        disabled: false,
                        sel_span: None,
                        hits: vec![],
                        roles: vec![],
                        segs,
                        pad_role: Role::PanelBody,
                    }
                }
                PopupRow::Rule => RenderedLine {
                    text: "─".repeat(width),
                    disabled: false,
                    sel_span: None,
                    hits: vec![],
                    roles: vec![],
                    segs: vec![],
                    pad_role: Role::PanelMeta,
                },
                PopupRow::Info { label, value } => {
                    // The plain two-column shape, inert: the label sits in the
                    // fixed key column as accent text (the bold left column a
                    // field list reads by), the value plain. No hit span and
                    // no selection: provenance text, never an action.
                    let text = pad(&format!(" {} {}", pad(label, kw), value), width);
                    RenderedLine {
                        text,
                        disabled: false,
                        sel_span: None,
                        hits: vec![],
                        roles: vec![],
                        segs: vec![(1usize, kw, Role::BodyAccent)],
                        pad_role: Role::PanelBody,
                    }
                }
                PopupRow::FullWidth(s) => {
                    let ti = target_idx;
                    target_idx += 1;
                    RenderedLine {
                        text: pad(&format!(" {s}"), width),
                        disabled: false,
                        sel_span: (sel == Some((ri, 0))).then_some((0, width)),
                        hits: vec![(ti, 0, width)],
                        roles: vec![],
                        segs: vec![],
                        pad_role: Role::PanelBody,
                    }
                }
                PopupRow::Entry {
                    glyph,
                    label,
                    hint,
                    enabled,
                } => {
                    let disabled = !*enabled;
                    let selected = !disabled && sel == Some((ri, 0));
                    // Plain-body: the two-column shape. The key column is
                    // fixed-width accent text, the label plain, the hint
                    // column dropped. The cursor row is the one filled band,
                    // riding the segs: a sel_span would paint the inverse
                    // cut-out, invisible on the plain ground.
                    let (text, segs, pad_role, sel_span) = if self.plain_body {
                        let left = format!(" {} {}", pad(glyph, kw), label);
                        let text = pad(&left, width);
                        let chars = text.chars().count();
                        let segs = if disabled {
                            vec![]
                        } else if selected {
                            vec![(0usize, chars, Role::BodyCursor)]
                        } else {
                            // Char 0 is the leading pad; the key chars carry
                            // the accent span.
                            vec![(1usize, kw, Role::BodyAccent)]
                        };
                        (
                            text,
                            segs,
                            if selected {
                                Role::BodyCursor
                            } else {
                                Role::PanelBody
                            },
                            None,
                        )
                    } else if self.label_first {
                        // The LEFT column is exact; the hint clips. A picker's
                        // hint is prose (a refusal's reason, a route), and the
                        // label is the diagnosis - the reverse of the key
                        // modal, whose hint is the stable id.
                        let left = format!(" {glyph} {label}");
                        let left_w = chrome::str_cols(&left).min(width);
                        // The hint never touches the right border: two cells
                        // of air stay between the last column and the edge.
                        let room = width.saturating_sub(left_w + BODY_RIGHT_GAP_COLS);
                        let hint_text = clip_cols(hint, room);
                        let gap = width
                            .saturating_sub(left_w + chrome::str_cols(&hint_text))
                            .max(BODY_RIGHT_GAP_COLS);
                        let mut text = left;
                        text.push_str(&" ".repeat(gap));
                        text.push_str(&hint_text);
                        (
                            pad(&text, width),
                            vec![],
                            Role::PanelBody,
                            (!disabled && selected).then_some((0, width)),
                        )
                    } else {
                        // The right column is EXACT; the left one ellipsizes. Padding
                        // the whole row and letting `pad` clip from the right ate the
                        // hint on a narrow modal, and in the key modal the hint is the
                        // stable action id an operator types into `config.mux.keys`.
                        // A clipped `grab-…` there is worse than absent, because it
                        // still looks like an id. The label is prose and survives
                        // clipping as something a reader can still recognise.
                        // The hint keeps the two-cell air before the border.
                        let hint_w = chrome::str_cols(hint);
                        let left = format!(" {glyph} {label}");
                        let text = if hint_w == 0 {
                            pad(&left, width)
                        } else {
                            let room = width.saturating_sub(hint_w + 1 + BODY_RIGHT_GAP_COLS);
                            pad(&format!("{} {hint} ", pad(&left, room)), width)
                        };
                        (
                            text,
                            vec![],
                            Role::PanelBody,
                            (!disabled && selected).then_some((0, width)),
                        )
                    };
                    // A disabled entry contributes no target and no hit span, so
                    // nav()/selected() (which read targets()) and the click router
                    // (which reads hits) both pass over it. Its row still renders,
                    // greyed via BodyDim, carrying the reason as the hint.
                    let hits = if disabled {
                        Vec::new()
                    } else {
                        let ti = target_idx;
                        target_idx += 1;
                        vec![(ti, 0, width)]
                    };
                    RenderedLine {
                        text,
                        disabled,
                        sel_span,
                        hits,
                        roles: vec![],
                        segs,
                        pad_role,
                    }
                }
                PopupRow::SwatchEntry {
                    glyph,
                    label,
                    hint,
                    enabled,
                    color,
                } => {
                    let disabled = !*enabled;
                    let selected = !disabled && sel == Some((ri, 0));
                    // The plain-body two-column shape with a two-cell swatch
                    // between the key column and the label; the swatch paints
                    // through Role::Swatch, so the color is content. The
                    // cursor row keeps the swatch cells literal by splitting
                    // its cursor seg around them.
                    let swatch_start = 1 + kw + 1;
                    let text = if self.plain_body {
                        pad(
                            &format!(" {} {} {}", pad(glyph, kw), "\u{2588}\u{2588}", label),
                            width,
                        )
                    } else {
                        // Defensive: no non-plain surface builds this row
                        // today; render it as a plain entry with no swatch.
                        let room = width.saturating_sub(chrome::str_cols(hint) + 2);
                        pad(
                            &format!("{} {hint} ", pad(&format!(" {glyph} {label}"), room)),
                            width,
                        )
                    };
                    let chars = text.chars().count();
                    let mut segs = Vec::new();
                    if !disabled && self.plain_body {
                        if selected {
                            let tail = chars.saturating_sub(swatch_start + 2);
                            segs.push((0usize, swatch_start, Role::BodyCursor));
                            segs.push((swatch_start, 2usize, Role::Swatch(*color)));
                            segs.push((swatch_start + 2, tail, Role::BodyCursor));
                        } else {
                            // The accent marks the CHOSEN row only: the
                            // builder's `●` glyph. Unchosen radios read as
                            // ordinary text, never a wall of brand marks.
                            if glyph == "\u{25cf}" {
                                segs.push((1usize, kw, Role::BodyAccent));
                            }
                            segs.push((swatch_start, 2usize, Role::Swatch(*color)));
                        }
                    }
                    let hits = if disabled {
                        Vec::new()
                    } else {
                        let ti = target_idx;
                        target_idx += 1;
                        vec![(ti, 0, width)]
                    };
                    RenderedLine {
                        text,
                        disabled,
                        sel_span: None,
                        hits,
                        roles: vec![],
                        segs,
                        pad_role: if selected && self.plain_body {
                            Role::BodyCursor
                        } else {
                            Role::PanelBody
                        },
                    }
                }
                PopupRow::Grid(cells) => {
                    let mut text = String::new();
                    let mut hits = Vec::new();
                    let mut sel_span = None;
                    let gcw = self.grid_cell_w();
                    for (ci, c) in cells.iter().enumerate() {
                        let cell = center(&format!("{} {}", c.glyph, c.label), gcw);
                        let off = ci * gcw;
                        hits.push((target_idx, off, gcw));
                        if sel == Some((ri, ci)) {
                            sel_span = Some((off, gcw));
                        }
                        target_idx += 1;
                        text.push_str(&cell);
                    }
                    RenderedLine {
                        text: pad(&text, width),
                        disabled: false,
                        sel_span,
                        hits,
                        roles: vec![],
                        segs: vec![],
                        pad_role: Role::PanelBody,
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
        // the left border and adds the chrome rows + scrollbar column. The
        // body width arrives pad-inclusive: frame() paints the two side pads
        // inside it, so the builder rows padded to `width` survive whole.
        let body: Vec<BodyLine> = windowed
            .iter()
            .map(|l| BodyLine {
                segs: l.segs.clone(),
                pad_role: l.pad_role,
                text: l.text.clone(),
                disabled: l.disabled,
                sel_span: l.sel_span,
                hits: l.hits.clone(),
            })
            .collect();
        let framed = chrome::frame(
            &body,
            &self.chrome,
            width + chrome::Chrome::FRAME_COLS,
            scroll_state,
        );
        let total_h = framed.lines.len();
        let origin = origin(self.anchor, framed.width, total_h, (trows, tcols));
        // Convert framed lines back to RenderedLines (roles carry styling; hits
        // are now in framed coordinates).
        let lines = framed
            .lines
            .into_iter()
            .map(|fl: FramedLine| RenderedLine {
                text: fl.text,
                disabled: false,
                sel_span: None,
                hits: fl.hits,
                roles: fl.roles,
                segs: vec![],
                pad_role: Role::PanelBody,
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

/// Truncate to `w` display columns (ellipsizing) and pad with spaces to `w`, so
/// a line is a fixed-width block that fully overwrites the content beneath it.
/// Measured in terminal columns, not chars: a fullwidth glyph is one char and
/// two cells. The cut is [`crate::chrome::fit_ellipsis`], the one ellipsis
/// rule; the pad stays local so the widget is self-contained.
fn pad(s: &str, w: usize) -> String {
    let cols = chrome::str_cols(s);
    if cols > w {
        chrome::fit_ellipsis(s, w)
    } else {
        let mut t = s.to_string();
        t.push_str(&" ".repeat(w - cols));
        t
    }
}

/// Clip `s` to `w` display columns, ellipsizing when the cut removes
/// anything (the label-first hint column's cut; [`pad`] pads to a fixed
/// block instead).
fn clip_cols(s: &str, w: usize) -> String {
    if chrome::str_cols(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    chrome::fit_ellipsis(s, w)
}

/// Center `s` within `w` display columns (space padded); truncates via [`pad`]
/// when too wide.
fn center(s: &str, w: usize) -> String {
    let cols = chrome::str_cols(s);
    if cols >= w {
        return pad(s, w);
    }
    let left = (w - cols) / 2;
    let right = w - cols - left;
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
        chrome::paint_line(
            cells,
            rows,
            cols,
            r0 + i,
            c0,
            &line.text,
            &line.roles,
            theme,
        );
        chrome::record_close_spans((rows, cols), (r0 + i, c0), r.width, &line.hits);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_glyph_guard_rejects_astral_tofu_risk() {
        assert!(menu_glyph_is_bmp("♺"));
        assert!(!menu_glyph_is_bmp("📄"));
    }

    #[test]
    #[should_panic(expected = "menu glyph must be a non-empty BMP string")]
    fn popup_rejects_astral_menu_glyphs() {
        let _ = Popup::new(vec![entry("📄", "document", "")], Anchor::Center);
    }

    fn entry(g: &str, l: &str, h: &str) -> PopupRow {
        PopupRow::Entry {
            glyph: g.into(),
            label: l.into(),
            hint: h.into(),
            enabled: true,
        }
    }

    #[test]
    fn label_first_keeps_the_label_whole_and_clips_the_hint() {
        // The composer's regression: a disabled row whose LABEL names the
        // failure must not ellipsize so a long prose hint can paint. The
        // default keeps the hint whole instead; both shapes hold.
        let rows = vec![disabled(
            "\u{2022}",
            "model list unavailable",
            "account records unavailable: Usage: fno-py config get [OPTIONS] {key}",
        )];
        let entry_line = |p: &Popup| -> String {
            p.render((24, 120))
                .lines
                .iter()
                .map(|l| l.text.clone())
                .find(|t| t.contains("unavailable"))
                .expect("the entry row renders")
        };
        let wide_hint = Popup::new(rows.clone(), Anchor::At { row: 1, col: 1 })
            .full_chrome()
            .full_width_selection();
        let text = entry_line(&wide_hint);
        assert!(
            !text.contains("model list unavailable"),
            "the default protects the hint and clips the label: {text:?}"
        );
        let label_whole = Popup::new(rows, Anchor::At { row: 1, col: 1 })
            .full_chrome()
            .full_width_selection()
            .label_first();
        let text = entry_line(&label_whole);
        assert!(
            text.contains("model list unavailable"),
            "label_first keeps the label whole: {text:?}"
        );
        assert!(!text.contains("{key}"), "the long hint clips: {text:?}");
    }

    fn disabled(g: &str, l: &str, reason: &str) -> PopupRow {
        PopupRow::Entry {
            glyph: g.into(),
            label: l.into(),
            hint: reason.into(),
            enabled: false,
        }
    }

    #[test]
    fn body_rows_keep_two_cells_of_air_before_the_right_border() {
        // The right column is EXACT; its hint ends two cells shy of the
        // border. The label-first shape holds the same gap after its clip.
        let rows = vec![entry("\u{25cf}", "run", "grab-x")];
        let p = Popup::new(rows, Anchor::At { row: 1, col: 1 })
            .full_chrome()
            .full_width_selection();
        let rendered = p.render((24, 120));
        let line = rendered
            .lines
            .iter()
            .find(|l| l.text.contains("grab-x"))
            .expect("the entry row renders");
        let text: Vec<char> = line.text.chars().collect();
        let hint_end = text.iter().rposition(|&c| c == 'x').unwrap();
        let width = text.len();
        assert!(
            width - 1 - hint_end >= 2,
            "hint must end two cells before the border (width {width}, hint at {hint_end})"
        );
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
    fn targets_skip_disabled_entries() {
        let p = Popup::new(
            vec![
                entry("a", "one", ""),
                disabled("d", "dimmed", "no plan"),
                entry("b", "two", ""),
            ],
            Anchor::Center,
        );
        // The disabled row contributes no target; the two selectable ones are
        // rows 0 and 2, never row 1.
        assert_eq!(p.targets(), vec![(0, 0), (2, 0)]);
        let mut p = p;
        assert_eq!(p.selected(), Some((0, 0)));
        p.nav(NavDir::Down);
        assert_eq!(p.selected(), Some((2, 0)), "arrow skips the disabled row");
    }

    #[test]
    fn disabled_entry_is_inert_to_click_and_keeps_menu_open() {
        // A disabled entry renders a line (greyed) carrying the reason as its
        // hint, but emits no hit span: a click on its cells resolves to no
        // target, and because the cells are still inside the block
        // (Rendered::contains) the router swallows the click rather than
        // dismissing. The two preconditions for "click leaves the menu open".
        let p = Popup::new(
            vec![entry("a", "one", ""), disabled("d", "dimmed", "no plan")],
            Anchor::Center,
        );
        let r = p.render((24, 80));
        // Full chrome: [0]=top border, [1]=body "one", [2]=body "dimmed",
        // [3]=bottom border.
        let disabled_line = &r.lines[2];
        assert!(disabled_line.text.contains("no plan"), "reason renders");
        assert!(
            disabled_line.hits.is_empty(),
            "disabled row has no hit target"
        );
        assert!(
            disabled_line.roles.contains(&Role::BodyDim),
            "disabled row greys via BodyDim"
        );
        // Every non-border cell on the disabled row is BodyDim: it carries no
        // normal (Body) or selected (BodySel) cell a selectable row would.
        assert!(
            disabled_line
                .roles
                .iter()
                .all(|&role| matches!(role, Role::BodyDim | Role::Border)),
            "disabled row has no selectable body cells"
        );
        // The cells are still inside the block, so the router reads an in-block
        // miss (swallow), not an off-block click (dismiss).
        let (r0, c0) = r.origin;
        assert!(r.contains((r0 + 2) as u16, c0 as u16));
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
    fn the_selection_highlight_spans_the_row() {
        // highlight-span fix:: a footer wider than the rows widens the frame;
        // the selected row's highlight must reach the borders, not stop at
        // the narrower content width.
        let p = Popup::new(
            vec![entry("a", "one", ""), entry("b", "two", "")],
            Anchor::Center,
        )
        .title("settings")
        .footer("tab switches section · esc close");
        let r = p.render((30, 100));
        // Line 1 is the first (selected) body row.
        let body = &r.lines[1];
        let non_border: Vec<&Role> = body.roles.iter().filter(|r| **r != Role::Border).collect();
        assert!(
            !non_border.is_empty() && non_border.iter().all(|r| matches!(r, Role::BodySel)),
            "every body cell of the selected row is highlighted: {:?}",
            body.roles
        );
    }

    #[test]
    fn a_tabbed_modal_keeps_one_width_across_tabs() {
        // tabbed-width fix:: the widest tab's floor pins the modal, so
        // switching tabs never resizes the box. The footer stays short: a
        // long footer widens every fixture equally and masks the difference.
        let narrow = Popup::new(vec![entry("a", "one", "")], Anchor::Center)
            .title("settings")
            .footer("esc close");
        let wide = Popup::new(
            vec![entry("a", "one", "hint-hint-hint"), entry("b", "two", "")],
            Anchor::Center,
        )
        .title("settings")
        .footer("esc close");
        let pinned = Popup::new(vec![entry("a", "one", "")], Anchor::Center)
            .title("settings")
            .footer("esc close")
            .min_width(wide.content_width());
        let w_narrow = narrow.render((30, 100)).width;
        let w_wide = wide.render((30, 100)).width;
        let w_pinned = pinned.render((30, 100)).width;
        assert!(w_wide > w_narrow, "the fixture tabs differ in width");
        assert_eq!(w_pinned, w_wide, "the floor pins the narrow tab wide");
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
        // Border plus the frame's two side pad cells.
        assert_eq!(
            body0.hits[0].1, 3,
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
        // Border plus the frame's two side pad cells.
        assert_eq!(off0, 3, "first cell past the left border");
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
        // The property belongs to the `terminal` theme specifically: it is
        // the no-op whose whole contract is byte-identity, while the default
        // theme paints the chrome.
        let theme = Theme::from_name("terminal").0;
        let mut cells = vec![Cell::default(); 24 * 80];
        draw(&mut cells, 24, 80, &r, &theme);
        // Positive control: the popup drew its top-left border corner.
        assert!(cells.iter().any(|c| c.c == '╭'), "drew the border");
        // Byte-identity except the two named slots the terminal theme still
        // colors: the border role (its own amber field, and any
        // mux.theme.border override) and the brand accent on the key column.
        let border = theme.border;
        for c in cells.iter() {
            assert!(
                c.fg == crate::proto::Color::Default || c.fg == border || c.fg == theme.brand,
                "unexpected fg {:?}",
                c.fg
            );
            assert_eq!(c.bg, crate::proto::Color::Default);
        }
    }
}
