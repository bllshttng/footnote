//! The stale-sweep test family, moved verbatim out of daemon.rs for file
//! budget (x-39f4): test motion is the sanctioned shrink. These tests pin
//! both state homes because a sweep that reads the operator's live pause
//! sentinel or fleet record flaps with machine timing.

use super::*;

/// Pin both homes away from the operator's live state for the duration of
/// `f`, passing it the sandbox agents home. The machine this suite runs
/// on may carry a real manual pause sentinel or fleet record; a sweep
/// test that reads them flaps with the operator's timing.
fn sandbox_pause_readers<R>(f: impl FnOnce(&std::path::Path) -> R) -> R {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let saved_home = std::env::var_os("HOME");
    let saved_agents = std::env::var_os("FNO_AGENTS_HOME");
    std::env::set_var("HOME", tmp.path());
    let agents = tmp.path().join("agents-home");
    std::fs::create_dir_all(&agents).unwrap();
    std::env::set_var("FNO_AGENTS_HOME", &agents);
    let out = f(&agents);
    match saved_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match saved_agents {
        Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
        None => std::env::remove_var("FNO_AGENTS_HOME"),
    }
    out
}

#[test]
fn stale_sweep_honours_its_own_6h_floor() {
    sandbox_pause_readers(|_| {
        let home = tmp_home("stale-sweep-floor");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let out = || {
            Some(
                r#"{"outcome": "asked", "question_id": "q-aa", "stale_count": 1, "oldest_h": 30, "summary": "Summary: 1 stale, outcome asked, oldest 30h"}"#
                    .to_string(),
            )
        };
        let now = 1_000_000;

        assert_eq!(stale_sweep(&home, &emitter, now, &out), 1);
        // Within the floor: skipped entirely, no second reading.
        assert_eq!(stale_sweep(&home, &emitter, now + 60, &out), 0);
        // Past the floor: fires again.
        assert_eq!(
            stale_sweep(&home, &emitter, now + STALE_SWEEP_INTERVAL_SECS + 1, &out),
            1
        );
    });
}

#[test]
fn stale_sweep_emits_on_a_quiet_run() {
    // A tick that stays silent when it finds nothing cannot be told from
    // a tick that never ran, and this lane exists precisely to prove the
    // sweep fires at all: outcome none still emits.
    sandbox_pause_readers(|_| {
        let home = tmp_home("stale-sweep-quiet");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let out = || {
            Some(
                r#"{"outcome": "none", "question_id": "", "stale_count": 0, "oldest_h": 0, "summary": "Summary: 0 stale, outcome none, oldest 0h"}"#
                    .to_string(),
            )
        };

        assert_eq!(stale_sweep(&home, &emitter, 1_000_000, &out), 1);
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(log.contains("stale_sweep"));
        assert!(log.contains("\"stale_count\":0"));
    });
}

#[test]
fn stale_sweep_records_an_unreadable_summary_rather_than_inventing_zeros() {
    sandbox_pause_readers(|_| {
        let home = tmp_home("stale-sweep-unreadable");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

        assert_eq!(stale_sweep(&home, &emitter, 1_000_000, &|| None), 0);
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(log.contains("unreadable-summary"));
        assert!(!log.contains("\"stale_count\""));
    });
}

/// x-39f4: while an effective dispatch pause holds, the due sweep never
/// calls the stale closure and never writes its cadence stamp; it emits a
/// positive skip row naming the pause. The positive control proves
/// recovery observation (the liveness planner) stays eligible in the same
/// paused state; on clear the overdue sweep runs exactly once.
#[test]
fn stale_sweep_suspends_without_consuming_cadence_while_dispatch_paused() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp_home("stale-sweep-paused");
    let saved_home = std::env::var_os("HOME");
    let saved_agents = std::env::var_os("FNO_AGENTS_HOME");
    std::env::set_var("HOME", tmp.path());
    let agents = tmp.path().join("agents-home");
    std::fs::create_dir_all(&agents).unwrap();
    std::env::set_var("FNO_AGENTS_HOME", &agents);
    let stamp = home.root().join("stale-escalate.stamp");

    // Stopped generation 5: the sweep is due but must not run.
    std::fs::write(
        crate::fleet_incident::fleet_stop_path(&crate::paths::AgentsHome::at(&agents)),
        serde_json::to_string(&crate::fleet_incident::IncidentRecord {
            version: crate::fleet_incident::STATE_VERSION,
            state: "stopped".into(),
            generation: 5,
            changed_at: "2026-09-13T01:07:00Z".into(),
            changed_by: "op".into(),
            reason: "load 385".into(),
            source: Some("file".into()),
        })
        .unwrap(),
    )
    .unwrap();
    let stale_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sc = Arc::clone(&stale_calls);
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let run = move || {
        sc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(
            r#"{"outcome": "asked", "question_id": "q-aa", "stale_count": 1, "oldest_h": 30, "summary": "Summary: 1 stale, outcome asked, oldest 30h"}"#
                .to_string(),
        )
    };

    let now = 1_000_000;
    assert_eq!(stale_sweep(&home, &emitter, now, &run), 0);
    assert_eq!(stale_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        !stamp.exists(),
        "a paused sweep must not consume its cadence"
    );
    let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(log.contains("\"outcome\":\"skipped\"") && log.contains("fleet_stop"));

    // Positive control: in the SAME paused state, the recovery observer
    // (liveness planner) still reconciles a reachable Orphaned row. The
    // entry needs short_id + pid so it reads as a PTY row, not a
    // one-shot ask (asks never reach the probe arm).
    let mut orphaned = crate::state::RegistryEntry::default();
    orphaned.name = "recovery-probe".into();
    orphaned.status = crate::AgentStatus::Orphaned;
    orphaned.short_id = "tp1".into();
    orphaned.pid = Some(4242);
    let (_, outcome) = crate::liveness_sweep::plan_reconcile(
        &[orphaned],
        |_| Ok(true),
        || false,
        |_| true,
        |_| false,
        |_| false,
        |_| false,
        |_| crate::client_verbs::RowLiveness::Unknown,
        true,
    );
    assert!(
        outcome.recovered.contains(&"recovery-probe".to_string()),
        "recovery observation must stay eligible while dispatch polls are held"
    );

    // Clear at generation 6: the overdue sweep runs exactly once.
    std::fs::write(
        crate::fleet_incident::fleet_stop_path(&crate::paths::AgentsHome::at(&agents)),
        serde_json::to_string(&crate::fleet_incident::IncidentRecord {
            version: crate::fleet_incident::STATE_VERSION,
            state: "clear".into(),
            generation: 6,
            changed_at: "2026-09-13T01:08:00Z".into(),
            changed_by: "op".into(),
            reason: "resolved".into(),
            source: Some("file".into()),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(stale_sweep(&home, &emitter, now + 120, &run), 1);
    assert_eq!(stale_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(stamp.exists(), "the clear run writes its normal stamp");

    match saved_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match saved_agents {
        Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
        None => std::env::remove_var("FNO_AGENTS_HOME"),
    }
}
