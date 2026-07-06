/// Integration tests for `fno-agents` loop_runtime module.
///
/// Each test drives `run_loop` directly via in-process mock impls of the
/// Queue / Dispatcher / Session traits. All file I/O uses temporary
/// directories; no network or real sessions are involved.
///
/// Test naming mirrors the acceptance criteria in the task spec:
///   1. empty_queue_terminates_nowork_without_dispatch
///   2. unit_closes_on_termination_event
///   3. no_event_exit_emits_node_failed_then_redispatches
///   4. iteration_ceiling_terminates_budget
///   5. preexisting_termination_skips_dispatch
///   6. cancel_returns_interrupted
///   7. zero_max_iterations_rejected
///   8. journal_write_failure_is_fatal
///   9. envelope_shape
use fno_agents::loop_runtime::{
    run_loop, CloseOutcome, DispatchCtx, Dispatcher, Evidence, Journal, LoopBudget, LoopError,
    Queue, Session, Unit,
};
// Note: tests use Journal::new_raw (cfg(test) convenience constructor) to avoid
// importing the newtype wrappers in every test.
use fno_agents::loopcheck::TerminationReason;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build a minimal Unit for tests.
fn make_unit(id: &str, session_key: &str) -> Unit {
    Unit {
        id: id.to_string(),
        title: format!("Test unit {id}"),
        session_key: session_key.to_string(),
        plan_path: None,
        extra_env: vec![],
    }
}

/// Build an Evidence value for a given TerminationReason.
fn make_evidence(reason: TerminationReason) -> Evidence {
    Evidence {
        reason,
        message: "test done".to_string(),
    }
}

/// Write a valid termination event line into `journal_path` for `session_key`.
fn seed_termination_event(journal_path: &Path, session_key: &str, reason: &str) {
    let line = format!(
        "{{\"ts\":\"2026-06-06T00:00:00Z\",\"type\":\"termination\",\"source\":\"hook\",\
         \"data\":{{\"session_id\":\"{session_key}\",\"reason\":\"{reason}\",\"message\":\"pre-seeded\"}}}}\n"
    );
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path)
        .expect("seed termination event");
    f.write_all(line.as_bytes()).expect("write seed");
}

/// Read all lines from a JSONL file and parse them as JSON values.
fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    if !path.exists() {
        return vec![];
    }
    let content = fs::read_to_string(path).expect("read jsonl");
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Count events of a given type in a JSONL file.
fn count_events(path: &Path, event_type: &str) -> usize {
    read_jsonl(path)
        .into_iter()
        .filter(|v| v["type"].as_str() == Some(event_type))
        .count()
}

// ── mock impls ────────────────────────────────────────────────────────────────

/// A Queue that serves a fixed list of units in order, then returns None.
struct FixedQueue {
    units: Mutex<Vec<Unit>>,
    /// Records each unit.id passed to close().
    closed: Mutex<Vec<String>>,
    close_outcome: CloseOutcome,
}

impl FixedQueue {
    fn new(units: Vec<Unit>) -> Self {
        Self {
            units: Mutex::new(units),
            closed: Mutex::new(vec![]),
            close_outcome: CloseOutcome::Closed,
        }
    }

    fn with_close_outcome(units: Vec<Unit>, outcome: CloseOutcome) -> Self {
        Self {
            units: Mutex::new(units),
            closed: Mutex::new(vec![]),
            close_outcome: outcome,
        }
    }

    fn close_count(&self) -> usize {
        self.closed.lock().unwrap().len()
    }
}

impl Queue for FixedQueue {
    fn next(&mut self) -> Result<Option<Unit>, LoopError> {
        let mut units = self.units.lock().unwrap();
        if units.is_empty() {
            Ok(None)
        } else {
            Ok(Some(units.remove(0)))
        }
    }

