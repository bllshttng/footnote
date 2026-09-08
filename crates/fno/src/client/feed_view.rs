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
use crate::feed_overlay::FeedItem;

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

/// The panel body: one header line, up to `visible_rows - 2` item rows
/// (the window opens at `offset` in display order), then the footer pinned to
/// the last row. Every line is padded to `w` (the panel's text width), so the
/// painter blits lines 1:1 into rows `0..visible_rows` and the click resolver
/// [`feed_row_item`] inverts the mapping exactly.
pub(crate) fn feed_panel_lines(
    o: &FeedOverlay,
    w: usize,
    visible_rows: usize,
    offset: usize,
) -> Vec<String> {
    let mut lines = vec![pad_to(" activity feed · click row opens · e close", w)];
    let visible = visible_rows.saturating_sub(2);
    for d in offset..offset + visible {
        match o.items.get(d) {
            Some(item) => {
                // Display index d is storage index len-1-d (newest first); the
                // hover marker lands on the row the pointer is on.
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
