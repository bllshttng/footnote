//! Unified loop runtime primitive for target, megawalk, and megatron drivers.
//!
//! This module is the generic "walk a queue of units, dispatch sessions,
//! handle termination" engine. Drivers differ only in their Queue and
//! Dispatcher implementations. The runtime itself has no opinion about what
//! a "unit" is (backlog node, fleet project, ...) or how sessions are launched.
//!
//! ## Why journal write failure is fatal (contrast with loopcheck.rs)
//!
//! `loopcheck.rs` runs as a stop hook: it is a read-only observer that must
//! never block the session from exiting. Journal writes there are best-effort
//! (logged to stderr, never propagated as errors) because a failed write
//! cannot undo a decision the runtime already made.
//!
//! Here the journal is the observability record of an *active walk*. If the
//! runtime cannot record that it dispatched a session, an operator watching
//! the log sees nothing and cannot tell whether work is happening. An
//! unobservable walk that continues spending compute (and potentially money)
//! is worse than stopping loudly. The invariant: "The system must handle
//! journal write failure by stopping dispatch loudly; an unobservable walk
//! must not continue spending."
//!
//! The global mirror (`~/.fno/events.jsonl`) is best-effort: a write
//! failure there is logged to stderr but never fatal, because the project
//! journal is the authoritative record.
//!
//! ## Source field
//!
//! Events written by this runtime carry `source: "loop"`, distinct from the
//! `source: "hook"` that loopcheck uses. The two streams have different
//! semantics: hook events are stop-hook decision records; loop events are
//! walk-level orchestration records. Consumers that aggregate both streams
//! can use the source field to distinguish them.
//!
//! ## Module naming
//!
//! The module name starts with `loop` so that the LOC-ratchet glob
//! `crates/fno-agents/src/loop*` counts this file's LOC toward the ratchet
//! budget deliberately (alongside loopcheck.rs).

use crate::loopcheck::TerminationReason;
use chrono::Utc;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

// ── newtype wrappers for Journal paths (F9) ───────────────────────────────────

/// Newtype for the project-authoritative journal path. Writes are FATAL on failure.
/// Using a distinct type prevents silent positional swap of the two same-type args.
pub struct ProjectJournalPath(pub PathBuf);

/// Newtype for the global mirror journal path (`~/.fno/events.jsonl`).
/// Writes are best-effort only.
pub struct GlobalJournalPath(pub PathBuf);

// ── public error type ─────────────────────────────────────────────────────────

/// Errors returned by the loop runtime.
#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    /// I/O failure (file open, read, write).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Journal write to the project events file failed (fatal per spec).
    #[error("journal write failure (project): {0}")]
    Journal(String),

    /// Queue operation failed.
    #[error("queue error: {0}")]
    Queue(String),

    /// Walk policy requests a pause. Not a true error: `run_loop` maps it to a
    /// `walk_paused` journal event + `TerminationReason::NoProgress`. Kept
    /// distinct from `Queue` so a real queue error whose message happens to
    /// start with `pause:` can never be misrouted to the pause path. Structural
    /// twin of `NextStep::Pause`, carrying the same typed `policy`/`detail`.
    #[error("walk paused (policy={policy}): {detail}")]
    Pause { policy: String, detail: String },

    /// Dispatcher operation failed.
    #[error("dispatch error: {0}")]
    Dispatch(String),

    /// Configuration error (e.g., invalid budget).
    #[error("configuration error: {0}")]
    Config(String),
}

// ── public types ──────────────────────────────────────────────────────────────

