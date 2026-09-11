//! x-f188 change 7: the update menu/modal restart surface (kept out of the
//! over-budget client_tests.rs; each shrink is banked).

use super::*;

/// x-f188 AC7-HP: two stale components and no update pending -> the menu
/// shows the restart row and the modal names each component with what
/// restart does and what survives, then offers the action.
#[test]
fn update_modal_names_stale_processes_and_offers_restart() {
    let outcome = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: false,
        installed_rev: Some("same".into()),
        source_rev: Some("same".into()),
        changelog: vec![],
        guidance: "installed same is current; 2 running process(es) are older builds".into(),
        degraded: None,
        running: vec![
            RunningRow {
                component: "daemon".into(),
                name: Some("agents home".into()),
                verdict: "stale".into(),
                on_restart: "restarts".into(),
                survives: "workers and panes".into(),
            },
            RunningRow {
                component: "pane-keeper".into(),
                name: Some("main-1991".into()),
                verdict: "stale".into(),
                on_restart: "kept".into(),
                survives: "its pane; current only when that pane ends".into(),
            },
            RunningRow {
                component: "store-keeper".into(),
                name: Some("g".into()),
                verdict: "current".into(),
                on_restart: "cycles; the next read respawns it".into(),
                survives: "the graph on disk".into(),
            },
        ],
        running_stale: 2,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome));
    let labels: Vec<&str> = menu
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        labels.iter().any(|l| *l == "restart: 2 stale, panes kept"),
        "the menu names the stale count: {labels:?}"
    );

    let modal = build_update_modal(Some(&outcome));
    let text: Vec<String> = modal
        .popup
        .rows
        .iter()
        .map(|r| match r {
            PopupRow::Header(h) => h.clone(),
            PopupRow::Entry { label, .. } => label.clone(),
            PopupRow::Rule => "-".into(),
            _ => String::new(),
        })
        .collect();
    let body = text.join("\n");
    assert!(
        body.contains("daemon agents home: restarts; keeps workers and panes"),
        "each stale row names what restart does: {body}"
    );
    assert!(
        body.contains("pane-keeper main-1991: kept; keeps its pane"),
        "{body}"
    );
    assert!(
        !body.contains("store-keeper"),
        "current rows are not listed: {body}"
    );
    assert!(
        body.contains(
            "restart keeps every pane. pane keepers stay on the old build until their pane ends."
        ),
        "{body}"
    );
    assert!(
        modal.actions.contains(&AuxAction::RestartAgents),
        "the modal offers the restart action"
    );
    let entry_label = modal
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .find(|l| *l == "restart now (keeps panes)");
    assert!(entry_label.is_some(), "the action row is present");
}

/// x-f188 AC7-EDGE: a readiness payload from an older fno with no `running`
/// key still parses and offers no restart action.
#[test]
fn update_payload_without_running_key_parses_and_offers_no_restart() {
    let payload = serde_json::json!({
        "update_ready": false,
        "installed_rev": "aaa1111",
        "source_rev": "aaa1111",
        "guidance": "up to date at aaa1111 - no update pending, 0 shell(s) unaffected",
        "degraded": null
    });
    let parsed: UpdateReadiness = serde_json::from_value(payload).expect("parses");
    assert!(parsed.running.is_empty());
    assert_eq!(parsed.running_stale, 0);

    let outcome = UpdateOutcome::Ok(parsed);
    let modal = build_update_modal(Some(&outcome));
    assert!(
        !modal.actions.contains(&AuxAction::RestartAgents),
        "no restart action without stale rows"
    );
}

/// AC3-HP mirrored client-side: not-ready builds the menu with no row.
#[test]
fn sideline_menu_omits_update_row_when_not_ready() {
    let outcome = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: false,
        installed_rev: Some("same".into()),
        source_rev: Some("same".into()),
        changelog: vec![],
        guidance: "up to date at same - no update pending, 0 shell(s) unaffected".into(),
        degraded: None,
        running: vec![],
        running_stale: 0,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome));
    assert!(!menu.actions.contains(&AuxAction::OpenUpdate));
}

/// AC6-EDGE: no probe yet, and a degraded probe, both build a menu that
/// stays interactive - no missing keybinds row, no panic.
#[test]
fn sideline_menu_handles_missing_and_degraded_probe() {
    let none_menu = build_sideline_menu(Anchor::Center, None);
    assert!(!none_menu.actions.contains(&AuxAction::OpenUpdate));
    assert!(none_menu.actions.contains(&AuxAction::OpenKeybinds));

    let degraded = UpdateOutcome::Degraded("update --check: exit 1".into());
    let degraded_menu = build_sideline_menu(Anchor::Center, Some(&degraded));
    let labels: Vec<&str> = degraded_menu
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(labels[0], "update check failed");
    assert_eq!(degraded_menu.actions[0], AuxAction::OpenUpdate);
}

/// Regression: a successfully-parsed probe (Python `--check` always exits
/// 0) can still be internally degraded - `update_ready: false` with
/// `degraded: Some(_)`. That must still surface a menu row rather than
/// silently falling to the `_ => {}` arm, which would hide a real check
/// failure the operator has no other way to see.
#[test]
fn sideline_menu_shows_row_for_ok_but_internally_degraded_probe() {
    let outcome = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: false,
        installed_rev: Some("same".into()),
        source_rev: Some("same".into()),
        changelog: vec![],
        guidance: "update check degraded (fno mux ls --json failed) - ...".into(),
        degraded: Some("fno mux ls --json failed".into()),
        running: vec![],
        running_stale: 0,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome));
    let labels: Vec<&str> = menu
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(labels[0], "update check degraded");
    assert_eq!(menu.actions[0], AuxAction::OpenUpdate);
}
