//! End-to-end integration test for `fno agents grid` (ab-3c063856, Wave 5.1).
//!
//! Walks the compositor through the operator-facing sequences the plan's
//! Acceptance Criteria pin, driving the public `pane` / `layout` / `state`
//! API the same way the runtime run loop will — but without spinning up the
//! daemon, which is a separate harness.
//!
//! Owned-PTY model (x-1356): footnote owns every panel's PTY, so there is no
//! watch/drive split and no exclusive-driver claim. A pane is `Connecting`
//! until its first bytes flip it `Live`; focus IS the keystroke target, with
//! no promote/release step. What this file covers:
//!
//! - **AC1-HP / AC1-UI:** two agents tile side by side; bytes from each land
//!   in distinct pane snapshots without crosstalk.
//! - **AC1-FR:** `connecting…` is visible before the first byte arrives.
//! - **AC2-FR:** a stream drop on the focused pane lands in `Disconnected`
//!   and the per-pane gate (`can_route_input`) stops routing keystrokes.
//! - **AC4-FR:** agent exit lands in `Exited`.
//! - **AC5-FR:** a SIGWINCH storm coalesces into one debounced winsize push.

use std::time::{Duration, Instant};

use fno_agents::grid::layout::{compute, LayoutError, TtySize};
use fno_agents::grid::pane::{Pane, WinsizeDebouncer};
use fno_agents::grid::state::{
    Compositor, CompositorAction, ConnAction, ConnEvent, ConnState, InputEvent, Mode,
};

/// Apply a [`ConnAction::FeedRenderer`] into a pane. The run loop will do this
/// in its select branch; we centralize it here so tests stay readable.
fn apply_renderer_action(pane: &mut Pane, action: ConnAction) {
    if let ConnAction::FeedRenderer(bytes) = action {
        pane.feed(&bytes);
    }
}

/// AC1-HP / AC1-UI: two streams render into two panes independently, no
/// crosstalk.
#[test]
fn two_agents_tile_and_render_independently() {
    let layout = compute(TtySize::new(24, 80), 2).expect("compute layout");
    assert_eq!(layout.tiles.len(), 2);
    assert_eq!(layout.tiles[0].cols, 40);
    assert_eq!(layout.tiles[1].col, 40);

    let mut panes: Vec<Pane> = layout
        .tiles
        .iter()
        .map(|t| Pane::new(t.rows, t.cols))
        .collect();
    let mut states: Vec<ConnState> = vec![ConnState::Connecting, ConnState::Connecting];

    let a0 = states[0].step(ConnEvent::BytesReceived(b"hi-A".to_vec()));
    apply_renderer_action(&mut panes[0], a0);
    let a1 = states[1].step(ConnEvent::BytesReceived(b"hi-B".to_vec()));
    apply_renderer_action(&mut panes[1], a1);

    assert_eq!(states[0], ConnState::Live);
    assert_eq!(states[1], ConnState::Live);

    let snap0 = panes[0].snapshot();
    let snap1 = panes[1].snapshot();
    assert_eq!(snap0.cell(0, 0).unwrap().text, "h");
    assert_eq!(snap0.cell(0, 3).unwrap().text, "A");
    assert_eq!(snap1.cell(0, 3).unwrap().text, "B");
    // No crosstalk.
    assert_ne!(snap0.cell(0, 3).unwrap().text, "B");
    assert_ne!(snap1.cell(0, 3).unwrap().text, "A");
}

/// AC1-FR: a pane that has connected but not yet emitted bytes shows
/// `connecting…`; the first byte flips it `live`.
#[test]
fn connecting_label_distinct_from_live_until_first_byte() {
    let mut state = ConnState::Connecting;
    assert_eq!(state.label(), "connecting…");
    state.step(ConnEvent::WatcherConnected);
    // Still no bytes; label remains "connecting…".
    assert_eq!(state.label(), "connecting…");
    state.step(ConnEvent::BytesReceived(b"prompt> ".to_vec()));
    assert_eq!(state.label(), "live");
}

