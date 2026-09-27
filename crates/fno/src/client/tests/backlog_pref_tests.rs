//! The backlog pref and its board-open chord: the sideline menu rows, the
//! prefix gate, and the Settings card-layout toggle.

use super::*;

#[test]
fn sideline_menu_backlog_rows_track_the_pref_and_name_the_chord() {
    // Off: the toggle row is there, no open row, and the toggle row stays
    // hint-less - the open menu does not run prefix chords (LD9), so the
    // chord is taught on the row that OPENS the board.
    let off = build_sideline_menu(Anchor::Center, None, false);
    assert!(off.popup.rows.iter().any(|row| matches!(
        row,
        PopupRow::Entry { glyph, label, hint, .. }
            if glyph == "☐" && label == "experimental: backlog view" && hint.is_empty()
    )));
    assert!(!off
        .popup
        .rows
        .iter()
        .any(|row| matches!(row, PopupRow::Entry { label, .. } if label == "backlog")));
    // On: exactly one visible backlog row the moment the pref is on (the
    // menu rebuild on toggle keeps it from ever being find-a-rebuild shape),
    // and its hint names the live chord.
    let on = build_sideline_menu(Anchor::Center, None, true);
    let backlog_rows: Vec<&PopupRow> = on
        .popup
        .rows
        .iter()
        .filter(|row| matches!(row, PopupRow::Entry { label, .. } if label == "backlog"))
        .collect();
    assert_eq!(backlog_rows.len(), 1, "one visible backlog row when on");
    match backlog_rows[0] {
        PopupRow::Entry { hint, .. } => {
            let want = crate::keys::key_for("open-backlog-board")
                .map(|k| format!("prefix {k}"))
                .unwrap_or_default();
            assert_eq!(hint, &want, "the row hint is the live chord");
            assert!(!hint.is_empty(), "a resolvable chord must render a hint");
        }
        _ => unreachable!(),
    }
    // The open row's action stays the tap path: OpenBacklogView.
    let backlog_idx = on
        .popup
        .rows
        .iter()
        .position(|row| matches!(row, PopupRow::Entry { label, .. } if label == "backlog"))
        .expect("the open row exists when the pref is on");
    let entries_before = on
        .popup
        .rows
        .iter()
        .take(backlog_idx + 1)
        .filter(|row| matches!(row, PopupRow::Entry { .. }))
        .count();
    assert_eq!(on.actions[entries_before - 1], AuxAction::OpenBacklogView);
}

#[test]
fn backlog_board_opens_pref_gated() {
    let mut v = two_pane_view();
    v.experimental_backlog = false;
    backlog_board::open_pref_gated(&mut v);
    assert!(v.backlog_board.is_none(), "off: nothing opens");
    assert!(
        v.notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("backlog board is off")),
        "off: the notice names how to enable the view"
    );
    v.experimental_backlog = true;
    backlog_board::open_pref_gated(&mut v);
    assert!(v.backlog_board.is_some(), "on: the board opens");
}

#[test]
fn settings_general_lists_the_sideline_card_layout_toggle() {
    let mut v = two_pane_view();
    v.settings_tab = SettingsTab::General;
    let modal = v.build_settings_modal();
    assert!(modal.popup.rows.iter().any(|row| matches!(
        row,
        PopupRow::Entry { label, .. } if label == "sideline card layout"
    )));
    assert!(modal.actions.contains(&AuxAction::ToggleSidelineLayout));
}

#[test]
fn sideline_menu_names_the_sweep_entry_off_dead() {
    let menu = build_sideline_menu(Anchor::Center, None, false);
    let i = menu
        .popup
        .rows
        .iter()
        .position(|row| {
            matches!(
                row,
                PopupRow::Entry { glyph, label, .. }
                    if glyph == "♺" && label == "sweep threads"
            )
        })
        .expect("sweep threads entry");
    let action_i = menu
        .popup
        .rows
        .iter()
        .take(i + 1)
        .filter(|row| matches!(row, PopupRow::Entry { .. }))
        .count()
        - 1;
    assert_eq!(menu.actions[action_i], AuxAction::OpenSweep);
    assert!(crate::popup::menu_glyph_is_bmp("♺"));
    assert!(!crate::popup::menu_glyph_is_bmp("📄"));
    assert_eq!(
        menu.actions
            .iter()
            .filter(|action| **action == AuxAction::Detach)
            .count(),
        1,
        "the global detach slot remains distinct"
    );
}
