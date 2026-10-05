//! The activity feed panel : questions, decisions and node
//! lifecycle, newest first, one deep link per row. The render moved-module
//! idiom matches `needs_view.rs`; the data comes from
//! [`crate::feed_overlay::feed_now`], which shells the `fno-agents feed`
//! projection off the UI loop and renders a typed reason when it fails
//!, never one generic sentence for five causes.
//!
//! The deep link is the sideline's own path, not a new one: a row that joins
//! a live roster row resolves through `agent_hit` exactly as a sideline click
//! does (FocusPane, or AttachAgent on portal 0); an unjoined row carrying a
//! session id attaches that id directly, and the server's existing
//! `no such agent` notice is what a dead session answers.
//!
//! Since the feed is the right-edge PANEL the operator specified, not
//! a centered modal: `e` toggles it, the border drags, and an UNFOCUSED panel
//! consumes no keys at all, so typing reaches the focused pane.
//!
//! That key-free rule is the default, not the whole story. `E` focuses the
//! panel explicitly: while focused it takes the arrows (row select and
//! horizontal pan), Enter, and Esc and e, which close the panel - there is
//! no release that leaves it open and key-less, a state no key could leave.
//! The property the rule protects - typing reaches the pane - holds whenever
//! the operator has not asked for the opposite.
//!
//! A click no longer fires the deep link straight off. It opens the row's
//! PROVENANCE (`feed_detail`), and the deep link is that view's footer
//! action. Inspecting never attaches or resumes anything on its own.

use super::*;
use crate::feed_overlay::{FeedError, FeedItem, event_fields};

pub(crate) mod page;
pub(crate) mod search;


/// The panel's open state: the items the last fold landed, the hover marker
/// (a display index, NEWEST FIRST, the order the rows render in), and the
/// same generation/single-flight discipline the needs fold runs. `want` arms
/// the run-loop kick; `gen` invalidates a result that lands after a close or
/// a re-open. `sel` follows the pointer while the panel is UNFOCUSED, and the
/// arrows while it is focused - the panel consumes keys only then.
pub(crate) struct FeedOverlay {
    pub(crate) win: page::FeedWindow,
    /// Hovered row in display order; the `▸` marker paints here.
    pub(crate) sel: usize,
    /// The typed fold failure, if the last fold failed. `Some` renders its
    /// reason verbatim; a success fold clears it.
    pub(crate) error: Option<crate::feed_overlay::FeedError>,
    pub(crate) inflight: bool,
    pub(crate) want: bool,
    pub(crate) gen: u64,
    /// Horizontal pan into the TITLE, in display columns. The timestamp, kind
    /// and node stay anchored so a panned row is still identifiable.
    pub(crate) hpan: usize,
    /// When the last fold LANDED. Arms the 15s auto-refresh; `None` until the
    /// first fold lands, and the first landing resets to the newest row.
    pub(crate) last_fold: Option<Instant>,
    /// The row order the panel renders in, persisted per client (`o` toggles).
    pub(crate) order: FeedOrder,
    /// The page the window asked for and the fold has not answered.
    pub(crate) want_page: Option<crate::feed_overlay::PageReq>,
    /// The active query text; empty means unfiltered.
    pub(crate) query_text: String,
    /// The parsed query (or its refusal, for the footer). `None` when the
    /// bar is empty.
    pub(crate) parsed: Option<Result<crate::search_query::Parsed, String>>,
    /// True when the whole query pushed into projection flags, so landed
    /// pages need no client-side matching.
    pub(crate) pushed: bool,
    /// The projection flags of the active query, when they pushed.
    pub(crate) filter: Option<crate::feed_overlay::FeedFilter>,
    /// The `?` keys overlay.
    pub(crate) keys_open: bool,
    /// Where the client-side scan reached (the footer's "scanned to" note).
    pub(crate) scan_note: Option<String>,
    /// The search bar is open (its text lives in `query_text`).
    pub(crate) bar_open: bool,
    /// Pages spent on the current client-matched scan gesture, capped so a
    /// query whose matches are sparse stops after a bounded read.
    pub(crate) scan_pages: u32,
    /// The debounce instant while the bar has text.
    pub(crate) bar_debounce: Option<Instant>,
}

/// The panel's row order. `Grouped` is the shipped order (one header per
/// owner, groups ordered by their newest row); `Recent` is one flat list,
/// newest first. The operator toggles with `o`; the choice persists through
/// the view store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FeedOrder {
    #[default]
    Grouped,
    Recent,
}

impl FeedOrder {
    /// The view-store spelling.
    fn key(self) -> &'static str {
        match self {
            FeedOrder::Grouped => "grouped",
            FeedOrder::Recent => "recent",
        }
    }

    /// The stored spelling back; anything unreadable stays `None` so the
    /// caller can fall to the shipped order.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "grouped" => Some(FeedOrder::Grouped),
            "recent" => Some(FeedOrder::Recent),
            _ => None,
        }
    }

    /// One press of `o`. A two-state cycle, so every press changes the order.
    fn next(self) -> Self {
        match self {
            FeedOrder::Grouped => FeedOrder::Recent,
            FeedOrder::Recent => FeedOrder::Grouped,
        }
    }
}

/// A fresh open: the prior items ride over (instant content), but a refold is
/// always armed - history may have moved since the last open, and the fold is
/// cheap and off-loop.
pub(crate) fn open_overlay(prior: Option<FeedOverlay>, gen: u64) -> FeedOverlay {
    let (win, query_text, parsed, pushed, filter) = match prior {
        Some(f) => (
            f.win,
            f.query_text,
            f.parsed,
            f.pushed,
            f.filter,
        ),
        None => (
            page::FeedWindow::default(),
            String::new(),
            None,
            false,
            None,
        ),
    };
    let order = crate::view_store::load_feed_order()
        .and_then(|o| FeedOrder::parse(&o))
        .unwrap_or_default();
    let sel = first_item_slot(&win.items, order);
    FeedOverlay {
        win,
        sel,
        error: None,
        inflight: false,
        want: false,
        gen,
        hpan: 0,
        last_fold: None,
        order,
        want_page: Some(crate::feed_overlay::PageReq::Head),
        query_text,
        parsed,
        pushed,
        filter,
        keys_open: false,
        scan_note: None,
        bar_open: false,
        bar_debounce: None,
        scan_pages: 0,
    }
}

