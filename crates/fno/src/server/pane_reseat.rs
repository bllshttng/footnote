//! The re-seat reach: moving a live pane-hosted worker INTO a portal seat
//! without ever re-attaching it. The detach/reattach trio the move builds on
//! lives here too, moved from the parent under the file-budget gate - the code
//! the change touched moves with it.

use super::*;

impl Core {
    /// Detach a live worker pane from the visible topology without reaping its
    /// child. A one-leaf tab gets a replacement shell so the session never
    /// renders an empty tab or loses its last anchor.
    pub(super) fn detach_worker_pane(&mut self, pane: u64) -> Result<(), String> {
        let (squad, tab_index) = self
            .session
            .find_pane(pane)
            .ok_or_else(|| format!("no such pane: {pane}"))?;
        let (rows, cols, entry_cwd) = {
            let entry = self
                .panes
                .get(&pane)
                .ok_or_else(|| format!("no such pane: {pane}"))?;
            if !entry.pty.is_child_alive() {
                return Err(format!("pane {pane} is no longer live"));
            }
            let (rows, cols) = entry.vt.size();
            (rows, cols, entry.cwd.clone())
        };
        let agent_matches = self
            .agents
            .iter()
            .filter(|agent| {
                !agent.exited
                    && (agent.mux.as_ref().is_some_and(|(session, candidate)| {
                        session == &self.session_name && *candidate == pane
                    }) || self.worker_pane_for_agent(agent) == Some(pane))
            })
            .collect::<Vec<_>>();
        let mapped_worker = self.worker_member_context(pane).filter(|worker| {
            self.worker_pane
                .get(&worker.name)
                .is_some_and(|panes| panes.contains(&pane))
        });
        let agent = match agent_matches.as_slice() {
            [agent] => Some((*agent).clone()),
            [] => None,
            matches => {
                return Err(format!(
                    "pane {pane} has no unique live worker row (matches: {}, session: {})",
                    matches.len(),
                    self.session_name
                ))
            }
        };
        if agent.is_none() && mapped_worker.is_none() {
            return Err(format!("pane {pane} has no unique live worker row"));
        }
        let (only_leaf, tab_name, tid, squad_name, squad_key, origins) = {
            let sq = self
                .session
                .squad(squad)
                .expect("find_pane returned live squad");
            let tab = &sq.tabs[tab_index];
            (
                tree::leaves(&tab.root).len() == 1,
                tab.name.clone(),
                tab.id,
                sq.name.clone().unwrap_or_default(),
                sq.key.clone(),
                sq.origins.clone(),
            )
        };
        let cwd = if entry_cwd.is_empty() {
            agent
                .as_ref()
                .map(|agent| agent.cwd.clone())
                .or_else(|| mapped_worker.as_ref().map(|worker| worker.cwd.clone()))
                .unwrap_or_default()
        } else {
            entry_cwd
        };
        let replacement = if only_leaf {
            Some(self.spawn_pane(rows, cols, &cwd)?)
        } else {
            None
        };
        let vp = self.tab_rect(tid);
        let si = self
            .session
            .squads
            .iter()
            .position(|s| s.id == squad)
            .expect("squad live");
        let outcome = {
            let tab = &mut self.session.squads[si].tabs[tab_index];
            tree::detach_leaf(tab, vp, pane).map_err(|error| error.to_string())?
        };
        if let (tree::DetachOutcome::TabEmptied, Some(shell)) = (outcome, replacement) {
            let tab = &mut self.session.squads[si].tabs[tab_index];
            tab.root = Node::Leaf(shell);
            tab.focus = shell;
        }
        let detached = if let Some(agent) = agent {
            DetachedPane::from_agent(&agent, squad, squad_name, squad_key, origins, tab_name)
        } else {
            mapped_worker.ok_or_else(|| format!("pane {pane} has no worker mapping"))?
        };
        self.detached_panes.insert(pane, detached.clone());
        self.persist_detached_member(&detached, true);
        Ok(())
    }

    pub(super) fn reattach_detached_pane(
        &mut self,
        pane: u64,
        fallback_squad: u64,
    ) -> Result<(u64, TabId), String> {
        let detached = self
            .detached_panes
            .get(&pane)
            .cloned()
            .ok_or_else(|| format!("no detached pane: {pane}"))?;
        if !self
            .panes
            .get(&pane)
            .is_some_and(|entry| entry.pty.is_child_alive())
        {
            return Err(format!("detached pane {pane} is no longer live"));
        }
        let squad = self
            .session
            .squad(detached.squad)
            .map(|_| detached.squad)
            .or_else(|| self.session.squad(fallback_squad).map(|_| fallback_squad))
            .ok_or_else(|| "target workspace no longer exists".to_string())?;
        let tid = self.session.mint_tab_id();
        self.session
            .squad_mut(squad)
            .expect("target squad checked")
            .tabs
            .push(Tab {
                name: detached.tab_name.clone(),
                id: tid,
                root: Node::Leaf(pane),
                focus: pane,
            });
        self.detached_panes.remove(&pane);
        self.bind_worker_pane(&detached, pane);
        self.persist_detached_member(&detached, false);
        Ok((squad, tid))
    }

