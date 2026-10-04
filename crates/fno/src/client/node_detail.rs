//! The node drill-down, held by the backlog board: one node's read from
//! the gathered `Inputs` via [`crate::backlog_model::node`] - no process,
//! the same answer the web board shows. Enter on a card opens it; Esc
//! pops the trail (or closes). The board routes its keys here and draws
//! the overlay when set.
//!
//! Session launches route through the agents section's own hit cascade
//! (`agent_hit`/`apply_hit`), re-resolving the live `AgentRow` at press
//! time so a worker that exited between paint and press answers with its
//! reason, never a stale launch.

use super::backlog_board::{rule, BoardView};
use super::backlog_style::{BLine, BRole, BSeg};
use super::*;
use crate::backlog_model::{session_action, SessionAction};

pub(crate) fn resolve_session(
    view: &View,
    sid: Option<&str>,
    harness: Option<&str>,
    captured: Option<&AgentRow>,
) -> Option<AgentRow> {
    let sid = sid.filter(|s| !s.is_empty()).or_else(|| {
        captured.and_then(|a| a.harness_session_id.as_deref().or(a.attach_id.as_deref()))
    });
    let harness = harness.or_else(|| captured.and_then(|a| a.harness.as_deref()));
    let candidates: Vec<_> =
        view.layout
            .agents
            .iter()
            .filter(|a| {
                if harness.is_some_and(|h| a.harness.as_deref() != Some(h)) {
                    return false;
                }
                match sid {
                    Some(sid) => [a.harness_session_id.as_deref(), a.attach_id.as_deref()]
                        .into_iter()
                        .flatten()
                        .any(|id| id == sid || (sid.len() <= 8 && id.starts_with(sid))),
                    None => captured
                        .is_some_and(|old| old.pane_id.is_some() && old.pane_id == a.pane_id),
                }
            })
            .collect();
    if let Some(old) = captured {
        let seats: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|a| {
                if let Some(pane) = old.pane_id {
                    a.pane_id == Some(pane)
                } else {
                    old.attach_id.is_some() && old.attach_id == a.attach_id
                }
            })
            .collect();
        if let [one] = seats.as_slice() {
            return Some((*one).clone());
        }
    }
    match candidates.as_slice() {
        [one] => Some((*one).clone()),
        _ => None,
    }
}

pub(crate) async fn press_session(
    view: &mut View,
    sid: Option<&str>,
    harness: Option<&str>,
    captured: Option<&AgentRow>,
    sock: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let joined = resolve_session(view, sid, harness, captured);
    match session_action(joined.as_ref()) {
        SessionAction::Attach | SessionAction::Resume => {
            let hit = agent_hit(
                joined.as_ref().expect("action has a row"),
                view.layout.active_squad,
            );
            view.backlog_board = None;
            view.org_board = None;
            view.region_owner = super::region_focus::RegionOwner::Pane;
            super::backlog_board::set_sideline_view(view, crate::view_store::SidelineView::Agents);
            apply_hit(view, hit, sock).await?;
        }
        SessionAction::Dim(why) => view.set_notice(why),
    }
    Ok(())
}
#[path = "backlog_md.rs"]
pub(crate) mod backlog_md;
/// One selectable row of the drill-down: a link to another node, or a
/// session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sel {
    Link(String),
    Session(usize),
}

/// What `y` (and `Y` for the command) copy from the detail: the selected
/// session row's full id or command, a link row's node id, or - nothing
/// selected - the current node id. A link row carries no command; `Y`
/// answers it with the notice instead.
pub(crate) fn copy_target(
    nv: &crate::backlog_model::NodeView,
    node_id: &str,
    sel: usize,
    command: bool,
) -> Option<String> {
    match sel_list(nv).get(sel) {
        Some(Sel::Session(i)) => {
            let s = nv.sessions.get(*i)?;
            if command {
                s.command.clone()
            } else {
                s.session_id.clone()
            }
        }
        Some(Sel::Link(id)) => (!command).then(|| id.clone()),
        None => (!command).then(|| node_id.to_string()),
    }
}

