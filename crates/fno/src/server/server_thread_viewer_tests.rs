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
