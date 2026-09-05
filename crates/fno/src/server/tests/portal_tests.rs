use super::*;

// ---- (x-8f9d) portals: the one thread pane becomes an addressable set --

/// The reach command naming an explicit portal index.
fn portal_reach_cmd(id: &str, portal: u8) -> Command {
    Command::AttachAgent {
        id: id.into(),
        placement: PanePlacement {
            portal: Some(portal),
            ..Default::default()
        },
    }
}

#[test]
fn portal_target_folds_the_deprecated_thread_pane_alias() {
    // AC2-HP: a pre-v64 client sends `thread_pane: true` and no `portal`.
    // It must resolve to portal 0 - where it always landed - so the
    // compatibility floor never has to move.
    let legacy = PanePlacement {
        thread_pane: true,
        ..Default::default()
    };
    assert_eq!(legacy.portal_target(), Some(0), "the alias is portal 0");

    // AC1-HP: an explicit index wins outright.
    let explicit = PanePlacement {
        portal: Some(1),
        ..Default::default()
    };
    assert_eq!(explicit.portal_target(), Some(1));

    // An explicit index wins even against a contradicting alias: one
    // normalisation, one answer, no code past the edge sees both.
    let both = PanePlacement {
        thread_pane: true,
        portal: Some(3),
        ..Default::default()
    };
    assert_eq!(both.portal_target(), Some(3));

    // Neither set is no portal at all, not portal 0.
    assert_eq!(PanePlacement::default().portal_target(), None);
}

#[test]
fn a_second_portal_opens_beside_the_first_and_leaves_it_alone() {
    // AC6-HP: reaching row B at portal 1 while portal 0 shows row A
    // leaves BOTH open. This is the whole feature: before x-8f9d the
    // second reach repointed the one slot and A vanished.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    let a_seat = core.portals.get(&0).expect("portal 0 open").seat;

    core.command(client_id, portal_reach_cmd("deadbee2", 1));

    assert_eq!(core.portals.len(), 2, "both portals are open");
    assert_eq!(
        core.portals.get(&0).map(|e| e.seat),
        Some(a_seat),
        "portal 0 still seats A's viewer, untouched by the second reach"
    );
    assert_eq!(
        core.portals.get(&1).map(|e| e.row_key.as_str()),
        Some("deadbee2"),
        "portal 1 shows B"
    );
    assert_ne!(
        core.portals[&1].seat, a_seat,
        "two portals never share a seat"
    );
}

#[test]
fn the_server_allocates_the_next_free_portal() {
    // `portal_new` names no index because the CALLER must not choose one.
    // Two clients computing "next free" from the rows they last rendered
    // pick the same number, and the second reach repoints the first one's
    // brand-new portal. The server handles reaches one at a time, so
    // allocating here cannot collide.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
        bg_row("target-c", "/tmp/seen", Some("deadbee3")),
    ];
    let new_portal = |id: &str| Command::AttachAgent {
        id: id.into(),
        placement: PanePlacement {
            portal_new: true,
            ..Default::default()
        },
    };

    core.command(client_id, new_portal("deadbee1"));
    assert_eq!(
        core.portals.keys().copied().collect::<Vec<_>>(),
        vec![0],
        "the first new portal is 0"
    );
    core.command(client_id, new_portal("deadbee2"));
    assert_eq!(
        core.portals.keys().copied().collect::<Vec<_>>(),
        vec![0, 1],
        "the second lands beside it, not on top of it"
    );

    // An explicit index still wins over "any": addressing is unchanged.
    core.command(client_id, portal_reach_cmd("deadbee3", 0));
    assert_eq!(
        core.portals.get(&0).map(|e| e.row_key.as_str()),
        Some("deadbee3"),
        "an addressed reach repoints the index it named"
    );
    assert_eq!(core.portals.len(), 2, "and mints nothing new");
}

#[test]
fn an_exhausted_portal_space_refuses_instead_of_repointing() {
    // x-0719 AC8-EDGE: when every index holds a portal whose seat is a
    // LIVE pane, the old `.unwrap_or(u8::MAX)` fallback handed back 255 -
    // an occupied index - and `P` silently repointed a portal the operator
    // was using. The reach now refuses with a notice naming the ceiling
    // and touches nothing.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, p1, mut rx) = thread_core();
    for idx in 0..=u8::MAX {
        core.portals.insert(
            idx,
            Portal {
                row_key: format!("sentinel-{idx}"),
                seat: p1, // a live pane, so every index is held
                tab: 1,
            },
        );
    }
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];

    core.command(
        client_id,
        Command::AttachAgent {
            id: "deadbee1".into(),
            placement: PanePlacement {
                portal_new: true,
                ..Default::default()
            },
        },
    );

    assert_eq!(core.portals.len(), 256, "no portal was added or moved");
    assert_eq!(
        core.portals.get(&255).map(|e| e.row_key.as_str()),
        Some("sentinel-255"),
        "the reach repointed nothing, not even the top index"
    );
    let notices = drain_notices(&mut rx);
    assert!(
        notices.iter().any(|t| t.contains("256")),
        "the refusal names the exhausted ceiling: {notices:?}"
    );
}

#[test]
fn the_portal_notice_latches_on_delivery() {
    // x-0719 AC10-HP: with a client attached, the first paneless live row
    // both delivers the discoverability notice and sets the latch.
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(32);
    core.attach(
        9,
        24,
        80,
        "/tmp/seen".into(),
        "/tmp/seen".into(),
        tx,
        DirtyMap::default(),
        Arc::new(Notify::new()),
    );
    while rx.try_recv().is_ok() {}
    let rows = vec![bg_row("bg-worker", "/tmp/seen", None)];

    core.handle_msg(CoreMsg::AgentRows {
        rows,
        branches: HashMap::new(),
        tails: HashMap::new(),
    });

    let notices = drain_notices(&mut rx);
    assert!(
        notices.iter().any(|t| t.contains("portal 0")),
        "the notice reached the attached client: {notices:?}"
    );
    assert!(core.portal_noticed, "the latch set on a real delivery");
}

#[test]
fn the_portal_notice_waits_for_a_client() {
    // x-0719 AC11-EDGE: a daemon whose workers register before an operator
    // attaches is the ORDINARY startup ordering. The old set-before-
    // broadcast burned the once-per-lifetime latch on nobody, and the
    // discoverability notice never fired again. With no client attached
    // the latch stays unset; a later row event delivers.
    let mut core = empty_core();
    let rows = vec![bg_row("bg-worker", "/tmp/seen", None)];
    core.handle_msg(CoreMsg::AgentRows {
        rows: rows.clone(),
        branches: HashMap::new(),
        tails: HashMap::new(),
    });
    assert!(!core.portal_noticed, "no client: the latch stays unset");

    core.shells = vec!["/bin/cat".into()];
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(32);
    core.attach(
        9,
        24,
        80,
        "/tmp/seen".into(),
        "/tmp/seen".into(),
        tx,
        DirtyMap::default(),
        Arc::new(Notify::new()),
    );
    while rx.try_recv().is_ok() {}
    core.handle_msg(CoreMsg::AgentRows {
        rows,
        branches: HashMap::new(),
        tails: HashMap::new(),
    });

    assert!(
        drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("portal 0")),
        "the deferred notice delivers once a client exists"
    );
    assert!(core.portal_noticed, "delivered once, then latched");
}

