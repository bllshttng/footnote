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
use serde_json::{json, Value};

use crate::claims::{self, ClaimState};
use crate::events::EventEmitter;
use crate::loop_dispatch::{fno_cmd, retry_etxtbsy};
use crate::loop_runtime::{
    CloseOutcome, Evidence, GlobalJournalPath, Journal, ProjectJournalPath, UnitResult,
};
use crate::loopcheck::TerminationReason;
use crate::run_outcome::classify;

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
    pub fno_bin: String,
    /// The active mission's epic id - the `advance --epic <mission>` argument.
    pub mission: String,
    /// The territory key (x-e221): the canonical crown scope this loop drains.
    /// Empty on a legacy receipt; the loop then keys by `mission`.
    pub scope: String,
    /// No live crown holds this territory; the readout names it kingless while
    /// the drain continues (machinery does not need a king to dispatch).
    pub kingless: bool,
    /// What one tick converges: epic members shell `advance --epic`, project
    /// members shell `advance --loose --project`. Empty falls back to a single
    /// epic member = `mission` (the legacy single-mission receipt).
    pub members: Vec<DrainMember>,
    /// Cross-tick consecutive-failure limit (the circuit breaker).
    pub failure_limit: u32,
    /// The mission's poll interval, for the control-plane tick row's staleness.
    pub interval_seconds: u64,
    /// 1-based position and population of this mission in the drain rotation
    /// (epic-id order), `None` when it is the only drainable mission. Readout-only:
    /// the arms-table detail prints `mission=x (1 of 4 draining)` so one row sampled
    /// from a many-mission drain cannot read as the whole population. The base is
    /// labeled because the mux band counts all graph-active missions - a mission
    /// with no workspace path drains nowhere but still renders - and two bare `of
    /// N` suffixes with different N read as a contradiction.
    pub rotation: Option<(usize, usize)>,
}

/// One member a territory tick converges (x-e221).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainMember {
    /// The epic id (rung 2) or project name (rungs 0/1) to converge.
    pub id: String,
    /// True: shell `advance --epic <id> --continuation`; false: shell
    /// `advance --loose --project <id>`.
    pub epic: bool,
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
/// `retry_etxtbsy` like every other shellout here - this was the one that
/// skipped it. A transient busy binary (a concurrent `fno doctor update`, or under
/// `cargo test` a sibling thread's freshly-written stub) makes the spawn fail,
/// and `let _ =` swallows it, so a tripped breaker silently never defers its
/// node and re-dispatches it into the same crash loop.
/// Returns whether the defer actually landed, so the caller's `parked` journal
/// row states what happened rather than asserting it. Retrying the spawn is only
/// half the fix: exhausted retries, any other spawn error, and a NON-ZERO exit
/// from `fno backlog defer` (unresolvable id, graph lock contention) all still
/// produce the silent re-dispatch loop described above, and `breaker.reset` runs
/// either way.
fn defer_node(fno_bin: &str, cwd: &Path, node: &str, reason: &str) -> bool {
    match retry_etxtbsy(|| {
        fno_cmd(fno_bin)
            .current_dir(cwd)
            .args(["backlog", "defer", node, "--reason", reason])
            .output()
    }) {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            eprintln!(
                "active-backlog: defer of {node} failed (exit {:?}): {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            eprintln!("active-backlog: defer of {node} could not run: {e}");
            false
        }
    }
}

/// Does the node carry a PR reference in graph state? Read through `fno backlog
/// get` so node resolution stays the CLI's job. FAIL-OPEN: an unreadable or
/// unparseable answer reports `true`, so a flaky read can never auto-defer a
/// healthy node.
fn node_has_pr_ref(cfg: &DrainConfig, node_id: &str) -> bool {
    // retry_etxtbsy like every other shellout here: a transient busy binary
    // (a concurrent `fno doctor update`) must not read as "healthy, has PR" and quietly
    // disable the guard.
    let Ok(out) = retry_etxtbsy(|| {
        fno_cmd(&cfg.fno_bin)
            .args(["backlog", "get", node_id])
            .current_dir(&cfg.cwd)
            .output()
    }) else {
        return true;
    };
    if !out.status.success() {
        return true;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return true;
    };
    // A ref must be USABLE, not merely present: `pr_number` an integer and
    // `pr_url` a non-empty string, matching what the CLI's node_pr_refs can
    // actually derive a ref from. An empty pr_url is not evidence of a ship.
    if v.get("pr_number").and_then(|n| n.as_u64()).is_some() {
        return true;
    }
    if v.get("pr_url")
        .and_then(|u| u.as_str())
        .is_some_and(|u| !u.trim().is_empty())
    {
        return true;
    }
    v.get("additional_prs")
        .and_then(|a| a.as_array())
        .is_some_and(|a| !a.is_empty())
}

