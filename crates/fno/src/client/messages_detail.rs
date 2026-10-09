//! The session details modal behind a Messages row's `d` (the design's
//! info button): everything the projection, the roster and the
//! `mail-threads details` action hold about one session, on the provenance
//! Popup. A field no source holds prints nothing - the provenance card's
//! rule - and the label-and-value row builder is feed_detail's, shared, so
//! the two modals cannot drift.

use super::*;
use crate::popup::{Anchor, Popup, PopupRow};
use serde_json::Value;

/// The open modal: its popup carries the rows; keys fold in the caller.
/// `key` is the participant the modal opened on: a token answer names its
/// key, and only a modal still open on that row paints it. `tokens_pending`
/// clears when an answer (or a miss) lands, so a duplicate is never painted
/// twice.
pub(crate) struct SessionDetail {
    pub(crate) popup: Popup,
    pub(crate) key: String,
    pub(crate) tokens_pending: bool,
}

/// Up/down move nothing yet (no entry rows); Esc or q closes (true).
pub(crate) fn detail_keys(_detail: &mut SessionDetail, bytes: &[u8]) -> bool {
    bytes == [27] || bytes == [b'q']
}

/// One bound over the `mail-threads details` subprocess: the modal opens
/// without tokens, and they arrive (or miss) inside this budget, never
/// later. The old path ran this subprocess inline on the UI thread with an
/// unbounded wait - a loaded machine froze the client on the `d` press.
const DETAILS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Open the modal on one participant key: projection row first, the
/// roster beside it, tokens and cost fetched OFF the UI loop - the modal
/// opens now, and [`apply_tokens`] paints the token rows when the bounded
/// subprocess answers.
pub(crate) fn open(view: &mut View, key: String) {
    let proj_row = view
        .messages_board
        .as_ref()
        .and_then(|b| b.snapshot.projection.as_ref())
        .and_then(|p| p.get("participants"))
        .and_then(Value::as_array)
        .and_then(|ps| {
            ps.iter()
                .find(|p| p.get("key").and_then(Value::as_str) == Some(key.as_str()))
        })
        .cloned();
    let roster = view
        .layout
        .agents
        .iter()
        .find(|a| a.harness_session_id.as_deref() == Some(key.as_str()) || a.name == key)
        .cloned();
    let sid = roster
        .as_ref()
        .and_then(|a| a.harness_session_id.clone())
        .unwrap_or_else(|| key.clone());
    let mut rows: Vec<PopupRow> = Vec::new();
    rows.push(PopupRow::Header(format!("session {key}")));
    rows.push(PopupRow::Rule);
    if let Some(p) = &proj_row {
        let g = |k: &str| -> Option<String> {
            p.get(k)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        super::feed_detail::info_row("fno id", g("fno_id"), &mut rows);
        super::feed_detail::info_row("session id", g("session_id"), &mut rows);
        super::feed_detail::info_row(
            "short id",
            g("session_id").and_then(|id| id.get(..8).map(str::to_string)),
            &mut rows,
        );
        super::feed_detail::info_row("harness", g("harness"), &mut rows);
        super::feed_detail::info_row("effort", g("effort"), &mut rows);
        super::feed_detail::info_row("node", g("node"), &mut rows);
        super::feed_detail::info_row("created", g("created_at"), &mut rows);
        super::feed_detail::info_row("ended", g("exited_at"), &mut rows);
    }
    if let Some(a) = &roster {
        super::feed_detail::info_row("model", a.model.clone(), &mut rows);
        super::feed_detail::info_row("pr", a.pr.map(|n| n.to_string()), &mut rows);
    }
    let popup = Popup::new(rows, Anchor::Center)
        .title("session details")
        .footer("esc close")
        .plain_body();
    if let Some(b) = view.messages_board.as_mut() {
        b.detail = Some(SessionDetail {
            popup,
            key: key.clone(),
            tokens_pending: view.details_tx.is_some(),
        });
    }
    if let Some(tx) = &view.details_tx {
        let tx = tx.clone();
        tokio::spawn(async move {
            let tokens = fetch_tokens(sid).await;
            let _ = tx.send((key, tokens));
        });
    }
}

/// The `mail-threads details` action over one session id, bounded and off
/// the UI thread: tokens and cost, or None when the binary, the registry
/// row, the transcript or the budget is gone (a modal never blocks on a
/// failed read, and now it never blocks at all).
async fn fetch_tokens(sid: String) -> Option<Value> {
    let out = tokio::time::timeout(
        DETAILS_TIMEOUT,
        tokio::process::Command::new(crate::digest_overlay::fno_agents_bin())
            .args(["mail-threads", "details", "--session", &sid])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Paint one token answer onto the open modal: the rows the synchronous
/// path used to build inline, appended after the provenance rows.
pub(crate) fn apply_tokens(popup: &mut Popup, tokens: &Value) {
    let get = |k: &str| {
        tokens
            .get("tokens")
            .and_then(|v| v.get(k))
            .and_then(Value::as_u64)
    };
    super::feed_detail::info_row(
        "fresh input",
        get("input").map(|n| n.to_string()),
        &mut popup.rows,
    );
    super::feed_detail::info_row(
        "output",
        get("output").map(|n| n.to_string()),
        &mut popup.rows,
    );
    super::feed_detail::info_row(
        "cache read",
        get("cache_read").map(|n| n.to_string()),
        &mut popup.rows,
    );
    super::feed_detail::info_row(
        "cache write",
        get("cache_write").map(|n| n.to_string()),
        &mut popup.rows,
    );
    let cost = tokens.get("cost_usd").and_then(Value::as_f64);
    super::feed_detail::info_row(
        "api cost",
        cost.map(|c| format!("${c:.4}")),
        &mut popup.rows,
    );
}
