//! The activity feed panel (x-4433, x-f089): questions, decisions and node
//! lifecycle, newest first, one deep link per row. The render moved-module
//! idiom matches `needs_view.rs`; the data comes from
//! [`crate::feed_overlay::feed_now`], which shells the `fno-agents feed`
//! projection off the UI loop and renders a typed reason when it fails
//! (x-d15a), never one generic sentence for five causes.
//!
//! The deep link is the sideline's own path, not a new one: a row that joins
//! a live roster row resolves through `agent_hit` exactly as a sideline click
//! does (FocusPane, or AttachAgent on portal 0); an unjoined row carrying a
//! session id attaches that id directly, and the server's existing
//! `no such agent` notice is what a dead session answers.
//!
//! Since x-f089 the feed is the right-edge PANEL the operator specified, not
//! a centered modal: `e` toggles it, the border drags, and an UNFOCUSED panel
//! consumes no keys at all, so typing reaches the focused pane.
//!
//! That key-free rule is the default, not the whole story. `E` focuses the
//! panel explicitly: while focused it takes the arrows (row select and
//! horizontal pan), Enter and Esc, and the header says so. Esc releases it.
//! The property the rule protects - typing reaches the pane - holds whenever
//! the operator has not asked for the opposite.
//!
//! A click no longer fires the deep link straight off. It opens the row's
//! PROVENANCE (`feed_detail`), and the deep link is that view's footer
//! action. Inspecting never attaches or resumes anything on its own.

use super::*;
use crate::feed_overlay::{FeedError, FeedItem};

/// The panel's open state: the items the last fold landed, the hover marker
/// (a display index, NEWEST FIRST, the order the rows render in), and the
/// same generation/single-flight discipline the needs fold runs. `want` arms
/// the run-loop kick; `gen` invalidates a result that lands after a close or
/// a re-open. `sel` follows the pointer while the panel is UNFOCUSED, and the
/// arrows while it is focused - the panel consumes keys only then.
pub(crate) struct FeedOverlay {
    pub(crate) items: Vec<FeedItem>,
    /// Hovered row in display order; the `▸` marker paints here.
    pub(crate) sel: usize,
    /// The typed fold failure, if the last fold failed. `Some` renders its
    /// reason verbatim; a success fold clears it.
    pub(crate) error: Option<crate::feed_overlay::FeedError>,
    pub(crate) inflight: bool,
    pub(crate) want: bool,
    pub(crate) gen: u64,
    /// True while the panel holds the keyboard. Set by `E`, cleared by Esc
    /// and by every close, so a reopen never starts holding it.
    pub(crate) focused: bool,
    /// Horizontal pan into the TITLE, in display columns. The timestamp, kind
    /// and node stay anchored so a panned row is still identifiable.
    pub(crate) hpan: usize,
}

/// A fresh open: the prior items ride over (instant content), but a refold is
/// always armed - history may have moved since the last open, and the fold is
/// cheap and off-loop.
pub(crate) fn open_overlay(prior: Option<FeedOverlay>, gen: u64) -> FeedOverlay {
    FeedOverlay {
        items: prior.map(|f| f.items).unwrap_or_default(),
        sel: 0,
        error: None,
        inflight: false,
        want: true,
        gen,
        focused: false,
        hpan: 0,
    }
}