#[test]
fn a_stale_portal_index_is_free_to_reuse() {
    // Liveness, not presence. An entry whose pane closed elsewhere holds
    // no portal, so its index is available - and the reach's own
    // stale-slot path then reads that leftover entry for its remembered
    // tab, landing the new viewer where the old one was.
    let mut core = empty_core();
    core.portals.insert(
        0,
        Portal {
            row_key: "gone".to_string(),
            seat: 99_999, // never in `panes`
            tab: 1,
        },
    );
    assert_eq!(
        core.next_free_portal(),
        Some(0),
        "a stale entry does not reserve its index"
    );
}

#[test]
fn one_row_never_holds_two_portals() {
    // The single slot enforced this by construction - there was nowhere
    // else for a row to be. With several portals the same-row arm sees
    // only the REQUESTED index, so a reach for a row another portal
    // already shows fell through to the fresh-open and minted a SECOND
    // viewer for it.
    //
    // That is not cosmetic. `attached` holds one pane per attach id, so
    // the second insert overwrites the first and strands a live pane no
    // row points at. Measured before the fix: portals=2, seats 2 and 3
    // both showing deadbee1, attached moved 2 -> 3, pane 2 still alive.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    let seat = core.portals.get(&0).expect("portal 0 open").seat;
    let attached_before = core.attached.get("deadbee1").copied();
    let panes_before = core.panes.len();

    // Reach the SAME row into a different portal.
    core.command(client_id, portal_reach_cmd("deadbee1", 1));

    assert!(
        !core.portals.contains_key(&1),
        "no second portal is minted for a row portal 0 already shows"
    );
    assert_eq!(core.portals.len(), 1, "still exactly one portal");
    assert_eq!(
        core.portals.get(&0).map(|e| e.seat),
        Some(seat),
        "portal 0 keeps its seat; the reach focused it"
    );
    assert_eq!(
        core.panes.len(),
        panes_before,
        "no second viewer pane was spawned"
    );
    assert_eq!(
        core.attached.get("deadbee1").copied(),
        attached_before,
        "the attach mapping still names the one live viewer"
    );

    // A row shown only through a STAND-IN is not being viewed, so its
    // portal stays repointable and never blocks a reach elsewhere.
    core.close_pane(seat);
    let stand_in = core
        .portals
        .get(&0)
        .expect("portal 0 holds a stand-in")
        .seat;
    assert!(
        core.panes.get(&stand_in).is_some_and(|e| e.cmd.is_none()),
        "fixture: the seat now holds an idle shell, not a viewer"
    );
    core.command(client_id, portal_reach_cmd("deadbee1", 1));
    assert!(
        core.portals.contains_key(&1),
        "a stand-in does not block the row from opening a real portal"
    );
}

#[test]
fn reaching_an_occupied_portal_repoints_only_that_index() {
    // AC7-HP: the repoint mechanic is unchanged, but scoped. Reaching a
    // third row at portal 0 must repoint portal 0 and leave portal 1 as
    // it was - the single-slot code had no way to express this.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
        bg_row("target-c", "/tmp/seen", Some("deadbee3")),
    ];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    core.command(client_id, portal_reach_cmd("deadbee2", 1));
    let b_seat = core.portals.get(&1).expect("portal 1 open").seat;
    let b_tab = core.portals[&1].tab;

    core.command(client_id, portal_reach_cmd("deadbee3", 0));

    assert_eq!(core.portals.len(), 2, "no third portal was minted");
    assert_eq!(
        core.portals.get(&0).map(|e| e.row_key.as_str()),
        Some("deadbee3"),
        "portal 0 repointed to C"
    );
    assert_eq!(
        core.portals.get(&1).map(|e| (e.seat, e.tab)),
        Some((b_seat, b_tab)),
        "portal 1 is byte-identical: same seat, same tab"
    );
}

#[test]
fn stored_tab_trees_prunes_every_portal_seat_tiled_in_one_tab() {
    // AC9-HP, and the reason Change 3 is its own change rather than a
    // rename. `node_without_leaf` removes ONE leaf per call and the
    // capture loop calls it once per tab, so a mechanical port of the
    // single-slot prune captures the SECOND portal and restore rebuilds
    // a pane for a thread - the exact re-bind the declaration forbids.
    //
    // A ONE-portal restart passes either way, so it cannot be this test.
    // Two seats in one tab is the discriminating fixture.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    core.command(client_id, portal_reach_cmd("deadbee2", 1));
    let seats: Vec<u64> = core.portals.values().map(|e| e.seat).collect();
    assert_eq!(seats.len(), 2, "fixture: two portals are open");

    // Tile them: BOTH seats plus one ordinary pane in ONE named tab, the
    // shape Join produces. The ordinary pane is load-bearing - it keeps
    // the tab from being hollowed away entirely, so the assertion counts
    // survivors rather than reading an absent tab. A single-call prune
    // leaves 2 slots here; the fold leaves 1.
    let (sid, ti) = core.session.find_pane(seats[0]).expect("seat 0 placed");
    let plain = core.spawn_pane(24, 40, "/tmp/seen").expect("plain pane");
    let tab = &mut core.session.squad_mut(sid).expect("live squad").tabs[ti];
    tab.name = Some("tiled".into());
    tab.root = Node::Branch {
        axis: Axis::Vertical,
        children: vec![
            (0.34, Node::Leaf(plain)),
            (0.33, Node::Leaf(seats[0])),
            (0.33, Node::Leaf(seats[1])),
        ],
    };
    tab.focus = plain;

    let (trees, _active) = core.stored_tab_trees(sid).expect("squad captured");
    let tiled = trees
        .iter()
        .find(|t| t.tab_name.as_deref() == Some("tiled"))
        .expect("the tiled tab survives capture: one ordinary pane remains");
    assert_eq!(
        tiled.slots.len(),
        1,
        "only the ordinary pane is captured - BOTH portal seats were pruned. \
             A prune that removes one leaf per tab leaves 2 here; slots = {:?}",
        tiled.slots
    );

    // Positive control: the prune touched the CAPTURE, not the live tree.
    assert_eq!(core.portals.len(), 2, "both portals still open");
    assert!(core.session.find_pane(seats[1]).is_some());
}

