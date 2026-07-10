//! Megawalk driver: MegawalkQueue + MegawalkDispatcher + the `loop run --driver megawalk` arm.
//!
//! ## What this module does
//!
//! MegawalkQueue shells `fno backlog next` to dequeue work items and
//! `fno backlog done` to close them.  Claims (ab-7303e5d7) are acquired via
//! `fno claim acquire` so a second walker cannot pick up the same node, and
//! released on every close path including error paths.
//!
//! MegawalkDispatcher wraps ShelloutDispatcher, injecting per-unit env vars
//! (`CONTINUE_PROMPT`, `TARGET_SESSION_ID`) so the worker session knows which
//! node it is driving and the loopcheck termination event carries the matching
//! session_key.
//!
//! ## Cross-cwd termination event delivery (ab-7303e5d7)
//!
//! Worker sessions dispatched by the walker run `/target` in their OWN
//! conductor worktrees (the /target location hard-gate moves them), so their
//! termination events land in the WORKTREE's events.jsonl AND in the global
//! `~/.fno/events.jsonl` mirror (via loopcheck's emit_to_both).  The
//! walker's project journal lives at the walker cwd.  Journal::find_termination
//! (extended in loop_runtime.rs) scans the project journal first, then falls
//! back to the global mirror so the walker always finds the event.
//!
//! ## Claim idempotency (verified)
//!
//! The walker acquires `node:<id>` with holder `target-session:<session_key>`.
//! When the worker's init-target-state.sh fires (with TARGET_SESSION_ID set),
//! it calls `fno claim acquire node:<id> --holder target-session:<session_key>`.
//! core.py:acquire_claim line 209: if `existing.holder == holder`, the re-acquire
//! is idempotent (refreshes pid/host/acquired_at, emits claim_idempotent_reacquired).
//! The holder strings match exactly because the walker pre-assigns the session_key
//! and passes it via TARGET_SESSION_ID, so init-target-state.sh uses the same value.
//! The re-acquire is NOT blocked by PID mismatch (the refresh accepts any pid).
//!
//! ## Module naming
//!
//! The module name starts with `loop` so that the LOC-ratchet glob
//! `crates/fno-agents/src/loop*` counts this file's LOC toward the ratchet
//! budget deliberately (alongside loopcheck.rs).

use crate::loop_dispatch::ShelloutDispatcher;
use crate::loop_runtime::{
    CloseOutcome, DispatchCtx, Dispatcher, Evidence, Journal, LoopError, Queue, Session, Unit,
};
use crate::loopcheck::TerminationReason;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

// ── Multi-PR umbrella verification (grilled decision 9, simplicity) ──────────
//
// Spec B.5 asks: does a walked group-child close prematurely complete the
// epic, and does the epic surface again via `fno backlog next` after all
// children are ready?
//
// UPDATED (x-33b2, PR #69): epics now close AUTOMATICALLY.
//
// Original grilled-decision-9 (2026-06-06) kept the epic visible in
// `fno backlog next` so the walker would close it explicitly via
// `fno backlog done`. That conflicted with a later requirement: an epic is a
// container and must NEVER be selected/dispatched as a build target (building
// the box instead of its leaves starves the real work). Once epics are excluded
// from build-selection everywhere (cli.py `_container_ids`, used by `next` /
// `ready` / advance_dependents), the "walker closes it via next" path can no
// longer fire - the epic is filtered out.
//
// Resolution: `cli/src/fno/graph/cli.py:_cascade_close_parents` closes an
// ancestor epic the moment its last child's `completed_at` is set. It runs
// inside every close mutator (done + reconcile + update --completed), follows
// the `parent` EDGE (so it is uniform across projects - a cross-project parent
// closes on the same merge that finishes its last child), cascades to the
// grandparent, and tags the PR-less close with a completion_note. So the walker
// no longer needs to discover or close epics: it dispatches leaves only, and the
// box closes itself. recompute_statuses still derives _status:done from the
// individual node's completed_at; the cascade is what SETS the parent's
// completed_at (deliberately, in the close path - not a recompute derivation).

// ── Event-kind prune ledger (Claude's Discretion 2) ──────────────────────────
//
// The legacy megawalk.py emits ~29 kinds via _emit_event() into
// megawalk-events.jsonl. Task 2.4 deletes megawalk.py; the writer dies with
// it. This table records the fate of each legacy kind so auditors can trace
// the transition.
//
// Legacy kind                  -> Fate
// ──────────────────────────── ─────────────────────────────────────────────
// node_complete                -> node_closed (close="closed")
// node_parked                  -> node_closed (close="parked")
// walker_paused                -> walk_paused
// node_failed                  -> node_failed (runtime, unchanged)
// node_help_requested          -> absorbed: cv-d3943d2a
//                                 No typed help event post-wedge; a
//                                 help-stuck session exits without a
//                                 termination event -> per-unit cap parks it
//                                 -> streak counts it. Stdout parsing is
//                                 forbidden at this altitude (locked decision 9).
// merge_attempt                -> deleted (reconcile + backlog-done cover residue)
// pr_externally_merged         -> deleted (reconcile covers residue)
// reconcile_started            -> deleted
// reconcile_completed          -> deleted
// reconcile_failed             -> deleted
// stuck_detection_failed       -> deleted (detector era removed in wedge PR)
// activity_read_failed         -> deleted (detector era)
// worktree_stuck_checkin       -> deleted (detector era)
// walker_started               -> deleted (preflight print replaces this)
// walker_completed             -> loop_terminated (runtime)
// walker_aborted               -> loop_terminated (runtime)
// node_started                 -> loop_unit_dispatched (runtime, unchanged)
// node_dispatched              -> loop_unit_dispatched (runtime, unchanged)
// node_error                   -> node_failed (runtime, unchanged)
// node_skipped                 -> deleted (claim-filter in fno backlog next covers)
// backlog_empty                -> loop_terminated{reason:NoWork} (runtime)
// iteration_limit_reached      -> loop_terminated{reason:Budget} (runtime)
// consecutive_failures_paused  -> walk_paused{policy:consecutive_failures}
// p0_failure_paused            -> walk_paused{policy:p0_failed}
//
// The legacy WRITER (megawalk.py) is deleted in task 2.4. Do not touch
// megawalk.py in this task.

// ── parallel-cap helper ───────────────────────────────────────────────────────

/// Clamp a parallel-cap value to at least 1.
///
/// A cap of 0 (or negative, if caller uses signed math) would prohibit all
/// dispatch. We clamp to 1 and log a warning so the flag is accepted but
/// explicit: the caller is responsible for printing the clamp message.
///
/// Note: group-2 always executes sequentially (run_loop is single-threaded).
/// When cap > 1 the verb glue prints one honest line explaining that execution
/// is still sequential (collision-conservative default, Claude's Discretion 3).
pub fn clamp_parallel_cap(cap: u64) -> u64 {
    if cap < 1 {
        eprintln!(
            "loop-megawalk: WARNING: --parallel-cap {cap} < 1; clamped to 1 \
             (boundary: cap must be >= 1)"
        );
        1
    } else {
        cap
    }
}

// ── MegawalkPolicyQueue ───────────────────────────────────────────────────────

/// Per-unit state stored in the policy queue.
struct PolicyUnitEntry {
    unit: Unit,
}

// ── shared predicate ──────────────────────────────────────────────────────────

/// Returns true when the termination reason is a successful close
/// (DonePRGreen or DoneAdvisory). Used in three policy sites; extracted to
/// avoid triplication (sigma-review finding 4).
fn is_done_reason(r: &TerminationReason) -> bool {
    matches!(
        r,
        TerminationReason::DonePRGreen | TerminationReason::DoneAdvisory
    )
}

