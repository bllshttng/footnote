//! Pane grid convergence. The resize path is fire-once (`requested_size`
//! guards the re-issue) over a lossy middle: a keeper `Resize` frame dropped
//! on a full frame queue, or an older-build keeper that predates
//! `ResizeAck`, otherwise pins a pane's grid at its pre-change size forever.
//! The client blit clamps a too-tall frame to the pane's content rect, so
//! the child's bottom rows silently vanish (a vertically stacked pane loses
//! its input tail and status line). The core loop's 1s pass re-issues any
//! size diff until the grid agrees; on a healthy keeper the ack lands well
//! inside that window and the pass is a size-compare no-op.

use super::Core;

impl Core {
    /// The direct vt flip trades the ack's stale-tail ordering for
    /// convergence only in the already-broken case: one frame of output
    /// drawn at the old size, healed by the child's next repaint, beats an
    /// infinitely clipped pane.
    pub(super) fn reconcile_grid_sizes(&mut self) {
        for entry in self.panes.values_mut() {
            let (rows, cols) = entry.requested_size;
            if entry.vt.size() == (rows, cols) {
                continue;
            }
            if let Err(e) = entry.pty.resize(rows, cols, 0, 0) {
                eprintln!("fno mux: grid reconcile resize failed: {e}");
            }
            entry.vt.resize(rows, cols);
        }
    }
}

#[cfg(test)]
#[path = "grid_reconcile_tests.rs"]
mod tests;
