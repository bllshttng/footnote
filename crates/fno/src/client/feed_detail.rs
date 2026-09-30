//! The provenance modal for one activity row: everything known about the
//! event, and how it is known, on ONE Popup.
//!
//! The popup system owns selection, hit-testing and the esc chip, so the
//! modal's action fields - node, session, pane, PR - are rows that Enter or a
//! click acts on, and the labels read bold from the same plain-body anatomy
//! every other modal wears. An empty field prints nothing: the modal hides
//! what the source lacks rather than filling the row with NOT RECORDED
//! (ruling 2026-09-29). NOT APPLICABLE survives - it is positive evidence,
//! not an absence.
//!
//! [`Destination`] stays the ONE resolution: the session row's action and the
//! pane row's value read it, so a row can never promise a command its action
//! does not send.

use super::*;
use crate::feed_overlay::FeedItem;
use crate::popup::{Anchor, Popup, PopupRow};
use crate::proto::AgentRow;

/// How this event's session can be reached right now, worst evidence last.
pub(crate) enum Destination<'a> {
    /// The receipt says how to bring a removed session back. A removal is a
    /// normal outcome, so this outranks every live lookup: the row is gone on
    /// purpose and the recovery line is the answer.
    Recovery(&'a str),
    /// The exact `harness_session_id` matched a live roster row. This event's
    /// own session, and the only evidence good enough for parent and lead.
    Exact(&'a AgentRow),
    /// Only the row NAME or worktree matched. A name is reusable, so this
    /// reaches the node's CURRENT worker, never necessarily this event's
    /// session - and the view says so rather than implying provenance.
    NameOnly(&'a AgentRow),
    /// A session id with no live row of any kind.
    SessionOnly(&'a str),
    None,
}

/// Resolve [`Destination`] once. Exact identity first, because a reused name
/// is the failure this ordering exists to avoid.
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
/// one, so parent and lead read it and nothing else.
fn exact_row<'a>(dest: &Destination<'a>) -> Option<&'a AgentRow> {
    match dest {
        Destination::Exact(row) => Some(row),
        _ => None,
    }
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

/// The owner line as the modal prints it: a holder the live roster no longer
/// holds says so, rather than naming a dead crown's handle as if it were
/// current (the report's dead `king (...)`). The holder is the parenthesized
/// name the owner assignment writes; a line without one passes through.
fn live_owner(owner: &str, agents: &[AgentRow]) -> String {
    let Some(open) = owner.rfind('(') else {
        return owner.to_string();
    };
    if !owner.ends_with(')') || open + 1 >= owner.len() - 1 {
        return owner.to_string();
    }
    let holder = &owner[open + 1..owner.len() - 1];
    let live = agents.iter().any(|a| a.name == holder && !a.exited);
    if live {
        owner.to_string()
    } else {
        format!("{owner} · gone")
    }
}

/// What one selectable row does when Enter fires or it is clicked.
#[derive(Debug, Clone)]
pub(crate) enum FeedAction {
    /// The session's own deep link, resolved ONCE at modal open (FocusPane,
    /// attach on portal 0, or the no-pane notice).
    Session(ChromeHit),
    /// Open the backlog drill-down on the node.
    Node(String),
    /// Open the PR in the browser.
    Pr(String),
    /// Show the removal's recovery line as a notice.
    Resume(String),
}

/// The open provenance modal, held on the view: the event, its popup, and the
/// per-target action and value lists. `actions[i]`/`values[i]` answer popup
/// target `i` (flat index) - only Entry rows contribute targets, and exactly
/// those rows push here, so the alignment holds by construction.
pub(crate) struct FeedDetailModal {
    pub(crate) popup: Popup,
    pub(crate) actions: Vec<FeedAction>,
    pub(crate) values: Vec<String>,
}

/// Build the modal: one Popup whose inert rows are the event's recorded
/// fields (empty ones hidden) and whose Entry rows are its actions. The
/// roster resolves Destination, parent, lead and the owner's liveness at
/// open; a roster change mid-read shows on reopen.
pub(crate) fn modal(view: &View, item: FeedItem) -> FeedDetailModal {
    let (popup, actions, values) = build(&view.layout.agents, view.layout.active_squad, &item);
    FeedDetailModal {
        popup,
        actions,
        values,
    }
}

/// The modal's parts against one roster reading: the framed popup, the
/// per-target actions, and the per-target copyable values. Free of `View`
/// so the tests can build it from plain rows.
pub(crate) fn build(
    agents: &[AgentRow],
    active_squad: u64,
    item: &FeedItem,
) -> (Popup, Vec<FeedAction>, Vec<String>) {
    let dest = destination(agents, item);
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<FeedAction> = Vec::new();
    let mut values: Vec<String> = Vec::new();

    // The event line: kind as printed, the title beside it.
    rows.push(PopupRow::Header(format!(
        "{} {}",
        super::feed_view::display_kind(&item.kind),
        item.title
    )));
    rows.push(PopupRow::Rule);

    // One inert field row. Absent prints nothing - the ruling that retired
    // NOT RECORDED - so the modal's height says what the source holds.
    let info = |label: &str, value: Option<String>, rows: &mut Vec<PopupRow>| {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            rows.push(PopupRow::Info {
                label: label.to_string(),
                value: v,
            });
        }
    };

    info("harness", item.harness.clone(), &mut rows);
    info("timestamp", Some(local_ts(&item.ts)), &mut rows);
    info("model", item.model.clone(), &mut rows);
    info("effort", item.effort.clone(), &mut rows);

    // node: a jump, not a label.
    if let Some(node) = item.node.as_deref() {
        rows.push(PopupRow::Entry {
            glyph: "node".to_string(),
            label: node.to_string(),
            hint: String::new(),
            enabled: true,
        });
        actions.push(FeedAction::Node(node.to_string()));
        values.push(node.to_string());
    }

    // session-id: the attach target, or plain text when no live reach exists.
    match &dest {
        Destination::Exact(a) | Destination::NameOnly(a) => {
            if let Some(sid) = item.session_id.as_deref() {
                rows.push(PopupRow::Entry {
                    glyph: "session-id".to_string(),
                    label: sid.to_string(),
                    hint: String::new(),
                    enabled: true,
                });
                actions.push(FeedAction::Session(agent_hit(a, active_squad)));
                values.push(sid.to_string());
            }
            // A name join reaches the node's CURRENT worker: the pane says
            // so rather than implying this event's session sits there.
            let seat_v = if matches!(dest, Destination::NameOnly(_)) {
                format!("{} · the node's current worker", seat(a))
            } else {
                seat(a)
            };
            rows.push(PopupRow::Entry {
                glyph: "pane".to_string(),
                label: seat_v.clone(),
                hint: String::new(),
                enabled: true,
            });
            actions.push(FeedAction::Session(agent_hit(a, active_squad)));
            values.push(seat_v);
        }
        Destination::SessionOnly(sid) => {
            rows.push(PopupRow::Entry {
                glyph: "session-id".to_string(),
                label: (*sid).to_string(),
                hint: String::new(),
                enabled: true,
            });
            actions.push(FeedAction::Session(ChromeHit::Cmds(vec![
                Command::AttachAgent {
                    id: (*sid).to_string(),
                    placement: PanePlacement {
                        portal: Some(0),
                        ..PanePlacement::default()
                    },
                },
            ])));
            values.push((*sid).to_string());
            info(
                "pane",
                Some("not in the live roster".to_string()),
                &mut rows,
            );
        }
        Destination::Recovery(_) | Destination::None => {}
    }

    // The ship row's PR: open it.
    if let Some(url) = item.url.as_deref() {
        rows.push(PopupRow::Entry {
            glyph: "pr".to_string(),
            label: url.to_string(),
            hint: String::new(),
            enabled: true,
        });
        actions.push(FeedAction::Pr(url.to_string()));
        values.push(url.to_string());
    }

    // parent: the exact row's named edge when one resolves, else the birth
    // stamp the graph carried (a node_created row's creating session).
    let parent = exact_row(&dest).and_then(|a| {
        a.spawned_by_session
            .as_deref()
            .map(|p| (a.spawned_by_name.as_deref(), a.lineage_kind.as_deref(), p))
            .map(|(name, kind, p)| match (name, kind) {
                (Some(n), Some("peer")) => format!("{n} ({p}) (handoff)"),
                (Some(n), _) => format!("{n} ({p})"),
                (None, _) => p.to_string(),
            })
    });
    let parent = parent.or_else(|| {
        exact_row(&dest)
            .and_then(|a| a.lineage_reason.clone())
            .filter(|r| !r.is_empty())
    });
    info("parent", parent.or_else(|| item.parent.clone()), &mut rows);

    // lead: the exact row's crown, else the row's own stamp.
    let lead = exact_row(&dest).and_then(|a| match a.crown_scope.as_deref() {
        Some(scope) => Some(
            a.crown_title
                .as_deref()
                .map(str::to_string)
                .or_else(|| a.crown_level.map(|l| format!("L{l} {scope}")))
                .unwrap_or_else(|| scope.to_string()),
        ),
        None => a.crown_title.clone(),
    });
    info("lead", lead.or_else(|| item.crown.clone()), &mut rows);
    info("reason", item.reason.clone(), &mut rows);
    info("crown", item.crown.clone(), &mut rows);
    info(
        "owner",
        item.owner.as_deref().map(|o| live_owner(o, agents)),
        &mut rows,
    );
    info("actor", item.actor.clone(), &mut rows);
    info("phase", item.phase.clone(), &mut rows);

    // A removal's answer: the recovery line, as its own row.
    if let Destination::Recovery(detail) = &dest {
        rows.push(PopupRow::Entry {
            glyph: "resume".to_string(),
            label: (*detail).to_string(),
            hint: String::new(),
            enabled: true,
        });
        actions.push(FeedAction::Resume((*detail).to_string()));
        values.push((*detail).to_string());
    }

    let footer = if actions.is_empty() {
        "y copy · esc close"
    } else {
        "enter open · y copy · esc close"
    };
    let popup = Popup::new(rows, Anchor::Center)
        .title("event provenance")
        .footer(footer)
        .plain_body();
    (popup, actions, values)
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

/// Enter on the modal: the selected row's action. Inspecting never closes
/// the modal - the next field is one arrow away - and a target past the
/// action list (never produced) acts as nothing.
pub(crate) async fn execute_selected(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let Some(m) = view.feed_detail.as_ref() else {
        return Ok(());
    };
    let Some(action) = m.actions.get(m.popup.sel) else {
        return Ok(());
    };
    match action {
        FeedAction::Session(hit) => apply_hit(view, hit.clone(), sock_w).await?,
        FeedAction::Node(id) => {
            let id = id.clone();
            // The drill-down lives on the experimental board: opening a node
            // opens the board on it. Off, the notice says where the jump
            // would land rather than pretending nothing exists.
            if !view.experimental_backlog {
                view.set_notice(format!(
                    "node {id}: the backlog view is off (sideline menu)"
                ));
                return Ok(());
            }
            View::open(view);
            if let Some(b) = view.backlog_board.as_mut() {
                b.detail = Some(node_detail::NodeDetailOverlay {
                    node_id: id,
                    trail: Vec::new(),
                    sel: 0,
                    scroll: 0,
                });
            }
            view.feed_detail = None;
        }
        FeedAction::Pr(url) => {
            let url = url.clone();
            // Off-loop for the same reason ServerMsg::OpenLink is: a cold
            // browser launch must not stall the render loop.
            let launched = url.clone();
            let outcome = tokio::task::spawn_blocking(move || crate::link::open_url(&launched))
                .await
                .unwrap_or_else(|_| Err("opener task failed".to_string()));
            match outcome {
                Ok(()) => view.set_notice(format!("opened {url}")),
                Err(e) => view.set_notice(format!("open failed: {e}")),
            }
        }
        FeedAction::Resume(detail) => view.set_notice(detail.clone()),
    }
    Ok(())
}

/// `y` on the modal: the selected value, whole, to the clipboard - local
/// tool first, OSC 52 to the outer terminal as fallback. The display clips
/// a long value; the copy never does.
pub(crate) fn copy_selected(view: &mut View) {
    let Some(m) = view.feed_detail.as_ref() else {
        return;
    };
    let Some(value) = m.values.get(m.popup.sel) else {
        return;
    };
    let value = value.clone();
    let outcome = crate::clipboard::deliver(&value, raw_out);
    let note = match outcome {
        crate::clipboard::CopyOutcome::Local(_) => format!("copied {value}"),
        crate::clipboard::CopyOutcome::Osc52 { truncated: false } => {
            format!("copied {value}")
        }
        crate::clipboard::CopyOutcome::Osc52 { truncated: true } => {
            "copied (truncated)".to_string()
        }
        crate::clipboard::CopyOutcome::Failed => "copy failed".to_string(),
    };
    view.set_notice(note);
}

/// The popup's flat target under a screen cell, `None` off a target. The
/// footer's close words are not a row target: returning them here would
/// clamp `select` onto the last entry on a hover sweep.
pub(crate) fn hit_at(view: &View, row: u16, col: u16) -> Option<usize> {
    let m = view.feed_detail.as_ref()?;
    let r = m.popup.render(view.term);
    let (r0, c0) = r.origin;
    let line = r.lines.get(row.checked_sub(r0 as u16)? as usize)?;
    let cc = (col as usize).checked_sub(c0)?;
    line.hits
        .iter()
        .find(|(t, off, len)| *t != crate::chrome::ESC_CLOSE_HIT && cc >= *off && cc < *off + *len)
        .map(|(t, _, _)| *t)
}

/// Whether a screen cell falls inside the modal's block: a click ON it that
/// hits no target is swallowed, a click OFF it dismisses.
pub(crate) fn block_contains(view: &View, row: u16, col: u16) -> bool {
    view.feed_detail
        .as_ref()
        .is_some_and(|m| m.popup.render(view.term).contains(row, col))
}
