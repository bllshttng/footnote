//! The (v70) re-seat tests: a live pane worker moves into a portal seat
//! without restarting. Its own module beside portal_tests, so the parent
//! stays shrink-only under the file-budget gate.
use super::portal_tests::{portal_reach_cmd, thread_core};
use super::*;

/// A pane-hosted worker row: mux ref plus attach id, the claude shape.
fn pane_worker_row(name: &str, attach: &str, pane: u64) -> RegistryAgent {
    let mut row = bg_row(name, "/tmp/seen", Some(attach));
    row.mux = Some(("test".into(), pane));
    row
}

/// One worker pane in its own tab of the fixture squad, bound to one row.
/// Returns the live client (and its channel) so a test can drive commands the
/// way a session does.
fn reseat_core(
    worker_name: &str,
    attach: &str,
) -> (Core, u64, u64, TabId, mpsc::Receiver<ServerMsg>) {
    let (mut core, client_id, _p1, rx) = thread_core();
    let w = core.spawn_pane(24, 40, "/tmp/seen").expect("worker pane");
    let worker_tab = core.session.mint_tab_id();
    core.session.squads[0].tabs.push(Tab {
        name: Some(worker_name.into()),
        id: worker_tab,
        root: Node::Leaf(w),
        focus: w,
    });
    core.agents = vec![pane_worker_row(worker_name, attach, w)];
    (core, client_id, w, worker_tab, rx)
}

/// The marker the node demands, all three at once: the harness child pid is
/// UNCHANGED across the move, exactly one `attached` entry points at the
/// attach id, and the portal index reports the pane as its seat. A test that
/// only checks a portal appeared proves nothing about the session surviving.
#[test]
fn reseat_moves_a_live_pane_worker_into_a_portal_seat_without_restarting_it() {
    let (mut core, _client_id, w, worker_tab, _rx) = reseat_core("pane-worker", "deadbee1");
    let child_before = core.panes[&w].pty.child_pid();

    let msg = core.reseat_pane_into_portal(w, None);

    let ServerMsg::Notice { text } = msg else {
        panic!("the reseat must land, got {msg:?}")
    };
    assert!(text.contains("deadbee1"), "{text}");
    assert_eq!(
        core.panes[&w].pty.child_pid(),
        child_before,
        "the harness child pid is unchanged across the move"
    );
    assert_eq!(
        core.attached.get("deadbee1"),
        Some(&w),
        "exactly one attached entry points at the attach id"
    );
    let portal = core
        .portals
        .values()
        .find(|p| p.seat == w)
        .expect("a portal seats the moved pane");
    assert_eq!(portal.row_key, "deadbee1");
    // The worker's old tab never empties: an only-leaf tab gets its
    // replacement shell, so the workspace keeps its anchor.
    let old_tab = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == worker_tab)
        .expect("the old tab survives the move");
    assert_ne!(
        tree::leaves(&old_tab.root),
        vec![w],
        "a replacement shell holds the worker's old tab"
    );
    assert!(
        core.detached_panes.get(&w).is_none(),
        "a re-seat is not a detach: no intermediate state reaches disk"
    );

    // Idempotence: the second call answers where the pane sits, moves nothing.
    let again = core.reseat_pane_into_portal(w, None);
    let ServerMsg::Notice { text } = again else {
        panic!("a re-reseat must answer, got {again:?}")
    };
    assert!(text.contains("already seated"), "{text}");
    assert_eq!(
        core.portals.values().filter(|p| p.seat == w).count(),
        1,
        "no second portal entry for one pane"
    );
}