/// A single unit of work to be dispatched. Drivers map their domain objects
/// (backlog nodes, fleet projects, ...) to this common shape.
pub struct Unit {
    /// Stable identifier, e.g. `ab-XXXXXXXX` for a backlog node.
    pub id: String,
    /// Human-readable title for log output.
    pub title: String,
    /// Correlation key matched against termination events' `data.session_id`.
    /// For target sessions this is the session identifier written into the
    /// target-state.md manifest; for other drivers it is whatever key the
    /// dispatcher embeds in its events.
    pub session_key: String,
    /// Optional plan path for context (not used by the runtime itself).
    pub plan_path: Option<String>,
    /// Driver-specific extra env vars to inject into the child process.
    /// The runtime passes these through to the Dispatcher without inspecting
    /// them. MegawalkQueue populates TARGET_MISSION_* for fleet nodes;
    /// TargetQueue leaves this empty. Megatron will use this seam too.
    pub extra_env: Vec<(String, String)>,
}

/// Evidence of termination extracted from the project journal.
pub struct Evidence {
    /// The parsed TerminationReason.
    pub reason: TerminationReason,
    /// Human-readable message from the event's `data.message` field.
    pub message: String,
}

/// Outcome of queue.close() for a single unit.
#[derive(Debug, PartialEq)]
pub enum CloseOutcome {
    /// Unit was closed successfully.
    Closed,
    /// Queue refused to close the unit (e.g. it was claimed by another walker).
    Refused(String),
    /// Unit was parked for later (e.g. dependents not yet resolved).
    Parked(String),
}

/// Runtime-varying context passed to each Dispatcher::run call. Static
/// configuration (project root, env vars, etc.) lives in the Dispatcher impl.
pub struct DispatchCtx {
    /// 1-based iteration counter across all units in this walk.
    pub iteration: u64,
}

// ── NextStep ──────────────────────────────────────────────────────────────────

/// The richer return type for Queue::next_step(). Used by policy-aware queues
/// (MegawalkQueue) to signal a walk-level pause without returning an error.
///
/// - `Dispatch(Unit)`: there is work to do; dispatch this unit.
/// - `Drained`: the queue is empty; the walk may complete with NoWork.
/// - `Pause{policy, detail}`: walk policy says stop; run_loop maps this to
///   walk_paused + loop_terminated{reason: NoProgress}.
///
/// Queues that do not implement walk policy (TargetQueue) return only
/// Dispatch / Drained (i.e., they translate their Option<Unit> to these two).
pub enum NextStep {
    /// There is a unit ready to dispatch.
    Dispatch(Unit),
    /// The queue is empty; no more work.
    Drained,
    /// Walk policy requests a pause.
    Pause {
        /// Short tag: "consecutive_failures" | "p0_failed".
        policy: String,
        /// Human-readable detail (unit IDs involved, streak count, etc.).
        detail: String,
    },
}

// ── traits ────────────────────────────────────────────────────────────────────

/// Source of work units. Each call to `next` either returns the next unit to
/// dispatch or `None` to signal that the walk is complete.
///
/// ## Why `&mut self` (F8 rationale)
///
/// Group-2 megawalk Queue carries real cursor state (current position in the
/// backlog, consecutive-failure counters, etc.). Using `&self` would force
/// pointless `Mutex`-wrapping for state that is inherently sequential in the
/// single-threaded walk loop. No threading requirement exists at this seam:
/// `run_loop` is always called from a single thread. `&mut self` is the natural
/// fit and avoids unnecessary interior-mutability noise.
pub trait Queue {
    /// Return the next unit, or `None` if the queue is empty.
    ///
    /// Queues that implement walk policy (e.g. MegawalkPolicyQueue) should
    /// use `next_step()` instead. This default impl calls `next_step()` and
    /// maps Dispatch->Some, Drained->None, Pause->Err(LoopError::Pause{..}).
    fn next(&mut self) -> Result<Option<Unit>, LoopError> {
        match self.next_step()? {
            NextStep::Dispatch(u) => Ok(Some(u)),
            NextStep::Drained => Ok(None),
            NextStep::Pause { policy, detail } => Err(LoopError::Pause { policy, detail }),
        }
    }

