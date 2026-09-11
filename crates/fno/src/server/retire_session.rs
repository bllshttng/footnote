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
        // pane-to-member join every worker close walks. A pane carries a
        // member's identity only while it IS that member's pane: portal
        // viewers are titled after the row they watch, so the join also
        // requires `member_pane` to agree (x-9b37).
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
        // (x-9b37) The receipt names what closed instead of only counting:
        // "closed the worker" and "closed the worker and the operator's
        // viewer" must not read the same.
        let closed_panes: Vec<String> = targets
            .iter()
            .map(|pid| {
                self.panes
                    .get(pid)
                    .and_then(|e| e.name.clone())
                    .unwrap_or_else(|| format!("pane {pid}"))
            })
            .collect();
        let tabs_before: Vec<(u64, crate::tree::TabId, String)> = self
            .session
            .squads
            .iter()
            .flat_map(|sq| {
                sq.tabs.iter().map(move |t| {
                    let label = match (&sq.name, &t.name) {
                        (Some(sq_name), Some(t_name)) => format!("{sq_name}/{t_name}"),
                        (Some(sq_name), None) => format!("{sq_name}/tab {}", t.id),
                        (None, Some(t_name)) => t_name.clone(),
                        (None, None) => format!("tab {}", t.id),
                    };
                    (sq.id, t.id, label)
                })
            })
            .collect();
        let mut flow = Flow::Continue;
        let closed = targets.len();
        for pid in targets {
            // close_pane inherits the established close semantics: empty-tab
            // removal, portal stand-in replacement and the de-persist
            // contract all stay one code path with every other close.
            if self.close_pane_reasoned(pid, "session retired") == Flow::Shutdown {
                flow = Flow::Shutdown;
            }
        }
        let tabs_removed: Vec<String> = tabs_before
            .into_iter()
            .filter(|(sid, tid, _)| {
                self.session
                    .squad(*sid)
                    .is_none_or(|sq| !sq.tabs.iter().any(|t| t.id == *tid))
            })
            .map(|(_, _, label)| label)
            .collect();
        let (retired, batch) = match crate::squad_store::retire_session_members_with_generations(
            Some(&self.store_generations),
            &harness,
            &session_id,
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = reply.send(ServerMsg::Err {
                    code: err_code::STORE_WRITE_FAILED,
                    msg: format!("retire-session: store write failed: {error}"),
                });
                return Flow::Continue;
            }
        };
        self.persist_result(Ok(batch));
        self.reload_members_from_store();
        let _ = reply.send(ServerMsg::SessionRetired {
            retired,
            panes_closed: closed,
            closed_panes,
            tabs_removed,
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
            // (x-9b37) The name alone is a lie a portal can tell. A
            // member's identity moves only with its own pane, so the
            // inverse join must agree before this pane carries it.
            if self.member_pane(member) == Some(pane) {
                return DetachedPane::from_member(
                    member,
                    squad,
                    sq.name.clone().unwrap_or_default(),
                    sq.key.clone(),
                    sq.origins.clone(),
                );
            }
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
                // (x-8b51) A pane death is a real observed event, so the
                // churn arm keeps tombstoning - it just names why now.
                member.tombstone_reason = Some("worker pane died".into());
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