    fn close(&mut self, unit: &Unit, _evidence: &Evidence) -> Result<CloseOutcome, LoopError> {
        self.closed.lock().unwrap().push(unit.id.clone());
        // Return a clone of the stored outcome.
        Ok(match &self.close_outcome {
            CloseOutcome::Closed => CloseOutcome::Closed,
            CloseOutcome::Refused(s) => CloseOutcome::Refused(s.clone()),
            CloseOutcome::Parked(s) => CloseOutcome::Parked(s.clone()),
        })
    }
}

/// A Dispatcher that panics if called (for "no dispatch expected" tests).
struct PanicDispatcher;

impl Dispatcher for PanicDispatcher {
    fn run(&self, _unit: &Unit, _ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
        panic!("PanicDispatcher: dispatch must not be called in this test");
    }
}

/// A Session that writes an optional termination event to a journal file, then
/// returns a fixed exit code. Used by MockDispatcher below.
struct MockSession {
    journal_path: PathBuf,
    session_key: String,
    termination_reason: Option<String>, // None -> write nothing
    exit_code: i32,
}

impl Session for MockSession {
    fn wait(&mut self) -> Result<i32, LoopError> {
        if let Some(reason) = &self.termination_reason {
            seed_termination_event(&self.journal_path, &self.session_key, reason);
        }
        Ok(self.exit_code)
    }
}

/// A Dispatcher whose sessions are configured by a list of responses (in order).
/// Each response specifies whether the session writes a termination event and
/// what exit code to return.
struct MockDispatcher {
    journal_path: PathBuf,
    /// Per-dispatch config: (termination_reason, exit_code). Consumed in order;
    /// last element is reused if the list is exhausted.
    responses: Mutex<Vec<(Option<String>, i32)>>,
    dispatch_count: AtomicU64,
}

impl MockDispatcher {
    fn new(journal_path: PathBuf, responses: Vec<(Option<String>, i32)>) -> Self {
        Self {
            journal_path,
            responses: Mutex::new(responses),
            dispatch_count: AtomicU64::new(0),
        }
    }

    fn count(&self) -> u64 {
        self.dispatch_count.load(Ordering::SeqCst)
    }
}

impl Dispatcher for MockDispatcher {
    fn run(&self, unit: &Unit, _ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
        self.dispatch_count.fetch_add(1, Ordering::SeqCst);
        let mut responses = self.responses.lock().unwrap();
        let (reason, exit_code) = if responses.len() > 1 {
            responses.remove(0)
        } else {
            // Reuse last response indefinitely.
            responses[0].clone()
        };
        Ok(Box::new(MockSession {
            journal_path: self.journal_path.clone(),
            session_key: unit.session_key.clone(),
            termination_reason: reason,
            exit_code,
        }))
    }
}

// ── test 1: empty queue terminates with NoWork, dispatcher not called ─────────

#[test]
fn empty_queue_terminates_nowork_without_dispatch() {
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    let mut queue = FixedQueue::new(vec![]);
    let dispatcher = PanicDispatcher;
    let budget = LoopBudget::new(10).unwrap();
    let journal = Journal::new_raw(project_events.clone(), global_events);

    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &|| false, None).unwrap();

    assert_eq!(outcome.reason, TerminationReason::NoWork);
    assert_eq!(outcome.iterations_used, 0);
    assert!(outcome.units.is_empty());

    // loop_terminated event must be present in the project journal.
    let events = read_jsonl(&project_events);
    let terminated: Vec<_> = events
        .iter()
        .filter(|v| v["type"].as_str() == Some("loop_terminated"))
        .collect();
    assert_eq!(
        terminated.len(),
        1,
        "expected exactly one loop_terminated event"
    );
    assert_eq!(
        terminated[0]["data"]["reason"].as_str(),
        Some("NoWork"),
        "loop_terminated reason must be NoWork"
    );
}

// ── test 2: unit closes on termination event ──────────────────────────────────

