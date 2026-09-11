//! `fno agents reap --release <row>` (x-e3cc): apply an operator ruling to
//! one escalated hold. The verb classifies with the dry run the report came
//! from, refuses a fresh hold or open work by name, and applies through the
//! sweep's own door: `run_with_release` runs the full real sweep with the
//! ruling, so other retire-eligible rows retire on it too and the receipt
//! prints every row's verdict.
//!
//! The refusals are the contract, never a downgrade:
//! - a row with no releasable hold (`is not held under a releasable reason`);
//! - a fresh hold (`below agents.hold_escalate_after_s`);
//! - a `sources disagree` witness that is not done (`a release never
//!   retires open work`).
//!
//! Split from `client.rs`, which is shrink-only, and beside `reap_render`,
//! for the same file-budget reason.

use crate::agents_config;
use crate::gc_sweep::{self, Release};
use crate::paths::AgentsHome;
use crate::state;
use std::path::Path;

/// Apply one release. Exits 2 on a named refusal, 0 on an applied ruling
/// (the receipt prints the whole sweep, other rows included).
pub fn run(home: &AgentsHome, cwd: &Path, handle: &str) -> i32 {
    let grace_secs = agents_config::retire_grace_secs(cwd) as i64;
    let escalate_after = agents_config::hold_escalate_after(cwd);
    // Classify with the same instrument the report came from.
    let mut dry = crate::gc::gc_sweep_dry_run(home, grace_secs);
    dry.mark_escalated(escalate_after);

    // Resolve the handle: exact hold id first, then the registry identities
    // (name, short id, session id) mapped onto the row handle.
    let hold = dry
        .holds
        .iter()
        .find(|h| h.id == handle)
        .or_else(|| {
            let row = load_registry_row(home, handle)?;
            let id = crate::gc::row_handle(&row);
            dry.holds.iter().find(|h| h.id == id)
        })
        .cloned();

    let Some(hold) = hold else {
        // No hold: name where the row actually sits, so the refusal is a
        // positive statement about the row's state, never a bare no.
        let bucket = row_bucket(home, cwd, grace_secs, handle);
        eprintln!("{handle} is not held under a releasable reason; it sits in {bucket}");
        return 2;
    };

    if !hold.escalated {
        eprintln!("{}", fresh_hold_refusal(&hold, escalate_after.as_secs()));
        return 2;
    }

    // A `sources disagree` release never retires open work: both witness
    // nodes must read done on the live graph.
    if hold.reason == "sources disagree" {
        if let Some(refusal) = witness_refusal(home, &hold) {
            eprintln!("{refusal}");
            return 2;
        }
    }

    let release = Release {
        handle: hold.id.clone(),
        reason: hold.reason.to_string(),
        detail: hold.detail.clone(),
    };
    let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "reap --release");
    let mut summary = crate::gc::gc_sweep_release(
        home,
        &emitter,
        grace_secs,
        agents_config::reap_receipt_retain_days(cwd),
        &release,
    );
    summary.mark_escalated(escalate_after);
    print!(
        "{}",
        crate::reap_render::render_reap(&summary, false, false)
    );
    0
}

/// The fresh-hold refusal line (x-e3cc): the age, the threshold, and what
/// a release needs before it applies.
pub(crate) fn fresh_hold_refusal(hold: &gc_sweep::Hold, threshold_s: u64) -> String {
    format!(
        "{} held {} under {}, below agents.hold_escalate_after_s {}; a fresh hold is not released",
        hold.id,
        crate::reap_render::human_duration(hold.age_s.unwrap_or(0)),
        hold.reason,
        threshold_s
    )
}

/// The witness refusal for a `sources disagree` release: a witness whose
/// node still carries ACTIVE work names itself and refuses. Done and the
/// inactive NODE statuses (deferred, idea, gc.rs INACTIVE_NODE_STATUSES)
/// are not open work - a node parked at deferred is exactly the shape the
/// x-2774 session-shaped releases already retire onto - so they release.
/// The witnesses ride the hold detail as `sessions <node> vs registry
/// <node>`.
pub(crate) fn witness_refusal(home: &AgentsHome, hold: &gc_sweep::Hold) -> Option<String> {
    let (a, b) = hold.detail.split_once(" vs ")?;
    let statuses = gc_sweep::read_graph_node_states(home);
    for side in [a, b] {
        let node = side.split_whitespace().last()?;
        let status = statuses
            .as_ref()
            .and_then(|s| s.get(node))
            .map(|(status, _)| status.as_str())
            .unwrap_or("unknown");
        let parked = crate::gc::INACTIVE_NODE_STATUSES.contains(&status);
        if status != "done" && !parked {
            return Some(format!(
                "{}: {node} reads {status}; a release never retires open work",
                hold.id
            ));
        }
    }
    None
}

/// The registry row a handle names: name, short id, or session id.
fn load_registry_row(home: &AgentsHome, handle: &str) -> Option<state::RegistryEntry> {
    let registry = state::load_registry(&home.registry_json()).ok()?;
    let handle_lc = handle.to_ascii_lowercase();
    registry.entries.into_iter().find(|e| {
        e.name.eq_ignore_ascii_case(&handle_lc)
            || e.short_id == handle
            || e.harness_session_id.as_deref() == Some(handle)
            || e.session_id.as_deref() == Some(handle)
    })
}

/// Where a row sits when it has no hold, by scanning the dry run's buckets.
fn row_bucket(home: &AgentsHome, cwd: &Path, grace_secs: i64, handle: &str) -> String {
    let dry = crate::gc::gc_sweep_dry_run(home, grace_secs);
    let in_plain = [
        dry.kept_operator,
        dry.kept_crowned,
        dry.kept_no_provenance,
        dry.kept_graph_unreadable,
    ]
    .iter()
    .any(|rows: &Vec<String>| rows.iter().any(|id| id == handle));
    if in_plain {
        return "a keep bucket this report names".to_string();
    }
    if dry.retired.iter().any(|(id, _)| id == handle) {
        return "retired".to_string();
    }
    if dry.kept_active.iter().any(|(id, _)| id == handle) {
        return "kept active".to_string();
    }
    if dry
        .kept_transcript_unresolved
        .iter()
        .any(|h| h.id == handle)
    {
        return "kept transcript unresolved".to_string();
    }
    if dry.kept_node_conflict.iter().any(|(id, _, _)| id == handle) {
        return "kept node conflict".to_string();
    }
    if dry.kept_open_do_row.iter().any(|(id, _)| id == handle) {
        return "kept open do row".to_string();
    }
    if dry.needs_live_stop.iter().any(|(id, _)| id == handle) {
        return "needs live stop".to_string();
    }
    if dry.stop_refused.iter().any(|(id, _)| id == handle) {
        return "stop refused".to_string();
    }
    if load_registry_row(home, handle).is_some() {
        "a keep bucket this report does not name".to_string()
    } else {
        "no registry row names this handle".to_string()
    }
}
