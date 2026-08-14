//! Modal chrome vocabulary (x-f75e): the border, title, esc chip, section tabs,
//! footer, and scrollbar every overlay wears. This is a FRAME FUNCTION over a
//! laid-out block, not a field on `Popup`, because the mux has two overlay
//! families with no code in common - `Popup` (structured rows, hit-testing) and
//! `draw_lines_overlay` (a string blitter) - and chrome has to sit where BOTH
//! pass through. Putting it on `Popup` fixes half the modals and leaves the
//! other half looking like a different product.
//!
//! [`frame`] takes body lines and returns framed lines; [`blit`] writes them to
//! the cell buffer colored by the active [`Theme`]. Family A calls `frame`
//! inside `Popup::render` (so the framed block and the click-coordinate space are
//! one and the same); family B calls it on its `Vec<String>` before measuring.
//!
//! The chrome level is DERIVED from the anchor and private: an anchored menu
//! wears `Bare` (a border + an inline esc label, 2 rows), a centered modal wears
//! `Full` (title + chip + optional subtitle/tabs/footer, 4+ rows). A call site
//! cannot opt out, so the fifth menu someone adds inherits the rule.

use crate::popup::Anchor;
use crate::proto::Cell;
use crate::theme::{cell_style, Role, Theme};

/// How much chrome a block wears. Derived from the anchor; never passed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Centered modal: top border with title + esc chip, optional subtitle /
    /// tabs, body, optional footer, bottom border.
    Full,
    /// Anchored menu: border only, the esc hint riding the bottom border inline
    /// so it costs no extra row. An anchored menu opens next to the pointer at
    /// whatever width its longest label needs, and `WIDTH_CAP` already
    /// ellipsizes it, so four rows of title/footer would take more of the
    /// screen than the menu itself.
    Bare,
}

/// The chrome a block wears. `level` is private with no setter: it comes from
/// [`Chrome::level_for`] of the anchor passed at construction, which is what
/// makes the level a rule rather than a convention a call site can override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chrome {
    pub title: String,
    pub subtitle: Option<String>,
    /// `(label, is_active)`. Empty = no tab strip.
    pub tabs: Vec<(String, bool)>,
    pub footer: Option<String>,
    level: Level,
}

impl Chrome {
    /// Construct chrome for a block anchored at `anchor`. The level is fixed
    /// here and cannot be changed - there is no `set_level`.
    pub fn new(title: impl Into<String>, anchor: Anchor) -> Self {
        Chrome {
            title: title.into(),
            subtitle: None,
            tabs: Vec::new(),
            footer: None,
            level: Self::level_for(anchor),
        }
    }
    pub fn subtitle(mut self, s: impl Into<String>) -> Self {
        self.subtitle = Some(s.into());
        self
    }
    pub fn tabs(mut self, tabs: Vec<(String, bool)>) -> Self {
        self.tabs = tabs;
        self
    }
    pub fn footer(mut self, f: impl Into<String>) -> Self {
        self.footer = Some(f.into());
        self
    }
    pub fn level(&self) -> Level {
        self.level
    }

    /// The rule, in code: anchored menus wear `Bare`, centered modals wear
    /// `Full`. Stated here rather than per call site so two menus can never
    /// disagree about it.
    fn level_for(anchor: Anchor) -> Level {
        match anchor {
            Anchor::At { .. } => Level::Bare,
            Anchor::Center => Level::Full,
        }
    }

    /// Columns the frame adds (left + right border). The scrollbar column is
    /// added on top of this when the body overflows; see [`frame`].
    pub const FRAME_COLS: usize = 2;

    /// Rows added above the body.
    pub fn rows_above(&self) -> usize {
        match self.level {
            Level::Bare => 1,
            Level::Full => {
                1 + usize::from(self.subtitle.is_some()) + usize::from(!self.tabs.is_empty())
            }
        }
    }

