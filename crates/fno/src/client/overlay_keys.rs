//! The overlay precedence chain - the one list deciding which surface owns
//! the client keyboard - moved out of `client.rs` `handle_stdin` for the
//! shrink-only ratchet, plus the quiet-window flush that releases a lone
//! ESC carry to the overlay on top.

use super::keys_modal::keys_modal_keys;
use super::*;
use super::{
    attach_place_keys, confirm_keys, connections_keys, is_sideline_verb, move_pick_keys, peek_keys,
    portal_pick_keys, row_menu_keys, selector_keys, yard_keys, StdinFlow, View,
};
use super::{aux_keys, questions, sideline};
use super::{backlog_board, org_board};

/// Route one stdin chunk to the overlay that owns the keyboard, in
/// precedence order. `None` when no overlay owns it: the caller falls
/// through to the chord scanner. An empty `bytes` is the quiet-window
/// flush; overlays with no esc carry (digest, confirm, move-to, the
/// hover-armed selector, the launcher dock) are unchanged by it - they act
/// on real bytes only - and the overlays with a carry release a lone ESC
/// as their Esc action through their fold's empty-read arm.
pub(super) async fn route(
    view: &mut View,
    scanner: &mut crate::keys::Scanner,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Option<Result<StdinFlow, String>> {
    if view.selector.is_none() {
        view.questions_block.cursor = None;
    }
    if view.digest.is_some() {
        // any key dismisses the catch-up digest into the normal view.
        // Same whole-chunk swallow as the key-table overlay below. A flush
        // is not a key: leave the digest up.
        if bytes.is_empty() {
            return Some(Ok(StdinFlow::Continue));
        }
        view.digest = None;
        return Some(Ok(StdinFlow::Continue));
    }
    if view.keys_modal.is_some() {
        // US3 which-key: a bound key executes through the shared dispatch,
        // arrows/pgup scroll+select, Enter runs the selected row, Esc/unbound
        // dismiss. Routed here (same precedence as the old poster) so its keys
        // never leak to a pane.
        return Some(keys_modal_keys(view, scanner, bytes, sock_w).await);
    }
    if view.row_menu.is_some() {
        // US2: the row context menu consumes keys while open (arrows walk
        // the entries + grid, Enter runs, Esc/q close) - never leaks to a pane.
        return Some(row_menu_keys(view, bytes, sock_w).await);
    }
    if view.aux.is_some() {
        // US4/US5: the MENU popup / settings modal consumes keys.
        return Some(aux_keys(view, bytes, sock_w).await);
    }
    if view.connections.is_some() {
        // the Connections modal consumes all keys while open (Tab
        // switches tabs, j/k move, R refreshes, Esc closes) - never leaks to a
        // pane. Routed here (top-level modal, like the MENU it opened from).
        return Some(connections_keys(view, bytes, sock_w).await);
    }
    if view.confirm.is_some() {
        if bytes.is_empty() {
            return Some(Ok(StdinFlow::Continue));
        }
        return Some(confirm_keys(view, bytes, sock_w).await);
    }
    if view.move_pick.is_some() {
        // Modal like confirm: a single digit/Esc resolves it. Ahead of
        // the selector (which it replaced on open) so its keys can't leak there.
        return Some(move_pick_keys(view, bytes, sock_w).await);
    }
    if view.attach_place.is_some() {
        return Some(attach_place_keys(view, bytes, sock_w).await);
    }
    if view.portal_pick.is_some() {
        // The portal picker consumes keys while open, ahead of the
        // selector it replaced - same precedence slot as its sibling.
        return Some(portal_pick_keys(view, bytes, sock_w).await);
    }
    if view.peek.is_some() {
        // peek sits ON TOP of the selector; routed BEFORE it so its keys
        // (j/k, Esc, later digit/attach) never leak to the selector underneath.
        return Some(peek_keys(view, bytes, sock_w).await);
    }
    // A hover-armed selector is motion-fresh: only the action-verb set
    // acts on the pointed-at row; the first key OUTSIDE it disarms the arm and
    // falls through to the pane, so a pointer parked over the sideline never
    // swallows typing into the focused shell (AC2-EDGE). An explicitly-opened
    // selector (sel_hover_armed=false) stays fully modal below.
    if view.selector.is_some() && view.sel_hover_armed {
        if bytes.is_empty() {
            return Some(Ok(StdinFlow::Continue));
        }
        if bytes.first().is_some_and(|&b| is_sideline_verb(b)) {
            return Some(selector_keys(view, bytes, sock_w).await);
        }
        view.selector = None;
        view.sel_hover_armed = false;
        // fall through: forward this chunk to the focused pane.
    }
    if view.selector.is_some() {
        return Some(selector_keys(view, bytes, sock_w).await);
    }
    if view.question_detail.is_some() {
        return Some(questions::detail_keys(view, bytes, sock_w).await);
    }
    if view.bell.open {
        bell::keys(view, bytes);
        return Some(Ok(StdinFlow::Continue));
    }
    if view.answers.is_some() {
        return Some(answer_keys(view, bytes, sock_w).await);
    }
    if view.yard.is_some() {
        return Some(yard_keys(view, bytes, sock_w).await);
    }
    // The feed panel is chrome and consumes no keys UNTIL the operator
    // focuses it (`E`, or a click inside the panel). Esc and e close the
    // panel outright from the focused mode, and a pane click moves the
    // keyboard back with the panel still open, so the property this slot
    // protects - typing reaches the focused pane - holds by default and is
    // set aside only on request.
    if view.feed_detail.is_some() || view.input_owner() == super::region_focus::RegionOwner::Feed {
        return Some(super::feed_view::feed_keys(view, bytes, sock_w).await);
    }
    if view.create.is_some() {
        return Some(create_keys(view, bytes, sock_w).await);
    }
    if view.rename.is_some() {
        // Same precedence slot as create_keys: AFTER selector/answers, so a
        // lingering overlay never swallows the typed name (finding).
        return Some(rename_keys(view, bytes, sock_w).await);
    }
    if view.move_to.is_some() {
        // Same precedence slot as the rename overlay it mirrors. No esc
        // carry: the first byte decides the prompt, so a flush is a no-op.
        if bytes.is_empty() {
            return Some(Ok(StdinFlow::Continue));
        }
        return Some(move_to_keys(view, bytes, sock_w).await);
    }
    if view.recruit.is_some() {
        return Some(recruit_keys(view, bytes, sock_w).await);
    }
    if view.search.is_some() {
        return Some(search_keys(view, bytes, sock_w).await);
    }
    if view.nav.is_some() {
        return Some(nav_keys(view, bytes, sock_w).await);
    }
    if view.launcher.is_some() {
        // The composer is a modal like every other: prefix chords still
        // resolve while it holds the keyboard (which-key parity), so the
        // chunk scans first and only the plain-byte chunks feed the
        // composer's own folder. Nothing here reaches a pane. Its quiet
        // window is the scanner's, upstream: a flush chunk scans to
        // nothing and must not reach the dock's folder.
        if bytes.is_empty() {
            return Some(Ok(StdinFlow::Continue));
        }
        return Some(sideline::route_launcher_keys(view, scanner, bytes, sock_w).await);
    }
    if view.messages_board.is_some() {
        // The Messages tab is a full-surface view: it owns the keyboard
        // unconditionally while open, the same rule a full-screen board keeps.
        return Some(messages_view::route_keys(view, scanner, bytes, sock_w).await);
    }
    if view.org_board.is_some()
        && (view.board_full || view.input_owner() == super::region_focus::RegionOwner::Board)
    {
        return Some(org_board::route_keys(view, scanner, bytes, sock_w).await);
    }
    if view.backlog_board.is_some()
        && (view.board_full || view.input_owner() == super::region_focus::RegionOwner::Board)
    {
        // the experimental backlog board consumes keys while it OWNS the
        // keyboard (open, or clicked); its inputs, pickers, and facets ride
        // inside it. A windowed board that lost the keyboard to a pane click
        // stays visible but takes nothing. Prefix chords still resolve first
        // (which-key parity): the board's folder sees only the plain-byte
        // chunks. The board sits BELOW every other modal: a chord can open
        // one over it (composer, selector, answers, yard, connections, ...),
        // and a visible child modal owns the keyboard - or its keys would
        // die in the board's folder behind it.
        return Some(backlog_board::route_board_keys(view, scanner, bytes, sock_w).await);
    }
    None
}

/// A chord candidate the quiet window releases while an overlay holds the
/// keyboard: a flushed plain chunk feeds the OWNING overlay's folder (the
/// composer's, the board's), never a pane that may not even be
/// painted; the overlay's own chord events dispatch through the shared path.
/// No overlay: the released event is the pane's.
pub(super) async fn flush_released_chord(
    view: &mut View,
    event: crate::keys::Event,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    if view.launcher.is_some() {
        if let crate::keys::Event::Forward(chunk) = &event {
            return super::agent_launcher::launcher_keys(view, chunk, sock_w)
                .await
                .map(|_| ());
        }
    } else if view.messages_board.is_some() {
        if let crate::keys::Event::Forward(chunk) = &event {
            return messages_view::keys(view, chunk, sock_w).await.map(|_| ());
        }
    } else if view.org_board.is_some()
        && view.input_owner() == super::region_focus::RegionOwner::Board
    {
        if let crate::keys::Event::Forward(chunk) = &event {
            return org_board::keys(view, chunk, sock_w).await.map(|_| ());
        }
    } else if view.backlog_board.is_some()
        && view.input_owner() == super::region_focus::RegionOwner::Board
    {
        if let crate::keys::Event::Forward(chunk) = &event {
            return backlog_board::board_keys(view, chunk, sock_w)
                .await
                .map(|_| ());
        }
    } else if view.feed.is_some() && view.input_owner() == super::region_focus::RegionOwner::Feed {
        if let crate::keys::Event::Forward(chunk) = &event {
            return super::feed_view::feed_keys(view, chunk, sock_w)
                .await
                .map(|_| ());
        }
    }
    super::dispatch_event(view, event, sock_w).await.map(|_| ())
}

/// The quiet window elapsed while a raw-fed overlay may hold a lone ESC in
/// its carry: route an empty read through the overlay chain so the owning
/// fold releases it as the Esc action. No overlay open: nothing to do.
pub(super) async fn flush_lone_esc(
    view: &mut View,
    scanner: &mut crate::keys::Scanner,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    match route(view, scanner, &[], sock_w).await {
        Some(flow) => flow,
        None => Ok(StdinFlow::Continue),
    }
}

/// Search-mode keys (v12). Typing: printable append, Backspace pops,
/// Enter submits (send [`ClientMsg::SearchOpen`]), Esc cancels locally. Browsing
/// (post-submit): `n`/`N` send [`ClientMsg::SearchStep`] (older/newer), Esc sends
/// [`ClientMsg::SearchClear`] and exits. Esc ALWAYS exits the mode locally even
/// if the server never replied (AC1-FR: a lost `SearchResult` never wedges the
/// input line). The mode is re-read per key so an Esc mid-chunk swallows the rest.
pub(super) async fn search_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.search_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.search_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it, and the rest of
        // the chunk must be swallowed, never forwarded.
        let Some(sv) = view.search.as_ref() else {
            break;
        };
        let (pane, submitted) = (sv.pane, sv.submitted);
        match key {
            SearchKey::Esc => {
                view.search = None;
                view.search_esc.clear();
                if submitted {
                    // Browsing: drop the shared server-side highlight + state.
                    write_msg(sock_w, &ClientMsg::SearchClear { pane })
                        .await
                        .map_err(|e| format!("search-clear send failed: {e}"))?;
                }
                break;
            }
            SearchKey::Byte(b) if !submitted => match b {
                b'\r' | b'\n' => {
                    if let Some(sv) = view.search.as_mut() {
                        sv.submitted = true;
                        let query = sv.query.clone();
                        write_msg(sock_w, &ClientMsg::SearchOpen { pane, query })
                            .await
                            .map_err(|e| format!("search-open send failed: {e}"))?;
                    }
                }
                0x7f | 0x08 => {
                    if let Some(sv) = view.search.as_mut() {
                        sv.query.pop();
                    }
                }
                0x15 => input_field::clear_opt(view.search.as_mut().map(|sv| &mut sv.query)),
                // ASCII printable appends (other control bytes ignored; query is
                // ASCII in v1). Capped so a held key / paste can't grow it unbounded
                // and drive an O(len * scrollback) server scan.
                0x20..=0x7e => {
                    if let Some(sv) = view.search.as_mut() {
                        if sv.query.len() < MAX_SEARCH_QUERY {
                            sv.query.push(b as char);
                        }
                    }
                }
                _ => {}
            },
            SearchKey::Byte(b) => match b {
                b'n' => write_msg(
                    sock_w,
                    &ClientMsg::SearchStep {
                        pane,
                        dir: BlockDir::Prev,
                    },
                )
                .await
                .map_err(|e| format!("search-step send failed: {e}"))?,
                b'N' => write_msg(
                    sock_w,
                    &ClientMsg::SearchStep {
                        pane,
                        dir: BlockDir::Next,
                    },
                )
                .await
                .map_err(|e| format!("search-step send failed: {e}"))?,
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Navigator-overlay keys: a client-owned typing overlay like search.
/// Printable bytes edit the text filter (Locked 5: letters are ALWAYS query
/// text, never state keys); Backspace widens; `Tab`/`Shift-Tab` cycle the state
/// chip forward/back; `Up`/`Down` (or `Ctrl-p`/`Ctrl-n`) move the cursor over the
/// filtered rows (clamped, no wrap); Enter or bare `Right` goto's the row;
/// `Esc` or bare `Left` closes. Uses
/// [`fold_nav_input`]'s split-arrow fold (which surfaces the motion finals while
/// swallowing every other escape) and a per-key re-read so an Esc mid-chunk
/// swallows the chunk's remainder.
pub(super) async fn nav_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.nav_esc);
    let keys = fold_nav_input(&mut esc, bytes);
    view.nav_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it and the rest of
        // the chunk must be swallowed, never forwarded.
        if view.nav.is_none() {
            break;
        }
        match key {
            NavKey::Esc | NavKey::Left => {
                view.nav = None;
                view.nav_esc.clear();
                break;
            }
            // Arrows mirror Ctrl-p/Ctrl-n; Shift-Tab reverses the state ring.
            NavKey::Up => view.nav_move_cursor(-1),
            NavKey::Down => view.nav_move_cursor(1),
            // Bare Right reaches the selected row - the same
            // nav_goto the Enter arm calls, so substrate behavior (attach a
            // thread, focus a pane, refuse with a notice) is the same too.
            NavKey::Right => nav_goto(view, sock_w).await?,
            NavKey::ShiftTab => {
                view.nav_cycle_state_rev();
                view.nav_ring_if_empty();
            }
            NavKey::Byte(b) => match b {
                b'\r' | b'\n' => nav_goto(view, sock_w).await?,
                b'\t' => {
                    view.nav_cycle_state();
                    view.nav_ring_if_empty();
                }
                // Ctrl-n / Ctrl-p move the cursor (readline convention), kept
                // alongside the arrow tokens for muscle memory.
                0x0e => view.nav_move_cursor(1),
                0x10 => view.nav_move_cursor(-1),
                0x7f | 0x08 => {
                    if let Some(n) = view.nav.as_mut() {
                        n.query.pop();
                        n.cursor = 0;
                    }
                    view.nav_ring_if_empty();
                }
                0x15 => {
                    input_field::clear_opt(view.nav.as_mut().map(|n| &mut n.query));
                    view.nav_ring_if_empty();
                }
                // Printable ASCII edits the query; capped like search so a held
                // key / paste can't grow it unbounded. Cursor re-anchors to 0.
                0x20..=0x7e => {
                    if let Some(n) = view.nav.as_mut() {
                        if n.query.len() < MAX_SEARCH_QUERY {
                            n.query.push(b as char);
                            n.cursor = 0;
                        }
                    }
                    view.nav_ring_if_empty();
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Teleport to the navigator's cursor row. Materializes the OWNED
/// target before mutating the view (`nav_rows` borrows the layout), re-reading
/// the filtered catalog at Enter time (per-key re-read; AC4-ERR relies on the
/// server refusing a stale id fail-closed). A refusal (`Notice`: blocked /
/// in-flight card, paneless agent) KEEPS the navigator open and shows the notice
/// (Locked 6), sending nothing. Otherwise it closes the overlay, switches squad
/// when the target lives in another one (a same-squad target collapses to a bare
/// hit), and applies the hit. Existing wire commands only - no new `Command`, no
/// proto bump (Locked 4).
pub(super) async fn nav_goto(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let target = match view.nav.as_ref() {
        Some(n) => match view.nav_filtered(n).into_iter().nth(n.cursor) {
            Some(r) => r,
            // Empty/stale cursor: BEL, keep the overlay open (never a silent
            // close), matching the selector's stale-cursor BEL.
            None => {
                let _ = raw_out(b"\x07");
                return Ok(());
            }
        },
        None => return Ok(()),
    };
    // A refusal keeps the overlay open (Locked 6), identical to the selector.
    if let ChromeHit::Notice(msg) = &target.hit {
        view.set_notice(msg.clone());
        return Ok(());
    }
    view.nav = None;
    view.nav_esc.clear();
    // Ordered goto prefix (Locked 4: existing wire commands only). An agent/pane
    // row in another squad switches squad first; a pane row then selects its tab
    // (FocusPane alone does not) so the sequence is SelectSquad -> SelectTab ->
    // FocusPane. Squad/tab rows carry their own switch in `hit` (both prefixes
    // None), so no double send; a pane already in the active view collapses to a
    // bare FocusPane.
    let switching_squad = target
        .goto_squad
        .is_some_and(|sq| sq != view.layout.active_squad);
    if let Some(sq) = target.goto_squad.filter(|_| switching_squad) {
        write_msg(sock_w, &ClientMsg::Command(Command::SelectSquad(sq)))
            .await
            .map_err(|e| format!("nav select-workspace send failed: {e}"))?;
    }
    if let Some(tid) = target.goto_tab {
        // Skip SelectTab only when the target is already the active view's tab
        // (same squad, same tab); a squad switch always needs it.
        let active_tab_id = view
            .layout
            .squads
            .iter()
            .find(|s| s.id == view.layout.active_squad)
            .and_then(|s| s.tabs.get(s.active_tab))
            .map(|t| t.id);
        if switching_squad || active_tab_id != Some(tid) {
            write_msg(sock_w, &ClientMsg::Command(Command::SelectTab(tid)))
                .await
                .map_err(|e| format!("nav select-tab send failed: {e}"))?;
        }
    }
    apply_hit(view, target.hit, sock_w).await
}

/// New-workspace name-input keys. Reuses the search input's split-arrow
/// folding: printable ASCII appends, Backspace pops, Enter sends
/// [`Command::NewSquad`] with the typed name (an empty name keeps the overlay
/// open - the server would reject it, and keeping it open avoids the round trip),
/// Esc cancels locally. The whole chunk is swallowed so an arrow's escape tail
/// never leaks into a pane.
pub(super) async fn create_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.create_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.create_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it, and the rest of
        // the chunk must be swallowed, never forwarded.
        if view.create.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                view.create = None;
                view.create_esc.clear();
                break;
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    // Validate on a reference; only allocate when actually sending.
                    if let Some(name) = view.create.as_deref().map(str::trim) {
                        if !name.is_empty() {
                            write_msg(
                                sock_w,
                                &ClientMsg::Command(Command::NewSquad {
                                    name: name.to_string(),
                                    origin: None,
                                }),
                            )
                            .await
                            .map_err(|e| format!("new-workspace send failed: {e}"))?;
                            view.create = None;
                            view.create_esc.clear();
                            break;
                        }
                    }
                    // Empty name: keep the overlay open (AC2-FR shape - a failed
                    // create leaves the input intact).
                }
                0x7f | 0x08 => {
                    if let Some(buf) = view.create.as_mut() {
                        buf.pop();
                    }
                }
                0x15 => input_field::clear_opt(view.create.as_mut()),
                0x20..=0x7e => {
                    if let Some(buf) = view.create.as_mut() {
                        if buf.len() < MAX_SEARCH_QUERY {
                            buf.push(b as char);
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Recruit workspace-name keys: the create overlay's shape - printable
/// append, Backspace pops, Esc cancels locally (marks kept), Enter sends
/// [`Command::RecruitAgents`] with the marked ids and CLEARS the marks. An empty
/// name keeps the overlay open (the server would refuse it). An empty mark set
/// falls back to nothing sendable, so Enter just closes (the `R` key already
/// fell back to marking the focused row before opening).
pub(super) async fn recruit_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.recruit_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.recruit_esc = esc;
    for key in keys {
        if view.recruit.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                view.recruit = None;
                view.recruit_esc.clear();
                break; // marks kept - Esc cancels the prompt only
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    if let Some(name) = view.recruit.as_deref().map(str::trim) {
                        if !name.is_empty() {
                            let ids: Vec<String> = view.marks.iter().cloned().collect();
                            write_msg(
                                sock_w,
                                &ClientMsg::Command(Command::RecruitAgents {
                                    squad: name.to_string(),
                                    ids,
                                }),
                            )
                            .await
                            .map_err(|e| format!("recruit send failed: {e}"))?;
                            view.recruit = None;
                            view.recruit_esc.clear();
                            view.marks.clear(); // submit clears the marks (AC2-HP)
                            break;
                        }
                    }
                    // Empty name: keep the overlay open (server would refuse).
                }
                0x7f | 0x08 => {
                    if let Some(buf) = view.recruit.as_mut() {
                        buf.pop();
                    }
                }
                0x15 => input_field::clear_opt(view.recruit.as_mut()),
                0x20..=0x7e => {
                    if let Some(buf) = view.recruit.as_mut() {
                        if buf.len() < MAX_SQUAD_NAME {
                            buf.push(b as char);
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Rename-tab name-input keys. The create overlay's shape (split-arrow
/// folding, printable append, Backspace pops, Esc cancels locally) with one
/// deliberate divergence: Enter ALWAYS sends [`Command::RenameTab`] - an empty
/// buffer is the "reset to auto" verb (blank clears server-side), not a kept-open
/// input. The buffer caps at [`MAX_TAB_NAME`] so the operator sees exactly what
/// the server will store (the server-side cap stays authoritative for the wire).
pub(super) async fn rename_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.rename_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.rename_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it, and the rest
        // of the chunk must be swallowed, never forwarded.
        if view.rename.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                // AC1-UI: no command sent, chrome restored, no state retained.
                view.rename = None;
                view.rename_esc.clear();
                break;
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    if let Some((target, name)) = view.rename.take() {
                        // An agent label is never derived, so an empty buffer
                        // is NOT the tab/squad "reset to auto": the overlay
                        // stays open and the send never happens.
                        if matches!(&target, RenameTarget::Agent(_)) && name.is_empty() {
                            view.rename = Some((target, name));
                            view.set_notice("label required - type the new registry label".into());
                            break;
                        }
                        view.rename_esc.clear();
                        let cmd = match target {
                            RenameTarget::Tab(tab) => Command::RenameTab { tab, name },
                            RenameTarget::Squad(squad) => Command::RenameSquad { squad, name },
                            RenameTarget::Agent(agent) => Command::RenameAgent {
                                name: agent,
                                new_name: name,
                            },
                        };
                        write_msg(sock_w, &ClientMsg::Command(cmd))
                            .await
                            .map_err(|e| format!("rename send failed: {e}"))?;
                    }
                    break;
                }
                0x7f | 0x08 => {
                    if let Some((_, buf)) = view.rename.as_mut() {
                        buf.pop();
                    }
                }
                0x15 => input_field::clear_opt(view.rename.as_mut().map(|(_, buf)| buf)),
                0x20..=0x7e => {
                    if let Some((target, buf)) = view.rename.as_mut() {
                        // Cap to the target's stored ceiling so the operator sees
                        // exactly what the server will keep (server stays
                        // authoritative for the wire).
                        let cap = match target {
                            RenameTarget::Tab(_) => MAX_TAB_NAME,
                            RenameTarget::Squad(_) => MAX_SQUAD_NAME,
                            // The registry grammar's own ceiling.
                            RenameTarget::Agent(_) => 64,
                        };
                        // An agent label admits only grammar bytes; a space or
                        // symbol never enters the buffer, so what is typed is
                        // what the server would keep.
                        let legal = match target {
                            RenameTarget::Agent(_) => {
                                b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
                            }
                            _ => true,
                        };
                        if legal && buf.len() < cap {
                            buf.push(b as char);
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Move-to-position prompt keys. The rename overlay's shape
/// ([`rename_keys`]: Esc cancels locally, Backspace pops, printable append)
/// with a numeric grammar: only digits enter the buffer, Enter resolves the
/// 1-based ordinal against the tab's squad strip, computes the delta to the
/// tab's current index, and sends ONE `Command::ReorderTab`. An out-of-range
/// ordinal keeps the prompt open with a notice - it never sends a clamped
/// guess, because a move that lands somewhere else than named is worse than
/// no move. The tab id was captured at open, so a tab switch mid-edit cannot
/// retarget the send.
pub(super) async fn move_to_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    for &b in bytes {
        if view.move_to.is_none() {
            break;
        }
        match b {
            0x1b => {
                view.move_to = None;
                break;
            }
            b'\r' | b'\n' => {
                if let Some((tab, buf)) = view.move_to.take() {
                    let ordinal: usize = buf.parse().unwrap_or(0);
                    let Some((squad, current_idx, _)) = view.find_tab(tab) else {
                        view.set_notice("tab is no longer here".into());
                        break;
                    };
                    let len = view
                        .layout
                        .squads
                        .iter()
                        .find(|s| s.id == squad)
                        .map(|s| s.tabs.len())
                        .unwrap_or(0);
                    if ordinal == 0 || ordinal > len {
                        // Re-open with the typed text intact: the prompt stays
                        // open with a notice, so a typo costs a Backspace, not
                        // the whole gesture.
                        view.move_to = Some((tab, buf));
                        view.set_notice(format!("position is 1..={len}"));
                    } else {
                        let delta = ordinal as i64 - 1 - current_idx as i64;
                        if delta == 0 {
                            view.set_notice(format!("tab already at {ordinal}"));
                        } else {
                            write_msg(
                                sock_w,
                                &ClientMsg::Command(Command::ReorderTab {
                                    squad,
                                    tab,
                                    delta: delta as i32,
                                }),
                            )
                            .await
                            .map_err(|e| format!("reorder-tab send failed: {e}"))?;
                        }
                    }
                }
                break;
            }
            0x7f | 0x08 => {
                if let Some((_, buf)) = view.move_to.as_mut() {
                    buf.pop();
                }
            }
            0x15 => input_field::clear_opt(view.move_to.as_mut().map(|(_, buf)| buf)),
            b'0'..=b'9' => {
                if let Some((_, buf)) = view.move_to.as_mut() {
                    // Four digits is far past any tab strip; the cap keeps the
                    // operator seeing exactly the number Enter will send.
                    if buf.len() < 4 {
                        buf.push(b as char);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// Needs-me overlay keys (grown from; folded MINE and
/// live questions in as editable/answerable lanes). A digit answers the
/// selected answerable NEED row (unchanged [`ClientMsg::PaneAnswer`]) or, for
/// a question with options, closes it via `outstanding clear --answer`.
/// `n`/`N` (and j/k/arrows) cycle lanes, Enter routes per kind, q/Esc closes;
/// `x`/`d` toggle/drop a MINE row, `a` opens a text entry that appends an
/// item. Mutations only queue `View::mine_action` / `View::question_action`
/// (each single-flighted): the file is the one writer, so the render updates
/// once the mutation lands, never optimistically. The projection is read once
/// per chunk from [`View::needs_projection`], so cursor and rows never
/// diverge. An empty overlay closes on any key except `a`. Closing bumps the
/// generation token so an in-flight fold result is discarded.
pub(super) async fn answer_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.ans_esc);
    let keys = fold_selector_keys(&mut esc, bytes); // arrows -> hjkl twins
    view.ans_esc = esc;
    let projection = view.needs_projection();
    // Active squad/tab, captured once (the layout is stable within a key chunk):
    // an Enter goto sends SelectSquad/SelectTab only when they would change the
    // view, mirroring the nav goto so a same-context row emits just
    // FocusPane (no redundant selects).
    let active_squad = view.layout.active_squad;
    let active_tab = view
        .layout
        .squads
        .iter()
        .find(|s| s.id == active_squad)
        .and_then(|s| s.tabs.get(s.active_tab))
        .map(|t| t.id);
    for &k in &keys {
        // The MINE add text entry owns the keyboard ahead of everything below
        // it - a typed letter must never be read as a cycle/answer key.
        if let Some(buf) = view.mine_adding.as_mut() {
            match k {
                b'\r' | b'\n' => {
                    let text = std::mem::take(buf).trim().to_string();
                    view.mine_adding = None;
                    if !text.is_empty() && !view.mine_acting {
                        view.mine_action = Some(crate::needs_overlay::MineMutation::Add(text));
                        view.mine_acting = true;
                    }
                }
                0x1b => view.mine_adding = None,
                0x7f | 0x08 => {
                    buf.pop();
                }
                0x15 => buf.clear(),
                0x20..=0x7e => buf.push(k as char),
                _ => {}
            }
            continue;
        }
        // `a` opens the add entry unconditionally - unlike every other key it
        // has a meaning on an empty overlay (start the operator's first
        // item), so it is handled ahead of the empty-dismisses-all rule.
        if k == b'a' {
            if !view.mine_acting {
                view.mine_adding = Some(String::new());
            }
            continue;
        }
        // The empty "nothing needs you" state: any other key dismisses it
        // (AC4-EDGE).
        if projection.rows.is_empty() {
            view.answers = None;
            view.needs_gen = view.needs_gen.wrapping_add(1);
            break;
        }
        let Some(cur0) = view.answers else {
            break; // closed mid-chunk
        };
        let cur = cur0.min(projection.rows.len() - 1);
        view.answers = Some(cur);
        match k {
            // Cycle: n/N are the documented keys; j/k and folded arrows too.
            b'n' | b'j' => view.answers = Some((cur + 1) % projection.rows.len()),
            b'N' | b'k' => {
                view.answers = Some((cur + projection.rows.len() - 1) % projection.rows.len())
            }
            // A MINE row toggle/drop; a NEED row is not addressable by either
            // key and both are a silent no-op there (only `a`/digit/Enter/q
            // act on a NEED row).
            b'x' if !view.mine_acting => {
                if let NeedsOverlayRow::Mine(item) = &projection.rows[cur] {
                    view.mine_action = Some(crate::needs_overlay::MineMutation::Toggle(item.n));
                    view.mine_acting = true;
                }
            }
            b'd' if !view.mine_acting => {
                if let NeedsOverlayRow::Mine(item) = &projection.rows[cur] {
                    view.mine_action = Some(crate::needs_overlay::MineMutation::Drop(item.n));
                    view.mine_acting = true;
                }
            }
            b'0'..=b'9' => {
                // A question row: a digit opens the full-context detail
                // overlay with that option preselected, where the answer is
                // reviewed and sent. A NEED row answers as before; a MINE row
                // has no options and beeps below.
                if let Some(q) = projection.rows[cur].question() {
                    view.open_detail_on(&q.id);
                    continue;
                }
                let picked = projection.rows[cur].need().and_then(|sel| {
                    sel.answerable
                        .as_ref()
                        .and_then(|a| {
                            a.options
                                .iter()
                                .find(|o| o.idx.as_bytes().first() == Some(&k))
                                .map(|o| (a, o))
                        })
                        .zip(sel.pane_id)
                });
                match picked {
                    Some(((ans, o), pane)) => {
                        // Only ever the daemon-pinned keystroke; focus unchanged.
                        // The answered pane drops from the queue on the next
                        // scrape tick; the overlay stays open to cycle onward.
                        write_msg(
                            sock_w,
                            &ClientMsg::PaneAnswer {
                                pane,
                                fingerprint: ans.fingerprint,
                                region_lines: ans.region_lines as u16,
                                keystroke: o.keystroke.clone(),
                            },
                        )
                        .await
                        .map_err(|e| format!("answer send failed: {e}"))?;
                    }
                    // A digit with no matching option (a MINE row, or a
                    // non-answerable NEED row, e.g. review-wedged / budget /
                    // focus-only) is a local BEL, never a stray key sent to
                    // any pane (invariant).
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b'\r' | b'\n' => {
                // A question row: Enter opens the full-context detail
                // overlay (the answer gestures live there now, for every
                // kind - options, free text, pins).
                if let Some(q) = projection.rows[cur].question() {
                    view.open_detail_on(&q.id);
                    continue;
                }
                // Goto the row's target: SelectSquad/SelectTab only when
                // they change the view, then FocusPane; a paneless watch-only row
                // attaches; a squadless live fold row - or a MINE row, which
                // owns no pane at all - has no reachable pane here, so it
                // degrades to a notice (Invariant: every item actionable).
                match projection.rows[cur].need() {
                    Some(row) if row.pane_id.is_some() => {
                        let pane = row.pane_id.expect("checked Some above");
                        let switching = row.squad.is_some_and(|s| s != active_squad);
                        if let Some(sq) = row.squad.filter(|_| switching) {
                            write_msg(sock_w, &ClientMsg::Command(Command::SelectSquad(sq)))
                                .await
                                .map_err(|e| format!("command send failed: {e}"))?;
                        }
                        if let Some(tid) = row.tab.filter(|&t| switching || active_tab != Some(t)) {
                            write_msg(sock_w, &ClientMsg::Command(Command::SelectTab(tid)))
                                .await
                                .map_err(|e| format!("command send failed: {e}"))?;
                        }
                        write_msg(sock_w, &ClientMsg::Command(Command::FocusPane(pane)))
                            .await
                            .map_err(|e| format!("command send failed: {e}"))?;
                    }
                    Some(row) if row.attach_id.is_some() => {
                        let id = row.attach_id.as_ref().expect("checked Some above");
                        write_msg(sock_w, &ClientMsg::Command(Command::attach_agent(id)))
                            .await
                            .map_err(|e| format!("command send failed: {e}"))?;
                    }
                    _ => {
                        view.set_notice("no pane here - focus it manually".into());
                    }
                }
                view.answers = None;
                view.needs_gen = view.needs_gen.wrapping_add(1);
            }
            0x1b | b'q' => {
                view.answers = None;
                view.needs_gen = view.needs_gen.wrapping_add(1);
            }
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}
