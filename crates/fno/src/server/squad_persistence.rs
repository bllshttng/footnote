//! Generation-aware writes from the live mux model to the shared squad store.

use super::*;

impl Core {
    /// The live squad that holds stored identity `(name, key)`, skipping
    /// `except`. An unnamed squad with no key yet holds the key its origins
    /// derive, because its first persist adopts exactly that key.
    pub(super) fn live_holder_of(&self, name: &str, key: &str, except: Option<u64>) -> Option<u64> {
        if name.is_empty() && key.is_empty() {
            return None;
        }
        self.session
            .squads
            .iter()
            .filter(|sq| Some(sq.id) != except)
            .filter(|sq| match sq.name.as_deref().filter(|n| !n.is_empty()) {
                Some(live) => live == name,
                None if !name.is_empty() => false,
                None if sq.key.is_empty() => {
                    !sq.origins.is_empty() && crate::squad_store::origin_key(&sq.origins) == key
                }
                None => sq.key == key,
            })
            .map(|sq| sq.id)
            .min()
    }

    /// Clearing a squad's name is refused when no label derives (no origins)
    /// or another live squad already holds the identity its origins derive.
    pub(super) fn clear_name_refused(&self, sid: u64, origins: &[String]) -> bool {
        origins.is_empty()
            || (self.live_holder_of("", &crate::squad_store::origin_key(origins), Some(sid)))
                .is_some()
    }

    /// True when another live squad shares `sid`'s stored identity. Neither
    /// live member list is known to be complete, so the write is skipped and
    /// the fault is noticed once per identity per server life.
    pub(super) fn shared_identity_write_skipped(
        &mut self,
        sid: u64,
        name: &str,
        key: &str,
    ) -> bool {
        if self.live_holder_of(name, key, Some(sid)).is_none() {
            return false;
        }
        let id = if name.is_empty() { key } else { name };
        if self.shared_identity_notified.insert(id.to_string()) {
            let text = format!(
                "squad {id}: two live workspaces share one stored identity; member write skipped"
            );
            eprintln!("fno mux: {text}");
            self.notice_all(text);
        }
        true
    }

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
        if let Some(sid) = self.live_holder_of(name, key, None) {
            if self.shared_identity_write_skipped(sid, name, key) {
                return;
            }
        }
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
    ) -> bool {
        match result {
            Ok(batch) => {
                let current = batch.conflicts.is_empty();
                self.store_generations.extend(batch.generations);
                if !batch.conflicts.is_empty() {
                    eprintln!(
                        "fno mux: stale partial-write baseline retained for {}",
                        batch.conflicts.join(", ")
                    );
                }
                current
            }
            Err(error) => {
                self.persist_degraded(&error);
                false
            }
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
