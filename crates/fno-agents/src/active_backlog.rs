//! Active backlog dispatcher: the mission drain-tick core + circuit breaker.
//!
//! This module is the engine for the always-on backlog drain. Since x-a4dc (K2)
//! the drain is MISSION-SCOPED: the daemon's resident supervisor
//! ([`run_supervisor`]) drives one independent drain loop PER ACTIVE MISSION -
//! an epic with `mission_active=true`, K1's activation record - not per project.
//! The legacy per-project interval drain is deleted (epic Locked Decision 4);
//! merge-triggered `fno backlog advance` is the same-project coverage.
//!
//! ## Mission tick (dispatch + reconcile)
//!
//! One *tick* first RECONCILES any dispatches fired on a prior tick from events
//! (feeding [`map_outcome`] -> the auto-defer breaker), then DISPATCHES by
//! shelling K1's converge core, `fno backlog advance --epic <id> --json`. That
//! core fans out the epic's ready LEAF children across ALL projects, doing its
//! own per-dependent-root `walker:<root>` respect, per-project `max_lanes` cap,
//! and `node:`/`dispatch:` claim dedup - so the mission drain reuses the exact
//! dispatch logic the merge-advance path uses and never forks it. See
//! [`dispatch_mission`] / [`mission_drain_tick`] / [`mission_drain_loop`].
//!
//! ## Fire-and-forget reconcile (x-0ad6, preserved)
//!
//! The tick does NOT own the worker child. `advance --epic` self-mints each
//! worker session and re-anchors the `node:<id>` claim to `target-session:<sid>`.
//! A later tick RECONCILES each dispatched node by reading its session id back
//! from the claim holder and polling its termination event
//! (`Journal::find_termination`), then feeding the outcome through
//! [`map_outcome`] - so the auto-defer streak is identical to the supervised
//! path. A worker that dies without a termination event is caught by the crash
//! floor (claim gone past the boot window). See [`reconcile_pending`].
//!
//! ## Circuit-breaker park (recoverable, per mission)
//!
//! When a child fails `failure_limit` consecutive drains the breaker trips and
//! the daemon `fno backlog defer`s the node (graph state), then resets the
//! in-memory streak. Independent branches keep dispatching while one branch is
//! parked. Deferring (not an endlessly-refreshed claim) is what makes the park
//! recoverable: `fno backlog undefer` returns the node with a fresh
//! `failure_limit` attempts. The breaker is per mission loop.
//!
//! ## Mission liveness
//!
//! Each tick re-checks the mission: `advance --epic` reporting `deactivated` or
//! `all_done`, or the epic dropping out of the resolved target set (its
//! `mission_active` cleared), RETIRES the loop - no zombie ticks.
//!
//! ## Events (Journal contract)
//!
//! Every transition emits through [`Journal::append`] (project journal fatal,
//! global mirror best-effort): `active_backlog_dispatched` / `_parked` /
//! `_skip` / `_mission_retired`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::claims::{self, ClaimState};
use crate::events::EventEmitter;
use crate::loop_megawalk::{abi_cmd, retry_etxtbsy};
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

/// Everything one [`mission_drain_tick`] needs, resolved by the daemon per tick.
///
/// The dispatch logic lives in `advance --epic` (K1's converge core), so the
/// daemon carries only what reconcile + the breaker need: the epic id to
/// converge, the epic's own cwd (roots the journal + node-global `done`/`defer`
/// reads), the `fno` binary, and the failure limit.
#[derive(Debug, Clone)]
pub struct DrainConfig {
    /// The mission's epic project cwd - roots the journal and the node-global
    /// `backlog done`/`defer` reads (a mission fans out across projects at
    /// dispatch time via `advance --epic`, not here).
    pub cwd: PathBuf,
    /// The `fno` binary name/path (FNO_BIN override honored by the caller).
    pub abi_bin: String,
    /// The active mission's epic id - the `advance --epic <mission>` argument.
    pub mission: String,
    /// Cross-tick consecutive-failure limit (the circuit breaker).
    pub failure_limit: u32,
}

