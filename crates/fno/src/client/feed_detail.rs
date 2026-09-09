//! The provenance view for one activity row: everything known about the
//! event, and how it is known.
//!
//! The operator asked for nine fields in one order - harness, timestamp,
//! model, effort, node, session-id, pane, parent, monitoring king - and three
//! of them are measured to be mostly or entirely unrecorded. So the render's
//! job is not to fill nine cells. It is to say which of three different
//! silences each empty one is:
//!
//! - `NOT RECORDED` - the source lacks the fact. Nothing to read.
//! - `NOT APPLICABLE` - positive evidence the concept does not apply here; a
//!   graph-derived row was never run by a session, so it has no model.
//! - a named live state - `not in the live roster`, `no seat`, `removed`.
//!
//! A blank cell reads as broken UI when the real defect is upstream, which is
//! the whole reason this view exists rather than a wider panel column.
//!
//! The first six fields come from the event itself. Pane, parent and king are
//! a LIVE lookup against the roster the client already holds, joined on the
//! exact `harness_session_id` - never on the row NAME, which a later worker
//! can reuse and which would answer about a different session.

use crate::feed_overlay::FeedItem;
use crate::proto::AgentRow;

pub(crate) const NOT_RECORDED: &str = "NOT RECORDED";
pub(crate) const NOT_APPLICABLE: &str = "NOT APPLICABLE";

/// The roster row this event's session is, right now. Exact identity only:
/// `harness_session_id` is the join key the registry and the feed share.
pub(crate) fn live_row<'a>(rows: &'a [AgentRow], item: &FeedItem) -> Option<&'a AgentRow> {
    let sid = item.session_id.as_deref()?;
    rows.iter()
        .find(|a| a.harness_session_id.as_deref() == Some(sid))
}

/// True when this row kind is a GRAPH field rather than something a session
/// did: no session ran it, so its session-shaped fields are inapplicable
/// rather than missing.
fn is_graph_derived(kind: &str) -> bool {
    matches!(kind, "node_created" | "node_ended")
}

fn or_not_recorded(v: Option<&str>) -> String {
    v.filter(|s| !s.is_empty())
        .unwrap_or(NOT_RECORDED)
        .to_string()
}

/// What the pane field can honestly say. Five states, and four of them are
/// good outcomes rather than errors.
fn pane_value(item: &FeedItem, row: Option<&AgentRow>) -> String {
    if item.kind == "session_reaped" {
        // A removal is a normal end, not a failure. The recovery line is the
        // footer's job; this field only says the seat is gone on purpose.
        return format!("{NOT_APPLICABLE} - the session was removed");
    }
    match row {
        Some(a) => match (a.pane_id, a.portal) {
            // Pane ids allocate from zero, so pane 0 is a real seat: compare
            // the Option, never its truthiness.
            (Some(pid), Some(portal)) => format!("pane {pid} · portal {portal}"),
            (Some(pid), None) => format!("pane {pid}"),
            (None, _) => "no seat · retarget portal 0".to_string(),
        },
        None if item.session_id.is_some() => "not in the live roster".to_string(),
        None if is_graph_derived(&item.kind) => {
            format!("{NOT_APPLICABLE} - a graph field, not a session")
        }
        None => NOT_RECORDED.to_string(),
    }
}

/// The nine fields, in the operator's order, as `(label, value)`.
pub(crate) fn detail_fields(
    item: &FeedItem,
    row: Option<&AgentRow>,
) -> Vec<(&'static str, String)> {
    let graph_derived = is_graph_derived(&item.kind);
    let session_absent = || {
        if graph_derived {
            format!("{NOT_APPLICABLE} - no session ran it")
        } else {
            NOT_RECORDED.to_string()
        }
    };
    vec![
        (
            "harness",
            item.harness
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(session_absent),
        ),
        ("timestamp", item.ts.clone()),
        (
            "model",
            item.model
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(session_absent),
        ),
        (
            "effort",
            item.effort
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(session_absent),
        ),
        ("node", or_not_recorded(item.node.as_deref())),
        (
            "session-id",
            item.session_id
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| match item.actor.as_deref() {
                    // The actor is why there is no session: a mechanism acted,
                    // and a mechanism has no session to attach to.
                    Some(actor) => format!("{NOT_APPLICABLE} - acted by {actor}"),
                    None => session_absent(),
                }),
        ),
        ("pane", pane_value(item, row)),
        (
            "parent",
            match row.and_then(|a| a.spawned_by_session.as_deref()) {
                Some(p) => p.to_string(),
                None => NOT_RECORDED.to_string(),
            },
        ),
        (
            "king",
            match row.and_then(|a| a.crown_scope.as_deref()) {
                Some(scope) => match row.and_then(|a| a.crown_level) {
                    Some(level) => format!("L{level} {scope}"),
                    None => scope.to_string(),
                },
                None => NOT_RECORDED.to_string(),
            },
        ),
    ]
}

/// The rendered body: the event line, the nine fields, then the recovery line
/// when the row carries one. Long ids print whole and the overlay's own width
/// is what clips, so an id is never silently halved into a different id.
pub(crate) fn detail_lines(item: &FeedItem, row: Option<&AgentRow>) -> Vec<String> {
    let mut lines = vec![format!("{}  {}", item.kind, item.title), String::new()];
    for (label, value) in detail_fields(item, row) {
        lines.push(format!("{label:<12} {value}"));
    }
    if let Some(actor) = item.actor.as_deref() {
        lines.push(format!("{:<12} {}", "actor", actor));
    }
    if let Some(phase) = item.phase.as_deref() {
        lines.push(format!("{:<12} {}", "phase", phase));
    }
    if let Some(detail) = item.detail.as_deref() {
        lines.push(String::new());
        lines.push(detail.to_string());
    }
    lines
}

/// The one-line footer: what pressing Enter does, named before it is pressed.
pub(crate) fn detail_footer(item: &FeedItem, row: Option<&AgentRow>) -> String {
    if item.detail.is_some() {
        return "enter: show the resume line · esc close".to_string();
    }
    match row {
        Some(a) if a.pane_id.is_some() => "enter: focus its pane · esc close".to_string(),
        Some(_) => "enter: retarget portal 0 · esc close".to_string(),
        None if item.session_id.is_some() => "enter: attach on portal 0 · esc close".to_string(),
        None => "esc close".to_string(),
    }
}
