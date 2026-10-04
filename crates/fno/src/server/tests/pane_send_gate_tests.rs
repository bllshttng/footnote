//! The pane_send fail-closed gate test family: an unreconciled pane
//! refuses a send and names its label; a labelled pane whose session id
//! resolves to nothing refuses an unaddressed send.
//! Moved verbatim out of server.rs (file budget shrink). Parent helpers
//! resolve through the glob.
use super::super::client_input::PaneInputRequest;
use super::*;
#[test]
fn pane_send_refuses_an_unreconciled_pane_and_names_the_label() {
    // Fail closed: a pane adopted at a fresh id is refused even unaddressed.
    // The assertion is the refusal itself, never "the bytes did not land".
    let (mut core, pane) = template_core();
    {
        let entry = core.panes.get_mut(&pane).unwrap();
        entry.name = Some("bp-f8b1-unplanned".into());
        entry.unreconciled = true;
    }
    match core.pane_send(pane, b"payload", false, None, Ok(Vec::new()), false) {
        ServerMsg::Err { code, msg } => {
            assert_eq!(code, err_code::TARGET_IDENTITY_MISMATCH);
            assert!(
                msg.contains(&format!("pane {pane}")),
                "names the pane: {msg}"
            );
            assert!(
                msg.contains("bp-f8b1-unplanned"),
                "names the label it carries: {msg}"
            );
            assert!(msg.contains("fno mux where"), "names the way out: {msg}");
        }
        other => panic!("expected unreconciled refusal, got {other:?}"),
    }
}

#[test]
fn pane_send_labelled_pane_identity_resolution() {
    // The measured incident shape: a worker label, a readable registry,
    // and no session id joining the two. A plain send used to type
    // straight into whatever the pty now held. The complement: identity is
    // the session uuid, never the name - a uuid-matched send lands despite
    // a stale label, and a different uuid refuses.
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane).unwrap().name = Some("drifter".into());
    let elsewhere = agent_in("sess", pane + 500, Some(AgentBadge::Done), false);
    match core.pane_send(pane, b"payload", false, None, Ok(vec![elsewhere]), false) {
        ServerMsg::Err { code, msg } => {
            assert_eq!(code, err_code::TARGET_IDENTITY_MISMATCH);
            assert!(msg.contains("drifter"), "names the label: {msg}");
            assert!(msg.contains("fno mux where"), "names the way out: {msg}");
        }
        other => panic!("expected unresolved-identity refusal, got {other:?}"),
    }
    core.panes.get_mut(&pane).unwrap().name = Some("kestrel-heir".into());
    let uuid = "01a0ee3f-235d-7671-8fbb-e09af1d5fb52";
    let mut good = agent_in("sess", pane, None, false);
    good.name = "bob".into();
    good.harness_session_id = Some(uuid.into());
    match core.pane_send(pane, b"payload", false, Some(uuid), Ok(vec![good]), false) {
        ServerMsg::Ok => {}
        other => panic!("a uuid-matched send must land, got {other:?}"),
    }
    let mut impostor = agent_in("sess", pane, None, false);
    impostor.name = "bob".into();
    impostor.harness_session_id = Some("d4c0ffee-0000-0000-0000-000000000000".into());
    match core.pane_send(
        pane,
        b"payload",
        false,
        Some(uuid),
        Ok(vec![impostor]),
        false,
    ) {
        ServerMsg::Err { code, msg } => {
            assert_eq!(code, err_code::TARGET_IDENTITY_MISMATCH);
            assert!(msg.contains(uuid), "names the uuid: {msg}");
        }
        other => panic!("a uuid mismatch must refuse, got {other:?}"),
    }
}

