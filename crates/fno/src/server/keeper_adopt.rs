//! Keeper re-adoption: the startup sweep that adopts each surviving keeper
//! child at the birth pane id its socket stem carries, plus the member
//! binding and leftover placement restore uses to seat adopted panes.
//! Child of `server`; parent items resolve through the glob.

use super::*;

/// Resolve the `fno-agents-worker` binary the keeper lane execs. Shared
/// shape with `fno_agents_bin` via `paired_bin`: env override, installed
/// sibling, dev-tree target dir, PATH.
pub(super) fn keeper_worker_bin() -> std::path::PathBuf {
    crate::digest_overlay::paired_bin("FNO_AGENTS_WORKER_BIN", "fno-agents-worker")
}

/// A pane this server re-adopted from a surviving keeper at startup, before
/// any stored member is knowable. Restore binds it to its member (or gives
/// it a tab of its own); `placed` is the once-only guard for that binding.
#[derive(Clone)]
pub(super) struct AdoptedKeeper {
    pub(super) pane: u64,
    pub(super) child_pid: Option<u32>,
    pub(super) argv: Vec<String>,
    pub(super) cwd: String,
    pub(super) placed: bool,
}

impl Core {
    /// Re-adopt surviving keeper panes at server start, BEFORE restore runs
    /// (an ordering constraint, not a preference: restore must see adopted
    /// panes as already-live members so it binds them instead of spawning
    /// replacements). For each socket: handshake with a short timeout, build
    /// the Keeper shell, replay the detached window into a fresh grid, and
    /// stage the adoption for restore to bind. A socket with nothing live
    /// behind it is unlinked and NAMED - it is a dead keeper's leftover, not
    /// a pane to wait on.
    pub(super) fn keeper_readopt(&mut self) {
        let sockets = crate::pty::keeper_sockets(&self.session_name);
        for (key, sock) in sockets {
            // The birth pane id is the socket stem's key: adopt at it whenever
            // it is reusable. Only key 0 or a key already live under a pane
            // falls back to a fresh id, and that pane then reads unreconciled.
            let (id, reconciled) = if key != 0 && !self.panes.contains_key(&key) {
                (key, true)
            } else {
                let reason = if key == 0 {
                    "pane key 0 is not adoptable".to_string()
                } else {
                    format!("pane {key} is already live")
                };
                let Ok(fresh) = self.reserve_pane_id() else {
                    break;
                };
                self.notice_all(format!(
                    "keeper readopt: {} carries pane key {key} but {reason}; adopting at fresh pane {fresh} as unreconciled",
                    sock.display()
                ));
                (fresh, false)
            };
            match crate::pty::adopt_keeper_socket(
                &sock,
                id,
                self.out_tx.clone(),
                self.exit_tx.clone(),
            ) {
                Ok(crate::pty::KeeperAdopt::NoListener) => {
                    let _ = std::fs::remove_file(&sock);
                    self.notice_all(format!(
                        "keeper readopt: {} had no live keeper behind it; removed",
                        sock.display()
                    ));
                }
                Ok(crate::pty::KeeperAdopt::SeatHeld) => {
                    // A live keeper whose subscriber seat is still held: a
                    // server mid-death. Leave the socket alone - the pane is
                    // real and the next start adopts it - and say so. Any
                    // freshly reserved id simply goes unused.
                    self.notice_all(format!(
                        "keeper readopt: {} still holds a subscriber seat; left for the next start",
                        sock.display()
                    ));
                }
                Ok(crate::pty::KeeperAdopt::Adopted(adoption)) => {
                    let str_list = |key: &str| -> Vec<String> {
                        adoption
                            .reply
                            .get(key)
                            .and_then(serde_json::Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(serde_json::Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    let str_field = |key: &str| -> String {
                        adoption
                            .reply
                            .get(key)
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    };
                    let num_field = |key: &str| -> u16 {
                        adoption
                            .reply
                            .get(key)
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(24)
                            .clamp(1, u16::MAX as u64) as u16
                    };
                    let argv = str_list("argv");
                    let cwd = str_field("cwd");
                    let child_pid = adoption
                        .reply
                        .get("child_pid")
                        .and_then(serde_json::Value::as_u64)
                        .map(|p| p as u32);
                    let rows = num_field("rows");
                    let cols = num_field("cols");
                    if let Err(e) = self.register_pane(
                        id,
                        adoption.shell,
                        rows,
                        cols,
                        node_from_argv(&argv),
                        agent_self_from_argv(&argv),
                        cwd.clone(),
                        cmd_from_argv(&argv),
                        account_from_argv(&argv),
                        resume_target_from_argv(&argv),
                        refused_worker_from_argv(&argv),
                    ) {
                        self.notice_all(format!(
                            "keeper readopt: {} refused registration ({e}); child was not adopted",
                            sock.display()
                        ));
                        continue;
                    }
                    // The detached window (AC3-HP): the keeper replayed its
                    // ring during the handshake; feed it before any layout
                    // push so the first frame the operator sees carries it.
                    if !adoption.ring.is_empty() {
                        if let Some(entry) = self.panes.get_mut(&id) {
                            entry.vt.feed(&adoption.ring);
                        }
                    }
                    if !reconciled {
                        if let Some(entry) = self.panes.get_mut(&id) {
                            entry.unreconciled = true;
                        }
                    }
                    self.keeper_adopted.push(AdoptedKeeper {
                        pane: id,
                        child_pid,
                        argv,
                        cwd,
                        placed: false,
                    });
                    self.notice_all(format!(
                        "keeper readopt: re-adopted pane {id} (child pid {}) from {}",
                        child_pid
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "?".into()),
                        sock.display()
                    ));
                }
                Err(e) => {
                    // A keeper that refuses the handshake is wedged or speaks
                    // an incompatible protocol: name it and keep adopting the
                    // rest. The socket STAYS - a live listener is the pane's
                    // only address, and unlinking it strands a running child
                    // with no path to re-adopt it (the same leave-alone policy
                    // as SeatHeld). The next start retries the handshake; a
                    // socket whose keeper is actually gone lands in the
                    // NoListener arm above and is removed there.
                    self.notice_all(format!(
                        "keeper readopt: {} refused adoption ({e}); left in place for the next start",
                        sock.display()
                    ));
                }
            }
        }
    }

    /// Bind one stored worker member to its re-adopted pane, once. The join
    /// is the member's own identity read back out of the pane's argv: the
    /// registered worker name (FNO_AGENT_SELF) or the resumed session id.
    /// Returns the pane and registers the `worker_pane` mapping restore's
    /// reconcile-first resume relies on, so a later resume FOCUSES the
    /// adopted pane instead of spawning a second writer.
    pub(super) fn take_adopted_for_member(
        &mut self,
        m: &crate::squad_store::StoredMember,
    ) -> Option<u64> {
        let worker = m.worker.as_deref();
        let session_id = m.harness_session_id.as_deref();
        let hit = self.keeper_adopted.iter_mut().find(|a| {
            if a.placed {
                return false;
            }
            let by_name = worker.is_some() && agent_self_from_argv(&a.argv).as_deref() == worker;
            let by_session =
                session_id.is_some() && resume_target_from_argv(&a.argv).as_deref() == session_id;
            by_name || by_session
        });
        let a = hit?;
        a.placed = true;
        Some(a.pane)
    }

    /// Place any adopted pane restore's member walk did not bind (its stored
    /// member is gone, or the store held no squads at all). A live pane must
    /// never be left dangling without a tab: one tab each, named from the
    /// pane's command, inside the squad owning its cwd (else home).
    pub(super) fn place_adopted_leftovers(&mut self, home_sid: u64) {
        let unplaced: Vec<AdoptedKeeper> = self
            .keeper_adopted
            .iter()
            .filter(|a| !a.placed)
            .cloned()
            .collect();
        for a in unplaced {
            let owner = self
                .session
                .squads
                .iter()
                .find(|s| !a.cwd.is_empty() && s.owns_path(&a.cwd))
                .map(|s| s.id)
                .unwrap_or(home_sid);
            if self.session.squad(owner).is_none() {
                continue;
            }
            let cmd = cmd_from_argv(&a.argv).unwrap_or_else(|| "pane".into());
            let tid = self.session.mint_tab_id();
            let tab = Tab {
                name: Some(cmd),
                id: tid,
                root: Node::Leaf(a.pane),
                focus: a.pane,
            };
            let Some(sq) = self.session.squads.iter_mut().find(|s| s.id == owner) else {
                continue;
            };
            sq.tabs.push(tab);
            if let Some(entry) = self.keeper_adopted.iter_mut().find(|x| x.pane == a.pane) {
                entry.placed = true;
            }
            self.notice_all(format!(
                "keeper readopt: pane {} (child pid {}) placed in its own tab; no stored member matches it",
                a.pane,
                a.child_pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            ));
        }
    }
}
