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
    /// The keeper attempt for a plain shell: try each shell candidate through
    /// a keeper, carrying the shell-integration rc as an `env` argv prefix
    /// (`pty::keeper_shell_argv`). `Ok(Some(id))` = hosted; the pane is
    /// registered, its ring fed, and its rc dir owned by `shell_rc_dirs`.
    /// `Ok(None)` = no keeper could host it: the caller falls back to the
    /// inline pty and marks the pane unkept. `Err` = the spawn itself is
    /// impossible (admission refused, no child pid). A failed candidate's rc
    /// dir is removed here; only a REGISTERED pane's dir is kept.
    ///
    /// Silent when NOT ONE candidate even reaches a real keeper attempt (a
    /// `SHELL` naming neither zsh nor bash - `keeper_shell_argv`'s known-shell
    /// gate, integration is bash/zsh-only by design): that pane was never
    /// going to be hosted, so it is expected non-participation, not a
    /// failure worth a client-visible notice. A genuine spawn attempt that
    /// errors (admission, handshake, a keeper binary that dies) still
    /// notifies - operationally that IS worth surfacing. Getting this
    /// backwards is user-visible: the notice renders into the client's
    /// status row and nothing re-draws to clear it once its TTL lapses
    /// (`client/row_stamp.rs`'s `NOTICE_TTL`), so on a plain `/bin/sh`
    /// session it would show on every single split, permanently, baked into
    /// whatever screen a test (or a real client) settles on next - exactly
    /// the byte-exact-reattach mismatch a `/bin/sh`-shelled session hits on
    /// every pane spawn, proven via `crates/fno/tests/persistence.rs`'s
    /// `persistence_multi_pane_reattach_is_screen_exact` (row 1 of the
    /// settled "before" screen read `keeper spawn failed for pane 2 (no
    /// shell candidate produce…`, a row no fresh reattach ever reproduces).
    #[cfg(not(test))]
    pub(super) fn spawn_pane_kept(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: &str,
        id: u64,
        dir: Option<&std::path::Path>,
    ) -> Result<Option<u64>, String> {
        let mut last = String::from("no shell candidate produced a keeper argv");
        let mut attempted = false;
        for cand in &self.shells {
            let Some((argv, rc_dir)) = crate::pty::keeper_shell_argv(cand, &self.session_name, id)
            else {
                continue;
            };
            attempted = true;
            let permit = match crate::process_admission::admit_fleet() {
                Ok(permit) => permit,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&rc_dir);
                    return Err(e.to_string());
                }
            };
            match crate::pty::PtyShell::spawn_cmd_keeper_with_permit(
                &keeper_worker_bin(),
                &argv,
                rows,
                cols,
                dir,
                &self.session_name,
                id,
                self.out_tx.clone(),
                self.exit_tx.clone(),
                permit,
            ) {
                Ok((shell, ring)) => {
                    // A shell pane carries no node provenance (no wrapper
                    // argv worth parsing: the env prefix is integration, not
                    // identity).
                    self.register_pane(
                        id,
                        shell,
                        rows,
                        cols,
                        None,
                        None,
                        cwd.to_string(),
                        None,
                        None,
                        None,
                        None,
                        None,
                    )?;
                    if !ring.is_empty() {
                        if let Some(entry) = self.panes.get_mut(&id) {
                            entry.vt.feed(&ring);
                        }
                    }
                    self.shell_rc_dirs.insert(id, rc_dir);
                    return Ok(Some(id));
                }
                Err(e) => {
                    last = e.to_string();
                    let _ = std::fs::remove_dir_all(&rc_dir);
                }
            }
        }
        if attempted {
            self.notice_all(format!(
                "keeper spawn failed for pane {id} ({last}); opening an unkept inline shell"
            ));
        }
        Ok(None)
    }

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
                        portal_hold_from_argv(&argv),
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
                    // The resume birthright rides the adoption: the argv's
                    // resume token is the session id the identity join answers
                    // with on THIS server, exactly as the spawning server
                    // recorded it. Without this, a restart drops a resumed
                    // worker's only address and every send to it refuses.
                    if let Some(session_id) = resume_target_from_argv(&argv) {
                        let harness = argv
                            .iter()
                            .find(|a| !a.contains('='))
                            .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
                            .unwrap_or_default();
                        self.worker_session_pane.insert((harness, session_id), id);
                    }
                    // The shell-integration rc dir a keeper shell's argv
                    // references outlived the spawning server: re-own it, so
                    // a later close removes it and an adopted shell never
                    // loses its rc to a server death.
                    let rc_dir = crate::pty::shell_rc_dir(&self.session_name, id);
                    if rc_dir.exists() {
                        self.shell_rc_dirs.insert(id, rc_dir);
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

    /// Bind one stored SHELL slot to its re-adopted pane by birth pane id:
    /// the slot recorded the pane id that lived in the leaf at capture, and
    /// the keeper re-adopts at the birth id, so the id is a safe join. Only
    /// an unplaced adoptee joins; a fresh-id adoption (unreconciled) never
    /// matches and lands in its own tab with today's notice.
    pub(crate) fn take_adopted_for_slot(&mut self, birth: u64) -> Option<u64> {
        let hit = self
            .keeper_adopted
            .iter_mut()
            .find(|a| !a.placed && a.pane == birth);
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