/// One display slot: a group header or a row (as its STORAGE index into
/// `items`, oldest first). The ONE mapping every reader walks, so a header
/// row never opens a detail and the marker never parks on one.
pub(crate) enum Slot {
    Header(String),
    Item(usize),
}

/// True when a row renders in the teams band: a team kind, or a removal
/// that names the team it held.
fn in_teams_band(kind: &str, team: &Option<String>) -> bool {
    kind == "team_granted" || kind == "team_vacated" || (kind == "session_reaped" && team.is_some())
}

/// The display order for one panel order. `Grouped` is the shipped shape;
/// `Recent` is every row, newest first, no headers - one flat list the
/// operator reads top as now.
pub(crate) fn display_slots(items: &[FeedItem], order: FeedOrder) -> Vec<Slot> {
    match order {
        FeedOrder::Grouped => display_slots_grouped(items),
        // Storage indexes ascending = oldest first, so the reversed range is
        // the flat newest-first list.
        FeedOrder::Recent => (0..items.len()).rev().map(Slot::Item).collect(),
    }
}

/// The grouped order: the teams band first (newest first), then one header
/// per owner, groups ordered by their newest row, then the unowned rows under
/// `other`. Within a group, newest first; ties keep storage order.
fn display_slots_grouped(items: &[FeedItem]) -> Vec<Slot> {
    let mut teams: Vec<usize> = Vec::new();
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    let mut other: Vec<usize> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if in_teams_band(&item.kind, &item.crown) {
            teams.push(i);
            continue;
        }
        match &item.owner {
            Some(owner) => match groups.iter_mut().find(|(o, _)| o == owner) {
                Some((_, idx)) => idx.push(i),
                None => groups.push((owner.clone(), vec![i])),
            },
            None => other.push(i),
        }
    }
    let mut slots = Vec::new();
    if !teams.is_empty() {
        slots.push(Slot::Header("teams".into()));
        for i in newest_first(items, &teams) {
            slots.push(Slot::Item(i));
        }
    }
    groups.sort_by(|a, b| {
        let newest = |g: &(String, Vec<usize>)| g.1.iter().map(|i| ts_key_of(&items[*i].ts)).max();
        newest(b).cmp(&newest(a))
    });
    for (owner, idx) in groups {
        slots.push(Slot::Header(owner));
        for i in newest_first(items, &idx) {
            slots.push(Slot::Item(i));
        }
    }
    if !other.is_empty() {
        slots.push(Slot::Header("other".into()));
        for i in newest_first(items, &other) {
            slots.push(Slot::Item(i));
        }
    }
    slots
}

/// Storage indexes newest first; ties keep storage order (a stable sort on
/// already-ascending indexes reverses tie groups together).
fn newest_first(items: &[FeedItem], idx: &[usize]) -> Vec<usize> {
    let mut v = idx.to_vec();
    v.sort_by(|a, b| ts_key_of(&items[*b].ts).cmp(&ts_key_of(&items[*a].ts)));
    v
}

/// ts sort key, the projection's own shape.
fn ts_key_of(ts: &str) -> (u8, i64) {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => (1, t.timestamp_millis()),
        Err(_) => (0, 0),
    }
}

/// The kind as printed. The ship event renamed to `node_shipped` (ruling
/// 2026-09-29); the projection derives rows at read time, so only a string
/// embedded in an older surface spells the old name - this is the one reader.
pub(crate) fn display_kind(kind: &str) -> &str {
    if kind == "pr_created" {
        "node_shipped"
    } else {
        kind
    }
}

/// True for kinds whose event NEEDS ACTION, so the kind renders bold in the
/// theme's accent: a question waiting, a team leaving, a teamed session
/// removed.
fn bold_kind(item: &FeedItem) -> bool {
    item.kind == "question_asked"
        || item.kind == "team_vacated"
        || (item.kind == "session_reaped" && item.crown.is_some())
}

/// The panel body: one header line, up to `visible_rows - 2` item rows, then
/// the footer pinned to the last row. The projection hands rows OLDEST
/// FIRST, so display index `d` reads storage index `len - 1 - d` and the top
/// row is the newest event - the same view `feed_row_item` inverts for the
/// click resolver, so a row and its deep link always name the same event.
pub(crate) fn feed_panel_rows(
    o: &FeedOverlay,
    focused: bool,
    w: usize,
    visible_rows: usize,
    offset: usize,
) -> Vec<Vec<Span>> {
    // The header says which input state the panel is in, because the rule is
    // not guessable: an unfocused panel takes no keys at all. It is also the
    // ONLY place the focus key is advertised, so it degrades to a shorter
    // spelling on a narrow panel rather than being clipped away.
    let mut rows: Vec<Vec<Span>> = Vec::new();
    rows.push(vec![Span::plain(pad_to(
        &header_line(focused, o.order, w),
        w,
    ))]);
    let visible = visible_rows.saturating_sub(2);
    let slots = display_slots(&o.win.items, o.order);
    for d in offset..offset + visible {
        match slots.get(d) {
            Some(Slot::Header(label)) => {
                rows.push(vec![Span {
                    text: pad_to(&format!(" ▾ {label}"), w),
                    bold: true,
                    brand: false,
                }]);
            }
            Some(Slot::Item(i)) => {
                let item = &o.win.items[*i];
                // The marker lands on the hovered row, or on the selected row
                // while the panel holds the keyboard - there it reads bold in
                // the theme accent, so the cursor survives a glance.
                let selected = focused && d == o.sel;
                let marker = if d == o.sel { '▸' } else { ' ' };
                let node = item.node.as_deref().unwrap_or("-");
                let ts = short_ts(&item.ts);
                let title = pan_by(&item.title, o.hpan);
                let kind = format!("{:<16}", display_kind(&item.kind));
                let mut row = vec![
                    Span {
                        text: format!(" {marker} {ts} "),
                        bold: selected,
                        brand: selected,
                    },
                    Span {
                        text: kind,
                        bold: bold_kind(item),
                        brand: bold_kind(item),
                    },
                    Span {
                        text: node.to_string(),
                        bold: item.node.is_some(),
                        brand: false,
                    },
                    Span::plain(format!(" · {title}")),
                ];
                pad_to_spans(&mut row, w);
                rows.push(row);
            }
            // Short list: the rows below the last item render blank.
            None => rows.push(vec![Span::plain("")]),
        }
    }
    // The empty notice only when the fold has SETTLED empty: "no activity"
    // beside a still-running fold is a claim the fold has not earned yet.
    if o.win.items.is_empty() && o.error.is_none() && !o.inflight && visible > 0 {
        rows[1] = vec![Span::plain(pad_to("   no activity in the last 24h", w))];
    }
    let footer = if let Some(e) = &o.error {
        // The typed reason renders verbatim: a timeout names its
        // budget, an admission refusal its slot count. The panel clips a
        // long stderr tail; the cause still leads the line.
        pad_to(&format!("   {e}"), w)
    } else if o.inflight && o.win.items.is_empty() {
        "   folding...".to_string()
    } else if o.win.items.len() >= 200 {
        format!("   200+ events · {}", o.order.key())
    } else {
        format!("   {} events · {}", o.win.items.len(), o.order.key())
    };
    rows.push(vec![Span::plain(pad_to(&footer, w))]);
    rows
}

