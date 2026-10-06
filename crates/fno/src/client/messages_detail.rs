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
pub(crate) struct SessionDetail {
    pub(crate) popup: Popup,
}

/// Up/down move nothing yet (no entry rows); Esc or q closes (true).
pub(crate) fn detail_keys(_detail: &mut SessionDetail, bytes: &[u8]) -> bool {
    bytes == [27] || bytes == [b'q']
}

/// Open the modal on one participant key: projection row first, the
/// roster beside it, tokens and cost from the details action.
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
    let tokens = details_action(
        &key,
        roster.as_ref().and_then(|a| a.harness_session_id.clone()),
    );
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
    if let Some(t) = &tokens {
        let get = |k: &str| {
            t.get("tokens")
                .and_then(|v| v.get(k))
                .and_then(Value::as_u64)
        };
        super::feed_detail::info_row(
            "fresh input",
            get("input").map(|n| n.to_string()),
            &mut rows,
        );
        super::feed_detail::info_row("output", get("output").map(|n| n.to_string()), &mut rows);
        super::feed_detail::info_row(
            "cache read",
            get("cache_read").map(|n| n.to_string()),
            &mut rows,
        );
        super::feed_detail::info_row(
            "cache write",
            get("cache_write").map(|n| n.to_string()),
            &mut rows,
        );
        let cost = t.get("cost_usd").and_then(Value::as_f64);
        super::feed_detail::info_row("api cost", cost.map(|c| format!("${c:.4}")), &mut rows);
    }
    let popup = Popup::new(rows, Anchor::Center)
        .title("session details")
        .footer("esc close")
        .plain_body();
    if let Some(b) = view.messages_board.as_mut() {
        b.detail = Some(SessionDetail { popup });
    }
}

/// The `mail-threads details` action over one session id: tokens and
/// cost, or None when the binary, the registry row or the transcript is
/// missing (a modal never blocks on a failed read).
fn details_action(key: &str, sid: Option<String>) -> Option<Value> {
    let sid = sid.unwrap_or_else(|| key.to_string());
    let out = std::process::Command::new("fno-agents")
        .args(["mail-threads", "details", "--session", &sid])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}
