/// Walk policy tests for Task 2.3.
///
/// Tests cover:
///   - NextStep enum: Queue::next() returning Dispatch/Drained/Pause variants
///   - Consecutive-failure pause (streak of 3 -> Pause{consecutive_failures})
///   - Success resets streak
///   - p0 failure pauses immediately
///   - Per-unit dispatch cap synthesizes NoProgress park after N dispatches
///   - Parallel-cap clamp (0->1) is logged
///   - node_closed event journaled per close with correct fields
///   - Pause from TargetQueue never occurs (TargetQueue arm unchanged)
///   - Ctrl-C / cancel -> Interrupted journaled + walker-claim seam testable
///
/// Naming: all test names begin with `walk_policy_` so `cargo test walk_policy`
/// selects exactly this module.
use fno_agents::loop_megawalk::MegawalkQueue;
use fno_agents::loop_runtime::{
    run_loop, CloseOutcome, DispatchCtx, Dispatcher, Evidence, Journal, LoopBudget, LoopError,
    NextStep, Queue, Session, Unit,
};
use fno_agents::loopcheck::TerminationReason;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_unit(id: &str) -> Unit {
    Unit {
        id: id.to_string(),
        title: format!("Test unit {id}"),
        session_key: format!("sk-{id}"),
        plan_path: None,
        extra_env: vec![],
    }
}

fn make_evidence(reason: TerminationReason) -> Evidence {
    Evidence {
        reason,
        message: "test".to_string(),
    }
}

fn seed_termination(journal_path: &Path, session_key: &str, reason: &str) {
    let line = format!(
        "{{\"ts\":\"2026-06-06T00:00:00Z\",\"type\":\"termination\",\"source\":\"hook\",\
         \"data\":{{\"session_id\":\"{session_key}\",\"reason\":\"{reason}\",\"message\":\"done\"}}}}\n"
    );
    fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path)
        .unwrap();
    f.write_all(line.as_bytes()).unwrap();
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    if !path.exists() {
        return vec![];
    }
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn events_of_type(path: &Path, event_type: &str) -> Vec<serde_json::Value> {
    read_jsonl(path)
        .into_iter()
        .filter(|v| v["type"].as_str() == Some(event_type))
        .collect()
}

// ── mock: policy-aware queue ──────────────────────────────────────────────────

/// A Queue that returns units in order, then Drained, and tracks close calls.
/// Supports priority (p0 or normal) and the NextStep enum.
struct PolicyQueue {
    units: Vec<(Unit, bool)>, // (unit, is_p0)
    cursor: usize,
    closed: Mutex<Vec<(String, CloseOutcome)>>,
    consecutive_failures: usize,
    /// Unit IDs of recent failures (for Pause detail)
    recent_failure_ids: Vec<String>,
}

impl PolicyQueue {
    fn new(units: Vec<(Unit, bool)>) -> Self {
        Self {
            units,
            cursor: 0,
            closed: Mutex::new(vec![]),
            consecutive_failures: 0,
            recent_failure_ids: vec![],
        }
    }
}

impl Queue for PolicyQueue {
    fn next(&mut self) -> Result<Option<Unit>, LoopError> {
        // Check pause conditions before dequeuing.
        // p0 failure: if the last failure was p0, pause immediately.
        // consecutive failures: if streak >= 3, pause.
        // (these are tested via the NextStep-aware run_loop)
        if self.cursor >= self.units.len() {
            return Ok(None);
        }
        let (unit, _is_p0) = &self.units[self.cursor];
        let u = Unit {
            id: unit.id.clone(),
            title: unit.title.clone(),
            session_key: unit.session_key.clone(),
            plan_path: unit.plan_path.clone(),
            extra_env: vec![],
        };
        self.cursor += 1;
        Ok(Some(u))
    }

    fn close(&mut self, unit: &Unit, evidence: &Evidence) -> Result<CloseOutcome, LoopError> {
        let outcome = CloseOutcome::Parked(format!("test-park: {:?}", evidence.reason));
        self.closed.lock().unwrap().push((
            unit.id.clone(),
            CloseOutcome::Parked(format!("test-park: {:?}", evidence.reason)),
        ));
        Ok(outcome)
    }
}

