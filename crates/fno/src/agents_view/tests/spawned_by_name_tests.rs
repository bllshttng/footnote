//! The derived parent NAME: `merge_rows` joins every row's
//! `spawned_by_session` to the row whose `harness_session_id` it names,
//! case- and whitespace-insensitive the way `spawn_edge::live_child_of`
//! matches. An edge naming a session no row holds reads as absent, and an
//! id two DIFFERENT names claim reads as absent too - an ambiguous parent
//! is never rendered as a confident wrong answer.

use super::*;

#[test]
fn an_edge_resolves_to_the_parent_row_name_despite_stray_case() {
    let mut parent = plain_row("t-x-lead", None, false);
    parent.harness_session_id = Some(" S-Lead ".into());
    let mut child = plain_row("jn-t-x-1", None, false);
    child.harness_session_id = Some("s-child".into());
    child.spawned_by_session = Some("s-lead".into());
    let merged = merge_rows(vec![parent, child], &[]);
    let kid = merged.iter().find(|r| r.name == "jn-t-x-1").unwrap();
    assert_eq!(kid.spawned_by_name.as_deref(), Some("t-x-lead"));
    let lead = merged.iter().find(|r| r.name == "t-x-lead").unwrap();
    assert_eq!(lead.spawned_by_name, None, "a root carries no parent name");
}

#[test]
fn an_edge_to_a_session_no_row_holds_reads_absent() {
    let mut child = plain_row("sob-t-x-1-glm", None, false);
    child.spawned_by_session = Some("s-gone".into());
    let merged = merge_rows(vec![child], &[]);
    assert_eq!(merged[0].spawned_by_name, None);
}

#[test]
fn an_id_claimed_by_two_names_reads_absent() {
    let mut first = plain_row("t-x-first", None, false);
    first.harness_session_id = Some("s-twin".into());
    let mut second = plain_row("t-x-second", None, false);
    second.harness_session_id = Some("S-TWIN".into());
    let mut child = plain_row("jn-t-x-1", None, false);
    child.spawned_by_session = Some("s-twin".into());
    let merged = merge_rows(vec![first, second, child], &[]);
    let kid = merged.iter().find(|r| r.name == "jn-t-x-1").unwrap();
    assert_eq!(
        kid.spawned_by_name, None,
        "an ambiguous parent reads as absent, never as the last row walked"
    );
}

#[test]
fn a_parked_fork_child_names_its_primary() {
    let parent = "11111111-1111-4111-8111-111111111111";
    let parked = "22222222-2222-4222-8222-222222222222";
    let mut primary = plain_row("primary", None, false);
    primary.harness_session_id = Some(parent.into());
    primary.related_session_id = Some(parked.into());
    let roster = vec![RosterWorker {
        short_id: parked.into(),
        name: "parked-worker".into(),
        cwd: "/w".into(),
        account: None,
    }];
    let merged = merge_rows(vec![primary], &roster);
    let child = merged
        .iter()
        .find(|r| r.harness_session_id.as_deref() == Some(parked))
        .expect("the parked id renders as its own row");
    assert_eq!(
        child.spawned_by_name.as_deref(),
        Some("primary"),
        "the synthesized fork names the worker it forked from"
    );
}
