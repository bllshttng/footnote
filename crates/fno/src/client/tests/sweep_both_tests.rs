//! (x-688b) The sweep modal's "both" row and its flag expansion.

use super::*;

fn counts(tabs: usize, used: usize, dead: usize) -> SweepCounts {
    SweepCounts {
        tabs,
        used,
        dead,
        ..SweepCounts::default()
    }
}

fn entry_labels(modal: &AuxPopup) -> Vec<(String, bool)> {
    modal
        .popup
        .rows
        .iter()
        .filter_map(|row| match row {
            PopupRow::Entry { label, enabled, .. } => Some((label.clone(), *enabled)),
            _ => None,
        })
        .collect()
}

/// The choice modal is centered (not anchored to the menu cell), offers
/// the choices with live counts, and a zero count greys its entry
/// out rather than offering a lie. The used-shell half (x-cf97) is its
/// own row with its own count - never a rider on the tabs half.
#[test]
fn sweep_modal_is_centered_with_live_counts_and_inert_zeroes() {
    let modal = build_sweep_modal(&counts(3, 19, 7));
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
    let labels = entry_labels(&modal);
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

    let half = build_sweep_modal(&counts(0, 0, 2));
    assert_eq!(
        half.popup.targets().len(),
        2,
        "a zero tab count greys its entry out"
    );
    assert_eq!(
        half.actions,
        vec![AuxAction::SweepDeadAgents, AuxAction::SweepBoth]
    );

    let empty = build_sweep_modal(&counts(0, 0, 0));
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
    let used_only = build_sweep_modal(&counts(0, 21, 0));
    assert_eq!(
        used_only.actions,
        vec![AuxAction::SweepUsedShells, AuxAction::SweepBoth]
    );
    assert!(entry_labels(&used_only).contains(&("both".into(), true)));

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

/// The receipt from the operator's board: tabs 0 while 7 tabs were not
/// pristine and 1 sat in a named workspace, beside 3 stale squad rows. The
/// modal draws the kept count with its reason, and offers the named tab and
/// the stale rows, each as its own tap.
#[test]
fn sweep_modal_draws_kept_reasons_and_offers_named_tabs_and_stale_rows() {
    let receipt = serde_json::json!({
        "tabs_would_close": 0,
        "tabs_used_shells": 0,
        "members_reaped": 0,
        "pruned_count": 3,
        "tabs_skipped_named": 1,
        "tabs_named_would_close": 1,
        "tabs_kept_not_pristine": 7,
        "tabs_kept_last_in_squad": 0,
        "tabs_kept_zero_panes": 0,
        "tabs_kept_not_probed": 0,
        "kept_protected": 0,
        "kept_unknown": 0,
        "skipped_named": 0,
        "members_kept_live": 0,
        "members_kept_unknown": 0,
        "notice": null,
    });
    let SweepMsg::Counts(parsed) = parse_sweep_receipt(SweepAction::Counts, &receipt) else {
        panic!("a full receipt must parse into counts");
    };
    let modal = build_sweep_modal(&parsed);
    assert_eq!(
        modal.actions,
        vec![AuxAction::SweepNamed, AuxAction::SweepSquads],
        "only the rows with something to close are tappable"
    );
    let labels = entry_labels(&modal);
    assert!(labels.contains(&("tabs (0)".into(), false)), "{labels:?}");
    assert!(
        labels.contains(&("+ named tabs (1)".into(), true)),
        "{labels:?}"
    );
    assert!(
        labels.contains(&("+ stale squad rows (3)".into(), true)),
        "{labels:?}"
    );
    let headers: Vec<&str> = modal
        .popup
        .rows
        .iter()
        .filter_map(|row| match row {
            PopupRow::Header(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        headers.contains(&"not-pristine tabs 7 - typed in or running"),
        "the kept count and its reason are on screen: {headers:?}"
    );
    assert!(
        !headers.iter().any(|h| h.starts_with("named tabs")),
        "the one named tab is closable, so no named tab is left in place: {headers:?}"
    );
}

/// Each new row expands to its own flags, and a CLI too old to count named
/// tabs refuses the probe instead of greying that row out with a false zero.
#[test]
fn named_and_stale_row_taps_expand_to_their_own_flags() {
    let mut named = Vec::new();
    sweep_apply_args(SweepScope::Named, &mut named);
    assert_eq!(named, vec!["--tabs-only", "--include-named"]);
    let mut squads = Vec::new();
    sweep_apply_args(SweepScope::Squads, &mut squads);
    assert!(squads.is_empty(), "the stale-row tap is the bare prune");

    let old_cli = serde_json::json!({
        "tabs_would_close": 0,
        "tabs_used_shells": 0,
        "members_reaped": 0,
        "pruned_count": 0,
        "tabs_skipped_named": 1,
    });
    let SweepMsg::Failed(reason) = parse_sweep_receipt(SweepAction::Counts, &old_cli) else {
        panic!("a receipt without tabs_named_would_close must refuse");
    };
    assert!(reason.contains("stale fno CLI"), "{reason}");
}
