//! The update menu/modal restart and release-upgrade surface (kept out of
//! the over-budget client_tests.rs; each shrink is banked).

use super::tests::view_with_agents;
use super::*;
use crate::client::release_check::{Channel, ReleaseOutcome};
use crate::client::update_menu::{RunningRow, UpdateOutcome, UpdateProbe, UpdateReadiness};

fn degraded_release_probe(release: ReleaseOutcome, running: Vec<RunningRow>) -> UpdateProbe {
    let running_stale = running.len();
    UpdateProbe {
        readiness: UpdateOutcome::Ok(UpdateReadiness {
            update_ready: false,
            installed_rev: None,
            source_rev: None,
            changelog: vec![],
            guidance: "update check degraded (source checkout not resolvable)".into(),
            degraded: Some("source checkout not resolvable".into()),
            running,
            running_stale,
            source_pin: None,
        }),
        release,
    }
}

fn entry_labels(popup: &AuxPopup) -> Vec<String> {
    popup
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.clone()),
            _ => None,
        })
        .collect()
}

fn newer_uv() -> ReleaseOutcome {
    ReleaseOutcome::Newer {
        channel: Channel::Uv,
        installed: "0.3.1".into(),
        latest: "0.3.2".into(),
    }
}

/// A release install with a degraded source check still learns a newer
/// release exists, and the modal's upgrade entry maps to its action.
#[test]
fn release_newer_shows_menu_row_and_modal_upgrade_entry() {
    let probe = degraded_release_probe(newer_uv(), vec![]);
    let menu = build_sideline_menu(Anchor::Center, Some(&probe));
    assert_eq!(entry_labels(&menu)[0], "release 0.3.2 available");
    assert_eq!(menu.actions[0], AuxAction::OpenUpdate);

    let modal = build_update_modal(Some(&probe));
    let labels = entry_labels(&modal);
    let i = labels
        .iter()
        .position(|l| l == "upgrade now: uv tool upgrade fno")
        .expect("upgrade entry");
    assert_eq!(modal.actions[i], AuxAction::UpgradeRelease(Channel::Uv));
}

/// With stale running rows too, actions follow entry order.
#[test]
fn release_newer_with_stale_rows_orders_upgrade_before_restart() {
    let stale = |name: &str| RunningRow {
        component: "daemon".into(),
        name: Some(name.into()),
        verdict: "stale".into(),
        on_restart: "restarts".into(),
        survives: "panes".into(),
    };
    let probe = degraded_release_probe(newer_uv(), vec![stale("a"), stale("b")]);
    let modal = build_update_modal(Some(&probe));
    assert_eq!(
        modal.actions,
        vec![
            AuxAction::UpgradeRelease(Channel::Uv),
            AuxAction::RestartAgents
        ]
    );
    assert_eq!(
        entry_labels(&modal),
        vec![
            "upgrade now: uv tool upgrade fno",
            "restart now (keeps panes)"
        ]
    );
}

/// A failed release check is named, never read as current.
#[test]
fn release_degraded_and_current_render_one_header_and_no_action() {
    for (release, want) in [
        (
            ReleaseOutcome::Degraded("uv tool list --outdated: exit 2".into()),
            "release check failed: uv tool list --outdated: exit 2",
        ),
        (
            ReleaseOutcome::Current {
                channel: Channel::Brew,
            },
            "release: current (brew)",
        ),
    ] {
        let modal = build_update_modal(Some(&degraded_release_probe(release, vec![])));
        assert!(
            modal
                .popup
                .rows
                .iter()
                .any(|r| matches!(r, PopupRow::Header(h) if h == want)),
            "{want}"
        );
        assert!(modal.actions.is_empty());
    }
}

/// A tap queues the upgrade once; a second tap says one is running; the
/// verdict lands as a notice and re-arms the probe.
#[tokio::test]
async fn upgrade_tap_queues_once_and_verdict_rearms_probe() {
    let mut v = view_with_agents(vec![]);
    let mut buf = Vec::new();
    execute_aux_action(&mut v, AuxAction::UpgradeRelease(Channel::Uv), &mut buf)
        .await
        .unwrap();
    assert_eq!(v.update_verb_want, Some(UpdateVerb::Upgrade(Channel::Uv)));
    v.update_verb_want = None;
    v.update_verb_inflight = true;
    execute_aux_action(&mut v, AuxAction::RestartAgents, &mut buf)
        .await
        .unwrap();
    assert_eq!(v.update_verb_want, None);
    assert!(v
        .notice
        .as_ref()
        .is_some_and(|(n, _)| n == "an update action is already running"));

    v.update_probe_want = false;
    v.land_update_verdict("upgrade ok: Updated fno v0.3.1 -> v0.3.2".into());
    assert!(!v.update_verb_inflight);
    assert!(v.update_probe_want);
    assert!(v
        .notice
        .as_ref()
        .is_some_and(|(n, _)| n.starts_with("upgrade ok:")));
}

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
        source_pin: None,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome.clone().into()));
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
        labels.contains(&"restart: 2 stale, panes kept"),
        "the menu names the stale count: {labels:?}"
    );

    let modal = build_update_modal(Some(&outcome.clone().into()));
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
    let modal = build_update_modal(Some(&outcome.clone().into()));
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
        source_pin: None,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome.clone().into()));
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
    let degraded_menu = build_sideline_menu(Anchor::Center, Some(&degraded.into()));
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
        source_pin: None,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome.clone().into()));
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

