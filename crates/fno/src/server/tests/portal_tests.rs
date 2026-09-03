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