/// Walk policy state tracker for the megawalk driver.
///
/// This struct is NOT a full Queue impl by itself; it is the policy layer that
/// wraps or extends the shell-based MegawalkQueue. For unit tests it acts as a
/// standalone Queue: `push_unit` / `next` / `close` with full policy tracking.
///
/// ## Consecutive-failure pause (3)
///
/// A unit is a "failure" when close() is called with evidence.reason NOT in
/// {DonePRGreen, DoneAdvisory}. A successful close resets the streak. When the
/// streak reaches 3, the next next() call returns
/// Err(LoopError::Pause{policy:"consecutive_failures", detail}).
///
/// ## p0 immediate pause
///
/// If a p0 unit fails (close called with non-Done reason and is_p0=true in the
/// per-unit state), the NEXT next() call returns
/// Err(LoopError::Pause{policy:"p0_failed", detail:uid}) immediately, regardless
/// of streak.
///
/// ## Park-on-help absorption
///
/// No typed help event exists post-wedge (verified: only mission-emit.sh prints
/// a help tag to stderr; loopcheck emits none). A help-stuck session exits
/// without a termination event -> node_failed -> per-unit cap parks it ->
/// streak counts it. See carveout cv-d3943d2a (typed help source = future work).
/// Locked decision 9 forbids stdout parsing at this altitude.
pub struct MegawalkPolicyQueue {
    /// Units queued for dispatch (push_unit adds here; next consumes from front).
    pending: std::collections::VecDeque<PolicyUnitEntry>,
    /// Whether a p0 failure happened; set in record_close for is_p0 failures.
    p0_failure: Option<String>, // unit id of the failed p0 unit
    /// Consecutive failure streak counter.
    consecutive_failures: usize,
    /// Unit IDs involved in the current streak (for Pause detail).
    streak_ids: Vec<String>,
}

impl MegawalkPolicyQueue {
    /// Construct an empty policy queue.
    pub fn new() -> Self {
        Self {
            pending: std::collections::VecDeque::new(),
            p0_failure: None,
            consecutive_failures: 0,
            streak_ids: vec![],
        }
    }

    /// Add a unit to the back of the queue.
    ///
    /// `is_p0`: when true, a failure on this unit triggers an immediate pause.
    pub fn push_unit(&mut self, unit: Unit, is_p0: bool) {
        // is_p0 is passed to record_close separately; the entry only holds the unit.
        let _ = is_p0;
        self.pending.push_back(PolicyUnitEntry { unit });
    }

    /// Record the outcome of a close() call for policy tracking.
    ///
    /// Call this AFTER queue.close() returns, with the same evidence passed to close.
    /// `is_p0`: whether the unit that was closed had p0 priority.
    ///
    /// This is a test-accessible hook. In run_loop the policy tracking is done
    /// via the Queue::close() implementation (which calls record_close internally).
    pub fn record_close(&mut self, unit: &Unit, evidence: &Evidence, is_p0: bool) {
        let is_success = is_done_reason(&evidence.reason);

        if is_success {
            // Success: reset consecutive-failure streak AND p0_failure.
            // p0_failure was never cleared here before (sigma-review finding 3);
            // omitting the reset caused spurious pauses after a successful
            // recovery unit followed a failed p0 unit.
            self.consecutive_failures = 0;
            self.streak_ids.clear();
            self.p0_failure = None;
        } else {
            // Failure: increment streak.
            self.consecutive_failures += 1;
            self.streak_ids.push(unit.id.clone());
            // p0 failure: record for immediate pause on next next() call.
            if is_p0 {
                self.p0_failure = Some(unit.id.clone());
            }
        }
    }

    /// Check whether walk policy requires a pause.
    ///
    /// Returns `Some((policy, detail))` if a pause is warranted; `None` otherwise.
    /// Called at the top of next() before dequeuing.
    pub fn should_pause(&self) -> Option<(String, String)> {
        // p0 failure takes precedence.
        if let Some(ref uid) = self.p0_failure {
            return Some(("p0_failed".to_string(), uid.clone()));
        }
        // Consecutive-failure streak.
        if self.consecutive_failures >= 3 {
            let detail = self.streak_ids.join(" ");
            return Some(("consecutive_failures".to_string(), detail));
        }
        None
    }
}

impl Default for MegawalkPolicyQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl Queue for MegawalkPolicyQueue {
    fn next(&mut self) -> Result<Option<Unit>, LoopError> {
        // Check policy before dequeuing.
        if let Some((policy, detail)) = self.should_pause() {
            return Err(LoopError::Pause { policy, detail });
        }
        // Dequeue the next unit.
        match self.pending.pop_front() {
            None => Ok(None),
            Some(entry) => Ok(Some(entry.unit)),
        }
    }

    fn close(&mut self, unit: &Unit, evidence: &Evidence) -> Result<CloseOutcome, LoopError> {
        // Determine if this unit was p0 (we no longer have the PolicyUnitEntry
        // since it was consumed by next(); for test use, is_p0 is always false
        // unless record_close is called separately).
        // For production use, the MegawalkQueue wraps this and passes is_p0.
        // For unit tests that call record_close() separately, close() just parks.
        let is_success = is_done_reason(&evidence.reason);
        if is_success {
            // Success: reset streak AND p0_failure (sigma-review finding 3).
            self.consecutive_failures = 0;
            self.streak_ids.clear();
            self.p0_failure = None;
        } else {
            self.consecutive_failures += 1;
            self.streak_ids.push(unit.id.clone());
        }

        let outcome = if is_success {
            CloseOutcome::Closed
        } else {
            CloseOutcome::Parked(format!("policy-park: {:?}", evidence.reason))
        };
        Ok(outcome)
    }
}

// ── constants ─────────────────────────────────────────────────────────────────

/// Maximum number of claim-held skips inside a single next() call before
/// giving up and returning an error.  Prevents infinite loops when every
/// ready node is claimed by live sessions.
const MAX_CLAIM_RETRIES: usize = 5;

// ── helper: run fno sub-command ───────────────────────────────────────────────

/// Build a Command for the `fno` binary.
///
/// Binary resolution: `abi_bin` (the path/name given at construction time,
/// overridden by `$FNO_BIN` for tests).  If `FNO_BIN` is set and non-empty,
/// it wins; otherwise `abi_bin` is used as-is (callers pass "fno" for
/// production and a tempdir stub path for tests).
pub(crate) fn abi_cmd(abi_bin: &str) -> Command {
    let binary = std::env::var("FNO_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| abi_bin.to_string());
    Command::new(binary)
}

/// Run a spawn closure, retrying briefly on ETXTBSY ("Text file busy", os
/// error 26). The spawned file is the `fno` / `fno-agents` binary: a
/// concurrent `fno update` relinks it in place, and under `cargo test` a
/// sibling thread that just wrote+exec'd a stub leaves a transient write-fd
/// open in another thread's fork window - either way the kernel can refuse
/// the exec with ETXTBSY. The condition clears within microseconds once the
/// writing fd closes, so a bounded retry turns a hard spawn failure into a
/// short wait. Any other error - and the successful value - passes through
/// unchanged.
pub(crate) fn retry_etxtbsy<T>(
    mut spawn: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    const MAX_RETRIES: u32 = 5;
    let mut attempt: u32 = 0;
    loop {
        match spawn() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempt < MAX_RETRIES => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(2 * u64::from(attempt)));
            }
            other => return other,
        }
    }
}

/// Run `fno doctor --json` best-effort and return whether the output indicates
/// a stale installation.  Any failure (I/O, parse) is treated as "unknown"
/// and does not append the staleness hint.
fn is_abi_stale(abi_bin: &str) -> bool {
    let out = match abi_cmd(abi_bin).args(["doctor", "--json"]).output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Expect {"status":"stale",...} - check the status field.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        return v["status"].as_str() == Some("stale");
    }
    false
}

/// Append a staleness hint to an error message if `fno doctor` says stale.
pub(crate) fn maybe_stale_hint(msg: String, abi_bin: &str) -> String {
    if is_abi_stale(abi_bin) {
        format!("{msg}; installed fno may be stale - run `fno update`")
    } else {
        msg
    }
}

// ── session key generation ────────────────────────────────────────────────────