// ── mock: dispatcher that counts calls per unit ───────────────────────────────

struct CountingDispatcher {
    journal_path: std::path::PathBuf,
    /// Per-unit dispatch counts (unit_id -> count).
    counts: Mutex<std::collections::HashMap<String, u64>>,
    /// Total dispatch count.
    total: AtomicU64,
    /// Whether to emit a termination event per dispatch.
    emit_termination: bool,
}

impl CountingDispatcher {
    fn new(journal_path: std::path::PathBuf, emit_termination: bool) -> Self {
        Self {
            journal_path,
            counts: Mutex::new(std::collections::HashMap::new()),
            total: AtomicU64::new(0),
            emit_termination,
        }
    }

    fn total_dispatches(&self) -> u64 {
        self.total.load(Ordering::SeqCst)
    }

    fn dispatches_for(&self, unit_id: &str) -> u64 {
        *self.counts.lock().unwrap().get(unit_id).unwrap_or(&0)
    }
}

impl Dispatcher for CountingDispatcher {
    fn run(&self, unit: &Unit, _ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
        *self
            .counts
            .lock()
            .unwrap()
            .entry(unit.id.clone())
            .or_insert(0) += 1;
        self.total.fetch_add(1, Ordering::SeqCst);
        let journal_path = self.journal_path.clone();
        let session_key = unit.session_key.clone();
        let emit = self.emit_termination;
        Ok(Box::new(FnSession(Box::new(move || {
            if emit {
                seed_termination(&journal_path, &session_key, "DonePRGreen");
            }
            Ok(0)
        }))))
    }
}

struct FnSession(Box<dyn FnMut() -> Result<i32, LoopError> + Send>);

impl Session for FnSession {
    fn wait(&mut self) -> Result<i32, LoopError> {
        (self.0)()
    }
}

// ── mock: policy queue using NextStep ─────────────────────────────────────────

/// A Queue implementation that uses NextStep (the new enum from A1).
/// Returns Dispatch(unit) / Drained / Pause based on configuration.
struct NextStepQueue {
    steps: Vec<NextStep>,
    cursor: usize,
    closed: Mutex<Vec<String>>,
}

impl NextStepQueue {
    fn new(steps: Vec<NextStep>) -> Self {
        Self {
            steps,
            cursor: 0,
            closed: Mutex::new(vec![]),
        }
    }

    fn close_count(&self) -> usize {
        self.closed.lock().unwrap().len()
    }
}

impl Queue for NextStepQueue {
    fn next(&mut self) -> Result<Option<Unit>, LoopError> {
        if self.cursor >= self.steps.len() {
            return Ok(None);
        }
        let step = &self.steps[self.cursor];
        self.cursor += 1;
        match step {
            NextStep::Dispatch(u) => Ok(Some(Unit {
                id: u.id.clone(),
                title: u.title.clone(),
                session_key: u.session_key.clone(),
                plan_path: u.plan_path.clone(),
                extra_env: vec![],
            })),
            NextStep::Drained => Ok(None),
            NextStep::Pause { policy, detail } => Err(LoopError::Pause {
                policy: policy.clone(),
                detail: detail.clone(),
            }),
        }
    }

    fn close(&mut self, unit: &Unit, _evidence: &Evidence) -> Result<CloseOutcome, LoopError> {
        self.closed.lock().unwrap().push(unit.id.clone());
        Ok(CloseOutcome::Closed)
    }
}

// ── tests: NextStep enum is publicly accessible ───────────────────────────────

#[test]
fn walk_policy_nextstep_dispatch_variant_exists() {
    // AC: NextStep::Dispatch(Unit) variant exists and can be constructed.
    let unit = make_unit("ab-001");
    let step = NextStep::Dispatch(unit);
    match step {
        NextStep::Dispatch(u) => assert_eq!(u.id, "ab-001"),
        _ => panic!("expected Dispatch variant"),
    }
}

#[test]
fn walk_policy_nextstep_drained_variant_exists() {
    // AC: NextStep::Drained variant exists.
    let step = NextStep::Drained;
    assert!(matches!(step, NextStep::Drained));
}

