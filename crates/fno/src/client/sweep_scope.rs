//! One sweep choice, from modal row to prune flags (x-cf97, x-688b): the
//! counts the modal offers, and the exact flag expansion each choice
//! promises - pure, so the promised expansion is unit-testable without
//! spawning the verb.

use super::*;

/// One default `mux workspace prune --dry-run --json` reading, as the modal
/// draws it. Each row's count bounds what its tap closes, and each bucket the
/// sweep leaves in place gets a `kept` line naming why, so a board that still
/// looks full says why on screen instead of reading as a broken button.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SweepCounts {
    pub(super) tabs: usize,
    pub(super) used: usize,
    pub(super) dead: usize,
    /// Named-workspace tabs a `--tabs-only --include-named` apply closes.
    pub(super) named: usize,
    /// Stale squad rows a bare prune removes (`pruned_count`).
    pub(super) squads: usize,
    /// One `what count - why` line per non-zero kept bucket.
    pub(super) kept: Vec<String>,
}

/// Read a finished prune's JSON receipt into the message the UI loop acts on.
/// A missing field means the two processes disagree about the JSON shape (a
/// stale deployed binary). The probe fails loud and names the remedy: a zero
/// default would grey a row out and LIE about a population the stale CLI
/// cannot count.
pub(super) fn parse_sweep_receipt(action: SweepAction, receipt: &serde_json::Value) -> SweepMsg {
    let n = |key: &str| receipt[key].as_u64().map(|v| v as usize);
    let stale = || {
        SweepMsg::Failed(
            "prune output missing count fields - stale fno CLI? run fno doctor update".into(),
        )
    };
    match action {
        SweepAction::Counts => {
            let Some(counts) = read_counts(&n) else {
                return stale();
            };
            match receipt["notice"].as_str() {
                Some(notice) if !notice.is_empty() => SweepMsg::Failed(notice.to_string()),
                _ => SweepMsg::Counts(counts),
            }
        }
        SweepAction::Apply(_) => match (n("tabs_closed"), n("members_reaped"), n("pruned_count")) {
            (Some(closed), Some(reaped), Some(removed)) => SweepMsg::Applied {
                closed,
                reaped,
                removed,
            },
            _ => stale(),
        },
    }
}

fn read_counts(n: &impl Fn(&str) -> Option<usize>) -> Option<SweepCounts> {
    let (named_tabs, named) = (n("tabs_skipped_named")?, n("tabs_named_would_close")?);
    let used = n("tabs_used_shells")?;
    let kept = [
        // With the flag off, each spent shell is also counted not-pristine,
        // and the + used shells row already offers it.
        (
            n("tabs_kept_not_pristine")?.saturating_sub(used),
            "not-pristine tabs",
            "agent, command, or unmeasured shell",
        ),
        (
            n("tabs_kept_last_in_squad")?,
            "last tabs",
            "closing one removes its workspace",
        ),
        (n("tabs_kept_zero_panes")?, "empty tabs", "no pane to judge"),
        (
            n("tabs_kept_not_probed")?,
            "unprobed tabs",
            "no server answered",
        ),
        (
            named_tabs.saturating_sub(named),
            "named tabs",
            "last tab, empty, or in use",
        ),
        (
            n("kept_protected")?,
            "workspace rows",
            "live or origin present",
        ),
        (n("kept_unknown")?, "workspace rows", "liveness unknown"),
        (
            n("skipped_named")?,
            "named workspace rows",
            "CLI only: prune --include-named",
        ),
        (n("members_kept_live")?, "agents", "still live"),
        (n("members_kept_unknown")?, "agents", "liveness unknown"),
    ]
    .into_iter()
    .filter(|(count, ..)| *count > 0)
    .map(|(count, what, why)| format!("{what} {count} - {why}"))
    .collect();
    Some(SweepCounts {
        tabs: n("tabs_would_close")?,
        used,
        dead: n("members_reaped")?,
        named,
        squads: n("pruned_count")?,
        kept,
    })
}

/// Build the centered sweep-threads choice modal from one
/// `mux workspace prune --dry-run` reading. A zero count greys its entry out
/// (0 targets, so arrows skip it and a click is swallowed); with every count
/// zero there is nothing to choose, and the header says so. Each row carries
/// its OWN count, and the tap IS the confirmation - a widening is a separate
/// row, never a rider on an existing choice, so the sweep's posture is
/// visible before it acts (x-cf97).
pub(super) fn build_sweep_modal(c: &SweepCounts) -> AuxPopup {
    let (tabs, used, dead) = (c.tabs, c.used, c.dead);
    let any = tabs > 0 || dead > 0 || used > 0;
    // (review) Every "+" row is ADDITIVE on the CLI: its apply also closes
    // the surplus pristine tabs, so the hint names the real total - the
    // count a row shows must bound what its tap closes.
    let choices = [
        (
            format!("tabs ({tabs})"),
            "close surplus shell tabs".to_string(),
            tabs > 0,
            AuxAction::SweepTabs,
        ),
        (
            format!("+ used shells ({used})"),
            format!(
                "close spent shells plus the {tabs} surplus tabs ({} total)",
                tabs + used
            ),
            used > 0,
            AuxAction::SweepUsedShells,
        ),
        (
            format!("dead agents ({dead})"),
            "reap dead member rows".to_string(),
            dead > 0,
            AuxAction::SweepDeadAgents,
        ),
        (
            "both".to_string(),
            "tabs, spent shells, and dead agents".to_string(),
            any,
            AuxAction::SweepBoth,
        ),
        (
            format!("+ named tabs ({})", c.named),
            format!(
                "close named-workspace tabs plus the {tabs} surplus tabs ({} total)",
                tabs + c.named
            ),
            c.named > 0,
            AuxAction::SweepNamed,
        ),
        (
            format!("+ stale squad rows ({})", c.squads),
            format!("remove them plus the {tabs} surplus tabs and {dead} dead agents"),
            c.squads > 0,
            AuxAction::SweepSquads,
        ),
    ];
    let mut rows = vec![PopupRow::Header("sweep threads".into()), PopupRow::Rule];
    let mut actions: Vec<AuxAction> = Vec::new();
    for (label, hint, enabled, action) in choices {
        rows.push(PopupRow::Entry {
            glyph: "♺".into(),
            label,
            hint,
            enabled,
        });
        if enabled {
            actions.push(action);
        }
    }
    if actions.is_empty() {
        rows.push(PopupRow::Header("nothing to sweep".into()));
    }
    if !c.kept.is_empty() {
        rows.push(PopupRow::Rule);
        rows.push(PopupRow::Header("left in place, and why".into()));
        rows.extend(c.kept.iter().cloned().map(PopupRow::Header));
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
pub(super) fn sweep_apply_args(scope: SweepScope, args: &mut Vec<String>) {
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
        // stale squad rows, which only their own row offers to remove.
        SweepScope::Both => {
            args.push("--tabs-only".to_string());
            args.push("--dead-only".to_string());
            args.push("--include-used-shells".to_string());
        }
        // Tab fold only: `--include-named` alone would also remove named
        // squad rows, which the modal names as CLI-only.
        SweepScope::Named => {
            args.push("--tabs-only".to_string());
            args.push("--include-named".to_string());
        }
        // The bare prune, the one scope that removes stale squad rows.
        SweepScope::Squads => {}
    }
}