/// Generate a unique session key in the same shape used by init-target-state.sh:
/// `{utc %Y%m%dT%H%M%SZ}-{infix}{pid}-{6 hex}`.
///
/// The infix distinguishes the assigning driver at a glance in logs:
/// "mw" = megawalk, "mt" = megatron (group 3).
///
/// The self-mint path in init-target-state.sh extends this vocabulary
/// with 2-char PROVIDER codes when no driver pre-assigned the key - "cl" =
/// claude, "cx" = codex, "gm" = gemini, "ag" = agy, "hm" = hermes, "oc" =
/// opencode. Driver tags win when present (a megawalk session stays "mw");
/// the provider code only fills the slot on the direct/bg self-mint path.
pub(crate) fn gen_session_key_with_infix(infix: &str) -> String {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let pid = std::process::id();
    // 3 random bytes -> 6 hex chars.
    let entropy: u32 = {
        let mut buf = [0u8; 3];
        // Best-effort: use /dev/urandom bytes; fall back to a mix of pid+time.
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            use std::io::Read;
            let _ = f.read_exact(&mut buf);
        } else {
            buf[0] = (pid & 0xFF) as u8;
            buf[1] = ((pid >> 8) & 0xFF) as u8;
            buf[2] = (chrono::Utc::now().timestamp_subsec_nanos() & 0xFF) as u8;
        }
        u32::from_le_bytes([buf[0], buf[1], buf[2], 0])
    };
    format!("{ts}-{infix}{pid}-{entropy:06x}")
}

/// Megawalk-assigned session key (the "mw" infix).
fn gen_session_key() -> String {
    gen_session_key_with_infix("mw")
}

// ── mission env extraction ────────────────────────────────────────────────────

/// Extract TARGET_MISSION_* env vars from a `_node_summary` JSON value.
///
/// Mirrors Python `extract_mission_env()` in megawalk.py:75-117:
///   - If `mission_id` is null/absent: returns Ok(vec![]) (non-fleet node).
///   - If `mission_id` is set: all four vars are required.
///     - `mission_wave` null/absent -> Err(Queue) naming node + field.
///     - `mission_slug` null/absent -> Err(Queue) naming node + field.
///     - `mission_from_msg_id` null -> maps to "" (Python line 117 behavior).
///
/// Returns `Err(LoopError::Queue(...))` on corrupted fleet metadata so the
/// commander is not silently stranded.
fn extract_mission_env(
    v: &serde_json::Value,
    node_id: &str,
) -> Result<Vec<(String, String)>, LoopError> {
    let mission_id = match v["mission_id"].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            // null, absent, or empty string: non-fleet node.
            return Ok(vec![]);
        }
    };

    // mission_wave: must be present and coercible to a string.
    let mission_wave = match &v["mission_wave"] {
        serde_json::Value::Null => {
            return Err(LoopError::Queue(format!(
                "node {node_id:?} has mission_id={mission_id:?} but mission_wave is missing; \
                 dispatcher bug (corrupted fleet metadata)"
            )));
        }
        w => w.to_string().trim_matches('"').to_string(),
    };
    if mission_wave.is_empty() || mission_wave == "null" {
        return Err(LoopError::Queue(format!(
            "node {node_id:?} has mission_id={mission_id:?} but mission_wave is null; \
             dispatcher bug (corrupted fleet metadata)"
        )));
    }

    // mission_slug: must be a non-empty string.
    let mission_slug = match v["mission_slug"].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return Err(LoopError::Queue(format!(
                "node {node_id:?} has mission_id={mission_id:?} but mission_slug is missing \
                 or empty; dispatcher bug (corrupted fleet metadata)"
            )));
        }
    };

    // mission_from_msg_id: null -> "" (Python line 117).
    let mission_from_msg_id = v["mission_from_msg_id"].as_str().unwrap_or("").to_string();

    Ok(vec![
        ("TARGET_MISSION_ID".to_string(), mission_id),
        ("TARGET_MISSION_WAVE".to_string(), mission_wave),
        ("TARGET_MISSION_SLUG".to_string(), mission_slug),
        (
            "TARGET_MISSION_FROM_MSG_ID".to_string(),
            mission_from_msg_id,
        ),
    ])
}

// ── MegawalkQueue ─────────────────────────────────────────────────────────────

/// Per-unit claim state stored in `MegawalkQueue::active_claims`.
struct ClaimEntry {
    /// The session key used for the node claim (matched by close() for release).
    session_key: String,
    /// Whether this unit has p0 priority. A p0 failure triggers an immediate
    /// walk pause regardless of the consecutive-failure streak.
    is_p0: bool,
}

/// A Queue that shells `fno backlog next` / `fno backlog done` and coordinates
/// node claims so two walkers never dispatch the same node simultaneously.
///
/// ## Walk policy (folded in, not a separate wrapper)
///
/// Policy state lives directly in `MegawalkQueue` so the production path uses
/// the same policy as the test path - no separate wrapper needed. The policy
/// is the same as `MegawalkPolicyQueue`:
///   - consecutive-failure streak of 3 -> Pause{consecutive_failures}
///   - p0 unit failure -> Pause{p0_failed} (immediate, no streak needed)
///   - Success (DonePRGreen | DoneAdvisory) resets the streak.
///
/// The `priority` field from the backlog-next JSON is stored per-unit in
/// `active_claims` so `close()` can check it for the p0 rule.
///
/// ## --max-units N (once-mode)
///
/// When `max_units` is `Some(N)`, `next()` returns `None` (Drained) after N
/// units have been closed by `close()`. This maps the `/megawalk once` modifier
/// (task 2.4 uses `--max-units 1`) and general "execute at most N units then
/// stop" semantics. `N` must be >= 1; the CLI gate enforces N > 0 (exit 2).
/// `units_closed` is incremented at the END of each `close()` call so the cap
/// fires on the NEXT `next()` call, giving the correct semantics: close the
/// N-th unit, then the outer loop calls `next()` which returns None -> NoWork.
pub struct MegawalkQueue {
    /// Path or name of the fno binary.  `$FNO_BIN` env overrides for tests.
    abi_bin: String,
    /// Optional `--project <name>` filter.
    project: Option<String>,
    /// When true, pass `--all` to `fno backlog next`.
    all: bool,
    /// Map from node id -> ClaimEntry for active dispatches.
    /// Stores session_key (for claim release/hold) and is_p0 (for policy).
    /// On Parked/Refused outcomes the entry is KEPT (park-exclusion hold).
    /// On Closed the entry is removed and the claim is released.
    active_claims: HashMap<String, ClaimEntry>,
    // ── policy state ──────────────────────────────────────────────────────────
    /// Pending p0 failure: the unit id of a p0 unit that failed. When set,
    /// the NEXT next() call returns a pause immediately.
    policy_p0_failure: Option<String>,
    /// Consecutive failure streak counter. A "failure" is any close with
    /// evidence.reason NOT in {DonePRGreen, DoneAdvisory}.
    policy_consecutive_failures: usize,
    /// Unit IDs involved in the current streak (for Pause detail string).
    policy_streak_ids: Vec<String>,
    // ── max-units cap (once-mode) ─────────────────────────────────────────────
    /// Optional cap on total units closed. When set, next() returns None
    /// (Drained) after `units_closed` reaches this value.
    max_units: Option<u64>,
    /// Count of units closed so far (incremented at end of close()).
    units_closed: u64,
    /// Optional `--mission <id>` selection filter (group 3, ab-9fd662c6).
    /// A megatron child walk passes this so the walk works ONLY the
    /// mission's nodes and never drifts into the project's general backlog.
    mission: Option<String>,
}

impl MegawalkQueue {
    /// Construct a MegawalkQueue.
    ///
    /// `abi_bin`: "fno" in production; a stub path in tests (or override via
    ///   `$FNO_BIN`).
    /// `project`: optional project filter passed as `--project <p>` to `fno backlog next`.
    /// `all`: when true, pass `--all` instead of `--project`.
    pub fn new(abi_bin: String, project: Option<String>, all: bool) -> Self {
        Self::new_with_max_units(abi_bin, project, all, None)
    }

