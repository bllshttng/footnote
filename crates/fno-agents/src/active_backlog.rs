//! Active backlog dispatcher: the drain-tick core + circuit breaker (node x-c070).
//!
//! This module is the engine for the always-on backlog drain. One *tick*
//! claims the project's `walker:<cwd>` singleton, asks the existing
//! [`MegawalkQueue`] for the next ready node, and runs it to termination
//! through the unchanged [`run_loop`] primitive. The daemon's resident
//! supervisor ([`run_supervisor`]) drives one independent drain loop PER
//! enabled project, so a long-running drain in one project never starves
//! another.
//!
//! ## Single-owner contract (AC1-FR)
//!
//! The tick acquires `walker:<cwd>` (holder `active-backlog:<pid>`) at the
//! start and RELEASES it at the end. Releasing per-tick is deliberate: it lets
//! a human `/megawalk` grab the singleton between ticks, after which the
//! daemon's next acquire fails and the tick YIELDS (`active_backlog_yield`).
//! The walker-claim commands run with `current_dir` set to the TARGET project's
//! cwd, so the daemon and a manual `/megawalk` (which runs in that cwd) store
//! the `walker:<root>` singleton in the SAME claims dir and genuinely contend -
//! a global daemon launched from elsewhere must still yield.
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
//! ## Circuit-breaker park (recoverable)
//!
//! When a node fails `failure_limit` consecutive drains the breaker trips and
//! the daemon `fno backlog defer`s the node (graph state), then resets the
//! in-memory streak. Deferring (not an endlessly-refreshed claim) is what makes
//! the park recoverable: `fno backlog undefer` returns the node to the ready
//! pool with a fresh `failure_limit` attempts, exactly as the plan specifies.
//!
//! ## Events (Journal contract)
//!
//! Every transition emits through [`Journal::append`] (project journal fatal,
//! global mirror best-effort), matching every other loop event:
//! `active_backlog_dispatched` / `_yield` / `_parked` / `_skip`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::events::EventEmitter;
use crate::loop_dispatch::ShelloutDispatcher;
use crate::loop_megawalk::{abi_cmd, MegawalkDispatcher, MegawalkQueue};
use crate::loop_runtime::{
    run_loop, CloseOutcome, DispatchCtx, Dispatcher, GlobalJournalPath, Journal, LoopBudget,
    LoopError, ProjectJournalPath, Session, Unit,
};
use crate::loopcheck::TerminationReason;

/// Cross-tick per-node consecutive-failure counter (the circuit breaker).
///
/// Hermes semantics: increment on a failed drain, reset to zero on a successful
/// close. When the streak reaches `failure_limit` the caller trips: it
/// `fno backlog defer`s the node and then [`reset`](Self::reset)s the streak, so
/// the graph (not an in-memory set) owns the exclusion and `fno backlog undefer`
/// recovers the node with a fresh `failure_limit` attempts. This struct is the
/// pure counting policy; the defer IO is the caller's step.
#[derive(Debug, Default)]
pub struct CircuitBreaker {
    failure_limit: u32,
    failures: HashMap<String, u32>,
}

impl CircuitBreaker {
    /// `failure_limit` is clamped to at least 1 (a zero limit would trip every
    /// node on its first failure, which is never the intent).
    pub fn new(failure_limit: u32) -> Self {
        Self {
            failure_limit: failure_limit.max(1),
            failures: HashMap::new(),
        }
    }

    /// Record a failed drain for `node`. Returns `true` iff this failure trips
    /// the breaker (the streak just reached `failure_limit`).
    pub fn record_failure(&mut self, node: &str) -> bool {
        let n = self.failures.entry(node.to_string()).or_insert(0);
        *n += 1;
        *n >= self.failure_limit
    }

    /// Record a successful close for `node`: clear the streak.
    pub fn record_success(&mut self, node: &str) {
        self.failures.remove(node);
    }

    /// Clear the streak for `node` (called after a trip+defer so a later
    /// `undefer` gives the node a fresh `failure_limit` attempts).
    pub fn reset(&mut self, node: &str) {
        self.failures.remove(node);
    }