/// One styled span of a panel row, in display columns. `brand` names the
/// theme's accent; both flags ride the span, so the paint pass reads them
/// without re-deriving which kind needs action.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Span {
    pub text: String,
    pub bold: bool,
    pub brand: bool,
}

impl Span {
    fn plain(text: impl Into<String>) -> Self {
        Span {
            text: text.into(),
            bold: false,
            brand: false,
        }
    }
}

/// Pad a span row to `w` display columns: pads the JOINED width, appending
/// one trailing plain span when the row falls short.
fn pad_to_spans(row: &mut Vec<Span>, w: usize) {
    let width: usize = row
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.text.as_str()))
        .sum();
    if width < w {
        row.push(Span::plain(" ".repeat(w - width)));
    }
}

/// The panel body as text: [`feed_panel_rows`] flattened, the shape the text
/// tests read and `draw_feed_panel` no longer re-derives. Test-only: the
/// paint path reads [`feed_panel_rows`] spans directly.
#[cfg(test)]
pub(crate) fn feed_panel_lines(
    o: &FeedOverlay,
    focused: bool,
    w: usize,
    visible_rows: usize,
    offset: usize,
) -> Vec<String> {
    feed_panel_rows(o, focused, w, visible_rows, offset)
        .iter()
        .map(|row| row.iter().map(|s| s.text.clone()).collect())
        .collect()
}

/// The click resolver, the exact inverse of the painter's slot list: painted
/// row 0 is the header, painted row `visible_rows - 1` is the footer, and a
/// row in between carries its STORAGE item index when that slot is an Item.
/// A header row resolves to None, so a header never opens a detail.
pub(crate) fn feed_row_item(
    items: &[FeedItem],
    painted_row: usize,
    visible_rows: usize,
    offset: usize,
    order: FeedOrder,
) -> Option<usize> {
    if painted_row == 0 || painted_row + 1 >= visible_rows {
        return None;
    }
    match display_slots(items, order).get(offset + painted_row - 1) {
        Some(Slot::Item(i)) => Some(*i),
        _ => None,
    }
}

/// The panel header for one input state, at the widest spelling that fits.
/// A clipped header is how the focus key stayed undiscoverable: the panel
/// drags to any width, and at the 40-column default the full sentence does
/// not fit.
///
/// The last spelling in each list leads with the KEY. The panel drags
/// narrower than any prose fits, and the caller pads and clips from the end,
/// so a label-first fallback loses the only place the key is advertised.
pub(crate) fn header_line(focused: bool, order: FeedOrder, w: usize) -> String {
    // The order word rides the FOCUSED spellings: `o` is a panel key, and
    // the header is the only place the key is advertised. Key-led fallbacks
    // for the widths prose cannot reach.
    let order_word = order.key();
    let candidates: [String; 4] = if focused {
        [
            format!(
                " FEED FOCUSED · up/down row · enter details · o order: {order_word} · esc close"
            ),
            format!(" FOCUSED · arrows move · enter details · o {order_word} · esc close"),
            format!(" FOCUSED · o {order_word} · esc close"),
            " esc close".to_string(),
        ]
    } else {
        [
            " activity feed · click row for details · E focus · e close".to_string(),
            " activity feed · click: details · E focus · e close".to_string(),
            " feed · click: details · E focus".to_string(),
            " E focus".to_string(),
        ]
    };
    let narrowest = if focused { " esc close" } else { " E focus" };
    candidates
        .into_iter()
        .find(|c| unicode_width::UnicodeWidthStr::width(c.as_str()) <= w)
        .unwrap_or_else(|| narrowest.to_string())
}

/// Drop `cols` DISPLAY columns off the front of `s`, so a pan never splits a
/// wide glyph in half: a two-column glyph straddling the cut is dropped whole.
/// Panning past the end yields the empty string rather than clamping, so the
/// caller's own ceiling is what stops the pan.
pub(crate) fn pan_by(s: &str, cols: usize) -> String {
    if cols == 0 {
        return s.to_string();
    }
    let mut skipped = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        if skipped >= cols {
            out.push(ch);
            continue;
        }
        skipped += unicode_width::UnicodeWidthChar::width(ch)
            .unwrap_or(0)
            .max(1);
    }
    out
}

/// The widest title in the panel, in display columns. The pan stops one
/// column short of this, so the widest row always keeps something on screen.
pub(crate) fn widest_title(items: &[FeedItem]) -> usize {
    items
        .iter()
        .map(|i| unicode_width::UnicodeWidthStr::width(i.title.as_str()))
        .max()
        .unwrap_or(0)
}