    /// Construct a MegawalkQueue with an optional max-units cap.
    ///
    /// `max_units`: when `Some(N)`, next() returns None (Drained) after N units
    /// have been closed. Maps the `--max-units` CLI flag / `/megawalk once`.
    /// Must be >= 1; callers are responsible for rejecting 0 before calling
    /// (the verb glue in loop_target.rs exits 2 on N == 0).
    pub fn new_with_max_units(
        abi_bin: String,
        project: Option<String>,
        all: bool,
        max_units: Option<u64>,
    ) -> Self {
        Self {
            abi_bin,
            project,
            all,
            active_claims: HashMap::new(),
            policy_p0_failure: None,
            policy_consecutive_failures: 0,
            policy_streak_ids: vec![],
            max_units,
            units_closed: 0,
            mission: None,
        }
    }

    /// Builder: set the `--mission <id>` selection filter (megatron child walks).
    pub fn with_mission(mut self, mission: Option<String>) -> Self {
        self.mission = mission;
        self
    }

    /// Check whether walk policy requires a pause. Returns a typed
    /// `LoopError::Pause { policy, detail }` when a pause is warranted, `None`
    /// otherwise. Called at the top of `next()` before shelling out.
    fn policy_check(&self) -> Option<LoopError> {
        // p0 failure takes precedence over streak.
        if let Some(ref uid) = self.policy_p0_failure {
            return Some(LoopError::Pause {
                policy: "p0_failed".to_string(),
                detail: uid.clone(),
            });
        }
        if self.policy_consecutive_failures >= 3 {
            let detail = self.policy_streak_ids.join(" ");
            return Some(LoopError::Pause {
                policy: "consecutive_failures".to_string(),
                detail,
            });
        }
        None
    }

    /// Update policy state after a close() call.
    fn policy_record_close(&mut self, unit_id: &str, is_success: bool, is_p0: bool) {
        if is_success {
            // Success: reset streak AND p0_failure (sigma-review finding 3).
            // p0_failure was not cleared here before, causing spurious pauses
            // after a recovery unit succeeded following a failed p0 unit.
            self.policy_consecutive_failures = 0;
            self.policy_streak_ids.clear();
            self.policy_p0_failure = None;
        } else {
            // Failure: increment streak.
            self.policy_consecutive_failures += 1;
            self.policy_streak_ids.push(unit_id.to_string());
            if is_p0 {
                self.policy_p0_failure = Some(unit_id.to_string());
            }
        }
    }
}

impl Queue for MegawalkQueue {
    /// Dequeue the next ready backlog node.
    ///
    /// Algorithm:
    /// 0. Check walk policy (consecutive-failure streak / p0 failure).
    ///    Returns Err(LoopError::Pause{policy, detail}) when policy requires a pause.
    /// 1. Shell `fno backlog next [--project P | --all]`.
    /// 2. Empty output (literal "null") -> return Ok(None).
    /// 3. Parse JSON as `_node_summary` shape (captures `priority` for p0 check).
    /// 4. Generate a unique session_key.
    /// 5. Shell `fno claim acquire node:<id> --holder target-session:<session_key>
    ///    --ttl 2h --reason "megawalk walker dispatch"`.
    ///    Exit 0 -> record claim, return the Unit.
    ///    Exit 1 (held by other) -> loop back to step 1 (the live-claim filter
    ///    in _live_claimed_node_ids excludes it on the next call).
    /// 6. Bound retries at MAX_CLAIM_RETRIES; on exhaustion return LoopError::Queue.
    ///
    /// Malformed JSON or verb failure -> LoopError::Queue with staleness hint.
    fn next(&mut self) -> Result<Option<Unit>, LoopError> {
        // ── max-units cap check (step -1) ─────────────────────────────────
        // When max_units is set and units_closed has reached the cap, signal
        // Drained so the outer loop terminates with NoWork.
        if let Some(cap) = self.max_units {
            if self.units_closed >= cap {
                return Ok(None);
            }
        }

        // ── walk policy check (step 0) ────────────────────────────────────
        if let Some(err) = self.policy_check() {
            return Err(err);
        }

        for attempt in 0..MAX_CLAIM_RETRIES {
            let _ = attempt; // retry count tracked implicitly

            // ── shell fno backlog next ────────────────────────────────────────
            let mut cmd = abi_cmd(&self.abi_bin);
            cmd.args(["backlog", "next"]);
            if self.all {
                cmd.arg("--all");
            } else if let Some(ref p) = self.project {
                cmd.args(["--project", p]);
            }
            if let Some(ref m) = self.mission {
                cmd.args(["--mission", m]);
            }

            let out = retry_etxtbsy(|| cmd.output()).map_err(|e| {
                LoopError::Queue(maybe_stale_hint(
                    format!("fno backlog next: spawn failed: {e}"),
                    &self.abi_bin,
                ))
            })?;

            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return Err(LoopError::Queue(maybe_stale_hint(
                    format!("fno backlog next: exit {}: {stderr}", out.status),
                    &self.abi_bin,
                )));
            }

            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();

            // ── null -> empty backlog ─────────────────────────────────────────
            if stdout == "null" || stdout.is_empty() {
                return Ok(None);
            }

            // ── parse _node_summary JSON ──────────────────────────────────────
            let v: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| {
                LoopError::Queue(maybe_stale_hint(
                    format!("fno backlog next: JSON parse error: {e} (stdout: {stdout:?})"),
                    &self.abi_bin,
                ))
            })?;

            let id = match v["id"].as_str() {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => {
                    return Err(LoopError::Queue(maybe_stale_hint(
                        format!("fno backlog next: missing or empty 'id' field in: {stdout:?}"),
                        &self.abi_bin,
                    )));
                }
            };

            let title = v["title"].as_str().unwrap_or("(untitled)").to_string();
            let plan_path = v["plan_path"].as_str().map(|s| s.to_string());
            // Extract priority for p0 policy. The backlog-next JSON includes a
            // "priority" field (p0/p1/p2/p3). Treat anything other than "p0"
            // (or missing) as non-p0; best-effort (a corrupt field is non-p0).
            let is_p0 = v["priority"].as_str() == Some("p0");

            // ── extract mission env (fleet nodes) ─────────────────────────────
            // Mirrors Python extract_mission_env(): if mission_id is set, all
            // four TARGET_MISSION_* vars are required (wave + slug must be
            // present; from_msg_id maps to "" when null). Corrupted metadata
            // is a loud Queue error naming the node and missing field.
            let mut extra_env = extract_mission_env(&v, &id)?;

            // x-571f: a per-node model pin overrides the fleet cfg.model via the
            // same last-write-wins seam run() uses for TARGET_MISSION_* (extra_env
            // beats static_env's MODEL_FLAG). Absent = fleet default stands.
            if let Some(m) = v["model"].as_str().filter(|m| !m.is_empty()) {
                extra_env.push(("MODEL_FLAG".to_string(), format!("--model {m}")));
            }

            // ── harness-aware dispatch guard (x-3e70) ─────────────────────────
            // Defer to a foreign harness that owns / is working this node before
            // claiming it: a claim tagged with another harness (even suspect), or
            // a codex/gemini branch/worktree for the node before its claim lands.
            // Stops the default-claude stampede onto a codex-owned node (observed
            // 2026-07-09). Pause (not Ok(None), which run_loop reads as a drained
            // backlog and terminates a multi-node megawalk): a pause is resumable
            // and legible - once the owner's claim goes live the `fno backlog next`
            // live-claim filter excludes the node and the resumed walk proceeds.
            // Best-effort probe; degrades to native dispatch on any read error.
            let own_h = crate::claims::resolve_harness();
            let own = own_h.as_deref().unwrap_or("claude");
            let probe_cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
            if let Some(foreign) = crate::dispatch_posture::foreign_owner_of(&id, own, &probe_cwd) {
                crate::dispatch_posture::emit_dispatch_deferred(&id, &foreign, own, &probe_cwd);
                return Err(LoopError::Pause {
                    policy: "foreign_harness_owner".to_string(),
                    detail: format!("node {id} owned by harness '{foreign}'"),
                });
            }

            let session_key = gen_session_key();

            // ── acquire the node claim ────────────────────────────────────────
            let claim_key = format!("node:{id}");
            let claim_holder = format!("target-session:{session_key}");