    /// Richer variant of `next()` that can signal a walk-level pause.
    /// Policy-aware queues override this; simple queues (TargetQueue) can
    /// leave the default which panics (they override `next()` directly instead).
    ///
    /// The default panics to make missing overrides detectable at test time.
    fn next_step(&mut self) -> Result<NextStep, LoopError> {
        // Default: not implemented. Queues override exactly one of next() or
        // next_step(). TargetQueue overrides next() only (returns Option).
        // MegawalkPolicyQueue overrides next_step() only (returns NextStep).
        panic!("Queue::next_step() not implemented; override next() or next_step()");
    }

    /// Mark a unit as closed (done/parked/refused) given the termination
    /// evidence extracted from the journal.
    fn close(&mut self, unit: &Unit, evidence: &Evidence) -> Result<CloseOutcome, LoopError>;
}

/// A live session handle returned by a Dispatcher.
pub trait Session {
    /// Block until the session exits and return its exit code.
    fn wait(&mut self) -> Result<i32, LoopError>;

    /// Tail of the session's captured driver output (the `OUTPUT_FILE` the bash
    /// driver redirects claude stdout+stderr into), if available. The walk reads
    /// it to classify a non-termination exit: claude's bg-guard refusal
    /// ("running as a background agent (bg)") must terminate the unit rather than
    /// be re-dispatched into an infinite respawn loop (x-4504, AC1-ERR). Default
    /// `None` -> a Session that captures no output is treated as an ordinary
    /// crash and re-dispatched exactly as before.
    fn output_tail(&self) -> Option<String> {
        None
    }
}

/// Launches sessions for units. Stateless with respect to the walk loop;
/// any per-dispatch state is internal to the impl.
pub trait Dispatcher {
    /// Launch a session for the given unit in the given walk context.
    fn run(&self, unit: &Unit, ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError>;
}

// ── budget ────────────────────────────────────────────────────────────────────

/// Walk-level iteration budget. Prevents unbounded loops when sessions never
/// produce termination events.
pub struct LoopBudget {
    max_iterations: u64,
}

impl LoopBudget {
    /// Create a budget. Rejects `max_iterations = 0` because a budget of zero
    /// would immediately terminate every walk at the pre-dispatch check,
    /// which is never a useful configuration.
    pub fn new(max_iterations: u64) -> Result<Self, LoopError> {
        if max_iterations == 0 {
            return Err(LoopError::Config("max_iterations must be > 0".to_string()));
        }
        Ok(Self { max_iterations })
    }
}

// ── journal ───────────────────────────────────────────────────────────────────

/// Append-only event log with a project-authoritative path and a best-effort
/// global mirror.
pub struct Journal {
    /// Project-scoped events file. Writes here are FATAL on failure.
    project_path: PathBuf,
    /// Global mirror (`~/.fno/events.jsonl`). Writes here are best-effort.
    global_path: PathBuf,
}

impl Journal {
    /// Create a Journal. Paths are injected via newtypes (F9) to prevent silent
    /// positional swap of the two same-type `PathBuf` arguments.
    /// The runtime never hard-codes `~/.fno/`. The driver CLI wires the real
    /// paths (task 1.2). Tests use tempdir paths.
    pub fn new(project_path: ProjectJournalPath, global_path: GlobalJournalPath) -> Self {
        Self {
            project_path: project_path.0,
            global_path: global_path.0,
        }
    }

    /// Convenience constructor accepting plain `PathBuf`s directly. Intended for
    /// tests where importing the newtype wrappers would add noise. Production
    /// callers should prefer `Journal::new` (which enforces distinct types).
    pub fn new_raw(project_path: PathBuf, global_path: PathBuf) -> Self {
        Self {
            project_path,
            global_path,
        }
    }