#[test]
fn unit_closes_on_termination_event() {
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    let unit = make_unit("ab-001", "sess-001");
    let mut queue = FixedQueue::new(vec![unit]);
    // Session writes DonePRGreen termination event, exits 0.
    let dispatcher = MockDispatcher::new(
        project_events.clone(),
        vec![(Some("DonePRGreen".to_string()), 0)],
    );
    let budget = LoopBudget::new(10).unwrap();
    let journal = Journal::new_raw(project_events.clone(), global_events);

    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &|| false, None).unwrap();

    // Walk reason: queue empty after ab-001 -> NoWork (walk level).
    assert_eq!(outcome.reason, TerminationReason::NoWork);
    assert_eq!(outcome.iterations_used, 1);
    assert_eq!(outcome.units.len(), 1);
    assert_eq!(
        outcome.units[0].evidence.reason,
        TerminationReason::DonePRGreen
    );
    assert_eq!(outcome.units[0].close, CloseOutcome::Closed);

    // close() was called exactly once.
    assert_eq!(queue.close_count(), 1);

    // loop_unit_dispatched event must be present.
    assert_eq!(
        count_events(&project_events, "loop_unit_dispatched"),
        1,
        "expected one loop_unit_dispatched"
    );
    // No node_failed events (session produced a termination event).
    assert_eq!(
        count_events(&project_events, "node_failed"),
        0,
        "expected no node_failed"
    );
}

// ── test 3: no event on exit -> node_failed then re-dispatch ─────────────────

#[test]
fn no_event_exit_emits_node_failed_then_redispatches() {
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    let unit = make_unit("ab-002", "sess-002");
    let mut queue = FixedQueue::new(vec![unit]);
    // First session: writes nothing, exits 1.
    // Second session: writes DonePRGreen, exits 0.
    let dispatcher = MockDispatcher::new(
        project_events.clone(),
        vec![(None, 1), (Some("DonePRGreen".to_string()), 0)],
    );
    let budget = LoopBudget::new(10).unwrap();
    let journal = Journal::new_raw(project_events.clone(), global_events);

    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &|| false, None).unwrap();

    assert_eq!(outcome.reason, TerminationReason::NoWork);
    assert_eq!(outcome.iterations_used, 2);
    assert_eq!(
        outcome.units[0].evidence.reason,
        TerminationReason::DonePRGreen
    );

    // Exactly 2 dispatches.
    assert_eq!(dispatcher.count(), 2, "expected exactly 2 dispatches");

    // Exactly 1 node_failed (from first session).
    assert_eq!(
        count_events(&project_events, "node_failed"),
        1,
        "expected exactly one node_failed"
    );

    // 2 loop_unit_dispatched events.
    assert_eq!(
        count_events(&project_events, "loop_unit_dispatched"),
        2,
        "expected two loop_unit_dispatched events"
    );
}

// ── test 4: iteration ceiling terminates budget ────────────────────────────────

#[test]
fn iteration_ceiling_terminates_budget() {
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    let unit = make_unit("ab-003", "sess-003");
    let mut queue = FixedQueue::new(vec![unit]);
    // Session always writes nothing, exits 1.
    let dispatcher = MockDispatcher::new(project_events.clone(), vec![(None, 1)]);
    let budget = LoopBudget::new(3).unwrap();
    let journal = Journal::new_raw(project_events.clone(), global_events);

    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &|| false, None).unwrap();

    assert_eq!(outcome.reason, TerminationReason::Budget);
    assert_eq!(
        outcome.iterations_used, 3,
        "should use exactly budget iterations"
    );
    assert_eq!(dispatcher.count(), 3, "expected exactly 3 dispatches");

    // 3 node_failed events.
    assert_eq!(
        count_events(&project_events, "node_failed"),
        3,
        "expected 3 node_failed"
    );

    // loop_terminated with axis=iterations.
    let events = read_jsonl(&project_events);
    let terminated: Vec<_> = events
        .iter()
        .filter(|v| v["type"].as_str() == Some("loop_terminated"))
        .collect();
    assert_eq!(terminated.len(), 1);
    assert_eq!(terminated[0]["data"]["reason"].as_str(), Some("Budget"));
    assert_eq!(
        terminated[0]["data"]["axis"].as_str(),
        Some("iterations"),
        "Budget termination must carry axis=iterations"
    );
}

