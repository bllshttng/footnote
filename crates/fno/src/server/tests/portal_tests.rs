use super::*;

use crate::server::portal_reach::portal_replay_placement;

// ---- (x-8f9d) portals: the one thread pane becomes an addressable set --

/// The reach command naming an explicit portal index.
pub(super) fn portal_reach_cmd(id: &str, portal: u8) -> Command {
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
        ctx: HashMap::new(),
        read_ok: false,
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
        ctx: HashMap::new(),
        read_ok: false,
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
        ctx: HashMap::new(),
        read_ok: false,
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
    core.close_viewer_died(seat, "viewer exited");
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
fn stored_tab_trees_captures_every_portal_seat_tiled_in_one_tab() {
    // AC1-HP: a tab with two tiled portals plus one ordinary pane captures
    // THREE slots. This is the discriminating fixture the old prune test
    // used in reverse: two portals in one tab is the shape the single-slot
    // prune hid, and the shape the operator's report named (two tiled in
    // tab 1, one alone in tab 2).
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

    // Tile them: BOTH seats plus one ordinary pane in ONE named tab.
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
        .expect("the tiled tab is captured");
    assert_eq!(
        tiled.slots.len(),
        3,
        "all three leaves are captured: slots = {:?}",
        tiled.slots
    );
    // The split is preserved: three leaves under one branch.
    assert!(
        matches!(&tiled.tree, LayoutTreeSpec::Split { children, .. } if children.len() == 3),
        "the tile survives as a split: {:?}",
        tiled.tree
    );
    let portal_slots: Vec<(u8, &str)> = tiled
        .slots
        .iter()
        .filter_map(|s| s.portal.as_ref().map(|p| (p.index, p.row.as_str())))
        .collect();
    assert_eq!(
        portal_slots,
        vec![(0, "deadbee1"), (1, "deadbee2")],
        "two slots carry the open indices and row keys"
    );
    assert!(
        tiled.slots.iter().any(|s| s.portal.is_none()
            && matches!(&s.binding, LayoutBinding::Shell)
            && s.name.starts_with('p')),
        "the ordinary pane stays an ordinal shell slot"
    );

    // Positive control: capture touched the STORED shape, not the live tree.
    assert_eq!(core.portals.len(), 2, "both portals still open");
    assert!(core.session.find_pane(seats[1]).is_some());
}

#[test]
fn every_viewer_death_keeps_its_seat_as_an_idle_shell() {
    // AC1-HP (x-3349). A portal is just another viewport: removing a row
    // is not removing a pane. Whichever portal's viewer dies, its seat
    // stays an idle shell at the same leaf in the same tab, the sibling
    // portal is untouched, and the notice names the kept seat. This
    // inverts the x-8f9d last-portal-only rule: the one-window premise
    // was never true of a fleet with several portals, and the deliberate
    // close path is where "no shell left behind" lives now.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    core.command(client_id, portal_reach_cmd("deadbee2", 1));
    let a_seat = core.portals.get(&0).expect("portal 0 open").seat;
    let b_seat = core.portals.get(&1).expect("portal 1 open").seat;
    let (b_tid, b_leaf_count) = {
        let (sid, ti) = core.session.find_pane(b_seat).expect("b seat in tree");
        let tab = &core.session.squad(sid).unwrap().tabs[ti];
        (tab.id, tree::leaves(&tab.root).len())
    };
    let panes_before = core.panes.len();
    while rx.try_recv().is_ok() {}

    core.close_viewer_died(b_seat, "viewer exited");

    let seat = core.portals.get(&1).expect("portal 1 keeps its seat").seat;
    assert_ne!(seat, b_seat, "the dead viewer was reaped");
    assert!(
        core.panes.get(&seat).is_some_and(|e| e.cmd.is_none()),
        "portal 1's seat holds an idle shell, not a viewer"
    );
    let (sid, ti) = core.session.find_pane(seat).expect("kept seat in tree");
    let tab = &core.session.squad(sid).unwrap().tabs[ti];
    assert_eq!(tab.id, b_tid, "the kept seat stays in the SAME tab");
    assert_eq!(
        tree::leaves(&tab.root).len(),
        b_leaf_count,
        "the shell replaced the viewer at the same leaf"
    );
    assert_eq!(
        core.portals.get(&0).map(|e| e.seat),
        Some(a_seat),
        "portal 0 is untouched"
    );
    assert_eq!(
        core.panes.len(),
        panes_before,
        "the shell replaced the viewer one for one"
    );
    let notices = drain_notices(&mut rx);
    assert!(
        notices
            .iter()
            .any(|t| t.contains("portal 1") && t.contains("deadbee2") && t.contains("seat kept")),
        "the notice names the kept seat and the row: {notices:?}"
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
pub(super) fn thread_core() -> (Core, u64, u64, mpsc::Receiver<ServerMsg>) {
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
fn thread_pane_ctl_new_portal_lands_in_its_own_tab_and_leaves_portal_0_alone() {
    // AC2-HP (x-3ea6): a machine reach asking for `portal new` takes the
    // next free index in a NEW tab. Portal 0 still seats row A, tab T's
    // focus never moves, and the reply names the index the server picked.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let agents = || {
        vec![
            bg_row("target-a", "/tmp/seen", Some("deadbee1")),
            bg_row("target-b", "/tmp/seen", Some("deadbee2")),
        ]
    };
    // Seed: portal 0 seats row A (its viewer is tab T's focus pane P).
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.portal_ctl("deadbee1", 0, PanePlacement::default(), Some(agents()), tx);
    let _ = rx.blocking_recv().expect("seed reply");
    let a_seat = core.portals.get(&0).expect("portal 0 open").seat;
    let (sid, tab_t) = core.session.find_pane(a_seat).expect("A's pane in tree");

    // Reach row B through the same door with portal_new + TabSel::New.
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.portal_ctl(
        "deadbee2",
        0,
        PanePlacement {
            portal_new: true,
            tab: Some(TabSel::New),
            ..Default::default()
        },
        Some(agents()),
        tx,
    );
    match rx.blocking_recv().expect("a reply") {
        ServerMsg::Notice { text } => assert!(
            text.contains("thread pane -> target-b") && text.contains("portal 1"),
            "the reply names B and the resolved index: {text}"
        ),
        other => panic!("expected a Notice landing, got {other:?}"),
    }
    assert_eq!(
        core.portals.get(&0).map(|e| e.seat),
        Some(a_seat),
        "portal 0 still seats row A"
    );
    let b_seat = core
        .portals
        .get(&1)
        .unwrap_or_else(|| panic!("row B at the server-picked index"))
        .seat;
    let (_, b_tab) = core.session.find_pane(b_seat).expect("B's pane in tree");
    assert_ne!(tab_t, b_tab, "row B landed in a tab of its own");
    assert_eq!(
        core.session.squad(sid).expect("squad").tabs[tab_t].focus,
        a_seat,
        "tab T's focus is still row A's viewer"
    );
    assert_eq!(
        core.session.squad(sid).expect("squad").tabs[b_tab].focus,
        b_seat,
        "the new tab's focus is row B's viewer"
    );
    core.reap_pane(a_seat);
    core.reap_pane(b_seat);
}

#[test]
fn thread_pane_ctl_new_portal_joins_the_portal_already_showing_the_row() {
    // A `portal new --tab new` reach for a row a portal already shows must
    // join that portal, not refuse: the focus is the landing, and the
    // caller's resolved index stays empty by design. Before the fix this
    // reach answered Err with the joined "already showing" text.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let agents = || vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.portal_ctl("deadbee1", 0, PanePlacement::default(), Some(agents()), tx);
    let _ = rx.blocking_recv().expect("seed reply");
    let a_seat = core.portals.get(&0).expect("portal 0 open").seat;
    let panes_before = core.panes.len();

    // Same row, control door, portal new + tab new.
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.portal_ctl(
        "target-a",
        0,
        PanePlacement {
            portal_new: true,
            tab: Some(TabSel::New),
            ..Default::default()
        },
        Some(agents()),
        tx,
    );
    match rx.blocking_recv().expect("a reply") {
        ServerMsg::Notice { text } => assert!(
            text.contains("portal 0: already showing target-a"),
            "the reply names the portal that already shows the row: {text}"
        ),
        other => panic!("expected a Notice landing, got {other:?}"),
    }
    assert_eq!(core.portals.len(), 1, "only portal 0 exists");
    assert_eq!(
        core.portals.get(&0).map(|e| e.seat),
        Some(a_seat),
        "portal 0 keeps its seat"
    );
    assert_eq!(
        core.panes.len(),
        panes_before,
        "no second viewer pane was spawned"
    );
    assert!(
        core.portal_landed("target-a", 1),
        "the focus counts as a landing for the caller's resolved index"
    );
    core.reap_pane(a_seat);
}

#[test]
fn portal_landed_check_does_not_count_a_stand_in_seat() {
    // The landed check reads a focus of the row's live viewer as a landing,
    // but a stand-in shell shows the row to nobody: it must never count.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, _p1, _rx) = thread_core();
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.portal_ctl(
        "deadbee1",
        0,
        PanePlacement::default(),
        Some(vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))]),
        tx,
    );
    let _ = rx.blocking_recv().expect("seed reply");
    let a_seat = core.portals.get(&0).expect("portal 0 open").seat;
    assert!(
        core.portal_landed("target-a", 1),
        "fixture: a live viewer elsewhere is a landing"
    );

    core.close_viewer_died(a_seat, "viewer exited");
    let stand_in = core
        .portals
        .get(&0)
        .expect("portal 0 holds a stand-in")
        .seat;
    assert!(
        core.panes.get(&stand_in).is_some_and(|e| e.cmd.is_none()),
        "fixture: the seat now holds an idle shell, not a viewer"
    );
    assert!(
        !core.portal_landed("target-a", 1),
        "a stand-in seat is not a landing"
    );
    core.reap_pane(stand_in);
}

