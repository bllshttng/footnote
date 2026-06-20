//! Active backlog dispatcher: the drain-tick core + circuit breaker (node x-c070).
//!
//! This module is the engine for the always-on backlog drain. One *tick*
//! claims the project's `walker:<cwd>` singleton, asks the existing
//! [`MegawalkQueue`] for the next ready node, and runs it to termination
//! through the unchanged [`run_loop`] primitive. The daemon (Wave 3) calls
//! [`drain_tick`] on a schedule (Wave 4); this file owns only what happens
//! inside one tick plus the cross-tick failure counter.
//!
//! ## Single-owner contract (AC1-FR)
//!
//! The tick acquires `walker:<cwd>` (holder `active-backlog:<pid>`) at the
//! start and RELEASES it at the end. Releasing per-tick is deliberate: it lets
//! a human `/megawalk` grab the singleton between ticks, after which the
//! daemon's next acquire fails and the tick YIELDS (`active_backlog_yield`).
//! Holding the claim across the whole drain (megawalk's model) would make the
//! daemon a permanent owner that a manual walk could never displace, which
//! AC1-FR forbids for v1. The `/megawalk` skill-body re-pointing (daemon
//! client vs peer dispatcher) is the deferred follow-up D1.
//!
//! ## One node per tick
//!
//! The queue is built with `max_units = Some(1)`, so `run_loop` stops after the
//! first unit closes. The per-tick budget is generous (not the plan's literal
//! `LoopBudget::new(1)`): a node that needs re-dispatch (a session that ends
//! without a termination event) must still reach termination within the tick,
//! and `LoopBudget::new(1)` would strand it claimed-but-unclosed for the claim
//! TTL. The per-unit dispatch cap is the in-tick crash-loop backstop; the
//! cross-tick [`CircuitBreaker`] is the slower, operator-visible one.
//!
//! ## Events (Journal contract)
//!
//! Every transition emits through [`Journal::append`] (project journal fatal,
//! global mirror best-effort), matching every other loop event:
//! `active_backlog_dispatched` / `_yield` / `_parked` / `_skip`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::events::EventEmitter;
use crate::loop_megawalk::{abi_cmd, MegawalkDispatcher, MegawalkQueue};
use crate::loop_runtime::{
    run_loop, CloseOutcome, GlobalJournalPath, Journal, LoopBudget, ProjectJournalPath,
};
use crate::loopcheck::TerminationReason;

/// Cross-tick per-node consecutive-failure counter (the circuit breaker).
///
/// Hermes semantics: increment on a failed drain, reset to zero ONLY on a
/// successful close, trip at `failure_limit` with NO auto-unpark. A tripped
/// node stays parked until an operator `fno backlog undefer` (or a later
/// success once the operator clears the claim) resets it. This struct is the
/// pure policy; holding the actual `node:<id>` claim that excludes the node
/// from selection is the caller's IO step.
#[derive(Debug, Default)]
pub struct CircuitBreaker {
    failure_limit: u32,
    failures: HashMap<String, u32>,
    parked: HashSet<String>,
}

impl CircuitBreaker {
    /// `failure_limit` is clamped to at least 1 (a zero limit would park every
    /// node on its first failure, which is never the intent).
    pub fn new(failure_limit: u32) -> Self {
        Self {
            failure_limit: failure_limit.max(1),
            failures: HashMap::new(),
            parked: HashSet::new(),
        }
    }

    /// Record a failed drain for `node`. Returns `true` iff this failure trips
    /// the breaker (the streak just reached `failure_limit` and the node should
    /// be parked now). An already-parked node never re-trips.
    pub fn record_failure(&mut self, node: &str) -> bool {
        if self.parked.contains(node) {
            return false;
        }
        let n = self.failures.entry(node.to_string()).or_insert(0);
        *n += 1;
        if *n >= self.failure_limit {
            self.parked.insert(node.to_string());
            true
        } else {
            false
        }
    }

    /// Record a successful close for `node`: clear the streak and unpark, so a
    /// human-fixed node gets a fresh `failure_limit` attempts.
    pub fn record_success(&mut self, node: &str) {
        self.failures.remove(node);
        self.parked.remove(node);
    }

    /// The current consecutive-failure count for `node` (0 if none).
    pub fn consecutive_failures(&self, node: &str) -> u32 {
        self.failures.get(node).copied().unwrap_or(0)
    }

    /// Whether `node` is currently parked by the breaker.
    pub fn is_parked(&self, node: &str) -> bool {
        self.parked.contains(node)
    }

