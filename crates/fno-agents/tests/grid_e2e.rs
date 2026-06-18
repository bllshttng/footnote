//! End-to-end integration test for `fno agents grid` (ab-3c063856, Wave 5.1).
//!
//! Walks the compositor through the operator-facing sequences the plan's
//! Acceptance Criteria pin, driving the public `pane` / `layout` /
//! `state` API the same way the runtime run loop will — but without
//! spinning up the daemon, which is a separate harness (see
//! `tests/daemon_e2e.rs`). The plan's notional "spawn 2 fake PTY agents,
//! assert tiled render" is split across this file (the FSM + render API
//! contract) and the per-module tests under `src/grid/`. The live
//! daemon-driven e2e is a Wave 5 follow-up tracked in the backlog.
//!
//! What this file actually covers:
//!
//! - **AC1-HP / AC1-UI:** two agents tile side by side; bytes from each
//!   watcher land in distinct pane snapshots without crosstalk.
//! - **AC1-FR:** `connecting…` is visible before the first byte arrives,
//!   distinct from a blank `watching`.
//! - **AC2-FR:** WS-drop while driving demotes to disconnected and
//!   never leaves the per-pane FSM in `Driving` with a dead socket;
//!   the global mode returns to WATCH on the next compositor step.
//! - **AC4-FR:** agent exit during DRIVE lands in `Exited` and the
//!   claim is gone with the agent.
//! - **AC5-FR:** a SIGWINCH storm coalesces into one debounced winsize
//!   push per pane.
//! - **Concurrency invariant:** keystrokes during the claim-acquire
//!   window do NOT reach the agent; only after `DriveClaimAcquired`
//!   does the per-pane FSM let bytes through.

use std::time::{Duration, Instant};

use fno_agents::grid::layout::{compute, LayoutError, TtySize};
use fno_agents::grid::pane::{Pane, WinsizeDebouncer};
use fno_agents::grid::state::{
    Compositor, CompositorAction, ConnAction, ConnEvent, ConnState, InputEvent, Mode,
};

/// Apply a [`ConnAction::FeedRenderer`] into a pane. The run loop will
/// do this in its select branch; we centralize it here so tests stay
/// readable.
fn apply_renderer_action(pane: &mut Pane, action: ConnAction) {
    if let ConnAction::FeedRenderer(bytes) = action {
        pane.feed(&bytes);
    }
}

/// AC1-HP / AC1-UI: two watcher streams render into two panes
/// independently. The "tiled" part is the layout (asserted separately);
/// here we prove the bytes route correctly to each pane without
/// crosstalk.
#[test]
fn two_agents_tile_and_render_independently() {
    // Layout: 24x80 terminal, two panes, side by side.
    let layout = compute(TtySize::new(24, 80), 2).expect("compute layout");
    assert_eq!(layout.tiles.len(), 2);
    assert_eq!(layout.tiles[0].cols, 40);
    assert_eq!(layout.tiles[1].col, 40);

    // Per-pane parsers sized to the tile rect.
    let mut panes: Vec<Pane> = layout
        .tiles
        .iter()
        .map(|t| Pane::new(t.rows, t.cols))
        .collect();
    let mut states: Vec<ConnState> = vec![ConnState::Connecting, ConnState::Connecting];

    // Both connections come up; bytes from each arrive. Watcher 0
    // emits "hi-A" and watcher 1 emits "hi-B".
    let a0 = states[0].step(ConnEvent::BytesReceived(b"hi-A".to_vec()));
    apply_renderer_action(&mut panes[0], a0);
    let a1 = states[1].step(ConnEvent::BytesReceived(b"hi-B".to_vec()));
    apply_renderer_action(&mut panes[1], a1);

    assert_eq!(states[0], ConnState::Watching);
    assert_eq!(states[1], ConnState::Watching);

    let snap0 = panes[0].snapshot();
    let snap1 = panes[1].snapshot();
    assert_eq!(snap0.cell(0, 0).unwrap().text, "h");
    assert_eq!(snap0.cell(0, 1).unwrap().text, "i");
    assert_eq!(snap0.cell(0, 2).unwrap().text, "-");
    assert_eq!(snap0.cell(0, 3).unwrap().text, "A");
    assert_eq!(snap1.cell(0, 3).unwrap().text, "B");
    // No crosstalk: pane 0 has no 'B', pane 1 has no 'A' at its leading
    // cell position.
    assert_ne!(snap0.cell(0, 3).unwrap().text, "B");
    assert_ne!(snap1.cell(0, 3).unwrap().text, "A");
}

