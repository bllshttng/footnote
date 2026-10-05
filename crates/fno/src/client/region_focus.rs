//! Region input ownership: the one client-local answer to "which visible
//! region takes typing", plus the mouse pre-pass moved out of `client.rs`
//! `handle_stdin` for the shrink-only ratchet. A left press against a
//! visible region (pane border or content, the feed panel, the backlog
//! column) sets the owner; keyboard routing and the focus paint read it.

use super::*;

/// The client-local keyboard owner among the visible regions. `Pane` means
/// the server's focused pane (`layout.focus`) takes typing: the client never
/// stores a pane id here, so server focus identity is preserved and a layout
/// response never races an optimistic local pane. `Feed` and `Board` name
/// the chrome panels; both release back to `Pane` when they close (the read
/// normalizes through [`View::input_owner`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RegionOwner {
    Pane,
    Feed,
    Board,
}

impl View {
    /// The effective owner: the stored click choice, normalized so a closed
    /// or hidden region never keeps the keyboard. Every reader (key routing,
    /// flush, paint) goes through this, so close sites need no bookkeeping.
    /// A full-screen board owns unconditionally: it covers every cell, so no
    /// pane is reachable to hold the keyboard under it.
    pub(crate) fn input_owner(&self) -> RegionOwner {
        if self.messages_board.is_some() {
            return RegionOwner::Board;
        }
        if self.board_full && (self.backlog_board.is_some() || self.org_board.is_some()) {
            return RegionOwner::Board;
        }
        match self.region_owner {
            RegionOwner::Feed if self.feed.is_some() => RegionOwner::Feed,
            RegionOwner::Board
                if (self.backlog_board.is_some()
                    && self.sideline_view == crate::view_store::SidelineView::Backlog)
                    || (self.org_board.is_some()
                        && self.sideline_view == crate::view_store::SidelineView::Org) =>
            {
                RegionOwner::Board
            }
            _ => RegionOwner::Pane,
        }
    }
}