    /// The set of currently-parked node ids (the daemon refreshes their claims
    /// each tick so the park outlives the 24h claim TTL).
    pub fn parked_nodes(&self) -> impl Iterator<Item = &String> {
        self.parked.iter()
    }
}

/// Everything one [`drain_tick`] needs, resolved by the caller (the daemon).
///
/// The driver-specific bits (`lib_path`, `max_turns`, `budget_usd`, `model`)
/// mirror what `loop_megawalk::run_inner` resolves before building its
/// dispatcher; the daemon resolves them once and reuses across ticks.
#[derive(Debug, Clone)]
pub struct DrainConfig {
    /// The project's working directory (the walk root).
    pub cwd: PathBuf,
    /// Project name filter for `fno backlog next` (None = auto-detect).
    pub project: Option<String>,
    /// Mission scope; when set, only that mission's nodes drain.
    pub mission: Option<String>,
    /// Resolved driver lib path (e.g. `scripts/lib/driver-claude-code.sh`).
    pub lib_path: PathBuf,
    /// The `fno` binary name/path (FNO_BIN override honored by the caller).
    pub abi_bin: String,
    /// Whether dispatched workers may auto-merge (default false: review-only).
    pub allow_merge: bool,
    /// Per-worker turn cap (forwarded to the driver as MAX_TURNS).
    pub max_turns: u64,
    /// Per-worker USD budget (forwarded as BUDGET_USD).
    pub budget_usd: f64,
    /// Optional model override forwarded as MODEL_FLAG.
    pub model: Option<String>,
    /// Iteration ceiling for the single-node walk (generous; see module docs).
    pub max_iterations: u64,
    /// In-tick re-dispatch cap before a node is parked as NoProgress.
    pub per_unit_max_dispatches: u64,
    /// Cross-tick consecutive-failure limit (the circuit breaker).
    pub failure_limit: u32,
}

/// What one [`drain_tick`] did, for the scheduler and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// A node was selected and closed successfully.
    Dispatched { node: String },
    /// A node tripped the circuit breaker and was parked.
    Parked { node: String, failures: u32 },
    /// The walker singleton is held by another walker (manual /megawalk); the
    /// tick yielded without dispatching.
    Yielded,
    /// No ready node in scope; the board (or mission) is drained.
    NoWork,
    /// The tick could not run to a node close (selection/loop error, or a node
    /// that failed without yet tripping the breaker).
    Skipped { reason: String },
}

/// The result of a `fno claim acquire` attempt.
enum ClaimResult {
    Acquired,
    Held,
    Error(String),
}

fn acquire_claim(abi_bin: &str, key: &str, holder: &str, ttl: &str, reason: &str) -> ClaimResult {
    match abi_cmd(abi_bin)
        .args(["claim", "acquire", key, "--holder", holder, "--ttl", ttl, "--reason", reason])
        .output()
    {
        Ok(o) if o.status.success() => ClaimResult::Acquired,
        Ok(o) => {
            // Exit 1 = held by a live, different holder. Any non-zero is treated
            // as "not ours" so we never dispatch without the singleton.
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            if o.status.code() == Some(1) {
                ClaimResult::Held
            } else {
                ClaimResult::Error(stderr)
            }
        }
        Err(e) => ClaimResult::Error(e.to_string()),
    }
}

fn release_claim(abi_bin: &str, key: &str, holder: &str) {
    let _ = abi_cmd(abi_bin)
        .args(["claim", "release", key, "--holder", holder])
        .output();
}

/// The walker singleton key for a project root (canonicalized to match the cwd
/// each dispatched worker is rooted at - same rule megawalk uses).
fn walker_key_for(cwd: &Path) -> String {
    let root = crate::paths::canonical_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    format!("walker:{}", root.display())
}