    /// Append one event line to the project journal (fatal on failure) and
    /// mirror it to the global file (best-effort).
    ///
    /// The envelope shape is:
    /// `{"ts":"YYYY-MM-DDTHH:MM:SSZ","type":"<kind>","source":"loop","data":{...}}`
    ///
    /// Method name is `append` (NOT `emit`/`emit_fields`) so the
    /// daemon-stream parity scanner in lib.rs (which greps for `.emit(`)
    /// does not require these kinds to be registered in KNOWN_EVENT_KINDS.
    /// These are loop-stream events, not daemon-stream events.
    pub fn append(&self, event_type: &str, data: Value) -> Result<(), LoopError> {
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let env = json!({
            "ts": ts,
            "type": event_type,
            "source": "loop",
            "data": data,
        });
        let mut line = serde_json::to_string(&env)
            .map_err(|e| LoopError::Journal(format!("serialize {event_type}: {e}")))?;
        line.push('\n');

        // Write to project file - FATAL on failure.
        self.append_to_file(&self.project_path, &line, true)?;

        // Mirror to global file - best-effort (warn, never fatal).
        if self.project_path != self.global_path {
            if let Err(e) = self.append_to_file(&self.global_path, &line, false) {
                eprintln!("loop-runtime: global mirror write failed (non-fatal): {e}");
            }
        }

        Ok(())
    }

    /// Open `path` in append+create mode and write `line`. If `fatal` is true,
    /// returns `Err(LoopError::Journal)` on failure; otherwise returns `Ok`.
    fn append_to_file(&self, path: &Path, line: &str, fatal: bool) -> Result<(), LoopError> {
        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                let msg = format!("create_dir_all {}: {e}", parent.display());
                if fatal {
                    return Err(LoopError::Journal(msg));
                } else {
                    return Err(LoopError::Io(e));
                }
            }
        }

        match fs::OpenOptions::new().create(true).append(true).open(path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(line.as_bytes()) {
                    let msg = format!("write to {}: {e}", path.display());
                    if fatal {
                        return Err(LoopError::Journal(msg));
                    } else {
                        return Err(LoopError::Io(e));
                    }
                }
                Ok(())
            }
            Err(e) => {
                let msg = format!("open {}: {e}", path.display());
                if fatal {
                    Err(LoopError::Journal(msg))
                } else {
                    Err(LoopError::Io(e))
                }
            }
        }
    }

    /// Scan journals for the LAST termination event matching `session_key`.
    ///
    /// ## Search order (ab-7303e5d7: cross-cwd delivery via global mirror)
    ///
    /// 1. Scan the project journal first (authoritative for single-cwd target walks).
    /// 2. When no match is found AND the global path differs from the project path,
    ///    scan the global journal (`~/.fno/events.jsonl`).
    ///
    /// Rationale: worker sessions dispatched by the megawalk walker run `/target`
    /// in their OWN conductor worktrees, so their termination events land in the
    /// WORKTREE's events.jsonl.  loopcheck's `emit_to_both` also mirrors them to
    /// `~/.fno/events.jsonl` (the global path).  The walker's project journal
    /// lives at the walker's cwd; only the global mirror is shared across cwds.
    ///
    /// Returns `None` if not found in either journal or on read errors (fail
    /// tolerant: unreadable journal = no pre-existing termination).
    ///
    /// Uses `BufReader` + `.lines()` to stream files rather than loading them
    /// entirely; the journal rotates at 8 MB but can grow to that before
    /// rotation, so streaming avoids a large allocation on hot paths.
    ///
    /// Unknown reason strings are treated as no-match (warn to stderr).
    /// Unreadable lines (I/O errors from `.lines()`) are skipped silently.
    pub fn find_termination(&self, session_key: &str) -> Result<Option<Evidence>, LoopError> {
        // Scan project journal first.
        if let Some(ev) = Self::scan_journal(&self.project_path, session_key) {
            return Ok(Some(ev));
        }

        // Fall back to global journal when it differs from the project journal
        // and no match was found in the project journal.  This handles the
        // megawalk case where worker termination events land in a different
        // worktree's journal but are mirrored to the global file.
        if self.project_path != self.global_path {
            if let Some(ev) = Self::scan_journal(&self.global_path, session_key) {
                return Ok(Some(ev));
            }
        }

        Ok(None)
    }

    /// Scan a single journal file for the LAST termination event matching
    /// `session_key`.  Returns `None` on missing file, read errors, or no
    /// matching event (all fail-tolerant).
    fn scan_journal(path: &Path, session_key: &str) -> Option<Evidence> {
        if !path.exists() {
            return None;
        }

        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "loop-runtime: could not read journal {}: {e}",
                    path.display()
                );
                return None;
            }
        };

        let mut last_match: Option<Evidence> = None;

        for line_result in BufReader::new(file).lines() {
            // Unreadable line (I/O error mid-file) -> skip silently.
            let raw = match line_result {
                Ok(l) => l,
                Err(_) => continue,
            };
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            // Skip unparseable lines silently.
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Match type == "termination" && data.session_id == session_key.
            if v["type"].as_str() != Some("termination") {
                continue;
            }
            if v["data"]["session_id"].as_str() != Some(session_key) {
                continue;
            }

            let reason_str = match v["data"]["reason"].as_str() {
                Some(s) => s,
                None => {
                    // F1: missing reason field is authoritative-record corruption; warn loudly.
                    eprintln!(
                        "loop-runtime: termination event for {session_key} missing 'reason' field, skipping"
                    );
                    continue;
                }
            };
            let reason = match parse_termination_reason(reason_str) {
                Some(r) => r,
                None => {
                    // Unknown reason: warn and skip (fail tolerant).
                    eprintln!(
                        "loop-runtime: unknown TerminationReason '{reason_str}' in journal, skipping"
                    );
                    continue;
                }
            };
            let message = v["data"]["message"].as_str().unwrap_or("").to_string();

            last_match = Some(Evidence { reason, message });
        }

        last_match
    }
}

