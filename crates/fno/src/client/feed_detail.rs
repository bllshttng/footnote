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
//!   row that records no session was never run by one, so it has no model.
//!   The ROW decides that, never its kind: a `node_ended` on a node that ran
//!   carries the last do or ship session, so its blank lane is NOT RECORDED.
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

/// True when the ROW itself records no session, so its session-shaped fields
/// are inapplicable rather than missing. The kind is the wrong test: a
/// `node_ended` on a node that ran carries the last do/ship session, and
/// calling that "no session" contradicts the session the same view attaches.
fn no_session(item: &FeedItem) -> bool {
    item.session_id.is_none() && item.harness.is_none()
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
        // A removal is a normal end, not a failure - but NOT APPLICABLE is
        // positive evidence the pane concept cannot apply, and a removed
        // session's pane can outlive its row for a day. No removal
        // record carries the pane's stop measurement on a field of its own
        // yet; when one does, that field - not the recovery line - prints
        // here. Until then the honest answer is not recorded.
        Destination::Recovery(_) => {
            format!("{NOT_RECORDED} - the removal did not measure the pane")
        }
        Destination::Exact(a) => seat(a),
        Destination::NameOnly(a) => format!("{} · the node's current worker", seat(a)),
        Destination::SessionOnly(_) => "not in the live roster".to_string(),
        Destination::None if no_session(item) => {
            format!("{NOT_APPLICABLE} - no session ran it")
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
    let unrun = no_session(item);
    let session_absent = || {
        if unrun {
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
        ("timestamp", local_ts(&item.ts)),
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
            match row.and_then(|a| {
                a.spawned_by_session
                    .as_deref()
                    .map(|p| (a.spawned_by_name.as_deref(), a.lineage_kind.as_deref(), p))
            }) {
                // An edge resolves to the parent's registry NAME when the set
                // holds that row: "<name> (<session>)". A PEER stays the
                // handoff it is; a pre-v32 edge (no word yet) reads plain,
                // like the child record it is. An edge naming a session no
                // row holds renders the bare id - the absence is a fact the
                // reader sees, never a fabricated name.
                Some((name, Some("peer"), p)) => match name {
                    Some(n) => format!("{n} ({p}) (handoff)"),
                    None => format!("{p} (handoff)"),
                },
                Some((name, _, p)) => match name {
                    Some(n) => format!("{n} ({p})"),
                    None => p.to_string(),
                },
                // No exact row: the birth event's parent id when the row
                // carries one (a removed or spawn row whose session is
                // gone); else the birth's reason, when one was recorded.
                None => match item.parent.as_deref() {
                    Some(p) => p.to_string(),
                    None => row
                        .and_then(|a| a.lineage_reason.as_deref())
                        .unwrap_or(NOT_RECORDED)
                        .to_string(),
                },
            },
        ),
        (
            "lead",
            match row.and_then(|a| a.crown_scope.as_deref()) {
                Some(scope) => match row.and_then(|a| a.crown_title.as_deref()) {
                    Some(title) => title.to_string(),
                    None => match row.and_then(|a| a.crown_level) {
                        Some(level) => format!("L{level} {scope}"),
                        None => scope.to_string(),
                    },
                },
                // A worker row rolling up to a titled lead carries the title.
                None => row
                    .and_then(|a| a.crown_title.as_deref())
                    .unwrap_or(NOT_RECORDED)
                    .to_string(),
            },
        ),
        ("reason", or_not_recorded(item.reason.as_deref())),
        ("crown", or_not_recorded(item.crown.as_deref())),
        ("owner", or_not_recorded(item.owner.as_deref())),
    ]
}

/// `YYYY-MM-DD HH:MM:SS +HH:MM` in the operator's zone; an unparseable
/// stamp shows raw.
fn local_ts(ts: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => chrono::TimeZone::from_utc_datetime(&chrono::Local, &t.naive_utc())
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string(),
        Err(_) => ts.to_string(),
    }
}

/// The rendered body: the event line, the nine fields, then the recovery line
/// when the row carries one. The overlay clips a line that outruns its width,
/// so a long id can read short here; the panel row behind it carries the same
/// id, and `--json` from the verb carries it whole.
pub(crate) fn detail_lines(item: &FeedItem, dest: &Destination<'_>, width: usize) -> Vec<String> {
    let mut lines = vec![format!("{}  {}", item.kind, item.title), String::new()];
    for (label, value) in detail_fields(item, dest) {
        push_wrapped(&mut lines, label, &value, width);
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

/// One field, wrapped: a value wider than the space after its 12-column
/// label continues on the next line indented 13 columns, so the modal grows
/// in height instead of clipping.
fn push_wrapped(lines: &mut Vec<String>, label: &str, value: &str, width: usize) {
    let budget = width.saturating_sub(13).max(8);
    let mut first = true;
    let mut chunk = String::new();
    let mut chunk_w = 0usize;
    for ch in value.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch)
            .unwrap_or(0)
            .max(1);
        if chunk_w + cw > budget && !chunk.is_empty() {
            if first {
                lines.push(format!("{label:<12} {chunk}"));
                first = false;
            } else {
                lines.push(format!("{}{chunk}", " ".repeat(13)));
            }
            chunk.clear();
            chunk_w = 0;
        }
        chunk.push(ch);
        chunk_w += cw;
    }
    if first {
        lines.push(format!("{label:<12} {chunk}"));
    } else if !chunk.is_empty() {
        lines.push(format!("{}{chunk}", " ".repeat(13)));
    }
}

