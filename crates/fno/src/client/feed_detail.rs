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
    let Some(holder) = owner_holder(owner) else {
        return owner.to_string();
    };
    let live = agents.iter().any(|a| a.name == holder && !a.exited);
    if live {
        owner.to_string()
    } else {
        format!("{owner} · gone")
    }
}

/// The parenthesized holder an owner string carries, when it has one.
fn owner_holder(owner: &str) -> Option<&str> {
    let open = owner.rfind('(')?;
    if !owner.ends_with(')') || open + 1 >= owner.len() - 1 {
        return None;
    }
    Some(&owner[open + 1..owner.len() - 1])
}

/// The hit for a session link on the modal: a live row's own agent_hit
/// (focus the pane, else the portal door); an exited row goes through the
/// row-menu resume path - `RespawnAgent`, the door whose per-harness plan
/// runs the claude cascade (adopt, then resume, then respawn) and plain
/// resume for every other harness.
fn session_hit(a: &AgentRow, active_squad: u64) -> ChromeHit {
    if a.exited {
        return ChromeHit::Cmds(vec![Command::RespawnAgent {
            name: a.name.clone(),
        }]);
    }
    agent_hit(a, active_squad)
}

/// What a session-id link lands as on the modal: an action with its
/// copyable value, or an inert row carrying the dim reason.
enum LinkRow {
    Action(FeedAction, String),
    Inert(String),
}

