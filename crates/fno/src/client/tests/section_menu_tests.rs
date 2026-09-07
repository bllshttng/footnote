//! The sideline SECTION-MENU family (workspace rename, move, clear-dead,
//! remove-squad confirm): moved out of client_tests.rs under the
//! file-budget gate. Helpers stay in the parent and arrive via super::*.
use super::*;

/// Open the section menu on a squad header and run its only entry.
pub(crate) async fn arm_clear_dead(v: &mut View, squad: u64) {
    let hdr = squad_header_at(v, squad);
    assert!(v.open_row_menu(hdr, Anchor::Center), "section menu opens");
    // Rename now leads the workspace menu, so explicitly select Clear dead.
    let m = v.row_menu.as_mut().unwrap();
    m.popup.sel = m
        .actions
        .iter()
        .position(|a| *a == super::MenuAction::ClearDead)
        .expect("clear-dead entry present");
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "the menu entry only arms the confirm");
}

#[tokio::test]
async fn clear_dead_removes_every_dead_row_in_the_section() {
    // (x-f300) The header menu's clear-dead sends one Remove per exited row
    // and leaves every live row alone.
    let mut v = view_with_dead_interleaved();
    arm_clear_dead(&mut v, 1).await;
    match v.confirm.as_ref().map(|c| &c.action) {
        Some(ConfirmKind::ClearDead { dead, .. }) => assert_eq!(*dead, 2),
        _ => panic!("expected a ClearDead confirm"),
    }
    let mut buf: Vec<u8> = Vec::new();
    confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert_eq!(
        decode_cmds(buf),
        vec![
            Command::RemoveAgent {
                harness_session_id: None,
                name: "dead-a".into(),
                pane_id: None,
                measure: false,
            },
            Command::RemoveAgent {
                harness_session_id: None,
                name: "dead-b".into(),
                pane_id: None,
                measure: false,
            },
        ],
        "only the exited rows are removed"
    );
}

#[test]
fn nonworkspace_section_with_no_dead_rows_gets_a_notice() {
    // The menu never renders a no-op entry: a NON-workspace band (Elsewhere)
    // with nothing to clear and nothing to rename gets a notice, not a
    // one-entry menu (AC-EDGE). A workspace section always opens (Rename).
    let orphan_live = {
        let mut r = lifecycle_row("stray-live", false, false);
        r.squad = Some(99); // no such squad -> Elsewhere band
        r
    };
    let mut v = view_with_agents(vec![orphan_live]);
    let hdr = v
        .display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Header { key, .. } if *key == SectionKey::Elsewhere))
        .expect("elsewhere band");
    assert!(!v.open_row_menu(hdr, Anchor::Center));
    assert!(v.row_menu.is_none());
    assert!(v.notice.is_some(), "and says why");
}

#[tokio::test]
async fn workspace_section_menu_offers_rename() {
    // US3: a workspace section header offers Rename (menu parity with
    // selector `r`), even with no dead rows to clear.
    let mut v = view_with_agents(vec![]);
    v.layout.agents = vec![lifecycle_row("live-a", false, false)];
    let hdr = squad_header_at(&v, 1);
    assert!(v.open_row_menu(hdr, Anchor::Center), "workspace menu opens");
    assert_eq!(
        v.row_menu.as_ref().unwrap().actions,
        vec![
            super::MenuAction::Rename,
            super::MenuAction::MoveSquad(-1),
            super::MenuAction::MoveSquad(1),
            super::MenuAction::RemoveSquad
        ],
        "no dead rows -> the five standing workspace verbs"
    );
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "opening the overlay sends nothing");
    assert_eq!(
        v.rename.map(|(t, _)| t),
        Some(RenameTarget::Squad(1)),
        "opens the rename overlay for this workspace"
    );
}

#[test]
fn workspace_section_menu_offers_rename_then_clear_dead() {
    // With dead rows present the workspace menu offers BOTH, Rename first.
    let mut v = view_with_dead_interleaved();
    let hdr = squad_header_at(&v, 1);
    assert!(v.open_row_menu(hdr, Anchor::Center));
    assert_eq!(
        v.row_menu.as_ref().unwrap().actions,
        vec![
            super::MenuAction::Rename,
            super::MenuAction::MoveSquad(-1),
            super::MenuAction::MoveSquad(1),
            super::MenuAction::RemoveSquad,
            super::MenuAction::ClearDead
        ]
    );
}

#[tokio::test]
async fn workspace_section_menu_move_sends_the_reorder_command() {
    // AC8-HP: Move up/down ride the same Command::MoveSquad the keyboard
    // J/K path sends; the server's silent clamp covers the at-edge case
    // (AC9-EDGE), so the client sends unconditionally and never bells.
    let mut v = view_with_agents(vec![]);
    v.layout.agents = vec![lifecycle_row("live-a", false, false)];
    let hdr = squad_header_at(&v, 1);
    assert!(v.open_row_menu(hdr, Anchor::Center));
    for (delta, label) in [(-1, "up"), (1, "down")] {
        let m = v.row_menu.as_mut().unwrap();
        m.popup.sel = m
            .actions
            .iter()
            .position(|a| *a == super::MenuAction::MoveSquad(delta))
            .unwrap_or_else(|| panic!("move-{label} entry present"));
        let mut buf: Vec<u8> = Vec::new();
        row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
        assert_eq!(
            decode_cmds(buf),
            vec![Command::MoveSquad { squad: 1, delta }],
            "move {label} sends the reorder command"
        );
        assert!(v.open_row_menu(hdr, Anchor::Center), "re-open for the next");
    }
}

#[tokio::test]
async fn workspace_section_menu_remove_opens_the_confirm_not_the_command() {
    // AC8-HP: Remove workspace routes through the SAME
    // ConfirmKind::RemoveSquad confirm the keyboard path builds - a mouse
    // click must not skip the destructive-action gate.
    let mut v = view_with_agents(vec![]);
    v.layout.agents = vec![lifecycle_row("live-a", false, false)];
    let hdr = squad_header_at(&v, 1);
    assert!(v.open_row_menu(hdr, Anchor::Center));
    let m = v.row_menu.as_mut().unwrap();
    m.popup.sel = m
        .actions
        .iter()
        .position(|a| *a == super::MenuAction::RemoveSquad)
        .unwrap();
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "the entry arms the confirm, sends nothing");
    match v.confirm.as_ref().map(|c| &c.action) {
        Some(ConfirmKind::RemoveSquad { squad, .. }) => assert_eq!(*squad, 1),
        _ => panic!("expected a RemoveSquad confirm"),
    }
}