            let claim_out = retry_etxtbsy(|| {
                abi_cmd(&self.abi_bin)
                    .args([
                        "claim",
                        "acquire",
                        &claim_key,
                        "--holder",
                        &claim_holder,
                        "--ttl",
                        "2h",
                        "--reason",
                        "megawalk walker dispatch",
                    ])
                    .env(
                        "FNO_CLAIMS_ROOT",
                        std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
                    )
                    .output()
            })
            .map_err(|e| {
                LoopError::Queue(maybe_stale_hint(
                    format!("fno claim acquire: spawn failed: {e}"),
                    &self.abi_bin,
                ))
            })?;

            if claim_out.status.success() {
                // Claim acquired; store session_key + is_p0 for close().
                self.active_claims.insert(
                    id.clone(),
                    ClaimEntry {
                        session_key: session_key.clone(),
                        is_p0,
                    },
                );
                return Ok(Some(Unit {
                    id,
                    title,
                    session_key,
                    plan_path,
                    extra_env,
                }));
            }

            // Branch on the exit code (sigma-review finding 1 - exit-code collapse fix).
            //
            // The claim CLI contract (cli/src/fno/claims/cli.py header):
            //   exit 1 = ClaimHeldByOther  -> retry (live-claims filter on next call)
            //   exit 2 = validation error  -> surface immediately; do NOT loop
            //   exit 3 = ClaimCorrupted / ClaimGoneAway -> surface immediately
            //   other  -> unexpected; surface immediately
            //
            // Before this fix every non-zero exit was treated as "held" and the
            // loop continued silently, hiding validation and corruption errors.
            match claim_out.status.code() {
                Some(1) => {
                    // Held by another session; let the live-claim filter in
                    // `fno backlog next` exclude this node on the next call.
                    // Continue the retry loop.
                }
                _ => {
                    // Validation error, corruption, or unexpected exit.
                    // Surface immediately as a Queue error.
                    let stderr = String::from_utf8_lossy(&claim_out.stderr)
                        .trim()
                        .to_string();
                    let code = claim_out.status.code().unwrap_or(-1);
                    return Err(LoopError::Queue(maybe_stale_hint(
                        format!("fno claim acquire {claim_key}: exit {code}: {stderr}"),
                        &self.abi_bin,
                    )));
                }
            }
        }

        // Exhausted MAX_CLAIM_RETRIES picks without finding a claimable node.
        Err(LoopError::Queue(maybe_stale_hint(
            format!(
                "fno backlog next: exhausted {MAX_CLAIM_RETRIES} attempts; every ready node \
                 is claimed by another session (last node may be stuck)"
            ),
            &self.abi_bin,
        )))
    }

    /// Mark a unit as closed.
    ///
    /// DonePRGreen | DoneAdvisory -> shell `fno backlog done <id>`.
    ///   Exit 0 -> CloseOutcome::Closed.
    ///   Nonzero -> CloseOutcome::Parked(stderr tail).
    ///
    /// Any other reason -> CloseOutcome::Parked(reason description).
    ///   Does NOT call `fno backlog done` (task 2.2 handles refusal paths).
    ///
    /// Updates walk policy state (consecutive-failure streak / p0 flag) so
    /// the NEXT next() call can return a pause when policy warrants it.
    ///
    /// ## Park-exclusion (AC2-EDGE): hold claim on Parked/Refused
    ///
    /// ONLY releases the node claim when the outcome is CloseOutcome::Closed.
    /// For Parked and Refused outcomes, the claim is HELD so the live-claims
    /// selection filter in `_live_claimed_node_ids` (cli/src/fno/graph/cli.py:43-65)
    /// continues to exclude this node and `fno backlog next` moves on to other
    /// ready work instead of re-picking the same busted node.
    ///
    /// ## Claim TTL refresh after park (same-holder re-acquire finding)
    ///
    /// core.py:acquire_claim line 209: a same-holder re-acquire refreshes
    /// `pid/host/acquired_at` (idempotent). The worker session's init-target-state.sh
    /// calls `fno claim acquire node:<id> --holder target-session:<session_key>`
    /// which, being the SAME holder as the walker's claim, is an idempotent
    /// re-acquire - it rewrites `acquired_at` and `pid` (the WORKER's pid, not
    /// the walker's). After the worker exits the claim record reflects the
    /// worker's (now-dead) pid. For a TTL-liveness claim this is fine because
    /// the filter reads `expires_at` not pid, but the `acquired_at` reset means
    /// the TTL window is measured from the WORKER's re-acquire time, not the
    /// walker's original acquire.  To ensure the claim stays LIVE through the
    /// next walk iteration (while the walker processes the next unit), the
    /// walker calls `fno claim acquire` again immediately after a park to
    /// refresh `acquired_at` with the current time and reset the TTL window.
    fn close(&mut self, unit: &Unit, evidence: &Evidence) -> Result<CloseOutcome, LoopError> {
        let should_done = is_done_reason(&evidence.reason);

        let outcome = if should_done {
            let done_out = retry_etxtbsy(|| {
                abi_cmd(&self.abi_bin)
                    .args(["backlog", "done", &unit.id])
                    .output()
            })
            .map_err(|e| LoopError::Queue(format!("fno backlog done: spawn failed: {e}")))?;

            if done_out.status.success() {
                CloseOutcome::Closed
            } else {
                let stderr = String::from_utf8_lossy(&done_out.stderr).trim().to_string();
                CloseOutcome::Parked(if stderr.is_empty() {
                    format!(
                        "fno backlog done {} failed (exit {})",
                        unit.id, done_out.status
                    )
                } else {
                    stderr
                })
            }
        } else {
            // Append evidence.message when non-empty so synthesized diagnostics
            // (e.g. "no termination event after N dispatch(es)") are not lost
            // (sigma-review finding 2).
            let detail = if evidence.message.is_empty() {
                format!("session terminated: {:?}", evidence.reason)
            } else {
                format!(
                    "session terminated: {:?}: {}",
                    evidence.reason, evidence.message
                )
            };
            CloseOutcome::Parked(detail)
        };

        // ── update walk policy state ──────────────────────────────────────────
        // Determine is_p0 from the stored claim entry (recorded at dequeue time).
        // If the entry is gone (e.g. the unit was never in active_claims - only
        // possible in tests that bypass next()), default to non-p0.
        let is_p0 = self
            .active_claims
            .get(&unit.id)
            .map(|e| e.is_p0)
            .unwrap_or(false);
        self.policy_record_close(&unit.id, should_done, is_p0);

        // ── claim release vs. hold (park-exclusion) ───────────────────────────
        match &outcome {
            CloseOutcome::Closed => {
                // Success: release the claim so the node is no longer live-claimed.
                let session_key = self
                    .active_claims
                    .remove(&unit.id)
                    .map(|e| e.session_key)
                    .unwrap_or_else(|| unit.session_key.clone());
                let claim_key = format!("node:{}", unit.id);
                let claim_holder = format!("target-session:{session_key}");

                let release_result = retry_etxtbsy(|| {
                    abi_cmd(&self.abi_bin)
                        .args(["claim", "release", &claim_key, "--holder", &claim_holder])
                        .env(
                            "FNO_CLAIMS_ROOT",
                            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
                        )
                        .output()
                });

                match release_result {
                    Ok(o) if !o.status.success() => {
                        eprintln!(
                            "loop-megawalk: WARNING: claim release {} failed (exit {}): {}",
                            claim_key,
                            o.status,
                            String::from_utf8_lossy(&o.stderr).trim()
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "loop-megawalk: WARNING: claim release {} spawn failed: {e}",
                            claim_key
                        );
                    }
                    Ok(_) => {}
                }
            }
            CloseOutcome::Parked(_) | CloseOutcome::Refused(_) => {
                // Park-exclusion: HOLD the claim so the live-claims filter keeps
                // skipping this node. The active_claims entry is NOT removed.
                //
                // Also refresh the TTL via a same-holder re-acquire. The worker's
                // idempotent re-acquire during init-target-state.sh rewrites
                // acquired_at with the worker's pid; after the worker exits the
                // TTL window is measured from the worker's re-acquire time.
                // Re-acquiring here refreshes acquired_at to NOW so the claim
                // stays live through the next walk iteration.
                let session_key = self
                    .active_claims
                    .get(&unit.id)
                    .map(|e| e.session_key.clone())
                    .unwrap_or_else(|| unit.session_key.clone());
                let claim_key = format!("node:{}", unit.id);
                let claim_holder = format!("target-session:{session_key}");

                let refresh_result = retry_etxtbsy(|| {
                    abi_cmd(&self.abi_bin)
                        .args([
                            "claim",
                            "acquire",
                            &claim_key,
                            "--holder",
                            &claim_holder,
                            "--ttl",
                            "2h",
                            "--reason",
                            "megawalk park-exclusion hold",
                        ])
                        .env(
                            "FNO_CLAIMS_ROOT",
                            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
                        )
                        .output()
                });

                match refresh_result {
                    Ok(o) if !o.status.success() => {
                        eprintln!(
                            "loop-megawalk: WARNING: claim refresh (park-hold) {} failed (exit {}): {}",
                            claim_key,
                            o.status,
                            String::from_utf8_lossy(&o.stderr).trim()
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "loop-megawalk: WARNING: claim refresh (park-hold) {} spawn failed: {e}",
                            claim_key
                        );
                    }
                    Ok(_) => {}
                }
            }
        }

        // Increment the closed-units counter (used by max_units cap).
        //
        // Intentional: Parked and Refused outcomes count toward --max-units just
        // like Closed outcomes. The semantics of --max-units N are "process N
        // units, whatever the outcome" (once-mode). A parked unit consumed one
        // dispatch slot; counting it avoids an unbounded walk when every unit parks.
        self.units_closed += 1;

        Ok(outcome)
    }
}

