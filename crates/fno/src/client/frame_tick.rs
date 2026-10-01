//! The client loop's frame timer: when to wake an otherwise idle terminal
//! (nothing else redraws there) so a timed animation advances. Two sources:
//! the open yard overlay's spotlight, and the spin glyph of Working rows.
//! Re-armed each loop pass; with neither running there is no deadline and
//! no wakeup.

use super::*;

impl View {
    pub(super) fn frame_deadline(&self) -> Option<Instant> {
        let yard = self
            .yard
            .as_ref()
            .map(|yv| (yv.opened_at, YARD_FRAME_MS as u64));
        let spin = crate::lattice::spin_epoch()
            .filter(|_| {
                self.layout
                    .agents
                    .iter()
                    .any(|a| agent_lattice_state(a) == LatticeState::Working)
            })
            .map(|t0| (t0, crate::lattice::SPIN_FRAME_MS));
        yard.into_iter()
            .chain(spin)
            .map(|(t0, step)| next_boundary(t0, step))
            .min()
    }
}

/// The next multiple of `step` ms after `t0`, strictly in the future.
fn next_boundary(t0: Instant, step: u64) -> Instant {
    let elapsed = t0.elapsed().as_millis() as u64;
    t0 + Duration::from_millis((elapsed / step + 1) * step)
}