#[test]
fn a_closing_portal_spawns_no_stand_in_while_another_is_open() {
    // AC13-HP. The stand-in exists so a dying viewer never deletes the
    // ONLY window onto the fleet. With portal 0 open that premise is
    // false for portal 1, so no idle shell is minted and portal 1 simply
    // goes away. Without this, closing four portals leaves four idle
    // shells each holding a tab open.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    core.command(client_id, portal_reach_cmd("deadbee2", 1));
    let a_seat = core.portals.get(&0).expect("portal 0 open").seat;
    let b_seat = core.portals.get(&1).expect("portal 1 open").seat;
    let panes_before = core.panes.len();

    core.close_pane(b_seat);

    assert!(
        !core.panes.contains_key(&b_seat),
        "portal 1's pane is gone; its entry may stay stale-named, which is \
             what the reach reads to land a replacement in the same tab"
    );
    assert_eq!(
        core.portals.get(&0).map(|e| e.seat),
        Some(a_seat),
        "portal 0 is untouched"
    );
    assert_eq!(
        core.panes.len(),
        panes_before - 1,
        "the viewer was reaped and NO stand-in shell replaced it"
    );

    // The other half of the pair: the LAST portal still gets its
    // stand-in. Either behavior alone looks correct in isolation, which
    // is why both halves live in one test.
    core.close_pane(a_seat);
    let seat = core
        .portals
        .get(&0)
        .expect("the last portal keeps its seat as a stand-in")
        .seat;
    assert!(
        core.panes.get(&seat).is_some_and(|e| e.cmd.is_none()),
        "the last portal's seat holds an idle shell stand-in"
    );
}

#[test]
fn the_portal_index_is_derived_and_pane_zero_is_a_valid_seat() {
    // AC19-EDGE, the x-d914 regression control. Pane ids allocate from
    // zero, so a portal seated on pane 0 is ordinary. A lookup written
    // as a truthiness test (or `pane_id > 0`) reads that seat as absent -
    // the defect that made six live workers invisible, in Rust this time.
    // It is invisible on every other pane id, so it needs its own test.
    let mut core = empty_core();
    core.portals.insert(
        0,
        Portal {
            row_key: "row-zero".to_string(),
            seat: 0,
            tab: 1,
        },
    );
    assert_eq!(
        core.portal_of(Some(0)),
        Some(0),
        "pane 0 is a valid seat, not an absent one"
    );

    // A pane that seats no portal, and a row with no pane at all, both
    // read as no portal - never as an unknown one.
    assert_eq!(core.portal_of(Some(7)), None);
    assert_eq!(core.portal_of(None), None);

    // Derived, not stored: moving the seat moves the index with it, and
    // nothing about the row had to be rewritten.
    core.portals.insert(
        1,
        Portal {
            row_key: "row-zero".to_string(),
            seat: 7,
            tab: 1,
        },
    );
    assert_eq!(core.portal_of(Some(7)), Some(1));
}

// ---- (x-07c2) the dedicated thread pane, plus the x-9b60 placement ----

// ---- (x-07c2) the dedicated thread pane ----------------------------------

/// The reach command with the thread_pane flag set.
fn thread_reach_cmd(id: &str) -> Command {
    Command::AttachAgent {
        id: id.into(),
        placement: PanePlacement {
            thread_pane: true,
            ..Default::default()
        },
    }
}

/// One squad, ONE minted-id tab, one shell pane: the thread-pane fixture.
/// `seen_test_core`'s manually-pushed second tab shares an id with the
/// next minted one (a push does not bump `next_tab_id`), which is fine
/// for its own tests but breaks a `find_pane`->`viewed_tab_mut` round
/// trip that must land on the tab the thread pane actually opened.
fn thread_core() -> (Core, u64, u64, mpsc::Receiver<ServerMsg>) {
    let scratch = std::env::temp_dir().join(format!(
        "fno-thread-store-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&scratch);
    crate::squad_store::set_test_path(&scratch);
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let p1 = core.spawn_pane(24, 40, "/tmp/seen").expect("pane 1");
    core.session.add_squad(
        1,
        vec!["/tmp/seen".into()],
        None,
        Tab {
            name: None,
            id: 1,
            root: Node::Leaf(p1),
            focus: p1,
        },
    );
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(32);
    core.attach(
        9,
        24,
        80,
        "/tmp/seen".into(),
        "/tmp/seen".into(),
        tx,
        DirtyMap::default(),
        Arc::new(Notify::new()),
    );
    while rx.try_recv().is_ok() {}
    (core, 9, p1, rx)
}

#[test]
fn thread_pane_opens_one_pane_and_persists_no_member() {
    // AC3-HP: no slot, a reach on thread row A opens exactly one pane
    // running A's tier argv, records the slot, and persists no squad
    // member - the deliberate difference from the ordinary attach tail.
    set_attach_program(&["/bin/cat"]); // stand in for `claude attach`
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    let panes_before = core.panes.len();
    let new_pid = core.next_pane_id;

    core.command(client_id, thread_reach_cmd("deadbee1"));

    assert_eq!(
        core.panes.len(),
        panes_before + 1,
        "exactly one pane opened"
    );
    assert!(
        core.portals.get(&0).is_some_and(|e| {
            let (k, p) = (&e.row_key, &e.seat);
            k == "deadbee1" && *p == new_pid
        }),
        "the slot records row A"
    );
    assert!(core.squad_members.is_empty(), "no squad member persisted");
    assert_eq!(
        core.attached.get("deadbee1"),
        Some(&new_pid),
        "a Drive row maps its viewer"
    );
    // `cmd` records the program base: the attach stand-in, proof the
    // pane runs the tier argv and not a shell.
    assert_eq!(
        core.panes[&new_pid].cmd.as_deref(),
        Some("cat"),
        "the pane runs the attach argv"
    );
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("thread pane ->")));
    core.reap_pane(new_pid); // don't leak the stand-in child
}

#[test]
fn thread_pane_repoints_the_same_slot_with_no_pane_count_change() {
    // AC4-EDGE: a slot showing A, a reach on B: the same tree slot now
    // runs B, the pane count is unchanged, and no other pane moved.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let slot_a = core.portals.get(&0).expect("portal 0 open").seat;
    let (slot_sid, slot_ti) = core.session.find_pane(slot_a).unwrap();
    let slot_tab_id = core.session.squad(slot_sid).unwrap().tabs[slot_ti].id;
    let panes_after_open = core.panes.len();
    let new_pid = core.next_pane_id;

    core.command(client_id, thread_reach_cmd("deadbee2"));

    assert_eq!(core.panes.len(), panes_after_open, "pane count unchanged");
    assert!(
        core.portals.get(&0).is_some_and(|e| {
            let (k, p) = (&e.row_key, &e.seat);
            k == "deadbee2" && *p == new_pid
        }),
        "the slot now names B"
    );
    assert!(!core.panes.contains_key(&slot_a), "A's viewer reaped");
    assert!(core.attached.contains_key("deadbee2"), "B mapped");
    assert!(
        !core.attached.contains_key("deadbee1"),
        "A resurfaces watch-only"
    );
    // The same tree slot: the geometry never moved, only its leaf id.
    let (sid, ti) = core.session.find_pane(new_pid).unwrap();
    let tab = &core.session.squad(sid).unwrap().tabs[ti];
    assert_eq!(
        (sid, tab.id),
        (slot_sid, slot_tab_id),
        "same tab, same slot"
    );
    assert!(
        matches!(tab.root, Node::Leaf(leaf) if leaf == new_pid),
        "the slot leaf is B's pane: {:?}",
        tab.root
    );
    core.reap_pane(new_pid);
}