/// `HH:MM` in the operator's zone; an unparseable ts shows raw.
pub(crate) fn short_ts_in<Tz: chrono::TimeZone>(ts: &str, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => tz
            .from_utc_datetime(&t.naive_utc())
            .format("%H:%M")
            .to_string(),
        Err(_) => ts.to_string(),
    }
}

fn short_ts(ts: &str) -> String {
    short_ts_in(ts, &chrono::Local)
}

/// The feed panel's width until the operator drags its border once; persisted
/// thereafter, like the sideline's.
pub(crate) const FEED_DEFAULT_W: u16 = 40;

/// How long a settled fold stays fresh before the next run-loop tick refolds
/// the panel on its own.
pub(crate) const FEED_REFRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// The fold result channel the run loop hands [`maybe_kick`] and reads back in
/// its `feed_rx` arm.
pub(crate) type FoldTx = tokio::sync::mpsc::UnboundedSender<(u64, crate::feed_overlay::FoldResult)>;

// ---- The panel's View integration: geometry, input, paint ----

impl View {
    /// The panel's width in columns, or 0 when it is closed or the terminal
    /// cannot admit it. The sideline is the senior panel: the cap here is the
    /// terminal cap MINUS the sideline's own width, so the feed yields its
    /// columns on a tight terminal and the content floor holds with both
    /// panels open. The clamp is TRANSIENT: `feed_width` is never mutated by
    /// it, so a shrink-then-grow restores the operator's chosen width.
    pub(super) fn feed_panel_w(&self) -> u16 {
        if self.feed.is_none() {
            return 0;
        }
        let cols = self.term.1;
        let max =
            sideline_max_width(cols).min(cols.saturating_sub(self.panel_w() + MIN_CONTENT_COLS));
        if max < MIN_SLIM_PANEL_W {
            return 0;
        }
        self.feed_width.clamp(MIN_SLIM_PANEL_W, max)
    }

    /// True on the panel's divider column - the grab band for the width drag.
    pub(super) fn on_feed_border(&self, row: u16, col: u16) -> bool {
        let w = self.feed_panel_w();
        w > 0 && row >= TAB_BAR_ROWS && col == self.term.1 - w
    }

    /// Begin the width drag when a press lands on the divider, remembering the
    /// width at grab so a bare Esc can revert. `false` leaves the press to
    /// fall through.
    pub(crate) fn begin_border_drag(&mut self, row: u16, col: u16) -> bool {
        if !self.on_feed_border(row, col) {
            return false;
        }
        self.feed_drag = Some(SidelineDrag {
            start_width: self.feed_width,
            last_at: Instant::now(),
        });
        true
    }

    /// Set the panel to a free width from the dragged border column, clamped
    /// exactly as [`Self::feed_panel_w`] clamps. `false` when no drag is live,
    /// the terminal is too tight, or the width did not change.
    pub(super) fn drag_feed_to(&mut self, col: u16, now: Instant) -> bool {
        if self.feed_drag.is_none() {
            return false;
        }
        let cols = self.term.1;
        let max =
            sideline_max_width(cols).min(cols.saturating_sub(self.panel_w() + MIN_CONTENT_COLS));
        let Some(w) = cols.checked_sub(col) else {
            return false;
        };
        if max < MIN_SLIM_PANEL_W {
            return false;
        }
        let want = w.clamp(MIN_SLIM_PANEL_W, max);
        if let Some(drag) = self.feed_drag.as_mut() {
            drag.last_at = now;
        }
        if want == self.feed_panel_w() {
            return false;
        }
        self.feed_width = want;
        true
    }

    /// End a feed-border drag: persist the width when it moved, then refresh
    /// the hover accent so it never lingers past the gesture.
    pub(super) fn end_feed_drag(&mut self, row: u16, col: u16) {
        if let Some(drag) = self.feed_drag.take() {
            if drag.start_width != self.feed_width {
                view_store::save_feed_width(self.feed_width);
            }
        }
        self.refresh_hover_affordances(row, col);
    }

    /// Revert a feed-border drag to the width at grab; `false` when no drag is
    /// live or the width did not change, so no resize travels at all.
    pub(super) fn revert_feed_drag(&mut self) -> bool {
        let Some(drag) = self.feed_drag.take() else {
            return false;
        };
        let changed = self.feed_width != drag.start_width;
        self.feed_width = drag.start_width;
        changed
    }

    /// Wheel-scroll the item window by one row. The offset re-clamps against
    /// the CURRENT viewport and item count first, so terminal growth (or a
    /// shorter fold) can never leave the window parked on blank rows.
    pub(super) fn scroll_feed(&mut self, down: bool) {
        let Some(f) = self.feed.as_mut() else {
            return;
        };
        let visible = (self.term.0 as usize).saturating_sub(2); // header + footer
        let slot_len = display_slots(&f.win.items, f.order).len();
        let max_off = slot_len.saturating_sub(visible);
        let next = if max_off == 0 {
            0
        } else if down {
            (self.feed_offset + 1).min(max_off)
        } else {
            self.feed_offset.saturating_sub(1)
        };
        self.feed_offset = next;
        if next > 0 {
            f.scan_pages = 0;
        }
        Self::arm_feed_page(f, down, next == 0, max_off, 5);
    }

    /// The page-arm policy, shared by the wheel and the arrow keys: a
    /// gesture reaching the top edge reads history, one reaching the bottom
    /// of a detached head reads forward, and a client-matched scan may spend
    /// at most `scan_cap` pages per gesture before the footer names how far
    /// back it reached.
    pub(super) fn arm_feed_page(
        f: &mut FeedOverlay,
        down: bool,
        at_top: bool,
        max_off: usize,
        scan_cap: u32,
    ) {
        if !down && at_top && !f.win.at_oldest && !f.win.scan_cursor.is_empty() {
            if f.scan_pages >= scan_cap {
                return;
            }
            f.scan_pages += 1;
            f.want_page = Some(crate::feed_overlay::PageReq::Older(
                f.win.scan_cursor.clone(),
            ));
            f.want = true;
        } else if down && max_off > 0 && !f.win.head_attached {
            if let Some(c) = f.win.items.last().map(|i| i.cursor.clone()) {
                f.want_page = Some(crate::feed_overlay::PageReq::Newer(c));
                f.want = true;
            }
        }
    }

