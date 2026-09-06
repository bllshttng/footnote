//! The default-dispatch lifecycle gesture (x-e763): with the confirm pref
//! at its default (off), Stop/Remove dispatch the same command the confirm
//! would commit, in one gesture. Lives outside client_tests.rs under the
//! file-budget gate.
use super::*;

#[tokio::test]
async fn menu_stop_on_a_live_row_dispatches_with_the_default_pref() {
    // AC10: no are-you-sure by default. The command the confirm would have
    // committed goes straight onto the wire; no overlay arms.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    assert!(!v.confirm_lifecycle, "the default is no confirm");
    let row = agent_row_at(&v, |a| a.name == "w");
    assert!(v.open_row_menu(row, Anchor::Center));
    menu_select(&mut v, super::MenuAction::Stop).await;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(v.confirm.is_none(), "no confirm armed");
    assert_eq!(
        decode_cmds(buf),
        vec![Command::StopAgent {
            name: "w".into(),
            harness_session_id: None,
            pane_id: Some(10)
        }],
        "the stop dispatched in one gesture, carrying the row's pane"
    );
}

#[tokio::test]
async fn menu_remove_with_the_pref_on_still_arms_the_overlay() {
    // AC11: the pref keeps today's confirm.
    let mut v = view_with_agents(vec![agent_row("w", 10, Some(AgentBadge::Working), false)]);
    v.confirm_lifecycle = true;
    let row = agent_row_at(&v, |a| a.name == "w");
    assert!(v.open_row_menu(row, Anchor::Center));
    menu_select(&mut v, super::MenuAction::Remove).await;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(v.confirm.is_some(), "the overlay armed");
    assert!(buf.is_empty(), "nothing on the wire until Enter");
}
