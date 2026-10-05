//! The one style helper the backlog surfaces share. Every backlog line -
//! full-screen board, node detail overlay, the sideline's backlog view -
//! is built from segments here, so rank is expressed ONCE through
//! attributes and palette-following slots (never hard colors, never
//! INVERSE) and every backlog surface inherits it.

use super::super::chrome;
use super::super::theme::{band_style, cell_style, Role, Theme};
use crate::proto::{cell_flags, Cell};

/// What a piece of backlog text is. The mapping through [`role_of`] is the
/// whole style policy: the surfaces pick segments; the policy picks styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BRole {
    Head,
    Label,
    Meta,
    Body,
    Pill,
}

/// One styled segment.
#[derive(Clone)]
pub(crate) struct BSeg {
    pub(crate) text: String,
    pub(crate) role: BRole,
}

/// One styled line: flattened text plus a per-char role walk, and - when
/// this is the cursor's row - the explicit highlight band pair the
/// sideline wears (a real fg/bg pair, zero DIM inside the band).
#[derive(Clone, Debug)]
pub(crate) struct BLine {
    pub(crate) text: String,
    pub(crate) roles: Vec<BRole>,
    /// The role for chars past the role walk (whole-line constructors).
    pub(crate) default_role: BRole,
    pub(crate) band: bool,
}

/// Helper: append one segment's chars.
fn push_seg(text: &mut String, roles: &mut Vec<BRole>, seg: &BSeg) {
    for ch in seg.text.chars() {
        text.push(ch);
        roles.push(seg.role);
    }
}

impl BLine {
    /// Split into lines of at most `w` display columns, breaking after the
    /// last space that fits, or mid-word when none does. Each char keeps its
    /// role.
    pub(crate) fn wrap(self, w: usize) -> Vec<BLine> {
        let w = w.max(1);
        let chars: Vec<char> = self.text.chars().collect();
        if chars.iter().map(|c| char_w(*c)).sum::<usize>() <= w {
            return vec![self];
        }
        let piece = |from: usize, to: usize| BLine {
            text: chars[from..to].iter().collect(),
            roles: self
                .roles
                .get(from..to.min(self.roles.len()))
                .unwrap_or_default()
                .to_vec(),
            default_role: self.default_role,
            band: self.band,
        };
        let mut out = Vec::new();
        let (mut start, mut used, mut space) = (0usize, 0usize, None);
        let mut i = 0usize;
        while i < chars.len() {
            let cw = char_w(chars[i]);
            if used + cw > w && i > start {
                let cut = space.filter(|&s| s > start).unwrap_or(i);
                out.push(piece(start, cut));
                start = cut;
                used = chars[start..i].iter().map(|c| char_w(*c)).sum();
                space = None;
                continue;
            }
            if chars[i] == ' ' {
                space = Some(i + 1);
            }
            used += cw;
            i += 1;
        }
        out.push(piece(start, chars.len()));
        out
    }

    /// Build from segments.
    pub(crate) fn of(segs: &[BSeg]) -> BLine {
        let mut text = String::new();
        let mut roles = Vec::new();
        for seg in segs {
            push_seg(&mut text, &mut roles, seg);
        }
        BLine {
            text,
            roles,
            default_role: BRole::Body,
            band: false,
        }
    }

    /// A plain body line.
    pub(crate) fn plain(s: impl Into<String>) -> BLine {
        BLine {
            text: s.into(),
            roles: Vec::new(),
            default_role: BRole::Body,
            band: false,
        }
    }

    /// An all-meta line (summary, query, errors).
    pub(crate) fn meta(s: impl Into<String>) -> BLine {
        BLine {
            text: s.into(),
            roles: Vec::new(),
            default_role: BRole::Meta,
            band: false,
        }
    }

    /// An all-head line (lane names).
    pub(crate) fn head(s: impl Into<String>) -> BLine {
        BLine {
            text: s.into(),
            roles: Vec::new(),
            default_role: BRole::Head,
            band: false,
        }
    }

    /// Append another line's chars and roles. Text and roles grow in
    /// lockstep: a merged-in plain line contributes no role entries, so the
    /// walk pads the gap with that line's default role - without this, every
    /// later column's roles shift and a header's style lands mid-word.
    pub(crate) fn push_line(&mut self, other: BLine) {
        self.text.push_str(&other.text);
        self.roles.extend(other.roles);
        let chars = self.text.chars().count();
        while self.roles.len() < chars {
            self.roles.push(other.default_role);
        }
    }