// ── test 5: pre-existing termination skips dispatch (AC1-FR core) ─────────────

#[test]
fn preexisting_termination_skips_dispatch() {
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    // Seed a termination event BEFORE run_loop is called.
    seed_termination_event(&project_events, "sess-004", "DoneAdvisory");

    let unit = make_unit("ab-004", "sess-004");
    let mut queue = FixedQueue::new(vec![unit]);
    // Dispatcher panics if called - proves no dispatch happens.
    let dispatcher = PanicDispatcher;
    let budget = LoopBudget::new(5).unwrap();
    let journal = Journal::new_raw(project_events.clone(), global_events);

    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &|| false, None).unwrap();

    // Walk closes ab-004 without dispatching.
    assert_eq!(
        outcome.iterations_used, 0,
        "resume guard: no dispatch iterations"
    );
    assert_eq!(outcome.units.len(), 1);
    assert_eq!(
        outcome.units[0].evidence.reason,
        TerminationReason::DoneAdvisory
    );
    assert_eq!(outcome.units[0].close, CloseOutcome::Closed);

    // close() was called exactly once.
    assert_eq!(queue.close_count(), 1);

    // No loop_unit_dispatched events.
    assert_eq!(
        count_events(&project_events, "loop_unit_dispatched"),
        0,
        "resume guard: no dispatch events"
    );
}

// ── test 6: cancel returns Interrupted ────────────────────────────────────────

#[test]
fn cancel_returns_interrupted() {
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    // cancel fires immediately (before any dispatch).
    let mut queue = FixedQueue::new(vec![make_unit("ab-005", "sess-005")]);
    let dispatcher = PanicDispatcher;
    let budget = LoopBudget::new(10).unwrap();
    let journal = Journal::new_raw(project_events.clone(), global_events);

    let cancel_flag = Arc::new(AtomicBool::new(true)); // pre-set to true
    let flag = cancel_flag.clone();
    let outcome = run_loop(
        &mut queue,
        &dispatcher,
        &budget,
        &journal,
        &move || flag.load(Ordering::SeqCst),
        None,
    )
    .unwrap();

    assert_eq!(outcome.reason, TerminationReason::Interrupted);

    let events = read_jsonl(&project_events);
    let terminated: Vec<_> = events
        .iter()
        .filter(|v| v["type"].as_str() == Some("loop_terminated"))
        .collect();
    assert_eq!(terminated.len(), 1);
    assert_eq!(
        terminated[0]["data"]["reason"].as_str(),
        Some("Interrupted")
    );
}

// ── test 7: zero max_iterations rejected ──────────────────────────────────────

#[test]
fn zero_max_iterations_rejected() {
    let result = LoopBudget::new(0);
    assert!(result.is_err(), "LoopBudget::new(0) must return Err");
}

// ── test 8: journal write failure is fatal ────────────────────────────────────

#[test]
fn journal_write_failure_is_fatal() {
    let dir = TempDir::new().unwrap();
    // Create a REGULAR FILE at the path where the journal directory would need
    // to be created, so open(create=true) fails with EISDIR or ENOTDIR.
    // Specifically: place a file as the "parent" component of the events path
    // so that create_dir_all or open fails.
    let blocker = dir.path().join("blocker");
    fs::write(&blocker, b"not a dir").unwrap();
    // Make the project events path be inside the regular file (impossible path).
    let project_events = blocker.join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    let mut queue = FixedQueue::new(vec![make_unit("ab-006", "sess-006")]);
    let dispatcher = PanicDispatcher; // should not be reached
    let budget = LoopBudget::new(5).unwrap();
    let journal = Journal::new_raw(project_events, global_events);

    let result = run_loop(&mut queue, &dispatcher, &budget, &journal, &|| false, None);
    assert!(
        result.is_err(),
        "journal write failure to project path must be fatal (Err)"
    );
}

// ── test 9: envelope shape ────────────────────────────────────────────────────

