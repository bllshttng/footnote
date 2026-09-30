//! The pane_send fail-closed gate test family: an unreconciled pane
//! refuses a send and names its label; a labelled pane whose session id
//! resolves to nothing refuses an unaddressed send.
//! Moved verbatim out of server.rs (file budget shrink). Parent helpers
//! resolve through the glob.
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