/// AC1-FR: a watcher that has connected but not yet emitted bytes shows
/// `connecting…`, never a blank `watching`. The label change happens on
/// the first byte event.
#[test]
fn connecting_label_distinct_from_watching_until_first_byte() {
    let mut state = ConnState::Connecting;
    assert_eq!(state.label(), "connecting…");
    state.step(ConnEvent::WatcherConnected);
    // Still no bytes; label remains "connecting…".
    assert_eq!(state.label(), "connecting…");
    state.step(ConnEvent::BytesReceived(b"prompt> ".to_vec()));
    assert_eq!(state.label(), "watching");
}

/// AC2-FR: WS drop during DRIVE demotes to Disconnected. The
/// compositor's per-pane gate (`can_route_input`) immediately stops
/// routing keystrokes even before the next compositor step; the
/// global mode goes back to WATCH via a Release event from the input
/// handler.
#[test]
fn ws_drop_during_drive_severs_input_routing() {
    let mut comp = Compositor::new(2);
    let mut states = vec![ConnState::Watching, ConnState::Watching];

    // Focus pane 1 and promote.
    comp.step(InputEvent::FocusNext, &states);
    let act = comp.step(InputEvent::Promote, &states);
    assert_eq!(act, CompositorAction::AttemptPromote { pane_idx: 1 });
    assert_eq!(comp.mode(), Mode::Drive);

    // Run loop fires SendDriveRpc on pane 1; ack arrives.
    states[1].step(ConnEvent::PromoteRequested);
    states[1].step(ConnEvent::DriveClaimAcquired);
    assert_eq!(states[1], ConnState::Driving);

    // A keystroke now routes to pane 1 (claim is live).
    let act = comp.step(InputEvent::Keystroke(b"x".to_vec()), &states);
    assert_eq!(
        act,
        CompositorAction::ForwardKeystrokes {
            pane_idx: 1,
            bytes: b"x".to_vec()
        }
    );

    // WS drops underneath us. Per-pane FSM demotes; subsequent
    // keystrokes are dropped by the per-pane gate even though the
    // compositor is still in Mode::Drive.
    states[1].step(ConnEvent::WsClosed {
        reason: "broken pipe".to_string(),
    });
    assert!(matches!(states[1], ConnState::Disconnected { .. }));

    let act = comp.step(InputEvent::Keystroke(b"y".to_vec()), &states);
    assert_eq!(
        act,
        CompositorAction::NoOp,
        "keystroke on dead pane must be dropped"
    );

    // C1 fix: the run loop calls observe_pane_states after every
    // per-pane FSM transition so the global mode snaps back without
    // requiring the operator to press Esc.
    comp.observe_pane_states(&states);
    assert_eq!(comp.mode(), Mode::Watch);
}

/// AC4-FR: agent exit during DRIVE — the claim is gone with the agent.
/// The per-pane FSM lands in Exited; the compositor returns to WATCH on
/// the next release step.
#[test]
fn agent_exit_during_drive_releases_claim_with_agent() {
    let mut comp = Compositor::new(1);
    let mut states = vec![ConnState::Watching];

    let act = comp.step(InputEvent::Promote, &states);
    assert_eq!(act, CompositorAction::AttemptPromote { pane_idx: 0 });
    states[0].step(ConnEvent::PromoteRequested);
    states[0].step(ConnEvent::DriveClaimAcquired);
    assert_eq!(states[0], ConnState::Driving);

    // Agent exits. The claim is gone with it; no SendDetach needed.
    states[0].step(ConnEvent::AgentExited { code: 0 });
    assert_eq!(states[0], ConnState::Exited { code: 0 });

    // C1 fix: the run loop calls observe_pane_states after the per-
    // pane FSM transitions, which snaps the global mode back to WATCH
    // without requiring an explicit Release event.
    comp.observe_pane_states(&states);
    assert_eq!(comp.mode(), Mode::Watch);
}