#[test]
fn thread_pane_ctl_new_portal_refuses_when_no_index_is_free() {
    // AC2-ERR (x-3ea6): a `new` reach on a full portal space replies Err
    // naming the ceiling and spawns nothing - it never repoints an index.
    set_attach_program(&["/bin/cat"]);
    let (mut core, _client_id, p1, _rx) = thread_core();
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
    let panes_before = core.panes.len();
    let (tx, rx) = tokio::sync::oneshot::channel::<ServerMsg>();
    core.portal_ctl(
        "deadbee2",
        0,
        PanePlacement {
            portal_new: true,
            tab: Some(TabSel::New),
            ..Default::default()
        },
        Some(vec![bg_row("target-b", "/tmp/seen", Some("deadbee2"))]),
        tx,
    );
    match rx.blocking_recv().expect("a reply") {
        ServerMsg::Err { msg, .. } => assert!(
            msg.contains("no free portal"),
            "the refusal names the exhausted space: {msg}"
        ),
        other => panic!("expected an Err refusal, got {other:?}"),
    }
    assert_eq!(core.panes.len(), panes_before, "nothing spawned");
    assert_eq!(
        core.portals.get(&255).map(|e| e.row_key.as_str()),
        Some("sentinel-255"),
        "no index was repointed"
    );
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
fn stored_tab_trees_captures_a_portal_only_tab_whole() {
    // (x-a9b4) AC1-EDGE: a tab holding ONLY a portal is captured with its
    // name, and if it was the active tab it stays the active one. This is
    // the operator's tab 2: the prune used to skip it whole and lose the
    // name with it.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let thread_pid = core.portals.get(&0).expect("portal 0 open").seat;
    let (sid, thread_tab_idx) = core
        .session
        .find_pane(thread_pid)
        .expect("the thread pane is in the live tree");
    let squad = core.session.squad_mut(sid).unwrap();
    squad.tabs[thread_tab_idx].name = Some("watch".into());
    squad.active_tab = thread_tab_idx;

    let (trees, active) = core.stored_tab_trees(sid).unwrap();
    assert_eq!(
        trees.len(),
        2,
        "both tabs are captured: the shell tab and the portal-only tab"
    );
    let watch = trees
        .iter()
        .find(|t| t.tab_name.as_deref() == Some("watch"))
        .expect("the portal tab is in the capture");
    assert_eq!(watch.slots.len(), 1, "one slot: the portal seat");
    let slot = &watch.slots[0];
    let portal = slot.portal.as_ref().expect("the seat carries its portal");
    assert_eq!(portal.index, 0, "the open index is kept");
    assert_eq!(portal.row, "deadbee1", "the row key is kept");
    assert!(
        matches!(&slot.binding, LayoutBinding::Shell),
        "the binding stays Shell: at restore the seat is a shell until filled"
    );
    assert_eq!(
        active,
        trees
            .iter()
            .position(|t| t.tab_name.as_deref() == Some("watch"))
            .expect("the portal tab is in the capture"),
        "the portal-only tab is the active one"
    );

    // Positive control: capture touched the STORED shape, not the live tree.
    assert!(core.session.find_pane(thread_pid).is_some());
    assert_eq!(
        core.portals.get(&0).map(|entry| entry.seat),
        Some(thread_pid)
    );
}

#[test]
fn stored_tab_trees_keeps_the_active_tab_when_it_holds_only_portals() {
    // (x-a9b4) The remap the old prune needed is gone with the prune: a
    // portal-only ACTIVE tab sandwiched between ordinary siblings is
    // captured in its position and active_tab points at it, not at some
    // other tab.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let thread_pid = core.portals.get(&0).expect("portal 0 open").seat;
    let (sid, thread_tab_idx) = core
        .session
        .find_pane(thread_pid)
        .expect("the thread pane is in the live tree");

    // Sandwich the portal tab between two ordinary sibling tabs, and
    // make the portal tab (pure portal seat, no other content) active.
    let squad = core.session.squad_mut(sid).unwrap();
    let thread_tab = squad.tabs.remove(thread_tab_idx);
    squad.tabs = vec![leaf_tab(9001, 9101), thread_tab, leaf_tab(9002, 9102)];
    squad.active_tab = 1;

    let (trees, active) = core.stored_tab_trees(sid).unwrap();
    assert_eq!(
        trees.len(),
        3,
        "every tab is captured, portal-only tab included"
    );
    assert_eq!(active, 1, "the portal-only tab is still the active one");
    assert!(
        trees[1]
            .slots
            .iter()
            .any(|s| s.portal.as_ref().is_some_and(|p| p.row == "deadbee1")),
        "the middle tree carries the portal slot"
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
    core.close_viewer_died(a_viewer, "viewer exited");
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

/// The new-portal reach the `P` picker's shift+HJKL sends: allocate the
/// index, split beside the viewed workspace.
fn portal_new_split_cmd(id: &str) -> Command {
    Command::AttachAgent {
        id: id.into(),
        placement: PanePlacement {
            portal_new: true,
            split: Some(Dir::Right),
            target: PaneTarget::SquadId(1),
            ..Default::default()
        },
    }
}

#[test]
fn portal_new_split_lands_beside_the_focused_pane() {
    // (x-4572, AC2-HP + AC2-EDGE) A new-portal reach with split Right lands
    // in the TARGET tab beside its shell - no tab added - and the STALE
    // reuse of that index keeps the direction: the open-close-split-again
    // loop splits right twice, never falling to place_with's Down default.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, p1, mut rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];
    let tabs_before = core.session.squad(1).unwrap().tabs.len();

    // The split reach first, while tab 1 is the viewed tab: a placement
    // naming no tab splits beside the squad's ACTIVE tab, so the AC's
    // precondition is "the viewed tab holds the shell".
    core.command(client_id, portal_new_split_cmd("deadbee1"));

    let seat = core.portals.get(&0).expect("portal 0 open").seat;
    assert_eq!(
        core.portals.get(&0).map(|e| e.tab),
        Some(1),
        "the split lands in the targeted tab"
    );
    assert_eq!(
        core.session.squad(1).unwrap().tabs.len(),
        tabs_before,
        "no tab is added"
    );
    {
        let tab = core
            .session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .find(|t| t.id == 1)
            .unwrap();
        match &tab.root {
            Node::Branch { axis, .. } => {
                assert_eq!(*axis, crate::tree::Axis::Horizontal, "a RIGHT split");
            }
            other => panic!("expected a split root, got {other:?}"),
        }
        let mut leaves = tree::leaves(&tab.root);
        leaves.sort_unstable();
        let mut expected = vec![p1, seat];
        expected.sort_unstable();
        assert_eq!(leaves, expected, "beside the shell, nothing else moved");
    }
    assert!(
        !drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("tab full")),
        "a split with room never falls back"
    );

    // AC2-EDGE: a second portal now, so the split portal (0) is not the
    // last index - the reuse scan has a live index to skip. It opens
    // unplaced (a fresh tab of its own). portal_new, not the thread_pane
    // alias: that alias names portal 0, which is live and would repoint.
    core.command(
        client_id,
        Command::AttachAgent {
            id: "deadbee2".into(),
            placement: PanePlacement {
                portal_new: true,
                ..Default::default()
            },
        },
    );
    assert_eq!(core.portals.get(&1).map(|e| e.tab), Some(2));
    // Close the split seat while portal 1 stays live. The reuse of
    // index 0 remembers tab 1; the caller's direction must survive it.
    let stale_seat = core.portals.get(&0).unwrap().seat;
    core.close_pane(stale_seat);
    core.command(client_id, portal_new_split_cmd("deadbee1"));
    let seat2 = core.portals.get(&0).expect("portal 0 reused").seat;
    assert_ne!(seat2, stale_seat, "a fresh viewer took the seat");
    assert_eq!(
        core.portals.get(&0).map(|e| e.tab),
        Some(1),
        "the reused index lands in the remembered tab"
    );
    let tab = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 1)
        .unwrap();
    match &tab.root {
        Node::Branch { axis, .. } => {
            assert_eq!(
                *axis,
                crate::tree::Axis::Horizontal,
                "the remembered tab keeps the caller's RIGHT, not a Down default"
            );
        }
        other => panic!("expected a split root, got {other:?}"),
    }
    let mut leaves = tree::leaves(&tab.root);
    leaves.sort_unstable();
    let mut expected = vec![p1, seat2];
    expected.sort_unstable();
    assert_eq!(leaves, expected, "beside the shell again");
}

#[tokio::test]
async fn portal_new_split_on_a_claude_row_survives_the_reentry_replay() {
    // (x-4572, AC2-ERR) A claude row's first reach pass parks; the replay
    // carries the placement the Drive arm built. That replay must name the
    // portal AND keep the caller's split and target, or every claude-row
    // split silently becomes a new tab - the common case.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, p1, _rx) = thread_core();
    core.agents = vec![claude_row("claude-row", "deadbee1")];
    let caller = PanePlacement {
        portal_new: true,
        split: Some(Dir::Right),
        target: PaneTarget::SquadId(1),
        ..Default::default()
    };
    let replay = portal_replay_placement(&caller, 0);
    assert_eq!(replay.portal, Some(0), "the replay names the reached index");
    assert!(!replay.portal_new);
    assert_eq!(replay.split, caller.split, "the split survives");
    assert_eq!(replay.target, caller.target, "the target survives");

    let panes_before = core.panes.len();
    core.command(
        client_id,
        Command::AttachAgent {
            id: "deadbee1".into(),
            placement: caller,
        },
    );
    assert!(core.portals.is_empty(), "the parked reach opens no portal");
    assert_eq!(
        core.panes.len(),
        panes_before,
        "the parked reach opens no pane"
    );

    core.handle(CoreMsg::ReentryPlanReady {
        id: client_id,
        request: Box::new(ReentrySpawnRequest::Attach {
            attach_id: "deadbee1".into(),
            placement: portal_replay_placement(
                &PanePlacement {
                    portal_new: true,
                    split: Some(Dir::Right),
                    target: PaneTarget::SquadId(1),
                    ..Default::default()
                },
                0,
            ),
        }),
        verdict: Ok(ReentryVerdict {
            argv: vec!["/bin/cat".into()],
            env: vec![],
            config_dir: None,
            mechanism: None,
        }),
    });

    let seat = core
        .portals
        .get(&0)
        .expect("the replay opens portal 0")
        .seat;
    assert_eq!(
        core.portals.get(&0).map(|e| e.tab),
        Some(1),
        "the replayed split lands in the targeted tab, not a new one"
    );
    assert_eq!(core.session.squad(1).unwrap().tabs.len(), 1);
    let tab = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 1)
        .unwrap();
    match &tab.root {
        Node::Branch { axis, .. } => {
            assert_eq!(*axis, crate::tree::Axis::Horizontal, "beside the shell");
        }
        other => panic!("expected a split root, got {other:?}"),
    }
    let mut leaves = tree::leaves(&tab.root);
    leaves.sort_unstable();
    let mut expected = vec![p1, seat];
    expected.sort_unstable();
    assert_eq!(
        leaves, expected,
        "the replayed split lands beside the shell"
    );
    core.reap_pane(seat); // don't leak the stand-in child
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
    // replacement viewer lands where the operator had it. The seat's tab
    // keeps a second pane so the operator close leaves the tab alive: an
    // operator close of a LONE-leaf tab closes the tab (the AC7 rule),
    // and a gone tab can win nothing.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, thread_reach_cmd("deadbee1"));
    let tab_a = core.portals.get(&0).expect("portal 0 open").tab;
    let seat_a = core.portals.get(&0).unwrap().seat;
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
    // A neighbour pane shares the seat's tab, so the operator close below
    // stales the seat without removing the tab.
    let (sid_a, ti_a) = core.session.find_pane(seat_a).unwrap();
    let neighbour = core
        .spawn_pane(24, 40, "/tmp/seen")
        .expect("neighbour pane");
    {
        let squad = core.session.squad_mut(sid_a).unwrap();
        let tab = &mut squad.tabs[ti_a];
        let leaf = std::mem::replace(&mut tab.root, Node::Leaf(neighbour));
        tab.root = Node::Branch {
            axis: crate::tree::Axis::Horizontal,
            children: vec![(0.5, leaf), (0.5, Node::Leaf(neighbour))],
        };
    }
    // An operator close is what leaves the entry stale now: the entry
    // goes stale on purpose, and the tab (with the neighbour) survives.
    core.close_pane(seat_a);
    assert!(
        core.session
            .squad(sid_a)
            .unwrap()
            .tabs
            .iter()
            .any(|t| t.id == tab_a),
        "fixture: the shared tab survives the operator close"
    );

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

    let flow = core.close_viewer_died(lone_viewer, "viewer exited");

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

    core.close_viewer_died(a_viewer, "viewer exited");
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
            mechanism: None,
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
            mechanism: None,
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
    core.close_viewer_died(a_viewer, "viewer exited"); // the stand-in takes the seat
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

// ---- (x-a9b4) portals survive the restart: capture, hold, fill ----------

/// A store row whose named tab is a two-child split of a portal slot
/// (index 1, row `deadbee1`) and an ordinary shell slot. The AC3-HP
/// fixture, shared by the hold and fill tests. The scratch is returned
/// alive: dropping it would repoint the store path and delete the row.
fn hold_fixture(store_name: &str) -> StoreScratch {
    let s = StoreScratch::new(store_name);
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let origin_str = origin.to_string_lossy().into_owned();
    let key = crate::squad_store::origin_key(&[origin_str.clone()]);
    crate::squad_store::upsert("p-hold", &key, &[origin_str.clone()], &[]).unwrap();
    let tree = crate::proto::LayoutTreeSpec::Split {
        axis: crate::tree::Axis::Horizontal,
        children: vec![
            crate::proto::LayoutTreeChild {
                weight: 0.5,
                tree: crate::proto::LayoutTreeSpec::Slot("portal1".into()),
            },
            crate::proto::LayoutTreeChild {
                weight: 0.5,
                tree: crate::proto::LayoutTreeSpec::Slot("p2".into()),
            },
        ],
    };
    let slots = vec![
        crate::proto::LayoutSlot {
            name: "portal1".into(),
            binding: LayoutBinding::Shell,
            cwd: None,
            portal: Some(PortalSlot {
                index: 1,
                row: "deadbee1".into(),
                harness: None,
                session_id: None,
            }),
            pane_id: None,
        },
        crate::proto::LayoutSlot {
            name: "p2".into(),
            binding: LayoutBinding::Shell,
            cwd: None,
            portal: None,
            pane_id: None,
        },
    ];
    crate::squad_store::set_tab_trees(
        "p-hold",
        &key,
        &[],
        &[crate::squad_store::StoredTabTree {
            tab_name: Some("watch".into()),
            tree,
            slots,
            focus: None,
        }],
        Some(0),
    )
    .unwrap();
    s
}

/// `empty_core` plus the shared pane-output receiver, so a test can judge
/// what actually traveled a pty (a typed command's echo) and not only what
/// a direct call fed the VT.
fn empty_core_with_output() -> (Core, mpsc::Receiver<(u64, PaneChunk)>) {
    let (out_tx, out_rx) = mpsc::channel::<(u64, PaneChunk)>(256);
    let mut core = empty_core();
    core.out_tx = out_tx;
    (core, out_rx)
}

#[test]
fn restore_held_seat_feeds_the_message_and_never_types_a_command() {
    // AC1-HP: the held message paints once onto the seat's screen. No
    // shell input is typed at the placeholder, so the message cannot come
    // back a second time as an echoed command or a third time as command
    // output.
    let _s = hold_fixture("portal-msg-once");
    let (mut core, mut out_rx) = empty_core_with_output();
    core.shells = vec!["/bin/cat".into()];
    let (c, _rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let seat = core.portals.get(&1).expect("portal 1 held").seat;
    // Give any typed-command echo time to travel the pty into the shared
    // output channel before judging it.
    std::thread::sleep(std::time::Duration::from_millis(250));
    let mut echoed = String::new();
    while let Ok((pid, chunk)) = out_rx.try_recv() {
        if pid == seat {
            if let PaneChunk::Output(bytes) = chunk {
                echoed.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
    assert!(
        !echoed.contains("printf"),
        "the placeholder was typed a command: {echoed:?}"
    );
    let text = core.panes[&seat].vt.text();
    assert_eq!(
        text.matches("held across restart").count(),
        1,
        "the message paints exactly once: {text:?}"
    );
}

#[test]
fn restore_message_with_an_apostrophe_is_literal_text_never_shell_input() {
    // AC1-EDGE: the message is screen text. An apostrophe feeds to the VT
    // verbatim; nothing is quoted for a shell and nothing is executed.
    let (mut core, mut out_rx) = empty_core_with_output();
    core.shells = vec!["/bin/cat".into()];
    let p = core.spawn_pane(24, 40, "/tmp").expect("pane");
    core.write_restore_message(p, "portal 0 (it's held, across restart) - reach the row");
    std::thread::sleep(std::time::Duration::from_millis(250));
    let mut echoed = String::new();
    while let Ok((pid, chunk)) = out_rx.try_recv() {
        if pid == p {
            if let PaneChunk::Output(bytes) = chunk {
                echoed.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
    assert!(
        !echoed.contains("printf"),
        "a command was typed: {echoed:?}"
    );
    let text = core.panes[&p].vt.text();
    assert!(
        text.contains("it's held, across restart"),
        "the message is literal: {text:?}"
    );
}

#[test]
fn restore_holds_a_portal_slot_idle_in_its_seat() {
    // AC3-HP: the named tab comes back with its split, the portal entry
    // names the first child of the split with the stored row key, that
    // pane runs no command, and the pane says what it waits for.
    let _s = hold_fixture("portal-hold-hp");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);

    let squad = core
        .session
        .squads
        .iter()
        .find(|s| !s.tabs.is_empty())
        .expect("a squad was restored");
    let tab = squad
        .tabs
        .iter()
        .find(|t| t.name.as_deref() == Some("watch"))
        .expect("the named tab came back");
    let leaves = tree::leaves(&tab.root);
    assert_eq!(leaves.len(), 2, "the split has two leaves");
    let portal = core.portals.get(&1).expect("portal 1 is held again");
    assert_eq!(portal.row_key, "deadbee1", "the stored row key came back");
    assert_eq!(portal.tab, tab.id, "the seat lives in the restored tab");
    assert!(
        leaves.contains(&portal.seat),
        "the seat is a leaf of the restored split"
    );
    let entry = core.panes.get(&portal.seat).expect("seat pane exists");
    assert_eq!(
        entry.portal_hold.as_deref(),
        Some("deadbee1"),
        "the placeholder carries its held row in its own argv"
    );
    assert_eq!(entry.name.as_deref(), Some("portal1"), "the seat is named");
    assert!(
        entry.vt.text().contains("held across restart"),
        "the pane says what it waits for"
    );
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("held 0 worker pane(s) and 1 portal(s)"),
        "the restore receipt names the held portal: {notices}"
    );
}

#[test]
fn the_held_live_reading_keys_on_provenance_not_command_presence() {
    // AC2: one classifier behind every portal door. A marked placeholder
    // reads held even though its wrapper argv gives it `cmd: Some` (the
    // state a keeper re-adoption produces); a bare shell reads held; only
    // a real command with no marker reads live.
    let _s = hold_fixture("portal-classifier");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let (c, _rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let seat = core.portals.get(&1).expect("fixture: portal 1 held").seat;
    assert!(
        core.panes[&seat].cmd.is_some(),
        "fixture: the placeholder's argv yields cmd"
    );
    assert!(
        !core.portal_seat_is_viewer(seat),
        "the marker keeps the seat held"
    );
    // A bare shell (no wrapper, no marker) is held too.
    core.panes.get_mut(&seat).unwrap().cmd = None;
    core.panes.get_mut(&seat).unwrap().portal_hold = None;
    assert!(
        !core.portal_seat_is_viewer(seat),
        "a bare shell is not a viewer"
    );
    // A surviving viewer: a real command and no marker.
    core.panes.get_mut(&seat).unwrap().cmd = Some("cat".into());
    assert!(core.portal_seat_is_viewer(seat), "a real viewer is live");
}

#[test]
fn a_surviving_viewer_rearmed_live_still_focuses_without_a_second_viewer() {
    // AC2-EDGE: a viewer that genuinely survived the restart re-adopts
    // with `cmd: Some` and no marker. A default reach focuses that pane
    // instead of minting a second viewer.
    set_attach_program(&["/bin/cat"]);
    let _s = hold_fixture("portal-survivor");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let seat = core.portals.get(&1).expect("fixture: portal 1 held").seat;
    core.panes.get_mut(&seat).unwrap().portal_hold = None;
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    let panes_before = core.panes.len();

    core.command(1, thread_reach_cmd("deadbee1"));

    let notices = drain_notices(&mut rx);
    assert!(
        notices.iter().any(|t| t.contains("already showing")),
        "the surviving viewer is the row's home: {notices:?}"
    );
    assert_eq!(core.panes.len(), panes_before, "no second viewer spawned");
    assert_eq!(
        core.portals.get(&1).map(|p| p.seat),
        Some(seat),
        "the seat is untouched"
    );
}

#[test]
fn a_fill_under_a_foreign_session_id_refuses_naming_both_ids() {
    // AC2-ERR: the recorded session guard binds the seat to the row's
    // incarnation at capture. A different full id under the same key is a
    // different thread wearing a familiar label: the fill refuses, names
    // both ids, and the seat stays held.
    let _s = hold_fixture("portal-guard");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let seat = core.portals.get(&1).expect("fixture: portal 1 held").seat;
    core.portal_session_guards
        .insert(1, "sid-recorded".to_string());
    let mut row = bg_row("target-a", "/tmp/seen", Some("deadbee1"));
    row.harness_session_id = Some("sid-arrived".to_string());
    core.agents = vec![row];
    let panes_before = core.panes.len();

    core.command(1, Command::FocusPane(seat));

    let notices = drain_notices(&mut rx);
    assert!(
        notices
            .iter()
            .any(|t| t.contains("sid-recorded") && t.contains("sid-arrived")),
        "the refusal names both ids: {notices:?}"
    );
    assert_eq!(core.panes.len(), panes_before, "nothing spawned");
    assert_eq!(
        core.portals.get(&1).map(|p| p.seat),
        Some(seat),
        "the seat stays held"
    );
    assert!(
        core.portal_session_guards.contains_key(&1),
        "the guard stays armed"
    );
}

#[test]
fn a_fill_with_no_live_row_names_the_register_action_once_per_attempt() {
    // AC3-ERR/EDGE: a held seat whose row is gone keeps the shell and
    // answers an attempted fill with ONE refusal naming the index, the
    // row, and one action. No per-frame notices: a second gesture earns
    // exactly one more refusal, nothing between.
    let _s = hold_fixture("portal-fill-norow");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let seat = core.portals.get(&1).expect("fixture: portal 1 held").seat;
    core.agents = vec![]; // no live row answers deadbee1
    let panes_before = core.panes.len();

    core.command(1, Command::FocusPane(seat));

    let notices = drain_notices(&mut rx);
    let refusals: Vec<_> = notices
        .iter()
        .filter(|t| t.contains("no live row answers"))
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "one refusal for the attempt: {notices:?}"
    );
    assert!(
        refusals[0].contains("portal 1") && refusals[0].contains("deadbee1"),
        "the index and the row are named: {}",
        refusals[0]
    );
    assert!(
        refusals[0].contains("fno agents register"),
        "one action is named: {}",
        refusals[0]
    );
    assert_eq!(core.panes.len(), panes_before, "no viewer starts");
    assert_eq!(
        core.portals.get(&1).map(|p| p.seat),
        Some(seat),
        "the seat stays held"
    );

    core.command(1, Command::FocusPane(seat));
    let notices = drain_notices(&mut rx);
    assert_eq!(
        notices
            .iter()
            .filter(|t| t.contains("no live row answers"))
            .count(),
        1,
        "one refusal per attempt, none between: {notices:?}"
    );
}

#[test]
fn restore_sends_a_clashed_portal_index_to_the_next_free_one() {
    // AC3-EDGE: two stored squads both carry a portal slot at index 0.
    // The second lands at the next free index and the notice says so; no
    // insert overwrites a held entry.
    let _s = hold_fixture("portal-hold-clash");
    // Second squad: same store, different name and origin dir.
    let origin = _s.dir.join("repo2");
    std::fs::create_dir_all(&origin).unwrap();
    let origin_str = origin.to_string_lossy().into_owned();
    let key = crate::squad_store::origin_key(&[origin_str.clone()]);
    crate::squad_store::upsert("p-hold-2", &key, &[origin_str.clone()], &[]).unwrap();
    crate::squad_store::set_tab_trees(
        "p-hold-2",
        &key,
        &[],
        &[crate::squad_store::StoredTabTree {
            tab_name: Some("other".into()),
            tree: crate::proto::LayoutTreeSpec::Slot("portal1".into()),
            slots: vec![crate::proto::LayoutSlot {
                name: "portal1".into(),
                binding: LayoutBinding::Shell,
                cwd: None,
                portal: Some(PortalSlot {
                    index: 1,
                    row: "deadbee9".into(),
                    harness: None,
                    session_id: None,
                }),
                pane_id: None,
            }],
            focus: None,
        }],
        Some(0),
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);

    assert_eq!(
        core.portals.get(&1).map(|p| p.row_key.as_str()),
        Some("deadbee1"),
        "the first squad keeps its index"
    );
    assert_eq!(
        core.portals.get(&0).map(|p| p.row_key.as_str()),
        Some("deadbee9"),
        "the second squad lands at the next free index"
    );
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("at portal 0 instead"),
        "the re-seat is named: {notices}"
    );
}

#[test]
fn the_restore_notice_names_both_held_kinds() {
    // AC4-HP: one worker pane and two portal seats held; the startup
    // notice reads both counts.
    let s = hold_fixture("portal-hold-notice");
    // A second tab on the same squad carrying one more portal slot, and
    // a worker member that holds a pane across the restore.
    let origin = s.dir.join("repo");
    let origin_str = origin.to_string_lossy().into_owned();
    let key = crate::squad_store::origin_key(&[origin_str.clone()]);
    let mut loaded = crate::squad_store::load();
    let mut trees = std::mem::take(&mut loaded.squads[0].tab_trees);
    trees.push(crate::squad_store::StoredTabTree {
        tab_name: Some("second".into()),
        tree: crate::proto::LayoutTreeSpec::Slot("portal2".into()),
        slots: vec![crate::proto::LayoutSlot {
            name: "portal2".into(),
            binding: LayoutBinding::Shell,
            cwd: None,
            portal: Some(PortalSlot {
                index: 2,
                row: "deadbee2".into(),
                harness: None,
                session_id: None,
            }),
            pane_id: None,
        }],
        focus: None,
    });
    crate::squad_store::set_tab_trees("p-hold", &key, &loaded.squads[0].origins, &trees, Some(0))
        .unwrap();
    // A worker member that holds a pane across the restore.
    crate::squad_store::upsert(
        "p-hold",
        &key,
        &loaded.squads[0].origins,
        &[crate::squad_store::StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: Some("t-live-one".into()),
            harness: Some("codex".into()),
            harness_session_id: Some("live-session".into()),
            pane_id: None,
        }],
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let mut one = exited_claude_row("t-live-one", None);
    one.harness = Some("codex".into());
    one.harness_session_id = Some("live-session".into());
    core.agents = vec![one];
    let _known = KnownWorkersGuard;
    set_known_workers(&["t-live-one"]);
    set_restore_policy(crate::digest_overlay::MuxRestorePolicy::Hold);
    let _pol = RestorePolicyGuard;
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);

    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("held 1 worker pane(s) and 2 portal(s)"),
        "the receipt names both held kinds: {notices}"
    );
}

#[test]
fn focusing_a_held_portal_seat_fills_it_in_place() {
    // AC6-HP: the held seat at portal 1 shows a shell; focusing it swaps
    // the row's viewer into THAT seat; portals[1] names the new pane.
    set_attach_program(&["/bin/cat"]);
    let _s = hold_fixture("portal-fill-focus");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let (c, _rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let seat = core.portals.get(&1).expect("fixture: portal 1 held").seat;
    let (sid, ti) = core.session.find_pane(seat).expect("seat in tree");
    let tab_id = core.session.squad(sid).unwrap().tabs[ti].id;
    let before = core.next_pane_id;
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];

    core.command(1, Command::FocusPane(seat));

    let filled = core.portals.get(&1).expect("portal 1 still open").seat;
    assert_ne!(filled, seat, "the shell seat was replaced");
    assert_eq!(filled, before, "exactly one pane was spawned");
    assert_eq!(
        core.panes[&filled].cmd.as_deref(),
        Some("cat"),
        "the seat now runs the row's tier argv"
    );
    assert_eq!(
        core.portals.get(&1).map(|p| p.tab),
        Some(tab_id),
        "the viewer landed in the same tab the seat held"
    );
}

#[test]
fn a_default_reach_fills_the_held_seat_naming_the_row() {
    // AC7-HP: Enter on the row carries no explicit index. A held portal
    // at index 1 names the row; the reach fills THAT seat instead of
    // stranding it while a fresh viewer mints at 0.
    set_attach_program(&["/bin/cat"]);
    let _s = hold_fixture("portal-fill-default");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let held_seat = core.portals.get(&1).expect("fixture: portal 1 held").seat;
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];

    core.command(
        1,
        Command::AttachAgent {
            id: "deadbee1".into(),
            placement: PanePlacement {
                thread_pane: true,
                ..Default::default()
            },
        },
    );

    assert_eq!(
        core.portals
            .get(&1)
            .map(|p| core.panes[&p.seat].cmd.as_deref()),
        Some(Some("cat")),
        "the held seat now runs the viewer"
    );
    assert!(
        core.panes.get(&held_seat).is_none(),
        "the shell stand-in was reaped"
    );
    assert!(!core.portals.contains_key(&0), "no portal 0 was minted");
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("portal 1: resuming"),
        "the resume is named: {notices}"
    );
}

#[test]
fn an_explicit_reach_never_hijacks_a_held_seat() {
    // AC7-EDGE: `--portal 0` means portal 0. The held seat at 1 stays
    // held; a fresh viewer opens at 0.
    set_attach_program(&["/bin/cat"]);
    let _s = hold_fixture("portal-fill-explicit");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let (c, _rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let held_seat = core.portals.get(&1).expect("fixture: portal 1 held").seat;
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];

    core.command(1, portal_reach_cmd("deadbee1", 0));

    assert_eq!(
        core.portals
            .get(&0)
            .map(|p| core.panes[&p.seat].cmd.as_deref()),
        Some(Some("cat")),
        "a fresh viewer opened at the named index"
    );
    assert_eq!(
        core.portals.get(&1).map(|p| p.seat),
        Some(held_seat),
        "the held seat at 1 is untouched"
    );
    assert!(core.panes.get(&held_seat).is_some());
}

#[test]
fn a_held_seat_whose_row_is_gone_stays_a_readable_shell() {
    // AC8-EDGE: the held row never appears in the registry. Focus falls
    // through to a plain focus: the pane stays a readable shell naming
    // the row and nothing is spawned.
    let _s = hold_fixture("portal-fill-gone");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    let (c, _rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);
    let seat = core.portals.get(&1).expect("fixture: portal 1 held").seat;
    // The placeholder names its row in its pane text (asserted at spawn
    // size; a later split's geometry pass may rewrap the visible grid).
    assert!(
        core.panes[&seat].vt.text().contains("deadbee1"),
        "the placeholder names its row"
    );
    let panes_before = core.panes.len();
    core.agents = vec![]; // no row answers deadbee1

    core.command(1, Command::FocusPane(seat));

    assert_eq!(core.panes.len(), panes_before, "nothing was spawned");
    assert!(
        !core.panes[&seat].vt.is_pristine_idle_shell(),
        "the placeholder is still readable"
    );
    assert_eq!(
        core.portals.get(&1).map(|p| p.seat),
        Some(seat),
        "the seat is still the held portal"
    );
}

#[test]
fn a_portal_onto_a_done_row_prunes_with_the_done_set() {
    // AC5-HP: the only slot is a portal onto attach id deadbee1 and the
    // done set carries that id (the forgotten-tombstone arm fills it).
    // No pane is minted and the skipped-tab count includes the tab.
    let s = StoreScratch::new("portal-done");
    let origin = s.dir.join("repo");
    std::fs::create_dir_all(&origin).unwrap();
    let origin_str = origin.to_string_lossy().into_owned();
    let key = crate::squad_store::origin_key(&[origin_str.clone()]);
    crate::squad_store::upsert("", &key, &[origin_str.clone()], &[]).unwrap();
    // The member died and the registry forgot its attach id: the same
    // arm that prunes the member puts the id in done_bindings.
    crate::squad_store::upsert(
        "",
        &key,
        &[origin_str.clone()],
        &[crate::squad_store::StoredMember {
            attach_id: "deadbee1".into(),
            tombstone: true,
            tombstone_reason: None,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: None,
            harness: None,
            harness_session_id: None,
            pane_id: None,
        }],
    )
    .unwrap();
    crate::squad_store::set_tab_trees(
        "",
        &key,
        &[],
        &[crate::squad_store::StoredTabTree {
            tab_name: None,
            tree: crate::proto::LayoutTreeSpec::Slot("portal1".into()),
            slots: vec![crate::proto::LayoutSlot {
                name: "portal1".into(),
                binding: LayoutBinding::Shell,
                cwd: None,
                portal: Some(PortalSlot {
                    index: 1,
                    row: "deadbee1".into(),
                    harness: None,
                    session_id: None,
                }),
                pane_id: None,
            }],
            focus: None,
        }],
        None,
    )
    .unwrap();
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    core.agents = vec![bg_row("other", "/tmp/seen", Some("cafebabe"))];
    // The forgotten-tombstone arm reads the registry, not core.agents; pin
    // the seam to rows naming OTHER ids so deadbee1 reads as forgotten even
    // on a machine with no registry file (CI).
    let _registry = RestoreRegistryRowsGuard;
    set_restore_registry_rows(vec![bg_row("other", "/tmp/seen", Some("cafebabe"))]);
    let (c, mut rx) = client_with_rx(1);
    core.clients.push(c);
    core.restore_squads(24, 80, 999);

    assert!(
        core.portals.is_empty(),
        "no portal came back for a done row"
    );
    assert_eq!(
        core.panes.len(),
        1,
        "only the empty-squad fallback shell exists; the done portal slot minted nothing"
    );
    let notices = drain_notices(&mut rx).join("\n");
    assert!(
        notices.contains("1 done tab(s)"),
        "the skipped tab is counted: {notices}"
    );
}

#[test]
fn the_notice_latch_holds_through_the_restart_path() {
    // AC9-HP: restore held portals with no client attached - the ordinary
    // startup ordering - then attach a client and feed a live paneless
    // row. The discoverability notice never fires (portals is non-empty)
    // and the latch stays unset; the restore receipt was the delivered
    // portal notice, and it is asserted present, not inferred from an
    // absence.
    set_attach_program(&["/bin/cat"]);
    let _s = hold_fixture("portal-latch");
    let mut core = empty_core();
    core.shells = vec!["/bin/cat".into()];
    core.restore_squads(24, 80, 999);
    assert!(!core.portal_noticed, "no client: the latch stays unset");

    let (c, mut rx) = client_with_rx(9);
    core.clients.push(c);
    while rx.try_recv().is_ok() {}
    core.agents = vec![bg_row("bg-worker", "/tmp/seen", None)];
    core.handle_msg(CoreMsg::AgentRows {
        rows: core.agents.clone(),
        branches: HashMap::new(),
        tails: HashMap::new(),
        ctx: HashMap::new(),
        read_ok: false,
    });

    let notices = drain_notices(&mut rx);
    assert!(
        !notices.iter().any(|t| t.contains("thread row present")),
        "a held portal disarms the discoverability notice: {notices:?}"
    );
    assert!(!core.portal_noticed, "nothing latched");
}

/// (x-9b37) AC2: with two portals open, the reap of one viewer's subject
/// closes only that viewer's pane. The neighbour's pane, its portal entry,
/// and its tab all survive. x-3349 sharpens the pin: the reaped viewer's
/// seat now KEEPS its place as a shell, which still never cascades.
#[test]
fn reaping_one_portals_subject_leaves_the_sibling_portal_alone() {
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];

    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    core.command(client_id, portal_reach_cmd("deadbee2", 1));
    let a_seat = core.portals.get(&0).expect("portal 0 open").seat;
    let b_seat = core.portals.get(&1).expect("portal 1 open").seat;

    core.close_viewer_died(a_seat, "child exited");

    assert!(
        !core.panes.contains_key(&a_seat),
        "the reaped subject's viewer is gone"
    );
    let kept = core.portals.get(&0).expect("portal 0 keeps its seat").seat;
    assert_ne!(kept, a_seat, "a shell stand-in took the seat");
    assert!(
        core.panes.contains_key(&b_seat),
        "the sibling portal's pane survives the neighbour's reap"
    );
    assert_eq!(
        core.portals.get(&1).map(|p| p.seat),
        Some(b_seat),
        "portal 1 still seats the sibling viewer"
    );
    assert!(
        core.session.find_pane(b_seat).is_some(),
        "the tab still exists for the sibling"
    );
    let _ = drain_notices(&mut rx);
}