    /// The scroll offset clamped to what the CURRENT items and viewport can
    /// show, the single value every read path (paint, click, hover) shares.
    pub(super) fn feed_offset_clamped(&self) -> usize {
        let Some(f) = &self.feed else {
            return 0;
        };
        let visible = (self.term.0 as usize).saturating_sub(2); // header + footer
        let slot_len = display_slots(&f.win.items, f.order).len();
        if slot_len <= visible {
            return 0;
        }
        self.feed_offset.min(slot_len - visible)
    }

    /// The right-edge panel, [`View::draw_sideline`] inverted: divider on the
    /// panel's LEFT edge, header at row 0, items below, footer pinned to the
    /// last row. Runs after the pane blit so a stale pane rect mid-resize
    /// cannot bleed through, and before the overlay pass so a modal still
    /// wins its cells.
    pub(super) fn draw_feed_panel(&self, cells: &mut [Cell], rows: usize, cols: usize) {
        let Some(f) = &self.feed else {
            return;
        };
        let w = self.feed_panel_w() as usize;
        if w == 0 {
            return;
        }
        let x0 = cols - w;
        let focused = self.input_owner() == super::region_focus::RegionOwner::Feed;
        let span_rows = feed_panel_rows(f, focused, w - 1, rows, self.feed_offset_clamped());
        // The selected row wears the full-width cursor band while the panel
        // owns the keyboard: the theme's own band pair (selection surface,
        // stamp text), the same vocabulary the backlog board bands with.
        let band_row = if focused {
            Some(1 + f.sel.saturating_sub(self.feed_offset_clamped()))
        } else {
            None
        };
        for (r, row) in span_rows.iter().enumerate() {
            if r >= rows {
                break;
            }
            // Advance by DISPLAY columns, not char index: a double-width glyph
            // claims two columns and marks its right half a WIDE_SPACER, the
            // sideline's own contract, so the row never desyncs against the
            // terminal. Feed titles are arbitrary text, so the width comes
            // from unicode-width, not the sideline's trigram-only glyph_cols.
            let mut dcol = 0usize;
            for span in row {
                for ch in span.text.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch)
                        .unwrap_or(0)
                        .max(1);
                    if dcol + cw > w - 1 {
                        break;
                    }
                    cells[r * cols + x0 + 1 + dcol] = Cell {
                        c: ch,
                        fg: if span.brand {
                            self.theme.brand
                        } else {
                            Color::Default
                        },
                        bg: Color::Default,
                        flags: if span.bold { cell_flags::BOLD } else { 0 },
                    };
                    if cw == 2 && dcol + 2 < w {
                        cells[r * cols + x0 + 1 + dcol + 1] = Cell {
                            c: ' ',
                            fg: Color::Default,
                            bg: Color::Default,
                            flags: cell_flags::WIDE_SPACER,
                        };
                    }
                    dcol += cw;
                }
            }
            if band_row == Some(r) {
                let (bfg, bbg, bflags) = crate::theme::band_style(&self.theme);
                for c in (x0 + 1)..(x0 + w) {
                    let cell = &mut cells[r * cols + c];
                    cell.fg = bfg;
                    cell.bg = bbg;
                    if cell.flags & cell_flags::WIDE_SPACER == 0 {
                        cell.flags = bflags;
                    }
                }
            }
        }
        let border_active = self.hover_feed_border
            || self.feed_drag.is_some()
            || self.input_owner() == super::region_focus::RegionOwner::Feed;
        let (border_fg, border_flags) = if border_active {
            (self.theme.brand, cell_flags::BOLD)
        } else {
            (Color::Default, cell_flags::DIM)
        };
        for r in 0..rows {
            cells[r * cols + x0] = Cell {
                c: '│',
                fg: border_fg,
                bg: Color::Default,
                flags: border_flags,
            };
        }
        // The panel's esc chip, at the header row's right edge. A tap on it
        // presses Esc through the shared chip path: a focused panel reads the
        // Esc itself and closes; an unfocused one owns no keys, so
        // `esc_close::tap` closes it directly.
        let chip = " esc ";
        if rows > 0 && w > chip.len() {
            for (i, ch) in chip.chars().enumerate() {
                cells[x0 + w - chip.len() + i] = Cell {
                    c: ch,
                    fg: self.theme.brand,
                    bg: Color::Default,
                    flags: cell_flags::BOLD,
                };
            }
            crate::chrome::record_close_spans(
                (rows, cols),
                (0, x0 + 1),
                w - 1,
                &[(crate::chrome::ESC_CLOSE_HIT, w - 5, 3)],
            );
        }
    }

    /// The `chrome_hit` branch for the panel's columns: a click opens that
    /// row's PROVENANCE, and the deep link is that view's action. The divider
    /// is the drag band and the header/footer rows are chrome, never rows.
    ///
    /// The click used to fire the deep link straight off. It moved because
    /// "take me there" and "tell me what this was" are different questions,
    /// and the row had only one gesture to answer both with.
    pub(super) fn chrome_hit_feed(&self, row: u16, col: u16) -> Option<ChromeHit> {
        let feed_w = self.feed_panel_w();
        if feed_w == 0 || col < self.term.1 - feed_w {
            return None;
        }
        if col == self.term.1 - feed_w {
            return None;
        }
        if row as usize == (self.term.0 as usize).saturating_sub(1) && self.bottom_row_is_chrome() {
            return None;
        }
        let Some(f) = &self.feed else {
            return None;
        };
        feed_row_item(
            &f.win.items,
            row as usize,
            self.term.0 as usize,
            self.feed_offset_clamped(),
            f.order,
        )
        .and_then(|i| {
            let item = f.win.items.get(i)?;
            // A question row answers from the feed: the whole question opens
            // on the same path the questions view uses, never a provenance
            // detour (ruling 2026-09-29).
            if item.kind == "question_asked" {
                // A row whose question id never landed still inspects: the
                // click falls back to the provenance modal rather than
                // resolving to nothing (which would forward to a pane).
                return Some(match item.r#ref.clone() {
                    Some(qid) => ChromeHit::OpenQuestionDetail(qid),
                    None => ChromeHit::OpenFeedDetail(item.clone()),
                });
            }
            Some(ChromeHit::OpenFeedDetail(item.clone()))
        })
    }

    /// Keep the selected row inside the window after a keyboard move. The
    /// offset is re-clamped against the CURRENT viewport first, exactly as
    /// [`Self::scroll_feed`] does, so a shrunken terminal cannot leave the
    /// window parked past the last row.
    pub(super) fn follow_feed_selection(&mut self) {
        let visible = (self.term.0 as usize).saturating_sub(2);
        let Some(f) = self.feed.as_mut() else {
            return;
        };
        if visible == 0 {
            return;
        }
        let max_off = display_slots(&f.win.items, f.order)
            .len()
            .saturating_sub(visible);
        let sel = f.sel;
        let mut off = self.feed_offset.min(max_off);
        let was_top = off == 0;
        if sel < off {
            off = sel;
        } else if sel >= off + visible {
            off = sel + 1 - visible;
        }
        self.feed_offset = off.min(max_off);
        if off > 0 {
            f.scan_pages = 0;
        }
        let down = sel >= off + visible;
        Self::arm_feed_page(f, down, off == 0 && was_top, max_off, 5);
    }

    /// Open the provenance view for the selected row, copying the item OUT of
    /// the list. A later fold replaces `items` wholesale, so holding an index
    /// would re-point the open view at a different event mid-read.
    pub(super) fn open_feed_detail(&mut self) {
        // `sel` is a SLOT index (headers included); a header has no detail.
        // The clone ends the feed borrow, so the question route below can
        // open the questions detail on the same view.
        let item = {
            let Some(f) = &self.feed else {
                return;
            };
            display_slots(&f.win.items, f.order)
                .get(f.sel)
                .and_then(|s| match s {
                    Slot::Item(i) => f.win.items.get(*i),
                    Slot::Header(_) => None,
                })
                .cloned()
        };
        let Some(item) = item else {
            return;
        };
        // A question row answers from the feed, the same detail view the bell
        // opens.
        if item.kind == "question_asked" {
            if let Some(qid) = item.r#ref.as_deref() {
                self.open_detail_on(qid);
                return;
            }
        }
        self.feed_detail = Some(feed_detail::modal(self, item));
    }

    /// The hover marker follows the pointer inside the panel; anything else
    /// parks it on the newest row. Marker only - a hover never deep-links.
    pub(super) fn hover_feed_marker(&mut self, row: u16, col: u16) {
        if self.feed.is_none() {
            return;
        }
        let feed_w = self.feed_panel_w();
        let d = if feed_w > 0 && col > self.term.1 - feed_w {
            let items = &self.feed.as_ref().unwrap().win.items;
            // The resolver answers in STORAGE indexes; `sel` is a SLOT index.
            let order = self.feed.as_ref().map(|f| f.order).unwrap_or_default();
            feed_row_item(
                items,
                row as usize,
                self.term.0 as usize,
                self.feed_offset_clamped(),
                order,
            )
            .map(|storage| slot_of(items, storage, order))
            .unwrap_or(0)
        } else {
            0
        };
        if let Some(f) = self.feed.as_mut() {
            f.sel = d;
        }
    }
}

