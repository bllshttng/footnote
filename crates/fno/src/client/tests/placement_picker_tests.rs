use super::*;

// The attach-placement picker family: how `p` opens it, what each key
// commits (cursor vs here), and the arrow twin guarantees the hints
// advertise. Lives in its own module; client_tests.rs is shrink-only under
// the file-budget gate.

#[tokio::test]
async fn selector_p_opens_attach_placement_without_sending() {
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    let picker = v.attach_place.as_ref().expect("placement picker opens");
    assert_eq!(picker.id, "c19cd2c3");
    assert_eq!(picker.target(), Some(1));
    assert_eq!(picker.squads, vec![1, 2]);
    // The footer must let each axis name its OWN keys, and must say which
    // key acts on the `›` marker. The old footer listed the split
    // directions as if they were list navigation, which was the mislabel
    // half of the reported defect. This footer collapses it to two lines,
    // names arrows before hjkl (the operator ruling for every hint), and
    // spells the split row `shift+arrows/HJKL` so the shift relationship
    // reads in words, not just in case.
    let overlay = v.attach_place_lines(picker).join("\n");
    for label in [
        "arrows/hjkl move",
        "1-9 jump",
        // enter/t send byte-identical new-tab messages; space/. send
        // byte-identical here messages. Each pair is one footer entry.
        "enter/t new tab in ›",
        "shift+arrows/HJKL split",
        "space/. here",
        "cancel",
    ] {
        assert!(overlay.contains(label), "missing {label}: {overlay}");
    }
    assert!(buf.is_empty());
    assert_eq!(v.selector, None);
}

#[tokio::test]
async fn attach_placement_selects_target_and_direction() {
    // The digit jumps the cursor; UPPERCASE commits with a split direction.
    // Lowercase `h` here would now only move the cursor (see
    // attach_placement_arrows_move_the_cursor_without_attaching).
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    attach_place_keys(&mut v, b"2H", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::AttachAgent {
            id: "c19cd2c3".into(),
            placement: PanePlacement {
                target: PaneTarget::SquadId(2),
                split: Some(Dir::Left),
                ..Default::default()
            },
        })
    );
    assert!(v.attach_place.is_none());
}

#[tokio::test]
async fn attach_placement_keys_do_not_depend_on_cursor_history() {
    // The hard constraint behind superseding x-fbb1: a key must mean the
    // same thing whether or not the cursor has moved. The tempting cheap
    // fix was "Enter means here on the starting row, and commits the cursor
    // once moved", which is a hidden mode - the exact class this node
    // closes. Enter on an UNMOVED cursor must still commit that cursor,
    // never silently fall back to the route.
    let mut v = unified_rows_view();
    widen_to_squads(&mut v, 14);
    open_attach_by_click(&mut v).await;
    let start = v.attach_place.as_ref().unwrap().target().unwrap();
    let mut buf = Vec::new();
    attach_place_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { placement, .. }) => {
            assert_eq!(
                placement.target,
                PaneTarget::SquadId(start),
                "Enter commits the cursor even when it has never moved"
            );
            assert!(!placement.here, "and is not secretly the route");
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }

    // And the cursor's ROUTE to a row does not change what commits: digit
    // and arrows landing on the same index must produce the same command.
    let by_digit = {
        let mut v = unified_rows_view();
        widen_to_squads(&mut v, 14);
        open_attach_by_click(&mut v).await;
        let mut buf = Vec::new();
        attach_place_keys(&mut v, b"4\r", &mut buf).await.unwrap();
        buf
    };
    let by_arrows = {
        let mut v = unified_rows_view();
        widen_to_squads(&mut v, 14);
        open_attach_by_click(&mut v).await;
        let mut buf = Vec::new();
        attach_place_keys(&mut v, b"jjj\r", &mut buf).await.unwrap();
        buf
    };
    assert_eq!(
        by_digit, by_arrows,
        "how the cursor got there cannot matter"
    );
}

#[tokio::test]
async fn attach_placement_out_of_range_digit_bels_and_moves_nothing() {
    // A digit past the end of the list is a BEL, not a selection, and the
    // notice says the row is not there rather than claiming the workspace is
    // "no longer available" - it never was. The cursor stays put and the
    // picker stays open, so the operator can just press the right key next.
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    attach_place_keys(&mut v, b"9", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "an out-of-range digit sends nothing");
    let picker = v.attach_place.as_ref().expect("picker stays open");
    assert_eq!(picker.cursor, 0, "and moves the cursor nowhere");
}

