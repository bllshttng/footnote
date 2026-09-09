use super::*;

impl Core {
    pub(super) fn persist_snapshots_if_current(
        &mut self,
        snapshots: &[crate::squad_store::SquadSnapshot],
        context: &str,
    ) -> bool {
        let batch = match crate::squad_store::set_snapshots_if_generations(
            &self.store_generations,
            snapshots,
        ) {
            Ok(batch) => batch,
            Err(e) => {
                self.persist_degraded(&e);
                return false;
            }
        };
        self.store_generations.extend(batch.generations);
        if batch.conflicts.is_empty() {
            true
        } else {
            eprintln!(
                "fno mux: stale {context} snapshots skipped for {}",
                batch.conflicts.join(", ")
            );
            false
        }
    }

    /// Capture live topology at teardown, even when the dirty flag is clear.
    pub(super) fn capture_topology_now(&mut self) -> bool {
        if !self.restored {
            eprintln!("fno mux: shutdown before the first attach; no layout captured");
            return false;
        }
        if self.restore_pending {
            eprintln!("fno mux: shutdown while startup restore was pending; no layout captured");
            return false;
        }
        let sids: Vec<u64> = self.session.squads.iter().map(|s| s.id).collect();
        let snapshots: Vec<_> = sids
            .into_iter()
            .filter_map(|sid| self.snapshot_squad(sid))
            .collect();
        let captured = self.persist_snapshots_if_current(&snapshots, "shutdown");
        self.topology_dirty = false;
        self.last_topology_flush = Some(Instant::now());
        captured
    }
}
