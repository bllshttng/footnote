//! What happens when one pane closes: the close cascade, the parked
//! screen a dead viewer's portal keeps showing, and the loss notice a
//! vanishing portal owes the operator who was reading it.

use super::*;

/// Why a pane is closing. The split: a portal outlives its viewer, so a
/// seat whose VIEWER died keeps its place and parks on the no-signal
/// screen, while an operator close closes the pane like any pane's. The
/// close path is told apart by cause, never by the free-text reason
/// string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseCause {
    /// The pane's child exited on its own: the PTY exit arm, the reap
    /// backstop, or a session retire. A live viewer seat always gets the
    /// parked screen.
    ViewerDied,
    /// An operator gesture: prefix+x, `fno mux pane kill`, the row menu's
    /// Close portal. Never mints a screen, so N deliberate closes can
    /// never leave N shells holding N tabs open.
    Operator,
}

impl CloseCause {
    /// The schema's cause word for the close, shared by the pane_closed
    /// row and the portal_closed row the same path emits.
    fn word(self) -> &'static str {
        match self {
            CloseCause::ViewerDied => "viewer_died",
            CloseCause::Operator => "operator",
        }
    }
}

/// The `pane_closed` journal row, pure so tests can assert the
/// envelope. Cause is the enum's word, never the free text alone; identity
/// fields ride null when no registry row binds the pane.
fn pane_closed_row(
    mux_session: &str,
    pane: u64,
    squad: u64,
    cause: &str,
    reason: &str,
    name: Option<&str>,
    harness_session: Option<&str>,
    harness: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "ts": crate::review_invocation::review_invocation_timestamp(),
        "type": "pane_closed",
        "source": "daemon",
        "data": {
            "mux_session": mux_session,
            "pane": pane,
            "squad": squad,
            "cause": cause,
            "reason": reason,
            "name": name,
            "harness_session": harness_session,
            "harness": harness,
        }
    })
}

/// The `server_stopped` journal row, pure so tests can assert the
/// envelope. One row per serve lifetime; a daemon-restart bounce that closes
/// nothing still names itself here.
fn server_stopped_row(mux_session: &str, cause: &str, panes: usize) -> serde_json::Value {
    serde_json::json!({
        "ts": crate::review_invocation::review_invocation_timestamp(),
        "type": "server_stopped",
        "source": "daemon",
        "data": {
            "mux_session": mux_session,
            "cause": cause,
            "panes": panes,
        }
    })
}

impl Core {
    /// Emit [`pane_closed_row`] for a close that removed a real pane, with
    /// the pane's identity bound the same way `witness_row` binds typing.
    /// Best-effort: a failed append never blocks the close.
    fn emit_pane_closed(&self, pid: u64, sid: u64, cause: &str, reason: &str) {
        let bound = super::agent_rows_join::bind_agent_to_pane(
            &self.agents,
            &self.session_name,
            pid,
            &self.attached,
            &|a| self.worker_pane_for_agent(a),
        )
        .map(|i| &self.agents[i]);
        let name = self.panes.get(&pid).and_then(|e| e.name.clone());
        let event = pane_closed_row(
            &self.session_name,
            pid,
            sid,
            cause,
            reason,
            name.as_deref(),
            bound.and_then(|a| a.harness_session_id.as_deref()),
            bound.and_then(|a| a.harness.as_deref()),
        );
        crate::pane_send_audit::queue_agents_event(
            &crate::pane_send_audit::pane_send_audit_events_path(),
            event,
            None,
        );
    }

    /// Emit [`server_stopped_row`] at the serve exit. Best-effort like
    /// [`Self::emit_pane_closed`]. The last row of the process, so it waits
    /// for the journal writer to drain before the server exits.
    pub(super) fn emit_server_stopped(&self, cause: &str) {
        let event = server_stopped_row(&self.session_name, cause, self.panes.len());
        crate::pane_send_audit::queue_agents_event(
            &crate::pane_send_audit::pane_send_audit_events_path(),
            event,
            None,
        );
        crate::pane_send_audit::flush_agents_journal(std::time::Duration::from_secs(2));
    }

    /// Close one pane whose child exited on its own: the death path. A live
    /// viewer seat always swaps to the parked screen (the portal outlives
    /// its viewer: the window stays, the channel goes quiet), and a spawn
    /// failure falls through to the plain close with its loss notice.
    pub(super) fn close_viewer_died(&mut self, pid: u64, reason: &str) -> Flow {
        self.close_pane_for(pid, reason, CloseCause::ViewerDied)
    }