/// `y`/`Y` in the detail: copy the selected target, or say why not.
fn copy_sel(view: &mut View, command: bool) {
    let target = view.backlog_board.as_ref().and_then(|b| {
        let d = b.detail.as_ref()?;
        let inputs = b.inputs.as_ref()?;
        crate::backlog_model::node(inputs, &d.node_id)
            .and_then(|nv| copy_target(&nv, &d.node_id, d.sel, command))
    });
    match target {
        Some(value) => feed_detail::copy_value(view, value),
        None => view.set_notice("no command for this row".to_string()),
    }
}

/// The detail pane's open state, held in `BoardView.detail`. `sel`
/// indexes ONE list: link rows first, then session rows. `trail` is the
/// pushed ids behind the current node; an empty trail's Esc returns focus
/// to the board. `scroll` is the pane's document scroll offset.
pub(crate) struct NodeDetailOverlay {
    pub(crate) node_id: String,
    pub(crate) trail: Vec<String>,
    pub(crate) sel: usize,
    pub(crate) scroll: usize,
}

/// The selectable rows in render order: the five link groups, then the
/// session rows. `sel` indexes THIS list.
fn sel_list(view: &crate::backlog_model::NodeView) -> Vec<Sel> {
    let mut v: Vec<Sel> = Vec::new();
    for l in view.parent.iter() {
        v.push(Sel::Link(l.id.clone()));
    }
    for l in view.children.iter() {
        v.push(Sel::Link(l.id.clone()));
    }
    for l in view.contained.iter() {
        v.push(Sel::Link(l.id.clone()));
    }
    for l in view.blocked_by.iter() {
        v.push(Sel::Link(l.id.clone()));
    }
    for l in view.blocks.iter() {
        v.push(Sel::Link(l.id.clone()));
    }
    for l in view.related.iter() {
        v.push(Sel::Link(l.id.clone()));
    }
    for i in 0..view.sessions.len() {
        v.push(Sel::Session(i));
    }
    v
}