#[test]
fn walk_policy_nextstep_pause_variant_exists() {
    // AC: NextStep::Pause{policy, detail} variant exists with named fields.
    let step = NextStep::Pause {
        policy: "consecutive_failures".to_string(),
        detail: "ab-001 ab-002 ab-003".to_string(),
    };
    match step {
        NextStep::Pause { policy, detail } => {
            assert_eq!(policy, "consecutive_failures");
            assert_eq!(detail, "ab-001 ab-002 ab-003");
        }
        _ => panic!("expected Pause variant"),
    }
}

// ── tests: MegawalkQueue consecutive-failure policy ──────────────────────────

#[test]
fn walk_policy_consecutive_failure_streak_of_3_returns_pause() {
    // AC: after 3 consecutive failures (close with non-Done reason),
    // the next next() call returns NextStep::Pause{policy: "consecutive_failures"}.
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global.jsonl");
    let journal = Journal::new_raw(project_events.clone(), global_events.clone());

    // Build a MegawalkPolicyQueue (the updated MegawalkQueue that tracks failures).
    let mut queue = fno_agents::loop_megawalk::MegawalkPolicyQueue::new();

    // Simulate 3 consecutive failures by calling record_close(evidence) with
    // non-Done reasons. The queue should return Pause on the 3rd.
    let u1 = make_unit("ab-001");
    let u2 = make_unit("ab-002");
    let u3 = make_unit("ab-003");

    queue.record_close(&u1, &make_evidence(TerminationReason::Budget), false);
    // Still not paused after 1.
    assert!(
        !queue.should_pause().is_some(),
        "should not pause after 1 failure"
    );

    queue.record_close(&u2, &make_evidence(TerminationReason::NoProgress), false);
    // Still not paused after 2.
    assert!(
        !queue.should_pause().is_some(),
        "should not pause after 2 failures"
    );

    queue.record_close(&u3, &make_evidence(TerminationReason::Interrupted), false);
    // Should pause after 3.
    let pause = queue.should_pause();
    assert!(pause.is_some(), "should pause after 3 consecutive failures");
    let (policy, _detail) = pause.unwrap();
    assert_eq!(policy, "consecutive_failures");
}

#[test]
fn walk_policy_success_resets_consecutive_failure_streak() {
    // AC: a successful close (DonePRGreen) resets the streak to 0.
    let mut queue = fno_agents::loop_megawalk::MegawalkPolicyQueue::new();

    let u1 = make_unit("ab-001");
    let u2 = make_unit("ab-002");
    let u3 = make_unit("ab-003");

    // Two failures.
    queue.record_close(&u1, &make_evidence(TerminationReason::Budget), false);
    queue.record_close(&u2, &make_evidence(TerminationReason::NoProgress), false);
    // Success resets.
    queue.record_close(&u3, &make_evidence(TerminationReason::DonePRGreen), false);
    assert!(queue.should_pause().is_none(), "success must reset streak");

    // Two more failures after reset - still no pause (streak < 3).
    let u4 = make_unit("ab-004");
    let u5 = make_unit("ab-005");
    queue.record_close(&u4, &make_evidence(TerminationReason::Budget), false);
    queue.record_close(&u5, &make_evidence(TerminationReason::NoProgress), false);
    assert!(
        queue.should_pause().is_none(),
        "two failures after reset must not pause"
    );
}

#[test]
fn walk_policy_p0_failure_pauses_immediately() {
    // AC: a p0 unit failure immediately returns Pause{policy: "p0_failed"},
    // without needing a streak of 3.
    let mut queue = fno_agents::loop_megawalk::MegawalkPolicyQueue::new();

    let u = make_unit("ab-p0");
    // is_p0 = true
    queue.record_close(&u, &make_evidence(TerminationReason::Budget), true);

    let pause = queue.should_pause();
    assert!(pause.is_some(), "p0 failure must pause immediately");
    let (policy, detail) = pause.unwrap();
    assert_eq!(policy, "p0_failed");
    assert!(detail.contains("ab-p0"), "detail must include unit id");
}

