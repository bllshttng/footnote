//! The pane-scroll half of pane input: the wheel-passthrough rate gate, the
//! interpreted-wheel helpers the core drain folds through, and the v118
//! keyboard-scroll wire ops. Split out of `server.rs` (file budget); the
//! mouse-routing DECISION lives in [`super::mouse_route`], this is the
//! execution.

use super::*;

/// Wheel-passthrough rate gate: forward at most `budget` wheel ticks per
/// [`WHEEL_GATE_WINDOW`] to a mouse-owning pane's PTY. A physical notch
/// stream (a few ticks/s) never gates; only a trackpad flood (hundreds/s)
/// clips. The budget is `config.mux.wheel_passthrough_rate` (default 3:
/// latched at Core build) - a trackpad flick on a Claude Code / Codex pane
/// reads "too big, too fast" at the old 12, so the default dropped with the
/// knob's arrival. ponytail: too low and fast-redrawing apps (vim) feel
/// sluggish - raise the configured rate if a deliberate notch scroll drops.
pub(super) const WHEEL_GATE_WINDOW: Duration = Duration::from_millis(100);

/// The legacy fixed budget the gate shipped with (12 per 100ms window),
/// kept as the explicit budget the gate tests pin. The live default is the
/// configured `mux.wheel_passthrough_rate` (default 3), latched at Core
/// build and passed to [`wheel_gate`] per event.
#[cfg(test)]
pub(super) const WHEEL_GATE_BUDGET: u32 = 12;

/// Per-pane wheel-gate state: the current window's start, ticks forwarded in
/// it, and the direction of the last forwarded tick (a reversal is fresh
/// intent and resets the window - brief Locked 3). Fields are crate-tree
/// visible: the reap path drops the entry whole, and one test builds one.
#[derive(Debug)]
pub(super) struct WheelGateState {
    pub(super) window_start: Instant,
    pub(super) count: u32,
    pub(super) dir: MouseKind,
}

/// Whether a wheel `dir` tick for `pane` should be forwarded now, recording
/// `now`. Pure over the state map (injected `Instant`, `touch_coalesce`
/// pattern) so it is PTY-free unit-testable. Rules: a direction reversal or an
/// expired window (at or after the boundary instant - no permanent mute,
/// AC1-FR) resets to a fresh budget and allows; under budget allows; else
/// drops. Drops only - forwarded ticks keep arrival order (brief Locked 5).
pub(super) fn wheel_gate(
    gate: &mut HashMap<u64, WheelGateState>,
    pane: u64,
    dir: MouseKind,
    now: Instant,
    budget: u32,
) -> bool {
    match gate.entry(pane) {
        std::collections::hash_map::Entry::Occupied(mut e) => {
            let st = e.get_mut();
            // saturating: a `now` behind window_start (virtualized clock skew)
            // treats the tick as inside the window instead of panicking.
            if st.dir != dir || now.saturating_duration_since(st.window_start) >= WHEEL_GATE_WINDOW
            {
                *st = WheelGateState {
                    window_start: now,
                    count: 1,
                    dir,
                };
                true
            } else if st.count < budget {
                st.count += 1;
                true
            } else {
                false
            }
        }
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(WheelGateState {
                window_start: now,
                count: 1,
                dir,
            });
            true
        }
    }
}

impl Core {
    /// The line delta an interpreted wheel-scroll applies, or `None` when the
    /// event isn't a mux-interpreted scroll (a mouse-app passthrough, a select, or
    /// an ignore). The core drain folds a contiguous run of wheel ticks on one
    /// pane - including a direction reversal queued behind in-flight opposite
    /// ticks - into a single broadcast, so a fast trackpad flick settles in one
    /// frame instead of rubber-banding through every intermediate offset.
    pub(super) fn scroll_delta(&self, pane: u64, event: &MouseEvent) -> Option<i32> {
        let modes = self.panes.get(&pane)?.vt.modes();
        match route_mouse(modes, event.kind) {
            // The routing stays 1 line per notch; the configured step applies
            // here, at the one place the interpreted delta is consumed
            // (`config.mux.wheel_lines`, latched at Core build).
            MouseAction::Scroll(delta) => Some(delta.saturating_mul(self.wheel_lines)),
            _ => None,
        }
    }

    /// Apply one wheel tick to a pane WITHOUT broadcasting, returning the
    /// resulting scroll offset. The drain applies a queued run tick-by-tick so
    /// `vt.scroll`'s per-tick clamp at the history top / live bottom is preserved
    /// - the algebraic net would wrongly cancel a tick that clamped, losing a
    /// reversal at a boundary - then broadcasts once. `0` if the pane is gone.
    pub(super) fn scroll_tick(&mut self, pane: u64, delta: i32) -> usize {
        self.panes.get_mut(&pane).map_or(0, |e| e.vt.scroll(delta))
    }

    /// The pane's current scroll offset (0 = live bottom), or 0 if it's gone.
    pub(super) fn scroll_offset(&self, pane: u64) -> usize {
        self.panes.get(&pane).map_or(0, |e| e.vt.display_offset())
    }

    /// The pane's viewport height in rows (0 if gone), the per-fold scroll cap.
    pub(super) fn pane_rows(&self, pane: u64) -> u16 {
        self.panes.get(&pane).map_or(0, |e| e.vt.size().0)
    }

    /// Apply a single interpreted wheel tick and push one frame (the
    /// per-message path; the drain coalesces a run through `scroll_tick`).
    pub(super) fn apply_scroll(&mut self, pane: u64, delta: i32) {
        self.scroll_tick(pane, delta);
        self.broadcast_pane(pane);
    }

    /// The largest single keyboard-scroll delta accepted from the wire. The
    /// scrollback ceiling is 100k lines, so anything past ten of those is
    /// meaningless; the bound keeps alacritty's `display_offset as i32 +
    /// count` add overflow-free however the offset sits.
    const MAX_SCROLL_DELTA: i32 = 1_000_000;

    /// Apply one keyboard-scroll op to `pane` (v118). Both move the SHARED
    /// per-pane offset the wheel drives and broadcast, so every co-viewer
    /// tracks (tmux copy-mode precedent). The relative move clamps at the
    /// history top / live bottom inside `vt.scroll` (and at
    /// [`MAX_SCROLL_DELTA`] here, before the delta reaches that add); the
    /// edge arm anchors exactly. A missing pane (focus raced a close) is a
    /// visible notice, never a silent drop.
    pub(super) fn pane_scroll(&mut self, client_id: u64, pane: u64, delta: i32) {
        let delta = delta.clamp(-Self::MAX_SCROLL_DELTA, Self::MAX_SCROLL_DELTA);
        match self.panes.get_mut(&pane) {
            Some(e) => {
                e.vt.scroll(delta);
                self.broadcast_pane(pane);
            }
            None => self.notice(client_id, "pane not found"),
        }
    }

    pub(super) fn pane_scroll_edge(&mut self, client_id: u64, pane: u64, top: bool) {
        match self.panes.get_mut(&pane) {
            Some(e) => {
                if top {
                    e.vt.scroll_to_top();
                } else {
                    e.vt.scroll_to_bottom();
                }
                self.broadcast_pane(pane);
            }
            None => self.notice(client_id, "pane not found"),
        }
    }
}
