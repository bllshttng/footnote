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
/// holds says so, rather than naming a dead team's handle as if it were
/// current (the report's dead `lead (...)`). The holder is the parenthesized
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
    /// A recovery row: the session is not reachable as a live seat. `name`
    /// carries the registry handle when the roster still holds the row
    /// (a removal's worker): Enter then resumes through
    /// `Command::ResumeAgent` (the fno-owned door) instead of only talking.
    /// Without it `line` is the command or revival form to copy, and Enter
    /// repeats that line as a notice.
    Resume { line: String, name: Option<String> },
}

/// The open provenance modal, held on the view: the event, its popup, and the
/// per-target action and value lists. `actions[i]`/`values[i]` answer popup
/// target `i` (flat index) - only Entry rows contribute targets, and exactly
/// those rows push here, so the alignment holds by construction.
pub(crate) struct FeedDetailModal {
    pub(crate) item: FeedItem,
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
        item,
        popup,
        actions,
        values,
    }
}

/// The one label-and-value row builder both modals share: a value the
/// source lacks prints nothing (the ruling that retired NOT RECORDED).
pub(crate) fn info_row(label: &str, value: Option<String>, rows: &mut Vec<PopupRow>) {
    if let Some(v) = value.filter(|v| !v.is_empty()) {
        rows.push(PopupRow::Info {
            label: label.to_string(),
            value: v,
        });
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

    // One inert field row per call below; the builder is shared with the
    // Messages details modal so the two cannot drift.
    let info = super::feed_detail::info_row;
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
            // The feed's session id is a session handle (an fno id or a
            // harness uuid), never the 8-hex jobId the AttachAgent door
            // resolves, so an attach here is a guaranteed refusal. The
            // resume verb takes the full session id directly; Enter repeats
            // the command and `y` copies it.
            let cmd = format!("fno agents resume {sid}");
            actions.push(FeedAction::Resume {
                line: cmd.clone(),
                name: None,
            });
            values.push(cmd);
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
                (None, Some("peer")) => format!("{p} (handoff)"),
                (None, _) => p.to_string(),
            })
    });
    let parent = parent.or_else(|| {
        exact_row(&dest)
            .and_then(|a| a.lineage_reason.clone())
            .filter(|r| !r.is_empty())
    });
    info("parent", parent.or_else(|| item.parent.clone()), &mut rows);

    // lead: the exact row's team, else the row's own stamp.
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

    // A removal's answer: resume the row through fno when the roster still
    // holds it, else the retirement receipt's revival line verbatim. The
    // copied value is the command either way, so `y` never hands over a raw
    // harness argv when the fno handle exists.
    if let Destination::Recovery(detail) = &dest {
        let named = item
            .name
            .as_deref()
            .filter(|n| !n.is_empty())
            .filter(|n| agents.iter().any(|a| a.name == *n));
        let (label, name) = match named {
            Some(n) => {
                info("revival", Some((*detail).to_string()), &mut rows);
                (format!("fno agents resume {n}"), Some(n.to_string()))
            }
            None => ((*detail).to_string(), None),
        };
        rows.push(PopupRow::Entry {
            glyph: "resume".to_string(),
            label: label.clone(),
            hint: String::new(),
            enabled: true,
        });
        actions.push(FeedAction::Resume {
            line: label.clone(),
            name,
        });
        values.push(label);
    }

    // The footer names every gesture the modal answers, including the
    // created-node composer key the line-built view advertised.
    let footer = if plan_node(item).is_some() {
        "enter open · b blueprint · y copy · esc close"
    } else if actions.is_empty() {
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
            let on = view.experimental_backlog;
            node_link::open_detail(view, id);
            if on {
                view.feed_detail = None;
            }
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
        FeedAction::Resume { line, name } => {
            // Own the strings first: they borrow into the modal, and the
            // mutable view work below must not hold that borrow.
            let line = line.clone();
            let name = name.clone();
            match name {
                Some(name) => {
                    // The verdict arrives on the wire either way: "resumed
                    // <name>" or the gate's one-line refusal, as a notice.
                    view.set_notice(format!("resuming {name}"));
                    write_msg(sock_w, &ClientMsg::Command(Command::ResumeAgent { name }))
                        .await
                        .map_err(|e| format!("resume send failed: {e}"))?;
                }
                None => view.set_notice(line),
            }
        }
    }
    Ok(())
}

/// The created node a `b` can blueprint, when this row is a node_created
/// event carrying an id.
pub(crate) fn plan_node(item: &FeedItem) -> Option<&str> {
    if item.kind != "node_created" {
        return None;
    }
    item.node
        .as_deref()
        .map(str::trim)
        .filter(|node| !node.is_empty())
}

/// `y` on the modal: the selected value, whole, to the clipboard - local
/// tool first, OSC 52 to the outer terminal as fallback. The display clips
/// a long value; the copy never does.
/// Deliver one value to the clipboard and say what happened: local tool
/// first, OSC 52 to the outer terminal as fallback. The display clips a
/// long value; the copy never does. Shared by the feed modal and the
/// backlog detail's y/Y.
pub(crate) fn copy_value(view: &mut View, value: String) {
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

pub(crate) fn copy_selected(view: &mut View) {
    let Some(m) = view.feed_detail.as_ref() else {
        return;
    };
    let Some(value) = m.values.get(m.popup.sel) else {
        return;
    };
    let value = value.clone();
    copy_value(view, value);
}

/// The popup's flat target under a screen cell, `None` off a target. The
/// footer's close words are not a row target: returning them here would
/// clamp `select` onto the last entry on a hover sweep.
pub(crate) fn hit_at(view: &View, row: u16, col: u16) -> Option<usize> {
    let m = view.feed_detail.as_ref()?;
    m.popup.render(view.term).row_target_at(row, col)
}

/// Open the modal on one row, resolving the roster once; a later fold
/// replaces the rows, never this open read.
pub(crate) fn open_into(view: &mut View, item: FeedItem) {
    let m = modal(view, item);
    view.feed_detail = Some(m);
}

/// One mouse report while the modal is open: hover selects, a left click on
/// the shared esc close target (footer words or border chip) closes, a click
/// on a target runs that row's action, a click inside the block that hits no
/// target is swallowed, a click off the popup dismisses. The row-menu
/// contract, on the feed's own actions.
pub(crate) async fn mouse(
    view: &mut View,
    rep: crate::mouse::MouseReport,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    match rep.kind {
        MouseKind::Move => {
            if let Some(t) = hit_at(view, rep.row, rep.col) {
                if let Some(m) = view.feed_detail.as_mut() {
                    m.popup.select(t);
                }
            }
        }
        MouseKind::Press(MouseButton::Left) => match hit_at(view, rep.row, rep.col) {
            Some(t) => {
                if let Some(m) = view.feed_detail.as_mut() {
                    m.popup.select(t);
                }
                execute_selected(view, sock_w).await?;
            }
            None => {
                if !block_contains(view, rep.row, rep.col) {
                    view.feed_detail = None;
                }
            }
        },
        _ => {}
    }
    Ok(())
}

/// Whether a screen cell falls inside the modal's block: a click ON it that
/// hits no target is swallowed, a click OFF it dismisses.
pub(crate) fn block_contains(view: &View, row: u16, col: u16) -> bool {
    view.feed_detail
        .as_ref()
        .is_some_and(|m| m.popup.render(view.term).contains(row, col))
}
