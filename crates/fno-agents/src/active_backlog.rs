//! Active backlog dispatcher: the drain-tick core + circuit breaker.
//!
//! This module is the engine for the always-on backlog drain. One *tick*
//! claims the project's `walker:<cwd>` singleton, RECONCILES any dispatch it
//! fired on a prior tick from events, and - when nothing is in flight -
//! DISPATCHES the next ready node fire-and-forget. The daemon's resident
//! supervisor ([`run_supervisor`]) drives one independent drain loop PER
//! enabled project, so a long-running drain in one project never starves
//! another.
//!
//! ## Fire-and-forget scheduler (x-0ad6)
//!
//! The tick does NOT own the worker child. Sequential dispatch goes through the
//! lane machinery (`fno backlog dispatch-lanes --max 1`, itself a `fno agents
//! spawn`), which self-mints the worker session and re-anchors the `node:<id>`
//! claim to `target-session:<sid>`. The tick returns immediately; a later tick
//! RECONCILES the dispatch by reading the worker's session id back from the
//! claim holder and polling its termination event (`Journal::find_termination`),
//! then feeding the outcome through [`map_outcome`] - the same policy the old
//! supervised path used, so the auto-defer streak is identical. A worker that
//! dies without emitting any termination event is caught by the crash floor
//! (claim gone past the boot window), which replaces the awaited-exit-code
//! watchdog the fire-and-forget model can no longer read. See
//! [`reconcile_pending`] / [`dispatch_one`].
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
//! Sequential mode keeps at most one worker in flight: a tick that finds a
//! prior dispatch still pending reconciles it and yields without dispatching
//! another. The in-flight worker's node closes when its termination event is
//! polled (or at merge via `fno backlog reconcile` for a no-merge dispatch);
//! only then does the next tick dispatch the next node.
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

use crate::claims::{self, ClaimState};
use crate::events::EventEmitter;
use crate::loop_megawalk::abi_cmd;
use crate::loop_runtime::{
    CloseOutcome, Evidence, GlobalJournalPath, Journal, ProjectJournalPath, UnitResult,
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
    /// Parallel-mode lane cap (config.parallel.max_lanes, x-42d5 G4). `>= 2`
    /// switches the tick to fire-and-forget lane-fill (`fno backlog
    /// dispatch-lanes`); anything below is today's sequential in-tick drain
    /// with no special-case code (AC1-EDGE).
    pub max_lanes: u64,
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
    /// Parallel mode (x-42d5 G4): the tick fire-and-forgot `dispatched` bg
    /// lanes (and `skipped` selected-but-unlaunched ones). Lanes run detached;
    /// their nodes close at merge via `fno backlog reconcile`, not in-tick.
    LanesDispatched { dispatched: usize, skipped: usize },
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
    pending: &mut Vec<PendingDispatch>,
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

    // Parallel mode (x-42d5 G4): with a lane cap >= 2, the tick fills lanes
    // instead of draining one node in-tick. Running under the walker singleton
    // is what makes the selection atomic (select_lane_fill's single-dispatcher
    // contract). Below 2 the sequential path runs, now itself fire-and-forget
    // (x-0ad6): reconcile prior dispatches from events, then dispatch one more
    // only when nothing is in flight (sequential = at most one worker at a time).
    let outcome = if cfg.max_lanes >= 2 {
        lane_fill_tick(cfg, journal)
    } else {
        reconcile_pending(cfg, breaker, pending, journal);
        if pending.is_empty() {
            dispatch_one(cfg, pending, journal)
        } else {
            // A worker from a prior tick is still in flight; a later tick
            // reconciles it from events. The tick returns immediately instead of
            // blocking on the child (the whole point of the retarget).
            DrainOutcome::Skipped {
                reason: format!("{} worker(s) in flight", pending.len()),
            }
        }
    };

    // Release the singleton on every exit path so a manual /megawalk can take
    // over before the next tick (AC1-FR).
    release_claim(&cfg.abi_bin, &cfg.cwd, &walker_key, &walker_holder);
    outcome
}