/// (x-9b37 AC3, reshaped by x-3349) a VANISHING portal says so, naming the
/// portal index, the row, and the reason. What vanishes a portal now is an
/// operator close (a death keeps the seat and announces it instead), so the
/// first half drives the operator path; the second half pins that the death
/// notice reads as kept, not lost.
#[test]
fn a_vanishing_portal_says_so_and_names_its_row() {
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];

    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    core.command(client_id, portal_reach_cmd("deadbee2", 1));
    let a_seat = core.portals.get(&0).expect("portal 0 open").seat;
    let b_seat = core.portals.get(&1).expect("portal 1 open").seat;
    while rx.try_recv().is_ok() {}

    core.close_pane_reasoned(b_seat, "closed by operator");
    let loss = collect_until_portal_closed(&mut rx, "portal 1").expect("the loss notice arrived");
    assert!(
        loss.contains("deadbee2") && loss.contains("closed by operator"),
        "the notice names the row and the reason: {loss:?}"
    );

    // A viewer DEATH keeps the seat and announces the kept shell instead:
    // evidence named, nothing lost.
    core.close_viewer_died(a_seat, "child exited");
    let kept = collect_until_portal_closed(&mut rx, "portal 0").expect("the kept notice arrived");
    assert!(
        kept.contains("deadbee1") && kept.contains("seat kept"),
        "the death notice reads as kept, not lost: {kept:?}"
    );
}