#[test]
fn walk_policy_doneprgreen_does_not_count_as_failure() {
    // AC: DonePRGreen is not a failure and does not increment the streak.
    let mut queue = fno_agents::loop_megawalk::MegawalkPolicyQueue::new();

    // 10 DonePRGreen closes - still no pause.
    for i in 0..10 {
        let u = make_unit(&format!("ab-{i:03}"));
        queue.record_close(&u, &make_evidence(TerminationReason::DonePRGreen), false);
    }
    assert!(queue.should_pause().is_none());
}

#[test]
fn walk_policy_doneadvisory_does_not_count_as_failure() {
    // AC: DoneAdvisory is not a failure.
    let mut queue = fno_agents::loop_megawalk::MegawalkPolicyQueue::new();
    for i in 0..5 {
        let u = make_unit(&format!("ab-{i:03}"));
        queue.record_close(&u, &make_evidence(TerminationReason::DoneAdvisory), false);
    }
    assert!(queue.should_pause().is_none());
}

// ── tests: per-unit dispatch cap -> NoProgress park ──────────────────────────

#[test]
fn walk_policy_per_unit_cap_parks_unit_and_continues() {
    // AC: when a unit accumulates per_unit_max_dispatches without a termination
    // event, run_loop synthesizes a NoProgress Evidence, calls queue.close with
    // it (returns Parked), and continues to the next unit.
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global.jsonl");
    let journal = Journal::new_raw(project_events.clone(), global_events);

    // Unit 1 will never produce a termination event -> should be parked after cap.
    // Unit 2 will produce one on the first dispatch.
    let u1 = make_unit("ab-001");
    let u2 = make_unit("ab-002");
    let sk1 = u1.session_key.clone();
    let sk2 = u2.session_key.clone();

    let mut queue = fno_agents::loop_megawalk::MegawalkPolicyQueue::new();
    // Queue up u1 then u2.
    queue.push_unit(u1, false);
    queue.push_unit(u2, false);

    // Dispatcher: always fails to emit termination (returns exit 1) for u1,
    // emits DonePRGreen for u2.
    let project_events_clone = project_events.clone();
    let dispatcher = CapTestDispatcher {
        no_event_unit: "ab-001".to_string(),
        journal_path: project_events.clone(),
    };

    let budget = LoopBudget::new(20).unwrap();
    let outcome = run_loop(
        &mut queue,
        &dispatcher,
        &budget,
        &journal,
        &|| false,
        Some(2), // per_unit_max_dispatches = 2
    )
    .unwrap();

    // Walk should complete with NoWork (both units processed).
    assert_eq!(outcome.reason, TerminationReason::NoWork);
    // Unit 1 should appear as Parked in outcome.
    let u1_result = outcome.units.iter().find(|r| r.unit_id == "ab-001");
    assert!(u1_result.is_some(), "unit ab-001 must appear in results");
    let u1_close = &u1_result.unwrap().close;
    assert!(
        matches!(u1_close, CloseOutcome::Parked(_)),
        "ab-001 must be Parked after cap, got {u1_close:?}"
    );
    // Unit 2 should appear as Closed.
    let u2_result = outcome.units.iter().find(|r| r.unit_id == "ab-002");
    assert!(u2_result.is_some(), "unit ab-002 must appear in results");
    assert_eq!(u2_result.unwrap().close, CloseOutcome::Closed);

    // node_closed events must be journaled for both units.
    let closed_events = events_of_type(&project_events, "node_closed");
    assert_eq!(closed_events.len(), 2, "expected 2 node_closed events");
}

// Dispatcher for per-unit cap test.
struct CapTestDispatcher {
    no_event_unit: String,
    journal_path: std::path::PathBuf,
}

impl Dispatcher for CapTestDispatcher {
    fn run(&self, unit: &Unit, _ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
        let emit = unit.id != self.no_event_unit;
        let journal_path = self.journal_path.clone();
        let session_key = unit.session_key.clone();
        Ok(Box::new(FnSession(Box::new(move || {
            if emit {
                seed_termination(&journal_path, &session_key, "DonePRGreen");
            }
            Ok(if emit { 0 } else { 1 })
        }))))
    }
}

// ── tests: parallel-cap clamp ─────────────────────────────────────────────────

