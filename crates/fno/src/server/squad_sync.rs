//! Keep the server's member list equal to the store after an external write
//! to `squads.json` (the CLI prune, the `SquadReload` control verb).

use super::*;

pub(super) struct SquadReloadReceipt {
    pub(super) squads: usize,
    pub(super) members: usize,
    pub(super) emptied: usize,
}

impl Core {
    /// Re-project `squads.json` into `squad_members` for every squad this
    /// server holds, keyed by (name, key). A squad the store no longer
    /// carries reads as empty.
    pub(super) fn reload_members_from_store(&mut self) -> SquadReloadReceipt {
        let loaded = crate::squad_store::load();
        let identities: HashMap<(String, String), Vec<_>> = loaded
            .squads
            .into_iter()
            .map(|s| ((s.name, s.key), s.members))
            .collect();
        let sids: Vec<u64> = self.squad_members.keys().copied().collect();
        let mut receipt = SquadReloadReceipt {
            squads: 0,
            members: 0,
            emptied: 0,
        };
        for sid in sids {
            let Some((name, key)) = self.squad_identity(sid) else {
                continue;
            };
            let members = identities.get(&(name, key)).cloned().unwrap_or_default();
            receipt.squads += 1;
            if members.is_empty() {
                receipt.emptied += 1;
            } else {
                receipt.members += members.len();
            }
            self.squad_members.insert(sid, members);
        }
        receipt
    }

    /// The `CoreMsg::SquadReload` arm body: reload, repaint the sideline, and
    /// answer with the counts the server now holds. Running on the core loop
    /// is what makes the reload atomic against `persist_squad`.
    pub(super) fn handle_squad_reload(&mut self, reply: ControlReply) {
        let receipt = self.reload_members_from_store();
        self.push_layout(true);
        let _ = reply.send(ServerMsg::SquadReloaded {
            squads: receipt.squads,
            members: receipt.members,
            emptied: receipt.emptied,
        });
    }
}