#[test]
fn thread_pane_stale_slot_never_reaches_an_argv() {
    // AC5-ERR: a recorded pane id the tree no longer knows reads as
    // absent - a fresh pane opens and the stale id never touches a spawn.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))];
    core.portals.insert(
        0,
        Portal {
            row_key: "deadbee1".to_string(),
            seat: 99_999,
            tab: 0,
        },
    ); // closed elsewhere
    let new_pid = core.next_pane_id;

    core.command(client_id, thread_reach_cmd("deadbee2"));

    assert!(
        core.portals.get(&0).is_some_and(|e| {
            let (k, p) = (&e.row_key, &e.seat);
            k == "deadbee2" && *p == new_pid
        }),
        "fresh pane recorded"
    );
    assert!(core.panes.contains_key(&new_pid));
    assert!(
        !core.panes.contains_key(&99_999),
        "the stale id stayed dead"
    );
    core.reap_pane(new_pid);
}

#[test]
fn thread_pane_same_row_refocuses_without_respawn() {
    // The "show me" rule: reaching the row the slot already shows is a
    // no-op focus, never a respawn and never a toggle-close.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let pid = core.portals.get(&0).expect("portal 0 open").seat;
    let panes_after_open = core.panes.len();

    core.command(client_id, thread_reach_cmd("deadbee1"));

    assert_eq!(core.panes.len(), panes_after_open, "no respawn");
    assert!(
        core.portals.get(&0).is_some_and(|e| {
            let (k, p) = (&e.row_key, &e.seat);
            k == "deadbee1" && *p == pid
        }),
        "the slot keeps the row and its pane"
    );
    let view = core.client_view(client_id).unwrap();
    assert_eq!(
        core.viewed_tab(view).unwrap().focus,
        pid,
        "the reach focused the showing pane"
    );
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("already showing")));
    core.reap_pane(pid);
}

#[test]
fn thread_pane_same_row_through_the_other_door_is_a_focus() {
    // The TUI door keys the slot by the attach id; `fno agents attach`
    // keys it by the registry name. Same row, so "show me": no respawn,
    // no repoint - reaching the row either way must never kill the viewer
    // the operator may be typing into.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let pid = core.portals.get(&0).expect("portal 0 open").seat;
    let panes_after_open = core.panes.len();

    core.command(client_id, thread_reach_cmd("target-a"));

    assert_eq!(core.panes.len(), panes_after_open, "no respawn, no repoint");
    assert!(
        core.portals.get(&0).is_some_and(|e| {
            let (k, p) = (&e.row_key, &e.seat);
            k == "deadbee1" && *p == pid
        }),
        "the slot keeps its original key and pane"
    );
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("already showing")));
    core.reap_pane(pid);
}

#[test]
fn thread_pane_ctl_by_name_on_an_attach_id_slot_replies_a_landing() {
    // The control door reaches by name while the slot is keyed by the
    // attach id: the focus path emits no Err. A key-only `landed` check
    // would turn that success into "no such agent".
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(9, thread_reach_cmd("deadbee1"));
    let pid = core.portals.get(&0).expect("portal 0 open").seat;
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();

    core.portal_ctl("target-a", 0, PanePlacement::default(), None, tx);

    match rx.blocking_recv().expect("a reply") {
        ServerMsg::Notice { text } => assert!(
            text.contains("already showing"),
            "the by-name reach on an attach-id slot is a focus: {text}"
        ),
        other => panic!("expected a Notice landing, got {other:?}"),
    }
    assert!(
        core.portals.get(&0).is_some_and(|e| {
            let (k, p) = (&e.row_key, &e.seat);
            k == "deadbee1" && *p == pid
        }),
        "the slot keeps the row and its pane"
    );
    core.reap_pane(pid);
}

#[test]
fn thread_pane_ctl_names_the_session_of_a_foreign_hosted_row() {
    // A row pane-hosted in ANOTHER session is that server's to view: the
    // reply names where it lives. "no such agent" would lie about a row
    // the registry knows (and the inline attach this verb replaced
    // attached it regardless of hosting session).
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    let mut hosted = bg_row("far-row", "/tmp/seen", None);
    hosted.mux = Some(("other-session".to_string(), 42));
    let agents = vec![hosted];

    core.portal_ctl("far-row", 0, PanePlacement::default(), Some(agents), tx);

    match rx.blocking_recv().expect("a reply") {
        ServerMsg::Notice { text } => assert!(
            text.contains("other-session") && text.contains("pane 42"),
            "names the foreign session and pane: {text}"
        ),
        other => panic!("expected a Notice, got {other:?}"),
    }
    assert!(core.portals.is_empty(), "no thread pane minted");
}

#[test]
fn thread_pane_follow_and_locate_rows_spawn_their_tier_argv() {
    // A paneless codex row (Follow) tails its transcript; a gemini row
    // (Locate) renders the self-teaching screen. Both reach by NAME -
    // neither carries an attach id - and neither maps into `attached`.
    // The peek program is overridden to a stand-in so the spawn path runs
    // without booting the deployed CLI (whose load-time boot can outlive
    // any sane test budget under the full suite); the real argv shape is
    // asserted in `peek_argv_is_the_peek_verb_with_follow`.
    set_peek_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    let mut codex = bg_row("codex-row", "/tmp/seen", None);
    codex.harness = Some("codex".into());
    let mut gem = bg_row("gem-row", "/tmp/seen", None);
    gem.harness = Some("gemini".into());
    core.agents = vec![codex, gem];

    core.command(client_id, thread_reach_cmd("codex-row"));
    let follow_pid = core.portals.get(&0).expect("portal 0 open").seat;
    assert_eq!(
        core.panes[&follow_pid].cmd.as_deref(),
        Some("cat"),
        "Follow tails through the peek stand-in"
    );
    assert!(
        core.attached.is_empty(),
        "no attach mapping for a Follow row"
    );

    core.command(client_id, thread_reach_cmd("gem-row"));
    let locate_pid = core.portals.get(&0).expect("portal 0 open").seat;
    assert_eq!(
        core.panes[&locate_pid].cmd.as_deref(),
        Some("sh"),
        "Locate renders its screen through sh"
    );
    assert!(
        core.attached.is_empty(),
        "no attach mapping for a Locate row"
    );
    core.reap_pane(locate_pid);
    core.reap_pane(follow_pid);
}