/// Reconcile passes a ref-less `DonePRGreen` must persist across before it counts
/// as a dead dispatch. `finalize` stamps `pr_number` AFTER loop-check emits the
/// termination event, and its tail (plan stamp, handoff, verifier) has no bounded
/// duration - so a single poll landing in that window would read a healthy ship
/// as ref-less. Re-checking on a later tick costs nothing and never blocks the
/// drain thread, which a sleep here would.
const PR_STAMP_GRACE_TICKS: u32 = 3;

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
    let predicates = classify(last.evidence.reason.clone()).projection();

    // Batch-lane: a member that terminated DoneBatched succeeded - its commits
    // are on the shared batch branch and it ships via the batch PR, so the node
    // closes at merge by `fno backlog reconcile`, not here. For the daemon that
    // is a SUCCESSFUL dispatch, not a failure. Recognize it in the keep-set so a
    // batched member never trips the cross-tick circuit breaker.
    if predicates.batch_member {
        breaker.record_success(&node);
        let _ = journal.append(
            "active_backlog_dispatched",
            json!({"node_id": node, "termination": "DoneBatched", "batched": true}),
        );
        return DrainOutcome::Dispatched { node };
    }

    // DoneAwaitingMerge / DoneAwaitingReview: the node built successfully (PR
    // up, green) but could not auto-complete - DoneAwaitingMerge is blocked by a
    // proven pre-existing main-red; DoneAwaitingReview by a rate-limited required
    // bot that posted a usage-limit comment instead of a review (x-9ab2). Both
    // are SUCCESSFUL dispatches for the daemon, not failures - the node is closed
    // at the human merge by `fno backlog reconcile`, exactly like DoneBatched.
    // Keep them out of the cross-tick circuit breaker (mirror the DoneBatched
    // keep-set).
    if predicates.circuit_breaker_success {
        breaker.record_success(&node);
        let _ = journal.append(
            "active_backlog_dispatched",
            json!({"node_id": node, "termination": format!("{:?}", last.evidence.reason), "awaiting_merge": true}),
        );
        return DrainOutcome::Dispatched { node };
    }

    // DoneUnreviewed (x-0eaf): green but nothing reviewed. A SUCCESSFUL dispatch
    // for the daemon - the node closes later at a human merge or the reconcile
    // path, exactly like DoneAwaitingMerge. Keep it out of the cross-tick
    // circuit breaker so an unreviewed-but-green terminal does not auto-defer
    // the node as if it had failed.
    if predicates.awaiting_review_notify {
        breaker.record_success(&node);
        let _ = journal.append(
            "active_backlog_dispatched",
            json!({"node_id": node, "termination": "DoneUnreviewed", "awaiting_review": true}),
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
                // Recorded, not asserted: `breaker.reset` below hands the node a
                // fresh streak allowance either way, so a `parked` row claiming
                // a defer that never landed is what an operator debugging a
                // re-dispatch loop would be misled by.
                let deferred = defer_node(&cfg.fno_bin, &cfg.cwd, &node, &reason_str);
                breaker.reset(&node);
                let _ = journal.append(
                    "active_backlog_parked",
                    json!({"node_id": node, "consecutive_failures": cfg.failure_limit, "detail": detail, "deferred": deferred}),
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
    /// Passes this entry has read a ref-less `DonePRGreen`. Lets the PR stamp land
    /// before a zero-artifact verdict sticks (see `PR_STAMP_GRACE_TICKS`).
    stamp_waits: u32,
}

/// Reconcile passes to wait for a dispatched worker to take its `node:<id>`
/// claim before a claim-absent verdict counts as a boot crash.
const BOOT_GRACE_TICKS: u32 = 3;

/// True for the terminal reasons that mark a node done (a successful code or
/// doc delivery). `DoneBatched`/`DoneAwaitingMerge` are NOT here - they close at
/// merge and are recognized as success by `map_outcome`'s keep-set instead.
fn is_done_reason(r: &TerminationReason) -> bool {
    classify(r.clone()).projection().node_closable
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
        // acquires it via `fno agents claim` with no explicit root) and this read must
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
                    // A ref-less DonePRGreen may just be racing finalize's stamp;
                    // keep the entry and re-read on a later tick before deciding.
                    if classify(ev.reason.clone()).projection().merge_armable
                        && !node_has_pr_ref(cfg, &p.node_id)
                        && p.stamp_waits < PR_STAMP_GRACE_TICKS
                    {
                        p.stamp_waits += 1;
                        return true;
                    }
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
    // A successful delivery close runs `fno backlog done` (retry_etxtbsy for a
    // transient busy binary) and Closes only on success - a failed `done` Parks
    // with the error, so the breaker counts it as a failure (never a false
    // success). Exit 5 (PR OPEN, not merged) is AwaitingMerge, not a failure:
    // a no-merge dispatch lands its PR open, so `done` exits 5 and the node
    // closes at the human merge via reconcile - map_outcome's keep-set counts
    // it as a successful dispatch. DoneBatched/DoneAwaitingMerge close at merge
    // via reconcile and are NOT marked here - map_outcome recognizes them too.
    // Park a dead dispatch BEFORE `fno backlog done`: its merged-PR cross-check
    // only runs when refs already exist, so a ref-less node would otherwise
    // close exit 0 and score the dead dispatch as a win.
    let close = if classify(ev.reason.clone()).projection().merge_armable
        && !node_has_pr_ref(cfg, node_id)
    {
        CloseOutcome::Parked(
            "DonePRGreen terminal with no PR ref on the node (zero-artifact dispatch)".to_string(),
        )
    } else if is_done_reason(&ev.reason) {
        match retry_etxtbsy(|| {
            fno_cmd(&cfg.fno_bin)
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

/// One child row from the `advance --epic --json` receipt's `children[]`, the
/// shared vocabulary with the per-child journal events at advance.py:3638.
/// Every field defaulted so an evolving receipt never fails the parse.
#[derive(Debug, Default, Deserialize)]
struct AdvanceChild {
    #[serde(default)]
    #[allow(dead_code)] // not read yet; kept for parity with the CLI receipt shape
    node_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    decision: String,
    #[serde(default)]
    reason: String,
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
    /// Every child the pass considered this tick, dispatched or not - the
    /// honest signal `mission_drain_tick` was previously discarding.
    #[serde(default)]
    children: Vec<AdvanceChild>,
    /// A receipt-level gate error (e.g. `"disabled"`, `"walker-live"`) reported
    /// with no children, distinct from a truly exhausted mission.
    #[serde(default)]
    error: Option<String>,
}

/// Facts about one `dispatch_mission` pass beyond the fire-and-forget dispatch
/// ids, used only by `mission_drain_tick` to name an honest skip reason when
/// nothing new dispatched. `ready` is how many children the pass considered;
/// `reason` is the shared per-child skip reason when every child in the pass
/// agrees, or `"skipped-mixed"` when they don't; `error` is a receipt-level
/// gate error reported with no children.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct DispatchFacts {
    ready: usize,
    reason: Option<String>,
    error: Option<String>,
}

fn undispatched_count(cfg: &DrainConfig) -> Result<usize, String> {
    let out = retry_etxtbsy(|| {
        fno_cmd(&cfg.fno_bin)
            .args(["backlog", "undispatched", "--json"])
            .current_dir(&cfg.cwd)
            .output()
    })
    .map_err(|error| error.to_string())?;
    if !out.status.success() {
        return Err(format!("exit {:?}", out.status.code()));
    }
    let receipt: Value = serde_json::from_slice(&out.stdout).map_err(|error| error.to_string())?;
    if receipt.get("status").and_then(Value::as_str) != Some("ok") {
        return Err("observer status was not ok".to_string());
    }
    receipt
        .get("rows")
        .and_then(Value::as_array)
        .filter(|rows| rows.iter().all(Value::is_object))
        .map(Vec::len)
        .ok_or_else(|| "missing valid rows array".to_string())
}

fn facts_from_receipt(receipt: &AdvanceEpicReceipt) -> DispatchFacts {
    let ready = receipt.children.len();
    let reason = if ready == 0 {
        None
    } else {
        let mut reasons = receipt.children.iter().map(|c| c.reason.as_str());
        let first = reasons.next().unwrap_or("");
        if !first.is_empty() && reasons.all(|r| r == first) {
            Some(first.to_string())
        } else {
            Some("skipped-mixed".to_string())
        }
    };
    DispatchFacts {
        ready,
        reason,
        error: receipt.error.clone(),
    }
}

/// Dispatch ONE territory member by shelling K1's converge core, recording each
/// dispatched child in `pending` for later reconcile. Epic members run
/// `advance --epic <id> --continuation` (Retire on deactivated/all-done);
/// project members run `advance --loose --project <id>` (never retire - a
/// loose territory has no mission lifecycle, x-e221).
///
/// The converge core owns ALL dispatch policy (cross-project fan-out, per-root
/// `walker:` respect, `max_lanes` cap, claim dedup), so this never forks it. A
/// non-zero exit or unparseable receipt is a transient skip (Continue) - a truly
/// gone mission is caught by the loop's re-resolve, not guessed at here.
fn dispatch_member(
    cfg: &DrainConfig,
    member: &DrainMember,
    pending: &mut Vec<PendingDispatch>,
    journal: &Journal,
) -> (MissionDispatch, DispatchFacts) {
    let (mode, extra): (&str, &[&str]) = if member.epic {
        // --continuation: never reactivate the mission and retire an inactive
        // one, so an operator `--stop` between drain ticks is not undone.
        ("--epic", &["--continuation"])
    } else {
        ("--loose", &[])
    };
    let out = match retry_etxtbsy(|| {
        fno_cmd(&cfg.fno_bin)
            .args([
                // The `backlog advance` argv literal at this indentation is the
                // seam marker the autonomous-dispatch census greps. Keep the
                // elements multi-line; `--json` stays last.
                "backlog",
                "advance",
                mode,
                member.id.as_str(),
            ])
            .args(extra)
            .arg("--json")
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
            return (MissionDispatch::Continue, DispatchFacts::default());
        }
        Err(e) => {
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "advance-epic-failed", "mission": cfg.mission, "detail": format!("{e}")}),
            );
            return (MissionDispatch::Continue, DispatchFacts::default());
        }
    };
    let receipt: AdvanceEpicReceipt = match serde_json::from_slice(&out.stdout) {
        Ok(r) => r,
        Err(e) => {
            let _ = journal.append(
                "active_backlog_skip",
                json!({"reason": "advance-epic-unparseable", "mission": cfg.mission, "detail": format!("{e}")}),
            );
            return (MissionDispatch::Continue, DispatchFacts::default());
        }
    };
    let facts = facts_from_receipt(&receipt);
    if member.epic && (receipt.deactivated || receipt.all_done) {
        return (MissionDispatch::Retire, facts);
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
            stamp_waits: 0,
        });
        new_ids.push(node_id.clone());
    }
    if !new_ids.is_empty() {
        let _ = journal.append(
            "active_backlog_dispatched",
            json!({"mission": cfg.mission, "dispatched": new_ids, "fire_and_forget": true}),
        );
    }
    (MissionDispatch::Continue, facts)
}