/// The run-loop fold kick: at most ONE fold in flight, armed by `want` (the
/// needs fold's generation/single-flight discipline).
pub(crate) fn maybe_kick(view: &mut View, tx: &FoldTx) {
    let Some(f) = view.feed.as_mut() else {
        return;
    };
    // Time-based re-arm, the backlog board's pattern: a settled fold goes
    // stale after FEED_REFRESH_EVERY and the next run-loop tick refolds, so
    // the panel is fresh without the operator touching anything.
    let due = f.want
        || f.last_fold
            .is_none_or(|t| t.elapsed() >= FEED_REFRESH_EVERY);
    if !due || f.inflight {
        return;
    }
    f.want = false;
    f.inflight = true;
    let tx = tx.clone();
    let gen = f.gen;
    // A wanted page first, else the 15s refresh tick as an incremental Live
    // read from the newest row ever seen.
    let req = f
        .want_page
        .take()
        .unwrap_or_else(|| {
            crate::feed_overlay::PageReq::Live(f.win.head_cursor.clone())
        });
    let filter = f.filter.clone();
    tokio::spawn(async move {
        let result = crate::feed_overlay::fetch_page(req, filter.as_ref()).await;
        let _ = tx.send((gen, result));
    });
}

/// A fold landed: apply only to the still-open, same-generation panel, and
/// reopen the scroll window on the newest row so a shorter result can never
/// leave the window parked past the last item (a blank panel).
pub(crate) fn apply_fold(
    view: &mut View,
    gen: u64,
    outcome: crate::feed_overlay::FoldResult,
) {
    let top_slot = view.feed_offset_clamped();
    let visible = (view.term.0 as usize).saturating_sub(2);
    let Some(f) = view.feed.as_mut() else {
        return;
    };
    if gen != f.gen {
        return;
    }
    f.inflight = false;
    let first = f.last_fold.is_none();
    f.last_fold = Some(Instant::now());
    match outcome {
        Ok(page) => {
            // The anchors: the cursors of the top visible row and the
            // selected row, recorded BEFORE the window moves, so a land
            // never scrolls the view (AC10).
            let order = f.order;
            let slots = display_slots(&f.win.items, order);
            // The anchor identity: the cursor when the row carries one, else
            // the (ts, kind, title) spelling a legacy row still answers to.
            let anchor_of = |it: &FeedItem| {
                if it.cursor.is_empty() {
                    format!("{}\u{1}{}\u{1}{}", it.ts, it.kind, it.title)
                } else {
                    it.cursor.clone()
                }
            };
            let anchor_top = slots
                .get(top_slot)
                .and_then(|s| match s {
                    Slot::Item(i) => f.win.items.get(*i).map(&anchor_of),
                    Slot::Header(_) => None,
                });
            let anchor_sel = slots
                .get(f.sel)
                .and_then(|s| match s {
                    Slot::Item(i) => f.win.items.get(*i).map(&anchor_of),
                    Slot::Header(_) => None,
                });
            let (req, raw) = (page.req, page.items);
            let scan_floor = raw.first().map(|i| i.cursor.clone());
            // Under a query the projection flags cannot express, the client
            // matcher filters each landed page before it enters the window.
            let rows = match (&f.parsed, f.pushed) {
                (Some(Ok(parsed)), false) => {
                    let ctx = crate::feed_overlay::EventCtx::from_rows(&[], &[]);
                    raw.into_iter()
                        .filter(|it| {
                            let fields = event_fields(it, &ctx);
                            parsed.keeps(&fields)
                        })
                        .collect()
                }
                _ => raw,
            };
            let land = f.win.land(&req, rows, scan_floor);
            let _ = land;
            // Re-anchor. The top anchor wins for the offset, the selection
            // anchor for the marker; a row the land dropped reads its slot
            // as absent and the clamp below keeps both in range.
            let slots = display_slots(&f.win.items, order);
            let slot_of_cursor = |cur: &str| {
                slots.iter().position(|s| matches!(s, Slot::Item(i) if {
                    f.win.items.get(*i).is_some_and(|it| &anchor_of(it) == cur)
                }))
            };
            if let (Some(cur), false) = (&anchor_top, first) {
                if let Some(d) = slot_of_cursor(cur) {
                    view.feed_offset = d;
                } else if matches!(req, crate::feed_overlay::PageReq::Head) {
                    view.feed_offset = 0;
                }
            } else if matches!(req, crate::feed_overlay::PageReq::Head) {
                view.feed_offset = 0;
            }
            if let Some(cur) = &anchor_sel {
                if let Some(d) = slot_of_cursor(cur) {
                    f.sel = d;
                }
            } else {
                f.sel = first_item_slot(&f.win.items, order);
            }
            // A refresh never lets the window park past the last row.
            let max_off = slots.len().saturating_sub(visible.max(1));
            if view.feed_offset > max_off {
                view.feed_offset = max_off;
            }
            f.error = None;
        }
        Err(e) => f.error = Some(e),
    }
}

