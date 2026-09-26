//! The TV-model acceptance tests: a portal is never a pane; side-effect
//! doors open transient views; the reach's named anchor (`--from`).

use super::*;

use super::portal_tests::thread_core;

#[test]
fn a_view_opens_a_pane_that_is_never_a_portal() {
    // A side effect's screen: the reach lands, the pane carries
    // FNO_VIEW_TRANSIENT in its own argv (re-derivable at adoption), and
    // no `portals` entry exists.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let tabs_before = core.session.squad(1).unwrap().tabs.len();
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.portal_ctl(
        "deadbee1",
        0,
        PanePlacement {
            view: true,
            ..Default::default()
        },
        Some(vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))]),
        tx,
    );
    match rx.blocking_recv().expect("a reply") {
        ServerMsg::Notice { text } => {
            assert!(
                text.contains("view pane ->") && text.contains("target-a"),
                "the landing names the view: {text}"
            );
        }
        other => panic!("expected a landing notice, got {other:?}"),
    }
    assert!(
        core.portals.is_empty(),
        "a view never enters the portals map"
    );
    let view_pane = core
        .panes
        .iter()
        .find(|(_, e)| e.transient_view)
        .map(|(pid, _)| *pid)
        .expect("the view pane exists");
    let entry = &core.panes[&view_pane];
    assert!(
        entry.cmd.as_deref() == Some("cat"),
        "the marker is provenance, not a shell prompt"
    );
    assert_eq!(entry.portal_hold, None, "a view is not a held seat");
    assert_eq!(
        core.attached.get("deadbee1"),
        Some(&view_pane),
        "the row's viewer mapping names the view pane"
    );
    assert_eq!(
        core.session.squad(1).unwrap().tabs.len(),
        tabs_before + 1,
        "the view owns its fresh tab"
    );
    // And it is never stored: an all-transient tab captures nothing.
    let (trees, _active) = core.stored_tab_trees(1).expect("the squad stores");
    assert_eq!(
        trees.len(),
        1,
        "only the operator's shell tab is stored; the view tab is not"
    );
    for pid in core.panes.keys().copied().collect::<Vec<_>>() {
        core.reap_pane(pid);
    }
}

#[test]
fn a_transient_leaf_captures_as_an_ordinal_shell_slot() {
    // A MIXED tab (operator leaf + view leaf) still stores, but the view
    // leaf binds Shell: no owner binding survives that would re-attach
    // the row behind the operator's back at restore.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, p1, _rx) = thread_core();
    let view_pane = core
        .spawn_pane_cmd(
            &[
                "env".to_string(),
                "FNO_VIEW_TRANSIENT=1".to_string(),
                "/bin/cat".to_string(),
            ],
            24,
            40,
            "/tmp/seen",
        )
        .expect("view pane");
    core.attached.insert("deadbee1".into(), view_pane);
    let (sid, ti) = core.session.find_pane(p1).expect("shell pane placed");
    {
        let squad = core.session.squad_mut(sid).unwrap();
        let tab = &mut squad.tabs[ti];
        let leaf = std::mem::replace(&mut tab.root, Node::Leaf(view_pane));
        tab.root = Node::Branch {
            axis: crate::tree::Axis::Horizontal,
            children: vec![(0.5, leaf), (0.5, Node::Leaf(view_pane))],
        };
    }
    let (trees, _active) = core.stored_tab_trees(sid).expect("the squad stores");
    assert_eq!(trees.len(), 1, "the mixed tab is stored");
    let slots = &trees[0].slots;
    let view_slots: Vec<_> = slots
        .iter()
        .filter(|s| s.pane_id == Some(view_pane))
        .collect();
    assert_eq!(view_slots.len(), 1, "the view leaf has exactly one slot");
    assert!(
        matches!(view_slots[0].binding, crate::proto::LayoutBinding::Shell),
        "the view leaf binds Shell, never Fno"
    );
    core.reap_pane(view_pane);
}
