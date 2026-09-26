//! The blueprint retirement families: the probe-unread hold, the polled stop
//! confirmation's apply path, and the empty-planning-set basis. Split out of
//! `gc_receipts` (file budget); the shared fixtures resolve through it.

use super::*;
use super::{no_agents, quiet_transcript, stage_graph, staged_graph_home, uniform_ages};
use crate::gc_sweep::{self, GcSummary};

/// The crown specimen (2026-09-14): a do-phase claude row whose transcript
/// ends mid-Edit with the badge still `working` and live. A fresh working
/// report is a turn plausibly in flight, so it blocks the tick retirement
/// even with an unanswered probe and an unchanged seq.
#[test]
fn a_live_working_report_blocks_the_tick_retirement() {
    let (_dir, home) = staged_probe_unread_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = chrono::Utc::now();
    crate::state::update_registry(&home.registry_json(), |r| {
        if let Some(row) = r.entries.iter_mut().find(|e| e.name == "worker-x-u1") {
            let leg = row.inside_leg.as_mut().unwrap();
            leg.state = state::InsideLegState::Working;
            leg.ttl_ms = Some(90_000);
            leg.received_at = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        }
    })
    .unwrap();
    let calls = std::cell::Cell::new(0u32);
    let age_seam = |entries: &[&state::RegistryEntry]| {
        let n = calls.get();
        calls.set(n + 1);
        entries
            .iter()
            .map(|e| {
                let age = if n == 0 { Some(1500i64) } else { None };
                (crate::gc::row_handle(e), age)
            })
            .collect()
    };
    let summary = run_probe_unread_sweep(&home, &emitter, &age_seam);
    assert_eq!(summary.retired.len(), 0, "{:?}", summary.retired);
    let (id, detail) = summary
        .kept_probe_unread
        .iter()
        .find(|(id, _)| id == "worker-x-u1")
        .expect("the live working row holds");
    assert_eq!(id, "worker-x-u1");
    assert!(
        detail.contains("a live working report is on the row"),
        "{detail}"
    );
    assert!(summary.kept_active.is_empty(), "{:?}", summary.kept_active);
}

/// The same badge aged past its ttl is no longer authoritative: the quiet
/// witness lifts, and the row retires on the same sweep logic.
#[test]
fn an_expired_working_report_no_longer_blocks() {
    let (_dir, home) = staged_probe_unread_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let stale = chrono::Utc::now() - chrono::Duration::seconds(200);
    crate::state::update_registry(&home.registry_json(), |r| {
        if let Some(row) = r.entries.iter_mut().find(|e| e.name == "worker-x-u1") {
            let leg = row.inside_leg.as_mut().unwrap();
            leg.state = state::InsideLegState::Working;
            leg.ttl_ms = Some(90_000);
            leg.received_at = stale.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        }
    })
    .unwrap();
    let calls = std::cell::Cell::new(0u32);
    let age_seam = |entries: &[&state::RegistryEntry]| {
        let n = calls.get();
        calls.set(n + 1);
        entries
            .iter()
            .map(|e| {
                let age = if n == 0 { Some(1500i64) } else { None };
                (crate::gc::row_handle(e), age)
            })
            .collect()
    };
    let summary = run_probe_unread_sweep(&home, &emitter, &age_seam);
    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);
}

/// AC1-HP: a claude thread row classified would-retire, whose fresh
/// re-read answers NOTHING (the staged timeout), whose registry
/// `inside_leg.seq` is unchanged since classification, still stages: the
/// in-process quiet witness answers where the subprocess probe starved.
#[test]
fn an_unanswered_re_read_with_no_new_turn_still_stages_the_retirement() {
    let (_dir, home) = staged_probe_unread_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let calls = std::cell::Cell::new(0u32);
    let age_seam = |entries: &[&state::RegistryEntry]| {
        let n = calls.get();
        calls.set(n + 1);
        entries
            .iter()
            .map(|e| {
                let age = if n == 0 { Some(1500i64) } else { None };
                (crate::gc::row_handle(e), age)
            })
            .collect()
    };
    let summary = run_probe_unread_sweep(&home, &emitter, &age_seam);
    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);
    assert!(
        summary.kept_active.is_empty(),
        "never kept_active on a staged timeout: {:?}",
        summary.kept_active
    );
    assert!(
        summary.kept_probe_unread.is_empty(),
        "the witness answered, so no probe-unread hold: {:?}",
        summary.kept_probe_unread
    );
}