/// Count lane receipts by status from `fno backlog dispatch-lanes` JSON output
/// (a list of `{node_id, status: dispatched|skipped, ...}` objects). Pure so the
/// receipt contract is unit-testable without spawning workers.
fn count_lane_receipts(stdout: &[u8]) -> Result<(usize, usize), String> {
    let receipts: Vec<serde_json::Value> =
        serde_json::from_slice(stdout).map_err(|e| format!("unparseable receipts: {e}"))?;
    let dispatched = receipts
        .iter()
        .filter(|r| r.get("status").and_then(|s| s.as_str()) == Some("dispatched"))
        .count();
    Ok((dispatched, receipts.len() - dispatched))
}

/// One parallel-mode tick: fire-and-forget up to `max_lanes` isolated bg lanes
/// via the Python dispatcher (`fno backlog dispatch-lanes`, which owns slot
/// claims, worktree isolation, and spawn - G1-G3). The tick does NOT wait on
/// lanes: they are detached `claude --bg` workers whose nodes close at merge
/// (`fno backlog reconcile`), and later ticks fill freed slots. Per-lane
/// failures are already contained inside dispatch_lanes (skip-and-log, slot
/// released); a whole-command failure is journaled and skips the tick.
fn lane_fill_tick(cfg: &DrainConfig, journal: &Journal) -> DrainOutcome {
    let mut cmd = abi_cmd(&cfg.abi_bin);
    cmd.args([
        "backlog",
        "dispatch-lanes",
        "--max",
        &cfg.max_lanes.to_string(),
    ]);
    if let Some(ref p) = cfg.project {
        cmd.args(["--project", p]);
    }
    // Mission scope must survive the seam: the sequential path applies it via
    // MegawalkQueue::with_mission, and dropping it here would let a
    // mission-scoped daemon lane-fill unrelated same-project nodes (codex P1).
    if let Some(ref m) = cfg.mission {
        cmd.args(["--mission", m]);
    }
    cmd.current_dir(&cfg.cwd);
    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let detail = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "dispatch-lanes-failed", "detail": detail}),
            );
            return DrainOutcome::Skipped {
                reason: format!("dispatch-lanes-failed: {detail}"),
            };
        }
        Err(e) => {
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "dispatch-lanes-failed", "detail": format!("{e}")}),
            );
            return DrainOutcome::Skipped {
                reason: format!("dispatch-lanes-failed: {e}"),
            };
        }
    };
    match count_lane_receipts(&out.stdout) {
        Ok((0, 0)) => DrainOutcome::NoWork,
        Ok((dispatched, skipped)) => {
            let _ = journal.append(
                "active_backlog_dispatched",
                json!({
                    "lanes": true,
                    "dispatched": dispatched,
                    "skipped": skipped,
                    "max_lanes": cfg.max_lanes,
                }),
            );
            DrainOutcome::LanesDispatched {
                dispatched,
                skipped,
            }
        }
        Err(detail) => {
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "dispatch-lanes-unparseable", "detail": detail}),
            );
            DrainOutcome::Skipped {
                reason: format!("dispatch-lanes-unparseable: {detail}"),
            }
        }
    }
}