#[test]
fn peek_argv_is_the_peek_verb_with_follow() {
    // The real (un-overridden) Follow argv, asserted directly so the
    // program override in the spawn tests never hides the shipped
    // command.
    PEEK_PROGRAM.with(|p| *p.borrow_mut() = None);
    assert_eq!(
        peek_argv("codex-row"),
        vec![
            "fno".to_string(),
            "agents".into(),
            "peek".into(),
            "codex-row".into(),
            "--follow".into()
        ]
    );
}

#[test]
fn thread_pane_locate_screen_names_the_row_and_its_routes() {
    // The Locate pane is self-teaching runtime text, not an empty pane:
    // the pure builder alone is asserted here (no spawn needed) - name,
    // harness, cwd, the why sentence, and the mail route that does reach
    // the row. Injection safety: every fact rides as an ARGV element.
    let mut gem = bg_row("gem-row'$(reboot)'", "/tmp/gem cwd", None);
    gem.harness = Some("gemini".into());
    let argv = locate_argv(&gem);
    assert_eq!(argv[0], "sh");
    assert_eq!(argv[1], "-c");
    assert_eq!(
        argv[2], "printf '%s\\n' \"$@\"; exec cat",
        "facts ride as argv, never as script"
    );
    assert_eq!(argv[3], "fno-locate");
    let screen = argv[4..].join("\n");
    assert!(
        screen.contains("gem-row'$(reboot)'"),
        "names the row verbatim"
    );
    assert!(screen.contains("harness:   gemini"), "names the harness");
    assert!(screen.contains("cwd:       /tmp/gem cwd"), "names the cwd");
    assert!(screen.contains("no live viewport"), "says why");
    assert!(
        screen.contains("fno agents mail send gem-row'$(reboot)'"),
        "names the route that reaches it"
    );
}

#[test]
fn thread_pane_refuses_an_unresolvable_or_ambiguous_key() {
    // A key no paneless live row answers, and a name two rows share:
    // both refuse fail-closed with a named reason, nothing spawns.
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![
        bg_row("dupe", "/tmp/seen", Some("deadbee1")),
        bg_row("dupe", "/tmp/seen", Some("deadbee2")),
    ];
    let panes_before = core.panes.len();
    core.command(client_id, thread_reach_cmd("nosuchrow"));
    core.command(client_id, thread_reach_cmd("dupe"));
    assert_eq!(core.panes.len(), panes_before, "nothing spawned");
    let notices = drain_notices(&mut rx);
    assert!(
        notices.iter().any(|t| t.contains("no live row answers")),
        "the reach's miss names the door and the key: {notices:?}"
    );
    assert!(notices.iter().any(|t| t.contains("more than one row")));
    assert!(core.portals.is_empty(), "no slot recorded on a refusal");
}

#[test]
fn thread_pane_ctl_lands_the_reach_and_replies_where() {
    // AC8-HP (server half): the control verb drives the SAME reach a TUI
    // gesture drives, records the slot, persists nothing, and replies
    // with the landing.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    let agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    let new_pid = core.next_pane_id;

    core.portal_ctl("deadbee1", 0, PanePlacement::default(), Some(agents), tx);

    assert!(
        core.portals.get(&0).is_some_and(|e| {
            let (k, p) = (&e.row_key, &e.seat);
            k == "deadbee1" && *p == new_pid
        }),
        "the control reach records the slot"
    );
    assert!(core.squad_members.is_empty(), "no squad member persisted");
    match rx.blocking_recv().expect("a reply") {
        ServerMsg::Notice { text } => assert!(
            text.contains("thread pane -> target-a"),
            "the reply names the landing: {text}"
        ),
        other => panic!("expected a Notice landing, got {other:?}"),
    }
    // The observer's removal rides the core queue (CoreMsg::Gone), so it
    // happens on the loop's next drain, not synchronously here.
    core.reap_pane(new_pid);
}

#[test]
fn thread_pane_ctl_refuses_an_unknown_name() {
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();

    core.portal_ctl("nosuchrow", 0, PanePlacement::default(), None, tx);

    match rx.blocking_recv().expect("a reply") {
        ServerMsg::Err { msg, .. } => assert!(
            msg.contains("no live row answers"),
            "names the door and the key it could not find: {msg}"
        ),
        other => panic!("expected an Err refusal, got {other:?}"),
    }
    assert!(core.portals.is_empty());
    assert!(core.panes.len() == 1, "nothing spawned");
}

#[test]
fn thread_pane_ctl_answers_a_pane_hosted_row_with_its_location() {
    // A row already pane-hosted in this session has its viewport: the
    // verb answers with the pane instead of opening a second one.
    let (mut core, _client_id, p1, _rx) = thread_core();
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    let mut hosted = bg_row("hosted-row", "/tmp/seen", None);
    hosted.mux = Some(("test".to_string(), p1));
    let name = core.session_name.clone();
    core.session_name = "test".to_string();
    let agents = vec![hosted];

    core.portal_ctl("hosted-row", 0, PanePlacement::default(), Some(agents), tx);

    core.session_name = name;
    match rx.blocking_recv().expect("a reply") {
        ServerMsg::Notice { text } => assert!(
            text.contains("hosts pane") && text.contains("hosted-row"),
            "names the existing pane: {text}"
        ),
        other => panic!("expected a Notice, got {other:?}"),
    }
    assert!(core.portals.is_empty(), "no thread pane minted");
}

#[test]
fn stored_tab_trees_prunes_the_thread_pane_and_remaps_active_tab() {
    // (x-07c2) The dedicated thread pane is never persisted: capture
    // prunes its leaf, a tab it hollowed out is not captured at all, and
    // a skipped tab - the active one included - never leaves a dangling
    // `active_tab` (position IS the durable tab identity in tab_trees).
    // The thread pane carries an `attached` binding, so an un-pruned
    // capture WOULD name its slot Fno(deadbee1): the absent binding is
    // the red/green pair, not a vacuous one.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let thread_pid = core.portals.get(&0).expect("portal 0 open").seat;
    let (sid, _ti) = core
        .session
        .find_pane(thread_pid)
        .expect("the thread pane is in the live tree");

    let (trees, active) = core.stored_tab_trees(sid).unwrap();
    assert!(
        trees
            .iter()
            .flat_map(|t| &t.slots)
            .all(|s| !matches!(&s.binding, LayoutBinding::Fno(id) if id == "deadbee1")),
        "no captured slot names the thread pane's attach id"
    );
    assert!(
        active < trees.len(),
        "active_tab remapped into the captured range (active={active}, {} trees)",
        trees.len()
    );
    assert!(
        trees.iter().all(|t| !t.slots.is_empty()),
        "no hollowed tab was captured"
    );
    // Positive control: the prune touched the CAPTURE, not the live
    // session - the pane is still there and still the dedicated slot.
    assert!(core.session.find_pane(thread_pid).is_some());
    assert_eq!(
        core.portals.get(&0).map(|entry| entry.seat),
        Some(thread_pid)
    );
}

