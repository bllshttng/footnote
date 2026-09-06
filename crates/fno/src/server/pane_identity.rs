//! Per-pane worker identity: the `fno_id` join and the orphan verdict (v71).

use super::*;

impl Core {
    /// The `fno_id` (durable session id) of the registry row hosting `pid` in
    /// this session, if any. The forward half of the identity join (Locked
    /// Decision 6); `PaneWhere` is the reverse.
    pub(super) fn fno_id_for_pane_with_agents(
        &self,
        pid: u64,
        agents: &[RegistryAgent],
    ) -> Option<String> {
        let mut ids = std::collections::BTreeSet::new();
        for a in agents {
            if let Some((sess, pane)) = &a.mux {
                if sess == &self.session_name && *pane == pid {
                    if let Some(identity) = a.effective_identity() {
                        ids.insert(identity.to_string());
                    }
                }
            }
        }
        // (x-b029) The resume birthright. A pane the daemon itself re-homed
        // (workspace restore, `pane run --worker`) is recorded in
        // `worker_session_pane` with its (harness, session id) at spawn - the
        // same id the resume argv carries. The registry FILE's row can still
        // point at the pre-restart pane, so the join above misses and the
        // pane read `-` while doing real work. This map is fno's own record,
        // not an argv heuristic: the id was stamped at birth by the code that
        // built the resume command.
        for ((_, session_id), pane) in &self.worker_session_pane {
            if *pane == pid {
                ids.insert(session_id.clone());
            }
        }
        if ids.is_empty() {
            ids.extend(crate::thread_viewer::identity_for_pane(
                &self.portals,
                pid,
                agents,
            ));
        }
        (ids.len() == 1).then(|| ids.into_iter().next()).flatten()
    }

    /// (v71) True when a stored member is bound to `pid` (`member_pane`) and
    /// the evidence built from `agents` and the reap journal judges it Dead,
    /// and no registry row is live on this pane. A refused restore placeholder
    /// reads `true` too: the same category with an earlier marker. The default
    /// prune closes such a tab; pristine stays the test for tabs that never
    /// hosted a worker.
    pub(super) fn orphaned_worker_for_pane(
        &self,
        pid: u64,
        agents: &[RegistryAgent],
        evidence: &crate::squad_store::MemberEvidence,
    ) -> bool {
        let live_row_on_pane = agents.iter().any(|a| {
            a.mux
                .as_ref()
                .is_some_and(|(sess, pane)| sess == &self.session_name && *pane == pid)
                && a.liveness == agents_view::Liveness::Alive
        });
        if live_row_on_pane {
            return false;
        }
        if self
            .panes
            .get(&pid)
            .and_then(|entry| entry.refused_worker.as_ref())
            .is_some()
        {
            return true;
        }
        self.squad_members.values().flatten().any(|member| {
            self.member_pane(member) == Some(pid)
                && matches!(
                    evidence.verdict(member),
                    crate::squad_store::MemberLiveness::Dead
                )
        })
    }
}
