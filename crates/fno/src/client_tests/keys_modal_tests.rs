//! The which-key modal render and the chords that open the menu-only
//! surfaces (kept out of the over-budget client_tests.rs; each shrink is
//! banked).

use super::tests::two_pane_view;
use super::*;
use crate::vt::frame_text;

#[test]
fn client_compose_keys_modal_renders_the_which_key_reference() {
    // prefix+? opens the centered which-key modal, built from the single-source binding table.
    // 53 rows: the global (no prefix) section spent five (a header + the
    // scanner's four chords) and the V chord one more, so the panes header
    // sits at body row 46 and needs this height to render from scroll 0.
    // Shorter terminals scroll to it (the title says so); the 72-row notes
    // pin upstream is the budget the notes themselves must stay honest
    // against.
    let mut view = two_pane_view();
    view.term = (53, 80);
    view.open_keys_modal();
    let text = frame_text(&view.compose());
    assert!(text.contains("keybinds"), "modal title present");
    assert!(text.contains("esc close"), "dismiss affordance present");
    // Section headers + a sampling of bindings the table advertises.
    assert!(text.contains("panes"), "section header");
    assert!(text.contains("detach"), "the d binding's action");
    assert!(
        text.contains("find: goto squad/tab/pane/agent"),
        "the f binding's action names every row class nav_rows emits"
    );
    // The digit row names the gesture and its resolve doors: an honest description of an input path the scanner really runs.
    assert!(
        text.contains("jump to tab by number")
            && text.contains("Enter")
            && text.contains("Alt works too"),
        "the digit row names the gesture and its resolve doors"
    );
}

#[tokio::test]
async fn menu_only_surfaces_open_from_their_new_chords() {
    // prefix+S / prefix+A / prefix+T reach the same surfaces the sideline
    // menu rows open, through the one mutation point (`execute_aux_action`),
    // so chord and menu can never disagree on what they open. All three are
    // client-local: nothing reaches the wire.
    let mut v = two_pane_view();
    v.term = (40, 80);
    let mut buf: Vec<u8> = Vec::new();
    dispatch_event(&mut v, crate::keys::Event::OpenSettings, &mut buf)
        .await
        .unwrap();
    assert!(v.aux.is_some(), "prefix+S opens the settings modal");
    v.aux = None;
    dispatch_event(&mut v, crate::keys::Event::OpenConnections, &mut buf)
        .await
        .unwrap();
    assert!(
        v.connections.is_some(),
        "prefix+A opens connections in its loading state"
    );
    dispatch_event(&mut v, crate::keys::Event::OpenSweepThreads, &mut buf)
        .await
        .unwrap();
    assert!(
        matches!(v.sweep_action, Some(SweepAction::Counts)),
        "prefix+T arms the sweep counts probe"
    );
    assert!(buf.is_empty(), "nothing to the wire");
}
