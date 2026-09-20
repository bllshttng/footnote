//! The composer toggle (prefix+i) and full-screen sideline (prefix+F).
//! Extracted under the file-budget ratchet: the tests this feature
//! added answer their own question in a file of their own.

use super::*;

#[tokio::test]
async fn toggle_composer_opens_closes_and_retains_the_draft() {
    let mut v = two_pane_view();
    let mut buf: Vec<u8> = Vec::new();
    dispatch_event(&mut v, Event::ToggleComposer, &mut buf)
        .await
        .unwrap();
    assert!(v.launcher.is_some(), "prefix+i opens the composer");
    if let Some(l) = v.launcher.as_mut() {
        l.draft.message = "hello".into();
    }
    dispatch_event(&mut v, Event::ToggleComposer, &mut buf)
        .await
        .unwrap();
    assert!(v.launcher.is_none(), "prefix+i closes it");
    assert_eq!(
        v.launcher_closed.as_ref().unwrap().draft.message,
        "hello",
        "draft retained"
    );
    dispatch_event(&mut v, Event::ToggleComposer, &mut buf)
        .await
        .unwrap();
    assert_eq!(
        v.launcher.as_ref().unwrap().draft.message,
        "hello",
        "reopen restores the draft"
    );
}

#[tokio::test]
async fn toggle_composer_shows_hidden_sideline_and_sends_resize() {
    let mut v = two_pane_view();
    v.panel_on = false;
    let mut buf: Vec<u8> = Vec::new();
    dispatch_event(&mut v, Event::ToggleComposer, &mut buf)
        .await
        .unwrap();
    assert!(v.panel_on, "hidden sideline shows first");
    assert!(v.launcher.is_some(), "composer opens");
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Resize { .. } => {}
        other => panic!("expected a Resize, got {other:?}"),
    }
}

#[tokio::test]
async fn toggle_panel_with_composer_open_closes_it() {
    let mut v = two_pane_view();
    let mut buf: Vec<u8> = Vec::new();
    dispatch_event(&mut v, Event::ToggleComposer, &mut buf)
        .await
        .unwrap();
    assert!(v.launcher.is_some());
    dispatch_event(&mut v, Event::TogglePanel, &mut buf)
        .await
        .unwrap();
    assert!(!v.panel_on, "sideline hidden");
    assert!(
        v.launcher.is_none(),
        "hiding the sideline closes the composer"
    );
}

#[tokio::test]
async fn full_screen_sideline_hides_panes_and_shows_the_composer() {
    let mut v = two_pane_view();
    v.term = (40, 120);
    v.frames.insert(10, text_frame(29, 35, 'Q'));
    v.frames.insert(11, text_frame(29, 36, 'Q'));
    assert!(
        frame_text(&v.compose()).contains('Q'),
        "sanity: panes paint in normal mode"
    );
    let mut buf: Vec<u8> = Vec::new();
    dispatch_event(&mut v, Event::ToggleFullSideline, &mut buf)
        .await
        .unwrap();
    assert!(v.sideline_full);
    assert!(v.launcher.is_some(), "entering opens the composer");
    let text = frame_text(&v.compose());
    assert!(!text.contains('Q'), "no pane cell paints");
    assert!(
        text.contains("last msg"),
        "the Extended table header paints"
    );
    assert!(
        text.contains("[Launch]"),
        "composer occupies the bottom rows"
    );
}

#[tokio::test]
async fn full_screen_sideline_toggles_back_resyncs_and_repaints_panes() {
    let mut v = two_pane_view();
    v.term = (40, 120);
    v.frames.insert(10, text_frame(29, 35, 'Q'));
    v.frames.insert(11, text_frame(29, 36, 'Q'));
    let mut buf: Vec<u8> = Vec::new();
    dispatch_event(&mut v, Event::ToggleFullSideline, &mut buf)
        .await
        .unwrap();
    dispatch_event(&mut v, Event::ToggleFullSideline, &mut buf)
        .await
        .unwrap();
    assert!(!v.sideline_full, "leaving clears the flag");
    assert!(
        frame_text(&v.compose()).contains('Q'),
        "the panes paint again at their old size"
    );
    // Leaving re-syncs the server's content area once: a full-screen visit
    // that showed a hidden sideline left the server on panel-free pane
    // rects, and an idempotent re-layout is cheaper than stale geometry.
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Resize { .. } => {}
        other => panic!("expected the leave Resize, got {other:?}"),
    }
}

#[tokio::test]
async fn full_screen_click_beyond_the_panel_never_sends_bytes() {
    let mut v = two_pane_view();
    v.term = (40, 120);
    v.frames.insert(10, text_frame(29, 35, 'Q'));
    v.frames.insert(11, text_frame(29, 36, 'Q'));
    let mut buf: Vec<u8> = Vec::new();
    dispatch_event(&mut v, Event::ToggleFullSideline, &mut buf)
        .await
        .unwrap();
    // A left press at a column that is pane content in normal mode: the
    // dock is far away, so launcher_mouse reads unconsumed and nothing goes
    // to any pane.
    let rep = crate::mouse::MouseReport {
        kind: crate::proto::MouseKind::Press(crate::proto::MouseButton::Left),
        row: 10,
        col: 100,
        shift: false,
    };
    agent_launcher::launcher_mouse(&mut v, rep, &mut buf)
        .await
        .unwrap();
    assert!(buf.is_empty(), "no mouse bytes forwarded");
}
