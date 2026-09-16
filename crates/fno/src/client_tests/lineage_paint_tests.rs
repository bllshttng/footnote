//! The lineage paint families: the sideline nests a CHILD under its
//! spawner, keeps subtrees together across sorts and densities, and roots
//! a child whose parent row does not render. Shared fixtures
//! (`crowned_row`, `rendered_depth`, ...) resolve through the parent
//! tests module.

use super::*;

/// A crowned_row carrying a lineage edge: `parent` names another row's
/// harness_session_id (None = a root). The row's own id is "sid-<name>".
fn lineage_row(name: &str, pane: u64, parent: Option<&str>) -> AgentRow {
    let mut r = crowned_row(name, pane, None, None);
    r.harness_session_id = Some(format!("sid-{name}"));
    r.spawned_by_session = parent.map(str::to_string);
    // A named parent in these fixtures is a CHILD edge: that is the edge
    // the sideline nests on.
    r.lineage_kind = parent.map(|_| "child".to_string());
    r
}

#[test]
fn extended_sort_keeps_lineage_subtrees_together() {
    let mut v = wide_view(vec![
        lineage_row("zeta", 4, None),
        lineage_row("alpha", 5, Some("sid-zeta")),
        lineage_row("aardvark", 6, None),
    ]);
    set_density(&mut v, Density::Extended);
    v.agent_sort = AgentSort::Squad;
    let names = agent_order(&v);
    assert_eq!(names, ["aardvark", "zeta", "alpha"]);
    assert_eq!(rendered_depth(&v, "zeta"), 0);
    assert_eq!(rendered_depth(&v, "alpha"), 1);
}

#[test]
fn extended_table_preserves_lineage_depth_in_rendered_agent_names() {
    let mut v = wide_view(vec![
        lineage_row("parent", 4, None),
        lineage_row("child", 5, Some("sid-parent")),
    ]);
    set_density(&mut v, Density::Extended);
    let rendered = frame_text(&v.compose());
    let child_line = rendered
        .lines()
        .find(|line| line.contains("child"))
        .unwrap();
    assert!(
        child_line.contains("  child"),
        "child keeps lineage indent: {child_line:?}"
    );
}

#[test]
fn lineage_child_sorts_beneath_its_parent_within_squad() {
    let v = view_with_agents(vec![
        lineage_row("worker-a", 2, Some("sid-king")),
        lineage_row("king", 3, None),
        lineage_row("worker-b", 4, Some("sid-king")),
    ]);
    // Pre-order: the parent first, its children beneath it keeping input
    // order among siblings. Authority rank (crown_level) no longer moves a
    // row; lineage does.
    assert_eq!(agent_order(&v), vec!["king", "worker-a", "worker-b"]);
}

#[test]
fn lineage_grandchild_renders_between_parent_and_later_sibling() {
    let v = view_with_agents(vec![
        lineage_row("king", 2, None),
        lineage_row("child-a", 3, Some("sid-king")),
        lineage_row("child-b", 4, Some("sid-king")),
        lineage_row("grandchild", 5, Some("sid-child-a")),
    ]);
    // Pre-order nests the grandchild under ITS parent, ahead of the
    // parent's later sibling.
    assert_eq!(
        agent_order(&v),
        vec!["king", "child-a", "grandchild", "child-b"]
    );
}

#[test]
fn lineage_indent_is_depth_within_squad() {
    let v = view_with_agents(vec![
        lineage_row("king", 2, None),
        lineage_row("dir", 3, Some("sid-king")),
        lineage_row("ic", 4, Some("sid-dir")),
    ]);
    let steps = |name: &str| rendered_depth(&v, name);
    assert_eq!(steps("king"), 0);
    assert_eq!(steps("dir"), 1);
    assert_eq!(steps("ic"), 2);

    // A parent and a stranger leaf: the leaf is a ROOT (absent parent),
    // never nested under a row it has no edge to.
    let v2 = view_with_agents(vec![
        lineage_row("king", 2, None),
        lineage_row("stranger", 3, None),
    ]);
    let steps2 = |name: &str| rendered_depth(&v2, name);
    assert_eq!(steps2("king"), 0);
    assert_eq!(steps2("stranger"), 0);
}

#[test]
fn lineage_indent_ignores_exited_parent_hidden_by_liveonly() {
    // The indent must reference only rows that render: an exited parent
    // dropped by a LiveOnly squad is ABSENT from the set, so its child
    // roots rather than indenting under a phantom.
    let mut parent = lineage_row("parent", 2, None);
    parent.exited = true;
    let child = lineage_row("child", 3, Some("sid-parent"));
    let mut v = view_with_agents(vec![parent, child]);
    let indent = |v: &View, name: &str| rendered_depth(v, name);
    // Expanded: the exited parent still renders, so the child indents.
    assert_eq!(indent(&v, "child"), 1);
    // LiveOnly hides the exited parent -> absent from the rendered set ->
    // the child is a root.
    v.cycle_squad(1);
    assert_eq!(v.squad_view(1), SectionView::LiveOnly);
    assert_eq!(
        indent(&v, "child"),
        0,
        "no phantom indent under a hidden parent"
    );
}

#[test]
fn lineage_nests_within_elsewhere_and_roots_strangers() {
    // `~ elsewhere` carries the same lineage join as the squads: an
    // orphan spawned by another orphan nests beneath it, while an unrelated
    // orphan stays flat (absent parent = root, never nested under a
    // stranger).
    let mut parent = lineage_row("orphan-parent", 2, None);
    parent.squad = None;
    let mut child = lineage_row("orphan-child", 3, Some("sid-orphan-parent"));
    child.squad = None;
    let mut stranger = lineage_row("orphan-stranger", 4, None);
    stranger.squad = None;
    // `~ elsewhere` defaults to Collapsed; open it so the nesting this
    // test asserts actually renders (the depth vec only covers rows that
    // paint - that is the point of the compose-pass design).
    let mut v = view_with_agents(vec![parent, child, stranger]);
    v.section_view
        .insert(SectionKey::Elsewhere, SectionView::Expanded);
    let indent = |name: &str| rendered_depth(&v, name);
    assert_eq!(indent("orphan-parent"), 0);
    assert_eq!(indent("orphan-child"), 1, "a child nests under its parent");
    assert_eq!(
        indent("orphan-stranger"),
        0,
        "no edge to either row: a root, not nested under a stranger"
    );
}