    /// The current consecutive-failure count for `node` (0 if none).
    pub fn consecutive_failures(&self, node: &str) -> u32 {
        self.failures.get(node).copied().unwrap_or(0)
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
    /// Batch-lane opt-in (config.batch.enabled, x-6cdf). When false the dispatch
    /// path is byte-for-byte today's one-PR-per-node behavior.
    pub batch: bool,
}

/// What one [`drain_tick`] did, for the scheduler and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// A node was selected and closed successfully.
    Dispatched { node: String },
    /// A node tripped the circuit breaker and was deferred (parked).
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

/// Acquire a claim, running in `cwd` so a per-repo claim (e.g. `walker:<root>`)
/// lands in the target project's claims dir - the same one a manual `/megawalk`
/// (run from that cwd) uses. Without this anchoring a global daemon would store
/// the walker singleton under its own cwd and never contend with a manual walk
/// (codex finding: AC1-FR).
fn acquire_claim(
    abi_bin: &str,
    cwd: &Path,
    key: &str,
    holder: &str,
    ttl: &str,
    reason: &str,
) -> ClaimResult {
    match abi_cmd(abi_bin)
        .current_dir(cwd)
        .args([
            "claim", "acquire", key, "--holder", holder, "--ttl", ttl, "--reason", reason,
        ])
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

fn release_claim(abi_bin: &str, cwd: &Path, key: &str, holder: &str) {
    let _ = abi_cmd(abi_bin)
        .current_dir(cwd)
        .args(["claim", "release", key, "--holder", holder])
        .output();
}

/// Best-effort `fno backlog defer <node>` for the circuit-breaker park. Graph
/// state, recoverable via `fno backlog undefer`.
fn defer_node(abi_bin: &str, cwd: &Path, node: &str, reason: &str) {
    let _ = abi_cmd(abi_bin)
        .current_dir(cwd)
        .args(["backlog", "defer", node, "--reason", reason])
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
/// a crash-looping node accumulates failures and is eventually deferred.
pub fn drain_tick(
    cfg: &DrainConfig,
    breaker: &mut CircuitBreaker,
    journal: &Journal,
) -> DrainOutcome {
    let walker_key = walker_key_for(&cfg.cwd);
    let walker_holder = format!("active-backlog:{}", std::process::id());

    match acquire_claim(
        &cfg.abi_bin,
        &cfg.cwd,
        &walker_key,
        &walker_holder,
        "24h",
        "active-backlog drain tick",
    ) {
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
            return DrainOutcome::Skipped {
                reason: format!("walker-claim-error: {e}"),
            };
        }
    }

    let outcome = run_one_node(cfg, breaker, journal);

    // Release the singleton on every exit path so a manual /megawalk can take
    // over before the next tick (AC1-FR).
    release_claim(&cfg.abi_bin, &cfg.cwd, &walker_key, &walker_holder);
    outcome
}

fn build_env(cfg: &DrainConfig) -> Vec<(String, String)> {
    let abilities_dir = cfg.cwd.join(".fno");
    let output_file = abilities_dir.join("target-last-output.txt");
    let history_file = abilities_dir.join("target-history.txt");
    let signal_file = abilities_dir.join("target-promise.signal");

    let mut env: Vec<(String, String)> = vec![
        (
            "OUTPUT_FILE".to_string(),
            output_file.to_string_lossy().into_owned(),
        ),
        (
            "HISTORY_FILE".to_string(),
            history_file.to_string_lossy().into_owned(),
        ),
        (
            "SIGNAL_FILE".to_string(),
            signal_file.to_string_lossy().into_owned(),
        ),
        ("MAX_TURNS".to_string(), cfg.max_turns.to_string()),
        ("BUDGET_USD".to_string(), format!("{}", cfg.budget_usd)),
        // CONTINUE_PROMPT is set per-unit by MegawalkDispatcher.
        ("CONTINUE_PROMPT".to_string(), String::new()),
        (
            "FNO_CWD".to_string(),
            cfg.cwd.to_string_lossy().into_owned(),
        ),
    ];
    match &cfg.model {
        Some(m) => env.push(("MODEL_FLAG".to_string(), format!("--model {m}"))),
        None => env.push(("MODEL_FLAG".to_string(), String::new())),
    }
    env
}

/// Batch-lane dispatch wrapper (x-6cdf). Consults `fno backlog batch prepare`
/// before each dispatch: on `batched` it dispatches `/target batched <id>` in
/// the shared batch worktree (env TARGET_BATCHED/TARGET_BATCH_WORKTREE/BRANCH);
/// on `solo` (or ANY failure - prepare is fail-safe) it delegates to the inner
/// [`MegawalkDispatcher`] for today's `/target no-merge <id>` behavior.
///
/// All the batch policy lives in Python (`batch prepare`); this wrapper only
/// rewrites the prompt + env when told to, so the Rust surface stays thin.
struct BatchDispatcher {
    inner: MegawalkDispatcher,
    driver_lib: PathBuf,
    static_env: Vec<(String, String)>,
    abi_bin: String,
    /// Canonical repo root, passed to `fno worktree ensure --repo`.
    repo: PathBuf,
    /// (node_id, domain) of the last node actually dispatched batched this tick.
    /// run_one_node reads it to abandon the batch if the member then FAILED
    /// (a solo dispatch never records here). Mutex because `Dispatcher::run`
    /// takes `&self`; the daemon dispatches one node per tick so contention is nil.
    last_batched: std::sync::Mutex<Option<(String, String)>>,
}

impl Dispatcher for BatchDispatcher {
    fn run(&self, unit: &Unit, ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
        let prep = abi_cmd(&self.abi_bin)
            .args([
                "backlog",
                "batch",
                "prepare",
                "--node",
                &unit.id,
                "--repo",
                &self.repo.to_string_lossy(),
            ])
            // Run from the target repo so the batch-state root resolves to this
            // project's canonical checkout (matches ship_closeable_batches), not
            // the daemon's launch cwd. Without this, prepare and ship-closeable
            // could resolve different `.fno/batches/` roots.
            .current_dir(&self.repo)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok());

        let batched = prep
            .as_ref()
            .and_then(|v| v.get("worktree").and_then(|w| w.as_str()));
        let (Some(prep), Some(worktree)) = (prep.as_ref(), batched) else {
            // solo, or prepare unavailable/failed -> today's behavior.
            return self.inner.run(unit, ctx);
        };
        if prep.get("mode").and_then(|m| m.as_str()) != Some("batched") {
            return self.inner.run(unit, ctx);
        }
        let branch = prep
            .get("branch")
            .and_then(|b| b.as_str())
            .unwrap_or_default();
        if worktree.is_empty() || branch.is_empty() {
            return self.inner.run(unit, ctx); // fail-safe: never dispatch batched without a worktree
        }
        // Record (node, domain) so run_one_node can abandon this batch if the
        // member then fails (a solo dispatch never reaches here).
        let domain = prep
            .get("domain")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        if let Ok(mut slot) = self.last_batched.lock() {
            *slot = Some((unit.id.clone(), domain));
        }

        // Batched dispatch: run `/target batched <id>` in the shared worktree.
        let mut env = self.static_env.clone();
        env.retain(|(k, _)| k != "CONTINUE_PROMPT" && k != "TARGET_SESSION_ID");
        env.push((
            "CONTINUE_PROMPT".to_string(),
            format!("/target batched {}", unit.id),
        ));
        env.push(("TARGET_SESSION_ID".to_string(), unit.session_key.clone()));
        env.push(("TARGET_BATCHED".to_string(), "1".to_string()));
        env.push(("TARGET_BATCH_WORKTREE".to_string(), worktree.to_string()));
        env.push(("TARGET_BATCH_BRANCH".to_string(), branch.to_string()));
        env.extend(unit.extra_env.iter().cloned());

        let dispatcher =
            ShelloutDispatcher::new(self.driver_lib.clone(), env, PathBuf::from(worktree));
        dispatcher.run(unit, ctx)
    }
}

/// After a batched tick, ship any open batch whose close condition tripped.
/// Best-effort: a failure here never wedges the daemon (the batch stays open
/// and a later tick / drain ships it).
fn ship_closeable_batches(cfg: &DrainConfig, journal: &Journal) {
    let mut cmd = abi_cmd(&cfg.abi_bin);
    cmd.args(["backlog", "batch", "ship-closeable"]);
    if let Some(ref p) = cfg.project {
        cmd.args(["--project", p]);
    }
    cmd.current_dir(&cfg.cwd);
    match cmd.output() {
        Ok(o) if o.status.success() => {
            let _ = journal.append(
                "active_backlog_batch_ship",
                json!({"stdout": String::from_utf8_lossy(&o.stdout).trim()}),
            );
        }
        Ok(o) => {
            let _ = journal.append(
                "active_backlog_batch_ship",
                json!({"error": String::from_utf8_lossy(&o.stderr).trim()}),
            );
        }
        Err(e) => {
            let _ = journal.append(
                "active_backlog_batch_ship",
                json!({"error": format!("{e}")}),
            );
        }
    }
}

/// Abandon a domain's open batch and requeue its members as individual PRs (v1
/// failure policy). Called when a batched member failed. Best-effort: a failure
/// here never wedges the daemon.
fn abandon_batch_for(cfg: &DrainConfig, journal: &Journal, domain: &str) {
    let out = abi_cmd(&cfg.abi_bin)
        .args(["backlog", "batch", "abandon", "--domain", domain])
        .current_dir(&cfg.cwd)
        .output();
    let detail = match out {
        Ok(o) if o.status.success() => "ok".to_string(),
        Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
        Err(e) => format!("{e}"),
    };
    let _ = journal.append(
        "active_backlog_batch_abandon",
        json!({"domain": domain, "detail": detail}),
    );
}

fn run_one_node(
    cfg: &DrainConfig,
    breaker: &mut CircuitBreaker,
    journal: &Journal,
) -> DrainOutcome {
    let mut queue =
        MegawalkQueue::new_with_max_units(cfg.abi_bin.clone(), cfg.project.clone(), false, Some(1))
            .with_mission(cfg.mission.clone());
    let inner = MegawalkDispatcher::new(
        cfg.lib_path.clone(),
        build_env(cfg),
        cfg.cwd.clone(),
        cfg.abi_bin.clone(),
        cfg.allow_merge,
    );
    // When batch-lane is on, wrap the dispatcher so a candidate node can be
    // coalesced onto a shared batch branch; otherwise dispatch as today.
    let batch_dispatcher = cfg.batch.then(|| BatchDispatcher {
        inner: MegawalkDispatcher::new(
            cfg.lib_path.clone(),
            build_env(cfg),
            cfg.cwd.clone(),
            cfg.abi_bin.clone(),
            cfg.allow_merge,
        ),
        driver_lib: cfg.lib_path.clone(),
        static_env: build_env(cfg),
        abi_bin: cfg.abi_bin.clone(),
        repo: crate::paths::canonical_repo_root(&cfg.cwd).unwrap_or_else(|| cfg.cwd.clone()),
        last_batched: std::sync::Mutex::new(None),
    });
    let dispatcher: &dyn Dispatcher = match &batch_dispatcher {
        Some(b) => b,
        None => &inner,
    };

    let budget = match LoopBudget::new(cfg.max_iterations.max(1)) {
        Ok(b) => b,
        Err(e) => {
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "bad-budget", "detail": format!("{e}")}),
            );
            return DrainOutcome::Skipped {
                reason: format!("bad-budget: {e}"),
            };
        }
    };

    // A daemon tick honors the project cancel sentinel (the same one /target and
    // megawalk watch); there is no SIGINT in the drain task's blocking thread.
    let cancel_file = cfg.cwd.join(".fno").join(".target-cancelled");
    let cancel = move || cancel_file.exists();

    let outcome = match run_loop(
        &mut queue,
        dispatcher,
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
            return DrainOutcome::Skipped {
                reason: format!("loop-error: {e}"),
            };
        }
    };

    // Batch-lane close handling, gated on the member's OUTCOME (codex P1):
    //  - DoneBatched (success): the member committed cleanly, so evaluate the
    //    close condition and ship any batch that should close.
    //  - a batched member that did NOT reach DoneBatched (NoProgress/Budget/
    //    Aborted/etc.): abandon its batch + requeue members (v1 failure policy),
    //    so ship-closeable can never open a PR over a failed member's partial
    //    commits. A solo dispatch recorded nothing, so it is untouched here.
    if cfg.batch {
        let last = outcome.units.last();
        let done_batched =
            last.is_some_and(|u| matches!(u.evidence.reason, TerminationReason::DoneBatched));
        if done_batched {
            ship_closeable_batches(cfg, journal);
        } else if let Some(u) = last {
            let batched_domain = batch_dispatcher
                .as_ref()
                .and_then(|b| b.last_batched.lock().ok().and_then(|s| s.clone()))
                .filter(|(nid, _)| nid == &u.unit_id)
                .map(|(_, domain)| domain);
            if let Some(domain) = batched_domain {
                abandon_batch_for(cfg, journal, &domain);
            }
        }
    }

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
                DrainOutcome::Skipped {
                    reason: format!("{other:?}"),
                }
            }
        };
    };

    let node = last.unit_id.clone();

    // Batch-lane (x-6cdf): a member that terminated DoneBatched succeeded - its
    // commits are on the shared batch branch and it ships via the batch PR (the
    // node is closed at merge by `fno backlog reconcile`, not here). MegawalkQueue
    // Parks it because the node is not `done`, but for the daemon that is a
    // SUCCESSFUL dispatch, not a failure. Recognize it here (in the keep-set,
    // never by deepening loop_megawalk.rs) so a batched member never trips the
    // cross-tick circuit breaker. ship-closeable (called by run_one_node) opens
    // the batch PR once the close condition trips.
    if matches!(last.evidence.reason, TerminationReason::DoneBatched) {
        breaker.record_success(&node);
        let _ = journal.append(
            "active_backlog_dispatched",
            json!({"node_id": node, "termination": "DoneBatched", "batched": true}),
        );
        return DrainOutcome::Dispatched { node };
    }

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
                // Park by deferring the node in graph state (recoverable via
                // `fno backlog undefer`), then reset the streak so a later
                // undefer gives it a fresh failure_limit attempts.
                let reason_str = format!(
                    "auto-failure: {} consecutive failed drains",
                    cfg.failure_limit
                );
                defer_node(&cfg.abi_bin, &cfg.cwd, &node, &reason_str);
                breaker.reset(&node);
                let _ = journal.append(
                    "active_backlog_parked",
                    json!({"node_id": node, "consecutive_failures": cfg.failure_limit, "detail": detail}),
                );
                DrainOutcome::Parked {
                    node,
                    failures: cfg.failure_limit,
                }
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
                DrainOutcome::Skipped {
                    reason: format!("node {node} not closed: {detail}"),
                }
            }
        }
    }
}

