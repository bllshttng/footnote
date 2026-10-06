//! The per-row context menu : the per-state entry builder for one
//! sideline row. Split out of client.rs so the over-budget file shrinks;
//! everything here reaches the client's private items through `super::*`.

use super::*;

/// An entry whose action has an IN-MENU accelerator: the hint is the
/// live glyph from the menu scope (`keys::menu_key_for`), never a prefix chord
/// - the open menu does not run prefix chords, so advertising one describes an
/// input path the reader is not on. An unscoped id resolves to nothing, which
/// is the honest hint (LD9 / AC8).
fn entry_acc(glyph: &str, label: &str, id: &str) -> PopupRow {
    PopupRow::Entry {
        glyph: glyph.into(),
        label: label.into(),
        hint: crate::keys::menu_key_for(id).unwrap_or_default(),
        enabled: true,
    }
}

/// Build the per-state row menu for the agent at `display_rows()` index `i`,
/// anchored at `anchor`. `None` for a non-agent row (the menu is agent-only).
/// Entry sets mirror the row's state so no dead item ever renders: a paneless
/// bg row gets the new-tab + 2x2 split grid (its whole point); a pane row gets
/// focus plus the move/break-out grid that relocates its live pane; an exited
/// row gets remove; peek/stop apply where they make sense.
pub(super) fn build_row_menu(agent: &AgentRow, anchor: Anchor) -> RowMenu {
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<MenuAction> = Vec::new();
    let mut add = |mut row: PopupRow, acts: &[MenuAction]| {
        if let (PopupRow::Entry { hint, .. }, [action]) = (&mut row, acts) {
            *hint = action
                .accelerator_id()
                .and_then(crate::keys::menu_key_for)
                .unwrap_or_default();
        }
        rows.push(row);
        actions.extend_from_slice(acts);
    };
    let entry = |glyph: &str, label: &str| PopupRow::Entry {
        glyph: glyph.into(),
        label: label.into(),
        hint: String::new(),
        enabled: true,
    };
    let cell = |glyph: &str, label: &str| GridCell {
        glyph: glyph.into(),
        label: label.into(),
    };
    add(PopupRow::Header(agent.name.clone()), &[]);
    add(PopupRow::Rule, &[]);
    if agent.exited {
        add(entry("✕", "Remove"), &[MenuAction::Remove]);
        add(entry("◉", "Peek"), &[MenuAction::Peek]);
        // Resume above the rule (AC7): the menu twin of peek `r`, on the row
        // state `r` accepts - an exited row.
        add(entry("↻", "Resume"), &[MenuAction::Resume]);
    } else if agent.pane_id.is_some() {
        // Live pane row: already placed, so re-placement is a MOVE of the live
        // pane, never an attach. Same 2x2 grid geometry the paneless branch uses
        // below, so the two menus read as one system; the verbs differ because
        // the operations do (move a running pane vs. place a new one).
        add(entry("→", "Focus"), &[MenuAction::Focus]);
        add(entry("◫", "Open in portal..."), &[MenuAction::PortalPicker]);
        add(entry("◉", "Peek"), &[MenuAction::Peek]);
        add(entry("✉", "Mail"), &[MenuAction::Mail]);
        add(PopupRow::Rule, &[]);
        add(
            PopupRow::FullWidth("▭ New Tab".into()),
            &[MenuAction::BreakOut],
        );
        add(entry("⇱", "Detach pane"), &[MenuAction::Detach]);
        // Ungated by pane count. A row whose pane is on screen and has no
        // neighbour `dir`-ward gets the server's "no pane in that direction"
        // notice, the same fail-closed feedback the paneless branch relies on;
        // a row whose pane is off screen always has somewhere to land (the
        // current view), so gating on the source tab would be wrong anyway.
        add(
            PopupRow::Grid(vec![cell("◧", "Move Left"), cell("◨", "Move Right")]),
            &[
                MenuAction::MoveDir(Dir::Left),
                MenuAction::MoveDir(Dir::Right),
            ],
        );
        add(
            PopupRow::Grid(vec![cell("⬒", "Move Up"), cell("⬓", "Move Down")]),
            &[MenuAction::MoveDir(Dir::Up), MenuAction::MoveDir(Dir::Down)],
        );
        // The viewer verb sits apart from the thread verbs (Stop and
        // Remove end the thread): closing the portal must not
        // touch the row. A bare stand-in seat row (portal set, pane now
        // gone) takes the same arm, so its menu offers Close portal too.
        if agent.portal.is_some() {
            add(
                entry_acc("⊟", "Close portal", "close-portal"),
                &[MenuAction::ClosePortal],
            );
        }
        add(PopupRow::Rule, &[]);
        add(entry("■", "Stop"), &[MenuAction::Stop]);
        add(entry("✕", "Remove"), &[MenuAction::Remove]);
    } else if agent.attach_id.is_some() {
        // Paneless bg row: the motivating case - open as a tab or a split pane.
        // Open-here leads (repoint the focused viewer). The client can't know viewer-ness, so the
        // server's fail-closed notice is the feedback path when the focus isn't a detachable viewer.
        add(entry("⊙", "Open Here"), &[MenuAction::OpenHere]);
        add(
            PopupRow::FullWidth("▭ New Tab".into()),
            &[MenuAction::NewTab],
        );
        add(entry("◫", "Open in portal..."), &[MenuAction::PortalPicker]);
        add(PopupRow::Rule, &[]);
        // 2x2 spatial grid: Left/Right on top, Up/Down below (the cell you pick
        // IS the direction). Glyphs are half-block squares; a non-nerd-font
        // terminal still shows the label beside them.
        add(
            PopupRow::Grid(vec![cell("◧", "Split Left"), cell("◨", "Split Right")]),
            &[MenuAction::Split(Dir::Left), MenuAction::Split(Dir::Right)],
        );
        add(
            PopupRow::Grid(vec![cell("⬒", "Split Up"), cell("⬓", "Split Down")]),
            &[MenuAction::Split(Dir::Up), MenuAction::Split(Dir::Down)],
        );
        add(PopupRow::Rule, &[]);
        add(entry("◉", "Peek"), &[MenuAction::Peek]);
        add(entry("✉", "Mail"), &[MenuAction::Mail]);
        add(entry("■", "Stop"), &[MenuAction::Stop]);
        add(entry("✕", "Remove"), &[MenuAction::Remove]);
    } else {
        // A live row that is neither pane-hosted nor attachable here.
        add(entry("◉", "Peek"), &[MenuAction::Peek]);
        add(entry("✉", "Mail"), &[MenuAction::Mail]);
        add(entry("◫", "Open in portal..."), &[MenuAction::PortalPicker]);
        if agent.no_pane_reason == Some(AgentNoPaneReason::LivePaneless) {
            add(entry("↩", "Reattach"), &[MenuAction::Reattach]);
        }
        add(entry("■", "Stop"), &[MenuAction::Stop]);
        add(entry("✕", "Remove"), &[MenuAction::Remove]);
    }
    // Diff is common to every row state: it reads the row's worktree,
    // which an exited or paneless row has just as much as a live pane-hosted
    // one - and a finished worker's diff is the one you most want to read.
    // Bound in menu scope now, so its hint is the live key.
    add(PopupRow::Rule, &[]);
    add(entry("±", "Diff"), &[MenuAction::Diff]);
    // The hold lift is common to every row state too: the mark rides the
    // row, not its pane, so a worker wearing [DND] or [HELD] can sit in any
    // of them. Only a row wearing one offers the release - no dead entry
    // ever renders, the same rule the state branches above follow.
    if agent.dnd {
        add(entry("⤴", "Release hold"), &[MenuAction::ReleaseHold]);
    }
    // Live AND exited rows are renamable; an EXTERNAL row is claude-owned.
    if !agent.external {
        add(entry("✎", "Rename"), &[MenuAction::RenameAgent]);
    }
    RowMenu {
        popup: Popup::new(rows, anchor),
        target: MenuTarget::Agent(AgentIdent::of(agent)),
        actions,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::focus_agent;
    use super::*;

    fn portal_row(portal: Option<u8>) -> AgentRow {
        let mut a = focus_agent(3);
        a.portal = portal;
        a.name = "watched-row".into();
        a
    }

    #[test]
    fn a_portal_row_offers_close_portal_with_the_c_hint() {
        // AC8-HP. A row shown through a portal lists `Close portal` with the
        // `c` hint and the ClosePortal action; the hint is the live menu byte.
        let menu = build_row_menu(&portal_row(Some(1)), test_anchor());
        let entry = menu
            .popup
            .rows
            .iter()
            .find(|r| matches!(r, PopupRow::Entry { label, .. } if label == "Close portal"))
            .expect("the portal row offers Close portal");
        if let PopupRow::Entry { hint, enabled, .. } = entry {
            assert_eq!(hint, "c", "the advertised key is the live menu byte");
            assert!(*enabled, "the entry is runnable");
        } else {
            panic!("expected a PopupRow::Entry");
        }
    }

    #[test]
    fn a_non_portal_pane_row_lists_no_close_portal() {
        // AC8-HP inverse. A pane row with portal: None lists no Close portal
        // entry: the verb is the portal's, not the pane's.
        let menu = build_row_menu(&portal_row(None), test_anchor());
        assert!(
            !menu.popup.rows.iter().any(|r| matches!(
                r,
                PopupRow::Entry { label, .. } if label == "Close portal"
            )),
            "no Close portal entry without a portal"
        );
    }

    fn test_anchor() -> Anchor {
        Anchor::At { row: 4, col: 4 }
    }

    #[test]
    fn a_row_carrying_a_hold_mark_offers_release_hold() {
        // The menu shows [DND] and [HELD] but offered no lift; a row wearing
        // either mark offers Release hold, and an unmarked row does not.
        let mut held = focus_agent(3);
        held.dnd = true;
        held.held_conversation = true;
        let menu = build_row_menu(&held, test_anchor());
        assert!(
            menu.popup.rows.iter().any(|r| matches!(
                r,
                PopupRow::Entry { label, .. } if label == "Release hold"
            )),
            "a held row offers Release hold"
        );
        let plain = focus_agent(3);
        let menu = build_row_menu(&plain, test_anchor());
        assert!(
            !menu.popup.rows.iter().any(|r| matches!(
                r,
                PopupRow::Entry { label, .. } if label == "Release hold"
            )),
            "no Release hold entry without a hold mark"
        );
    }

    #[test]
    fn a_live_row_menu_offers_open_in_portal_and_an_exited_one_does_not() {
        // The portal CHOICE lives where placement lives: both live row
        // shapes offer the picker entry; an exited row (nothing left to
        // show in a portal) does not.
        let menu = build_row_menu(&portal_row(None), test_anchor());
        assert!(
            menu.popup.rows.iter().any(|r| matches!(
                r,
                PopupRow::Entry { label, .. } if label == "Open in portal..."
            )),
            "a live pane row offers the portal picker"
        );
        let mut paneless = focus_agent(3);
        paneless.pane_id = None;
        paneless.attach_id = Some("deadbee1".into());
        let menu = build_row_menu(&paneless, test_anchor());
        assert!(
            menu.popup.rows.iter().any(|r| matches!(
                r,
                PopupRow::Entry { label, .. } if label == "Open in portal..."
            )),
            "a live paneless row offers the portal picker"
        );
        let mut dead = focus_agent(3);
        dead.exited = true;
        let menu = build_row_menu(&dead, test_anchor());
        assert!(
            !menu.popup.rows.iter().any(|r| matches!(
                r,
                PopupRow::Entry { label, .. } if label == "Open in portal..."
            )),
            "an exited row offers no portal picker"
        );
    }
}

// (5.1) The tab-strip context menu builder, moved under the file-budget
// gate; the split/join face gate lives in the `viewed` arm.
/// (5.1) The tab-strip context menu for one tab cell, resolved through
/// the SAME `tab_cell_at` the drag pickup uses (LD-A: one hit test per
/// surface, so a drag and a click can never disagree about where a tab is).
/// Every item binds an existing wire command; nothing here needs a server
/// change, because a tab-bar cell sits in no pane rect and was never
/// forwarded. Destructive items sit last, after a `Rule`.
///
/// Save/apply layout are deliberately ABSENT: `ControlVerb::LayoutGet` /
/// `LayoutApply` ride one-shot `ClientMsg::Control` connections (`fno mux
/// pane ...`), which an attached TUI client cannot send, so a menu item for
/// them would bind to a verb this socket can never carry. That needs a
/// `Command` surface and is filed rather than faked.
pub(super) fn build_tab_menu(idx: usize, tab: &TabMeta, anchor: Anchor, viewed: bool) -> RowMenu {
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<MenuAction> = Vec::new();
    let mut add = |row: PopupRow, acts: &[MenuAction]| {
        rows.push(row);
        actions.extend_from_slice(acts);
    };
    let cell = |glyph: &str, label: &str| GridCell {
        glyph: glyph.into(),
        label: label.into(),
    };
    // The split grid and the join grid are the two faces of one placement
    // decision: the picked cell IS the side of the focused pane the new or
    // joined pane lands on. The menu's own tab picks the face - Split on the
    // viewed tab (joining a tab into itself never made sense), Join on any
    // other. No hint: no prefix binding names these (the split family's
    // chords live in the key table, join's gesture is the tab drag), and
    // LD9 forbids a literal chord standing in for one.
    add(
        PopupRow::Header(tab_group_label(
            tab_label_text(&tab.name, idx, tab.named),
            tab.panes.len(),
        )),
        &[],
    );
    add(PopupRow::Rule, &[]);
    // Every tab verb answers a bare in-menu key from the same
    // registry its hint reads: n for New tab, the angle brackets for the
    // reorder pair - app vocabulary beside the prefix chords (prefix+c, and
    // prefix+< / prefix+> mean the same moves from outside the menu).
    add(entry_acc("▭", "New tab", "new-tab"), &[MenuAction::TabNew]);
    add(
        entry_acc("✎", "Rename", "rename-tab"),
        &[MenuAction::TabRename],
    );
    add(
        entry_acc("◧", "Move left", "move-tab-left"),
        &[MenuAction::TabReorder(-1)],
    );
    add(
        entry_acc("◨", "Move right", "move-tab-right"),
        &[MenuAction::TabReorder(1)],
    );
    add(
        entry_acc("⇥", "Move to…", "move-tab-to"),
        &[MenuAction::TabMoveTo],
    );
    if viewed {
        // Same 2x2 grammar as the row menu's split grid: Left/Right on
        // top, Up/Down below (the cell you pick IS the direction).
        add(
            PopupRow::Grid(vec![cell("◧", "Split Left"), cell("◨", "Split Right")]),
            &[
                MenuAction::TabSplit(Dir::Left),
                MenuAction::TabSplit(Dir::Right),
            ],
        );
        add(
            PopupRow::Grid(vec![cell("⬒", "Split Up"), cell("⬓", "Split Down")]),
            &[
                MenuAction::TabSplit(Dir::Up),
                MenuAction::TabSplit(Dir::Down),
            ],
        );
    } else {
        add(
            PopupRow::Grid(vec![cell("◧", "Join Left"), cell("◨", "Join Right")]),
            &[
                MenuAction::TabJoin(Dir::Left),
                MenuAction::TabJoin(Dir::Right),
            ],
        );
        add(
            PopupRow::Grid(vec![cell("⬒", "Join Up"), cell("⬓", "Join Down")]),
            &[MenuAction::TabJoin(Dir::Up), MenuAction::TabJoin(Dir::Down)],
        );
    }
    add(PopupRow::Rule, &[]);
    // `✕ Close`, not `✕ Close tab`: one shape with the row menu's `✕ Remove`,
    // so the two destructive affordances read as one vocabulary. The prefix
    // `&` chord is untouched; in-menu the entry answers the scoped `x`.
    add(
        entry_acc("✕", "Close", "close-tab"),
        &[MenuAction::TabClose],
    );
    RowMenu {
        popup: Popup::new(rows, anchor),
        target: MenuTarget::Tab(tab.id),
        actions,
    }
}

/// The section-header context menu. A workspace section (`squad`
/// present) offers `Rename` - menu parity with selector `r`. `Clear dead` is
/// added only when `dead > 0`; its label count is both what it advertises AND
/// what the commit runs, so the two can never disagree. The caller guarantees
/// at least one of {renamable, `dead > 0`} holds, so the menu is never empty.
pub(super) fn build_section_menu(
    key: SectionKey,
    label: String,
    squad: Option<u64>,
    dead: usize,
    anchor: Anchor,
) -> RowMenu {
    let mut rows = vec![PopupRow::Header(label.clone()), PopupRow::Rule];
    let mut actions: Vec<MenuAction> = Vec::new();
    if squad.is_some() {
        let entry = |glyph: &str, label: &str| PopupRow::Entry {
            glyph: glyph.into(),
            label: label.into(),
            hint: String::new(),
            enabled: true,
        };
        rows.push(entry_acc("✎", "Rename", "rename-workspace"));
        actions.push(MenuAction::Rename);
        rows.push(entry("▲", "Move up"));
        actions.push(MenuAction::MoveSquad(-1));
        rows.push(entry("▼", "Move down"));
        actions.push(MenuAction::MoveSquad(1));
        rows.push(PopupRow::Rule);
        rows.push(entry("✕", "Remove workspace"));
        actions.push(MenuAction::RemoveSquad);
    }
    if dead > 0 {
        rows.push(PopupRow::Entry {
            glyph: "✕".into(),
            label: format!("Clear dead ({dead})"),
            hint: String::new(),
            enabled: true,
        });
        actions.push(MenuAction::ClearDead);
    }
    RowMenu {
        popup: Popup::new(rows, anchor),
        target: MenuTarget::Section { key, label, squad },
        actions,
    }
}

/// Run a row-menu entry (US2) against the LIVE agent row (resolved by the
/// pinned identity). A stale OR ambiguous target is a Notice (AC1-ERR / codex
/// P1), never a misrouted action; every action maps to an existing Command /
/// overlay / confirm (zero proto).
pub(super) async fn execute_row_menu_action(
    view: &mut View,
    action: MenuAction,
    target: MenuTarget,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    // The chooser remembers its picks; the row menu does not. Captured
    // before the target is consumed by resolution below.
    let from_chooser = matches!(target, MenuTarget::OpenSession(_));
    let target = match (target, action) {
        // The section menu's clear-dead action, resolved against the
        // section rather than a single row.
        (MenuTarget::Section { key, label, squad }, MenuAction::ClearDead) => {
            return clear_dead_confirm(view, key, label, squad);
        }
        // A workspace section's Rename opens the same overlay as selector `r`
        //. The id is the section's squad, so a non-workspace header
        // (`squad: None`) can never reach it - it falls to the refuse arm below.
        (
            MenuTarget::Section {
                squad: Some(id), ..
            },
            MenuAction::Rename,
        ) => {
            view.open_rename(RenameTarget::Squad(id));
            return Ok(());
        }
        // A workspace section's Move up/down sends the same `MoveSquad` the
        // selector's `J`/`K` send; the server clamps at the edges silently, so
        // an at-edge click is a no-op exactly like the key.
        (
            MenuTarget::Section {
                squad: Some(sq), ..
            },
            MenuAction::MoveSquad(delta),
        ) => {
            view.sel_follow = Some(sq);
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::MoveSquad { squad: sq, delta }),
            )
            .await
            .map_err(|e| format!("move-workspace send failed: {e}"))?;
            return Ok(());
        }
        // A workspace section's Remove opens the SAME confirm the keyboard
        // path builds - the destructive-action gate, never skipped by a mouse.
        (
            MenuTarget::Section {
                squad: Some(sq), ..
            },
            MenuAction::RemoveSquad,
        ) => {
            let Some(s) = view.layout.squads.iter().find(|s| s.id == sq) else {
                view.set_notice("workspace is no longer here".into());
                return Ok(());
            };
            if view.term.0 < MIN_ROWS_FOR_STATUS {
                view.set_notice("terminal too short for the confirm prompt".into());
                return Ok(());
            }
            view.open_confirm(ConfirmAction {
                action: ConfirmKind::RemoveSquad {
                    squad: sq,
                    panes: s.panes,
                    last: view.layout.squads.len() == 1,
                },
                label: s.name.clone(),
            });
            return Ok(());
        }
        // (5.1) The tab menu: every item re-resolves the pinned tab id
        // against the live layout first, so a tab that closed or moved between
        // open and pick is a notice, never a redirected action.
        (MenuTarget::Tab(_tid), MenuAction::TabNew) => {
            view.note_command_sent(&Command::NewTab);
            write_msg(sock_w, &ClientMsg::Command(Command::NewTab))
                .await
                .map_err(|e| format!("new-tab send failed: {e}"))?;
            return Ok(());
        }
        (MenuTarget::Tab(tid), MenuAction::TabRename) => {
            if view.find_tab(tid).is_none() {
                view.set_notice("tab is no longer here".into());
                return Ok(());
            }
            view.open_rename(RenameTarget::Tab(tid));
            return Ok(());
        }
        (MenuTarget::Tab(tid), MenuAction::TabReorder(delta)) => {
            // The squad is resolved at execute, not carried from the menu: a
            // tab can move workspaces between open and pick, and ReorderTab
            // names both ids explicitly.
            let Some((squad, _, _)) = view.find_tab(tid) else {
                view.set_notice("tab is no longer here".into());
                return Ok(());
            };
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::ReorderTab {
                    squad,
                    tab: tid,
                    delta,
                }),
            )
            .await
            .map_err(|e| format!("reorder-tab send failed: {e}"))?;
            return Ok(());
        }
        (MenuTarget::Tab(tid), MenuAction::TabMoveTo) => {
            // Same execute-time re-resolution as the reorder pair: a tab that
            // closed or moved between open and pick is a notice, never a
            // redirected action.
            if view.find_tab(tid).is_none() {
                view.set_notice("tab is no longer here".into());
                return Ok(());
            }
            view.open_move_to(tid);
            return Ok(());
        }
        (MenuTarget::Tab(tid), MenuAction::TabJoin(dir)) => {
            // Join the whole tab into the VIEWED tab as a split of the focused
            // pane - the menu twin of dragging the tab cell onto a content
            // edge. A join into itself is suppressed client-side (the wire's
            // own rule), so it is named as a refusal rather than sent.
            let Some((_, _, tab)) = view.find_tab(tid) else {
                view.set_notice("tab is no longer here".into());
                return Ok(());
            };
            if tab.panes.iter().any(|p| p.id == view.layout.focus) {
                view.set_notice("cannot join a tab into itself".into());
                return Ok(());
            }
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::JoinTab {
                    src_tab: tid,
                    anchor_pane: view.layout.focus,
                    dir,
                }),
            )
            .await
            .map_err(|e| format!("join-tab send failed: {e}"))?;
            return Ok(());
        }
        (MenuTarget::Tab(tid), MenuAction::TabSplit(dir)) => {
            // Split the viewed tab from its own cell: a SplitDir on the
            // focused pane, the menu twin of the `%` family. The menu only
            // builds Split rows on the viewed tab, so a non-viewed target
            // here is a stale menu; name it rather than guess.
            if view.active_squad_active_tab_id() != Some(tid) {
                view.set_notice("split acts on the viewed tab".into());
                return Ok(());
            }
            if !server_has_splitdir(view.server_proto) {
                view.set_notice(split_skew_notice());
                return Ok(());
            }
            write_msg(sock_w, &ClientMsg::Command(Command::SplitDir(dir)))
                .await
                .map_err(|e| format!("split-tab send failed: {e}"))?;
            return Ok(());
        }
        (MenuTarget::Tab(tid), MenuAction::TabClose) => {
            let Some((_, _, tab)) = view.find_tab(tid) else {
                view.set_notice("tab is no longer here".into());
                return Ok(());
            };
            // A confirm owns the bottom row; a too-short terminal refuses
            // rather than arm an invisible prompt (same gate as stop/remove).
            if view.term.0 < MIN_ROWS_FOR_STATUS {
                view.set_notice("terminal too short for the confirm prompt".into());
                return Ok(());
            }
            view.open_confirm(ConfirmAction {
                action: ConfirmKind::CloseTab { tab: tid },
                label: tab.name.clone(),
            });
            return Ok(());
        }
        // A menu is built for exactly one target kind, so a crossed pair can only
        // come from a bug; refuse rather than guess at a target.
        (MenuTarget::Section { .. }, _)
        | (MenuTarget::Tab(_), _)
        | (_, MenuAction::ClearDead)
        | (_, MenuAction::TabNew)
        | (_, MenuAction::TabRename)
        | (_, MenuAction::TabReorder(_))
        | (_, MenuAction::TabMoveTo)
        | (_, MenuAction::TabJoin(_))
        | (_, MenuAction::TabSplit(_))
        | (_, MenuAction::TabClose) => {
            view.set_notice("action does not apply to this row".into());
            return Ok(());
        }
        (MenuTarget::Agent(a) | MenuTarget::OpenSession(a), _) => a,
    };
    // Fail closed unless the identity resolves to EXACTLY one live row: two rows
    // sharing a name must never let a menu act on the wrong one (codex P1).
    let mut hits = view.layout.agents.iter().filter(|a| target.matches(a));
    let a = match (hits.next(), hits.next()) {
        (Some(a), None) => a.clone(),
        _ => {
            view.set_notice(format!("agent {} is no longer uniquely here", target.name));
            return Ok(());
        }
    };
    // The chooser refuses an exited row at open, but the row can die between
    // open and pick, and every one of its six arms would place a dead
    // session. The same refusal the builder gave, at the execute end.
    if from_chooser && a.exited {
        view.set_notice(format!("{} has exited", a.name));
        return Ok(());
    }
    // The chooser remembers: the pick that just executed is the pre-selected
    // row next time. Only the chooser's own six mappings persist
    // (`pick_of_action` answers None for everything else).
    if from_chooser {
        if let Some(pick) = super::open_chooser::pick_of_action(action) {
            crate::view_store::save_open_target(pick);
        }
    }
    match action {
        MenuAction::OpenHere => {
            let Some(id) = a.attach_id.clone() else {
                view.set_notice("agent is no longer attachable".into());
                return Ok(());
            };
            write_msg(sock_w, &ClientMsg::Command(Command::attach_agent_here(id)))
                .await
                .map_err(|e| format!("attach send failed: {e}"))?;
        }
        MenuAction::NewTab | MenuAction::Split(_) => {
            let Some(id) = a.attach_id.clone() else {
                view.set_notice("agent is no longer attachable".into());
                return Ok(());
            };
            let split = match action {
                MenuAction::Split(d) => Some(d),
                _ => None,
            };
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::AttachAgent {
                    id,
                    placement: PanePlacement {
                        target: PaneTarget::CurrentRoute,
                        split,
                        ..Default::default()
                    },
                }),
            )
            .await
            .map_err(|e| format!("attach send failed: {e}"))?;
        }
        // Where the pane already IS decides what "move it `dir`" can mean, so
        // the destination is chosen on that and nothing else.
        MenuAction::MoveDir(dir) => match a.pane_id {
            Some(pid) => {
                // On screen: step one place `dir`-ward from the pane itself, so
                // leave `target` unset and let the server navigate from the
                // mover - the geometry the keyboard bind uses. Naming the focus
                // here would instead teleport the pane across any panes between
                // them, and whenever it already sits `dir`-ward of the focus the
                // reshape is identical to the current tree, which `move_leaf`
                // reports as an origin drop and the server discards WITHOUT a
                // notice - a menu entry that does nothing and says nothing.
                //
                // Off screen: there is no meaningful in-tab neighbour to step
                // toward, so name the viewed focus and let the server graft the
                // pane into the current view (the cross-tab arm). That is the
                // destination `commit_row_drag` names from its drop zone.
                let on_screen = view.layout.panes.iter().any(|(id, _)| *id == pid);
                let target = (!on_screen).then_some(view.layout.focus);
                write_msg(
                    sock_w,
                    &ClientMsg::Command(Command::MovePane {
                        mover: Some(pid),
                        target,
                        dir,
                    }),
                )
                .await
                .map_err(|e| format!("move send failed: {e}"))?;
            }
            None => view.set_notice("agent has no pane here".into()),
        },
        MenuAction::BreakOut => match a.pane_id {
            Some(pid) => write_msg(
                sock_w,
                &ClientMsg::Command(Command::BreakPane { pane: pid }),
            )
            .await
            .map_err(|e| format!("break send failed: {e}"))?,
            None => view.set_notice("agent has no pane here".into()),
        },
        MenuAction::PortalAt(dir) => {
            // A paneless LIVE thread (no attach id): open a fresh portal
            // view at the chosen spot - the same placement the portal
            // picker's new-portal row builds, target named when split so
            // the seat grafts beside the active tab's focus.
            let placement = match dir {
                Some(d) => PanePlacement {
                    portal_new: true,
                    split: Some(d),
                    target: PaneTarget::SquadId(view.layout.active_squad),
                    ..Default::default()
                },
                None => PanePlacement {
                    portal_new: true,
                    ..Default::default()
                },
            };
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::AttachAgent {
                    id: a.attach_id.clone().unwrap_or(a.name.clone()),
                    placement,
                }),
            )
            .await
            .map_err(|e| format!("portal placement send failed: {e}"))?;
        }
        MenuAction::Detach => match (a.pane_id, a.exited) {
            (Some(pid), false) => write_msg(
                sock_w,
                &ClientMsg::Command(Command::DetachPane { pane: pid }),
            )
            .await
            .map_err(|e| format!("detach send failed: {e}"))?,
            _ => view.set_notice("only a live pane-hosted worker can detach".into()),
        },
        MenuAction::ClosePortal => match a.pane_id {
            // One command, the seat named: FocusPane plus ClosePane would
            // close whatever holds focus if the seat vanished between the
            // two sends.
            Some(pid) => write_msg(
                sock_w,
                &ClientMsg::Command(Command::ClosePortal { seat: pid }),
            )
            .await
            .map_err(|e| format!("close portal send failed: {e}"))?,
            None => view.set_notice("agent has no pane here".into()),
        },
        MenuAction::PortalPicker => {
            // One decision path with sideline `P`: the picker itself refuses
            // what it cannot show (not attachable, no open portals to keep).
            match view.portal_pick_decision(Some(&a)) {
                PortalPickDecision::Open(id) => view.open_portal_pick(id),
                PortalPickDecision::Refuse(text) => view.set_notice(text),
            }
        }
        MenuAction::MoveToWorkspace => match a.pane_id {
            Some(pid) => {
                // Recomputed at execute (a workspace added or removed between
                // open and pick is reflected); `move_pick_keys` re-validates.
                let dsts = view.move_dst_squads(a.squad);
                if dsts.is_empty() {
                    view.set_notice("no other workspace to move into".into());
                } else {
                    view.open_move_pick(MoveSrc::Pane(pid), dsts);
                }
            }
            None => view.set_notice("agent has no pane here".into()),
        },
        MenuAction::Focus => match a.pane_id {
            Some(pid) => write_msg(sock_w, &ClientMsg::Command(Command::FocusPane(pid)))
                .await
                .map_err(|e| format!("focus send failed: {e}"))?,
            None => view.set_notice("agent has no pane here".into()),
        },
        MenuAction::Diff => {
            // Send the pane too: the server prefers it, which keeps the diff on
            // the row that was clicked when two share a name, and reaches a row
            // the registry never had.
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::ToggleDiffPane {
                    agent: Some(a.name.clone()),
                    pane: a.pane_id,
                }),
            )
            .await
            .map_err(|e| format!("diff send failed: {e}"))?;
        }
        MenuAction::RenameAgent => {
            // The CURRENT label, re-resolved at execute above (a rename
            // between menu-open and pick addresses the live row), seeded so
            // Enter with no edit lands on the verb's same-label no-op.
            view.open_rename_seeded(RenameTarget::Agent(a.name.clone()), a.name.clone());
        }
        MenuAction::ReleaseHold => {
            // The by-session release verb (mail_hold.rs `--release`): the
            // lift answers in microseconds; the held-mail delivery leg runs
            // detached, so the bounded wait below never sits on the
            // transport. The verb's EXIT is the truth a stale paired binary
            // cannot paper over: one that predates `--release` exits 2 on
            // the unknown flag, and the notice must say so, not "released".
            let Some(sid) = a.harness_session_id.clone() else {
                view.set_notice("row carries no session id to release".into());
                return Ok(());
            };
            let child = std::process::Command::new(crate::digest_overlay::fno_agents_bin())
                .args(["mail-hold", "--session", &sid, "--release"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            let mut child = match child {
                Ok(child) => child,
                Err(exc) => {
                    view.set_notice(format!("release did not start: {exc}"));
                    return Ok(());
                }
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    _ => break None,
                }
            };
            match status {
                Some(s) if s.success() => view.set_notice(format!(
                    "hold released for {}; held mail is delivering",
                    a.name
                )),
                Some(s) => view.set_notice(format!("release failed ({s}); is fno-agents current?")),
                None => {
                    view.set_notice("release is still running; the hold lifts when it lands".into())
                }
            }
        }
        MenuAction::Peek | MenuAction::Mail => {
            let idx = view
                .display_rows()
                .iter()
                .position(|r| matches!(r, DisplayRow::Agent(x) if target.matches(x)));
            match idx {
                Some(idx) => {
                    fetch_peek(view, idx, a.name.clone(), sock_w).await?;
                    // (6.2) Mail arms the SAME free-text composer peek
                    // `m` opens - one input surface, two doors.
                    if matches!(action, MenuAction::Mail) {
                        view.peek_input =
                            Some((a.name.clone(), super::composer_draft::load(&a.name)));
                        view.peek_input_esc.clear();
                    }
                }
                None => view.set_notice("agent is no longer here".into()),
            }
        }
        // (6.2) Resume: the same command peek `r` sends, re-checked
        // against the row's LIVE state - a row that restarted on its own
        // between open and pick must not be respawned again.
        MenuAction::Resume => {
            if a.exited {
                write_msg(
                    sock_w,
                    &ClientMsg::Command(Command::RespawnAgent {
                        name: a.name.clone(),
                    }),
                )
                .await
                .map_err(|e| format!("respawn send failed: {e}"))?;
            } else {
                view.set_notice("only an exited row can resume".into());
            }
        }
        MenuAction::Reattach => {
            if !a.exited && a.pane_id.is_none() {
                write_msg(
                    sock_w,
                    &ClientMsg::Command(Command::ResumeAgent {
                        name: a.name.clone(),
                    }),
                )
                .await
                .map_err(|e| format!("reattach send failed: {e}"))?;
            } else {
                view.set_notice("only a live paneless row can reattach".into());
            }
        }
        // Unreachable: Rename is built only for a workspace section, which
        // returns above. Visible refusal over a silent no-op.
        MenuAction::Rename => view.set_notice("action does not apply to an agent".into()),
        MenuAction::Stop | MenuAction::Remove => {
            let kind = match action {
                MenuAction::Stop => match (a.external, a.attach_id.clone()) {
                    (true, Some(id)) => ConfirmKind::StopExternal {
                        attach_id: id,
                        name: a.name.clone(),
                    },
                    _ => ConfirmKind::StopAgent {
                        name: a.name.clone(),
                        sid: a.harness_session_id.clone(),
                        pane_id: a.pane_id,
                    },
                },
                // Remove routes by row KIND through [`remove_dead`], the same
                // mapping the bulk clear uses, so the single-row and section
                // paths cannot disagree about which store owns a dead row.
                _ => match remove_dead(&a) {
                    Command::DismissMember { squad, attach_id } => {
                        ConfirmKind::DismissMember { squad, attach_id }
                    }
                    Command::RemoveExternal { attach_id, name } => {
                        ConfirmKind::RemoveExternal { attach_id, name }
                    }
                    _ => ConfirmKind::RemoveAgent {
                        name: a.name.clone(),
                        sid: a.harness_session_id.clone(),
                        pane_id: a.pane_id,
                        measure: agent_lattice_state(&a) == LatticeState::Unmeasured,
                    },
                },
            };
            // (scope a+c) Same post-commit re-anchor slot the bare `x`
            // arms: the sideline stays open, the cursor stays on the row.
            view.row_slot = view
                .display_rows()
                .iter()
                .position(|r| matches!(r, DisplayRow::Agent(row) if row.name == a.name));
            // The confirm pref arms the overlay; the default (off)
            // dispatches the SAME command the confirm would commit, so the two
            // paths cannot disagree about what a gesture sends. The
            // too-short-terminal guard rides the confirm branch only: it exists
            // so the prompt is visible, which the default never shows.
            if view.confirm_lifecycle {
                // A confirm owns the bottom row; a too-short terminal refuses
                // rather than arm an invisible prompt (matching the selector's
                // stop/reap).
                if view.term.0 < MIN_ROWS_FOR_STATUS {
                    view.set_notice("terminal too short for the confirm prompt".into());
                    return Ok(());
                }
                view.open_confirm(ConfirmAction {
                    action: kind,
                    label: a.name.clone(),
                });
            } else {
                // The confirm path stamps the row when it commits; the default
                // dispatch gets the same treatment, so the outcome notice
                // renders at the row either way.
                view.arm_row_stamp(&kind);
                let sent = match kind.command() {
                    Some(cmd) => {
                        write_msg(sock_w, &ClientMsg::Command(cmd))
                            .await
                            .map_err(|e| format!("lifecycle dispatch send failed: {e}"))?;
                        true
                    }
                    None => false,
                };
                if sent {
                    view.reanchor_after_row_commit(Some(a.name.as_str()));
                }
            }
        }
        // Only ever built alongside `MenuTarget::Section`, which returned above.
        // A Notice rather than `unreachable!` - a panic here would take the whole
        // multiplexer down over a menu-construction bug.
        MenuAction::ClearDead => view.set_notice("clear dead needs a section header".into()),
        MenuAction::MoveSquad(_) | MenuAction::RemoveSquad => {
            view.set_notice("move and remove need a workspace section header".into())
        }
        // Unreachable: the tab actions all pair with `MenuTarget::Tab`, which
        // returns in the target match above. Visible refusal over a no-op.
        MenuAction::TabNew
        | MenuAction::TabRename
        | MenuAction::TabReorder(_)
        | MenuAction::TabMoveTo
        | MenuAction::TabJoin(_)
        | MenuAction::TabSplit(_)
        | MenuAction::TabClose => view.set_notice("tab actions need a tab cell".into()),
    }
    Ok(())
}

/// Arm the clear-dead confirm for a section, over the dead set as it
/// stands NOW rather than as the menu found it.
fn clear_dead_confirm(
    view: &mut View,
    key: SectionKey,
    label: String,
    squad: Option<u64>,
) -> Result<(), String> {
    let dead = view
        .section_dead_rows(&key, squad)
        .len()
        .min(CLEAR_DEAD_MAX);
    if dead == 0 {
        view.set_notice(format!("no dead rows in {label}"));
        return Ok(());
    }
    // A confirm owns the bottom row; a too-short terminal refuses rather than
    // arm an invisible prompt (matching the selector's stop/reap).
    if view.term.0 < MIN_ROWS_FOR_STATUS {
        view.set_notice("terminal too short for the confirm prompt".into());
        return Ok(());
    }
    view.open_confirm(ConfirmAction {
        action: ConfirmKind::ClearDead { key, squad, dead },
        label,
    });
    Ok(())
}
