//! Tap-to-reply flow for the Messages thread column, plus the existing peek
//! reply-key owner moved here so client.rs keeps only its routing seam.

use super::mail_input::peek_input_keys;
use super::*;
use crate::popup::{Anchor, Popup, PopupRow};
use serde_json::Value;

#[derive(Clone)]
struct Endpoint {
    name: String,
    session: String,
}

enum Mode {
    Choose(Popup),
    Compose {
        popup: Popup,
        target: Endpoint,
        body: String,
        esc: Vec<u8>,
    },
}

pub(super) struct ReplyState {
    message: String,
    sender: Endpoint,
    receiver: Endpoint,
    summary: String,
    mode: Mode,
    esc: Vec<u8>,
}

pub(super) struct PendingJournal {
    pane: u64,
    to: String,
    session: String,
    message: String,
    body: String,
}

fn text<'a>(row: &'a Value, field: &str) -> &'a str {
    row.get(field).and_then(Value::as_str).unwrap_or("")
}

fn choice_popup(receiver: &str, sender: &str) -> Popup {
    let entry = |label: &str| PopupRow::Entry {
        glyph: "›".into(),
        label: label.into(),
        hint: "enter".into(),
        enabled: true,
    };
    Popup::new(
        vec![
            PopupRow::Header("reply from".into()),
            PopupRow::Rule,
            entry(receiver),
            entry(sender),
        ],
        Anchor::Center,
    )
    .title("reply to message")
    .footer("↑↓ choose · enter · esc cancel")
}

fn compose_popup(body: &str) -> Popup {
    Popup::new(
        vec![
            PopupRow::Header("reply".into()),
            PopupRow::Rule,
            PopupRow::Input {
                label: "text".into(),
                text: body.into(),
                cursor: body.chars().count(),
                placeholder: "type a reply".into(),
            },
        ],
        Anchor::Center,
    )
    .title("mux reply composer")
    .footer("enter send · esc cancel")
}

/// Open the two-person choice for one projected message row.
pub(super) fn open(view: &mut View, row: Value) {
    let Some(board) = view.messages_board.as_ref() else {
        return;
    };
    let sender_key = text(&row, "from_key").to_string();
    let receiver_key = text(&row, "to_key").to_string();
    if sender_key.is_empty() || receiver_key.is_empty() {
        view.set_notice("reply: message has no sender or receiver session".into());
        return;
    }
    let sender = Endpoint {
        name: board.participant_name(&sender_key),
        session: sender_key,
    };
    let receiver = Endpoint {
        name: board.participant_name(&receiver_key),
        session: receiver_key,
    };
    let message = text(&row, "id").to_string();
    if let Some(board) = view.messages_board.as_mut() {
        board.reply = Some(ReplyState {
            message,
            summary: text(&row, "summary").to_string(),
            mode: Mode::Choose(choice_popup(&receiver.name, &sender.name)),
            sender,
            receiver,
            esc: Vec::new(),
        });
    }
}

pub(super) fn popup(view: &View) -> Option<&Popup> {
    let state = view.messages_board.as_ref()?.reply.as_ref()?;
    match &state.mode {
        Mode::Choose(p) | Mode::Compose { popup: p, .. } => Some(p),
    }
}

pub(super) fn active(view: &View) -> bool {
    view.messages_board
        .as_ref()
        .is_some_and(|b| b.reply.is_some())
}

pub(super) fn paint(view: &View, cells: &mut [Cell], rows: usize, cols: usize) {
    if let Some(popup) = popup(view) {
        super::draw_popup_overlay(cells, rows, cols, popup, view.term, &view.theme);
    } else if let Some(detail) = view.messages_board.as_ref().and_then(|b| b.detail.as_ref()) {
        super::draw_popup_overlay(cells, rows, cols, &detail.popup, view.term, &view.theme);
    }
}

pub(super) fn endpoint_pane(
    view: &View,
    target_name: &str,
    target_session: &str,
) -> Option<(u64, String)> {
    let visible_pane = |agent: &crate::proto::AgentRow| {
        let pane = agent
            .pane_id
            .filter(|id| view.layout.panes.iter().any(|(visible, _)| visible == id))?;
        Some((pane, agent.effective_identity()?.to_string()))
    };
    let mut exact = view
        .layout
        .agents
        .iter()
        .filter(|a| !a.exited && a.harness_session_id.as_deref() == Some(target_session))
        .filter_map(visible_pane);
    let exact_match = exact.next();
    if exact.next().is_some() {
        return None;
    }
    if exact_match.is_some() {
        return exact_match;
    }
    let mut named = view
        .layout
        .agents
        .iter()
        .filter(|a| !a.exited && a.name == target_name)
        .filter_map(visible_pane);
    let named_match = named.next();
    if named.next().is_some() {
        None
    } else {
        named_match
    }
}