    /// Close one pane by operator gesture: the pane goes away like any
    /// pane's, no screen is minted, and a vanishing portal's loss notice
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
            // The channel stays parked: the portal keeps its index and
            // leaf and shows the no-signal screen. No interactive shell is
            // minted, so a dead viewer can never multiply tabs.
            let channel = self
                .portals
                .get(&seat_portal.expect("seat implies a portal"))
                .map(|portal| portal.row_key.clone())
                .unwrap_or_default();
            if let Ok(screen_pid) = self.spawn_parked_screen(&channel, rows, cols, &cwd) {
                let tab = &mut self.session.squad_mut(sid).expect("live squad").tabs[ti];
                if tree::replace_leaf(tab, pid, screen_pid) {
                    // Spawn-first paid off: swap the seat to the parked
                    // screen and reap the dead viewer last, the repoint
                    // arm's ordering.
                    if let Some(portal) = seat_portal.and_then(|idx| self.portals.get_mut(&idx)) {
                        portal.seat = screen_pid;
                    }
                    self.emit_pane_closed(
                        pid,
                        sid,
                        "viewer_died",
                        &format!("{reason} (seat parked)"),
                    );
                    self.reap_pane(pid);
                    self.push_layout(true);
                    if let Some(idx) = seat_portal {
                        let line = format!("portal {idx}: no signal - {channel} ended");
                        self.write_restore_message(screen_pid, &line);
                        self.notice_all(line);
                    }
                    return Flow::Continue;
                }
                // The tab closed under the swap: undo the screen and fall
                // through to the plain close below.
                self.reap_pane(screen_pid);
            }
        }
        // No screen took the seat. The portal is GONE: an operator close
        // removes the entry, and a death whose parked screen failed to
        // spawn loses it too - a portal lives until the operator closes
        // it, and every survivor keeps a live screen on its entry. The
        // reach still reads a remembered tab for one generation, but the
        // map no longer carries a stale row.
        if seat {
            let idx = seat_portal.expect("seat implies a portal");
            if let Some(portal) = self.portals.get(&idx) {
                self.notice_all(format!(
                    "portal {idx} ({}) closed: {reason}",
                    portal.row_key
                ));
            }
            self.journal_portal_take(idx, cause.word());
        }
        self.emit_pane_closed(
            pid,
            sid,
            match cause {
                CloseCause::ViewerDied => "viewer_died",
                CloseCause::Operator => "operator",
            },
            reason,
        );
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
        // Liveness in the find: a stale entry still NAMES a closed seat,
        // and a close that lands on nothing must refuse, not no-op.
        let seat_portal = self
            .portals
            .iter()
            .find(|(_, portal)| portal.seat == seat && self.panes.contains_key(&portal.seat))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The envelope carries the cause word and the reason verbatim, and the
    /// schema's required fields are present.
    #[test]
    fn pane_closed_row_carries_cause_and_reason() {
        let row = pane_closed_row(
            "main",
            7,
            1,
            "operator",
            "closed by operator",
            Some("w1"),
            Some("sess-a"),
            Some("codex"),
        );
        assert_eq!(row["type"], "pane_closed");
        assert_eq!(row["source"], "daemon");
        let data = &row["data"];
        assert_eq!(data["mux_session"], "main");
        assert_eq!(data["pane"], 7);
        assert_eq!(data["squad"], 1);
        assert_eq!(data["cause"], "operator");
        assert_eq!(data["reason"], "closed by operator");
        assert_eq!(data["name"], "w1");
        assert_eq!(data["harness_session"], "sess-a");
        assert_eq!(data["harness"], "codex");
        let bare = pane_closed_row(
            "main",
            9,
            2,
            "viewer_died",
            "child exited",
            None,
            None,
            None,
        );
        assert_eq!(bare["data"]["cause"], "viewer_died");
        assert!(bare["data"]["name"].is_null());
        let stop = server_stopped_row("main", "shutdown", 3);
        assert_eq!(stop["type"], "server_stopped");
        assert_eq!(stop["data"]["cause"], "shutdown");
        assert_eq!(stop["data"]["panes"], 3);
        // The portal rows ride the same envelope contract: one open row
        // with the seat, one close row with the door's cause word.
        let opened = crate::server::portal_journal::portal_opened_row("main", 3, "candor", 42);
        assert_eq!(opened["type"], "portal_opened");
        assert_eq!(opened["data"]["portal"], 3);
        assert_eq!(opened["data"]["row_key"], "candor");
        assert_eq!(opened["data"]["seat"], 42);
        let closed =
            crate::server::portal_journal::portal_closed_row("main", 3, "candor", "retune");
        assert_eq!(closed["type"], "portal_closed");
        assert_eq!(closed["data"]["cause"], "retune");
        assert_eq!(closed["data"]["row_key"], "candor");
    }
}