#[tokio::test]
async fn attach_placement_out_of_range_digit_drops_the_rest_of_the_batch() {
    // The BEL alone is not enough. Terminal reads arrive in batches, so the
    // keys typed AFTER a bad digit are already in the same buffer - and they
    // were composed believing row 9 existed. `L` would commit a right split
    // into whatever the cursor happened to be on, a placement the operator
    // never chose, with only a beep between intent and commit. So the bad
    // digit abandons the whole read: nothing is sent, the cursor is untouched
    // and the picker stays open on screen the operator can now actually read.
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    attach_place_keys(&mut v, b"9L", &mut buf).await.unwrap();
    assert!(
        buf.is_empty(),
        "the trailing commit key must not reach the socket"
    );
    let picker = v.attach_place.as_ref().expect("picker stays open");
    assert_eq!(picker.cursor, 0, "and the cursor never moved");
    assert!(
        v.notice.is_some(),
        "the operator is told why nothing happened"
    );
}

#[tokio::test]
async fn attach_placement_arrows_move_the_cursor_without_attaching() {
    // AC3-FR, the exact reported defect: `j` (and therefore Down, which
    // fold_selector_keys rewrites to `j`) used to return
    // Some(Some(Dir::Down)) and attach IMMEDIATELY. Scanning the list with
    // the arrow keys finalized a placement the operator never chose. This is
    // the test that had to fail before the fix.
    for key in [b"j".as_slice(), b"\x1b[B".as_slice()] {
        let mut v = unified_rows_view();
        v.selector = Some(8); // bg-claude
        let mut buf = Vec::new();
        selector_keys(&mut v, b"p", &mut buf).await.unwrap();
        assert_eq!(v.attach_place.as_ref().unwrap().cursor, 0);
        attach_place_keys(&mut v, key, &mut buf).await.unwrap();
        assert!(buf.is_empty(), "key {key:?} must send no AttachAgent");
        let picker = v.attach_place.as_ref().expect("picker stays open");
        assert_eq!(picker.cursor, 1, "key {key:?} moves the cursor");
        assert_eq!(picker.target(), Some(2));
    }
    // ...and back up, clamped at the top rather than wrapping.
    let mut v = unified_rows_view();
    v.selector = Some(8);
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    attach_place_keys(&mut v, b"kk", &mut buf).await.unwrap();
    assert!(buf.is_empty());
    assert_eq!(
        v.attach_place.as_ref().unwrap().cursor,
        0,
        "clamped, no wrap"
    );
}

#[tokio::test]
async fn attach_placement_new_tab_and_cancel_are_distinct() {
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    // `t` is Enter's named alias: both open a new tab in the
    // cursor-marked workspace. Space and `.` are the separate "here" pair.
    attach_place_keys(&mut v, b"t", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::AttachAgent {
            id: "c19cd2c3".into(),
            placement: PanePlacement {
                target: PaneTarget::SquadId(1),
                split: None,
                ..Default::default()
            },
        })
    );

    v.selector = Some(8); // bg-claude
    let mut cancelled = Vec::new();
    selector_keys(&mut v, b"p", &mut cancelled).await.unwrap();
    attach_place_keys(&mut v, b"q", &mut cancelled)
        .await
        .unwrap();
    assert!(cancelled.is_empty());
    assert!(v.attach_place.is_none());
}