// ── target resolution + resident supervisor ─────────────────────────────────────

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
    /// config.batch.enabled for this repo (x-6cdf). Absent in an older emitter
    /// -> false, so a stale `fno` never accidentally enables batched dispatch.
    #[serde(default)]
    pub batch: bool,
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
        batch: target.batch,
    })
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
/// The blocking `stat` is offloaded to the blocking pool so polling it every
/// 500ms never blocks the async executor (gemini finding). `tokio::fs` is not
/// used to avoid adding the `fs` feature to the tokio dependency.
async fn nudge_mtime() -> Option<std::time::SystemTime> {
    tokio::task::spawn_blocking(|| {
        std::fs::metadata(nudge_sentinel_path())
            .and_then(|m| m.modified())
            .ok()
    })
    .await
    .ok()
    .flatten()
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
        let current = nudge_mtime().await;
        if current != *last {
            *last = current;
            return; // event nudge: wake early (coalesced)
        }
        let chunk = step.min(total - elapsed);
        tokio::time::sleep(chunk).await;
        elapsed += chunk;
    }
}

/// The resident drain supervisor (node x-c070).
///
/// Spawns ONE independent drain loop per enabled project so a long-running drain
/// in one project never blocks or starves another (gemini finding). It sets
/// `live` true whenever there is >=1 enabled target so the daemon's idle-exit
/// stays out (OQ1 Option A: an enabled but drained board keeps the daemon
/// resident and polling). Runs until `shutdown` is set, then aborts the
/// per-project loops; an in-flight `spawn_blocking` tick is not abortable, but
/// that is safe by design - the dispatched worker owns its `node:<id>` claim
/// independently and the live-claims filter excludes it on the next start.
pub async fn run_supervisor(
    abi_bin: String,
    emitter: EventEmitter,
    live: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    let mut tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let recheck = Duration::from_secs(60);

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // Drop handles for loops that have exited (e.g. a project was disabled).
        tasks.retain(|_, h| !h.is_finished());

        let targets = resolve_targets(&abi_bin);
        live.store(!targets.is_empty(), Ordering::SeqCst);

        for target in targets {
            if tasks.contains_key(&target.project) {
                continue;
            }
            let project = target.project.clone();
            let h = tokio::spawn(per_project_drain_loop(
                target,
                abi_bin.clone(),
                emitter.clone(),
                Arc::clone(&shutdown),
            ));
            tasks.insert(project, h);
        }

        sleep_interruptible(recheck, &shutdown).await;
    }

    for (_, h) in tasks {
        h.abort();
    }
    live.store(false, Ordering::SeqCst);
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