/// The one-line footer: what pressing Enter does, named before it is pressed.
/// Reads the SAME [`Destination`] the action does, so the two cannot disagree.
pub(crate) fn plan_node(item: &FeedItem) -> Option<&str> {
    if item.kind != "node_created" {
        return None;
    }
    item.node
        .as_deref()
        .map(str::trim)
        .filter(|node| !node.is_empty())
}

/// The one-line footer names the existing Enter action and, for a created
/// node with an id, the composer's node-bound blueprint action.
pub(crate) fn detail_footer(item: &FeedItem, dest: &Destination<'_>) -> String {
    let enter = match dest {
        Destination::Recovery(_) => "enter: show the resume line · esc close",
        Destination::Exact(a) | Destination::NameOnly(a) if a.pane_id.is_some() => {
            "enter: focus its pane · esc close"
        }
        Destination::Exact(_) | Destination::NameOnly(_) => {
            "enter: reach it on portal 0 · esc close"
        }
        Destination::SessionOnly(_) => "enter: attach on portal 0 · esc close",
        Destination::None => "esc close",
    };
    if plan_node(item).is_some() {
        format!("b: blueprint · {enter}")
    } else {
        enter.to_string()
    }
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
    // The chrome's frame plus its two side pad cells.
    let inner_w = dims.1.saturating_sub(chrome::Chrome::FRAME_COLS + 2);
    let lines = detail_lines(item, &dest, inner_w);
    let chrome =
        chrome::Chrome::new("event provenance", Anchor::Center).footer(detail_footer(item, &dest));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn reaped_item() -> FeedItem {
        FeedItem {
            ts: "2026-09-28T16:48:49Z".into(),
            kind: "session_reaped".into(),
            node: None,
            cwd: None,
            session_id: None,
            harness: Some("codex".into()),
            title: "jolly-finch removed".into(),
            r#ref: None,
            actor: Some("fno-py".into()),
            model: None,
            effort: None,
            phase: None,
            detail: None,
            reason: Some(
                "no unique codex rollout for this cwd after spawn, and here is a very long tail that cannot fit forty columns at all".into(),
            ),
            crown: Some("L2 x-eeee".into()),
            owner: Some("king jolly-finch L2".into()),
            parent: None,
        }
    }

    // (AC11-HP) The three new fields render, and every line fits the width:
    // a long value wraps under its label instead of clipping.
    #[test]
    fn the_removal_fields_render_and_wrap_at_width() {
        let item = reaped_item();
        let d = destination(&[], &item);
        let lines = detail_lines(&item, &d, 40);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("reason") && l.contains("no unique codex rollout")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("crown") && l.contains("L2 x-eeee")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("owner") && l.contains("king jolly-finch L2")),
            "{lines:?}"
        );
        // Continuation lines are indented 13 columns; the value is not lost.
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("             ") && l.contains("long tail")),
            "the wrapped tail continues indented: {lines:?}"
        );
        // Every line fits 40 display columns.
        for l in &lines {
            assert!(
                unicode_width::UnicodeWidthStr::width(l.as_str()) <= 40,
                "line overflowed: {l:?}"
            );
        }
    }

    // (AC12-EDGE) Absent fields read NOT RECORDED, and a spawn row's parent
    // survives the session's death through the row's own parent field.
    #[test]
    fn absent_fields_read_not_recorded_and_parent_falls_back() {
        let mut item = reaped_item();
        item.reason = None;
        item.crown = None;
        item.owner = None;
        item.parent = Some("49a80492-388e-44a3-bd91-017be26bcaa0".into());
        let d = destination(&[], &item);
        let fields = detail_fields(&item, &d);
        let by = |label: &str| {
            fields
                .iter()
                .find(|(l, _)| *l == label)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(by("reason"), NOT_RECORDED);
        assert_eq!(by("crown"), NOT_RECORDED);
        assert_eq!(by("owner"), NOT_RECORDED);
        assert_eq!(
            by("parent"),
            "49a80492-388e-44a3-bd91-017be26bcaa0",
            "the birth event's parent survives the removal"
        );
    }
}
