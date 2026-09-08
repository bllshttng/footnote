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
//! a centered modal: `e` toggles it, a click deep-links a row, the border
//! drags, and typing reaches the focused pane because the panel consumes no
//! keys at all.

use super::*;
use crate::feed_overlay::{FeedError, FeedItem};

/// The panel's open state: the items the last fold landed, the hover marker
/// (a display index, NEWEST FIRST, the order the rows render in), and the
/// same generation/single-flight discipline the needs fold runs. `want` arms
/// the run-loop kick; `gen` invalidates a result that lands after a close or
/// a re-open. `sel` follows the pointer, never the keyboard: the panel is
/// chrome and consumes no keys.
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
    let mut lines = vec![pad_to(" activity feed · click row opens · e close", w)];
    let visible = visible_rows.saturating_sub(2);
    for d in offset..offset + visible {
        match o
            .items
            .len()
            .checked_sub(d + 1)
            .and_then(|i| o.items.get(i))
        {
            Some(item) => {
                // The hover marker lands on the row the pointer is on.
                let marker = if d == o.sel { '▸' } else { ' ' };
                let node = item.node.as_deref().unwrap_or("-");
                lines.push(pad_to(
                    &format!(
                        " {marker} {} {:<16} {} · {}",
                        short_ts(&item.ts),
                        item.kind,
                        node,
                        item.title
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

/// `HH:MM` out of an RFC3339 stamp; an unparseable ts shows raw.
fn short_ts(ts: &str) -> String {
    ts.get(11..16).unwrap_or(ts).to_string()
}

/// The deep link. Joined first (the sideline's own resolution - node id
/// matches a row's name or worktree basename, session id its harness id), so
/// a live worker's row gets exactly the command a sideline click on that
/// worker yields. Unjoined but carrying a session id: attach it on portal 0;
/// a dead session answers through the server's existing refusal notice. No
/// session id at all: not selectable.
pub(crate) fn feed_hit(view: &View, item: &FeedItem) -> Option<ChromeHit> {
    let keys: Vec<&str> = [item.node.as_deref(), item.session_id.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    if let Some(row) = view.layout.agents.iter().find(|a| {
        keys.iter().any(|k| a.name == *k)
            || a.cwd_base.as_deref().is_some_and(|c| keys.contains(&c))
    }) {
        return Some(agent_hit(row, view.layout.active_squad));
    }
    item.session_id.as_deref().map(|sid| {
        ChromeHit::Cmds(vec![Command::AttachAgent {
            id: sid.to_string(),
            placement: PanePlacement {
                portal: Some(0),
                ..PanePlacement::default()
            },
        }])
    })
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

    /// The `chrome_hit` branch for the panel's columns: a click resolves
    /// through the feed's deep link; the divider is the drag band and the
    /// header/footer rows are chrome, never rows.
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
            feed_hit(self, item)
        })
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
    if !(f.want && !f.inflight) {
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
        view.feed = None;
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