/// Map a dispatched node's termination outcome to a [`DrainOutcome`], updating
/// the breaker and emitting the decision event. Fed by [`reconcile_pending`]
/// with the evidence polled from the worker's own termination event, so the
/// success/park policy is identical to the old supervised path without spawning
/// a real worker.
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

    // Batch-lane: a member that terminated DoneBatched succeeded - its commits
    // are on the shared batch branch and it ships via the batch PR, so the node
    // closes at merge by `fno backlog reconcile`, not here. For the daemon that
    // is a SUCCESSFUL dispatch, not a failure. Recognize it in the keep-set so a
    // batched member never trips the cross-tick circuit breaker.
    if matches!(last.evidence.reason, TerminationReason::DoneBatched) {
        breaker.record_success(&node);
        let _ = journal.append(
            "active_backlog_dispatched",
            json!({"node_id": node, "termination": "DoneBatched", "batched": true}),
        );
        return DrainOutcome::Dispatched { node };
    }

    // DoneAwaitingMerge: the node built successfully (PR up, reviewed)
    // but could not merge past a proven pre-existing main-red. That is a
    // SUCCESSFUL dispatch for the daemon, not a failure - the node is closed at
    // the human merge by `fno backlog reconcile`, exactly like DoneBatched. Keep
    // it out of the cross-tick circuit breaker (mirror the DoneBatched keep-set).
    if matches!(last.evidence.reason, TerminationReason::DoneAwaitingMerge) {
        breaker.record_success(&node);
        let _ = journal.append(
            "active_backlog_dispatched",
            json!({"node_id": node, "termination": "DoneAwaitingMerge", "awaiting_merge": true}),
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

// ── fire-and-forget sequential scheduler (x-0ad6) ────────────────────────────
//
// The retargeted sequential drain: a tick DISPATCHES one ready node
// fire-and-forget through the proven lane machinery (`fno backlog dispatch-lanes
// --max 1`, which routes through `fno agents spawn`, self-mints the worker
// session, and re-anchors the `node:<id>` claim to `target-session:<sid>`), then
// RECONCILES prior dispatches from events across later ticks - never owning the
// worker child. This replaces the supervised `run_one_node`/`ShelloutDispatcher`
// path whose `session.wait()` blocked the whole tick.
//
// Failure accounting is reconstructed from the worker's own termination event
// (find_termination on the session id read back from the claim holder) fed
// through the SAME `map_outcome` policy the supervised path used, so the
// auto-defer streak is identical by construction. A worker that dies without
// emitting any termination event is caught by the crash floor (claim gone past
// the boot window), replacing the awaited-exit-code `node_failed` watchdog the
// fire-and-forget model can no longer read.

/// A ready node dispatched fire-and-forget in a prior tick, polled to completion
/// from events.
#[derive(Debug, Clone)]
pub struct PendingDispatch {
    node_id: String,
    /// The worker's session id, read back from the `node:<id>` claim holder
    /// (`target-session:<sid>`) once the worker inits and re-anchors the claim.
    /// `None` until first observed; find_termination cannot be polled before it.
    session_id: Option<String>,
    /// Reconcile passes since dispatch. Guards the boot window: a worker that has
    /// not yet taken the node claim holds none, which must not read as a death
    /// until `BOOT_GRACE_TICKS` have elapsed.
    ticks: u32,
}

/// Reconcile passes to wait for a dispatched worker to take its `node:<id>`
/// claim before a claim-absent verdict counts as a boot crash.
const BOOT_GRACE_TICKS: u32 = 3;

/// True for the terminal reasons `MegawalkQueue::close` marks the node done
/// (mirrors `loop_megawalk::is_done_reason`, kept local to avoid widening its
/// visibility). `DoneBatched`/`DoneAwaitingMerge` are NOT here - they close at
/// merge and are recognized as success by `map_outcome`'s keep-set instead.
fn is_done_reason(r: &TerminationReason) -> bool {
    matches!(
        r,
        TerminationReason::DonePRGreen | TerminationReason::DoneAdvisory
    )
}

/// Poll each in-flight dispatch and retire the ones that finished, updating the
/// breaker through `map_outcome` (identical policy to the supervised path).
/// Resolved entries are removed from `pending`.
fn reconcile_pending(
    cfg: &DrainConfig,
    breaker: &mut CircuitBreaker,
    pending: &mut Vec<PendingDispatch>,
    journal: &Journal,
) {
    let root = Some(cfg.cwd.as_path());
    pending.retain_mut(|p| {
        p.ticks += 1;
        let (state, rec) = claims::status(&format!("node:{}", p.node_id), root);
        if let Some(sid) = rec
            .as_ref()
            .and_then(|r| r.holder.strip_prefix("target-session:"))
        {
            p.session_id = Some(sid.to_string());
        }
        // Live/Suspect: the worker (or its TTL) still holds the node claim.
        // Suspect is a respawned-supervisor worker, never a death (claims.rs).
        let worker_live = matches!(state, ClaimState::Live | ClaimState::Suspect);

        // A termination event is authoritative whenever we can poll for it,
        // held claim or not (a worker can terminate a tick before release).
        if let Some(sid) = p.session_id.clone() {
            match journal.find_termination(&sid) {
                Ok(Some(ev)) => {
                    resolve_dispatch(cfg, breaker, journal, &p.node_id, ev);
                    return false;
                }
                Ok(None) if !worker_live => {
                    // Claim gone, session known, no event: the worker died
                    // mid-flight without terminating. Crash floor -> failure.
                    resolve_crash(cfg, breaker, journal, &p.node_id);
                    return false;
                }
                _ => {} // still running, or an unreadable journal this pass: keep
            }
        } else if !worker_live && p.ticks >= BOOT_GRACE_TICKS {
            // Never observed the worker take the node claim within the boot
            // window: the dispatch failed to start. Crash floor -> failure.
            resolve_crash(cfg, breaker, journal, &p.node_id);
            return false;
        }
        true
    });
}

/// Apply a polled termination event to the breaker via the shared `map_outcome`
/// policy, mirroring the supervised path's `queue.close` side effects.
fn resolve_dispatch(
    cfg: &DrainConfig,
    breaker: &mut CircuitBreaker,
    journal: &Journal,
    node_id: &str,
    ev: Evidence,
) {
    // Mirror MegawalkQueue::close: a DonePRGreen/DoneAdvisory close marks the
    // node done (idempotent, best-effort). DoneBatched/DoneAwaitingMerge close
    // at merge via reconcile and are NOT marked here - map_outcome's keep-set
    // records them as a successful dispatch without a `done` write.
    let close = if is_done_reason(&ev.reason) {
        let _ = abi_cmd(&cfg.abi_bin)
            .args(["backlog", "done", node_id])
            .current_dir(&cfg.cwd)
            .output();
        CloseOutcome::Closed
    } else {
        CloseOutcome::Parked(format!("session terminated: {:?}", ev.reason))
    };
    let reason = ev.reason.clone();
    let ur = UnitResult {
        unit_id: node_id.to_string(),
        evidence: ev,
        close,
    };
    map_outcome(cfg, breaker, journal, &reason, Some(&ur));
}

/// Crash floor: a dispatched worker died with no termination event. Synthesize
/// NoProgress evidence and feed the SAME `map_outcome` path, so the failure
/// counts toward the auto-defer streak exactly as the supervised `node_failed`
/// watchdog did.
fn resolve_crash(cfg: &DrainConfig, breaker: &mut CircuitBreaker, journal: &Journal, node_id: &str) {
    let message = "worker exited with no termination event (fire-and-forget crash floor)";
    let ur = UnitResult {
        unit_id: node_id.to_string(),
        evidence: Evidence {
            reason: TerminationReason::NoProgress,
            message: message.to_string(),
        },
        close: CloseOutcome::Parked(message.to_string()),
    };
    map_outcome(cfg, breaker, journal, &TerminationReason::NoProgress, Some(&ur));
}

/// Node ids the daemon dispatched fire-and-forget, parsed from a `fno backlog
/// dispatch-lanes` JSON receipt array (`status == "dispatched"`). Malformed or
/// empty input yields an empty list (the caller records nothing to reconcile).
fn dispatched_node_ids(stdout: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.get("status").and_then(|s| s.as_str()) == Some("dispatched"))
        .filter_map(|r| {
            r.get("node_id")
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// Fire-and-forget dispatch of one ready node through the lane machinery, with
/// the dispatched node recorded in `pending` for later event reconcile. Used by
/// the retargeted sequential tick (`max_lanes < 2`) in place of the supervised
/// `run_one_node`.
fn dispatch_one(cfg: &DrainConfig, pending: &mut Vec<PendingDispatch>, journal: &Journal) -> DrainOutcome {
    let mut cmd = abi_cmd(&cfg.abi_bin);
    cmd.args(["backlog", "dispatch-lanes", "--max", "1"]);
    if let Some(ref p) = cfg.project {
        cmd.args(["--project", p]);
    }
    if let Some(ref m) = cfg.mission {
        cmd.args(["--mission", m]);
    }
    cmd.current_dir(&cfg.cwd);
    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let detail = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "dispatch-one-failed", "detail": detail}),
            );
            return DrainOutcome::Skipped {
                reason: format!("dispatch-one-failed: {detail}"),
            };
        }
        Err(e) => {
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "dispatch-one-failed", "detail": format!("{e}")}),
            );
            return DrainOutcome::Skipped {
                reason: format!("dispatch-one-failed: {e}"),
            };
        }
    };
    let ids = dispatched_node_ids(&out.stdout);
    if ids.is_empty() {
        return DrainOutcome::NoWork;
    }
    for node_id in &ids {
        pending.push(PendingDispatch {
            node_id: node_id.clone(),
            session_id: None,
            ticks: 0,
        });
    }
    let node = ids[0].clone();
    let _ = journal.append(
        "active_backlog_dispatched",
        json!({"node_id": node, "fire_and_forget": true}),
    );
    DrainOutcome::Dispatched { node }
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
    /// config.parallel.max_lanes for this repo (x-42d5 G4). Absent in an older
    /// emitter -> 1 (sequential), so a stale `fno` never fans out into lanes.
    #[serde(default = "default_max_lanes")]
    pub max_lanes: u64,
}