    /// Rows added below the body.
    pub fn rows_below(&self) -> usize {
        match self.level {
            Level::Bare => 1, // bottom border carries the esc label inline
            Level::Full => usize::from(self.footer.is_some()) + 1,
        }
    }

    pub fn rows_overhead(&self) -> usize {
        self.rows_above() + self.rows_below()
    }

    /// The minimum INNER width the chrome itself needs, so a title or the esc
    /// chip can never make a border row wider than the body rows (which would
    /// break the rectangle). Normal modals are far wider than this; it only
    /// kicks in for a tiny body with a long title.
    fn min_inner_w(&self) -> usize {
        // ` esc ` is 5; every level reserves at least that plus a leading `─`.
        const ESC_INNER: usize = 6;
        match self.level {
            Level::Bare => ESC_INNER,
            Level::Full if self.title.is_empty() => ESC_INNER,
            Level::Full => self.title.chars().count() + 9, // `─ {title} ─ esc `
        }
    }
}

/// Scrollbar geometry, when the body overflows its viewport. `pos` is the
/// topmost visible body line; `total` is the full body length; `visible` is how
/// many body lines the viewport shows.
#[derive(Debug, Clone, Copy)]
pub struct Scroll {
    pub pos: usize,
    pub total: usize,
    pub visible: usize,
}

/// One body line handed to [`frame`]. Family A fills `header` / `sel_span` /
/// `hits` (the last is the mouse hit-test spans, which `frame` shifts past the
/// left border so a click in the framed block still resolves); family B leaves
/// them default.
#[derive(Debug, Clone, Default)]
pub struct BodyLine {
    pub text: String,
    pub header: bool,
    /// `(offset, len)` within `text` that is the selected cut-out.
    pub sel_span: Option<(usize, usize)>,
    /// `(target, offset, len)` hit spans within `text`, offsets relative to the
    /// line's first char.
    pub hits: Vec<(usize, usize, usize)>,
}

impl BodyLine {
    /// A plain content line (family B's string, or a non-selectable body row).
    pub fn from_str(s: impl Into<String>) -> Self {
        BodyLine {
            text: s.into(),
            ..Default::default()
        }
    }
}

/// A laid-out, framed line: its text, one [`Role`] per char, and the hit spans
/// (offsets now relative to the framed line's first char, i.e. past the left
/// border) for mouse hit-testing. Chrome rows carry no hits.
#[derive(Debug, Clone)]
pub struct FramedLine {
    pub text: String,
    pub roles: Vec<Role>,
    pub hits: Vec<(usize, usize, usize)>,
}

/// A fully framed block: the lines to draw and the total framed width.
#[derive(Debug, Clone)]
pub struct Framed {
    pub lines: Vec<FramedLine>,
    pub width: usize,
}