/// Dispatch the territory by converging EVERY member, recording each dispatched
/// child in `pending` for later reconcile. Retires only when the territory has
/// epic members and EVERY one reports deactivated / all children done - a
/// project member never retires its territory (x-e221: a loose territory has
/// no mission lifecycle; it drains while the workspace exists).
fn dispatch_mission(
    cfg: &DrainConfig,
    pending: &mut Vec<PendingDispatch>,
    journal: &Journal,
) -> (MissionDispatch, DispatchFacts) {
    let members: Vec<DrainMember> = if cfg.members.is_empty() {
        // Legacy single-mission receipt: one epic member = the mission itself.
        vec![DrainMember {
            id: cfg.mission.clone(),
            epic: true,
        }]
    } else {
        cfg.members.clone()
    };
    let mut merged = DispatchFacts::default();
    let mut epic_members = 0usize;
    let mut retired = 0usize;
    for member in &members {
        let (outcome, facts) = dispatch_member(cfg, member, pending, journal);
        merged.ready += facts.ready;
        if merged.error.is_none() {
            merged.error = facts.error.clone();
        }
        if merged.reason.is_none() {
            merged.reason = facts.reason.clone();
        }
        if member.epic {
            epic_members += 1;
            if outcome == MissionDispatch::Retire {
                retired += 1;
            }
        }
    }
    let outcome = if epic_members > 0 && retired == epic_members {
        MissionDispatch::Retire
    } else {
        MissionDispatch::Continue
    };
    (outcome, merged)
}

/// The seed prompt for a machinery-spawned territory blueprinter (x-e221): a
/// worker holds no crown, dispatches nothing, and self-reports nothing - it
/// designs the mailed idea nodes and waits for the next one.
fn blueprinter_prompt(scope: &str) -> String {
    format!(
        "You are the territory blueprinter for scope {scope}. \
When mail arrives carrying /fno:blueprint <node-id>, run the fno:blueprint \
skill for that node. You hold no crown, dispatch nothing, and report \
nothing: finish each blueprint and wait."
    )
}