/// The first ITEM slot: where a fresh selection parks, so the marker never
/// sits on a group header.
pub(crate) fn first_item_slot(items: &[FeedItem], order: FeedOrder) -> usize {
    display_slots(items, order)
        .iter()
        .position(|s| matches!(s, Slot::Item(_)))
        .unwrap_or(0)
}

/// The SLOT index a storage index renders at, for selection-keeping across
/// folds. A header-hunting fallback lands on the newest row.
fn slot_of(items: &[FeedItem], storage: usize, order: FeedOrder) -> usize {
    display_slots(items, order)
        .iter()
        .position(|s| matches!(s, Slot::Item(i) if *i == storage))
        .unwrap_or(0)
}

/// The `e` toggle. Opening keeps the contract - prior rows render
/// instantly, a fresh fold always arms, a failure degrades loudly - and both
/// transitions re-report the content area: a close that sent no Resize would
/// leave the panes narrowed for good.
pub(crate) async fn toggle(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let gen = view
        .feed
        .as_ref()
        .map(|f| f.gen.wrapping_add(1))
        .unwrap_or(0);
    if view.feed.is_none() {
        view.feed = Some(open_overlay(view.feed.take(), gen));
    } else {
        // Closing releases the keyboard with the panel, so a later reopen
        // never starts already holding it. The owner field is reset too:
        // `input_owner` normalizes a closed panel to Pane, but the stored
        // choice would silently RE-focus a reopened panel.
        view.feed = None;
        view.feed_detail = None;
        view.region_owner = super::region_focus::RegionOwner::Pane;
        // A half-read escape sequence must not survive the close: carried
        // into the next focus it folds with the fresh bytes into a key
        // nobody pressed.
        view.feed_esc.clear();
    }
    let (r, c) = view.content_dims();
    write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
        .await
        .map_err(|e| format!("resize send failed: {e}"))
}