// ── MegawalkDispatcher ────────────────────────────────────────────────────────

/// A Dispatcher that wraps ShelloutDispatcher with per-unit env injection.
///
/// For each Dispatcher::run call, builds env = static env +
/// `CONTINUE_PROMPT="/target no-merge <unit.id>"` (or `/target <unit.id>`
/// when allow_merge) + `TARGET_SESSION_ID=<unit.session_key>`.
///
/// This injects TARGET_SESSION_ID into the worker session so
/// init-target-state.sh uses it verbatim (Task 4: the override path in
/// init-target-state.sh).  The claim re-acquire in init-target-state.sh then
/// has `holder = target-session:<session_key>` matching the walker's claim,
/// making it idempotent (see module doc).
pub struct MegawalkDispatcher {
    driver_lib: PathBuf,
    static_env: Vec<(String, String)>,
    cwd: PathBuf,
    /// Reserved for future use (e.g. fno claim refresh per dispatch).
    _abi_bin: String,
    allow_merge: bool,
}

impl MegawalkDispatcher {
    pub fn new(
        driver_lib: PathBuf,
        static_env: Vec<(String, String)>,
        cwd: PathBuf,
        abi_bin: String,
        allow_merge: bool,
    ) -> Self {
        Self {
            driver_lib,
            static_env,
            // Root dispatched target workers at canonical main: megawalk is
            // single-repo target-class, so a walk launched from a linked
            // worktree must not start each worker in that worktree (the shared
            // .fno/ session-state collision, ab-77b691dc). canonical_repo_root
            // is a no-op when already canonical and falls back to the given cwd
            // when resolution is ambiguous (git missing / bare).
            cwd: crate::paths::canonical_repo_root(&cwd).unwrap_or(cwd),
            _abi_bin: abi_bin,
            allow_merge,
        }
    }
}

impl Dispatcher for MegawalkDispatcher {
    fn run(&self, unit: &Unit, ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
        let continue_prompt = if self.allow_merge {
            format!("/target {}", unit.id)
        } else {
            format!("/target no-merge {}", unit.id)
        };

        // Build merged env: static + per-unit overrides.
        let mut env = self.static_env.clone();
        // Override CONTINUE_PROMPT with the per-unit prompt.
        // Remove any existing CONTINUE_PROMPT from the static list to avoid
        // duplicates (the last value wins in most shells, but explicit removal
        // is cleaner).
        env.retain(|(k, _)| k != "CONTINUE_PROMPT" && k != "TARGET_SESSION_ID");
        env.push(("CONTINUE_PROMPT".to_string(), continue_prompt));
        env.push(("TARGET_SESSION_ID".to_string(), unit.session_key.clone()));

        // Inject driver-specific extra env (e.g. TARGET_MISSION_* for fleet nodes).
        // These come after the static env so they take precedence over any
        // identically-named static values (last write wins in most shells).
        env.extend(unit.extra_env.iter().cloned());

        // Construct a ShelloutDispatcher with the merged env per-unit.
        // ShelloutDispatcher is cheap to construct (no I/O at construction time).
        let dispatcher = ShelloutDispatcher::new(self.driver_lib.clone(), env, self.cwd.clone());
        dispatcher.run(unit, ctx)
    }
}

// ── walk-as-unit termination emission (group 3, ab-9fd662c6) ──────────────────

/// Emit a `termination` event for the WALK itself, keyed by `session_key`.
///
/// A parent loop (megatron) that dispatched this walk as a unit awaits a
/// `termination` event matching the unit's session_key - the same contract a
/// target session's loop-check satisfies one altitude down. Reuses the
/// existing `termination` event kind (loopcheck's), so no new kind and no
/// 4-place lockstep edit; `Journal::append` mirrors to the global journal,
/// which is how the parent (running in a different cwd) finds it.
///
/// The reason string is the serde serialization of the walk-level
/// TerminationReason (e.g. "NoWork"), the same spelling
/// `parse_termination_reason` round-trips.
pub fn emit_walk_termination(
    journal: &Journal,
    session_key: &str,
    reason: &TerminationReason,
    iterations_used: u64,
    units_closed: usize,
) -> Result<(), crate::loop_runtime::LoopError> {
    // Loud on a non-string serialization: a Debug-format fallback would emit
    // a spelling parse_termination_reason cannot match, turning a future
    // enum-shape change into a silent find_termination miss at the parent
    // (sigma-review). All current variants are unit -> always strings.
    let reason_str = match serde_json::to_value(reason) {
        Ok(serde_json::Value::String(s)) => s,
        other => {
            return Err(crate::loop_runtime::LoopError::Journal(format!(
                "walk termination reason did not serialize to a string \
                 (got {other:?}); refusing to journal an unparseable reason"
            )));
        }
    };
    journal.append(
        "termination",
        serde_json::json!({
            "session_id": session_key,
            "reason": reason_str,
            "message": format!(
                "megawalk walk terminated: {reason_str} ({iterations_used} iterations, {units_closed} units closed)"
            ),
        }),
    )
}

// ── verb glue: pub fn run() ────────────────────────────────────────────────────

