//! Row-set join tests, moved out of `server_tests.rs` with the
//! driving-session join they assert: the file is over budget and may only
//! shrink, and the join is row_set's question.

use super::{bg_row, empty_core};
use crate::tree::{Node, Tab};
use std::collections::HashMap;

#[test]
fn agent_rows_join_pr_from_holder_map() {
    // A name-resolved row gets its pr without a claim; a holder-only row
    // keeps the harness-native fallback. updated_at passes through.
    let mut core = empty_core();
    core.session_name = "main".into();
    let mut worker = bg_row("t-xdae5-reviewflags-glm", "/w", None);
    worker.updated_at = Some(42);
    core.agents = vec![worker, bg_row("holder-only", "/x", None)];
    core.backlog_holders = HashMap::from([("x-9c5f".to_string(), "holder-only".to_string())]);
    core.backlog_pr = HashMap::from([("x-dae5".to_string(), 999), ("x-9c5f".to_string(), 385)]);
    core.backlog_driver = HashMap::from([("x-dae5".to_string(), "09234474".to_string())]);
    let rows = core.agent_rows();
    let joined = rows
        .iter()
        .find(|r| r.name == "t-xdae5-reviewflags-glm")
        .unwrap();
    assert_eq!(joined.pr, Some(999));
    assert_eq!(joined.updated_at, Some(42));
    // The attach handle joins through the same node resolution as the pr:
    // the row names the driving session even when its own session id differs.
    assert_eq!(joined.pr_session_short.as_deref(), Some("09234474"));
    let fallback = rows.iter().find(|r| r.name == "holder-only").unwrap();
    assert_eq!(fallback.pr, Some(385));
    // No driver map entry behind the fallback's pr: the row says so.
    assert_eq!(fallback.pr_session_short, None);
}

// d-954c2cbf: a spawn joins the spawner's workspace. A live paneless codex
// thread row with a foreign cwd joins the squad its parent edge resolves to:
// the edge names the parent's harness session id, the parent is seated via
// its own cwd, and the child renders there - the child's cwd decides nothing
// (x-3909).
#[test]
fn paneless_row_joins_the_squad_its_spawn_edge_names() {
    let mut core = empty_core();
    core.session_name = "main".into();
    core.session.add_squad(
        1,
        vec!["/repo/footnote".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(42),
            focus: 42,
        },
    );
    let mut parent = bg_row("lead", "/repo/footnote", None);
    parent.harness = Some("claude".into());
    parent.harness_session_id = Some("parent-sid".into());
    let mut child = bg_row("think-thread", "/elsewhere/tools", None);
    child.harness = Some("codex".into());
    child.harness_session_id = Some("child-sid".into());
    child.spawned_by_session = Some("Parent-SID".into());
    core.agents = vec![parent, child];
    let rows = core.agent_rows();
    let row = rows.iter().find(|r| r.name == "think-thread").unwrap();
    assert_eq!(
        row.squad,
        Some(1),
        "the spawn edge attributes the child to the spawner's squad, case-insensitively"
    );
}

// The repro's row: no edge, foreign cwd. It joins nothing - cwd decides
// nothing for it - but the row set still emits it. Absence of the edge stays
// a fact the `~ elsewhere` reader can see.
#[test]
fn paneless_row_with_no_parent_edge_stays_orphaned_but_emitted() {
    let mut core = empty_core();
    core.session_name = "main".into();
    core.session.add_squad(
        1,
        vec!["/repo/footnote".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(42),
            focus: 42,
        },
    );
    let mut child = bg_row("think-thread", "/elsewhere/tools", None);
    child.harness = Some("codex".into());
    child.harness_session_id = Some("child-sid".into());
    core.agents = vec![child];
    let rows = core.agent_rows();
    let row = rows.iter().find(|r| r.name == "think-thread").unwrap();
    assert_eq!(row.squad, None);
    // x-cd47 1.5: same-project checkouts never read foreign. Canonical
    // beside worktree, either direction, and sibling worktrees are one
    // project; a genuinely different repo stays foreign.
    assert!(crate::server::row_set::same_project(
        "/repo/footnote/footnote",
        "/wts/footnote/x-58f7"
    ));
    assert!(crate::server::row_set::same_project(
        "/wts/footnote/x-58f7",
        "/repo/footnote/footnote"
    ));
    assert!(crate::server::row_set::same_project(
        "/wts/footnote/x-58f7",
        "/wts/footnote/x-cd47"
    ));
    assert!(!crate::server::row_set::same_project(
        "/repo/readyrule/regready",
        "/repo/footnote/footnote"
    ));
}

// An edge naming a session no registry row holds resolves nothing: the row
// stays orphaned rather than cycling or guessing.
#[test]
fn parent_edge_join_never_resolves_through_an_absent_parent() {
    let mut core = empty_core();
    core.session_name = "main".into();
    core.session.add_squad(
        1,
        vec!["/repo/footnote".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(42),
            focus: 42,
        },
    );
    let mut child = bg_row("think-thread", "/elsewhere/tools", None);
    child.harness = Some("codex".into());
    child.harness_session_id = Some("child-sid".into());
    child.spawned_by_session = Some("ghost-sid".into());
    core.agents = vec![child];
    let rows = core.agent_rows();
    let row = rows.iter().find(|r| r.name == "think-thread").unwrap();
    assert_eq!(row.squad, None);
}

// An id two parent rows claim that resolve to DIFFERENT squads is ambiguous:
// the child reads as absent rather than picking a confident wrong squad.
#[test]
fn parent_edge_join_reads_an_ambiguous_parent_as_absent() {
    let mut core = empty_core();
    core.session_name = "main".into();
    core.session.add_squad(
        1,
        vec!["/repo/one".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(42),
            focus: 42,
        },
    );
    core.session.add_squad(
        2,
        vec!["/repo/two".into()],
        None,
        Tab {
            name: None,
            id: 2,
            root: Node::Leaf(50),
            focus: 50,
        },
    );
    let mut parent_a = bg_row("lead-a", "/repo/one", None);
    parent_a.harness_session_id = Some("twin-sid".into());
    let mut parent_b = bg_row("lead-b", "/repo/two", None);
    parent_b.harness_session_id = Some("twin-sid".into());
    let mut child = bg_row("think-thread", "/elsewhere/tools", None);
    child.harness = Some("codex".into());
    child.harness_session_id = Some("child-sid".into());
    child.spawned_by_session = Some("twin-sid".into());
    core.agents = vec![parent_a, parent_b, child];
    let rows = core.agent_rows();
    let row = rows.iter().find(|r| r.name == "think-thread").unwrap();
    assert_eq!(row.squad, None);
}
