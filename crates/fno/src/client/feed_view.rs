//! The activity feed overlay (x-4433): questions, decisions and node
//! lifecycle, newest first, one deep link per row. The render moved-module
//! idiom matches `needs_view.rs`; the data comes from
//! [`crate::feed_overlay::feed_now`], which shells the `fno-agents feed`
//! projection off the UI loop under the shared 800ms cap.
//!
//! The deep link is the sideline's own path, not a new one: a row that joins
//! a live roster row resolves through `agent_hit` exactly as a sideline click
//! does (FocusPane, or AttachAgent on portal 0); an unjoined row carrying a
//! session id attaches that id directly, and the server's existing
//! `no such agent` notice is what a dead session answers.

use super::*;
use crate::feed_overlay::FeedItem;

/// The overlay's open state: the items the last fold landed, the display
/// cursor (indexed NEWEST FIRST, the order the rows render in), and the same
/// generation/single-flight discipline the needs fold runs. `want` arms the
/// run-loop kick; `gen` invalidates a result that lands after a close or a
/// re-open.
pub(crate) struct FeedOverlay {
    pub(crate) items: Vec<FeedItem>,
    pub(crate) sel: usize,
    pub(crate) degraded: bool,
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
        degraded: false,
        inflight: false,
        want: true,
        gen,
    }
}

/// The cursor's line in `feed_overlay_lines` output, for the follow-the-cursor
/// scroll the overlay viewport does. Pinned 1:1 with the line builder below:
/// one instruction line, then one line per item.
pub(crate) fn feed_selected_line(sel: usize) -> usize {
    1 + sel
}

/// The overlay body, newest first: a `▸` marks the cursor, each row shows the
/// local time, the kind, the node (when the row carries one) and the title.
/// The footer states the true count and the fold state - a failed fold
/// degrades loudly, never silently empty.
pub(crate) fn feed_overlay_lines(o: &FeedOverlay) -> Vec<String> {
    let mut lines = vec![pad_to(
        " activity feed · j/k move · ⏎ open session · q close",
        ANSWER_OVERLAY_W,
    )];
    // sel indexes DISPLAY order (newest first), so it maps to the storage
    // index len-1-sel; the marker and the Enter deep link must agree.
    let marked = o.items.len().saturating_sub(1) - o.sel.min(o.items.len().saturating_sub(1));
    for (i, item) in o.items.iter().enumerate().rev() {
        let marker = if i == marked { '▸' } else { ' ' };
        let node = item.node.as_deref().unwrap_or("-");
        lines.push(pad_to(
            &format!(
                " {marker} {} {:<16} {} · {}",
                short_ts(&item.ts),
                item.kind,
                node,
                item.title
            ),
            ANSWER_OVERLAY_W,
        ));
    }
    if o.items.is_empty() && !o.degraded {
        lines.push(pad_to("   no activity in the last 24h", ANSWER_OVERLAY_W));
    }
    let footer = if o.degraded {
        "   feed unavailable - fno agents feed failed".to_string()
    } else if o.inflight && o.items.is_empty() {
        "   folding...".to_string()
    } else if o.items.len() >= 200 {
        "   200+ events · newest first".to_string()
    } else {
        format!("   {} events · newest first", o.items.len())
    };
    lines.push(pad_to(&footer, ANSWER_OVERLAY_W));
    lines
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

/// Feed overlay keys (x-4433): j/k (or folded arrows) move the cursor,
/// Enter deep-links the row's session and closes, q/Esc closes, and an empty
/// overlay dismisses on any key. Closing drops the whole state, so an
/// in-flight fold's result lands on `None` and is discarded.
pub(crate) async fn feed_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.feed_esc);
    let keys = fold_selector_keys(&mut esc, bytes);
    view.feed_esc = esc;
    for &k in &keys {
        let Some(f) = view.feed.as_mut() else {
            break; // closed mid-chunk
        };
        if f.items.is_empty() {
            view.feed = None;
            continue;
        }
        let len = f.items.len();
        let cur = f.sel.min(len - 1);
        match k {
            b'j' | b'n' => f.sel = (cur + 1) % len,
            b'k' | b'N' => f.sel = (cur + len - 1) % len,
            b'\r' | b'\n' => {
                let item = f.items[len - 1 - cur].clone();
                view.feed = None;
                if let Some(hit) = feed_hit(view, &item) {
                    apply_hit(view, hit, sock_w).await?;
                }
            }
            b'q' | 0x1b => view.feed = None,
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}