#[tokio::test]
async fn attach_placement_enter_commits_the_cursor_and_space_attaches_here() {
    // SUPERSEDES x-fbb1's Enter-is-here ruling, which was correct while the
    // picker had no cursor to contradict it. Adding a cursor removed its
    // premise: the overlay drew a marker on one workspace and Enter
    // attached to another, which is this node's own defect one layer up.
    //
    // This re-splits Enter and Space, which had been merged into one
    // "new tab in ›" commit: Enter (and its alias `t`) always commits the
    // cursor; Space (and its alias `.`) always attaches HERE. No key's
    // meaning depends on cursor history either way.
    let mut v = unified_rows_view();
    widen_to_squads(&mut v, 14);
    open_attach_by_click(&mut v).await;
    let mut buf = Vec::new();
    // Drive the cursor somewhere the digits cannot reach, which is the
    // case the cursor exists for and the case the old Enter broke.
    attach_place_keys(&mut v, b"jjjjjjjjj", &mut buf)
        .await
        .unwrap();
    assert_eq!(v.attach_place.as_ref().unwrap().target(), Some(10));
    attach_place_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(buf);
    let msg: ClientMsg = crate::proto::read_msg_sync(&mut cur).unwrap();
    assert_eq!(
        msg,
        ClientMsg::Command(Command::AttachAgent {
            id: "c19cd2c3".into(),
            placement: PanePlacement {
                target: PaneTarget::SquadId(10),
                split: None,
                ..Default::default()
            },
        }),
        "Enter must attach to the marked workspace, not here"
    );
    assert!(v.attach_place.is_none());

    // Space and `.` both ignore the cursor BY DESIGN rather than by
    // accident - including after the cursor has moved, so their meaning
    // is history-independent too.
    for key in [b" ".as_slice(), b".".as_slice()] {
        let mut v = unified_rows_view();
        widen_to_squads(&mut v, 14);
        open_attach_by_click(&mut v).await;
        let mut buf = Vec::new();
        attach_place_keys(&mut v, b"jjj", &mut buf).await.unwrap();
        attach_place_keys(&mut v, key, &mut buf).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
            ClientMsg::Command(Command::AttachAgent { placement, .. }) => {
                assert_eq!(placement.target, PaneTarget::CurrentRoute);
                assert!(placement.here, "key {key:?} is route-anchored");
            }
            other => panic!("expected AttachAgent, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn attach_placement_refuses_stale_target_without_sending() {
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"p", &mut buf).await.unwrap();
    // Park the cursor on squad 2, then delete it out from under the open
    // picker: the cursor guarantees an in-range index, never a live squad.
    v.attach_place.as_mut().unwrap().cursor = 1;
    v.layout.squads.retain(|s| s.id != 2);
    attach_place_keys(&mut v, b"L", &mut buf).await.unwrap();
    assert!(buf.is_empty());
    assert!(v.notice.is_some());
    assert!(v.attach_place.is_none());
}

#[test]
fn the_placement_pickers_fold_shift_arrows_to_their_split_twin() {
    // The arrow twin for shift+HJKL: in the placement pickers' fold, Shift+
    // arrow folds to the UPPERCASE split key, split-across-reads included.
    // Scoped to the pickers - the selector fold still drops a Shift+arrow
    // whole (uppercase J/K reorder rows there).
    let twins = [
        (&b"\x1b[1;2A"[..], b'K'),
        (&b"\x1b[1;2B"[..], b'J'),
        (&b"\x1b[1;2C"[..], b'L'),
        (&b"\x1b[1;2D"[..], b'H'),
    ];
    for (seq, want) in twins {
        let mut esc = Vec::new();
        assert_eq!(
            fold_selector_keys_with_split_arrows(&mut esc, seq),
            vec![want],
            "{seq:?} folds to its uppercase twin"
        );
        assert!(esc.is_empty());
    }
    // Split across reads folds all the same.
    let mut esc = Vec::new();
    let mut keys = Vec::new();
    for chunk in [&b"\x1b"[..], &b"[1;2"[..], &b"D"[..]] {
        keys.extend(fold_selector_keys_with_split_arrows(&mut esc, chunk));
    }
    assert_eq!(keys, b"H".to_vec());
    // The selector fold still drops a Shift+arrow whole.
    let mut esc = Vec::new();
    assert_eq!(fold_selector_keys(&mut esc, b"\x1b[1;2A"), b"");
}

#[test]
fn every_split_key_has_its_hjkl_and_arrow_twin() {
    // The twin guarantee the hints advertise: the picker's split vocabulary
    // (HJKL) covers the four directions and the shift-arrow fold produces
    // exactly that set, so "shift+arrows/HJKL split" cannot silently lose a
    // direction. Movement is the same promise on the plain fold: arrows
    // produce exactly the hjkl set.
    assert_eq!(
        crate::client::placement_pickers::split_dir(b'H'),
        Some(Dir::Left)
    );
    assert_eq!(
        crate::client::placement_pickers::split_dir(b'J'),
        Some(Dir::Down)
    );
    assert_eq!(
        crate::client::placement_pickers::split_dir(b'K'),
        Some(Dir::Up)
    );
    assert_eq!(
        crate::client::placement_pickers::split_dir(b'L'),
        Some(Dir::Right)
    );
    let shift = [
        fold_selector_keys_with_split_arrows(&mut Vec::new(), b"\x1b[1;2A"),
        fold_selector_keys_with_split_arrows(&mut Vec::new(), b"\x1b[1;2B"),
        fold_selector_keys_with_split_arrows(&mut Vec::new(), b"\x1b[1;2D"),
        fold_selector_keys_with_split_arrows(&mut Vec::new(), b"\x1b[1;2C"),
    ];
    assert_eq!(shift.concat(), b"KJHL".to_vec());
    let plain = [
        fold_selector_keys(&mut Vec::new(), b"\x1b[A"),
        fold_selector_keys(&mut Vec::new(), b"\x1b[B"),
        fold_selector_keys(&mut Vec::new(), b"\x1b[D"),
        fold_selector_keys(&mut Vec::new(), b"\x1b[C"),
    ];
    assert_eq!(plain.concat(), b"kjhl".to_vec());
}