#[test]
fn stored_tab_trees_remaps_active_tab_when_the_active_tab_is_pruned_away() {
    // (x-07c2) When the ACTIVE tab is the one entirely hollowed out by
    // the thread-pane prune, the remap in the loop above never fires
    // (its guard never sees a tab that `continue`d past it) - active_tab
    // must not silently default to 0, which would land restore on a
    // tab unrelated to the one that vanished.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let thread_pid = core.portals.get(&0).expect("portal 0 open").seat;
    let (sid, thread_tab_idx) = core
        .session
        .find_pane(thread_pid)
        .expect("the thread pane is in the live tree");

    // Sandwich the thread tab between two ordinary sibling tabs, and
    // make the thread tab (pure thread pane, no other content) active.
    let squad = core.session.squad_mut(sid).unwrap();
    let thread_tab = squad.tabs.remove(thread_tab_idx);
    squad.tabs = vec![leaf_tab(9001, 9101), thread_tab, leaf_tab(9002, 9102)];
    squad.active_tab = 1;

    let (trees, active) = core.stored_tab_trees(sid).unwrap();
    assert_eq!(
        trees.len(),
        2,
        "only the two sibling tabs survive the prune"
    );
    assert_eq!(
        active, 1,
        "active_tab lands on the surviving tab that followed the pruned one, not index 0"
    );
}

#[test]
fn same_row_reach_on_a_stand_in_respawns_the_viewer_in_place() {
    // A same-row reach on a stand-in seat is NOT "already showing" - the
    // seat holds a shell, not the row. It repoints the shell into a fresh
    // viewer of the SAME row, in the same tab.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let (a_viewer, a_tid) = {
        let e = core.portals.get(&0).expect("portal 0 open");
        (e.seat, e.tab)
    };
    core.close_pane(a_viewer);
    let panes_before = core.panes.len();

    core.command(client_id, thread_reach_cmd("deadbee1"));

    assert!(
        !drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("already showing")),
        "a stand-in seat never reads as already showing"
    );
    assert!(
        core.portals.get(&0).is_some_and(|e| {
            e.row_key == "deadbee1" && e.tab == a_tid && core.panes[&e.seat].cmd.is_some()
        }),
        "the portal names a fresh live viewer in the same tab"
    );
    assert_eq!(
        core.panes.len(),
        panes_before,
        "the respawn reused the seat pane"
    );
}

#[test]
fn portal_open_here_is_refused_before_any_lookup() {
    // (x-9b60) open-here repoints the sender's focused pane; a portal
    // mints its own seat. The one geometry refused in BOTH cases
    // (repoint and fresh open), exactly as the decode edge refused it
    // before the decision moved into the reach.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    let panes_before = core.panes.len();
    core.command(
        client_id,
        Command::AttachAgent {
            id: "deadbee1".into(),
            placement: PanePlacement {
                portal: Some(0),
                here: true,
                ..Default::default()
            },
        },
    );
    assert_eq!(core.panes.len(), panes_before, "no pane spawned");
    assert!(core.portals.is_empty(), "no portal entry written");
    assert!(drain_notices(&mut rx)
        .iter()
        .any(|t| t.contains("a portal takes no split")));
}

#[test]
fn portal_fresh_open_honors_caller_tab_and_split() {
    // (x-9b60, AC1-HP) A fresh open at an index has no geometry to own
    // yet, so the caller's tab and split are honored: the second
    // portal's viewer lands beside the first, in the tab the caller
    // named, and Portal.tab records where it actually landed.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];
    // Portal 0 first, unplaced: it opens into a fresh tab of the owner
    // squad. That tab's id is what the second call names.
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let tab_a = core.portals.get(&0).expect("portal 0 open").tab;

    core.command(
        client_id,
        Command::AttachAgent {
            id: "deadbee2".into(),
            placement: PanePlacement {
                portal: Some(1),
                tab: Some(crate::proto::TabSel::Id(tab_a)),
                split: Some(Dir::Right),
                ..Default::default()
            },
        },
    );
    let entry = core.portals.get(&1).expect("portal 1 open");
    let seat_b = entry.seat;
    assert_eq!(
        entry.tab, tab_a,
        "the viewer landed in the tab the caller named"
    );
    assert_eq!(entry.row_key, "deadbee2");
    let sq = core.session.squad(1).unwrap();
    let tab = sq.tabs.iter().find(|t| t.id == tab_a).unwrap();
    let mut leaves = tree::leaves(&tab.root);
    leaves.sort_unstable();
    // tab_a is the portal tab the first reach minted (the manually
    // pushed tab 1 holds only the fixture shell p1), so its leaves are
    // the two viewers.
    let mut expected = vec![core.portals.get(&0).unwrap().seat, seat_b];
    expected.sort_unstable();
    assert_eq!(
        leaves, expected,
        "portal 1 split into the named tab, beside the first"
    );
    assert!(
        !drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("tab full")),
        "a fresh open with room never falls back"
    );
}

#[test]
fn portal_repoint_keeps_its_geometry_and_says_so() {
    // (x-9b60, AC2-REG) A portal with a live viewer owns its geometry: a
    // reach for another row carrying a tab/split for somewhere else is
    // repointed IN PLACE, the tab never moves, and the caller is TOLD
    // the geometry was refused rather than silently dropped.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let (seat_a, tab_a) = {
        let e = core.portals.get(&0).expect("portal 0 open");
        (e.seat, e.tab)
    };
    core.command(
        client_id,
        Command::AttachAgent {
            id: "deadbee2".into(),
            placement: PanePlacement {
                portal: Some(0),
                tab: Some(crate::proto::TabSel::Id(9999)),
                split: Some(Dir::Right),
                ..Default::default()
            },
        },
    );
    let entry = core.portals.get(&0).expect("portal 0 still open");
    assert_eq!(entry.tab, tab_a, "the repoint never moves the tab");
    assert_ne!(entry.seat, seat_a, "the viewer was replaced in place");
    assert_eq!(entry.row_key, "deadbee2");
    let notices = drain_notices(&mut rx);
    assert!(
        notices
            .iter()
            .any(|t| t.contains("a portal takes no split")),
        "the geometry refusal is visible: {notices:?}"
    );
}