/// Parse a reason string into TerminationReason. Returns None for unknown values.
///
/// F7: uses serde round-trip instead of a hand-written match so that new
/// variants added in group 2 are picked up automatically without a silent desync.
fn parse_termination_reason(s: &str) -> Option<TerminationReason> {
    serde_json::from_value(Value::String(s.to_string())).ok()
}

// ── outcome types ─────────────────────────────────────────────────────────────

/// The result of closing a single unit (after its session produced a
/// TerminationReason event).
pub struct UnitResult {
    /// The unit's identifier.
    pub unit_id: String,
    /// Termination evidence that drove the close decision.
    pub evidence: Evidence,
    /// What the queue did when asked to close the unit.
    pub close: CloseOutcome,
}

/// The result of a complete walk.
pub struct LoopOutcome {
    /// Why the walk stopped (walk-level reason, not per-unit reason).
    pub reason: TerminationReason,
    /// Total iterations consumed across all units.
    pub iterations_used: u64,
    /// Per-unit results for every unit that reached the close step.
    pub units: Vec<UnitResult>,
}

// ── bg-guard refusal classifier ─────────────────────────────────────────────

/// Claude's bg-guard refusal marker. When a `/target --resume` lands on a
/// session claude still has registered as a live background agent, claude exits
/// via `exit_with_message` (exit 1) after printing a message containing this
/// phrase (e.g. "<sid> is currently running as a background agent (bg)"). The
/// walk resumes by shelling `claude --resume`, so re-dispatching just re-hits
/// the guard forever -- the x-4504 respawn loop. Match is case-insensitive.
const BG_GUARD_MARKER: &str = "running as a background agent";

/// True iff a non-termination session exit is claude's bg-guard refusal and
/// therefore must be treated as terminal (parked) rather than re-dispatched.
/// Gated on a non-zero exit AND the marker in the captured output: a bare
/// non-zero exit WITHOUT the marker stays an ordinary crash-respawn, and a clean
/// (exit 0) run that merely mentions the phrase is never suppressed.
fn is_bg_guard_refusal(exit_code: i32, output_tail: Option<&str>) -> bool {
    exit_code != 0
        && output_tail
            .map(|t| t.to_ascii_lowercase().contains(BG_GUARD_MARKER))
            .unwrap_or(false)
}

// ── main loop ─────────────────────────────────────────────────────────────────