fn default_max_lanes() -> u64 {
    1
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

/// A project the status-fanout supervisor should tick (x-2057). Enablement is
/// "has >=1 enabled status sink", INDEPENDENT of the drain's active_backlog set -
/// a project can fan status out without opting into the backlog drain.
#[derive(Debug, Clone, serde::Deserialize)]
struct FanoutTarget {
    pub project: String,
    pub cwd: String,
    pub interval_seconds: u64,
}

/// Shell `fno config status-sinks --json` to discover fanout targets. Best-effort:
/// any failure (missing fno, non-zero exit, unparseable output) yields an empty
/// list, so a broken config never crashes the daemon - it just runs no fanout.
fn resolve_fanout_targets(abi_bin: &str) -> Vec<FanoutTarget> {
    match abi_cmd(abi_bin)
        .args(["config", "status-sinks", "--json"])
        .output()
    {
        Ok(o) if o.status.success() => serde_json::from_slice(&o.stdout).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// One project's status-fanout loop: shell `fno status-fanout tick` in the
/// project cwd on the configured cadence, best-effort. Independent of the drain
/// loops; a tick failure is swallowed and the next tick retries. Between ticks it
/// re-resolves its own enablement (codex P2): a new `interval_secs` is picked up,
/// and removing the project's sinks EXITS the loop (so `retain(!is_finished)`
/// reaps it) rather than ticking forever. Exits on shutdown.
async fn per_project_fanout_loop(target: FanoutTarget, abi_bin: String, shutdown: Arc<AtomicBool>) {
    let project = target.project.clone();
    let mut interval = Duration::from_secs(target.interval_seconds.max(1));
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // Re-resolve between ticks so config changes land without a daemon restart.
        match resolve_fanout_targets(&abi_bin)
            .into_iter()
            .find(|t| t.project == project)
        {
            Some(t) => interval = Duration::from_secs(t.interval_seconds.max(1)),
            None => break, // sinks removed for this project -> stop ticking.
        }
        let bin = abi_bin.clone();
        let cwd = target.cwd.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = std::process::Command::new(&bin)
                .args(["status-fanout", "tick"])
                .current_dir(&cwd)
                .output();
        })
        .await;
        sleep_interruptible(interval, &shutdown).await;
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
        max_lanes: target.max_lanes,
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
    // Sibling loop family (x-2057): status-fanout ticks, keyed by project. A
    // separate enablement set (projects with >=1 status sink) from the drain
    // above, so a sinks-only project fans out without opting into the drain.
    let mut fanout_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let recheck = Duration::from_secs(60);

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // Drop handles for loops that have exited (e.g. a project was disabled).
        tasks.retain(|_, h| !h.is_finished());
        fanout_tasks.retain(|_, h| !h.is_finished());

        let targets = resolve_targets(&abi_bin);
        let fanout_targets = resolve_fanout_targets(&abi_bin);
        // `live` keeps the daemon out of idle-exit while ANY supervised work
        // exists - drain OR fanout. A sink-only project (no active_backlog) must
        // keep the daemon alive, else the daemon idle-exits and kills its fanout
        // loop (codex P1).
        live.store(
            !targets.is_empty() || !fanout_targets.is_empty(),
            Ordering::SeqCst,
        );

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

        for ft in fanout_targets {
            // Entry API: one lookup, and only spawn when this project has no live
            // loop yet. A loop that already exists self-reconciles config changes.
            if let std::collections::hash_map::Entry::Vacant(slot) =
                fanout_tasks.entry(ft.project.clone())
            {
                slot.insert(tokio::spawn(per_project_fanout_loop(
                    ft,
                    abi_bin.clone(),
                    Arc::clone(&shutdown),
                )));
            }
        }

        sleep_interruptible(recheck, &shutdown).await;
    }

    for (_, h) in tasks {
        h.abort();
    }
    for (_, h) in fanout_tasks {
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
    // In-flight fire-and-forget dispatches, reconciled from events across ticks
    // (x-0ad6). Resident like the breaker so a worker dispatched one tick is
    // polled to completion on the next.
    let mut pending: Vec<PendingDispatch> = Vec::new();
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

        // drain_tick is synchronous; offload so the async runtime is never
        // stalled. Move the breaker AND pending set in and hand them back so the
        // streak and in-flight tracking survive the tick.
        let taken_b = std::mem::take(&mut breaker);
        let taken_p = std::mem::take(&mut pending);
        let handle = tokio::task::spawn_blocking(move || {
            let mut b = taken_b;
            let mut p = taken_p;
            let outcome = drain_tick(&cfg, &mut b, &mut p, &journal);
            (outcome, b, p)
        });
        match handle.await {
            Ok((_outcome, b, p)) => {
                breaker = b;
                pending = p;
                backoff = Duration::from_secs(1);
            }
            Err(join_err) => {
                let _ = emitter.emit(
                    "active_backlog_task_crashed",
                    &json!({"project": project, "error": join_err.to_string()}),
                );
                // The panicked breaker's streak is lost (rare); a fresh one is
                // safe (a crash-looping node re-accrues failures and re-defers).
                // Pending tracking is also lost, but the in-flight workers still
                // run and their nodes close at merge via `fno backlog reconcile`.
                breaker = CircuitBreaker::new(t.failure_limit);
                pending = Vec::new();
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
    fn status_fanout_targets_parse_from_json() {
        let json = br#"[{"project":"fno","cwd":"/repo/fno","interval_seconds":5}]"#;
        let targets: Vec<FanoutTarget> = serde_json::from_slice(json).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].project, "fno");
        assert_eq!(targets[0].cwd, "/repo/fno");
        assert_eq!(targets[0].interval_seconds, 5);
    }

    #[test]
    fn status_fanout_targets_empty_on_garbage() {
        let targets: Vec<FanoutTarget> = serde_json::from_slice(b"not json").unwrap_or_default();
        assert!(targets.is_empty());
    }

    #[test]
    fn lane_receipts_counts_by_status() {
        let out = br#"[
            {"node_id": "x-a", "status": "dispatched", "short_id": "s1"},
            {"node_id": "x-b", "status": "skipped", "error": "spawn rc=127"},
            {"node_id": "x-c", "status": "dispatched", "short_id": "s2"}
        ]"#;
        assert_eq!(count_lane_receipts(out), Ok((2, 1)));
    }

    #[test]
    fn lane_receipts_empty_list() {
        assert_eq!(count_lane_receipts(b"[]"), Ok((0, 0)));
    }

    #[test]
    fn lane_receipts_garbage_is_error() {
        assert!(count_lane_receipts(b"wedged traceback").is_err());
    }

    #[test]
    fn dispatched_node_ids_keeps_only_dispatched() {
        let out = br#"[
            {"node_id": "x-a", "status": "dispatched", "short_id": "s1"},
            {"node_id": "x-b", "status": "skipped", "error": "no slot"},
            {"node_id": "x-c", "status": "dispatched", "short_id": "s2"}
        ]"#;
        assert_eq!(dispatched_node_ids(out), vec!["x-a", "x-c"]);
    }

    #[test]
    fn dispatched_node_ids_empty_on_garbage_or_none() {
        // Malformed input, an empty receipt, and an all-skipped receipt all
        // yield nothing to reconcile (never a panic).
        assert!(dispatched_node_ids(b"wedged traceback").is_empty());
        assert!(dispatched_node_ids(b"[]").is_empty());
        assert!(dispatched_node_ids(br#"[{"node_id":"x-a","status":"skipped"}]"#).is_empty());
    }

    #[test]
    fn is_done_reason_only_pr_green_and_advisory() {
        // The two reasons MegawalkQueue::close treats as a `backlog done`;
        // DoneBatched/DoneAwaitingMerge are the map_outcome keep-set, not here.
        assert!(is_done_reason(&TerminationReason::DonePRGreen));
        assert!(is_done_reason(&TerminationReason::DoneAdvisory));
        assert!(!is_done_reason(&TerminationReason::DoneBatched));
        assert!(!is_done_reason(&TerminationReason::DoneAwaitingMerge));
        assert!(!is_done_reason(&TerminationReason::NoProgress));
    }

    #[test]
    fn resolved_target_max_lanes_defaults_to_sequential() {
        // A stale emitter (no max_lanes field) must never fan out into lanes.
        let t: ResolvedTarget = serde_json::from_str(
            r#"{"project":"p","cwd":"/x","interval_seconds":60,"failure_limit":3}"#,
        )
        .unwrap();
        assert_eq!(t.max_lanes, 1);
        assert!(!t.batch);
    }

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
