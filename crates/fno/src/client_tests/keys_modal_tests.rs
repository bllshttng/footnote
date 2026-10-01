//! The which-key modal render and the chords that open the menu-only
//! surfaces (kept out of the over-budget client_tests.rs; each shrink is
//! banked).

use super::keys_modal::{build_keys_modal, keys_modal_keys};
use super::tests::two_pane_view;
use super::*;
use crate::vt::frame_text;

#[test]
fn which_key_lists_the_dead_row_removal_verbs() {
    // (x-f300) The gap this node closed was discoverability: if the modal
    // stops naming these, removal is invisible again.
    let modal = build_keys_modal();
    let labels: Vec<String> = modal
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { glyph, label, .. } => Some(format!("{glyph} {label}")),
            PopupRow::Header(h) => Some(h.clone()),
            _ => None,
        })
        .collect();
    let joined = labels.join("\n");
    assert!(joined.contains("sideline rows"), "the section renders");
    assert!(joined.contains("x stop a live row · remove a dead one"));
    assert!(joined.contains("X reap all exited agents"));
    // (x-7683) The context-menu row names every trigger, not just the
    // right-click, and keeps the header-only clear-dead behavior named
    // too - the only in-app documentation that a header's menu offers it.
    assert!(joined.contains("context menu · or m · or hold L 500ms · on a header: clear dead"));
    // Display-only: Enter on them must BEL, never dispatch a bogus chord.
    // Matched by label: bare `X` on a sideline row is display-only, while
    // prefix+X (questions-show-done) is a real chord that shares the glyph.
    for (i, r) in modal.popup.rows.iter().enumerate() {
        if matches!(r, PopupRow::Entry { glyph, label, .. } if glyph == "X" && label.contains("reap"))
        {
            assert!(
                modal.row_events[i].is_none(),
                "a bare sideline key is not a prefix chord"
            );
        }
    }
    // The tail notes and the glyph legend each sit under their own header
    // with one blank line before it, like the binding sections.
    for head in ["right click", "sideline glyphs"] {
        let at = labels.iter().position(|l| l == head).expect(head);
        assert_eq!(labels[at - 1], "", "a blank line before {head}");
    }
    // Fit to the screen: no row ellipsizes at 160 columns or at 50; at 50 the
    // widest rows wrap into inert continuations, and every binding row keeps
    // its own chord.
    for cols in [160u16, 50] {
        let fitted = build_keys_modal().fit(cols);
        assert_eq!(fitted.popup.rows.len(), fitted.row_events.len());
        let r = fitted.popup.render((200, cols));
        assert!(
            r.lines.iter().all(|l| !l.text.contains('\u{2026}')),
            "no ellipsis at {cols} columns"
        );
        let bound = |m: &KeysModal| {
            m.popup
                .rows
                .iter()
                .zip(&m.row_events)
                .filter_map(|(row, ev)| match (row, ev) {
                    (PopupRow::Entry { glyph, .. }, Some(ev)) => Some((glyph.clone(), ev.clone())),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(bound(&fitted), bound(&modal), "chords follow at {cols}");
        if cols == 50 {
            assert!(
                fitted.popup.rows.len() > modal.popup.rows.len(),
                "rows wrap"
            );
        }
    }
}

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
    assert!(text.contains("Keybindings"), "chrome title present");
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

/// Mouse motion alone never scrolls or moves a menu: Move reports over every
/// cell of a short terminal leave each popup's scroll and origin where they
/// were, while a wheel report over the keys modal still scrolls it.
#[tokio::test]
async fn hover_never_scrolls_a_menu() {
    use super::tests::{agent_row_at, blocked_row, view_with_agents};
    fn popup(v: &View) -> &Popup {
        let m = v.keys_modal.as_ref().map(|m| &m.popup);
        m.or(v.row_menu.as_ref().map(|m| &m.popup))
            .or(v.aux.as_ref().map(|m| &m.popup))
            .expect("a menu is open")
    }
    let opens: [(&str, fn(&mut View)); 4] = [
        ("menu", |v| v.open_sideline_menu(Anchor::Center)),
        ("settings", |v| v.aux = Some(v.build_settings_modal())),
        ("keys modal", |v| v.open_keys_modal()),
        ("row menu", |v| {
            let i = agent_row_at(v, |a| a.name == "w");
            assert!(v.open_row_menu(i, Anchor::At { row: 1, col: 1 }));
        }),
    ];
    for (name, open) in opens {
        let mut v = view_with_agents(vec![blocked_row("w", 10, None)]);
        v.term = (8, 80);
        open(&mut v);
        let (mut scanner, mut carry, mut buf) = (Scanner::default(), Vec::new(), Vec::new());
        if name == "keys modal" {
            let wheel = b"\x1b[<65;40;4M";
            handle_stdin(&mut v, &mut scanner, &mut carry, wheel, &mut buf)
                .await
                .unwrap();
            assert!(popup(&v).scroll > 0, "a wheel report scrolls");
        }
        let before = (popup(&v).scroll, popup(&v).render(v.term).origin);
        for row in 1..=8 {
            for col in 1..=80 {
                v.compose();
                let report = format!("\x1b[<35;{col};{row}M");
                handle_stdin(
                    &mut v,
                    &mut scanner,
                    &mut carry,
                    report.as_bytes(),
                    &mut buf,
                )
                .await
                .unwrap();
                let after = (popup(&v).scroll, popup(&v).render(v.term).origin);
                assert_eq!(after, before, "{name}: Move at ({row}, {col})");
            }
        }
        assert!(buf.is_empty(), "{name}: motion sends nothing to a pane");
    }
}