#[test]
fn portal_stale_seat_prefers_the_remembered_tab_over_the_caller_tab() {
    // (x-d545 via x-9b60, AC3-REG) A stale seat's remembered tab still
    // wins on a fresh open, even once a caller can supply a tab: the
    // replacement viewer lands where the operator had it.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let tab_a = core.portals.get(&0).expect("portal 0 open").tab;
    // A second, real tab for the caller to name: mint the id so it
    // cannot collide the way a manual push would.
    let tab_b = core.session.mint_tab_id();
    let shell_b = core.spawn_pane(24, 40, "/tmp/seen").expect("pane b");
    core.session.squad_mut(1).unwrap().tabs.push(Tab {
        name: None,
        id: tab_b,
        root: Node::Leaf(shell_b),
        focus: shell_b,
    });
    // Kill the portal's viewer: the entry goes stale, the remembered
    // tab (tab_a) survives.
    let seat_a = core.portals.get(&0).unwrap().seat;
    core.close_pane(seat_a);

    core.command(
        client_id,
        Command::AttachAgent {
            id: "deadbee1".into(),
            placement: PanePlacement {
                portal: Some(0),
                tab: Some(crate::proto::TabSel::Id(tab_b)),
                split: Some(Dir::Right),
                ..Default::default()
            },
        },
    );
    assert!(
        !drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("already showing")),
        "a dead viewer never reads as already showing"
    );
    let entry = core.portals.get(&0).expect("portal 0 reopened");
    assert_eq!(
        entry.tab, tab_a,
        "the remembered tab wins over the caller's tab"
    );
}

#[test]
fn portal_fresh_open_refuses_a_missing_tab_before_any_pane() {
    // (x-9b60, AC4-EDGE) A caller tab the server cannot resolve refuses
    // BEFORE a pane exists: no spawn, no portal entry, a notice that
    // names the problem.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    let panes_before = core.panes.len();
    core.command(
        client_id,
        Command::AttachAgent {
            id: "deadbee1".into(),
            placement: PanePlacement {
                portal: Some(0),
                tab: Some(crate::proto::TabSel::Id(9999)),
                split: Some(Dir::Right),
                ..Default::default()
            },
        },
    );
    assert_eq!(core.panes.len(), panes_before, "no pane spawned");
    assert!(core.portals.is_empty(), "no portal entry written");
    let notices = drain_notices(&mut rx);
    assert!(
        notices.iter().any(|t| t.contains("tab")),
        "the refusal names the tab: {notices:?}"
    );
}

// ---- (x-d545) the remembered tab outlives its viewer ----

#[test]
fn close_pane_viewer_seat_lone_leaf_keeps_tab_with_idle_shell() {
    // AC1-HP + AC3-HP: the viewport tab is the only tab of its squad; the
    // recorded viewer's child exits; the tab survives with the SAME id
    // and one idle shell, the squad and the session survive, and the
    // close is Flow::Continue - never the SessionEmpty shutdown the old
    // path took.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    // A viewer carries argv provenance (the tier argv), a bare shell
    // none - the arm's seat check keys on exactly that difference.
    let lone_viewer = core
        .spawn_pane_cmd(&["/bin/cat".to_string()], 24, 40, "/tmp/seen")
        .expect("viewer pane");
    core.session.add_squad(
        7,
        vec!["/tmp/seen".into()],
        None,
        Tab {
            name: None,
            id: 900,
            root: Node::Leaf(lone_viewer),
            focus: lone_viewer,
        },
    );
    core.portals.insert(
        0,
        Portal {
            row_key: "row-x".to_string(),
            seat: lone_viewer,
            tab: 900,
        },
    );

    let flow = core.close_pane(lone_viewer);

    assert!(
        matches!(flow, Flow::Continue),
        "a viewer swap never shuts the session down"
    );
    assert!(core.session.squad(7).is_some(), "the squad survives");
    let tab = &core.session.squad(7).unwrap().tabs[0];
    assert_eq!(tab.id, 900, "AC1: the same TabId survives");
    let leaves = tree::leaves(&tab.root);
    assert_eq!(leaves.len(), 1, "one pane holds the seat");
    let shell = leaves[0];
    assert!(core.panes.contains_key(&shell), "the seat pane is live");
    assert!(
        core.panes[&shell].cmd.is_none(),
        "the seat holds an idle shell, not a viewer"
    );
    let entry = core.portals.get(&0).expect("portal 0 still open");
    assert_eq!(
        (entry.row_key.as_str(), entry.seat, entry.tab),
        ("row-x", shell, 900),
        "the portal names the stand-in seat"
    );
    assert!(
        !core.panes.contains_key(&lone_viewer),
        "the dead viewer is reaped"
    );
}

#[test]
fn reach_after_viewer_death_opens_in_the_same_tab() {
    // AC2-HP: reach A, A's viewer child exits (the swap leaves the idle
    // shell stand-in), reach B: B's viewer opens in the SAME TabId, the
    // repoint reuses the seat pane, and no second viewport tab is minted.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let (a_viewer, a_tid) = {
        let e = core.portals.get(&0).expect("portal 0 open");
        (e.seat, e.tab)
    };
    let squad_tabs = core.session.squad(1).unwrap().tabs.len();

    core.close_pane(a_viewer);
    let (seat, seat_tid) = {
        let e = core.portals.get(&0).expect("portal 0 open");
        (e.seat, e.tab)
    };
    assert_eq!(seat_tid, a_tid, "the stand-in seat keeps the tab id");
    let panes_before_b = core.panes.len();

    core.command(client_id, thread_reach_cmd("deadbee2"));

    assert!(
        core.portals.get(&0).is_some_and(|e| {
            let (k, p, t) = (&e.row_key, &e.seat, &e.tab);
            k == "deadbee2" && *p != seat && *t == a_tid
        }),
        "B's viewer takes the seat in the same tab"
    );
    let (b_pid, b_tid) = {
        let e = core.portals.get(&0).expect("portal 0 open");
        (e.seat, e.tab)
    };
    let (sid, ti) = core.session.find_pane(b_pid).unwrap();
    assert_eq!(
        core.session.squad(sid).unwrap().tabs[ti].id,
        a_tid,
        "B lands in A's viewport tab"
    );
    assert_eq!(b_tid, a_tid);
    assert_eq!(
        core.session.squad(1).unwrap().tabs.len(),
        squad_tabs,
        "no second viewport tab minted"
    );
    assert_eq!(
        core.panes.len(),
        panes_before_b,
        "the repoint reused the seat pane"
    );
    assert!(
        !core.panes.contains_key(&a_viewer),
        "A's viewer stays reaped"
    );
    core.reap_pane(b_pid);
}

#[test]
fn close_pane_plain_lone_pane_still_removes_its_tab() {
    // AC8-FR: the new arm fires only for the recorded thread pane. A
    // plain pane alone in its tab keeps today's semantics: the tab goes.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let p2 = core.spawn_pane(24, 40, "/tmp/seen").expect("plain pane");
    core.session.squad_mut(1).unwrap().tabs.push(Tab {
        name: None,
        id: 901,
        root: Node::Leaf(p2),
        focus: p2,
    });
    core.portals.insert(
        0,
        Portal {
            row_key: "row-x".to_string(),
            seat: 99_999,
            tab: 0,
        },
    ); // viewer elsewhere

    let flow = core.close_pane(p2);

    assert!(
        matches!(flow, Flow::Continue),
        "the session survives (tab 1 remains)"
    );
    assert!(
        !core
            .session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .any(|t| t.id == 901),
        "a plain pane's tab is removed exactly as today"
    );
}