/// Land one shared [`crate::backlog_model::OpenSession`] verdict on the
/// modal: wire commands ride [`FeedAction::Session`], a shell line rides
/// [`FeedAction::Resume`] and copies itself, a dim verdict renders its
/// reason and offers nothing.
fn open_row(open: crate::backlog_model::OpenSession, sid: &str) -> LinkRow {
    match open {
        crate::backlog_model::OpenSession::Cmds(cmds) => {
            LinkRow::Action(FeedAction::Session(ChromeHit::Cmds(cmds)), sid.to_string())
        }
        crate::backlog_model::OpenSession::Shell(line) => {
            let value = line.clone();
            LinkRow::Action(FeedAction::Resume { line, name: None }, value)
        }
        crate::backlog_model::OpenSession::Dim(why) => LinkRow::Inert(why),
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

    // session-id: the shared open-session action (backlog_model::open_session),
    // the step the board and the questions overlay wire next.
    // Live or not: a live row focuses its seat (a portal when paneless), a
    // registry row takes the row-menu resume door, a session the registry
    // lacks takes the adopt line. The roster name rides beside the id when
    // the join held; the copy keeps the raw id.
    if let Some(sid) = item
        .session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .filter(|_| !matches!(dest, Destination::Recovery(_) | Destination::None))
    {
        // The name rides beside the id only on an EXACT join: a name join
        // resolves the node's current worker, and printing that name beside
        // this event's session id would imply the seat and the session are
        // one row.
        let name = match &dest {
            Destination::Exact(a) => (!a.name.is_empty()).then(|| a.name.clone()),
            _ => None,
        };
        let open = crate::backlog_model::open_session(agents, sid, item.cwd.as_deref());
        match open_row(open, sid) {
            LinkRow::Action(action, value) => {
                let label = match name {
                    Some(n) => format!("{n} ({sid})"),
                    None => sid.to_string(),
                };
                rows.push(PopupRow::Entry {
                    glyph: "session-id".to_string(),
                    label,
                    hint: String::new(),
                    enabled: true,
                });
                actions.push(action);
                values.push(value);
            }
            LinkRow::Inert(why) => info("session-id", Some(format!("{sid} · {why}")), &mut rows),
        }
    }
    // The seat: a name join reaches the node's CURRENT worker, and the pane
    // says so rather than implying this event's session sits there.
    match &dest {
        Destination::Exact(a) | Destination::NameOnly(a) => {
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
            actions.push(FeedAction::Session(session_hit(a, active_squad)));
            values.push(seat_v);
        }
        Destination::SessionOnly(_) => info(
            "pane",
            Some("not in the live roster".to_string()),
            &mut rows,
        ),
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
    // stamp the graph carried (a node_created row's creating session). The
    // same shared open-session action: Enter opens the parent session,
    // live or not.
    let parent_sid = exact_row(&dest)
        .and_then(|a| a.spawned_by_session.clone())
        .or_else(|| item.parent.clone())
        .filter(|p| !p.is_empty());
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
    let parent = parent.or_else(|| item.parent.clone());
    let parent_row = parent_sid.as_deref().map(|sid| {
        open_row(
            crate::backlog_model::open_session(agents, sid, item.cwd.as_deref()),
            sid,
        )
    });
    match (parent, parent_row) {
        (Some(text), Some(LinkRow::Action(action, value))) => {
            rows.push(PopupRow::Entry {
                glyph: "parent".to_string(),
                label: text,
                hint: String::new(),
                enabled: true,
            });
            actions.push(action);
            values.push(value);
        }
        (Some(text), Some(LinkRow::Inert(why))) => {
            info("parent", Some(format!("{text} · {why}")), &mut rows)
        }
        (Some(text), None) => info("parent", Some(text), &mut rows),
        (None, _) => {}
    }

    // lead: the exact row's team, else the row's own stamp. The same session
    // link: Enter opens the session that holds the role.
    let lead = exact_row(&dest).and_then(|a| match a.role_scope.as_deref() {
        Some(scope) => Some(
            a.role_title
                .as_deref()
                .map(str::to_string)
                .or_else(|| a.role_level.map(|l| format!("L{l} {scope}")))
                .unwrap_or_else(|| scope.to_string()),
        ),
        None => a.role_title.clone(),
    });
    match (lead.or_else(|| item.role.clone()), exact_row(&dest)) {
        (Some(text), Some(a)) => {
            rows.push(PopupRow::Entry {
                glyph: "lead".to_string(),
                label: text.clone(),
                hint: String::new(),
                enabled: true,
            });
            actions.push(FeedAction::Session(session_hit(a, active_squad)));
            values.push(text);
        }
        (Some(text), None) => info("lead", Some(text), &mut rows),
        (None, _) => {}
    }
    info("reason", item.reason.clone(), &mut rows);
    info("role", item.role.clone(), &mut rows);
    // owner: the lead AT EVENT TIME, focusable to its holder when the roster
    // still holds that person and the name answers for exactly one live row
    // (the fleet's fail-closed name rule: an ambiguous name is no target).
    let owner_text = item.owner.as_deref().map(|o| live_owner(o, agents));
    let owner_action = item
        .owner
        .as_deref()
        .and_then(owner_holder)
        .and_then(|holder| {
            let live: Vec<&AgentRow> = agents
                .iter()
                .filter(|a| a.name == holder && !a.exited)
                .collect();
            match live.as_slice() {
                [a] => Some(FeedAction::Session(session_hit(a, active_squad))),
                _ => None,
            }
        });
    match (owner_text, owner_action) {
        (Some(text), Some(action)) => {
            rows.push(PopupRow::Entry {
                glyph: "owner".to_string(),
                label: text.clone(),
                hint: String::new(),
                enabled: true,
            });
            actions.push(action);
            values.push(text);
        }
        (Some(text), None) => info("owner", Some(text), &mut rows),
        (None, _) => {}
    }
    // now-led-by: the node's CURRENT coverage, the other of the two names.
    // Enter opens the covering lead's session. The narrowest LIVE team
    // naming the node comes from the shared lead_of resolver - an exited
    // row with the scope does not answer for it.
    if let Some(node) = item.node.as_deref() {
        let live: Vec<AgentRow> = agents.iter().filter(|a| !a.exited).cloned().collect();
        let cover = crate::backlog_model::lead_of(&live, node, None, None).and_then(|(name, _)| {
            agents.iter().find(|a| {
                a.name == name
                    && !a.exited
                    && a.role_scope
                        .as_deref()
                        .is_some_and(|s| s.split(',').any(|seg| seg.trim() == node))
            })
        });
        if let Some(a) = cover {
            let title = a.role_title.clone().or_else(|| {
                a.role_level
                    .zip(a.role_scope.clone())
                    .map(|(l, s)| format!("L{l} {s}"))
            });
            let label = match title {
                Some(t) => format!("{} · {t}", a.name),
                None => a.name.clone(),
            };
            rows.push(PopupRow::Entry {
                glyph: "now-led-by".to_string(),
                label: label.clone(),
                hint: String::new(),
                enabled: true,
            });
            actions.push(FeedAction::Session(session_hit(a, active_squad)));
            values.push(label);
        }
    }
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
