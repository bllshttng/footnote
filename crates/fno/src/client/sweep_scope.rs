//! One sweep choice, from modal row to prune flags (x-cf97, x-688b): the
//! counts the modal offers, and the exact flag expansion each choice
//! promises - pure, so the promised expansion is unit-testable without
//! spawning the verb.

use super::*;

/// Build the centered sweep-threads choice modal from one
/// `mux workspace prune --dry-run` reading: close the surplus pristine
/// tabs, close the opt-in used-shell tabs, reap the dead member rows, or
/// combinations. A zero count greys its entry out (0 targets, so arrows skip
/// it and a click is swallowed); with every count zero there is nothing to
/// choose, and the header says so. Each row carries its OWN count, and the
/// tap IS the confirmation - the used-shell half is a separate row, never a
/// rider on the default tabs half, so the sweep's posture is visible before
/// it acts (x-cf97).
pub(crate) fn build_sweep_modal(tabs: usize, used: usize, dead: usize) -> AuxPopup {
    let choice = |label: String, hint: &str, enabled: bool| PopupRow::Entry {
        glyph: "♺".into(),
        label,
        hint: hint.into(),
        enabled,
    };
    let mut rows = vec![PopupRow::Header("sweep threads".into()), PopupRow::Rule];
    let mut actions: Vec<AuxAction> = Vec::new();
    rows.push(choice(
        format!("tabs ({tabs})"),
        "close surplus shell tabs",
        tabs > 0,
    ));
    if tabs > 0 {
        actions.push(AuxAction::SweepTabs);
    }
    rows.push(choice(
        format!("+ used shells ({used})"),
        // (review) The flag is ADDITIVE on the CLI: the apply closes the
        // spent shells AND the surplus pristine tabs, so the hint names the
        // real total and the row says "+" - the count a row shows must bound
        // what its tap closes.
        &format!(
            "close spent shells plus the {tabs} surplus tabs ({} total)",
            tabs + used
        ),
        used > 0,
    ));
    if used > 0 {
        actions.push(AuxAction::SweepUsedShells);
    }
    rows.push(choice(
        format!("dead agents ({dead})"),
        "reap dead member rows",
        dead > 0,
    ));
    if dead > 0 {
        actions.push(AuxAction::SweepDeadAgents);
    }
    rows.push(choice(
        "both".into(),
        "tabs, spent shells, and dead agents",
        tabs > 0 || dead > 0 || used > 0,
    ));
    if tabs > 0 || dead > 0 || used > 0 {
        actions.push(AuxAction::SweepBoth);
    }
    if actions.is_empty() {
        rows.push(PopupRow::Header("nothing to sweep".into()));
    }
    AuxPopup {
        popup: Popup::new(rows, Anchor::Center)
            .title("sweep threads")
            .footer("esc close"),
        actions,
    }
}


/// The prune flags one sweep scope maps to, pure so the expansion each
/// choice promises is unit-testable without spawning the verb. `both` means
/// all three populations (x-688b): tabs, spent shells, and dead agents -
/// with tabs 0 and used shells 21, a both that skipped the shells half
/// closed nothing while the operator watched.
pub(crate) fn sweep_apply_args(scope: SweepScope, args: &mut Vec<String>) {
    match scope {
        SweepScope::Tabs => args.push("--tabs-only".to_string()),
        // (x-cf97) The opt-in half: tabs-only PLUS the flag that
        // widens the tab fold to spent shells. Never the default.
        SweepScope::UsedShells => {
            args.push("--tabs-only".to_string());
            args.push("--include-used-shells".to_string());
        }
        SweepScope::Dead => args.push("--dead-only".to_string()),
        // Both halves, and nothing else: bare prune would also remove
        // stale squad rows, which the modal never offered to remove.
        SweepScope::Both => {
            args.push("--tabs-only".to_string());
            args.push("--dead-only".to_string());
            args.push("--include-used-shells".to_string());
        }
    }
}