/// Run a walk over the queue until the queue is empty, the budget is exhausted,
/// or the cancel sentinel fires.
///
/// ## Algorithm
///
/// ```text
/// loop:
///   check cancel -> Interrupted
///   unit = queue.next()  -> None: NoWork
///                        -> Err(LoopError::Pause{policy, detail}):
///                             journal walk_paused + loop_terminated(NoProgress) -> NoProgress
///   resume guard: if journal has a termination event for unit.session_key,
///     close the unit without dispatching, journal node_closed, and continue.
///   inner dispatch loop:
///     check budget -> Budget
///     check cancel -> Interrupted
///     iterations_used += 1
///     per_unit_cap check: if unit_dispatches >= cap, synthesize NoProgress park
///     emit loop_unit_dispatched
///     run session, wait
///     if journal has termination event: close unit, journal node_closed, break
///     if exit is claude's bg-guard refusal: close unit (NoProgress) + node_closed,
///       break -- do NOT re-dispatch (x-4504, AC1-ERR)
///     else: emit node_failed, continue inner loop (re-dispatch)
/// ```
///
/// ## Journal invariant
///
/// Every `journal.append` call for project events is fatal on failure.
/// An unobservable walk must not continue spending.
///
/// ## per_unit_max_dispatches
///
/// When `Some(N)`, a unit that accumulates N dispatches without a termination
/// event is synthesized a `NoProgress` Evidence and parked via `queue.close()`.
/// The walk continues to the next unit. `None` means no per-unit cap (the
/// walk-level budget is the only ceiling, as in the original degenerate policy).
pub fn run_loop(
    queue: &mut dyn Queue,
    dispatcher: &dyn Dispatcher,
    budget: &LoopBudget,
    journal: &Journal,
    cancel: &dyn Fn() -> bool,
    per_unit_max_dispatches: Option<u64>,
) -> Result<LoopOutcome, LoopError> {
    let mut iterations_used: u64 = 0;
    let mut units: Vec<UnitResult> = Vec::new();

    loop {
        // ── cancel check (outer loop top) ─────────────────────────────────
        if cancel() {
            journal.append(
                "loop_terminated",
                json!({
                    "reason": "Interrupted",
                    "iterations_used": iterations_used,
                    "units_closed": units.len(),
                }),
            )?;
            return Ok(LoopOutcome {
                reason: TerminationReason::Interrupted,
                iterations_used,
                units,
            });
        }

        // ── dequeue next unit ─────────────────────────────────────────────
        // queue.next() may return:
        //   Ok(None)  -> backlog empty -> NoWork
        //   Ok(Some)  -> dispatch this unit
        //   Err(LoopError::Pause{policy, detail}) -> walk policy pause -> NoProgress
        //   Err(other) -> hard failure (a real LoopError::Queue now falls here)
        let unit = match queue.next() {
            Ok(None) => {
                journal.append(
                    "loop_terminated",
                    json!({
                        "reason": "NoWork",
                        "iterations_used": iterations_used,
                        "units_closed": units.len(),
                    }),
                )?;
                return Ok(LoopOutcome {
                    reason: TerminationReason::NoWork,
                    iterations_used,
                    units,
                });
            }
            Ok(Some(u)) => u,
            Err(LoopError::Pause { policy, detail }) => {
                // Walk policy pause: the typed variant carries policy/detail
                // directly, so there is nothing to parse. A real LoopError::Queue
                // can no longer reach this arm (it hits the catch-all below).
                journal.append(
                    "walk_paused",
                    json!({
                        "policy": policy,
                        "detail": detail,
                        "iterations_used": iterations_used,
                        "units_closed": units.len(),
                    }),
                )?;
                journal.append(
                    "loop_terminated",
                    json!({
                        "reason": "NoProgress",
                        "iterations_used": iterations_used,
                        "units_closed": units.len(),
                    }),
                )?;
                return Ok(LoopOutcome {
                    reason: TerminationReason::NoProgress,
                    iterations_used,
                    units,
                });
            }
            Err(e) => return Err(e),
        };

        // ── resume guard (AC1-FR): check for pre-existing termination ─────
        // If a prior session for this unit already terminated (e.g. the walk
        // restarted mid-flight), close the unit without dispatching so work
        // is not duplicated.
        if let Some(evidence) = journal.find_termination(&unit.session_key)? {
            let close = queue.close(&unit, &evidence)?;
            // AC2-UI: journal node_closed for every close path.
            journal_node_closed(journal, &unit, &evidence, &close, iterations_used)?;
            units.push(UnitResult {
                unit_id: unit.id.clone(),
                evidence,
                close,
            });
            // Do NOT increment iterations_used: no dispatch happened.
            continue;
        }

        // ── inner dispatch loop ───────────────────────────────────────────
        // Re-dispatch until a TerminationReason event appears, the per-unit
        // cap is hit, or the walk-level budget is exhausted.
        let mut unit_dispatches: u64 = 0;
        loop {
            // Budget check (inner loop top, before dispatch).
            if iterations_used >= budget.max_iterations {
                journal.append(
                    "loop_terminated",
                    json!({
                        "reason": "Budget",
                        "iterations_used": iterations_used,
                        "units_closed": units.len(),
                        "axis": "iterations",
                    }),
                )?;
                return Ok(LoopOutcome {
                    reason: TerminationReason::Budget,
                    iterations_used,
                    units,
                });
            }

            // Cancel check (inner loop, before dispatch).
            if cancel() {
                journal.append(
                    "loop_terminated",
                    json!({
                        "reason": "Interrupted",
                        "iterations_used": iterations_used,
                        "units_closed": units.len(),
                    }),
                )?;
                return Ok(LoopOutcome {
                    reason: TerminationReason::Interrupted,
                    iterations_used,
                    units,
                });
            }

            // Per-unit dispatch cap: if this unit has been dispatched N times
            // without a termination event, synthesize a NoProgress park and
            // continue to the next unit.
            if let Some(cap) = per_unit_max_dispatches {
                if unit_dispatches >= cap {
                    let evidence = Evidence {
                        reason: TerminationReason::NoProgress,
                        message: format!(
                            "no termination event after {cap} dispatch(es); unit parked"
                        ),
                    };
                    let close = queue.close(&unit, &evidence)?;
                    journal_node_closed(journal, &unit, &evidence, &close, iterations_used)?;
                    units.push(UnitResult {
                        unit_id: unit.id.clone(),
                        evidence,
                        close,
                    });
                    break; // Break inner loop -> continue outer loop (next unit).
                }
            }

            iterations_used += 1;
            unit_dispatches += 1;

            // Emit loop_unit_dispatched before running the session.
            journal.append(
                "loop_unit_dispatched",
                json!({
                    "unit_id": unit.id,
                    "session_id": unit.session_key,
                    "iteration": iterations_used,
                    "title": unit.title,
                }),
            )?;

            // Launch and wait for the session.
            let mut session = dispatcher
                .run(
                    &unit,
                    &DispatchCtx {
                        iteration: iterations_used,
                    },
                )
                .map_err(|e| LoopError::Dispatch(e.to_string()))?;
            let exit_code = session.wait()?;

            // Check whether the session produced a termination event.
            if let Some(evidence) = journal.find_termination(&unit.session_key)? {
                let close = queue.close(&unit, &evidence)?;
                // AC2-UI: journal node_closed for every close path.
                journal_node_closed(journal, &unit, &evidence, &close, iterations_used)?;
                units.push(UnitResult {
                    unit_id: unit.id.clone(),
                    evidence,
                    close,
                });
                break; // Break inner loop -> continue outer loop (next unit).
            }

            // x-4504 / AC1-ERR: claude's bg-guard refusal is terminal, not a
            // crash to re-dispatch. When `/target --resume` lands on a session
            // claude still holds as a live background agent, claude refuses with
            // `exit_with_message` ("running as a background agent (bg)"). The
            // next dispatch re-runs `claude --resume` and re-hits the guard, so
            // re-dispatching is an infinite respawn loop. Park the unit instead
            // (a later native attach / detach frees the slot for a fresh walk).
            // Mirror the per-unit-cap park: close with NoProgress evidence + a
            // node_closed event whose detail identifies the bg-guard cause. Do
            // NOT emit walk_paused -- that event is reserved for QUEUE-level
            // policy pauses (schema.yaml enum: consecutive_failures|p0_failed),
            // not per-unit parks (codex peer review).
            if is_bg_guard_refusal(exit_code, session.output_tail().as_deref()) {
                let evidence = Evidence {
                    reason: TerminationReason::NoProgress,
                    message: "claude bg-guard refusal (session running as a background agent); re-dispatch halted".to_string(),
                };
                let close = queue.close(&unit, &evidence)?;
                journal_node_closed(journal, &unit, &evidence, &close, iterations_used)?;
                units.push(UnitResult {
                    unit_id: unit.id.clone(),
                    evidence,
                    close,
                });
                break; // Break inner loop -> continue outer loop (next unit).
            }

            // No termination event: emit node_failed (watchdog synthesis) and
            // re-dispatch in the next inner iteration.
            journal.append(
                "node_failed",
                json!({
                    "unit_id": unit.id,
                    "session_id": unit.session_key,
                    "iteration": iterations_used,
                    "exit_code": exit_code,
                }),
            )?;
        }
    }
}

