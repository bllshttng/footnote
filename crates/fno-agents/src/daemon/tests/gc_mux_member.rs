//! The `mux-member` effect inside `stage_session_retirement`.
//! The effect lands after active-surface and before resume-evidence; a
//! non-applied outcome rewrites the receipt and refuses, the same hold shape
//! the active-surface removal uses.

use crate::daemon::CascadeOutcome;
use crate::gc_sweep::{
    stage_session_retirement, RetireMode, RetireRefusal, StagedRetirement, StopObservation,
};
use crate::paths::AgentsHome;
use crate::receipt::{read_reap_receipt, ReapReceipt};
use crate::state::RegistryEntry;
use std::collections::BTreeMap;

fn temp_home(tag: &str) -> AgentsHome {
    let dir = tempfile::tempdir().unwrap();
    let home = AgentsHome::at(dir.path().join(format!("agents-{tag}")));
    home.ensure_root().unwrap();
    home
}

fn row(name: &str) -> RegistryEntry {
    serde_json::from_str(&format!(
        r#"{{"name":"{name}","short_id":"sid-{name}","harness":"claude","harness_session_id":"sess-{name}","cwd":"/tmp/x","created_at":"2026-09-01T00:00:00Z","status":"live"}}"#
    ))
    .unwrap()
}

fn stage(
    home: &AgentsHome,
    e: &RegistryEntry,
    mux_member: &dyn Fn(&RegistryEntry) -> CascadeOutcome,
) -> Result<StagedRetirement, RetireRefusal> {
    let mut receipts = BTreeMap::new();
    stage_session_retirement(
        home,
        e,
        None,
        RetireMode::Apply,
        StopObservation::Unproven,
        false,
        &|_| true,
        &|_, _| Ok(true),
        &|_| CascadeOutcome::Removed,
        mux_member,
        &mut receipts,
    )
}

/// The one receipt on disk, re-read AFTER the stage returned, so a refusal
/// rewrite is what the test sees.
fn sole_receipt(home: &AgentsHome) -> ReapReceipt {
    let dir = home.root().join("reap-receipts");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .expect("the receipt dir exists")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    assert_eq!(names.len(), 1, "exactly one receipt: {names:?}");
    read_reap_receipt(&names.remove(0)).expect("the staged receipt parses")
}

// AC1-HP: a live squad member and a Removed answer leave
// mux-member=confirmed-removed between active-surface and resume-evidence,
// and the stage returns Retired.
#[test]
fn ac1_hp_the_mux_member_effect_lands_between_surface_and_resume() {
    let home = temp_home("hp");
    let e = row("hp");
    let staged = stage(&home, &e, &|_| CascadeOutcome::Removed);
    assert!(
        matches!(staged, Ok(StagedRetirement::Retired)),
        "expected Retired"
    );
    let ops: Vec<String> = sole_receipt(&home)
        .effects
        .iter()
        .map(|effect| effect.op.clone())
        .collect();
    let mux = ops
        .iter()
        .position(|op| op == "mux-member")
        .expect("the receipt carries a mux-member op");
    let surface = ops
        .iter()
        .position(|op| op == "active-surface")
        .expect("the receipt carries active-surface");
    let resume = ops
        .iter()
        .position(|op| op == "resume-evidence")
        .expect("the receipt carries resume-evidence");
    assert!(
        surface < mux && mux < resume,
        "effect order must be surface < mux < resume: {ops:?}"
    );
    let effect = &sole_receipt(&home).effects[mux];
    assert_eq!(effect.outcome, "confirmed-removed", "{effect:?}");
}

// AC1-ERR: a Failed mux answer refuses with the named reason, rewrites the
// receipt with mux-member=failed, and never writes resume-evidence.
#[test]
fn ac1_err_a_failed_mux_answer_holds_the_row_and_rewrites_the_receipt() {
    let home = temp_home("err");
    let e = row("err");
    let staged = stage(&home, &e, &|_| {
        CascadeOutcome::Failed("mux server main unreachable".into())
    });
    match staged {
        Err(RetireRefusal::NativeRemoval(reason)) => {
            assert!(
                reason.contains("mux member retirement did not confirm"),
                "{reason}"
            );
        }
        Err(_) => panic!("expected NativeRemoval"),
        Ok(_) => panic!("expected NativeRemoval"),
    }
    let receipt = sole_receipt(&home);
    let mux = receipt
        .effects
        .iter()
        .find(|effect| effect.op == "mux-member")
        .expect("the refusal rewrote the receipt with the mux effect");
    assert_eq!(mux.outcome, "failed", "{mux:?}");
    assert!(
        !receipt
            .effects
            .iter()
            .any(|effect| effect.op == "resume-evidence"),
        "no resume evidence after a refused mux member: {receipt:?}"
    );
}

// AC1-EDGE at the stage level: a measured not-applicable (no live member, no
// ref) still retires and records the outcome.
#[test]
fn ac1_edge_a_not_applicable_mux_answer_still_retires() {
    let home = temp_home("edge");
    let e = row("edge");
    let staged = stage(&home, &e, &|_| CascadeOutcome::NotApplicable);
    assert!(
        matches!(staged, Ok(StagedRetirement::Retired)),
        "expected Retired"
    );
    let receipt = sole_receipt(&home);
    let mux = receipt
        .effects
        .iter()
        .find(|effect| effect.op == "mux-member")
        .expect("the receipt carries the mux-member op");
    assert_eq!(mux.outcome, "not-applicable", "{mux:?}");
}