    /// Pad with spaces to `w` display columns.
    pub(crate) fn pad_to(self, w: usize) -> BLine {
        let mut line = self;
        let used: usize = line.text.chars().map(char_w).sum();
        for _ in used..w {
            line.text.push(' ');
            line.roles.push(line.default_role);
        }
        line
    }
}

impl std::fmt::Display for BLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// A styled line reads as its text, so string-shaped call sites (tests,
/// notices) keep working unchanged.
impl std::ops::Deref for BLine {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl PartialEq<&str> for BLine {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for BLine {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

/// Display width of one char (fullwidth = 2, else 1).
pub(crate) fn char_w(ch: char) -> usize {
    unicode_width::UnicodeWidthChar::width(ch)
        .unwrap_or(1)
        .max(1)
}

impl BLine {
    /// Truncate to `w` display columns; wide glyphs never straddle the cut.
    pub(crate) fn trunc(self, w: usize) -> BLine {
        let mut used = 0usize;
        let cut = self
            .text
            .char_indices()
            .find(|(_, ch)| {
                used += char_w(*ch);
                used > w
            })
            .map(|(i, _)| i)
            .unwrap_or(self.text.len());
        let keep = self.text[..cut].chars().count();
        BLine {
            text: self.text[..cut].to_string(),
            roles: self.roles.into_iter().take(keep).collect(),
            default_role: self.default_role,
            band: self.band,
        }
    }
}

/// The policy: one role becomes one theme role. A char with no segment
/// role defaults to the plain panel body.
pub(crate) fn role_of(role: BRole) -> Role {
    match role {
        BRole::Head => Role::PanelHead,
        BRole::Label => Role::PanelLabel,
        BRole::Meta => Role::PanelMeta,
        BRole::Body => Role::PanelBody,
        BRole::Pill => Role::PanelPill,
    }
}

/// One painted node-id span in screen cells, recorded at paint time so a
/// press routes to what the last frame actually drew (the esc-chip
/// precedent): the board's card rows and the detail's id and link rows
/// paint node ids as Label segments, and the collector keeps their
/// screen rectangles for the tap.
#[derive(Debug, Clone)]
pub(crate) struct NodeSpan {
    pub(crate) row: usize,
    pub(crate) col: usize,
    pub(crate) len: usize,
    pub(crate) id: String,
}

thread_local! {
    /// The node spans one compose painted, collected between
    /// [`node_spans_begin`] and [`node_spans_end`]; `None` outside them.
    static NODE_SPANS: std::cell::RefCell<Option<Vec<NodeSpan>>> =
        const { std::cell::RefCell::new(None) };
    /// The last compose's recorded spans: the one store a board tap reads.
    /// The client holds one view, so last-compose-wins is the same
    /// freshness the esc-chip store has.
    static PAINTED: std::cell::RefCell<Vec<NodeSpan>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Start collecting the node-id spans the backlog painters put on screen.
pub(crate) fn node_spans_begin() {
    NODE_SPANS.with(|c| *c.borrow_mut() = Some(Vec::new()));
}

/// Stop collecting and park what the frame painted for the taps.
pub(crate) fn node_spans_end() {
    let recorded = NODE_SPANS.with(|c| c.borrow_mut().take().unwrap_or_default());
    PAINTED.with(|p| *p.borrow_mut() = recorded);
}

/// The spans the last compose painted (tests read them to find a target).
#[cfg(test)]
pub(crate) fn painted_spans() -> Vec<NodeSpan> {
    PAINTED.with(|p| p.borrow().clone())
}

/// The node id painted at `(row, col)` by the last compose, if any.
pub(crate) fn span_at(row: usize, col: usize) -> Option<String> {
    PAINTED.with(|p| {
        p.borrow()
            .iter()
            .find(|s| s.row == row && col >= s.col && col < s.col + s.len)
            .map(|s| s.id.clone())
    })
}

/// Whether a Label run reads as a backlog node id: `<prefix>-<hex>` with a
/// hex tail of at least four (`x-0fd1`, `fno-a3f9`). The shape gate is what
/// keeps every other Label run (a channel label) out of the taps.
fn node_id_shape(s: &str) -> bool {
    let Some((prefix, hex)) = s.split_once('-') else {
        return false;
    };
    let prefix_ok = !prefix.is_empty()
        && prefix.len() <= 8
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    prefix_ok && (4..=16).contains(&hex.len()) && hex.chars().all(|c| c.is_ascii_hexdigit())
}

/// Record the node-id runs of one painted line: the screen rectangle of
/// each maximal Label-role run whose text is node-id shaped and fully
/// painted (a run the width cut in half is no tap target, and its id
/// would not be the id the text shows).
fn record_node_spans(r: usize, c0: usize, w: usize, line: &BLine, roles: &[Role]) {
    NODE_SPANS.with(|c| {
        let mut slot = c.borrow_mut();
        let Some(spans) = slot.as_mut() else {
            return;
        };
        let label = role_of(BRole::Label);
        let mut col = c0;
        let mut runs: Vec<(usize, String)> = Vec::new();
        let mut open = false;
        for (j, ch) in line.text.chars().enumerate() {
            if roles.get(j).copied() == Some(label) {
                if open {
                    runs.last_mut().expect("an open run exists").1.push(ch);
                } else {
                    runs.push((col, ch.to_string()));
                    open = true;
                }
            } else {
                open = false;
            }
            col += char_w(ch);
        }
        for (start, text) in runs {
            if node_id_shape(&text) && start + text.chars().count() <= c0 + w {
                spans.push(NodeSpan {
                    row: r,
                    col: start,
                    len: text.chars().count(),
                    id: text,
                });
            }
        }
    });
}
/// Compress the per-char walk into `(start, len, theme Role)` spans for the
/// chrome's per-char role resolution; unroled chars stay Body.
pub(crate) fn to_body_line(line: &BLine) -> chrome::BodyLine {
    let mut segs: Vec<(usize, usize, Role)> = Vec::new();
    let mut i = 0usize;
    let total = line.text.chars().count();
    while i < total {
        let role = role_of(line.roles.get(i).copied().unwrap_or(line.default_role));
        let start = i;
        while i < total && role_of(line.roles.get(i).copied().unwrap_or(line.default_role)) == role
        {
            i += 1;
        }
        segs.push((start, i - start, role));
    }
    let mut body_line = chrome::BodyLine::plain(line.text.clone());
    body_line.segs = segs;
    body_line.pad_role = role_of(line.default_role);
    body_line
}

/// Paint `lines` into the sideline column starting at screen row `top`:
/// window them so the `follow` line stays visible, then paint each char by
/// its role. The band line wears the explicit band pair full width. No
/// border, no title, no esc chip: the backlog view shares the agent list's
/// column chrome (the divider paints after).
pub(crate) fn paint_panel(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    top: usize,
    text_w: usize,
    area_h: usize,
    lines: &[BLine],
    follow: Option<usize>,
    theme: &Theme,
) {
    let area_h = area_h.min(rows.saturating_sub(top));
    if area_h == 0 || text_w == 0 {
        return;
    }
    // The panel owns its full rect: default cells past the last line, so a
    // short body never lets stale pane content bleed through.
    for r in top..top + area_h {
        for c in 0..text_w.min(cols) {
            cells[r * cols + c] = Cell {
                c: ' ',
                fg: crate::proto::Color::Default,
                bg: crate::proto::Color::Default,
                flags: 0,
            };
        }
    }
    if lines.is_empty() {
        return;
    }
    // Window the same way the framed overlay windows: top-pinned, scrolled
    // by the minimum needed to contain `follow`.
    let start = match follow {
        Some(f) if lines.len() > area_h => f.saturating_sub(area_h - 1).min(lines.len() - area_h),
        _ => 0,
    };
    let visible = lines.len().min(area_h);
    for (i, line) in lines[start..start + visible].iter().enumerate() {
        let r = top + i;
        let band = line.band && r < rows;
        let roles_for_paint: Vec<Role> = line.roles.iter().map(|&br| role_of(br)).collect();
        paint_bline(
            cells,
            rows,
            cols,
            r,
            0,
            text_w,
            line,
            &roles_for_paint,
            theme,
        );
        if band && r < rows {
            let (fg, bg, flags) = band_style(theme);
            let row_base = r * cols;
            for c in 0..text_w.min(cols) {
                let cell = &mut cells[row_base + c];
                cell.fg = fg;
                cell.bg = bg;
                cell.flags = flags;
            }
        }
    }
}

/// [`paint_panel`] at a column offset: the Messages tab's three columns
/// paint one panel each into disjoint column ranges of the same buffer.
pub(crate) fn paint_panel_at(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    x0: usize,
    top: usize,
    text_w: usize,
    area_h: usize,
    lines: &[BLine],
    follow: Option<usize>,
    theme: &Theme,
) {
    let area_h = area_h.min(rows.saturating_sub(top));
    if area_h == 0 || text_w == 0 {
        return;
    }
    // Same full-rect ownership as [`paint_panel`], at the column offset.
    for r in top..top + area_h {
        for c in x0..(x0 + text_w).min(cols) {
            cells[r * cols + c] = Cell {
                c: ' ',
                fg: crate::proto::Color::Default,
                bg: crate::proto::Color::Default,
                flags: 0,
            };
        }
    }
    if lines.is_empty() {
        return;
    }
    let start = match follow {
        Some(f) if lines.len() > area_h => f.saturating_sub(area_h - 1).min(lines.len() - area_h),
        _ => 0,
    };
    let visible = lines.len().min(area_h);
    for (i, line) in lines[start..start + visible].iter().enumerate() {
        let r = top + i;
        let roles: Vec<Role> = line.roles.iter().map(|&br| role_of(br)).collect();
        paint_bline(cells, rows, cols, r, x0, text_w, line, &roles, theme);
        if line.band && r < rows {
            let (fg, bg, flags) = band_style(theme);
            for c in x0..(x0 + text_w).min(cols) {
                let cell = &mut cells[r * cols + c];
                cell.fg = fg;
                cell.bg = bg;
                cell.flags = flags;
            }
        }
    }
}

/// Paint one BLine's chars by display column, then plain-pad the tail with
/// default cells so stale pane content never bleeds through.
fn paint_bline(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    r: usize,
    c0: usize,
    w: usize,
    line: &BLine,
    roles: &[Role],
    theme: &Theme,
) {
    if r >= rows {
        return;
    }
    record_node_spans(r, c0, w, line, roles);
    let mut sc = c0;
    for (j, ch) in line.text.chars().enumerate() {
        if sc - c0 >= w || sc >= cols {
            break;
        }
        let (fg, bg, flags) = cell_style(
            roles
                .get(j)
                .copied()
                .unwrap_or_else(|| role_of(line.default_role)),
            theme,
        );
        cells[r * cols + sc] = Cell {
            c: ch,
            fg,
            bg,
            flags,
        };
        let cw = char_w(ch);
        if cw == 2 && sc + 1 < cols {
            cells[r * cols + sc + 1] = Cell {
                c: ' ',
                fg,
                bg,
                flags: flags | crate::proto::cell_flags::WIDE_SPACER,
            };
        }
        sc += cw;
    }
    while sc - c0 < w && sc < cols {
        cells[r * cols + sc] = Cell {
            c: ' ',
            fg: crate::proto::Color::Default,
            bg: crate::proto::Color::Default,
            flags: 0,
        };
        sc += 1;
    }
}

/// The framed overlay path's band: paint the highlight pair across one
/// body row of an already-blitted frame. `body_row` indexes FRAMED lines
/// from the first body line (the caller adds the chrome offset).
pub(crate) fn paint_framed_band(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    origin: (usize, usize),
    frame_w: usize,
    body_row: usize,
    theme: &Theme,
) {
    let r = origin.0 + body_row;
    if r >= rows {
        return;
    }
    let (fg, bg, flags) = band_style(theme);
    let c0 = (origin.1 + 1).min(cols);
    let c1 = (origin.1 + frame_w.saturating_sub(1)).min(cols);
    for c in c0..c1 {
        let cell = &mut cells[r * cols + c];
        cell.fg = fg;
        cell.bg = bg;
        cell.flags = flags;
    }
}

/// One region's title row, the focus mark the frames used to carry: the
/// owning region's title takes the accent fill across the full row, every
/// other title reads dim. Exactly one visible mark names the keyboard
/// owner; no region draws a border for it.
pub(crate) fn paint_title_row(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    row: usize,
    left: usize,
    w: usize,
    text: &str,
    focused: bool,
    theme: &Theme,
) {
    if row >= rows || w == 0 {
        return;
    }
    let width = w.min(cols.saturating_sub(left));
    if width == 0 {
        return;
    }
    if focused {
        let (fg, _, _) = band_style(theme);
        for c in left..left + width {
            cells[row * cols + c] = Cell {
                c: ' ',
                fg,
                bg: theme.brand,
                flags: cell_flags::BOLD,
            };
        }
        let mut sc = left;
        for ch in text.chars() {
            if sc >= left + width {
                break;
            }
            cells[row * cols + sc] = Cell {
                c: ch,
                fg,
                bg: theme.brand,
                flags: cell_flags::BOLD,
            };
            sc += char_w(ch);
        }
    } else {
        let line = BLine::meta(text);
        paint_bline(
            cells,
            rows,
            cols,
            row,
            left,
            width,
            &line,
            &[Role::PanelMeta],
            theme,
        );
    }
}