/// Run one drain tick: acquire the walker singleton, drain one node to
/// termination, release the singleton. The `breaker` persists across ticks so
/// a crash-looping node accumulates failures and is eventually parked.
pub fn drain_tick(cfg: &DrainConfig, breaker: &mut CircuitBreaker, journal: &Journal) -> DrainOutcome {
    let walker_key = walker_key_for(&cfg.cwd);
    let walker_holder = format!("active-backlog:{}", std::process::id());

    // Refresh any already-parked node claims so the park outlives the 24h TTL
    // (Hermes: no auto-unpark - a parked node stays excluded until an operator
    // intervenes). Best-effort; a missed refresh just lets the claim expire and
    // the node re-enters selection, where it will trip the breaker again.
    refresh_parked_claims(cfg, breaker);

    match acquire_claim(&cfg.abi_bin, &walker_key, &walker_holder, "24h", "active-backlog drain tick") {
        ClaimResult::Acquired => {}
        ClaimResult::Held => {
            let _ = journal.append(
                "active_backlog_yield",
                json!({"reason": "walker-live", "key": walker_key}),
            );
            return DrainOutcome::Yielded;
        }
        ClaimResult::Error(e) => {
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "walker-claim-error", "detail": e}),
            );
            return DrainOutcome::Skipped { reason: format!("walker-claim-error: {e}") };
        }
    }

    let outcome = run_one_node(cfg, breaker, journal);

    // Release the singleton on every exit path so a manual /megawalk can take
    // over before the next tick (AC1-FR).
    release_claim(&cfg.abi_bin, &walker_key, &walker_holder);
    outcome
}

/// Re-acquire (idempotent, same-holder) every parked node's claim to reset its
/// TTL window, keeping the circuit-breaker park in force across ticks.
fn refresh_parked_claims(cfg: &DrainConfig, breaker: &CircuitBreaker) {
    let holder = format!("active-backlog-park:{}", std::process::id());
    for node in breaker.parked_nodes() {
        let _ = acquire_claim(
            &cfg.abi_bin,
            &format!("node:{node}"),
            &holder,
            "24h",
            "active-backlog circuit-breaker park hold",
        );
    }
}

fn build_env(cfg: &DrainConfig) -> Vec<(String, String)> {
    let abilities_dir = cfg.cwd.join(".fno");
    let output_file = abilities_dir.join("target-last-output.txt");
    let history_file = abilities_dir.join("target-history.txt");
    let signal_file = abilities_dir.join("target-promise.signal");

    let mut env: Vec<(String, String)> = vec![
        ("OUTPUT_FILE".to_string(), output_file.to_string_lossy().into_owned()),
        ("HISTORY_FILE".to_string(), history_file.to_string_lossy().into_owned()),
        ("SIGNAL_FILE".to_string(), signal_file.to_string_lossy().into_owned()),
        ("MAX_TURNS".to_string(), cfg.max_turns.to_string()),
        ("BUDGET_USD".to_string(), format!("{}", cfg.budget_usd)),
        // CONTINUE_PROMPT is set per-unit by MegawalkDispatcher.
        ("CONTINUE_PROMPT".to_string(), String::new()),
        ("FNO_CWD".to_string(), cfg.cwd.to_string_lossy().into_owned()),
    ];
    match &cfg.model {
        Some(m) => env.push(("MODEL_FLAG".to_string(), format!("--model {m}"))),
        None => env.push(("MODEL_FLAG".to_string(), String::new())),
    }
    env
}

fn run_one_node(cfg: &DrainConfig, breaker: &mut CircuitBreaker, journal: &Journal) -> DrainOutcome {
    let mut queue =
        MegawalkQueue::new_with_max_units(cfg.abi_bin.clone(), cfg.project.clone(), false, Some(1))
            .with_mission(cfg.mission.clone());
    let dispatcher = MegawalkDispatcher::new(
        cfg.lib_path.clone(),
        build_env(cfg),
        cfg.cwd.clone(),
        cfg.abi_bin.clone(),
        cfg.allow_merge,
    );

    let budget = match LoopBudget::new(cfg.max_iterations.max(1)) {
        Ok(b) => b,
        Err(e) => {
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "bad-budget", "detail": format!("{e}")}),
            );
            return DrainOutcome::Skipped { reason: format!("bad-budget: {e}") };
        }
    };

    // A daemon tick honors the project cancel sentinel (the same one /target and
    // megawalk watch); there is no SIGINT in the drain task's blocking thread.
    let cancel_file = cfg.cwd.join(".fno").join(".target-cancelled");
    let cancel = move || cancel_file.exists();

    let outcome = match run_loop(
        &mut queue,
        &dispatcher,
        &budget,
        journal,
        &cancel,
        Some(cfg.per_unit_max_dispatches),
    ) {
        Ok(o) => o,
        Err(e) => {
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "loop-error", "detail": format!("{e}")}),
            );
            return DrainOutcome::Skipped { reason: format!("loop-error: {e}") };
        }
    };

    map_outcome(cfg, breaker, journal, &outcome.reason, outcome.units.last())
}