/// AC1-ERR: same row, but between classification and the re-read the
/// inside-leg seq ADVANCED (a new turn report landed). The row keeps in
/// `kept_probe_unread` with a `probe unread` hold naming the seq move; it
/// never appears in kept_active.
#[test]
fn an_unanswered_re_read_after_a_new_turn_holds_by_name() {
    let (_dir, home) = staged_probe_unread_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let reg = home.registry_json();
    let calls = std::cell::Cell::new(0u32);
    let age_seam = move |entries: &[&state::RegistryEntry]| {
        let n = calls.get();
        calls.set(n + 1);
        if n > 0 {
            // The staged timeout: and while the probe starves, a new turn
            // report lands - the seq advances.
            crate::state::update_registry(&reg, |r| {
                if let Some(row) = r.entries.iter_mut().find(|e| e.name == "worker-x-u1") {
                    if let Some(leg) = row.inside_leg.as_mut() {
                        leg.seq += 1;
                    }
                }
            })
            .unwrap();
        }
        entries
            .iter()
            .map(|e| {
                let age = if n == 0 { Some(1500i64) } else { None };
                (crate::gc::row_handle(e), age)
            })
            .collect()
    };
    let summary = run_probe_unread_sweep(&home, &emitter, &age_seam);
    assert_eq!(summary.retired.len(), 0, "{:?}", summary.retired);
    let (id, detail) = summary
        .kept_probe_unread
        .iter()
        .find(|(id, _)| id == "worker-x-u1")
        .expect("the row holds as probe unread");
    assert_eq!(id, "worker-x-u1");
    assert!(
        detail.contains("seq moved 4 -> 5"),
        "the detail names the seq change: {detail}"
    );
    let hold = summary
        .holds
        .iter()
        .find(|h| h.id == "worker-x-u1")
        .expect("a hold rides the bucket");
    assert_eq!(hold.reason, "probe unread");
    assert!(summary.kept_active.is_empty(), "{:?}", summary.kept_active);
}

/// AC1-EDGE: a row with no inside-leg report whose fresh re-read
/// answers nothing holds in `kept_probe_unread` - never kept_active, never
/// an age of 0.
#[test]
fn a_row_with_no_inside_leg_holds_on_an_unanswered_re_read() {
    let (_dir, home) = staged_probe_unread_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    // Strip the inside-leg report the fixture row carries.
    crate::state::update_registry(&home.registry_json(), |r| {
        for row in r.entries.iter_mut() {
            row.inside_leg = None;
        }
    })
    .unwrap();
    let calls = std::cell::Cell::new(0u32);
    let age_seam = |entries: &[&state::RegistryEntry]| {
        let n = calls.get();
        calls.set(n + 1);
        entries
            .iter()
            .map(|e| {
                let age = if n == 0 { Some(1500i64) } else { None };
                (crate::gc::row_handle(e), age)
            })
            .collect()
    };
    let summary = run_probe_unread_sweep(&home, &emitter, &age_seam);
    assert_eq!(summary.retired.len(), 0, "{:?}", summary.retired);
    let (_id, detail) = summary
        .kept_probe_unread
        .iter()
        .find(|(id, _)| id == "worker-x-u1")
        .expect("the row holds as probe unread");
    assert!(
        detail.contains("no inside-leg report on the row"),
        "{detail}"
    );
    assert!(
        summary.kept_active.is_empty(),
        "an invented age 0 never lands in kept_active: {:?}",
        summary.kept_active
    );
    assert!(summary.holds.iter().any(|h| h.id == "worker-x-u1"));
}