fn start_compose(state: &mut ReplyState, sender: bool) {
    let target = if sender {
        state.sender.clone()
    } else {
        state.receiver.clone()
    };
    let seed = format!(
        "re {}/{}: \"{}\" ",
        state.sender.name, state.message, state.summary
    );
    let body: String = seed.chars().take(MAX_MAIL_TEXT).collect();
    state.mode = Mode::Compose {
        popup: compose_popup(&body),
        target,
        body,
        esc: Vec::new(),
    };
}

pub(super) async fn keys(
    view: &mut View,
    bytes: &[u8],
    sock: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let Some(mut state) = view.messages_board.as_mut().and_then(|b| b.reply.take()) else {
        return Ok(StdinFlow::Continue);
    };
    let mut close = false;
    let mut selected_sender = None;
    match &mut state.mode {
        Mode::Choose(popup) => {
            for key in fold_modal_keys(&mut state.esc, bytes) {
                match key {
                    ModalKey::Esc | ModalKey::Byte(b'q') => {
                        close = true;
                        break;
                    }
                    ModalKey::Up | ModalKey::Byte(b'k') => popup.nav(crate::popup::NavDir::Up),
                    ModalKey::Down | ModalKey::Byte(b'j') => popup.nav(crate::popup::NavDir::Down),
                    ModalKey::Enter => {
                        selected_sender = Some(popup.selected().is_some_and(|(row, _)| row >= 3));
                        break;
                    }
                    _ => {}
                }
            }
        }
        Mode::Compose {
            target, body, esc, ..
        } => {
            for key in fold_search_input(esc, bytes) {
                match key {
                    SearchKey::Esc => {
                        close = true;
                        break;
                    }
                    SearchKey::Byte(b'\r' | b'\n') => {
                        let Some((pane, expected_identity)) =
                            endpoint_pane(view, &target.name, &target.session)
                        else {
                            view.set_notice(format!(
                                "reply: {} has no pane on screen; open a portal first",
                                target.name
                            ));
                            break;
                        };
                        if body.trim().is_empty() {
                            break;
                        }
                        let request_id = view.next_reply_request_id;
                        view.next_reply_request_id =
                            view.next_reply_request_id.wrapping_add(1).max(1);
                        let mut input = body.as_bytes().to_vec();
                        input.push(b'\r');
                        // A bus reply is a delivery claim; journal only after
                        // the server acknowledges this exact pane write.
                        write_msg(
                            sock,
                            &ClientMsg::PaneInput {
                                request_id,
                                pane,
                                expected_identity,
                                bytes: input,
                            },
                        )
                        .await
                        .map_err(|e| format!("reply input send failed: {e}"))?;
                        view.pending_reply_journals.insert(
                            request_id,
                            PendingJournal {
                                pane,
                                to: target.name.clone(),
                                session: target.session.clone(),
                                message: state.message.clone(),
                                body: body.clone(),
                            },
                        );
                        view.set_notice(format!("reply to {} sending", target.name));
                        close = true;
                        break;
                    }
                    SearchKey::Byte(0x7f | 0x08) => {
                        body.pop();
                    }
                    SearchKey::Byte(0x15) => body.clear(),
                    SearchKey::Byte(b @ 0x20..=0x7e) if body.chars().count() < MAX_MAIL_TEXT => {
                        body.push(b as char)
                    }
                    _ => {}
                }
            }
            if let Mode::Compose { popup, body, .. } = &mut state.mode {
                *popup = compose_popup(body);
            }
        }
    }
    if let Some(sender) = selected_sender {
        start_compose(&mut state, sender);
    }
    if let Some(board) = view.messages_board.as_mut() {
        if !close {
            board.reply = Some(state);
        }
    }
    Ok(StdinFlow::Continue)
}