/// The `blueprint-feed --json` status receipt (x-e221). `ideas` stays raw
/// JSON: the tick only journals ids, it never interprets rungs.
#[derive(Debug, Clone, Deserialize, Default)]
struct BlueprinterStatus {
    #[serde(default)]
    worker: Option<BlueprinterWorker>,
    #[serde(default)]
    worker_name_next: String,
    #[serde(default)]
    ideas: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct BlueprinterWorker {
    #[serde(default)]
    name: String,
    #[serde(default)]
    live: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct BlueprinterDelivery {
    #[serde(default)]
    delivered: Vec<String>,
    #[serde(default)]
    failed: Vec<serde_json::Value>,
}

/// One `agents worker blueprint-feed` call: the Python verb owns the policy
/// (membership, feed windows, the record store, mail transport); the
/// supervisor only decides when to spawn and when to deliver.
fn run_blueprint_feed(cfg: &DrainConfig, extra: &[String]) -> Option<serde_json::Value> {
    let mut args = vec![
        "agents".to_string(),
        "worker".to_string(),
        "blueprint-feed".to_string(),
        "--scope".to_string(),
        cfg.scope.clone(),
        "--json".to_string(),
    ];
    args.extend(extra.iter().cloned());
    let out = retry_etxtbsy(|| {
        fno_cmd(&cfg.fno_bin)
            .args(&args)
            .current_dir(&cfg.cwd)
            .output()
    })
    .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// One territory's blueprinter tick (x-e221 AC5/AC6): with unfed triaged
/// ideas and no live standing worker, spawn AT MOST ONE replacement through
/// the standard `fno agents spawn` gates; then deliver. A refused spawn is
/// recorded as a repair and the ideas stay preserved for the next tick.
fn blueprinter_tick(cfg: &DrainConfig, journal: &Journal) {
    if cfg.scope.is_empty() {
        return; // legacy receipt: no territory, no blueprinter
    }
    let Some(raw) = run_blueprint_feed(cfg, &[]) else {
        let _ = journal.append(
            "blueprinter_status_skip",
            json!({"scope": cfg.scope, "reason": "feed verb failed or unparseable"}),
        );
        return;
    };
    let status: BlueprinterStatus = serde_json::from_value(raw).unwrap_or_default();
    if status.ideas.is_empty() {
        return; // nothing to feed: never spawn a worker without work
    }
    let needs_worker = status.worker.as_ref().map(|w| !w.live).unwrap_or(true);
    if needs_worker {
        if status.worker_name_next.is_empty() {
            let _ = journal.append(
                "blueprinter_spawn_refused",
                json!({"scope": cfg.scope, "reason": "receipt named no worker to spawn"}),
            );
            return;
        }
        let prompt = blueprinter_prompt(&cfg.scope);
        let args = vec![
            "agents".to_string(),
            "spawn".to_string(),
            "--substrate".to_string(),
            "thread".to_string(),
            "--name".to_string(),
            status.worker_name_next.clone(),
            prompt,
        ];
        let spawn = retry_etxtbsy(|| {
            fno_cmd(&cfg.fno_bin)
                .args(&args)
                .current_dir(&cfg.cwd)
                .output()
        });
        match spawn {
            Ok(o) if o.status.success() => {
                let _ = journal.append(
                    "blueprinter_spawned",
                    json!({"scope": cfg.scope, "worker": status.worker_name_next}),
                );
            }
            Ok(o) => {
                let detail = String::from_utf8_lossy(&o.stderr);
                let reason = format!("spawn refused: {}", detail.lines().next().unwrap_or(""));
                run_blueprint_feed(cfg, &["--repair".to_string(), reason.clone()]);
                let _ = journal.append(
                    "blueprinter_spawn_refused",
                    json!({"scope": cfg.scope, "reason": reason}),
                );
                return;
            }
            Err(e) => {
                let reason = format!("spawn failed: {e}");
                run_blueprint_feed(cfg, &["--repair".to_string(), reason.clone()]);
                let _ = journal.append(
                    "blueprinter_spawn_refused",
                    json!({"scope": cfg.scope, "reason": reason}),
                );
                return;
            }
        }
    }
    let Some(raw) = run_blueprint_feed(cfg, &["--deliver".to_string()]) else {
        let _ = journal.append(
            "blueprinter_deliver_skip",
            json!({"scope": cfg.scope, "reason": "deliver verb failed or unparseable"}),
        );
        return;
    };
    let delivery: BlueprinterDelivery = serde_json::from_value(raw).unwrap_or_default();
    if delivery.delivered.is_empty() && delivery.failed.is_empty() {
        return; // blocked receipt or nothing due: the verb recorded its own state
    }
    let worker_name = status
        .worker
        .as_ref()
        .map(|w| w.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| status.worker_name_next.clone());
    let _ = journal.append(
        "blueprinter_delivered",
        json!({
            "scope": cfg.scope,
            "worker": worker_name,
            "delivered": delivery.delivered,
            "failed": delivery.failed.len(),
        }),
    );
}

/// One mission drain tick: reconcile prior dispatches (feeding the breaker), then
/// dispatch the mission's currently-ready children. Reconcile runs FIRST so a
/// child that just auto-deferred is excluded from this tick's `advance --epic`
/// selection. Synchronous (the loop offloads it to a blocking task).
///
/// Every tick also appends one `control_plane_tick` row (arm `active_backlog`),
/// so the arms readout can tell a draining mission from a dead supervisor.
pub fn mission_drain_tick(
    cfg: &DrainConfig,
    breaker: &mut CircuitBreaker,
    pending: &mut Vec<PendingDispatch>,
    journal: &Journal,
) -> MissionDispatch {
    let pending_before = pending.len();
    reconcile_pending(cfg, breaker, pending, journal);
    let closed = pending_before.saturating_sub(pending.len()) as u64;
    let pre_dispatch = pending.len();
    let (outcome, facts) = dispatch_mission(cfg, pending, journal);
    let newly_dispatched = (pending.len() - pre_dispatch) as u64;
    let skip_reason: Option<String> = match outcome {
        MissionDispatch::Retire => Some("mission_retired".to_string()),
        MissionDispatch::Continue if closed + newly_dispatched > 0 => None,
        // Something dispatched by an earlier tick is still running: a full
        // spawn lane on THIS pass does not make that stale.
        MissionDispatch::Continue if !pending.is_empty() => Some("in_flight".to_string()),
        MissionDispatch::Continue => {
            if let Some(err) = &facts.error {
                // The `disabled` / `walker-live` early returns stop
                // masquerading as a genuinely exhausted mission.
                Some(format!("gate:{err}"))
            } else if facts.ready == 0 {
                // True exhaustion: the pass found nothing at all.
                Some("no_work".to_string())
            } else {
                // Children found, none dispatched: name the real skip reason
                // (e.g. `lane-cap`) instead of collapsing it into no_work.
                facts.reason.clone()
            }
        }
    };
    let rotation = cfg
        .rotation
        .map(|(pos, total)| format!(" ({pos} of {total} draining)"))
        .unwrap_or_default();
    let (label, kingless_mark) = if cfg.scope.is_empty() {
        (format!("mission={}", cfg.mission), String::new())
    } else {
        (
            format!("territory={}", cfg.scope),
            if cfg.kingless {
                " kingless".to_string()
            } else {
                String::new()
            },
        )
    };
    let detail = format!(
        "{}{}{} ready={} closed={} dispatched={} pending={}{}",
        label,
        kingless_mark,
        rotation,
        facts.ready,
        closed,
        newly_dispatched,
        pending.len(),
        match skip_reason.as_deref() {
            Some("no_work") => match undispatched_count(cfg) {
                Ok(count) => format!(" stranded={count}"),
                Err(error) => {
                    eprintln!("active-backlog: board-wide stranded count unavailable: {error}");
                    " stranded=unknown".to_string()
                }
            },
            _ => String::new(),
        }
    );
    crate::tick_ledger::emit_tick(
        journal,
        "active_backlog",
        "daemon",
        closed + newly_dispatched,
        skip_reason.as_deref(),
        Some(&detail),
        cfg.interval_seconds.max(1),
    );
    outcome
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
    /// The territory key (x-e221): the canonical crown scope, empty on a
    /// legacy receipt (the loop then keys by `mission`).
    #[serde(default)]
    pub scope: String,
    /// The crown rung of the scope; 0 on a legacy receipt.
    #[serde(default)]
    pub rung: u8,
    /// No live crown holds the scope; the drain continues regardless.
    #[serde(default)]
    pub kingless: bool,
    /// What one tick converges: epic ids at rung 2, project names at rungs 0/1.
    #[serde(default)]
    pub members: Vec<String>,
}

/// The drain loop's key: the territory scope when the receipt carries one,
/// else the legacy mission id (an older Python resolver still in the field).
fn territory_key(target: &ResolvedTarget) -> String {
    if target.scope.is_empty() {
        target.mission.clone().unwrap_or_default()
    } else {
        target.scope.clone()
    }
}

/// Shell `fno config active-backlog --json` to discover enabled drain targets.
/// Best-effort: any failure (missing fno, non-zero exit, unparseable output)
/// yields an empty list, so the feature simply stays dormant.
pub fn resolve_targets(fno_bin: &str) -> Vec<ResolvedTarget> {
    resolve_targets_report(fno_bin).0
}

/// [`resolve_targets`] plus the failure detail the supervisor reports in its
/// tick row: an empty target list from a broken resolver (`env_broken`, the
/// missing-click class) is a different arm state from an empty list because
/// nothing is enabled (`no_missions`).
pub fn resolve_targets_report(fno_bin: &str) -> (Vec<ResolvedTarget>, Option<String>) {
    match fno_cmd(fno_bin)
        .args(["config", "active-backlog", "--json"])
        .output()
    {
        Ok(o) if o.status.success() => match serde_json::from_slice(&o.stdout) {
            Ok(targets) => (targets, None),
            Err(e) => (
                Vec::new(),
                Some(format!("active-backlog receipt unparseable: {e}")),
            ),
        },
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            (
                Vec::new(),
                Some(format!(
                    "active-backlog resolve exited {}: {}",
                    o.status,
                    stderr.chars().take(120).collect::<String>()
                )),
            )
        }
        Err(e) => (
            Vec::new(),
            Some(format!("active-backlog resolve failed: {e}")),
        ),
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
fn resolve_fanout_targets(fno_bin: &str) -> Vec<FanoutTarget> {
    match fno_cmd(fno_bin)
        .args(["config", "status-sinks", "--json"])
        .output()
    {
        Ok(o) if o.status.success() => serde_json::from_slice(&o.stdout).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// One project's status-fanout loop: shell `fno doctor event fanout tick` in the
/// project cwd on the configured cadence, best-effort. Independent of the drain
/// loops; a tick failure is swallowed and the next tick retries. Between ticks it
/// re-resolves its own enablement (codex P2): a new `interval_secs` is picked up,
/// and removing the project's sinks EXITS the loop (so `retain(!is_finished)`
/// reaps it) rather than ticking forever. Exits on shutdown.
/// Cap on a single `fno doctor event fanout tick` child. A legitimately slow tick
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

async fn per_project_fanout_loop(target: FanoutTarget, fno_bin: String, shutdown: Arc<AtomicBool>) {
    let project = target.project.clone();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // Re-resolve between ticks so config changes land without a daemon
        // restart; removing this project's sinks EXITS the loop.
        let interval = match resolve_fanout_targets(&fno_bin)
            .into_iter()
            .find(|t| t.project == project)
        {
            Some(t) => Duration::from_secs(t.interval_seconds.max(1)),
            None => break, // sinks removed for this project -> stop ticking.
        };
        let mut cmd = tokio::process::Command::new(&fno_bin);
        cmd.args(["doctor", "event", "fanout", "tick"])
            .current_dir(&target.cwd);
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
    let project_events = ProjectJournalPath::from_caller_root(cwd);
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let global_events = home.join(".fno").join("events.jsonl");
    Journal::new(project_events, GlobalJournalPath(global_events))
}

/// Resolve a [`DrainConfig`] for a mission target, or `None` if the target
/// carries no mission id (a malformed receipt). No driver-lib preflight: the
/// worker drivers are resolved per CHILD project inside `advance --epic`, not at
/// the epic's cwd, so the epic project need not itself be drivable.
fn drain_config_for(
    target: &ResolvedTarget,
    fno_bin: &str,
    rotation: Option<(usize, usize)>,
) -> Option<DrainConfig> {
    let key = territory_key(target);
    if key.is_empty() {
        // A malformed receipt (no scope and no mission) keys an unnamed loop.
        return None;
    }
    let members: Vec<DrainMember> = target
        .members
        .iter()
        .map(|id| DrainMember {
            id: id.clone(),
            epic: target.rung == 2,
        })
        .collect();
    Some(DrainConfig {
        cwd: PathBuf::from(&target.cwd),
        fno_bin: fno_bin.to_string(),
        mission: target.mission.clone().unwrap_or_else(|| key.clone()),
        scope: target.scope.clone(),
        kingless: target.kingless,
        members,
        failure_limit: target.failure_limit,
        interval_seconds: target.interval_seconds,
        rotation,
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
    fno_bin: String,
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

        let (targets, resolve_failure) = resolve_targets_report(&fno_bin);
        let fanout_targets = resolve_fanout_targets(&fno_bin);
        // `live` keeps the daemon out of idle-exit while ANY supervised work
        // exists - drain OR fanout. A sink-only project (no active_backlog) must
        // keep the daemon alive, else the daemon idle-exits and kills its fanout
        // loop (codex P1).
        live.store(
            !targets.is_empty() || !fanout_targets.is_empty(),
            Ordering::SeqCst,
        );

        // The arm's supervisor-level tick row, ONLY while no mission loop is
        // live to write its own (fresher) rows: it says why the drain has
        // nothing to do - a broken resolver (env_broken, the class that ticked
        // silently for hours because its Python env lacked click) or simply no
        // enabled missions. ab_live covers the fanout family too.
        if targets.is_empty() {
            let _ = emitter.emit(
                crate::tick_ledger::EVENT_TYPE,
                &serde_json::json!({
                    "arm": "active_backlog",
                    "scheduler": "daemon",
                    "acted": 0,
                    "skip_reason": if resolve_failure.is_some() { "env_broken" } else { "no_missions" },
                    "detail": format!(
                        "targets=0 ab_live={} fanouts={}{}",
                        !fanout_targets.is_empty(),
                        fanout_targets.len(),
                        resolve_failure.as_deref().map(|f| format!(" resolve={f}")).unwrap_or_default()
                    ),
                    // The recheck cadence this row actually rides, not the
                    // configured drain interval: staleness must bound the
                    // supervisor's own 60s loop.
                    "interval_s": 60,
                }),
            );
        }

        for target in targets {
            // Key by territory (x-e221): the canonical crown scope; a legacy
            // receipt without one still keys by mission. An empty key is a
            // malformed receipt; skip it rather than key an unnamed loop.
            let key = territory_key(&target);
            if key.is_empty() {
                continue;
            }
            // Entry API (single lookup): only spawn when this territory has no
            // live loop yet, mirroring the fanout family below.
            if let std::collections::hash_map::Entry::Vacant(slot) = tasks.entry(key) {
                slot.insert(tokio::spawn(mission_drain_loop(
                    target,
                    fno_bin.clone(),
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
                    fno_bin.clone(),
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
    fno_bin: String,
    emitter: EventEmitter,
    shutdown: Arc<AtomicBool>,
) {
    // A malformed target with no key is filtered by the supervisor before
    // spawn; default to empty so this never panics if one slips through (the
    // re-resolve below then finds no match and exits).
    let key = territory_key(&target);
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

        // Re-resolve this territory's liveness. If its scope dropped out of the
        // target set (crown revoked / workspace gone), exit the loop (the
        // supervisor will not respawn it). The position in this list (already
        // scope-ordered) names the rotation in the tick's detail row; a lone
        // territory prints no `(1 of 1)` - that reads as a fault, not a count.
        let all = resolve_targets(&fno_bin);
        let Some(pos) = all.iter().position(|t| territory_key(t) == key) else {
            break;
        };
        let t = &all[pos];
        let interval = Duration::from_secs(t.interval_seconds.max(1));
        let rotation = (all.len() >= 2).then_some((pos + 1, all.len()));

        let Some(cfg) = drain_config_for(t, &fno_bin, rotation) else {
            // Malformed target (no mission id); back off and re-check.
            sleep_interruptible(interval, &shutdown).await;
            continue;
        };
        let journal = journal_for(&cfg.cwd);

        // The tick is synchronous; offload so the async runtime is never stalled.
        // Move the breaker AND pending set in and hand them back so the streak
        // and in-flight tracking survive the tick. The blueprinter tick rides
        // the same blocking task, BEFORE the drain: a plan designed this tick
        // can dispatch on it the same tick.
        let taken_b = std::mem::take(&mut breaker);
        let taken_p = std::mem::take(&mut pending);
        let handle = tokio::task::spawn_blocking(move || {
            blueprinter_tick(&cfg, &journal);
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
                    let _ =
                        emitter.emit("active_backlog_mission_retired", &json!({"mission": key}));
                    break;
                }
            }
            Err(join_err) => {
                let _ = emitter.emit(
                    "active_backlog_task_crashed",
                    &json!({"mission": key, "error": join_err.to_string()}),
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
    fn is_done_reason_includes_generic_delivery() {
        // The terminal reasons that count as a `backlog done`;
        // DoneBatched/DoneAwaitingMerge are the map_outcome keep-set, not here.
        assert!(is_done_reason(&TerminationReason::DonePRGreen));
        assert!(is_done_reason(&TerminationReason::DoneAdvisory));
        assert!(is_done_reason(&TerminationReason::DoneDelivery));
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

    /// Hold this for the whole body of any test that shells `fno_cmd`.
    ///
    /// `fno_cmd` resolves its binary from the process-global `$FNO_BIN` IN
    /// PREFERENCE to the path passed in, and cargo runs a crate's tests as
    /// threads in ONE process. So while a sibling test has `FNO_BIN` set to its
    /// own stub (`scrape.rs` does exactly this), every stub built here is
    /// silently bypassed and that sibling's stub runs instead - which answers
    /// nothing this test asked, leaving an empty receipt that
    /// `dispatch_mission` correctly treats as a benign skip. The failure
    /// therefore surfaces as a plain empty-vec assertion far from its cause,
    /// and only under enough parallelism to overlap the two.
    ///
    /// Reading `$FNO_BIN` needs the lock exactly as much as writing it: the
    /// race is reader-vs-writer, so a lock only the writer takes excludes
    /// nobody.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

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

    /// Like [`stub_fno`], but `backlog defer` FAILS. `stub_fno` exits 0 for every
    /// verb, so the defer failure branch - the one the retry/report exists for -
    /// is unreachable with it, and the whole thing passes green when reverted.
    fn stub_fno_defer_fails(dir: &std::path::Path, record: &std::path::Path) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("fno");
        std::fs::write(
            &p,
            format!(
                "#!/usr/bin/env bash\n\
                 echo \"$@\" >> \"{}\"\n\
                 if [ \"$2\" = \"defer\" ]; then echo 'node not found' >&2; exit 1; fi\n\
                 exit 0\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    /// Like [`stub_fno`], but `backlog get` answers with `node_json` on stdout so
    /// a test can control whether the node carries a PR ref. Every other verb
    /// records its argv and exits 0.
    fn stub_fno_get(dir: &std::path::Path, record: &std::path::Path, node_json: &str) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("fno");
        std::fs::write(
            &p,
            format!(
                "#!/usr/bin/env bash\n\
                 if [ \"$2\" = \"get\" ]; then printf '%s' '{}'; exit 0; fi\n\
                 echo \"$@\" >> \"{}\"\nexit 0\n",
                node_json,
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    fn test_cfg(tmp: &std::path::Path, fno_bin: String, failure_limit: u32) -> DrainConfig {
        DrainConfig {
            cwd: tmp.to_path_buf(),
            fno_bin,
            mission: "x-epic".to_string(),
            scope: String::new(),
            kingless: false,
            members: Vec::new(),
            failure_limit,
            interval_seconds: 300,
            rotation: None,
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
        let _env = env_guard();
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
    fn resolve_dispatch_done_pr_green_without_pr_ref_is_a_failure() {
        let _env = env_guard();
        // The dead-dispatch signature. A DonePRGreen terminal asserts a
        // PR; a node carrying none means the worker died leaving nothing. It must
        // count toward the streak AND must not `backlog done` (whose merged-PR
        // cross-check is skipped entirely for a ref-less node).
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno_get(
            &tmp.path().join("bin"),
            &record,
            r#"{"id":"x-dead0001","status":"in_review"}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);

        resolve_dispatch(
            &cfg,
            &mut breaker,
            &journal,
            "x-dead0001",
            Evidence {
                reason: TerminationReason::DonePRGreen,
                message: "promised".to_string(),
            },
        );

        assert_eq!(
            breaker.consecutive_failures("x-dead0001"),
            1,
            "a zero-artifact DonePRGreen counts toward the streak"
        );
        let calls = std::fs::read_to_string(&record).unwrap_or_default();
        assert!(
            !calls.contains("backlog done"),
            "must not close a node whose terminal lied: {calls}"
        );
        assert!(journal_lines(&project_journal)
            .iter()
            .any(|l| l.contains("active_backlog_skip") && l.contains("x-dead0001")));
    }

    #[test]
    fn resolve_dispatch_done_pr_green_with_pr_ref_is_success() {
        let _env = env_guard();
        // The healthy counterpart: a PR ref present means the terminal told the
        // truth, so the existing close path runs untouched.
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno_get(
            &tmp.path().join("bin"),
            &record,
            r#"{"id":"x-live0001","pr_number":477}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, _pj) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);

        resolve_dispatch(
            &cfg,
            &mut breaker,
            &journal,
            "x-live0001",
            Evidence {
                reason: TerminationReason::DonePRGreen,
                message: String::new(),
            },
        );

        assert_eq!(breaker.consecutive_failures("x-live0001"), 0);
        let calls = std::fs::read_to_string(&record).unwrap_or_default();
        assert!(calls.contains("backlog done x-live0001"), "calls: {calls}");
    }

    #[test]
    fn zero_artifact_check_fails_open_on_unreadable_node() {
        let _env = env_guard();
        // Fail-open is the safety property: an unparseable `backlog get` must
        // never auto-defer a healthy node. `stub_fno` prints nothing, so the
        // parse fails and the node reports as PR-bearing.
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno(&tmp.path().join("bin"), &record);
        let cfg = test_cfg(tmp.path(), fno, 3);

        assert!(
            node_has_pr_ref(&cfg, "x-unknown1"),
            "unreadable node must fail open"
        );
    }

    #[test]
    fn pr_ref_read_unions_additional_prs() {
        let _env = env_guard();
        // The CLI's node_pr_refs unions additional_prs; if this predicate did not,
        // a node whose only ref lives there would read as a dead dispatch.
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno_get(
            &tmp.path().join("bin"),
            &record,
            r#"{"id":"x-addl0001","additional_prs":[{"number":12}]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);

        assert!(node_has_pr_ref(&cfg, "x-addl0001"));
    }

    #[test]
    fn empty_pr_url_is_not_a_ref() {
        let _env = env_guard();
        // A ref must be usable: `--pr-url ""` is present-but-empty and the CLI
        // can derive no ref from it, so it must not read as evidence of a ship.
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno_get(
            &tmp.path().join("bin"),
            &record,
            r#"{"id":"x-empt0001","pr_url":"  "}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);

        assert!(!node_has_pr_ref(&cfg, "x-empt0001"));
    }

    #[test]
    fn resolve_dispatch_advisory_without_pr_ref_is_still_success() {
        let _env = env_guard();
        // DoneAdvisory is a doc terminal with no PR by design - the zero-artifact
        // guard must not touch it, or every doc run would trip the breaker.
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno_get(
            &tmp.path().join("bin"),
            &record,
            r#"{"id":"x-doc00001","status":"in_review"}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, _pj) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);

        resolve_dispatch(
            &cfg,
            &mut breaker,
            &journal,
            "x-doc00001",
            Evidence {
                reason: TerminationReason::DoneAdvisory,
                message: String::new(),
            },
        );

        assert_eq!(breaker.consecutive_failures("x-doc00001"), 0);
        let calls = std::fs::read_to_string(&record).unwrap_or_default();
        assert!(calls.contains("backlog done x-doc00001"), "calls: {calls}");
    }

    #[test]
    fn resolve_dispatch_awaiting_merge_is_success_without_done() {
        let _env = env_guard();
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
        // If `fno backlog done` FAILS, the node was not actually closed, so the
        // dispatch must Park (a failure toward the streak), never a false success.
        // Regression guard for the review finding.
        let _env = env_guard();
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
        // record a failure for the exit-5 awaiting-merge mapping.
        let _env = env_guard();
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
    fn resolve_dispatch_done_exit6_is_parked_with_refusal_intact() {
        // x-5d34: the promise gate refuses with exit 6. The loop closer must NOT
        // need a new arm - the catch-all (non-zero, non-5) maps it to Parked with
        // the stderr intact, so an unmet promise parks the node with its refusal
        // as the recorded reason rather than closing it.
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fno = bin.join("fno");
        std::fs::write(
            &fno,
            "#!/usr/bin/env bash\nif [[ \"$1\" == backlog && \"$2\" == done ]]; then\n  echo 'Refused: x-5d34 promised 2 waves and asserts none of them.' >&2\n  exit 6\nfi\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&fno, std::fs::Permissions::from_mode(0o755)).unwrap();
        // failure_limit 1 so a single exit-6 trips and emits the parked event.
        let cfg = test_cfg(tmp.path(), fno.display().to_string(), 1);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(1);

        resolve_dispatch(
            &cfg,
            &mut breaker,
            &journal,
            "x-prom6001",
            Evidence {
                reason: TerminationReason::DonePRGreen,
                message: "done".to_string(),
            },
        );

        // Exit 6 is a failure (unlike exit 5's awaiting-merge success): the
        // streak tripped at limit 1, so the node is parked, not closed.
        let parked = journal_lines(&project_journal)
            .into_iter()
            .find(|l| l.contains("active_backlog_parked") && l.contains("x-prom6001"))
            .expect("a promise-gate refusal (exit 6) must park, not close");
        // The refusal text rode through the catch-all arm intact - an operator
        // reading the journal sees WHY the close was refused, not a bare code.
        assert!(
            parked.contains("promised 2 waves"),
            "refusal text must survive into the parked detail: {parked}"
        );
    }

    #[test]
    fn resolve_crash_at_limit_defers_and_parks() {
        let _env = env_guard();
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
        let parked = journal_lines(&project_journal)
            .into_iter()
            .find(|l| l.contains("active_backlog_parked") && l.contains("x-cra0001"))
            .expect("parked event");
        // A defer that landed is recorded as such, not assumed.
        assert!(parked.contains("\"deferred\":true"), "parked: {parked}");
    }

    #[test]
    fn park_records_a_defer_that_did_not_land() {
        let _env = env_guard();
        // `breaker.reset` runs whether or not the defer succeeded, so a `parked`
        // row that ASSERTS the park misleads whoever debugs the resulting
        // re-dispatch loop: the node is back with a fresh streak allowance and
        // the journal says it was parked. Spawning the child is not evidence it
        // worked - the stub every other test uses exits 0 for every verb, which
        // is why reverting the report left the suite green.
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno_defer_fails(&tmp.path().join("bin"), &record);
        let cfg = test_cfg(tmp.path(), fno, 2);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(2);

        resolve_crash(&cfg, &mut breaker, &journal, "x-cra0002");
        resolve_crash(&cfg, &mut breaker, &journal, "x-cra0002"); // trips

        let calls = std::fs::read_to_string(&record).unwrap_or_default();
        assert!(calls.contains("backlog defer x-cra0002"), "calls: {calls}");
        let parked = journal_lines(&project_journal)
            .into_iter()
            .find(|l| l.contains("active_backlog_parked") && l.contains("x-cra0002"))
            .expect("parked event still emitted on a failed defer");
        assert!(
            parked.contains("\"deferred\":false"),
            "a defer that exited non-zero must be recorded as not landed: {parked}"
        );
    }

    #[test]
    fn reconcile_boot_grace_then_crash_floor() {
        let _env = env_guard();
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
            stamp_waits: 0,
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
    fn refless_done_pr_green_waits_for_the_stamp_before_parking() {
        let _env = env_guard();
        // finalize stamps pr_number after loop-check emits termination, and its
        // tail is unbounded - so a ref-less read is held across ticks rather than
        // decided on the spot. Only a dispatch still ref-less after the grace is
        // a dead dispatch.
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("fno-calls.txt");
        let fno = stub_fno_get(
            &tmp.path().join("bin"),
            &record,
            r#"{"id":"x-grace-never-real"}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        std::fs::write(
            &project_journal,
            "{\"type\":\"termination\",\"data\":{\"session_id\":\"sid-grace\",\"reason\":\"DonePRGreen\"}}\n",
        )
        .unwrap();
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = vec![PendingDispatch {
            node_id: "x-grace-never-real".to_string(),
            session_id: Some("sid-grace".to_string()),
            ticks: 0,
            stamp_waits: 0,
        }];

        for _ in 0..PR_STAMP_GRACE_TICKS {
            reconcile_pending(&cfg, &mut breaker, &mut pending, &journal);
            assert_eq!(pending.len(), 1, "held while the stamp may still land");
            assert_eq!(breaker.consecutive_failures("x-grace-never-real"), 0);
        }

        reconcile_pending(&cfg, &mut breaker, &mut pending, &journal);
        assert!(
            pending.is_empty(),
            "grace exhausted: the dispatch is retired"
        );
        assert_eq!(
            breaker.consecutive_failures("x-grace-never-real"),
            1,
            "a still-ref-less DonePRGreen counts toward the streak"
        );
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

    fn stub_fno_advance_with_observer(
        dir: &std::path::Path,
        advance_json: &str,
        observer_json: &str,
        observer_marker: Option<&std::path::Path>,
    ) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("fno");
        let observer = observer_marker
            .map(|path| format!("printf 'called' > '{}'\n", path.display()))
            .unwrap_or_default();
        std::fs::write(
            &p,
            format!(
                "#!/usr/bin/env bash\nif [[ \"$1\" == backlog && \"$2\" == advance ]]; then \\
                 cat <<'JSON'\n{advance_json}\nJSON\nelif [[ \"$1\" == backlog && \"$2\" == undispatched ]]; then \\
                 {observer}printf '%s' '{observer_json}'\nfi\nexit 0\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    #[test]
    fn dispatch_mission_records_dispatched_and_continues() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"epic_id":"x-epic","deactivated":false,"all_done":false,"dispatched":["x-a","x-b"]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut pending = Vec::new();

        let (outcome, _facts) = dispatch_mission(&cfg, &mut pending, &journal);
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
    fn mission_drain_tick_appends_one_control_plane_tick_row() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"epic_id":"x-epic","deactivated":false,"all_done":false,"dispatched":["x-a"]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = Vec::new();

        let outcome = mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);

        assert_eq!(outcome, MissionDispatch::Continue);
        let rows: Vec<serde_json::Value> = journal_lines(&project_journal)
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "control_plane_tick")
            .collect();
        assert_eq!(rows.len(), 1, "exactly one arm row per tick");
        let data = &rows[0]["data"];
        assert_eq!(data["arm"], "active_backlog");
        assert_eq!(data["scheduler"], "daemon");
        assert_eq!(data["acted"], 1);
        assert_eq!(data["interval_s"], 300);
        assert!(data["skip_reason"].is_null());
        assert!(data["detail"].as_str().unwrap().contains("dispatched=1"));
    }

    #[test]
    fn mission_drain_tick_does_not_read_stranded_count_after_dispatch() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join("observer-called");
        let fno = stub_fno_advance_with_observer(
            &tmp.path().join("bin"),
            r#"{"epic_id":"x-epic","deactivated":false,"all_done":false,"dispatched":["x-a"]}"#,
            r#"{"status":"ok","rows":[{}]}"#,
            Some(&marker),
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, _project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = Vec::new();

        mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);

        assert!(!marker.exists(), "stranded observer should be no-work only");
    }

    #[test]
    fn tick_detail_names_the_rotation_when_many_missions() {
        // The arms table shows ONE row per arm (newest tick wins), so a drain
        // with several active missions must say which sample it is: `mission=x
        // (1 of 4)`. Without it one mission's row reads as the whole rotation.
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"epic_id":"x-epic","deactivated":false,"all_done":false,"dispatched":[]}"#,
        );
        let mut cfg = test_cfg(tmp.path(), fno, 3);
        cfg.rotation = Some((1, 4));
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = Vec::new();

        mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);

        let detail = journal_lines(&project_journal)
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "control_plane_tick")
            .map(|v| v["data"]["detail"].as_str().unwrap().to_string())
            .expect("one tick row");
        assert!(
            detail.contains("mission=x-epic (1 of 4 draining) "),
            "rotation named in: {detail}"
        );
    }

    #[test]
    fn tick_detail_omits_rotation_when_single_mission() {
        // Control half: a lone mission still names itself, and a bare `(1 of 1)`
        // never prints - it reads as a fault, not a count.
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"epic_id":"x-epic","deactivated":false,"all_done":false,"dispatched":[]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = Vec::new();

        mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);

        let detail = journal_lines(&project_journal)
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "control_plane_tick")
            .map(|v| v["data"]["detail"].as_str().unwrap().to_string())
            .expect("one tick row");
        assert!(
            detail.contains("mission=x-epic ready="),
            "mission named bare in: {detail}"
        );
        assert!(!detail.contains("1 of 1"), "no bare (1 of 1) in: {detail}");
    }

    #[test]
    fn dispatch_mission_retires_on_deactivated() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"epic_id":"x-epic","deactivated":true,"all_done":false,"dispatched":[]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, _pj) = test_journal(tmp.path());
        let mut pending = Vec::new();
        assert_eq!(
            dispatch_mission(&cfg, &mut pending, &journal).0,
            MissionDispatch::Retire
        );
    }

    #[test]
    fn dispatch_mission_retires_on_all_done() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"epic_id":"x-epic","deactivated":false,"all_done":true,"dispatched":[]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, _pj) = test_journal(tmp.path());
        let mut pending = Vec::new();
        assert_eq!(
            dispatch_mission(&cfg, &mut pending, &journal).0,
            MissionDispatch::Retire
        );
    }

    #[test]
    fn dispatch_mission_dedups_already_pending() {
        let _env = env_guard();
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
            stamp_waits: 0,
        }];
        dispatch_mission(&cfg, &mut pending, &journal);
        assert_eq!(pending.len(), 1, "x-a already pending must not be re-added");
    }

    #[test]
    fn dispatch_mission_unparseable_receipt_continues() {
        let _env = env_guard();
        // A garbled receipt is a transient skip (Continue), never a crash or a
        // false Retire (the loop's re-resolve catches a truly gone mission).
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(&tmp.path().join("bin"), "wedged python traceback");
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut pending = Vec::new();
        assert_eq!(
            dispatch_mission(&cfg, &mut pending, &journal).0,
            MissionDispatch::Continue
        );
        assert!(pending.is_empty());
        assert!(journal_lines(&project_journal)
            .iter()
            .any(|l| l.contains("advance-epic-unparseable")));
    }

    #[test]
    fn mission_drain_tick_names_lane_cap_instead_of_no_work() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"children":[{"node_id":"x-a","decision":"skipped","reason":"lane-cap"}],"dispatched":[]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = Vec::new();

        let outcome = mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);
        assert_eq!(outcome, MissionDispatch::Continue);

        let rows: Vec<serde_json::Value> = journal_lines(&project_journal)
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "control_plane_tick")
            .collect();
        let data = &rows[0]["data"];
        assert_eq!(data["skip_reason"], "lane-cap");
        let detail = data["detail"].as_str().unwrap();
        assert!(detail.contains("ready=1"), "detail was {detail}");
        assert!(detail.contains("pending=0"), "detail was {detail}");
    }

    #[test]
    fn mission_drain_tick_reports_no_work_only_when_truly_exhausted() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(
            &tmp.path().join("bin"),
            r#"{"children":[],"dispatched":[]}"#,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = Vec::new();

        mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);

        let rows: Vec<serde_json::Value> = journal_lines(&project_journal)
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "control_plane_tick")
            .collect();
        assert_eq!(rows[0]["data"]["skip_reason"], "no_work");
    }

    #[test]
    fn mission_drain_tick_reports_stranded_board_rows_on_no_work() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let observer = r#"{"status":"ok","rows":[{}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}]}"#;
        let fno = stub_fno_advance_with_observer(
            &tmp.path().join("bin"),
            r#"{"children":[],"dispatched":[]}"#,
            observer,
            None,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = Vec::new();

        mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);

        let row = journal_lines(&project_journal)
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "control_plane_tick")
            .expect("one tick row");
        assert_eq!(row["data"]["skip_reason"], "no_work");
        let detail = row["data"]["detail"].as_str().unwrap();
        assert!(detail.contains("mission=x-epic"), "detail was {detail}");
        assert!(detail.contains("stranded=25"), "detail was {detail}");
    }

    #[test]
    fn mission_drain_tick_names_unknown_stranded_count_on_observer_failure() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance_with_observer(
            &tmp.path().join("bin"),
            r#"{"children":[],"dispatched":[]}"#,
            "not-json",
            None,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = Vec::new();

        mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);

        let row = journal_lines(&project_journal)
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "control_plane_tick")
            .expect("one tick row");
        let detail = row["data"]["detail"].as_str().unwrap();
        assert!(detail.contains("stranded=unknown"), "detail was {detail}");
        assert!(!detail.contains("stranded=0"), "detail was {detail}");
    }

    #[test]
    fn mission_drain_tick_names_unknown_stranded_count_on_observer_error() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance_with_observer(
            &tmp.path().join("bin"),
            r#"{"children":[],"dispatched":[]}"#,
            r#"{"status":"error","rows":[]}"#,
            None,
        );
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = Vec::new();

        mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);

        let row = journal_lines(&project_journal)
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["type"] == "control_plane_tick")
            .expect("one tick row");
        let detail = row["data"]["detail"].as_str().unwrap();
        assert!(detail.contains("stranded=unknown"), "detail was {detail}");
        assert!(!detail.contains("stranded=0"), "detail was {detail}");
    }

    #[test]
    fn mission_drain_tick_names_gate_error_instead_of_no_work() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let fno = stub_fno_advance(&tmp.path().join("bin"), r#"{"error":"disabled"}"#);
        let cfg = test_cfg(tmp.path(), fno, 3);
        let (journal, project_journal) = test_journal(tmp.path());
        let mut breaker = CircuitBreaker::new(3);
        let mut pending = Vec::new();

        mission_drain_tick(&cfg, &mut breaker, &mut pending, &journal);

        let rows: Vec<serde_json::Value> = journal_lines(&project_journal)
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "control_plane_tick")
            .collect();
        assert_eq!(rows[0]["data"]["skip_reason"], "gate:disabled");
    }

    /// A stub `fno` that records every argv, answers `agents worker
    /// blueprint-feed` with `status_json`, and (optionally) fails
    /// `agents spawn` so the repair path is reachable.
    fn stub_fno_blueprint_feed(
        dir: &std::path::Path,
        record: &std::path::Path,
        status_json: &str,
        spawn_fails: bool,
    ) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("fno");
        let spawn_arm = if spawn_fails {
            "if [ \"$2\" = \"spawn\" ]; then echo 'gate: refused' >&2; exit 1; fi\n"
        } else {
            ""
        };
        std::fs::write(
            &p,
            format!(
                "#!/usr/bin/env bash\n\
                 echo \"$@\" >> \"{}\"\n\
                 if [ \"$2\" = \"worker\" ]; then\n\
                 case \"$*\" in\n\
                 *--deliver*) printf '%s' '{{\"action\":\"deliver\",\"delivered\":[\"x-1\",\"x-2\"],\"failed\":[]}}';;\n\
                 *) printf '%s' '{}';;\n\
                 esac\n\
                 exit 0\n\
                 fi\n\
                 {}\
                 exit 0\n",
                record.display(),
                status_json,
                spawn_arm
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    fn territory_cfg(tmp: &std::path::Path, fno_bin: String) -> DrainConfig {
        let mut cfg = test_cfg(tmp, fno_bin, 3);
        cfg.scope = "x-a792".to_string();
        cfg
    }

    fn journal_rows(p: &std::path::Path, event: &str) -> Vec<serde_json::Value> {
        journal_lines(p)
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == event)
            .collect()
    }

    #[test]
    fn blueprinter_tick_spawns_one_worker_then_delivers() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("argv.log");
        let status = r#"{"action":"status","scope":"x-a792","worker":null,
            "worker_name_next":"blueprinter-x-a792-abc123",
            "ideas":[{"id":"x-1","rung":"idea"},{"id":"x-2","rung":"design"}]}"#;
        let fno = stub_fno_blueprint_feed(&tmp.path().join("bin"), &record, status, false);
        let cfg = territory_cfg(tmp.path(), fno);
        let (journal, project_journal) = test_journal(tmp.path());

        blueprinter_tick(&cfg, &journal);

        let argv = std::fs::read_to_string(&record).unwrap();
        assert_eq!(argv.matches("agents spawn").count(), 1);
        assert!(argv.contains("--substrate thread"));
        assert!(argv.contains("--name blueprinter-x-a792-abc123"));
        assert!(argv.contains("territory blueprinter for scope x-a792"));
        assert_eq!(argv.matches("--deliver").count(), 1);
        eprintln!(
            "JOURNAL RAW: {:?}",
            std::fs::read_to_string(&project_journal).unwrap_or_default()
        );
        eprintln!(
            "JOURNAL ROWS: {:?}",
            journal_rows(&project_journal, "blueprinter_spawned")
        );
        eprintln!("JOURNAL LINES: {:?}", journal_lines(&project_journal));
        assert_eq!(
            journal_rows(&project_journal, "blueprinter_spawned").len(),
            1
        );
        let delivered = journal_rows(&project_journal, "blueprinter_delivered");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0]["data"]["worker"], "blueprinter-x-a792-abc123");
    }

    #[test]
    fn blueprinter_tick_reuses_a_live_worker() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("argv.log");
        let status = r#"{"action":"status","scope":"x-a792",
            "worker":{"name":"blueprinter-x-a792-abc123","live":true},
            "worker_name_next":"blueprinter-x-a792-abc123",
            "ideas":[{"id":"x-1","rung":"idea"}]}"#;
        let fno = stub_fno_blueprint_feed(&tmp.path().join("bin"), &record, status, false);
        let cfg = territory_cfg(tmp.path(), fno);
        let (journal, project_journal) = test_journal(tmp.path());

        blueprinter_tick(&cfg, &journal);

        let argv = std::fs::read_to_string(&record).unwrap();
        assert!(!argv.contains(" agents spawn "));
        assert_eq!(argv.matches("--deliver").count(), 1);
        assert!(journal_rows(&project_journal, "blueprinter_spawned").is_empty());
    }

    #[test]
    fn blueprinter_tick_records_repair_when_spawn_refused() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("argv.log");
        let status = r#"{"action":"status","scope":"x-a792","worker":null,
            "worker_name_next":"blueprinter-x-a792-abc123",
            "ideas":[{"id":"x-1","rung":"idea"}]}"#;
        let fno = stub_fno_blueprint_feed(&tmp.path().join("bin"), &record, status, true);
        let cfg = territory_cfg(tmp.path(), fno);
        let (journal, project_journal) = test_journal(tmp.path());

