//! The sideline nests only CHILD rows: a live CHILD indents under its
//! spawner, and a PEER handoff, or a pre-v32 row with no word, roots at
//! depth 0 beside its spawner.

use super::*;

/// Build an AgentRow from its wire JSON: every lineage field carries
/// `#[serde(default)]`, so a minimal object decodes.
fn row_from(json: &str) -> AgentRow {
    serde_json::from_str(json).unwrap()
}

/// Render depths through the same join the client paints with.
fn depths_of(rows: &[AgentRow]) -> Vec<usize> {
    let (order, depths) = lineage_layout(rows, |r| r.harness_session_id.as_deref(), lineage_parent);
    order.iter().map(|&i| depths[i]).collect()
}

/// AC4-HP: a spawner, its CHILD, and its PEER: the child indents one level
/// beneath the spawner; the peer roots beside it.
#[test]
fn a_child_nests_and_a_peer_roots_beside_its_spawner() {
    let rows = vec![
        row_from(r#"{"name":"s","exited":false,"harness_session_id":"s-1"}"#),
        row_from(
            r#"{"name":"c","exited":false,"harness_session_id":"s-2","spawned_by_session":"s-1","lineage_kind":"child"}"#,
        ),
        row_from(
            r#"{"name":"p","exited":false,"harness_session_id":"s-3","spawned_by_session":"s-1","lineage_kind":"peer"}"#,
        ),
    ];
    assert_eq!(depths_of(&rows), vec![0, 1, 0]);
}

/// AC4-EDGE: a pre-v32 row (spawn edge recorded, no lineage word) renders
/// flat, never nested under a spawner it does not wait on.
#[test]
fn a_pre_v32_row_without_a_word_roots_flat() {
    let rows = vec![
        row_from(r#"{"name":"s","exited":false,"harness_session_id":"s-1"}"#),
        row_from(
            r#"{"name":"o","exited":false,"harness_session_id":"s-2","spawned_by_session":"s-1"}"#,
        ),
    ];
    assert_eq!(depths_of(&rows), vec![0, 0]);
}
