//! The which-key modal render and the chords that open the menu-only
//! surfaces (kept out of the over-budget client_tests.rs; each shrink is
//! banked).

use super::keys_modal::keys_modal_keys;
use super::tests::two_pane_view;
use super::*;
use crate::vt::frame_text;

#[test]
fn client_compose_keys_modal_renders_the_which_key_reference() {
    // prefix+? opens the centered which-key modal in the plain-body shape:
    // title + esc chip, a dim hint under the title, a fixed 60% viewport
    // with two-column rows and blank-line section spacing. Deep sections sit
    // below the fold; the table is browsed by scroll or '/' filter, and the
    // tail notes + glyph legend ride the unfiltered tail.
    let mut view = two_pane_view();
    view.term = (57, 80);
    view.open_keys_modal();
    let text = frame_text(&view.compose());
    assert!(text.contains("keybinds"), "chrome title present");
    assert!(text.contains("esc close"), "dismiss affordance present");
    assert!(
        text.contains("⏎ runs the selected chord"),
        "dim hint under the title"
    );
    assert!(
        text.contains("j/k scroll · / filter · ⏎ run · esc close"),
        "footer names the scroll and search keys"
    );
    assert!(
        text.contains("global (no prefix)"),
        "the first section header renders"
    );
    assert!(
        !text.contains("panes"),
        "fixed height: deep sections sit below the fold (scroll/filter to them)"
    );
    // The two-column anatomy: the global `w` row renders key then label on
    // one line, key left. The ` w ` cell matches only the w row - the
    // Ctrl+Opt+Left meta row's label also says "row selector".
    let key_line = text
        .lines()
        .find(|l| l.contains(" w ") && l.contains("sideline row selector"))
        .expect("the global w row renders");
    let wcol = key_line.find(" w ").expect("the w key cell");
    let label = key_line.find("sideline row selector").unwrap();
    assert!(wcol < label, "key column left of the label: {key_line:?}");
    // The tail notes + glyph legend sit BELOW the fold at scroll 0 (they
    // ride after every section, panes included) and are reached by scroll;
    // x7683_keys_modal_names_every_menu_trigger pins that reachability.
}

#[tokio::test]
async fn keys_modal_j_k_and_filter_drive_the_plain_body_modal() {
    // The modal's own grammar: j/k move the cursor; '/' enters the live
    // filter (key, label, or action id, case-insensitive); printable bytes
    // edit the query; backspace pops it and an empty backspace exits the
    // filter; the query rides the hint line.
    let mut v = two_pane_view();
    v.term = (53, 80);
    v.open_keys_modal();
    let sel_row = |v: &View| {
        v.keys_modal
            .as_ref()
            .unwrap()
            .popup
            .selected()
            .expect("a selectable row")
            .0
    };
    let start = sel_row(&v);
    keys_modal_keys(&mut v, &mut Scanner::default(), b"k", &mut Vec::<u8>::new())
        .await
        .unwrap();
    assert_eq!(sel_row(&v), start, "k at the top stays (no wrap)");
    keys_modal_keys(&mut v, &mut Scanner::default(), b"j", &mut Vec::<u8>::new())
        .await
        .unwrap();
    assert!(sel_row(&v) > start, "j moves the cursor down a row");
    // '/' opens the filter; typing narrows; the query rides the hint line.
    keys_modal_keys(
        &mut v,
        &mut Scanner::default(),
        b"/detach",
        &mut Vec::<u8>::new(),
    )
    .await
    .unwrap();
    let text = frame_text(&v.compose());
    assert!(
        text.contains("/detach"),
        "the live query rides the hint line"
    );
    let m = v.keys_modal.as_ref().unwrap();
    assert_eq!(m.filter.as_deref(), Some("detach"));
    assert!(
        m.popup.rows.len() < 15,
        "the filter narrows the table: {} rows",
        m.popup.rows.len()
    );
    let rendered = m.popup.render(v.term);
    let body: Vec<String> = rendered
        .lines
        .iter()
        .map(|l| l.text.trim().to_string())
        .collect();
    assert!(
        body.iter().any(|l| l.contains("detach")),
        "the detach row survives the filter"
    );
    assert!(
        !body.iter().any(|l| l.contains("sideline row selector")),
        "non-matching rows drop"
    );
    // Backspace to empty exits the filter mode back to the full table.
    keys_modal_keys(
        &mut v,
        &mut Scanner::default(),
        b"\x7f\x7f\x7f\x7f\x7f\x7f\x7f",
        &mut Vec::<u8>::new(),
    )
    .await
    .unwrap();
    let m = v.keys_modal.as_ref().unwrap();
    assert!(m.filter.is_none(), "empty backspace exits the filter");
    assert!(
        m.popup.rows.len() > 40,
        "the full table is back: {} rows",
        m.popup.rows.len()
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