/// Journal a `node_closed` loop event after every queue.close() call.
///
/// Fields: unit_id, session_id, reason (evidence reason string), close
/// ("closed"|"parked"|"refused"), detail (Parked/Refused string, "" for Closed),
/// iterations_used (walk iteration count at close time).
///
/// The TUI and progress-line consumers read this event to track per-unit
/// close outcomes. It is emitted on EVERY close path: resume guard, normal
/// termination, per-unit cap park, and (in megawalk) consecutive-failure park.
fn journal_node_closed(
    journal: &Journal,
    unit: &Unit,
    evidence: &Evidence,
    close: &CloseOutcome,
    iterations_used: u64,
) -> Result<(), LoopError> {
    let (close_str, detail) = match close {
        CloseOutcome::Closed => ("closed", String::new()),
        CloseOutcome::Parked(s) => ("parked", s.clone()),
        CloseOutcome::Refused(s) => ("refused", s.clone()),
    };
    let reason_str = format!("{:?}", evidence.reason);
    journal.append(
        "node_closed",
        json!({
            "unit_id": unit.id,
            "session_id": unit.session_key,
            "reason": reason_str,
            "close": close_str,
            "detail": detail,
            "iterations_used": iterations_used,
        }),
    )
}

#[cfg(test)]
mod bg_guard_tests {
    use super::is_bg_guard_refusal;

    #[test]
    fn refusal_marker_with_nonzero_exit_is_terminal() {
        let out = "abc123 is currently running as a background agent (bg). \
                   Use 'claude agents' to view it, or add --fork-session.";
        assert!(is_bg_guard_refusal(1, Some(out)));
        // Case-insensitive.
        assert!(is_bg_guard_refusal(
            1,
            Some("RUNNING AS A BACKGROUND AGENT")
        ));
    }

    #[test]
    fn bare_nonzero_exit_without_marker_is_not_terminal() {
        // An ordinary crash must still re-dispatch (not be suppressed).
        assert!(!is_bg_guard_refusal(1, Some("panic: index out of bounds")));
        assert!(!is_bg_guard_refusal(1, None));
        assert!(!is_bg_guard_refusal(137, Some("killed"))); // SIGKILL
    }

    #[test]
    fn clean_exit_is_never_a_refusal_even_with_marker() {
        // A successful run that merely mentions the phrase must not be parked.
        assert!(!is_bg_guard_refusal(
            0,
            Some("running as a background agent")
        ));
    }
}
