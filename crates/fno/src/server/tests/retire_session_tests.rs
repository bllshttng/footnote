//! The v75 exact-session retirement handler (x-7649): one (harness, full
//! session id) identity retires across every held squad; only its panes
//! close; a repeat retires nothing. Mounted as a child of server.rs's
//! `mod tests` (use super::*), so the restore-family helpers resolve
//! through the glob.

use super::*;

#[test]
fn retirement_cannot_launder_a_newer_topology_generation() {
    let _scratch = StoreScratch::new("retire-session-topology-conflict");
    let (mut core, _) = template_core();
    core.restored = true;
    core.topology_dirty = true;
    core.flush_topology();
    let member = crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: Some("retiring".into()),
        harness: Some("codex".into()),
        harness_session_id: Some("session-retiring".into()),
        pane_id: None,
    };
    core.squad_members.insert(1, vec![member.clone()]);
    core.persist_stored("sq", "", &["/a".into()], &[member]);
    let mut external = core.snapshot_squad(1).unwrap();
    external.tab_trees[0].tab_name = Some("external-tree".into());
    crate::squad_store::set_snapshots_if_generations(
        &core.store_generations,
        std::slice::from_ref(&external),
    )
    .unwrap();

    let (reply_tx, _) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.handle_retire_session("codex".into(), "session-retiring".into(), reply_tx);
    assert!(!core.capture_topology_now());
    let stored = crate::squad_store::load();
    let squad = stored.squads.iter().find(|s| s.name == "sq").unwrap();
    assert_eq!(
        squad.tab_trees[0].tab_name.as_deref(),
        Some("external-tree")
    );
    assert!(squad.members[0].tombstone);
}