#[test]
fn envelope_shape() {
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    // Seed a garbage line first - must be silently skipped by find_termination.
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&project_events)
            .unwrap();
        f.write_all(b"not json at all\n").unwrap();
        // A valid JSON line that is NOT a termination event.
        f.write_all(b"{\"type\":\"other\",\"data\":{}}\n").unwrap();
    }

    let unit = make_unit("ab-007", "sess-007");
    let mut queue = FixedQueue::new(vec![unit]);
    let dispatcher = MockDispatcher::new(
        project_events.clone(),
        vec![(Some("DonePRGreen".to_string()), 0)],
    );
    let budget = LoopBudget::new(5).unwrap();
    let journal = Journal::new_raw(project_events.clone(), global_events.clone());

    run_loop(&mut queue, &dispatcher, &budget, &journal, &|| false, None).unwrap();

    // Read all lines from project events (skip the pre-seeded garbage).
    let content = fs::read_to_string(&project_events).unwrap();
    let runtime_lines: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        // Skip lines that aren't valid JSON or are our pre-seeded non-runtime lines.
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        // Filter to lines written by the runtime (source == "loop").
        .filter(|v| v["source"].as_str() == Some("loop"))
        .collect();

    assert!(
        !runtime_lines.is_empty(),
        "runtime must have written at least one loop-source event"
    );

    for line in &runtime_lines {
        // ts: present, string, parseable as RFC3339 with Z suffix.
        let ts = line["ts"].as_str().expect("ts must be a string");
        assert!(ts.ends_with('Z'), "ts must end with Z: {ts}");
        ts.parse::<chrono::DateTime<chrono::Utc>>()
            .expect("ts must be valid RFC3339");

        // type: present and string.
        assert!(line["type"].is_string(), "type must be a string: {line}");

        // source: "loop".
        assert_eq!(
            line["source"].as_str(),
            Some("loop"),
            "source must be 'loop': {line}"
        );

        // data: present and an object.
        assert!(line["data"].is_object(), "data must be an object: {line}");
    }

    // Verify global mirror also has loop-source events.
    let global_content = fs::read_to_string(&global_events).unwrap_or_default();
    let global_runtime_lines: Vec<serde_json::Value> = global_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &serde_json::Value| v["source"].as_str() == Some("loop"))
        .collect();
    assert!(
        !global_runtime_lines.is_empty(),
        "global mirror must also have loop-source events"
    );
}

// ── test 10: F1 - termination event with missing reason field skips (no panic) ─

#[test]
fn termination_event_missing_reason_skips_no_panic() {
    // A termination event matching type+session_id but with no reason field is
    // authoritative-record corruption. The runtime must warn on stderr and skip
    // (no panic, no match) - behavioral no-change from the caller's perspective.
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    // Write a termination event with no reason field for sess-f1.
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&project_events)
            .unwrap();
        // Missing "reason" key entirely.
        f.write_all(
            b"{\"ts\":\"2026-06-06T00:00:00Z\",\"type\":\"termination\",\"source\":\"hook\",\
              \"data\":{\"session_id\":\"sess-f1\",\"message\":\"corrupt\"}}\n",
        )
        .unwrap();
    }

    // Because the corrupt event has no reason, find_termination returns None
    // and the loop dispatches normally (node_failed path), then hits Budget.
    let unit = make_unit("ab-f1", "sess-f1");
    let mut queue = FixedQueue::new(vec![unit]);
    // Dispatcher always returns nothing -> no termination event gets written.
    let dispatcher = MockDispatcher::new(project_events.clone(), vec![(None, 1)]);
    let budget = LoopBudget::new(1).unwrap();
    let journal = Journal::new_raw(project_events.clone(), global_events);

    // Must not panic; budget terminates the walk.
    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &|| false, None).unwrap();
    assert_eq!(
        outcome.reason,
        fno_agents::loopcheck::TerminationReason::Budget,
        "corrupt termination event must not match; walk terminates with Budget"
    );
}