// ---- (x-3349) closing a portal is its own gesture -------------------------

#[test]
fn close_portal_closes_only_the_seat_and_leaves_the_row_live() {
    // AC4-HP. ClosePortal closes the viewer pane with no stand-in, never
    // touches the registry (no Stop, no Remove), and the seat entry goes
    // stale so the next reach lands back in the remembered tab.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    let seat = core.portals.get(&0).expect("portal 0 open").seat;
    let panes_before = core.panes.len();

    core.command(client_id, Command::ClosePortal { seat });

    assert!(!core.panes.contains_key(&seat), "the seat pane closed");
    assert_eq!(
        core.panes.len(),
        panes_before - 1,
        "the viewer was reaped and NO stand-in replaced it"
    );
    assert_eq!(
        core.portals.get(&0).map(|e| e.seat),
        Some(seat),
        "the slot stays stale-named so a later reach lands in the same tab"
    );
    let rows = core.agent_rows();
    let row = rows
        .iter()
        .find(|r| r.name == "target-a")
        .expect("the row still builds");
    assert!(row.pane_id.is_none(), "the row shows through no pane now");
    assert_eq!(row.portal, None, "the row wears no portal marker now");
    assert!(!row.exited, "the row reads live, not stopped or removed");
}

#[test]
fn close_portal_refuses_a_pane_that_is_no_portal_seat() {
    // AC5-ERR. A pane id that is not a live portal seat (a worker pane,
    // a plain shell) gets the notice and nothing closes.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    while rx.try_recv().is_ok() {}

    core.command(client_id, Command::ClosePortal { seat: p1 });

    assert!(
        core.panes.contains_key(&p1),
        "the non-seat pane is untouched"
    );
    let notices = drain_notices(&mut rx);
    assert!(
        notices
            .iter()
            .any(|t| t == &format!("pane {p1} is not a portal seat")),
        "the notice names the pane: {notices:?}"
    );
    assert!(
        core.portals.get(&0).is_some(),
        "the real portal is untouched"
    );
}