/// The feed-border drag in flight: drag reports resize per crossed column,
/// release (or any non-left event) ends it. `Ok(true)` = consumed.
pub(crate) async fn drag_mouse(
    view: &mut View,
    row: u16,
    col: u16,
    kind: MouseKind,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<bool, String> {
    if view.feed_drag.is_none() {
        return Ok(false);
    }
    match kind {
        MouseKind::Drag(MouseButton::Left) => {
            if view.drag_feed_to(col, Instant::now()) {
                let (r, c) = view.content_dims();
                write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                    .await
                    .map_err(|e| format!("feed resize send failed: {e}"))?;
            }
            Ok(true)
        }
        MouseKind::Release(MouseButton::Left) => {
            view.end_feed_drag(row, col);
            Ok(true)
        }
        _ => {
            view.end_feed_drag(row, col);
            Ok(true)
        }
    }
}

/// A bare Esc during a feed-border drag reverts the width to where the drag
/// began; `Ok(true)` = the Esc was the drag's.
pub(crate) async fn esc_revert(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<bool, String> {
    if view.feed_drag.is_none() {
        return Ok(false);
    }
    if view.revert_feed_drag() {
        let (rows, cols) = view.content_dims();
        write_msg(sock_w, &ClientMsg::Resize { rows, cols })
            .await
            .map_err(|e| format!("feed revert resize send failed: {e}"))?;
    }
    Ok(true)
}

/// The `E` focus. Opens the panel when it is closed, then takes the keyboard
/// either way. This is the ONE place the key-free default is set aside, and
/// only ever on the operator's explicit gesture.
pub(crate) async fn focus(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    if view.feed.is_none() {
        toggle(view, sock_w).await?;
    }
    view.region_owner = super::region_focus::RegionOwner::Feed;
    view.feed_esc.clear();
    Ok(())
}

/// The focused feed panel's keys, and the provenance view's.
///
/// Reached only when the operator asked for it: `E` focused the panel, or a
/// click opened a row's provenance. Every other time the panel takes no keys
/// at all and this function is never called, which is the whole point.
///
/// Precedence inside: the provenance view wins while it is open (it is the
/// thing in front), then the focused panel. Esc unwinds one layer at a time -
/// the view first, then the panel - so a reader never loses both at once, and
/// each landing keeps the keyboard with the feed. From the panel itself, Esc
/// and e close it; the old Esc-only release left the panel open but key-less,
/// a state no further key could leave.
pub(crate) async fn feed_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.feed_esc);
    let toks = fold_modal_keys(&mut esc, bytes);
    view.feed_esc = esc;
    for tok in toks {
        if view.feed_detail.is_some() {
            match tok {
                ModalKey::Esc | ModalKey::Byte(b'q') | ModalKey::Byte(b'e') => {
                    view.feed_detail = None;
                }
                ModalKey::Up => {
                    if let Some(m) = view.feed_detail.as_mut() {
                        m.popup.nav(crate::popup::NavDir::Up);
                        m.popup.follow_sel(view.term);
                    }
                }
                // The node_created modal's composer gesture, carried over
                // from the line-built view: b opens the agent composer
                // pre-filled with the node-bound blueprint command.
                ModalKey::Byte(b'b') => {
                    let launch = view.feed_detail.as_ref().and_then(|m| {
                        feed_detail::plan_node(&m.item)
                            .map(|node| (node.to_owned(), m.item.cwd.clone()))
                    });
                    if let Some((node, cwd)) = launch {
                        if super::sideline::show_composer(view, sock_w).await? {
                            view.feed_detail = None;
                            if let Err(err) = super::agent_launcher::open_with(
                                view,
                                format!("/fno:blueprint {node}"),
                                cwd.as_deref(),
                                node,
                            ) {
                                view.set_notice(err);
                            }
                        }
                    }
                }
                // The selected row's action, never the modal's dismissal:
                // inspecting stays open so the next field is one arrow away.
                ModalKey::Down => {
                    if let Some(m) = view.feed_detail.as_mut() {
                        m.popup.nav(crate::popup::NavDir::Down);
                        m.popup.follow_sel(view.term);
                    }
                }
                ModalKey::Enter => feed_detail::execute_selected(view, sock_w).await?,
                ModalKey::Byte(b'y') => feed_detail::copy_selected(view),
                _ => {}
            }
            continue;
        }
        if matches!(tok, ModalKey::Esc | ModalKey::Byte(b'e')) {
            // Close once; a second close token in the same chunk is
            // swallowed, never a reopen.
            if view.feed.is_some() {
                toggle(view, sock_w).await?;
            }
            continue;
        }
        let Some(f) = view.feed.as_mut() else {
            break; // closed mid-chunk: swallow the rest, never forward
        };
        let len = display_slots(&f.win.items, f.order).len();
        match tok {
            ModalKey::Esc => {}
            ModalKey::Up => {
                // The marker skips headers: the nearest ITEM slot above.
                let slots = display_slots(&f.win.items, f.order);
                f.sel = (0..f.sel)
                    .rev()
                    .find(|s| matches!(slots.get(*s), Some(Slot::Item(_))))
                    .unwrap_or(f.sel);
                view.follow_feed_selection();
            }
            ModalKey::Down => {
                let slots = display_slots(&f.win.items, f.order);
                f.sel = (f.sel + 1..slots.len())
                    .find(|s| matches!(slots.get(*s), Some(Slot::Item(_))))
                    .unwrap_or(f.sel);
                view.follow_feed_selection();
            }
            // Panning moves the TITLE only; the stamp, kind and node stay
            // anchored, so a panned row is still the row you selected.
            ModalKey::Left => f.hpan = f.hpan.saturating_sub(1),
            ModalKey::Right => {
                // One column short of the widest title. AT that width every
                // row is blank, so the pan would strand the operator in an
                // empty panel with nothing on screen to pan back by.
                let ceiling = feed_view::widest_title(&f.win.items).saturating_sub(1);
                f.hpan = (f.hpan + 1).min(ceiling);
            }
            ModalKey::PageUp => {
                let page = (view.term.0 as usize).saturating_sub(2).max(1);
                f.sel = f.sel.saturating_sub(page);
                view.follow_feed_selection();
            }
            ModalKey::PageDown => {
                let page = (view.term.0 as usize).saturating_sub(2).max(1);
                f.sel = (f.sel + page).min(len.saturating_sub(1));
                view.follow_feed_selection();
            }
            ModalKey::Enter => view.open_feed_detail(),
            // Jump to the top (or arm a Head when the head is detached),
            // clearing the new-row marker.
            ModalKey::Byte(b'g') => {
                if f.win.home_is_local() {
                    view.feed_offset = 0;
                    f.win.new_count = 0;
                    f.sel = first_item_slot(&f.win.items, f.order);
                } else {
                    f.want_page = Some(crate::feed_overlay::PageReq::Head);
                    f.want = true;
                }
            }
            ModalKey::Byte(b'G') => {
                let visible = (view.term.0 as usize).saturating_sub(2).max(1);
                let slot_len = display_slots(&f.win.items, f.order).len();
                let max_off = slot_len.saturating_sub(visible);
                view.feed_offset = max_off;
                let sel = display_slots(&f.win.items, f.order)
                    .len()
                    .saturating_sub(1);
                f.sel = sel;
                View::arm_feed_page(f, true, false, max_off, 5);
            }
            // The order toggle: the panel's ONE local preference, persisted
            // per client like the dragged width.
            ModalKey::Byte(b'o') => {
                f.order = f.order.next();
                crate::view_store::save_feed_order(f.order.key());
            }
            ModalKey::Byte(_) => {}
        }
    }
    Ok(StdinFlow::Continue)
}
