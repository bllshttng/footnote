//! (x-b64e) One classifier answers "can this persisted squad member come
//! back?", shared by the startup restore loop and the `workspace restore`
//! verb so both paths can never disagree about who is a corpse.

use std::collections::{HashMap, HashSet};

use crate::agents_view::RegistryAgent;
use crate::proto::AgentNoPaneReason;
use crate::spawn_journal::{receipt_for_member, HeldWorker};
use crate::squad_store::StoredMember;

/// What the evidence says about a persisted worker member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberVerdict {
    /// A registry row names it. An `exited` row counts: that is the dim
    /// resumable card, and this module does not touch it.
    Resumable,
    /// No registry row yet, but the spawn journal names it. A fresh spawn
    /// whose registry write has not landed must never be retired.
    RecentlySpawned,
    /// No registry row and no spawn receipt. Nothing can bring it back.
    Gone,
    /// The evidence could not be read (or the member carries no worker
    /// name and belongs to the attach lane). Retire nothing.
    Undecidable,
}

/// Classify a persisted member from the registry's name set and the spawn
/// journal. The fail-safe arms win first: an unreadable registry
/// (`known_workers` is `None`) is `Undecidable`, never `Gone`.
pub(crate) fn classify_member(
    member: &StoredMember,
    known_workers: Option<&HashSet<String>>,
    receipts: &HashMap<(String, String), HeldWorker>,
) -> MemberVerdict {
    let Some(worker) = member.worker.as_deref().filter(|w| !w.trim().is_empty()) else {
        // No worker name: a claude attach member, the other lane's problem.
        return MemberVerdict::Undecidable;
    };
    let Some(known) = known_workers else {
        return MemberVerdict::Undecidable;
    };
    if known.contains(worker) {
        return MemberVerdict::Resumable;
    }
    if receipt_for_member(receipts, member).is_some() {
        return MemberVerdict::RecentlySpawned;
    }
    MemberVerdict::Gone
}

/// (x-7b5e) Why a worker member cannot be held, in the member's own terms.
/// Moved verbatim from server.rs: pure over its inputs.
pub(crate) fn restore_worker_refusal_reason(
    member: &StoredMember,
    row: Option<&RegistryAgent>,
    receipt_store_error: Option<&str>,
    receipts: &HashMap<(String, String), HeldWorker>,
    never_bound: &HashMap<String, String>,
) -> String {
    if let Some(reason) = row
        .and_then(crate::server::Core::row_no_pane_reason)
        .map(crate::server::Core::no_pane_reason_text)
    {
        return reason.to_string();
    }
    let Some(session_id) = member.harness_session_id.as_deref() else {
        // (x-6b0b) No session id and no harness is the never-bound shape; when
        // the journal carries the name's removal marker, say why the member
        // can never bind instead of only that it did not.
        if member.harness.is_none() {
            if let Some(reason) = member.worker.as_deref().and_then(|w| never_bound.get(w)) {
                return format!("never bound: {reason}");
            }
        }
        return "session id is missing".into();
    };
    if let Some(error) = receipt_store_error {
        return error.to_string();
    }
    if let Some(receipt) = receipt_for_member(receipts, member) {
        let harness = member
            .harness
            .as_deref()
            .unwrap_or(receipt.harness.as_str());
        return format!("{harness} session {session_id} is not resumable");
    }
    let Some(harness) = member.harness.as_deref() else {
        return "harness is unknown".into();
    };
    if !crate::server::Core::resume_form(harness) {
        return no_resume_form_reason(harness, session_id);
    }
    format!("spawn receipt is missing for {harness} session {session_id}")
}

/// (x-7b5e) The one no-form refusal string, shared by the held-worker
/// restore reason and the bulk driver's report so the two surfaces cannot
/// teach different vocabularies for the same structural gap.
pub(crate) fn no_resume_form_reason(harness: &str, session_id: &str) -> String {
    format!("{harness} has no resume form; session {session_id} is not resumable")
}

/// Whether a registry row IS this member: exact harness + session id when
/// the member carries them, the plain name otherwise. Moved verbatim from
/// server.rs: pure over its inputs.
pub(crate) fn worker_registry_match(
    member: &StoredMember,
    agent: &RegistryAgent,
    worker_name: &str,
) -> bool {
    match (
        member.harness.as_deref(),
        member.harness_session_id.as_deref(),
    ) {
        (Some(harness), Some(session_id)) => {
            agent.harness.as_deref() == Some(harness)
                && crate::server::agent_harness_session_id(agent) == Some(session_id)
        }
        _ => agent.name == worker_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(worker: Option<&str>) -> StoredMember {
        StoredMember {
            attach_id: String::new(),
            tombstone: false,
            detached: false,
            tab_name: None,
            cwd: None,
            worker: worker.map(String::from),
            harness: None,
            harness_session_id: None,
        }
    }

    fn receipts() -> HashMap<(String, String), HeldWorker> {
        HashMap::new()
    }

    #[test]
    fn a_member_the_registry_and_the_journal_both_forgot_is_gone() {
        let known = HashSet::from([String::from("t-live")]);
        let m = member(Some("t-corpse"));
        assert_eq!(
            classify_member(&m, Some(&known), &receipts()),
            MemberVerdict::Gone
        );
    }

    #[test]
    fn an_unreadable_registry_is_undecidable_and_retires_nothing() {
        let m = member(Some("t-corpse"));
        assert_eq!(
            classify_member(&m, None, &receipts()),
            MemberVerdict::Undecidable
        );
    }

    #[test]
    fn a_listed_worker_is_resumable_even_without_a_session_identity() {
        let known = HashSet::from([String::from("t-live")]);
        let m = member(Some("t-live"));
        assert_eq!(
            classify_member(&m, Some(&known), &receipts()),
            MemberVerdict::Resumable
        );
    }

    #[test]
    fn an_unlisted_member_with_a_spawn_receipt_is_recently_spawned() {
        let known = HashSet::from([String::from("t-other")]);
        let mut rs = receipts();
        rs.insert(
            ("codex".into(), "fresh-session".into()),
            HeldWorker {
                name: "t-fresh".into(),
                harness: "codex".into(),
                harness_session_id: "fresh-session".into(),
                cwd: String::new(),
            },
        );
        let mut m = member(Some("t-fresh"));
        m.harness = Some("codex".into());
        m.harness_session_id = Some("fresh-session".into());
        assert_eq!(
            classify_member(&m, Some(&known), &rs),
            MemberVerdict::RecentlySpawned
        );
    }

    #[test]
    fn a_member_with_no_worker_name_is_undecidable_other_lane() {
        let known = HashSet::from([String::from("t-live")]);
        assert_eq!(
            classify_member(&member(None), Some(&known), &receipts()),
            MemberVerdict::Undecidable
        );
        assert_eq!(
            classify_member(&member(Some("   ")), Some(&known), &receipts()),
            MemberVerdict::Undecidable
        );
    }
}