#[test]
fn close_portal_refuses_a_seat_that_already_closed() {
    // AC5-ERR, the stale-seat half. The portals entry still NAMES a seat
    // whose pane an operator close already took; the close must refuse
    // with the same notice, not no-op through the stale entry.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    let seat = core.portals.get(&0).expect("portal 0 open").seat;
    core.close_by_operator(seat);
    assert!(
        !core.panes.contains_key(&seat),
        "fixture: the seat pane is gone"
    );
    while rx.try_recv().is_ok() {}

    core.command(client_id, Command::ClosePortal { seat });

    assert_eq!(
        core.panes.len(),
        1,
        "nothing closed: the seat was already gone"
    );
    let notices = drain_notices(&mut rx);
    assert!(
        notices
            .iter()
            .any(|t| t == &format!("pane {seat} is not a portal seat")),
        "the stale seat gets the refusal, not a silent no-op: {notices:?}"
    );
}

#[test]
fn close_portal_refuses_the_sessions_only_pane() {
    // AC6-EDGE. Closing the seat that is the session's last pane would
    // end the session; ClosePortal refuses and says so instead.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    // Retire the fixture's plain shell so the seat is the only pane left.
    core.close_pane(p1);
    let seat = core.portals.get(&0).expect("portal 0 open").seat;
    assert_eq!(core.panes.len(), 1, "fixture: the seat is the only pane");
    while rx.try_recv().is_ok() {}

    core.command(client_id, Command::ClosePortal { seat });

    assert!(core.panes.contains_key(&seat), "the last pane stays open");
    let notices = drain_notices(&mut rx);
    assert!(
        notices.iter().any(|t| t.contains("would end the session")),
        "the refusal says why: {notices:?}"
    );
}