/// The mouse pre-pass: reports route first (see the caller for the chunk
/// split), and a left press against a visible region sets `region_owner`
/// before the caller hands the chunk's remaining bytes to the key scanner,
/// so a click and typing in one chunk land on the clicked region.
pub(super) async fn mouse_pre_pass(
    view: &mut View,
    scanner: &mut Scanner,
    reports: Vec<crate::mouse::MouseReport>,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    for rep in reports {
        // DIAGNOSTIC (header right-click toggle): log every mouse event one stdin
        // chunk produces, so a single operator right-click can be COUNTED. Inert
        // unless FNO_MUX_MOUSE_TRACE is set; drop once the event pair is read.
        // Cached in a OnceLock: a drag or scroll emits dozens of reports a
        // second, and the flag never changes mid-process.
        static MOUSE_TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *MOUSE_TRACE.get_or_init(|| std::env::var_os("FNO_MUX_MOUSE_TRACE").is_some()) {
            eprintln!(
                "mux-mouse kind={:?} row={} col={} shift={}",
                rep.kind, rep.row, rep.col, rep.shift
            );
        }
        // Shift-modified reports are dropped so the terminal's own native
        // selection keeps working. A RELEASE while a drag is in flight is the
        // exception: some terminals report Shift on the release, and dropping it
        // would leave the drag latched - visibly stuck, eating input until its
        // timeout - for a gesture the operator has already finished. Nobody is
        // shift-selecting text mid-drag, so nothing is taken away here.
        // `press_hold` belongs in this list for the same reason and with
        // more at stake: the press it latched DEFERRED a click, so dropping its
        // release does not merely leave state armed - it swallows the action
        // entirely. On a terminal that marks releases with Shift, every plain
        // click on a workspace, section or card row would become a no-op, and
        // the reaper would later open a menu nobody asked for.
        let ends_a_drag = matches!(rep.kind, MouseKind::Release(MouseButton::Left))
            && (view.pane_drag.is_some()
                || view.seam_drag.is_some()
                || view.tab_drag.is_some()
                || view.row_drag.is_some()
                || view.press_hold.is_some());
        // A close-chip gesture is also release-owned state. Let every event in
        // it reach `modal_mouse`, even when the terminal marks it as Shift.
        if rep.shift && !ends_a_drag && !view.modal_release_swallow {
            continue;
        }
        if consume_modal_close_gesture(view, rep.kind) {
            continue;
        }
        // A tap on any painted esc chip is a pressed Esc; its release is
        // swallowed so nothing under the closed overlay sees half a click.
        if matches!(rep.kind, MouseKind::Press(MouseButton::Left))
            && super::esc_close::chip_at(view, rep.row, rep.col)
        {
            view.modal_release_swallow = true;
            if let StdinFlow::Detach = super::esc_close::tap(view, scanner, sock_w).await? {
                return Ok(StdinFlow::Detach);
            }
            continue;
        }
        // A pointer action - click/press/wheel/drag, anything but passive hover
        // (Move) - is "other input": it disarms the resize repeat window exactly
        // as a non-resize keystroke does. Without this, a click that may have
        // refocused a pane could be followed by a bare H/J/K/L that silently
        // resizes (the mouse pre-pass strips reports before the scanner runs).
        // Hover is left armed so mouse drift never breaks a held resize.
        if !matches!(rep.kind, MouseKind::Move) {
            scanner.disarm_repeat();
        }
        // (hover affordance) While a popup or modal owns the pointer, a
        // pointer event reports ITS hover, not a pane cell's: drop the link
        // probe so the underline cannot linger beneath the overlay. Any event
        // counts, not just Move - a right-click that opens a menu or an
        // overlay's own press should clear the underline too, and a family-B
        // overlay (confirm, rename, create, nav) owning the mouse is as much
        // a popup as the three menus.
        if view.keys_modal.is_some()
            || view.row_menu.is_some()
            || view.aux.is_some()
            || view.active_overlay_layout().is_some()
        {
            view.link_hover.clear();
        }
        // The feed's provenance modal, the top surface, owns the pointer:
        // hover selects, a click runs the row's action, off-popup dismisses.
        if view.feed_detail.is_some() {
            feed_detail::mouse(view, rep, sock_w).await?;
            continue;
        }
        // The questions view paints next and owns every cell it covers: off
        // the chip, a click reaches no pane and no sideline row under it.
        if view.question_detail.is_some() {
            continue;
        }
        if view.bell.open {
            if bell::button_at(view, rep.row, rep.col) {
                if matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
                    apply_hit(view, ChromeHit::Bell(bell::Hit::Toggle), sock_w).await?;
                }
                continue;
            }
            if let Some(hit) = bell::hit(view, rep.row, rep.col) {
                if matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
                    apply_hit(view, hit, sock_w).await?;
                }
                continue;
            }
            if !matches!(rep.kind, MouseKind::Move) {
                bell::close(view);
            }
        }
        // US3: while the which-key modal is open, the mouse drives it
        // (hover selects, wheel scrolls, click executes or dismisses) and is
        // SWALLOWED - it never reaches a pane or the chrome underneath.
        if view.keys_modal.is_some() {
            if let StdinFlow::Detach =
                keys_modal::keys_modal_mouse(view, scanner, rep, sock_w).await?
            {
                return Ok(StdinFlow::Detach);
            }
            continue;
        }
        // US2: the row context menu owns the mouse while open (hover
        // selects, click runs, right-press re-anchors) and is swallowed.
        if view.row_menu.is_some() {
            row_menu_mouse(view, rep, sock_w).await?;
            continue;
        }
        // US4/US5: the MENU popup / settings modal owns the mouse.
        if view.aux.is_some() {
            if let StdinFlow::Detach = aux_mouse(view, rep, sock_w).await? {
                return Ok(StdinFlow::Detach);
            }
            continue;
        }
        // Full-screen sideline: the sideline owns every cell, so every
        // report routes to the sideline's own handlers - the dock first,
        // then the hit path a normal-mode sideline click runs (clamped into
        // the panel's column space) - and no byte ever reaches a pane.
        if view.messages_board.is_some() {
            if view.launcher.is_some() && agent_launcher::launcher_mouse(view, rep, sock_w).await? {
                continue;
            }
            if modal_mouse(view, rep) {
                continue;
            }
            view.hover_pending = None;
            view.hover_row = None;
            messages_view::mouse(view, rep, sock_w).await?;
            continue;
        }
        if view.org_board.is_some() && view.board_full {
            if view.launcher.is_some() && agent_launcher::launcher_mouse(view, rep, sock_w).await? {
                continue;
            }
            if modal_mouse(view, rep) {
                continue;
            }
            view.hover_pending = None;
            view.hover_row = None;
            org_board::mouse(view, rep, sock_w).await?;
            continue;
        }
        if view.sideline_full && view.sideline_view == crate::view_store::SidelineView::Agents {
            // The bell lives on the tab bar now (terminal row 0, far right),
            // above the sideline's own rows: answer its seat before the
            // full-surface delegation, which rejects row 0 outright.
            if bell::button_at(view, rep.row, rep.col) {
                if matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
                    apply_hit(view, ChromeHit::Bell(bell::Hit::Toggle), sock_w).await?;
                }
                continue;
            }
            sideline::route_mouse(view, rep, sock_w).await?;
            continue;
        }
        // Full-screen board: the overlay owns every cell, so no press or
        // wheel reaches the panes it covers. Keys stay with the board. A
        // left press on a painted node id opens it (plan, else the node's
        // link, else the details pane) - the sideline card tap's cascade.
        // Under an overlay or a board popup the recorded spans describe
        // cells something else now paints, so the tap resolves nothing.
        if view.backlog_board.is_some() && view.board_full {
            if matches!(rep.kind, MouseKind::Press(MouseButton::Left))
                && view.active_overlay_layout().is_none()
                && !view.backlog_board.as_ref().is_none_or(|b| b.popup_open())
            {
                if let Some(id) = node_link::span_at(rep.row, rep.col) {
                    node_link::open(view, id).await;
                }
            }
            continue;
        }
        // The new-agent composer owns presses that land inside its dock
        // (a click focuses the row it hit; Launch submits). Anything else
        // falls through: the list above and the panes stay live while it
        // is open.
        if view.launcher.is_some() && agent_launcher::launcher_mouse(view, rep, sock_w).await? {
            continue;
        }
        // a seam drag in flight owns the mouse. The pointer routinely
        // leaves the divider it grabbed - that is what dragging is - so this
        // precedes every position-based route below, including the pane forward
        // that would otherwise hand the drag to a PTY as text selection.
        if view.seam_drag.is_some() {
            match rep.kind {
                MouseKind::Drag(MouseButton::Left) => {
                    if let Some(cmd) = view.seam_drag_to(rep.row, rep.col, Instant::now()) {
                        write_msg(sock_w, &ClientMsg::Command(cmd))
                            .await
                            .map_err(|e| format!("seam resize send failed: {e}"))?;
                    }
                    continue;
                }
                MouseKind::Release(MouseButton::Left) => {
                    // The last applied ratio stands (no command travels).
                    view.end_seam_drag(rep.row, rep.col);
                    continue;
                }
                // Anything else (a wheel, another button) means the gesture is
                // over; drop the drag and let the event route normally.
                _ => view.end_seam_drag(rep.row, rep.col),
            }
        }
        // a relocation drag owns the mouse, for the same reason a seam
        // drag does - the pointer's whole job is to leave the pane it grabbed,
        // so this must precede the pane forward that would otherwise feed the
        // gesture to a PTY as a text selection.
        if view.pane_drag.is_some() {
            match rep.kind {
                MouseKind::Drag(MouseButton::Left) => {
                    view.pane_drag_to(rep.row, rep.col, Instant::now());
                    continue;
                }
                MouseKind::Release(MouseButton::Left) => {
                    // Re-hit-test at the RELEASE coordinates rather than trusting
                    // the zone the last motion cached. A co-viewer can move the
                    // targeted seam between that motion and this release, which
                    // leaves the cached zone naming a slot the pointer no longer
                    // sits in - and `set_layout` only clears the cache when the
                    // target disappears, not when it merely moves.
                    view.pane_drag_to(rep.row, rep.col, Instant::now());
                    // Nothing goes on the wire until here: the whole drag is a
                    // client-local preview, so the server only ever learns the
                    // outcome (AC5-EDGE - a cancel sends nothing at all).
                    if let Some(cmd) = view.commit_pane_drag() {
                        write_msg(sock_w, &ClientMsg::Command(cmd))
                            .await
                            .map_err(|e| format!("pane move send failed: {e}"))?;
                    }
                    // Same release-recompute as the seam/sideline drags: clear a
                    // grip accent the drag left on if the pointer ended off it.
                    view.refresh_hover_affordances(rep.row, rep.col);
                    continue;
                }
                _ => {
                    view.cancel_pane_drag();
                    // A non-left termination ends the drag with no Release;
                    // recompute hover so a grip accent the drag left on does not
                    // linger (codex peer review).
                    view.refresh_hover_affordances(rep.row, rep.col);
                }
            }
        }
        // (G2) a tab-cell join drag owns the mouse, same ownership rule as
        // a pane drag: the pointer's whole job is to leave the strip it grabbed.
        if view.tab_drag.is_some() {
            match rep.kind {
                MouseKind::Drag(MouseButton::Left) => {
                    view.tab_drag_to(rep.row, rep.col, Instant::now());
                    // Real motion disqualifies the long-press. Set
                    // here, not in tab_drag_to: the Release arm calls it too
                    // (zone recompute at release coords) and a hold that never
                    // moved must stay a hold.
                    if let Some(d) = view.tab_drag.as_mut() {
                        d.moved = true;
                    }
                    continue;
                }
                MouseKind::Release(MouseButton::Left) => {
                    view.tab_drag_to(rep.row, rep.col, Instant::now());
                    let held = view.tab_drag.map(|d| (d.src_tab, d.start_at, d.moved));
                    // A motionless hold past MENU_LONG_PRESS consumes
                    // the release BEFORE any commit: `moved` gates it (the
                    // clock alone cannot tell a hold from a slow drag), and a
                    // terminal that drops drag reports can place the release
                    // coords on a drop zone - a hold must never execute a join
                    // (codex peer review on #975). Under a usurping overlay
                    // (rename typing) the hold degrades to the plain flow
                    // below - a menu there would steal the overlay's keys.
                    // Open the CAPTURED tab's menu, not whatever cell the
                    // release reports: with no drag report ever arriving, the
                    // release coords are the one unchecked signal left.
                    let long_press = !view.menu_usurping_open()
                        && held.is_some_and(|(_, start, moved)| held_long_enough(start, moved));
                    if long_press {
                        let opened = held.is_some_and(|(tid, _, _)| {
                            view.open_tab_menu_by_id(
                                tid,
                                Anchor::At {
                                    row: rep.row,
                                    col: rep.col,
                                },
                            )
                        });
                        // The held tab closed mid-hold (e.g. a co-attached
                        // client or server-driven layout change) - say so
                        // rather than let the hold end in silence, mirroring
                        // the row arm's "no menu on the held row" notice.
                        if !opened {
                            view.set_notice("no menu on the held tab".into());
                        }
                        view.tab_drag = None;
                        view.refresh_hover_affordances(rep.row, rep.col);
                        continue;
                    }
                    match view.commit_tab_drag() {
                        Some(cmd) => {
                            write_msg(sock_w, &ClientMsg::Command(cmd))
                                .await
                                .map_err(|e| format!("tab join send failed: {e}"))?;
                        }
                        // A zone-less release still ON the strip is a plain click:
                        // select the tab (the click-to-select affordance the strip
                        // has always had). Released off the strip it is a cancelled
                        // drag - nothing travels.
                        None => {
                            if let Some((tid, _, _)) = held {
                                if view.strip_at(rep.row, rep.col) {
                                    write_msg(sock_w, &ClientMsg::Command(Command::SelectTab(tid)))
                                        .await
                                        .map_err(|e| format!("tab select send failed: {e}"))?;
                                }
                            }
                        }
                    }
                    view.refresh_hover_affordances(rep.row, rep.col);
                    continue;
                }
                _ => {
                    view.cancel_tab_drag();
                    view.refresh_hover_affordances(rep.row, rep.col);
                }
            }
        }
        // (G3) a sideline-row placement drag owns the mouse.
        if view.row_drag.is_some() {
            match rep.kind {
                MouseKind::Drag(MouseButton::Left) => {
                    view.row_drag_to(rep.row, rep.col, Instant::now());
                    // Real motion disqualifies the long-press. Set
                    // here, not in row_drag_to: the Release arm calls it too
                    // (zone recompute at release coords) and a hold that never
                    // moved must stay a hold.
                    if let Some(d) = view.row_drag.as_mut() {
                        d.moved = true;
                    }
                    continue;
                }
                MouseKind::Release(MouseButton::Left) => {
                    view.row_drag_to(rep.row, rep.col, Instant::now());
                    // Capture the pressed row's source BEFORE commit consumes the
                    // drag, so a zone-less release can verify it landed back on the
                    // SAME row.
                    let pressed = view.row_drag.as_ref().map(|d| d.src.clone());
                    let held = view.row_drag.as_ref().map(|d| (d.start_at, d.moved));
                    let still_on_row =
                        pressed.is_some() && view.row_drag_source_at(rep.row, rep.col) == pressed;
                    // A motionless hold past MENU_LONG_PRESS consumes the
                    // release BEFORE any commit, exactly like the tab arm: a
                    // terminal that drops drag reports can place the release
                    // coords on a drop zone, and a hold must never execute a
                    // placement (codex peer review on #975). long_press is a
                    // TIME question, not a position one - it must not be gated
                    // on still_on_row, or a release that slips off the pressed
                    // row during a genuine motionless hold ends in total
                    // silence. A hold that opens nothing still SAYS so. Under a
                    // usurping overlay the hold degrades to the plain flow
                    // below.
                    let long_press = !view.menu_usurping_open()
                        && held.is_some_and(|(start, moved)| held_long_enough(start, moved));
                    if long_press {
                        let opened = still_on_row
                            && view.sideline_row_at(rep.row, rep.col).is_some_and(|i| {
                                view.open_row_menu(
                                    i,
                                    Anchor::At {
                                        row: rep.row,
                                        col: rep.col,
                                    },
                                )
                            });
                        if !opened {
                            view.set_notice("no menu on the held row".into());
                        }
                        view.row_drag = None;
                        view.refresh_hover_affordances(rep.row, rep.col);
                        continue;
                    }
                    match view.commit_row_drag() {
                        Some(cmd) => {
                            write_msg(sock_w, &ClientMsg::Command(cmd))
                                .await
                                .map_err(|e| format!("row place send failed: {e}"))?;
                        }
                        // A zone-less release is a plain click ONLY when the
                        // pointer is still on the row it was pressed on: run that
                        // row's own action (focus / attach), unchanged from a
                        // press-click. A slip to a different row - or a layout
                        // shift under a held button, or a release over pinned
                        // chrome (row_drag_source_at skips the density button) -
                        // resolves to a different source (or None), so the gesture
                        // cancels rather than acting on the wrong agent.
                        None => {
                            if still_on_row {
                                if let Some(hit) = view.chrome_hit(rep.row, rep.col) {
                                    apply_hit(view, hit, sock_w).await?;
                                }
                            }
                        }
                    }
                    view.refresh_hover_affordances(rep.row, rep.col);
                    continue;
                }
                _ => {
                    view.cancel_row_drag();
                    view.refresh_hover_affordances(rep.row, rep.col);
                }
            }
        }
        // A press held on a menu-bearing sideline row that is not a drag
        // source. The drag arms above own their own releases; this owns the rest,
        // so a workspace row answers a hold the way an agent row does.
        if view.press_hold.is_some() {
            match rep.kind {
                MouseKind::Release(MouseButton::Left) => {
                    let held = view.press_hold.take();
                    // A TIME question, not a position one, matching the row-drag
                    // arm: a release that slips off the pressed row during a
                    // genuine motionless hold must not end in silence. The menu
                    // opens on the row the press LANDED on, never on whatever
                    // the release reports, so a slip can never act on a
                    // neighbour. Under a usurping overlay the hold degrades to
                    // the plain click below - a menu there would steal the
                    // overlay's keys.
                    // Fail closed unless the pressed row is STILL that row. A
                    // layout push during the hold rebuilds `display_rows()`, so
                    // a row that vanished slides its neighbour under the same
                    // index and the menu would open on a worker nobody pressed
                    // - Stop and Remove aimed at the wrong agent. Re-checking
                    // the identity is what `still_on_row` is for the drag arm.
                    let same_row = held
                        .as_ref()
                        .is_some_and(|(i, id, _)| view.row_identity(*i).as_ref() == Some(id));
                    let long_press = same_row
                        && !view.menu_usurping_open()
                        && held
                            .as_ref()
                            .is_some_and(|(_, _, start)| held_long_enough(*start, false));
                    if long_press {
                        let opened = held.as_ref().is_some_and(|(i, _, _)| {
                            view.open_row_menu(
                                *i,
                                Anchor::At {
                                    row: rep.row,
                                    col: rep.col,
                                },
                            )
                        });
                        // A row whose menu `open_row_menu` declines (an inert
                        // label) still SAYS so - the same
                        // notice the row-drag arm emits, for the same reason.
                        if !opened {
                            view.set_notice("no menu on the held row".into());
                        }
                        view.refresh_hover_affordances(rep.row, rep.col);
                        continue;
                    }
                    // Too short to be a hold: it was a click, so run the action
                    // the press deferred.
                    //
                    // Two gates, because `chrome_hit` resolves at the RELEASE
                    // coordinates and the identity check only vouches for the
                    // PRESSED index. A release that slipped to another row - no
                    // intervening Drag report is required, the row-drag arm
                    // above assumes terminals that omit them - would otherwise
                    // run the OTHER row's action: a different workspace
                    // selected, a different card's dispatch confirm armed. So
                    // the release must still land on the row that was pressed,
                    // which is `still_on_row` in the drag arm's vocabulary.
                    // Position matters here even though it must not gate the
                    // MENU: opening a menu on the pressed row is unambiguous,
                    // acting on a row nobody pressed is not.
                    let still_on_row = held.as_ref().is_some_and(|(i, _, _)| {
                        view.press_hold_row_at(rep.row, rep.col).map(|(j, _)| j) == Some(*i)
                    });
                    if same_row && still_on_row {
                        if let Some(hit) = view.chrome_hit(rep.row, rep.col) {
                            apply_hit(view, hit, sock_w).await?;
                        }
                    }
                    view.refresh_hover_affordances(rep.row, rep.col);
                    continue;
                }
                // Real motion under the held button disqualifies the hold, and
                // any other termination drops it. Neither runs the deferred
                // click: a gesture that turned into something else is not one.
                MouseKind::Drag(MouseButton::Left) => {
                    view.press_hold = None;
                }
                MouseKind::Move => {}
                _ => view.press_hold = None,
            }
        }
        // the sideline border drag, same ownership rule as a seam drag.
        // Client-local: the sideline is never on the wire, so a width change only
        // tells the server its content area changed (: a free width now,
        // reported per crossed column so inner apps reflow live).
        if view.sideline_drag.is_some() {
            match rep.kind {
                MouseKind::Drag(MouseButton::Left) => {
                    if view.drag_sideline_to(rep.col, Instant::now()) {
                        let (r, c) = view.content_dims();
                        write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                            .await
                            .map_err(|e| format!("sideline resize send failed: {e}"))?;
                    }
                    continue;
                }
                MouseKind::Release(MouseButton::Left) => {
                    view.end_sideline_drag(rep.row, rep.col);
                    continue;
                }
                // A non-left termination (a wheel, another button) ends the drag.
                _ => view.end_sideline_drag(rep.row, rep.col),
            }
        }
        if feed_view::drag_mouse(view, rep.row, rep.col, rep.kind, sock_w).await? {
            continue;
        }
        // Name and confirmation overlays share the same framed layout and own
        // every pointer event, including clicks outside their block.
        if modal_mouse(view, rep) {
            continue;
        }
        // Bare motion is hover: record the sideline highlight + the
        // focus-follows-mouse settle target, and swallow it - a Move is never
        // forwarded to a pane. The actual FocusPane is committed by the select
        // loop's settle timer (a rested pointer emits no further motion event).
        if view.org_board.is_some()
            && !view.board_full
            && view.sideline_view == crate::view_store::SidelineView::Org
            && (rep.col as usize) + 1 < view.panel_w() as usize
            && !(rep.row as usize == view.term.0 as usize - 1 && view.bottom_row_is_chrome())
        {
            view.hover_pending = None;
            view.hover_row = None;
            if rep.row == 0 {
                // The strip row: only its words act (R15); the rest of the
                // row is dead.
                if bell::button_range(view).contains(&(rep.col as usize)) {
                    apply_hit(view, ChromeHit::Bell(bell::Hit::Toggle), sock_w).await?;
                }
                for (start, w, v) in view.top_row_spans() {
                    if (rep.col as usize) >= start && (rep.col as usize) < start + w {
                        apply_hit(view, ChromeHit::TopRow(v), sock_w).await?;
                    }
                }
                continue;
            }
            let rep = crate::mouse::MouseReport {
                row: rep.row - 1,
                ..rep
            };
            org_board::mouse(view, rep, sock_w).await?;
            continue;
        }
        if matches!(rep.kind, MouseKind::Move) {
            view.on_hover(rep.row, rep.col, Instant::now());
            continue;
        }
        // A left click on chrome (tab bar / sideline) switches tab/squad, focuses
        // an agent's pane, opens a tab, or opens a card-dispatch confirm - it
        // never reaches the pane underneath.
        if matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
            // Explicit region focus precedes row actions: a press inside
            // the feed panel puts the feed on the keyboard (a row keeps its
            // provenance action; chrome and blank cells just focus).
            if view.feed.is_some() {
                let feed_w = view.feed_panel_w() as usize;
                let over_chrome_row =
                    rep.row as usize == view.term.0 as usize - 1 && view.bottom_row_is_chrome();
                if feed_w > 0
                    && rep.col as usize > view.term.1 as usize - feed_w
                    && !over_chrome_row
                {
                    view.region_owner = RegionOwner::Feed;
                    // The footer's `↑ N new` marker acts before row
                    // resolution: a click there jumps home, it never opens a
                    // row underneath (no new ChromeHit variant).
                    if view.feed_new_marker_hit(rep.row, rep.col) {
                        view.feed_home();
                        continue;
                    }
                    if let Some(hit) = view.chrome_hit_feed(rep.row, rep.col) {
                        apply_hit(view, hit, sock_w).await?;
                    }
                    continue;
                }
            }
            // The windowed board column is the board's surface: focus it and
            // resolve nothing else there - a stale agents row underneath must
            // never act (no phantom drags, no phantom row actions). A press
            // on a painted node id opens it, the sideline card tap's
            // cascade; under a board popup the tap resolves nothing.
            if view.backlog_board.is_some()
                && !view.board_full
                && view.sideline_view == crate::view_store::SidelineView::Backlog
                && (rep.col as usize) + 1 < view.panel_w() as usize
                && !(rep.row as usize == view.term.0 as usize - 1 && view.bottom_row_is_chrome())
            {
                view.region_owner = RegionOwner::Board;
                if !view.backlog_board.as_ref().is_none_or(|b| b.popup_open()) {
                    if let Some(id) = node_link::span_at(rep.row, rep.col) {
                        node_link::open(view, id).await;
                    }
                }
                continue;
            }
            // (G2/G3) a tab cell and a sideline agent row are DRAG SOURCES.
            // Begin the drag before chrome_hit (which would apply the click action
            // immediately); a zone-less release falls back to that same click
            // action (select / focus / attach), so a plain click is unchanged.
            if let Some(tid) = view.tab_cell_at(rep.row, rep.col) {
                view.begin_tab_drag(tid, Instant::now());
                continue;
            }
            if let Some(src) = view.row_drag_source_at(rep.row, rep.col) {
                view.begin_row_drag(src, Instant::now());
                continue;
            }
            // A sideline row that is NOT a drag source still has a menu
            // to hold for - a workspace name row is the motivating case. Arm the
            // hold clock and DEFER the click: the release decides between the
            // menu and `chrome_hit`'s own action, exactly as the drag arm defers
            // a zone-less release to that same action. Before `chrome_hit`, for
            // the same reason `begin_row_drag` is: applying the click here would
            // spend the press before the hold could be measured.
            if let Some((i, id)) = view.press_hold_row_at(rep.row, rep.col) {
                view.press_hold = Some((i, id, Instant::now()));
                continue;
            }
            if let Some(hit) = view.chrome_hit(rep.row, rep.col) {
                apply_hit(view, hit, sock_w).await?;
                continue;
            }
            // a press on a pane's grip starts a relocation. Before the
            // seam check only for readability - grips sit on cells a pane
            // covers and seams only on cells no pane covers, so the two can
            // never contend for the same press.
            if let Some(mover) = view.grip_at(rep.row, rep.col) {
                view.begin_pane_drag(mover, Instant::now());
                continue;
            }
            // a press on a divider grabs the seam. After chrome_hit so
            // sideline and tab-bar affordances still win their own cells.
            if let Some(seam) = view.seam_at(rep.row, rep.col) {
                view.begin_seam_drag(seam, Instant::now());
                continue;
            }
            // a press on a framed pane's border ring (name tab included)
            // focuses that pane and reaches nothing else. After the grip and
            // seam checks so a drag affordance always wins its own cells
            // (AC9-EDGE); before the forward, so the frame never forwards.
            if let Some(pid) = view.border_pane_at(rep.row, rep.col) {
                // A border click is an explicit pane focus: the client-local
                // owner moves with the server one, and a hover pending its
                // settle dies here - the click is the operator's decision,
                // and a stale pointer landing must not override it.
                view.region_owner = RegionOwner::Pane;
                view.hover_pending = None;
                write_msg(sock_w, &ClientMsg::Command(Command::FocusPane(pid)))
                    .await
                    .map_err(|e| format!("focus send failed: {e}"))?;
                continue;
            }
            // Likewise the sideline's own border. Also after chrome_hit, so the
            // density button keeps the cells it draws on. Remember the width at
            // grab so a bare Esc reverts, and stamp `last_at` for the stuck-drag
            // timeout.
            if view.on_sideline_border(rep.row, rep.col) {
                view.sideline_drag = Some(SidelineDrag {
                    start_width: view.sideline_width,
                    last_at: Instant::now(),
                });
                continue;
            }
            if view.begin_border_drag(rep.row, rep.col) {
                continue;
            }
        }
        // US2: right-click a sideline row opens its context menu (agent
        // rows) or is swallowed (non-agent chrome). A right-click on a PANE cell
        // (sideline_row_at -> None) falls through and forwards to the inner app,
        // so pane right-click behavior is untouched (AC3-EDGE).
        // Menu paths are blocked under overlays they would USURP -
        // text inputs (including nav's typed filter) and interactive modals
        // (rename's Enter would run a menu action; the key router checks
        // row_menu first). The read-only peek overlay deliberately does not
        // block the row/tab paths: a right-press on a row opened its menu
        // over an open peek before this diff, and the open path clears peek
        // itself. The pane path keeps the full
        // overlay_open guard: a pane press under ANY overlay always fell
        // through to the pane, and it still does.
        if matches!(rep.kind, MouseKind::Press(MouseButton::Right)) && !view.menu_usurping_open() {
            // (5.1) A tab cell opens the tab menu, resolved through the
            // same tab_cell_at the drag pickup uses. Checked first to mirror
            // the left-press ordering; the strip and the sideline own disjoint
            // columns, so the two tests can never contend for one cell.
            if view.open_tab_menu(
                rep.row,
                rep.col,
                Anchor::At {
                    row: rep.row,
                    col: rep.col,
                },
            ) {
                continue;
            }
            if let Some(i) = view.sideline_row_at(rep.row, rep.col) {
                // Swallow the press only when a menu actually opened: a header
                // with no menu leaves the press to fall through instead of
                // eating it silently, so the right-click never reads as a
                // no-op that a following Left press then turns into a
                // collapse toggle.
                if view.open_row_menu(
                    i,
                    Anchor::At {
                        row: rep.row,
                        col: rep.col,
                    },
                ) {
                    continue;
                }
            }
            // A right-press on a PANE cell opens the owning agent's
            // row menu - the same menu its sideline row opens - so a pane is a
            // menu-bearing surface too. Same swallow-only-when-opened rule: an
            // agent-less pane falls through to the forward below, keeping the
            // inner app's own right-click (AC3-EDGE).
            if !view.overlay_open() && view.open_pane_menu(rep.row, rep.col) {
                continue;
            }
        }
        // Wheel over the sideline scrolls the workspace/session list (there is no
        // pane there to forward to); a wheel over the content area falls through
        // to the pane below, unchanged.
        if matches!(rep.kind, MouseKind::WheelUp | MouseKind::WheelDown) {
            let panel_w = view.panel_w();
            if panel_w > 0 && rep.col < panel_w {
                view.scroll_sideline(matches!(rep.kind, MouseKind::WheelDown));
                continue;
            }
            // The feed panel's columns: scroll its window, never a pane.
            let feed_w = view.feed_panel_w();
            if feed_w > 0 && rep.col >= view.term.1 - feed_w {
                view.scroll_feed(matches!(rep.kind, MouseKind::WheelDown));
                continue;
            }
        }
        if let Some((pane, prow, pcol)) = view.hit_test(rep.row, rep.col) {
            // A content click is an explicit pane focus: only the press moves
            // the owner (motion during a selection never steals typing), and
            // the press refocuses the server the way a border click does, so
            // the clicked pane receives typing without waiting on the
            // hover-settle the human path usually supplies first.
            if matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
                view.region_owner = RegionOwner::Pane;
                view.hover_pending = None;
                write_msg(sock_w, &ClientMsg::Command(Command::FocusPane(pane)))
                    .await
                    .map_err(|e| format!("focus send failed: {e}"))?;
            }
            write_msg(
                sock_w,
                &ClientMsg::Mouse {
                    pane,
                    event: MouseEvent {
                        row: prow,
                        col: pcol,
                        kind: rep.kind,
                    },
                },
            )
            .await
            .map_err(|e| format!("mouse send failed: {e}"))?;
        }
    }
    Ok(StdinFlow::Continue)
}
