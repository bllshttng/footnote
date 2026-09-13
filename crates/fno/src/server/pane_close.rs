//! What happens when one pane closes (x-9b37): the close cascade, the
//! portal stand-in swap, and the loss notice a vanishing portal owes the
//! operator who was reading it.

use super::*;

impl Core {
    /// Close one pane: kill+reap its PTY, remove it from the tree (collapse +
    /// focus re-anchor inside `tree::close`), cascade empty tab -> squad ->
    /// session (Locked 8). Idempotent: an unknown pane (double-close race,
    /// AC4-ERR) is a no-op.
    ///
    /// (x-d545) A portal seat is the one exception, and only when it is alone
    /// in its tab AND is the LAST open portal: a viewer whose child died must
    /// not delete the only window onto the fleet. An idle shell takes the leaf
    /// (`tree::replace_leaf`, the repoint mechanic) and the entry names the
    /// shell as a stand-in seat, so the next reach lands in the SAME tab. A
    /// spawn failure falls through to today's behavior: losing the tab is bad,
    /// wedging a tab around a dead pane is worse. A plain pane keeps today's
    /// semantics exactly (AC8-FR) - the arm is gated on a recorded seat id.
    ///
    /// (x-8f9d) With another portal open, "the only window" is false, so the
    /// swap does not fire and the closing portal simply goes away with its
    /// pane. Either way the entry is dropped unless a stand-in took the seat.
    pub(super) fn close_pane(&mut self, pid: u64) -> Flow {
        self.close_pane_reasoned(pid, "pane closed")
    }

    /// [`Core::close_pane`] with the reason the pane is dying, which a
    /// vanishing portal's loss notice carries (x-9b37).
    pub(super) fn close_pane_reasoned(&mut self, pid: u64, reason: &str) -> Flow {
        let Some((sid, ti)) = self.session.find_pane(pid) else {
            // Unknown to the tree; still reap a stray registry entry so a
            // half-created pane can never leak a child process.
            self.reap_pane(pid);
            return Flow::Continue;
        };
        // (x-8f9d) Which portal, if any, this pane seats. Equality against the
        // recorded seat, never a truthiness test: pane ids allocate from zero,
        // so pane 0 is a valid seat (the x-d914 defect).
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
        // (x-8f9d) The stand-in exists because "a viewer whose child died must
        // not delete the only window onto the fleet". With another portal open
        // that premise is false, so only the LAST portal keeps its seat alive.
        // Without this, closing four portals leaves four idle stand-in shells
        // each holding a tab open.
        //
        // LIVE seats, not `portals.len()`. An entry whose pane closed by some
        // other path stays in the map on purpose - the reach reads its tab id
        // to land a replacement viewer lands back where the operator had it,
        // the stale-slot behavior the single slot always had. Counting entries
        // would let one of those dead rows disarm the swap for a real portal.
        // The dying pane is still in `panes` here (the reap is last), so it
        // counts itself: `<= 1` means it is the only live one.
        let live_portals = self
            .portals
            .values()
            .filter(|portal| self.panes.contains_key(&portal.seat))
            .count();
        let last_portal = live_portals <= 1;
        let lone = seat
            && last_portal
            && self.session.squad(sid).is_some_and(|sq| {
                sq.tabs
                    .get(ti)
                    .is_some_and(|t| tree::leaves(&t.root).len() == 1)
            });
        if lone {
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
                    return Flow::Continue;
                }
                // The tab closed under the swap: undo the shell and fall
                // through to today's path.
                self.reap_pane(shell_pid);
            }
        }
        // (x-8f9d) No stand-in took the seat. The entry is deliberately LEFT
        // naming the now-dead pane, exactly as closing the single dedicated
        // pane always did: the reach treats a recorded pane the tree no longer
        // knows as absent, and reads its remembered tab id so a replacement
        // viewer lands back where the operator had it. Liveness is computed
        // from `panes` above, so a stale row can never be mistaken for an open
        // portal.
        // (x-9b37) A portal vanishing under a live operator destroys the
        // evidence they were reading, so the loss is broadcast, never silent.
        // The x-d545 swap above keeps the view, so it does not reach this.
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
                // its row returns at restart (same shape as the x-cde1 spec
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

    /// Shared re-anchor tail of `close_pane`'s surviving-session arms.
    fn close_pane_reanchor(&mut self, tid: TabId, sid: u64) -> Flow {
        // The tab (and possibly its squad) died: every client whose view named
        // it re-anchors in this same mutation, then the push delivers
        // ModeSync -> Layout -> frames in order (AC2-ERR).
        self.tab_areas.remove(&tid);
        // (x-cde1) Closing the last pane removes the tab too, so it must
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
