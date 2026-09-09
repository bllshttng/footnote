//! Generation-aware writes from the live mux model to the shared squad store.

use super::*;

impl Core {
    pub(super) fn persist_template_specs(&mut self, sid: u64) {
        let Some(sq) = self.session.squad(sid) else {
            return;
        };
        let Some(name) = sq.name.clone().filter(|name| !name.is_empty()) else {
            return;
        };
        let specs: Vec<crate::squad_store::StoredTabSpec> = sq
            .tabs
            .iter()
            .filter_map(|tab| {
                let tab_name = tab.name.clone().filter(|name| !name.is_empty())?;
                let spec = self.template_specs.get(&tab.id)?.clone();
                Some(crate::squad_store::StoredTabSpec { tab_name, spec })
            })
            .collect();
        let result = crate::squad_store::set_tab_specs_with_generations(
            Some(&self.store_generations),
            &name,
            &specs,
        );
        self.persist_result(result);
    }

    pub(super) fn persist_squad(&mut self, sid: u64) {
        let Some(snapshot) = self.snapshot_squad(sid) else {
            return;
        };
        self.persist_snapshots_if_current(std::slice::from_ref(&snapshot), "topology");
    }

    pub(super) fn persist_stored(
        &mut self,
        name: &str,
        key: &str,
        origins: &[String],
        members: &[crate::squad_store::StoredMember],
    ) {
        let result = crate::squad_store::upsert_with_generations(
            Some(&self.store_generations),
            name,
            key,
            origins,
            members,
        );
        self.persist_result(result);
    }

    pub(super) fn persist_remove(&mut self, name: &str, key: &str) {
        let result =
            crate::squad_store::remove_with_generations(Some(&self.store_generations), name, key);
        self.persist_result(result);
    }

    pub(super) fn persist_result(
        &mut self,
        result: std::io::Result<crate::squad_store::SnapshotBatch>,
    ) {
        match result {
            Ok(batch) => {
                self.store_generations.extend(batch.generations);
                if !batch.conflicts.is_empty() {
                    eprintln!(
                        "fno mux: stale partial-write baseline retained for {}",
                        batch.conflicts.join(", ")
                    );
                }
            }
            Err(error) => self.persist_degraded(&error),
        }
    }

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
            Err(error) => {
                self.persist_degraded(&error);
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
}
