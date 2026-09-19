//! The backlog lane's card-facing tests: the id-first card label, the scope
//! subline under the `~ backlog` header, and the card menu (float, defer,
//! plan, open plan). Moved out of `client_tests.rs` with the lane change
//! they assert - the file is over budget and may only shrink.

use super::tests::{bcard, blocked_row, two_pane_view, view_with_agents};
use super::*;
use crate::backlog_view::card_label;
use crate::vt::frame_text;

#[test]
fn a_pr_row_names_the_session_driving_it() {
    // The operator's ask: a PR row shows the attach handle with the short
    // id of its driving session; no session id reads "no session".
    let mut a = blocked_row("w1", 3, None);
    a.pr = Some(9);
    // The server joins the driving session's short id from the graph: the
    // live claim holder's session, else the node's last do/ship session.
    a.pr_session_short = Some("09234474".into());
    let view = view_with_agents(vec![a]);
    let frame = view.compose();
    let text = frame_text(&frame);
    assert!(
        text.contains("attach 09234474"),
        "the row carries the attach handle, got:\n{text}"
    );

    let mut b = blocked_row("w2", 4, None);
    b.pr = Some(10);
    b.pr_session_short = None;
    let view = view_with_agents(vec![b]);
    let text = frame_text(&view.compose());
    assert!(
        text.contains("no session"),
        "a PR row with no session says so, got:\n{text}"
    );
}

#[test]
fn card_label_leads_with_the_id() {
    // The id is the handle every verb takes, so it leads and the slug
    // follows; an empty slug renders the id alone.
    let mk = |id: &str, slug: &str| -> String {
        card_label(&BacklogCard {
            id: id.into(),
            slug: slug.into(),
            priority: "p1".into(),
            state: CardState::Ready,
            pane_id: None,
            attach_id: None,
            where_hint: None,
            project: None,
            lane: None,
            plan_path: None,
            head: false,
        })
    };
    assert_eq!(
        mk("n1", "agent-native-backlog-view"),
        "n1 agent-native-backlog-view"
    );
    assert_eq!(mk("n2", ""), "n2");
}

#[test]
fn the_backlog_header_states_its_scope() {
    // The scope reason rides under the header as the section's subline.
    let mut view = two_pane_view();
    view.layout.backlog = vec![BacklogCard {
        id: "x-1".into(),
        slug: "feat".into(),
        priority: "p1".into(),
        state: CardState::Ready,
        pane_id: None,
        attach_id: None,
        where_hint: None,
        project: None,
        lane: None,
        plan_path: None,
        head: false,
    }];
    view.expand_pull_sections();
    let rows = view.display_rows();
    let hdr = rows
        .iter()
        .position(|r| matches!(r, DisplayRow::Header { label, .. } if label.contains("backlog")))
        .expect("the backlog header paints");
    match rows.get(hdr + 1) {
        Some(DisplayRow::Sub(s)) => assert!(
            s.starts_with("scope: "),
            "the subline names the scope, got: {s}"
        ),
        other => panic!(
            "the scope subline is not the row under the backlog header at {} (got {:?})",
            hdr + 1,
            other.is_some()
        ),
    }
}

#[test]
fn card_menu_open_plan_follows_ld7_grey_versus_absent() {
    let mut card = bcard("x-a", CardState::Ready);
    let off = crate::digest_overlay::ObsidianCfg::default();
    let on = crate::digest_overlay::ObsidianCfg {
        enabled: true,
        // Absolute, so resolution never depends on the test host's HOME.
        vault: Some("/tmp/vault".into()),
    };

    // Obsidian disabled: the item cannot apply no matter what the operator
    // does in this menu, so LD7 says absent, never greyed. Rows: Header,
    // Rule, Float, Defer, Plan.
    card.plan_path = Some("/tmp/vault/plans/x-a.md".into());
    let m = build_card_menu(&card, &off, Anchor::Center);
    assert_eq!(
        m.popup.rows.len(),
        5,
        "no open-plan row when obsidian is off"
    );
    assert_eq!(
        m.actions.len(),
        3,
        "no OpenPlan action when obsidian is off"
    );

    // No plan_path: state can change (a plan can be added later), so LD7
    // says greyed with the reason, not absent.
    card.plan_path = None;
    let m = build_card_menu(&card, &on, Anchor::Center);
    match &m.popup.rows[5] {
        PopupRow::Entry {
            label,
            hint,
            enabled,
            ..
        } => {
            assert_eq!(label, "Open plan");
            assert_eq!(hint, "no plan");
            assert!(!enabled);
        }
        other => panic!("expected the open-plan entry, got {other:?}"),
    }
    assert_eq!(
        m.actions.len(),
        3,
        "a disabled entry contributes no action slot"
    );

    // Plan present and obsidian on: enabled, and the fourth action lines up
    // with the fourth selectable target (after the Plan entry).
    card.plan_path = Some("/tmp/vault/plans/x-a.md".into());
    let m = build_card_menu(&card, &on, Anchor::Center);
    match &m.popup.rows[5] {
        PopupRow::Entry { label, enabled, .. } => {
            assert_eq!(label, "Open plan");
            assert!(enabled);
        }
        other => panic!("expected the open-plan entry, got {other:?}"),
    }
    assert_eq!(m.actions.len(), 4);
    assert_eq!(m.actions[3], MenuAction::OpenPlan);
    // The Plan entry rides between Defer and Open plan, action-aligned.
    match &m.popup.rows[4] {
        PopupRow::Entry { label, enabled, .. } => {
            assert_eq!(label, "Plan");
            assert!(enabled);
        }
        other => panic!("expected the plan entry, got {other:?}"),
    }
    assert_eq!(m.actions[2], MenuAction::PlanSpawn);
}
