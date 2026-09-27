//! A lone ESC in an overlay carry is a bare Esc once input has gone quiet:
//! the client flushes the carry through an empty read after the quiet
//! window. Pins the four folds' flush contract; split-arrow safety and
//! mid-sequence carries must be untouched by it.

use super::input_folds::{
    fold_modal_keys, fold_nav_input, fold_search_input, fold_selector_keys, ModalKey, NavKey,
    SearchKey,
};
use super::overlay_keys;
use super::placement_pickers::{attach_place_keys, portal_pick_keys, AttachPlace, PortalPick};
use super::tests::two_pane_view;
use super::{move_pick_keys, MovePick, MoveSrc};
use crate::keys::Scanner;

#[test]
fn an_empty_read_flushes_a_lone_esc_in_every_fold() {
    // The carry a lone ESC byte left is released by the quiet-window flush
    // (the empty read) as exactly one Esc token, carry emptied.
    let mut esc = Vec::new();
    assert_eq!(fold_selector_keys(&mut esc, b"\x1b"), b"");
    assert_eq!(fold_selector_keys(&mut esc, b""), vec![0x1b]);
    assert!(esc.is_empty());

    let mut esc = Vec::new();
    assert!(fold_modal_keys(&mut esc, b"\x1b").is_empty());
    assert_eq!(fold_modal_keys(&mut esc, b""), vec![ModalKey::Esc]);
    assert!(esc.is_empty());

    let mut esc = Vec::new();
    assert!(fold_search_input(&mut esc, b"\x1b").is_empty());
    assert_eq!(fold_search_input(&mut esc, b""), vec![SearchKey::Esc]);
    assert!(esc.is_empty());

    let mut esc = Vec::new();
    assert!(fold_nav_input(&mut esc, b"\x1b").is_empty());
    assert!(
        matches!(fold_nav_input(&mut esc, b"").as_slice(), [NavKey::Esc]),
        "an empty read releases the lone ESC as NavKey::Esc"
    );
    assert!(esc.is_empty());
}

#[test]
fn a_split_arrow_still_rejoins_without_an_empty_read_between() {
    // The flush must not turn an arrow's leading ESC into an Esc: the two
    // halves arrive as back-to-back reads, never an empty read between.
    let mut esc = Vec::new();
    assert_eq!(fold_selector_keys(&mut esc, b"\x1b"), b"");
    assert_eq!(fold_selector_keys(&mut esc, b"[B"), b"j");
    assert!(esc.is_empty());

    let mut esc = Vec::new();
    assert!(fold_modal_keys(&mut esc, b"\x1b").is_empty());
    assert_eq!(fold_modal_keys(&mut esc, b"[B"), vec![ModalKey::Down]);
    assert!(esc.is_empty());

    let mut esc = Vec::new();
    assert_eq!(fold_selector_keys(&mut esc, b"\x1b[1;5A"), b"");
    assert!(esc.is_empty(), "a modified arrow leaves nothing behind");
}

#[test]
fn a_partial_carry_never_flushes_on_an_empty_read() {
    // Only a carry of exactly [0x1b] is a bare Esc; a sequence still
    // mid-flight is kept for its tail, byte for byte.
    for chunk in [&b"\x1b["[..], &b"\x1b[1"[..]] {
        let mut esc = Vec::new();
        assert_eq!(fold_selector_keys(&mut esc, chunk), b"");
        assert_eq!(fold_selector_keys(&mut esc, b""), b"");
        assert_eq!(esc, chunk.to_vec(), "selector carry unchanged");

        let mut esc = Vec::new();
        assert!(fold_modal_keys(&mut esc, chunk).is_empty());
        assert!(fold_modal_keys(&mut esc, b"").is_empty());
        assert_eq!(esc, chunk.to_vec(), "modal carry unchanged");

        let mut esc = Vec::new();
        assert!(fold_search_input(&mut esc, chunk).is_empty());
        assert!(fold_search_input(&mut esc, b"").is_empty());
        assert_eq!(esc, chunk.to_vec(), "search carry unchanged");

        let mut esc = Vec::new();
        assert!(fold_nav_input(&mut esc, chunk).is_empty());
        assert!(fold_nav_input(&mut esc, b"").is_empty());
        assert_eq!(esc, chunk.to_vec(), "nav carry unchanged");
    }
}

#[tokio::test]
async fn a_lone_esc_left_in_a_picker_carry_flushes_closed() {
    // The stall: one ESC byte leaves each picker open, its carry held.
    // The flush: the quiet window routes an empty read and the picker
    // takes its Esc action, with nothing sent to the server.
    let mut v = two_pane_view();
    v.move_pick = Some(MovePick::new(MoveSrc::Tab(7), vec![2]));
    move_pick_keys(&mut v, b"\x1b", &mut Vec::new())
        .await
        .unwrap();
    assert!(
        v.move_pick.is_some(),
        "stall: the picker holds the lone ESC"
    );
    assert_eq!(v.move_pick.as_ref().unwrap().esc, vec![0x1b]);
    let mut buf: Vec<u8> = Vec::new();
    overlay_keys::flush_lone_esc(&mut v, &mut Scanner::default(), &mut buf)
        .await
        .unwrap();
    assert!(
        v.move_pick.is_none(),
        "flush: the lone ESC closed the picker"
    );
    assert!(buf.is_empty(), "a picker Esc sends nothing");

    let mut v = two_pane_view();
    v.attach_place = Some(AttachPlace {
        id: "a".into(),
        cursor: 0,
        squads: vec![2],
        esc: Vec::new(),
    });
    attach_place_keys(&mut v, b"\x1b", &mut Vec::new())
        .await
        .unwrap();
    assert!(
        v.attach_place.is_some(),
        "stall: the picker holds the lone ESC"
    );
    overlay_keys::flush_lone_esc(&mut v, &mut Scanner::default(), &mut Vec::new())
        .await
        .unwrap();
    assert!(
        v.attach_place.is_none(),
        "flush: the lone ESC closed the picker"
    );

    let mut v = two_pane_view();
    v.portal_pick = Some(PortalPick {
        id: "a".into(),
        cursor: 0,
        esc: Vec::new(),
    });
    portal_pick_keys(&mut v, b"\x1b", &mut Vec::new())
        .await
        .unwrap();
    assert!(
        v.portal_pick.is_some(),
        "stall: the picker holds the lone ESC"
    );
    overlay_keys::flush_lone_esc(&mut v, &mut Scanner::default(), &mut Vec::new())
        .await
        .unwrap();
    assert!(
        v.portal_pick.is_none(),
        "flush: the lone ESC closed the picker"
    );
}

#[tokio::test]
async fn the_flush_leaves_the_carryless_move_to_prompt_unchanged() {
    // The move-to prompt has no esc carry and acts on real bytes only, so
    // a flush resolves nothing and sends nothing.
    let mut v = two_pane_view();
    v.move_to = Some((1, String::new()));
    let mut buf: Vec<u8> = Vec::new();
    overlay_keys::flush_lone_esc(&mut v, &mut Scanner::default(), &mut buf)
        .await
        .unwrap();
    assert!(v.move_to.is_some(), "a flush never resolves the prompt");
    assert!(buf.is_empty(), "and sends nothing");
}
