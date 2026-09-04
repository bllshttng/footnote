//! The rename-overlay test family, in its own module. `use super::*` reaches
//! the tests module's imports (two_pane_view, rename_keys, blocked_row, ...).

use super::*;

#[tokio::test]
async fn rename_keys_enter_sends_the_typed_name_for_the_captured_tab() {
    // AC2-HP (client half): type + Enter -> one RenameTab for the tab id
    // captured at open time.
    let mut v = two_pane_view();
    v.open_rename(RenameTarget::Tab(7));
    let mut buf: Vec<u8> = Vec::new();
    rename_keys(&mut v, b"debug\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::RenameTab {
            tab: 7,
            name: "debug".into()
        })
    );
    assert_eq!(v.rename, None, "submit closes the overlay");
}

// Agent-target rename: grammar-filtered buffer, refused empty
// send, and the captured-label wire command.

#[tokio::test]
async fn rename_keys_enter_sends_rename_agent_for_the_captured_label() {
    let mut v = two_pane_view();
    v.open_rename_seeded(RenameTarget::Agent("old-label".into()), "old-label".into());
    // Editing starts from the seeded current label: backspace pops the
    // last three chars, the typed tail completes the new one.
    let mut sent: Vec<u8> = Vec::new();
    rename_keys(&mut v, b"\x7f\x7f\x7ftwo\r", &mut sent)
        .await
        .unwrap();
    let mut cur = std::io::Cursor::new(sent);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::RenameAgent {
            name: "old-label".into(),
            new_name: "old-latwo".into()
        })
    );
    assert_eq!(v.rename, None, "submit closes the overlay");
}

#[tokio::test]
async fn rename_keys_agent_target_filters_non_grammar_bytes() {
    let mut v = two_pane_view();
    v.open_rename_seeded(RenameTarget::Agent("w1".into()), "w1".into());
    // Space and symbol are OUTSIDE the registry grammar: they never enter
    // the buffer, so what is typed is what the server would keep.
    let mut sent: Vec<u8> = Vec::new();
    rename_keys(&mut v, b"a b!2", &mut sent).await.unwrap();
    let (_, name) = v.rename.clone().unwrap();
    assert_eq!(name, "w1ab2", "only grammar bytes land in the buffer");
}

#[tokio::test]
async fn rename_keys_agent_empty_enter_keeps_the_overlay_open() {
    let mut v = two_pane_view();
    v.open_rename(RenameTarget::Agent("w1".into()));
    let mut sent: Vec<u8> = Vec::new();
    // Enter on an empty buffer: a label is never derived, so this is a
    // refusal that keeps the overlay open, NOT reset-to-auto.
    rename_keys(&mut v, b"\r", &mut sent).await.unwrap();
    assert!(sent.is_empty(), "nothing is sent on an empty label");
    assert!(v.rename.is_some(), "the overlay stays open for a retry");
    // A backspace on an empty buffer is harmless; the state stays armed.
    rename_keys(&mut v, b"\x7f", &mut sent).await.unwrap();
    assert!(v.rename.is_some());
}

#[tokio::test]
async fn rename_agent_menu_entry_opens_the_seeded_overlay() {
    let mut v = view_with_agents(vec![blocked_row("worker-x", 3, None)]);
    let idx = agent_row_at(&v, |a| a.name == "worker-x");
    assert!(v.open_row_menu(idx, Anchor::Center));
    // Rename has no accelerator: select it by index, then Enter drives
    // the same execute path a click uses.
    let pos = v
        .row_menu
        .as_ref()
        .expect("row menu open")
        .actions
        .iter()
        .position(|a| matches!(a, MenuAction::RenameAgent))
        .expect("rename entry present on a plain fno row");
    v.row_menu.as_mut().unwrap().popup.select(pos);
    let mut sent: Vec<u8> = Vec::new();
    row_menu_keys(&mut v, b"\r", &mut sent).await.unwrap();
    let (target, seed) = v.rename.clone().expect("rename overlay opened");
    match target {
        RenameTarget::Agent(name) => {
            assert_eq!(name, "worker-x", "the captured label is the row's");
            assert_eq!(seed, "worker-x", "the buffer is seeded with the label");
        }
        other => panic!("expected an agent rename target, got {other:?}"),
    }
}

#[tokio::test]
async fn rename_agent_menu_entry_absent_on_external_rows() {
    let mut external = blocked_row("daemon-row", 4, None);
    external.external = true;
    let mut v = view_with_agents(vec![external]);
    let idx = agent_row_at(&v, |a| a.name == "daemon-row");
    assert!(v.open_row_menu(idx, Anchor::Center));
    let menu = v.row_menu.as_ref().expect("row menu open");
    assert!(
        !menu
            .actions
            .iter()
            .any(|a| matches!(a, MenuAction::RenameAgent)),
        "an external row's claude daemon owns its name; no rename entry"
    );
}

#[tokio::test]
async fn rename_keys_empty_enter_still_sends_the_clear() {
    // Locked 2 / AC3-HP: Enter on an EMPTY buffer sends (blank = reset to
    // auto) - the one deliberate divergence from create_keys.
    let mut v = two_pane_view();
    v.open_rename(RenameTarget::Tab(7));
    let mut buf: Vec<u8> = Vec::new();
    rename_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::RenameTab {
            tab: 7,
            name: String::new()
        })
    );
    assert_eq!(v.rename, None);
}

#[tokio::test]
async fn rename_keys_esc_cancels_without_sending_and_swallows_the_tail() {
    // AC1-UI: Esc closes, sends nothing; same-chunk bytes after the Esc
    // die with the overlay instead of leaking into the pane.
    let mut v = two_pane_view();
    v.open_rename(RenameTarget::Tab(7));
    let mut buf: Vec<u8> = Vec::new();
    rename_keys(&mut v, b"deb\x1bx", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "cancel sends no command");
    assert_eq!(v.rename, None);
}

#[tokio::test]
async fn rename_keys_caps_the_buffer_at_max_tab_name() {
    // The TUI affordance half of AC2-ERR: the operator sees exactly what
    // the server will store (the server cap stays authoritative).
    let mut v = two_pane_view();
    v.open_rename(RenameTarget::Tab(7));
    let mut buf: Vec<u8> = Vec::new();
    let long = "a".repeat(MAX_TAB_NAME + 8);
    rename_keys(&mut v, long.as_bytes(), &mut buf)
        .await
        .unwrap();
    assert_eq!(v.rename.as_ref().unwrap().1.len(), MAX_TAB_NAME);
}
