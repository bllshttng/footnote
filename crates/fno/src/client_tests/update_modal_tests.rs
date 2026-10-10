//! The update menu/modal restart and release-upgrade surface (kept out of
//! the over-budget client_tests.rs; each shrink is banked).

use super::tests::view_with_agents;
use super::*;
use crate::client::release_check::{Channel, ReleaseNotesSection, ReleaseOutcome};
use crate::client::update_menu::{
    ReleaseNoteLine, ReleaseNotes, ReleaseNotesGroup, RunningRow, UpdateOutcome, UpdateProbe,
    UpdateReadiness,
};

fn degraded_release_probe(release: ReleaseOutcome, running: Vec<RunningRow>) -> UpdateProbe {
    let running_stale = running.len();
    UpdateProbe {
        readiness: UpdateOutcome::Ok(UpdateReadiness {
            update_ready: false,
            installed_rev: None,
            source_rev: None,
            installed_version: None,
            source_prs_ahead: None,
            changelog: vec![],
            release_notes: None,
            guidance: "update check degraded (local source tree unavailable)".into(),
            degraded: Some("local source tree unavailable".into()),
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

/// Every row's visible label (headers, body text, entries): for assertions
/// that care about presence, not row kind.
fn row_labels(popup: &AuxPopup) -> Vec<String> {
    popup
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Header(h) | PopupRow::Text(h) | PopupRow::FullWidth(h) => Some(h.clone()),
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
        notes: vec![],
    }
}

/// A release install with a degraded source check still learns a newer
/// release exists, and the modal's upgrade entry maps to its action.
#[test]
fn release_newer_shows_menu_row_and_modal_upgrade_entry() {
    let probe = degraded_release_probe(newer_uv(), vec![]);
    let menu = build_sideline_menu(Anchor::Center, Some(&probe), false);
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
/// verdict lands as a notice and re-arms the probe. The restart tap queues
/// nothing: nothing inflight, it detaches with the restart armed (run_inner
/// runs the foreground restart); anything inflight, it says so and stays.
#[tokio::test]
async fn upgrade_tap_queues_once_and_verdict_rearms_probe() {
    let mut v = view_with_agents(vec![]);
    let mut buf = Vec::new();
    assert!(matches!(
        execute_aux_action(&mut v, AuxAction::RestartAgents, &mut buf)
            .await
            .unwrap(),
        DispatchFlow::Detach
    ));
    assert!(v.restart_pending, "the clean tap arms the restart unwind");
    assert_eq!(v.update_verb_want, None, "the restart never queues");
    v.restart_pending = false;
    execute_aux_action(&mut v, AuxAction::UpgradeRelease(Channel::Uv), &mut buf)
        .await
        .unwrap();
    assert_eq!(v.update_verb_want, Some(Channel::Uv));
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
        installed_version: None,
        source_prs_ahead: None,
        changelog: vec![],
        release_notes: None,
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
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome.clone().into()), false);
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

    // Twenty stale pane keepers fold into one count line; full revs cut to
    // ten characters; no row runs past the popup width.
    let mut wide = outcome.clone();
    if let UpdateOutcome::Ok(r) = &mut wide {
        r.installed_rev = Some("a".repeat(40));
        r.source_rev = Some("b".repeat(40));
        let keeper = r.running[1].clone();
        r.running.extend(std::iter::repeat_n(keeper, 19));
        r.changelog
            .push(format!("feat: {}", "a long subject ".repeat(8)));
    }
    let wide = build_update_modal(Some(&wide.into()));
    let headers: Vec<&str> = wide
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Header(h) => Some(h.as_str()),
            _ => None,
        })
        .collect();
    assert!(headers.contains(&"aaaaaaaaaa -> bbbbbbbbbb"), "{headers:?}");
    assert_eq!(
        row_labels(&wide)
            .iter()
            .filter(|h| h.contains("keeper"))
            .count(),
        2,
        "the count line and the promise: {headers:?}"
    );
    assert!(
        row_labels(&wide).contains(&"20 pane keepers on the old build".to_string()),
        "{headers:?}"
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
    let body = text.join(" ");
    assert!(
        body.contains("daemon agents home: restarts; keeps workers and panes"),
        "each stale row names what restart does: {body}"
    );
    assert!(body.contains("1 pane keeper on the old build"), "{body}");
    assert!(
        !body.contains("store-keeper"),
        "current rows are not listed: {body}"
    );
    assert!(
        body.contains("restart keeps panes. keepers stay on the old build until their pane ends."),
        "{body}"
    );
    assert!(
        body.contains(
            "restart detaches, runs `fno agents restart --mux` in the foreground, reattaches."
        ),
        "the modal names the flow the tap starts: {body}"
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

/// The guard: no overlay cuts its text with an ellipsis. Each overlay opens
/// with long text over a sideline of short names, renders at 200 columns and
/// at 50, and every screen line must hold no `…` and fit the screen. The long
/// text must still be all there once the wrapped lines are joined.
#[test]
fn no_overlay_cuts_text_with_an_ellipsis() {
    use super::tests::{agent_row, agent_row_at, named_meta, shot_view};
    let long = format!(
        "{}see https://example.com/{}end",
        "a long subject ".repeat(8),
        "path/".repeat(20)
    );
    let outcome = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: false,
        installed_rev: Some("a".repeat(40)),
        source_rev: Some("b".repeat(40)),
        installed_version: None,
        source_prs_ahead: None,
        changelog: vec![format!("feat: {long}")],
        release_notes: None,
        guidance: long.clone(),
        degraded: None,
        running: vec![],
        running_stale: 0,
        source_pin: None,
    });
    let name = "n".repeat(120);
    type Open = Box<dyn Fn(&mut View)>;
    let opens: Vec<(&str, &str, Open)> = vec![
        (
            "update modal",
            "https://example.com/",
            Box::new({
                let outcome = outcome.clone();
                move |v| v.aux = Some(build_update_modal(Some(&outcome.clone().into())))
            }),
        ),
        (
            "rename",
            name.as_str(),
            Box::new({
                let name = name.clone();
                move |v| v.rename = Some((RenameTarget::Squad(1), name.clone()))
            }),
        ),
        (
            "peek",
            "https://example.com/",
            Box::new({
                let long = long.clone();
                move |v| {
                    v.peek = Some(PeekView {
                        cursor: agent_row_at(v, |a| a.name == "w"),
                        seq: 1,
                        body: Some(vec![long.clone()]),
                        name: "w".into(),
                        last_fetch: std::time::Instant::now(),
                        refresh_pending: false,
                        squad: None,
                    })
                }
            }),
        ),
        (
            "confirm",
            "https://example.com/",
            Box::new({
                let long = long.clone();
                move |v| {
                    v.confirm = Some(ConfirmAction {
                        action: ConfirmKind::StopAgent {
                            sid: None,
                            name: "w".into(),
                            pane_id: None,
                        },
                        label: long.clone(),
                    })
                }
            }),
        ),
    ];
    for cols in [200u16, 50] {
        for (label, whole, open) in &opens {
            let mut v = shot_view(
                (40, cols),
                vec![named_meta(1, "footnote", &["main"], 0)],
                vec![agent_row("w", 10, None, false)],
            );
            open(&mut v);
            let text = crate::vt::frame_text(&v.compose());
            for line in text.lines() {
                assert!(
                    !line.contains('\u{2026}'),
                    "{label} cut text at {cols} columns: {line:?}"
                );
                assert!(
                    crate::chrome::str_cols(line) <= cols as usize,
                    "{label} runs past {cols} columns: {line:?}"
                );
            }
            // The overlay's own lines, joined with the frame and the wrap
            // spaces squeezed out, still hold the long text whole.
            let body: String = match v.active_overlay_layout() {
                Some(l) => l.framed.lines.iter().map(|l| l.text.as_str()).collect(),
                None => v.aux.as_ref().map_or(String::new(), |a| {
                    a.popup
                        .render(v.term)
                        .lines
                        .iter()
                        .map(|l| l.text.as_str())
                        .collect()
                }),
            };
            let squeeze = |s: &str| -> String {
                s.chars()
                    .filter(|c| !c.is_whitespace() && *c != '│')
                    .collect()
            };
            assert!(
                squeeze(&body).contains(&squeeze(whole)),
                "{label} lost text at {cols} columns: {text}"
            );
        }
    }
    let wide = build_update_modal(Some(&outcome.clone().into()));
    assert!(
        wide.popup.render((80, 200)).width > crate::popup::WIDTH_CAP + 4,
        "the update modal grows past the old cap"
    );
}

/// Only card message previews use ellipsis; every other row field keeps the no-marker clipping rule (d-36438ea4).
#[test]
fn no_row_cuts_text_with_an_ellipsis() {
    use super::tests::{agent_row, named_meta, shot_view};

    let long_name = "n".repeat(120);
    let long_tail = format!("{} end", "a long worker message ".repeat(12));
    let long_cwd = "c".repeat(60);
    let mut long_agent = agent_row(&long_name, 10, None, false);
    long_agent.tail = Some(long_tail.clone());
    long_agent.cwd_base = Some(long_cwd.clone());
    let plain = agent_row("w", 11, None, false);

    for cols in [50u16, 80, 120, 200] {
        for density in [
            crate::view_store::Density::Regular,
            crate::view_store::Density::Extended,
            crate::view_store::Density::Slim,
        ] {
            let mut v = shot_view(
                (40, cols),
                vec![named_meta(1, "footnote", &["main"], 0)],
                vec![long_agent.clone(), plain.clone()],
            );
            v.density = density;
            let text = crate::vt::frame_text(&v.compose());
            for line in text.lines() {
                assert!(
                    !line.contains('\u{2026}') || line.contains(" tok · …"),
                    "{density:?} cut a row at {cols} columns: {line:?}"
                );
            }
        }

        // The backlog board's stats and card lines clip too.
        let mut b = super::backlog_board::BoardView::new(0);
        let rows = vec![
            serde_json::json!({"id": "x-1", "status": "ready", "priority": "p2", "title": long_tail}),
            serde_json::json!({"id": "x-2", "status": "in_progress", "priority": "p2", "title": long_tail}),
        ];
        b.inputs = Some(crate::backlog_model::fixture(rows));
        let q = b.query.to_query().expect("the default query parses");
        b.body = Some(crate::backlog_model::board(b.inputs.as_ref().unwrap(), &q));
        let (lines, _) = super::backlog_board::render(&b, cols as usize);
        for line in &lines {
            assert!(
                !line.text.contains('\u{2026}'),
                "board cut a row at {cols} columns: {:?}",
                line.text
            );
        }

        // A pane border with a long name and long edge fields.
        let fields = crate::pane_border::EdgeFields {
            name: &long_name,
            status: Some(('●', "Working")),
            model: Some("claude-opus-5-5[1m]"),
            node: Some(&long_cwd),
            branch: Some("feature/x-a38d-fixed-height-mux-rows"),
            ctx: Some("ctx 48% of 1.0M"),
        };
        let e = crate::pane_border::edges(
            &fields,
            crate::tree::Rect {
                x: 0,
                y: 0,
                cols,
                rows: 12,
            },
            false,
        );
        for edge in [&e.top, &e.bottom] {
            let text: String = edge.iter().map(|(c, _)| *c).collect();
            assert!(
                !text.contains('\u{2026}'),
                "pane border cut a span at {cols} columns: {text:?}"
            );
        }
    }
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
        installed_version: None,
        source_prs_ahead: None,
        changelog: vec![],
        release_notes: None,
        guidance: "up to date at same - no update pending, 0 shell(s) unaffected".into(),
        degraded: None,
        running: vec![],
        running_stale: 0,
        source_pin: None,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome.clone().into()), false);
    assert!(!menu.actions.contains(&AuxAction::OpenUpdate));
}

