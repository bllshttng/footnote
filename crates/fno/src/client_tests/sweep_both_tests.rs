//! (x-688b) The sweep modal's "both" row and its flag expansion.

use super::*;

/// The choice modal is centered (not anchored to the menu cell), offers
/// the choices with live counts, and a zero count greys its entry
/// out rather than offering a lie. The used-shell half (x-cf97) is its
/// own row with its own count - never a rider on the tabs half.
#[test]
fn sweep_modal_is_centered_with_live_counts_and_inert_zeroes() {
    let modal = build_sweep_modal(3, 19, 7);
    assert_eq!(modal.popup.anchor, Anchor::Center);
    assert_eq!(modal.popup.targets().len(), 4, "{:?}", modal.popup.rows);
    assert_eq!(
        modal.actions,
        vec![
            AuxAction::SweepTabs,
            AuxAction::SweepUsedShells,
            AuxAction::SweepDeadAgents,
            AuxAction::SweepBoth
        ]
    );
    let labels: Vec<(String, bool)> = modal
        .popup
        .rows
        .iter()
        .filter_map(|row| match row {
            PopupRow::Entry { label, enabled, .. } => Some((label.clone(), *enabled)),
            _ => None,
        })
        .collect();
    assert!(labels.contains(&("tabs (3)".into(), true)), "{labels:?}");
    assert!(
        labels.contains(&("+ used shells (19)".into(), true)),
        "{labels:?}"
    );
    assert!(
        labels.contains(&("dead agents (7)".into(), true)),
        "{labels:?}"
    );
    assert!(labels.contains(&("both".into(), true)), "{labels:?}");

    let half = build_sweep_modal(0, 0, 2);
    assert_eq!(
        half.popup.targets().len(),
        2,
        "a zero tab count greys its entry out"
    );
    assert_eq!(
        half.actions,
        vec![AuxAction::SweepDeadAgents, AuxAction::SweepBoth]
    );

    let empty = build_sweep_modal(0, 0, 0);
    assert_eq!(empty.popup.targets().len(), 0, "{:?}", empty.popup.rows);
    assert!(empty
        .popup
        .rows
        .iter()
        .any(|row| matches!(row, PopupRow::Header(text) if text == "nothing to sweep")));
}

/// (x-688b) "Both" means both: with the measured workspace (tabs 0, dead 0,
/// used shells 21) the both row is enabled, and the apply it queues carries
/// all three flags - the old expansion excluded used shells, so an operator
/// pressing both closed nothing while 21 sat there.
#[test]
fn sweep_both_row_enables_on_used_shells_alone_and_applies_all_three() {
    let used_only = build_sweep_modal(0, 21, 0);
    assert_eq!(
        used_only.actions,
        vec![AuxAction::SweepUsedShells, AuxAction::SweepBoth]
    );
    assert!(used_only.popup.rows.iter().any(|row| match row {
        PopupRow::Entry { label, enabled, .. } => label == "both" && *enabled,
        _ => false,
    }));

    let mut both_args = Vec::new();
    sweep_apply_args(SweepScope::Both, &mut both_args);
    assert_eq!(
        both_args,
        vec![
            "--tabs-only".to_string(),
            "--dead-only".to_string(),
            "--include-used-shells".to_string(),
        ],
        "both must include the used-shells widening"
    );
    let mut used_args = Vec::new();
    sweep_apply_args(SweepScope::UsedShells, &mut used_args);
    assert_eq!(
        used_args,
        vec![
            "--tabs-only".to_string(),
            "--include-used-shells".to_string(),
        ]
    );
    let mut dead_args = Vec::new();
    sweep_apply_args(SweepScope::Dead, &mut dead_args);
    assert_eq!(dead_args, vec!["--dead-only".to_string()]);
}