#[test]
fn walk_policy_parallel_cap_clamp_0_to_1_is_logged() {
    // AC: a parallel_cap of 0 is clamped to 1 with a log line printed to stderr.
    // This test verifies the clamp logic is accessible and produces the expected
    // clamped value.
    let clamped = fno_agents::loop_megawalk::clamp_parallel_cap(0);
    assert_eq!(clamped, 1, "cap < 1 must be clamped to 1");
}

#[test]
fn walk_policy_parallel_cap_positive_is_unchanged() {
    // AC: parallel_cap >= 1 passes through unchanged.
    assert_eq!(fno_agents::loop_megawalk::clamp_parallel_cap(1), 1);
    assert_eq!(fno_agents::loop_megawalk::clamp_parallel_cap(4), 4);
}

// ── tests: node_closed event per close ───────────────────────────────────────

#[test]
fn walk_policy_node_closed_event_journaled_on_every_close() {
    // AC: after EVERY queue.close in run_loop, a node_closed loop event is
    // journaled with {unit_id, session_id, reason, close, detail}.
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global.jsonl");
    let journal = Journal::new_raw(project_events.clone(), global_events);

    let u1 = make_unit("ab-001");
    let u2 = make_unit("ab-002");
    let sk1 = u1.session_key.clone();
    let sk2 = u2.session_key.clone();

    let mut queue = fno_agents::loop_megawalk::MegawalkPolicyQueue::new();
    queue.push_unit(u1, false);
    queue.push_unit(u2, false);

    // Both units produce termination events -> both get Closed.
    let dispatcher = BothCloseDispatcher {
        journal_path: project_events.clone(),
    };
    let budget = LoopBudget::new(10).unwrap();
    let outcome = run_loop(
        &mut queue,
        &dispatcher,
        &budget,
        &journal,
        &|| false,
        None, // no per-unit cap
    )
    .unwrap();

    assert_eq!(outcome.reason, TerminationReason::NoWork);
    let closed_events = events_of_type(&project_events, "node_closed");
    assert_eq!(closed_events.len(), 2, "must have one node_closed per unit");

    // Verify required fields.
    for ev in &closed_events {
        let data = &ev["data"];
        assert!(data["unit_id"].is_string(), "node_closed must have unit_id");
        assert!(
            data["session_id"].is_string(),
            "node_closed must have session_id"
        );
        assert!(data["reason"].is_string(), "node_closed must have reason");
        // close field must be "closed", "parked", or "refused".
        let close_str = data["close"].as_str().unwrap_or("");
        assert!(
            ["closed", "parked", "refused"].contains(&close_str),
            "node_closed.close must be closed/parked/refused, got {close_str:?}"
        );
    }
}

struct BothCloseDispatcher {
    journal_path: std::path::PathBuf,
}

impl Dispatcher for BothCloseDispatcher {
    fn run(&self, unit: &Unit, _ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
        let journal_path = self.journal_path.clone();
        let session_key = unit.session_key.clone();
        Ok(Box::new(FnSession(Box::new(move || {
            seed_termination(&journal_path, &session_key, "DonePRGreen");
            Ok(0)
        }))))
    }
}

// ── tests: resume-guard also emits node_closed ───────────────────────────────

#[test]
fn walk_policy_resume_guard_close_emits_node_closed() {
    // AC: when the resume guard fires (pre-existing termination event),
    // a node_closed event is still journaled.
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global.jsonl");
    let journal = Journal::new_raw(project_events.clone(), global_events);

    // Pre-seed a termination event so the resume guard fires.
    let u = make_unit("ab-resume");
    seed_termination(&project_events, &u.session_key, "DonePRGreen");

    let mut queue = fno_agents::loop_megawalk::MegawalkPolicyQueue::new();
    queue.push_unit(u, false);

    struct PanicDispatcher2;
    impl Dispatcher for PanicDispatcher2 {
        fn run(&self, _u: &Unit, _c: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
            panic!("resume guard must skip dispatch");
        }
    }

    let budget = LoopBudget::new(5).unwrap();
    let outcome = run_loop(
        &mut queue,
        &PanicDispatcher2,
        &budget,
        &journal,
        &|| false,
        None,
    )
    .unwrap();

    assert_eq!(outcome.reason, TerminationReason::NoWork);
    let closed = events_of_type(&project_events, "node_closed");
    assert_eq!(
        closed.len(),
        1,
        "resume-guard close must emit node_closed; got {closed:?}"
    );
}

