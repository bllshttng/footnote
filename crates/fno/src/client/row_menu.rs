//! The per-row context menu : the per-state entry builder for one
//! sideline row. Split out of client.rs so the over-budget file shrinks;
//! everything here reaches the client's private items through `super::*`.

use super::*;

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
