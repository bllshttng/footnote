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
    match core.pane_send(pane, b"payload", false, None, Ok(Vec::new())) {
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
fn pane_send_refuses_a_labelled_pane_whose_identity_resolves_nothing() {
    // The measured incident shape: a worker label, a readable registry,
    // and no session id joining the two. A plain send used to type
    // straight into whatever the pty now held.
    let (mut core, pane) = template_core();
    core.session_name = "sess".into();
    core.panes.get_mut(&pane).unwrap().name = Some("drifter".into());
    let elsewhere = agent_in("sess", pane + 500, Some(AgentBadge::Done), false);
    match core.pane_send(pane, b"payload", false, None, Ok(vec![elsewhere])) {
        ServerMsg::Err { code, msg } => {
            assert_eq!(code, err_code::TARGET_IDENTITY_MISMATCH);
            assert!(msg.contains("drifter"), "names the label: {msg}");
            assert!(msg.contains("fno mux where"), "names the way out: {msg}");
        }
        other => panic!("expected unresolved-identity refusal, got {other:?}"),
    }
}
