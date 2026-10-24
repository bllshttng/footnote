//! Keyboard-scroll (v118) server-op tests: the shared-offset moves and
//! edge anchors ride the SAME offset the wheel drives. Split out of
//! `server_tests.rs` (file budget).

use super::*;

#[test]
fn pane_scroll_moves_shared_offset_and_edges_anchor() {
    // The scroll-mode wire ops move the SAME shared offset the wheel drives,
    // and the edge arm anchors exactly (g = oldest history line, G = live
    // bottom) - never past either clamp.
    let (mut core, _cid, p1, _p2, _rx) = seen_test_core();
    let lines = 60;
    core.panes
        .get_mut(&p1)
        .unwrap()
        .vt
        .feed("row\r\n".repeat(lines).as_bytes());
    assert_eq!(core.scroll_offset(p1), 0, "starts at the live bottom");

    // j/k-equivalent relative moves clamp both ways.
    core.pane_scroll(_cid, p1, 5);
    assert_eq!(core.scroll_offset(p1), 5, "5 lines up into history");
    core.pane_scroll(_cid, p1, -10);
    assert_eq!(core.scroll_offset(p1), 0, "10 lines down clamps at live");

    // A missing pane is a notice, never a panic.
    core.pane_scroll(_cid, 9999, 3);
    core.pane_scroll_edge(_cid, 9999, true);

    // g/G: top anchors at the SAME clamp a huge relative scroll reaches (the
    // history size is the grid's own count, so the test reads it rather than
    // recomputes it), and bottom snaps to live.
    core.pane_scroll(_cid, p1, 1_000_000);
    let top = core.scroll_offset(p1);
    assert!(top > 0, "a huge scroll clamps at the history top");
    core.pane_scroll_edge(_cid, p1, false);
    assert_eq!(core.scroll_offset(p1), 0, "bottom snaps to live");
    core.pane_scroll_edge(_cid, p1, true);
    assert_eq!(
        core.scroll_offset(p1),
        top,
        "top anchors at the oldest history line"
    );
}

// AC1-HP / AC2-HP: a 30-tick same-direction flood inside one window
// forwards at most WHEEL_GATE_BUDGET; ticks spaced past the window all pass.
#[test]
fn wheel_gate_bounds_flood_and_passes_notch_rate() {
    let mut g = HashMap::new();
    let t0 = Instant::now();
    let allowed = (0..30)
        .filter(|_| wheel_gate(&mut g, 7, MouseKind::WheelDown, t0, 12))
        .count();
    assert_eq!(
        allowed, WHEEL_GATE_BUDGET as usize,
        "a same-window flood forwards exactly the budget, drops the rest"
    );

    // Notch rate: one tick per window, none dropped.
    let mut g2 = HashMap::new();
    let passed = (0..30)
        .filter(|i| {
            wheel_gate(
                &mut g2,
                7,
                MouseKind::WheelDown,
                t0 + WHEEL_GATE_WINDOW * (*i as u32),
                12,
            )
        })
        .count();
    assert_eq!(passed, 30, "notch-rate input is forwarded 1:1");
}

// AC1-EDGE: exactly budget ticks pass, the (budget+1)th in the window drops.
#[test]
fn wheel_gate_exact_budget_boundary() {
    let mut g = HashMap::new();
    let t0 = Instant::now();
    for i in 0..WHEEL_GATE_BUDGET {
        assert!(
            wheel_gate(&mut g, 1, MouseKind::WheelUp, t0, 12),
            "tick {i} within budget forwards"
        );
    }
    assert!(
        !wheel_gate(&mut g, 1, MouseKind::WheelUp, t0, 12),
        "the tick past budget drops"
    );
}

// AC1-UI: a reversal mid-flood forwards immediately and resets the budget.
#[test]
fn wheel_gate_reversal_passes_immediately() {
    let mut g = HashMap::new();
    let t0 = Instant::now();
    // Exhaust the down budget so drops are occurring.
    for _ in 0..WHEEL_GATE_BUDGET {
        wheel_gate(&mut g, 1, MouseKind::WheelDown, t0, 12);
    }
    assert!(
        !wheel_gate(&mut g, 1, MouseKind::WheelDown, t0, 12),
        "same-direction is dropping"
    );
    assert!(
        wheel_gate(&mut g, 1, MouseKind::WheelUp, t0, 12),
        "the opposite tick forwards immediately (reversal is fresh intent)"
    );
    // Reversal reset the window: a fresh up budget is available.
    for _ in 1..WHEEL_GATE_BUDGET {
        assert!(wheel_gate(&mut g, 1, MouseKind::WheelUp, t0, 12));
    }
    assert!(
        !wheel_gate(&mut g, 1, MouseKind::WheelUp, t0, 12),
        "the reset up budget then exhausts"
    );
}

// AC1-FR: a tick at or after exactly window_start + window re-admits
// (no permanent mute), testing the boundary instant itself.
#[test]
fn wheel_gate_readmits_at_window_boundary() {
    let mut g = HashMap::new();
    let t0 = Instant::now();
    for _ in 0..WHEEL_GATE_BUDGET {
        wheel_gate(&mut g, 1, MouseKind::WheelDown, t0, 12);
    }
    assert!(
        !wheel_gate(&mut g, 1, MouseKind::WheelDown, t0, 12),
        "budget exhausted inside the window"
    );
    assert!(
        wheel_gate(&mut g, 1, MouseKind::WheelDown, t0 + WHEEL_GATE_WINDOW, 12),
        "the exact boundary instant counts as a fresh window and forwards"
    );
}

// AC2-ERR: a now behind window_start (virtualized clock) saturates instead
// of panicking, and treats the tick as inside the window.
#[test]
fn wheel_gate_clock_skew_saturates() {
    let mut g = HashMap::new();
    let t0 = Instant::now() + WHEEL_GATE_WINDOW * 10;
    assert!(wheel_gate(&mut g, 1, MouseKind::WheelDown, t0, 12));
    // A now BEFORE the stored window_start: saturating_duration_since is 0,
    // so the tick is inside the window and consumes budget (never panics).
    let earlier = t0 - WHEEL_GATE_WINDOW * 5;
    for _ in 1..WHEEL_GATE_BUDGET {
        assert!(wheel_gate(&mut g, 1, MouseKind::WheelDown, earlier, 12));
    }
    assert!(
        !wheel_gate(&mut g, 1, MouseKind::WheelDown, earlier, 12),
        "skewed-early ticks stay inside the window and hit the budget"
    );
}

// Per-pane independence: one pane's flood never spends another's budget.
#[test]
fn wheel_gate_per_pane() {
    let mut g = HashMap::new();
    let t0 = Instant::now();
    for _ in 0..WHEEL_GATE_BUDGET {
        wheel_gate(&mut g, 1, MouseKind::WheelDown, t0, 12);
    }
    assert!(!wheel_gate(&mut g, 1, MouseKind::WheelDown, t0, 12));
    assert!(
        wheel_gate(&mut g, 2, MouseKind::WheelDown, t0, 12),
        "a different pane draws from its own budget"
    );
}

// AC2-EDGE: reaping a pane drops its gate entry (touch_last_emit pattern).
#[test]
fn reap_pane_clears_wheel_gate() {
    let mut core = empty_core();
    core.wheel_gate.insert(
        42,
        WheelGateState {
            window_start: Instant::now(),
            count: 3,
            dir: MouseKind::WheelDown,
        },
    );
    core.reap_pane(42);
    assert!(
        !core.wheel_gate.contains_key(&42),
        "the closed pane's gate entry is removed"
    );
}