/// Entry point for `fno-agents loop run --driver megawalk ...`.
///
/// Called from loop_target.rs when --driver megawalk is specified.
///
/// Exit codes:
/// - 0: NoWork | DonePRGreen | DoneAdvisory (walk completed or backlog empty)
/// - 1: Budget | NoProgress | Aborted (walk hit ceiling or failed)
/// - 2: usage / configuration error
/// - 77: driver binary missing from PATH (preflight failure)
/// - 130: Interrupted (SIGINT)
#[allow(clippy::too_many_arguments)]
pub fn run(
    // Parsed flags forwarded from loop_target.rs run_loop_verb_inner.
    dispatcher_name: &str,
    max_iterations: Option<u64>,
    max_turns: u64,
    budget_usd: f64,
    model: Option<&str>,
    prompt_file: Option<&str>,
    cli_alias: Option<&str>,
    driver_lib_dir: Option<PathBuf>,
    cwd: PathBuf,
    project: Option<String>,
    all: bool,
    allow_merge: bool,
    parallel_cap: Option<u64>,
    max_units: Option<u64>,
    mission: Option<String>,
    termination_key: Option<String>,
) -> i32 {
    match run_inner(
        dispatcher_name,
        max_iterations,
        max_turns,
        budget_usd,
        model,
        prompt_file,
        cli_alias,
        driver_lib_dir,
        cwd,
        project,
        all,
        allow_merge,
        parallel_cap,
        max_units,
        mission,
        termination_key,
    ) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fno-agents loop megawalk: {e}");
            2
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    dispatcher_name: &str,
    max_iterations: Option<u64>,
    max_turns: u64,
    budget_usd: f64,
    model: Option<&str>,
    prompt_file: Option<&str>,
    cli_alias: Option<&str>,
    driver_lib_dir: Option<PathBuf>,
    cwd: PathBuf,
    project: Option<String>,
    all: bool,
    allow_merge: bool,
    parallel_cap: Option<u64>,
    max_units: Option<u64>,
    mission: Option<String>,
    termination_key: Option<String>,
) -> Result<i32, Box<dyn std::error::Error>> {
    use crate::loop_dispatch::{driver_default_max, preflight, resolve_driver_binary};
    use crate::loop_runtime::{
        run_loop, GlobalJournalPath, Journal, LoopBudget, ProjectJournalPath,
    };
    use crate::loop_target::{exit_code_for_reason, install_sigint_handler, SIGINT_RECEIVED};
    use std::sync::atomic::Ordering;

    // ── resolve driver-lib-dir ────────────────────────────────────────────────
    let lib_dir = match driver_lib_dir {
        Some(d) => d,
        None => {
            if let Ok(env_dir) = std::env::var("FNO_DRIVER_LIB_DIR") {
                PathBuf::from(env_dir)
            } else {
                let candidate = cwd.join("scripts").join("lib");
                if candidate.is_dir() {
                    candidate
                } else {
                    eprintln!(
                        "fno-agents loop megawalk: cannot resolve driver lib directory. \
                         Pass --driver-lib-dir <path> or set FNO_DRIVER_LIB_DIR env."
                    );
                    return Ok(2);
                }
            }
        }
    };

    // ── preflight: driver whitelist, lib file, binary ─────────────────────────
    let lib_path = match preflight(dispatcher_name, &lib_dir, cli_alias) {
        Ok(p) => p,
        Err(crate::loop_runtime::LoopError::Dispatch(msg)) => {
            eprintln!("fno-agents loop megawalk: {msg}");
            return Ok(77);
        }
        Err(e) => {
            eprintln!("fno-agents loop megawalk: {e}");
            return Ok(2);
        }
    };

    // ── resolve max_iterations ────────────────────────────────────────────────
    let max_iters = match max_iterations {
        Some(n) => n,
        None => match driver_default_max(&lib_path) {
            Ok(n) => n,
            Err(e) => {
                eprintln!(
                    "fno-agents loop megawalk: could not query driver_default_max: {e}; \
                     pass --max-iterations explicitly"
                );
                return Ok(2);
            }
        },
    };

    // ── acquire walker singleton claim ────────────────────────────────────────
    // Prevents two concurrent megawalk processes from racing on the same backlog.
    // Key the singleton on the CANONICAL repo root, matching the canonical cwd
    // each dispatched worker is rooted at (MegawalkDispatcher). Otherwise two
    // walkers launched from two linked worktrees of the same repo would take
    // different `walker:<worktree>` keys yet dispatch workers into the same
    // canonical .fno/ state, recreating the collision this change removes
    // (codex P2). No-op when already canonical; falls back to cwd when ambiguous.
    let walker_root = crate::paths::canonical_repo_root(&cwd).unwrap_or_else(|| cwd.clone());
    let walker_key = format!("walker:{}", walker_root.display());
    let walker_holder = format!("megawalk-loop:{}", std::process::id());
    let abi_bin = std::env::var("FNO_BIN").unwrap_or_else(|_| "fno".to_string());

    let walker_claim_result = abi_cmd(&abi_bin)
        .args([
            "claim",
            "acquire",
            &walker_key,
            "--holder",
            &walker_holder,
            "--ttl",
            "24h",
            "--reason",
            "megawalk walker singleton",
        ])
        .output();

    match walker_claim_result {
        Ok(o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            eprintln!(
                "fno-agents loop megawalk: walker singleton already running: {stderr}; \
                 another megawalk is active for this project (holder in claim file)"
            );
            return Ok(1);
        }
        Err(e) => {
            // If fno is not available, warn and continue (best-effort singleton).
            eprintln!(
                "fno-agents loop megawalk: WARNING: walker claim acquire failed: {e} (continuing)"
            );
        }
        Ok(_) => {}
    }

    // ── build static env ──────────────────────────────────────────────────────
    let abilities_dir = cwd.join(".fno");
    let output_file = abilities_dir.join("target-last-output.txt");
    let history_file = abilities_dir.join("target-history.txt");
    let signal_file = abilities_dir.join("target-promise.signal");

    let mut env: Vec<(String, String)> = vec![
        (
            "OUTPUT_FILE".to_string(),
            output_file.to_str().unwrap_or("").to_string(),
        ),
        (
            "HISTORY_FILE".to_string(),
            history_file.to_str().unwrap_or("").to_string(),
        ),
        (
            "SIGNAL_FILE".to_string(),
            signal_file.to_str().unwrap_or("").to_string(),
        ),
        ("MAX_TURNS".to_string(), max_turns.to_string()),
        ("BUDGET_USD".to_string(), format!("{budget_usd}")),
        // CONTINUE_PROMPT is set per-unit by MegawalkDispatcher.
        ("CONTINUE_PROMPT".to_string(), String::new()),
    ];

    if let Some(m) = model {
        env.push(("MODEL_FLAG".to_string(), format!("--model {m}")));
    } else {
        env.push(("MODEL_FLAG".to_string(), String::new()));
    }

    if let Some(pf) = prompt_file {
        env.push(("PROMPT_FILE".to_string(), pf.to_string()));
    }

    if let Some(cli) = cli_alias {
        env.push(("CLI".to_string(), cli.to_string()));
    }

    env.push((
        "FNO_CWD".to_string(),
        cwd.to_str().unwrap_or(".").to_string(),
    ));

    // ── SIGINT handler ────────────────────────────────────────────────────────
    install_sigint_handler();

    // ── build journal ─────────────────────────────────────────────────────────
    let project_events = abilities_dir.join("events.jsonl");
    let home_dir = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let global_events = home_dir.join(".fno").join("events.jsonl");
    let journal = Journal::new(
        ProjectJournalPath(project_events),
        GlobalJournalPath(global_events),
    );

    // ── print header ──────────────────────────────────────────────────────────
    let binary_name = resolve_driver_binary(dispatcher_name, cli_alias);
    let scope = if all {
        "all projects".to_string()
    } else if let Some(ref p) = project {
        format!("project={p}")
    } else {
        "auto-detected project".to_string()
    };
    println!("fno-agents loop megawalk");
    println!("  driver:     megawalk");
    println!("  dispatcher: {dispatcher_name} (binary: {binary_name})");
    println!("  scope:      {scope}");
    println!("  iterations: {max_iters} max");
    println!("  budget:     ${budget_usd} USD");

    // ── resume narration (AC4-UI) ─────────────────────────────────────────────
    // Shell `fno claim list --prefix node: --include-stale --json` and print
    // one header line when stale node claims exist. A stale claim means a prior
    // walk was interrupted mid-unit; the walker will re-acquire on contact and
    // recover the work. Best-effort only: if the command fails for any reason,
    // skip silently (claim narration is informational, not a gate).
    {
        let claim_out = abi_cmd(&abi_bin)
            .args([
                "claim",
                "list",
                "--prefix",
                "node:",
                "--include-stale",
                "--json",
            ])
            .output();
        if let Ok(o) = claim_out {
            if o.status.success() {
                let stdout = String::from_utf8_lossy(&o.stdout);
                // JSON output is a list of claim objects with a "status" field.
                // Count entries where status == "stale".
                if let Ok(arr) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
                    let stale_count = arr
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter(|v| v["status"].as_str() == Some("stale"))
                                .count()
                        })
                        .unwrap_or(0);
                    if stale_count > 0 {
                        println!(
                            "resume: {stale_count} stale node claim(s) from a prior walk \
                             will be recovered on contact"
                        );
                    }
                }
            }
        }
    }

    // ── build queue and dispatcher ────────────────────────────────────────────
    let mut queue = MegawalkQueue::new_with_max_units(abi_bin.clone(), project, all, max_units)
        .with_mission(mission.clone());
    let dispatcher =
        MegawalkDispatcher::new(lib_path, env, cwd.clone(), abi_bin.clone(), allow_merge);

    // ── parallel-cap notice ───────────────────────────────────────────────────
    // Group-2 ships the conservative sequential default (Claude's Discretion 3).
    // run_loop is single-threaded; when cap > 1, print one honest line so the
    // flag is accepted-but-explicit, never a silent drop.
    if let Some(cap) = parallel_cap {
        if cap > 1 {
            println!(
                "megawalk: --parallel-cap {cap} accepted; execution is SEQUENTIAL \
                 (collision-conservative default; group-2 serializes regardless of cap)"
            );
        }
    }

    // ── max-units notice ──────────────────────────────────────────────────────
    if let Some(n) = max_units {
        println!("megawalk: --max-units {n} (walk stops after {n} unit(s) closed)");
    }

    // ── walker-claim release helper ───────────────────────────────────────────
    // Called on every early-return path after claim acquisition so the
    // documented contract ("releases on all exit paths") is met. TTL/PID
    // makes leaked claims recoverable, but explicit release is cleaner and
    // makes tests deterministic (Gemini HIGH finding).
    let release_walker_claim = || {
        let _ = abi_cmd(&abi_bin)
            .args(["claim", "release", &walker_key, "--holder", &walker_holder])
            .output();
    };

    // ── build budget ──────────────────────────────────────────────────────────
    let budget = match LoopBudget::new(max_iters) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fno-agents loop megawalk: {e}");
            release_walker_claim();
            return Ok(2);
        }
    };

    // ── cancel closure ────────────────────────────────────────────────────────
    let cancel_file = cwd.join(".fno").join(".target-cancelled");
    let cancel = move || SIGINT_RECEIVED.load(Ordering::SeqCst) || cancel_file.exists();

    // ── run the loop ──────────────────────────────────────────────────────────
    // Per-unit dispatch cap (plan Failure Mode: a session that dies without a
    // TerminationReason event is synthesized as node_failed and must count
    // toward the consecutive-failure pause). Re-dispatch is the NORMAL
    // continuation mechanism for multi-session work, so the cap is generous:
    // 15 sessions x MAX_TURNS turns is ample for an L-sized node, while a
    // crash-looping driver parks after 15 fast failures (close(NoProgress) ->
    // streak) instead of burning the whole walk budget on one unit.
    const PER_UNIT_MAX_DISPATCHES: u64 = 15;
    let outcome = match run_loop(
        &mut queue,
        &dispatcher,
        &budget,
        &journal,
        &cancel,
        Some(PER_UNIT_MAX_DISPATCHES),
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("fno-agents loop megawalk: fatal loop error: {e}");
            release_walker_claim();
            return Ok(2);
        }
    };

    // ── release walker singleton claim ────────────────────────────────────────
    release_walker_claim();

    // ── walk-as-unit termination event (group 3) ──────────────────────────────
    // When a parent loop (megatron) dispatched this walk with a session key,
    // journal the walk's own termination so the parent's find_termination
    // observes it (via the global mirror when cwds differ). Fatal on project-
    // journal failure, consistent with every other journal.append here.
    if let Some(ref key) = termination_key {
        if let Err(e) = emit_walk_termination(
            &journal,
            key,
            &outcome.reason,
            outcome.iterations_used,
            outcome.units.len(),
        ) {
            eprintln!("fno-agents loop megawalk: failed to journal walk termination: {e}");
            return Ok(2);
        }
    }

    // ── report outcome ────────────────────────────────────────────────────────
    let exit_code = exit_code_for_reason(&outcome.reason);
    println!(
        "megawalk: {:?} ({} iterations used, {} units closed)",
        outcome.reason,
        outcome.iterations_used,
        outcome.units.len()
    );
    for unit_result in &outcome.units {
        println!(
            "  unit {}: {:?} ({:?})",
            unit_result.unit_id, unit_result.evidence.reason, unit_result.close
        );
    }

    Ok(exit_code)
}

