//! Native retirement effects (x-70e1 task 3): the harness-native half of a
//! retirement, applied through the transports each harness already owns and
//! reported as typed per-effect outcomes.
//!
//! The sweep owns the sequence (stop, active-surface removal, registry drop,
//! tree prune); this module owns the ACTIVE-SURFACE removal - claude's agent
//! list, codex's session index, cursor-agent's detached worker servers -
//! through the same cascade the `rm` verb walks. A history deletion never
//! happens here: the codex index drop keeps the rollout files, the claude
//! list removal keeps the transcript, and an archive op (codex
//! `thread/archive`) is history-preserving by construction.
//!
//! Absence is only accepted after a complete enumeration of the exact
//! identity; a failed read is `Unverified` or `Failed`, never absence.

use crate::daemon::{cascade_harness_session_result_with, CascadeOutcome};
use crate::receipt::EffectRecord;
use crate::state::RegistryEntry;

impl CascadeOutcome {
    /// The effect-record vocabulary: `confirmed-removed`,
    /// `confirmed-already-absent`, `kept`, `failed`, `not-applicable`. One
    /// string per outcome, named in the receipt, so a partial retirement is
    /// never readable as a full one.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            CascadeOutcome::Removed => "confirmed-removed",
            CascadeOutcome::AlreadyAbsent(_) => "confirmed-already-absent",
            CascadeOutcome::Unverified(_) => "kept",
            CascadeOutcome::Failed(_) => "failed",
            CascadeOutcome::NotApplicable => "not-applicable",
        }
    }

    pub(crate) fn detail(&self) -> Option<String> {
        match self {
            CascadeOutcome::AlreadyAbsent(r)
            | CascadeOutcome::Unverified(r)
            | CascadeOutcome::Failed(r) => Some(r.clone()),
            CascadeOutcome::Removed | CascadeOutcome::NotApplicable => None,
        }
    }

    /// Whether this outcome satisfies the applied gate: only a positive
    /// confirmation (or a measured not-applicable) does. `kept` and `failed`
    /// hold the row for retry.
    pub(crate) fn satisfies_applied(&self) -> bool {
        matches!(
            self,
            CascadeOutcome::Removed
                | CascadeOutcome::AlreadyAbsent(_)
                | CascadeOutcome::NotApplicable
        )
    }

    /// The receipt's EffectRecord for this outcome under a named op.
    pub(crate) fn effect_record(&self, op: &str) -> EffectRecord {
        EffectRecord {
            op: op.to_string(),
            outcome: self.as_str().to_string(),
            detail: self.detail(),
            at: crate::daemon::now_rfc3339_like(),
        }
    }
}

/// Apply the ACTIVE-SURFACE removal for one row through the production
/// seams (the daemon roster read and `claude rm`), returning the typed
/// outcome the sweep records on the receipt. The roster read unions every
/// account root, and the removal is routed to the root the row's launch
/// account owns - absence from a single ambient read is a WRONG-ROOT
/// absence and has never been removal evidence.
pub(crate) fn apply_active_surface_removal(e: &RegistryEntry) -> CascadeOutcome {
    // The snapshot is computed ONCE here and handed to the cascade, matching
    // the rm handler: the pre-check, the removal and the post-read must see
    // the same listing generation, and a claude arm without a snapshot is a
    // panic the caller cannot recover from.
    let snapshot = crate::claude_roster::read_all_agents_union();
    let account_dir = crate::claude_roster::isolated_account_dirs()
        .into_iter()
        .find(|(id, _)| Some(id.as_str()) == e.launch_account.as_deref())
        .map(|(_, dir)| dir);
    let account_for_rm = account_dir.as_deref();
    cascade_harness_session_result_with(
        e,
        Some(&snapshot),
        &crate::claude_roster::read_all_agents_union,
        &move |short_id| crate::daemon::run_claude_rm_in(account_for_rm, short_id),
    )
}
