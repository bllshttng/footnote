//! The first real attach and the startup restore it triggers. The restore
//! read (the healed squad store, the registry, the live-id rosters) runs on
//! the blocking pool while the attach waits; the attach and the restore then
//! land in ONE loop turn, so the client's first layout push already holds
//! both. Splitting them into a pre-restore and a post-restore push is what
//! broke the byte-exact reattach contract: a client acting in between saw
//! its own fresh squad persisted and then restored a second time.

use super::*;

/// The squad store as restore reads it: healed of duplicate rows first,
/// then loaded. The heal is a write; its failure degrades to a notice.
pub(super) struct RestoreStore {
    pub(super) collapse_error: Option<String>,
    pub(super) loaded: crate::squad_store::Loaded,
}

/// Everything the first restore reads before it can run.
pub(crate) struct RestoreRead {
    store: RestoreStore,
    /// The live claude attach members whose re-entry plans restore needs,
    /// as (attach_id, registry name) pairs.
    targets: Vec<(String, String)>,
}

pub(super) fn read_restore_store() -> RestoreStore {
    let collapse_error = crate::squad_store::collapse_duplicate_squads()
        .err()
        .map(|e| e.to_string());
    RestoreStore {
        collapse_error,
        loaded: crate::squad_store::load(),
    }
}

fn read_restore() -> RestoreRead {
    let store = read_restore_store();
    let targets = restore_plan_targets(&store.loaded);
    RestoreRead { store, targets }
}

/// The live claude attach members restore must plan for. Reads the same
/// sources the restore loop reads - the squad store, the registry file, the
/// live-id snapshot - so the batch and the loop agree on membership without
/// a third resolver. Worker members never appear: restore holds them idle
/// and their resume gesture (a focus) plans its own re-entry.
fn restore_plan_targets(store: &crate::squad_store::Loaded) -> Vec<(String, String)> {
    if store.squads.is_empty() {
        return Vec::new();
    }
    let live = live_attach_ids_snapshot();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rows = agents_view::registry_text()
        .ok()
        .and_then(|raw| agents_view::derive_rows(&raw, now));
    let Some(rows) = rows else {
        return Vec::new();
    };
    store
        .squads
        .iter()
        .flat_map(|s| s.members.iter())
        .filter(|m| !m.tombstone && m.worker.is_none() && live.contains(&m.attach_id))
        .filter_map(|m| {
            rows.iter()
                .find(|a| {
                    a.attach_id.as_deref() == Some(m.attach_id.as_str())
                        && a.harness.as_deref() == Some("claude")
                })
                .map(|a| (m.attach_id.clone(), a.name.clone()))
        })
        .collect()
}

/// A real attach waiting on the restore read.
struct HeldAttach {
    id: u64,
    rows: u16,
    cols: u16,
    cwd: String,
    squad_key: String,
    reliable_tx: mpsc::Sender<ServerMsg>,
    dirty: DirtyMap,
    notify: Arc<Notify>,
}

#[derive(Default)]
pub(super) struct RestoreHold {
    held: Vec<HeldAttach>,
    reading: bool,
    /// The landed read, consumed by the first held attach's restore.
    staged: Option<RestoreRead>,
}

impl Core {
    /// Route an attach: before the first restore, a real attach waits for
    /// the off-loop restore read and lands with it. Observers (0,0) never
    /// restore, so they attach at once; unit fixtures drive no loop, so
    /// under `cfg(test)` the read stays inline inside [`Core::attach`].
    #[allow(clippy::too_many_arguments)]
    pub(super) fn attach_or_hold(
        &mut self,
        id: u64,
        rows: u16,
        cols: u16,
        cwd: String,
        squad_key: String,
        reliable_tx: mpsc::Sender<ServerMsg>,
        dirty: DirtyMap,
        notify: Arc<Notify>,
    ) {
        let passive = rows == 0 && cols == 0;
        if cfg!(test) || self.restored || passive {
            self.attach(id, rows, cols, cwd, squad_key, reliable_tx, dirty, notify);
            return;
        }
        self.restore_hold.held.push(HeldAttach {
            id,
            rows,
            cols,
            cwd,
            squad_key,
            reliable_tx,
            dirty,
            notify,
        });
        if self.restore_hold.reading {
            return;
        }
        self.restore_hold.reading = true;
        let core_tx = self.self_tx.clone();
        tokio::task::spawn_blocking(move || {
            let read = read_restore();
            let _ = core_tx.blocking_send(CoreMsg::RestoreReadReady {
                read: Box::new(read),
            });
        });
    }

    /// The restore read landed: replay every held attach in arrival order.
    /// The first one runs the restore with this read; the rest find it done.
    /// With every holder gone, the read is dropped and the next real attach
    /// reads again.
    pub(super) fn restore_read_ready(&mut self, read: RestoreRead) {
        self.restore_hold.reading = false;
        let held = std::mem::take(&mut self.restore_hold.held);
        if held.is_empty() {
            return;
        }
        self.restore_hold.staged = Some(read);
        for h in held {
            self.attach(
                h.id,
                h.rows,
                h.cols,
                h.cwd,
                h.squad_key,
                h.reliable_tx,
                h.dirty,
                h.notify,
            );
        }
        self.restore_hold.staged = None;
    }

    /// A held client left before the read landed.
    pub(super) fn drop_held_attach(&mut self, id: u64) {
        self.restore_hold.held.retain(|h| h.id != id);
    }

    /// A held client resized before the read landed: attach at the new size.
    pub(super) fn resize_held_attach(&mut self, id: u64, rows: u16, cols: u16) {
        for h in self.restore_hold.held.iter_mut().filter(|h| h.id == id) {
            (h.rows, h.cols) = (rows, cols);
        }
    }

    /// The first real attach's restore, run BEFORE that attach pushes its
    /// layout: a member-less store restores in this turn, so one push carries
    /// the pre- and post-restore state together. Live claude members first
    /// resolve their re-entry plans off-loop; the restore then runs when the
    /// batch lands, as before.
    pub(super) fn restore_first_attach(
        &mut self,
        client_id: u64,
        rows: u16,
        cols: u16,
        home_sid: u64,
    ) {
        let RestoreRead { store, targets } =
            self.restore_hold.staged.take().unwrap_or_else(read_restore);
        if targets.is_empty() {
            self.restore_squads_from(rows, cols, home_sid, store);
            self.reconcile_external_lifecycle();
            return;
        }
        self.restore_pending = true;
        self.resolve_plan_batch(
            client_id,
            targets,
            BatchReplay::Restore {
                home_sid,
                rows,
                cols,
            },
        );
    }
}