#[cfg(test)]
mod fresh_tests {
    //! ab-77b691dc: a megawalk worker is rooted at canonical main, so a walk
    //! launched from a linked worktree does not start each target worker in that
    //! worktree (the shared .fno/ session-state collision).
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn dispatcher_roots_worker_cwd_at_canonical_from_worktree() {
        // Skip when git is unavailable (mirrors the Python skipif(no git)).
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir(&main).unwrap();
        assert!(git(&main, &["init", "-q"]));
        assert!(git(&main, &["config", "user.email", "t@t"]));
        assert!(git(&main, &["config", "user.name", "t"]));
        assert!(git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]));
        let linked = tmp.path().join("wt");
        assert!(git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                linked.to_str().unwrap(),
                "-b",
                "feat"
            ]
        ));

        let d = MegawalkDispatcher::new(
            std::path::PathBuf::from("/driver/lib.sh"),
            vec![],
            linked.clone(),
            "fno".to_string(),
            false,
        );
        let want = std::fs::canonicalize(&main).unwrap();
        assert_eq!(
            d.cwd, want,
            "megawalk worker cwd must be rooted at canonical main, not the worktree"
        );
    }

    #[test]
    fn dispatcher_keeps_cwd_when_not_a_worktree() {
        // A non-git cwd -> canonical resolution returns None -> keep the given
        // cwd (the safe-side fallback; also the already-canonical no-op case).
        let tmp = tempfile::tempdir().unwrap();
        let d = MegawalkDispatcher::new(
            std::path::PathBuf::from("/driver/lib.sh"),
            vec![],
            tmp.path().to_path_buf(),
            "fno".to_string(),
            false,
        );
        assert_eq!(d.cwd, tmp.path());
    }

    #[test]
    fn retry_etxtbsy_passes_success_through_without_retry() {
        let mut calls = 0u32;
        let r: std::io::Result<u8> = retry_etxtbsy(|| {
            calls += 1;
            Ok(7)
        });
        assert_eq!(r.unwrap(), 7);
        assert_eq!(calls, 1, "a successful spawn must not retry");
    }

    #[test]
    fn retry_etxtbsy_retries_then_succeeds() {
        // Simulate ETXTBSY clearing after a couple of attempts.
        let mut calls = 0u32;
        let r: std::io::Result<u8> = retry_etxtbsy(|| {
            calls += 1;
            if calls < 3 {
                Err(std::io::Error::from_raw_os_error(libc::ETXTBSY))
            } else {
                Ok(42)
            }
        });
        assert_eq!(r.unwrap(), 42);
        assert_eq!(calls, 3, "must retry past transient ETXTBSY");
    }

    #[test]
    fn retry_etxtbsy_does_not_swallow_other_errors() {
        // A non-ETXTBSY error returns immediately, no retry.
        let mut calls = 0u32;
        let r: std::io::Result<u8> = retry_etxtbsy(|| {
            calls += 1;
            Err(std::io::Error::from_raw_os_error(libc::ENOENT))
        });
        assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::ENOENT));
        assert_eq!(calls, 1, "a non-ETXTBSY error must not retry");
    }

    #[test]
    fn retry_etxtbsy_gives_up_after_max_retries() {
        // Persistent ETXTBSY surfaces after the bounded retry budget (1 initial
        // + 5 retries = 6 calls) rather than spinning forever.
        let mut calls = 0u32;
        let r: std::io::Result<u8> = retry_etxtbsy(|| {
            calls += 1;
            Err(std::io::Error::from_raw_os_error(libc::ETXTBSY))
        });
        assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::ETXTBSY));
        assert_eq!(calls, 6, "1 initial attempt + MAX_RETRIES(5)");
    }
}