/// AC6-EDGE: no probe yet, and a degraded probe, both build a menu that
/// stays interactive - no missing keybinds row, no panic.
#[test]
fn sideline_menu_handles_missing_and_degraded_probe() {
    let none_menu = build_sideline_menu(Anchor::Center, None, false);
    assert!(!none_menu.actions.contains(&AuxAction::OpenUpdate));
    assert!(none_menu.actions.contains(&AuxAction::OpenKeybinds));

    let degraded = UpdateOutcome::Degraded("update --check: exit 1".into());
    let degraded_menu = build_sideline_menu(Anchor::Center, Some(&degraded.into()), false);
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
        installed_version: None,
        source_prs_ahead: None,
        changelog: vec![],
        release_notes: None,
        guidance: "update check degraded (fno mux ls --json failed) - ...".into(),
        degraded: Some("fno mux ls --json failed".into()),
        running: vec![],
        running_stale: 0,
        source_pin: None,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome.clone().into()), false);
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
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome.clone().into()), false);
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
        let menu = build_sideline_menu(
            Anchor::Center,
            Some(&UpdateOutcome::Ok(parsed).into()),
            false,
        );
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
        installed_version: None,
        source_prs_ahead: None,
        changelog: vec!["fix(x): thing".into()],
        release_notes: None,
        guidance: "update ready bbb2222 - wire unchanged - 14 shells survive".into(),
        degraded: None,
        running: vec![],
        running_stale: 0,
        source_pin: None,
    });
    let menu = build_sideline_menu(Anchor::Center, Some(&outcome.clone().into()), false);
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
    assert_eq!(labels[2], "new agent");
    assert_eq!(labels[3], "experimental: backlog view");
    assert_eq!(labels[4], "keybindings");
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
        installed_version: None,
        source_prs_ahead: None,
        changelog: vec!["fix(x): thing".into(), "feat(y): other thing".into()],
        release_notes: None,
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
    assert!(row_labels(&modal).contains(&"fix(x): thing".to_string()));
    assert!(row_labels(&modal).contains(&"feat(y): other thing".to_string()));
    assert!(row_labels(&modal)
        .iter()
        .any(|h| h.contains("14 shells survive")));

    // Shaped notes win over the raw changelog: highlights lead as tappable
    // Entries carrying OpenPr (actions pair with selectable rows by index),
    // area groups follow as Headers, the hidden count renders, and a notes
    // payload with no rows falls back to the raw subjects.
    let notes = ReleaseNotes {
        highlights: vec![ReleaseNoteLine {
            pr: Some(105),
            url: Some("https://github.com/o/r/pull/105".into()),
            text: "card rows".into(),
        }],
        groups: vec![ReleaseNotesGroup {
            area: "mux".into(),
            lines: vec![ReleaseNoteLine {
                pr: Some(104),
                url: None,
                text: "stop the crash".into(),
            }],
        }],
        hidden_line: Some("3 test/docs/ci/chore PRs hidden".into()),
    };
    let outcome = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: true,
        installed_rev: Some("aaa1111".into()),
        source_rev: Some("bbb2222".into()),
        installed_version: None,
        source_prs_ahead: None,
        changelog: vec!["fix(x): raw subject".into()],
        release_notes: Some(notes),
        guidance: "update ready bbb2222 - wire unchanged - 14 shells survive".into(),
        degraded: None,
        running: vec![],
        running_stale: 0,
        source_pin: None,
    });
    let modal = build_update_modal(Some(&outcome.clone().into()));
    let entry_labels: Vec<&str> = modal
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    // Only the URL line is an Entry; no stale rows means no restart row.
    assert_eq!(entry_labels, vec!["card rows (#105)"]);
    let headers: Vec<&str> = modal
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Header(h) => Some(h.as_str()),
            _ => None,
        })
        .collect();
    assert!(headers.contains(&"mux"));
    assert!(row_labels(&modal).contains(&"stop the crash (#104)".to_string()));
    assert!(row_labels(&modal).contains(&"3 test/docs/ci/chore PRs hidden".to_string()));
    assert!(!row_labels(&modal).contains(&"fix(x): raw subject".to_string()));
    assert_eq!(
        modal.actions,
        vec![AuxAction::OpenPr("https://github.com/o/r/pull/105".into())]
    );

    let empty = UpdateOutcome::Ok(UpdateReadiness {
        update_ready: true,
        installed_rev: Some("aaa1111".into()),
        source_rev: Some("bbb2222".into()),
        installed_version: None,
        source_prs_ahead: None,
        changelog: vec!["fix(x): raw subject".into()],
        release_notes: Some(ReleaseNotes {
            highlights: vec![],
            groups: vec![],
            hidden_line: None,
        }),
        guidance: "update ready bbb2222 - wire unchanged - 14 shells survive".into(),
        degraded: None,
        running: vec![],
        running_stale: 0,
        source_pin: None,
    });
    let modal = build_update_modal(Some(&empty.clone().into()));
    assert!(row_labels(&modal).contains(&"fix(x): raw subject".to_string()));
}

