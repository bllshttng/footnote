//! Native retirement effects (x-70e1 task 3): the harness-native half of a
//! retirement, applied through the transports each harness already owns and
//! reported as typed per-effect outcomes.
//!
//! The sweep owns the sequence (stop, active-surface removal, registry drop,
//! tree prune); this module owns the ACTIVE-SURFACE removal - claude's agent
//! list, codex's session index, cursor-agent's detached worker servers,
//! opencode's active session listing - through the same cascade the `rm` verb
//! walks. A history deletion never happens here: the codex index drop keeps
//! the rollout files, the claude list removal keeps the transcript, and an
//! archive op (codex `thread/archive`, opencode `time.archived`) is
//! history-preserving by construction.
//!
//! opencode is the one arm that does NOT run through the shared cascade. The
//! cascade is also the `rm` verb's, and the deleted Python rm twin left an
//! opencode record alone on purpose. Archiving inside the cascade would move
//! one of those two legs and not the other, so the arm lives here, in the
//! retirement lane, which has no twin.
//!
//! Absence is only accepted after a complete enumeration of the exact
//! identity; a failed read is `Unverified` or `Failed`, never absence.

use crate::daemon::{cascade_harness_session_result_with, CascadeOutcome};
use crate::opencode_serve::ArchiveOutcome;
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

/// The typed effect record for the confirmed stop (x-5aef task 1.1). The
/// stop seam answers a bare bool, so the vocabulary is two-valued: a
/// confirmed stop reads `confirmed-removed`, anything else `failed` - and a
/// `failed` stop holds the row for retry, never retires it.
pub(crate) fn stop_outcome_effect(confirmed: bool) -> EffectRecord {
    EffectRecord {
        op: "native-stop".into(),
        outcome: if confirmed {
            "confirmed-removed".into()
        } else {
            "failed".into()
        },
        detail: None,
        at: crate::daemon::now_rfc3339_like(),
    }
}

/// Apply the ACTIVE-SURFACE removal for one row through the production
/// seams (the daemon roster read and `claude rm`), returning the typed
/// outcome the sweep records on the receipt. The roster read unions every
/// account root, and the removal is routed to the root where that read found
/// the row - absence from a single ambient read is a WRONG-ROOT absence and
/// has never been removal evidence.
pub(crate) fn apply_active_surface_removal(e: &RegistryEntry) -> CascadeOutcome {
    if e.harness_name() == "opencode" {
        return apply_opencode_archive(e);
    }
    // The snapshot is computed ONCE here and handed to the cascade, matching
    // the rm handler: the pre-check, the removal and the post-read must see
    // the same listing generation, and a claude arm without a snapshot is a
    // panic the caller cannot recover from.
    let snapshot = crate::claude_roster::read_all_agents_union();
    cascade_harness_session_result_with(
        e,
        Some(&snapshot),
        &crate::claude_roster::read_all_agents_union,
        &|short_id| {
            let dir = crate::claude_roster::removal_config_dir(
                &snapshot,
                short_id,
                e.launch_account.as_deref(),
            )?;
            crate::daemon::run_claude_rm_in(dir.as_deref(), short_id)
        },
    )
}

/// opencode's active-surface removal, wired to the production seams: the
/// recorded serve and the archive PATCH.
fn apply_opencode_archive(e: &RegistryEntry) -> CascadeOutcome {
    let serve = crate::paths::AgentsHome::from_env_opt()
        .and_then(|home| crate::opencode_serve::archive_capable_serve(&home))
        .map(|handle| (handle.base_url, handle.token));
    opencode_archive_outcome(
        e.harness_session_id.as_deref(),
        serve,
        &crate::opencode_serve::archive_session,
    )
}

/// Map one archive attempt onto the effect vocabulary.
///
/// Every skip answers `NotApplicable`, which is what an opencode row measured
/// before this op existed. That is deliberate: a missing serve or a serve too
/// old for the op is not evidence about the session, and answering `kept`
/// there would hold every opencode row on a machine that runs no serve.
///
/// A transport error is `Unverified` (the row comes back next sweep). A write
/// the server accepted and did not store is `Failed` - the same shape as a
/// claude row surviving a successful `claude rm`.
///
/// The id must be shape-valid before any request carries it, the same gate the
/// reachability probe applies before it reaches SQL. An id of another harness's
/// shape would 404 and read as `confirmed-already-absent`: a receipt claiming a
/// measured absence for a session this code never addressed.
pub(crate) fn opencode_archive_outcome(
    session_id: Option<&str>,
    serve: Option<(String, String)>,
    archive: &dyn Fn(&str, &str, &str) -> Result<ArchiveOutcome, String>,
) -> CascadeOutcome {
    let Some(sid) = session_id.filter(|s| crate::provider::is_opencode_session_id(s)) else {
        return CascadeOutcome::NotApplicable;
    };
    let Some((base_url, token)) = serve else {
        return CascadeOutcome::NotApplicable;
    };
    match archive(&base_url, &token, sid) {
        Ok(ArchiveOutcome::Archived) => CascadeOutcome::Removed,
        Ok(ArchiveOutcome::AlreadyArchived) => {
            CascadeOutcome::AlreadyAbsent(format!("opencode session {sid} was already archived"))
        }
        Ok(ArchiveOutcome::Gone) => CascadeOutcome::AlreadyAbsent(format!(
            "opencode session {sid} is absent from the store"
        )),
        Ok(ArchiveOutcome::Survived) => CascadeOutcome::Failed(format!(
            "opencode session {sid} is still unarchived after an accepted archive write"
        )),
        Err(reason) => CascadeOutcome::Unverified(reason),
    }
}
