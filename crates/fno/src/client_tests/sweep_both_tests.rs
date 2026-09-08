//! (x-688b) The sweep modal's "both" row and its flag expansion.

use super::*;

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