/// The operator's sequence: a squad holds two codex members; retiring one
/// identity closes only its pane, tombstones only its member, leaves the
/// sibling live, and a repeat retires nothing (idempotent, never an error).
#[test]
fn retire_session_tombstones_the_identity_and_closes_only_its_panes() {
    let s = StoreScratch::new("retire-session-server");
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let members = vec![
        crate::squad_store::StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("t-r-one".into()),
            harness: Some("codex".into()),
            harness_session_id: Some("sess-retire-one".into()),
            pane_id: None,
        },
        crate::squad_store::StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("t-r-two".into()),
            harness: Some("codex".into()),
            harness_session_id: Some("sess-retire-two".into()),
            pane_id: None,
        },
    ];
    crate::squad_store::upsert(
        "",
        &crate::squad_store::origin_key(&[origin.to_string_lossy().into_owned()]),
        &[origin.to_string_lossy().into_owned()],
        &members,
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let mut one = exited_claude_row("t-r-one", None);
    one.harness = Some("codex".into());
    one.harness_session_id = Some("sess-retire-one".into());
    let mut two = exited_claude_row("t-r-two", None);
    two.harness = Some("codex".into());
    two.harness_session_id = Some("sess-retire-two".into());
    core.agents = vec![one, two];
    let _known = KnownWorkersGuard;
    set_known_workers(&["t-r-one", "t-r-two"]);
    let (c, _rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    assert_eq!(core.panes.len(), 2, "both stored members hold a pane");
    let one_pane = core
        .panes
        .iter()
        .find(|(_, e)| e.name.as_deref() == Some("t-r-one"))
        .map(|(pid, _)| *pid)
        .expect("t-r-one holds a pane");
    let two_pane = core
        .panes
        .iter()
        .find(|(_, e)| e.name.as_deref() == Some("t-r-two"))
        .map(|(pid, _)| *pid)
        .expect("t-r-two holds a pane");

    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    let flow = core.handle_retire_session("codex".into(), "sess-retire-one".into(), reply_tx);
    assert!(
        matches!(flow, Flow::Continue),
        "the sibling pane keeps the session alive"
    );
    let reply = reply_rx.try_recv().expect("the handler replied");
    let ServerMsg::SessionRetired {
        retired,
        panes_closed,
        ..
    } = reply
    else {
        panic!("expected SessionRetired, got {reply:?}");
    };
    assert_eq!(retired, 1, "only the matching identity tombstoned");
    assert_eq!(panes_closed, 1, "only the matching pane closed");

    let store = crate::squad_store::load();
    assert_eq!(store.squads.len(), 1);
    let one_row = store.squads[0]
        .members
        .iter()
        .find(|m| m.harness_session_id.as_deref() == Some("sess-retire-one"))
        .expect("the retired member stays in the store");
    assert!(one_row.tombstone, "the retired member is tombstoned");
    let two_row = store.squads[0]
        .members
        .iter()
        .find(|m| m.harness_session_id.as_deref() == Some("sess-retire-two"))
        .expect("the sibling stays in the store");
    assert!(!two_row.tombstone, "the sibling is untouched");
    assert!(
        !core.panes.contains_key(&one_pane),
        "the retired pane is gone"
    );
    assert!(
        core.panes.contains_key(&two_pane),
        "the sibling pane survives"
    );

    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.handle_retire_session("codex".into(), "sess-retire-one".into(), reply_tx);
    let reply = reply_rx.try_recv().expect("the handler replied again");
    let ServerMsg::SessionRetired {
        retired,
        panes_closed,
        ..
    } = reply
    else {
        panic!("expected SessionRetired, got {reply:?}");
    };
    assert_eq!(
        (retired, panes_closed),
        (0, 0),
        "a repeat retires nothing: idempotent, never an error"
    );
}

/// (x-9b37) A portal viewer pane is titled after the row it watches, so a
/// name-only pane-to-member join hands the viewed worker's identity to the
/// viewer. Retiring that identity then closes the operator's portal, and a
/// portal pane dying would tombstone a worker that is still alive. The join
/// is identity: the pane must BE the member's pane.
#[test]
fn a_portal_named_after_the_worker_never_carries_its_identity() {
    let s = StoreScratch::new("retire-session-portal-name-leak");
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let members = vec![crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: Some("t-r-one".into()),
        harness: Some("codex".into()),
        harness_session_id: Some("sess-retire-one".into()),
        pane_id: None,
    }];
    crate::squad_store::upsert(
        "",
        &crate::squad_store::origin_key(&[origin.to_string_lossy().into_owned()]),
        &[origin.to_string_lossy().into_owned()],
        &members,
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let mut one = exited_claude_row("t-r-one", None);
    one.harness = Some("codex".into());
    one.harness_session_id = Some("sess-retire-one".into());
    core.agents = vec![one];
    let _known = KnownWorkersGuard;
    set_known_workers(&["t-r-one"]);
    let (c, _rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let worker_pane = core
        .panes
        .iter()
        .find(|(_, e)| e.name.as_deref() == Some("t-r-one"))
        .map(|(pid, _)| *pid)
        .expect("t-r-one holds a pane");

    // A portal seat in the same squad, named after the worker: exactly the
    // state name_thread_viewer_pane leaves a Follow viewer in.
    let seat = core.spawn_pane(24, 80, "/tmp/seen").expect("portal seat");
    core.panes.get_mut(&seat).unwrap().name = Some("t-r-one".into());
    let (sid, _) = core.session.find_pane(worker_pane).unwrap();
    let tid = core.session.mint_tab_id();
    let sq = core.session.squad_mut(sid).unwrap();
    sq.tabs.push(Tab {
        name: Some("portal".into()),
        id: tid,
        root: Node::Leaf(seat),
        focus: seat,
    });
    core.portals.insert(
        0,
        Portal {
            row_key: "deadbeez".into(),
            seat,
            tab: tid,
        },
    );

    // The join pin: the portal pane resolves to no member identity.
    assert!(
        core.worker_member_context(seat).is_none(),
        "a portal pane never carries the identity of the row it watches"
    );
    // The x-119e outcome: the viewer's pane dying must not tombstone the
    // live worker it was watching.
    core.close_pane(seat);
    let store = crate::squad_store::load();
    let member = store.squads[0]
        .members
        .iter()
        .find(|m| m.harness_session_id.as_deref() == Some("sess-retire-one"))
        .expect("the worker member survives its viewer's death");
    assert!(!member.tombstone, "a closed viewer is not a dead worker");

    // A plain extra tab keeps the squad (and its store row) alive through
    // the retire, so the identity tombstone is measurable after the close.
    let keeper = core.spawn_pane(24, 80, "/tmp/seen").expect("keeper pane");
    let ktid = core.session.mint_tab_id();
    core.session.squad_mut(sid).unwrap().tabs.push(Tab {
        name: Some("keeper".into()),
        id: ktid,
        root: Node::Leaf(keeper),
        focus: keeper,
    });

    // AC1: retiring the identity closes the worker's own pane and leaves
    // no portal behind it, because the viewer pane already went.
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    let flow = core.handle_retire_session("codex".into(), "sess-retire-one".into(), reply_tx);
    // Either terminal is legal here: the worker was the only member left, so
    // the retire may end the session; the point is what the reply counts.
    assert!(matches!(flow, Flow::Continue | Flow::Shutdown));
    let reply = reply_rx.try_recv().expect("the handler replied");
    let ServerMsg::SessionRetired {
        retired,
        panes_closed,
        ..
    } = reply
    else {
        panic!("expected SessionRetired, got {reply:?}");
    };
    assert_eq!(retired, 1);
    assert_eq!(
        panes_closed, 1,
        "only the worker's own pane closes; the portal is not a target"
    );
    assert!(
        !core.panes.contains_key(&worker_pane),
        "the worker's own pane is gone"
    );
}

/// (x-9b37) AC4: the receipt names the panes it closed, and names any tab
/// the closes emptied and removed. `closed_panes` uses the pane's title, so
/// "closed the worker" and "closed the operator's viewer" never read alike.
#[test]
fn the_retire_receipt_names_the_panes_and_tabs_it_removed() {
    let s = StoreScratch::new("retire-session-named-receipt");
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let members = vec![crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: Some("t-r-one".into()),
        harness: Some("codex".into()),
        harness_session_id: Some("sess-retire-one".into()),
        pane_id: None,
    }];
    crate::squad_store::upsert(
        "",
        &crate::squad_store::origin_key(&[origin.to_string_lossy().into_owned()]),
        &[origin.to_string_lossy().into_owned()],
        &members,
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let mut one = exited_claude_row("t-r-one", None);
    one.harness = Some("codex".into());
    one.harness_session_id = Some("sess-retire-one".into());
    core.agents = vec![one];
    let _known = KnownWorkersGuard;
    set_known_workers(&["t-r-one"]);
    let (c, _rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let worker_pane = core
        .panes
        .iter()
        .find(|(_, e)| e.name.as_deref() == Some("t-r-one"))
        .map(|(pid, _)| *pid)
        .expect("t-r-one holds a pane");

    // The close emptied the worker's only tab, so the receipt must name it:
    // compute the expectation from the pre-close tree, the same shape the
    // handler labels with.
    let (sid, ti) = core.session.find_pane(worker_pane).unwrap();
    let (tab_id, expected_label) = {
        let sq = core.session.squad(sid).unwrap();
        let tab = &sq.tabs[ti];
        let label = match (&sq.name, &tab.name) {
            (Some(sq_name), Some(t_name)) => format!("{sq_name}/{t_name}"),
            (Some(sq_name), None) => format!("{sq_name}/tab {}", tab.id),
            (None, Some(t_name)) => t_name.clone(),
            (None, None) => format!("tab {}", tab.id),
        };
        (tab.id, label)
    };

    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.handle_retire_session("codex".into(), "sess-retire-one".into(), reply_tx);
    let reply = reply_rx.try_recv().expect("the handler replied");
    let ServerMsg::SessionRetired {
        closed_panes,
        tabs_removed,
        ..
    } = reply
    else {
        panic!("expected SessionRetired, got {reply:?}");
    };
    assert_eq!(
        closed_panes,
        vec!["t-r-one".to_string()],
        "the receipt names the closed pane by title"
    );
    let tab_gone = core
        .session
        .squad(sid)
        .is_none_or(|sq| !sq.tabs.iter().any(|t| t.id == tab_id));
    if tab_gone {
        assert!(
            tabs_removed.iter().any(|l| *l == expected_label),
            "a removed tab is named: got {tabs_removed:?}, wanted {expected_label:?}"
        );
    } else {
        assert!(
            tabs_removed.is_empty(),
            "nothing was removed, so nothing is named: got {tabs_removed:?}"
        );
    }
}

/// (x-9b37) AC5, the negative control for "had entered the portal before":
/// a portal repointed off a row carries only the NEW row's name, so retiring
/// the old row's identity never touches the portal.
#[test]
fn a_portal_repointed_off_a_row_keeps_no_trace_of_the_old_row() {
    let s = StoreScratch::new("retire-session-portal-repoint");
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let members = vec![crate::squad_store::StoredMember {
        attach_id: String::new(),
        tombstone: false,
        tombstone_reason: None,
        detached: false,
        tab_name: None,
        cwd: None,
        worker: Some("t-r-one".into()),
        harness: Some("codex".into()),
        harness_session_id: Some("sess-retire-one".into()),
        pane_id: None,
    }];
    crate::squad_store::upsert(
        "",
        &crate::squad_store::origin_key(&[origin.to_string_lossy().into_owned()]),
        &[origin.to_string_lossy().into_owned()],
        &members,
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let mut one = exited_claude_row("t-r-one", None);
    one.harness = Some("codex".into());
    one.harness_session_id = Some("sess-retire-one".into());
    core.agents = vec![one];
    let _known = KnownWorkersGuard;
    set_known_workers(&["t-r-one"]);
    let (c, _rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let worker_pane = core
        .panes
        .iter()
        .find(|(_, e)| e.name.as_deref() == Some("t-r-one"))
        .map(|(pid, _)| *pid)
        .expect("t-r-one holds a pane");

    // After a repoint the portal pane carries the NEW row's name only; the
    // seat's stored tab id is all that remembers where it lives.
    let seat = core.spawn_pane(24, 80, "/tmp/seen").expect("portal seat");
    core.panes.get_mut(&seat).unwrap().name = Some("target-v".into());
    let (sid, _) = core.session.find_pane(worker_pane).unwrap();
    let tid = core.session.mint_tab_id();
    core.session.squad_mut(sid).unwrap().tabs.push(Tab {
        name: Some("portal".into()),
        id: tid,
        root: Node::Leaf(seat),
        focus: seat,
    });
    core.portals.insert(
        0,
        Portal {
            row_key: "target-v".into(),
            seat,
            tab: tid,
        },
    );

    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.handle_retire_session("codex".into(), "sess-retire-one".into(), reply_tx);
    let reply = reply_rx.try_recv().expect("the handler replied");
    let ServerMsg::SessionRetired {
        panes_closed,
        closed_panes,
        ..
    } = reply
    else {
        panic!("expected SessionRetired, got {reply:?}");
    };
    assert_eq!(
        closed_panes,
        vec!["t-r-one".to_string()],
        "only the worker closes"
    );
    assert_eq!(panes_closed, 1, "the portal pane is not a retire target");
    assert!(core.panes.contains_key(&seat), "the portal survives");
}