#[test]
fn pane_send_addresses_either_id_of_a_split_row() {
    // After the id split the row carries two ids: its own minted fno_id and
    // its harness session id. A send naming EITHER lands; a third id refuses.
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane).unwrap().name = Some("split".into());
    let own_id = "0f6a4b2e-1111-4222-8333-444444444444";
    let harness_id = "01a0ee3f-235d-7671-8fbb-e09af1d5fb52";
    let mut row = agent_in("sess", pane, None, false);
    row.name = "bob".into();
    row.session_id = Some(own_id.into());
    row.harness_session_id = Some(harness_id.into());
    match core.pane_send(
        pane,
        b"payload",
        false,
        Some(own_id),
        Ok(vec![row.clone()]),
        false,
    ) {
        ServerMsg::Ok => {}
        other => panic!("a send naming the row's own fno_id must land, got {other:?}"),
    }
    match core.pane_send(
        pane,
        b"payload",
        false,
        Some(harness_id),
        Ok(vec![row.clone()]),
        false,
    ) {
        ServerMsg::Ok => {}
        other => panic!("a send naming the harness session id must land, got {other:?}"),
    }
    let mut third = agent_in("sess", pane, None, false);
    third.name = "bob".into();
    third.session_id = Some(own_id.into());
    third.harness_session_id = Some(harness_id.into());
    match core.pane_send(
        pane,
        b"payload",
        false,
        Some("d4c0ffee-0000-4000-8000-000000000000"),
        Ok(vec![third.clone()]),
        false,
    ) {
        ServerMsg::Err { code, .. } => {
            assert_eq!(code, err_code::TARGET_IDENTITY_MISMATCH);
        }
        other => panic!("a send naming a third id must refuse, got {other:?}"),
    }

    let _guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let events_dir = std::env::temp_dir().join(format!(
        "fno-pane-input-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&events_dir);
    std::env::set_var("FNO_AGENTS_HOME", &events_dir);
    core.agents = vec![third.clone()];
    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(1);
    core.clients.push(Client {
        id: 1,
        reliable_tx: reply_tx,
        dirty: Default::default(),
        notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        synced_modes: Default::default(),
        view: (1, 5),
        visible: Default::default(),
        dims: (24, 80),
        passive: false,
        last_press: None,
    });
    core.handle(CoreMsg::PaneInput(PaneInputRequest {
        id: 1,
        request_id: 9,
        pane,
        expected_identity: harness_id.into(),
        bytes: b"reply\r".to_vec(),
        agents: Ok(vec![third]),
    }));
    assert!(matches!(
        reply_rx.try_recv().unwrap(),
        ServerMsg::PaneInputResult(receipt)
            if receipt.request_id == 9 && receipt.pane_id == pane && receipt.result == Ok(())
    ));
    let submit_rows = crate::event_store::query_events(
        &events_dir.join("events.jsonl"),
        &crate::event_store::EventQuery::of_types(&["operator_submit"]),
    )
    .unwrap();
    assert_eq!(
        submit_rows.len(),
        1,
        "addressed human input keeps its witness"
    );
    std::env::remove_var("FNO_AGENTS_HOME");
    let _ = std::fs::remove_dir_all(&events_dir);
}

#[test]
fn pane_send_dnd_refuses_plain_and_accepts_hold_pass() {
    // AC17-EDGE: a held pane refuses a plain PaneSend with TARGET_DND and
    // accepts the same send when the caller carries the hold gate's pass.
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane).unwrap().name = Some("held".into());
    let mut held = agent_in("sess", pane, None, false);
    held.name = "held".into();
    held.harness_session_id = Some("target-id".into());
    held.dnd = true;
    let rows = vec![held];

    match core.pane_send(pane, b"payload", false, None, Ok(rows.clone()), false) {
        ServerMsg::Err { code, msg } => {
            assert_eq!(code, err_code::TARGET_DND);
            assert!(msg.contains("DND"), "the refusal names the hold: {msg}");
        }
        other => panic!("expected DND refusal, got {other:?}"),
    }
    // The same write with the gate's pass lands (identity unaddressed, so
    // the remaining guards pass a clean row).
    match core.pane_send(pane, b"payload", false, None, Ok(rows), true) {
        ServerMsg::Ok => {}
        other => panic!("a hold-passed send must land, got {other:?}"),
    }
}