/// AC5-FR: a SIGWINCH storm coalesces into a single debounced winsize
/// push per pane. The run loop drives this by reading SIGWINCH and
/// calling `pane.schedule_winsize_push` repeatedly; only the last
/// scheduled rect ever fires.
#[test]
fn sigwinch_storm_coalesces_to_single_push() {
    let mut pane = Pane::new(24, 80);
    let t0 = Instant::now();
    let delay = WinsizeDebouncer::DEFAULT_DELAY;

    // Storm: three rapid resize events within ~delay/2 of each other.
    pane.schedule_winsize_push(24, 80, t0);
    pane.schedule_winsize_push(30, 100, t0 + Duration::from_millis(50));
    pane.schedule_winsize_push(40, 120, t0 + Duration::from_millis(100));

    // Polling before the deadline (relative to LAST schedule = t0+100ms
    // + delay) returns None.
    assert!(pane
        .poll_pending_winsize(t0 + Duration::from_millis(150))
        .is_none());
    // After the deadline elapses, the LAST scheduled rect fires once.
    let fired = pane
        .poll_pending_winsize(t0 + Duration::from_millis(100) + delay + Duration::from_millis(10));
    assert_eq!(fired, Some((40, 120)));
    // Subsequent polls without new schedules return None.
    assert!(pane
        .poll_pending_winsize(t0 + Duration::from_secs(1))
        .is_none());
}

/// Concurrency invariant: keystrokes during the claim-acquire window
/// (compositor in DRIVE, per-pane FSM still in Connecting/Watching)
/// do NOT route to the agent. Only `DriveClaimAcquired` (the per-pane
/// FSM transitioning to Driving) flips `can_route_input` to true.
#[test]
fn claim_acquire_window_drops_keystrokes() {
    let mut comp = Compositor::new(1);
    let mut states = vec![ConnState::Watching];

    let act = comp.step(InputEvent::Promote, &states);
    assert_eq!(act, CompositorAction::AttemptPromote { pane_idx: 0 });
    assert_eq!(comp.mode(), Mode::Drive);

    // Per-pane FSM is mid-RPC.
    states[0].step(ConnEvent::PromoteRequested);
    assert_eq!(states[0], ConnState::PromotePending);

    // Keystroke during the window is dropped by the per-pane gate.
    let act = comp.step(InputEvent::Keystroke(b"early".to_vec()), &states);
    assert_eq!(act, CompositorAction::NoOp);

    // Claim acquired; subsequent keystroke routes.
    states[0].step(ConnEvent::DriveClaimAcquired);
    let act = comp.step(InputEvent::Keystroke(b"late".to_vec()), &states);
    assert_eq!(
        act,
        CompositorAction::ForwardKeystrokes {
            pane_idx: 0,
            bytes: b"late".to_vec()
        }
    );
}

/// Failure Modes / Boundaries: a too-small terminal is rejected with a
/// structured `LayoutError::TerminalTooSmall` — the run loop will
/// surface this to the operator and exit cleanly, never paint garbage.
#[test]
fn too_small_terminal_returns_structured_error() {
    let err = compute(TtySize::new(4, 10), 1).unwrap_err();
    assert!(matches!(
        err,
        LayoutError::TerminalTooSmall { rows: 4, cols: 10 }
    ));
    let msg = err.to_string();
    assert!(msg.contains("terminal too small"), "got: {msg}");
}

/// AC1-EDGE: 100 panes on a 24x80 terminal renders the max that fits at
/// minimum pane size (12 tiles) and reports the remaining 88 as overflow.
#[test]
fn overflow_indicator_for_too_many_panes() {
    let layout = compute(TtySize::new(24, 80), 100).expect("compute layout");
    assert_eq!(layout.tiles.len(), 12);
    assert_eq!(layout.overflow, 88);
}

/// Quit through the compositor cleanly demotes any held driver and
/// returns Quit. The run loop will use this to schedule a final
/// release-then-exit sequence and let the RawMode guard restore the
/// terminal on the way out.
#[test]
fn quit_in_watch_returns_quit_action() {
    let mut comp = Compositor::new(2);
    let states = vec![ConnState::Watching, ConnState::Watching];
    let act = comp.step(InputEvent::Quit, &states);
    assert_eq!(act, CompositorAction::Quit);
}