/// The drill-down's body: the styled title (id, bold title, the
/// status/priority pill), dim fields, and every section (body, links,
/// sessions, notes) under a bold header with a thin rule. A line wider than
/// `w` wraps onto the next rows.
/// The detail pane's body for an explicit node: the pane shows the
/// cursor card's node when it lacks focus, and marks no link row then
/// (`sel` is `None`).
pub(crate) fn pane_lines(
    b: &BoardView,
    node_id: &str,
    sel_arg: Option<usize>,
    w: usize,
) -> (Vec<BLine>, Option<usize>) {
    let mut lines: Vec<BLine> = Vec::new();
    let Some(inputs) = b.inputs.as_ref() else {
        return (vec![BLine::meta("reading board...")], None);
    };
    let Some(view) = crate::backlog_model::node(inputs, node_id) else {
        return (
            vec![BLine::meta(format!("no node {node_id} in the board read"))],
            None,
        );
    };
    let sels = sel_list(&view);
    let sel = match sel_arg {
        None => usize::MAX,
        Some(_) if sels.is_empty() => usize::MAX,
        Some(s) => s.min(sels.len() - 1),
    };
    let mut k: usize = 0;
    let mut follow: Option<usize> = None;
    // Title: the id (accent), the title (bold), and the status/priority pill.
    let status = view.card.status.clone().unwrap_or_else(|| "none".into());
    let prio = view.card.priority.clone().unwrap_or_else(|| "none".into());
    let title = BLine::of(&[
        BSeg {
            text: view.card.id.clone(),
            role: BRole::Label,
        },
        BSeg {
            text: format!("  {}", view.card.title),
            role: BRole::Head,
        },
        BSeg {
            text: format!("  [{} {}]", status, prio),
            role: BRole::Pill,
        },
    ]);
    lines.push(title);
    // Meta.
    let king = match &view.card.king {
        Some(king) => format!("{} (L{})", king.name, king.level),
        None => "none".into(),
    };
    lines.push(BLine::meta(format!(
        "{} \u{b7} {} \u{b7} {} \u{b7} {} \u{b7} {} \u{b7} king: {}",
        status,
        view.card.project.as_deref().unwrap_or("none"),
        prio,
        view.card.size.as_deref().unwrap_or("none"),
        view.difficulty.as_deref().unwrap_or("none"),
        king
    )));
    lines.push(BLine::plain(String::new()));
    // Field lines: the label dim (the accent-dim slot is the id's, so a
    // field label reads as the meta rank) and the value plain.
    let field = |label: &str, value: String| {
        BLine::of(&[
            BSeg {
                text: format!("{label}:"),
                role: BRole::Meta,
            },
            BSeg {
                text: format!(" {}", value),
                role: BRole::Body,
            },
        ])
    };
    lines.push(field(
        "kind",
        view.kind.clone().unwrap_or_else(|| "none".into()),
    ));
    lines.push(field(
        "column",
        format!(
            "{} \u{b7} rank {}",
            view.card.column,
            view.card
                .rank
                .map(|r| r.to_string())
                .unwrap_or_else(|| "unranked".into())
        ),
    ));
    lines.push(field(
        "plan",
        view.plan_path.clone().unwrap_or_else(|| "none".into()),
    ));
    lines.push(field(
        "cwd",
        view.cwd.clone().unwrap_or_else(|| "none".into()),
    ));
    for f in &view.unavailable {
        lines.push(BLine::meta(format!("{}: {}", f.feature, f.reason)));
    }
    lines.push(BLine::plain(String::new()));

    // Links section: the parent first, then one row per id under each bold
    // heading, selectable.
    let groups: [(&str, &Vec<crate::backlog_model::Link>); 6] = [
        ("parent", &view.parent),
        ("children", &view.children),
        ("contained", &view.contained),
        ("blocked by", &view.blocked_by),
        ("blocks", &view.blocks),
        ("related", &view.related),
    ];
    let mut any_links = false;
    for (name, group) in groups {
        if group.is_empty() {
            continue;
        }
        any_links = true;
        lines.push(BLine::head(format!("{name} ({}):", group.len())));
        lines.push(BLine::meta(rule(w)));
        for l in group.iter() {
            let marker = if k == sel {
                follow = Some(lines.len());
                k += 1;
                ">"
            } else {
                k += 1;
                " "
            };
            let col = l.column.clone().unwrap_or_default();
            let title = l.title.clone().unwrap_or_default();
            lines.push(BLine::of(&[
                BSeg {
                    text: format!("{marker} "),
                    role: BRole::Body,
                },
                BSeg {
                    text: l.id.clone(),
                    role: BRole::Label,
                },
                BSeg {
                    text: format!(" {} {}", col, title),
                    role: BRole::Body,
                },
            ]));
        }
    }
    if !any_links {
        lines.push(BLine::head("links"));
        lines.push(BLine::meta(rule(w)));
        lines.push(BLine::meta("none"));
    }
    if !view.prs.is_empty() {
        let pr: Vec<String> = view
            .prs
            .iter()
            .map(|p| match p.merge_status.as_deref() {
                Some(s) => format!("#{} ({})", p.number, s),
                None => format!("#{}", p.number),
            })
            .collect();
        lines.push(BLine::of(&[
            BSeg {
                text: "prs:".into(),
                role: BRole::Meta,
            },
            BSeg {
                text: format!(" {}", pr.join(", ")),
                role: BRole::Body,
            },
        ]));
    }
    lines.push(BLine::plain(String::new()));

    // Session section: bold header + thin rule, then the phase table.
    lines.push(BLine::head("sessions"));
    lines.push(BLine::meta(rule(w)));
    lines.push(BLine::meta(format!(
        "{:<9} {:<7} {:<9} {}",
        "phase", "harness", "model", "action"
    )));
    for s in view.sessions.iter() {
        let marker = if k == sel {
            follow = Some(lines.len());
            k += 1;
            ">"
        } else {
            k += 1;
            " "
        };
        let action = match s.action.as_str() {
            "none" => s.reason.clone().unwrap_or_else(|| "none".into()),
            other => other.to_string(),
        };
        lines.push(BLine::plain(format!(
            "{marker} {:<8} {:<7} {:<9} {}",
            s.phase.as_deref().unwrap_or("-"),
            s.harness.as_deref().unwrap_or("-"),
            s.model.as_deref().unwrap_or("-"),
            action
        )));
        if let Some(sid) = s.session_id.as_deref() {
            lines.push(BLine::meta(sid.to_string()));
        }
        if let Some(cmd) = s.command.as_deref() {
            lines.push(BLine::meta(format!("$ {cmd}")));
        }
    }
    if view.sessions.is_empty() {
        lines.push(BLine::meta("sessions: none"));
    }
    lines.push(BLine::plain(String::new()));

    // The comment thread: bold header + thin rule, oldest first, replies
    // indented under their heads; the ask state rides as a mark. Note-kind
    // feed rows render in the same thread, each with its writer
    // identity line when one is stamped.
    let thread: Vec<&crate::backlog_model::Note> = view
        .notes
        .iter()
        .filter(|n| {
            matches!(
                n.kind.as_deref(),
                Some("comment")
                    | Some("reply")
                    | Some("progress")
                    | Some("finding")
                    | Some("ruling")
                    | Some("collision")
            )
        })
        .collect();
    let open = thread
        .iter()
        .filter(|n| n.state.as_deref() == Some("open"))
        .count();
    lines.push(BLine::head(format!(
        "comments ({}, {} open) \u{b7} decisions ({})",
        thread.len(),
        open,
        view.decisions.len()
    )));
    lines.push(BLine::meta(rule(w)));
    for note in &thread {
        let mark = match note.state.as_deref() {
            Some("open") => "\u{25cb}",
            Some("accepted") => "\u{25d0}",
            Some("done") => "\u{2713}",
            Some("declined") => "\u{2717}",
            _ => " ",
        };
        let indent = if note.kind.as_deref() == Some("reply") {
            "  "
        } else {
            ""
        };
        let author = note.author.as_deref().unwrap_or("?");
        let age = note_age(&note.ts);
        let mut row = format!("{indent}{mark} {author} \u{b7} {age}  {}", note.text);
        if let Some(refer) = &note.state_ref {
            row.push_str(&format!(" \u{b7} {refer}"));
        }
        lines.push(BLine::plain(row));
        // Who wrote it, shown in the thread and copyable.
        let mut who: Vec<String> = Vec::new();
        if let Some(name) = &note.agent_name {
            who.push(name.clone());
        }
        if let Some(model) = &note.model {
            who.push(model.clone());
        }
        if let Some(session) = &note.source_session_id {
            who.push(format!("session {session}"));
        }
        if let Some(working) = &note.working_node {
            who.push(format!("on {working}"));
        }
        if !who.is_empty() {
            lines.push(BLine::plain(format!(
                "{indent}      \u{b7} {}",
                who.join(" \u{b7} ")
            )));
        }
    }
    lines.push(BLine::plain(String::new()));
    // Document section: the node's markdown plan when readable, else its
    // details text.
    lines.push(BLine::head("document"));
    lines.push(BLine::meta(rule(w)));
    let doc = b.doc.as_ref().filter(|d| d.node_id == node_id);
    match (&view.plan_path, doc) {
        (Some(path), Some(d)) if d.error.is_empty() => {
            lines.extend(backlog_md::md_lines(&d.lines_src, w));
        }
        (Some(path), _) => {
            let reason = doc
                .map(|d| d.error.clone())
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| "still loading".into());
            lines.push(BLine::meta(format!("plan: {path} (unreadable: {reason})")));
            render_details(&view, w, &mut lines);
        }
        (None, _) => render_details(&view, w, &mut lines),
    }
    // One wrap pass: a line wider than the pane continues on the next rows,
    // and the selected link keeps its first row.
    let mut out = Vec::with_capacity(lines.len());
    let mut moved = None;
    for (i, line) in lines.into_iter().enumerate() {
        if follow == Some(i) {
            moved = Some(out.len());
        }
        out.extend(line.wrap(w));
    }
    (out, moved)
}