/// Frame `body` (already windowed to the viewport, each line padded/truncated
/// to `body_w`) with `chrome`. When `scroll` is `Some` and the body overflows, a
/// scrollbar column is appended inside the right border. Pure: styles are
/// resolved later by the caller via [`blit`].
///
/// `body_w` is clamped to at least 1 so a terminal too narrow for a border plus
/// one content column degrades to a border-only block rather than underflowing
/// (the precedent `anchored_origin_degrades_when_block_exceeds_screen` sets).
pub fn frame(body: &[BodyLine], chrome: &Chrome, body_w: usize, scroll: Option<Scroll>) -> Framed {
    let body_w = body_w.max(1).max(chrome.min_inner_w());
    let has_scroll = scroll.is_some_and(|s| s.total > s.visible && s.visible > 0);
    let inner_w = body_w + usize::from(has_scroll);
    let width = chrome_frame_width(body_w, has_scroll);

    let mut out: Vec<FramedLine> = Vec::with_capacity(body.len() + chrome.rows_overhead());

    out.push(top_border(chrome, inner_w));
    // Subtitle, tabs, and footer are Full-only: a Bare menu is border-only by
    // definition (rows_above/below already account for this), so even if a Bare
    // chrome carries them they are not rendered.
    if chrome.level == Level::Full {
        if let Some(s) = &chrome.subtitle {
            out.push(content_row(s, Role::Subtitle, inner_w));
        }
        if !chrome.tabs.is_empty() {
            out.push(tab_row(&chrome.tabs, inner_w));
        }
    }

    let sc = scroll.filter(|_| has_scroll);
    for (idx, line) in body.iter().enumerate() {
        out.push(body_row(line, idx, body_w, inner_w, sc));
    }

    // Footer rides above the bottom border under Full; under Bare the esc hint
    // IS the bottom border, so a footer (if any was set) is ignored there.
    if chrome.level == Level::Full {
        if let Some(f) = &chrome.footer {
            let mut row = content_row(f, Role::Footer, inner_w);
            // The close words are a mouse target, defined once here so every
            // popup that sets this footer inherits the clickable close (a
            // per-modal hit test leaves the others printing the same lie).
            if let Some((off, len)) = esc_close_span(f) {
                row.hits.push((ESC_CLOSE_HIT, off + 1, len)); // +1: past the left border
            }
            out.push(row);
        }
    }
    out.push(bottom_border(chrome, inner_w));

    Framed { lines: out, width }
}

/// The hit target marking a chrome footer's `esc close` span: `usize::MAX` can
/// never collide with a body row's real target (an index).
pub const ESC_CLOSE_HIT: usize = usize::MAX;

/// The `(char offset, char len)` of a footer's close affordance when it
/// carries one: the words `esc close` when present, else a bare `esc`. Char
/// offsets, matching how `content_row` lays the text out; `frame` shifts them
/// into framed coordinates.
fn esc_close_span(footer: &str) -> Option<(usize, usize)> {
    let chars: Vec<char> = footer.chars().collect();
    let find = |needle: &[char]| chars.windows(needle.len()).position(|w| w == needle);
    let long: Vec<char> = "esc close".chars().collect();
    let short: Vec<char> = "esc".chars().collect();
    find(&long)
        .map(|o| (o, long.len()))
        .or_else(|| find(&short).map(|o| (o, short.len())))
}

/// Framed width: left border + body + optional scrollbar + right border.
pub fn chrome_frame_width(body_w: usize, has_scroll: bool) -> usize {
    Chrome::FRAME_COLS + body_w + usize::from(has_scroll)
}