/// Map a completed walk to a [`DrainOutcome`], updating the breaker and emitting
/// the decision event. Split out from [`run_one_node`] so the policy is unit-
/// testable without spawning a real worker.
fn map_outcome(
    cfg: &DrainConfig,
    breaker: &mut CircuitBreaker,
    journal: &Journal,
    reason: &TerminationReason,
    last_unit: Option<&crate::loop_runtime::UnitResult>,
) -> DrainOutcome {
    let Some(last) = last_unit else {
        // No unit reached close.
        return match reason {
            TerminationReason::NoWork => DrainOutcome::NoWork,
            other => {
                let _ = journal.append(
                    "active_backlog_skip",
                    json!({"reason": "no-close", "termination": format!("{other:?}")}),
                );
                DrainOutcome::Skipped { reason: format!("{other:?}") }
            }
        };
    };

    let node = last.unit_id.clone();
    match &last.close {
        CloseOutcome::Closed => {
            breaker.record_success(&node);
            let _ = journal.append(
                "active_backlog_dispatched",
                json!({"node_id": node, "termination": format!("{:?}", last.evidence.reason)}),
            );
            DrainOutcome::Dispatched { node }
        }
        CloseOutcome::Parked(detail) | CloseOutcome::Refused(detail) => {
            let tripped = breaker.record_failure(&node);
            if tripped {
                // Hold the node claim so `fno backlog next`'s live-claims filter
                // excludes it from future selection (the park is the claim).
                let holder = format!("active-backlog-park:{}", std::process::id());
                let _ = acquire_claim(
                    &cfg.abi_bin,
                    &format!("node:{node}"),
                    &holder,
                    "24h",
                    "active-backlog circuit-breaker park",
                );
                let failures = breaker.consecutive_failures(&node);
                let _ = journal.append(
                    "active_backlog_parked",
                    json!({"node_id": node, "consecutive_failures": failures, "detail": detail}),
                );
                DrainOutcome::Parked { node, failures }
            } else {
                let _ = journal.append(
                    "active_backlog_skip",
                    json!({
                        "reason": "node-not-closed",
                        "node_id": node,
                        "close": detail,
                        "consecutive_failures": breaker.consecutive_failures(&node),
                    }),
                );
                DrainOutcome::Skipped { reason: format!("node {node} not closed: {detail}") }
            }
        }
    }
}

// ── target resolution + resident supervisor (Wave 3) ───────────────────────────

/// One drain target as resolved by the Python `fno config active-backlog --json`
/// helper (config.active_backlog + the workspace project->path map).
#[derive(Debug, Clone, Deserialize)]
pub struct ResolvedTarget {
    pub project: String,
    pub cwd: String,
    pub interval_seconds: u64,
    pub failure_limit: u32,
    #[serde(default)]
    pub mission: Option<String>,
}