/// A reach for a re-seated row (after the registry flip the caller makes)
/// FOCUSES the existing seat instead of minting a second viewer - the
/// duplicate-viewer invariant the epic exists to enforce.
#[test]
fn a_reach_for_a_reseated_row_focuses_the_seat_not_a_second_viewer() {
    set_attach_program(&["/bin/cat"]);
    let (mut core, client_id, w, _worker_tab, mut rx) = reseat_core("pane-worker", "deadbee1");
    // The worker pane runs an argv (a harness), not a shell: the seat must
    // read as a viewer, or the reach would repoint it as a stand-in.
    if let Some(entry) = core.panes.get_mut(&w) {
        entry.cmd = Some("claude".into());
    }
    assert!(matches!(
        core.reseat_pane_into_portal(w, None),
        ServerMsg::Notice { .. }
    ));
    // The caller's half: the registry now says the row is paneless.
    core.agents[0].mux = None;
    let panes_before = core.panes.len();
    while rx.try_recv().is_ok() {}

    core.command(client_id, portal_reach_cmd("deadbee1", 0));

    // POSITIVE control: the focus arm says so; a refused reach would not.
    let mut landed = false;
    while let Ok(ServerMsg::Notice { text }) = rx.try_recv() {
        if text.contains("already showing") {
            landed = true;
        }
    }
    assert!(
        landed,
        "the reach focused the seated pane (no refusal notice path)"
    );

    assert_eq!(
        core.panes.len(),
        panes_before,
        "the reach focused the existing seat; no second viewer was minted"
    );
    assert_eq!(
        core.attached.get("deadbee1"),
        Some(&w),
        "still exactly one attached entry for the attach id"
    );
    assert_eq!(
        core.portals.values().filter(|p| p.seat == w).count(),
        1,
        "the seat is unchanged by the reach"
    );
}

/// Every refusal mutates nothing: an unknown pane, a pane with no unique live
/// row, and a named slot whose seat is live each refuse with the tree,
/// portals, and member state exactly as they were.
#[test]
fn reseat_refusals_mutate_nothing() {
    let (mut core, client_id, w, _worker_tab, mut rx) = reseat_core("pane-worker", "deadbee1");
    let portals_before: Vec<(u8, u64, String)> = core
        .portals
        .iter()
        .map(|(idx, p)| (*idx, p.seat, p.row_key.clone()))
        .collect();
    let tabs_before = core.session.squads[0].tabs.len();

    // An unknown pane id.
    assert!(matches!(
        core.reseat_pane_into_portal(999_999, None),
        ServerMsg::Err { .. }
    ));

    // A live pane no unique row answers (the row's mux ref points elsewhere).
    core.agents = vec![bg_row("target-a", "/tmp/seen", Some("deadbee9"))];
    let orphan = core.reseat_pane_into_portal(w, None);
    let ServerMsg::Err { msg, .. } = orphan else {
        panic!("a pane with no unique row refuses, got {orphan:?}")
    };
    assert!(msg.contains("no unique live worker row"), "{msg}");

    // A named slot whose seat is live is never displaced.
    set_attach_program(&["/bin/cat"]);
    core.agents = vec![
        bg_row("target-a", "/tmp/seen", Some("deadbee9")),
        pane_worker_row("pane-worker", "deadbee1", w),
    ];
    core.command(client_id, portal_reach_cmd("deadbee9", 0));
    assert!(
        core.portals
            .get(&0)
            .is_some_and(|p| core.panes.contains_key(&p.seat)),
        "the setup reach opened a LIVE seat at portal 0"
    );
    // The refusal snapshot starts HERE, after the setup reach settled: what
    // follows must mutate nothing.
    let portals_before: Vec<(u8, u64, String)> = core
        .portals
        .iter()
        .map(|(idx, p)| (*idx, p.seat, p.row_key.clone()))
        .collect();
    let attached_before = core.attached.clone();
    let tabs_before = core.session.squads[0].tabs.len();
    let displaced = core.reseat_pane_into_portal(w, Some(0));
    let ServerMsg::Err { msg, .. } = displaced else {
        panic!("a live slot must refuse, got {displaced:?}")
    };
    assert!(msg.contains("portal 0 is live"), "{msg}");
    assert!(
        core.portals.values().all(|p| p.seat != w),
        "the refused reseat seated nothing"
    );
    assert_eq!(core.attached, attached_before);
    let portals_after: Vec<(u8, u64, String)> = core
        .portals
        .iter()
        .map(|(idx, p)| (*idx, p.seat, p.row_key.clone()))
        .collect();
    assert_eq!(
        portals_after, portals_before,
        "the refusals never touched the portal map"
    );
    assert_eq!(
        core.session.squads[0].tabs.len(),
        tabs_before,
        "the refusals never touched the tab tree"
    );
}
