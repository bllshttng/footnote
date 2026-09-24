//! What happens when one pane closes : the close cascade, the
//! portal stand-in swap, and the loss notice a vanishing portal owes the
//! operator who was reading it.

use super::*;

/// Why a pane is closing. The split: a portal is just another
/// viewport, so a seat whose VIEWER died keeps its place, while an operator
/// close closes the pane like any pane's. The close path is told apart by
/// cause, never by the free-text reason string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseCause {
    /// The pane's child exited on its own: the PTY exit arm, the reap
    /// backstop, or a session retire. A live viewer seat always gets the
    /// stand-in swap.
    ViewerDied,
    /// An operator gesture: prefix+x, `fno mux pane kill`, the row menu's
    /// Close portal. Never mints a stand-in, so N deliberate closes can
    /// never leave N shells holding N tabs open.
    Operator,
}

impl Core {
    /// Close one pane whose child exited on its own: the death path. A live
    /// viewer seat always runs the stand-in swap (a portal is a viewport:
    /// the seat outlives the viewer and the row it showed), and a spawn
    /// failure falls through to the plain close with its loss notice.
    pub(super) fn close_viewer_died(&mut self, pid: u64, reason: &str) -> Flow {
        self.close_pane_for(pid, reason, CloseCause::ViewerDied)
    }

    /// Close one pane by operator gesture: the pane goes away like any
    /// pane's, no stand-in is minted, and a vanishing portal's loss notice
    /// still names what the operator was reading.
    pub(super) fn close_pane_reasoned(&mut self, pid: u64, reason: &str) -> Flow {
        self.close_pane_for(pid, reason, CloseCause::Operator)
    }

    /// [`Core::close_pane_reasoned`] with the default reason.
    pub(super) fn close_pane(&mut self, pid: u64) -> Flow {
        self.close_pane_reasoned(pid, "pane closed")
    }

    fn close_pane_for(&mut self, pid: u64, reason: &str, cause: CloseCause) -> Flow {
        let Some((sid, ti)) = self.session.find_pane(pid) else {
            // Unknown to the tree; still reap a stray registry entry so a
            // half-created pane can never leak a child process.
            self.reap_pane(pid);
            return Flow::Continue;
        };
        // Which portal, if any, this pane seats. Equality against the
        // recorded seat, never a truthiness test: pane ids allocate from zero,
        // so pane 0 is a valid seat (the defect).
        let seat_portal = self
            .portals
            .iter()
            .find(|(_, portal)| portal.seat == pid)
            .map(|(idx, _)| *idx);
        let seat = seat_portal.is_some()
            // A stand-in shell seat closing must NOT re-arm the swap: the tab
            // has to stay closable by hand, so only a real viewer (argv
            // provenance) triggers the replacement.
            && self.panes.get(&pid).is_some_and(|e| e.cmd.is_some());
        // The stand-in exists because a viewer whose child died must not
        // delete a window onto the fleet - ANY window, not only the last
        // one. Every live viewer seat keeps its place on the death path;
        // `tree::replace_leaf` works in a tab of any leaf count. The
        // operator path skips the swap: a deliberate close must leave the
        // tab closable, not swapped for a shell the operator never asked
        // for.
        let keep_seat = seat && cause == CloseCause::ViewerDied;
        if keep_seat {
            let (rows, cols) = self
                .panes
                .get(&pid)
                .map(|e| e.vt.size())
                .unwrap_or((24, 80));
            let cwd = self
                .session
                .squad(sid)
                .map(|s| s.canonical_cwd().to_string())
                .unwrap_or_default();
            if let Ok(shell_pid) = self.spawn_pane(rows, cols, &cwd) {
                let tab = &mut self.session.squad_mut(sid).expect("live squad").tabs[ti];
                if tree::replace_leaf(tab, pid, shell_pid) {
                    // Spawn-first paid off: swap the seat to the stand-in and
                    // reap the dead viewer last, the repoint arm's ordering.
                    if let Some(portal) = seat_portal.and_then(|idx| self.portals.get_mut(&idx)) {
                        portal.seat = shell_pid;
                    }
                    self.reap_pane(pid);
                    self.push_layout(true);
                    // The view was kept, but the pane the operator was
                    // reading changed identity: say so, naming the row.
                    if let Some(idx) = seat_portal {
                        if let Some(portal) = self.portals.get(&idx) {
                            self.notice_all(format!(
                                "portal {idx} ({}): viewer exited, seat kept",
                                portal.row_key
                            ));
                        }
                    }
                    return Flow::Continue;
                }
                // The tab closed under the swap: undo the shell and fall
                // through to the plain close below.
                self.reap_pane(shell_pid);
            }
        }
        // No stand-in took the seat. The entry is deliberately LEFT
        // naming the now-dead pane, exactly as closing the single dedicated
        // pane always did: the reach treats a recorded pane the tree no longer
        // knows as absent, and reads its remembered tab id so a replacement
        // viewer lands back where the operator had it. Liveness is computed
        // from `panes` above, so a stale row can never be mistaken for an open
        // portal.
        // A portal vanishing under a live operator destroys the
        // evidence they were reading, so the loss is broadcast, never silent.
        // The swap above keeps the view, so it does not reach this.
        if seat {
            let idx = seat_portal.expect("seat implies a portal");
            if let Some(portal) = self.portals.get(&idx) {
                self.notice_all(format!(
                    "portal {idx} ({}) closed: {reason}",
                    portal.row_key
                ));
            }
        }
        self.reap_pane(pid);
        let ident = self.squad_identity(sid);
        let tid = self
            .session
            .squad(sid)
            .expect("find_pane returned a live squad id")
            .tabs[ti]
            .id;
        let vp = self.tab_rect(tid);
        let squad = self
            .session
            .squad_mut(sid)
            .expect("find_pane returned a live squad id");
        let tab = &mut squad.tabs[ti];
        if !tree::close(tab, vp, pid) {
            self.push_layout(true);
            return Flow::Continue;
        }
        match self.session.remove_tab(sid, ti) {
            RemoveOutcome::SessionEmpty => {
                self.squad_members.remove(&sid);
                if let Some((name, key)) = ident {
                    self.persist_remove(&name, &key);
                }
                Flow::Shutdown
            }
            RemoveOutcome::SquadRemoved => {
                // The last pane's close removed the whole workspace - it must
                // honor the same de-persist contract as Command::CloseTab or
                // its row returns at restart (same shape as the spec
                // drop below).
                self.squad_members.remove(&sid);
                if let Some((name, key)) = ident {
                    self.persist_remove(&name, &key);
                }
                self.close_pane_reanchor(tid, sid)
            }
            _ => self.close_pane_reanchor(tid, sid),
        }
    }

