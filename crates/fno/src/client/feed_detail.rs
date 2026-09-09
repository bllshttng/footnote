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
//! a LIVE lookup, and [`Destination`] is the ONE resolution the pane field, the
//! footer and the action all read. One resolution is the point: a footer that
//! named its action from one join while Enter fired another sent a command the
//! footer never promised.

use super::*;
use crate::feed_overlay::FeedItem;
use crate::proto::AgentRow;

pub(crate) const NOT_RECORDED: &str = "NOT RECORDED";
pub(crate) const NOT_APPLICABLE: &str = "NOT APPLICABLE";

/// How this event's session can be reached right now, worst evidence last.
pub(crate) enum Destination<'a> {
    /// The receipt says how to bring a removed session back. A removal is a
    /// normal outcome, so this outranks every live lookup: the row is gone on
    /// purpose and the recovery line is the answer.
    Recovery(&'a str),
    /// The exact `harness_session_id` matched a live roster row. This event's
    /// own session, and the only evidence good enough for parent and king.
    Exact(&'a AgentRow),
    /// Only the row NAME or worktree matched. A name is reusable, so this
    /// reaches the node's CURRENT worker, never necessarily this event's
    /// session - and the view says so rather than implying provenance.
    NameOnly(&'a AgentRow),
    /// A session id with no live row of any kind.
    SessionOnly(&'a str),
    None,
}

/// Resolve [`Destination`] once. Exact identity first, because a reused name is the
/// failure this ordering exists to avoid.
pub(crate) fn destination<'a>(rows: &'a [AgentRow], item: &'a FeedItem) -> Destination<'a> {
    if let Some(detail) = item.detail.as_deref() {
        return Destination::Recovery(detail);
    }
    if let Some(sid) = item.session_id.as_deref() {
        if let Some(row) = rows
            .iter()
            .find(|a| a.harness_session_id.as_deref() == Some(sid))
        {
            return Destination::Exact(row);
        }
    }
    let keys: Vec<&str> = [item.node.as_deref(), item.session_id.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    if let Some(row) = rows.iter().find(|a| {
        keys.iter().any(|k| a.name == *k)
            || a.cwd_base.as_deref().is_some_and(|c| keys.contains(&c))
    }) {
        return Destination::NameOnly(row);
    }
    match item.session_id.as_deref() {
        Some(sid) => Destination::SessionOnly(sid),
        None => Destination::None,
    }
}

/// The roster row that is provably THIS event's session. A name match is not
/// one, so parent and king read it and nothing else.
fn exact_row<'a>(dest: &Destination<'a>) -> Option<&'a AgentRow> {
    match dest {
        Destination::Exact(row) => Some(row),
        _ => None,
    }
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

fn seat(a: &AgentRow) -> String {
    match (a.pane_id, a.portal) {
        // Pane ids allocate from zero, so pane 0 is a real seat: compare the
        // Option, never its truthiness.
        (Some(pid), Some(portal)) => format!("pane {pid} · portal {portal}"),
        (Some(pid), None) => format!("pane {pid}"),
        (None, _) => "no seat · retarget portal 0".to_string(),
    }
}

/// What the pane field can honestly say. Most of these are good outcomes
/// rather than errors.
fn pane_value(item: &FeedItem, dest: &Destination<'_>) -> String {
    match dest {
        // A removal is a normal end, not a failure. The recovery line is the
        // footer's job; this field only says the seat is gone on purpose.
        Destination::Recovery(_) => format!("{NOT_APPLICABLE} - the session was removed"),
        Destination::Exact(a) => seat(a),
        Destination::NameOnly(a) => format!("{} · the node's current worker", seat(a)),
        Destination::SessionOnly(_) => "not in the live roster".to_string(),
        Destination::None if is_graph_derived(&item.kind) => {
            format!("{NOT_APPLICABLE} - a graph field, not a session")
        }
        Destination::None => NOT_RECORDED.to_string(),
    }
}

/// The nine fields, in the operator's order, as `(label, value)`.
pub(crate) fn detail_fields(
    item: &FeedItem,
    dest: &Destination<'_>,
) -> Vec<(&'static str, String)> {
    let row = exact_row(dest);
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
        ("pane", pane_value(item, dest)),
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
/// when the row carries one. The overlay clips a line that outruns its width,
/// so a long id can read short here; the panel row behind it carries the same
/// id, and `--json` from the verb carries it whole.
pub(crate) fn detail_lines(item: &FeedItem, dest: &Destination<'_>) -> Vec<String> {
    let mut lines = vec![format!("{}  {}", item.kind, item.title), String::new()];
    for (label, value) in detail_fields(item, dest) {
        lines.push(format!("{label:<12} {value}"));
    }
    if let Some(actor) = item.actor.as_deref() {
        lines.push(format!("{:<12} {}", "actor", actor));
    }
    if let Some(phase) = item.phase.as_deref() {
        lines.push(format!("{:<12} {}", "phase", phase));
    }
    if let Destination::Recovery(detail) = dest {
        lines.push(String::new());
        lines.push((*detail).to_string());
    }
    lines
}

/// The one-line footer: what pressing Enter does, named before it is pressed.
/// Reads the SAME [`Destination`] the action does, so the two cannot disagree.
pub(crate) fn detail_footer(dest: &Destination<'_>) -> String {
    match dest {
        Destination::Recovery(_) => "enter: show the resume line · esc close",
        Destination::Exact(a) | Destination::NameOnly(a) if a.pane_id.is_some() => {
            "enter: focus its pane · esc close"
        }
        Destination::Exact(_) | Destination::NameOnly(_) => {
            "enter: reach it on portal 0 · esc close"
        }
        Destination::SessionOnly(_) => "enter: attach on portal 0 · esc close",
        Destination::None => "esc close",
    }
    .to_string()
}

/// The action Enter sends, from that same resolution.
pub(crate) fn detail_hit(view: &View, dest: &Destination<'_>) -> Option<ChromeHit> {
    match dest {
        Destination::Recovery(detail) => Some(ChromeHit::Notice((*detail).to_string())),
        Destination::Exact(row) | Destination::NameOnly(row) => {
            Some(agent_hit(row, view.layout.active_squad))
        }
        Destination::SessionOnly(sid) => Some(ChromeHit::Cmds(vec![Command::AttachAgent {
            id: (*sid).to_string(),
            placement: PanePlacement {
                portal: Some(0),
                ..PanePlacement::default()
            },
        }])),
        Destination::None => None,
    }
}

/// Paint the provenance view. Lives here rather than in the compose pass so
/// the render and the fields it renders read as one module.
pub(crate) fn draw(
    view: &View,
    item: &FeedItem,
    cells: &mut [Cell],
    (rows, cols): (usize, usize),
    origin: (usize, usize),
    dims: (usize, usize),
) {
    let dest = destination(&view.layout.agents, item);
    let lines = detail_lines(item, &dest);
    let chrome =
        chrome::Chrome::new("event provenance", Anchor::Center).footer(detail_footer(&dest));
    draw_lines_overlay(
        cells,
        rows,
        cols,
        origin,
        dims,
        &chrome,
        &lines,
        &view.theme,
        None,
    );
}