#[test]
fn an_operator_close_of_the_last_portal_mints_no_stand_in() {
    // AC7-HP. prefix+x (`Command::ClosePane`, routed through
    // close_by_operator) on the last open portal closes the pane like any
    // pane's: no stand-in, and a tab left with no leaves closes.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    let seat = core.portals.get(&0).expect("portal 0 open").seat;
    let (a_tid, panes_before) = {
        let (sid, ti) = core.session.find_pane(seat).expect("seat in tree");
        (
            core.session.squad(sid).unwrap().tabs[ti].id,
            core.panes.len(),
        )
    };

    core.close_by_operator(seat);

    assert!(!core.panes.contains_key(&seat), "the pane closed");
    assert_eq!(core.panes.len(), panes_before - 1, "no stand-in was minted");
    assert!(
        !core
            .session
            .squad(1)
            .unwrap()
            .tabs
            .iter()
            .any(|t| t.id == a_tid),
        "a tab left with no leaves closes"
    );
}

#[test]
fn pane_list_names_the_portal_and_never_reads_a_seat_pristine() {
    // AC9-HP. The pane listing carries `portal` on a live seat and
    // omits it on a plain pane; a portal seat never reads
    // pristine_idle_shell, so no cleanup caller closes a kept seat.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, p1, _rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    let seat = core.portals.get(&0).expect("portal 0 open").seat;

    let infos = core.pane_infos_with_agents(&core.agents);
    let seat_info = infos
        .iter()
        .find(|i| i.pane_id == seat)
        .expect("seat listed");
    assert_eq!(
        seat_info.portal,
        Some(0),
        "the seat carries its portal index"
    );
    assert!(
        !seat_info.pristine_idle_shell,
        "a portal seat never reads pristine"
    );
    let plain = infos
        .iter()
        .find(|i| i.pane_id == p1)
        .expect("plain listed");
    assert_eq!(plain.portal, None, "a plain pane carries no portal");
    let json = serde_json::to_string(plain).expect("serializable");
    assert!(
        !json.contains("portal"),
        "a plain pane's JSON carries no portal key: {json}"
    );
}