/// The panel body: one header line, up to `visible_rows - 2` item rows, then
/// the footer pinned to the last row. The projection hands rows OLDEST
/// FIRST, so display index `d` reads storage index `len - 1 - d` and the top
/// row is the newest event - the same view `feed_row_item` inverts for the
/// click resolver, so a row and its deep link always name the same event.
pub(crate) fn feed_panel_lines(
    o: &FeedOverlay,
    w: usize,
    visible_rows: usize,
    offset: usize,
) -> Vec<String> {
    // The header says which input state the panel is in, because the rule is
    // not guessable: an unfocused panel takes no keys at all. It is also the
    // ONLY place the focus key is advertised, so it degrades to a shorter
    // spelling on a narrow panel rather than being clipped away.
    let mut lines = vec![pad_to(header_line(o.focused, w), w)];
    let visible = visible_rows.saturating_sub(2);
    for d in offset..offset + visible {
        match o
            .items
            .len()
            .checked_sub(d + 1)
            .and_then(|i| o.items.get(i))
        {
            Some(item) => {
                // The marker lands on the hovered row, or on the selected row
                // while the panel holds the keyboard.
                let marker = if d == o.sel { '▸' } else { ' ' };
                let node = item.node.as_deref().unwrap_or("-");
                lines.push(pad_to(
                    &format!(
                        " {marker} {} {:<16} {} · {}",
                        short_ts(&item.ts),
                        item.kind,
                        node,
                        pan_by(&item.title, o.hpan)
                    ),
                    w,
                ));
            }
            // Short list: the rows below the last item render blank.
            None => lines.push(pad_to("", w)),
        }
    }
    // The empty notice only when the fold has SETTLED empty: "no activity"
    // beside a still-running fold is a claim the fold has not earned yet.
    if o.items.is_empty() && o.error.is_none() && !o.inflight && visible > 0 {
        lines[1] = pad_to("   no activity in the last 24h", w);
    }
    let footer = if let Some(e) = &o.error {
        // The typed reason renders verbatim (x-d15a): a timeout names its
        // budget, an admission refusal its slot count. pad_to truncates a
        // long stderr tail; the cause still leads the line.
        pad_to(&format!("   {e}"), w)
    } else if o.inflight && o.items.is_empty() {
        "   folding...".to_string()
    } else if o.items.len() >= 200 {
        "   200+ events · newest first".to_string()
    } else {
        format!("   {} events · newest first", o.items.len())
    };
    lines.push(pad_to(&footer, w));
    lines
}

/// The click resolver, the exact inverse of [`feed_panel_lines`]'s mapping:
/// painted row 0 is the header, painted row `visible_rows - 1` is the footer,
/// and a row in between carries display item `offset + row - 1` when that
/// index exists. `None` everywhere else, so chrome rows never deep-link.
pub(crate) fn feed_row_item(
    item_len: usize,
    painted_row: usize,
    visible_rows: usize,
    offset: usize,
) -> Option<usize> {
    if painted_row == 0 || painted_row + 1 >= visible_rows {
        return None;
    }
    let d = offset + painted_row - 1;
    (d < item_len).then_some(d)
}

/// The panel header for one input state, at the widest spelling that fits.
/// A clipped header is how the focus key stayed undiscoverable: the panel
/// drags to any width, and at the 40-column default the full sentence does
/// not fit.
/// The last spelling in each list leads with the KEY. The panel drags
/// narrower than any prose fits, and the caller pads and clips from the end,
/// so a label-first fallback loses the only place the key is advertised.
pub(crate) fn header_line(focused: bool, w: usize) -> &'static str {
    let candidates: [&str; 4] = if focused {
        [
            " FEED FOCUSED · up/down row · left/right pan · enter details · esc release",
            " FEED FOCUSED · arrows move · enter details · esc release",
            " FOCUSED · enter details · esc release",
            " esc release",
        ]
    } else {
        [
            " activity feed · click row for details · E focus · e close",
            " activity feed · click: details · E focus · e close",
            " feed · click: details · E focus",
            " E focus",
        ]
    };
    candidates
        .into_iter()
        .find(|c| unicode_width::UnicodeWidthStr::width(*c) <= w)
        .unwrap_or(candidates[candidates.len() - 1])
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

/// `HH:MM` out of an RFC3339 stamp; an unparseable ts shows raw.
fn short_ts(ts: &str) -> String {
    ts.get(11..16).unwrap_or(ts).to_string()
}

/// The feed panel's width until the operator drags its border once; persisted
/// thereafter, like the sideline's.
pub(crate) const FEED_DEFAULT_W: u16 = 40;

/// The fold result channel the run loop hands [`maybe_kick`] and reads back in
/// its `feed_rx` arm.
pub(crate) type FoldTx = tokio::sync::mpsc::UnboundedSender<(u64, crate::feed_overlay::FoldResult)>;

