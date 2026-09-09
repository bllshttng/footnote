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