/// Drain `rx` until a Notice naming `needle` arrives, collecting it; the
/// caller asserts on the text. Returns None at channel exhaustion so a
/// missing notice fails the caller's expect, never hangs.
fn collect_until_portal_closed(rx: &mut mpsc::Receiver<ServerMsg>, needle: &str) -> Option<String> {
    while let Ok(msg) = rx.try_recv() {
        if let ServerMsg::Notice { text } = msg {
            if text.contains(needle) {
                return Some(text);
            }
        }
    }
    None
}

// ---- (x-3cea) a portal seat follows the session its claude viewer shows --

/// Feed the seat pane an OSC title like a live claude viewer emits.
fn feed_seat_title(core: &mut Core, seat: u64, title: &str) {
    core.panes
        .get_mut(&seat)
        .unwrap()
        .vt
        .feed(format!("\x1b]0;{title}\x07").as_bytes());
}

#[test]
fn a_portal_follows_the_session_its_viewer_title_names() {
    // AC2-HP: a claude viewer switches from A to B inside its own TUI; the
    // 1s follow repoints the slot, the attach mapping and the pane name to
    // B, and a second tick is a no-op.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    let seat = core.portals.get(&0).expect("portal 0 open").seat;
    drain_notices(&mut rx);
    feed_seat_title(&mut core, seat, "◐ target-b");

    core.follow_portal_viewer_titles();

    assert_eq!(core.portals[&0].row_key, "deadbee2", "the slot claims B");
    assert_eq!(core.attached.get("deadbee2"), Some(&seat), "B maps it");
    assert!(
        !core.attached.contains_key("deadbee1"),
        "A holds no mapping"
    );
    assert_eq!(core.panes[&seat].name.as_deref(), Some("target-b"));
    let rows = core.agent_rows();
    let b = rows.iter().find(|r| r.name == "target-b").expect("row B");
    assert_eq!(b.pane_id, Some(seat), "B wears the seat");
    assert_eq!(b.portal, Some(0), "B carries the marker");
    let a = rows.iter().find(|r| r.name == "target-a").expect("row A");
    assert_eq!(a.pane_id, None, "A is paneless");
    assert!(
        drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("portal 0 now shows target-b")),
        "the follow says so"
    );

    core.follow_portal_viewer_titles();
    assert!(
        !drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("now shows")),
        "the second tick emits no notice"
    );
    core.reap_pane(seat);
}

