//! The server-resume strategy: the pane TUI exits, the shared app-server
//! resumes the same session.
//!
//! A Codex pane TUI holds its rollout in-process, so nothing can resume that
//! session while the TUI is alive - the resume would open a second reader on
//! one rollout. The pane must be gone first, and "gone" here means ESRCH on
//! the child pid, never a kill that was merely sent.
//!
//! The session id survives, which is the measured basis for the strategy:
//! `docs/architecture/workspace-restore.md` records a Codex resumed under its
//! own id after a SIGKILL.
//!
//! The rollback is the reason the row is snapshotted before anything moves.
//! If the resume refuses after the pane stopped, the row goes back to its
//! pane shape and the pane is relaunched under the same id. If the relaunch
//! also fails, the row is left EXITED and the receipt prints both errors -
//! a row reading `live` with nothing behind it is the one outcome worse than
//! a failed conversion.

use crate::state::RegistryEntry;

/// The pane-shape fields a rollback restores. Only the fields the flip
/// touches: a snapshot of the whole row would restore state that legitimately
/// moved on while the conversion ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowSnapshot {
    pub substrate: Option<String>,
    pub host_mode: Option<String>,
    pub short_id: String,
    pub mux: Option<crate::state::MuxRef>,
    pub pid: Option<u32>,
    pub pid_start_time: Option<u64>,
}

impl RowSnapshot {
    pub fn of(entry: &RegistryEntry) -> Self {
        RowSnapshot {
            substrate: entry.substrate.clone(),
            host_mode: entry.host_mode.clone(),
            short_id: entry.short_id.clone(),
            mux: entry.mux.clone(),
            pid: entry.pid,
            pid_start_time: entry.pid_start_time,
        }
    }

    pub fn restore(&self, entry: &mut RegistryEntry) {
        entry.substrate = self.substrate.clone();
        entry.host_mode = self.host_mode.clone();
        entry.short_id = self.short_id.clone();
        entry.mux = self.mux.clone();
        entry.pid = self.pid;
        entry.pid_start_time = self.pid_start_time;
    }
}

/// Flip a row to the shape `is_codex_thread_entry` recognises: harness codex,
/// host mode interactive, EMPTY short id, no mux ref. The full
/// `harness_session_id` is deliberately untouched - it is the rollout the
/// resume addresses, and it is the whole point of the conversion.
///
/// The pid is cleared because a codex thread owns no process of its own: the
/// shared app-server holds the session, and a leftover pane pid would have
/// the liveness ladder probing a process that is already gone.
pub fn to_codex_thread(entry: &mut RegistryEntry) {
    entry.substrate = Some("thread".to_string());
    entry.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
    entry.short_id = String::new();
    entry.mux = None;
    entry.pid = None;
    entry.pid_start_time = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MuxRef;
    use crate::AgentStatus;

    fn codex_pane() -> RegistryEntry {
        let mut entry = RegistryEntry {
            name: "king-delivery".to_string(),
            cwd: "/repo".to_string(),
            status: AgentStatus::Live,
            created_at: "2026-09-20T00:00:00Z".to_string(),
            ..Default::default()
        };
        entry.harness = Some("codex".to_string());
        entry.harness_session_id = Some("01a09bcd-8b5f-7391-83f8-d9ed91b00ac5".to_string());
        entry.substrate = Some("pane".to_string());
        entry.host_mode = Some("exec".to_string());
        entry.short_id = "kingdeli".to_string();
        entry.mux = Some(MuxRef {
            session: "fno".to_string(),
            pane_id: 2313,
        });
        entry.pid = Some(63890);
        entry.pid_start_time = Some(1234);
        entry
    }

    #[test]
    fn the_flip_produces_a_row_the_codex_thread_reader_recognises() {
        let mut entry = codex_pane();
        let session_id = entry.harness_session_id.clone();
        to_codex_thread(&mut entry);
        // The four facts `is_codex_thread_entry` keys on.
        assert_eq!(entry.harness_name(), "codex");
        assert_eq!(
            entry.host_mode_or_default(),
            crate::state::HOST_MODE_INTERACTIVE
        );
        assert!(entry.short_id.is_empty(), "a thread row has no short id");
        assert!(entry.mux.is_none(), "a thread row holds no mux ref");
        // The session id is the rollout the resume addresses; losing it is
        // losing the conversation.
        assert_eq!(entry.harness_session_id, session_id);
        // A thread owns no process: a leftover pane pid would have the
        // liveness ladder probing a process that is already gone.
        assert_eq!(entry.pid, None);
        assert_eq!(entry.pid_start_time, None);
    }

    #[test]
    fn a_snapshot_round_trips_the_pane_shape_exactly() {
        let original = codex_pane();
        let snapshot = RowSnapshot::of(&original);
        let mut entry = original.clone();
        to_codex_thread(&mut entry);
        assert_ne!(entry.substrate, original.substrate);
        snapshot.restore(&mut entry);
        assert_eq!(entry.substrate, original.substrate);
        assert_eq!(entry.host_mode, original.host_mode);
        assert_eq!(entry.short_id, original.short_id);
        assert_eq!(entry.mux, original.mux);
        assert_eq!(entry.pid, original.pid);
        assert_eq!(entry.pid_start_time, original.pid_start_time);
    }

    #[test]
    fn a_rollback_leaves_fields_the_conversion_never_touched_alone() {
        // The snapshot is deliberately narrow: a whole-row restore would
        // undo state that legitimately moved while the conversion ran.
        let mut entry = codex_pane();
        let snapshot = RowSnapshot::of(&entry);
        to_codex_thread(&mut entry);
        entry.node = Some("x-node".to_string());
        entry.model = Some("gpt-5.6-luna".to_string());
        snapshot.restore(&mut entry);
        assert_eq!(entry.node.as_deref(), Some("x-node"));
        assert_eq!(entry.model.as_deref(), Some("gpt-5.6-luna"));
    }
}