// ── test 11: F7 - parse_termination_reason via serde round-trips correctly ────

#[test]
fn parse_termination_reason_serde_roundtrip() {
    // Verify all known variants round-trip through the journal path correctly.
    // Unknown strings must produce no match (walk continues, not panic).
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    for (reason_str, expected) in &[
        (
            "DonePRGreen",
            fno_agents::loopcheck::TerminationReason::DonePRGreen,
        ),
        (
            "DoneAdvisory",
            fno_agents::loopcheck::TerminationReason::DoneAdvisory,
        ),
        (
            "DoneBatched",
            fno_agents::loopcheck::TerminationReason::DoneBatched,
        ),
        ("NoWork", fno_agents::loopcheck::TerminationReason::NoWork),
        ("Budget", fno_agents::loopcheck::TerminationReason::Budget),
        (
            "NoProgress",
            fno_agents::loopcheck::TerminationReason::NoProgress,
        ),
        (
            "Interrupted",
            fno_agents::loopcheck::TerminationReason::Interrupted,
        ),
        ("Aborted", fno_agents::loopcheck::TerminationReason::Aborted),
    ] {
        // Clear and re-seed for each variant.
        let _ = fs::remove_file(&project_events);
        seed_termination_event(&project_events, "sess-serde", reason_str);

        let journal = Journal::new_raw(project_events.clone(), global_events.clone());
        let evidence = journal
            .find_termination("sess-serde")
            .expect("find_termination must not error")
            .expect(&format!("must find termination for reason '{reason_str}'"));
        assert_eq!(
            &evidence.reason, expected,
            "serde parse must round-trip '{reason_str}'"
        );
    }

    // Unknown reason string must produce None (no match, no panic).
    let _ = fs::remove_file(&project_events);
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&project_events)
            .unwrap();
        f.write_all(
            b"{\"ts\":\"2026-06-06T00:00:00Z\",\"type\":\"termination\",\"source\":\"hook\",\
              \"data\":{\"session_id\":\"sess-serde\",\"reason\":\"UnknownFuture\",\"message\":\"\"}}\n",
        )
        .unwrap();
    }
    let journal = Journal::new_raw(project_events.clone(), global_events.clone());
    let result = journal
        .find_termination("sess-serde")
        .expect("find_termination must not error");
    assert!(
        result.is_none(),
        "unknown reason string must not match (returns None)"
    );
}

// ── bg-guard refusal (x-4504, AC1-ERR) ────────────────────────────────────────

/// A Session that returns a fixed exit code and a fixed `output_tail`, never
/// writing a termination event. Models a `claude --resume` that exited with the
/// bg-guard refusal message (or an ordinary crash, when the tail lacks it).
struct OutputSession {
    exit_code: i32,
    output_tail: Option<String>,
}

impl Session for OutputSession {
    fn wait(&mut self) -> Result<i32, LoopError> {
        Ok(self.exit_code)
    }
    fn output_tail(&self) -> Option<String> {
        self.output_tail.clone()
    }
}

/// A Dispatcher whose every session returns the same exit code + output tail.
struct OutputDispatcher {
    exit_code: i32,
    output_tail: Option<String>,
    dispatch_count: AtomicU64,
}

impl OutputDispatcher {
    fn new(exit_code: i32, output_tail: Option<&str>) -> Self {
        Self {
            exit_code,
            output_tail: output_tail.map(str::to_string),
            dispatch_count: AtomicU64::new(0),
        }
    }
    fn count(&self) -> u64 {
        self.dispatch_count.load(Ordering::SeqCst)
    }
}

impl Dispatcher for OutputDispatcher {
    fn run(&self, _unit: &Unit, _ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
        self.dispatch_count.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(OutputSession {
            exit_code: self.exit_code,
            output_tail: self.output_tail.clone(),
        }))
    }
}