/// Blit framed lines into the cell buffer at `origin`, each cell colored by its
/// role against `theme`. Cell-bounds-checked (a tiny terminal clips, no panic).
pub fn blit(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    origin: (usize, usize),
    framed: &Framed,
    theme: &Theme,
) {
    let (r0, c0) = origin;
    for (i, line) in framed.lines.iter().enumerate() {
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

// ---- line builders -------------------------------------------------------

/// A `(char, Role)` pair; a row is a `Vec<Seg>` flattened to text + roles.
type Seg = (char, Role);

fn flatten(mut row: Vec<Seg>) -> FramedLine {
    let mut text = String::with_capacity(row.len());
    let mut roles = Vec::with_capacity(row.len());
    for (ch, role) in row.drain(..) {
        text.push(ch);
        roles.push(role);
    }
    FramedLine {
        text,
        roles,
        hits: Vec::new(),
    }
}

/// Pad `inner` with `fill` up to `inner_w`, then wrap in left/right edges.
fn edge_row(
    left: char,
    right: char,
    fill: char,
    mut inner: Vec<Seg>,
    inner_w: usize,
) -> FramedLine {
    let used = inner.len();
    for _ in used..inner_w {
        inner.push((fill, Role::Border));
    }
    let mut row = Vec::with_capacity(inner_w + 2);
    row.push((left, Role::Border));
    row.extend(inner);
    row.push((right, Role::Border));
    flatten(row)
}

/// `│ content │`, content left-aligned, padded with spaces (same role) so the
/// inverse block stays a clean rectangle.
fn content_row(content: &str, role: Role, inner_w: usize) -> FramedLine {
    let mut inner: Vec<Seg> = content.chars().map(|c| (c, role)).collect();
    let used = inner.len();
    for _ in used..inner_w {
        inner.push((' ', role));
    }
    let mut row = vec![('│', Role::Border)];
    row.extend(inner);
    row.push(('│', Role::Border));
    flatten(row)
}

/// The section tab strip: `●`/`○` (active gets the accent) + label per tab.
fn tab_row(tabs: &[(String, bool)], inner_w: usize) -> FramedLine {
    let mut inner: Vec<Seg> = vec![(' ', Role::Tab(false))];
    for (i, (label, active)) in tabs.iter().enumerate() {
        if i > 0 {
            inner.push((' ', Role::Tab(false)));
            inner.push((' ', Role::Tab(false)));
        }
        inner.push((if *active { '●' } else { '○' }, Role::Tab(*active)));
        inner.push((' ', Role::Tab(*active)));
        for ch in label.chars() {
            inner.push((ch, Role::Tab(*active)));
        }
    }
    content_row_from_segs(inner, inner_w)
}

fn content_row_from_segs(mut inner: Vec<Seg>, inner_w: usize) -> FramedLine {
    let used = inner.len();
    for _ in used..inner_w {
        inner.push((' ', Role::Tab(false)));
    }
    let mut row = vec![('│', Role::Border)];
    row.extend(inner);
    row.push(('│', Role::Border));
    flatten(row)
}

fn esc_segs() -> Vec<Seg> {
    // ` esc ` with the word as Chip and its padding as Border, so the chip is
    // distinguishable from the border even under `terminal` (BOLD vs plain).
    " esc "
        .chars()
        .map(|c| (c, if c == ' ' { Role::Border } else { Role::Chip }))
        .collect()
}

fn top_border(chrome: &Chrome, inner_w: usize) -> FramedLine {
    match chrome.level {
        Level::Bare => edge_row('┌', '┐', '─', Vec::new(), inner_w),
        Level::Full => {
            // `┌─ Title ──── esc ─┐`: title left (after `─`), esc chip right.
            let mut inner: Vec<Seg> = vec![('─', Role::Border)];
            if !chrome.title.is_empty() {
                inner.push((' ', Role::Title));
                for ch in chrome.title.chars() {
                    inner.push((ch, Role::Title));
                }
                inner.push((' ', Role::Title));
                inner.push(('─', Role::Border));
            }
            // Right-align the chip, then fill the gap with border rules.
            let chip = esc_segs();
            let used = inner.len();
            let reserve = chip.len();
            for _ in used..inner_w.saturating_sub(reserve) {
                inner.push(('─', Role::Border));
            }
            inner.extend(chip);
            edge_row('┌', '┐', '─', inner, inner_w)
        }
    }
}

fn bottom_border(chrome: &Chrome, inner_w: usize) -> FramedLine {
    match chrome.level {
        Level::Full => edge_row('└', '┘', '─', Vec::new(), inner_w),
        // Bare: `└─ esc ─┘` - the esc hint rides the bottom border at zero row.
        Level::Bare => {
            let mut inner: Vec<Seg> = vec![('─', Role::Border)];
            let chip = esc_segs();
            let used = inner.len();
            let reserve = chip.len();
            for _ in used..inner_w.saturating_sub(reserve) {
                inner.push(('─', Role::Border));
            }
            inner.extend(chip);
            edge_row('└', '┘', '─', inner, inner_w)
        }
    }
}

/// A body row: `│` + body text (padded/truncated to `body_w`, per-char roles
/// from header/sel_span) + optional scrollbar char + `│`. Hit offsets shift by
/// the left border so they land in framed coordinates. `row_idx` is this body
/// line's position in the viewport (drives the scrollbar thumb).
fn body_row(
    line: &BodyLine,
    row_idx: usize,
    body_w: usize,
    inner_w: usize,
    scroll: Option<Scroll>,
) -> FramedLine {
    let chars: Vec<char> = line.text.chars().collect();
    let mut text = String::with_capacity(inner_w + 2);
    let mut roles = Vec::with_capacity(inner_w + 2);

    text.push('│');
    roles.push(Role::Border);

    let in_sel = |j: usize| {
        line.sel_span
            .is_some_and(|(off, len)| j >= off && j < off + len)
    };
    for j in 0..body_w {
        let (ch, role) = match chars.get(j) {
            Some(&c) => {
                let role = if line.header {
                    Role::BodyHead
                } else if in_sel(j) {
                    Role::BodySel
                } else {
                    Role::Body
                };
                (c, role)
            }
            None => (' ', Role::Body),
        };
        text.push(ch);
        roles.push(role);
    }

    if let Some(s) = scroll {
        let (ch, role) = scroll_cell(row_idx, s);
        text.push(ch);
        roles.push(role);
    }

    text.push('│');
    roles.push(Role::Border);

    let hits = line
        .hits
        .iter()
        .map(|(t, off, len)| (*t, off + 1, *len))
        .collect();

    FramedLine { text, roles, hits }
}

/// The scrollbar cell for viewport row `row_idx`: `█` (thumb) over the rows the
/// thumb covers, `░` (track) elsewhere. The thumb height and start are the
/// standard proportional mapping.
fn scroll_cell(row_idx: usize, s: Scroll) -> (char, Role) {
    if s.visible == 0 || s.total <= s.visible {
        return ('░', Role::ScrollTrack);
    }
    let thumb_h = ((s.visible as u64).pow(2) / s.total.max(1) as u64) as usize;
    let thumb_h = thumb_h.max(1);
    // Both the height and the start floor, so the proportional formula alone
    // stops SHORT of the bottom at maximum scroll: the last track cell stays
    // `░` and reports more content below when the operator is already at the
    // end. Pin the thumb to the bottom there instead, and clamp elsewhere so it
    // can never overhang. This only became reachable when `pos` started
    // reporting the real window position - it was hardcoded 0 before, which
    // parked the thumb at the top and hid the case entirely.
    let last_row = s.visible.saturating_sub(thumb_h);
    let thumb_start = if s.pos + s.visible >= s.total {
        last_row
    } else {
        ((s.pos as u64 * s.visible as u64 / s.total.max(1) as u64) as usize).min(last_row)
    };
    if row_idx >= thumb_start && row_idx < thumb_start + thumb_h {
        ('█', Role::ScrollThumb)
    } else {
        ('░', Role::ScrollTrack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bl(s: &str) -> BodyLine {
        BodyLine::from_str(s)
    }

    #[test]
    fn scrollbar_thumb_reaches_the_bottom_at_maximum_scroll() {
        // A track cell below the thumb means "there is more". At the end of the
        // list that is a lie, and it was unreachable only because `pos` used to
        // be hardcoded 0.
        let s = Scroll {
            pos: 1,
            total: 11,
            visible: 10,
        };
        let cells: Vec<char> = (0..s.visible).map(|r| scroll_cell(r, s).0).collect();
        assert_eq!(
            cells.last(),
            Some(&'█'),
            "at max scroll the thumb must touch the bottom: {cells:?}"
        );
        // ...and at the top it still starts at the top.
        let top = Scroll { pos: 0, ..s };
        assert_eq!(scroll_cell(0, top).0, '█');
        assert_eq!(
            scroll_cell(s.visible - 1, top).0,
            '░',
            "at the top the track below still says there is more"
        );
        // The thumb never overhangs the track, at any position.
        for total in [11usize, 14, 30, 100] {
            for pos in 0..=(total - 10) {
                let sc = Scroll {
                    pos,
                    total,
                    visible: 10,
                };
                let painted = (0..sc.visible)
                    .filter(|r| scroll_cell(*r, sc).0 == '█')
                    .count();
                assert!(painted > 0, "thumb vanished at pos={pos} total={total}");
            }
        }
    }

    #[test]
    fn level_is_derived_from_anchor() {
        // Centered -> Full, anchored -> Bare. No set_level exists.
        assert_eq!(Chrome::new("t", Anchor::Center).level(), Level::Full);
        assert_eq!(
            Chrome::new("t", Anchor::At { row: 1, col: 1 }).level(),
            Level::Bare
        );
    }

    #[test]
    fn full_frame_dimensions_match_its_overhead() {
        // subtitle + footer each add a row under Full.
        let c = Chrome::new("Title", Anchor::Center)
            .subtitle("sub")
            .footer("f");
        let framed = frame(&[bl("a"), bl("b"), bl("c")], &c, 5, None);
        assert_eq!(c.rows_above(), 2);
        assert_eq!(c.rows_below(), 2);
        assert_eq!(framed.lines.len(), 3 + c.rows_overhead());
    }

    #[test]
    fn bare_frame_is_top_body_bottom_only() {
        // Bare ignores subtitle/footer (an anchored menu cannot afford them).
        let c = Chrome::new("", Anchor::At { row: 0, col: 0 })
            .subtitle("ignored")
            .footer("ignored");
        assert_eq!(c.level(), Level::Bare);
        let framed = frame(&[bl("x")], &c, 3, None);
        assert_eq!(framed.lines.len(), 3);
        assert!(framed.lines.iter().all(|l| !l.text.contains("ignored")));
    }

    #[test]
    fn full_top_border_carries_title_and_esc_chip() {
        let c = Chrome::new("Settings", Anchor::Center);
        let framed = frame(&[bl("body")], &c, 6, None);
        let top = &framed.lines[0];
        assert!(top.text.starts_with('┌'));
        assert!(top.text.ends_with('┐'));
        assert!(top.text.contains("Settings"));
        assert!(top.text.contains("esc"));
        // Positive markers: the chip and title carry their own roles.
        assert!(top.roles.contains(&Role::Chip));
        assert!(top.roles.contains(&Role::Title));
    }

    #[test]
    fn bare_bottom_border_carries_inline_esc() {
        let c = Chrome::new("", Anchor::At { row: 0, col: 0 });
        let framed = frame(&[bl("body")], &c, 6, None);
        let bottom = framed.lines.last().unwrap();
        assert!(bottom.text.contains("esc"));
        assert!(bottom.roles.contains(&Role::Chip));
    }

    #[test]
    fn hit_offsets_shift_past_the_left_border() {
        let mut line = BodyLine::from_str("hello");
        line.hits.push((0, 0, 5));
        let c = Chrome::new("T", Anchor::Center);
        let framed = frame(&[line], &c, 5, None);
        let body = framed.lines.iter().find(|l| !l.hits.is_empty()).unwrap();
        assert_eq!(body.hits[0], (0, 1, 5));
    }

    #[test]
    fn footer_esc_close_words_carry_a_hit_span() {
        // The clickable close is defined HERE, once, so every popup that sets
        // an `esc close` footer inherits the target. The span covers the
        // words, offset past the left border exactly like a body hit, and a
        // footer without the words carries no target.
        let footer = "tab switches section · esc close";
        let c = Chrome::new("settings", Anchor::Center).footer(footer);
        let framed = frame(&[bl("body")], &c, 40, None);
        let off = footer
            .char_indices()
            .find_map(|(i, _)| footer[i..].starts_with("esc close").then_some(i))
            .unwrap();
        let char_off = footer[..off].chars().count();
        let row = framed
            .lines
            .iter()
            .find(|l| !l.hits.is_empty())
            .expect("the footer line carries the close target");
        assert_eq!(row.hits, vec![(ESC_CLOSE_HIT, char_off + 1, 9)]);
        // The span is inside the line and lands on the words.
        let words: String = row.text.chars().skip(char_off + 1).take(9).collect();
        assert_eq!(words, "esc close");

        // No words, no target: only the footer line ever carries one.
        let c = Chrome::new("t", Anchor::Center).footer("just some text");
        let framed = frame(&[bl("body")], &c, 40, None);
        assert!(framed
            .lines
            .iter()
            .all(|l| { l.hits.iter().all(|(t, _, _)| *t != ESC_CLOSE_HIT) }));
    }

    #[test]
    fn selected_body_cell_gets_the_sel_role() {
        let mut line = BodyLine::from_str("hello");
        line.sel_span = Some((0, 5));
        let c = Chrome::new("T", Anchor::Center);
        let framed = frame(&[line], &c, 5, None);
        let body = framed
            .lines
            .iter()
            .find(|l| l.text.contains("hello"))
            .unwrap();
        // The first body char (past the left border) is BodySel.
        assert_eq!(body.roles[1], Role::BodySel);
    }

    #[test]
    fn scrollbar_column_appears_only_on_overflow() {
        // Empty title + body_w above the chrome minimum, so the width math
        // isolates the scrollbar from the title-widening floor.
        let c = Chrome::new("", Anchor::Center);
        let fits = frame(
            &[bl("ab")],
            &c,
            8,
            Some(Scroll {
                pos: 0,
                total: 2,
                visible: 2,
            }),
        );
        assert!(fits
            .lines
            .iter()
            .flat_map(|l| l.roles.iter())
            .all(|r| !matches!(*r, Role::ScrollThumb | Role::ScrollTrack)));
        assert_eq!(fits.width, chrome_frame_width(8, false));

        let over = frame(
            &[bl("ab"), bl("cd")],
            &c,
            8,
            Some(Scroll {
                pos: 0,
                total: 8,
                visible: 2,
            }),
        );
        assert_eq!(over.width, chrome_frame_width(8, true));
        assert!(over
            .lines
            .iter()
            .flat_map(|l| l.roles.iter())
            .any(|r| matches!(*r, Role::ScrollThumb | Role::ScrollTrack)));
        // Thumb is proportional: of 2 visible rows for 8 total, the thumb
        // covers >=1 row.
        let thumbs = over
            .lines
            .iter()
            .flat_map(|l| l.roles.iter())
            .filter(|r| **r == Role::ScrollThumb)
            .count();
        assert!(thumbs >= 1);
    }

    #[test]
    fn degenerate_narrow_terminal_does_not_underflow() {
        let c = Chrome::new("T", Anchor::Center);
        let framed = frame(&[bl("")], &c, 0, None);
        assert!(framed.width >= Chrome::FRAME_COLS);
        assert!(!framed.lines.is_empty());
        // Every line is at least the two border chars wide.
        assert!(framed.lines.iter().all(|l| l.text.chars().count() >= 2));
    }

    #[test]
    fn blit_is_cell_bounds_safe_and_theme_colored() {
        let c = Chrome::new("T", Anchor::Center);
        let framed = frame(&[bl("body")], &c, 4, None);
        let theme = Theme::default_theme();
        let mut cells = vec![Cell::default(); 2 * 40]; // 2 rows clips the taller block
        blit(&mut cells, 2, 40, (0, 0), &framed, &theme);
        assert_eq!(cells[0].c, '┌');
        let (fg, bg, _) = cell_style(Role::Border, &theme);
        assert_eq!((cells[0].fg, cells[0].bg), (fg, bg));
    }

    #[test]
    fn blit_under_catppuccin_colors_the_border() {
        let c = Chrome::new("T", Anchor::Center);
        let framed = frame(&[bl("body")], &c, 4, None);
        let theme = crate::theme::Theme::from_name("catppuccin").0;
        let mut cells = vec![Cell::default(); 10 * 40];
        blit(&mut cells, 10, 40, (0, 0), &framed, &theme);
        assert_eq!(cells[0].c, '┌');
        assert_eq!(cells[0].fg, theme.border);
    }
}