#[test]
fn readiness_payload_from_the_native_verb_parses_for_the_tui() {
    let _lock = crate::model_catalog::state_env_lock();
    let tmp = std::env::temp_dir().join(format!("fno-du-parse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("FNO_STATE_DIR", &tmp);
    let before = std::env::var_os("FNO_AGENTS_BIN");
    std::env::set_var("FNO_AGENTS_BIN", "/usr/bin/false");
    let payload = crate::doctor_update::update_readiness(None);
    std::env::remove_var("FNO_STATE_DIR");
    match before {
        Some(v) => std::env::set_var("FNO_AGENTS_BIN", v),
        None => std::env::remove_var("FNO_AGENTS_BIN"),
    }
    let _ = std::fs::remove_dir_all(&tmp);
    let parsed: Result<UpdateReadiness, _> = serde_json::from_value(payload.clone());
    assert!(parsed.is_ok());
    assert!(payload.get("probes").is_some());
}

/// The source modal leads with the installed version and the distance in
/// PRs; an older payload (or a zero/unknown distance) falls back to the
/// bare sha pair.
#[test]
fn update_modal_shows_version_and_pr_distance_or_falls_back() {
    let ready = |version: Option<&str>, ahead: Option<u64>| {
        UpdateOutcome::Ok(UpdateReadiness {
            update_ready: true,
            installed_rev: Some("af56e2f24e".into()),
            source_rev: Some("6c3024996f".into()),
            installed_version: version.map(str::to_string),
            source_prs_ahead: ahead,
            changelog: vec![],
            release_notes: None,
            guidance: "update ready - detach, fno doctor update, reattach".into(),
            degraded: None,
            running: vec![],
            running_stale: 0,
            source_pin: None,
        })
    };
    let headers = |probe: UpdateProbe| -> Vec<String> {
        build_update_modal(Some(&probe))
            .popup
            .rows
            .iter()
            .filter_map(|r| match r {
                PopupRow::Header(h) => Some(h.clone()),
                _ => None,
            })
            .collect()
    };
    let has = |probe: UpdateProbe, want: &str| headers(probe).iter().any(|h| h == want);
    assert!(has(
        ready(Some("0.4.1"), Some(2)).into(),
        "fno 0.4.1 at af56e2f24e, main is 2 PRs ahead"
    ));
    assert!(has(
        ready(Some("0.4.1"), Some(1)).into(),
        "fno 0.4.1 at af56e2f24e, main is 1 PR ahead"
    ));
    for fallback in [
        ready(None, None),
        ready(Some("0.4.1"), Some(0)),
        ready(None, Some(2)),
    ] {
        assert!(has(fallback.into(), "af56e2f24e -> 6c3024996f"));
        // A release install has neither rev: no sha line at all.
        let bare = UpdateOutcome::Ok(UpdateReadiness {
            update_ready: false,
            installed_rev: None,
            source_rev: None,
            installed_version: Some("0.4.0".into()),
            source_prs_ahead: None,
            changelog: vec![],
            release_notes: None,
            guidance: "release install refreshes with the upgrade command".into(),
            degraded: Some("local source tree unavailable".into()),
            running: vec![],
            running_stale: 0,
            source_pin: None,
        });
        assert!(!headers(bare.into()).iter().any(|h| h.contains("->")));
    }
}

/// A newer release renders the GitHub release body under the version pair:
/// intro prose and area bullets as Headers (a release body carries no PR
/// urls), and the upgrade entry stays the one action.
#[test]
fn release_newer_renders_release_body_notes() {
    let release = ReleaseOutcome::Newer {
        channel: Channel::Uv,
        installed: "0.4.0".into(),
        latest: "0.4.1".into(),
        notes: vec![
            ReleaseNotesSection {
                area: String::new(),
                bullets: vec!["42 merged pull requests since v0.4.0.".into()],
            },
            ReleaseNotesSection {
                area: "mux".into(),
                bullets: vec!["Portals open operator-owned windows".into()],
            },
        ],
    };
    let modal = build_update_modal(Some(&degraded_release_probe(release, vec![])));
    let headers: Vec<&str> = modal
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Header(h) => Some(h.as_str()),
            _ => None,
        })
        .collect();
    assert!(headers.contains(&"release 0.4.0 -> 0.4.1 (uv)"));
    assert!(row_labels(&modal).contains(&"- 42 merged pull requests since v0.4.0.".to_string()));
    assert!(headers.contains(&"mux"));
    assert!(row_labels(&modal).contains(&"- Portals open operator-owned windows".to_string()));
    assert_eq!(
        entry_labels(&modal),
        vec!["upgrade now: uv tool upgrade fno"]
    );
    assert_eq!(modal.actions, vec![AuxAction::UpgradeRelease(Channel::Uv)]);
}