/// One project's independent drain loop: drain a node, wait the poll floor (or an
/// event nudge), repeat. Owns its own [`CircuitBreaker`] so failure streaks are
/// per-project. Exits when `shutdown` flips OR the project drops out of
/// `config.active_backlog` (re-resolved between ticks - AC2-EDGE: a disable
/// finishes the current dispatch then schedules no more).
async fn per_project_drain_loop(
    target: ResolvedTarget,
    abi_bin: String,
    emitter: EventEmitter,
    shutdown: Arc<AtomicBool>,
) {
    let project = target.project.clone();
    let mut breaker = CircuitBreaker::new(target.failure_limit);
    let mut last_nudge = nudge_mtime().await;
    let mut backoff = Duration::from_secs(1);

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        // Re-resolve this project's enablement (config may have changed). If it
        // is no longer enabled, exit the loop (the supervisor will not respawn).
        let current = resolve_targets(&abi_bin)
            .into_iter()
            .find(|t| t.project == project);
        let Some(t) = current else {
            break;
        };
        let interval = Duration::from_secs(t.interval_seconds.max(1));

        let Some(cfg) = drain_config_for(&t, &abi_bin) else {
            // No driver lib (yet); back off and re-check rather than spin.
            sleep_interruptible(interval, &shutdown).await;
            continue;
        };
        let journal = journal_for(&cfg.cwd);

        // run_loop is synchronous; offload so the async runtime is never stalled.
        // Move the breaker in and hand it back so the streak survives the tick.
        let taken = std::mem::take(&mut breaker);
        let handle = tokio::task::spawn_blocking(move || {
            let mut b = taken;
            let outcome = drain_tick(&cfg, &mut b, &journal);
            (outcome, b)
        });
        match handle.await {
            Ok((_outcome, b)) => {
                breaker = b;
                backoff = Duration::from_secs(1);
            }
            Err(join_err) => {
                let _ = emitter.emit(
                    "active_backlog_task_crashed",
                    &json!({"project": project, "error": join_err.to_string()}),
                );
                // The panicked breaker's streak is lost (rare); a fresh one is
                // safe (a crash-looping node re-accrues failures and re-defers).
                breaker = CircuitBreaker::new(t.failure_limit);
                sleep_interruptible(backoff, &shutdown).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        }

        wait_for_wake(interval, &shutdown, &mut last_nudge).await;
    }
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
        assert_eq!(b.consecutive_failures("n1"), 3);
    }

    #[test]
    fn breaker_success_resets_streak() {
        let mut b = CircuitBreaker::new(2);
        b.record_failure("n1");
        assert_eq!(b.consecutive_failures("n1"), 1);
        b.record_success("n1");
        assert_eq!(b.consecutive_failures("n1"), 0);
        // a fresh streak starts after the success
        assert!(!b.record_failure("n1"));
        assert!(b.record_failure("n1"));
    }

    #[test]
    fn breaker_reset_gives_fresh_attempts() {
        // Models trip -> defer -> reset: after a reset the node gets a fresh
        // failure_limit run (the undefer-recovery contract).
        let mut b = CircuitBreaker::new(2);
        assert!(!b.record_failure("n1"));
        assert!(b.record_failure("n1")); // trips
        b.reset("n1"); // caller deferred + reset
        assert_eq!(b.consecutive_failures("n1"), 0);
        assert!(!b.record_failure("n1")); // fresh streak
        assert!(b.record_failure("n1")); // trips again
    }

    #[test]
    fn breaker_tracks_nodes_independently() {
        let mut b = CircuitBreaker::new(2);
        b.record_failure("a");
        b.record_failure("b");
        assert_eq!(b.consecutive_failures("a"), 1);
        assert_eq!(b.consecutive_failures("b"), 1);
        assert!(b.record_failure("a")); // a trips
        assert_eq!(b.consecutive_failures("b"), 1); // b unaffected
    }

    #[test]
    fn zero_limit_is_clamped_to_one() {
        let mut b = CircuitBreaker::new(0);
        // clamped to 1: first failure trips
        assert!(b.record_failure("n1"));
    }

    #[test]
    fn walker_key_is_canonical_and_prefixed() {
        let k = walker_key_for(&PathBuf::from("/tmp"));
        assert!(k.starts_with("walker:"));
    }
}