    /// The `Command::ClosePane` body: close the pane the operator's view
    /// focuses, de-recruiting any worker membership it carried. Shared with
    /// `close_portal`, which validates the seat first.
    pub(super) fn close_by_operator(&mut self, pid: u64) -> Flow {
        // Capture membership BEFORE the reap clears it, reconcile AFTER
        // the close settles (so squad-survival is known) - user close
        // de-recruits (AC3-EDGE).
        let ctx = self.member_ctx(pid);
        let worker_ctx = self.worker_member_context(pid);
        let flow = self.close_pane_reasoned(pid, "closed by operator");
        self.reconcile_member_close(ctx, false);
        if let Some(worker_ctx) = worker_ctx {
            self.reconcile_worker_member_close(&worker_ctx, false);
        }
        flow
    }

    /// Close ONLY the portal seat `seat`: the viewer pane, never the row it
    /// shows. The thread keeps running and can be shown again anywhere.
    /// Fail-closed: a pane that is no live portal seat, or the session's
    /// last pane (its close would end the session), gets a notice and
    /// nothing else.
    pub(super) fn close_portal(&mut self, client_id: u64, seat: u64) -> Flow {
        let seat_portal = self
            .portals
            .iter()
            .find(|(_, portal)| portal.seat == seat)
            .map(|(idx, _)| *idx);
        let Some(idx) = seat_portal else {
            self.notice(client_id, format!("pane {seat} is not a portal seat"));
            return Flow::Continue;
        };
        if self.panes.len() <= 1 {
            self.notice(
                client_id,
                format!(
                    "portal {idx} is the session's only pane; closing it would end the session"
                ),
            );
            return Flow::Continue;
        }
        self.close_by_operator(seat)
    }

    /// Shared re-anchor tail of `close_pane`'s surviving-session arms.
    fn close_pane_reanchor(&mut self, tid: TabId, sid: u64) -> Flow {
        // The tab (and possibly its squad) died: every client whose view named
        // it re-anchors in this same mutation, then the push delivers
        // ModeSync -> Layout -> frames in order (AC2-ERR).
        self.tab_areas.remove(&tid);
        // Closing the last pane removes the tab too, so it must
        // honor the same de-persist contract as Command::CloseTab: a
        // template tab drops its stored spec or restore resurrects the
        // closed tab (persist rewrites the squad's list from live tabs).
        if self.template_specs.remove(&tid).is_some() {
            self.persist_template_specs(sid);
        }
        self.reanchor_views();
        self.push_layout(true);
        Flow::Continue
    }
}