#[test]
fn a_title_naming_no_single_row_drops_the_claim() {
    // AC2-ERR: a title naming two rows drops the claim instead of guessing;
    // the seat keeps the title text as its key with nothing wearing it, a
    // second tick is silent, and a later unambiguous title claims its row.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![
        bg_row("twin", "/tmp/seen", Some("deadbee1")),
        bg_row("twin", "/tmp/seen", Some("deadbee3")),
    ];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    let seat = core.portals.get(&0).expect("portal 0 open").seat;
    drain_notices(&mut rx);
    feed_seat_title(&mut core, seat, "✳ twin");

    core.follow_portal_viewer_titles();

    assert!(
        !core.attached.values().any(|p| *p == seat),
        "no row claims the seat"
    );
    assert_eq!(core.portals[&0].row_key, "twin", "the key is the title");
    assert_eq!(core.panes[&seat].name.as_deref(), Some("twin"));
    assert!(
        core.agent_rows().iter().all(|r| r.portal != Some(0)),
        "no row carries the portal marker"
    );
    assert!(
        drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("portal 0 now shows twin")),
        "the drop says so"
    );

    core.follow_portal_viewer_titles();
    assert!(
        !drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("now shows")),
        "the second tick is silent"
    );

    core.agents
        .push(bg_row("solo", "/tmp/seen", Some("deadbee4")));
    feed_seat_title(&mut core, seat, "solo");
    core.follow_portal_viewer_titles();
    assert_eq!(
        core.attached.get("deadbee4"),
        Some(&seat),
        "an unclaimed seat is not stuck"
    );
    core.reap_pane(seat);
}

#[test]
fn a_glyph_only_title_leaves_the_seat_alone() {
    // AC2-EDGE: a bare spinner frame names no session. The seat keeps its
    // row instead of unclaiming onto the glyph.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee1"))];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    let seat = core.portals.get(&0).expect("portal 0 open").seat;
    drain_notices(&mut rx);
    feed_seat_title(&mut core, seat, "◐");

    core.follow_portal_viewer_titles();

    assert_eq!(core.portals[&0].row_key, "deadbee1", "the row is kept");
    assert_eq!(core.attached.get("deadbee1"), Some(&seat));
    assert_eq!(core.panes[&seat].name.as_deref(), Some("target-a"));
    assert!(
        !drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("now shows")),
        "a glyph-only title is silent"
    );
    core.reap_pane(seat);
}

#[test]
fn a_portal_whose_title_names_its_own_row_is_left_alone() {
    // AC2-EDGE: the seated row named by `name`, by `harness_title`, an
    // unset title, a held stand-in seat and a non-claude viewer seat all
    // leave the slot, the mapping and the pane name untouched, with no
    // notice.
    fn undisturbed(core: &Core, seat: u64) {
        assert_eq!(core.portals[&0].row_key, "deadbee1");
        assert_eq!(core.attached.get("deadbee1"), Some(&seat));
        assert_eq!(core.panes[&seat].name.as_deref(), Some("target-a"));
    }
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, mut rx) = thread_core();
    let mut aliased = bg_row("target-a", "/tmp/seen", Some("deadbee1"));
    aliased.harness_title = Some("alias-a".to_string());
    core.agents = vec![aliased];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    let seat = core.portals.get(&0).expect("portal 0 open").seat;
    drain_notices(&mut rx);

    core.follow_portal_viewer_titles(); // no title at all
    undisturbed(&core, seat);
    feed_seat_title(&mut core, seat, "◐ target-a"); // the seated row by name
    core.follow_portal_viewer_titles();
    undisturbed(&core, seat);
    feed_seat_title(&mut core, seat, "alias-a"); // the seated row by harness title
    core.follow_portal_viewer_titles();
    undisturbed(&core, seat);
    core.panes.get_mut(&seat).unwrap().cmd = None; // a held stand-in
    feed_seat_title(&mut core, seat, "◐ target-b");
    core.follow_portal_viewer_titles();
    undisturbed(&core, seat);
    core.panes.get_mut(&seat).unwrap().cmd = Some("sh".into()); // not the attach program
    core.follow_portal_viewer_titles();
    undisturbed(&core, seat);
    assert!(
        !drain_notices(&mut rx)
            .iter()
            .any(|t| t.contains("now shows")),
        "an agreeing seat is silent"
    );
    core.reap_pane(seat);
}

#[test]
fn a_title_naming_a_row_another_portal_shows_does_not_steal_it() {
    // AC2-EDGE: portal 1 already shows B; portal 0's viewer switches to B.
    // Portal 1 keeps its row, its mapping and its pane name; portal 0 drops
    // its claim rather than minting a second viewer.
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, _p1, _rx) = thread_core();
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee1")),
        bg_row("target-b", "/tmp/seen", Some("deadbee2")),
    ];
    core.command(client_id, portal_reach_cmd("deadbee1", 0));
    core.command(client_id, portal_reach_cmd("deadbee2", 1));
    let seat0 = core.portals.get(&0).expect("portal 0 open").seat;
    let seat1 = core.portals.get(&1).expect("portal 1 open").seat;
    feed_seat_title(&mut core, seat0, "◐ target-b");

    core.follow_portal_viewer_titles();

    assert_eq!(core.portals[&1].row_key, "deadbee2", "portal 1 keeps B");
    assert_eq!(core.attached.get("deadbee2"), Some(&seat1));
    assert_eq!(core.panes[&seat1].name.as_deref(), Some("target-b"));
    assert_eq!(
        core.portals[&0].row_key, "target-b",
        "portal 0 drops its claim"
    );
    assert!(
        !core.attached.values().any(|p| *p == seat0),
        "portal 0's seat is unclaimed"
    );
    core.reap_pane(seat0);
    core.reap_pane(seat1);
}