async fn journal_reply(to: &str, sid: &str, msg: &str, body: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("fno-agents")
        .args([
            "mail-threads",
            "journal-reply",
            "--to",
            to,
            "--to-session",
            sid,
            "--in-reply-to",
            msg,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .ok_or("journal stdin unavailable")?
        .write_all(body.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let output = child.wait_with_output().await.map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub(super) fn input_result(
    view: &mut View,
    request_id: u64,
    pane: u64,
    result: Result<(), String>,
) {
    let Some(pending) = view.pending_reply_journals.remove(&request_id) else {
        return;
    };
    if pending.pane != pane {
        view.set_notice("reply delivery receipt named a different pane".into());
        return;
    }
    let Err(reason) = result else {
        let PendingJournal {
            to,
            session,
            message,
            body,
            ..
        } = pending;
        let notice_to = to.clone();
        let notice_tx = view.reply_notice_tx.clone();
        tokio::spawn(async move {
            if let Err(reason) = journal_reply(&to, &session, &message, &body).await {
                if let Some(tx) = notice_tx {
                    let _ = tx.send(format!("reply journal failed: {reason}"));
                }
            }
        });
        view.set_notice(format!("reply delivered to {notice_to}"));
        return;
    };
    view.set_notice(format!("reply not delivered: {reason}"));
}

/// The peek overlay's established key path lives here alongside reply input.
pub(super) async fn peek_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    if view.peek_input.is_some() {
        return peek_input_keys(view, bytes, sock_w).await;
    }
    let mut esc = std::mem::take(&mut view.peek_esc);
    let keys = fold_selector_keys(&mut esc, bytes);
    view.peek_esc = esc;
    for &k in &keys {
        let Some(cursor) = view.peek.as_ref().map(|p| p.cursor) else {
            break;
        };
        match k {
            b'j' | b'k' => match view.peek_next_agent(cursor, if k == b'j' { 1 } else { -1 }) {
                Some(next) => {
                    if let Some(name) = match view.display_rows().get(next) {
                        Some(DisplayRow::Agent(a)) => Some(a.name.clone()),
                        _ => None,
                    } {
                        fetch_peek(view, next, name, sock_w).await?;
                    }
                }
                None => {
                    let _ = raw_out(b"\x07");
                }
            },
            b'0'..=b'9' => {
                let payload = match view.display_rows().get(cursor) {
                    Some(DisplayRow::Agent(a)) => {
                        a.answerable
                            .as_ref()
                            .zip(a.pane_id)
                            .and_then(|(ans, pane)| {
                                ans.options
                                    .iter()
                                    .find(|o| o.idx.as_bytes().first() == Some(&k))
                                    .map(|o| {
                                        (
                                            pane,
                                            ans.fingerprint,
                                            ans.region_lines as u16,
                                            o.keystroke.clone(),
                                        )
                                    })
                            })
                    }
                    _ => None,
                };
                if let Some((pane, fingerprint, region_lines, keystroke)) = payload {
                    write_msg(
                        sock_w,
                        &ClientMsg::PaneAnswer {
                            pane,
                            fingerprint,
                            region_lines,
                            keystroke,
                        },
                    )
                    .await
                    .map_err(|e| format!("answer send failed: {e}"))?;
                } else {
                    let _ = raw_out(b"\x07");
                }
            }
            b'l' | b'\r' | b'\n' => match view.display_rows().get(cursor) {
                Some(DisplayRow::Agent(a)) => match agent_hit(a, view.layout.active_squad) {
                    ChromeHit::Notice(msg) => view.set_notice(msg.to_string()),
                    hit => {
                        view.clear_peek();
                        view.selector = None;
                        apply_hit(view, hit, sock_w).await?;
                    }
                },
                _ => {
                    let _ = raw_out(b"\x07");
                }
            },
            b'm' => match view.display_rows().get(cursor) {
                Some(DisplayRow::Agent(a)) => {
                    view.peek_input = Some((a.name.clone(), super::composer_draft::load(&a.name)));
                    view.peek_input_esc.clear();
                    break;
                }
                _ => {
                    let _ = raw_out(b"\x07");
                }
            },
            b'r' => match view.display_rows().get(cursor) {
                Some(DisplayRow::Agent(a)) if a.exited => {
                    write_msg(
                        sock_w,
                        &ClientMsg::Command(Command::RespawnAgent {
                            name: a.name.clone(),
                        }),
                    )
                    .await
                    .map_err(|e| format!("respawn send failed: {e}"))?;
                }
                _ => {
                    let _ = raw_out(b"\x07");
                }
            },
            0x1b | b'q' => {
                let restore = view.selector.is_some();
                view.clear_peek();
                if restore {
                    view.selector = Some(cursor);
                }
            }
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}