/// A live paneless claude row (the Drive tier): harness claude plus an
/// attach id is exactly the shape the re-entry resolver owns.
fn claude_row(name: &str, attach: &str) -> RegistryAgent {
    let mut row = bg_row(name, "/tmp/seen", Some(attach));
    row.harness = Some("claude".into());
    row
}

#[tokio::test]
async fn portal_ctl_claude_row_replies_the_landing_not_the_fallback() {
    // AC1-HP marker: the control door on a LIVE paneless claude row. The
    // join behind the reply has two refusal arms: (false, Some) means
    // reach_portal ran and reported; (false, None) means it produced
    // nothing and the fallback invented "no such agent: NAME" - the reply
    // that sent a reader hunting a resolver that is not in this chain.
    // The reply must be the reach's own verdict, and it must name the
    // portal index it opened.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let row = claude_row("claude-row", "deadbee1");
    let new_pid = core.next_pane_id;
    let (tx, mut rx) = tokio::sync::oneshot::channel::<ServerMsg>();

    core.portal_ctl(
        "claude-row",
        1,
        PanePlacement::default(),
        Some(vec![row]),
        tx,
    );
    // The reach parked: no pane, and no reply yet - it waits for the verdict
    // instead of answering the old fallback.
    assert!(core.portals.get(&1).is_none(), "the park opens nothing");
    assert!(
        rx.try_recv().is_err(),
        "the held reply waits for the verdict"
    );
    // Pump the continuation by hand: the real verdict arrives on the core
    // channel (the fixture does not run the loop); the handler is what
    // finishes the park.
    core.handle(CoreMsg::ReentryPlanReady {
        id: u64::MAX, // the control door's observer client
        request: Box::new(ReentrySpawnRequest::Attach {
            attach_id: "deadbee1".into(),
            placement: PanePlacement {
                portal: Some(1),
                ..Default::default()
            },
        }),
        verdict: Ok(ReentryVerdict {
            argv: vec!["/bin/cat".into()],
            env: vec![],
            config_dir: None,
        }),
    });

    match rx.await.expect("a reply") {
        ServerMsg::Err { msg, .. } => {
            panic!("the (false, None) fallback arm fired - reach_portal never reported: {msg}")
        }
        ServerMsg::Notice { text } => assert!(
            text.contains("portal 1"),
            "the landing must name the portal index the caller asked for: {text}"
        ),
        other => panic!("expected a Notice landing, got {other:?}"),
    }
    assert!(
        core.portals
            .get(&1)
            .is_some_and(|e| e.row_key == "deadbee1" && e.seat == new_pid),
        "portal 1 holds the row's viewer"
    );
    assert_eq!(
        core.panes[&new_pid].cmd.as_deref(),
        Some("cat"),
        "the pane runs the verdict's argv, not a guess"
    );
    core.reap_pane(new_pid); // don't leak the stand-in child
}

#[tokio::test]
async fn portal_ctl_claude_row_with_a_refused_plan_names_the_reason() {
    // The resolver's refusal must reach the operator verbatim, never
    // collapse into the reach-never-ran fallback: (false, Some) is the
    // honest arm - the reach ran and was refused by name.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let row = claude_row("claude-row", "deadbee1");
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();

    core.portal_ctl(
        "claude-row",
        1,
        PanePlacement::default(),
        Some(vec![row]),
        tx,
    );
    core.handle(CoreMsg::ReentryPlanReady {
        id: u64::MAX, // the control door's observer client
        request: Box::new(ReentrySpawnRequest::Attach {
            attach_id: "deadbee1".into(),
            placement: PanePlacement {
                portal: Some(1),
                ..Default::default()
            },
        }),
        verdict: Err("row claude-row is on the account axis and records no launch account".into()),
    });

    match rx.await.expect("a reply") {
        ServerMsg::Err { msg, .. } => assert!(
            msg.contains("account axis"),
            "the resolver's refusal reaches the operator verbatim, never as the fallback: {msg}"
        ),
        other => panic!("expected an Err refusal, got {other:?}"),
    }
    assert!(core.portals.is_empty(), "no portal on a refused plan");
}

#[tokio::test]
async fn portal_ctl_reaches_a_paneless_row_whose_key_also_matches_a_hosted_row() {
    // Door parity: the duplicate refusal counts live paneless rows, the rows
    // a reach could serve - the same filter reach_portal applies. A hosted
    // namesake answers the location only when no reachable row exists.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let mut hosted = claude_row("hosted-name", "deadbee1");
    hosted.mux = Some(("some-session".into(), 7));
    let live = claude_row("live-name", "deadbee1");
    let new_pid = core.next_pane_id;
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();

    core.portal_ctl(
        "deadbee1",
        1,
        PanePlacement::default(),
        Some(vec![hosted, live]),
        tx,
    );
    core.handle(CoreMsg::ReentryPlanReady {
        id: u64::MAX, // the control door's observer client
        request: Box::new(ReentrySpawnRequest::Attach {
            attach_id: "deadbee1".into(),
            placement: PanePlacement {
                portal: Some(1),
                ..Default::default()
            },
        }),
        verdict: Ok(ReentryVerdict {
            argv: vec!["/bin/cat".into()],
            env: vec![],
            config_dir: None,
        }),
    });

    match rx.await.expect("a reply") {
        ServerMsg::Err { msg, .. } => {
            panic!("the hosted namesake turned a reachable row into a refusal: {msg}")
        }
        ServerMsg::Notice { text } => assert!(
            text.contains("portal 1"),
            "the reach served the live paneless row: {text}"
        ),
        other => panic!("expected a Notice landing, got {other:?}"),
    }
    assert!(core.portals.get(&1).is_some_and(|e| e.seat == new_pid));
    core.reap_pane(new_pid); // don't leak the stand-in child
}

#[test]
fn close_pane_stand_in_shell_still_removes_its_tab() {
    // The idle-shell stand-in must stay closable by hand: its own close
    // removes the tab as today and never re-arms the swap (no shell
    // chain wedging the tab open forever).
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let (a_viewer, a_tid) = {
        let e = core.portals.get(&0).expect("portal 0 open");
        (e.seat, e.tab)
    };
    core.close_pane(a_viewer); // the stand-in takes the seat
    let shell = core.portals.get(&0).expect("portal 0 open").seat;

    core.close_pane(shell);

    assert!(
        !core
            .session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .any(|t| t.id == a_tid),
        "the hand-closed stand-in still removes the tab"
    );
    assert!(
        core.portals.get(&0).is_some_and(|e| {
            let (k, p, t) = (&e.row_key, &e.seat, &e.tab);
            k == "deadbee1" && *p == shell && *t == a_tid
        }),
        "the slot stays stale-named, exactly as closing a dedicated pane always did"
    );
}