/// What one [`mission_drain_tick`]'s reconcile did, for tests. Dispatch itself
/// returns [`MissionDispatch`]; these are the outcomes [`map_outcome`] produces
/// as it feeds the breaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// A node was reconciled and closed successfully.
    Dispatched { node: String },
    /// A node tripped the circuit breaker and was deferred (parked).
    Parked { node: String, failures: u32 },
    /// No node to reconcile / dispatch this tick.
    NoWork,
    /// The tick could not reconcile a node to a close (a node that failed
    /// without yet tripping the breaker).
    Skipped { reason: String },
}

/// Whether the mission is still live after a dispatch, or should retire its loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionDispatch {
    /// The mission dispatched (or found nothing new); keep ticking.
    Continue,
    /// `advance --epic` reported the mission deactivated / all children done.
    Retire,
}

/// Best-effort `fno backlog defer <node>` for the circuit-breaker park. Graph
/// state, recoverable via `fno backlog undefer`. Node ids are global, so the
/// epic's cwd is a valid working dir for the child-node defer.
fn defer_node(abi_bin: &str, cwd: &Path, node: &str, reason: &str) {
    let _ = abi_cmd(abi_bin)
        .current_dir(cwd)
        .args(["backlog", "defer", node, "--reason", reason])
        .output();
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
        // x-aba7: an exit-5 (PR OPEN, not merged) close arrives here as
        // AwaitingMerge with a DonePRGreen reason (the DoneAwaitingMerge-reason
        // early return above handles the other producer). It is a SUCCESSFUL
        // dispatch - closed later at the human merge by reconcile - so it must
        // never trip the cross-tick circuit breaker (mirror the DoneBatched /
        // DoneAwaitingMerge-reason keep-set). Without this, every healthy
        // ship-green close would count as a failed drain and auto-defer the node.
        CloseOutcome::AwaitingMerge => {
            breaker.record_success(&node);
            let _ = journal.append(
                "active_backlog_dispatched",
                json!({"node_id": node, "awaiting_merge": true, "close": "awaiting-merge"}),
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

// ── fire-and-forget reconcile (x-0ad6) ───────────────────────────────────────
//
// A tick DISPATCHES the mission's ready children fire-and-forget via K1's
// converge core (`fno backlog advance --epic`, which routes through `fno agents
// spawn`, self-mints each worker session, and re-anchors the `node:<id>` claim
// to `target-session:<sid>`), then RECONCILES prior dispatches from events across
// later ticks - never owning the worker child.
//
// Failure accounting is reconstructed from the worker's own termination event
// (find_termination on the session id read back from the claim holder) fed
// through the `map_outcome` policy, so the auto-defer streak is identical by
// construction. A worker that dies without emitting any termination event is
// caught by the crash floor (claim gone past the boot window), replacing the
// awaited-exit-code `node_failed` watchdog the fire-and-forget model can no
// longer read.

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
    pending.retain_mut(|p| {
        p.ticks += 1;
        // `node:<id>` is a GLOBAL-id claim: it routes to $FNO_CLAIMS_ROOT (else
        // $HOME) by prefix, NOT under the project cwd, so the worker (which
        // acquires it via `fno claim` with no explicit root) and this read must
        // resolve the SAME dir. Passing Some(cfg.cwd) would look in the wrong
        // place and never find the worker's claim (claim_status root mismatch).
        let (state, rec) = claims::status(&format!("node:{}", p.node_id), None);
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
    // Mirror MegawalkQueue::close EXACTLY: a DonePRGreen/DoneAdvisory close runs
    // `fno backlog done` (retry_etxtbsy for a transient busy binary) and Closes
    // only on success - a failed `done` Parks with the error, so the breaker
    // counts it as a failure just as the supervised path did (never a false
    // success). Exit 5 (PR OPEN, not merged) is AwaitingMerge, not a failure:
    // a no-merge dispatch lands its PR open, so `done` exits 5 and the node
    // closes at the human merge via reconcile - map_outcome's keep-set counts
    // it as a successful dispatch. DoneBatched/DoneAwaitingMerge close at merge
    // via reconcile and are NOT marked here - map_outcome recognizes them too.
    let close = if is_done_reason(&ev.reason) {
        match retry_etxtbsy(|| {
            abi_cmd(&cfg.abi_bin)
                .args(["backlog", "done", node_id])
                .current_dir(&cfg.cwd)
                .output()
        }) {
            Ok(o) if o.status.success() => CloseOutcome::Closed,
            Ok(o) if o.status.code() == Some(5) => CloseOutcome::AwaitingMerge,
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                CloseOutcome::Parked(if stderr.is_empty() {
                    format!("fno backlog done {node_id} failed (exit {})", o.status)
                } else {
                    stderr
                })
            }
            Err(e) => CloseOutcome::Parked(format!("fno backlog done {node_id} spawn failed: {e}")),
        }
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
fn resolve_crash(
    cfg: &DrainConfig,
    breaker: &mut CircuitBreaker,
    journal: &Journal,
    node_id: &str,
) {
    let message = "worker exited with no termination event (fire-and-forget crash floor)";
    let ur = UnitResult {
        unit_id: node_id.to_string(),
        evidence: Evidence {
            reason: TerminationReason::NoProgress,
            message: message.to_string(),
        },
        close: CloseOutcome::Parked(message.to_string()),
    };
    map_outcome(
        cfg,
        breaker,
        journal,
        &TerminationReason::NoProgress,
        Some(&ur),
    );
}

/// The `fno backlog advance --epic <id> --json` receipt, the only fields the
/// mission drain reads. `#[serde(default)]` on every field so a partial or
/// evolving receipt never fails the parse (a missing field defaults benignly).
#[derive(Debug, Default, Deserialize)]
struct AdvanceEpicReceipt {
    #[serde(default)]
    deactivated: bool,
    #[serde(default)]
    all_done: bool,
    /// Node ids `advance --epic` dispatched this pass (fire-and-forget), to be
    /// reconciled from events on later ticks.
    #[serde(default)]
    dispatched: Vec<String>,
}

/// Dispatch the mission by shelling K1's converge core, recording each dispatched
/// child in `pending` for later reconcile. Returns [`MissionDispatch::Retire`]
/// when `advance --epic` reports the mission deactivated / all children done.
///
/// The converge core owns ALL dispatch policy (cross-project fan-out, per-root
/// `walker:` respect, `max_lanes` cap, claim dedup), so this never forks it. A
/// non-zero exit or unparseable receipt is a transient skip (Continue) - a truly
/// gone mission is caught by the loop's re-resolve, not guessed at here.
fn dispatch_mission(
    cfg: &DrainConfig,
    pending: &mut Vec<PendingDispatch>,
    journal: &Journal,
) -> MissionDispatch {
    let out = match retry_etxtbsy(|| {
        abi_cmd(&cfg.abi_bin)
            // --continuation: never reactivate the mission and retire an inactive
            // one, so an operator `--stop` between drain ticks is not undone.
            .args([
                "backlog",
                "advance",
                "--epic",
                &cfg.mission,
                "--continuation",
                "--json",
            ])
            .current_dir(&cfg.cwd)
            .output()
    }) {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let detail = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "advance-epic-failed", "mission": cfg.mission, "detail": detail}),
            );
            return MissionDispatch::Continue;
        }
        Err(e) => {
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "advance-epic-failed", "mission": cfg.mission, "detail": format!("{e}")}),
            );
            return MissionDispatch::Continue;
        }
    };
    let receipt: AdvanceEpicReceipt = match serde_json::from_slice(&out.stdout) {
        Ok(r) => r,
        Err(e) => {
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "advance-epic-unparseable", "mission": cfg.mission, "detail": format!("{e}")}),
            );
            return MissionDispatch::Continue;
        }
    };
    if receipt.deactivated || receipt.all_done {
        return MissionDispatch::Retire;
    }
    let mut new_ids = Vec::new();
    for node_id in &receipt.dispatched {
        // Guard against re-recording a still-pending node (a prior tick's
        // dispatch whose worker has not yet closed): advance already dedups by
        // live claim, but a boot-window respawn could echo the id.
        if pending.iter().any(|p| p.node_id == *node_id) {
            continue;
        }
        pending.push(PendingDispatch {
            node_id: node_id.clone(),
            session_id: None,
            ticks: 0,
        });
        new_ids.push(node_id.clone());
    }
    if !new_ids.is_empty() {
        let _ = journal.append(
            "active_backlog_dispatched",
            json!({"mission": cfg.mission, "dispatched": new_ids, "fire_and_forget": true}),
        );
    }
    MissionDispatch::Continue
}

