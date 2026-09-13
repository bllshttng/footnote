use super::*;

#[test]
fn pane_send_accepts_the_dedicated_thread_viewer_identity() {
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane).unwrap().name = Some("thread".into());
    core.portals.insert(
        0,
        Portal {
            row_key: "thread-id".into(),
            seat: pane,
            tab: 5,
        },
    );
    let mut thread = agent_in("other", 99, None, false);
    thread.name = "thread".into();
    thread.mux = None;
    thread.session_id = Some("thread-id".into());

    assert!(matches!(
        core.pane_send(pane, b"payload", false, Some("thread-id"), Ok(vec![thread]),),
        ServerMsg::Ok
    ));
}

#[test]
fn fno_id_for_pane_uses_thread_viewer_portal() {
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane).unwrap().name = Some("thread".into());
    core.portals.insert(
        0,
        Portal {
            row_key: "CODEX-THREAD".into(),
            seat: pane,
            tab: 5,
        },
    );
    let mut thread = agent_in("thread", 99, None, false);
    thread.mux = None;
    thread.session_id = Some("CODEX-THREAD".into());
    core.agents = vec![thread];

    assert_eq!(core.fno_id_for_pane(pane), Some("CODEX-THREAD".into()));
}

#[test]
fn portal_seat_refusal_keys_on_the_claude_attach_argv() {
    // AC3 (x-3ea6): the refusal is a function of the seat's row and the
    // child's argv. Fixture argv, so no live claude is needed.
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    core.portals.insert(
        0,
        Portal {
            row_key: "deadbee1".into(),
            seat: pane,
            tab: 5,
        },
    );
    let mut row = agent_in("sess", 99, None, false);
    row.name = "worker".into();
    row.mux = None;
    row.session_id = Some("worker-id".into());
    row.harness = Some("claude".into());
    row.attach_id = Some("deadbee1".into());
    let rows = vec![row];

    // Still attached - the isolated-account argv passes before env execs.
    let good = [
        "env",
        "claude",
        "attach",
        "deadbee1",
        "--settings",
        "/s.json",
    ];
    let good: Vec<String> = good.iter().map(|s| s.to_string()).collect();
    assert!(core.portal_seat_refusal(pane, &rows, Some(good)).is_none());

    // Detached to agent view: refuses, naming pane, row and argv.
    let drifted: Vec<String> = ["claude", "agents"].iter().map(|s| s.to_string()).collect();
    let refusal = core
        .portal_seat_refusal(pane, &rows, Some(drifted))
        .expect("a drifted seat refuses");
    assert!(
        refusal.contains("worker")
            && refusal.contains("deadbee1")
            && refusal.contains("claude agents"),
        "{refusal}"
    );

    // Unreadable child refuses too (fail closed).
    let refusal = core
        .portal_seat_refusal(pane, &rows, None)
        .expect("an unreadable child refuses");
    assert!(refusal.contains("an unreadable process"), "{refusal}");

    // A codex row (no attach_id) is unchanged, whatever its child runs.
    let mut codex = agent_in("worker", 99, None, false);
    codex.mux = None;
    codex.session_id = Some("codex-id".into());
    codex.harness = Some("codex".into());
    let codex_rows = vec![codex];
    let drifted2: Vec<String> = ["claude", "agents"].iter().map(|s| s.to_string()).collect();
    assert!(core
        .portal_seat_refusal(pane, &codex_rows, Some(drifted2))
        .is_none());
}

/// The drifted-to-agent-view argv the AC3 cases share.
fn drifted_argv() -> Vec<String> {
    ["claude", "agents"].iter().map(|s| s.to_string()).collect()
}

/// A claude Drive row keyed by its attach id on the portal under test.
fn claude_portal_row() -> crate::agents_view::RegistryAgent {
    let mut row = agent_in("sess", 99, None, false);
    row.name = "worker".into();
    row.mux = None;
    row.session_id = Some("worker-id".into());
    row.harness = Some("claude".into());
    row.attach_id = Some("deadbee1".into());
    row
}

#[test]
fn the_send_gate_refuses_a_claude_portal_seat_that_left_its_worker() {
    // AC3-ERR (x-3ea6): the seat's child runs /bin/cat, not `attach
    // deadbee1`; every programmatic send refuses, addressed or not, naming
    // the pane, the row and its argv, and the seat answers no identity.
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane).unwrap().name = Some("thread".into());
    core.portals.insert(
        0,
        Portal {
            row_key: "deadbee1".into(),
            seat: pane,
            tab: 5,
        },
    );
    let rows = vec![claude_portal_row()];

    for expected in [Some("worker-id"), None] {
        match core.pane_send(pane, b"probe", false, expected, Ok(rows.clone())) {
            ServerMsg::Err { code, msg } => {
                assert_eq!(code, err_code::TARGET_IDENTITY_MISMATCH, "{msg}");
                assert!(
                    msg.contains("worker")
                        && msg.contains("deadbee1")
                        && msg.contains("the viewer left that session"),
                    "{msg}"
                );
            }
            other => panic!("expected an Err refusal, got {other:?}"),
        }
    }
    assert_eq!(core.fno_id_for_pane(pane), None);
}

#[test]
fn pane_ls_publishes_thread_identity_on_the_dedicated_viewer() {
    let (mut core, pane_id) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane_id).unwrap().name = Some("thread".into());
    core.portals.insert(
        0,
        Portal {
            row_key: "thread-id".into(),
            seat: pane_id,
            tab: 5,
        },
    );
    let mut thread = agent_in("other", 99, None, false);
    thread.name = "thread".into();
    thread.mux = None;
    thread.session_id = Some("thread-id".into());

    match core.pane_ls_from_fresh_agents(Some(&[thread])) {
        ServerMsg::PaneList { panes } => {
            let pane = panes.iter().find(|pane| pane.pane_id == pane_id).unwrap();
            assert_eq!(pane.fno_id.as_deref(), Some("thread-id"));
        }
        other => panic!("pane ls should identify the thread viewer, got {other:?}"),
    }
}