/// AC4-EDGE: a `bp-` row whose planning set is EMPTY retires through
/// the session release - the basis uses the session arm, never the planning
/// wording. The `released` spelling is reserved for planning_released.
#[test]
fn a_bp_row_with_an_empty_planning_set_retires_on_the_session_basis() {
    use crate::daemon::CascadeOutcome;

    let (dir, home) = staged_graph_home();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    // The node exists but names no sessions row for s-rel, so the reverse
    // join gives the bp row an EMPTY assignment set; the name route still
    // resolves the node.
    stage_graph(
        dir.path(),
        json!([{
            "id": "x-a75f",
            "status": "idea",
            "project": "p",
        }]),
    );
    crate::state::update_registry(&home.registry_json(), |r| {
        let mut e = state::RegistryEntry::default();
        e.name = "bp-x-a75f-repro".into();
        e.short_id = "bp-x-a75f-repro".into();
        e.origin = Some("spawn".into());
        e.harness = Some("codex".into());
        e.harness_session_id = Some("s-rel".into());
        e.created_at = "2026-09-01T00:00:00Z".into();
        r.entries.push(e);
    })
    .unwrap();
    let store = home.root().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let quiet = quiet_transcript(&store, "q.jsonl", 2 * 3600);
    let summary = gc_sweep::run(
        &home,
        &emitter,
        900,
        false,
        7,
        &crate::gc_sweep::read_graph_entries,
        &move |_| Some(vec![quiet.clone()]),
        &uniform_ages(2 * 3600),
        &|_| true,
        &|_| CascadeOutcome::Removed,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (Some(true), Some(true)),
        &|_| None,
    );
    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);
    assert!(
        summary.retired[0]
            .1
            .contains("node x-a75f is idea, not active work"),
        "{:?}",
        summary.retired[0].1
    );
    assert!(
        !summary.retired[0].1.contains("released"),
        "the planning wording stays reserved: {:?}",
        summary.retired[0].1
    );
}

/// The AC1 fixture: one claude thread row (`pid` null is the
/// default), `inside_leg` present, staged on a done node so classification
/// answers would-retire on its quiet age.
fn staged_probe_unread_home() -> (tempfile::TempDir, AgentsHome) {
    let (dir, home) = staged_graph_home();
    stage_graph(
        dir.path(),
        json!([{
            "id": "x-u1",
            "status": "done",
            "sessions": [{
                "phase": "execute",
                "harness": "claude",
                "session_id": "s-u1",
                "started_at": "2026-09-01T00:00:00Z",
                "ended_at": "2026-09-01T01:00:00Z",
            }],
        }]),
    );
    crate::state::update_registry(&home.registry_json(), |r| {
        let mut e = state::RegistryEntry::default();
        e.name = "worker-x-u1".into();
        e.short_id = "worker-x-u1".into();
        e.origin = Some("spawn".into());
        e.harness = Some("claude".into());
        e.harness_session_id = Some("s-u1".into());
        e.created_at = "2026-09-01T00:00:00Z".into();
        e.inside_leg = Some(state::InsideLegReport {
            state: state::InsideLegState::Done,
            seq: 4,
            reason: None,
            received_at: "2026-09-01T00:00:00Z".into(),
            ttl_ms: None,
        });
        r.entries.push(e);
    })
    .unwrap();
    (dir, home)
}

/// The AC1 sweep shape: grace 900, apply mode, a stop seam that confirms,
/// and the age seam handed in per test (batch answers, single re-read
/// stages the timeout).
fn run_probe_unread_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    age_seam: &dyn Fn(&[&state::RegistryEntry]) -> std::collections::HashMap<String, Option<i64>>,
) -> GcSummary {
    gc_sweep::run(
        home,
        emitter,
        900,
        false,
        7,
        &crate::gc_sweep::read_graph_entries,
        &|_| Some(vec![]),
        age_seam,
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::Removed,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (Some(true), Some(true)),
        &|_| None,
    )
}
