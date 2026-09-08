use super::*;

impl Core {
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
        if self.topology_dirty {
            self.flush_topology();
            return true;
        }
        let sids: Vec<u64> = self.session.squads.iter().map(|s| s.id).collect();
        let snapshots: Vec<_> = sids
            .into_iter()
            .filter_map(|sid| self.snapshot_squad(sid))
            .collect();
        let generations = match crate::squad_store::set_snapshots_if_generations(
            &self.store_generations,
            &snapshots,
        ) {
            Ok(Some(next)) => next,
            Ok(None) => {
                eprintln!(
                    "fno mux: squads.json changed after this session's last write; stale shutdown capture skipped"
                );
                return false;
            }
            Err(e) => {
                self.persist_degraded(&e);
                return false;
            }
        };
        self.store_generations.extend(generations);
        self.last_topology_flush = Some(Instant::now());
        true
    }
}
