//! The TV-model acceptance tests: a portal is never a pane; side-effect
//! doors open transient views; the restore prune sweeps the stand-ins an
//! older server minted.

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

#[test]
fn restore_prunes_the_old_stand_in_shapes() {
    // The 17:47Z shape, minted the old way: portalN shells, orphaned held
    // screens, a paneless row's shell stand-in. All close, operator-shaped.
    // A pane whose child runs a child of its own (the operator typed vim
    // into the portal2 shell) is never a candidate.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let pn = core.spawn_pane(24, 40, "/tmp/seen").expect("portalN shell");
    let held = core
        .spawn_pane_cmd(
            &[
                "env".to_string(),
                "FNO_PORTAL_HELD=deadbee9".to_string(),
                "/bin/cat".to_string(),
            ],
            24,
            40,
            "/tmp/seen",
        )
        .expect("held screen");
    let candor = core.spawn_pane(24, 40, "/tmp/seen").expect("row stand-in");
    let working = core
        .spawn_pane_cmd(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "sleep 60 & wait".to_string(),
            ],
            24,
            40,
            "/tmp/seen",
        )
        .expect("a shell with a subchild");
    core.panes.get_mut(&pn).unwrap().name = Some("portal7".into());
    core.panes.get_mut(&candor).unwrap().name = Some("candor".into());
    core.panes.get_mut(&working).unwrap().name = Some("portal2".into());
    // The name arm reads the registry the restore walk reads: pin it.
    let _rows = crate::restore_gate::RestoreRegistryRowsGuard;
    crate::restore_gate::set_restore_registry_rows(vec![bg_row("candor", "/tmp/seen", None)]);
    // The fixture's subchild is forked asynchronously: wait until the
    // process table can see it, so the prune's never-prune guard reads
    // the fact, not the race.
    let child = core.panes[&working].pty.child_pid().expect("child pid");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while crate::process_admission::pid_has_child(child) != Some(true) {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture: the sleep child never appeared"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    core.prune_portal_standins();

    assert!(!core.panes.contains_key(&pn), "the portalN shell pruned");
    assert!(
        !core.panes.contains_key(&held),
        "the orphan held screen pruned"
    );
    assert!(
        !core.panes.contains_key(&candor),
        "a paneless row's shell stand-in pruned"
    );
    assert!(
        core.panes.contains_key(&working),
        "a pane whose child has a child is never pruned"
    );
}

#[test]
fn a_pane_substrate_row_shell_is_kept_and_a_leftover_view_is_reaped() {
    // A worker hold whose row hosts a pane is that worker's resume door,
    // kept on purpose. An unplaced adopted leftover that is a portal
    // remnant (a transient view here) is reaped by the leftovers pass,
    // never tabbed.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    // The worker hold: FNO_AGENT_SELF candor2, its row hosts pane p1-ish.
    let hold = core
        .spawn_pane_cmd(
            &[
                "env".to_string(),
                "FNO_AGENT_SELF=candor2".to_string(),
                "/bin/cat".to_string(),
            ],
            24,
            40,
            "/tmp/seen",
        )
        .expect("worker hold");
    core.panes.get_mut(&hold).unwrap().name = Some("candor2".into());
    let mut host = bg_row("candor2", "/tmp/seen", None);
    host.mux = Some(("main".into(), 41));
    core.agents = vec![host];
    core.prune_portal_standins();
    assert!(
        core.panes.contains_key(&hold),
        "a pane-substrate row's hold is the resume door: kept"
    );

    // The leftover: an unplaced adopted transient view.
    let view = core
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
        .expect("leftover view");
    core.keeper_adopted.push(AdoptedKeeper {
        pane: view,
        child_pid: core.panes.get(&view).and_then(|e| e.pty.child_pid()),
        argv: vec![
            "env".to_string(),
            "FNO_VIEW_TRANSIENT=1".to_string(),
            "/bin/cat".to_string(),
        ],
        cwd: "/tmp/seen".into(),
        placed: false,
    });
    let tabs_before = core.session.squad(1).unwrap().tabs.len();
    core.place_adopted_leftovers(1);
    assert!(
        !core.panes.contains_key(&view),
        "a leftover view is reaped, not tabbed"
    );
    assert_eq!(
        core.session.squad(1).unwrap().tabs.len(),
        tabs_before,
        "no tab was minted for the remnant"
    );
}