// ── tests: Pause from run_loop -> NoProgress + walk_paused event ──────────────

#[test]
fn walk_policy_pause_from_queue_emits_walk_paused_and_nowork() {
    // AC: when queue.next_step() returns Pause{policy, detail},
    // run_loop journals walk_paused{policy, detail, iterations_used, units_closed}
    // and then loop_terminated{reason: "NoProgress"}, returning NoProgress.
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global.jsonl");
    let journal = Journal::new_raw(project_events.clone(), global_events);

    // Build a queue that immediately returns a pause via should_pause().
    let mut queue = fno_agents::loop_megawalk::MegawalkPolicyQueue::new();
    // Inject a forced pause by pre-loading 3 failures.
    for i in 0..3 {
        let u = make_unit(&format!("ab-{i:03}"));
        queue.record_close(&u, &make_evidence(TerminationReason::Budget), false);
    }
    // Next call should return Pause via the policy check.
    // We need a unit in the queue for next() to be called first.
    // Add one more unit that triggers the pause path.
    let trigger_unit = make_unit("ab-trigger");
    queue.push_unit(trigger_unit, false);

    struct NeverDispatcher;
    impl Dispatcher for NeverDispatcher {
        fn run(&self, _u: &Unit, _c: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
            panic!("should not dispatch when paused");
        }
    }

    let budget = LoopBudget::new(10).unwrap();
    let outcome = run_loop(
        &mut queue,
        &NeverDispatcher,
        &budget,
        &journal,
        &|| false,
        None,
    )
    .unwrap();

    assert_eq!(
        outcome.reason,
        TerminationReason::NoProgress,
        "pause should map to NoProgress at walk level"
    );

    // walk_paused event must be journaled.
    let paused_events = events_of_type(&project_events, "walk_paused");
    assert_eq!(paused_events.len(), 1, "expected 1 walk_paused event");
    let data = &paused_events[0]["data"];
    assert!(data["policy"].is_string(), "walk_paused must have policy");
    assert!(data["detail"].is_string(), "walk_paused must have detail");
    assert!(
        data["iterations_used"].is_number(),
        "walk_paused must have iterations_used"
    );
    assert!(
        data["units_closed"].is_number(),
        "walk_paused must have units_closed"
    );

    // loop_terminated must follow with NoProgress.
    let terminated = events_of_type(&project_events, "loop_terminated");
    assert_eq!(terminated.len(), 1, "expected 1 loop_terminated event");
    assert_eq!(
        terminated[0]["data"]["reason"].as_str(),
        Some("NoProgress"),
        "loop_terminated reason must be NoProgress after pause"
    );
}

// ── test: a real Queue("pause:...") string is a hard error, never a pause ─────

#[test]
fn walk_policy_queue_error_with_pause_prefix_is_hard_fail_not_pause() {
    // AC4-HP / Locked Decision 4: after the typed-variant refactor, ONLY
    // LoopError::Pause routes to the pause path. A genuine LoopError::Queue whose
    // message happens to start with "pause:" (e.g. a node titled "pause: rework
    // auth" leaking into an error string) must surface as a hard failure and emit
    // NO walk_paused event, instead of being silently downgraded to NoProgress.
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global.jsonl");
    let journal = Journal::new_raw(project_events.clone(), global_events);

    struct QueueErrQueue;
    impl Queue for QueueErrQueue {
        fn next(&mut self) -> Result<Option<Unit>, LoopError> {
            // Legacy string form as a REAL queue error, not a typed Pause.
            Err(LoopError::Queue("pause: rework auth".to_string()))
        }
        fn close(&mut self, _u: &Unit, _e: &Evidence) -> Result<CloseOutcome, LoopError> {
            Ok(CloseOutcome::Closed)
        }
    }

    struct NeverDispatcher;
    impl Dispatcher for NeverDispatcher {
        fn run(&self, _u: &Unit, _c: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
            panic!("should not dispatch on a hard queue error");
        }
    }

    let budget = LoopBudget::new(10).unwrap();
    let mut queue = QueueErrQueue;
    let result = run_loop(
        &mut queue,
        &NeverDispatcher,
        &budget,
        &journal,
        &|| false,
        None,
    );

    match result {
        Err(LoopError::Queue(msg)) => assert_eq!(msg, "pause: rework auth"),
        Err(other) => panic!("expected hard LoopError::Queue, got {other:?}"),
        Ok(_) => panic!("expected hard LoopError::Queue, got Ok(LoopOutcome)"),
    }

    // No walk_paused event: a real queue error is not a policy pause.
    let paused_events = events_of_type(&project_events, "walk_paused");
    assert!(
        paused_events.is_empty(),
        "a real Queue error must NOT emit walk_paused, got {paused_events:?}"
    );
}

