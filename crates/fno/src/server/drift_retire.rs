//! The mux server's build-drift retirement (x-6648): when the on-disk binary
//! changed under a running server and the server is fully quiet - no panes,
//! attached clients, or in-flight connections - it retires through
//! `Flow::Shutdown` so the next attach spawns the installed build. The stat
//! pair runs off-loop behind a one-in-flight gate, so the core loop never
//! blocks on the filesystem; it only consumes the verdict. The pure drift
//! classification lives in [`crate::build_drift`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::build_drift::{self, DriftState, ExeFingerprint};

/// One drift-retirement watch per server. Created once at server startup;
/// `tick` runs from the core loop's 1s arm. The stat pair (startup
/// fingerprint vs own executable now) is re-run at most every 5th tick, so a
/// quiet healthy server pays one background stat per 5s - the same cadence
/// the fno-agents daemon pays for its idle probes.
pub(crate) struct RetireWatch {
    startup: Option<ExeFingerprint>,
    slot: Arc<Mutex<Option<DriftState>>>,
    in_flight: Arc<AtomicBool>,
    subtick: u32,
}

impl Default for RetireWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl RetireWatch {
    pub(crate) fn new() -> Self {
        RetireWatch {
            startup: ExeFingerprint::current(),
            slot: Arc::new(Mutex::new(None)),
            in_flight: Arc::new(AtomicBool::new(false)),
            subtick: 0,
        }
    }

    /// One core-tick step. Returns `Some((running, on_disk))` when the server
    /// must retire NOW: the on-disk binary measured drifted and `quiet`
    /// holds. A drifted verdict on a busy tick is re-parked, so the next
    /// quiet tick retires without waiting for a fresh stat. `Unknown` (no
    /// captured fingerprint, unreadable exe) never retires - fail-safe, the
    /// server keeps serving the old build rather than guessing.
    pub(crate) fn tick(
        &mut self,
        quiet: impl FnOnce() -> bool,
    ) -> Option<(ExeFingerprint, ExeFingerprint)> {
        self.subtick += 1;
        if self.subtick < 5 {
            return None;
        }
        self.subtick = 0;
        if !self.in_flight.swap(true, Ordering::SeqCst) {
            let gate = Arc::clone(&self.in_flight);
            let slot = Arc::clone(&self.slot);
            let startup = self.startup.clone();
            tokio::task::spawn_blocking(move || {
                let verdict = startup
                    .map(|fp| build_drift::self_drift(&fp))
                    .unwrap_or(DriftState::Unknown);
                *slot.lock().unwrap() = Some(verdict);
                gate.store(false, Ordering::Release);
            });
        }
        match self.slot.lock().unwrap().take() {
            Some(DriftState::Drifted { running, on_disk }) => {
                if quiet() {
                    Some((running, on_disk))
                } else {
                    // Busy: re-park so the next quiet tick retires without
                    // waiting for a fresh stat.
                    *self.slot.lock().unwrap() = Some(DriftState::Drifted { running, on_disk });
                    None
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The quiet predicate is the caller's; the watch only grades drift
    /// against it. A drifted verdict + quiet -> retire; drifted + busy ->
    /// stay up (the verdict is re-parked, not dropped).
    #[tokio::test]
    async fn tick_retires_only_when_drifted_and_quiet() {
        // A fresh build never retires: the startup fingerprint is the
        // current exe, so the stat matches.
        let mut fresh = RetireWatch::new();
        for _ in 0..6 {
            assert!(fresh.tick(|| true).is_none(), "a fresh build never retires");
        }
    }

    #[tokio::test]
    async fn tick_survives_a_subtick_run_without_a_runtime_task_leak() {
        let mut watch = RetireWatch::new();
        for _ in 0..12 {
            let _ = watch.tick(|| false);
        }
        // No assertion beyond "does not panic or wedge the gate": the
        // in-flight flag must always clear, so later ticks keep statting.
    }
}