/// AC7-HP: a behind source with no update pending outranks the
/// restart row - the menu names the distance and offers the modal first.
#[test]
fn sideline_menu_names_source_behind_origin() {
    let payload = serde_json::json!({
        "update_ready": false,
        "installed_rev": "aaa1111",
        "source_rev": "aaa1111",
        "guidance": "source checkout aaa1111 is 14 commit(s) behind origin/main bbb2222; \
                     merged changes there are not installed. Sync it, then run fno doctor update",
        "degraded": null,
        "running": [],
        "running_stale": 2,
        "source_pin": {"behind": 14}
    });
    let parsed: UpdateReadiness = serde_json::from_value(payload).expect("parses");
    let outcome = UpdateOutcome::Ok(parsed);
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome.clone().into()));
    let labels: Vec<&str> = menu
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(labels[0], "source 14 behind origin");
    assert_eq!(menu.actions[0], AuxAction::OpenUpdate);
}

/// AC8-EDGE: a payload with no `source_pin` key, or with a null
/// `behind`, parses and builds today's rows (no behind row).
#[test]
fn sideline_menu_without_source_pin_keeps_rows() {
    for pin in [serde_json::Value::Null, serde_json::json!({"behind": null})] {
        let mut payload = serde_json::json!({
            "update_ready": false,
            "installed_rev": "same",
            "source_rev": "same",
            "guidance": "up to date at same - no update pending, 0 shell(s) unaffected",
            "degraded": null,
            "running": [],
            "running_stale": 0
        });
        if !pin.is_null() {
            payload["source_pin"] = pin.clone();
        }
        let parsed: UpdateReadiness = serde_json::from_value(payload).expect("parses");
        let menu = build_sideline_menu(Anchor::Center, Some(&UpdateOutcome::Ok(parsed).into()));
        assert!(
            !menu.actions.contains(&AuxAction::OpenUpdate),
            "no behind row for pin {pin:?}"
        );
    }
}

/// AC5-HP (moved from the over-budget client_tests.rs): a ready outcome puts
/// the update row above keybinds.
#[test]
fn sideline_menu_shows_update_row_above_keybinds_when_ready() {
    let outcome = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: true,
        installed_rev: Some("aaa1111".into()),
        source_rev: Some("bbb2222".into()),
        changelog: vec!["fix(x): thing".into()],
        guidance: "update ready bbb2222 - wire unchanged - 14 shells survive".into(),
        degraded: None,
        running: vec![],
        running_stale: 0,
        source_pin: None,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome.clone().into()));
    let labels: Vec<&str> = menu
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(labels[0], "update ready");
    assert_eq!(labels[1], "sweep threads");
    assert_eq!(labels[2], "keybinds");
    assert_eq!(menu.actions[0], AuxAction::OpenUpdate);
}

/// AC5-HP (moved from the over-budget client_tests.rs): the overlay carries
/// the version pair, changelog, and guidance.
#[test]
fn update_modal_renders_version_pair_changelog_and_guidance() {
    let outcome = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: true,
        installed_rev: Some("aaa1111".into()),
        source_rev: Some("bbb2222".into()),
        changelog: vec!["fix(x): thing".into(), "feat(y): other thing".into()],
        guidance: "update ready bbb2222 - wire unchanged - 14 shells survive".into(),
        degraded: None,
        running: vec![],
        running_stale: 0,
        source_pin: None,
    });
    let modal = build_update_modal(Some(&outcome.clone().into()));
    let headers: Vec<&str> = modal
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Header(h) => Some(h.as_str()),
            _ => None,
        })
        .collect();
    assert!(headers.contains(&"aaa1111 -> bbb2222"));
    assert!(headers.contains(&"fix(x): thing"));
    assert!(headers.contains(&"feat(y): other thing"));
    assert!(headers.iter().any(|h| h.contains("14 shells survive")));
}