// ---- (x-f089) The panel's View integration: geometry, input, paint ----

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
        let Some(f) = &self.feed else {
            return;
        };
        let visible = (self.term.0 as usize).saturating_sub(2); // header + footer
        let max_off = f.items.len().saturating_sub(visible);
        self.feed_offset = if max_off == 0 {
            0
        } else if down {
            (self.feed_offset + 1).min(max_off)
        } else {
            self.feed_offset.saturating_sub(1)
        };
    }

    /// The scroll offset clamped to what the CURRENT items and viewport can
    /// show, the single value every read path (paint, click, hover) shares.
    pub(super) fn feed_offset_clamped(&self) -> usize {
        let Some(f) = &self.feed else {
            return 0;
        };
        let visible = (self.term.0 as usize).saturating_sub(2); // header + footer
        if f.items.len() <= visible {
            return 0;
        }
        self.feed_offset.min(f.items.len() - visible)
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
        let lines = feed_panel_lines(f, w - 1, rows, self.feed_offset_clamped());
        for (r, line) in lines.iter().enumerate() {
            if r >= rows {
                break;
            }
            // Advance by DISPLAY columns, not char index: a double-width glyph
            // claims two columns and marks its right half a WIDE_SPACER, the
            // sideline's own contract, so the row never desyncs against the
            // terminal. Feed titles are arbitrary text, so the width comes
            // from unicode-width, not the sideline's trigram-only glyph_cols.
            let mut dcol = 0usize;
            for ch in line.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch)
                    .unwrap_or(0)
                    .max(1);
                if dcol + cw > w - 1 {
                    break;
                }
                cells[r * cols + x0 + 1 + dcol] = Cell {
                    c: ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    flags: 0,
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
        let border_active = self.hover_feed_border || self.feed_drag.is_some();
        let (border_fg, border_flags) = if border_active {
            (self.theme.accent, cell_flags::BOLD)
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
            f.items.len(),
            row as usize,
            self.term.0 as usize,
            self.feed_offset_clamped(),
        )
        .and_then(|d| {
            let item = f.items.get(f.items.len() - 1 - d)?;
            Some(ChromeHit::OpenFeedDetail(item.clone()))
        })
    }

    /// Keep the selected row inside the window after a keyboard move. The
    /// offset is re-clamped against the CURRENT viewport first, exactly as
    /// [`Self::scroll_feed`] does, so a shrunken terminal cannot leave the
    /// window parked past the last row.
    pub(super) fn follow_feed_selection(&mut self) {
        let visible = (self.term.0 as usize).saturating_sub(2);
        let Some(f) = &self.feed else {
            return;
        };
        if visible == 0 {
            return;
        }
        let max_off = f.items.len().saturating_sub(visible);
        let sel = f.sel;
        let mut off = self.feed_offset.min(max_off);
        if sel < off {
            off = sel;
        } else if sel >= off + visible {
            off = sel + 1 - visible;
        }
        self.feed_offset = off.min(max_off);
    }

    /// Open the provenance view for the selected row, copying the item OUT of
    /// the list. A later fold replaces `items` wholesale, so holding an index
    /// would re-point the open view at a different event mid-read.
    pub(super) fn open_feed_detail(&mut self) {
        let Some(f) = &self.feed else {
            return;
        };
        // `sel` is a DISPLAY index, newest first; the list is oldest first.
        let Some(item) = f
            .items
            .len()
            .checked_sub(f.sel + 1)
            .and_then(|i| f.items.get(i))
        else {
            return;
        };
        self.feed_detail_of = Some(item.clone());
    }

    /// What the provenance view's Enter does, resolved from the SAME evidence
    /// the view rendered its footer from - one `Destination`, read twice. Resolving
    /// it twice is how the footer came to promise a command the action did not
    /// send: the footer joined on the exact session id while the action joined
    /// on the row name, so a node_created row with a live worker on that node
    /// read `esc close` and then focused a pane.
    pub(super) fn feed_detail_hit(&self) -> Option<ChromeHit> {
        let item = self.feed_detail_of.as_ref()?;
        let dest = feed_detail::destination(&self.layout.agents, item);
        feed_detail::detail_hit(self, &dest)
    }

    /// The hover marker follows the pointer inside the panel; anything else
    /// parks it on the newest row. Marker only - a hover never deep-links.
    pub(super) fn hover_feed_marker(&mut self, row: u16, col: u16) {
        if self.feed.is_none() {
            return;
        }
        let len = self.feed.as_ref().map(|f| f.items.len()).unwrap_or(0);
        let feed_w = self.feed_panel_w();
        let d = if feed_w > 0 && col > self.term.1 - feed_w {
            feed_row_item(
                len,
                row as usize,
                self.term.0 as usize,
                self.feed_offset_clamped(),
            )
            .map(|d| d.min(len.saturating_sub(1)))
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
    if !f.want || f.inflight {
        return;
    }
    f.want = false;
    f.inflight = true;
    let tx = tx.clone();
    let gen = f.gen;
    let since = crate::digest_overlay::now_secs()
        .saturating_sub(NEEDS_WINDOW_SECS)
        .to_string();
    tokio::spawn(async move {
        let result = crate::feed_overlay::feed_now(&since).await;
        let _ = tx.send((gen, result));
    });
}

/// A fold landed: apply only to the still-open, same-generation panel, and
/// reopen the scroll window on the newest row so a shorter result can never
/// leave the window parked past the last item (a blank panel).
pub(crate) fn apply_fold(view: &mut View, gen: u64, outcome: Result<Vec<FeedItem>, FeedError>) {
    let Some(f) = view.feed.as_mut() else {
        return;
    };
    if gen != f.gen {
        return;
    }
    f.inflight = false;
    match outcome {
        Ok(items) => {
            f.items = items;
            f.sel = 0;
            f.error = None;
            view.feed_offset = 0;
        }
        Err(e) => f.error = Some(e),
    }
}

/// The `e` toggle. Opening keeps the x-4433 contract - prior rows render
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
        // never starts already holding it.
        view.feed = None;
        view.feed_detail_of = None;
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
    if let Some(f) = view.feed.as_mut() {
        f.focused = true;
    }
    view.feed_esc.clear();
    Ok(())
}

/// Release the keyboard, leaving the panel open. The Esc half of `focus`.
pub(crate) fn release(view: &mut View) -> bool {
    match view.feed.as_mut() {
        Some(f) if f.focused => {
            f.focused = false;
            true
        }
        _ => false,
    }
}

/// The focused feed panel's keys, and the provenance view's.
///
/// Reached only when the operator asked for it: `E` focused the panel, or a
/// click opened a row's provenance. Every other time the panel takes no keys
/// at all and this function is never called, which is the whole point.
///
/// Precedence inside: the provenance view wins while it is open (it is the
/// thing in front), then the focused panel. Esc unwinds one layer at a time -
/// the view first, then the focus - so a reader never loses both at once.
pub(crate) async fn feed_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.feed_esc);
    let toks = fold_modal_keys(&mut esc, bytes);
    view.feed_esc = esc;
    for tok in toks {
        if view.feed_detail_of.is_some() {
            match tok {
                ModalKey::Esc | ModalKey::Byte(b'q') | ModalKey::Byte(b'e') => {
                    view.feed_detail_of = None;
                }
                ModalKey::Enter => {
                    // The deep link is the view's ACTION, never its opening
                    // gesture: inspecting attaches and resumes nothing.
                    if let Some(hit) = view.feed_detail_hit() {
                        apply_hit(view, hit, sock_w).await?;
                    }
                    view.feed_detail_of = None;
                }
                _ => {}
            }
            continue;
        }
        if matches!(tok, ModalKey::Esc) {
            release(view);
            continue;
        }
        let Some(f) = view.feed.as_mut() else {
            break; // closed mid-chunk: swallow the rest, never forward
        };
        let len = f.items.len();
        match tok {
            ModalKey::Esc => {}
            ModalKey::Up => {
                f.sel = f.sel.saturating_sub(1);
                view.follow_feed_selection();
            }
            ModalKey::Down => {
                f.sel = (f.sel + 1).min(len.saturating_sub(1));
                view.follow_feed_selection();
            }
            // Panning moves the TITLE only; the stamp, kind and node stay
            // anchored, so a panned row is still the row you selected.
            ModalKey::Left => f.hpan = f.hpan.saturating_sub(1),
            ModalKey::Right => {
                // One column short of the widest title. AT that width every
                // row is blank, so the pan would strand the operator in an
                // empty panel with nothing on screen to pan back by.
                let ceiling = feed_view::widest_title(&f.items).saturating_sub(1);
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
            ModalKey::Byte(_) => {}
        }
    }
    Ok(StdinFlow::Continue)
}
