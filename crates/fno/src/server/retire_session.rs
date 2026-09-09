//! The v75 exact-session retirement (x-7649): the mux server's half of a
//! deliberate session retirement. PR 1599 shipped the store-level
//! `squad_store::retire_session_members` with no caller; this module is the
//! server call site the PR named as not switched.

use super::*;

impl Core {
    /// Retire one (harness, full session id) identity: close only the
    /// attached panes whose member matches, tombstone through the store's own
    /// `retire_session_members` (ONE locked mutation, the durable half),
    /// then re-project from the store so the in-memory member list cannot
    /// write the retired member back (the v71 prune rule). Unrelated panes,
    /// shared squads and the operator's own shells never match the identity
    /// filter and stay.
    ///
    /// Replies once. `retired` counts newly tombstoned members and
    /// `panes_closed` the panes this call closed; both are zero on a repeat
    /// call - retirement is idempotent, never an error. A failed store write
    /// replies `Err`: the durable half did not land, so the caller retries
    /// the whole verb (the already-closed panes close zero on the retry).
    pub(super) fn handle_retire_session(
        &mut self,
        harness: String,
        session_id: String,
        reply: ControlReply,
    ) -> Flow {
        // Identity resolution runs pane-side, through the same
        // pane-to-member join every worker close walks. A pane with no
        // member context (an operator shell, a portal viewer) carries no
        // harness identity and can never match.
        let candidates: Vec<u64> = self.panes.keys().copied().collect();
        let mut targets = Vec::new();
        for pid in candidates {
            let matches = self.worker_member_context(pid).is_some_and(|ctx| {
                ctx.harness.as_deref() == Some(harness.as_str())
                    && ctx.harness_session_id.as_deref() == Some(session_id.as_str())
            });
            if matches {
                targets.push(pid);
            }
        }
        let mut flow = Flow::Continue;
        let mut closed = 0usize;
        for pid in targets {
            // close_pane inherits the established close semantics: empty-tab
            // removal, portal stand-in replacement and the de-persist
            // contract all stay one code path with every other close.
            closed += 1;
            if self.close_pane(pid) == Flow::Shutdown {
                flow = Flow::Shutdown;
            }
        }
        let retired = match crate::squad_store::retire_session_members(&harness, &session_id) {
            Ok(retired) => retired,
            Err(error) => {
                let _ = reply.send(ServerMsg::Err {
                    code: err_code::STORE_WRITE_FAILED,
                    msg: format!("retire-session: store write failed: {error}"),
                });
                return Flow::Continue;
            }
        };
        self.reload_members_from_store();
        let _ = reply.send(ServerMsg::SessionRetired {
            retired,
            panes_closed: closed,
        });
        flow
    }

    /// Capture a worker member before a visible pane is reaped. The detached
    /// path cannot use `member_ctx`, whose attach-id shape belongs to claude
    /// attach members.
    pub(super) fn worker_member_context(&self, pane: u64) -> Option<DetachedPane> {
        let (squad, tab_index) = self.session.find_pane(pane)?;
        let entry = self.panes.get(&pane)?;
        let name = entry.name.as_deref()?.to_string();
        let sq = self.session.squad(squad)?;
        let tab_name = sq.tabs.get(tab_index).and_then(|tab| tab.name.clone());
        if let Some(member) = self.squad_members.get(&squad).and_then(|members| {
            members
                .iter()
                .find(|member| member.worker.as_deref() == Some(name.as_str()))
        }) {
            return DetachedPane::from_member(
                member,
                squad,
                sq.name.clone().unwrap_or_default(),
                sq.key.clone(),
                sq.origins.clone(),
            );
        }
        self.agents
            .iter()
            .find(|agent| {
                agent.name == name
                    && agent.mux.as_ref().is_some_and(|(session, candidate)| {
                        session == &self.session_name && *candidate == pane
                    })
            })
            .map(|agent| {
                DetachedPane::from_agent(
                    agent,
                    squad,
                    sq.name.clone().unwrap_or_default(),
                    sq.key.clone(),
                    sq.origins.clone(),
                    tab_name,
                )
            })
    }

    /// Reconcile the store and in-memory membership after a worker pane
    /// closes: churn keeps the member with a tombstone (a reaped worker is a
    /// past member), a non-churn close removes the member outright.
    pub(super) fn reconcile_worker_member_close(&mut self, detached: &DetachedPane, churn: bool) {
        let Some(members) = self.squad_members.get_mut(&detached.squad) else {
            return;
        };
        if churn {
            if let Some(member) = members
                .iter_mut()
                .find(|member| detached.matches_member(member))
            {
                member.tombstone = true;
                member.detached = false;
            }
        } else {
            members.retain(|member| !detached.matches_member(member));
        }
        if self.session.squad(detached.squad).is_some() {
            self.persist_squad(detached.squad);
        } else {
            self.squad_members.remove(&detached.squad);
            self.persist_remove(&detached.squad_name, &detached.squad_key);
        }
    }
}