/// AC1-ERR: a claude bg-guard refusal exit parks the unit after ONE dispatch —
/// it must not re-dispatch into a sustained respawn loop.
#[test]
fn bg_guard_refusal_parks_without_redispatch() {
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    let unit = make_unit("ab-bg1", "sess-bg1");
    let mut queue = FixedQueue::new(vec![unit]);
    // exit 1 + the claude bg-guard message, never a termination event.
    let dispatcher = OutputDispatcher::new(
        1,
        Some("sess-bg1 is currently running as a background agent (bg). Use 'claude agents'."),
    );
    // Generous budget + NO per-unit cap: only the bg-guard branch can stop it,
    // so an unbounded budget would spin forever if the branch were absent.
    let budget = LoopBudget::new(100).unwrap();
    let journal = Journal::new_raw(project_events.clone(), global_events);

    let outcome = run_loop(&mut queue, &dispatcher, &budget, &journal, &|| false, None).unwrap();

    // The unit is parked (NoProgress) after exactly one dispatch; the walk then
    // drains the queue and finishes NoWork.
    assert_eq!(outcome.reason, TerminationReason::NoWork);
    assert_eq!(
        dispatcher.count(),
        1,
        "bg-guard refusal must NOT re-dispatch"
    );
    assert_eq!(
        outcome.units[0].evidence.reason,
        TerminationReason::NoProgress
    );

    // No node_failed (we did not treat it as a crash); exactly one dispatch.
    assert_eq!(
        count_events(&project_events, "node_failed"),
        0,
        "bg-guard refusal is terminal, not a crash-respawn"
    );
    assert_eq!(count_events(&project_events, "loop_unit_dispatched"), 1);

    // The bg-guard park is journaled as node_closed (like the per-unit-cap
    // park); the bg-guard identity lives in the in-memory evidence message.
    // It must NOT emit walk_paused (reserved for queue-level policy pauses,
    // schema.yaml enum consecutive_failures|p0_failed).
    assert_eq!(
        count_events(&project_events, "walk_paused"),
        0,
        "per-unit bg-guard park must not emit the queue-level walk_paused event"
    );
    assert_eq!(count_events(&project_events, "node_closed"), 1);
    assert!(
        outcome.units[0]
            .evidence
            .message
            .to_lowercase()
            .contains("bg-guard refusal"),
        "evidence must identify the bg-guard park"
    );
}

/// A bare non-zero exit WITHOUT the bg-guard marker must still re-dispatch like
/// an ordinary crash (the bg-guard branch must not suppress real crashes).
#[test]
fn bare_crash_exit_still_redispatches() {
    let dir = TempDir::new().unwrap();
    let project_events = dir.path().join("events.jsonl");
    let global_events = dir.path().join("global-events.jsonl");

    let unit = make_unit("ab-bg2", "sess-bg2");
    let mut queue = FixedQueue::new(vec![unit]);
    // exit 1 with an ordinary crash message (no bg-guard marker).
    let dispatcher = OutputDispatcher::new(1, Some("panic: index out of bounds"));
    let budget = LoopBudget::new(100).unwrap();
    let journal = Journal::new_raw(project_events.clone(), global_events);

    // per_unit cap = 2 so the ordinary crash-respawn is bounded for the test.
    let outcome = run_loop(
        &mut queue,
        &dispatcher,
        &budget,
        &journal,
        &|| false,
        Some(2),
    )
    .unwrap();

    assert_eq!(outcome.reason, TerminationReason::NoWork);
    // Re-dispatched up to the cap (2), proving it was NOT suppressed as a
    // bg-guard refusal; then parked NoProgress by the cap.
    assert_eq!(
        dispatcher.count(),
        2,
        "ordinary crash must re-dispatch to cap"
    );
    assert_eq!(
        count_events(&project_events, "node_failed"),
        2,
        "each ordinary crash emits node_failed"
    );
    // The cap park is an ordinary NoProgress park, NOT a bg-guard one.
    assert!(
        !outcome.units[0]
            .evidence
            .message
            .to_lowercase()
            .contains("bg-guard"),
        "markerless crash must not be classified as a bg-guard park"
    );
}