/// One mission drain tick: reconcile prior dispatches (feeding the breaker), then
/// dispatch the mission's currently-ready children. Reconcile runs FIRST so a
/// child that just auto-deferred is excluded from this tick's `advance --epic`
/// selection. Synchronous (the loop offloads it to a blocking task).
pub fn mission_drain_tick(
    cfg: &DrainConfig,
    breaker: &mut CircuitBreaker,
    pending: &mut Vec<PendingDispatch>,
    journal: &Journal,
) -> MissionDispatch {
    reconcile_pending(cfg, breaker, pending, journal);
    dispatch_mission(cfg, pending, journal)
}

// ── target resolution + resident supervisor ─────────────────────────────────────

/// One mission drain target as resolved by the Python `fno config
/// active-backlog --json` helper (an active mission + the epic's workspace path).
#[derive(Debug, Clone, Deserialize)]
pub struct ResolvedTarget {
    /// The mission epic's own project (for keying + cwd resolution).
    pub project: String,
    /// The epic project's cwd - roots the loop's journal + node-global reads.
    pub cwd: String,
    pub interval_seconds: u64,
    pub failure_limit: u32,
    /// The active mission's epic id (the drain's `advance --epic` argument).
    /// Optional only so a malformed receipt deserializes; a target with no
    /// mission is skipped by the supervisor.
    #[serde(default)]
    pub mission: Option<String>,
}

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
/// Cap on a single `fno status-fanout tick` child. A legitimately slow tick
/// (several stalled sinks x (retries+1) x http_timeout + backoff) can reach
/// minutes; 300s bounds the pathological hang, not normal work.
const TICK_CHILD_CAP: Duration = Duration::from_secs(300);