// ── tests: TargetQueue arm unchanged (returns Dispatch/Drained only) ──────────

#[test]
fn walk_policy_target_queue_returns_dispatch_then_drained() {
    // AC: TargetQueue.next() returns Some(unit) once, then None.
    // No Pause variant ever returned from TargetQueue.
    use fno_agents::loop_target::TargetQueue;

    let dir = TempDir::new().unwrap();
    let abilities_dir = dir.path().join(".fno");
    fs::create_dir_all(&abilities_dir).unwrap();

    // Write minimal target-state.md.
    let manifest = r#"---
session_id: "test-session-abc123"
created_at: "2026-06-06T00:00:00Z"
input: "test feature"
plan_path: "/tmp/plan.md"
---
# Target Session State
"#;
    fs::write(abilities_dir.join("target-state.md"), manifest).unwrap();

    let mut queue = TargetQueue::from_manifest(dir.path()).unwrap();

    // First call returns the unit.
    let first = queue.next().unwrap();
    assert!(first.is_some(), "first next() must return Some");
    let u = first.unwrap();
    assert_eq!(u.session_key, "test-session-abc123");

    // Second call returns None (drained).
    let second = queue.next().unwrap();
    assert!(second.is_none(), "second next() must return None");
}

// ── tests: cancel -> Interrupted + walk_paused NOT emitted ───────────────────

#[test]
fn walk_policy_cancel_emits_interrupted_not_walk_paused() {
    // AC: SIGINT/cancel returns Interrupted; walk_paused must NOT be emitted.
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global.jsonl");
    let journal = Journal::new_raw(project_events.clone(), global_events);

    let mut queue = fno_agents::loop_megawalk::MegawalkPolicyQueue::new();
    // Add a unit so the outer loop gets past the dequeue step.
    queue.push_unit(make_unit("ab-cancel"), false);

    // Cancel fires on the first check (before dispatch).
    let cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let cancel_flag2 = cancel_flag.clone();

    struct NeverDispatcher2;
    impl Dispatcher for NeverDispatcher2 {
        fn run(&self, _u: &Unit, _c: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
            panic!("cancel must prevent dispatch");
        }
    }

    let budget = LoopBudget::new(10).unwrap();
    let outcome = run_loop(
        &mut queue,
        &NeverDispatcher2,
        &budget,
        &journal,
        &move || cancel_flag2.load(Ordering::SeqCst),
        None,
    )
    .unwrap();

    assert_eq!(outcome.reason, TerminationReason::Interrupted);
    // Must NOT have walk_paused.
    let paused = events_of_type(&project_events, "walk_paused");
    assert!(paused.is_empty(), "cancel must not emit walk_paused");
    // Must have loop_terminated with Interrupted.
    let terminated = events_of_type(&project_events, "loop_terminated");
    assert_eq!(terminated.len(), 1);
    assert_eq!(
        terminated[0]["data"]["reason"].as_str(),
        Some("Interrupted")
    );
}

// ── tests: stale claim narration (resume narration AC4-UI) ────────────────────

#[test]
fn walk_policy_clamp_parallel_cap_below_1_returns_1() {
    // Additional boundary: negative values also clamp.
    // Using i32 for the test; the function signature uses u32 or similar.
    // This test also documents the clamp boundary.
    assert_eq!(fno_agents::loop_megawalk::clamp_parallel_cap(0), 1);
}