    /// Detach `pane` from whatever tab currently holds it, keeping the PTY alive
    /// (the [`Self::pane_break`] cleanup, minus the new-tab step). A no-op if the
    /// pane is in no tab. Used to relocate a bound session's live pane into a
    /// template tab without ever reaping it (Reconcile: relocate, never kill).
    pub(super) fn detach_pane_keep_pty(&mut self, pane: u64) {
        let Some((sid, ti)) = self.session.find_pane(pane) else {
            return;
        };
        let tid = self.session.squad(sid).expect("find_pane live").tabs[ti].id;
        let vp = self.tab_rect(tid);
        let si = self
            .session
            .squads
            .iter()
            .position(|s| s.id == sid)
            .expect("squad live");
        let outcome = {
            let tab = &mut self.session.squads[si].tabs[ti];
            tree::detach_leaf(tab, vp, pane)
        };
        // TabEmptied leaves the tree unchanged (the pane still nominally in that
        // single-pane tab); dropping the tab frees the pane. Either way the PTY
        // survives, ready to graft into the template tree.
        if matches!(outcome, Ok(tree::DetachOutcome::TabEmptied)) {
            self.session.remove_tab(sid, ti);
            self.tab_areas.remove(&tid);
            self.reanchor_views();
        }
    }

    /// (v69) Re-seat a live pane-hosted worker into a portal seat: the ONE
    /// existing viewer moves, none is minted. The pane keeps its PTY and child
    /// (the harness process never restarts); it stops being persisted as a
    /// squad member, so restore never rebuilds it - being a thread means the
    /// row binds the session, not the geometry. The registry `mux` flip is the
    /// CALLER's half, on this receipt: the server is a reader of the registry,
    /// never its writer.
    ///
    /// Refuses before any mutation: a dead or unknown pane, a pane no unique
    /// live row answers, a full portal space, or a named slot whose seat is
    /// live (a re-seat never displaces a viewer). Idempotent: a pane already
    /// seated answers where it sits without touching the tree.
    pub(super) fn reseat_pane_into_portal(
        &mut self,
        pane: u64,
        portal_idx: Option<u8>,
    ) -> ServerMsg {
        let (squad, tab_index) = match self.session.find_pane(pane) {
            Some(found) => found,
            None => {
                return ServerMsg::Err {
                    code: err_code::NOT_FOUND,
                    msg: format!("no such pane: {pane}"),
                }
            }
        };
        let (rows, cols) = match self.panes.get(&pane) {
            Some(entry) if entry.pty.is_child_alive() => entry.vt.size(),
            Some(_) => {
                return ServerMsg::Err {
                    code: err_code::DEAD_PANE,
                    msg: format!("pane {pane} is no longer live"),
                }
            }
            None => {
                return ServerMsg::Err {
                    code: err_code::NOT_FOUND,
                    msg: format!("no such pane: {pane}"),
                }
            }
        };
        // Idempotence: a pane that already seats a portal is answered, not moved.
        if let Some((idx, _)) = self.portals.iter().find(|(_, p)| p.seat == pane) {
            let idx = *idx;
            self.push_layout(true);
            return ServerMsg::Notice {
                text: format!("already seated: portal {idx} (pane {pane})"),
            };
        }
        // The row the pane binds to, exactly one live one (the detach filter:
        // the mux ref, or the worker mapping a detach would tear down).
        let agent_matches = self
            .agents
            .iter()
            .filter(|agent| {
                !agent.exited
                    && (agent.mux.as_ref().is_some_and(|(session, candidate)| {
                        session == &self.session_name && *candidate == pane
                    }) || self.worker_pane_for_agent(agent) == Some(pane))
            })
            .cloned()
            .collect::<Vec<_>>();
        let row = match agent_matches.as_slice() {
            [one] => one.clone(),
            [] => {
                return ServerMsg::Err {
                    code: err_code::BAD_REQUEST,
                    msg: format!("pane {pane} has no unique live worker row"),
                }
            }
            many => {
                return ServerMsg::Err {
                    code: err_code::BAD_REQUEST,
                    msg: format!(
                        "pane {pane} has no unique live worker row (matches: {})",
                        many.len()
                    ),
                }
            }
        };
        let key = row.attach_id.clone().unwrap_or_else(|| row.name.clone());
        // Slot: caller index or the next free one; a live seat is never
        // displaced, a full space refuses (the x-0719 texts).
        let slot = match portal_idx {
            Some(idx) => {
                if let Some(occupied) = self.portals.get(&idx) {
                    let seat_live = self.panes.contains_key(&occupied.seat);
                    if seat_live {
                        return ServerMsg::Err {
                            code: err_code::BAD_REQUEST,
                            msg: format!("portal {idx} is live; close it or name another"),
                        };
                    }
                }
                idx
            }
            None => match self.next_free_portal() {
                Some(idx) => idx,
                None => {
                    return ServerMsg::Err {
                        code: err_code::BAD_REQUEST,
                        msg: "all 256 portal indices are live; close one first".to_string(),
                    }
                }
            },
        };
        // Membership context BEFORE the surgery: after the leaf leaves the
        // tree, `worker_member_context` can no longer resolve it (the ClosePane
        // ordering).
        let worker_ctx = self.worker_member_context(pane);
        let (only_leaf, old_tid, cwd) = {
            let sq = self.session.squad(squad).expect("find_pane live");
            let tab = &sq.tabs[tab_index];
            (
                tree::leaves(&tab.root).len() == 1,
                tab.id,
                if row.cwd.is_empty() {
                    sq.canonical_cwd().to_string()
                } else {
                    row.cwd.clone()
                },
            )
        };
        // Spawn-first: a one-leaf tab gets its replacement shell before the
        // tree is touched, so a spawn failure leaves everything as it was.
        let replacement = if only_leaf {
            let entry_cwd = self
                .panes
                .get(&pane)
                .map(|entry| entry.cwd.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or(cwd.clone());
            match self.spawn_pane(rows, cols, &entry_cwd) {
                Ok(shell) => Some(shell),
                Err(e) => {
                    return ServerMsg::Err {
                        code: err_code::SPAWN_FAILED,
                        msg: format!("reseat failed: {e}"),
                    }
                }
            }
        } else {
            None
        };
        let vp = self.tab_rect(old_tid);
        let si = self
            .session
            .squads
            .iter()
            .position(|s| s.id == squad)
            .expect("squad live");
        let outcome = {
            let tab = &mut self.session.squads[si].tabs[tab_index];
            match tree::detach_leaf(tab, vp, pane) {
                Ok(outcome) => outcome,
                Err(e) => {
                    if let Some(shell) = replacement {
                        self.reap_pane(shell);
                    }
                    return ServerMsg::Err {
                        code: err_code::BAD_REQUEST,
                        msg: format!("reseat failed: {e}"),
                    };
                }
            }
        };
        if let (tree::DetachOutcome::TabEmptied, Some(shell)) = (outcome, replacement) {
            let tab = &mut self.session.squads[si].tabs[tab_index];
            tab.root = Node::Leaf(shell);
            tab.focus = shell;
        }
        // The graft: a fresh tab in the owner-routed squad (the
        // reattach_detached_pane shape), so the pane keeps rendering while
        // owning no squad membership.
        let dest = self.session.find_by_cwd(&cwd).unwrap_or(squad);
        if self.session.squad(dest).is_none() {
            self.reap_pane(pane);
            return ServerMsg::Err {
                code: err_code::BAD_REQUEST,
                msg: "reseat failed: the target workspace vanished".to_string(),
            };
        }
        let tid = self.session.mint_tab_id();
        self.session
            .squad_mut(dest)
            .expect("target squad checked")
            .tabs
            .push(Tab {
                name: clean_tab_name(Some(row.name.clone())),
                id: tid,
                root: Node::Leaf(pane),
                focus: pane,
            });
        // Bookkeeping: de-recruit the member (stop persisting it as a squad
        // member - restore never rebuilds a portal seat), keep exactly one
        // `attached` viewer per attach id, and record the seat.
        if let Some(worker_ctx) = worker_ctx {
            self.reconcile_worker_member_close(&worker_ctx, false);
        }
        if let Some(id) = row.attach_id.clone() {
            self.attached.insert(id, pane);
        }
        self.portals.insert(
            slot,
            Portal {
                row_key: key.clone(),
                seat: pane,
                tab: tid,
            },
        );
        self.claim_eligible.insert(pane);
        self.push_layout(true);
        ServerMsg::Notice {
            text: format!("reseat -> {key} (portal {slot}, pane {pane})"),
        }
    }
}