/// Await `cmd`'s completion bounded by `cap`, killing the child on timeout.
/// Returns `true` if the child exceeded the cap and was killed. `kill_on_drop`
/// is load-bearing: on timeout the `output()` future is dropped, which SIGKILLs
/// the child - without it a wedged tick parks the loop's shutdown response and
/// leaks one subprocess per tick. Extracted for unit-testability.
async fn output_with_cap(mut cmd: tokio::process::Command, cap: Duration) -> bool {
    cmd.kill_on_drop(true);
    match tokio::time::timeout(cap, cmd.output()).await {
        Ok(Ok(_)) => false,
        // Spawn/exec failure (binary missing, cwd gone, ...) is best-effort like
        // the tick itself, but log it - a swallowed missing-`fno` is undiagnosable.
        Ok(Err(e)) => {
            eprintln!("fanout tick failed to execute: {e}");
            false
        }
        Err(_) => true,
    }
}

async fn per_project_fanout_loop(target: FanoutTarget, abi_bin: String, shutdown: Arc<AtomicBool>) {
    let project = target.project.clone();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // Re-resolve between ticks so config changes land without a daemon
        // restart; removing this project's sinks EXITS the loop.
        let interval = match resolve_fanout_targets(&abi_bin)
            .into_iter()
            .find(|t| t.project == project)
        {
            Some(t) => Duration::from_secs(t.interval_seconds.max(1)),
            None => break, // sinks removed for this project -> stop ticking.
        };
        let mut cmd = tokio::process::Command::new(&abi_bin);
        cmd.args(["status-fanout", "tick"]).current_dir(&target.cwd);
        // Failure otherwise swallowed (next tick retries; at-least-once cursor
        // semantics). The kill must NOT be silent - the one line below is required.
        if output_with_cap(cmd, TICK_CHILD_CAP).await {
            eprintln!(
                "fanout tick for {project} exceeded {TICK_CHILD_CAP:?}; killed, retrying next tick"
            );
        }
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

/// Resolve a [`DrainConfig`] for a mission target, or `None` if the target
/// carries no mission id (a malformed receipt). No driver-lib preflight: the
/// worker drivers are resolved per CHILD project inside `advance --epic`, not at
/// the epic's cwd, so the epic project need not itself be drivable.
fn drain_config_for(target: &ResolvedTarget, abi_bin: &str) -> Option<DrainConfig> {
    let mission = target.mission.clone()?;
    Some(DrainConfig {
        cwd: PathBuf::from(&target.cwd),
        abi_bin: abi_bin.to_string(),
        mission,
        failure_limit: target.failure_limit,
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
    // Mission drain loops, keyed by epic id (x-a4dc K2): one per active mission.
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
        // Drop handles for loops that have exited (a mission retired / deactivated).
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
            // Key by mission (epic id). A target with no mission is a malformed
            // receipt; skip it rather than key an unnamed loop.
            let Some(mission) = target.mission.clone() else {
                continue;
            };
            // Entry API (single lookup): only spawn when this mission has no live
            // loop yet, mirroring the fanout family below.
            if let std::collections::hash_map::Entry::Vacant(slot) = tasks.entry(mission) {
                slot.insert(tokio::spawn(mission_drain_loop(
                    target,
                    abi_bin.clone(),
                    emitter.clone(),
                    Arc::clone(&shutdown),
                )));
            }
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

/// One mission's independent drain loop: reconcile + dispatch the mission's ready
/// children, wait the poll floor (or an event nudge), repeat. Owns its own
/// [`CircuitBreaker`] so failure streaks are per mission. Exits when `shutdown`
/// flips, the mission drops out of the resolved target set (its `mission_active`
/// was cleared), or `advance --epic` reports the mission deactivated / all done.
async fn mission_drain_loop(
    target: ResolvedTarget,
    abi_bin: String,
    emitter: EventEmitter,
    shutdown: Arc<AtomicBool>,
) {
    // A malformed target with no mission is filtered by the supervisor before
    // spawn; default to empty so this never panics if one slips through (the
    // re-resolve below then finds no match and exits).
    let mission = target.mission.clone().unwrap_or_default();
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

        // Re-resolve this mission's liveness. If its epic dropped out of the
        // target set (mission_active cleared externally), exit the loop (the
        // supervisor will not respawn it).
        let current = resolve_targets(&abi_bin)
            .into_iter()
            .find(|t| t.mission.as_deref() == Some(mission.as_str()));
        let Some(t) = current else {
            break;
        };
        let interval = Duration::from_secs(t.interval_seconds.max(1));

        let Some(cfg) = drain_config_for(&t, &abi_bin) else {
            // Malformed target (no mission id); back off and re-check.
            sleep_interruptible(interval, &shutdown).await;
            continue;
        };
        let journal = journal_for(&cfg.cwd);

        // The tick is synchronous; offload so the async runtime is never stalled.
        // Move the breaker AND pending set in and hand them back so the streak
        // and in-flight tracking survive the tick.
        let taken_b = std::mem::take(&mut breaker);
        let taken_p = std::mem::take(&mut pending);
        let handle = tokio::task::spawn_blocking(move || {
            let mut b = taken_b;
            let mut p = taken_p;
            let outcome = mission_drain_tick(&cfg, &mut b, &mut p, &journal);
            (outcome, b, p)
        });
        match handle.await {
            Ok((outcome, b, p)) => {
                breaker = b;
                pending = p;
                backoff = Duration::from_secs(1);
                if outcome == MissionDispatch::Retire {
                    let _ = emitter.emit(
                        "active_backlog_mission_retired",
                        &json!({"mission": mission}),
                    );
                    break;
                }
            }
            Err(join_err) => {
                let _ = emitter.emit(
                    "active_backlog_task_crashed",
                    &json!({"mission": mission, "error": join_err.to_string()}),
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

    #[tokio::test]
    async fn tick_child_killed_at_cap() {
        // A tick child that never exits must be dead within cap+epsilon so the
        // loop (and daemon shutdown) proceeds, not block on the hung child.
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("60");
        let start = std::time::Instant::now();
        let timed_out = output_with_cap(cmd, Duration::from_millis(150)).await;
        assert!(timed_out, "a hung child must report timed-out");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must return near the cap, not wait on the 60s child"
        );
    }

    #[tokio::test]
    async fn tick_child_within_cap_reports_ok() {
        // A child that finishes under the cap is not reported as timed-out.
        let cmd = tokio::process::Command::new("true");
        let timed_out = output_with_cap(cmd, Duration::from_secs(30)).await;
        assert!(!timed_out, "a fast child must not be reported as timed-out");
    }

    #[test]
    fn advance_epic_receipt_parses_dispatched_and_liveness() {
        // The mission drain reads only dispatched + deactivated + all_done.
        let r: AdvanceEpicReceipt = serde_json::from_slice(
            br#"{"epic_id":"x-e","error":null,"activated":true,"deactivated":false,
                 "all_done":false,"dispatched":["x-a","x-b"],"children":[]}"#,
        )
        .unwrap();
        assert_eq!(r.dispatched, vec!["x-a", "x-b"]);
        assert!(!r.deactivated);
        assert!(!r.all_done);
    }

    #[test]
    fn advance_epic_receipt_defaults_on_partial_json() {
        // A minimal / evolving receipt must never fail the parse (every field
        // defaults benignly): no dispatched nodes, mission still live.
        let r: AdvanceEpicReceipt = serde_json::from_slice(br#"{"epic_id":"x-e"}"#).unwrap();
        assert!(r.dispatched.is_empty());
        assert!(!r.deactivated && !r.all_done);
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

    // ── reconcile policy (x-0ad6) ────────────────────────────────────────────
    //
    // These drive the private reconcile helpers directly with a stub `fno` (for
    // the defer/done side effects) + a temp Journal, so the failure-streak policy
    // is covered without env-mutating claim setup. The crash-floor boot-grace
    // path uses a unique fake node id that is naturally `Free` at the real global
    // claims root, so it reads real state for a key that never exists (and never
    // writes there).

    use std::os::unix::fs::PermissionsExt;

    /// A stub `fno` that appends its argv to `record` and exits 0, so a test can
    /// assert which `backlog done`/`defer` side effects the reconcile fired.
    fn stub_fno(dir: &std::path::Path, record: &std::path::Path) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("fno");
        std::fs::write(
            &p,
            format!(
                "#!/usr/bin/env bash\necho \"$@\" >> \"{}\"\nexit 0\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    fn test_cfg(tmp: &std::path::Path, abi_bin: String, failure_limit: u32) -> DrainConfig {
        DrainConfig {
            cwd: tmp.to_path_buf(),
            abi_bin,
            mission: "x-epic".to_string(),
            failure_limit,
        }
    }

    fn test_journal(tmp: &std::path::Path) -> (Journal, PathBuf) {
        let project = tmp.join(".fno").join("events.jsonl");
        let global = tmp.join("global-events.jsonl");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        (Journal::new_raw(project.clone(), global), project)
    }

    fn journal_lines(p: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn resolve_dispatch_done_records_success_and_marks_done() {
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno(&tmp.path().join("bin"), &record);
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        breaker.record_failure("x-suc0001"); // pre-existing streak to prove reset

        resolve_dispatch(
            &cfg,
            &mut breaker,
            &journal,
            "x-suc0001",
            Evidence {
                reason: TerminationReason::DonePRGreen,
                message: "done".to_string(),
            },
        );

        assert_eq!(
            breaker.consecutive_failures("x-suc0001"),
            0,
            "success resets the streak"
        );
        // is_done_reason -> the reconcile marks the node done (mirrors queue.close).
        let calls = std::fs::read_to_string(&record).unwrap_or_default();
        assert!(calls.contains("backlog done x-suc0001"), "calls: {calls}");
        assert!(journal_lines(&project_journal)
            .iter()
            .any(|l| l.contains("active_backlog_dispatched") && l.contains("x-suc0001")));
    }

    #[test]
    fn resolve_dispatch_awaiting_merge_is_success_without_done() {
        // DoneAwaitingMerge is a successful dispatch (closes at merge via
        // reconcile) - the keep-set records success but must NOT `backlog done`.
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno(&tmp.path().join("bin"), &record);
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, _pj) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        breaker.record_failure("x-awm0001");

        resolve_dispatch(
            &cfg,
            &mut breaker,
            &journal,
            "x-awm0001",
            Evidence {
                reason: TerminationReason::DoneAwaitingMerge,
                message: String::new(),
            },
        );

        assert_eq!(breaker.consecutive_failures("x-awm0001"), 0);
        let calls = std::fs::read_to_string(&record).unwrap_or_default();
        assert!(
            !calls.contains("backlog done"),
            "awaiting-merge must not mark done: {calls}"
        );
    }

    #[test]
    fn resolve_dispatch_failed_done_records_failure_not_false_success() {
        // Parity with MegawalkQueue::close: if `fno backlog done` FAILS, the node
        // was not actually closed, so the dispatch must Park (a failure toward the
        // streak), never a false success. Regression guard for the review finding.
        let tmp = tempfile::TempDir::new().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fno = bin.join("fno");
        std::fs::write(
            &fno,
            "#!/usr/bin/env bash\nif [[ \"$1\" == backlog && \"$2\" == done ]]; then echo 'node has open blockers' >&2; exit 1; fi\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&fno, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = test_cfg(tmp.path(), fno.display().to_string(), 3);
        let (journal, _pj) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);

        resolve_dispatch(
            &cfg,
            &mut breaker,
            &journal,
            "x-donefail",
            Evidence {
                reason: TerminationReason::DonePRGreen,
                message: "done".to_string(),
            },
        );

        assert_eq!(
            breaker.consecutive_failures("x-donefail"),
            1,
            "a failed `backlog done` must count as a failure, not a false success"
        );
    }

    #[test]
    fn resolve_dispatch_done_exit5_is_awaiting_merge_success() {
        // x-aba7: a no-merge dispatch lands its PR OPEN, so `fno backlog done`
        // exits 5 (awaiting merge). That is a SUCCESSFUL dispatch (the node
        // closes at the human merge via reconcile), so the breaker must NOT
        // record a failure - mirror of MegawalkQueue::close's exit-5 mapping.
        let tmp = tempfile::TempDir::new().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fno = bin.join("fno");
        std::fs::write(
            &fno,
            "#!/usr/bin/env bash\nif [[ \"$1\" == backlog && \"$2\" == done ]]; then echo 'awaiting merge: PR OPEN' >&2; exit 5; fi\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&fno, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = test_cfg(tmp.path(), fno.display().to_string(), 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        breaker.record_failure("x-awm5001"); // pre-existing streak to prove reset

        resolve_dispatch(
            &cfg,
            &mut breaker,
            &journal,
            "x-awm5001",
            Evidence {
                reason: TerminationReason::DonePRGreen,
                message: "done".to_string(),
            },
        );

        assert_eq!(
            breaker.consecutive_failures("x-awm5001"),
            0,
            "done exit 5 (awaiting merge) is a success, never a failure"
        );
        assert!(journal_lines(&project_journal)
            .iter()
            .any(|l| l.contains("active_backlog_dispatched") && l.contains("awaiting_merge")));
    }

    #[test]
    fn resolve_crash_at_limit_defers_and_parks() {
        // AC1-FR: a worker death (no termination event) counts as a failure; the
        // Nth consecutive death trips the breaker -> defer + parked event.
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno(&tmp.path().join("bin"), &record);
        let cfg = test_cfg(tmp.path(), fno, 2);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(2);

        resolve_crash(&cfg, &mut breaker, &journal, "x-cra0001"); // failure 1/2
        assert_eq!(breaker.consecutive_failures("x-cra0001"), 1);
        resolve_crash(&cfg, &mut breaker, &journal, "x-cra0001"); // failure 2/2 -> trip

        // Trip defers the node (graph exclusion) and resets the streak.
        assert_eq!(breaker.consecutive_failures("x-cra0001"), 0);
        let calls = std::fs::read_to_string(&record).unwrap_or_default();
        assert!(calls.contains("backlog defer x-cra0001"), "calls: {calls}");
        assert!(journal_lines(&project_journal)
            .iter()
            .any(|l| l.contains("active_backlog_parked") && l.contains("x-cra0001")));
    }

    #[test]
    fn reconcile_boot_grace_then_crash_floor() {
        // A dispatched worker that never takes its `node:<id>` claim (never
        // booted) is kept for BOOT_GRACE_TICKS reconcile passes, then counted as
        // a crash. Uses a unique fake node id (naturally Free at the global root).
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno(&tmp.path().join("bin"), &record);
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, _pj) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = vec![PendingDispatch {
            node_id: "x-bootgrace-never-real".to_string(),
            session_id: None,
            ticks: 0,
        }];

        // Passes before the grace expires keep the dispatch and record nothing.
        for _ in 1..BOOT_GRACE_TICKS {
            reconcile_pending(&cfg, &mut breaker, &mut pending, &journal);
            assert_eq!(
                pending.len(),
                1,
                "must keep the dispatch during the boot window"
            );
            assert_eq!(breaker.consecutive_failures("x-bootgrace-never-real"), 0);
        }
        // The pass that reaches the grace counts a crash-floor failure and drops it.
        reconcile_pending(&cfg, &mut breaker, &mut pending, &journal);
        assert!(
            pending.is_empty(),
            "the never-booted dispatch is retired as a crash"
        );
        assert_eq!(breaker.consecutive_failures("x-bootgrace-never-real"), 1);
    }

    #[test]
    fn resolved_target_parses_mission_target() {
        // The Python emitter's mission-target shape round-trips; a receipt with
        // no mission deserializes (mission=None) so the supervisor can skip it.
        let t: ResolvedTarget = serde_json::from_str(
            r#"{"project":"fno","cwd":"/x","interval_seconds":60,"failure_limit":3,"mission":"x-epic"}"#,
        )
        .unwrap();
        assert_eq!(t.mission.as_deref(), Some("x-epic"));
        let no_mission: ResolvedTarget = serde_json::from_str(
            r#"{"project":"p","cwd":"/x","interval_seconds":60,"failure_limit":3}"#,
        )
        .unwrap();
        assert_eq!(no_mission.mission, None);
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

    /// A stub `fno` whose `backlog advance --epic` prints a fixed JSON receipt on
    /// stdout (exit 0). Any other subcommand is a no-op exit 0.
    fn stub_fno_advance(dir: &std::path::Path, receipt_json: &str) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("fno");
        std::fs::write(
            &p,
            format!(
                "#!/usr/bin/env bash\nif [[ \"$1\" == backlog && \"$2\" == advance ]]; then \
                 cat <<'JSON'\n{receipt_json}\nJSON\nfi\nexit 0\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    #[test]
    fn dispatch_mission_records_dispatched_and_continues() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"epic_id":"x-epic","deactivated":false,"all_done":false,"dispatched":["x-a","x-b"]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut pending = Vec::new();

        let outcome = dispatch_mission(&cfg, &mut pending, &journal);
        assert_eq!(outcome, MissionDispatch::Continue);
        assert_eq!(
            pending
                .iter()
                .map(|p| p.node_id.clone())
                .collect::<Vec<_>>(),
            vec!["x-a", "x-b"]
        );
        assert!(journal_lines(&project_journal)
            .iter()
            .any(|l| l.contains("active_backlog_dispatched") && l.contains("x-a")));
    }

    #[test]
    fn dispatch_mission_retires_on_deactivated() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"epic_id":"x-epic","deactivated":true,"all_done":false,"dispatched":[]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, _pj) = test_journal(tmp.path());
        let mut pending = Vec::new();
        assert_eq!(
            dispatch_mission(&cfg, &mut pending, &journal),
            MissionDispatch::Retire
        );
    }

    #[test]
    fn dispatch_mission_retires_on_all_done() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"epic_id":"x-epic","deactivated":false,"all_done":true,"dispatched":[]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, _pj) = test_journal(tmp.path());
        let mut pending = Vec::new();
        assert_eq!(
            dispatch_mission(&cfg, &mut pending, &journal),
            MissionDispatch::Retire
        );
    }

    #[test]
    fn dispatch_mission_dedups_already_pending() {
        // A boot-window re-echo of a still-pending node must not double-record it.
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"epic_id":"x-epic","dispatched":["x-a"]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, _pj) = test_journal(tmp.path());
        let mut pending = vec![PendingDispatch {
            node_id: "x-a".to_string(),
            session_id: None,
            ticks: 2,
        }];
        dispatch_mission(&cfg, &mut pending, &journal);
        assert_eq!(pending.len(), 1, "x-a already pending must not be re-added");
    }

    #[test]
    fn dispatch_mission_unparseable_receipt_continues() {
        // A garbled receipt is a transient skip (Continue), never a crash or a
        // false Retire (the loop's re-resolve catches a truly gone mission).
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(&tmp.path().join("bin"), "wedged python traceback");
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut pending = Vec::new();
        assert_eq!(
            dispatch_mission(&cfg, &mut pending, &journal),
            MissionDispatch::Continue
        );
        assert!(pending.is_empty());
        assert!(journal_lines(&project_journal)
            .iter()
            .any(|l| l.contains("advance-epic-unparseable")));
    }
}
