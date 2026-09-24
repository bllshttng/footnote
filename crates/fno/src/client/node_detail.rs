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

use super::backlog_board::{trunc, BoardView};
use super::*;
use crate::backlog_model::{session_action, SessionAction};
/// One selectable row of the drill-down: a link to another node, or a
/// session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sel {
    Link(String),
    Session(usize),
}

/// The overlay's open state, held in `BoardView.detail`. `sel` indexes
/// ONE list: link rows first, then session rows. `trail` is the pushed
/// ids behind the current node; an empty trail's Esc returns to the board.
pub(crate) struct NodeDetailOverlay {
    pub(crate) node_id: String,
    pub(crate) trail: Vec<String>,
    pub(crate) sel: usize,
    pub(crate) details_open: bool,
}

/// The selectable rows in render order: the five link groups, then the
/// session rows. `sel` indexes THIS list.
fn sel_list(view: &crate::backlog_model::NodeView) -> Vec<Sel> {
    let mut v: Vec<Sel> = Vec::new();
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

/// The overlay body: header, meta, column and rank, epic, plan, details
/// (first 12 lines until `d` opens the whole text), the links block, PRs,
/// the session table, and the newest notes. `w` truncates every line.
/// Returns the lines and the selected row for the painter's follow.
pub(crate) fn overlay_lines(b: &BoardView, w: usize) -> (Vec<String>, Option<usize>) {
    let mut lines: Vec<String> = Vec::new();
    let Some(o) = &b.detail else {
        return (lines, None);
    };
    let Some(inputs) = b.inputs.as_ref() else {
        return (vec!["reading board...".into()], None);
    };
    let Some(view) = crate::backlog_model::node(inputs, &o.node_id) else {
        return (
            vec![format!("no node {} in the board read", o.node_id)],
            None,
        );
    };
    let sels = sel_list(&view);
    let sel = if sels.is_empty() {
        usize::MAX
    } else {
        o.sel.min(sels.len() - 1)
    };
    let mut k: usize = 0;
    let mut follow: Option<usize> = None;
    let t = |s: &str| trunc(s, w);

    // Header: the id first (the handle every verb takes), then the title.
    lines.push(t(&format!("{}  {}", view.card.id, view.card.title)));
    // Meta.
    let king = match &view.card.king {
        Some(king) => format!("{} (L{})", king.name, king.level),
        None => "none".into(),
    };
    lines.push(t(&format!(
        "{} · {} · {} · {} · {} · king: {}",
        view.card.status.as_deref().unwrap_or("none"),
        view.card.project.as_deref().unwrap_or("none"),
        view.card.priority.as_deref().unwrap_or("none"),
        view.card.size.as_deref().unwrap_or("none"),
        view.difficulty.as_deref().unwrap_or("none"),
        king
    )));
    lines.push(t(&format!(
        "kind: {}",
        view.kind.as_deref().unwrap_or("none")
    )));
    lines.push(t(&format!(
        "column: {} · rank: {}",
        view.card.column,
        view.card
            .rank
            .map(|r| r.to_string())
            .unwrap_or_else(|| "unranked".into())
    )));
    lines.push(t(&format!(
        "plan: {}",
        view.plan_path.as_deref().unwrap_or("none")
    )));
    lines.push(t(&format!(
        "cwd: {}",
        view.cwd.as_deref().unwrap_or("none")
    )));
    for f in &view.unavailable {
        lines.push(t(&format!("{}: {}", f.feature, f.reason)));
    }
    lines.push(String::new());

    // Details: word-wrapped to the width; the first 12 lines until `d`
    // opens the whole text.
    if let Some(details) = view.details.as_ref().and_then(|d| d.as_str()) {
        let mut wrapped: Vec<String> = Vec::new();
        for para in details.split('\n') {
            wrap_line(para, w, &mut wrapped);
        }
        if o.details_open || wrapped.len() <= 12 {
            wrapped.truncate(wrapped.len());
            for l in &wrapped {
                lines.push(t(l));
            }
        } else {
            let shown = wrapped.iter().take(12);
            for l in shown {
                lines.push(t(l));
            }
            lines.push(t(&format!(
                "\u{2026} {} more lines (d opens)",
                wrapped.len() - 12
            )));
        }
    } else {
        lines.push(t("details: none"));
    }
    lines.push(String::new());

    // Links block: one row per id under each heading, selectable.
    let groups: [(&str, &Vec<crate::backlog_model::Link>); 5] = [
        ("children", &view.children),
        ("contained", &view.contained),
        ("blocked by", &view.blocked_by),
        ("blocks", &view.blocks),
        ("related", &view.related),
    ];
    for (name, group) in groups {
        if group.is_empty() {
            continue;
        }
        lines.push(t(&format!("{} ({}):", name, group.len())));
        for l in group.iter() {
            let marker = if k == sel {
                follow = Some(lines.len());
                k += 1;
                ">"
            } else {
                k += 1;
                " "
            };
            let col = l.column.unwrap_or("");
            let title = l.title.as_deref().unwrap_or("");
            lines.push(t(&format!("{marker} {} {} {}", l.id, col, title)));
        }
    }
    if view.children.is_empty()
        && view.contained.is_empty()
        && view.blocked_by.is_empty()
        && view.blocks.is_empty()
        && view.related.is_empty()
    {
        lines.push(t("links: none"));
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
        lines.push(t(&format!("prs: {}", pr.join(", "))));
    }
    lines.push(String::new());

    // Session table: phase harness id model action.
    lines.push(t(&format!(
        "{:<9} {:<7} {:<9} {:<12} {}",
        "phase", "harness", "id", "model", "action"
    )));
    for (i, s) in view.sessions.iter().enumerate() {
        let _ = i;
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
        lines.push(t(&format!(
            "{marker} {:<8} {:<7} {:<9} {:<12} {}",
            s.phase.as_deref().unwrap_or("-"),
            s.harness.as_deref().unwrap_or("-"),
            short_id(s.session_id.as_deref().unwrap_or("-")),
            s.model.as_deref().unwrap_or("-"),
            action
        )));
    }
    if view.sessions.is_empty() {
        lines.push(t("sessions: none"));
    }
    lines.push(String::new());

    // The newest notes, then the decision count.
    lines.push(t(&format!(
        "notes ({}) - decisions ({})",
        view.notes.len(),
        view.decisions.len()
    )));
    for note in view.notes.iter().take(3) {
        lines.push(t(&format!("   {}", note.text)));
    }
    (lines, follow)
}

/// Word-wrap one paragraph into lines of at most `w` chars on whitespace
/// boundaries; a single word longer than `w` is hard-cut.
fn wrap_line(para: &str, w: usize, out: &mut Vec<String>) {
    if para.is_empty() {
        out.push(String::new());
        return;
    }
    let mut line = String::new();
    for word in para.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > w {
            out.push(std::mem::take(&mut line));
        }
        if line.is_empty() {
            let n = word.chars().count();
            if n > w {
                let cut: String = word.chars().take(w).collect();
                out.push(cut);
                let rest: String = word.chars().skip(w).collect();
                line = rest;
                continue;
            }
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
}

/// First 8 chars of a session id - the join key `fno agents top` prints.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// The overlay's keys. j/k (and arrows) move the selection, Enter runs
/// the selected row (a link drills in, a session launches through the hit
/// cascade, a dim row answers with its reason), `d` toggles the whole
/// details text, `b` plans and `A` asks the king (the board's own sends).
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
        pop_or_close(view);
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
            ModalKey::Byte(b'd') => {
                if let Some(o) = view.backlog_board.as_mut().and_then(|b| b.detail.as_mut()) {
                    o.details_open = !o.details_open;
                }
            }
            ModalKey::Byte(b'b') => backlog_board::dispatch_plan(view, sock_w).await?,
            ModalKey::Byte(b'A') => backlog_board::ask_the_king(view, sock_w).await?,
            ModalKey::Byte(b'e') => backlog_board::edit_title(view)?,
            ModalKey::Byte(b'p') => backlog_board::edit_priority(view)?,
            ModalKey::Byte(b's') => backlog_board::edit_size(view)?,
            ModalKey::Byte(b'S') => backlog_board::edit_status(view)?,
            ModalKey::Byte(b'D') => backlog_board::append_details(view)?,
            _ => {}
        }
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
            let joined = view
                .layout
                .agents
                .iter()
                .find(|a| a.harness_session_id.as_deref() == s.session_id.as_deref());
            match session_action(joined) {
                SessionAction::Attach | SessionAction::Resume => {
                    let a = joined.expect("a derivable action has a row");
                    let hit = agent_hit(a, view.layout.active_squad);
                    view.backlog_board = None;
                    apply_hit(view, hit, sock_w).await?;
                }
                SessionAction::Dim(why) => view.set_notice(why),
            }
        }
    }
    Ok(())
}
