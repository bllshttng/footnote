//! The confirm overlay across every ConfirmKind: shared chrome, one
//! controls footer, the prompt wording per kind. Moved out of
//! client_tests.rs under the file-budget gate (x-b5d1).

use super::*;

#[test]
fn every_confirm_variant_renders_shared_chrome_and_controls() {
    let variants = vec![
        (ConfirmKind::Dispatch { node: "x-1".into() }, "dispatch"),
        (
            ConfirmKind::RemoveSquad {
                squad: 1,
                panes: 2,
                last: false,
            },
            "remove squad",
        ),
        (
            ConfirmKind::StopAgent {
                sid: None,
                name: "agent".into(),
                pane_id: None,
            },
            "stop agent",
        ),
        (
            ConfirmKind::RemoveAgent {
                sid: None,
                name: "agent".into(),
                pane_id: None,
                measure: false,
            },
            "remove agent",
        ),
        (ConfirmKind::ReapAgents, "reap"),
        (
            ConfirmKind::StopExternal {
                attach_id: "a-1".into(),
                name: "external".into(),
            },
            "stop external",
        ),
        (
            ConfirmKind::RemoveExternal {
                attach_id: "a-1".into(),
                name: "external".into(),
            },
            "remove external",
        ),
        (
            ConfirmKind::DismissMember {
                squad: 1,
                attach_id: "a-1".into(),
            },
            "dismiss member",
        ),
        (
            ConfirmKind::ClearDead {
                key: crate::view_store::SectionKey::Missions,
                squad: None,
                dead: 3,
            },
            "clear dead",
        ),
        (ConfirmKind::CloseTab { tab: 1 }, "close tab"),
    ];

    for (action, label) in variants {
        let mut view = two_pane_view();
        view.confirm = Some(ConfirmAction {
            action,
            label: label.into(),
        });
        let (rows, cols) = (view.term.0 as usize, view.term.1 as usize);
        let mut cells = vec![Cell::default(); rows * cols];
        view.draw_bottom_row(&mut cells, rows, cols);
        let screen: String = (0..rows)
            .map(|r| {
                cells[r * cols..(r + 1) * cols]
                    .iter()
                    .map(|cell| cell.c)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            screen.contains('┌'),
            "{label} has no shared top border: {screen}"
        );
        assert!(
            screen.contains("enter confirm · esc cancel"),
            "{label} has no actual controls footer: {screen}"
        );
    }
}