        blueprinter_tick(&cfg, &journal);

        let argv = std::fs::read_to_string(&record).unwrap();
        assert_eq!(argv.matches("agents spawn").count(), 1);
        assert!(argv.contains("--repair"));
        assert!(!argv.contains("--deliver"));
        let repairs = journal_rows(&project_journal, "blueprinter_spawn_refused");
        assert_eq!(repairs.len(), 1);
        assert!(repairs[0]["data"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("spawn refused"));
    }

    #[test]
    fn blueprinter_tick_idles_without_ideas_or_scope() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let record = tmp.path().join("argv.log");
        let fno = stub_fno_blueprint_feed(
            &tmp.path().join("bin"),
            &record,
            r#"{"action":"status","ideas":[]}"#,
            false,
        );
        let cfg = territory_cfg(tmp.path(), fno);
        let (journal, project_journal) = test_journal(tmp.path());
        blueprinter_tick(&cfg, &journal);
        assert!(journal_rows(&project_journal, "blueprinter_spawned").is_empty());

        // A legacy receipt (empty scope) never calls the feed verb at all.
        let record2 = tmp.path().join("argv2.log");
        let fno2 = stub_fno_blueprint_feed(
            &tmp.path().join("bin2"),
            &record2,
            r#"{"action":"status","ideas":[{"id":"x-1"}]}"#,
            false,
        );
        let mut legacy = test_cfg(tmp.path(), fno2, 3);
        legacy.scope = String::new();
        blueprinter_tick(&legacy, &journal);
        assert!(journal_lines(&record2).is_empty());
    }

    #[test]
    fn blueprinter_status_defaults_on_partial_json() {
        let status: BlueprinterStatus = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(status.worker.is_none());
        assert!(status.worker_name_next.is_empty());
        assert!(status.ideas.is_empty());
    }
}
