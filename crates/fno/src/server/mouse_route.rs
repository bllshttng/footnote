//! The mouse-routing half of pane input: what one mouse event MEANS mux-side,
//! decided purely from the pane's modes + the gesture, so the routing is unit
//! testable without a PTY. `Core::mouse` (parent) executes the chosen action;
//! the click-pair claims it consults first live in `open_link`. Moved here
//! beside the claim helpers it feeds (file-budget): the code the alt-click
//! link gesture touched travels with it.

use crate::proto::{MouseButton, MouseEvent, MouseKind};
use crate::vt::Modes;

/// Lines a single wheel notch scrolls a mux-interpreted pane. ONE, because the
/// host terminal already did the accumulation and a notch here IS one cell of
/// finger travel - multiplying re-amplifies input that arrives pre-normalized.
///
/// Every precision-scroll host converts trackpad pixel deltas into whole cells
/// by accumulating a float and keeping the sub-cell remainder, then emits one
/// wheel notch per cell crossed: alacritty `input/mod.rs::scroll_terminal`
/// (`accumulated_scroll.y %= height`, and it forces its user multiplier to 1
/// while an app owns the mouse - which is us), ghostty `Surface.zig`
/// (`pending_scroll_y`, `mouse-scroll-multiplier.precision` default 1).
///
/// Three is the right step for a DISCRETE wheel detent, where the host emits
/// one notch per physical click and a 1:1 step feels sluggish. It is 3x too
/// fast for a trackpad, and SGR reports both as button 64/65 with nothing to
/// tell them apart - so this favors the trackpad, the common case. A wheel-mouse
/// operator wanting a coarser step sets `config.mux.wheel_lines`
/// ([`crate::digest_overlay::wheel_lines`]); this const stays the routing
/// step the configured multiplier applies at [`Core::scroll_delta`].
pub(super) const MOUSE_WHEEL_LINES: i32 = 1;

/// What the server does with a mouse event, decided purely from the pane's
/// modes + the gesture so the routing (the brief's crux, Locked 2) is unit
/// testable without a PTY. The `mouse` method executes the chosen action.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MouseAction {
    /// The app owns its mouse: SGR-encode and write to its PTY (AC3-HP).
    Passthrough,
    /// Mux-interpret a wheel notch: scroll the pane by N lines (US1).
    Scroll(i32),
    /// Left press begins a selection anchor (US2).
    SelectStart,
    /// Left drag extends the selection (US2).
    SelectUpdate,
    /// Left release finalizes: auto-copy a real selection, else clear (US2).
    SelectRelease,
    /// No mux meaning (middle/right buttons in v1).
    Ignore,
}

/// Route by the pane's mode (brief Locked 2). A pane that negotiated SGR mouse
/// reporting gets passthrough, but only for the event kinds its mode actually
/// asked for: a click-only app (`?1000`) must not receive drag reports it never
/// requested, so a drag over such a pane is ignored (not mux-interpreted -
/// mux-interpreting a mouse app's pane would fight its own click handling). Only
/// SGR is honored - a mouse app that never negotiated SGR falls through to
/// interpretation rather than receiving garbage (Domain: legacy X10 truncates at
/// column 223). Known limit: the client now enables `?1003` too, but consumes
/// bare-motion (`MouseKind::Move`) LOCALLY for hover (focus-follows-mouse +
/// sideline highlight) and never forwards it, so a pane app's own
/// `?1003` all-motion request still never sees hover motion - hover is a mux
/// affordance, not passthrough.
pub(super) fn route_mouse(modes: Modes, kind: MouseKind) -> MouseAction {
    let reports_mouse = modes.mouse_click || modes.mouse_drag || modes.mouse_motion;
    if reports_mouse && modes.sgr_mouse {
        let wants = match kind {
            // Wheel, press, and release are reported by every mouse mode.
            MouseKind::WheelUp
            | MouseKind::WheelDown
            | MouseKind::Press(_)
            | MouseKind::Release(_) => true,
            // Motion-while-held is only wanted by ?1002 (drag) / ?1003 (motion).
            MouseKind::Drag(_) => modes.mouse_drag || modes.mouse_motion,
            // Bare motion is client-local hover (never forwarded); the arm is
            // dead but honest - only a ?1003 app would want it.
            MouseKind::Move => modes.mouse_motion,
        };
        return if wants {
            MouseAction::Passthrough
        } else {
            MouseAction::Ignore
        };
    }
    match kind {
        MouseKind::WheelUp => MouseAction::Scroll(MOUSE_WHEEL_LINES),
        MouseKind::WheelDown => MouseAction::Scroll(-MOUSE_WHEEL_LINES),
        MouseKind::Press(MouseButton::Left) => MouseAction::SelectStart,
        MouseKind::Drag(MouseButton::Left) => MouseAction::SelectUpdate,
        MouseKind::Release(MouseButton::Left) => MouseAction::SelectRelease,
        _ => MouseAction::Ignore,
    }
}

/// Bound a folded wheel burst's net offset move to `cap` lines (one viewport)
/// so a high-event-rate trackpad flick lands a screen at a time instead of
/// teleporting hundreds of lines. `before`/`after` are the offsets around an
/// in-order fold, so the per-tick history clamp already happened - this only
/// caps the aggregate. A single mouse-wheel notch is one tick far under the
/// cap and passes through unchanged; the cap self-selects the trackpad burst.
pub(super) fn bounded_scroll_target(before: i32, after: i32, cap: i32) -> i32 {
    before + (after - before).clamp(-cap, cap)
}

/// SGR-encode one mouse event for an app that negotiated mouse reporting
/// (`ESC [ < b; x; y {M|m}`, brief Locked 12 / Domain: SGR 1006 only). `b` is
/// the button code plus the drag-motion bit and the event's alt modifier (8),
/// so an unclaimed alt click reaches the app byte-identical to what the host
/// sent; coordinates are 1-based. Press and motion terminate with `M`, release
/// with `m`.
pub(super) fn sgr_mouse_bytes(event: &MouseEvent) -> Vec<u8> {
    let (button, released) = match event.kind {
        MouseKind::Press(b) => (button_code(b), false),
        MouseKind::Release(b) => (button_code(b), true),
        // Motion bit (32) rides on top of the held button (SGR drag report).
        MouseKind::Drag(b) => (button_code(b) + 32, false),
        // Bare motion: no-button code (3) plus the motion bit (32). Client-local
        // hover never forwards Move, so this arm is dead but wire-correct.
        MouseKind::Move => (3 + 32, false),
        MouseKind::WheelUp => (64, false),
        MouseKind::WheelDown => (65, false),
    };
    let button = if event.alt { button + 8 } else { button };
    let x = event.col as u32 + 1;
    let y = event.row as u32 + 1;
    let terminator = if released { 'm' } else { 'M' };
    format!("\x1b[<{button};{x};{y}{terminator}").into_bytes()
}

fn button_code(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}