/// AC2-FR: a stream drop on the focused pane lands in Disconnected, and the
/// per-pane gate (`can_route_input`) immediately stops routing keystrokes -
/// no claim to release, no mode to snap back (owned-PTY).
#[test]
fn ws_drop_severs_input_routing() {
    let mut comp = Compositor::new(2);
    let mut states = vec![ConnState::Live, ConnState::Live];

    // Focus pane 1 and type into it - focus IS drive.
    comp.step(InputEvent::FocusNext, &states);
    let act = comp.step(InputEvent::Keystroke(b"x".to_vec()), &states);
    assert_eq!(
        act,
        CompositorAction::ForwardKeystrokes {
            pane_idx: 1,
            bytes: b"x".to_vec()
        }
    );

    // The stream drops underneath us; the per-pane gate now eats keystrokes.
    states[1].step(ConnEvent::WsClosed {
        reason: "broken pipe".to_string(),
    });
    assert!(matches!(states[1], ConnState::Disconnected { .. }));

    let act = comp.step(InputEvent::Keystroke(b"y".to_vec()), &states);
    assert_eq!(
        act,
        CompositorAction::NoOp,
        "keystroke on a dead pane must be dropped"
    );
    // Mode is unaffected - the resting mode is always Drive.
    assert_eq!(comp.mode(), Mode::Drive);
}

/// AC4-FR: agent exit lands in Exited; nothing to detach (owned-PTY).
#[test]
fn agent_exit_lands_in_exited() {
    let mut states = vec![ConnState::Live];
    states[0].step(ConnEvent::AgentExited { code: 0 });
    assert_eq!(states[0], ConnState::Exited { code: 0 });
    // A keystroke to the focused (now dead) pane is dropped.
    let mut comp = Compositor::new(1);
    let act = comp.step(InputEvent::Keystroke(b"z".to_vec()), &states);
    assert_eq!(act, CompositorAction::NoOp);
}

/// AC5-FR: a SIGWINCH storm coalesces into a single debounced winsize push.
#[test]
fn sigwinch_storm_coalesces_to_single_push() {
    let mut pane = Pane::new(24, 80);
    let t0 = Instant::now();
    let delay = WinsizeDebouncer::DEFAULT_DELAY;

    pane.schedule_winsize_push(24, 80, t0);
    pane.schedule_winsize_push(30, 100, t0 + Duration::from_millis(50));
    pane.schedule_winsize_push(40, 120, t0 + Duration::from_millis(100));

    assert!(pane
        .poll_pending_winsize(t0 + Duration::from_millis(150))
        .is_none());
    let fired = pane
        .poll_pending_winsize(t0 + Duration::from_millis(100) + delay + Duration::from_millis(10));
    assert_eq!(fired, Some((40, 120)));
    assert!(pane
        .poll_pending_winsize(t0 + Duration::from_secs(1))
        .is_none());
}

/// Owned-PTY: a connected (still `Connecting`) pane already routes input -
/// the moment you type, the bytes reach its PTY (no claim-acquire window).
#[test]
fn connecting_pane_routes_input() {
    let mut comp = Compositor::new(1);
    let states = vec![ConnState::Connecting];
    let act = comp.step(InputEvent::Keystroke(b"hi".to_vec()), &states);
    assert_eq!(
        act,
        CompositorAction::ForwardKeystrokes {
            pane_idx: 0,
            bytes: b"hi".to_vec()
        }
    );
}

/// Failure Modes / Boundaries: a too-small terminal is rejected with a
/// structured `LayoutError::TerminalTooSmall`.
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

/// AC1-EDGE: 100 panes on a 24x80 terminal renders the max that fits (12) and
/// reports the remaining 88 as overflow.
#[test]
fn overflow_indicator_for_too_many_panes() {
    let layout = compute(TtySize::new(24, 80), 100).expect("compute layout");
    assert_eq!(layout.tiles.len(), 12);
    assert_eq!(layout.overflow, 88);
}

/// Quit through the compositor returns Quit so the run loop can schedule a
/// clean exit.
#[test]
fn quit_returns_quit_action() {
    let mut comp = Compositor::new(2);
    let states = vec![ConnState::Live, ConnState::Live];
    let act = comp.step(InputEvent::Quit, &states);
    assert_eq!(act, CompositorAction::Quit);
}
