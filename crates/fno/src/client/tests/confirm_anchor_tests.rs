//! The confirm anchor's identity tiers: the captured sid first, then the
//! pane, and a name two rows answer anchors at the bottom row - never a
//! first-match guess beside an unrelated row. Lives outside client_tests.rs
//! under the file-budget gate.
use super::*;

#[test]
fn confirm_anchor_prefers_identity_and_refuses_duplicate_names() {
    // a stop/remove confirm anchors by the captured sid first, then
    // the pane; a name-only capture whose name two rows answer anchors at the
    // bottom row - never a first-match guess beside an unrelated row.
    let agent = |name: &str, pane_id, sid: Option<&str>| AgentRow {
        squad: Some(1),
        name: name.into(),
        pane_id,
        harness_session_id: sid.map(Into::into),
        ..Default::default()
    };
    let v = view_with_agents(vec![
        agent("dup", None, None),
        agent("dup", Some(7), Some("s-seven")),
        agent("solo", Some(9), None),
    ]);
    let rows = v.term.0 as usize;
    let idx_of = |v: &View, f: fn(&AgentRow) -> bool| {
        v.display_rows()
            .iter()
            .position(|r| matches!(r, DisplayRow::Agent(a) if f(a)))
            .unwrap()
    };
    let seven = idx_of(&v, |a| a.pane_id == Some(7));
    let solo = idx_of(&v, |a| a.name == "solo");
    let action = |kind| ConfirmAction {
        action: kind,
        label: "x".into(),
    };
    let pane_cap = action(ConfirmKind::StopAgent {
        sid: None,
        name: "dup".into(),
        pane_id: Some(7),
    });
    assert_eq!(v.confirm_anchor_row(rows, &pane_cap), seven);
    let sid_cap = action(ConfirmKind::StopAgent {
        sid: Some("s-seven".into()),
        name: "dup".into(),
        pane_id: None,
    });
    assert_eq!(v.confirm_anchor_row(rows, &sid_cap), seven);
    let solo_cap = action(ConfirmKind::StopAgent {
        sid: None,
        name: "solo".into(),
        pane_id: None,
    });
    assert_eq!(v.confirm_anchor_row(rows, &solo_cap), solo);
    let ambiguous = action(ConfirmKind::StopAgent {
        sid: None,
        name: "dup".into(),
        pane_id: None,
    });
    assert_eq!(
        v.confirm_anchor_row(rows, &ambiguous),
        rows - 1,
        "a name two rows answer anchors at the bottom row, never a guess"
    );
}
