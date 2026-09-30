//! Dropping a pane entry, with or without killing what it hosts.
//!
//! Moved out of the parent under the file-budget gate, with the code the
//! change touched: `reap_pane` was one body that always killed, and the
//! pane-to-thread hand-off needs the SAME bookkeeping without the kill.
//! Two copies of that bookkeeping would drift, and the half that forgot to
//! clear `worker_pane` would leave a mapping pointing at a pane the server
//! no longer hosts.
//!
//! The difference is one frame. A keeper's Kill makes the keeper kill its
//! own child and exit; simply DROPPING the connection is a hangup, which a
//! keeper survives by design. That is what lets a live session outlive the
//! server that was showing it.

use super::*;

impl Core {
    /// Drop `pid`'s entry and every mapping onto it, killing what it hosts.
    pub(super) fn reap_pane(&mut self, pid: u64) {
        self.drop_pane_entry(pid, true);
    }

    /// Drop `pid`'s entry and every mapping onto it, LEAVING what it hosts
    /// running. The pty connection closes, which a keeper reads as a hangup
    /// and survives with its child; an inline pane has no such owner, so
    /// this is only ever correct for a keeper-hosted pane, and the caller
    /// checks that before it gets here.
    pub(super) fn release_pane(&mut self, pid: u64) {
        self.drop_pane_entry(pid, false);
    }

    fn drop_pane_entry(&mut self, pid: u64, kill: bool) {
        if let Some(entry) = self.panes.remove(&pid) {
            if let Ok(mut children) = self.pane_children.lock() {
                if let Some(child_pid) = entry.pty.child_pid() {
                    let child = PaneChild {
                        pid: child_pid,
                        keeper_hosted: entry.pty.is_keeper_hosted(),
                    };
                    children.remove(&child);
                }
            }
            if kill {
                entry.pty.kill();
            }
        }
        // A keeper shell's rc dir is this server's to clean: the pane is
        // closing, so the dir its shell still references goes with it. (An
        // inline pane's dir dies with its own `ShellRc`.) A released pane
        // keeps its dir: the child is still running and still referencing it.
        if kill {
            if let Some(dir) = self.shell_rc_dirs.remove(&pid) {
                let _ = std::fs::remove_dir_all(&dir);
            }
        } else {
            self.shell_rc_dirs.remove(&pid);
        }
        // Pane exit releases the writer claim UNCONDITIONALLY (Locked 5): a
        // held claim never blocks the close cascade.
        self.claims.remove(&pid);
        self.claim_eligible.remove(&pid);
        self.touch_last_emit.remove(&pid);
        self.wheel_gate.remove(&pid);
        self.pane_stats.write().unwrap().remove(&pid);
        // Drop any attach mapping onto the dead pane so a re-attach
        // spawns fresh rather than focusing a corpse (the lazy `panes` check in
        // `agent_rows()` is the belt to this eager suspenders - Discretion 3).
        self.attached.retain(|_, p| *p != pid);
        // Same for the worker resume map: the pane died, so the row
        // returns to idle and resumable - never a mapping at a corpse.
        self.worker_pane.retain(|_, panes| {
            panes.retain(|candidate| *candidate != pid);
            !panes.is_empty()
        });
        self.worker_session_pane.retain(|_, p| *p != pid);
        self.held_workers.remove(&pid);
        self.detached_panes.remove(&pid);
        if let Some(tx) = self.pane_watch.remove(&pid) {
            // Last observable tick before the sender drops: a watcher that
            // reads it sees `exited`; one blocked in `changed()` sees the
            // sender-dropped error and treats it identically.
            tx.send_modify(|t| t.exited = true);
        }
    }
}