/// Per-worker turn cap for daemon-dispatched drains (overridable via env). The
/// daemon has no `--max-turns` flag like megawalk, so it carries a generous
/// default; an operator tuning knob lands as config if ever needed.
fn daemon_max_turns() -> u64 {
    std::env::var("FNO_ACTIVE_BACKLOG_MAX_TURNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200)
}

fn daemon_budget_usd() -> f64 {
    std::env::var("FNO_ACTIVE_BACKLOG_BUDGET_USD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20.0)
}

/// In-tick re-dispatch cap, matching megawalk's `PER_UNIT_MAX_DISPATCHES`.
const PER_UNIT_MAX_DISPATCHES: u64 = 15;

/// Shell `fno config active-backlog --json` to discover enabled drain targets.
/// Best-effort: any failure (missing fno, non-zero exit, unparseable output)
/// yields an empty list, so the feature simply stays dormant.
pub fn resolve_targets(abi_bin: &str) -> Vec<ResolvedTarget> {
    match abi_cmd(abi_bin)
        .args(["config", "active-backlog", "--json"])
        .output()
    {
        Ok(o) if o.status.success() => serde_json::from_slice(&o.stdout).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Build the per-project loop journal (project events.jsonl fatal, global mirror
/// best-effort) for a drain target's cwd.
fn journal_for(cwd: &Path) -> Journal {
    let project_events = cwd.join(".fno").join("events.jsonl");
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let global_events = home.join(".fno").join("events.jsonl");
    Journal::new(
        ProjectJournalPath(project_events),
        GlobalJournalPath(global_events),
    )
}

/// Resolve a [`DrainConfig`] for a target, or `None` if its driver lib is absent
/// (a project without `scripts/lib/driver-claude-code.sh` cannot be drained).
fn drain_config_for(target: &ResolvedTarget, abi_bin: &str) -> Option<DrainConfig> {
    use crate::loop_dispatch::{driver_default_max, preflight};
    let cwd = PathBuf::from(&target.cwd);
    let lib_dir = cwd.join("scripts").join("lib");
    let lib_path = preflight("claude-code", &lib_dir, None).ok()?;
    let max_iterations = driver_default_max(&lib_path).unwrap_or(PER_UNIT_MAX_DISPATCHES);
    Some(DrainConfig {
        cwd,
        project: Some(target.project.clone()),
        mission: target.mission.clone(),
        lib_path,
        abi_bin: abi_bin.to_string(),
        allow_merge: false,
        max_turns: daemon_max_turns(),
        budget_usd: daemon_budget_usd(),
        model: None,
        max_iterations,
        per_unit_max_dispatches: PER_UNIT_MAX_DISPATCHES,
        failure_limit: target.failure_limit,
    })
}

/// Sleep `total`, waking early if `shutdown` flips. Checked in small steps so a
/// long poll interval still tears down promptly at daemon shutdown.
async fn sleep_interruptible(total: Duration, shutdown: &Arc<AtomicBool>) {
    let step = Duration::from_millis(500);
    let mut elapsed = Duration::ZERO;
    while elapsed < total {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let chunk = step.min(total - elapsed);
        tokio::time::sleep(chunk).await;
        elapsed += chunk;
    }
}

/// The wake nudge sentinel path ($HOME/.fno/.active-backlog-nudge by default).
/// Mirrors the Python writer (`fno.active_backlog.nudge_sentinel_path`) under
/// the default state dir; a non-default state_dir only loses the latency
/// optimization, never correctness (the poll floor is the guarantee).
fn nudge_sentinel_path() -> PathBuf {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    home.join(".fno").join(".active-backlog-nudge")
}

/// The sentinel's mtime, or `None` if it does not exist / cannot be stat'd.
fn nudge_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(nudge_sentinel_path())
        .and_then(|m| m.modified())
        .ok()
}

/// Wait up to `total` for the next poll tick, waking EARLY if the nudge sentinel
/// changes (an event nudge) or `shutdown` flips. `last` carries the mtime across
/// calls; a burst of touches during a tick coalesces to a single wake because
/// `last` advances to the newest mtime once, here. The poll floor (`total`) is
/// the backstop, so a missed nudge just delays a drain by at most one interval.
async fn wait_for_wake(
    total: Duration,
    shutdown: &Arc<AtomicBool>,
    last: &mut Option<std::time::SystemTime>,
) {
    let step = Duration::from_millis(500);
    let mut elapsed = Duration::ZERO;
    while elapsed < total {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let current = nudge_mtime();
        if current != *last {
            *last = current;
            return; // event nudge: wake early (coalesced)
        }
        let chunk = step.min(total - elapsed);
        tokio::time::sleep(chunk).await;
        elapsed += chunk;
    }
}

/// The resident drain supervisor (node x-c070, Wave 3).
///
/// Runs until `shutdown` is set. Sets `live` true whenever there is >=1 enabled
/// target so the daemon's idle-exit stays out (OQ1 Option A: an enabled but
/// drained board keeps the daemon resident and polling). Each pass drains one
/// node per enabled project (serial, v1). Every tick runs on `spawn_blocking`
/// (`run_loop` is synchronous and blocks for the worker lifetime); a panic in a
/// tick is caught, emitted as `active_backlog_task_crashed`, and the supervisor
/// restarts with exponential backoff rather than taking down the daemon.
///
/// Config changes are picked up by re-resolving targets each pass: a project
/// that drops out of `config.active_backlog` simply stops being ticked after its
/// current (already-running) tick finishes - AC2-EDGE's "disable mid-flight
/// completes the current dispatch, schedules no more" falls out for free because
/// targets are only re-checked BETWEEN ticks, never mid-tick.
pub async fn run_supervisor(
    abi_bin: String,
    emitter: EventEmitter,
    live: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    let mut breakers: HashMap<String, CircuitBreaker> = HashMap::new();
    let recheck = Duration::from_secs(60);
    let mut backoff = Duration::from_secs(1);
    // Nudge cursor: the sentinel mtime last observed, so wait_for_wake can wake
    // the loop early when a backlog mutation / advance touches it (Wave 4).
    let mut last_nudge = nudge_mtime();

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let targets = resolve_targets(&abi_bin);
        if targets.is_empty() {
            live.store(false, Ordering::SeqCst);
            sleep_interruptible(recheck, &shutdown).await;
            continue;
        }
        live.store(true, Ordering::SeqCst);

        let mut min_interval = Duration::from_secs(300);
        for target in &targets {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            min_interval = min_interval.min(Duration::from_secs(target.interval_seconds.max(1)));

            let Some(cfg) = drain_config_for(target, &abi_bin) else {
                continue;
            };
            let journal = journal_for(&cfg.cwd);
            let project = target.project.clone();
            let breaker = breakers
                .remove(&project)
                .unwrap_or_else(|| CircuitBreaker::new(target.failure_limit));
            let emitter_for_crash = emitter.clone();

            // run_loop is synchronous; offload so the daemon's async serve loop
            // is never stalled. Move the breaker in and hand it back so its
            // cross-tick failure counts survive.
            let handle = tokio::task::spawn_blocking(move || {
                let mut b = breaker;
                let outcome = drain_tick(&cfg, &mut b, &journal);
                (outcome, b)
            });

            match handle.await {
                Ok((_outcome, b)) => {
                    breakers.insert(project, b);
                    backoff = Duration::from_secs(1);
                }
                Err(join_err) => {
                    let _ = emitter_for_crash.emit(
                        "active_backlog_task_crashed",
                        &json!({"project": project, "error": join_err.to_string()}),
                    );
                    // Lose the panicked breaker's counts (rare); a fresh one is
                    // safe (a crash-looping node re-accrues failures next pass).
                    breakers.insert(project, CircuitBreaker::new(target.failure_limit));
                    sleep_interruptible(backoff, &shutdown).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }

        // Wait the poll floor between passes, waking early on an event nudge
        // (Wave 4): a backlog mutation / advance touches the sentinel so a fresh
        // ready node drains sooner than the floor.
        wait_for_wake(min_interval, &shutdown, &mut last_nudge).await;
    }
    live.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_trips_at_limit() {
        let mut b = CircuitBreaker::new(3);
        assert!(!b.record_failure("n1"));
        assert_eq!(b.consecutive_failures("n1"), 1);
        assert!(!b.record_failure("n1"));
        assert_eq!(b.consecutive_failures("n1"), 2);
        // third failure trips
        assert!(b.record_failure("n1"));
        assert!(b.is_parked("n1"));
        assert_eq!(b.consecutive_failures("n1"), 3);
    }

    #[test]
    fn breaker_success_resets_streak_and_unparks() {
        let mut b = CircuitBreaker::new(2);
        b.record_failure("n1");
        assert_eq!(b.consecutive_failures("n1"), 1);
        b.record_success("n1");
        assert_eq!(b.consecutive_failures("n1"), 0);
        assert!(!b.is_parked("n1"));
        // a fresh streak starts after the success
        assert!(!b.record_failure("n1"));
        assert!(b.record_failure("n1"));
        assert!(b.is_parked("n1"));
    }

    #[test]
    fn breaker_tracks_nodes_independently() {
        let mut b = CircuitBreaker::new(2);
        b.record_failure("a");
        b.record_failure("b");
        assert_eq!(b.consecutive_failures("a"), 1);
        assert_eq!(b.consecutive_failures("b"), 1);
        assert!(b.record_failure("a")); // a trips
        assert!(b.is_parked("a"));
        assert!(!b.is_parked("b")); // b unaffected
    }

    #[test]
    fn parked_node_does_not_retrip() {
        let mut b = CircuitBreaker::new(1);
        assert!(b.record_failure("n1")); // trips at limit 1
        assert!(b.is_parked("n1"));
        // further failures on a parked node return false (no re-trip / no event spam)
        assert!(!b.record_failure("n1"));
    }

    #[test]
    fn zero_limit_is_clamped_to_one() {
        let mut b = CircuitBreaker::new(0);
        // clamped to 1: first failure trips
        assert!(b.record_failure("n1"));
        assert!(b.is_parked("n1"));
    }

    #[test]
    fn parked_nodes_enumerates_parked_set() {
        let mut b = CircuitBreaker::new(1);
        b.record_failure("a");
        b.record_failure("b");
        let mut parked: Vec<String> = b.parked_nodes().cloned().collect();
        parked.sort();
        assert_eq!(parked, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn walker_key_is_canonical_and_prefixed() {
        let k = walker_key_for(&PathBuf::from("/tmp"));
        assert!(k.starts_with("walker:"));
    }
}