/// The details text wrapped as plain lines, or `details: none`.
fn render_details(view: &crate::backlog_model::NodeView, w: usize, out: &mut Vec<BLine>) {
    let text = view.details.as_ref().and_then(|d| d.as_str()).unwrap_or("");
    if text.trim().is_empty() {
        out.push(BLine::meta("details: none"));
        return;
    }
    let mut wrapped: Vec<String> = Vec::new();
    for para in text.split('\n') {
        wrap_line(para, w, &mut wrapped);
    }
    out.extend(wrapped.into_iter().map(BLine::plain));
}

/// Word-wrap one paragraph into lines of at most `w` display columns on
/// whitespace. The one wrap rule: a word wider than `w` breaks across lines,
/// and a wide char never straddles a break.
pub(crate) fn wrap_line(para: &str, w: usize, out: &mut Vec<String>) {
    use crate::chrome::{char_cols, str_cols};
    let w = w.max(1);
    if para.is_empty() {
        out.push(String::new());
        return;
    }
    let mut line = String::new();
    let mut used = 0;
    for word in para.split_whitespace() {
        if used > 0 && used + 1 + str_cols(word) > w {
            out.push(std::mem::take(&mut line));
            used = 0;
        }
        if used > 0 {
            line.push(' ');
            used += 1;
        }
        for ch in word.chars() {
            let cw = char_cols(ch);
            if used > 0 && used + cw > w {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(ch);
            used += cw;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
}

/// Relative age for a thread row, from the row's RFC 3339 stamp.
fn note_age(ts: &Option<String>) -> String {
    use chrono::{DateTime, Utc};
    let Some(text) = ts.as_deref() else {
        return "?".into();
    };
    let Ok(then) = DateTime::parse_from_rfc3339(text) else {
        return text.to_string();
    };
    let secs = (Utc::now() - then.with_timezone(&Utc)).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// The detail pane's keys. j/k (and arrows) move the selection, Enter
/// runs the selected row (a link drills in, a session launches through
/// the hit cascade, a dim row answers with its reason), PgUp/PgDn scroll
/// the document, `b` plans, `t` launches the node as a target through the
/// prefilled launcher, and `A` asks the king (the board's own sends).
/// `c` posts a user comment on the node's thread.
pub(crate) async fn detail_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let esc_carry_empty = view
        .backlog_board
        .as_ref()
        .map(|b| b.detail_esc.is_empty())
        .unwrap_or(true);
    if bytes == [0x1b] && esc_carry_empty {
        // The keys overlay opened from the detail closes first; the second
        // Esc pops the detail itself.
        let overlay_open = view
            .backlog_board
            .as_ref()
            .map(|b| b.keys_overlay)
            .unwrap_or(false);
        if overlay_open {
            if let Some(b) = view.backlog_board.as_mut() {
                b.keys_overlay = false;
            }
        } else {
            pop_or_close(view);
        }
        return Ok(StdinFlow::Continue);
    }
    let toks = {
        let b = view.backlog_board.as_mut().expect("board open");
        let mut esc = std::mem::take(&mut b.detail_esc);
        let toks = fold_modal_keys(&mut esc, bytes);
        b.detail_esc = esc;
        toks
    };
    for tok in toks {
        let Some(b) = view.backlog_board.as_mut() else {
            break;
        };
        if b.detail.is_none() {
            break;
        }
        match tok {
            ModalKey::Esc | ModalKey::Byte(b'q') => pop_or_close(view),
            ModalKey::Up | ModalKey::Byte(b'k') => move_sel(view, false),
            ModalKey::Down | ModalKey::Byte(b'j') => move_sel(view, true),
            ModalKey::Enter => activate(view, sock_w).await?,
            ModalKey::PageUp => scroll_detail(view, false),
            ModalKey::PageDown => scroll_detail(view, true),
            ModalKey::Byte(b'b') => backlog_board::dispatch_plan(view, sock_w).await?,
            ModalKey::Byte(b't') => backlog_board::launch_target(view, sock_w).await?,
            ModalKey::Byte(b'A') => backlog_board::ask_the_king(view, sock_w).await?,
            ModalKey::Byte(b'y') => copy_sel(view, false),
            ModalKey::Byte(b'Y') => copy_sel(view, true),
            ModalKey::Byte(b'c') => backlog_board::begin_comment(view),
            ModalKey::Byte(b'?') => {
                if let Some(b) = view.backlog_board.as_mut() {
                    b.keys_overlay = !b.keys_overlay;
                }
            }
            _ => {}
        }
    }
    if let Some(b) = view.backlog_board.as_mut() {
        backlog_board::backlog_panes::sync_doc(b);
    }
    Ok(StdinFlow::Continue)
}

/// Esc: pop the trail, or close the overlay back to the board with the
/// cursor on the card it opened from (it never moved).
fn pop_or_close(view: &mut View) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let Some(o) = b.detail.as_mut() else {
        return;
    };
    match o.trail.pop() {
        Some(prev) => {
            o.node_id = prev;
            o.sel = 0;
        }
        None => b.detail = None,
    }
}

/// Clamp `sel` against the current node's selectable rows.
fn move_sel(view: &mut View, down: bool) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let Some((o, count)) = detail_with_count(b) else {
        return;
    };
    if count == 0 {
        return;
    }
    o.sel = if down {
        (o.sel + 1).min(count - 1)
    } else {
        o.sel.saturating_sub(1)
    };
}

/// PgUp/PgDn: scroll the pane's document, in steps of eight lines.
fn scroll_detail(view: &mut View, down: bool) {
    let Some(b) = view.backlog_board.as_mut() else {
        return;
    };
    let Some(o) = b.detail.as_mut() else {
        return;
    };
    o.scroll = if down {
        o.scroll.saturating_add(8)
    } else {
        o.scroll.saturating_sub(8)
    };
}

/// The overlay with its selectable-row count, resolved fresh against the
/// gathered inputs (the painted cell can lag a gather by one frame).
fn detail_with_count(b: &mut BoardView) -> Option<(&mut NodeDetailOverlay, usize)> {
    let o = b.detail.as_mut()?;
    let inputs = b.inputs.as_ref()?;
    let view = crate::backlog_model::node(inputs, &o.node_id)?;
    Some((o, sel_list(&view).len()))
}

/// Enter on the selected row: a link drills in (an id the read does not
/// hold answers with a notice and keeps the current node up, never
/// fabricated fields); a session row re-resolves its live `AgentRow` and
/// launches through the board's own hit cascade, closing the board; a dim
/// row shows its reason and launches nothing.
async fn activate(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let Some(b) = view.backlog_board.as_mut() else {
        return Ok(());
    };
    let Some(o) = b.detail.as_ref() else {
        return Ok(());
    };
    let Some(inputs) = b.inputs.as_ref() else {
        return Ok(());
    };
    let Some(nv) = crate::backlog_model::node(inputs, &o.node_id) else {
        return Ok(());
    };
    let sels = sel_list(&nv);
    let sel = o.sel.min(sels.len().saturating_sub(1));
    let Some(sel) = sels.get(sel) else {
        return Ok(());
    };
    match sel {
        Sel::Link(id) => {
            let hold = crate::backlog_model::node(inputs, id).map(|_| id.clone());
            if let Some(id) = hold {
                let b = view.backlog_board.as_mut().expect("board open");
                if let Some(o) = b.detail.as_mut() {
                    o.trail.push(o.node_id.clone());
                    o.node_id = id;
                    o.sel = 0;
                }
            } else {
                view.set_notice(format!("no node {id} in the board read"));
            }
        }
        Sel::Session(i) => {
            let Some(s) = nv.sessions.get(*i) else {
                return Ok(());
            };
            press_session(
                view,
                s.session_id.as_deref(),
                s.harness.as_deref(),
                None,
                sock_w,
            )
            .await?;
        }
    }
    Ok(())
}
